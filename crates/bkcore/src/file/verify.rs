//! Proving the destination still matches the source.
//!
//! A mirror run reports what it *did*. This answers a different and more
//! valuable question: is the spare drive, right now, actually a faithful copy?
//! That is the question you want answered before you rely on a backup, and it
//! is not the same as "the last run said it succeeded" — bit rot, a failing
//! cable, or somebody editing the backup directly all break the copy without
//! any run having gone wrong.
//!
//! Two depths:
//!
//! * **shallow** — compares size and modification time. Fast enough to run
//!   nightly after the mirror, and catches truncation, missing files and
//!   anything that changed size.
//! * **deep** — hashes both sides with BLAKE3. The only check that catches
//!   silent corruption, at the cost of reading every byte on both drives.

use super::copy;
use super::filter::Filter;
use super::path::RelPath;
use super::walk::{self, ScanOptions};
use super::Cancel;
use crate::Result;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

/// How a file failed to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Difference {
    /// In the source, absent from the destination.
    Missing,
    /// In the destination, absent from the source.
    Extra,
    /// A file on one side, a directory or link on the other.
    KindDiffers,
    SizeDiffers {
        source: u64,
        dest: u64,
    },
    /// Same size, different bytes. Only a deep check finds these.
    ContentDiffers,
    /// Could not be read on one side or the other.
    Unreadable(String),
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Difference::Missing => f.write_str("missing from the destination"),
            Difference::Extra => f.write_str("only in the destination"),
            Difference::KindDiffers => f.write_str("different kind of entry"),
            Difference::SizeDiffers { source, dest } => {
                write!(f, "size {source} vs {dest}")
            }
            Difference::ContentDiffers => f.write_str("same size, different contents"),
            Difference::Unreadable(e) => write!(f, "unreadable: {e}"),
        }
    }
}

/// The result of a comparison.
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub deep: bool,
    pub files_checked: u64,
    pub files_matching: u64,
    pub bytes_checked: u64,
    /// Everything that did not match, capped so a wholly divergent pair of
    /// trees does not exhaust memory.
    pub differences: Vec<(RelPath, Difference)>,
    pub differences_total: u64,
    pub cancelled: bool,
}

/// Beyond this many recorded differences we keep counting but stop listing.
const MAX_RECORDED: usize = 5_000;

impl VerifyReport {
    /// Did everything match?
    pub fn matched(&self) -> bool {
        self.differences_total == 0 && !self.cancelled
    }

    pub fn describe(&self) -> String {
        let mut s = format!(
            "  {} files checked ({}), {} matched, {} differed  [{}]\n",
            self.files_checked,
            super::plan::human_bytes(self.bytes_checked),
            self.files_matching,
            self.differences_total,
            if self.deep {
                "deep: hashed"
            } else {
                "shallow: size + time"
            }
        );
        for (rel, d) in self.differences.iter().take(20) {
            s.push_str(&format!("    {rel} — {d}\n"));
        }
        if self.differences.len() > 20 {
            s.push_str(&format!(
                "    ... and {} more\n",
                self.differences_total as usize - 20
            ));
        }
        if self.cancelled {
            s.push_str("  (cancelled before finishing)\n");
        }
        s
    }
}

/// Compare two trees.
///
/// `deep` hashes file contents; otherwise size and modification time are
/// compared, using the destination filesystem's timestamp tolerance.
#[allow(clippy::too_many_arguments)]
pub fn compare(
    source: &Path,
    dest: &Path,
    filter: &Filter,
    scan: &ScanOptions,
    deep: bool,
    workers: usize,
    cancel: &Cancel,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<VerifyReport> {
    let src_tree = walk::scan(source, filter, scan, cancel, &mut |_| {})?;
    let dst_tree = walk::scan(dest, filter, scan, cancel, &mut |_| {})?;
    let dst_index = dst_tree.index();
    let tolerance = super::sys::time_tolerance_for(dest);

    let mut report = VerifyReport {
        deep,
        ..Default::default()
    };

    // Pair up the two trees first. This part is cheap and sequential; the
    // expensive hashing below is what gets parallelized.
    let mut to_hash: Vec<(RelPath, u64)> = Vec::new();
    for s in &src_tree.entries {
        if s.is_dir() {
            continue;
        }
        let Some(d) = dst_index.get(&s.rel.key()).copied() else {
            record(&mut report, s.rel.clone(), Difference::Missing);
            continue;
        };
        if s.kind != d.kind {
            record(&mut report, s.rel.clone(), Difference::KindDiffers);
            continue;
        }
        if s.is_link() {
            // A link matches when it points at the same place.
            if s.link_target != d.link_target {
                record(&mut report, s.rel.clone(), Difference::ContentDiffers);
            } else {
                report.files_checked += 1;
                report.files_matching += 1;
            }
            continue;
        }
        if s.size != d.size {
            record(
                &mut report,
                s.rel.clone(),
                Difference::SizeDiffers {
                    source: s.size,
                    dest: d.size,
                },
            );
            continue;
        }
        if deep {
            to_hash.push((s.rel.clone(), s.size));
        } else {
            report.files_checked += 1;
            report.bytes_checked += s.size;
            if s.looks_unchanged_from(d, tolerance) {
                report.files_matching += 1;
            } else {
                record_counted(&mut report, s.rel.clone(), Difference::ContentDiffers);
            }
        }
    }

    // Destination entries the source does not have.
    let src_index = src_tree.index();
    for d in &dst_tree.entries {
        if d.is_dir() {
            continue;
        }
        if !src_index.contains_key(&d.rel.key()) {
            record(&mut report, d.rel.clone(), Difference::Extra);
        }
    }

    if deep && !to_hash.is_empty() {
        hash_pass(
            source,
            dest,
            &to_hash,
            workers,
            cancel,
            &mut report,
            progress,
        );
    }
    report.cancelled = cancel.is_cancelled();
    Ok(report)
}

fn record(report: &mut VerifyReport, rel: RelPath, d: Difference) {
    report.files_checked += 1;
    record_counted(report, rel, d);
}

fn record_counted(report: &mut VerifyReport, rel: RelPath, d: Difference) {
    report.differences_total += 1;
    if report.differences.len() < MAX_RECORDED {
        report.differences.push((rel, d));
    }
}

/// Hash every paired file on both sides, in parallel.
fn hash_pass(
    source: &Path,
    dest: &Path,
    files: &[(RelPath, u64)],
    workers: usize,
    cancel: &Cancel,
    report: &mut VerifyReport,
    progress: &mut dyn FnMut(u64, u64),
) {
    let cursor = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let found: Mutex<Vec<(RelPath, Difference)>> = Mutex::new(Vec::new());
    let matched = AtomicU64::new(0);

    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            scope.spawn(|| loop {
                if cancel.is_cancelled() {
                    return;
                }
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                let Some((rel, size)) = files.get(i) else {
                    return;
                };
                let sp = rel.under(source);
                let dp = rel.under(dest);
                let outcome = match (copy::hash_file(&sp), copy::hash_file(&dp)) {
                    (Ok(a), Ok(b)) if a == b => None,
                    (Ok(_), Ok(_)) => Some(Difference::ContentDiffers),
                    (Err(e), _) | (_, Err(e)) => Some(Difference::Unreadable(e.to_string())),
                };
                match outcome {
                    None => {
                        matched.fetch_add(1, Ordering::Relaxed);
                    }
                    Some(d) => {
                        if let Ok(mut g) = found.lock() {
                            g.push((rel.clone(), d));
                        }
                    }
                }
                done.fetch_add(1, Ordering::Relaxed);
                bytes.fetch_add(*size, Ordering::Relaxed);
            });
        }
    });

    let checked = done.load(Ordering::Relaxed);
    report.files_checked += checked;
    report.files_matching += matched.load(Ordering::Relaxed);
    report.bytes_checked += bytes.load(Ordering::Relaxed);
    progress(checked, files.len() as u64);

    // Consume the mutex rather than locking it: the workers are all joined by
    // now, so there is nothing to share, and taking it by value avoids a
    // borrow that would outlive the local.
    let mut collected = found.into_inner().unwrap_or_default();
    // Stable output regardless of which worker happened to finish first.
    collected.sort_by(|a, b| a.0.cmp(&b.0));
    for (rel, d) in collected {
        report.differences_total += 1;
        if report.differences.len() < MAX_RECORDED {
            report.differences.push((rel, d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_verify_{n}"));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn write(&self, rel: &str, bytes: &[u8]) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, bytes).unwrap();
            // Pin the modification time.
            //
            // Two trees built independently get different mtimes, and the
            // shallow check would rightly call that a difference. A real
            // mirror copies the source's mtime to the destination, so the
            // fixtures have to as well or they are not modelling a mirror.
            set_mtime(&p, FIXED_MTIME);
        }
    }

    /// Every fixture file gets this timestamp, the way a mirrored copy would.
    const FIXED_MTIME: u64 = 1_700_000_000;

    fn set_mtime(p: &Path, unix: u64) {
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix);
        fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn check(a: &Path, b: &Path, deep: bool) -> VerifyReport {
        compare(
            a,
            b,
            &Filter::permissive(),
            &ScanOptions::default(),
            deep,
            2,
            &Cancel::none(),
            &mut |_, _| {},
        )
        .unwrap()
    }

    #[test]
    fn identical_trees_verify_clean() {
        let a = Tmp::new("same_a");
        let b = Tmp::new("same_b");
        for t in [&a, &b] {
            t.write("one.txt", b"hello");
            t.write("sub/two.bin", &vec![9u8; 5000]);
        }
        for deep in [false, true] {
            let r = check(&a.0, &b.0, deep);
            assert!(r.matched(), "{}", r.describe());
            assert_eq!(r.files_checked, 2);
            assert_eq!(r.files_matching, 2);
        }
    }

    #[test]
    fn a_missing_file_is_reported() {
        let a = Tmp::new("miss_a");
        let b = Tmp::new("miss_b");
        a.write("one.txt", b"hello");
        a.write("gone.txt", b"absent from the copy");
        b.write("one.txt", b"hello");

        let r = check(&a.0, &b.0, false);
        assert!(!r.matched());
        assert_eq!(r.differences_total, 1);
        assert_eq!(r.differences[0].1, Difference::Missing);
    }

    #[test]
    fn an_extra_file_is_reported() {
        let a = Tmp::new("extra_a");
        let b = Tmp::new("extra_b");
        a.write("one.txt", b"hello");
        b.write("one.txt", b"hello");
        b.write("stray.txt", b"only here");

        let r = check(&a.0, &b.0, false);
        assert_eq!(r.differences_total, 1);
        assert_eq!(r.differences[0].1, Difference::Extra);
    }

    #[test]
    fn a_size_difference_is_caught_without_hashing() {
        let a = Tmp::new("size_a");
        let b = Tmp::new("size_b");
        a.write("f.bin", b"longer contents");
        b.write("f.bin", b"short");

        let r = check(&a.0, &b.0, false);
        assert_eq!(r.differences_total, 1);
        assert!(matches!(r.differences[0].1, Difference::SizeDiffers { .. }));
    }

    #[test]
    fn only_a_deep_check_catches_same_size_corruption() {
        // This is the case the whole deep mode exists for: identical length
        // and timestamp, different bytes. Bit rot looks exactly like this.
        let a = Tmp::new("rot_a");
        let b = Tmp::new("rot_b");
        a.write("f.bin", b"AAAAAAAAAA");
        b.write("f.bin", b"AAAAAAAAAB");
        // `write` already pinned both to the same mtime, so the shallow check
        // has nothing to distinguish them by — which is the whole point here.
        let shallow = check(&a.0, &b.0, false);
        assert!(shallow.matched(), "shallow cannot see this, by design");

        let deep = check(&a.0, &b.0, true);
        assert!(!deep.matched());
        assert_eq!(deep.differences_total, 1);
        assert_eq!(deep.differences[0].1, Difference::ContentDiffers);
    }

    #[test]
    fn deep_verification_counts_the_bytes_it_read() {
        let a = Tmp::new("bytes_a");
        let b = Tmp::new("bytes_b");
        for t in [&a, &b] {
            t.write("big.bin", &vec![4u8; 100_000]);
        }
        let r = check(&a.0, &b.0, true);
        assert!(r.matched());
        assert_eq!(r.bytes_checked, 100_000);
    }

    #[test]
    fn cancellation_is_reported_rather_than_passing_silently() {
        // A cancelled verification must never look like a clean bill of health.
        let a = Tmp::new("cancel_a");
        let b = Tmp::new("cancel_b");
        for t in [&a, &b] {
            t.write("f.bin", b"data");
        }
        let cancel = Cancel::new();
        cancel.cancel();
        let r = compare(
            &a.0,
            &b.0,
            &Filter::permissive(),
            &ScanOptions::default(),
            true,
            2,
            &cancel,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(r.cancelled);
        assert!(!r.matched(), "cancelled must not report a match");
    }

    #[test]
    fn many_differences_are_counted_even_when_not_all_listed() {
        let a = Tmp::new("many_a");
        let b = Tmp::new("many_b");
        for i in 0..50 {
            a.write(&format!("f{i}.txt"), b"source");
        }
        let r = check(&a.0, &b.0, false);
        assert_eq!(r.differences_total, 50);
        assert_eq!(r.files_checked, 50);
        assert_eq!(r.files_matching, 0);
    }
}
