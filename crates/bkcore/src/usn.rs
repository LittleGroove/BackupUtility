//! NTFS USN Change Journal reader — our v2 "free CBT".
//!
//! NTFS keeps a running journal of every change to files and directories on a
//! volume. We read it entirely from user mode (no driver, no signing, no
//! Secure Boot changes) to answer: "which files changed since the last
//! backup?" Then we re-hash only those files' blocks instead of the whole
//! volume — most of Macrium's incremental speed for none of the kernel risk.
//!
//! ## Real implementation notes
//!   - Open the volume handle: `CreateFileW("\\.\C:", ..)`.
//!   - `FSCTL_QUERY_USN_JOURNAL` -> `USN_JOURNAL_DATA` gives the journal id and
//!     current high-water USN. We store that USN in each manifest
//!     (`usn_high_water`).
//!   - Next run: `FSCTL_READ_USN_JOURNAL` starting at the stored USN, walking
//!     `USN_RECORD_V2/V3` entries, collecting `FileReferenceNumber`s and
//!     resolving them to paths.
//!   - Rollover / journal-id mismatch (journal was deleted/recreated, or our
//!     stored USN aged out) -> we can't trust the delta, so fall back to a full
//!     read + hash diff. This fallback is mandatory for correctness.

use crate::Result;

/// A position in a volume's change journal. Stored in the manifest so the next
/// backup knows where to resume reading.
#[derive(Debug, Clone, Copy)]
pub struct UsnCursor {
    pub journal_id: u64,
    pub next_usn: u64,
}

/// Is change-journal tracking available?
///
/// False until the two functions below are implemented. Callers branch on this
/// rather than calling and catching, so a job never advertises a fast
/// incremental it cannot deliver.
pub const AVAILABLE: bool = false;

/// Query the current journal cursor for a volume (call at snapshot time and
/// record it in the manifest).
///
/// Errors rather than panicking — see [`crate::vss::Snapshot::create`] for why.
pub fn query_cursor(volume: &str) -> Result<UsnCursor> {
    Err(crate::BkError::Other(anyhow::anyhow!(
        "reading the USN journal on {volume} is not implemented yet          (FSCTL_QUERY_USN_JOURNAL); a full scan is used instead"
    )))
}

/// List paths changed on `volume` since `since`. `Ok(None)` signals the journal
/// can't be trusted (rollover / id mismatch) and the caller must do a full
/// re-scan instead.
pub fn changed_since(
    _volume: &str,
    _since: UsnCursor,
) -> Result<Option<Vec<std::path::PathBuf>>> {
    // `Ok(None)` is the documented "cannot trust the journal, do a full scan"
    // answer, which is exactly the right response while this is unimplemented:
    // callers already have to handle it, and the fallback is correct.
    Ok(None)
}
