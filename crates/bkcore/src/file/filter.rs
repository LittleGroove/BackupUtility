//! Which entries a job actually looks at.
//!
//! When the source is a drive root, exclusions are not a nicety — they are the
//! difference between a backup that finishes and one that spends a day copying
//! `pagefile.sys`, `System Volume Information`, and a recycle bin. So the
//! defaults here already exclude the things Windows will not let you read (or
//! that are meaningless on another volume), and jobs layer their own patterns
//! on top.
//!
//! ## Pattern syntax
//! Deliberately gitignore-shaped, because that is the syntax people already
//! know:
//!
//! | Pattern | Meaning |
//! |---|---|
//! | `*.tmp` | no `/`, so it matches that **name at any depth** |
//! | `build/*.log` | contains `/`, so it is **anchored to the tree root** |
//! | `**` | matches any number of path segments |
//! | `cache/` | trailing `/` — matches **directories only** |
//! | `?` | exactly one character, within a segment |
//! | `[a-z]`, `[!0-9]` | a character class, within a segment |
//!
//! A single `*` never crosses a `/`; use `**` for that. Matching is
//! case-insensitive on Windows and case-sensitive elsewhere, matching how the
//! filesystem itself behaves.

use super::meta::Entry;
use super::path::RelPath;
use serde::{Deserialize, Serialize};

/// Paths on a Windows system volume that a mirror should never try to copy.
///
/// These are either locked by the kernel (so reading them fails), or per-volume
/// state that is actively harmful to place on another disk.
pub const WINDOWS_SYSTEM_EXCLUDES: &[&str] = &[
    "pagefile.sys",
    "swapfile.sys",
    "hiberfil.sys",
    "DumpStack.log",
    "DumpStack.log.tmp",
    "System Volume Information/",
    "$RECYCLE.BIN/",
    "$Recycle.Bin/",
    "Recovery/",
    "Config.Msi/",
    "$WinREAgent/",
    "Windows/CSC/",
    "Windows/Temp/",
    "Windows/memory.dmp",
];

/// Paths a Linux mirror should not descend into: kernel-synthesized filesystems
/// that are not real data and in some cases are infinite.
pub const LINUX_SYSTEM_EXCLUDES: &[&str] = &[
    "proc/",
    "sys/",
    "dev/",
    "run/",
    "tmp/",
    "lost+found/",
    "var/cache/",
];

/// Our own bookkeeping directories, which must never be mirrored back into a
/// tree — doing so would nest state inside state on every run.
pub const SELF_EXCLUDES: &[&str] = &[".bku-state/", ".bku-attic/"];

/// What a job includes and excludes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Filter {
    /// If non-empty, an entry must match one of these to be considered at all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Entries matching any of these are skipped (and directories are not
    /// descended into).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Skip files larger than this many bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    /// Skip files smaller than this many bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<u64>,
    /// Skip files whose data lives in the cloud (OneDrive Files On-Demand).
    ///
    /// Default **on**: reading a placeholder forces a download, so a mirror of
    /// a OneDrive folder would otherwise silently pull terabytes over the
    /// network and fill the local disk.
    #[serde(default = "yes")]
    pub skip_cloud_placeholders: bool,
    /// Apply the built-in system exclusions for the running platform.
    #[serde(default = "yes")]
    pub use_system_excludes: bool,
}

fn yes() -> bool {
    true
}

/// Why an entry was skipped — surfaced in the run report so a user can tell
/// "excluded on purpose" from "failed to copy".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    NotIncluded,
    Excluded,
    SystemPath,
    TooLarge,
    TooSmall,
    CloudPlaceholder,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SkipReason::NotIncluded => "not matched by any include pattern",
            SkipReason::Excluded => "matched an exclude pattern",
            SkipReason::SystemPath => "system path",
            SkipReason::TooLarge => "larger than max-size",
            SkipReason::TooSmall => "smaller than min-size",
            SkipReason::CloudPlaceholder => "cloud placeholder (would trigger download)",
        })
    }
}

impl Filter {
    /// A filter that accepts everything except the built-in system paths.
    pub fn permissive() -> Self {
        Filter {
            skip_cloud_placeholders: true,
            use_system_excludes: true,
            ..Default::default()
        }
    }

    /// Decide whether to process `entry`. `None` means "keep it".
    pub fn check(&self, entry: &Entry) -> Option<SkipReason> {
        let path = entry.rel.as_str();
        let is_dir = entry.is_dir();

        if self.use_system_excludes && matches_any(system_excludes(), path, is_dir) {
            return Some(SkipReason::SystemPath);
        }
        // Our own state/attic directories are excluded unconditionally: they
        // are not user data and mirroring them nests state inside state.
        if matches_any(SELF_EXCLUDES, path, is_dir) {
            return Some(SkipReason::SystemPath);
        }
        if matches_any_owned(&self.exclude, path, is_dir) {
            return Some(SkipReason::Excluded);
        }

        // Directories are never rejected by include/size rules: a wanted file
        // can live inside a directory that does not itself match the pattern,
        // so pruning here would make `include: ["**/*.psd"]` find nothing.
        if is_dir {
            return None;
        }

        if !self.include.is_empty() && !matches_any_owned(&self.include, path, is_dir) {
            return Some(SkipReason::NotIncluded);
        }
        if self.skip_cloud_placeholders && entry.is_cloud_placeholder() {
            return Some(SkipReason::CloudPlaceholder);
        }
        if let Some(max) = self.max_size {
            if entry.size > max {
                return Some(SkipReason::TooLarge);
            }
        }
        if let Some(min) = self.min_size {
            if entry.size < min {
                return Some(SkipReason::TooSmall);
            }
        }
        None
    }

    /// Should the scanner descend into this directory?
    ///
    /// Only exclusion rules prune the walk; see [`Filter::check`] for why
    /// include rules must not.
    pub fn should_descend(&self, rel: &RelPath) -> bool {
        let path = rel.as_str();
        if rel.is_root() {
            return true;
        }
        if self.use_system_excludes && matches_any(system_excludes(), path, true) {
            return false;
        }
        if matches_any(SELF_EXCLUDES, path, true) {
            return false;
        }
        !matches_any_owned(&self.exclude, path, true)
    }
}

fn system_excludes() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        WINDOWS_SYSTEM_EXCLUDES
    }
    #[cfg(not(windows))]
    {
        LINUX_SYSTEM_EXCLUDES
    }
}

fn matches_any(pats: &[&str], path: &str, is_dir: bool) -> bool {
    pats.iter().any(|p| matches(p, path, is_dir))
}

fn matches_any_owned(pats: &[String], path: &str, is_dir: bool) -> bool {
    pats.iter().any(|p| matches(p, path, is_dir))
}

/// Match one gitignore-style pattern against a `/`-separated relative path.
pub fn matches(pattern: &str, path: &str, is_dir: bool) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }

    // Trailing `/` restricts the pattern to directories.
    let (pattern, dir_only) = match pattern.strip_suffix('/') {
        Some(p) => (p, true),
        None => (pattern, false),
    };
    if dir_only && !is_dir {
        // A file *inside* a dir-only excluded directory still matches, because
        // the path prefix does. That is handled by the segment matcher below,
        // which allows a pattern to match a prefix of the path.
        // But the entry itself being a file means an exact match is not enough.
    }
    let pattern = pattern.trim_start_matches('/');

    let anchored = pattern.contains('/');
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = if path.is_empty() {
        Vec::new()
    } else {
        path.split('/').collect()
    };

    if anchored {
        // Anchored: match from the root. A pattern may match a *prefix* of the
        // path, which is what makes `Windows/Temp/` exclude everything beneath
        // it rather than only the directory entry itself.
        for end in 0..=path_segs.len() {
            if seg_match(&pat_segs, &path_segs[..end]) {
                // Matching a strict prefix means we are inside the matched
                // directory, so dir_only is satisfied regardless of our kind.
                if !dir_only || is_dir || end < path_segs.len() {
                    return true;
                }
            }
        }
        false
    } else {
        // Unanchored: the pattern matches any single segment, at any depth...
        for (i, seg) in path_segs.iter().enumerate() {
            if one_seg_match(pat_segs[0].as_bytes(), seg.as_bytes()) {
                let is_last = i == path_segs.len() - 1;
                // ...and if it matched an interior segment, we are inside it.
                if !dir_only || is_dir || !is_last {
                    return true;
                }
            }
        }
        false
    }
}

/// Match a list of pattern segments against a list of path segments, with `**`
/// free to consume any number of segments.
fn seg_match(pat: &[&str], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // `**` can consume 0..n leading path segments.
            for skip in 0..=path.len() {
                if seg_match(&pat[1..], &path[skip..]) {
                    return true;
                }
            }
            false
        }
        Some(p) => match path.first() {
            None => false,
            Some(s) => {
                one_seg_match(p.as_bytes(), s.as_bytes()) && seg_match(&pat[1..], &path[1..])
            }
        },
    }
}

/// Match one pattern segment against one path segment. `*` and `?` never cross
/// a `/` because neither side contains one by construction.
fn one_seg_match(pat: &[u8], text: &[u8]) -> bool {
    // Iterative backtracking match: O(n*m) worst case, no recursion depth risk.
    let (mut p, mut t) = (0usize, 0usize);
    // Where to resume if the current `*` guess fails.
    let (mut star, mut star_t) = (usize::MAX, 0usize);

    while t < text.len() {
        if p < pat.len() {
            match pat[p] {
                b'*' => {
                    star = p;
                    star_t = t;
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => {
                    if let Some((matched, next_p)) = class_match(pat, p, text[t]) {
                        if matched {
                            p = next_p;
                            t += 1;
                            continue;
                        }
                    } else if eq_char(pat[p], text[t]) {
                        // Unterminated `[` is a literal bracket.
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
                c => {
                    if eq_char(c, text[t]) {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
            }
        }
        // Mismatch: backtrack to the last `*` and let it eat one more char.
        if star != usize::MAX {
            star_t += 1;
            t = star_t;
            p = star + 1;
        } else {
            return false;
        }
    }
    // Trailing `*`s can match the empty remainder.
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Compare one byte, case-insensitively on Windows.
fn eq_char(a: u8, b: u8) -> bool {
    #[cfg(windows)]
    {
        a.eq_ignore_ascii_case(&b)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Evaluate a `[...]` character class starting at `pat[start]` (which is `[`).
/// Returns `(matched, index_after_class)`, or `None` if unterminated.
fn class_match(pat: &[u8], start: usize, ch: u8) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negated = matches!(pat.get(i), Some(b'!') | Some(b'^'));
    if negated {
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < pat.len() {
        // A `]` in the first position is a literal, per glob convention.
        if pat[i] == b']' && !first {
            return Some((matched != negated, i + 1));
        }
        first = false;
        // A range like `a-z`.
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let (lo, hi) = (pat[i], pat[i + 2]);
            if in_range(lo, hi, ch) {
                matched = true;
            }
            i += 3;
        } else {
            if eq_char(pat[i], ch) {
                matched = true;
            }
            i += 1;
        }
    }
    None // unterminated
}

fn in_range(lo: u8, hi: u8, ch: u8) -> bool {
    if (lo..=hi).contains(&ch) {
        return true;
    }
    #[cfg(windows)]
    {
        // Case-insensitively, `[a-z]` should also accept `Q`.
        let alt = if ch.is_ascii_uppercase() {
            ch.to_ascii_lowercase()
        } else if ch.is_ascii_lowercase() {
            ch.to_ascii_uppercase()
        } else {
            return false;
        };
        (lo..=hi).contains(&alt)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::meta::{attr, EntryKind};

    fn file(rel: &str) -> Entry {
        let mut e = Entry::dir(RelPath::from_key_text(rel));
        e.kind = EntryKind::File;
        e.attrs = 0;
        e
    }
    fn dir(rel: &str) -> Entry {
        Entry::dir(RelPath::from_key_text(rel))
    }

    #[test]
    fn unanchored_pattern_matches_at_any_depth() {
        assert!(matches("*.tmp", "a/b/c.tmp", false));
        assert!(matches("*.tmp", "c.tmp", false));
        assert!(!matches("*.tmp", "a/b/c.txt", false));
    }

    #[test]
    fn anchored_pattern_matches_only_from_the_root() {
        assert!(matches("build/*.log", "build/out.log", false));
        assert!(!matches("build/*.log", "src/build/out.log", false));
    }

    #[test]
    fn single_star_does_not_cross_a_separator() {
        assert!(!matches("build/*.log", "build/deep/out.log", false));
        assert!(matches("build/**/*.log", "build/deep/out.log", false));
        assert!(matches("build/**/*.log", "build/a/b/c/out.log", false));
    }

    #[test]
    fn doublestar_matches_zero_segments() {
        assert!(matches("build/**/*.log", "build/out.log", false));
    }

    #[test]
    fn trailing_slash_restricts_to_directories_but_covers_their_contents() {
        assert!(matches("cache/", "cache", true));
        assert!(!matches("cache/", "cache", false), "a *file* named cache");
        // Everything under it is excluded, which is the point.
        assert!(matches("cache/", "cache/x.dat", false));
        assert!(matches("Windows/Temp/", "Windows/Temp/a/b.tmp", false));
    }

    #[test]
    fn question_mark_and_char_classes() {
        assert!(matches("a?c.txt", "abc.txt", false));
        assert!(!matches("a?c.txt", "ac.txt", false));
        assert!(matches("file[0-9].dat", "file7.dat", false));
        assert!(!matches("file[0-9].dat", "filex.dat", false));
        assert!(matches("file[!0-9].dat", "filex.dat", false));
        assert!(!matches("file[!0-9].dat", "file7.dat", false));
    }

    #[test]
    fn star_backtracking_handles_repeated_wildcards() {
        assert!(matches("*a*b*c", "xxaxxbxxc", false));
        assert!(!matches("*a*b*c", "xxaxxbxx", false));
        assert!(matches("*", "anything", false));
        assert!(matches("**", "a/b/c", false));
    }

    #[test]
    fn system_paths_are_excluded_by_default() {
        let f = Filter::permissive();
        #[cfg(windows)]
        {
            assert_eq!(f.check(&file("pagefile.sys")), Some(SkipReason::SystemPath));
            assert_eq!(
                f.check(&file("System Volume Information/tracking.log")),
                Some(SkipReason::SystemPath)
            );
            assert!(!f.should_descend(&RelPath::from_key_text("$RECYCLE.BIN")));
        }
        // Our own bookkeeping is excluded on every platform.
        assert_eq!(
            f.check(&file(".bku-state/plan.jsonl")),
            Some(SkipReason::SystemPath)
        );
        assert!(!f.should_descend(&RelPath::from_key_text(".bku-attic")));
        // Ordinary data is kept.
        assert_eq!(f.check(&file("Photos/2024/a.jpg")), None);
    }

    #[test]
    fn include_rules_never_prune_directories() {
        // The bug this guards: pruning `Photos/` because the *directory* does
        // not match `**/*.psd` would make the include pattern find nothing.
        let f = Filter {
            include: vec!["**/*.psd".into()],
            ..Filter::permissive()
        };
        assert_eq!(f.check(&dir("Photos")), None);
        assert!(f.should_descend(&RelPath::from_key_text("Photos")));
        assert_eq!(f.check(&file("Photos/a.psd")), None);
        assert_eq!(
            f.check(&file("Photos/a.jpg")),
            Some(SkipReason::NotIncluded)
        );
    }

    #[test]
    fn exclude_rules_do_prune_directories() {
        let f = Filter {
            exclude: vec!["node_modules/".into()],
            ..Filter::permissive()
        };
        assert!(!f.should_descend(&RelPath::from_key_text("app/node_modules")));
        assert_eq!(
            f.check(&file("app/node_modules/x/index.js")),
            Some(SkipReason::Excluded)
        );
    }

    #[test]
    fn size_bounds_apply_to_files_only() {
        let f = Filter {
            max_size: Some(1000),
            ..Filter::permissive()
        };
        let mut big = file("big.iso");
        big.size = 5000;
        assert_eq!(f.check(&big), Some(SkipReason::TooLarge));
        assert_eq!(f.check(&dir("somedir")), None);
    }

    #[test]
    fn cloud_placeholders_are_skipped_by_default() {
        let f = Filter::permissive();
        let mut ph = file("OneDrive/big.mkv");
        ph.attrs = attr::RECALL_ON_DATA_ACCESS;
        assert_eq!(f.check(&ph), Some(SkipReason::CloudPlaceholder));

        let f2 = Filter {
            skip_cloud_placeholders: false,
            ..Filter::permissive()
        };
        assert_eq!(f2.check(&ph), None);
    }

    #[cfg(windows)]
    #[test]
    fn matching_is_case_insensitive_on_windows() {
        assert!(matches("*.TMP", "a/b/c.tmp", false));
        assert!(matches("Build/*.log", "build/OUT.LOG", false));
        assert!(matches("file[a-z].dat", "fileQ.dat", false));
    }
}
