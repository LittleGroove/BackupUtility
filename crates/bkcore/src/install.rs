//! Giving the program a fixed home.
//!
//! Three things in this tool depend on the binaries staying where they are:
//!
//! * a scheduled job is registered with the **absolute path** of `bkengine`,
//!   so a program run from a Downloads folder stops working the day that
//!   folder is tidied;
//! * the updater swaps new binaries in **beside the running one**, wherever
//!   that is: a USB stick, or the temporary folder Explorer unpacks a zip
//!   into when a file inside it is double-clicked;
//! * on Windows the scheduled task runs the engine as SYSTEM, and a binary in
//!   a user-writable folder is one a standard user could replace. The jobs
//!   directory is already locked down for that reason (see
//!   [`crate::paths::restrict_to_admins`]); the binaries belong under the same
//!   protection.
//!
//! So the GUI, when it finds itself anywhere other than the home below, offers
//! to copy itself there, hand over to the installed copy and remove the copy
//! it was started from. `bkengine install` does the same from a terminal.
//!
//! | | Windows | Linux as root | Linux as a user |
//! |---|---|---|---|
//! | binaries | `%ProgramFiles%\BackupUtility` | `/opt/backuputility` | `~/.local/share/BackupUtility` |
//! | menu entry | Start Menu, all users | `/usr/share/applications` | `~/.local/share/applications` |
//! | desktop shortcut | the public desktop | `~/Desktop` | `~/Desktop` |
//! | on the PATH | — | `/usr/local/bin` | `~/.local/bin` |
//! | listed in | Settings > Apps | — | — |
//!
//! Jobs, settings and run state are never moved by an install, and an
//! uninstall keeps them unless asked to purge. An install re-points every
//! registered schedule at the installed engine, which is what makes moving
//! from an unzipped-anywhere copy safe: the old engine is deleted afterwards.
//!
//! ## Portable use
//! A file named `.portable` beside the executable switches all of this off:
//! the program then runs where it is. The release tooling drops that file
//! into the staging folder so a build can be tested without installing it;
//! it is not in the published archive.

use crate::job::JobStore;
use crate::update::{self, local_name, Version, BINARIES};
use crate::{scheduler, BkError, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const APP_NAME: &str = "BackupUtility";
/// The identity the Windows shell groups and pins the window under. The
/// process sets it on itself at startup and the shortcuts carry it, so a pin
/// made from either ends up on the same taskbar button.
pub const APP_USER_MODEL_ID: &str = "LittleGroove.BackupUtility";
/// Beside the executable: run from here, never offer to install.
pub const PORTABLE_MARKER: &str = ".portable";
/// Scratch directory inside the home while an install copies files in.
const STAGE_DIR: &str = ".bku-install";
const LICENSE_FILE: &str = "LICENSE";
/// The application icon, for the Linux menu entry. Windows reads it out of
/// the executable's resources.
pub const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

// ---------------------------------------------------------------------------
// Where home is, and whether we are there
// ---------------------------------------------------------------------------

/// The directory an installed copy lives in on this machine.
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        win::program_files().join(APP_NAME)
    }
    #[cfg(not(windows))]
    {
        nix::dirs().home
    }
}

/// Is `dir` the home directory (by identity, so any spelling of the path counts)?
pub fn is_home(dir: &Path) -> bool {
    update::same_file(dir, &home_dir())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residence {
    /// Running from the home: nothing to do.
    Installed,
    /// A [`PORTABLE_MARKER`] sits beside the executable: leave it alone.
    Portable,
    /// Anywhere else: an install is wanted.
    Elsewhere,
}

/// Where the running executable stands.
pub fn residence() -> Residence {
    match std::env::current_exe() {
        Ok(exe) => residence_of(&exe, &home_dir()),
        // Not knowing where we are is no reason to start copying things about.
        Err(_) => Residence::Portable,
    }
}

pub fn residence_of(exe: &Path, home: &Path) -> Residence {
    if exe
        .parent()
        .is_some_and(|d| d.join(PORTABLE_MARKER).exists())
    {
        return Residence::Portable;
    }
    let Some(name) = exe.file_name() else {
        return Residence::Portable;
    };
    if update::same_file(exe, &home.join(name)) {
        Residence::Installed
    } else {
        Residence::Elsewhere
    }
}

/// What is already in the home.
#[derive(Debug, Clone)]
pub struct Existing {
    pub home: PathBuf,
    /// `None` when binaries are there but the engine could not say which
    /// version they are; treated as older than anything.
    pub version: Option<Version>,
}

pub fn existing_install() -> Option<Existing> {
    let home = home_dir();
    let engine = home.join(local_name("bkengine"));
    let gui = home.join(local_name("bkgui"));
    if !engine.exists() && !gui.exists() {
        return None;
    }
    let version = if engine.exists() {
        installed_version(&engine)
    } else {
        None
    };
    Some(Existing { home, version })
}

/// Ask the installed engine what version it is. Works for every version ever
/// released, which a marker file written by the installer would not.
fn installed_version(engine: &Path) -> Option<Version> {
    let mut cmd = Command::new(engine);
    cmd.arg("version")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console flashing up behind the window.
        cmd.creation_flags(win::CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    parse_version_line(&String::from_utf8_lossy(&out.stdout))
}

/// The first line of `bkengine version` is `bkengine X.Y.Z (platform)`.
pub fn parse_version_line(s: &str) -> Option<Version> {
    s.lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .and_then(Version::parse)
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub desktop_shortcut: bool,
    pub menu_shortcut: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            desktop_shortcut: false,
            menu_shortcut: true,
        }
    }
}

/// What an install or uninstall did, and what it could not do.
#[derive(Debug, Default)]
pub struct Report {
    pub home: PathBuf,
    /// Jobs whose schedule now runs the installed engine.
    pub rescheduled: Vec<String>,
    /// Things that failed without stopping the operation.
    pub warnings: Vec<String>,
}

/// Install the binaries beside `source_dir` into the home, create the
/// shortcuts asked for, register with the OS, and re-point every registered
/// schedule at the installed engine.
///
/// Running it from the home itself is fine and refreshes everything but the
/// binaries, so `bkengine install` is also how to add a shortcut later.
pub fn install(
    source_dir: &Path,
    opts: &Options,
    progress: &mut dyn FnMut(&str),
) -> Result<Report> {
    #[cfg(windows)]
    crate::file::sys::require_elevation("installing into Program Files")?;
    let home = home_dir();
    let mut report = Report {
        home: home.clone(),
        ..Default::default()
    };

    if !update::same_file(source_dir, &home) {
        progress("copying the program");
        copy_binaries(source_dir, &home)?;
    }
    let _ = update::cleanup_old_binaries(&home);

    progress("creating shortcuts");
    report.warnings.extend(shortcuts::create(&home, opts));
    report
        .warnings
        .extend(register_with_os(&home, crate::VERSION));

    progress("re-pointing scheduled jobs");
    let (done, warnings) = repoint_schedules(&home.join(local_name("bkengine")));
    report.rescheduled = done;
    report.warnings.extend(warnings);
    Ok(report)
}

/// Copy both binaries (and the licence) from `source_dir` into `home`,
/// replacing what is there the way an update does, so an engine that is
/// running a job at that moment keeps running from its parked copy.
pub fn copy_binaries(source_dir: &Path, home: &Path) -> Result<()> {
    let err = |m: String| BkError::Other(anyhow::anyhow!(m));
    for b in BINARIES {
        let name = local_name(b);
        if !source_dir.join(&name).is_file() {
            return Err(err(format!(
                "{name} is not beside this program in {} — extract the whole archive \
                 first, then start the program from the extracted folder",
                source_dir.display()
            )));
        }
    }
    std::fs::create_dir_all(home)
        .map_err(|e| err(format!("cannot create {}: {e}", home.display())))?;
    let stage = home.join(STAGE_DIR);
    std::fs::create_dir_all(&stage)?;
    for b in BINARIES {
        let name = local_name(b);
        let tmp = stage.join(&name);
        std::fs::copy(source_dir.join(&name), &tmp)
            .map_err(|e| err(format!("cannot copy {name} into {}: {e}", home.display())))?;
        update::make_executable(&tmp)?;
        strip_mark_of_the_web(&tmp);
        update::swap_file(&tmp, &home.join(&name))
            .map_err(|e| err(format!("could not install {name}: {e}")))?;
    }
    let license = source_dir.join(LICENSE_FILE);
    if license.is_file() {
        let _ = std::fs::copy(&license, home.join(LICENSE_FILE));
    }
    let _ = std::fs::remove_dir_all(&stage);
    Ok(())
}

/// A file copied out of a downloaded archive keeps the download's zone in an
/// alternate stream, and the first launch from a shortcut would get the same
/// SmartScreen prompt the person already answered when they started the
/// installer. An installed program is not a download.
fn strip_mark_of_the_web(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        let _ = std::fs::remove_file(stream);
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

/// Re-register every job that has a schedule *and* is registered now, with
/// `engine` as the program to run. Jobs whose schedule was removed by hand
/// stay removed. Returns the names done and the failures.
pub fn repoint_schedules(engine: &Path) -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut warnings = Vec::new();
    let dir = crate::job::default_jobs_dir();
    if !dir.is_dir() {
        return (done, warnings);
    }
    let jobs = match JobStore::open(&dir).and_then(|s| s.list()) {
        Ok((jobs, _)) => jobs,
        Err(e) => {
            warnings.push(format!("the jobs directory could not be read: {e}"));
            return (done, warnings);
        }
    };
    for job in jobs
        .iter()
        .filter(|j| j.schedule.is_some() && scheduler::is_registered(j))
    {
        match scheduler::register(job, engine, Some(&dir)) {
            Ok(()) => done.push(job.name.clone()),
            Err(e) => warnings.push(format!(
                "the schedule for '{}' could not be re-pointed: {e}",
                job.name
            )),
        }
    }
    (done, warnings)
}

/// Tell the OS about the install. Windows: the entry in Settings > Apps,
/// which the updater refreshes so the version shown there follows
/// self-updates. Linux: the links that put both binaries on the PATH.
pub fn register_with_os(home: &Path, version: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        match win::write_uninstall_key(home, version) {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("the entry in Settings > Apps was not written: {e}")],
        }
    }
    #[cfg(not(windows))]
    {
        let _ = version;
        nix::link_into_path(home)
    }
}

fn unregister_from_os() -> Vec<String> {
    #[cfg(windows)]
    {
        match win::delete_uninstall_key() {
            Ok(()) => Vec::new(),
            Err(e) => vec![format!("the entry in Settings > Apps was not removed: {e}")],
        }
    }
    #[cfg(not(windows))]
    {
        nix::unlink_from_path()
    }
}

/// Keep what the OS knows about the install current. Cheap and idempotent;
/// the GUI calls it on every start from the home.
pub fn refresh_registration() {
    if residence() == Residence::Installed {
        let _ = register_with_os(&home_dir(), crate::VERSION);
    }
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallOptions {
    /// Also delete jobs, settings, save states and run logs.
    pub purge_data: bool,
}

/// Remove the scheduled tasks, the shortcuts, the OS registration and the
/// installed binaries — all but the executable doing the removing, which
/// [`finish_uninstall`] takes care of last.
pub fn uninstall(
    opts: &UninstallOptions,
    running: Option<&Path>,
    progress: &mut dyn FnMut(&str),
) -> Result<Report> {
    let home = home_dir();
    let mut report = Report {
        home: home.clone(),
        ..Default::default()
    };

    progress("removing scheduled tasks");
    report.warnings.extend(unschedule_all());

    progress("removing shortcuts");
    report.warnings.extend(shortcuts::remove());
    report.warnings.extend(unregister_from_os());

    progress("removing the program");
    if home.is_dir() {
        let _ = update::cleanup_old_binaries(&home);
        let _ = std::fs::remove_dir_all(home.join(STAGE_DIR));
        let mut names: Vec<String> = BINARIES.iter().map(|b| local_name(b)).collect();
        names.push(LICENSE_FILE.to_string());
        for name in names {
            let path = home.join(&name);
            if !path.exists() || running.is_some_and(|r| update::same_file(r, &path)) {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&path) {
                report.warnings.push(format!(
                    "{} was not removed ({e}); a scheduled job may be running from it",
                    path.display()
                ));
            }
        }
    }

    if opts.purge_data {
        progress("deleting jobs, settings and run state");
        // State first: on Windows it lives under the configuration root.
        for dir in [crate::paths::state_root(), crate::paths::config_root()] {
            if dir.is_dir() {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    report
                        .warnings
                        .push(format!("{} was not deleted: {e}", dir.display()));
                }
            }
        }
    }
    Ok(report)
}

fn unschedule_all() -> Vec<String> {
    let mut warnings = Vec::new();
    let dir = crate::job::default_jobs_dir();
    if !dir.is_dir() {
        return warnings;
    }
    match JobStore::open(&dir).and_then(|s| s.list()) {
        Ok((jobs, _)) => {
            for job in jobs.iter().filter(|j| j.schedule.is_some()) {
                if let Err(e) = scheduler::unregister(job) {
                    warnings.push(format!(
                        "the schedule for '{}' was not removed: {e}",
                        job.name
                    ));
                }
            }
        }
        Err(e) => warnings.push(format!("the jobs directory could not be read: {e}")),
    }
    warnings
}

/// The last step of an uninstall: if this executable lives in the home,
/// schedule its own deletion, then remove the home directory if nothing else
/// is left in it. Exit soon after.
pub fn finish_uninstall(home: &Path) -> std::io::Result<()> {
    let me = std::env::current_exe()?;
    if me.parent().is_some_and(|d| update::same_file(d, home)) {
        self_replace::self_delete_outside_path(home)?;
    }
    // Only an empty directory goes; anything a person put there stays.
    let _ = std::fs::remove_dir(home);
    Ok(())
}

/// Delete the running executable: now on Unix, when it exits on Windows.
pub fn delete_self() -> std::io::Result<()> {
    self_replace::self_delete()
}

// ---------------------------------------------------------------------------
// Windows: known folders, shortcuts, the Apps entry
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use windows::core::{Interface, GUID, PCWSTR, PROPVARIANT};
    use windows::Win32::Foundation::{BOOL, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
        KEY_WOW64_64KEY, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY};
    use windows::Win32::UI::Shell::{
        FOLDERID_CommonPrograms, FOLDERID_ProgramFiles, FOLDERID_PublicDesktop, IShellLinkW,
        SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT,
    };

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const UNINSTALL_KEY: &str =
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\BackupUtility";
    /// `System.AppUserModel.ID`, spelled out here rather than pulling in a
    /// whole feature of the `windows` crate for one constant.
    const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
        pid: 5,
    };

    /// Null-terminated UTF-16, the plain spelling (shell APIs do not want `\\?\`).
    fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// `REG_SZ` bytes: UTF-16LE with a terminator.
    pub fn utf16z_bytes(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    fn known_folder(id: &GUID) -> Option<PathBuf> {
        // SAFETY: a documented shell call; the buffer it returns is freed with
        // CoTaskMemFree exactly once, after being copied out.
        unsafe {
            let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            s.map(PathBuf::from)
        }
    }

    pub fn program_files() -> PathBuf {
        known_folder(&FOLDERID_ProgramFiles)
            .or_else(|| std::env::var_os("ProgramFiles").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
    }

    /// The all-users Start Menu programs folder. Per-machine, like the
    /// install: an elevated process's own profile may belong to whoever typed
    /// the UAC password, not to the person at the keyboard.
    pub fn start_menu_dir() -> Option<PathBuf> {
        known_folder(&FOLDERID_CommonPrograms)
    }

    pub fn public_desktop_dir() -> Option<PathBuf> {
        known_folder(&FOLDERID_PublicDesktop)
    }

    /// COM for the duration of a shortcut write. Initialising a thread that
    /// already has COM in another mode fails harmlessly; the calls still work.
    struct ComScope(bool);

    impl ComScope {
        fn enter() -> Self {
            // SAFETY: plain initialisation call, balanced by Drop when it succeeded.
            ComScope(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
        }
    }

    impl Drop for ComScope {
        fn drop(&mut self) {
            if self.0 {
                // SAFETY: balances the successful CoInitializeEx above.
                unsafe { CoUninitialize() }
            }
        }
    }

    /// Write a `.lnk` to `target`, carrying the application identity so a
    /// pin made from it joins the running window's taskbar button.
    pub fn create_shortcut(
        lnk: &Path,
        target: &Path,
        description: &str,
    ) -> windows::core::Result<()> {
        let _com = ComScope::enter();
        let target_w = wide(target.as_os_str());
        let dir_w = wide(target.parent().unwrap_or(target).as_os_str());
        let desc_w = wide(OsStr::new(description));
        let lnk_w = wide(lnk.as_os_str());
        let aumid = PROPVARIANT::from(super::APP_USER_MODEL_ID);
        // SAFETY: standard IShellLink use; every string outlives the call it
        // is passed to, and the interfaces are released by their Drop.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(target_w.as_ptr()))?;
            link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()))?;
            link.SetDescription(PCWSTR(desc_w.as_ptr()))?;
            link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)?;
            let store: IPropertyStore = link.cast()?;
            store.SetValue(&PKEY_APP_USER_MODEL_ID, &aumid)?;
            store.Commit()?;
            let file: IPersistFile = link.cast()?;
            file.Save(PCWSTR(lnk_w.as_ptr()), BOOL::from(true))?;
        }
        Ok(())
    }

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by RegCreateKeyExW and is closed once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// The entry Settings > Apps lists, with the uninstall command.
    pub fn write_uninstall_key(home: &Path, version: &str) -> std::io::Result<()> {
        let gui = home.join(super::local_name("bkgui"));
        let engine = home.join(super::local_name("bkengine"));
        let size_kb = [&gui, &engine]
            .iter()
            .filter_map(|p| p.metadata().ok())
            .map(|m| m.len() / 1024)
            .sum::<u64>()
            .min(u32::MAX as u64) as u32;
        let sub = wide(OsStr::new(UNINSTALL_KEY));
        let mut raw = HKEY::default();
        // SAFETY: documented registry calls with valid, null-terminated inputs.
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(sub.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE | KEY_WOW64_64KEY,
                None,
                &mut raw,
                None,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc.0 as i32));
        }
        let key = Key(raw);
        let set = |name: &str, kind, data: &[u8]| -> std::io::Result<()> {
            let name_w = wide(OsStr::new(name));
            // SAFETY: as above; `data` is the full value, read before return.
            let rc = unsafe { RegSetValueExW(key.0, PCWSTR(name_w.as_ptr()), 0, kind, Some(data)) };
            if rc == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(std::io::Error::from_raw_os_error(rc.0 as i32))
            }
        };
        let sz = |name: &str, value: &str| set(name, REG_SZ, &utf16z_bytes(value));
        let dword = |name: &str, value: u32| set(name, REG_DWORD, &value.to_le_bytes());
        sz("DisplayName", super::APP_NAME)?;
        sz("DisplayVersion", version)?;
        sz("Publisher", "LittleGroove")?;
        sz("InstallLocation", &home.display().to_string())?;
        sz("DisplayIcon", &gui.display().to_string())?;
        sz(
            "UninstallString",
            &format!("\"{}\" --uninstall", gui.display()),
        )?;
        sz(
            "QuietUninstallString",
            &format!("\"{}\" uninstall --yes", engine.display()),
        )?;
        sz("URLInfoAbout", crate::update::RELEASES_URL)?;
        dword("NoModify", 1)?;
        dword("NoRepair", 1)?;
        dword("EstimatedSize", size_kb)?;
        Ok(())
    }

    pub fn delete_uninstall_key() -> std::io::Result<()> {
        let sub = wide(OsStr::new(UNINSTALL_KEY));
        // SAFETY: documented registry call with a valid, null-terminated path.
        let rc = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr())) };
        // Absent is what we want.
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(rc.0 as i32))
        }
    }
}

#[cfg(windows)]
mod shortcuts {
    use super::{local_name, win, Options};
    use std::path::Path;

    const LINK_NAME: &str = "BackupUtility.lnk";
    const DESCRIPTION: &str = "Mirror a path to another drive, on a schedule";

    pub fn create(home: &Path, opts: &Options) -> Vec<String> {
        let mut warnings = Vec::new();
        let target = home.join(local_name("bkgui"));
        let mut wanted = Vec::new();
        if opts.menu_shortcut {
            match win::start_menu_dir() {
                Some(d) => wanted.push(("Start Menu", d.join(LINK_NAME))),
                None => warnings.push("the Start Menu folder could not be found".to_string()),
            }
        }
        if opts.desktop_shortcut {
            match win::public_desktop_dir() {
                Some(d) => wanted.push(("desktop", d.join(LINK_NAME))),
                None => warnings.push("the desktop folder could not be found".to_string()),
            }
        }
        for (what, lnk) in wanted {
            if let Err(e) = win::create_shortcut(&lnk, &target, DESCRIPTION) {
                warnings.push(format!("the {what} shortcut was not created: {e}"));
            }
        }
        warnings
    }

    pub fn remove() -> Vec<String> {
        let mut warnings = Vec::new();
        for dir in [win::start_menu_dir(), win::public_desktop_dir()]
            .into_iter()
            .flatten()
        {
            let lnk = dir.join(LINK_NAME);
            if lnk.exists() {
                if let Err(e) = std::fs::remove_file(&lnk) {
                    warnings.push(format!("{} was not removed: {e}", lnk.display()));
                }
            }
        }
        warnings
    }
}

// ---------------------------------------------------------------------------
// Linux: XDG directories, the menu entry, PATH links
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
mod nix {
    use super::BINARIES;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub const DESKTOP_FILE: &str = "backuputility.desktop";
    pub const ICON_FILE: &str = "backuputility.png";

    pub struct Dirs {
        pub home: PathBuf,
        pub bin: PathBuf,
        pub applications: PathBuf,
        pub icons: PathBuf,
    }

    /// The same layout `packaging/linux/install.sh` has always produced.
    pub fn dirs() -> Dirs {
        if crate::file::sys::is_elevated() {
            Dirs {
                home: PathBuf::from("/opt/backuputility"),
                bin: PathBuf::from("/usr/local/bin"),
                applications: PathBuf::from("/usr/share/applications"),
                icons: PathBuf::from("/usr/share/icons/hicolor/256x256/apps"),
            }
        } else {
            let data = crate::paths::xdg("XDG_DATA_HOME", ".local/share");
            Dirs {
                home: data.join("BackupUtility"),
                bin: crate::paths::home().join(".local/bin"),
                applications: data.join("applications"),
                icons: data.join("icons/hicolor/256x256/apps"),
            }
        }
    }

    pub fn desktop_entry(home: &Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=BackupUtility\n\
             Comment=Mirror a path to another drive, on a schedule\n\
             Exec={}\n\
             Icon=backuputility\n\
             Terminal=false\n\
             Categories=System;Utility;\n\
             StartupWMClass=bkgui\n",
            home.join("bkgui").display()
        )
    }

    /// The desktop folder as the desktop environment names it (it is
    /// localised), falling back to `~/Desktop`.
    pub fn desktop_dir() -> PathBuf {
        let home = crate::paths::home();
        if let Ok(out) = Command::new("xdg-user-dir").arg("DESKTOP").output() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // With no desktop configured the tool answers with $HOME itself.
            if out.status.success() && !s.is_empty() && Path::new(&s) != home {
                return PathBuf::from(s);
            }
        }
        home.join("Desktop")
    }

    pub fn link_into_path(home: &Path) -> Vec<String> {
        let d = dirs();
        let mut warnings = Vec::new();
        if let Err(e) = std::fs::create_dir_all(&d.bin) {
            warnings.push(format!("cannot create {}: {e}", d.bin.display()));
            return warnings;
        }
        for b in BINARIES {
            let link = d.bin.join(b);
            let target = home.join(b);
            if std::fs::read_link(&link).is_ok_and(|t| t == target) {
                continue;
            }
            let _ = std::fs::remove_file(&link);
            if let Err(e) = std::os::unix::fs::symlink(&target, &link) {
                warnings.push(format!("{} was not linked: {e}", link.display()));
            }
        }
        if !on_path(&d.bin) {
            warnings.push(format!(
                "{} is not on your PATH yet; log out and in, or use the full path",
                d.bin.display()
            ));
        }
        warnings
    }

    pub fn unlink_from_path() -> Vec<String> {
        let d = dirs();
        let mut warnings = Vec::new();
        for b in BINARIES {
            let link = d.bin.join(b);
            // Only our own links go, never a binary someone placed there.
            if std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
                if let Err(e) = std::fs::remove_file(&link) {
                    warnings.push(format!("{} was not removed: {e}", link.display()));
                }
            }
        }
        warnings
    }

    fn on_path(dir: &Path) -> bool {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d == dir))
            .unwrap_or(false)
    }
}

#[cfg(not(windows))]
mod shortcuts {
    use super::{nix, Options, ICON_PNG};
    use std::path::Path;
    use std::process::Command;

    pub fn create(home: &Path, opts: &Options) -> Vec<String> {
        let mut warnings = Vec::new();
        if !opts.menu_shortcut && !opts.desktop_shortcut {
            return warnings;
        }
        let d = nix::dirs();
        let entry = nix::desktop_entry(home);
        let icon = || -> std::io::Result<()> {
            std::fs::create_dir_all(&d.icons)?;
            std::fs::write(d.icons.join(nix::ICON_FILE), ICON_PNG)
        };
        if let Err(e) = icon() {
            warnings.push(format!("the icon was not installed: {e}"));
        }
        if opts.menu_shortcut {
            let menu = || -> std::io::Result<()> {
                std::fs::create_dir_all(&d.applications)?;
                std::fs::write(d.applications.join(nix::DESKTOP_FILE), &entry)
            };
            match menu() {
                Ok(()) => {
                    let _ = Command::new("update-desktop-database")
                        .arg(&d.applications)
                        .output();
                }
                Err(e) => warnings.push(format!("the menu entry was not created: {e}")),
            }
        }
        if opts.desktop_shortcut {
            let dir = nix::desktop_dir();
            let path = dir.join(nix::DESKTOP_FILE);
            let desktop = || -> std::io::Result<()> {
                use std::os::unix::fs::PermissionsExt;
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&path, &entry)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            };
            match desktop() {
                Ok(()) => {
                    // GNOME also wants an explicit vote of trust before it
                    // launches a desktop file; harmless elsewhere.
                    let _ = Command::new("gio")
                        .args(["set", &path.to_string_lossy(), "metadata::trusted", "true"])
                        .output();
                }
                Err(e) => warnings.push(format!("the desktop shortcut was not created: {e}")),
            }
        }
        warnings
    }

    pub fn remove() -> Vec<String> {
        let d = nix::dirs();
        let mut warnings = Vec::new();
        for path in [
            d.applications.join(nix::DESKTOP_FILE),
            d.icons.join(nix::ICON_FILE),
            nix::desktop_dir().join(nix::DESKTOP_FILE),
        ] {
            if path.exists() {
                if let Err(e) = std::fs::remove_file(&path) {
                    warnings.push(format!("{} was not removed: {e}", path.display()));
                }
            }
        }
        let _ = Command::new("update-desktop-database")
            .arg(&d.applications)
            .output();
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_install_{n}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_home_is_an_absolute_path_named_after_the_program() {
        let home = home_dir();
        assert!(home.is_absolute(), "{}", home.display());
        assert!(home
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("backuputility"));
    }

    #[test]
    fn parses_the_engine_version_line() {
        let out = "bkengine 0.1.0 (windows-x86_64)\n  executable  C:\\x\\bkengine.exe\n";
        assert_eq!(
            parse_version_line(out),
            Some(Version {
                major: 0,
                minor: 1,
                patch: 0
            })
        );
        assert_eq!(parse_version_line(""), None);
        assert_eq!(parse_version_line("error: no such option"), None);
    }

    #[test]
    fn residence_is_decided_by_identity_and_the_marker() {
        let t = Tmp::new("residence");
        let home = t.0.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let exe = home.join(local_name("bkgui"));
        std::fs::write(&exe, b"gui").unwrap();
        assert_eq!(residence_of(&exe, &home), Residence::Installed);

        let elsewhere = t.0.join("downloads");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let other = elsewhere.join(local_name("bkgui"));
        std::fs::write(&other, b"gui").unwrap();
        assert_eq!(residence_of(&other, &home), Residence::Elsewhere);

        // Another spelling of the same file still counts as home.
        #[cfg(windows)]
        {
            let shouted = PathBuf::from(exe.to_string_lossy().to_uppercase());
            assert_eq!(residence_of(&shouted, &home), Residence::Installed);
        }

        std::fs::write(elsewhere.join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(residence_of(&other, &home), Residence::Portable);
        // Even at home, the marker wins: nothing is ever moved.
        std::fs::write(home.join(PORTABLE_MARKER), b"").unwrap();
        assert_eq!(residence_of(&exe, &home), Residence::Portable);
    }

    #[test]
    fn copying_refuses_a_lone_binary_and_installs_the_pair() {
        let t = Tmp::new("copy");
        let src = t.0.join("src");
        let home = t.0.join("home");
        std::fs::create_dir_all(&src).unwrap();
        let gui = local_name("bkgui");
        let engine = local_name("bkengine");
        std::fs::write(src.join(&gui), b"gui").unwrap();

        // Explorer's zip preview hands us exactly this: the GUI alone.
        let err = copy_binaries(&src, &home).unwrap_err().to_string();
        assert!(err.contains(&engine), "{err}");
        assert!(!home.exists(), "nothing is created before the check passes");

        std::fs::write(src.join(&engine), b"engine").unwrap();
        std::fs::write(src.join(LICENSE_FILE), b"gpl").unwrap();
        copy_binaries(&src, &home).unwrap();
        assert_eq!(std::fs::read(home.join(&gui)).unwrap(), b"gui");
        assert_eq!(std::fs::read(home.join(&engine)).unwrap(), b"engine");
        assert_eq!(std::fs::read(home.join(LICENSE_FILE)).unwrap(), b"gpl");
        assert!(
            !home.join(STAGE_DIR).exists(),
            "the scratch directory is removed"
        );

        // Installing over an existing copy parks the old binaries the way an
        // update does, so a running engine keeps its file.
        std::fs::write(src.join(&gui), b"gui-2").unwrap();
        copy_binaries(&src, &home).unwrap();
        assert_eq!(std::fs::read(home.join(&gui)).unwrap(), b"gui-2");
        assert_eq!(
            std::fs::read(home.join(format!("{gui}.old"))).unwrap(),
            b"gui"
        );
        assert_eq!(update::cleanup_old_binaries(&home), 2);
    }

    #[test]
    fn the_default_options_add_a_menu_entry_but_no_desktop_shortcut() {
        let o = Options::default();
        assert!(o.menu_shortcut);
        assert!(!o.desktop_shortcut);
    }

    #[cfg(not(windows))]
    #[test]
    fn the_desktop_entry_launches_the_installed_gui() {
        let entry = nix::desktop_entry(Path::new("/opt/backuputility"));
        assert!(entry.starts_with("[Desktop Entry]\n"));
        assert!(entry.contains("Exec=/opt/backuputility/bkgui\n"), "{entry}");
        assert!(entry.contains("Icon=backuputility\n"));
    }

    #[cfg(windows)]
    #[test]
    fn registry_strings_are_utf16_with_a_terminator() {
        assert_eq!(win::utf16z_bytes("A"), vec![0x41, 0, 0, 0]);
        assert_eq!(win::utf16z_bytes(""), vec![0, 0]);
    }
}
