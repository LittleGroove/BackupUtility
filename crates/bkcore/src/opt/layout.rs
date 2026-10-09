//! Making two drives structurally alike.
//!
//! An ordinary mirror guarantees the *contents* match. This makes the
//! *physical layout* match too: after a pass, a file occupies the same
//! cluster range on the spare drive that it does on the source. NTFS exposes
//! `FSCTL_MOVE_FILE`, the same interface every defragmenter uses, which asks
//! the filesystem to relocate a run of a file's clusters to a chosen physical
//! position — online, transactionally, with the volume mounted and in use.
//!
//! ## Why bother
//! Two drives with matching layouts stay matched. On a spinning disk, the
//! read pattern that streams well on the source streams identically on the
//! spare, so a verification pass or a restore runs at sequential speed instead
//! of degenerating into seeks. And as the drives fill, the free space left
//! behind is in the same shape on both, so the *next* file to arrive lands in
//! the same place on each — the divergence never gets a chance to compound.
//!
//! ## Why it is guarded so heavily
//! Every move is a real write to a real filesystem. So:
//!
//! * NTFS only — nothing else exposes the interface.
//! * Administrator only — the volume handle demands it.
//! * A cluster range is never targeted unless the volume bitmap says it is
//!   free. Overlapping an existing file is not something NTFS would allow, but
//!   checking first turns a failed IOCTL into a clean, counted skip.
//! * The free map is updated as we go, so two files in one pass can never be
//!   sent to the same clusters.
//! * Nothing here ever changes file *contents*. The worst case of a failed
//!   move is that the file stays where it was.

use super::extents::{self, FileLayout};
#[cfg(windows)]
use super::extents::{Extent, FreeMap};
use super::media::VolumeInfo;
use crate::file::path::RelPath;
use crate::file::Cancel;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// How a layout pass should behave.
#[derive(Debug, Clone)]
pub struct LayoutOptions {
    /// Measure and report, but move nothing.
    pub dry_run: bool,
    /// Ignore files smaller than this many clusters. Tiny files dominate a
    /// tree by count and contribute nothing to streaming performance, so
    /// moving them is mostly wasted writes.
    pub min_clusters: u64,
    /// Stop after this many files, so a nightly job can make steady progress
    /// on a huge volume instead of running for a day.
    pub max_files: Option<usize>,
    /// Stop after this many cluster moves.
    pub max_clusters_moved: Option<u64>,
    /// Place the extents that fit even when the whole file cannot be matched.
    /// With this off, a file is left alone unless every run can be placed.
    pub allow_partial: bool,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        LayoutOptions {
            dry_run: false,
            // 16 clusters is 64 KiB at the usual 4 KiB cluster size.
            min_clusters: 16,
            max_files: None,
            max_clusters_moved: None,
            allow_partial: true,
        }
    }
}

/// The outcome of comparing, and possibly aligning, two trees.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayoutReport {
    pub files_compared: u64,
    /// Files already laid out identically on both drives.
    pub files_identical: u64,
    /// Files skipped as too small, or unreadable on one side.
    pub files_skipped: u64,
    pub files_moved: u64,
    pub clusters_moved: u64,
    /// Extents we wanted to place but could not, because the target clusters
    /// were already occupied.
    pub extents_blocked: u64,
    pub move_failures: u64,
    /// Clusters that matched before the pass, and the total considered.
    pub clusters_matching_before: u64,
    pub clusters_matching_after: u64,
    pub clusters_total: u64,
    pub dry_run: bool,
    /// Set when the pass could not run at all.
    pub refused: Option<String>,
}

impl LayoutReport {
    /// Fraction of clusters sitting at the same physical position on both
    /// drives, before the pass.
    pub fn similarity_before(&self) -> f64 {
        ratio(self.clusters_matching_before, self.clusters_total)
    }

    /// The same, after.
    pub fn similarity_after(&self) -> f64 {
        ratio(self.clusters_matching_after, self.clusters_total)
    }

    pub fn describe(&self) -> String {
        if let Some(why) = &self.refused {
            return format!("  layout pass refused: {why}\n");
        }
        let mut s = format!(
            "  {} files compared, {} already aligned, {} skipped\n",
            self.files_compared, self.files_identical, self.files_skipped
        );
        s.push_str(&format!(
            "  layout similarity: {:.1}% -> {:.1}%\n",
            self.similarity_before() * 100.0,
            self.similarity_after() * 100.0
        ));
        if self.dry_run {
            s.push_str("  (dry run — nothing was moved)\n");
        } else {
            s.push_str(&format!(
                "  {} files moved, {} clusters relocated\n",
                self.files_moved, self.clusters_moved
            ));
        }
        if self.extents_blocked > 0 {
            s.push_str(&format!(
                "  {} runs blocked by occupied target clusters\n",
                self.extents_blocked
            ));
        }
        if self.move_failures > 0 {
            s.push_str(&format!("  {} moves failed\n", self.move_failures));
        }
        s
    }
}

fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        // Nothing to compare means nothing is misaligned.
        1.0
    } else {
        part as f64 / whole as f64
    }
}

/// How many clusters of these two layouts sit at the same physical position.
///
/// Two extents that overlap in virtual-cluster space put their overlapping
/// clusters in the same physical place exactly when they share the same
/// `lcn - vcn` offset — so the whole overlap can be settled with one
/// comparison rather than cluster by cluster.
pub fn matching_clusters(a: &FileLayout, b: &FileLayout) -> u64 {
    let (mut i, mut j) = (0usize, 0usize);
    let mut total = 0u64;

    while i < a.extents.len() && j < b.extents.len() {
        let (x, y) = (&a.extents[i], &b.extents[j]);
        let (x_end, y_end) = (x.vcn + x.clusters as i64, y.vcn + y.clusters as i64);

        let start = x.vcn.max(y.vcn);
        let end = x_end.min(y_end);
        if end > start && x.is_allocated() && y.is_allocated() && (x.lcn - x.vcn) == (y.lcn - y.vcn)
        {
            total += (end - start) as u64;
        }

        // Advance whichever range finishes first.
        if x_end <= y_end {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

/// Compare one file's layout on both drives without changing anything.
pub fn compare_file(source: &Path, dest: &Path) -> Result<(FileLayout, FileLayout, u64)> {
    let s = extents::file_layout(source)?;
    let d = extents::file_layout(dest)?;
    let m = matching_clusters(&s, &d);
    Ok((s, d, m))
}

/// Run a layout pass over a list of files that exist in both trees.
///
/// `files` are relative paths; each is resolved under both roots. Files
/// missing from either side are skipped.
pub fn run(
    source_root: &Path,
    dest_root: &Path,
    dest_volume: &VolumeInfo,
    files: &[RelPath],
    opts: &LayoutOptions,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&LayoutReport),
) -> Result<LayoutReport> {
    let mut report = LayoutReport {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    // Refuse, with a reason, rather than half-doing it.
    if !dest_volume.supports_layout_control() {
        report.refused = Some(format!(
            "the destination is {} — only NTFS exposes the cluster-placement \
             interface this needs",
            dest_volume
                .filesystem
                .as_deref()
                .unwrap_or("an unknown filesystem")
        ));
        return Ok(report);
    }
    if !opts.dry_run && !crate::file::sys::is_elevated() {
        report.refused =
            Some("moving clusters requires Administrator (the volume handle demands it)".into());
        return Ok(report);
    }

    imp::run_pass(
        source_root,
        dest_root,
        dest_volume,
        files,
        opts,
        cancel,
        &mut report,
        progress,
    )?;
    Ok(report)
}

// ===========================================================================
// Windows
// ===========================================================================
#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::sync::Arc;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Ioctl::{FSCTL_MOVE_FILE, MOVE_FILE_DATA};
    use windows::Win32::System::IO::DeviceIoControl;

    /// `MOVE_FILE_DATA::ClusterCount` is a `u32`, and issuing enormous single
    /// moves makes cancellation coarse, so long runs are moved in pieces.
    const MAX_CLUSTERS_PER_MOVE: u64 = 64 * 1024;

    /// One run of one file we want placed at a specific physical position.
    struct Placement {
        dest: PathBuf,
        /// `(vcn, target lcn, clusters)`, in file order.
        runs: Vec<(i64, i64, u64)>,
        /// The source layout, kept to re-measure the result afterwards.
        source: FileLayout,
    }

    /// Who currently owns a stretch of the volume, so it can be moved aside.
    #[derive(Clone)]
    struct Occupant {
        end: i64,
        path: Arc<PathBuf>,
        vcn: i64,
    }

    /// Hands out free space from the top of the volume downwards.
    ///
    /// Evacuated data goes high while placements fill in low, so the two never
    /// chase each other and nothing we have already positioned gets displaced
    /// by a later eviction.
    struct HighAllocator {
        cursor: i64,
        floor: i64,
    }

    impl HighAllocator {
        fn new(total_clusters: i64, floor: i64) -> Self {
            HighAllocator {
                cursor: total_clusters.saturating_sub(1),
                floor,
            }
        }

        /// Claim `count` contiguous free clusters above the floor.
        fn alloc(&mut self, free: &mut FreeMap, count: u64) -> Option<i64> {
            if count == 0 {
                return None;
            }
            let n = count as i64;
            while self.cursor - n >= self.floor {
                let start = self.cursor - n + 1;
                if free.range_is_free(start, count) {
                    free.claim(start, count);
                    self.cursor = start - 1;
                    return Some(start);
                }
                self.cursor -= 1;
            }
            None
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_pass(
        source_root: &Path,
        dest_root: &Path,
        dest_volume: &VolumeInfo,
        files: &[RelPath],
        opts: &LayoutOptions,
        cancel: &Cancel,
        report: &mut LayoutReport,
        progress: &mut dyn FnMut(&LayoutReport),
    ) -> Result<()> {
        // ---- Work out what we want, before touching anything -------------
        let mut placements: Vec<Placement> = Vec::new();

        for rel in files.iter() {
            if cancel.is_cancelled() {
                break;
            }
            if opts
                .max_files
                .is_some_and(|m| report.files_compared as usize >= m)
            {
                break;
            }

            let src = rel.under(source_root);
            let dst = rel.under(dest_root);
            let (src_layout, dst_layout) =
                match (extents::file_layout(&src), extents::file_layout(&dst)) {
                    (Ok(a), Ok(b)) => (a, b),
                    // Locked, missing, or resident in the MFT: nothing to align.
                    _ => {
                        report.files_skipped += 1;
                        continue;
                    }
                };

            let src_clusters = src_layout.allocated_clusters();
            if src_clusters < opts.min_clusters {
                report.files_skipped += 1;
                continue;
            }

            report.files_compared += 1;
            report.clusters_total += src_clusters;
            let matched = matching_clusters(&src_layout, &dst_layout);
            report.clusters_matching_before += matched;

            // The runs this file still needs moving into place. A file that
            // is already aligned simply has none — it is still tracked, so the
            // final measurement covers every file we considered.
            let mut runs = Vec::new();
            if matched == src_clusters {
                report.files_identical += 1;
            } else {
                for e in src_layout.extents.iter().filter(|e| e.is_allocated()) {
                    let span = usable_span(&dst_layout, e);
                    if span == 0 {
                        continue;
                    }
                    // Skip the parts already sitting in the right place.
                    if dst_layout.lcn_of(e.vcn) == Some(e.lcn) && span == e.clusters {
                        continue;
                    }
                    runs.push((e.vcn, e.lcn, span));
                }
            }
            placements.push(Placement {
                dest: dst,
                runs,
                source: src_layout,
            });
        }

        let has_work = placements.iter().any(|p| !p.runs.is_empty());
        if opts.dry_run || !has_work {
            // Nothing was moved, so the result is where we started.
            report.clusters_matching_after = report.clusters_matching_before;
            progress(report);
            return Ok(());
        }

        // ---- Now the destructive half ------------------------------------
        let volume = extents::open_volume(&dest_volume.volume, true)?;
        let mut free =
            extents::free_map(&dest_volume.volume, dest_volume.total_clusters.unwrap_or(0))?;
        // Take the volume size from the bitmap we just read, not from the
        // reported geometry: the bitmap is authoritative and always present,
        // whereas `total_clusters` is `None` whenever the geometry query was
        // refused. Trusting the latter left the evacuation allocator with a
        // zero-sized volume and no free space it was willing to hand out.
        let total_clusters = free
            .cluster_count()
            .max(dest_volume.total_clusters.unwrap_or(0));

        // Who is sitting where. Needed because a file's target position is
        // almost always occupied by something else — on a real volume nothing
        // would ever move without being able to clear the way first.
        let mut occupancy = build_occupancy(dest_volume, cancel);

        // Evacuations go above every position we intend to fill, so moving one
        // file out of the way can never land it on another file's target.
        let ceiling = placements
            .iter()
            .flat_map(|p| p.runs.iter())
            .map(|(_, lcn, n)| lcn + *n as i64)
            .max()
            .unwrap_or(0);
        let mut high = HighAllocator::new(total_clusters as i64, ceiling);

        // Low to high: we fill the volume in the same order the source laid it
        // out, so each placement only ever has to clear space ahead of it.
        placements.sort_by_key(|p| p.runs.first().map(|(_, lcn, _)| *lcn).unwrap_or(0));

        for (n, place) in placements.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            if place.runs.is_empty() {
                continue; // already where it should be
            }
            if opts
                .max_clusters_moved
                .is_some_and(|m| report.clusters_moved >= m)
            {
                break;
            }

            let handle = match extents::open_file(&place.dest, true) {
                Ok(h) => h,
                Err(_) => {
                    report.move_failures += 1;
                    continue;
                }
            };
            let mut moved_any = false;

            for &(vcn, target, count) in &place.runs {
                if cancel.is_cancelled() {
                    break;
                }
                // Clear the target range, moving whatever is there up high.
                if !clear_range(
                    volume.0,
                    target,
                    count,
                    &place.dest,
                    &mut free,
                    &mut occupancy,
                    &mut high,
                    report,
                ) {
                    report.extents_blocked += 1;
                    continue;
                }
                if move_run(volume.0, handle.0, vcn, target, count, cancel, report) {
                    free.claim(target, count);
                    record_occupant(&mut occupancy, target, count, &place.dest, vcn);
                    report.clusters_moved += count;
                    moved_any = true;
                }
            }
            if moved_any {
                report.files_moved += 1;
            }
            if n % 32 == 0 {
                progress(report);
            }
        }

        // ---- Re-measure rather than assume -------------------------------
        // Read every compared file's layout back and compute the real figure.
        // A partially blocked file has a genuine in-between similarity, and
        // assuming success would overstate what the pass achieved.
        report.clusters_matching_after = placements
            .iter()
            .map(|p| match extents::file_layout(&p.dest) {
                Ok(after) => matching_clusters(&p.source, &after),
                // Unreadable now: fall back to what it was, rather than
                // reporting it as newly misaligned.
                Err(_) => matching_clusters(&p.source, &FileLayout::default()),
            })
            .sum();

        progress(report);
        Ok(())
    }

    /// Index every file on the destination volume by the clusters it holds.
    ///
    /// This has to cover the whole volume, not just the mirrored tree: the
    /// thing sitting on a file's target position is frequently something we
    /// are not otherwise interested in.
    fn build_occupancy(dest_volume: &VolumeInfo, cancel: &Cancel) -> BTreeMap<i64, Occupant> {
        let mut map = BTreeMap::new();
        let root = super::super::media::volume_root(&dest_volume.volume);
        let tree = match crate::file::walk::scan(
            Path::new(&root),
            &crate::file::filter::Filter::permissive(),
            &crate::file::walk::ScanOptions::default(),
            cancel,
            &mut |_| {},
        ) {
            Ok(t) => t,
            Err(_) => return map,
        };
        for entry in tree.entries.iter().filter(|e| !e.is_dir() && !e.is_link()) {
            if cancel.is_cancelled() {
                break;
            }
            let path = entry.rel.under(Path::new(&root));
            let Ok(layout) = extents::file_layout(&path) else {
                continue;
            };
            if layout.extents.is_empty() {
                continue;
            }
            let shared = Arc::new(path);
            for e in layout.extents.iter().filter(|e| e.is_allocated()) {
                map.insert(
                    e.lcn,
                    Occupant {
                        end: e.end_lcn(),
                        path: shared.clone(),
                        vcn: e.vcn,
                    },
                );
            }
        }
        map
    }

    /// Note that `path` now holds `[lcn, lcn+count)`.
    fn record_occupant(
        map: &mut BTreeMap<i64, Occupant>,
        lcn: i64,
        count: u64,
        path: &Path,
        vcn: i64,
    ) {
        map.insert(
            lcn,
            Occupant {
                end: lcn + count as i64,
                path: Arc::new(path.to_path_buf()),
                vcn,
            },
        );
    }

    /// Everything overlapping `[lcn, lcn+count)`.
    fn occupants_of(map: &BTreeMap<i64, Occupant>, lcn: i64, count: u64) -> Vec<(i64, Occupant)> {
        let end = lcn + count as i64;
        map.range(..end)
            .filter(|(start, o)| o.end > lcn && **start < end)
            .map(|(start, o)| (*start, o.clone()))
            .collect()
    }

    /// Make `[target, target+count)` free, moving any occupants up high.
    ///
    /// Returns false if it could not be cleared, in which case the caller
    /// leaves the file where it is — never a failure, just a miss.
    #[allow(clippy::too_many_arguments)]
    fn clear_range(
        volume: HANDLE,
        target: i64,
        count: u64,
        placing: &Path,
        free: &mut FreeMap,
        occupancy: &mut BTreeMap<i64, Occupant>,
        high: &mut HighAllocator,
        report: &mut LayoutReport,
    ) -> bool {
        if free.range_is_free(target, count) {
            return true;
        }
        for (start, occ) in occupants_of(occupancy, target, count) {
            let span = (occ.end - start) as u64;
            // Moving the file we are about to place, out from under itself,
            // would be self-defeating: just release its claim instead.
            if occ.path.as_path() == placing {
                free.release(start, span);
                occupancy.remove(&start);
                continue;
            }
            let Some(dest) = high.alloc(free, span) else {
                return false; // nowhere to put it
            };
            let Ok(handle) = extents::open_file(&occ.path, true) else {
                free.release(dest, span);
                return false;
            };
            if !move_run(
                volume,
                handle.0,
                occ.vcn,
                dest,
                span,
                &Cancel::none(),
                report,
            ) {
                free.release(dest, span);
                return false;
            }
            free.release(start, span);
            occupancy.remove(&start);
            record_occupant(occupancy, dest, span, &occ.path, occ.vcn);
            report.clusters_moved += span;
        }
        free.range_is_free(target, count)
    }

    /// Ask NTFS to relocate one run. Returns whether it worked.
    fn move_run(
        volume: HANDLE,
        file: HANDLE,
        vcn: i64,
        lcn: i64,
        count: u64,
        cancel: &Cancel,
        report: &mut LayoutReport,
    ) -> bool {
        let mut done = 0u64;
        while done < count {
            if cancel.is_cancelled() {
                return done > 0;
            }
            let chunk = (count - done).min(MAX_CLUSTERS_PER_MOVE);
            let data = MOVE_FILE_DATA {
                FileHandle: file,
                StartingVcn: vcn + done as i64,
                StartingLcn: lcn + done as i64,
                ClusterCount: chunk as u32,
            };
            let mut returned = 0u32;
            // SAFETY: `data` is exactly the documented input for
            // FSCTL_MOVE_FILE, and both handles are live for the call.
            let rc = unsafe {
                DeviceIoControl(
                    volume,
                    FSCTL_MOVE_FILE,
                    Some(&data as *const _ as *const c_void),
                    std::mem::size_of::<MOVE_FILE_DATA>() as u32,
                    None,
                    0,
                    Some(&mut returned),
                    None,
                )
            };
            if rc.is_err() {
                // NTFS declined — the target may have been taken since we read
                // the bitmap, or the file is being written. Not fatal: the file
                // simply stays where it is.
                report.move_failures += 1;
                return done > 0;
            }
            done += chunk;
        }
        true
    }

    /// How many clusters starting at `e.vcn` the destination file actually
    /// has, capped at the source run's length.
    fn usable_span(dst_layout: &FileLayout, e: &Extent) -> u64 {
        let mut span = 0u64;
        while span < e.clusters {
            match dst_layout.lcn_of(e.vcn + span as i64) {
                Some(_) => span += 1,
                None => break,
            }
        }
        span
    }
}

// ===========================================================================
// Linux
// ===========================================================================
#[cfg(not(windows))]
mod imp {
    use super::*;

    /// ext4 has `EXT4_IOC_MOVE_EXT`, which relocates a file's extents to
    /// match a donor file — the closest analogue to `FSCTL_MOVE_FILE`, and how
    /// `e4defrag` works. It targets a donor rather than an absolute physical
    /// position, so matching two drives cluster-for-cluster needs a different
    /// strategy there than on NTFS. The comparison half of this module is
    /// portable; the placement half is not, and says so.
    #[allow(clippy::too_many_arguments)]
    pub fn run_pass(
        _source_root: &Path,
        _dest_root: &Path,
        _dest_volume: &VolumeInfo,
        _files: &[RelPath],
        _opts: &LayoutOptions,
        _cancel: &Cancel,
        report: &mut LayoutReport,
        _progress: &mut dyn FnMut(&LayoutReport),
    ) -> Result<()> {
        report.refused = Some(
            "cluster placement is implemented for NTFS only; ext4 would need \
             EXT4_IOC_MOVE_EXT with a donor file"
                .into(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::extents::Extent;
    use super::*;

    fn layout(runs: &[(i64, i64, u64)]) -> FileLayout {
        FileLayout {
            extents: runs
                .iter()
                .map(|&(vcn, lcn, clusters)| Extent { vcn, lcn, clusters })
                .collect(),
        }
    }

    #[test]
    fn identical_layouts_match_completely() {
        let a = layout(&[(0, 1000, 50)]);
        assert_eq!(matching_clusters(&a, &a), 50);
    }

    #[test]
    fn different_positions_match_nowhere() {
        let a = layout(&[(0, 1000, 50)]);
        let b = layout(&[(0, 9000, 50)]);
        assert_eq!(matching_clusters(&a, &b), 0);
    }

    #[test]
    fn partial_overlap_counts_only_the_aligned_clusters() {
        // Both files hold VCNs 0..50. The destination has the first 20 in the
        // right place and the rest somewhere else.
        let src = layout(&[(0, 1000, 50)]);
        let dst = layout(&[(0, 1000, 20), (20, 7000, 30)]);
        assert_eq!(matching_clusters(&src, &dst), 20);
    }

    #[test]
    fn differently_split_extents_still_match_when_positions_agree() {
        // One contiguous run versus two adjacent runs describing the same
        // physical clusters: the layouts are the same, and must be seen as
        // the same even though the extent lists differ.
        let src = layout(&[(0, 1000, 50)]);
        let dst = layout(&[(0, 1000, 20), (20, 1020, 30)]);
        assert_eq!(matching_clusters(&src, &dst), 50);
    }

    #[test]
    fn sparse_holes_never_count_as_matching() {
        // Two files both with a hole at the same VCNs: a hole occupies no
        // physical clusters, so there is nothing to align.
        let a = layout(&[(0, 100, 10), (10, -1, 100), (110, 200, 10)]);
        assert_eq!(matching_clusters(&a, &a), 20, "only the real clusters");
    }

    #[test]
    fn a_shorter_destination_matches_only_what_it_has() {
        let src = layout(&[(0, 1000, 100)]);
        let dst = layout(&[(0, 1000, 40)]);
        assert_eq!(matching_clusters(&src, &dst), 40);
    }

    #[test]
    fn empty_layouts_are_handled() {
        let empty = FileLayout::default();
        assert_eq!(matching_clusters(&empty, &empty), 0);
        assert_eq!(matching_clusters(&layout(&[(0, 1, 5)]), &empty), 0);
    }

    #[test]
    fn similarity_reports_a_sensible_ratio() {
        let r = LayoutReport {
            clusters_matching_before: 620,
            clusters_matching_after: 970,
            clusters_total: 1000,
            ..Default::default()
        };
        assert!((r.similarity_before() - 0.62).abs() < 1e-9);
        assert!((r.similarity_after() - 0.97).abs() < 1e-9);
        assert!(r.describe().contains("62.0% -> 97.0%"), "{}", r.describe());
    }

    #[test]
    fn nothing_to_compare_is_full_similarity_not_a_divide_by_zero() {
        let r = LayoutReport::default();
        assert_eq!(r.similarity_before(), 1.0);
        assert_eq!(r.similarity_after(), 1.0);
    }

    #[test]
    fn a_non_ntfs_destination_is_refused_with_a_reason() {
        let vol = VolumeInfo {
            volume: "E:".into(),
            kind: super::super::media::MediaKind::Hdd,
            trim: false,
            filesystem: Some("exFAT".into()),
            bytes_per_cluster: Some(4096),
            total_clusters: Some(1000),
            free_clusters: Some(500),
        };
        let r = run(
            Path::new("D:/src"),
            Path::new("E:/dst"),
            &vol,
            &[],
            &LayoutOptions::default(),
            &Cancel::none(),
            &mut |_| {},
        )
        .unwrap();
        assert!(r.refused.as_ref().unwrap().contains("NTFS"), "{r:?}");
        assert_eq!(r.files_moved, 0);
    }
}
