//! Keeping the attic from eating the drive.
//!
//! In [`MirrorMode::Attic`](super::plan::MirrorMode::Attic) every run that
//! displaces files leaves a timestamped folder behind. That is the whole point
//! — it is the safety net that makes "delete" reversible — but nothing ever
//! removed one, so on a spare drive that is slowly filling, the attic is
//! eventually what fills it.
//!
//! ## What this will and will not delete
//! Only direct children of the attic root whose names parse as a
//! [`timefmt::stamp`](crate::timefmt::stamp) — that is, only folders this tool
//! created. A stray directory a user dropped in there is left alone, and so is
//! anything whose name does not parse, because a name we cannot read is a name
//! we cannot date, and pruning on a guess is how you delete the wrong backup.
//!
//! The newest folder is never pruned, whatever the policy says. A policy that
//! could empty the attic completely would silently discard the one copy of a
//! file the source had just lost.

use super::copy;
use super::path;
use crate::timefmt;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// When old attic folders are removed.
///
/// The default is 10 runs and no age or size limit: enough history to undo a
/// mistake several backups later, without growing forever. Set every field to
/// `None` to keep everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtticPolicy {
    /// Keep at most this many attic folders.
    #[serde(default = "default_keep")]
    pub keep_runs: Option<usize>,
    /// Delete folders older than this many days.
    #[serde(default)]
    pub max_age_days: Option<u64>,
    /// Delete oldest-first until the attic is under this size.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

fn default_keep() -> Option<usize> {
    Some(10)
}

impl Default for AtticPolicy {
    fn default() -> Self {
        AtticPolicy {
            keep_runs: default_keep(),
            max_age_days: None,
            max_bytes: None,
        }
    }
}

impl AtticPolicy {
    /// Never delete anything.
    pub fn keep_everything() -> Self {
        AtticPolicy {
            keep_runs: None,
            max_age_days: None,
            max_bytes: None,
        }
    }

    pub fn prunes_anything(&self) -> bool {
        self.keep_runs.is_some() || self.max_age_days.is_some() || self.max_bytes.is_some()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = self.keep_runs {
            parts.push(format!("keep {n} runs"));
        }
        if let Some(d) = self.max_age_days {
            parts.push(format!("drop after {d} days"));
        }
        if let Some(b) = self.max_bytes {
            parts.push(format!("cap at {}", super::plan::human_bytes(b)));
        }
        if parts.is_empty() {
            "keep everything".into()
        } else {
            parts.join(", ")
        }
    }
}

/// One dated folder in the attic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtticEntry {
    pub path: PathBuf,
    pub stamp: String,
    pub created_unix: u64,
    pub bytes: u64,
}

/// What a prune did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub folders_before: usize,
    pub folders_removed: usize,
    pub bytes_removed: u64,
    pub bytes_remaining: u64,
    /// Names removed, for the run log.
    pub removed: Vec<String>,
}

impl PruneReport {
    pub fn describe(&self) -> String {
        if self.folders_removed == 0 {
            return format!(
                "  attic: {} folders, {} — nothing to prune\n",
                self.folders_before,
                super::plan::human_bytes(self.bytes_remaining)
            );
        }
        format!(
            "  attic: pruned {} of {} folders, freed {} ({} remaining)\n    removed: {}\n",
            self.folders_removed,
            self.folders_before,
            super::plan::human_bytes(self.bytes_removed),
            super::plan::human_bytes(self.bytes_remaining),
            self.removed.join(", ")
        )
    }
}

/// List the dated folders in an attic root, oldest first.
///
/// `measure` controls whether each folder's size is computed, which means
/// walking it — worth skipping when the policy has no size limit.
pub fn list(attic_root: &Path, measure: bool) -> Result<Vec<AtticEntry>> {
    if !attic_root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path::long(attic_root))?.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Only folders we created. Anything else is someone else's data.
        let Some(created) = timefmt::parse_stamp(&name) else {
            continue;
        };
        out.push(AtticEntry {
            bytes: if measure { dir_size(&entry.path()) } else { 0 },
            path: entry.path(),
            stamp: name,
            created_unix: created,
        });
    }
    out.sort_by_key(|e| e.created_unix);
    Ok(out)
}

/// Apply a retention policy to an attic root.
///
/// `now_unix` is passed in rather than read from the clock so the decision is
/// deterministic and testable.
pub fn prune(attic_root: &Path, policy: &AtticPolicy, now_unix: u64) -> Result<PruneReport> {
    let mut report = PruneReport::default();
    if !policy.prunes_anything() {
        return Ok(report);
    }

    let entries = list(attic_root, policy.max_bytes.is_some())?;
    report.folders_before = entries.len();
    report.bytes_remaining = entries.iter().map(|e| e.bytes).sum();
    if entries.is_empty() {
        return Ok(report);
    }

    // Oldest first, and never the last one standing.
    let mut doomed: Vec<usize> = Vec::new();
    let keepable = entries.len().saturating_sub(1);

    if let Some(keep) = policy.keep_runs {
        let excess = entries.len().saturating_sub(keep.max(1));
        for i in 0..excess.min(keepable) {
            doomed.push(i);
        }
    }
    if let Some(days) = policy.max_age_days {
        let cutoff = now_unix.saturating_sub(days.saturating_mul(86_400));
        for (i, e) in entries.iter().enumerate().take(keepable) {
            if e.created_unix < cutoff && !doomed.contains(&i) {
                doomed.push(i);
            }
        }
    }
    if let Some(limit) = policy.max_bytes {
        let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
        for (i, e) in entries.iter().enumerate().take(keepable) {
            if total <= limit {
                break;
            }
            if !doomed.contains(&i) {
                doomed.push(i);
                total = total.saturating_sub(e.bytes);
            }
        }
    }

    doomed.sort_unstable();
    doomed.dedup();
    for i in doomed {
        let e = &entries[i];
        let bytes = if e.bytes > 0 {
            e.bytes
        } else {
            dir_size(&e.path)
        };
        match copy::force_remove(&e.path) {
            Ok(()) => {
                report.folders_removed += 1;
                report.bytes_removed += bytes;
                report.bytes_remaining = report.bytes_remaining.saturating_sub(bytes);
                report.removed.push(e.stamp.clone());
            }
            // A folder we cannot delete (locked, in use) is left for next time.
            Err(_) => continue,
        }
    }
    Ok(report)
}

/// Total size of a directory tree. Iterative, so a deep attic cannot overflow
/// the stack.
fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(path::long(&dir)) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_attic_{n}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        /// Create an attic folder named for `unix`, holding `bytes` of data.
        fn folder(&self, unix: u64, bytes: usize) -> PathBuf {
            let p = self.0.join(timefmt::stamp(unix));
            std::fs::create_dir_all(p.join("sub")).unwrap();
            std::fs::write(p.join("sub").join("f.bin"), vec![7u8; bytes]).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const DAY: u64 = 86_400;
    const NOW: u64 = 1_800_000_000;

    #[test]
    fn keeping_n_runs_removes_the_oldest_first() {
        let t = Tmp::new("keepn");
        for i in 0..5 {
            t.folder(NOW - (5 - i) * DAY, 10);
        }
        let policy = AtticPolicy {
            keep_runs: Some(2),
            ..AtticPolicy::keep_everything()
        };
        let r = prune(&t.0, &policy, NOW).unwrap();

        assert_eq!(r.folders_before, 5);
        assert_eq!(r.folders_removed, 3);
        let left = list(&t.0, false).unwrap();
        assert_eq!(left.len(), 2);
        // The two that survived must be the newest two.
        assert_eq!(left[0].created_unix, NOW - 2 * DAY);
        assert_eq!(left[1].created_unix, NOW - DAY);
    }

    #[test]
    fn the_newest_folder_is_never_pruned() {
        // A policy that could empty the attic would discard the only copy of
        // files the source had just lost.
        let t = Tmp::new("never_empty");
        t.folder(NOW - 400 * DAY, 10);
        let policy = AtticPolicy {
            keep_runs: Some(0),
            max_age_days: Some(1),
            max_bytes: Some(0),
        };
        let r = prune(&t.0, &policy, NOW).unwrap();
        assert_eq!(r.folders_removed, 0);
        assert_eq!(list(&t.0, false).unwrap().len(), 1);
    }

    #[test]
    fn age_limits_drop_only_what_is_actually_old() {
        let t = Tmp::new("age");
        t.folder(NOW - 100 * DAY, 10); // ancient
        t.folder(NOW - 50 * DAY, 10); // old
        t.folder(NOW - 2 * DAY, 10); // recent
        let policy = AtticPolicy {
            keep_runs: None,
            max_age_days: Some(90),
            max_bytes: None,
        };
        let r = prune(&t.0, &policy, NOW).unwrap();
        assert_eq!(r.folders_removed, 1);
        let left = list(&t.0, false).unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|e| e.created_unix > NOW - 90 * DAY));
    }

    #[test]
    fn size_limits_drop_oldest_first_until_under_the_cap() {
        let t = Tmp::new("size");
        for i in 0..4 {
            t.folder(NOW - (4 - i) * DAY, 1000);
        }
        let policy = AtticPolicy {
            keep_runs: None,
            max_age_days: None,
            max_bytes: Some(2500),
        };
        let r = prune(&t.0, &policy, NOW).unwrap();
        assert!(r.folders_removed >= 1, "should have freed something");
        assert!(
            r.bytes_remaining <= 2500 || list(&t.0, false).unwrap().len() == 1,
            "should be under the cap, or down to the one folder we never remove"
        );
    }

    #[test]
    fn folders_we_did_not_create_are_left_alone() {
        // The dangerous case: a user's own directory in the attic root must
        // not be dated by guesswork and deleted.
        let t = Tmp::new("foreign");
        t.folder(NOW - 100 * DAY, 10);
        t.folder(NOW - 99 * DAY, 10);
        let mine = t.0.join("important-do-not-delete");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("keep.txt"), b"mine").unwrap();

        let policy = AtticPolicy {
            keep_runs: Some(1),
            max_age_days: Some(1),
            max_bytes: None,
        };
        prune(&t.0, &policy, NOW).unwrap();

        assert!(
            mine.exists(),
            "an unparseable folder name must be untouched"
        );
        assert_eq!(std::fs::read(mine.join("keep.txt")).unwrap(), b"mine");
    }

    #[test]
    fn a_policy_that_prunes_nothing_does_nothing() {
        let t = Tmp::new("noop");
        for i in 0..5 {
            t.folder(NOW - (5 - i) * DAY, 10);
        }
        let r = prune(&t.0, &AtticPolicy::keep_everything(), NOW).unwrap();
        assert_eq!(r.folders_removed, 0);
        assert_eq!(list(&t.0, false).unwrap().len(), 5);
    }

    #[test]
    fn an_absent_attic_is_not_an_error() {
        let t = Tmp::new("absent");
        let missing = t.0.join("never-created");
        assert!(list(&missing, false).unwrap().is_empty());
        let r = prune(&missing, &AtticPolicy::default(), NOW).unwrap();
        assert_eq!(r.folders_before, 0);
    }

    #[test]
    fn the_default_policy_bounds_growth_without_being_aggressive() {
        let p = AtticPolicy::default();
        assert_eq!(p.keep_runs, Some(10));
        assert!(
            p.prunes_anything(),
            "an unbounded attic is what fills the drive"
        );
        assert!(!AtticPolicy::keep_everything().prunes_anything());
        assert!(p.describe().contains("10 runs"));
    }

    #[test]
    fn sizes_are_measured_across_the_whole_tree() {
        let t = Tmp::new("measure");
        t.folder(NOW, 4096);
        let entries = list(&t.0, true).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].bytes, 4096, "should include nested files");
    }
}
