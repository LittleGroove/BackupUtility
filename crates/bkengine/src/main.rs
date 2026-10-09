//! bkengine — the headless engine and the command line.
//!
//! This is what the Task Scheduler invokes, what the GUI shells out to, and
//! what an operator drives by hand. The common case is one line:
//!
//! ```text
//!   bkengine mirror --source D:\ --dest E:\
//! ```
//!
//! Everything else — saved jobs, schedules, disk optimization, the block-level
//! imaging path — hangs off that.
//!
//! ## Interrupting a run
//! Ctrl+C is not a kill. It sets the cancellation token, so the workers finish
//! the file they are on, the save state is checkpointed, and the next run picks
//! up exactly where this one stopped.

use bkcore::apply;
use bkcore::backup::{self, BackupParams};
use bkcore::diskio::FileTarget;
use bkcore::file::attic::AtticPolicy;
use bkcore::file::filter::Filter;
use bkcore::file::mirror::{self, Event, MirrorJob, MirrorReport};
use bkcore::file::path::RelPath;
use bkcore::file::plan::{human_bytes, CompareBy, MirrorMode};
use bkcore::file::state::{self as filestate, SaveState};
use bkcore::file::verify;
use bkcore::file::Cancel;
use bkcore::job::{Fidelity, Frequency, Job, JobKind, JobStore, Schedule};
use bkcore::manifest::Manifest;
use bkcore::opt::{self, media, optimize, OptimizeSettings};
use bkcore::settings::Settings;
use bkcore::store::ChunkStore;
use bkcore::timefmt;
use bkcore::update::{self, UpdateEvent};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(
    name = "bkengine",
    version,
    about = "Mirror a path to another drive, with save states and optional disk optimization"
)]
struct Cli {
    /// Directory holding job JSON files.
    #[arg(long, global = true)]
    jobs_dir: Option<PathBuf>,

    /// Suppress the live progress display.
    #[arg(long, short, global = true)]
    quiet: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mirror one path to another, ad hoc.
    Mirror(MirrorArgs),
    /// Run a saved job. This is what a scheduled task invokes.
    Run {
        #[arg(long)]
        job: String,
        /// Plan and report without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the progress of a running (or last-finished) job.
    Status {
        #[arg(long)]
        job: String,
        /// Where run state lives. Needed for an ad-hoc `mirror --name` run,
        /// which has no saved job to read the location from.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Manage saved jobs.
    #[command(subcommand)]
    Jobs(JobsCmd),
    /// Manage a job's schedule.
    #[command(subcommand)]
    Schedule(ScheduleCmd),
    /// List local volumes and what kind of media they are.
    Drives,
    /// Show the version, the platform, and where this tool keeps its files.
    Version,
    /// Check the GitHub releases page for a newer version, and install it.
    Update {
        /// Only report whether an update exists; install nothing.
        #[arg(long)]
        check: bool,
        /// Consider pre-releases too, for this run only.
        #[arg(long)]
        beta: bool,
        /// Install without asking. Required when there is no terminal.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show or change application settings (update checks, beta channel, theme).
    #[command(subcommand)]
    Settings(SettingsCmd),
    /// Defragment or retrim a volume, choosing the operation from the media.
    Optimize {
        /// Volume to optimize, e.g. `D:`.
        #[arg(long)]
        volume: String,
        /// Report what would happen without doing it.
        #[arg(long)]
        dry_run: bool,
        /// Permit a defrag pass on media we could not confirm is spinning.
        #[arg(long)]
        force: bool,
    },
    /// Compare (and optionally align) the physical layout of two trees.
    Layout {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        dest: PathBuf,
        /// Actually move clusters. Without this it only measures.
        #[arg(long)]
        apply: bool,
        /// Stop after this many files.
        #[arg(long)]
        max_files: Option<usize>,
        /// Ignore files smaller than this many clusters.
        #[arg(long)]
        min_clusters: Option<u64>,
    },
    /// Measure fragmentation under a path.
    Analyze {
        #[arg(long)]
        path: PathBuf,
    },

    // --- block-level imaging (the original pipeline) ---
    /// Capture a volume image into the chunk store.
    Backup {
        #[arg(long)]
        job: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Image a live device without a snapshot, accepting that the result
        /// may be inconsistent. Required because VSS is not implemented yet.
        #[arg(long)]
        allow_torn: bool,
    },
    /// List completed images for a job.
    List {
        #[arg(long)]
        job: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Check that a backup still matches its source.
    ///
    /// For a mirror job this compares the two trees; for an image job it
    /// re-reads and re-hashes every stored chunk.
    Verify {
        /// A saved job to check.
        #[arg(long)]
        job: Option<String>,
        /// Ad-hoc source path, used with --dest.
        #[arg(long)]
        source: Option<PathBuf>,
        /// Ad-hoc destination path.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Hash every file rather than comparing size and time. The only
        /// check that catches silent corruption, and it reads both drives.
        #[arg(long)]
        deep: bool,
        /// Chunk store, for image jobs.
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        workers: Option<usize>,
    },
    /// Show a job's run logs.
    Logs {
        #[arg(long)]
        job: String,
        /// Print the newest log instead of listing them.
        #[arg(long)]
        show: bool,
        /// Where run state lives, for an ad-hoc run.
        #[arg(long)]
        state_root: Option<PathBuf>,
    },
    /// Restore an image to a file (non-destructive).
    Restore {
        #[arg(long)]
        job: String,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        target: PathBuf,
        #[arg(long)]
        from: Option<String>,
    },
    /// Clone an image onto a raw physical disk. DESTRUCTIVE.
    Clone {
        #[arg(long)]
        job: String,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Target device, e.g. \\.\PhysicalDrive9.
        #[arg(long)]
        target: String,
        #[arg(long)]
        from: Option<String>,
        /// Write only blocks that differ from what is already there.
        #[arg(long)]
        delta: bool,
        /// Required acknowledgement that this ERASES the target device.
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
enum SettingsCmd {
    /// Print every setting and where the settings file lives.
    Show,
    /// Change one setting, e.g. `settings set beta_updates true`.
    Set { key: String, value: String },
}

#[derive(Args)]
struct MirrorArgs {
    /// The path to back up. Usually a drive root, sometimes a folder.
    #[arg(long)]
    source: PathBuf,
    /// Where it goes.
    #[arg(long)]
    dest: PathBuf,
    /// A name for the run, so it can be resumed and its state found.
    #[arg(long, default_value = "adhoc")]
    name: String,
    /// What happens to destination files the source no longer has.
    #[arg(long, value_enum, default_value_t = ModeArg::Additive)]
    mode: ModeArg,
    /// How hard to look before deciding a file is unchanged.
    #[arg(long, value_enum, default_value_t = CompareArg::SizeAndTime)]
    compare: CompareArg,
    /// Plan and report without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Re-read and hash every file after writing it.
    #[arg(long)]
    verify: bool,
    /// Exclusion pattern; repeatable. Gitignore-style.
    #[arg(long)]
    exclude: Vec<String>,
    /// Inclusion pattern; repeatable. If given, only matches are copied.
    #[arg(long)]
    include: Vec<String>,
    /// Concurrent file copies.
    #[arg(long)]
    workers: Option<usize>,
    /// Start over rather than resuming an interrupted run.
    #[arg(long)]
    no_resume: bool,
    /// Preserve hard links (costs one extra open per file).
    #[arg(long)]
    hardlinks: bool,
    /// Follow symlinks and junctions instead of re-creating them.
    #[arg(long)]
    follow_links: bool,
    /// Copy data and timestamps only — for a FAT/exFAT or network destination.
    #[arg(long)]
    data_only: bool,
    /// Where run state lives.
    #[arg(long)]
    state_root: Option<PathBuf>,
    /// Where displaced files go in attic mode.
    #[arg(long)]
    attic_root: Option<PathBuf>,
    /// Keep at most this many attic folders. 0 means keep all of them.
    #[arg(long)]
    attic_keep: Option<usize>,
    /// Delete attic folders older than this many days.
    #[arg(long)]
    attic_days: Option<u64>,
    /// Start even when the destination looks too small to hold the copy.
    #[arg(long)]
    no_space_check: bool,
    /// Do not write a run log.
    #[arg(long)]
    no_log: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Never remove anything from the destination.
    Additive,
    /// Move vanished files into a timestamped attic folder.
    Attic,
    /// Delete vanished files, so the trees match exactly.
    Delete,
}

impl From<ModeArg> for MirrorMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Additive => MirrorMode::Additive,
            ModeArg::Attic => MirrorMode::Attic,
            ModeArg::Delete => MirrorMode::Delete,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CompareArg {
    /// Same size and modification time. Fast.
    SizeAndTime,
    /// Same size only.
    SizeOnly,
    /// Hash both files. Slow, but catches an in-place rewrite.
    Content,
}

impl From<CompareArg> for CompareBy {
    fn from(c: CompareArg) -> Self {
        match c {
            CompareArg::SizeAndTime => CompareBy::SizeAndTime,
            CompareArg::SizeOnly => CompareBy::SizeOnly,
            CompareArg::Content => CompareBy::Content,
        }
    }
}

#[derive(Subcommand)]
enum JobsCmd {
    /// List saved jobs.
    List,
    /// Print one job's configuration.
    Show {
        #[arg(long)]
        job: String,
    },
    /// Create or replace a mirror job.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        dest: PathBuf,
        #[arg(long, value_enum, default_value_t = ModeArg::Additive)]
        mode: ModeArg,
        #[arg(long)]
        exclude: Vec<String>,
        /// Schedule frequency; omit for a manual-only job.
        #[arg(long, value_enum)]
        every: Option<FreqArg>,
        /// 24-hour time for daily/weekly schedules.
        #[arg(long, default_value = "03:00")]
        at: String,
        /// Day for a weekly schedule, e.g. SUN.
        #[arg(long)]
        day: Option<String>,
        /// Keep the two drives structurally identical (optimize + layout).
        #[arg(long)]
        identical_layout: bool,
        /// Re-read and hash every file after writing.
        #[arg(long)]
        verify: bool,
    },
    /// Delete a job and its scheduled task.
    Remove {
        #[arg(long)]
        job: String,
    },
    /// Enable a disabled job.
    Enable {
        #[arg(long)]
        job: String,
    },
    /// Disable a job without deleting it.
    Disable {
        #[arg(long)]
        job: String,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum FreqArg {
    Hourly,
    Daily,
    Weekly,
    OnLogon,
    OnStart,
}

impl From<FreqArg> for Frequency {
    fn from(f: FreqArg) -> Self {
        match f {
            FreqArg::Hourly => Frequency::Hourly,
            FreqArg::Daily => Frequency::Daily,
            FreqArg::Weekly => Frequency::Weekly,
            FreqArg::OnLogon => Frequency::OnLogon,
            FreqArg::OnStart => Frequency::OnStart,
        }
    }
}

#[derive(Subcommand)]
enum ScheduleCmd {
    /// Register the job's scheduled task.
    Install {
        #[arg(long)]
        job: String,
    },
    /// Remove the job's scheduled task.
    Remove {
        #[arg(long)]
        job: String,
    },
    /// Show whether a task is registered.
    Status {
        #[arg(long)]
        job: String,
    },
    /// Print the task definition without registering it.
    Preview {
        #[arg(long)]
        job: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    bkcore::file::sys::reset_sigpipe();
    let cli = Cli::parse();
    let cancel = install_signal_handler();

    // Sweep what an earlier self-update parked beside the binaries. Cheap: one
    // directory listing. Best effort: a file still held open is left for later.
    if let Ok(dir) = bkcore::paths::install_dir() {
        let _ = update::cleanup_old_binaries(&dir);
    }

    match cli.cmd {
        Cmd::Mirror(args) => cmd_mirror(&args, &cancel, cli.quiet),
        Cmd::Run { job, dry_run } => cmd_run(&cli.jobs_dir, &job, dry_run, &cancel, cli.quiet),
        Cmd::Status { job, state_root } => cmd_status(&cli.jobs_dir, &job, state_root),
        Cmd::Jobs(c) => cmd_jobs(&cli.jobs_dir, c),
        Cmd::Schedule(c) => cmd_schedule(&cli.jobs_dir, c),
        Cmd::Drives => cmd_drives(),
        Cmd::Version => cmd_version(),
        Cmd::Update { check, beta, yes } => cmd_update(check, beta, yes),
        Cmd::Settings(c) => cmd_settings(c),
        Cmd::Optimize {
            volume,
            dry_run,
            force,
        } => cmd_optimize(&volume, dry_run, force, &cancel),
        Cmd::Layout {
            source,
            dest,
            apply,
            max_files,
            min_clusters,
        } => cmd_layout(&source, &dest, apply, max_files, min_clusters, &cancel),
        Cmd::Analyze { path } => cmd_analyze(&path),

        Cmd::Backup {
            job,
            source,
            store,
            allow_torn,
        } => cmd_backup(&cli.jobs_dir, &job, source, store, allow_torn),
        Cmd::List { job, store } => cmd_list(&cli.jobs_dir, &job, store),
        Cmd::Verify {
            job,
            source,
            dest,
            deep,
            store,
            workers,
        } => cmd_verify(
            &cli.jobs_dir,
            job,
            source,
            dest,
            deep,
            store,
            workers,
            &cancel,
        ),
        Cmd::Logs {
            job,
            show,
            state_root,
        } => cmd_logs(&cli.jobs_dir, &job, show, state_root),
        Cmd::Restore {
            job,
            store,
            target,
            from,
        } => cmd_restore(&cli.jobs_dir, &job, store, target, from),
        Cmd::Clone {
            job,
            store,
            target,
            from,
            delta,
            confirm,
        } => cmd_clone(&cli.jobs_dir, &job, store, target, from, delta, confirm),
    }
}

/// Turn Ctrl+C into a cancellation rather than a kill.
///
/// A killed process leaves the save state un-checkpointed, so the next run
/// redoes more work than it needed to. Cancelling lets the engine finish the
/// file it is on and flush the journal, and the run resumes cleanly.
fn install_signal_handler() -> Cancel {
    let cancel = Cancel::new();
    let handler = cancel.clone();
    let already_asked = std::sync::atomic::AtomicBool::new(false);
    let _ = ctrlc::set_handler(move || {
        if already_asked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            // A second Ctrl+C means they want out now.
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!(
            "\nstopping — finishing the current file and saving progress (Ctrl+C again to force)"
        );
        handler.cancel();
    });
    cancel
}

// ---------------------------------------------------------------------------
// Mirror
// ---------------------------------------------------------------------------

fn cmd_mirror(args: &MirrorArgs, cancel: &Cancel, quiet: bool) -> anyhow::Result<()> {
    let mut job = MirrorJob::new(&args.name, &args.source, &args.dest);
    job.mode = args.mode.into();
    job.compare = args.compare.into();
    job.dry_run = args.dry_run;
    job.resume = !args.no_resume;
    job.scan.detect_hardlinks = args.hardlinks;
    job.scan.follow_links = args.follow_links;
    job.filter = Filter {
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        ..Filter::permissive()
    };
    job.copy = if args.data_only {
        Fidelity::data_only().to_copy_options()
    } else {
        job.copy
    };
    job.copy.verify = args.verify;
    if let Some(w) = args.workers {
        job.workers = w;
    }
    if let Some(s) = &args.state_root {
        job.state_root = s.clone();
    }
    job.attic_root = args.attic_root.clone();
    job.check_free_space = !args.no_space_check;
    job.log = !args.no_log;
    if args.attic_keep.is_some() || args.attic_days.is_some() {
        job.attic_policy = AtticPolicy {
            // `--attic-keep 0` reads as "no limit", which is friendlier than
            // making it mean "delete everything".
            keep_runs: args.attic_keep.filter(|n| *n > 0),
            max_age_days: args.attic_days,
            max_bytes: None,
        };
    }

    let report = run_mirror(&job, cancel, quiet)?;
    print!("{}", report.summary());
    finish(&report)
}

fn cmd_run(
    jobs_dir: &Option<PathBuf>,
    name: &str,
    dry_run: bool,
    cancel: &Cancel,
    quiet: bool,
) -> anyhow::Result<()> {
    let store = job_store(jobs_dir)?;
    let job = store.load(name)?;
    if job.disabled {
        println!("job '{name}' is disabled; nothing to do");
        return Ok(());
    }
    job.validate()?;

    match &job.kind {
        JobKind::Mirror(_) => {
            let mut mj = job.to_mirror_job()?;
            mj.dry_run = dry_run;
            let report = run_mirror(&mj, cancel, quiet)?;
            print!("{}", report.summary());

            // The optimization stage runs only if the copy succeeded — there
            // is no sense rearranging clusters under a half-finished mirror.
            if let Some(settings) = &job.optimize {
                if settings.is_enabled() && report.succeeded() && !dry_run {
                    // Hand the layout pass the list the mirror already built,
                    // rather than walking a multi-terabyte tree a second time.
                    // Deliberately not fatal. The backup is the job; disk
                    // optimization is maintenance layered on top. Failing the
                    // whole run because a retrim needed Administrator would
                    // report a successful backup as a failure, and a nightly
                    // task that cries wolf gets ignored.
                    if let Err(e) =
                        run_optimize_stage(&job, settings, &report.file_list, cancel, quiet)
                    {
                        eprintln!("warning: the optimization stage did not run: {e}");
                    }
                }
            }
            finish(&report)
        }
        JobKind::Image(spec) => cmd_backup(
            jobs_dir,
            &job.name,
            Some(spec.source.clone()),
            Some(spec.store_root.clone()),
            false,
        ),
    }
}

/// Run a mirror with a live progress display.
fn run_mirror(job: &MirrorJob, cancel: &Cancel, quiet: bool) -> anyhow::Result<MirrorReport> {
    let interactive = std::io::stderr().is_terminal() && !quiet;
    let started = Instant::now();
    let mut last_line_len = 0usize;
    // A scheduled run has no terminal, so it logs a line every so often
    // instead of redrawing one — otherwise the log fills with control codes.
    let mut last_log = Instant::now();

    let report = mirror::run(job, cancel, &mut |event| match event {
        Event::ScanningSource(s) => {
            if interactive {
                last_line_len = redraw(
                    &format!(
                        "scanning source: {} files, {} dirs, {}",
                        s.files,
                        s.dirs,
                        human_bytes(s.bytes)
                    ),
                    last_line_len,
                );
            }
        }
        Event::ScanningDest(s) => {
            if interactive {
                last_line_len = redraw(
                    &format!("scanning destination: {} files, {} dirs", s.files, s.dirs),
                    last_line_len,
                );
            }
        }
        Event::Planned(plan) => {
            if interactive {
                clear(last_line_len);
                last_line_len = 0;
            }
            if !quiet {
                println!("plan for '{}': {} actions", job.name, plan.len());
                print!("{}", bkcore::file::plan::summarize(plan, job.mode));
                for c in plan.conflicts.iter().take(10) {
                    println!("  conflict: {} — {}", c.rel, c.reason);
                }
            }
        }
        Event::Resuming { done, total } => {
            if !quiet {
                println!("resuming an interrupted run: {done} of {total} actions already done");
            }
        }
        Event::Progress(p) => {
            if interactive {
                let eta = p
                    .eta_secs()
                    .map(timefmt::duration)
                    .unwrap_or_else(|| "--".into());
                last_line_len = redraw(
                    &format!(
                        "{:>5.1}%  {} / {}  {}/s  eta {}  {}",
                        p.percent(),
                        human_bytes(p.bytes_done),
                        human_bytes(p.bytes_total),
                        human_bytes(p.rate_bps),
                        eta,
                        p.current.as_deref().unwrap_or("")
                    ),
                    last_line_len,
                );
            } else if !quiet && last_log.elapsed().as_secs() >= 30 {
                last_log = Instant::now();
                println!(
                    "  {:>5.1}%  {} of {}  ({} files)",
                    p.percent(),
                    human_bytes(p.bytes_done),
                    human_bytes(p.bytes_total),
                    p.files_copied
                );
            }
        }
        Event::Failed { rel, message } => {
            if interactive {
                clear(last_line_len);
                last_line_len = 0;
            }
            eprintln!("  failed: {rel} — {message}");
        }
    })?;

    if interactive {
        clear(last_line_len);
    }
    let _ = started;
    Ok(report)
}

/// Redraw a single status line in place. Returns its length so the next
/// redraw can erase whatever it leaves behind.
fn redraw(text: &str, previous_len: usize) -> usize {
    let mut err = std::io::stderr();
    let padding = previous_len.saturating_sub(text.chars().count());
    let _ = write!(err, "\r{text}{}", " ".repeat(padding));
    let _ = err.flush();
    text.chars().count()
}

fn clear(len: usize) {
    if len > 0 {
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{}\r", " ".repeat(len));
        let _ = err.flush();
    }
}

/// Exit non-zero when a run did not fully succeed, so the Task Scheduler's
/// "Last Run Result" column tells the truth.
fn finish(report: &MirrorReport) -> anyhow::Result<()> {
    if report.actions_failed > 0 {
        anyhow::bail!(
            "{} of {} actions failed — see the errors above",
            report.actions_failed,
            report.actions_total
        );
    }
    if report.status == bkcore::file::state::RunStatus::Cancelled {
        anyhow::bail!("run was cancelled; re-run to resume from where it stopped");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Optimization stage
// ---------------------------------------------------------------------------

fn run_optimize_stage(
    job: &Job,
    settings: &OptimizeSettings,
    known_files: &[RelPath],
    cancel: &Cancel,
    quiet: bool,
) -> anyhow::Result<()> {
    let spec = job.as_mirror().expect("checked by the caller");
    // Prefer the list the mirror already produced; only walk the destination
    // when we were not given one (an ad-hoc invocation).
    let scanned;
    let files: &[RelPath] = if known_files.is_empty() {
        scanned = list_files(&spec.dest)?;
        &scanned
    } else {
        known_files
    };
    let report = opt::run(
        &spec.source,
        &spec.dest,
        files,
        settings,
        cancel,
        &mut |stage| {
            if !quiet {
                println!("  {stage}...");
            }
        },
    )?;
    print!("{}", report.summary());
    Ok(())
}

/// Relative paths of every file under `root`.
fn list_files(root: &Path) -> anyhow::Result<Vec<RelPath>> {
    let tree = bkcore::file::walk::scan(
        root,
        &Filter::permissive(),
        &bkcore::file::walk::ScanOptions::default(),
        &Cancel::none(),
        &mut |_| {},
    )?;
    Ok(tree
        .entries
        .into_iter()
        .filter(|e| !e.is_dir() && !e.is_link())
        .map(|e| e.rel)
        .collect())
}

// ---------------------------------------------------------------------------
// Status, jobs, schedules
// ---------------------------------------------------------------------------

fn cmd_status(
    jobs_dir: &Option<PathBuf>,
    name: &str,
    state_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    // An explicit flag wins; otherwise take it from the saved job; otherwise
    // fall back to the machine default.
    let state_root = state_root
        .or_else(|| {
            job_store(jobs_dir)
                .ok()
                .and_then(|s| s.load(name).ok())
                .and_then(|j| j.state_root)
        })
        .unwrap_or_else(bkcore::file::state::default_state_root);

    match SaveState::load(&state_root, name)? {
        None => println!("job '{name}' has never run"),
        Some((state, actions)) => {
            let run = state.run();
            println!("job '{name}'");
            println!("  status     {:?}", run.status);
            println!("  started    {}", timefmt::readable(run.started_unix));
            if let Some(f) = run.finished_unix {
                println!("  finished   {}", timefmt::readable(f));
            }
            println!(
                "  progress   {} of {} actions",
                state.done_count(),
                actions.len()
            );
            if let Some(p) = SaveState::read_progress(&state_root, name)? {
                println!(
                    "  bytes      {} of {} ({:.1}%)",
                    human_bytes(p.bytes_done),
                    human_bytes(p.bytes_total),
                    p.percent()
                );
                if let Some(c) = &p.current {
                    println!("  current    {c}");
                }
            }
            let errors = state.errors()?;
            if !errors.is_empty() {
                println!("  errors     {}", errors.len());
                for e in errors.iter().take(10) {
                    println!("    {} — {}", e.rel, e.message);
                }
            }
            match filestate::list_logs(&state_root, name) {
                Ok(logs) if !logs.is_empty() => {
                    println!("  log        {}", logs[0].display());
                    if logs.len() > 1 {
                        println!(
                            "             ({} older, see `bkengine logs`)",
                            logs.len() - 1
                        );
                    }
                }
                _ => {}
            }
            if run.status.is_resumable() {
                println!("\n  this run can be resumed: bkengine run --job {name}");
            }
        }
    }
    Ok(())
}

fn job_store(jobs_dir: &Option<PathBuf>) -> anyhow::Result<JobStore> {
    let dir = jobs_dir
        .clone()
        .unwrap_or_else(bkcore::job::default_jobs_dir);
    Ok(JobStore::open(dir)?)
}

fn cmd_jobs(jobs_dir: &Option<PathBuf>, cmd: JobsCmd) -> anyhow::Result<()> {
    let store = job_store(jobs_dir)?;
    match cmd {
        JobsCmd::List => {
            let (jobs, broken) = store.list()?;
            if jobs.is_empty() && broken.is_empty() {
                println!("no jobs in {}", store.dir().display());
                return Ok(());
            }
            for j in &jobs {
                let what = match &j.kind {
                    JobKind::Mirror(m) => format!(
                        "{} -> {}",
                        bkcore::file::path::display(&m.source),
                        bkcore::file::path::display(&m.dest)
                    ),
                    JobKind::Image(i) => {
                        format!("image {} -> {}", i.source, i.store_root.display())
                    }
                };
                let sched = j
                    .schedule
                    .as_ref()
                    .map(|s| format!("{:?} {}", s.frequency, s.at))
                    .unwrap_or_else(|| "manual".into());
                println!(
                    "{}{:<20} {:<50} {}",
                    if j.disabled { "[off] " } else { "      " },
                    j.name,
                    what,
                    sched
                );
            }
            for (path, err) in &broken {
                eprintln!("  unreadable: {} — {err}", path.display());
            }
        }
        JobsCmd::Show { job } => {
            let j = store.load(&job)?;
            println!("{}", serde_json::to_string_pretty(&j)?);
            match j.validate() {
                Ok(()) => println!("\n(valid)"),
                Err(e) => println!("\nWARNING: {e}"),
            }
        }
        JobsCmd::Add {
            name,
            source,
            dest,
            mode,
            exclude,
            every,
            at,
            day,
            identical_layout,
            verify,
        } => {
            let mut job = Job::mirror(&name, &source, &dest);
            if let JobKind::Mirror(m) = &mut job.kind {
                m.mode = mode.into();
                m.filter.exclude = exclude;
                m.fidelity.verify = verify;
            }
            if let Some(freq) = every {
                job.schedule = Some(Schedule {
                    frequency: freq.into(),
                    at,
                    day,
                    // A server job should run with nobody logged in.
                    run_as_system: true,
                    run_if_missed: true,
                    only_on_ac_power: false,
                });
            }
            if identical_layout {
                job.optimize = Some(OptimizeSettings::identical_drives());
            }
            job.validate()?;
            let path = store.save(&job)?;
            println!("saved job '{name}' -> {}", path.display());
            if job.schedule.is_some() {
                println!("register it with: bkengine schedule install --job {name}");
            }
        }
        JobsCmd::Remove { job } => {
            if let Ok(j) = store.load(&job) {
                let _ = bkcore::scheduler::unregister(&j);
            }
            store.delete(&job)?;
            println!("removed job '{job}'");
        }
        JobsCmd::Enable { job } => set_disabled(&store, &job, false)?,
        JobsCmd::Disable { job } => set_disabled(&store, &job, true)?,
    }
    Ok(())
}

fn set_disabled(store: &JobStore, name: &str, disabled: bool) -> anyhow::Result<()> {
    let mut job = store.load(name)?;
    job.disabled = disabled;
    store.save(&job)?;
    // Keep the scheduled task in step, or a "disabled" job keeps running.
    if job.schedule.is_some() {
        let exe = std::env::current_exe()?;
        let _ = bkcore::scheduler::register(&job, &exe, Some(store.dir()));
    }
    println!(
        "job '{name}' is now {}",
        if disabled { "disabled" } else { "enabled" }
    );
    Ok(())
}

fn cmd_schedule(jobs_dir: &Option<PathBuf>, cmd: ScheduleCmd) -> anyhow::Result<()> {
    let store = job_store(jobs_dir)?;
    let exe = std::env::current_exe()?;
    match cmd {
        ScheduleCmd::Install { job } => {
            let j = store.load(&job)?;
            if j.schedule.is_none() {
                anyhow::bail!(
                    "job '{job}' has no schedule; add one with `bkengine jobs add --every daily`"
                );
            }
            bkcore::scheduler::register(&j, &exe, Some(store.dir()))?;
            println!("registered {}", bkcore::scheduler::describe(&j));
        }
        ScheduleCmd::Remove { job } => {
            let j = store.load(&job)?;
            bkcore::scheduler::unregister(&j)?;
            println!("removed {}", bkcore::scheduler::describe(&j));
        }
        ScheduleCmd::Status { job } => {
            let j = store.load(&job)?;
            println!(
                "job '{}': {} is {}",
                j.name,
                bkcore::scheduler::describe(&j),
                if bkcore::scheduler::is_registered(&j) {
                    "registered"
                } else {
                    "not registered"
                }
            );
        }
        ScheduleCmd::Preview { job } => {
            let j = store.load(&job)?;
            #[cfg(windows)]
            print!(
                "{}",
                bkcore::scheduler::task_xml(&j, &exe, Some(store.dir()))?
            );
            #[cfg(not(windows))]
            {
                println!("--- bku-{}.service ---", j.name);
                print!(
                    "{}",
                    bkcore::scheduler::systemd_service(
                        &j,
                        &exe,
                        Some(store.dir()),
                        bkcore::scheduler::unit_location().1
                    )
                );
                println!("--- bku-{}.timer ---", j.name);
                print!("{}", bkcore::scheduler::systemd_timer(&j)?);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Disks
// ---------------------------------------------------------------------------

fn cmd_drives() -> anyhow::Result<()> {
    let volumes = media::list_volumes();
    if volumes.is_empty() {
        println!("no volumes found");
        return Ok(());
    }
    let width = volumes
        .iter()
        .map(|v| v.volume.chars().count())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:<width$}  media    filesystem  {:>10}  {:>10}  recommended",
        "volume", "free", "size"
    );
    for info in &volumes {
        println!(
            "{:<width$}  {:<8} {:<11} {:>10}  {:>10}  {}",
            info.volume,
            info.kind.to_string(),
            info.filesystem.as_deref().unwrap_or("?"),
            info.free_bytes()
                .map(human_bytes)
                .unwrap_or_else(|| "?".into()),
            info.total_bytes()
                .map(human_bytes)
                .unwrap_or_else(|| "?".into()),
            media::recommended_op(info)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Version, updates, settings
// ---------------------------------------------------------------------------

fn cmd_version() -> anyhow::Result<()> {
    println!("bkengine {} ({})", bkcore::VERSION, update::platform_tag());
    println!(
        "  executable  {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!("  config      {}", bkcore::paths::config_root().display());
    println!(
        "  jobs        {}",
        bkcore::job::default_jobs_dir().display()
    );
    println!("  state       {}", bkcore::paths::state_root().display());
    println!("  settings    {}", Settings::path().display());
    println!(
        "  privileges  {}",
        if bkcore::file::sys::is_elevated() {
            if cfg!(windows) {
                "elevated (Administrator)"
            } else {
                "root"
            }
        } else if cfg!(windows) {
            "not elevated"
        } else {
            "user"
        }
    );
    println!("  releases    {}", update::RELEASES_URL);
    Ok(())
}

fn cmd_update(check_only: bool, beta: bool, yes: bool) -> anyhow::Result<()> {
    let mut settings = Settings::load();
    let beta = beta || settings.beta_updates;
    let updater = update::Updater::github();
    println!(
        "checking {} for a newer release{}...",
        update::RELEASES_URL,
        if beta { " (betas included)" } else { "" }
    );
    let found = updater.check(beta)?;
    settings.last_update_check_unix = Some(timefmt::now_unix());
    let _ = settings.save();

    let Some(release) = found else {
        println!("you have the newest version ({})", bkcore::VERSION);
        return Ok(());
    };
    println!(
        "version {} is available (you have {}){}",
        release.version,
        bkcore::VERSION,
        if release.prerelease {
            "  [pre-release]"
        } else {
            ""
        }
    );
    if !release.notes.trim().is_empty() {
        println!();
        for line in release.notes.lines() {
            println!("    {line}");
        }
        println!();
    }
    if check_only {
        println!("install it with: bkengine update");
        return Ok(());
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("no terminal to ask on; pass --yes to install without a prompt");
        }
        print!("install it now? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("not installed");
            return Ok(());
        }
    }

    let mut last_line_len = 0usize;
    let applied = update::apply(&updater, &release, &mut |event| match event {
        UpdateEvent::Downloading { name, done, total } => {
            let text = match total {
                Some(t) if t > 0 => format!(
                    "  downloading {name}: {} of {} ({:.0}%)",
                    human_bytes(done),
                    human_bytes(t),
                    done as f64 / t as f64 * 100.0
                ),
                _ => format!("  downloading {name}: {}", human_bytes(done)),
            };
            last_line_len = redraw(&text, last_line_len);
        }
        UpdateEvent::Verifying(name) => {
            clear(last_line_len);
            last_line_len = 0;
            println!("  verifying {name}");
        }
        UpdateEvent::Installing(name) => println!("  installing {name}"),
    })?;
    clear(last_line_len);
    println!("updated to {}:", applied.version);
    for p in &applied.replaced {
        println!("  {}", p.display());
    }
    if applied.relaunch.is_some() {
        println!("the new engine runs from your next command on");
    }
    Ok(())
}

fn cmd_settings(cmd: SettingsCmd) -> anyhow::Result<()> {
    match cmd {
        SettingsCmd::Show => {
            let s = Settings::load();
            println!("{}", Settings::path().display());
            for (k, v) in s.entries() {
                println!("  {k:<24} {v}");
            }
            if let Some(t) = s.last_update_check_unix {
                println!("  {:<24} {}", "last_update_check", timefmt::readable(t));
            }
        }
        SettingsCmd::Set { key, value } => {
            let mut s = Settings::load();
            s.set(&key, &value)?;
            let path = s.save()?;
            println!("{key} = {value}  (saved to {})", path.display());
        }
    }
    Ok(())
}

fn cmd_optimize(volume: &str, dry_run: bool, force: bool, cancel: &Cancel) -> anyhow::Result<()> {
    let info = media::probe(volume)?;
    println!("{}", info.describe());
    let opts = optimize::OptimizeOptions {
        dry_run,
        allow_defrag_on_unknown_media: force,
        ..Default::default()
    };
    let result = optimize::run(&info, &opts, cancel)?;
    print!("{}", result.summary());
    if !result.output.trim().is_empty() {
        println!("---");
        println!("{}", result.output.trim());
    }
    if !result.succeeded() {
        anyhow::bail!("optimization did not complete successfully");
    }
    Ok(())
}

fn cmd_layout(
    source: &Path,
    dest: &Path,
    apply_moves: bool,
    max_files: Option<usize>,
    min_clusters: Option<u64>,
    cancel: &Cancel,
) -> anyhow::Result<()> {
    let dest_volume = media::probe(&opt::volume_of(dest))?;
    println!("destination: {}", dest_volume.describe());

    let files = list_files(dest)?;
    println!("comparing {} files", files.len());

    let opts = opt::layout::LayoutOptions {
        dry_run: !apply_moves,
        max_files,
        min_clusters: min_clusters.unwrap_or(opt::layout::LayoutOptions::default().min_clusters),
        ..Default::default()
    };
    let report = opt::layout::run(
        source,
        dest,
        &dest_volume,
        &files,
        &opts,
        cancel,
        &mut |_| {},
    )?;
    print!("{}", report.describe());
    if !apply_moves && report.refused.is_none() {
        println!("  (re-run with --apply to move clusters)");
    }
    Ok(())
}

fn cmd_analyze(path: &Path) -> anyhow::Result<()> {
    let files = list_files(path)?;
    println!("analyzing {} files under {}", files.len(), path.display());
    let paths: Vec<PathBuf> = files.iter().map(|r| r.under(path)).collect();
    let report = opt::extents::analyze(paths.iter().map(|p| p.as_path()));
    print!("{}", report.describe());
    Ok(())
}

// ---------------------------------------------------------------------------
// Block-level imaging (unchanged pipeline, adapted to the new job model)
// ---------------------------------------------------------------------------

fn load_image_spec(
    jobs_dir: &Option<PathBuf>,
    name: &str,
) -> anyhow::Result<bkcore::job::ImageSpec> {
    let store = job_store(jobs_dir)?;
    let job = store.load(name)?;
    job.as_image()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("job '{name}' is not an image job"))
}

fn store_root_for(
    jobs_dir: &Option<PathBuf>,
    job: &str,
    store: Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    match store {
        Some(s) => Ok(s),
        None => Ok(load_image_spec(jobs_dir, job)?.store_root),
    }
}

/// Map a user-facing source ("C:") to a readable device path.
fn resolve_source(source: &str) -> String {
    if source.len() == 2 && source.ends_with(':') && source.as_bytes()[0].is_ascii_alphabetic() {
        format!(r"\\.\{source}")
    } else {
        source.to_string()
    }
}

fn cmd_backup(
    jobs_dir: &Option<PathBuf>,
    job: &str,
    source: Option<String>,
    store: Option<PathBuf>,
    allow_torn: bool,
) -> anyhow::Result<()> {
    let (source_device, source_label, store_root, verify_after) = match (source, store) {
        (Some(src), Some(store_root)) => (resolve_source(&src), src, store_root, true),
        _ => {
            let spec = load_image_spec(jobs_dir, job)?;
            (
                resolve_source(&spec.source),
                spec.source.clone(),
                spec.store_root.clone(),
                spec.verify_after,
            )
        }
    };

    let is_device = source_device.starts_with(r"\\.\") || source_device.starts_with(r"\\?\");
    if is_device && !bkcore::vss::is_elevated() {
        anyhow::bail!(
            "backing up a live volume requires Administrator (raw disk access). \
             Re-run elevated, or use the GUI which requests elevation on launch."
        );
    }

    // Reading a mounted volume while Windows is writing to it produces a torn
    // image: files change underneath the read and the result may not even be
    // mountable. VSS exists to prevent exactly this and is not implemented
    // yet, so refuse rather than hand back an image that looks fine and is
    // not restorable.
    if is_device && !bkcore::vss::AVAILABLE && !allow_torn {
        anyhow::bail!(
            "refusing to image {source_label}: a consistent snapshot needs VSS, which is \
             not implemented yet, so reading a live volume can produce an \
             inconsistent image that will not restore.\n\
             \n\
             Either back up a quiesced device (an unmounted disk, or a VHD \
             nothing is writing to) and pass --allow-torn, or use a file-level \
             mirror instead:\n\
             \n\
             \x20   bkengine mirror --source <path> --dest <path>"
        );
    }

    let params = BackupParams {
        job_name: job,
        source_device: &source_device,
        source_label: &source_label,
        store_root: &store_root,
        created_unix: timefmt::now_unix(),
        verify_after,
    };

    let s = backup::run(&params)?;
    println!(
        "Backup '{}' -> {}\n  {} chunks total | {} new | {} unchanged (delta) | {} read | verified: {}",
        job,
        s.manifest_path.display(),
        s.total_chunks,
        s.new_chunks,
        s.deduped_chunks,
        human_bytes(s.bytes_read),
        s.verified,
    );
    Ok(())
}

fn cmd_list(jobs_dir: &Option<PathBuf>, job: &str, store: Option<PathBuf>) -> anyhow::Result<()> {
    let root = store_root_for(jobs_dir, job, store)?;
    let store = ChunkStore::open(&root)?;
    let manifests = store.list_manifests(job)?;
    if manifests.is_empty() {
        println!("No backups found for job '{job}' in {}", root.display());
        return Ok(());
    }
    println!("Backups for '{job}':");
    for m in &manifests {
        println!(
            "  {}-{}  | {} | {} chunks | {} | parent: {}",
            m.job,
            m.created_unix,
            timefmt::readable(m.created_unix),
            m.chunks.len(),
            human_bytes(m.volume_size),
            m.parent.as_deref().unwrap_or("(full)"),
        );
    }
    Ok(())
}

/// Check that a backup still matches its source.
///
/// Three shapes: an ad-hoc pair of paths, a saved mirror job, or a saved image
/// job. They answer the same question by different means, so they share one
/// command rather than making the user remember which is which.
#[allow(clippy::too_many_arguments)]
fn cmd_verify(
    jobs_dir: &Option<PathBuf>,
    job: Option<String>,
    source: Option<PathBuf>,
    dest: Option<PathBuf>,
    deep: bool,
    store: Option<PathBuf>,
    workers: Option<usize>,
    cancel: &Cancel,
) -> anyhow::Result<()> {
    let workers = workers.unwrap_or_else(mirror::default_workers);

    // Ad-hoc: two paths, no saved job.
    if let (Some(src), Some(dst)) = (&source, &dest) {
        return compare_trees(src, dst, &Filter::permissive(), deep, workers, cancel);
    }
    let Some(name) = job else {
        anyhow::bail!("give either --job, or both --source and --dest");
    };

    // An image job verifies its chunk store; a mirror job compares two trees.
    let saved = job_store(jobs_dir).ok().and_then(|s| s.load(&name).ok());
    match saved.as_ref().map(|j| &j.kind) {
        Some(JobKind::Mirror(spec)) => compare_trees(
            &spec.source,
            &spec.dest,
            &spec.filter,
            deep,
            spec.workers.unwrap_or(workers),
            cancel,
        ),
        _ => {
            // No saved job, or an image job: fall through to the chunk store,
            // which is also what an ad-hoc `--store` invocation wants.
            let root = store_root_for(jobs_dir, &name, store)?;
            let store = ChunkStore::open(&root)?;
            let Some(latest) = store.latest_manifest(&name)? else {
                anyhow::bail!("no backups to verify for job '{name}'");
            };
            backup::verify(&store, &latest)?;
            println!(
                "OK: '{}-{}' verified — {} chunks re-read and re-hashed, all match.",
                latest.job,
                latest.created_unix,
                latest.chunks.len()
            );
            Ok(())
        }
    }
}

fn compare_trees(
    source: &Path,
    dest: &Path,
    filter: &Filter,
    deep: bool,
    workers: usize,
    cancel: &Cancel,
) -> anyhow::Result<()> {
    println!(
        "comparing {} against {} ({})",
        bkcore::file::path::display(source),
        bkcore::file::path::display(dest),
        if deep {
            "hashing every file"
        } else {
            "size and time"
        }
    );
    let report = verify::compare(
        source,
        dest,
        filter,
        &bkcore::file::walk::ScanOptions::default(),
        deep,
        workers,
        cancel,
        &mut |_, _| {},
    )?;
    print!("{}", report.describe());
    if !report.matched() {
        anyhow::bail!(
            "{} differences — the destination does not match the source",
            report.differences_total
        );
    }
    println!("OK: the destination matches the source.");
    Ok(())
}

fn cmd_logs(
    jobs_dir: &Option<PathBuf>,
    name: &str,
    show: bool,
    state_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    let state_root = state_root
        .or_else(|| {
            job_store(jobs_dir)
                .ok()
                .and_then(|s| s.load(name).ok())
                .and_then(|j| j.state_root)
        })
        .unwrap_or_else(bkcore::file::state::default_state_root);

    let logs = filestate::list_logs(&state_root, name)?;
    if logs.is_empty() {
        println!("no run logs for '{name}' under {}", state_root.display());
        return Ok(());
    }
    if show {
        let newest = &logs[0];
        println!("--- {} ---", newest.display());
        print!("{}", std::fs::read_to_string(newest)?);
    } else {
        println!("{} run logs for '{name}' (newest first):", logs.len());
        for p in &logs {
            let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            println!("  {}  ({})", p.display(), human_bytes(size));
        }
        println!("\nshow the newest with: bkengine logs --job {name} --show");
    }
    Ok(())
}

fn pick_manifest(store: &ChunkStore, job: &str, from: Option<String>) -> anyhow::Result<Manifest> {
    match from {
        Some(stem) => Ok(store.load_manifest(&stem)?),
        None => store
            .latest_manifest(job)?
            .ok_or_else(|| anyhow::anyhow!("no backups found for job '{job}'")),
    }
}

fn cmd_restore(
    jobs_dir: &Option<PathBuf>,
    job: &str,
    store: Option<PathBuf>,
    target: PathBuf,
    from: Option<String>,
) -> anyhow::Result<()> {
    let root = store_root_for(jobs_dir, job, store)?;
    let store = ChunkStore::open(&root)?;
    let manifest = pick_manifest(&store, job, from)?;

    let mut ft = FileTarget::create(&target, manifest.volume_size)?;
    let s = apply::apply(&manifest, &store, &mut ft, false, true)?;
    println!(
        "Restored '{}-{}' -> {}\n  {} chunks written | {} | verified: {}",
        manifest.job,
        manifest.created_unix,
        target.display(),
        s.chunks_written,
        human_bytes(s.bytes_written),
        s.verified,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_clone(
    jobs_dir: &Option<PathBuf>,
    job: &str,
    store: Option<PathBuf>,
    target: String,
    from: Option<String>,
    delta: bool,
    confirm: bool,
) -> anyhow::Result<()> {
    let is_device = target.starts_with(r"\\.\") || target.starts_with(r"\\?\");
    if !is_device {
        anyhow::bail!(
            "clone target must be a raw device path like \\\\.\\PhysicalDrive9, not '{target}'. \
             To write a file image use `restore` instead."
        );
    }
    if !confirm {
        anyhow::bail!(
            "refusing to clone: this ERASES {target}. Re-run with --confirm if you are certain."
        );
    }
    if !bkcore::vss::is_elevated() {
        anyhow::bail!("cloning to a raw device requires Administrator. Re-run elevated.");
    }

    #[cfg(windows)]
    {
        let root = store_root_for(jobs_dir, job, store)?;
        let store = ChunkStore::open(&root)?;
        let manifest = pick_manifest(&store, job, from)?;

        let mut dev = bkcore::diskio::DeviceTarget::open_device(&target)?;
        println!(
            "Cloning '{}-{}' -> {} ({} mode)...",
            manifest.job,
            manifest.created_unix,
            target,
            if delta { "incremental" } else { "full" }
        );
        let s = apply::apply(&manifest, &store, &mut dev, delta, true)?;
        println!(
            "Clone complete -> {}\n  {} blocks written | {} skipped (unchanged) | {} | verified: {}",
            target,
            s.chunks_written,
            s.chunks_skipped,
            human_bytes(s.bytes_written),
            s.verified,
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (jobs_dir, job, store, from, delta);
        anyhow::bail!("device cloning is only implemented on Windows so far");
    }
}
