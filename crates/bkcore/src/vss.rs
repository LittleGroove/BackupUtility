//! Volume Shadow Copy Service (VSS) — taking a consistent point-in-time
//! snapshot of a *live* volume so we can back up the OS drive while Windows is
//! running.
//!
//! Without this, reading C: while it's in use gives you a torn image: files
//! change under you mid-read and the result may not even be a mountable
//! filesystem. VSS asks Windows (and VSS-aware apps like SQL Server) to flush
//! and quiesce, then exposes a frozen, read-only shadow device you read from.
//!
//! ## Real implementation notes (for when we wire this up)
//! VSS is a COM API, reached via the `windows` crate:
//!   1. `CoInitializeEx` (COINIT_MULTITHREADED).
//!   2. `CreateVssBackupComponents` -> `IVssBackupComponents`.
//!   3. `InitializeForBackup`, `SetBackupState`, `GatherWriterMetadata`.
//!   4. `StartSnapshotSet` / `AddToSnapshotSet(volume)`.
//!   5. `PrepareForBackup`, then `DoSnapshotSet` (async — wait on the
//!      `IVssAsync`).
//!   6. `GetSnapshotProperties` -> `m_pwszSnapshotDeviceObject`, a
//!      `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN` path you open like
//!      any device.
//!   7. On drop: `BackupComplete`, release the snapshot, `CoUninitialize`.
//!
//! Requires elevation. There's no pure-Rust VSS crate we'd trust; we bind the
//! COM interfaces ourselves through `windows`.

use crate::Result;

/// A live shadow copy. Reading from [`device_path`](Snapshot::device_path)
/// yields the frozen volume. The snapshot is released when this is dropped.
pub struct Snapshot {
    device_path: String,
    // Real impl holds the IVssBackupComponents + snapshot id here so Drop can
    // tear it down.
}

/// Is a consistent snapshot of a live volume available?
///
/// Callers check this before offering to image a mounted volume, so the
/// limitation surfaces as a clear refusal rather than a torn image.
pub const AVAILABLE: bool = false;

impl Snapshot {
    /// Create a shadow copy of `volume` (e.g. "C:").
    ///
    /// Returns an error rather than panicking. A `todo!()` here would take the
    /// whole process down the first time anyone wired this up, and an engine
    /// that aborts mid-backup is worse than one that declines the job.
    pub fn create(volume: &str) -> Result<Self> {
        Err(crate::BkError::Vss(format!(
            "snapshotting {volume} is not implemented yet — the VSS COM              sequence documented in this module has not been written. Back up              a quiesced volume, or use a file-level mirror instead."
        )))
    }

    /// The device path to read the frozen volume from, e.g.
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1`.
    pub fn device_path(&self) -> &str {
        &self.device_path
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // TODO(v1): BackupComplete + release snapshot set + CoUninitialize.
    }
}

/// True if the current process is running elevated (Administrator).
/// Every VSS/raw-disk path asserts this first for a clean error.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard token-query dance; all handles/out-params are valid and
    // the token is closed exactly once before returning.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    true
}
