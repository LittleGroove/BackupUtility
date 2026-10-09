//! The two small windows that are not the application: the install offer a
//! copy makes when it is started from anywhere but its home, and the
//! confirmation behind the entry in Settings > Apps. Both are plain eframe
//! windows in this same binary, so there is no second executable to ship and
//! the person sees the same icon and theme throughout.
//!
//! The decision happens in [`decide`], before the application is built, so
//! the update check in `App::new` only ever runs in the installed copy.

use bkcore::install::{self, Existing, Options, Residence, UninstallOptions};
use bkcore::settings::{Settings, Theme};
use bkcore::update::{local_name, relaunch, Version};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What `main` does with the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    Run,
    Install,
    Uninstall,
}

pub fn decide(args: &[String]) -> Launch {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    if has("--uninstall") {
        return Launch::Uninstall;
    }
    if has("--install") {
        return Launch::Install;
    }
    // A development build is never a distributed one: it runs where cargo
    // put it. `--install` above is how to try the installer from one.
    if has("--portable") || cfg!(debug_assertions) {
        return Launch::Run;
    }
    match install::residence() {
        Residence::Elsewhere => Launch::Install,
        Residence::Installed | Residence::Portable => Launch::Run,
    }
}

fn window(title: &str, size: [f32; 2]) -> eframe::NativeOptions {
    let icon = egui::IconData {
        rgba: include_bytes!("../../../assets/icon_64.rgba").to_vec(),
        width: 64,
        height: 64,
    };
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_resizable(false)
            .with_title(title)
            .with_icon(Arc::new(icon)),
        ..Default::default()
    }
}

fn apply_theme(ctx: &egui::Context) {
    match Settings::load().theme {
        Theme::Dark => ctx.set_visuals(egui::Visuals::dark()),
        Theme::Light => ctx.set_visuals(egui::Visuals::light()),
        Theme::System => {}
    }
}

const WARN: egui::Color32 = egui::Color32::from_rgb(0xE5, 0x7A, 0x24);
const FAIL: egui::Color32 = egui::Color32::from_rgb(0xD1, 0x4A, 0x4A);

/// A step running on a worker thread, and what it reports back.
struct Work {
    status: Arc<Mutex<String>>,
    result: Arc<Mutex<Option<Result<install::Report, String>>>>,
    done: Arc<AtomicBool>,
}

impl Work {
    fn start(
        ctx: &egui::Context,
        job: impl FnOnce(&mut dyn FnMut(&str)) -> Result<install::Report, String> + Send + 'static,
    ) -> Work {
        let work = Work {
            status: Arc::new(Mutex::new(String::new())),
            result: Arc::new(Mutex::new(None)),
            done: Arc::new(AtomicBool::new(false)),
        };
        let (status, result, done, ctx) = (
            work.status.clone(),
            work.result.clone(),
            work.done.clone(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            let outcome = job(&mut |step: &str| {
                *status.lock().unwrap() = step.to_string();
                ctx.request_repaint();
            });
            *result.lock().unwrap() = Some(outcome);
            done.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        });
        work
    }

    fn take(&self) -> Option<Result<install::Report, String>> {
        if self.done.load(Ordering::SeqCst) {
            self.result.lock().unwrap().take()
        } else {
            None
        }
    }

    fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Install,
    LaunchInstalled,
}

/// Told to `main` after the window closes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Quit,
    /// The installed copy is running; the one we started from goes.
    HandedOver,
    /// A newer installed copy was launched instead; this one stays.
    LaunchedExisting,
}

struct InstallApp {
    source_dir: PathBuf,
    home: PathBuf,
    existing: Option<Existing>,
    choice: Choice,
    opts: Options,
    work: Option<Work>,
    error: Option<String>,
    /// From a finished install, shown before the window closes.
    warnings: Vec<String>,
    outcome: Arc<Mutex<Outcome>>,
}

impl InstallApp {
    fn newer_installed(&self) -> Option<Version> {
        self.existing
            .as_ref()?
            .version
            .filter(|v| *v > Version::current())
    }

    fn installed_version(&self) -> Option<Version> {
        self.existing.as_ref()?.version
    }

    fn set_outcome(&self, outcome: Outcome) {
        *self.outcome.lock().unwrap() = outcome;
    }

    fn installed_gui(&self) -> PathBuf {
        self.home.join(local_name("bkgui"))
    }

    fn launch_installed(&mut self, ctx: &egui::Context) {
        match relaunch(&self.installed_gui(), &[]) {
            Ok(_) => {
                self.set_outcome(Outcome::LaunchedExisting);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.error = Some(format!("could not start the installed copy: {e}")),
        }
    }

    fn start_install(&mut self, ctx: &egui::Context) {
        let (source, opts) = (self.source_dir.clone(), self.opts.clone());
        self.error = None;
        self.work = Some(Work::start(ctx, move |progress| {
            install::install(&source, &opts, progress).map_err(|e| e.to_string())
        }));
    }

    fn reap(&mut self, ctx: &egui::Context) {
        let Some(result) = self.work.as_ref().and_then(Work::take) else {
            return;
        };
        self.work = None;
        match result {
            Ok(report) => {
                self.warnings = report.warnings;
                // Installed, whatever happens next: the source copy is spent.
                self.set_outcome(Outcome::HandedOver);
                match relaunch(&self.installed_gui(), &[]) {
                    Ok(_) if self.warnings.is_empty() => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close)
                    }
                    Ok(_) => {}
                    Err(e) => self.warnings.push(format!(
                        "installed, but it could not be started automatically ({e}); \
                         start it from the menu"
                    )),
                }
            }
            Err(e) => self.error = Some(e),
        }
    }
}

impl eframe::App for InstallApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.reap(ctx);
        let busy = self.work.is_some();
        let finished = !busy && *self.outcome.lock().unwrap() == Outcome::HandedOver;
        let newer = self.newer_installed();
        let installed = self.installed_version();
        let has_existing = self.existing.is_some();
        let current = Version::current();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading(format!("Install BackupUtility {}", bkcore::VERSION));
            ui.add_space(8.0);

            if finished {
                ui.label("BackupUtility is installed and running.");
                for w in &self.warnings {
                    ui.colored_label(WARN, format!("⚠ {w}"));
                }
                ui.add_space(12.0);
                if ui.button("Close").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                return;
            }

            ui.label(
                "BackupUtility lives in one fixed place, so scheduled jobs and updates \
                 keep working. It will be installed to:",
            );
            ui.add_space(4.0);
            ui.monospace(self.home.display().to_string());
            ui.add_space(8.0);

            if has_existing {
                match (installed, newer) {
                    (_, Some(v)) => {
                        ui.label(format!(
                            "Version {v} is already installed there, newer than this {current}."
                        ));
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.radio_value(
                                &mut self.choice,
                                Choice::LaunchInstalled,
                                format!("Launch the installed {v}; this copy stays where it is"),
                            );
                            ui.radio_value(
                                &mut self.choice,
                                Choice::Install,
                                format!("Replace it with this {current} (a downgrade)"),
                            );
                        });
                    }
                    (Some(v), None) if v == current => {
                        ui.label(format!(
                            "Version {v} is already installed there and will be reinstalled."
                        ));
                    }
                    (Some(v), None) => {
                        ui.label(format!(
                            "Version {v} is installed there and will be updated to {current}."
                        ));
                    }
                    (None, None) => {
                        ui.label("An earlier copy is installed there and will be replaced.");
                    }
                }
                ui.add_space(8.0);
            }

            ui.add_enabled_ui(!busy && self.choice == Choice::Install, |ui| {
                ui.checkbox(
                    &mut self.opts.menu_shortcut,
                    if cfg!(windows) {
                        "Add to the Start Menu"
                    } else {
                        "Add to the application menu"
                    },
                );
                ui.checkbox(&mut self.opts.desktop_shortcut, "Create a desktop shortcut");
            });
            ui.add_space(8.0);
            if self.choice == Choice::Install {
                ui.weak(
                    "Scheduled jobs are re-pointed at the installed copy, and the copy \
                     you started is removed once it is running.",
                );
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.colored_label(FAIL, e);
            }
            ui.add_space(12.0);

            ui.horizontal(|ui| {
                if let Some(work) = &self.work {
                    ui.spinner();
                    ui.label(work.status());
                    return;
                }
                let primary = match (self.choice, newer, installed) {
                    (Choice::LaunchInstalled, _, _) => "Launch",
                    (Choice::Install, Some(_), _) => "Replace",
                    (Choice::Install, None, Some(v)) if v == current => "Reinstall",
                    (Choice::Install, None, Some(_)) => "Update",
                    (Choice::Install, None, None) => "Install",
                };
                if ui.button(primary).clicked() {
                    match self.choice {
                        Choice::Install => self.start_install(ctx),
                        Choice::LaunchInstalled => self.launch_installed(ctx),
                    }
                }
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

/// The install window, then the hand-over: once the installed copy is
/// running, the engine beside this executable and this executable itself
/// are deleted.
pub fn run_install() -> eframe::Result<()> {
    let source_dir = bkcore::paths::install_dir().unwrap_or_default();
    let outcome = Arc::new(Mutex::new(Outcome::Quit));
    let mut app = InstallApp {
        source_dir: source_dir.clone(),
        home: install::home_dir(),
        existing: install::existing_install(),
        choice: Choice::Install,
        opts: Options::default(),
        work: None,
        error: None,
        warnings: Vec::new(),
        outcome: outcome.clone(),
    };
    if app.newer_installed().is_some() {
        app.choice = Choice::LaunchInstalled;
    }
    eframe::run_native(
        "BackupUtility setup",
        window("BackupUtility setup", [560.0, 330.0]),
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )?;
    if *outcome.lock().unwrap() == Outcome::HandedOver {
        // Best effort: read-only media, or an engine mid-job, just stays.
        let _ = std::fs::remove_file(source_dir.join(local_name("bkengine")));
        let _ = install::delete_self();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

struct UninstallApp {
    home: PathBuf,
    opts: UninstallOptions,
    work: Option<Work>,
    /// Once the removal ran: its warnings, or why it failed.
    result: Option<Result<Vec<String>, String>>,
}

impl eframe::App for UninstallApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(r) = self.work.as_ref().and_then(Work::take) {
            self.work = None;
            let home = self.home.clone();
            self.result = Some(r.map(|report| {
                let mut warnings = report.warnings;
                if let Err(e) = install::finish_uninstall(&home) {
                    warnings.push(format!("the program folder was not removed: {e}"));
                }
                warnings
            }));
        }
        let purge = self.opts.purge_data;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Remove BackupUtility");
            ui.add_space(8.0);
            match &self.result {
                Some(Ok(warnings)) => {
                    ui.label("BackupUtility has been removed.");
                    if !purge {
                        ui.weak("Jobs, settings, save states and run logs were kept.");
                    }
                    for w in warnings {
                        ui.colored_label(WARN, format!("⚠ {w}"));
                    }
                    ui.add_space(12.0);
                    if ui.button("Close").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Some(Err(e)) => {
                    ui.colored_label(FAIL, format!("The removal failed: {e}"));
                    ui.add_space(12.0);
                    if ui.button("Close").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                None => {
                    ui.label(
                        "This removes the program, its shortcuts and its scheduled tasks from:",
                    );
                    ui.add_space(4.0);
                    ui.monospace(self.home.display().to_string());
                    ui.add_space(8.0);
                    let busy = self.work.is_some();
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.checkbox(
                            &mut self.opts.purge_data,
                            "Also delete jobs, settings, save states and run logs",
                        );
                    });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if let Some(work) = &self.work {
                            ui.spinner();
                            ui.label(work.status());
                            return;
                        }
                        if ui.button("Remove").clicked() {
                            let opts = self.opts.clone();
                            let me = std::env::current_exe().ok();
                            self.work = Some(Work::start(ctx, move |progress| {
                                install::uninstall(&opts, me.as_deref(), progress)
                                    .map_err(|e| e.to_string())
                            }));
                        }
                        if ui.button("Cancel").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                }
            }
        });
    }
}

pub fn run_uninstall() -> eframe::Result<()> {
    let app = UninstallApp {
        home: install::home_dir(),
        opts: UninstallOptions::default(),
        work: None,
        result: None,
    };
    eframe::run_native(
        "Remove BackupUtility",
        window("Remove BackupUtility", [520.0, 250.0]),
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
