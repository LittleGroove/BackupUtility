//! What kind of drive is this, really?
//!
//! This is the gate on every other operation in [`crate::opt`], because the
//! right thing to do depends entirely on the answer:
//!
//! * **Spinning disk** — consolidating a file's clusters into one run is a real
//!   win, because the head stops seeking. Defragmenting is worth doing.
//! * **Solid state** — there is no seek penalty, so defragmenting buys nothing
//!   and costs write endurance. The useful operations are **retrim** (telling
//!   the drive which blocks are free so its garbage collector can work) and, on
//!   a badly fragmented volume, reducing *metadata* fragmentation so NTFS does
//!   not hit its extent limit.
//!
//! Getting this backwards is not a small mistake: running a full defrag pass
//! over an SSD rewrites tens of gigabytes for no benefit at all. So the
//! detection is conservative — anything we cannot positively identify as a
//! spinning disk is treated as if it might be solid state.

use crate::{BkError, Result};
use serde::{Deserialize, Serialize};

/// What the storage underneath a volume is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Rotational: the device reports a seek penalty.
    Hdd,
    /// Solid state: no seek penalty.
    Ssd,
    /// The device would not say. Treated as SSD for safety.
    Unknown,
}

impl MediaKind {
    /// Is a traditional defragmentation pass appropriate?
    ///
    /// Only for a confirmed spinning disk. `Unknown` deliberately answers
    /// "no" — a needless defrag on an HDD wastes an hour, but a needless
    /// defrag on an SSD wastes write cycles that do not come back.
    pub fn benefits_from_defrag(self) -> bool {
        self == MediaKind::Hdd
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MediaKind::Hdd => "HDD",
            MediaKind::Ssd => "SSD",
            MediaKind::Unknown => "unknown",
        })
    }
}

/// What we learned about one volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// As the user named it, e.g. `"D:"`.
    pub volume: String,
    pub kind: MediaKind,
    /// The device accepts TRIM/UNMAP, so a retrim is meaningful.
    pub trim: bool,
    /// `"NTFS"`, `"ReFS"`, `"exFAT"`, ...
    pub filesystem: Option<String>,
    /// Allocation unit size. `None` only if the volume would not report it.
    pub bytes_per_cluster: Option<u32>,
    pub total_clusters: Option<u64>,
    pub free_clusters: Option<u64>,
}

impl VolumeInfo {
    pub fn total_bytes(&self) -> Option<u64> {
        Some(self.total_clusters? * self.bytes_per_cluster? as u64)
    }

    pub fn free_bytes(&self) -> Option<u64> {
        Some(self.free_clusters? * self.bytes_per_cluster? as u64)
    }

    /// Is this a filesystem whose layout we can read and rearrange? Only NTFS
    /// exposes the retrieval-pointer and move-file interfaces we need.
    pub fn supports_layout_control(&self) -> bool {
        matches!(self.filesystem.as_deref(), Some("NTFS"))
    }

    pub fn describe(&self) -> String {
        let mut s = format!("{}  {}", self.volume, self.kind);
        if let Some(fs) = &self.filesystem {
            s.push_str(&format!("  {fs}"));
        }
        if self.trim {
            s.push_str("  TRIM");
        }
        if let (Some(t), Some(f)) = (self.total_bytes(), self.free_bytes()) {
            s.push_str(&format!(
                "  {} free of {}",
                crate::file::plan::human_bytes(f),
                crate::file::plan::human_bytes(t)
            ));
        }
        s
    }
}

/// The operation that actually suits a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeOp {
    /// Consolidate fragmented files. Spinning disks only.
    Defrag,
    /// Re-issue TRIM for free space. Solid state only.
    Retrim,
    /// Let Windows decide (`defrag /O`). For media we could not identify —
    /// Windows makes the same distinction we do and errs the same way.
    Auto,
    /// Report only; do not touch the drive.
    AnalyzeOnly,
}

impl OptimizeOp {
    pub fn modifies_the_drive(self) -> bool {
        !matches!(self, OptimizeOp::AnalyzeOnly)
    }
}

impl std::fmt::Display for OptimizeOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OptimizeOp::Defrag => "defragment",
            OptimizeOp::Retrim => "retrim",
            OptimizeOp::Auto => "optimize (Windows' choice)",
            OptimizeOp::AnalyzeOnly => "analyze only",
        })
    }
}

/// Pick the operation that suits this volume.
pub fn recommended_op(info: &VolumeInfo) -> OptimizeOp {
    match info.kind {
        MediaKind::Hdd => OptimizeOp::Defrag,
        // Retrim is the SSD-appropriate maintenance. Without TRIM support
        // there is nothing useful and non-destructive to do.
        MediaKind::Ssd if info.trim => OptimizeOp::Retrim,
        MediaKind::Ssd => OptimizeOp::AnalyzeOnly,
        MediaKind::Unknown => OptimizeOp::Auto,
    }
}

/// Normalize a user-supplied volume into the `\\.\X:` device form that the
/// storage IOCTLs expect. Accepts `"D"`, `"D:"`, `"D:\"`, or a path on the
/// volume.
pub fn device_path(volume: &str) -> String {
    let v = volume.trim();
    if v.starts_with(r"\\.\") || v.starts_with(r"\\?\") {
        return v.trim_end_matches('\\').to_string();
    }
    let letter = v.chars().next().unwrap_or('C').to_ascii_uppercase();
    format!(r"\\.\{letter}:")
}

/// The `X:\` root of a volume, for the APIs that want a path rather than a
/// device.
pub fn volume_root(volume: &str) -> String {
    let v = volume.trim();
    let letter = v
        .trim_start_matches(r"\\.\")
        .trim_start_matches(r"\\?\")
        .chars()
        .next()
        .unwrap_or('C')
        .to_ascii_uppercase();
    format!(r"{letter}:\")
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
pub fn probe(volume: &str) -> Result<VolumeInfo> {
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, StorageDeviceTrimProperty,
        DEVICE_SEEK_PENALTY_DESCRIPTOR, DEVICE_TRIM_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_PROPERTY_QUERY,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    let dev = device_path(volume);
    let wide: Vec<u16> = dev.encode_utf16().chain(std::iter::once(0)).collect();

    // Zero desired access is enough for the property queries and avoids
    // needing Administrator just to *look* at a drive. The NTFS volume-data
    // query below does need read access, so it is allowed to fail.
    // SAFETY: `wide` is a valid null-terminated UTF-16 device path.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            HANDLE::default(),
        )
    }
    .map_err(|e| BkError::Other(anyhow::anyhow!("cannot open {dev}: {e}")))?;
    let handle = crate::file::sys::OwnedHandle(handle);

    /// Run one `IOCTL_STORAGE_QUERY_PROPERTY` and return the descriptor.
    fn query<T: Default>(
        h: windows::Win32::Foundation::HANDLE,
        property: windows::Win32::System::Ioctl::STORAGE_PROPERTY_ID,
    ) -> Option<T> {
        let q = STORAGE_PROPERTY_QUERY {
            PropertyId: property,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut out = T::default();
        let mut returned = 0u32;
        // SAFETY: input and output buffers are valid and sized to match what
        // this IOCTL documents for the requested property.
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                Some(&q as *const _ as *const c_void),
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                Some(&mut out as *mut _ as *mut c_void),
                std::mem::size_of::<T>() as u32,
                Some(&mut returned),
                None,
            )
        };
        if ok.is_ok() && returned as usize >= std::mem::size_of::<T>() {
            Some(out)
        } else {
            None
        }
    }

    // The seek-penalty descriptor is the canonical HDD/SSD test — it is what
    // Windows' own Optimize Drives uses.
    let kind = match query::<DEVICE_SEEK_PENALTY_DESCRIPTOR>(
        handle.0,
        StorageDeviceSeekPenaltyProperty,
    ) {
        Some(d) if d.IncursSeekPenalty.as_bool() => MediaKind::Hdd,
        Some(_) => MediaKind::Ssd,
        // USB enclosures and some RAID controllers do not pass the query
        // through. We must not guess "HDD" here; see the module docs.
        None => MediaKind::Unknown,
    };

    let trim = query::<DEVICE_TRIM_DESCRIPTOR>(handle.0, StorageDeviceTrimProperty)
        .map(|d| d.TrimEnabled.as_bool())
        .unwrap_or(false);

    let root = volume_root(volume);
    let filesystem = crate::file::sys::filesystem_name(std::path::Path::new(&root));

    // Cluster geometry, from the plain filesystem APIs rather than a raw
    // volume handle.
    //
    // `FSCTL_GET_NTFS_VOLUME_DATA` would also answer this, but it requires the
    // *handle* to have been opened with read access — and the handle above
    // deliberately asks for none, so that merely listing drives does not need
    // Administrator. Windows checks the access granted to the handle, not the
    // privileges of the process, so running elevated does not rescue a
    // zero-access handle: the query fails either way, and the volume's size
    // comes back unknown even to an Administrator. That is what left the
    // layout pass's evacuation allocator believing the disk had no space.
    //
    // These two need no special access at all, so capacity is now reported for
    // every user, and a layout dry run gets real numbers without elevation.
    //
    // Sizes come from the Ex variant because its counts are `u64`. The cluster
    // counts from `GetDiskFreeSpaceW` are `u32` and saturate past 2^32
    // clusters, which a large array would reach.
    let root = volume_root(volume);
    let root_w: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();

    let (mut sectors_per_cluster, mut bytes_per_sector) = (0u32, 0u32);
    // SAFETY: `root_w` is a valid null-terminated path; both out-params live
    // for the call.
    let have_geometry = unsafe {
        GetDiskFreeSpaceW(
            PCWSTR(root_w.as_ptr()),
            Some(&mut sectors_per_cluster),
            Some(&mut bytes_per_sector),
            None,
            None,
        )
    }
    .is_ok();
    let bytes_per_cluster = have_geometry
        .then(|| sectors_per_cluster.saturating_mul(bytes_per_sector))
        .filter(|c| *c > 0);

    let (mut total_bytes, mut free_bytes) = (0u64, 0u64);
    // SAFETY: as above.
    let have_sizes = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(root_w.as_ptr()),
            None,
            Some(&mut total_bytes),
            Some(&mut free_bytes),
        )
    }
    .is_ok();

    let (total_clusters, free_clusters) = match (bytes_per_cluster, have_sizes) {
        (Some(c), true) => (
            Some(total_bytes / c as u64),
            Some(free_bytes / c as u64),
        ),
        _ => (None, None),
    };

    Ok(VolumeInfo {
        volume: volume.trim().to_string(),
        kind,
        trim,
        filesystem,
        bytes_per_cluster,
        total_clusters,
        free_clusters,
    })
}

// ===========================================================================
// Linux
// ===========================================================================
/// Read the rotational flag Linux already exposes.
///
/// `/sys/block/<dev>/queue/rotational` is `1` for spinning media and `0` for
/// solid state — the same distinction the Windows seek-penalty query makes,
/// and considerably easier to obtain.
#[cfg(not(windows))]
pub fn probe(volume: &str) -> Result<VolumeInfo> {
    let dev = volume
        .trim()
        .trim_start_matches("/dev/")
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .to_string();

    let rotational = std::fs::read_to_string(format!("/sys/block/{dev}/queue/rotational"))
        .ok()
        .map(|s| s.trim() == "1");
    let kind = match rotational {
        Some(true) => MediaKind::Hdd,
        Some(false) => MediaKind::Ssd,
        None => MediaKind::Unknown,
    };
    // A non-zero discard granularity means the device accepts TRIM.
    let trim = std::fs::read_to_string(format!("/sys/block/{dev}/queue/discard_granularity"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|g| g > 0)
        .unwrap_or(false);

    Ok(VolumeInfo {
        volume: volume.trim().to_string(),
        kind,
        trim,
        filesystem: None,
        bytes_per_cluster: None,
        total_clusters: None,
        free_clusters: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: MediaKind, trim: bool) -> VolumeInfo {
        VolumeInfo {
            volume: "D:".into(),
            kind,
            trim,
            filesystem: Some("NTFS".into()),
            bytes_per_cluster: Some(4096),
            total_clusters: Some(1_000_000),
            free_clusters: Some(250_000),
        }
    }

    #[test]
    fn an_ssd_is_never_defragmented() {
        // The mistake this exists to prevent: a full defrag pass on solid
        // state rewrites the drive for no benefit and costs write endurance.
        assert_eq!(recommended_op(&info(MediaKind::Ssd, true)), OptimizeOp::Retrim);
        assert_eq!(
            recommended_op(&info(MediaKind::Ssd, false)),
            OptimizeOp::AnalyzeOnly
        );
        assert!(!MediaKind::Ssd.benefits_from_defrag());
    }

    #[test]
    fn a_spinning_disk_is_defragmented() {
        assert_eq!(recommended_op(&info(MediaKind::Hdd, false)), OptimizeOp::Defrag);
        assert!(MediaKind::Hdd.benefits_from_defrag());
    }

    #[test]
    fn unidentified_media_defers_rather_than_guessing_hdd() {
        // A USB enclosure often will not answer the seek-penalty query.
        // Assuming "spinning" there would defrag an SSD in a caddy.
        assert_eq!(recommended_op(&info(MediaKind::Unknown, true)), OptimizeOp::Auto);
        assert!(!MediaKind::Unknown.benefits_from_defrag());
    }

    #[test]
    fn device_and_root_paths_normalize_every_spelling() {
        for input in ["D", "D:", r"D:\", r"d:\some\path", r"\\.\D:"] {
            assert_eq!(device_path(input), r"\\.\D:", "input was {input}");
            assert_eq!(volume_root(input), r"D:\", "input was {input}");
        }
    }

    #[test]
    fn capacity_is_derived_from_cluster_geometry() {
        let i = info(MediaKind::Hdd, false);
        assert_eq!(i.total_bytes(), Some(4_096_000_000));
        assert_eq!(i.free_bytes(), Some(1_024_000_000));

        // Without elevation there is no geometry, and we say so rather than
        // reporting a made-up zero.
        let mut partial = i.clone();
        partial.bytes_per_cluster = None;
        assert_eq!(partial.total_bytes(), None);
    }

    #[test]
    fn layout_control_needs_ntfs() {
        let mut i = info(MediaKind::Hdd, false);
        assert!(i.supports_layout_control());
        i.filesystem = Some("exFAT".into());
        assert!(!i.supports_layout_control());
        i.filesystem = None;
        assert!(!i.supports_layout_control());
    }

    #[cfg(windows)]
    #[test]
    fn probing_a_real_volume_returns_something_sane() {
        let root = std::env::temp_dir();
        let letter = root.to_string_lossy().chars().next().unwrap().to_string();
        let info = probe(&letter).expect("should be able to probe the temp volume");
        // We may not be elevated, so geometry is optional — but the identity
        // and filesystem should always come back.
        assert!(!info.volume.is_empty());
        assert!(info.filesystem.is_some(), "expected a filesystem name");
        // And whatever it decided, the recommendation must be self-consistent.
        let op = recommended_op(&info);
        if op == OptimizeOp::Defrag {
            assert_eq!(info.kind, MediaKind::Hdd);
        }
        println!("probed: {}", info.describe());
    }
}
