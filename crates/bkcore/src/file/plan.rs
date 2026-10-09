//! Diffing two trees into an ordered list of actions.
//!
//! This module is deliberately pure: it takes two [`Tree`]s and produces a
//! [`Plan`], touching no disk. That makes every interesting decision — what
//! counts as changed, what happens to a file the source no longer has, what
//! order things must happen in — testable in milliseconds instead of only
//! observable by running an 8 TB job.
//!
//! ## Ordering is a correctness property, not a nicety
//! The plan is emitted in phases, and the phases exist for concrete reasons:
//!
//! 1. **Directories, parents first.** You cannot write `a/b/c.txt` before `a/b`
//!    exists.
//! 2. **Content**, with hard-link primaries before the extra names that link
//!    to them.
//! 3. **Removals, deepest first.** A directory will not delete until it is
//!    empty.
//! 4. **Directory metadata, last.** This is the subtle one: writing a file into
//!    a directory updates that directory's modified time. Restoring a
//!    directory's timestamps *before* filling it means the timestamps are
//!    immediately destroyed, and the next run sees every directory as changed.

use super::meta::{Entry, EntryKind};
use super::path::{RelKey, RelPath};
use super::walk::Tree;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What to do about files the destination has and the source does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MirrorMode {
    /// Leave them alone. The destination only ever grows.
    #[default]
    Additive,
    /// Move them into a timestamped attic folder. Reversible.
    Attic,
    /// Delete them, so the destination matches the source exactly.
    Delete,
}

/// How hard we look before deciding a file is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompareBy {
    /// Same size and same modification time. Fast, and what every practical
    /// mirroring tool uses.
    #[default]
    SizeAndTime,
    /// Same size only. Fastest, and appropriate when the source rewrites
    /// timestamps but not contents.
    SizeOnly,
    /// Hash both files. Correct even against a file rewritten in place with a
    /// restored timestamp, but it reads every byte on both drives every run.
    Content,
}

/// The knobs that decide "changed or not".
#[derive(Debug, Clone)]
pub struct ComparePolicy {
    pub compare: CompareBy,
    /// Timestamp slack, from the destination filesystem's granularity.
    pub tolerance_nanos: u64,
    /// Re-apply attributes/timestamps when only those differ.
    pub fix_metadata: bool,
    /// Re-create the destination when the source changed kind (file became a
    /// directory, etc). Without this such an entry is reported as a conflict.
    pub replace_on_kind_change: bool,
}

impl Default for ComparePolicy {
    fn default() -> Self {
        ComparePolicy {
            compare: CompareBy::SizeAndTime,
            tolerance_nanos: super::meta::TOLERANCE_UNKNOWN,
            fix_metadata: true,
            replace_on_kind_change: true,
        }
    }
}

/// One unit of work. Small and cloneable so the executor can hand them to
/// worker threads freely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Action {
    /// Create a directory in the destination.
    CreateDir { rel: RelPath },
    /// Copy a file that the destination does not have.
    CopyFile { rel: RelPath, size: u64 },
    /// Overwrite a file whose content differs.
    UpdateFile { rel: RelPath, size: u64 },
    /// Give an already-copied file a second name, preserving the hard link
    /// rather than storing the bytes twice.
    Hardlink { rel: RelPath, to: RelPath },
    /// Re-create a symlink or junction.
    CreateLink {
        rel: RelPath,
        kind: EntryKind,
        target: String,
    },
    /// Contents already match; only attributes/timestamps need fixing.
    UpdateMeta { rel: RelPath },
    /// Remove a destination file the source no longer has.
    RemoveFile { rel: RelPath },
    /// Remove a destination directory the source no longer has.
    RemoveDir { rel: RelPath },
    /// Move a destination entry into the attic instead of deleting it.
    Attic { rel: RelPath, is_dir: bool },
    /// Apply a directory's timestamps. Always last — see the module docs.
    SetDirMeta { rel: RelPath },
}

impl Action {
    pub fn rel(&self) -> &RelPath {
        match self {
            Action::CreateDir { rel }
            | Action::CopyFile { rel, .. }
            | Action::UpdateFile { rel, .. }
            | Action::Hardlink { rel, .. }
            | Action::CreateLink { rel, .. }
            | Action::UpdateMeta { rel }
            | Action::RemoveFile { rel }
            | Action::RemoveDir { rel }
            | Action::Attic { rel, .. }
            | Action::SetDirMeta { rel } => rel,
        }
    }

    /// Bytes this action will transfer. Used for progress and ETA.
    pub fn bytes(&self) -> u64 {
        match self {
            Action::CopyFile { size, .. } | Action::UpdateFile { size, .. } => *size,
            _ => 0,
        }
    }

    /// True if this action removes data from the destination. The dry-run
    /// summary calls these out separately, because they are the only ones a
    /// user can be genuinely surprised by.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            Action::RemoveFile { .. } | Action::RemoveDir { .. } | Action::UpdateFile { .. }
        )
    }
}

/// A source entry that could not be planned, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub rel: RelPath,
    pub reason: String,
}

/// Totals for the run report and the progress bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanStats {
    pub dirs_to_create: u64,
    pub files_to_copy: u64,
    pub files_to_update: u64,
    pub links_to_create: u64,
    pub hardlinks: u64,
    pub meta_to_fix: u64,
    pub to_remove: u64,
    pub to_attic: u64,
    /// Files already identical in both trees — the work we avoided.
    pub unchanged: u64,
    /// Bytes that will actually be transferred.
    pub bytes_to_copy: u64,
    /// Bytes the removal phase will free on the destination. Removals run
    /// first, so this space is available to the copies that follow — which
    /// matters when the spare is nearly full.
    pub bytes_to_free: u64,
    /// Bytes we did not have to transfer because they already matched.
    pub bytes_unchanged: u64,
}

impl PlanStats {
    /// Nothing to do.
    pub fn is_noop(&self) -> bool {
        self.dirs_to_create == 0
            && self.files_to_copy == 0
            && self.files_to_update == 0
            && self.links_to_create == 0
            && self.hardlinks == 0
            && self.meta_to_fix == 0
            && self.to_remove == 0
            && self.to_attic == 0
    }
}

/// The ordered work list.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub conflicts: Vec<Conflict>,
    pub stats: PlanStats,
}

impl Plan {
    pub fn len(&self) -> usize {
        self.actions.len()
    }
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Build the plan that makes `dest` look like `src`.
pub fn build(src: &Tree, dest: &Tree, mode: MirrorMode, policy: &ComparePolicy) -> Plan {
    let dest_index: HashMap<RelKey, &Entry> = dest.index();
    let src_index: HashMap<RelKey, &Entry> = src.index();

    // Phase buckets, concatenated at the end in dependency order.
    let mut dirs: Vec<Action> = Vec::new();
    let mut content: Vec<Action> = Vec::new();
    let mut links: Vec<Action> = Vec::new();
    let mut hardlinks: Vec<Action> = Vec::new();
    let mut metas: Vec<Action> = Vec::new();
    let mut removals: Vec<Action> = Vec::new();
    let mut dir_metas: Vec<Action> = Vec::new();
    let mut conflicts: Vec<Conflict> = Vec::new();
    let mut stats = PlanStats::default();

    // First name seen for each hard-linked inode. Later names become links to
    // this one instead of separate copies.
    let mut hardlink_primary: HashMap<super::meta::FileId, RelPath> = HashMap::new();

    for s in &src.entries {
        let key = s.rel.key();
        let d = dest_index.get(&key).copied();

        // A destination entry of a different kind has to go before we can
        // create the right thing in its place.
        if let Some(d) = d {
            if d.kind.is_dir() != s.kind.is_dir() || (d.is_link() != s.is_link()) {
                if !policy.replace_on_kind_change {
                    conflicts.push(Conflict {
                        rel: s.rel.clone(),
                        reason: format!(
                            "source is a {:?} but destination is a {:?}",
                            s.kind, d.kind
                        ),
                    });
                    continue;
                }
                // Remove the wrong-kind destination first, in the removal phase
                // (which runs before content) — see `assemble`.
                removals.push(match mode {
                    MirrorMode::Attic => Action::Attic {
                        rel: s.rel.clone(),
                        is_dir: d.is_dir(),
                    },
                    _ if d.is_dir() => Action::RemoveDir { rel: s.rel.clone() },
                    _ => Action::RemoveFile { rel: s.rel.clone() },
                });
                stats.to_remove += 1;
            }
        }

        match s.kind {
            EntryKind::Dir => {
                let exists_as_dir = d.is_some_and(|d| d.is_dir());
                if !exists_as_dir {
                    dirs.push(Action::CreateDir { rel: s.rel.clone() });
                    stats.dirs_to_create += 1;
                }
                // Directory timestamps are applied in the final phase.
                dir_metas.push(Action::SetDirMeta { rel: s.rel.clone() });
            }

            EntryKind::Symlink | EntryKind::Junction => {
                let target = s.link_target.clone().unwrap_or_default();
                let same = d.is_some_and(|d| {
                    d.kind == s.kind && d.link_target.as_deref() == Some(target.as_str())
                });
                if target.is_empty() {
                    conflicts.push(Conflict {
                        rel: s.rel.clone(),
                        reason: "link target could not be read".into(),
                    });
                } else if !same {
                    links.push(Action::CreateLink {
                        rel: s.rel.clone(),
                        kind: s.kind,
                        target,
                    });
                    stats.links_to_create += 1;
                } else {
                    stats.unchanged += 1;
                }
            }

            EntryKind::File => {
                // A second name for a file we are already copying: link it.
                if let Some(id) = s.file_id {
                    if let Some(primary) = hardlink_primary.get(&id) {
                        hardlinks.push(Action::Hardlink {
                            rel: s.rel.clone(),
                            to: primary.clone(),
                        });
                        stats.hardlinks += 1;
                        continue;
                    }
                    hardlink_primary.insert(id, s.rel.clone());
                }

                match d {
                    None => {
                        content.push(Action::CopyFile {
                            rel: s.rel.clone(),
                            size: s.size,
                        });
                        stats.files_to_copy += 1;
                        stats.bytes_to_copy += s.size;
                    }
                    Some(d) if d.is_dir() || d.is_link() => {
                        // Replaced above; copy fresh.
                        content.push(Action::CopyFile {
                            rel: s.rel.clone(),
                            size: s.size,
                        });
                        stats.files_to_copy += 1;
                        stats.bytes_to_copy += s.size;
                    }
                    Some(d) => {
                        if unchanged(s, d, policy) {
                            stats.unchanged += 1;
                            stats.bytes_unchanged += s.size;
                            if policy.fix_metadata && needs_meta_fix(s, d, policy) {
                                metas.push(Action::UpdateMeta { rel: s.rel.clone() });
                                stats.meta_to_fix += 1;
                            }
                        } else {
                            content.push(Action::UpdateFile {
                                rel: s.rel.clone(),
                                size: s.size,
                            });
                            stats.files_to_update += 1;
                            stats.bytes_to_copy += s.size;
                        }
                    }
                }
            }
        }
    }

    // Destination entries the source no longer has.
    if mode != MirrorMode::Additive {
        let mut extras: Vec<&Entry> = dest
            .entries
            .iter()
            .filter(|d| !src_index.contains_key(&d.rel.key()))
            .collect();
        // Deepest first: a directory only deletes once it is empty, and moving
        // a parent into the attic before its children would strand them.
        extras.sort_by(|a, b| b.rel.depth().cmp(&a.rel.depth()).then(a.rel.cmp(&b.rel)));

        for d in extras {
            // Skip anything already handled as a kind-change replacement.
            match mode {
                MirrorMode::Attic => {
                    removals.push(Action::Attic {
                        rel: d.rel.clone(),
                        is_dir: d.is_dir(),
                    });
                    stats.to_attic += 1;
                }
                MirrorMode::Delete => {
                    removals.push(if d.is_dir() {
                        Action::RemoveDir { rel: d.rel.clone() }
                    } else {
                        Action::RemoveFile { rel: d.rel.clone() }
                    });
                    stats.to_remove += 1;
                    stats.bytes_to_free += d.size;
                }
                MirrorMode::Additive => unreachable!(),
            }
        }
    }

    let actions = assemble(dirs, removals, content, links, hardlinks, metas, dir_metas);
    Plan {
        actions,
        conflicts,
        stats,
    }
}

/// Concatenate the phases in dependency order.
#[allow(clippy::too_many_arguments)]
fn assemble(
    dirs: Vec<Action>,
    removals: Vec<Action>,
    content: Vec<Action>,
    links: Vec<Action>,
    hardlinks: Vec<Action>,
    metas: Vec<Action>,
    dir_metas: Vec<Action>,
) -> Vec<Action> {
    let mut out =
        Vec::with_capacity(dirs.len() + removals.len() + content.len() + dir_metas.len());
    // Removals first: they free space before we start writing, which matters
    // when the destination is nearly full, and they clear wrong-kind entries
    // out of the way of the content phase.
    out.extend(removals);
    out.extend(dirs);
    out.extend(content);
    out.extend(links);
    // Hard links must follow the primary copy they point at.
    out.extend(hardlinks);
    out.extend(metas);
    // Directory timestamps last, or filling the directory destroys them.
    out.extend(dir_metas);
    out
}

/// Is the destination file already the same as the source?
fn unchanged(s: &Entry, d: &Entry, policy: &ComparePolicy) -> bool {
    match policy.compare {
        CompareBy::SizeOnly => s.size == d.size,
        CompareBy::SizeAndTime => s.looks_unchanged_from(d, policy.tolerance_nanos),
        // The planner cannot hash without doing I/O, which would break its
        // purity and double the run time up front. So in Content mode every
        // file that still exists is planned as an update *candidate*, and the
        // executor hashes both sides and skips the write when they match.
        // The byte estimate is therefore an upper bound in this mode, which is
        // the honest way round: better to over-estimate the work than to
        // promise a short run and deliver a long one.
        CompareBy::Content => false,
    }
}

/// Contents match but attributes or timestamps do not.
fn needs_meta_fix(s: &Entry, d: &Entry, policy: &ComparePolicy) -> bool {
    s.attrs_differ_from(d) || !s.modified.close_to(d.modified, policy.tolerance_nanos)
}

/// A human-readable summary, used by `--dry-run` and the run report.
pub fn summarize(plan: &Plan, mode: MirrorMode) -> String {
    let s = &plan.stats;
    let mut out = String::new();
    let line = |out: &mut String, n: u64, label: &str| {
        if n > 0 {
            out.push_str(&format!("  {n:>10}  {label}\n"));
        }
    };
    line(&mut out, s.dirs_to_create, "directories to create");
    line(&mut out, s.files_to_copy, "files to copy (new)");
    line(&mut out, s.files_to_update, "files to update (changed)");
    line(&mut out, s.links_to_create, "links to re-create");
    line(&mut out, s.hardlinks, "hard links to re-link");
    line(&mut out, s.meta_to_fix, "metadata fixes");
    match mode {
        MirrorMode::Attic => line(&mut out, s.to_attic, "entries to move to the attic"),
        MirrorMode::Delete => line(&mut out, s.to_remove, "entries to DELETE"),
        MirrorMode::Additive => {}
    }
    line(&mut out, s.unchanged, "unchanged (skipped)");
    out.push_str(&format!(
        "  {:>10}  to transfer ({} already in place)\n",
        human_bytes(s.bytes_to_copy),
        human_bytes(s.bytes_unchanged)
    ));
    if !plan.conflicts.is_empty() {
        out.push_str(&format!("  {:>10}  conflicts\n", plan.conflicts.len()));
    }
    out
}

/// Format a byte count the way a human reads it.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::meta::{attr, FileTime};
    use crate::file::walk::{ScanStats, Tree};
    use std::path::PathBuf;

    fn tree(entries: Vec<Entry>) -> Tree {
        Tree {
            root: PathBuf::from("/x"),
            entries,
            errors: vec![],
            stats: ScanStats::default(),
            skipped: vec![],
        }
    }

    fn f(rel: &str, size: u64, mtime: i64) -> Entry {
        let mut e = Entry::dir(RelPath::from_key_text(rel));
        e.kind = EntryKind::File;
        e.attrs = 0;
        e.size = size;
        e.modified = FileTime::from_unix_secs_nanos(mtime, 0);
        e
    }

    fn d(rel: &str) -> Entry {
        Entry::dir(RelPath::from_key_text(rel))
    }

    fn link(rel: &str, target: &str) -> Entry {
        let mut e = Entry::dir(RelPath::from_key_text(rel));
        e.kind = EntryKind::Symlink;
        e.attrs = 0;
        e.link_target = Some(target.to_string());
        e
    }

    fn policy() -> ComparePolicy {
        ComparePolicy {
            tolerance_nanos: 0,
            ..Default::default()
        }
    }

    fn pos(plan: &Plan, pred: impl Fn(&Action) -> bool) -> usize {
        plan.actions.iter().position(pred).expect("action not found")
    }

    #[test]
    fn an_empty_destination_copies_everything() {
        let src = tree(vec![d("a"), f("a/1.txt", 10, 100), f("b.txt", 20, 100)]);
        let plan = build(&src, &tree(vec![]), MirrorMode::Additive, &policy());

        assert_eq!(plan.stats.dirs_to_create, 1);
        assert_eq!(plan.stats.files_to_copy, 2);
        assert_eq!(plan.stats.bytes_to_copy, 30);
        assert_eq!(plan.stats.unchanged, 0);
    }

    #[test]
    fn identical_trees_produce_no_work() {
        let entries = vec![d("a"), f("a/1.txt", 10, 100)];
        let plan = build(
            &tree(entries.clone()),
            &tree(entries),
            MirrorMode::Delete,
            &policy(),
        );
        assert_eq!(plan.stats.unchanged, 1);
        assert_eq!(plan.stats.files_to_copy, 0);
        assert_eq!(plan.stats.files_to_update, 0);
        // Only the (idempotent) directory timestamp pass remains.
        assert!(plan
            .actions
            .iter()
            .all(|a| matches!(a, Action::SetDirMeta { .. })));
    }

    #[test]
    fn a_changed_file_is_an_update_not_a_copy() {
        let src = tree(vec![f("x.bin", 100, 200)]);
        let dst = tree(vec![f("x.bin", 100, 100)]); // same size, newer source
        let plan = build(&src, &dst, MirrorMode::Additive, &policy());
        assert_eq!(plan.stats.files_to_update, 1);
        assert_eq!(plan.stats.files_to_copy, 0);
        assert_eq!(plan.stats.bytes_to_copy, 100);
    }

    #[test]
    fn size_only_comparison_ignores_timestamps() {
        let src = tree(vec![f("x.bin", 100, 999)]);
        let dst = tree(vec![f("x.bin", 100, 100)]);
        let p = ComparePolicy {
            compare: CompareBy::SizeOnly,
            fix_metadata: false,
            ..policy()
        };
        let plan = build(&src, &dst, MirrorMode::Additive, &p);
        assert_eq!(plan.stats.unchanged, 1);
        assert_eq!(plan.stats.files_to_update, 0);
    }

    #[test]
    fn timestamp_tolerance_prevents_a_full_recopy_onto_fat() {
        // 1.5s apart — identical as far as FAT32 can represent.
        let src = tree(vec![f("x.bin", 100, 1_000_000_001)]);
        let mut dst_entry = f("x.bin", 100, 1_000_000_000);
        dst_entry.modified.unix_nanos += 500_000_000;
        let dst = tree(vec![dst_entry]);

        let exact = build(&src, &dst, MirrorMode::Additive, &policy());
        assert_eq!(exact.stats.files_to_update, 1, "exact comparison sees a change");

        let forgiving = ComparePolicy {
            tolerance_nanos: crate::file::meta::TOLERANCE_FAT,
            fix_metadata: false,
            ..policy()
        };
        let plan = build(&src, &dst, MirrorMode::Additive, &forgiving);
        assert_eq!(plan.stats.unchanged, 1, "FAT tolerance sees no change");
    }

    #[test]
    fn content_mode_defers_the_decision_to_the_executor() {
        // Hashing in the planner would double the run time before a single
        // byte moved, so Content mode plans every surviving file as an update
        // candidate and lets the executor skip the ones that already match.
        let src = tree(vec![f("x.bin", 100, 100)]);
        let dst = tree(vec![f("x.bin", 100, 100)]);
        let p = ComparePolicy {
            compare: CompareBy::Content,
            ..policy()
        };
        let plan = build(&src, &dst, MirrorMode::Additive, &p);
        assert_eq!(plan.stats.files_to_update, 1);
        assert_eq!(plan.stats.unchanged, 0);
    }

    #[test]
    fn additive_mode_never_removes_anything() {
        let src = tree(vec![f("keep.txt", 1, 1)]);
        let dst = tree(vec![f("keep.txt", 1, 1), f("gone.txt", 1, 1), d("olddir")]);
        let plan = build(&src, &dst, MirrorMode::Additive, &policy());
        assert_eq!(plan.stats.to_remove, 0);
        assert_eq!(plan.stats.to_attic, 0);
        assert!(!plan.actions.iter().any(|a| a.is_destructive()));
    }

    #[test]
    fn delete_mode_removes_extras_deepest_first() {
        let src = tree(vec![f("keep.txt", 1, 1)]);
        let dst = tree(vec![
            f("keep.txt", 1, 1),
            d("old"),
            d("old/deeper"),
            f("old/deeper/x.txt", 1, 1),
        ]);
        let plan = build(&src, &dst, MirrorMode::Delete, &policy());
        assert_eq!(plan.stats.to_remove, 3);

        // A directory cannot be removed before its contents.
        let file_at = pos(&plan, |a| a.rel().as_str() == "old/deeper/x.txt");
        let deeper_at = pos(&plan, |a| a.rel().as_str() == "old/deeper");
        let old_at = pos(&plan, |a| a.rel().as_str() == "old");
        assert!(file_at < deeper_at, "file must be removed before its directory");
        assert!(deeper_at < old_at, "child dir must go before its parent");
    }

    #[test]
    fn attic_mode_moves_instead_of_deleting() {
        let src = tree(vec![]);
        let dst = tree(vec![f("gone.txt", 1, 1)]);
        let plan = build(&src, &dst, MirrorMode::Attic, &policy());
        assert_eq!(plan.stats.to_attic, 1);
        assert_eq!(plan.stats.to_remove, 0);
        assert!(matches!(plan.actions[0], Action::Attic { .. }));
    }

    #[test]
    fn directories_are_created_before_the_files_inside_them() {
        let src = tree(vec![d("a"), d("a/b"), f("a/b/c.txt", 5, 1)]);
        let plan = build(&src, &tree(vec![]), MirrorMode::Additive, &policy());
        let a = pos(&plan, |x| matches!(x, Action::CreateDir { rel } if rel.as_str() == "a"));
        let ab = pos(&plan, |x| matches!(x, Action::CreateDir { rel } if rel.as_str() == "a/b"));
        let file = pos(&plan, |x| matches!(x, Action::CopyFile { .. }));
        assert!(a < ab && ab < file);
    }

    #[test]
    fn directory_timestamps_are_applied_after_their_contents() {
        // The bug this guards: setting a directory's mtime and *then* writing
        // files into it immediately destroys the mtime we just restored.
        let src = tree(vec![d("a"), f("a/1.txt", 5, 1)]);
        let plan = build(&src, &tree(vec![]), MirrorMode::Additive, &policy());
        let copy = pos(&plan, |x| matches!(x, Action::CopyFile { .. }));
        let dirmeta = pos(&plan, |x| matches!(x, Action::SetDirMeta { .. }));
        assert!(
            copy < dirmeta,
            "directory metadata must be the final phase, got copy={copy} dirmeta={dirmeta}"
        );
    }

    #[test]
    fn hardlinks_copy_once_and_link_the_rest() {
        let id = crate::file::meta::FileId {
            volume: 1,
            index: 42,
        };
        let mut a = f("a.bin", 1_000_000, 1);
        let mut b = f("b.bin", 1_000_000, 1);
        a.file_id = Some(id);
        b.file_id = Some(id);
        let plan = build(&tree(vec![a, b]), &tree(vec![]), MirrorMode::Additive, &policy());

        assert_eq!(plan.stats.files_to_copy, 1, "the bytes are stored once");
        assert_eq!(plan.stats.hardlinks, 1);
        assert_eq!(plan.stats.bytes_to_copy, 1_000_000, "not 2 MB");

        let copy = pos(&plan, |x| matches!(x, Action::CopyFile { .. }));
        let hl = pos(&plan, |x| matches!(x, Action::Hardlink { .. }));
        assert!(copy < hl, "cannot link to a file that is not there yet");
        match &plan.actions[hl] {
            Action::Hardlink { to, .. } => assert_eq!(to.as_str(), "a.bin"),
            other => panic!("expected a hard link, got {other:?}"),
        }
    }

    #[test]
    fn links_are_recreated_when_the_target_changes() {
        let src = tree(vec![link("l", "D:/new")]);
        let dst = tree(vec![link("l", "D:/old")]);
        let plan = build(&src, &dst, MirrorMode::Additive, &policy());
        assert_eq!(plan.stats.links_to_create, 1);

        let same = build(&src, &src.clone(), MirrorMode::Additive, &policy());
        assert_eq!(same.stats.links_to_create, 0);
        assert_eq!(same.stats.unchanged, 1);
    }

    #[test]
    fn a_kind_change_removes_the_destination_first() {
        // Source has a *file* where the destination has a *directory*.
        let src = tree(vec![f("thing", 10, 1)]);
        let dst = tree(vec![d("thing")]);
        let plan = build(&src, &dst, MirrorMode::Delete, &policy());

        let rm = pos(&plan, |a| matches!(a, Action::RemoveDir { .. }));
        let cp = pos(&plan, |a| matches!(a, Action::CopyFile { .. }));
        assert!(rm < cp, "the wrong-kind entry must be cleared first");
    }

    #[test]
    fn a_kind_change_can_be_reported_instead_of_forced() {
        let src = tree(vec![f("thing", 10, 1)]);
        let dst = tree(vec![d("thing")]);
        let p = ComparePolicy {
            replace_on_kind_change: false,
            ..policy()
        };
        let plan = build(&src, &dst, MirrorMode::Delete, &p);
        assert_eq!(plan.conflicts.len(), 1);
        assert!(plan.actions.iter().all(|a| !a.is_destructive()));
    }

    #[test]
    fn metadata_only_differences_produce_a_cheap_fix_not_a_recopy() {
        let mut s = f("x.txt", 100, 500);
        s.attrs = attr::READONLY | attr::HIDDEN;
        let mut dv = f("x.txt", 100, 500);
        dv.attrs = 0;
        let plan = build(&tree(vec![s]), &tree(vec![dv]), MirrorMode::Additive, &policy());

        assert_eq!(plan.stats.meta_to_fix, 1);
        assert_eq!(plan.stats.files_to_update, 0, "must not re-copy 100 bytes");
        assert_eq!(plan.stats.bytes_to_copy, 0);
    }

    #[test]
    fn summary_reports_the_delta_savings() {
        let src = tree(vec![f("a", 1000, 1), f("b", 2000, 2)]);
        let dst = tree(vec![f("a", 1000, 1)]);
        let plan = build(&src, &dst, MirrorMode::Additive, &policy());
        let text = summarize(&plan, MirrorMode::Additive);
        assert!(text.contains("files to copy"), "{text}");
        assert!(text.contains("already in place"), "{text}");
        assert_eq!(plan.stats.bytes_unchanged, 1000);
    }

    #[test]
    fn human_bytes_reads_naturally() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(8 * 1024 * 1024 * 1024 * 1024), "8.0 TiB");
    }
}
