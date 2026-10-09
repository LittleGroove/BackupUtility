//! bkgui — the desktop front-end.
//!
//! Thin over `bkcore`: it edits the same job JSON the engine reads, shows live
//! progress, and triggers runs. Because the GUI already launches elevated (see
//! `build.rs`), a run happens **in-process** on a worker thread rather than by
//! spawning `bkengine` and scraping its output — the progress you see is the
//! engine's own state, not a parsed approximation of it.
//!
//! ## Threading
//! One background thread per run. It owns the engine call and writes into a
//! shared [`RunShared`] behind mutexes; the UI thread only ever reads. The
//! engine's event callback also pokes `egui::Context::request_repaint`, so the
//! window updates as work happens instead of polling on a timer.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod installer;

use bkcore::file::mirror::{self, Event, MirrorReport};
use bkcore::file::plan::{human_bytes, CompareBy, MirrorMode};
use bkcore::file::state::{self as filestate, Progress};
use bkcore::file::Cancel;
use bkcore::job::{Frequency, Job, JobKind, JobStore, Schedule};
use bkcore::opt::{media, OptimizeSettings};
use bkcore::settings::{Settings, Theme};
use bkcore::timefmt;
use bkcore::update::{self, Release, UpdateEvent};
use eframe::egui;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> eframe::Result<()> {
    #[cfg(windows)]
    set_app_user_model_id();
    // Sweep what an earlier self-update parked beside the binaries.
    if let Ok(dir) = bkcore::paths::install_dir() {
        let _ = update::cleanup_old_binaries(&dir);
    }

    // Started from anywhere but the program's home, this is the installer
    // rather than the application; see `installer`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match installer::decide(&args) {
        installer::Launch::Install => return installer::run_install(),
        installer::Launch::Uninstall => return installer::run_uninstall(),
        installer::Launch::Run => {}
    }
    // Keep what the OS lists about the installed copy (its version, above
    // all) in step with the binary that is actually here.
    bkcore::install::refresh_registration();

    let icon = egui::IconData {
        rgba: include_bytes!("../../../assets/icon_64.rgba").to_vec(),
        width: 64,
        height: 64,
    };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 700.0])
            .with_min_inner_size([820.0, 520.0])
            .with_title(format!("BackupUtility {}", bkcore::VERSION))
            .with_icon(std::sync::Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "BackupUtility",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

/// Tell the Windows shell which application this process is, so the taskbar
/// groups and pins it under one identity instead of the exe path — which
/// changes when the tool is moved, and would otherwise split pinned icons.
#[cfg(windows)]
fn set_app_user_model_id() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    // The installer writes the same identity into the shortcuts it creates.
    let id: Vec<u16> = bkcore::install::APP_USER_MODEL_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: a plain Win32 call with a null-terminated wide string that
    // outlives it.
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(PCWSTR(id.as_ptr()));
    }
}

// ---------------------------------------------------------------------------
// Shared state between the UI thread and a running job
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RunShared {
    phase: Mutex<String>,
    progress: Mutex<Progress>,
    plan: Mutex<String>,
    errors: Mutex<Vec<String>>,
    done: AtomicBool,
    /// `Err` carries the message, because `BkError` is not `Clone`.
    result: Mutex<Option<Result<MirrorReport, String>>>,
}

struct RunHandle {
    job_name: String,
    dry_run: bool,
    cancel: Cancel,
    shared: Arc<RunShared>,
    started: Instant,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct App {
    store: Option<JobStore>,
    jobs: Vec<Job>,
    broken: Vec<(std::path::PathBuf, String)>,
    selected: Option<usize>,
    /// The job being edited. Edits are held here until Save, so a mistake can
    /// be abandoned by selecting something else.
    draft: Option<Job>,
    dirty: bool,
    run: Option<RunHandle>,
    last_report: Option<MirrorReport>,
    drives: Vec<media::VolumeInfo>,
    toast: Option<(String, bool, Instant)>,
    load_error: Option<String>,
    /// Set when a destructive run is waiting on an explicit confirmation.
    confirm_delete: bool,
    /// Past run logs for the selected job, newest first.
    history: Vec<std::path::PathBuf>,
    /// A log the user opened, held as (path, contents).
    viewing_log: Option<(std::path::PathBuf, String)>,
    settings: Settings,
    show_settings: bool,
    updates: UpdateState,
    /// The last run of each job, read from its `run.json`.
    statuses: HashMap<String, Option<filestate::RunState>>,
}

/// What an update check produced: a newer release, nothing new, or why it
/// could not tell.
type CheckResult = Result<Option<Release>, String>;

/// Everything about update checks and installs, kept apart from the job UI.
#[derive(Default)]
struct UpdateState {
    /// Filled by the check thread.
    check: Arc<Mutex<Option<CheckResult>>>,
    checking: bool,
    /// The user clicked "Check now", so "nothing new" deserves a message.
    manual: bool,
    /// The release the banner offers.
    offered: Option<Release>,
    /// Banner dismissed for this session.
    dismissed: bool,
    show_notes: bool,
    install: Option<InstallHandle>,
}

struct InstallHandle {
    version: String,
    status: Arc<Mutex<String>>,
    fraction: Arc<Mutex<Option<f32>>>,
    result: Arc<Mutex<Option<Result<update::Applied, String>>>>,
    done: Arc<AtomicBool>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let settings = Settings::load();
        let mut app = App {
            store: None,
            jobs: Vec::new(),
            broken: Vec::new(),
            selected: None,
            draft: None,
            dirty: false,
            run: None,
            last_report: None,
            drives: Vec::new(),
            toast: None,
            load_error: None,
            confirm_delete: false,
            history: Vec::new(),
            viewing_log: None,
            settings,
            show_settings: false,
            updates: UpdateState::default(),
            statuses: HashMap::new(),
        };
        app.apply_theme(ctx);
        match JobStore::open(bkcore::job::default_jobs_dir()) {
            Ok(s) => {
                app.store = Some(s);
                app.reload_jobs();
            }
            Err(e) => app.load_error = Some(format!("cannot open the jobs directory: {e}")),
        }
        app.refresh_drives();
        if app.settings.check_updates_on_start {
            app.start_update_check(ctx, false);
        }
        app
    }

    fn reload_jobs(&mut self) {
        if let Some(store) = &self.store {
            match store.list() {
                Ok((jobs, broken)) => {
                    self.jobs = jobs;
                    self.broken = broken;
                    self.reload_statuses();
                }
                Err(e) => self.load_error = Some(e.to_string()),
            }
        }
    }

    fn refresh_drives(&mut self) {
        self.drives = media::list_volumes();
    }

    fn notify(&mut self, message: impl Into<String>, is_error: bool) {
        self.toast = Some((message.into(), is_error, Instant::now()));
    }

    fn select(&mut self, index: usize) {
        self.selected = Some(index);
        self.draft = self.jobs.get(index).cloned();
        self.dirty = false;
        self.last_report = None;
        self.viewing_log = None;
        self.confirm_delete = false;
        self.reload_history();
    }

    /// Load the selected job's past run logs.
    fn reload_history(&mut self) {
        self.history.clear();
        let Some(job) = &self.draft else { return };
        let root = job
            .state_root
            .clone()
            .unwrap_or_else(bkcore::file::state::default_state_root);
        self.history = filestate::list_logs(&root, &job.name).unwrap_or_default();
    }

    fn save_draft(&mut self) {
        let (Some(store), Some(draft)) = (&self.store, &self.draft) else {
            return;
        };
        if let Err(e) = draft.validate() {
            self.notify(format!("cannot save: {e}"), true);
            return;
        }
        match store.save(draft) {
            Ok(_) => {
                let name = draft.name.clone();
                // Keep the scheduled task in step with the file, or the two
                // disagree and the job runs on yesterday's settings.
                let sched_result = if draft.schedule.is_some() {
                    std::env::current_exe().ok().map(|exe| {
                        bkcore::scheduler::register(draft, &engine_beside(&exe), Some(store.dir()))
                    })
                } else {
                    Some(bkcore::scheduler::unregister(draft))
                };
                self.dirty = false;
                self.reload_jobs();
                if let Some(Err(e)) = sched_result {
                    self.notify(
                        format!("saved '{name}', but the schedule failed: {e}"),
                        true,
                    );
                } else {
                    self.notify(format!("saved '{name}'"), false);
                }
            }
            Err(e) => self.notify(format!("save failed: {e}"), true),
        }
    }

    fn start_run(&mut self, dry_run: bool, ctx: &egui::Context) {
        let Some(draft) = self.draft.clone() else {
            return;
        };
        if self.run.is_some() {
            return;
        }
        if let Err(e) = draft.validate() {
            self.notify(format!("cannot run: {e}"), true);
            return;
        }
        let mut mirror_job = match draft.to_mirror_job() {
            Ok(j) => j,
            Err(e) => {
                self.notify(format!("cannot run: {e}"), true);
                return;
            }
        };
        mirror_job.dry_run = dry_run;

        let cancel = Cancel::new();
        let shared = Arc::new(RunShared::default());
        let (thread_cancel, thread_shared, ctx) = (cancel.clone(), shared.clone(), ctx.clone());
        let optimize = draft.optimize.clone();

        std::thread::spawn(move || {
            *thread_shared.phase.lock().unwrap() = "scanning".into();
            let result = mirror::run(&mirror_job, &thread_cancel, &mut |event| {
                match event {
                    Event::ScanningSource(s) => {
                        *thread_shared.phase.lock().unwrap() = format!(
                            "scanning source — {} files, {}",
                            s.files,
                            human_bytes(s.bytes)
                        );
                    }
                    Event::ScanningDest(s) => {
                        *thread_shared.phase.lock().unwrap() =
                            format!("scanning destination — {} files", s.files);
                    }
                    Event::Planned(p) => {
                        *thread_shared.plan.lock().unwrap() =
                            bkcore::file::plan::summarize(p, mirror_job.mode);
                        *thread_shared.phase.lock().unwrap() = "copying".into();
                    }
                    Event::Resuming { done, total } => {
                        *thread_shared.phase.lock().unwrap() =
                            format!("resuming — {done} of {total} already done");
                    }
                    Event::Progress(p) => {
                        *thread_shared.progress.lock().unwrap() = p.clone();
                    }
                    Event::Failed { rel, message } => {
                        let mut errs = thread_shared.errors.lock().unwrap();
                        // A run against a system drive can produce thousands of
                        // access-denied entries; showing the first few and a
                        // count keeps the window responsive.
                        if errs.len() < 200 {
                            errs.push(format!("{rel} — {message}"));
                        }
                    }
                }
                ctx.request_repaint();
            });

            // Optional maintenance pass, and never fatal to the backup.
            if let (Ok(report), Some(settings)) = (&result, &optimize) {
                if settings.is_enabled() && report.succeeded() && !mirror_job.dry_run {
                    *thread_shared.phase.lock().unwrap() = "optimizing".into();
                    ctx.request_repaint();
                    let files = files_under(&mirror_job.dest);
                    if let Err(e) = bkcore::opt::run(
                        &mirror_job.source,
                        &mirror_job.dest,
                        &files,
                        settings,
                        &thread_cancel,
                        &mut |stage| {
                            *thread_shared.phase.lock().unwrap() = stage.to_string();
                            ctx.request_repaint();
                        },
                    ) {
                        thread_shared
                            .errors
                            .lock()
                            .unwrap()
                            .push(format!("optimization stage: {e}"));
                    }
                }
            }

            *thread_shared.result.lock().unwrap() = Some(result.map_err(|e| e.to_string()));
            thread_shared.done.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        });

        self.last_report = None;
        self.run = Some(RunHandle {
            job_name: draft.name.clone(),
            dry_run,
            cancel,
            shared,
            started: Instant::now(),
        });
    }

    /// Collect a finished run's result.
    fn reap_run(&mut self) {
        let finished = self
            .run
            .as_ref()
            .is_some_and(|r| r.shared.done.load(Ordering::SeqCst));
        if !finished {
            return;
        }
        let handle = self.run.take().unwrap();
        let outcome = handle.shared.result.lock().unwrap().take();
        match outcome {
            Some(Ok(report)) => {
                let ok = report.succeeded();
                let msg = format!(
                    "'{}' finished — {} copied in {}",
                    handle.job_name,
                    human_bytes(report.bytes_copied),
                    timefmt::duration(report.elapsed_secs())
                );
                self.last_report = Some(report);
                self.notify(msg, !ok);
            }
            Some(Err(e)) => self.notify(format!("'{}' failed: {e}", handle.job_name), true),
            None => self.notify("the run ended without reporting a result", true),
        }
        self.reload_statuses();
        self.reload_history();
    }
}

/// Every file under a root, for the layout pass.
fn files_under(root: &std::path::Path) -> Vec<bkcore::file::path::RelPath> {
    bkcore::file::walk::scan(
        root,
        &bkcore::file::filter::Filter::permissive(),
        &bkcore::file::walk::ScanOptions::default(),
        &Cancel::none(),
        &mut |_| {},
    )
    .map(|t| {
        t.entries
            .into_iter()
            .filter(|e| !e.is_dir() && !e.is_link())
            .map(|e| e.rel)
            .collect()
    })
    .unwrap_or_default()
}

/// `bkengine.exe` sitting beside the GUI — what a scheduled task should run,
/// since the GUI itself would try to open a window at 3am.
fn engine_beside(gui_exe: &std::path::Path) -> std::path::PathBuf {
    gui_exe
        .parent()
        .map(|d| {
            d.join(if cfg!(windows) {
                "bkengine.exe"
            } else {
                "bkengine"
            })
        })
        .unwrap_or_else(|| std::path::PathBuf::from("bkengine"))
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.reap_run();
        self.reap_update_check();
        self.reap_install(ctx);
        self.apply_theme(ctx);
        if self.run.is_some() {
            // Keep the elapsed time and rate ticking even between engine events.
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.top_bar(ctx);
        self.update_banner(ctx);
        self.side_panel(ctx);
        self.central(ctx);
        self.confirm_dialog(ctx);
        self.log_viewer(ctx);
        self.settings_window(ctx);
        self.notes_window(ctx);
        self.install_window(ctx);
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("BackupUtility");
                ui.separator();
                let (colour, notice) = privilege_notice();
                ui.colored_label(colour, notice);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .selectable_label(self.show_settings, "⚙ Settings")
                        .on_hover_text("Updates, theme, and where files live")
                        .clicked()
                    {
                        self.show_settings = !self.show_settings;
                    }
                    ui.separator();
                    if let Some((msg, is_error, at)) = &self.toast {
                        if at.elapsed() < Duration::from_secs(8) {
                            let colour = if *is_error {
                                egui::Color32::from_rgb(0xD1, 0x4A, 0x4A)
                            } else {
                                egui::Color32::GRAY
                            };
                            ui.colored_label(colour, msg);
                        }
                    }
                });
            });
        });
    }

    fn side_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("jobs")
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.strong("Jobs");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("+ New").clicked() {
                            let name = unique_name(&self.jobs);
                            self.draft = Some(Job::mirror(name, "", ""));
                            self.selected = None;
                            self.dirty = true;
                        }
                    });
                });
                ui.separator();

                if let Some(e) = &self.load_error {
                    ui.colored_label(egui::Color32::from_rgb(0xD1, 0x4A, 0x4A), e);
                }

                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() * 0.55)
                    .show(ui, |ui| {
                        let mut clicked = None;
                        for (i, job) in self.jobs.iter().enumerate() {
                            let label = if job.disabled {
                                format!("{}  (off)", job.name)
                            } else {
                                job.name.clone()
                            };
                            let (colour, badge, hover) =
                                status_badge(self.statuses.get(&job.name).and_then(|s| s.as_ref()));
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(self.selected == Some(i), label)
                                    .clicked()
                                {
                                    clicked = Some(i);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.colored_label(colour, badge).on_hover_text(hover);
                                    },
                                );
                            });
                        }
                        if let Some(i) = clicked {
                            self.select(i);
                        }
                        if self.jobs.is_empty() {
                            ui.weak("no jobs yet — press “+ New”");
                        }
                        for (path, err) in &self.broken {
                            ui.colored_label(
                                egui::Color32::from_rgb(0xD1, 0x4A, 0x4A),
                                format!("unreadable: {} ({err})", path.display()),
                            );
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("Drives");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("refresh").clicked() {
                            self.refresh_drives();
                        }
                    });
                });
                egui::ScrollArea::vertical()
                    .id_source("drives")
                    .show(ui, |ui| {
                        for d in &self.drives {
                            ui.label(d.describe());
                            ui.weak(format!("    suggested: {}", media::recommended_op(d)));
                        }
                    });
            });
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.run.is_some() {
                self.running_view(ui);
                return;
            }
            if self.draft.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.label("Select a job, or create one, to configure a backup.");
                    ui.add_space(8.0);
                    ui.weak("A job points at one path and mirrors it to another.");
                });
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.editor(ui, ctx);
                if let Some(report) = &self.last_report {
                    ui.separator();
                    ui.strong("Last run");
                    ui.monospace(report.summary());
                }
                ui.separator();
                self.history_section(ui);
            });
        });
    }

    fn editor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // Work on a clone so the borrow checker lets us call &mut self methods
        // (save, run) from inside the closure-heavy UI code.
        let mut draft = self.draft.clone().unwrap();
        let mut changed = false;

        ui.horizontal(|ui| {
            ui.label("Name");
            changed |= ui.text_edit_singleline(&mut draft.name).changed();
            changed |= ui.checkbox(&mut draft.disabled, "disabled").changed();
        });

        let mut description = draft.description.clone().unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label("Note");
            if ui.text_edit_singleline(&mut description).changed() {
                draft.description = (!description.is_empty()).then_some(description.clone());
                changed = true;
            }
        });

        ui.separator();

        if let JobKind::Mirror(spec) = &mut draft.kind {
            changed |= path_row(ui, "Source", &mut spec.source, true);
            changed |= path_row(ui, "Destination", &mut spec.dest, true);

            ui.horizontal(|ui| {
                ui.label("Vanished files");
                changed |= combo(
                    ui,
                    "mode",
                    &mut spec.mode,
                    &[
                        (MirrorMode::Additive, "keep them (additive)"),
                        (MirrorMode::Attic, "move to attic"),
                        (MirrorMode::Delete, "delete them"),
                    ],
                );
                ui.label("Compare by");
                changed |= combo(
                    ui,
                    "compare",
                    &mut spec.compare,
                    &[
                        (CompareBy::SizeAndTime, "size + time (fast)"),
                        (CompareBy::SizeOnly, "size only"),
                        (CompareBy::Content, "contents (hash)"),
                    ],
                );
            });

            if spec.mode == MirrorMode::Attic {
                ui.indent("attic", |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Attic keeps");
                        let mut keep = spec.attic_policy.keep_runs.unwrap_or(0);
                        if ui
                            .add(egui::DragValue::new(&mut keep).range(0..=500).suffix(" runs"))
                            .on_hover_text("0 keeps every attic folder forever")
                            .changed()
                        {
                            spec.attic_policy.keep_runs = (keep > 0).then_some(keep);
                            changed = true;
                        }
                        let mut days = spec.attic_policy.max_age_days.unwrap_or(0);
                        ui.label("and drops after");
                        if ui
                            .add(egui::DragValue::new(&mut days).range(0..=3650).suffix(" days"))
                            .on_hover_text("0 disables the age limit")
                            .changed()
                        {
                            spec.attic_policy.max_age_days = (days > 0).then_some(days);
                            changed = true;
                        }
                    });
                    if !spec.attic_policy.prunes_anything() {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xE5, 0x7A, 0x24),
                            "⚠ with no limit the attic grows forever and will eventually fill the drive",
                        );
                    }
                });
            }

            ui.collapsing("Filters", |ui| {
                changed |= list_edit(ui, "Exclude", &mut spec.filter.exclude);
                changed |= list_edit(ui, "Include (empty = everything)", &mut spec.filter.include);
                changed |= ui
                    .checkbox(
                        &mut spec.filter.use_system_excludes,
                        "skip system paths (pagefile, System Volume Information, recycle bin)",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut spec.filter.skip_cloud_placeholders,
                        "skip cloud placeholders (do not force OneDrive downloads)",
                    )
                    .changed();
            });

            ui.collapsing("What to preserve", |ui| {
                let f = &mut spec.fidelity;
                changed |= ui.checkbox(&mut f.timestamps, "timestamps").changed();
                changed |= ui
                    .checkbox(&mut f.attributes, "attributes (read-only, hidden, system)")
                    .changed();
                changed |= ui
                    .checkbox(&mut f.acls, "security descriptors (NTFS permissions)")
                    .changed();
                changed |= ui
                    .checkbox(&mut f.alternate_data_streams, "alternate data streams")
                    .changed();
                changed |= ui.checkbox(&mut f.sparse, "sparse regions").changed();
                changed |= ui
                    .checkbox(&mut f.verify, "verify every file after writing (slower)")
                    .changed();
                ui.add_space(4.0);
                changed |= ui
                    .checkbox(&mut spec.preserve_hardlinks, "preserve hard links")
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut spec.follow_links,
                        "follow symlinks and junctions (risky)",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut spec.cross_volume, "descend into mounted volumes")
                    .changed();
                ui.add_space(4.0);
                changed |= ui
                    .checkbox(
                        &mut spec.check_free_space,
                        "refuse to start if the destination is too small",
                    )
                    .changed();
                changed |= ui
                    .checkbox(
                        &mut spec.log,
                        "write a run log (needed to review a scheduled run)",
                    )
                    .changed();
            });
        }

        ui.collapsing("Schedule", |ui| {
            let mut enabled = draft.schedule.is_some();
            if ui.checkbox(&mut enabled, "run on a schedule").changed() {
                draft.schedule = enabled.then(|| Schedule {
                    frequency: Frequency::Daily,
                    at: "03:00".into(),
                    day: Some("SUN".into()),
                    run_as_system: true,
                    run_if_missed: true,
                    only_on_ac_power: false,
                });
                changed = true;
            }
            if let Some(s) = &mut draft.schedule {
                ui.horizontal(|ui| {
                    changed |= combo(
                        ui,
                        "freq",
                        &mut s.frequency,
                        &[
                            (Frequency::Hourly, "hourly"),
                            (Frequency::Daily, "daily"),
                            (Frequency::Weekly, "weekly"),
                            (Frequency::OnLogon, "at logon"),
                            (Frequency::OnStart, "at startup"),
                        ],
                    );
                    if s.frequency.needs_time() {
                        ui.label("at");
                        changed |= ui
                            .add(egui::TextEdit::singleline(&mut s.at).desired_width(60.0))
                            .changed();
                    }
                    if s.frequency == Frequency::Weekly {
                        let mut day = s.day.clone().unwrap_or_else(|| "SUN".into());
                        ui.label("on");
                        if ui
                            .add(egui::TextEdit::singleline(&mut day).desired_width(60.0))
                            .changed()
                        {
                            s.day = Some(day);
                            changed = true;
                        }
                    }
                });
                changed |= ui
                    .checkbox(&mut s.run_as_system, run_as_system_label())
                    .changed();
                changed |= ui
                    .checkbox(&mut s.run_if_missed, "run late if the machine was off")
                    .changed();
                changed |= ui
                    .checkbox(&mut s.only_on_ac_power, "only on mains power")
                    .changed();
            }
        });

        ui.collapsing("Disk optimization (optional)", |ui| {
            ui.weak("Most jobs need none of this. Turn it on for a pair of drives you want kept structurally identical.");
            let mut enabled = draft.optimize.is_some();
            if ui.checkbox(&mut enabled, "run a disk pass after the copy").changed() {
                draft.optimize = enabled.then(OptimizeSettings::identical_drives);
                changed = true;
            }
            if let Some(o) = &mut draft.optimize {
                changed |= ui.checkbox(&mut o.mirror_layout, "align the destination's physical layout to the source").changed();
                changed |= ui.checkbox(&mut o.optimize_destination, "optimize the destination drive").changed();
                changed |= ui.checkbox(&mut o.optimize_source, "optimize the source drive").changed();
                changed |= ui.checkbox(&mut o.analyze_only, "analyze only — report, change nothing").changed();
                ui.weak("The operation is chosen from the media: defragment a spinning disk, retrim an SSD. An SSD is never defragmented.");
            }
        });

        ui.separator();
        ui.horizontal(|ui| {
            let can_run = matches!(draft.kind, JobKind::Mirror(_));
            if ui
                .add_enabled(self.dirty || changed, egui::Button::new("Save"))
                .clicked()
            {
                self.draft = Some(draft.clone());
                self.save_draft();
                return;
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Dry run"))
                .on_hover_text("Scan and plan, but change nothing")
                .clicked()
            {
                self.draft = Some(draft.clone());
                self.start_run(true, ctx);
                return;
            }
            if ui
                .add_enabled(can_run, egui::Button::new("Back up now"))
                .clicked()
            {
                self.draft = Some(draft.clone());
                // Delete mode is the only setting that can destroy data the
                // user did not ask to touch, so it gets an explicit
                // confirmation rather than sharing a button with the safe modes.
                let destructive = draft
                    .as_mirror()
                    .is_some_and(|m| m.mode == MirrorMode::Delete);
                if destructive {
                    self.confirm_delete = true;
                } else {
                    self.start_run(false, ctx);
                }
                return;
            }
            if let Some(i) = self.selected {
                if ui.button("Delete job").clicked() {
                    if let Some(store) = &self.store {
                        let name = self.jobs[i].name.clone();
                        let _ = bkcore::scheduler::unregister(&self.jobs[i]);
                        let r = store.delete(&name);
                        self.selected = None;
                        self.draft = None;
                        self.reload_jobs();
                        match r {
                            Ok(()) => self.notify(format!("deleted '{name}'"), false),
                            Err(e) => self.notify(format!("delete failed: {e}"), true),
                        }
                    }
                }
            }
        });

        if let Err(e) = draft.validate() {
            ui.add_space(6.0);
            ui.colored_label(egui::Color32::from_rgb(0xE5, 0x7A, 0x24), format!("⚠ {e}"));
        }

        if changed {
            self.dirty = true;
        }
        self.draft = Some(draft);
    }

    /// Confirmation for a run that will delete files from the destination.
    fn confirm_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_delete {
            return;
        }
        let mut start = false;
        let mut dry = false;
        let mut cancel = false;

        egui::Window::new("This job deletes files")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.colored_label(
                    egui::Color32::from_rgb(0xD1, 0x4A, 0x4A),
                    "Delete mode is on.",
                );
                ui.add_space(6.0);
                ui.label(
                    "Any file on the destination that is no longer in the source \
                     will be permanently removed, so the two trees match exactly.",
                );
                ui.add_space(6.0);
                if let Some(m) = self.draft.as_ref().and_then(|d| d.as_mirror()) {
                    ui.monospace(format!(
                        "{}\n  -> {}",
                        bkcore::file::path::display(&m.source),
                        bkcore::file::path::display(&m.dest)
                    ));
                }
                ui.add_space(6.0);
                ui.weak(
                    "A dry run shows exactly what would be removed and changes nothing. \
                     Switching the job to “move to attic” makes deletions reversible.",
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if ui.button("Dry run instead").clicked() {
                        dry = true;
                    }
                    if ui
                        .button("Delete and back up")
                        .on_hover_text("Files removed from the source will be deleted here too")
                        .clicked()
                    {
                        start = true;
                    }
                });
            });

        if cancel {
            self.confirm_delete = false;
        } else if dry {
            self.confirm_delete = false;
            self.start_run(true, ctx);
        } else if start {
            self.confirm_delete = false;
            self.start_run(false, ctx);
        }
    }

    /// A window showing one past run log.
    fn log_viewer(&mut self, ctx: &egui::Context) {
        let Some((path, text)) = self.viewing_log.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new(format!(
            "Run log — {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
        .open(&mut open)
        .default_size([760.0, 480.0])
        .show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                ui.monospace(&text);
            });
        });
        if !open {
            self.viewing_log = None;
        }
    }

    /// Past runs, read from the logs the engine leaves behind.
    fn history_section(&mut self, ui: &mut egui::Ui) {
        ui.collapsing(format!("Run history ({})", self.history.len()), |ui| {
            if self.history.is_empty() {
                ui.weak("no runs recorded yet");
                return;
            }
            let mut open_this: Option<std::path::PathBuf> = None;
            egui::ScrollArea::vertical()
                .id_source("history")
                .max_height(180.0)
                .show(ui, |ui| {
                    for p in &self.history {
                        let name = p.file_name().unwrap_or_default().to_string_lossy();
                        // Log names are `YYYYMMDD-HHMMSS`, so turn them back
                        // into something a person reads without effort.
                        let label = p
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .and_then(timefmt::parse_stamp)
                            .map(timefmt::readable)
                            .unwrap_or_else(|| name.to_string());
                        if ui.link(label).clicked() {
                            open_this = Some(p.clone());
                        }
                    }
                });
            if ui.small_button("refresh").clicked() {
                self.reload_history();
            }
            if let Some(p) = open_this {
                let text = std::fs::read_to_string(&p)
                    .unwrap_or_else(|e| format!("could not read the log: {e}"));
                self.viewing_log = Some((p, text));
            }
        });
    }

    fn running_view(&mut self, ui: &mut egui::Ui) {
        let Some(handle) = &self.run else { return };
        let phase = handle.shared.phase.lock().unwrap().clone();
        let progress = handle.shared.progress.lock().unwrap().clone();
        let plan = handle.shared.plan.lock().unwrap().clone();
        let errors = handle.shared.errors.lock().unwrap().clone();

        ui.horizontal(|ui| {
            ui.heading(&handle.job_name);
            if handle.dry_run {
                ui.weak("(dry run)");
            }
        });
        ui.label(&phase);
        ui.add_space(6.0);

        let fraction = (progress.percent() / 100.0).clamp(0.0, 1.0) as f32;
        ui.add(
            egui::ProgressBar::new(fraction)
                .show_percentage()
                .desired_width(ui.available_width()),
        );
        ui.add_space(4.0);
        ui.monospace(format!(
            "{} of {}   {}/s   {} elapsed   eta {}",
            human_bytes(progress.bytes_done),
            human_bytes(progress.bytes_total),
            human_bytes(progress.rate_bps),
            timefmt::duration(handle.started.elapsed().as_secs()),
            progress
                .eta_secs()
                .map(timefmt::duration)
                .unwrap_or_else(|| "—".into())
        ));
        ui.monospace(format!(
            "{} of {} actions   {} files copied   {} errors",
            progress.actions_done, progress.actions_total, progress.files_copied, progress.errors
        ));
        if let Some(current) = &progress.current {
            ui.weak(current);
        }

        ui.add_space(8.0);
        if !plan.is_empty() {
            ui.collapsing("Plan", |ui| {
                ui.monospace(plan);
            });
        }
        if !errors.is_empty() {
            ui.collapsing(format!("Errors ({})", errors.len()), |ui| {
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for e in &errors {
                            ui.colored_label(egui::Color32::from_rgb(0xD1, 0x4A, 0x4A), e);
                        }
                    });
            });
        }

        ui.separator();
        if handle.cancel.is_cancelled() {
            ui.weak("stopping — finishing the current file and saving progress…");
        } else if ui
            .button("Stop")
            .on_hover_text(
                "Finishes the file in flight and saves progress. The next run resumes from here.",
            )
            .clicked()
        {
            handle.cancel.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// Small widgets
// ---------------------------------------------------------------------------

/// A labelled path field with a Browse button.
fn path_row(ui: &mut egui::Ui, label: &str, value: &mut std::path::PathBuf, folder: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let mut text = value.to_string_lossy().into_owned();
        if ui
            .add(egui::TextEdit::singleline(&mut text).desired_width(460.0))
            .changed()
        {
            *value = std::path::PathBuf::from(&text);
            changed = true;
        }
        if ui.button("Browse…").clicked() && folder {
            if let Some(picked) = rfd::FileDialog::new().pick_folder() {
                *value = picked;
                changed = true;
            }
        }
    });
    changed
}

/// A combo box over a small fixed set of values.
fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    let current = options
        .iter()
        .find(|(v, _)| v == value)
        .map(|(_, label)| *label)
        .unwrap_or("?");
    egui::ComboBox::from_id_source(id)
        .selected_text(current)
        .show_ui(ui, |ui| {
            for (v, label) in options {
                if ui.selectable_label(*value == *v, *label).clicked() {
                    *value = *v;
                    changed = true;
                }
            }
        });
    changed
}

/// An editable list of strings, one per line.
fn list_edit(ui: &mut egui::Ui, label: &str, items: &mut Vec<String>) -> bool {
    let mut text = items.join("\n");
    ui.label(label);
    let changed = ui
        .add(
            egui::TextEdit::multiline(&mut text)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("one pattern per line, e.g. *.tmp or node_modules/"),
        )
        .changed();
    if changed {
        *items = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
    }
    changed
}

/// A name not already taken.
fn unique_name(jobs: &[Job]) -> String {
    let mut n = 1;
    loop {
        let candidate = if n == 1 {
            "new-job".to_string()
        } else {
            format!("new-job-{n}")
        };
        if !jobs.iter().any(|j| j.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}

// ---------------------------------------------------------------------------
// Settings, updates, status
// ---------------------------------------------------------------------------

impl App {
    /// Read each job's last run summary. One small file per job; no plans or
    /// journals are opened.
    fn reload_statuses(&mut self) {
        self.statuses.clear();
        for job in &self.jobs {
            let root = job
                .state_root
                .clone()
                .unwrap_or_else(bkcore::file::state::default_state_root);
            let run = filestate::SaveState::peek(&root, &job.name).ok().flatten();
            self.statuses.insert(job.name.clone(), run);
        }
    }

    fn apply_theme(&self, ctx: &egui::Context) {
        match self.settings.theme {
            Theme::Dark => ctx.set_visuals(egui::Visuals::dark()),
            Theme::Light => ctx.set_visuals(egui::Visuals::light()),
            // Follow the system: eframe set it at startup and tracks changes.
            Theme::System => {}
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save() {
            self.notify(format!("settings were not saved: {e}"), true);
        }
    }

    /// Ask GitHub on a background thread. `manual` means the user asked, so
    /// the outcome is always reported.
    fn start_update_check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.updates.checking {
            return;
        }
        self.updates.checking = true;
        self.updates.manual = manual;
        let beta = self.settings.beta_updates;
        let slot = self.updates.check.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = update::Updater::github()
                .check(beta)
                .map_err(|e| e.to_string());
            *slot.lock().unwrap() = Some(result);
            ctx.request_repaint();
        });
    }

    fn reap_update_check(&mut self) {
        if !self.updates.checking {
            return;
        }
        let Some(result) = self.updates.check.lock().unwrap().take() else {
            return;
        };
        self.updates.checking = false;
        self.settings.last_update_check_unix = Some(timefmt::now_unix());
        let manual = self.updates.manual;
        match result {
            Ok(Some(release)) => {
                let skipped = self.settings.skipped_version.as_deref()
                    == Some(release.version.to_string().as_str());
                if skipped && !manual {
                    // They asked not to hear about this one again.
                } else {
                    self.updates.offered = Some(release);
                    self.updates.dismissed = false;
                }
            }
            Ok(None) => {
                if manual {
                    self.notify(
                        format!("you have the newest version ({})", bkcore::VERSION),
                        false,
                    );
                }
            }
            Err(e) => {
                if manual {
                    self.notify(format!("could not check for updates: {e}"), true);
                }
            }
        }
        self.save_settings();
    }

    /// A strip under the top bar offering the newer release.
    fn update_banner(&mut self, ctx: &egui::Context) {
        if self.updates.dismissed || self.updates.install.is_some() {
            return;
        }
        let Some(release) = self.updates.offered.clone() else {
            return;
        };
        let mut install = false;
        let mut later = false;
        let mut skip = false;
        let mut notes = false;
        egui::TopBottomPanel::top("update-banner").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(0x4C, 0xAF, 0x50),
                    format!(
                        "Version {} is available{} — you have {}.",
                        release.version,
                        if release.prerelease {
                            " (pre-release)"
                        } else {
                            ""
                        },
                        bkcore::VERSION
                    ),
                );
                let busy = self.run.is_some();
                if ui
                    .add_enabled(!busy, egui::Button::new("Update now"))
                    .on_hover_text("Downloads the new binaries, verifies them and restarts")
                    .on_disabled_hover_text("Wait for the running job to finish")
                    .clicked()
                {
                    install = true;
                }
                if ui.button("What's new").clicked() {
                    notes = true;
                }
                if ui.button("Later").clicked() {
                    later = true;
                }
                if ui
                    .button("Skip this version")
                    .on_hover_text("Do not offer this version again")
                    .clicked()
                {
                    skip = true;
                }
            });
            ui.add_space(2.0);
        });
        if install {
            self.start_install(ctx, release);
        } else if notes {
            self.updates.show_notes = true;
        } else if later {
            self.updates.dismissed = true;
        } else if skip {
            self.settings.skipped_version = Some(release.version.to_string());
            self.updates.dismissed = true;
            self.save_settings();
        }
    }

    fn start_install(&mut self, ctx: &egui::Context, release: Release) {
        if self.updates.install.is_some() || self.run.is_some() {
            return;
        }
        let handle = InstallHandle {
            version: release.version.to_string(),
            status: Arc::new(Mutex::new("starting".into())),
            fraction: Arc::new(Mutex::new(None)),
            result: Arc::new(Mutex::new(None)),
            done: Arc::new(AtomicBool::new(false)),
        };
        let (status, fraction, result, done) = (
            handle.status.clone(),
            handle.fraction.clone(),
            handle.result.clone(),
            handle.done.clone(),
        );
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let updater = update::Updater::github();
            let outcome = update::apply(&updater, &release, &mut |event| {
                match event {
                    UpdateEvent::Downloading { name, done, total } => {
                        *status.lock().unwrap() = match total {
                            Some(t) if t > 0 => format!(
                                "downloading {name} — {} of {}",
                                human_bytes(done),
                                human_bytes(t)
                            ),
                            _ => format!("downloading {name} — {}", human_bytes(done)),
                        };
                        *fraction.lock().unwrap() =
                            total.filter(|t| *t > 0).map(|t| done as f32 / t as f32);
                    }
                    UpdateEvent::Verifying(name) => {
                        *status.lock().unwrap() = format!("verifying {name}");
                    }
                    UpdateEvent::Installing(name) => {
                        *status.lock().unwrap() = format!("installing {name}");
                        *fraction.lock().unwrap() = None;
                    }
                }
                ctx.request_repaint();
            });
            *result.lock().unwrap() = Some(outcome.map_err(|e| e.to_string()));
            done.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        });
        self.updates.install = Some(handle);
    }

    fn reap_install(&mut self, ctx: &egui::Context) {
        let finished = self
            .updates
            .install
            .as_ref()
            .is_some_and(|h| h.done.load(Ordering::SeqCst));
        if !finished {
            return;
        }
        let handle = self.updates.install.take().unwrap();
        let outcome = handle.result.lock().unwrap().take();
        match outcome {
            Some(Ok(applied)) => {
                self.updates.offered = None;
                match &applied.relaunch {
                    Some(exe) => match update::relaunch(exe, &[]) {
                        Ok(_) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(e) => self.notify(
                            format!(
                                "updated to {}, but could not restart automatically ({e}) — close and reopen",
                                applied.version
                            ),
                            true,
                        ),
                    },
                    None => self.notify(
                        format!("updated to {} — restart to use it", applied.version),
                        false,
                    ),
                }
            }
            Some(Err(e)) => self.notify(format!("update to {} failed: {e}", handle.version), true),
            None => self.notify("the update ended without reporting a result", true),
        }
    }

    fn install_window(&mut self, ctx: &egui::Context) {
        let Some(handle) = &self.updates.install else {
            return;
        };
        let status = handle.status.lock().unwrap().clone();
        let fraction = *handle.fraction.lock().unwrap();
        egui::Window::new(format!("Updating to {}", handle.version))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_min_width(380.0);
                ui.label(&status);
                ui.add_space(4.0);
                let bar = match fraction {
                    Some(f) => egui::ProgressBar::new(f).show_percentage(),
                    None => egui::ProgressBar::new(0.0).animate(true),
                };
                ui.add(bar.desired_width(ui.available_width()));
                ui.add_space(4.0);
                ui.weak("The window closes and reopens on the new version when this finishes.");
            });
    }

    fn notes_window(&mut self, ctx: &egui::Context) {
        if !self.updates.show_notes {
            return;
        }
        let Some(release) = &self.updates.offered else {
            self.updates.show_notes = false;
            return;
        };
        let mut open = true;
        egui::Window::new(format!("What's new in {}", release.version))
            .open(&mut open)
            .default_size([620.0, 420.0])
            .show(ctx, |ui| {
                ui.hyperlink_to("Open the release page", &release.url);
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let notes = if release.notes.trim().is_empty() {
                        "(no release notes)"
                    } else {
                        release.notes.as_str()
                    };
                    ui.label(notes);
                });
            });
        if !open {
            self.updates.show_notes = false;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let mut changed = false;
        let mut check_now = false;
        egui::Window::new("Settings")
            .open(&mut open)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.strong("Updates");
                changed |= ui
                    .checkbox(
                        &mut self.settings.check_updates_on_start,
                        "check for a newer version when the app starts",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut self.settings.beta_updates, "include beta (pre-release) versions")
                    .on_hover_text("Betas come out more often and may be less tested. Turning this off never downgrades you.")
                    .changed();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!self.updates.checking, egui::Button::new("Check now"))
                        .clicked()
                    {
                        check_now = true;
                    }
                    if self.updates.checking {
                        ui.spinner();
                        ui.weak("checking…");
                    } else if let Some(t) = self.settings.last_update_check_unix {
                        ui.weak(format!("last checked {}", timefmt::readable(t)));
                    }
                });
                if let Some(v) = self.settings.skipped_version.clone() {
                    ui.horizontal(|ui| {
                        ui.weak(format!("version {v} is being skipped"));
                        if ui.small_button("offer it again").clicked() {
                            self.settings.skipped_version = None;
                            changed = true;
                        }
                    });
                }

                ui.add_space(8.0);
                ui.strong("Appearance");
                ui.horizontal(|ui| {
                    ui.label("Theme");
                    changed |= combo(
                        ui,
                        "theme",
                        &mut self.settings.theme,
                        &[
                            (Theme::System, "follow the system"),
                            (Theme::Dark, "dark"),
                            (Theme::Light, "light"),
                        ],
                    );
                });

                ui.add_space(8.0);
                ui.strong("About");
                ui.label(format!(
                    "BackupUtility {} ({})",
                    bkcore::VERSION,
                    update::platform_tag()
                ));
                ui.hyperlink_to("Releases and source on GitHub", update::RELEASES_URL);
                ui.add_space(4.0);
                egui::Grid::new("paths").num_columns(2).show(ui, |ui| {
                    let row = |ui: &mut egui::Ui, k: &str, v: String| {
                        ui.weak(k);
                        ui.add(egui::Label::new(egui::RichText::new(v).monospace()).selectable(true));
                        ui.end_row();
                    };
                    row(
                        ui,
                        "program",
                        bkcore::paths::install_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    );
                    row(ui, "jobs", bkcore::job::default_jobs_dir().display().to_string());
                    row(ui, "state and logs", bkcore::paths::state_root().display().to_string());
                    row(ui, "settings", Settings::path().display().to_string());
                });
            });
        if changed {
            self.save_settings();
        }
        if check_now {
            self.start_update_check(ctx, true);
        }
        if !open {
            self.show_settings = false;
        }
    }
}

/// What the top bar says about privileges, per platform. Running as a user on
/// Linux is the normal state, not a warning.
fn privilege_notice() -> (egui::Color32, &'static str) {
    let ok = egui::Color32::from_rgb(0x4C, 0xAF, 0x50);
    let warn = egui::Color32::from_rgb(0xE5, 0x7A, 0x24);
    let info = egui::Color32::GRAY;
    let elevated = bkcore::file::sys::is_elevated();
    if cfg!(windows) {
        if elevated {
            (ok, "elevated")
        } else {
            (
                warn,
                "not elevated — NTFS permissions, layout moves and scheduled tasks need Administrator",
            )
        }
    } else if elevated {
        (ok, "running as root — schedules install as system timers")
    } else {
        (
            info,
            "running as a user — schedules install as user timers; files you cannot read are skipped",
        )
    }
}

fn run_as_system_label() -> &'static str {
    if cfg!(windows) {
        "run as SYSTEM (works with nobody logged in)"
    } else {
        "run as a system service (needs root; as a user this installs a user timer instead)"
    }
}

/// A short badge for the job list: the last run's outcome and when.
fn status_badge(run: Option<&filestate::RunState>) -> (egui::Color32, String, String) {
    use bkcore::file::state::RunStatus;
    let Some(run) = run else {
        return (
            egui::Color32::GRAY,
            "never run".into(),
            "This job has not run yet".into(),
        );
    };
    let when = run
        .finished_unix
        .map(timefmt::readable)
        .unwrap_or_else(|| timefmt::readable(run.started_unix));
    match run.status {
        RunStatus::Completed => (
            egui::Color32::from_rgb(0x4C, 0xAF, 0x50),
            format!("✓ {when}"),
            "Last run completed".into(),
        ),
        RunStatus::Failed => (
            egui::Color32::from_rgb(0xD1, 0x4A, 0x4A),
            format!("✗ {when}"),
            "Last run failed — see the run history".into(),
        ),
        RunStatus::Cancelled => (
            egui::Color32::from_rgb(0xE5, 0x7A, 0x24),
            format!("⏸ {when}"),
            "Last run was stopped; the next run resumes it".into(),
        ),
        RunStatus::Running => (
            egui::Color32::from_rgb(0xE5, 0x7A, 0x24),
            format!("… {when}"),
            "Running now, or interrupted without a chance to save; the next run resumes it".into(),
        ),
    }
}
