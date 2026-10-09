//! Scanning a tree.
//!
//! Three properties matter more than speed here:
//!
//! * **It must not recurse.** A user's tree can nest hundreds of levels deep,
//!   and a recursive walker overflows the stack and takes the process down
//!   mid-backup. The walk is an explicit queue.
//! * **It must not follow links by default.** A junction pointing at its own
//!   ancestor is an infinite tree. Even a benign one (`C:\Users\All Users` ->
//!   `C:\ProgramData`) silently duplicates gigabytes. Links are recorded and
//!   re-created, not descended into.
//! * **One unreadable directory must not kill the run.** On a real drive there
//!   is always something locked or ACL-denied. Errors are collected per path
//!   and the scan continues; the run report shows them.

use super::filter::{Filter, SkipReason};
#[cfg(windows)]
use super::meta::tag;
use super::meta::{attr, Entry, EntryKind, FileTime};
use super::path::{self, RelPath};
use super::sys;
use super::Cancel;
use crate::Result;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Knobs for a scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Descend into symlinked/junctioned directories instead of recording them
    /// as links. Off by default — see the module docs.
    pub follow_links: bool,
    /// Resolve hard links so a file with several names is stored once and
    /// re-linked. Costs one extra file open per candidate, so it is opt-in.
    pub detect_hardlinks: bool,
    /// Descend into directories that live on a different volume (Windows
    /// volume mount points, Unix mount points). Off by default: a mirror of
    /// `D:\` should not wander onto `E:` through a mounted folder.
    pub cross_volume: bool,
    /// Emit a progress callback every this many entries.
    pub progress_every: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            follow_links: false,
            detect_hardlinks: false,
            cross_volume: false,
            progress_every: 2000,
        }
    }
}

/// Running totals, handed to the progress callback and kept in the result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub dirs: u64,
    pub files: u64,
    pub links: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub errors: u64,
}

/// A path the scan could not read, and why.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub rel: RelPath,
    pub message: String,
}

/// The result of scanning one tree.
#[derive(Debug, Clone)]
pub struct Tree {
    pub root: PathBuf,
    /// Entries in breadth-first order, so a parent always precedes its
    /// children. The planner relies on this to create directories before the
    /// files that go in them.
    pub entries: Vec<Entry>,
    pub errors: Vec<ScanError>,
    pub stats: ScanStats,
    /// Entries the filter rejected, with the reason. Capped so a job that
    /// excludes a million files does not blow up memory.
    pub skipped: Vec<(RelPath, SkipReason)>,
}

/// Beyond this many recorded skip reasons we keep counting but stop listing.
const MAX_RECORDED_SKIPS: usize = 10_000;

impl Tree {
    /// Look an entry up by its platform-correct key.
    pub fn index(&self) -> std::collections::HashMap<super::path::RelKey, &Entry> {
        self.entries.iter().map(|e| (e.rel.key(), e)).collect()
    }
}

/// Scan `root`, applying `filter`.
///
/// `progress` is called periodically with the running totals; return `false`
/// from `cancel` to abort. A cancelled scan returns what it found so far
/// rather than an error, because the caller may still want the partial totals.
pub fn scan(
    root: &Path,
    filter: &Filter,
    opts: &ScanOptions,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&ScanStats),
) -> Result<Tree> {
    let root = root.to_path_buf();
    let mut entries: Vec<Entry> = Vec::new();
    let mut errors: Vec<ScanError> = Vec::new();
    let mut skipped: Vec<(RelPath, SkipReason)> = Vec::new();
    let mut stats = ScanStats::default();

    // The volume the root lives on. Used to notice when a directory crosses
    // onto a different volume through a mount point.
    let root_volume = if opts.cross_volume {
        None
    } else {
        sys::file_identity(&root).ok().map(|(id, _)| id.volume)
    };

    let mut queue: VecDeque<RelPath> = VecDeque::new();
    queue.push_back(RelPath::root());
    let mut seen_since_progress = 0u64;

    while let Some(dir_rel) = queue.pop_front() {
        if cancel.is_cancelled() {
            break;
        }
        let dir_path = dir_rel.under(&root);

        let rd = match std::fs::read_dir(path::long(&dir_path)) {
            Ok(rd) => rd,
            Err(e) => {
                // Locked, denied, or vanished between listing and opening.
                // Record and move on — one bad directory is not a failed run.
                errors.push(ScanError {
                    rel: dir_rel.clone(),
                    message: format!("read_dir: {e}"),
                });
                stats.errors += 1;
                continue;
            }
        };

        for dirent in rd {
            if cancel.is_cancelled() {
                break;
            }
            let dirent = match dirent {
                Ok(d) => d,
                Err(e) => {
                    errors.push(ScanError {
                        rel: dir_rel.clone(),
                        message: format!("dir entry: {e}"),
                    });
                    stats.errors += 1;
                    continue;
                }
            };

            let name = dirent.file_name().to_string_lossy().into_owned();
            let rel = dir_rel.join(&name);
            let full = rel.under(&root);

            // `DirEntry::metadata` describes the link itself, not its target —
            // which is what we want, and is free on Windows (it reuses the
            // data the directory enumeration already returned).
            let md = match dirent.metadata() {
                Ok(m) => m,
                Err(e) => {
                    errors.push(ScanError {
                        rel: rel.clone(),
                        message: format!("metadata: {e}"),
                    });
                    stats.errors += 1;
                    continue;
                }
            };

            let mut entry = match build_entry(rel.clone(), &full, &md, opts) {
                Ok(e) => e,
                Err(e) => {
                    errors.push(ScanError {
                        rel: rel.clone(),
                        message: e.to_string(),
                    });
                    stats.errors += 1;
                    continue;
                }
            };

            if let Some(reason) = filter.check(&entry) {
                stats.skipped += 1;
                if skipped.len() < MAX_RECORDED_SKIPS {
                    skipped.push((rel.clone(), reason));
                }
                continue;
            }

            match entry.kind {
                EntryKind::Dir => {
                    stats.dirs += 1;
                    let same_volume = root_volume.is_none_or(|rv| {
                        sys::file_identity(&full)
                            .map(|(id, _)| id.volume == rv)
                            .unwrap_or(true)
                    });
                    if same_volume && filter.should_descend(&rel) {
                        queue.push_back(rel.clone());
                    } else if !same_volume {
                        // Recorded as a directory but not descended into, so a
                        // mounted volume appears as an empty folder rather
                        // than silently pulling in another disk.
                        entry.attrs |= attr::REPARSE_POINT;
                    }
                }
                EntryKind::Symlink | EntryKind::Junction => {
                    stats.links += 1;
                    if opts.follow_links && md.is_dir() {
                        queue.push_back(rel.clone());
                    }
                }
                EntryKind::File => {
                    stats.files += 1;
                    stats.bytes += entry.size;
                }
            }

            entries.push(entry);
            seen_since_progress += 1;
            if seen_since_progress >= opts.progress_every {
                seen_since_progress = 0;
                progress(&stats);
            }
        }
    }

    progress(&stats);
    Ok(Tree {
        root,
        entries,
        errors,
        stats,
        skipped,
    })
}

/// Turn one directory entry into an [`Entry`], resolving what kind of thing it
/// really is.
fn build_entry(
    rel: RelPath,
    full: &Path,
    md: &std::fs::Metadata,
    opts: &ScanOptions,
) -> Result<Entry> {
    let mut e = Entry::dir(rel);
    e.size = md.len();
    read_platform_meta(&mut e, full, md);

    // Decide the kind. On Windows this cannot be read off `Metadata` alone,
    // because a reparse point may be a link, a dedup stub, a WOF-compressed
    // system file, or a OneDrive placeholder — and only the first is a link.
    e.kind = classify(full, md, &mut e);

    if e.kind.is_link() {
        e.size = 0;
        e.link_target = std::fs::read_link(path::long(full))
            .ok()
            .map(|t| path::display(&t));
    }
    if e.kind.is_dir() {
        e.size = 0;
    }

    // Hard-link identity costs an open, so only pay for it when asked and only
    // for plain files (directories cannot have multiple hard links).
    if opts.detect_hardlinks && e.kind == EntryKind::File {
        if let Ok((id, nlink)) = sys::file_identity(full) {
            e.nlink = Some(nlink);
            if nlink > 1 {
                e.file_id = Some(id);
            }
        }
    }
    Ok(e)
}

#[cfg(windows)]
fn read_platform_meta(e: &mut Entry, _full: &Path, md: &std::fs::Metadata) {
    use std::os::windows::fs::MetadataExt;
    e.attrs = md.file_attributes();
    // These are raw FILETIME ticks, so the conversion is exact.
    e.modified = FileTime::from_filetime(md.last_write_time());
    e.created = Some(FileTime::from_filetime(md.creation_time()));
    e.accessed = Some(FileTime::from_filetime(md.last_access_time()));
}

#[cfg(not(windows))]
fn read_platform_meta(e: &mut Entry, _full: &Path, md: &std::fs::Metadata) {
    use std::os::unix::fs::MetadataExt;
    e.mode = md.mode();
    e.modified = FileTime::from_unix_secs_nanos(md.mtime(), md.mtime_nsec() as u32);
    // Unix has no sparse attribute; a file whose allocated blocks fall short
    // of its length has holes. The same heuristic GNU cp --sparse=auto uses.
    if md.is_file() && md.blocks().saturating_mul(512) < md.len() {
        e.attrs |= attr::SPARSE_FILE;
    }
    e.accessed = Some(FileTime::from_unix_secs_nanos(
        md.atime(),
        md.atime_nsec() as u32,
    ));
    if md.is_dir() {
        e.attrs |= attr::DIRECTORY;
    }
}

#[cfg(windows)]
fn classify(full: &Path, md: &std::fs::Metadata, e: &mut Entry) -> EntryKind {
    if e.attrs & attr::REPARSE_POINT != 0 {
        e.reparse_tag = sys::reparse_tag(full);
        if tag::is_link(e.reparse_tag) {
            return if e.attrs & attr::DIRECTORY != 0 {
                EntryKind::Junction_or_symlink(e.reparse_tag)
            } else {
                EntryKind::Symlink
            };
        }
        // Dedup / WOF / cloud placeholder: reads pass through the filter
        // driver and yield the real data, so treat it as an ordinary file.
    }
    if md.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

#[cfg(not(windows))]
fn classify(_full: &Path, md: &std::fs::Metadata, _e: &mut Entry) -> EntryKind {
    if md.file_type().is_symlink() {
        EntryKind::Symlink
    } else if md.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
    }
}

impl EntryKind {
    /// A directory reparse point is a junction if it carries the mount-point
    /// tag, and a directory symlink otherwise. They are re-created differently,
    /// so the distinction has to survive the scan.
    #[cfg(windows)]
    #[allow(non_snake_case)]
    fn Junction_or_symlink(reparse_tag: u32) -> EntryKind {
        if reparse_tag == tag::MOUNT_POINT {
            EntryKind::Junction
        } else {
            EntryKind::Symlink
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::filter::Filter;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_walk_{name}"));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn file(&self, rel: &str, bytes: &[u8]) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            p
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scan_all(root: &Path, filter: &Filter) -> Tree {
        scan(
            root,
            filter,
            &ScanOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn finds_files_and_dirs_with_correct_relative_paths() {
        let t = Tmp::new("basic");
        t.file("a.txt", b"hello");
        t.file("sub/b.txt", b"world!!");
        t.dir("empty");

        let tree = scan_all(&t.0, &Filter::permissive());
        let mut names: Vec<&str> = tree.entries.iter().map(|e| e.rel.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "empty", "sub", "sub/b.txt"]);

        assert_eq!(tree.stats.files, 2);
        assert_eq!(tree.stats.dirs, 2);
        assert_eq!(tree.stats.bytes, 5 + 7);
    }

    #[test]
    fn parents_precede_children_so_dirs_can_be_created_in_order() {
        let t = Tmp::new("order");
        t.file("x/y/z/deep.txt", b"d");
        let tree = scan_all(&t.0, &Filter::permissive());

        let pos = |s: &str| {
            tree.entries
                .iter()
                .position(|e| e.rel.as_str() == s)
                .unwrap()
        };
        assert!(pos("x") < pos("x/y"));
        assert!(pos("x/y") < pos("x/y/z"));
        assert!(pos("x/y/z") < pos("x/y/z/deep.txt"));
    }

    #[test]
    fn excluded_directories_are_not_descended_into() {
        let t = Tmp::new("exclude");
        t.file("keep/a.txt", b"a");
        t.file("node_modules/deep/b.txt", b"b");

        let filter = Filter {
            exclude: vec!["node_modules/".into()],
            ..Filter::permissive()
        };
        let tree = scan_all(&t.0, &filter);
        assert!(tree
            .entries
            .iter()
            .all(|e| !e.rel.as_str().contains("node_modules")));
        assert!(tree.entries.iter().any(|e| e.rel.as_str() == "keep/a.txt"));
        assert!(tree.stats.skipped >= 1);
    }

    #[test]
    fn a_deeply_nested_tree_does_not_overflow_the_stack() {
        let t = Tmp::new("deep");
        // 300 levels: comfortably past what a naive recursive walker survives
        // in a debug build, and well past MAX_PATH so `\\?\` is exercised too.
        let mut rel = String::new();
        for i in 0..300 {
            rel.push_str(&format!("d{i}/"));
        }
        t.file(&format!("{rel}bottom.txt"), b"deep");

        let tree = scan_all(&t.0, &Filter::permissive());
        assert!(
            tree.entries.iter().any(|e| e.rel.name() == "bottom.txt"),
            "did not reach the bottom; errors: {:?}",
            &tree.errors[..tree.errors.len().min(3)]
        );
        assert_eq!(tree.entries.iter().filter(|e| e.is_dir()).count(), 300);
    }

    #[test]
    fn cancellation_stops_the_walk_without_erroring() {
        let t = Tmp::new("cancel");
        for i in 0..50 {
            t.file(&format!("f{i}.txt"), b"x");
        }
        let cancel = Cancel::new();
        cancel.cancel();
        let tree = scan(
            &t.0,
            &Filter::permissive(),
            &ScanOptions::default(),
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert!(tree.entries.is_empty());
    }

    #[test]
    fn a_missing_root_is_recorded_as_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join("bku_walk_definitely_not_here");
        let _ = std::fs::remove_dir_all(&missing);
        let tree = scan_all(&missing, &Filter::permissive());
        assert!(tree.entries.is_empty());
        assert_eq!(tree.stats.errors, 1);
    }

    #[test]
    fn progress_is_reported() {
        let t = Tmp::new("progress");
        for i in 0..25 {
            t.file(&format!("f{i}.txt"), b"x");
        }
        let mut calls = 0;
        let opts = ScanOptions {
            progress_every: 10,
            ..Default::default()
        };
        scan(
            &t.0,
            &Filter::permissive(),
            &opts,
            &Cancel::none(),
            &mut |_| calls += 1,
        )
        .unwrap();
        assert!(calls >= 2, "expected periodic progress, got {calls}");
    }

    #[cfg(windows)]
    #[test]
    fn hardlinks_are_identified_when_requested() {
        let t = Tmp::new("hardlink");
        let a = t.file("a.txt", b"shared");
        let b = t.0.join("b.txt");
        if std::fs::hard_link(&a, &b).is_err() {
            return; // filesystem does not support it; nothing to assert
        }
        let opts = ScanOptions {
            detect_hardlinks: true,
            ..Default::default()
        };
        let tree = scan(
            &t.0,
            &Filter::permissive(),
            &opts,
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        let ids: Vec<_> = tree.entries.iter().filter_map(|e| e.file_id).collect();
        assert_eq!(ids.len(), 2, "both names should carry an identity");
        assert_eq!(ids[0], ids[1], "and it should be the same identity");
    }
}
