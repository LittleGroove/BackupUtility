//! Embeds a Windows application manifest into `bkgui.exe`.
//!
//! Four settings, each for a concrete reason:
//!
//! * **`requireAdministrator`** — one UAC prompt at launch, after which the
//!   process and any `bkengine` it spawns run elevated. VSS, raw-disk reads,
//!   `FSCTL_MOVE_FILE`, registering scheduled tasks and `SeBackupPrivilege`
//!   all need it, and prompting once beats failing halfway through a job.
//! * **`longPathAware`** — lets paths past 260 characters work even where a
//!   call slips through without our own `\\?\` prefix. Windows 10 1607+.
//! * **`supportedOS` through Windows 10** — this element also covers Windows
//!   11 and Server 2016/2019/2022. Without it Windows applies compatibility
//!   shims and reports an old version number.
//! * **per-monitor v2 DPI awareness** — an egui window that is not DPI aware
//!   is bitmap-scaled by Windows and looks blurry on any modern display.

fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::manifest::{DpiAwareness, ExecutionLevel, Setting, SupportedOS};
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(
            new_manifest("LittleGroove.BackupUtility")
                .requested_execution_level(ExecutionLevel::RequireAdministrator)
                .long_path_aware(Setting::Enabled)
                .supported_os(SupportedOS::Windows7..=SupportedOS::Windows10)
                .dpi_awareness(DpiAwareness::PerMonitorV2),
        )
        .expect("failed to embed the application manifest");
        println!("cargo:rerun-if-changed=build.rs");
    }
}
