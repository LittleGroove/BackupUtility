//! The metadata model — what we know about a file, and what "the same file"
//! means when comparing the two trees.
//!
//! Everything here is platform-neutral in its *representation* (so a state file
//! written on Windows still parses on Linux) while the *reading* of it is
//! `#[cfg]`-gated. Timestamps are stored as nanoseconds since the Unix epoch,
//! which round-trips an NTFS `FILETIME` exactly: FILETIME is 100 ns ticks since
//! 1601, so the conversion is pure integer arithmetic with no precision loss
//! for any date this tool will ever see.

use super::path::RelPath;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// Number of 100 ns ticks between 1601-01-01 (the FILETIME epoch) and
/// 1970-01-01 (the Unix epoch).
const FILETIME_TO_UNIX: i64 = 116_444_736_000_000_000;

/// A file timestamp, as nanoseconds since the Unix epoch.
///
/// Signed so pre-1970 timestamps (which do occur, usually from bad clocks or
/// archive extraction) survive a round trip instead of wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct FileTime {
    pub unix_nanos: i64,
}

impl FileTime {
    pub const ZERO: FileTime = FileTime { unix_nanos: 0 };

    /// From a Windows `FILETIME` already flattened to a u64.
    pub fn from_filetime(ticks: u64) -> Self {
        FileTime {
            unix_nanos: (ticks as i64 - FILETIME_TO_UNIX).saturating_mul(100),
        }
    }

    /// Back to a Windows `FILETIME` tick count. Saturates at 0 rather than
    /// wrapping for timestamps before 1601, which Windows cannot represent.
    pub fn to_filetime(self) -> u64 {
        let ticks = self.unix_nanos / 100 + FILETIME_TO_UNIX;
        if ticks < 0 {
            0
        } else {
            ticks as u64
        }
    }

    pub fn from_unix_secs_nanos(secs: i64, nanos: u32) -> Self {
        FileTime {
            unix_nanos: secs.saturating_mul(1_000_000_000) + nanos as i64,
        }
    }

    pub fn is_zero(self) -> bool {
        self.unix_nanos == 0
    }

    /// Absolute difference in nanoseconds.
    pub fn diff_nanos(self, other: FileTime) -> u64 {
        self.unix_nanos.abs_diff(other.unix_nanos)
    }

    /// Equal within `tolerance_nanos`.
    ///
    /// A tolerance is needed because filesystems disagree about granularity:
    /// NTFS and ReFS store 100 ns, but exFAT stores 10 ms and FAT32 stores
    /// **2 seconds**. Writing an NTFS timestamp to a FAT destination and
    /// reading it back gives a different number, so an exact comparison would
    /// re-copy the entire tree on every single run.
    pub fn close_to(self, other: FileTime, tolerance_nanos: u64) -> bool {
        self.diff_nanos(other) <= tolerance_nanos
    }
}

impl std::fmt::Display for FileTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.unix_nanos)
    }
}

impl From<std::time::SystemTime> for FileTime {
    fn from(t: std::time::SystemTime) -> Self {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => FileTime {
                unix_nanos: d.as_nanos().min(i64::MAX as u128) as i64,
            },
            // Before the epoch: duration_since fails, so measure backwards.
            Err(e) => FileTime {
                unix_nanos: -(e.duration().as_nanos().min(i64::MAX as u128) as i64),
            },
        }
    }
}

/// Timestamp granularity of a destination filesystem, in nanoseconds. Used as
/// the default comparison tolerance so we do not re-copy a whole tree just
/// because the destination cannot store the source's precision.
pub const TOLERANCE_NTFS: u64 = 0;
pub const TOLERANCE_EXFAT: u64 = 10_000_000; // 10 ms
pub const TOLERANCE_FAT: u64 = 2_000_000_000; // 2 s
/// ext2, ext3, HFS+ and friends store whole seconds.
pub const TOLERANCE_SECOND: u64 = 1_000_000_000;
/// A forgiving default for filesystems we could not identify.
pub const TOLERANCE_UNKNOWN: u64 = 2_000_000_000;

// ---------------------------------------------------------------------------
// Entry kind
// ---------------------------------------------------------------------------

/// What sort of thing an entry is.
///
/// The distinction that matters most for a backup: a **link** is re-created as
/// a link (we never follow it and copy the target, which would both duplicate
/// data and risk an infinite loop through a self-referential junction), while
/// everything else is copied by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    /// A symbolic link, to a file or a directory. Re-created with
    /// `CreateSymbolicLinkW` / `symlink`.
    Symlink,
    /// An NTFS junction (directory mount point). Windows-only; re-created by
    /// writing a reparse point.
    Junction,
}

impl EntryKind {
    pub fn is_dir(self) -> bool {
        matches!(self, EntryKind::Dir)
    }
    /// True for things we re-create rather than copy the contents of.
    pub fn is_link(self) -> bool {
        matches!(self, EntryKind::Symlink | EntryKind::Junction)
    }
}

// ---------------------------------------------------------------------------
// Windows file attributes we care about
// ---------------------------------------------------------------------------

pub mod attr {
    pub const READONLY: u32 = 0x0000_0001;
    pub const HIDDEN: u32 = 0x0000_0002;
    pub const SYSTEM: u32 = 0x0000_0004;
    pub const DIRECTORY: u32 = 0x0000_0010;
    pub const ARCHIVE: u32 = 0x0000_0020;
    pub const NORMAL: u32 = 0x0000_0080;
    pub const TEMPORARY: u32 = 0x0000_0100;
    pub const SPARSE_FILE: u32 = 0x0000_0200;
    pub const REPARSE_POINT: u32 = 0x0000_0400;
    pub const COMPRESSED: u32 = 0x0000_0800;
    pub const OFFLINE: u32 = 0x0000_1000;
    pub const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
    pub const ENCRYPTED: u32 = 0x0000_4000;
    /// The file's data lives in the cloud and touching it triggers a download
    /// (OneDrive / Files On-Demand placeholders).
    pub const RECALL_ON_OPEN: u32 = 0x0004_0000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    /// Attributes it makes sense to copy from source to destination. Excludes
    /// state the destination filesystem owns itself (`DIRECTORY`,
    /// `REPARSE_POINT`, `COMPRESSED`, `SPARSE_FILE`, `ENCRYPTED`), which are
    /// set by *doing* the corresponding thing, not by `SetFileAttributes`.
    pub const MIRRORED: u32 =
        READONLY | HIDDEN | SYSTEM | ARCHIVE | TEMPORARY | NOT_CONTENT_INDEXED | OFFLINE;
}

/// Windows reparse tags we recognize.
pub mod tag {
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    pub const SYMLINK: u32 = 0xA000_000C;
    /// Data Deduplication — the file reads normally through the filter driver.
    pub const DEDUP: u32 = 0x8000_0013;
    /// Windows Overlay Filter (compressed system files) — also reads normally.
    pub const WOF: u32 = 0x8000_0017;
    pub const APPEXECLINK: u32 = 0x8000_001B;
    /// OneDrive and other cloud-sync placeholders.
    pub const CLOUD: u32 = 0x9000_001A;

    /// True if this tag means "a link we should re-create rather than follow".
    ///
    /// Critically this is *not* the same as "has a reparse point". Dedup, WOF
    /// and cloud-sync files all carry reparse tags but are ordinary files as
    /// far as reading is concerned — their filter driver materializes the data
    /// transparently. Treating them as links would silently back up a stub
    /// instead of the user's actual data, which is the worst possible failure
    /// mode for a backup tool.
    pub fn is_link(tag: u32) -> bool {
        matches!(tag, MOUNT_POINT | SYMLINK)
    }

    /// True if reading this file's data will pull it down from the cloud.
    pub fn is_cloud_placeholder(tag: u32) -> bool {
        // The cloud tags occupy a range; match the family, not one value.
        tag & 0xFFFF_0000 == 0x9000_0000
    }
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

/// A file's identity on disk, used to detect hard links.
///
/// Two paths with the same [`FileId`] are the *same* file with two names.
/// Copying both would turn one file into two and inflate the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId {
    pub volume: u64,
    pub index: u64,
}

/// One entry in a tree, as produced by the scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub rel: RelPath,
    pub kind: EntryKind,
    /// Logical size in bytes. Zero for directories and links.
    pub size: u64,
    pub modified: FileTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<FileTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed: Option<FileTime>,
    /// Windows attribute bits. Zero on Unix.
    #[serde(default)]
    pub attrs: u32,
    /// Windows reparse tag, or 0.
    #[serde(default)]
    pub reparse_tag: u32,
    /// Unix mode bits. Zero on Windows.
    #[serde(default)]
    pub mode: u32,
    /// Number of hard links to this file, when known. `1` for ordinary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nlink: Option<u64>,
    /// Identity used to pair up hard links. Only populated when hard-link
    /// preservation is enabled, because obtaining it costs an extra open().
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_id: Option<FileId>,
    /// Where a symlink/junction points. `None` for everything else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

impl Entry {
    /// A directory entry with no interesting metadata — used for the synthetic
    /// parents the planner creates and in tests.
    pub fn dir(rel: RelPath) -> Self {
        Entry {
            rel,
            kind: EntryKind::Dir,
            size: 0,
            modified: FileTime::ZERO,
            created: None,
            accessed: None,
            attrs: attr::DIRECTORY,
            reparse_tag: 0,
            mode: 0,
            nlink: None,
            file_id: None,
            link_target: None,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind.is_dir()
    }

    pub fn is_link(&self) -> bool {
        self.kind.is_link()
    }

    pub fn is_sparse(&self) -> bool {
        self.attrs & attr::SPARSE_FILE != 0
    }

    /// True if reading this file would trigger a cloud download.
    pub fn is_cloud_placeholder(&self) -> bool {
        self.attrs & (attr::RECALL_ON_OPEN | attr::RECALL_ON_DATA_ACCESS) != 0
            || tag::is_cloud_placeholder(self.reparse_tag)
    }

    /// Might this file have more than one hard link?
    pub fn maybe_hardlink(&self) -> bool {
        self.nlink.is_some_and(|n| n > 1)
    }

    /// The quick "has this changed?" test: same size and same mtime.
    ///
    /// This is what every practical mirroring tool uses, because hashing 8 TB
    /// to discover that nothing changed defeats the purpose. It is not
    /// airtight — a file rewritten with identical length and a restored mtime
    /// slips through — so the job can escalate to content verification when
    /// that risk matters.
    pub fn looks_unchanged_from(&self, dest: &Entry, tolerance_nanos: u64) -> bool {
        self.size == dest.size && self.modified.close_to(dest.modified, tolerance_nanos)
    }

    /// Do the mirrored attribute bits differ? Cheap to fix (one syscall) and
    /// worth fixing even when the contents match.
    pub fn attrs_differ_from(&self, dest: &Entry) -> bool {
        (self.attrs & attr::MIRRORED) != (dest.attrs & attr::MIRRORED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_round_trips_through_the_windows_epoch() {
        // A known pair: 1970-01-01T00:00:00Z is exactly FILETIME_TO_UNIX ticks.
        let epoch = FileTime::from_filetime(FILETIME_TO_UNIX as u64);
        assert_eq!(epoch.unix_nanos, 0);
        assert_eq!(epoch.to_filetime(), FILETIME_TO_UNIX as u64);

        // An arbitrary modern timestamp must survive exactly — NTFS's 100 ns
        // resolution is the limit, and we store nanoseconds, so nothing is lost.
        let ticks = 133_500_000_123_456_789u64;
        assert_eq!(FileTime::from_filetime(ticks).to_filetime(), ticks);
    }

    #[test]
    fn pre_1601_timestamps_saturate_instead_of_wrapping() {
        let ancient = FileTime {
            unix_nanos: -20_000_000_000_000_000_000i128 as i64,
        };
        // Whatever we do, we must not produce a garbage huge u64.
        assert!(ancient.to_filetime() < FILETIME_TO_UNIX as u64);
    }

    #[test]
    fn tolerance_absorbs_fat_granularity() {
        let a = FileTime::from_unix_secs_nanos(1_700_000_000, 0);
        let b = FileTime::from_unix_secs_nanos(1_700_000_001, 500_000_000);
        // 1.5s apart: identical to FAT32, different to NTFS.
        assert!(!a.close_to(b, TOLERANCE_NTFS));
        assert!(a.close_to(b, TOLERANCE_FAT));
        assert!(!a.close_to(b, TOLERANCE_EXFAT));
    }

    #[test]
    fn dedup_and_cloud_tags_are_not_links() {
        // The important one: these are reparse points but NOT links. Getting
        // this wrong backs up a stub instead of the file.
        assert!(!tag::is_link(tag::DEDUP));
        assert!(!tag::is_link(tag::WOF));
        assert!(!tag::is_link(tag::CLOUD));
        assert!(tag::is_link(tag::MOUNT_POINT));
        assert!(tag::is_link(tag::SYMLINK));
    }

    #[test]
    fn cloud_placeholders_are_detected_by_tag_family() {
        assert!(tag::is_cloud_placeholder(tag::CLOUD));
        assert!(tag::is_cloud_placeholder(0x9000_0027));
        assert!(!tag::is_cloud_placeholder(tag::SYMLINK));
    }

    #[test]
    fn unchanged_detection_uses_size_and_mtime() {
        let mut src = Entry::dir(RelPath::root().join("f"));
        src.kind = EntryKind::File;
        src.size = 100;
        src.modified = FileTime::from_unix_secs_nanos(1_700_000_000, 0);

        let mut dst = src.clone();
        assert!(src.looks_unchanged_from(&dst, TOLERANCE_NTFS));

        dst.size = 101;
        assert!(!src.looks_unchanged_from(&dst, TOLERANCE_NTFS));

        dst.size = 100;
        dst.modified = FileTime::from_unix_secs_nanos(1_700_000_060, 0);
        assert!(!src.looks_unchanged_from(&dst, TOLERANCE_NTFS));
    }

    #[test]
    fn mirrored_attribute_mask_excludes_filesystem_owned_bits() {
        // Setting these via SetFileAttributes is meaningless or harmful; they
        // are consequences of how the file was created.
        assert_eq!(attr::MIRRORED & attr::DIRECTORY, 0);
        assert_eq!(attr::MIRRORED & attr::REPARSE_POINT, 0);
        assert_eq!(attr::MIRRORED & attr::COMPRESSED, 0);
        assert_eq!(attr::MIRRORED & attr::SPARSE_FILE, 0);
        assert_eq!(attr::MIRRORED & attr::ENCRYPTED, 0);
        // But the ones a user actually notices are included.
        assert_ne!(attr::MIRRORED & attr::READONLY, 0);
        assert_ne!(attr::MIRRORED & attr::HIDDEN, 0);
        assert_ne!(attr::MIRRORED & attr::SYSTEM, 0);
    }
}
