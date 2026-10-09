//! Path handling for the file mirror.
//!
//! Two problems this module exists to solve:
//!
//! 1. **MAX_PATH.** Windows APIs default to a 260-character limit. A backup
//!    tool that dies on a deeply nested folder is worthless, so every path we
//!    hand to a Win32 call goes through [`long`], which applies the `\\?\`
//!    verbatim prefix. That opts out of the limit (up to ~32767 chars) on every
//!    Windows 10/11 build, with no registry change and no manifest setting.
//!    Verbatim paths disable `.`/`..` resolution, so we normalize first.
//!
//! 2. **Identity across two trees.** A mirror is a correspondence between
//!    `<source>/<rel>` and `<dest>/<rel>`. The relative path is the identity
//!    key, and on Windows that comparison has to be case-insensitive
//!    (`Photos\a.jpg` and `photos\A.JPG` are the same file) while on Linux it
//!    must not be. [`RelPath::key`] encapsulates that so the planner never has
//!    to think about it.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Prefix that opts a Windows path out of the MAX_PATH limit.
#[cfg(windows)]
const VERBATIM: &str = r"\\?\";
/// Verbatim form of a UNC share: `\\server\share` -> `\\?\UNC\server\share`.
#[cfg(windows)]
const VERBATIM_UNC: &str = r"\\?\UNC\";

/// Convert a path into the form we hand to the OS.
///
/// On Windows this applies the `\\?\` verbatim prefix so long paths work. On
/// Unix there is no such limit and the path passes through untouched.
///
/// Idempotent: a path that is already verbatim is returned as-is.
#[cfg(windows)]
pub fn long(p: &Path) -> PathBuf {
    let s = p.as_os_str().to_string_lossy();

    // Already verbatim (`\\?\...`) or a device path (`\\.\...`) — leave alone.
    // Rewriting `\\.\PhysicalDrive0` would break raw-disk access.
    if s.starts_with(VERBATIM) || s.starts_with(r"\\.\") {
        return p.to_path_buf();
    }
    // Verbatim paths bypass normalization, so a relative path cannot be made
    // verbatim without resolving it first. Leave it for the caller.
    if !p.is_absolute() {
        return p.to_path_buf();
    }

    // Passed to the kernel literally: forward slashes are not translated and
    // `.`/`..` are not resolved. Normalize both here.
    let normalized = normalize(p);
    let s = normalized.as_os_str().to_string_lossy();

    if let Some(rest) = s.strip_prefix(r"\\") {
        PathBuf::from(format!("{VERBATIM_UNC}{rest}"))
    } else {
        PathBuf::from(format!("{VERBATIM}{s}"))
    }
}

#[cfg(not(windows))]
pub fn long(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// Resolve `.` and `..` lexically and force `\` separators, without touching
/// the filesystem (unlike `canonicalize`, which requires the path to exist —
/// no good for a destination we are about to create).
#[cfg(windows)]
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop past the prefix/root.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Strip the verbatim prefix so a path can be shown to a human or written to a
/// log. The inverse of [`long`], for display purposes only.
pub fn display(p: &Path) -> String {
    let s = p.as_os_str().to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(rest) = s.strip_prefix(VERBATIM_UNC) {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = s.strip_prefix(VERBATIM) {
            return rest.to_string();
        }
    }
    s.into_owned()
}

/// A relative path used as the shared identity of a file in both trees.
///
/// Stored as `/`-separated text regardless of platform so a state file written
/// on Windows is readable on Linux, and so the key is stable no matter which
/// separator the OS handed us.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelPath(String);

impl RelPath {
    /// The root of the tree itself (empty relative path).
    pub fn root() -> Self {
        RelPath(String::new())
    }

    /// Build from a path relative to a tree root.
    pub fn from_relative(p: &Path) -> Self {
        let mut s = String::new();
        for c in p.components() {
            if let Component::Normal(part) = c {
                if !s.is_empty() {
                    s.push('/');
                }
                s.push_str(&part.to_string_lossy());
            }
        }
        RelPath(s)
    }

    /// Reconstruct from the text form written into a state file.
    pub fn from_key_text(s: &str) -> Self {
        RelPath(s.trim_matches('/').to_string())
    }

    /// Append one path segment (a single directory or file name).
    pub fn join(&self, name: &str) -> Self {
        if self.0.is_empty() {
            RelPath(name.to_string())
        } else {
            RelPath(format!("{}/{}", self.0, name))
        }
    }

    /// Resolve against a tree root to get a real, OS-usable path.
    pub fn under(&self, root: &Path) -> PathBuf {
        if self.0.is_empty() {
            return root.to_path_buf();
        }
        let mut p = root.to_path_buf();
        for part in self.0.split('/') {
            p.push(part);
        }
        p
    }

    /// The final segment (file or directory name), or `""` at the root.
    pub fn name(&self) -> &str {
        match self.0.rsplit_once('/') {
            Some((_, n)) => n,
            None => &self.0,
        }
    }

    /// The containing directory, or `None` at the root.
    pub fn parent(&self) -> Option<RelPath> {
        if self.0.is_empty() {
            return None;
        }
        match self.0.rsplit_once('/') {
            Some((head, _)) => Some(RelPath(head.to_string())),
            None => Some(RelPath::root()),
        }
    }

    /// How deep this path sits below the root (root itself is 0).
    pub fn depth(&self) -> usize {
        if self.0.is_empty() {
            0
        } else {
            self.0.split('/').count()
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The comparison key for matching a source entry to a destination entry.
    ///
    /// Case-insensitive on Windows (NTFS is case-preserving but
    /// case-insensitive, so `A.TXT` and `a.txt` are one file and mirroring them
    /// as two would corrupt the destination). Case-sensitive on Unix, where
    /// they really are two files.
    pub fn key(&self) -> RelKey {
        #[cfg(windows)]
        {
            RelKey(self.0.to_lowercase())
        }
        #[cfg(not(windows))]
        {
            RelKey(self.0.clone())
        }
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            f.write_str(".")
        } else {
            f.write_str(&self.0)
        }
    }
}

/// A [`RelPath`] normalized for platform-correct equality/hashing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelKey(String);

impl RelKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// True if `child` is inside `parent` (or is `parent`), compared the way the
/// platform compares paths.
///
/// This is the guard that stops a job from mirroring a tree into itself — e.g.
/// `D:\` into `D:\Backup`, which would recurse until the drive filled.
pub fn is_within(child: &Path, parent: &Path) -> bool {
    let norm = |p: &Path| -> String {
        let s = display(&long(p)).replace('\\', "/");
        let s = s.trim_end_matches('/').to_string();
        #[cfg(windows)]
        {
            s.to_lowercase()
        }
        #[cfg(not(windows))]
        {
            s
        }
    };
    let c = norm(child);
    let p = norm(parent);
    if c == p {
        return true;
    }
    // Compare on a segment boundary so `D:/Backup2` is not "inside" `D:/Backup`.
    c.starts_with(&format!("{p}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relpath_is_platform_neutral_text() {
        let r = RelPath::root().join("Photos").join("2024").join("a.jpg");
        assert_eq!(r.as_str(), "Photos/2024/a.jpg");
        assert_eq!(r.name(), "a.jpg");
        assert_eq!(r.depth(), 3);
        assert_eq!(r.parent().unwrap().as_str(), "Photos/2024");
    }

    #[test]
    fn root_relpath_resolves_to_the_root_itself() {
        let root = Path::new("X:/data");
        assert_eq!(RelPath::root().under(root), PathBuf::from("X:/data"));
        assert!(RelPath::root().parent().is_none());
        assert_eq!(RelPath::root().depth(), 0);
    }

    #[test]
    fn a_top_level_entry_has_the_root_as_its_parent() {
        let r = RelPath::root().join("a.txt");
        assert_eq!(r.parent(), Some(RelPath::root()));
    }

    #[test]
    fn under_rebuilds_a_real_path() {
        let r = RelPath::root().join("a").join("b.txt");
        let p = r.under(Path::new("D:/src"));
        assert_eq!(p, PathBuf::from("D:/src").join("a").join("b.txt"));
    }

    #[test]
    fn is_within_matches_on_segment_boundaries() {
        assert!(is_within(Path::new("D:/Backup/x"), Path::new("D:/Backup")));
        assert!(is_within(Path::new("D:/Backup"), Path::new("D:/Backup")));
        // The trap this guard exists for: mirroring a drive into itself.
        assert!(is_within(Path::new("D:/Backup"), Path::new("D:/")));
        // ...but a sibling with a shared prefix is NOT inside.
        assert!(!is_within(Path::new("D:/Backup2"), Path::new("D:/Backup")));
        assert!(!is_within(Path::new("E:/Backup"), Path::new("D:/Backup")));
    }

    #[cfg(windows)]
    #[test]
    fn key_is_case_insensitive_on_windows() {
        let a = RelPath::from_relative(Path::new(r"Photos\A.JPG"));
        let b = RelPath::from_relative(Path::new("photos/a.jpg"));
        assert_ne!(a, b, "the display form should preserve case");
        assert_eq!(a.key(), b.key(), "but they are the same file on NTFS");
    }

    #[cfg(windows)]
    #[test]
    fn long_applies_verbatim_prefix_and_round_trips() {
        assert_eq!(long(Path::new(r"D:\data\x")).as_os_str(), r"\\?\D:\data\x");
        assert_eq!(display(&long(Path::new(r"D:\data\x"))), r"D:\data\x");
        assert_eq!(
            long(Path::new(r"\\nas\share\f")).as_os_str(),
            r"\\?\UNC\nas\share\f"
        );
        assert_eq!(display(&long(Path::new(r"\\nas\share\f"))), r"\\nas\share\f");
    }

    #[cfg(windows)]
    #[test]
    fn long_is_idempotent_and_leaves_device_paths_alone() {
        let v = Path::new(r"\\?\D:\x");
        assert_eq!(long(v), v);
        let dev = Path::new(r"\\.\PhysicalDrive0");
        assert_eq!(long(dev), dev);
    }

    #[cfg(windows)]
    #[test]
    fn long_normalizes_because_verbatim_paths_are_not_resolved() {
        // The kernel does not resolve `..` in a verbatim path, so we must.
        assert_eq!(long(Path::new(r"D:\a\..\b")).as_os_str(), r"\\?\D:\b");
        assert_eq!(long(Path::new("D:/a/./b")).as_os_str(), r"\\?\D:\a\b");
        // `..` must never escape the drive root.
        assert_eq!(long(Path::new(r"D:\..\..\x")).as_os_str(), r"\\?\D:\x");
    }
}
