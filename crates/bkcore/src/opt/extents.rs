//! Reading where a file's clusters actually live on disk.
//!
//! NTFS exposes a file's cluster map through `FSCTL_GET_RETRIEVAL_POINTERS`,
//! which answers "virtual cluster N of this file is physical cluster M of this
//! volume". That single fact underpins both features here:
//!
//! * **Fragmentation analysis** — a file in one contiguous run has one extent;
//!   a badly fragmented one has thousands. Computing this ourselves rather than
//!   parsing `defrag /A` output means the numbers are exact and do not change
//!   with the system's display language.
//! * **Layout mirroring** — to place a file on the spare drive at the same
//!   physical position it occupies on the source, we first have to know where
//!   that is.
//!
//! The volume's free-space bitmap comes from `FSCTL_GET_VOLUME_BITMAP`, which
//! tells us whether a destination position is actually available before we try
//! to move anything there.

use crate::{BkError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One contiguous run of a file on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    /// First virtual cluster of the file covered by this run.
    pub vcn: i64,
    /// First physical cluster on the volume. See [`Extent::is_allocated`].
    pub lcn: i64,
    /// How many clusters long the run is.
    pub clusters: u64,
}

impl Extent {
    /// NTFS reports an LCN of -1 for a run that occupies no physical space —
    /// a sparse hole, or the compressed-away tail of a compressed file.
    /// Treating that as a real position would send a move to cluster -1.
    pub fn is_allocated(&self) -> bool {
        self.lcn >= 0
    }

    /// One past the last physical cluster, for allocated runs.
    pub fn end_lcn(&self) -> i64 {
        self.lcn + self.clusters as i64
    }
}

/// Where a whole file lives.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileLayout {
    pub extents: Vec<Extent>,
}

impl FileLayout {
    /// Clusters that occupy real space.
    pub fn allocated_clusters(&self) -> u64 {
        self.extents
            .iter()
            .filter(|e| e.is_allocated())
            .map(|e| e.clusters)
            .sum()
    }

    /// Runs that occupy real space. A value of 0 or 1 means "not fragmented".
    pub fn fragment_count(&self) -> usize {
        self.extents.iter().filter(|e| e.is_allocated()).count()
    }

    /// True if the file is split across more than one physical run.
    ///
    /// Note that adjacent extents in the retrieval-pointer list are already
    /// merged by NTFS, so every boundary here is a genuine discontinuity.
    pub fn is_fragmented(&self) -> bool {
        self.fragment_count() > 1
    }

    /// The physical cluster this file starts at, if it has any real content.
    pub fn start_lcn(&self) -> Option<i64> {
        self.extents.iter().find(|e| e.is_allocated()).map(|e| e.lcn)
    }

    /// The physical cluster holding virtual cluster `vcn`, if allocated.
    pub fn lcn_of(&self, vcn: i64) -> Option<i64> {
        for e in &self.extents {
            if vcn >= e.vcn && vcn < e.vcn + e.clusters as i64 {
                return e.is_allocated().then(|| e.lcn + (vcn - e.vcn));
            }
        }
        None
    }
}

/// Fragmentation across a set of files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragReport {
    pub files_examined: u64,
    pub files_fragmented: u64,
    /// Files small enough to live inside their MFT record, occupying no
    /// clusters at all. They cannot be fragmented and must not drag the
    /// average down — a tree of tiny files would otherwise report well under
    /// one run per file, which reads as nonsense.
    pub files_resident: u64,
    /// Files we could not read a layout for (locked, denied, or not NTFS).
    pub files_unreadable: u64,
    pub total_fragments: u64,
    pub total_clusters: u64,
    /// The single worst file, as `(fragments, path)`.
    pub worst: Option<(usize, String)>,
}

impl FragReport {
    /// Files that actually occupy clusters — the only ones fragmentation
    /// means anything for.
    pub fn files_on_disk(&self) -> u64 {
        self.files_examined.saturating_sub(self.files_resident)
    }

    /// Fraction of on-disk files split across more than one run.
    pub fn fragmented_fraction(&self) -> f64 {
        let n = self.files_on_disk();
        if n == 0 {
            return 0.0;
        }
        self.files_fragmented as f64 / n as f64
    }

    /// Average runs per on-disk file. 1.0 is perfect.
    pub fn average_fragments(&self) -> f64 {
        let n = self.files_on_disk();
        if n == 0 {
            return 0.0;
        }
        self.total_fragments as f64 / n as f64
    }

    pub fn record(&mut self, path: &str, layout: &FileLayout) {
        self.files_examined += 1;
        let n = layout.fragment_count();
        if n == 0 {
            self.files_resident += 1;
            return;
        }
        self.total_fragments += n as u64;
        self.total_clusters += layout.allocated_clusters();
        if n > 1 {
            self.files_fragmented += 1;
            if self.worst.as_ref().is_none_or(|(worst, _)| n > *worst) {
                self.worst = Some((n, path.to_string()));
            }
        }
    }

    pub fn describe(&self) -> String {
        let mut s = format!(
            "  {} files on disk, {:.1}% fragmented, {:.2} runs per file\n",
            self.files_on_disk(),
            self.fragmented_fraction() * 100.0,
            self.average_fragments()
        );
        if self.files_resident > 0 {
            s.push_str(&format!(
                "  {} tiny files stored inside the MFT (never fragmented)\n",
                self.files_resident
            ));
        }
        if let Some((n, p)) = &self.worst {
            s.push_str(&format!("  worst: {n} runs — {p}\n"));
        }
        if self.files_unreadable > 0 {
            s.push_str(&format!("  {} files unreadable\n", self.files_unreadable));
        }
        s
    }
}

/// A volume's free-space map, so we can tell whether a target position is
/// available before trying to move a file there.
#[derive(Debug, Clone)]
pub struct FreeMap {
    start_lcn: i64,
    /// One bit per cluster; set means **in use**.
    bits: Vec<u8>,
    cluster_count: u64,
}

impl FreeMap {
    pub fn cluster_count(&self) -> u64 {
        self.cluster_count
    }

    /// Is this cluster free? Clusters outside the mapped range answer `false`,
    /// so an out-of-range target is never mistaken for available space.
    pub fn is_free(&self, lcn: i64) -> bool {
        if lcn < self.start_lcn {
            return false;
        }
        let idx = (lcn - self.start_lcn) as u64;
        if idx >= self.cluster_count {
            return false;
        }
        let byte = self.bits.get((idx / 8) as usize).copied().unwrap_or(0xFF);
        byte & (1 << (idx % 8)) == 0
    }

    /// Are `count` consecutive clusters starting at `lcn` all free?
    pub fn range_is_free(&self, lcn: i64, count: u64) -> bool {
        (0..count).all(|i| self.is_free(lcn + i as i64))
    }

    pub fn free_clusters(&self) -> u64 {
        (0..self.cluster_count)
            .filter(|i| self.is_free(self.start_lcn + *i as i64))
            .count() as u64
    }

    /// Mark a range as in use, so a caller placing several files in one pass
    /// does not hand out the same clusters twice.
    pub fn claim(&mut self, lcn: i64, count: u64) {
        for i in 0..count {
            let c = lcn + i as i64;
            if c < self.start_lcn {
                continue;
            }
            let idx = (c - self.start_lcn) as u64;
            if idx >= self.cluster_count {
                continue;
            }
            if let Some(b) = self.bits.get_mut((idx / 8) as usize) {
                *b |= 1 << (idx % 8);
            }
        }
    }

    /// Mark a range as free again.
    ///
    /// Used to hand back clusters that were claimed for a move that then did
    /// not happen. Without this, an abandoned plan would permanently poison
    /// those clusters for the rest of the pass and later files would be
    /// needlessly reported as blocked.
    pub fn release(&mut self, lcn: i64, count: u64) {
        for i in 0..count {
            let c = lcn + i as i64;
            if c < self.start_lcn {
                continue;
            }
            let idx = (c - self.start_lcn) as u64;
            if idx >= self.cluster_count {
                continue;
            }
            if let Some(b) = self.bits.get_mut((idx / 8) as usize) {
                *b &= !(1 << (idx % 8));
            }
        }
    }

    /// Build one directly. Exposed for tests and for callers that have a
    /// bitmap from elsewhere.
    pub fn from_bits(start_lcn: i64, bits: Vec<u8>, cluster_count: u64) -> Self {
        FreeMap {
            start_lcn,
            bits,
            cluster_count,
        }
    }
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
mod imp {
    use super::*;
    use crate::file::sys::{wide, OwnedHandle};
    use std::ffi::c_void;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_MORE_DATA, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_RETRIEVAL_POINTERS, FSCTL_GET_VOLUME_BITMAP, RETRIEVAL_POINTERS_BUFFER,
        STARTING_LCN_INPUT_BUFFER, STARTING_VCN_INPUT_BUFFER, VOLUME_BITMAP_BUFFER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    /// Enough for a file's read-attributes/FSCTL access without needing the
    /// data itself, so this works on files we are not allowed to read.
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const GENERIC_READ_U32: u32 = 0x8000_0000;
    const GENERIC_WRITE_U32: u32 = 0x4000_0000;

    /// Open a file just to interrogate or move its clusters.
    pub fn open_file(path: &Path, write: bool) -> Result<OwnedHandle> {
        let w = wide(path);
        let access = if write {
            GENERIC_READ_U32 | GENERIC_WRITE_U32
        } else {
            FILE_READ_ATTRIBUTES
        };
        // SAFETY: `w` is a valid null-terminated UTF-16 path outliving the call.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                HANDLE::default(),
            )
        }
        .map_err(|e| {
            BkError::Other(anyhow::anyhow!(
                "open {} for layout: {e}",
                crate::file::path::display(path)
            ))
        })?;
        Ok(OwnedHandle(h))
    }

    /// Open a volume for the bitmap query, or for `FSCTL_MOVE_FILE`.
    ///
    /// Moving clusters requires read **and** write access to the raw volume,
    /// which requires Administrator. Reading the bitmap needs only read access,
    /// which still requires Administrator in practice.
    pub fn open_volume(volume: &str, write: bool) -> Result<OwnedHandle> {
        let dev = super::super::media::device_path(volume);
        let w: Vec<u16> = dev.encode_utf16().chain(std::iter::once(0)).collect();
        let access = if write {
            GENERIC_READ_U32 | GENERIC_WRITE_U32
        } else {
            GENERIC_READ_U32
        };
        // SAFETY: valid null-terminated device path.
        let h = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                HANDLE::default(),
            )
        }
        .map_err(|e| {
            BkError::Other(anyhow::anyhow!(
                "open volume {dev}: {e} (this needs Administrator)"
            ))
        })?;
        Ok(OwnedHandle(h))
    }

    /// Read a file's complete cluster map.
    pub fn file_layout(path: &Path) -> Result<FileLayout> {
        let h = open_file(path, false)?;
        layout_of_handle(h.0)
    }

    /// The same, from an already-open handle.
    pub fn layout_of_handle(h: HANDLE) -> Result<FileLayout> {
        let mut out = FileLayout::default();
        let mut starting_vcn: i64 = 0;

        // The output is variable-length and the call returns ERROR_MORE_DATA
        // when it did not fit. The data it *did* return is valid, so we keep it
        // and resume from the last NextVcn rather than growing the buffer
        // forever — a heavily fragmented file can have millions of extents.
        const EXTENTS_PER_CALL: usize = 4096;
        let bytes = std::mem::size_of::<RETRIEVAL_POINTERS_BUFFER>()
            + EXTENTS_PER_CALL * std::mem::size_of::<i64>() * 2;
        let mut buf = vec![0u8; bytes];

        loop {
            let input = STARTING_VCN_INPUT_BUFFER {
                StartingVcn: starting_vcn,
            };
            let mut returned = 0u32;
            // SAFETY: `buf` is at least as large as we claim, and the input is
            // exactly the documented type for this control code.
            let rc = unsafe {
                DeviceIoControl(
                    h,
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    Some(&input as *const _ as *const c_void),
                    std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    buf.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };

            let more = match rc {
                Ok(()) => false,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
                // A resident file (small enough to live inside its MFT record)
                // has no clusters at all, and reports EOF. That is not an
                // error — it is a file with an empty layout.
                Err(e) if e.code() == ERROR_HANDLE_EOF.to_hresult() => return Ok(out),
                Err(e) => {
                    return Err(BkError::Other(anyhow::anyhow!(
                        "FSCTL_GET_RETRIEVAL_POINTERS: {e}"
                    )))
                }
            };

            // SAFETY: the driver filled `buf` with a RETRIEVAL_POINTERS_BUFFER
            // header followed by ExtentCount (NextVcn, Lcn) pairs, and
            // `returned` bounds what we read below.
            let header = unsafe { &*(buf.as_ptr() as *const RETRIEVAL_POINTERS_BUFFER) };
            let count = header.ExtentCount as usize;
            let mut vcn = header.StartingVcn;

            // Where the extents array starts. Computing this by hand means
            // getting the padding right (`ExtentCount` is a u32 followed by an
            // 8-aligned i64), so ask the compiler instead.
            let extents_offset = std::mem::offset_of!(RETRIEVAL_POINTERS_BUFFER, Extents);
            let pairs = unsafe {
                std::slice::from_raw_parts(
                    buf.as_ptr().add(extents_offset) as *const [i64; 2],
                    count.min((returned as usize).saturating_sub(extents_offset) / 16),
                )
            };

            for pair in pairs {
                let (next_vcn, lcn) = (pair[0], pair[1]);
                let clusters = (next_vcn - vcn).max(0) as u64;
                if clusters > 0 {
                    out.extents.push(Extent {
                        vcn,
                        lcn,
                        clusters,
                    });
                }
                vcn = next_vcn;
            }

            if !more || pairs.is_empty() {
                break;
            }
            starting_vcn = vcn;
        }
        Ok(out)
    }

    /// Read the volume's cluster allocation bitmap.
    pub fn free_map(volume: &str, total_clusters: u64) -> Result<FreeMap> {
        let h = open_volume(volume, false)?;
        let mut bits: Vec<u8> = Vec::with_capacity((total_clusters / 8) as usize + 8);
        let mut next_lcn: i64 = 0;
        let start_lcn = 0i64;
        let mut mapped: u64 = 0;

        // The bitmap for a large volume is many megabytes, so it comes back in
        // chunks; each reply says where it starts and how many clusters it
        // covers.
        const CHUNK_BYTES: usize = 1 << 20;
        let mut buf = vec![0u8; std::mem::size_of::<VOLUME_BITMAP_BUFFER>() + CHUNK_BYTES];

        loop {
            let input = STARTING_LCN_INPUT_BUFFER {
                StartingLcn: next_lcn,
            };
            let mut returned = 0u32;
            // SAFETY: documented input type; `buf` is sized as claimed.
            let rc = unsafe {
                DeviceIoControl(
                    h.0,
                    FSCTL_GET_VOLUME_BITMAP,
                    Some(&input as *const _ as *const c_void),
                    std::mem::size_of::<STARTING_LCN_INPUT_BUFFER>() as u32,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    buf.len() as u32,
                    Some(&mut returned),
                    None,
                )
            };
            let more = match rc {
                Ok(()) => false,
                Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
                Err(e) => {
                    return Err(BkError::Other(anyhow::anyhow!(
                        "FSCTL_GET_VOLUME_BITMAP: {e}"
                    )))
                }
            };

            // SAFETY: the driver wrote a VOLUME_BITMAP_BUFFER header followed
            // by BitmapSize bits.
            let header = unsafe { &*(buf.as_ptr() as *const VOLUME_BITMAP_BUFFER) };
            let bitmap_clusters = header.BitmapSize.max(0) as u64;
            let header_bytes = std::mem::offset_of!(VOLUME_BITMAP_BUFFER, Buffer);
            let available = (returned as usize).saturating_sub(header_bytes);
            let want = ((bitmap_clusters + 7) / 8) as usize;
            let take = want.min(available);
            bits.extend_from_slice(&buf[header_bytes..header_bytes + take]);
            mapped += bitmap_clusters;

            if !more || bitmap_clusters == 0 {
                break;
            }
            next_lcn = header.StartingLcn + bitmap_clusters as i64;
        }

        Ok(FreeMap {
            start_lcn,
            bits,
            cluster_count: mapped,
        })
    }
}

// ===========================================================================
// Linux
// ===========================================================================
#[cfg(not(windows))]
mod imp {
    use super::*;

    /// The Linux analogue is `FIEMAP` (`ioctl(FS_IOC_FIEMAP)`), which returns
    /// the same physical-extent information for ext4, XFS and Btrfs. Wiring it
    /// up is the remaining step for Linux layout support; until then the
    /// caller sees an honest "unsupported" rather than a fabricated layout.
    pub fn file_layout(_path: &Path) -> Result<FileLayout> {
        Err(BkError::Other(anyhow::anyhow!(
            "reading file extents on this platform needs FS_IOC_FIEMAP, which is not wired up yet"
        )))
    }

    pub fn free_map(_volume: &str, _total_clusters: u64) -> Result<FreeMap> {
        Err(BkError::Other(anyhow::anyhow!(
            "reading the free-space map on this platform is not implemented"
        )))
    }
}

pub use imp::{file_layout, free_map};

#[cfg(windows)]
pub use imp::{layout_of_handle, open_file, open_volume};

/// Measure fragmentation across a list of files.
pub fn analyze<'a>(paths: impl Iterator<Item = &'a Path>) -> FragReport {
    let mut report = FragReport::default();
    for p in paths {
        match file_layout(p) {
            Ok(layout) => report.record(&crate::file::path::display(p), &layout),
            Err(_) => report.files_unreadable += 1,
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(vcn: i64, lcn: i64, clusters: u64) -> Extent {
        Extent { vcn, lcn, clusters }
    }

    #[test]
    fn a_contiguous_file_is_not_fragmented() {
        let l = FileLayout {
            extents: vec![ext(0, 1000, 50)],
        };
        assert!(!l.is_fragmented());
        assert_eq!(l.fragment_count(), 1);
        assert_eq!(l.allocated_clusters(), 50);
        assert_eq!(l.start_lcn(), Some(1000));
    }

    #[test]
    fn a_split_file_is_fragmented() {
        let l = FileLayout {
            extents: vec![ext(0, 1000, 20), ext(20, 5000, 30)],
        };
        assert!(l.is_fragmented());
        assert_eq!(l.fragment_count(), 2);
        assert_eq!(l.allocated_clusters(), 50);
    }

    #[test]
    fn sparse_holes_do_not_count_as_fragments_or_space() {
        // NTFS reports LCN -1 for a run that occupies no physical clusters.
        // Counting those as fragments would make every sparse VM disk look
        // catastrophically fragmented; using -1 as a position would send a
        // move to a nonsense cluster.
        let l = FileLayout {
            extents: vec![ext(0, 1000, 10), ext(10, -1, 1000), ext(1010, 2000, 10)],
        };
        assert_eq!(l.fragment_count(), 2, "the hole is not a fragment");
        assert_eq!(l.allocated_clusters(), 20, "and occupies no space");
        assert!(!l.extents[1].is_allocated());
    }

    #[test]
    fn virtual_clusters_map_to_physical_ones() {
        let l = FileLayout {
            extents: vec![ext(0, 1000, 10), ext(10, 5000, 10)],
        };
        assert_eq!(l.lcn_of(0), Some(1000));
        assert_eq!(l.lcn_of(9), Some(1009));
        assert_eq!(l.lcn_of(10), Some(5000));
        assert_eq!(l.lcn_of(19), Some(5009));
        assert_eq!(l.lcn_of(20), None, "past the end of the file");
    }

    #[test]
    fn a_resident_file_has_an_empty_layout() {
        // Files small enough to live inside their MFT record occupy no
        // clusters at all. That is not an error and not fragmentation.
        let l = FileLayout::default();
        assert!(!l.is_fragmented());
        assert_eq!(l.allocated_clusters(), 0);
        assert_eq!(l.start_lcn(), None);
    }

    #[test]
    fn resident_files_do_not_drag_the_average_below_one() {
        // A tree of tiny files lives inside the MFT and occupies no clusters.
        // Counting them would report "0.33 runs per file", which is nonsense.
        let mut r = FragReport::default();
        r.record("tiny1.txt", &FileLayout::default());
        r.record("tiny2.txt", &FileLayout::default());
        r.record("real.bin", &FileLayout { extents: vec![ext(0, 10, 100)] });

        assert_eq!(r.files_examined, 3);
        assert_eq!(r.files_resident, 2);
        assert_eq!(r.files_on_disk(), 1);
        assert_eq!(r.average_fragments(), 1.0, "one run for the one real file");
        assert_eq!(r.fragmented_fraction(), 0.0);
        assert!(r.describe().contains("inside the MFT"), "{}", r.describe());
    }

    #[test]
    fn a_tree_of_only_resident_files_does_not_divide_by_zero() {
        let mut r = FragReport::default();
        r.record("a.txt", &FileLayout::default());
        assert_eq!(r.files_on_disk(), 0);
        assert_eq!(r.average_fragments(), 0.0);
        assert_eq!(r.fragmented_fraction(), 0.0);
    }

    #[test]
    fn fragmentation_is_aggregated_across_files() {
        let mut r = FragReport::default();
        r.record("clean.txt", &FileLayout { extents: vec![ext(0, 10, 5)] });
        r.record(
            "messy.bin",
            &FileLayout {
                extents: vec![ext(0, 10, 1), ext(1, 90, 1), ext(2, 500, 1)],
            },
        );
        assert_eq!(r.files_examined, 2);
        assert_eq!(r.files_fragmented, 1);
        assert_eq!(r.fragmented_fraction(), 0.5);
        assert_eq!(r.total_fragments, 4);
        assert_eq!(r.average_fragments(), 2.0);
        assert_eq!(r.worst, Some((3, "messy.bin".into())));
    }

    #[test]
    fn an_empty_report_does_not_divide_by_zero() {
        let r = FragReport::default();
        assert_eq!(r.fragmented_fraction(), 0.0);
        assert_eq!(r.average_fragments(), 0.0);
    }

    #[test]
    fn the_free_map_reads_bits_least_significant_first() {
        // Bit 0 of byte 0 is cluster 0. Value 0b0000_0101 means clusters 0
        // and 2 are in use.
        let m = FreeMap::from_bits(0, vec![0b0000_0101], 8);
        assert!(!m.is_free(0));
        assert!(m.is_free(1));
        assert!(!m.is_free(2));
        assert!(m.is_free(3));
        assert_eq!(m.free_clusters(), 6);
    }

    #[test]
    fn out_of_range_clusters_are_never_reported_free() {
        // The dangerous failure would be answering "free" for a cluster we
        // have no information about, and then moving a file onto it.
        let m = FreeMap::from_bits(0, vec![0x00], 8);
        assert!(m.is_free(7));
        assert!(!m.is_free(8), "past the mapped range");
        assert!(!m.is_free(-1), "before the mapped range");
        assert!(!m.range_is_free(6, 4), "a range that runs off the end");
    }

    #[test]
    fn ranges_and_claims_work_together() {
        let mut m = FreeMap::from_bits(0, vec![0x00, 0x00], 16);
        assert!(m.range_is_free(0, 16));
        // Placing one file must stop a later one being sent to the same place.
        m.claim(4, 4);
        assert!(!m.range_is_free(0, 16));
        assert!(m.range_is_free(0, 4));
        assert!(!m.is_free(4));
        assert!(!m.is_free(7));
        assert!(m.is_free(8));
        assert_eq!(m.free_clusters(), 12);
    }

    #[cfg(windows)]
    #[test]
    fn a_real_file_reports_a_real_layout() {
        let dir = std::env::temp_dir().join("bku_extents_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("blob.bin");
        // Big enough that NTFS cannot keep it resident in the MFT record.
        std::fs::write(&p, vec![0x5Au8; 2 * 1024 * 1024]).unwrap();

        match file_layout(&p) {
            Ok(l) => {
                assert!(
                    l.allocated_clusters() > 0,
                    "a 2 MiB file must occupy clusters"
                );
                assert!(l.start_lcn().is_some());
                // Freshly written on a healthy volume it is usually one run,
                // but that is not guaranteed, so only assert the invariants.
                for e in &l.extents {
                    assert!(e.clusters > 0);
                }
            }
            // Not NTFS, or the handle was refused — acceptable, not a failure.
            Err(e) => println!("layout unavailable (fine on non-NTFS): {e}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
