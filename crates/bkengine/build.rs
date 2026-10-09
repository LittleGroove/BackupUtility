//! Windows-only build steps for `bkengine.exe`: an application manifest and
//! the icon/version resources.
//!
//! The manifest asks for `asInvoker`, not `requireAdministrator`: this is the
//! binary a scheduled task runs as SYSTEM, and the one an operator calls from
//! a prompt that is already elevated when it needs to be. Forcing a UAC prompt
//! on every `bkengine jobs list` would be wrong. `longPathAware` and the
//! `supportedOS` list are the same as the GUI's, for the same reasons.

fn main() {
    #[cfg(windows)]
    {
        use embed_manifest::manifest::{ExecutionLevel, Setting, SupportedOS};
        use embed_manifest::{embed_manifest, new_manifest};

        embed_manifest(
            new_manifest("LittleGroove.BackupUtility.Engine")
                .requested_execution_level(ExecutionLevel::AsInvoker)
                .long_path_aware(Setting::Enabled)
                .supported_os(SupportedOS::Windows7..=SupportedOS::Windows10),
        )
        .expect("failed to embed the application manifest");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon.ico");
        res.set("ProductName", "BackupUtility");
        res.set(
            "FileDescription",
            "BackupUtility engine (command line and scheduled runs)",
        );
        res.set("CompanyName", "LittleGroove");
        res.set("LegalCopyright", "Licensed under the GPL-3.0-or-later");
        res.set("OriginalFilename", "bkengine.exe");
        res.compile()
            .expect("failed to compile the Windows resources: is rc.exe (Windows SDK) installed?");

        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=../../assets/icon.ico");
    }
}
