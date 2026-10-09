//! Save states: making a multi-hour job survive a crash, a reboot, or a
//! deliberate stop.
//!
//! Mirroring 8 TB can run overnight. If a power cut at hour six meant starting
//! from zero, the tool would be unusable, so a run is checkpointed as it goes
//! and can pick up exactly where it stopped.
//!
//! ## On-disk layout
//! ```text
//!   <state_root>/<job>/
//!     run.json        the run's identity and status  (atomically replaced)
//!     plan.jsonl      one action per line; the line number is its index
//!     done.bin        4-byte LE indices of completed actions (append-only)
//!     errors.jsonl    per-action failures, appended as they happen
//!     progress.json   a live snapshot for the GUI    (atomically replaced)
//! ```
//!
//! ## Why an append-only index journal
//! The obvious design — rewrite a "completed" list periodically — is both slow
//! and unsafe: a crash during the rewrite can lose the whole record. Appending
//! a fixed-size record instead means a crash can only ever damage the *last*
//! record, and because every record is exactly 4 bytes a torn tail is
//! detectable (`len % 4 != 0`) and discarded. The worst case is that we redo a
//! handful of actions, and every action is idempotent, so redoing is harmless.

use super::plan::{Action, MirrorMode, Plan};
use crate::{BkError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// Bump when the layout changes incompatibly; an older state is then ignored
/// rather than misread.
pub const STATE_VERSION: u32 = 1;

/// Bytes per record in `done.bin`.
const REC: usize = 4;

/// Flush the journal to disk at least this often (in completed actions). A
/// crash can therefore cost at most this many redone actions.
const FLUSH_EVERY: usize = 256;

/// Where a run got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Executing the plan. A run left in this state was interrupted.
    ///
    /// There is deliberately no "planning" status: the save state does not
    /// exist until the plan is committed, so a status the engine can never
    /// write would only mislead whoever read it.
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl RunStatus {
    /// Can this run be picked back up?
    pub fn is_resumable(self) -> bool {
        matches!(self, RunStatus::Running | RunStatus::Cancelled)
    }
}

/// The identity and status of one run — everything needed to decide whether a
/// stored state may be resumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub version: u32,
    pub job: String,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub mode: MirrorMode,
    pub started_unix: u64,
    #[serde(default)]
    pub finished_unix: Option<u64>,
    pub status: RunStatus,
    pub total_actions: u64,
    pub total_bytes: u64,
    /// The attic subfolder this run moves displaced files into, so a resumed
    /// run keeps using the same one instead of creating a second.
    #[serde(default)]
    pub attic_stamp: Option<String>,
}

/// A live snapshot for the GUI and for `bku status`. Written atomically so a
/// reader never sees half a file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub actions_done: u64,
    pub actions_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_copied: u64,
    pub errors: u64,
    /// What a worker is on right now, for the "currently copying..." line.
    #[serde(default)]
    pub current: Option<String>,
    pub elapsed_secs: u64,
    /// Bytes per second over the run so far.
    pub rate_bps: u64,
}

impl Progress {
    pub fn percent(&self) -> f64 {
        if self.bytes_total > 0 {
            (self.bytes_done as f64 / self.bytes_total as f64) * 100.0
        } else if self.actions_total > 0 {
            (self.actions_done as f64 / self.actions_total as f64) * 100.0
        } else {
            100.0
        }
    }

    /// Seconds remaining at the current rate, or `None` if we cannot tell yet.
    pub fn eta_secs(&self) -> Option<u64> {
        let remaining = self.bytes_total.checked_sub(self.bytes_done)?;
        if self.rate_bps == 0 {
            return None;
        }
        Some(remaining / self.rate_bps)
    }
}

/// One action that failed, recorded for the run report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionError {
    pub index: u64,
    pub rel: String,
    pub message: String,
}

/// The default place run state lives.
pub fn default_state_root() -> PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("BackupUtility").join("state")
    }
    #[cfg(not(windows))]
    {
        // Fall back to a user-writable path when not running as root.
        if super::sys::is_elevated() {
            PathBuf::from("/var/lib/bku/state")
        } else {
            crate::paths::xdg("XDG_DATA_HOME", ".local/share").join("bku/state")
        }
    }
}

/// How many run logs to keep per job before the oldest are deleted.
pub const KEEP_LOGS: usize = 30;

/// A per-run text log.
///
/// A scheduled job writes its progress to a console nobody is watching, and
/// Task Scheduler keeps only the exit code. Without a log on disk, "the 3am
/// backup failed" comes with no way to find out what it did, how far it got,
/// or what it skipped — so every run leaves one behind.
///
/// Writes are best-effort: a log that cannot be written must never take down
/// the backup it is describing.
pub struct RunLog {
    path: Option<PathBuf>,
    file: std::sync::Mutex<Option<File>>,
}

impl RunLog {
    /// Open `<state_root>/<job>/logs/<stamp>.log`.
    ///
    /// Two runs starting in the same second get `-2`, `-3`, ... appended, so
    /// the history lists one entry per run rather than silently concatenating
    /// several into one file. `timefmt::parse_stamp` understands the suffix,
    /// so pruning and the history view still read the date correctly.
    pub fn create(state_root: &Path, job: &str, stamp: &str) -> Self {
        let dir = SaveState::dir_for(state_root, job).join("logs");
        if fs::create_dir_all(&dir).is_err() {
            return Self::disabled();
        }
        let mut path = dir.join(format!("{stamp}.log"));
        for n in 2..1000 {
            if !path.exists() {
                break;
            }
            path = dir.join(format!("{stamp}-{n}.log"));
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => RunLog {
                path: Some(path),
                file: std::sync::Mutex::new(Some(f)),
            },
            Err(_) => Self::disabled(),
        }
    }

    /// A log that goes nowhere, for callers that do not want one.
    pub fn disabled() -> Self {
        RunLog {
            path: None,
            file: std::sync::Mutex::new(None),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// Append one line. Takes `&self` so it can be called from anywhere in the
    /// run without threading a mutable borrow through every function.
    pub fn line(&self, text: &str) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "{text}");
            }
        }
    }

    /// Append a block of text, one line at a time so each is prefixed
    /// consistently by the caller.
    pub fn block(&self, text: &str) {
        for line in text.lines() {
            self.line(line);
        }
    }

    /// Flush to disk. Called at the end of a run, and after failures, so a
    /// later crash cannot lose the part that explains what went wrong.
    pub fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
                let _ = f.sync_data();
            }
        }
    }
}

/// Delete all but the newest `keep` logs for a job.
///
/// Log names are timestamps from [`crate::timefmt::stamp`], which sort
/// chronologically as text, so this needs no metadata lookups.
pub fn prune_logs(state_root: &Path, job: &str, keep: usize) -> Result<usize> {
    let dir = SaveState::dir_for(state_root, job).join("logs");
    if !dir.exists() {
        return Ok(0);
    }
    let mut logs: Vec<PathBuf> = fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    if logs.len() <= keep {
        return Ok(0);
    }
    logs.sort();
    let remove = logs.len() - keep;
    let mut removed = 0;
    for p in logs.into_iter().take(remove) {
        if fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// List a job's run logs, newest first.
pub fn list_logs(state_root: &Path, job: &str) -> Result<Vec<PathBuf>> {
    let dir = SaveState::dir_for(state_root, job).join("logs");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut logs: Vec<PathBuf> = fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    logs.sort();
    logs.reverse();
    Ok(logs)
}

/// A job's on-disk save state.
pub struct SaveState {
    dir: PathBuf,
    run: RunState,
    /// Bitset of completed action indices.
    done: Bitset,
    journal: Option<File>,
    unflushed: usize,
}

impl SaveState {
    /// The directory this job's state lives in.
    pub fn dir_for(state_root: &Path, job: &str) -> PathBuf {
        state_root.join(sanitize(job))
    }

    /// Start a fresh run, discarding any previous state for this job.
    pub fn create(state_root: &Path, run: RunState, plan: &Plan) -> Result<Self> {
        let dir = Self::dir_for(state_root, &run.job);
        fs::create_dir_all(&dir)?;

        // A new run supersedes the previous one's artifacts — leaving a stale
        // plan behind would let a later resume execute the wrong work list.
        //
        // Only these named files, though, not the whole directory: `logs/`
        // lives here too and is deliberately a history *across* runs. Wiping
        // the directory would delete the log of the run currently in progress,
        // which is exactly what it did before this was narrowed.
        for f in [
            "run.json",
            "plan.jsonl",
            "done.bin",
            "errors.jsonl",
            "progress.json",
        ] {
            let p = dir.join(f);
            if p.exists() {
                fs::remove_file(&p)?;
            }
        }

        write_plan(&dir.join("plan.jsonl"), plan)?;
        let mut s = SaveState {
            done: Bitset::with_len(plan.actions.len()),
            dir,
            run,
            journal: None,
            unflushed: 0,
        };
        s.open_journal()?;
        s.write_run()?;
        Ok(s)
    }

    /// Load an existing state, if there is one.
    /// Read only the run summary, for a status display. Leaves the plan and
    /// the journal alone, which for a large job are the expensive part.
    pub fn peek(state_root: &Path, job: &str) -> Result<Option<RunState>> {
        let run_path = Self::dir_for(state_root, job).join("run.json");
        if !run_path.exists() {
            return Ok(None);
        }
        let run: RunState = serde_json::from_slice(&fs::read(&run_path)?)
            .map_err(|e| BkError::Other(anyhow::anyhow!("run.json is unreadable: {e}")))?;
        Ok(Some(run))
    }

    pub fn load(state_root: &Path, job: &str) -> Result<Option<(Self, Vec<Action>)>> {
        let dir = Self::dir_for(state_root, job);
        let run_path = dir.join("run.json");
        if !run_path.exists() {
            return Ok(None);
        }
        let run: RunState = serde_json::from_slice(&fs::read(&run_path)?)
            .map_err(|e| BkError::Other(anyhow::anyhow!("run.json is unreadable: {e}")))?;
        if run.version != STATE_VERSION {
            // A state we do not understand is not resumable; a fresh run is
            // always correct, only slower.
            return Ok(None);
        }

        let actions = read_plan(&dir.join("plan.jsonl"))?;
        let mut done = Bitset::with_len(actions.len());
        let completed = read_journal(&dir.join("done.bin"))?;
        for idx in completed {
            done.set(idx as usize);
        }

        let mut s = SaveState {
            dir,
            run,
            done,
            journal: None,
            unflushed: 0,
        };
        s.open_journal()?;
        Ok(Some((s, actions)))
    }

    /// Is this stored state a valid resume point for the job about to run?
    ///
    /// Refuses if the run finished, or if the source/destination/mode differ —
    /// resuming a plan built for a different pair of paths would write the
    /// wrong files to the wrong place.
    pub fn can_resume(&self, source: &Path, dest: &Path, mode: MirrorMode) -> ResumeCheck {
        if !self.run.status.is_resumable() {
            return ResumeCheck::No("the previous run already finished");
        }
        if !same_path(&self.run.source, source) {
            return ResumeCheck::No("the source path changed since that run");
        }
        if !same_path(&self.run.dest, dest) {
            return ResumeCheck::No("the destination path changed since that run");
        }
        if self.run.mode != mode {
            return ResumeCheck::No("the mirror mode changed since that run");
        }
        ResumeCheck::Yes
    }

    /// How old the stored run is, in seconds, relative to `now_unix`.
    pub fn age_secs(&self, now_unix: u64) -> u64 {
        now_unix.saturating_sub(self.run.started_unix)
    }

    pub fn run(&self) -> &RunState {
        &self.run
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Has this action already been completed by an earlier attempt?
    pub fn is_done(&self, index: usize) -> bool {
        self.done.get(index)
    }

    pub fn done_count(&self) -> usize {
        self.done.count()
    }

    /// Record an action as complete. Flushed to disk in batches.
    pub fn mark_done(&mut self, index: usize) -> Result<()> {
        if self.done.get(index) {
            return Ok(());
        }
        self.done.set(index);
        if let Some(f) = self.journal.as_mut() {
            f.write_all(&(index as u32).to_le_bytes())?;
            self.unflushed += 1;
            if self.unflushed >= FLUSH_EVERY {
                f.flush()?;
                f.sync_data()?;
                self.unflushed = 0;
            }
        }
        Ok(())
    }

    /// Force the journal to durable storage.
    pub fn checkpoint(&mut self) -> Result<()> {
        if let Some(f) = self.journal.as_mut() {
            f.flush()?;
            f.sync_data()?;
            self.unflushed = 0;
        }
        Ok(())
    }

    /// Append a failure to `errors.jsonl`.
    pub fn record_error(&self, e: &ActionError) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("errors.jsonl"))?;
        let line = serde_json::to_string(e).map_err(|e| BkError::Other(e.into()))?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    /// Read back everything that failed, for the run report.
    pub fn errors(&self) -> Result<Vec<ActionError>> {
        let path = self.dir.join("errors.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for line in BufReader::new(File::open(path)?)
            .lines()
            .map_while(|l| l.ok())
        {
            // A truncated final line (the run was killed mid-append) is
            // skipped rather than failing the whole report.
            if let Ok(e) = serde_json::from_str(&line) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Publish a progress snapshot for other processes to read.
    pub fn publish_progress(&self, p: &Progress) -> Result<()> {
        write_atomic(
            &self.dir.join("progress.json"),
            &serde_json::to_vec(p).map_err(|e| BkError::Other(e.into()))?,
        )
    }

    /// Read the progress a *running* job last published. Used by `bku status`
    /// and the GUI to watch a scheduled run from outside.
    pub fn read_progress(state_root: &Path, job: &str) -> Result<Option<Progress>> {
        let p = Self::dir_for(state_root, job).join("progress.json");
        if !p.exists() {
            return Ok(None);
        }
        Ok(serde_json::from_slice(&fs::read(p)?).ok())
    }

    /// Move the run to a terminal (or interrupted) status and persist it.
    pub fn set_status(&mut self, status: RunStatus, now_unix: u64) -> Result<()> {
        self.run.status = status;
        if !matches!(status, RunStatus::Running) {
            self.run.finished_unix = Some(now_unix);
        }
        self.checkpoint()?;
        self.write_run()
    }

    pub fn set_attic_stamp(&mut self, stamp: String) -> Result<()> {
        self.run.attic_stamp = Some(stamp);
        self.write_run()
    }

    fn open_journal(&mut self) -> Result<()> {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("done.bin"))?;
        self.journal = Some(f);
        Ok(())
    }

    fn write_run(&self) -> Result<()> {
        write_atomic(
            &self.dir.join("run.json"),
            &serde_json::to_vec_pretty(&self.run).map_err(|e| BkError::Other(e.into()))?,
        )
    }
}

/// Whether a stored state can be resumed, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeCheck {
    Yes,
    No(&'static str),
}

impl ResumeCheck {
    pub fn is_yes(self) -> bool {
        matches!(self, ResumeCheck::Yes)
    }
    pub fn reason(self) -> Option<&'static str> {
        match self {
            ResumeCheck::Yes => None,
            ResumeCheck::No(r) => Some(r),
        }
    }
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// Replace a file's contents atomically: write a temp file, fsync it, rename
/// over the target. A reader always sees either the old file or the new one,
/// never a partial write — the same discipline the chunk store uses.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn write_plan(path: &Path, plan: &Plan) -> Result<()> {
    let mut f = std::io::BufWriter::new(File::create(path)?);
    for a in &plan.actions {
        let line = serde_json::to_string(a).map_err(|e| BkError::Other(e.into()))?;
        writeln!(f, "{line}")?;
    }
    let f = f
        .into_inner()
        .map_err(|e| BkError::Other(anyhow::anyhow!("{e}")))?;
    f.sync_all()?;
    Ok(())
}

fn read_plan(path: &Path) -> Result<Vec<Action>> {
    let f = File::open(path)?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let a: Action = serde_json::from_str(&line)
            .map_err(|e| BkError::Other(anyhow::anyhow!("plan.jsonl line {}: {e}", i + 1)))?;
        out.push(a);
    }
    Ok(out)
}

/// Read completed indices, discarding a torn final record.
fn read_journal(path: &Path) -> Result<Vec<u32>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
    // A crash mid-append can leave a partial record. Every record is exactly
    // REC bytes, so anything past the last whole one is truncated and the
    // affected action is simply redone.
    let whole = buf.len() - (buf.len() % REC);
    Ok(buf[..whole]
        .as_chunks::<REC>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

/// Compare two paths the way the platform does.
fn same_path(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| {
        let s = super::path::display(&super::path::long(p))
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        #[cfg(windows)]
        {
            s.to_lowercase()
        }
        #[cfg(not(windows))]
        {
            s
        }
    };
    n(a) == n(b)
}

/// Make a job name safe to use as a directory name.
fn sanitize(job: &str) -> String {
    let s: String = job
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "job".into()
    } else {
        s
    }
}

/// A compact set of completed action indices.
///
/// A `HashSet<u32>` would cost ~48 bytes per entry; at 20 million actions that
/// is a gigabyte of RAM for bookkeeping alone. One bit each is 2.5 MB.
#[derive(Debug, Default)]
struct Bitset {
    words: Vec<u64>,
    count: usize,
}

impl Bitset {
    fn with_len(n: usize) -> Self {
        Bitset {
            words: vec![0; n.div_ceil(64)],
            count: 0,
        }
    }
    fn set(&mut self, i: usize) {
        let w = i / 64;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let mask = 1u64 << (i % 64);
        if self.words[w] & mask == 0 {
            self.words[w] |= mask;
            self.count += 1;
        }
    }
    fn get(&self, i: usize) -> bool {
        let w = i / 64;
        w < self.words.len() && self.words[w] & (1u64 << (i % 64)) != 0
    }
    fn count(&self) -> usize {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::path::RelPath;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_state_{n}"));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn plan_of(n: usize) -> Plan {
        let mut p = Plan::default();
        for i in 0..n {
            p.actions.push(Action::CopyFile {
                rel: RelPath::from_key_text(&format!("f{i}.txt")),
                size: 10,
            });
        }
        p
    }

    fn run_state(job: &str) -> RunState {
        RunState {
            version: STATE_VERSION,
            job: job.into(),
            source: PathBuf::from("D:/src"),
            dest: PathBuf::from("E:/dst"),
            mode: MirrorMode::Additive,
            started_unix: 1_700_000_000,
            finished_unix: None,
            status: RunStatus::Running,
            total_actions: 0,
            total_bytes: 0,
            attic_stamp: None,
        }
    }

    #[test]
    fn a_run_resumes_from_where_it_stopped() {
        let t = Tmp::new("resume");
        let plan = plan_of(10);
        {
            let mut s = SaveState::create(&t.0, run_state("j"), &plan).unwrap();
            for i in 0..4 {
                s.mark_done(i).unwrap();
            }
            s.checkpoint().unwrap();
            // Dropped without ever setting a terminal status — i.e. killed.
        }

        let (s2, actions) = SaveState::load(&t.0, "j").unwrap().unwrap();
        assert_eq!(actions.len(), 10, "the plan must survive verbatim");
        assert_eq!(s2.done_count(), 4);
        for i in 0..4 {
            assert!(s2.is_done(i), "action {i} was completed before the crash");
        }
        for i in 4..10 {
            assert!(!s2.is_done(i), "action {i} still needs doing");
        }
        assert!(s2
            .can_resume(
                Path::new("D:/src"),
                Path::new("E:/dst"),
                MirrorMode::Additive
            )
            .is_yes());
    }

    #[test]
    fn a_torn_final_journal_record_is_discarded_not_misread() {
        let t = Tmp::new("torn");
        let plan = plan_of(10);
        {
            let mut s = SaveState::create(&t.0, run_state("j"), &plan).unwrap();
            for i in 0..5 {
                s.mark_done(i).unwrap();
            }
            s.checkpoint().unwrap();
        }
        // Simulate a power cut in the middle of appending record 6.
        let jp = SaveState::dir_for(&t.0, "j").join("done.bin");
        let mut bytes = fs::read(&jp).unwrap();
        bytes.extend_from_slice(&[0x09, 0x00]); // half a record
        fs::write(&jp, &bytes).unwrap();

        let (s2, _) = SaveState::load(&t.0, "j").unwrap().unwrap();
        assert_eq!(s2.done_count(), 5, "the partial record must be ignored");
        assert!(!s2.is_done(9), "and must not be read as a completed index");
    }

    #[test]
    fn resume_is_refused_when_the_paths_changed() {
        let t = Tmp::new("paths");
        let _s = SaveState::create(&t.0, run_state("j"), &plan_of(3)).unwrap();
        let (s2, _) = SaveState::load(&t.0, "j").unwrap().unwrap();

        // The trap: resuming a plan built for a different destination would
        // write the wrong files to the wrong disk.
        let bad = s2.can_resume(
            Path::new("D:/src"),
            Path::new("F:/other"),
            MirrorMode::Additive,
        );
        assert!(!bad.is_yes());
        assert!(bad.reason().unwrap().contains("destination"));

        let bad_mode = s2.can_resume(Path::new("D:/src"), Path::new("E:/dst"), MirrorMode::Delete);
        assert!(!bad_mode.is_yes());
    }

    #[cfg(windows)]
    #[test]
    fn resume_path_comparison_is_case_insensitive_on_windows() {
        let t = Tmp::new("case");
        let _s = SaveState::create(&t.0, run_state("j"), &plan_of(1)).unwrap();
        let (s2, _) = SaveState::load(&t.0, "j").unwrap().unwrap();
        assert!(s2
            .can_resume(
                Path::new(r"d:\SRC"),
                Path::new(r"E:\Dst\"),
                MirrorMode::Additive
            )
            .is_yes());
    }

    #[test]
    fn a_completed_run_is_not_resumable() {
        let t = Tmp::new("done");
        {
            let mut s = SaveState::create(&t.0, run_state("j"), &plan_of(3)).unwrap();
            s.set_status(RunStatus::Completed, 1_700_000_100).unwrap();
        }
        let (s2, _) = SaveState::load(&t.0, "j").unwrap().unwrap();
        assert_eq!(s2.run().status, RunStatus::Completed);
        assert_eq!(s2.run().finished_unix, Some(1_700_000_100));
        assert!(!s2
            .can_resume(
                Path::new("D:/src"),
                Path::new("E:/dst"),
                MirrorMode::Additive
            )
            .is_yes());
    }

    #[test]
    fn creating_a_new_run_discards_the_previous_plan() {
        let t = Tmp::new("fresh");
        {
            let mut s = SaveState::create(&t.0, run_state("j"), &plan_of(10)).unwrap();
            s.mark_done(0).unwrap();
            s.checkpoint().unwrap();
        }
        // A new run must not inherit the old completion marks, or it would
        // skip work it never actually did.
        let _s = SaveState::create(&t.0, run_state("j"), &plan_of(4)).unwrap();
        let (s2, actions) = SaveState::load(&t.0, "j").unwrap().unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(s2.done_count(), 0);
    }

    #[test]
    fn errors_and_progress_round_trip() {
        let t = Tmp::new("report");
        let s = SaveState::create(&t.0, run_state("j"), &plan_of(2)).unwrap();
        s.record_error(&ActionError {
            index: 1,
            rel: "a/b.txt".into(),
            message: "access denied".into(),
        })
        .unwrap();
        let errs = s.errors().unwrap();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].rel, "a/b.txt");

        s.publish_progress(&Progress {
            actions_done: 1,
            actions_total: 2,
            bytes_done: 50,
            bytes_total: 100,
            rate_bps: 25,
            ..Default::default()
        })
        .unwrap();
        let p = SaveState::read_progress(&t.0, "j").unwrap().unwrap();
        assert_eq!(p.percent(), 50.0);
        assert_eq!(p.eta_secs(), Some(2));
    }

    #[test]
    fn no_state_means_no_resume_rather_than_an_error() {
        let t = Tmp::new("absent");
        assert!(SaveState::load(&t.0, "never-run").unwrap().is_none());
    }

    #[test]
    fn marking_the_same_action_twice_is_idempotent() {
        let t = Tmp::new("idem");
        let mut s = SaveState::create(&t.0, run_state("j"), &plan_of(3)).unwrap();
        s.mark_done(1).unwrap();
        s.mark_done(1).unwrap();
        assert_eq!(s.done_count(), 1);
    }

    #[test]
    fn bitset_is_compact_and_correct() {
        let mut b = Bitset::with_len(1000);
        assert!(!b.get(0));
        b.set(0);
        b.set(999);
        b.set(999);
        assert!(b.get(0) && b.get(999));
        assert!(!b.get(500));
        assert_eq!(b.count(), 2);
        // Growing past the initial length must not panic.
        b.set(5000);
        assert!(b.get(5000));
    }

    #[test]
    fn job_names_are_sanitized_into_safe_directory_names() {
        // Hyphens and underscores survive; drive colons and arrows do not.
        assert_eq!(sanitize("Nightly D:->E:"), "Nightly_D_-_E_");
        assert_eq!(sanitize(""), "job");
        assert_eq!(sanitize("photos_2024-a"), "photos_2024-a");
    }
}
