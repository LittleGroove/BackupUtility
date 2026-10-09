//! Running a mirror job end to end.
//!
//! ```text
//!   validate -> sweep temps -> scan source -> scan dest -> plan
//!            -> (dry run stops here)
//!            -> create or resume save state
//!            -> execute, checkpointing as we go
//!            -> report
//! ```
//!
//! ## Parallelism, and where it is not allowed
//! Copying files is I/O-bound and embarrassingly parallel, so file actions run
//! across a worker pool. Removals do **not**: the plan orders them
//! deepest-first precisely so a directory is empty by the time we try to remove
//! it, and running them out of order would break that. So the executor walks
//! the plan splitting it into runs — parallel batches of independent work,
//! separated by strictly ordered removals — rather than treating the whole plan
//! as a bag of tasks.

use super::attic::{self, AtticPolicy, PruneReport};
use super::copy::{self, CopyOptions};
use super::filter::Filter;
use super::meta::Entry;
use super::path::{self, RelKey, RelPath};
use super::plan::{self, Action, CompareBy, ComparePolicy, MirrorMode, Plan, PlanStats};
use super::state::{
    self, ActionError, Progress, RunLog, RunState, RunStatus, SaveState, STATE_VERSION,
};
use super::sys;
use super::walk::{self, ScanOptions, ScanStats, Tree};
use super::Cancel;
use crate::timefmt;
use crate::{BkError, Result};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// Everything one mirror run needs.
#[derive(Debug, Clone)]
pub struct MirrorJob {
    pub name: String,
    /// The path being backed up. Usually a drive root, sometimes a subfolder.
    pub source: PathBuf,
    /// Where it goes. Likewise.
    pub dest: PathBuf,
    pub mode: MirrorMode,
    pub compare: CompareBy,
    pub filter: Filter,
    pub scan: ScanOptions,
    pub copy: CopyOptions,
    /// Concurrent file copies. More is not always faster: on one spinning disk
    /// heavy concurrency turns sequential reads into seeks.
    pub workers: usize,
    /// Plan and report, but change nothing.
    pub dry_run: bool,
    /// Pick up an interrupted run instead of starting over.
    pub resume: bool,
    pub state_root: PathBuf,
    /// Where displaced files go in [`MirrorMode::Attic`]. Defaults to
    /// `<dest>/.bku-attic/<timestamp>`.
    pub attic_root: Option<PathBuf>,
    /// When old attic folders are removed. Without this the attic grows
    /// without bound and eventually fills the spare drive.
    pub attic_policy: AtticPolicy,
    /// Refuse to start when the destination plainly cannot hold the copy.
    /// Catching it up front turns a wasted night into an instant error.
    pub check_free_space: bool,
    /// Write a per-run log under the state directory.
    pub log: bool,
    /// Return the source tree's file list in the report.
    ///
    /// Only the layout pass needs it, and on a multi-terabyte drive it is
    /// millions of strings, so ordinary jobs do not pay for it. Set by the
    /// caller when it intends to run an optimization stage afterwards, which
    /// saves walking the whole tree a second time.
    pub report_file_list: bool,
}

impl MirrorJob {
    /// A job with sensible defaults for a local disk-to-disk mirror.
    pub fn new(name: impl Into<String>, source: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        MirrorJob {
            name: name.into(),
            source: source.into(),
            dest: dest.into(),
            mode: MirrorMode::Additive,
            compare: CompareBy::SizeAndTime,
            filter: Filter::permissive(),
            scan: ScanOptions::default(),
            copy: CopyOptions::default(),
            workers: default_workers(),
            dry_run: false,
            resume: true,
            state_root: super::state::default_state_root(),
            attic_root: None,
            attic_policy: AtticPolicy::default(),
            check_free_space: true,
            log: true,
            report_file_list: false,
        }
    }

    /// The folder that holds this job's dated attic subfolders.
    pub fn attic_parent(&self) -> PathBuf {
        self.attic_root
            .clone()
            .unwrap_or_else(|| self.dest.join(".bku-attic"))
    }
}

/// A conservative default: enough concurrency to keep an SSD or a NAS busy,
/// not so much that a single spinning disk thrashes.
pub fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4)
}

/// Progress and lifecycle notifications for a front-end.
#[derive(Debug)]
pub enum Event<'a> {
    ScanningSource(&'a ScanStats),
    ScanningDest(&'a ScanStats),
    Planned(&'a Plan),
    /// An earlier run is being continued.
    Resuming { done: usize, total: usize },
    Progress(&'a Progress),
    Failed { rel: String, message: String },
}

/// What a run did.
#[derive(Debug, Clone)]
pub struct MirrorReport {
    pub job: String,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub mode: MirrorMode,
    pub started_unix: u64,
    pub finished_unix: u64,
    pub status: RunStatus,
    pub dry_run: bool,
    pub resumed: bool,
    pub plan_stats: PlanStats,
    pub actions_total: usize,
    /// Completed actions, including any carried over from an interrupted
    /// earlier attempt.
    pub actions_done: usize,
    /// Of `actions_done`, how many an earlier attempt had already finished.
    pub actions_carried_over: usize,
    pub actions_failed: usize,
    /// Bytes **this attempt** transferred. A resumed run reports only its own
    /// work here; `bytes_carried_over` holds what the earlier attempt did.
    pub bytes_copied: u64,
    pub bytes_carried_over: u64,
    pub files_copied: u64,
    /// Files that Content comparison proved were already identical, so no
    /// bytes moved.
    pub content_matches: u64,
    pub streams_copied: u64,
    pub scan_errors: usize,
    /// Metadata we could not preserve, counted by kind.
    pub degraded: BTreeMap<String, u64>,
    pub errors: Vec<ActionError>,
    pub attic: Option<PathBuf>,
    /// What attic pruning removed, if it ran.
    pub attic_prune: Option<PruneReport>,
    /// The run log this job wrote, for a front-end to link to.
    pub log_path: Option<PathBuf>,
    /// Every non-directory in the source, when `report_file_list` was set.
    pub file_list: Vec<RelPath>,
}

impl MirrorReport {
    pub fn elapsed_secs(&self) -> u64 {
        self.finished_unix.saturating_sub(self.started_unix)
    }

    pub fn succeeded(&self) -> bool {
        self.status == RunStatus::Completed && self.actions_failed == 0
    }

    /// A short report for a console or a log.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "{} — {} -> {}\n",
            self.job,
            path::display(&self.source),
            path::display(&self.dest)
        ));
        s.push_str(&format!(
            "  status      {:?}{}\n",
            self.status,
            if self.dry_run { " (dry run)" } else { "" }
        ));
        if self.resumed {
            s.push_str("  resumed     yes\n");
        }
        s.push_str(&format!(
            "  actions     {} of {} done, {} failed\n",
            self.actions_done, self.actions_total, self.actions_failed
        ));
        if self.actions_carried_over > 0 {
            s.push_str(&format!(
                "  carried     {} actions ({}) from the interrupted run\n",
                self.actions_carried_over,
                plan::human_bytes(self.bytes_carried_over)
            ));
        }
        s.push_str(&format!(
            "  copied      {} in {} files\n",
            plan::human_bytes(self.bytes_copied),
            self.files_copied
        ));
        if self.content_matches > 0 {
            s.push_str(&format!(
                "  verified    {} files already identical\n",
                self.content_matches
            ));
        }
        if self.streams_copied > 0 {
            s.push_str(&format!("  streams     {}\n", self.streams_copied));
        }
        if self.scan_errors > 0 {
            s.push_str(&format!("  scan errors {}\n", self.scan_errors));
        }
        for (what, n) in &self.degraded {
            s.push_str(&format!("  degraded    {n} x {what}\n"));
        }
        if let Some(a) = &self.attic {
            s.push_str(&format!("  attic       {}\n", path::display(a)));
        }
        if let Some(p) = &self.attic_prune {
            if p.folders_removed > 0 {
                s.push_str(&format!(
                    "  pruned      {} attic folders, freed {}\n",
                    p.folders_removed,
                    plan::human_bytes(p.bytes_removed)
                ));
            }
        }
        if let Some(l) = &self.log_path {
            s.push_str(&format!("  log         {}\n", path::display(l)));
        }
        s.push_str(&format!(
            "  elapsed     {}\n",
            timefmt::duration(self.elapsed_secs())
        ));
        s
    }
}

/// Reject job configurations that would destroy data or never terminate.
pub fn validate(job: &MirrorJob) -> Result<()> {
    if !job.source.exists() {
        return Err(BkError::Other(anyhow::anyhow!(
            "source does not exist: {}",
            path::display(&job.source)
        )));
    }
    if !job.source.is_dir() {
        return Err(BkError::Other(anyhow::anyhow!(
            "source is not a directory: {}",
            path::display(&job.source)
        )));
    }
    // The two traps that eat a drive:
    //   dest inside source  -> we copy the destination into itself, forever
    //   source inside dest  -> the destination scan sees the source
    if path::is_within(&job.dest, &job.source) {
        return Err(BkError::Other(anyhow::anyhow!(
            "destination {} is inside the source {} — this would copy the \
             backup into itself until the drive filled",
            path::display(&job.dest),
            path::display(&job.source)
        )));
    }
    if path::is_within(&job.source, &job.dest) {
        return Err(BkError::Other(anyhow::anyhow!(
            "source {} is inside the destination {} — pick a destination that \
             does not contain the source",
            path::display(&job.source),
            path::display(&job.dest)
        )));
    }
    if job.workers == 0 {
        return Err(BkError::Other(anyhow::anyhow!("workers must be at least 1")));
    }
    Ok(())
}

/// Run a mirror job.
pub fn run(
    job: &MirrorJob,
    cancel: &Cancel,
    events: &mut dyn FnMut(Event),
) -> Result<MirrorReport> {
    let started = timefmt::now_unix();
    let log = if job.log {
        RunLog::create(&job.state_root, &job.name, &timefmt::stamp(started))
    } else {
        RunLog::disabled()
    };
    log.line(&format!(
        "=== {} — {} -> {} — started {} ===",
        job.name,
        path::display(&job.source),
        path::display(&job.dest),
        timefmt::readable(started)
    ));
    log.line(&format!(
        "mode {:?}, compare {:?}, {} workers{}",
        job.mode,
        job.compare,
        job.workers,
        if job.dry_run { ", DRY RUN" } else { "" }
    ));

    // Validate after opening the log so a misconfigured job leaves a record of
    // *why* it refused, rather than failing invisibly at 3am.
    if let Err(e) = validate(job) {
        log.line(&format!("REFUSED: {e}"));
        log.flush();
        return Err(e);
    }

    // Ask for backup/restore privileges up front. Failing is not fatal — we
    // simply will not be able to read files whose ACLs exclude us, and those
    // are reported as errors rather than silently skipped.
    if job.copy.use_backup_privilege {
        let _ = copy::enable_backup_privileges();
    }

    if !job.dry_run {
        copy::create_dir(&job.dest)?;
        // Clear temps from a previous interrupted attempt so they cannot be
        // mistaken for real files or grow without bound.
        let _ = copy::sweep_part_files(&job.dest);
    }

    // Comparison tolerance comes from the *destination* filesystem, because it
    // is the one that has to store the timestamps we write.
    let tolerance = sys::time_tolerance_for(&job.dest);
    let policy = ComparePolicy {
        compare: job.compare,
        tolerance_nanos: tolerance,
        ..Default::default()
    };

    let src_tree = walk::scan(&job.source, &job.filter, &job.scan, cancel, &mut |s| {
        events(Event::ScanningSource(s))
    })?;
    if cancel.is_cancelled() {
        return Ok(cancelled_report(job, started, &src_tree));
    }

    // The destination is scanned with the same filter so an excluded file is
    // not seen as "extra" and deleted from the backup.
    let dest_tree = if job.dest.exists() {
        walk::scan(&job.dest, &job.filter, &job.scan, cancel, &mut |s| {
            events(Event::ScanningDest(s))
        })?
    } else {
        empty_tree(&job.dest)
    };
    if cancel.is_cancelled() {
        return Ok(cancelled_report(job, started, &src_tree));
    }

    let the_plan = plan::build(&src_tree, &dest_tree, job.mode, &policy);
    events(Event::Planned(&the_plan));

    let scan_errors = src_tree.errors.len() + dest_tree.errors.len();
    log.line(&format!(
        "scanned: source {} files / {} dirs, destination {} files, {scan_errors} scan errors",
        src_tree.stats.files, src_tree.stats.dirs, dest_tree.stats.files
    ));
    log.block(&plan::summarize(&the_plan, job.mode));
    for c in &the_plan.conflicts {
        log.line(&format!("conflict: {} — {}", c.rel, c.reason));
    }
    for e in src_tree.errors.iter().chain(dest_tree.errors.iter()) {
        log.line(&format!("scan error: {} — {}", e.rel, e.message));
    }

    // Will it even fit? The plan knows what it needs and the volume knows what
    // it has, so a destination that plainly cannot hold the copy is caught in
    // milliseconds instead of eight hours in.
    if job.check_free_space && !job.dry_run {
        if let Err(e) = check_space(job, &the_plan) {
            log.line(&format!("REFUSED: {e}"));
            log.flush();
            return Err(e);
        }
    }
    if job.dry_run {
        return Ok(MirrorReport {
            job: job.name.clone(),
            source: job.source.clone(),
            dest: job.dest.clone(),
            mode: job.mode,
            started_unix: started,
            finished_unix: timefmt::now_unix(),
            status: RunStatus::Completed,
            dry_run: true,
            resumed: false,
            plan_stats: the_plan.stats,
            actions_total: the_plan.len(),
            actions_done: 0,
            actions_carried_over: 0,
            actions_failed: 0,
            bytes_copied: 0,
            bytes_carried_over: 0,
            files_copied: 0,
            content_matches: 0,
            streams_copied: 0,
            scan_errors,
            degraded: BTreeMap::new(),
            errors: Vec::new(),
            attic: None,
            attic_prune: None,
            log_path: log.path().map(|p| p.to_path_buf()),
            file_list: Vec::new(),
        });
    }

    // ---- Save state: resume the previous attempt, or start a new one ------
    let (mut state, actions, resumed) = open_state(job, &the_plan, started, events)?;

    let attic_parent = job.attic_parent();
    let attic_root = if job.mode == MirrorMode::Attic {
        // A resumed run keeps the folder it started; a new one takes a fresh
        // name that is not already taken.
        let stamp = state
            .run()
            .attic_stamp
            .clone()
            .unwrap_or_else(|| unique_attic_stamp(&attic_parent, started));
        state.set_attic_stamp(stamp.clone())?;
        Some(attic_parent.join(stamp))
    } else {
        None
    };

    let src_entries: HashMap<RelKey, Entry> = src_tree
        .entries
        .iter()
        .map(|e| (e.rel.key(), e.clone()))
        .collect();

    let ctx = Ctx {
        src_root: job.source.clone(),
        dest_root: job.dest.clone(),
        attic_root: attic_root.clone().unwrap_or_default(),
        src_entries,
        copy: job.copy.clone(),
        compare: job.compare,
        cancel: cancel.clone(),
    };

    let tally = Tally::default();

    // Work a previous attempt already finished.
    //
    // This is deliberately kept *out* of the tally: the tally is what this
    // attempt did, and folding the carried-over bytes into it would make a
    // resumed run report a transfer it never performed. The baseline is added
    // back only for progress display, so the percentage continues from where
    // the interrupted run stopped rather than restarting at zero.
    let mut baseline = Baseline::default();
    for (i, a) in actions.iter().enumerate() {
        if state.is_done(i) {
            baseline.bytes += a.bytes();
            baseline.actions += 1;
        }
    }

    let totals = Totals {
        actions: actions.len() as u64,
        bytes: the_plan.stats.bytes_to_copy,
    };
    let start_instant = Instant::now();

    execute(
        &actions,
        &ctx,
        &mut state,
        &tally,
        &totals,
        &baseline,
        start_instant,
        job.workers,
        cancel,
        &log,
        events,
    )?;

    let status = if cancel.is_cancelled() {
        RunStatus::Cancelled
    } else if tally.failed.load(Ordering::Relaxed) > 0 {
        // The run finished, but not everything landed. Still "completed" —
        // partial success with a listed error set is more useful than a
        // blanket failure that hides what did work.
        RunStatus::Completed
    } else {
        RunStatus::Completed
    };
    let finished = timefmt::now_unix();
    state.set_status(status, finished)?;

    // Prune the attic only after a run that actually finished. Pruning after a
    // cancelled run could delete history while the current attic folder is
    // still half-populated.
    let attic_prune = if job.mode == MirrorMode::Attic && status == RunStatus::Completed {
        match attic::prune(&job.attic_parent(), &job.attic_policy, finished) {
            Ok(p) => {
                if p.folders_removed > 0 {
                    log.block(&p.describe());
                }
                Some(p)
            }
            Err(e) => {
                // Never fatal: failing to tidy up must not fail the backup.
                log.line(&format!("attic prune failed: {e}"));
                None
            }
        }
    } else {
        None
    };

    let errors = state.errors()?;
    let report = MirrorReport {
        job: job.name.clone(),
        source: job.source.clone(),
        dest: job.dest.clone(),
        mode: job.mode,
        started_unix: started,
        finished_unix: finished,
        status,
        dry_run: false,
        resumed,
        plan_stats: the_plan.stats,
        actions_total: actions.len(),
        actions_done: (baseline.actions + tally.actions_done.load(Ordering::Relaxed)) as usize,
        actions_carried_over: baseline.actions as usize,
        bytes_carried_over: baseline.bytes,
        actions_failed: tally.failed.load(Ordering::Relaxed) as usize,
        bytes_copied: tally.bytes.load(Ordering::Relaxed),
        files_copied: tally.files.load(Ordering::Relaxed),
        content_matches: tally.content_matches.load(Ordering::Relaxed),
        streams_copied: tally.streams.load(Ordering::Relaxed),
        scan_errors,
        degraded: tally.degraded.lock().map(|d| d.clone()).unwrap_or_default(),
        errors,
        // Only name the attic if something actually went into it. Reporting a
        // folder that was never created sends the user looking for it.
        attic: attic_root.filter(|_| the_plan.stats.to_attic > 0),
        attic_prune,
        log_path: log.path().map(|p| p.to_path_buf()),
        file_list: if job.report_file_list {
            src_tree
                .entries
                .iter()
                .filter(|e| !e.is_dir() && !e.is_link())
                .map(|e| e.rel.clone())
                .collect()
        } else {
            Vec::new()
        },
    };
    log.block(&report.summary());
    log.line(&format!("=== finished {} ===", timefmt::readable(finished)));
    log.flush();
    // Keep the log directory bounded the same way the attic is.
    let _ = state::prune_logs(&job.state_root, &job.name, state::KEEP_LOGS);
    Ok(report)
}

/// An attic folder name that is not already in use.
///
/// Two runs of the same job starting within the same second would otherwise
/// share a folder, and the second would overwrite files the first had put
/// there — quietly losing the older copy of anything atticked twice.
fn unique_attic_stamp(parent: &Path, started: u64) -> String {
    let base = timefmt::stamp(started);
    if !parent.join(&base).exists() {
        return base;
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if !parent.join(&candidate).exists() {
            return candidate;
        }
    }
    base
}

/// Refuse a run the destination cannot hold.
///
/// Removals run before copies, so the space they free counts as available —
/// otherwise a delete-mode job that swaps one large file for another would be
/// refused despite needing no extra room at all.
fn check_space(job: &MirrorJob, the_plan: &Plan) -> Result<()> {
    let needed = the_plan.stats.bytes_to_copy;
    if needed == 0 {
        return Ok(());
    }
    // If we cannot read the free space (a network share, an odd filesystem),
    // say nothing and let the run proceed. A check that cannot answer must not
    // block the backup.
    let Some(free) = sys::free_space(&job.dest) else {
        return Ok(());
    };
    match shortfall(needed, free, the_plan.stats.bytes_to_free) {
        None => Ok(()),
        Some(short) => Err(BkError::Other(anyhow::anyhow!(
            "not enough room on {}: the copy needs {} but only {} is available \
             ({} free{}). Short by {}.",
            path::display(&job.dest),
            plan::human_bytes(needed),
            plan::human_bytes(free.saturating_add(the_plan.stats.bytes_to_free)),
            plan::human_bytes(free),
            if the_plan.stats.bytes_to_free > 0 {
                format!(
                    ", plus {} freed by removals",
                    plan::human_bytes(the_plan.stats.bytes_to_free)
                )
            } else {
                String::new()
            },
            plan::human_bytes(short)
        ))),
    }
}

/// How many bytes short the destination is, or `None` if the copy fits.
///
/// Pure, so the arithmetic — particularly the part where removals free space
/// the copies can then use — is testable without needing a full disk.
pub fn shortfall(needed: u64, free: u64, freed_by_removals: u64) -> Option<u64> {
    let available = free.saturating_add(freed_by_removals);
    (needed > available).then(|| needed - available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_that_fits_is_not_refused() {
        assert_eq!(shortfall(100, 100, 0), None);
        assert_eq!(shortfall(100, 500, 0), None);
        assert_eq!(shortfall(0, 0, 0), None);
    }

    #[test]
    fn a_copy_that_does_not_fit_reports_how_short_it_is() {
        assert_eq!(shortfall(1000, 400, 0), Some(600));
    }

    #[test]
    fn space_freed_by_removals_counts_as_available() {
        // Delete mode runs removals first, so swapping one large file for
        // another of the same size needs no extra room at all. Ignoring that
        // would refuse a job that would have worked perfectly.
        assert_eq!(shortfall(1000, 0, 1000), None);
        assert_eq!(shortfall(1000, 200, 500), Some(300));
    }

    #[test]
    fn the_arithmetic_does_not_overflow_on_huge_volumes() {
        assert_eq!(shortfall(1, u64::MAX, u64::MAX), None);
    }

    #[test]
    fn a_fresh_attic_stamp_is_used_when_the_folder_is_free() {
        let dir = std::env::temp_dir().join("bku_stamp_free");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            unique_attic_stamp(&dir, 1_700_000_000),
            timefmt::stamp(1_700_000_000)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_runs_in_the_same_second_get_different_attic_folders() {
        // Otherwise the second run overwrites files the first put there.
        let dir = std::env::temp_dir().join("bku_stamp_clash");
        let _ = std::fs::remove_dir_all(&dir);
        let base = timefmt::stamp(1_700_000_000);
        std::fs::create_dir_all(dir.join(&base)).unwrap();
        assert_eq!(unique_attic_stamp(&dir, 1_700_000_000), format!("{base}-2"));

        std::fs::create_dir_all(dir.join(format!("{base}-2"))).unwrap();
        assert_eq!(unique_attic_stamp(&dir, 1_700_000_000), format!("{base}-3"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Decide between resuming and starting fresh.
fn open_state(
    job: &MirrorJob,
    the_plan: &Plan,
    started: u64,
    events: &mut dyn FnMut(Event),
) -> Result<(SaveState, Vec<Action>, bool)> {
    if job.resume {
        if let Some((existing, actions)) = SaveState::load(&job.state_root, &job.name)? {
            let check = existing.can_resume(&job.source, &job.dest, job.mode);
            if check.is_yes() {
                events(Event::Resuming {
                    done: existing.done_count(),
                    total: actions.len(),
                });
                return Ok((existing, actions, true));
            }
        }
    }
    let run = RunState {
        version: STATE_VERSION,
        job: job.name.clone(),
        source: job.source.clone(),
        dest: job.dest.clone(),
        mode: job.mode,
        started_unix: started,
        finished_unix: None,
        status: RunStatus::Running,
        total_actions: the_plan.len() as u64,
        total_bytes: the_plan.stats.bytes_to_copy,
        attic_stamp: None,
    };
    let state = SaveState::create(&job.state_root, run, the_plan)?;
    Ok((state, the_plan.actions.clone(), false))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

struct Ctx {
    src_root: PathBuf,
    dest_root: PathBuf,
    attic_root: PathBuf,
    src_entries: HashMap<RelKey, Entry>,
    copy: CopyOptions,
    compare: CompareBy,
    cancel: Cancel,
}

#[derive(Default)]
struct Tally {
    bytes: AtomicU64,
    files: AtomicU64,
    streams: AtomicU64,
    content_matches: AtomicU64,
    actions_done: AtomicU64,
    failed: AtomicU64,
    degraded: Mutex<BTreeMap<String, u64>>,
    current: Mutex<Option<String>>,
}

struct Totals {
    actions: u64,
    bytes: u64,
}

/// Work completed by an earlier attempt at the same plan.
#[derive(Debug, Clone, Copy, Default)]
struct Baseline {
    actions: u64,
    bytes: u64,
}

/// One action's result, sent back to the coordinating thread.
enum Msg {
    Ok {
        index: usize,
        outcome: ExecOutcome,
    },
    Err {
        index: usize,
        rel: String,
        message: String,
    },
}

#[derive(Default)]
struct ExecOutcome {
    bytes: u64,
    is_file_copy: bool,
    streams: u32,
    content_match: bool,
    degraded: Vec<&'static str>,
}

/// Removals are order-sensitive (deepest first) and must not be parallelized.
fn is_sequential(a: &Action) -> bool {
    matches!(
        a,
        Action::RemoveDir { .. } | Action::RemoveFile { .. } | Action::Attic { .. }
    )
}

#[allow(clippy::too_many_arguments)]
fn execute(
    actions: &[Action],
    ctx: &Ctx,
    state: &mut SaveState,
    tally: &Tally,
    totals: &Totals,
    baseline: &Baseline,
    start: Instant,
    workers: usize,
    cancel: &Cancel,
    log: &RunLog,
    events: &mut dyn FnMut(Event),
) -> Result<()> {
    let mut i = 0usize;
    while i < actions.len() {
        if cancel.is_cancelled() {
            break;
        }
        if is_sequential(&actions[i]) {
            if !state.is_done(i) {
                match execute_one(&actions[i], ctx) {
                    Ok(o) => {
                        absorb(tally, &o);
                        state.mark_done(i)?;
                        tally.actions_done.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        record_failure(state, tally, i, &actions[i], &e.to_string(), log, events)?;
                    }
                }
            }
            i += 1;
            continue;
        }

        // Collect the longest run of independent actions and do them at once.
        let mut j = i;
        while j < actions.len() && !is_sequential(&actions[j]) {
            j += 1;
        }
        run_batch(
            &actions[i..j],
            i,
            ctx,
            state,
            tally,
            totals,
            baseline,
            start,
            workers,
            cancel,
            log,
            events,
        )?;
        i = j;
    }
    state.checkpoint()?;
    publish(state, tally, totals, baseline, start, events);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_batch(
    batch: &[Action],
    base: usize,
    ctx: &Ctx,
    state: &mut SaveState,
    tally: &Tally,
    totals: &Totals,
    baseline: &Baseline,
    start: Instant,
    workers: usize,
    cancel: &Cancel,
    log: &RunLog,
    events: &mut dyn FnMut(Event),
) -> Result<()> {
    let cursor = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<Msg>();

    // Snapshot which actions a previous attempt already finished.
    //
    // The workers need to read this while the coordinating thread is calling
    // `state.mark_done`, and a `&SaveState` in the workers cannot coexist with
    // the `&mut SaveState` here. Taking the snapshot up front resolves that
    // and costs nothing: within a single batch, nothing becomes done that a
    // worker would need to notice — each action is claimed by exactly one
    // worker.
    let already_done: Vec<bool> = (0..batch.len()).map(|k| state.is_done(base + k)).collect();

    let mut result: Result<()> = Ok(());
    std::thread::scope(|scope| {
        for _ in 0..workers.max(1) {
            let tx = tx.clone();
            let cursor = &cursor;
            let already_done = &already_done;
            scope.spawn(move || {
                loop {
                    if ctx.cancel.is_cancelled() {
                        return;
                    }
                    let k = cursor.fetch_add(1, Ordering::Relaxed);
                    if k >= batch.len() {
                        return;
                    }
                    let index = base + k;
                    // Skipping here rather than when building the batch keeps
                    // indices aligned with the persisted plan.
                    if already_done[k] {
                        continue;
                    }
                    let action = &batch[k];
                    if let Ok(mut cur) = tally.current.lock() {
                        *cur = Some(action.rel().to_string());
                    }
                    let msg = match execute_one(action, ctx) {
                        Ok(outcome) => Msg::Ok { index, outcome },
                        Err(e) => Msg::Err {
                            index,
                            rel: action.rel().to_string(),
                            message: e.to_string(),
                        },
                    };
                    if tx.send(msg).is_err() {
                        return; // coordinator went away
                    }
                }
            });
        }
        // Dropping our own sender lets the loop below end when the last worker
        // finishes; without this it would block forever.
        drop(tx);

        let mut last_publish = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Ok { index, outcome }) => {
                    absorb(tally, &outcome);
                    if let Err(e) = state.mark_done(index) {
                        result = Err(e);
                        cancel.cancel();
                        break;
                    }
                    tally.actions_done.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Msg::Err {
                    index,
                    rel,
                    message,
                }) => {
                    tally.failed.fetch_add(1, Ordering::Relaxed);
                    let _ = state.record_error(&ActionError {
                        index: index as u64,
                        rel: rel.clone(),
                        message: message.clone(),
                    });
                    log.line(&format!("FAILED {rel} — {message}"));
                    events(Event::Failed { rel, message });
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if last_publish.elapsed() >= Duration::from_millis(500) {
                publish(state, tally, totals, baseline, start, events);
                last_publish = Instant::now();
            }
        }
    });

    result?;
    state.checkpoint()?;
    publish(state, tally, totals, baseline, start, events);
    Ok(())
}

fn absorb(tally: &Tally, o: &ExecOutcome) {
    tally.bytes.fetch_add(o.bytes, Ordering::Relaxed);
    if o.is_file_copy {
        tally.files.fetch_add(1, Ordering::Relaxed);
    }
    if o.content_match {
        tally.content_matches.fetch_add(1, Ordering::Relaxed);
    }
    tally.streams.fetch_add(o.streams as u64, Ordering::Relaxed);
    if !o.degraded.is_empty() {
        if let Ok(mut d) = tally.degraded.lock() {
            for what in &o.degraded {
                *d.entry((*what).to_string()).or_insert(0) += 1;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn record_failure(
    state: &mut SaveState,
    tally: &Tally,
    index: usize,
    action: &Action,
    message: &str,
    log: &RunLog,
    events: &mut dyn FnMut(Event),
) -> Result<()> {
    tally.failed.fetch_add(1, Ordering::Relaxed);
    state.record_error(&ActionError {
        index: index as u64,
        rel: action.rel().to_string(),
        message: message.to_string(),
    })?;
    log.line(&format!("FAILED {} — {message}", action.rel()));
    events(Event::Failed {
        rel: action.rel().to_string(),
        message: message.to_string(),
    });
    Ok(())
}

fn publish(
    state: &SaveState,
    tally: &Tally,
    totals: &Totals,
    baseline: &Baseline,
    start: Instant,
    events: &mut dyn FnMut(Event),
) {
    let elapsed = start.elapsed().as_secs();
    // Progress answers "how far through the job am I", so it counts the
    // carried-over work. The rate is computed from this attempt's bytes alone,
    // because that is the only part that took the time we are measuring.
    let bytes = tally.bytes.load(Ordering::Relaxed);
    let p = Progress {
        actions_done: baseline.actions + tally.actions_done.load(Ordering::Relaxed),
        actions_total: totals.actions,
        bytes_done: baseline.bytes + bytes,
        bytes_total: totals.bytes,
        files_copied: tally.files.load(Ordering::Relaxed),
        errors: tally.failed.load(Ordering::Relaxed),
        current: tally.current.lock().ok().and_then(|c| c.clone()),
        elapsed_secs: elapsed,
        rate_bps: bytes.checked_div(elapsed).unwrap_or(0),
    };
    let _ = state.publish_progress(&p);
    events(Event::Progress(&p));
}

/// Perform one action.
fn execute_one(action: &Action, ctx: &Ctx) -> Result<ExecOutcome> {
    let rel = action.rel();
    let dst = rel.under(&ctx.dest_root);

    match action {
        Action::CreateDir { .. } => {
            copy::create_dir(&dst)?;
            Ok(ExecOutcome::default())
        }

        Action::CopyFile { .. } | Action::UpdateFile { .. } => {
            let src = rel.under(&ctx.src_root);
            let entry = ctx
                .src_entries
                .get(&rel.key())
                .ok_or_else(|| BkError::Other(anyhow::anyhow!("{rel}: vanished from the plan")))?;

            // Content mode: prove the file really differs before moving bytes.
            if ctx.compare == CompareBy::Content && dst.exists() {
                let same_size = std::fs::metadata(path::long(&dst))
                    .map(|m| m.len() == entry.size)
                    .unwrap_or(false);
                if same_size && copy::verify_same(&src, &dst).unwrap_or(false) {
                    return Ok(ExecOutcome {
                        content_match: true,
                        ..Default::default()
                    });
                }
            }

            let out = copy::copy_file(&src, &dst, entry, &ctx.copy, &ctx.cancel, &mut |_| {})?;
            Ok(ExecOutcome {
                bytes: out.bytes_written,
                is_file_copy: true,
                streams: out.streams,
                content_match: false,
                degraded: out.degraded,
            })
        }

        Action::Hardlink { to, .. } => {
            copy::create_hardlink(&dst, &to.under(&ctx.dest_root))?;
            Ok(ExecOutcome::default())
        }

        Action::CreateLink { kind, target, .. } => {
            copy::create_link(&dst, *kind, target)?;
            Ok(ExecOutcome::default())
        }

        Action::UpdateMeta { .. } => {
            if let Some(entry) = ctx.src_entries.get(&rel.key()) {
                let mut degraded = Vec::new();
                if ctx.copy.preserve_attributes && !copy::apply_attributes(&dst, entry) {
                    degraded.push("attributes");
                }
                if ctx.copy.preserve_timestamps && !copy::apply_timestamps(&dst, entry) {
                    degraded.push("timestamps");
                }
                return Ok(ExecOutcome {
                    degraded,
                    ..Default::default()
                });
            }
            Ok(ExecOutcome::default())
        }

        Action::SetDirMeta { .. } => {
            if let Some(entry) = ctx.src_entries.get(&rel.key()) {
                copy::set_dir_meta(&dst, entry, &ctx.copy)?;
            }
            Ok(ExecOutcome::default())
        }

        Action::RemoveFile { .. } => {
            copy::remove_file(&dst)?;
            Ok(ExecOutcome::default())
        }

        Action::RemoveDir { .. } => {
            copy::remove_dir(&dst)?;
            Ok(ExecOutcome::default())
        }

        Action::Attic { .. } => {
            copy::move_to_attic(&dst, &ctx.attic_root, rel)?;
            Ok(ExecOutcome::default())
        }
    }
}

fn empty_tree(root: &Path) -> Tree {
    Tree {
        root: root.to_path_buf(),
        entries: Vec::new(),
        errors: Vec::new(),
        stats: ScanStats::default(),
        skipped: Vec::new(),
    }
}

fn cancelled_report(job: &MirrorJob, started: u64, src: &Tree) -> MirrorReport {
    MirrorReport {
        job: job.name.clone(),
        source: job.source.clone(),
        dest: job.dest.clone(),
        mode: job.mode,
        started_unix: started,
        finished_unix: timefmt::now_unix(),
        status: RunStatus::Cancelled,
        dry_run: job.dry_run,
        resumed: false,
        plan_stats: PlanStats::default(),
        actions_total: 0,
        actions_done: 0,
        actions_carried_over: 0,
        actions_failed: 0,
        bytes_copied: 0,
        bytes_carried_over: 0,
        files_copied: 0,
        content_matches: 0,
        streams_copied: 0,
        scan_errors: src.errors.len(),
        degraded: BTreeMap::new(),
        errors: Vec::new(),
        attic: None,
        attic_prune: None,
        log_path: None,
        file_list: Vec::new(),
    }
}

