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

use bkcore::file::mirror::{self, Event, MirrorReport};
use bkcore::file::plan::{human_bytes, CompareBy, MirrorMode};
use bkcore::file::state::{self as filestate, Progress};
use bkcore::file::Cancel;
use bkcore::job::{Frequency, Job, JobKind, JobStore, Schedule};
use bkcore::opt::{media, OptimizeSettings};
use bkcore::timefmt;
use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 700.0])
            .with_min_inner_size([820.0, 520.0])
            .with_title("BackupUtility"),
        ..Default::default()
    };
    eframe::run_native(
        "BackupUtility",
        opts,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
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
}

impl App {
    fn new() -> Self {
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
        };
        match JobStore::open(bkcore::job::default_jobs_dir()) {
            Ok(s) => {
                app.store = Some(s);
                app.reload_jobs();
            }
            Err(e) => app.load_error = Some(format!("cannot open the jobs directory: {e}")),
        }
        app.refresh_drives();
        app
    }

    fn reload_jobs(&mut self) {
        if let Some(store) = &self.store {
            match store.list() {
                Ok((jobs, broken)) => {
                    self.jobs = jobs;
                    self.broken = broken;
                }
                Err(e) => self.load_error = Some(e.to_string()),
            }
        }
    }

    fn refresh_drives(&mut self) {
        self.drives.clear();
        #[cfg(windows)]
        for letter in 'A'..='Z' {
            let root = format!("{letter}:\\");
            if std::path::Path::new(&root).exists() {
                if let Ok(info) = media::probe(&letter.to_string()) {
                    self.drives.push(info);
                }
            }
        }
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
                    self.notify(format!("saved '{name}', but the schedule failed: {e}"), true);
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
                        *thread_shared.phase.lock().unwrap() =
                            format!("scanning source — {} files, {}", s.files, human_bytes(s.bytes));
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
        .map(|d| d.join(if cfg!(windows) { "bkengine.exe" } else { "bkengine" }))
        .unwrap_or_else(|| std::path::PathBuf::from("bkengine"))
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.reap_run();
        if self.run.is_some() {
            // Keep the elapsed time and rate ticking even between engine events.
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        self.top_bar(ctx);
        self.side_panel(ctx);
        self.central(ctx);
        self.confirm_dialog(ctx);
        self.log_viewer(ctx);
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("BackupUtility");
                ui.separator();
                if bkcore::file::sys::is_elevated() {
                    ui.colored_label(egui::Color32::from_rgb(0x4C, 0xAF, 0x50), "elevated");
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xE5, 0x7A, 0x24),
                        "not elevated — VSS, layout moves and scheduling are unavailable",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
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
                            if ui
                                .selectable_label(self.selected == Some(i), label)
                                .clicked()
                            {
                                clicked = Some(i);
                            }
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
                changed |= ui.checkbox(&mut f.attributes, "attributes (read-only, hidden, system)").changed();
                changed |= ui.checkbox(&mut f.acls, "security descriptors (NTFS permissions)").changed();
                changed |= ui.checkbox(&mut f.alternate_data_streams, "alternate data streams").changed();
                changed |= ui.checkbox(&mut f.sparse, "sparse regions").changed();
                changed |= ui.checkbox(&mut f.verify, "verify every file after writing (slower)").changed();
                ui.add_space(4.0);
                changed |= ui.checkbox(&mut spec.preserve_hardlinks, "preserve hard links").changed();
                changed |= ui.checkbox(&mut spec.follow_links, "follow symlinks and junctions (risky)").changed();
                changed |= ui.checkbox(&mut spec.cross_volume, "descend into mounted volumes").changed();
                ui.add_space(4.0);
                changed |= ui
                    .checkbox(
                        &mut spec.check_free_space,
                        "refuse to start if the destination is too small",
                    )
                    .changed();
                changed |= ui
                    .checkbox(&mut spec.log, "write a run log (needed to review a scheduled run)")
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
                    .checkbox(&mut s.run_as_system, "run as SYSTEM (works with nobody logged in)")
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
                        return;
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
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    for e in &errors {
                        ui.colored_label(egui::Color32::from_rgb(0xD1, 0x4A, 0x4A), e);
                    }
                });
            });
        }

        ui.separator();
        if handle.cancel.is_cancelled() {
            ui.weak("stopping — finishing the current file and saving progress…");
        } else if ui.button("Stop").on_hover_text(
            "Finishes the file in flight and saves progress. The next run resumes from here.",
        ).clicked()
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
