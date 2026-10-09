//! Where the tool keeps its own files, per platform.
//!
//! | | Windows | Linux as root | Linux as a user |
//! |---|---|---|---|
//! | configuration: jobs, settings | `%PROGRAMDATA%\BackupUtility` | `/etc/bku` | `~/.config/bku` |
//! | run state and logs | `%PROGRAMDATA%\BackupUtility\state` | `/var/lib/bku/state` | `~/.local/share/bku/state` |
//! | the binaries | wherever they were unpacked; the updater swaps them in place |
//!
//! On Windows everything is per-machine because the scheduled task runs as
//! SYSTEM and must find the same job files the GUI wrote. On Linux a user
//! install keeps everything under the home directory and schedules through a
//! user-level systemd timer. That is the normal case on SteamOS, where the
//! root filesystem is read-only and `sudo` has no password until the user sets
//! one.

use std::path::PathBuf;

/// Per-machine (or per-user on Linux) root for configuration.
pub fn config_root() -> PathBuf {
    #[cfg(windows)]
    {
        program_data().join("BackupUtility")
    }
    #[cfg(not(windows))]
    {
        if crate::file::sys::is_elevated() {
            PathBuf::from("/etc/bku")
        } else {
            xdg("XDG_CONFIG_HOME", ".config").join("bku")
        }
    }
}

/// Where run state and logs live. One definition, in the state module.
pub fn state_root() -> PathBuf {
    crate::file::state::default_state_root()
}

/// The directory the running executable lives in — the install directory.
pub fn install_dir() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    exe.parent().map(|p| p.to_path_buf()).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the executable has no parent directory",
        )
    })
}

/// Restrict `path` (a file or a directory, recursively) to Administrators and
/// SYSTEM, with read access for everyone else.
///
/// The scheduled task runs the job files as SYSTEM, and `%PROGRAMDATA%`'s
/// default permissions let any local user create and modify files under it.
/// Left alone, a standard user could edit a job to point a SYSTEM-run mirror
/// somewhere else. So the jobs directory and the settings file are tightened
/// the first time an elevated process touches them.
///
/// Best effort, and a no-op unless elevated (an unelevated process could not
/// change the ACL anyway) or off Windows. Each path is done once per process.
pub fn restrict_to_admins(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use std::collections::HashSet;
        use std::sync::{Mutex, OnceLock};
        static DONE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
        if !crate::file::sys::is_elevated() || !path.exists() {
            return;
        }
        let mut done = DONE
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap();
        if !done.insert(path.to_path_buf()) {
            return;
        }
        drop(done);
        // Well-known SIDs rather than names, because names are localized:
        // S-1-5-32-544 Administrators, S-1-5-18 SYSTEM, S-1-5-32-545 Users.
        let (admins, system, users) = if path.is_dir() {
            (
                "*S-1-5-32-544:(OI)(CI)F",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-545:(OI)(CI)RX",
            )
        } else {
            ("*S-1-5-32-544:F", "*S-1-5-18:F", "*S-1-5-32-545:R")
        };
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                admins,
                "/grant:r",
                system,
                "/grant:r",
                users,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

#[cfg(windows)]
pub fn program_data() -> PathBuf {
    std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

/// An XDG base directory: the variable if set and non-empty, else the
/// conventional folder under `$HOME`.
#[cfg(not(windows))]
pub fn xdg(var: &str, fallback_under_home: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(var) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    home().join(fallback_under_home)
}

#[cfg(not(windows))]
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_root_is_an_absolute_path() {
        let root = config_root();
        assert!(root.is_absolute(), "{}", root.display());
        assert!(root.to_string_lossy().to_ascii_lowercase().contains("b"));
    }

    #[test]
    fn the_install_dir_is_where_this_test_binary_lives() {
        let dir = install_dir().unwrap();
        assert!(dir.is_dir());
        assert!(std::env::current_exe().unwrap().starts_with(&dir));
    }
}
