//! Disk optimization — the opt-in half of the tool.
//!
//! Most jobs are a plain file mirror and never touch any of this. Some pairs of
//! drives are meant to stay structurally identical, and for those a job can
//! turn on an optimization pass that runs after the copy:
//!
//! ```text
//!   media::probe      what is this drive, actually?      HDD / SSD / unknown
//!     -> optimize     run the OS optimizer, correctly    defrag | retrim | none
//!     -> extents      read the real cluster maps         fragmentation, free space
//!       -> layout     place files at matching positions  FSCTL_MOVE_FILE
//! ```
//!
//! The ordering matters. [`media`] gates everything, because defragmenting
//! solid state is actively harmful and no amount of user insistence makes it a
//! good idea. [`optimize`] hands the general case to Windows' own `defrag.exe`,
//! which is better at it than anything worth writing here. [`layout`] does the
//! part Windows has no interest in: making the *spare* drive look like the
//! *source* drive, rather than merely making each one tidy on its own.

pub mod extents;
pub mod layout;
pub mod media;
pub mod optimize;

use crate::file::path::RelPath;
use crate::file::Cancel;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// A job's optimization settings. Absent from a job means "just copy the
/// files", which is the common case.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OptimizeSettings {
    /// Run the OS optimizer (defrag or retrim, chosen by media type) on the
    /// destination after a successful mirror.
    #[serde(default)]
    pub optimize_destination: bool,
    /// Also optimize the source.
    #[serde(default)]
    pub optimize_source: bool,
    /// Align the destination's physical layout to the source's.
    #[serde(default)]
    pub mirror_layout: bool,
    /// Report fragmentation and layout divergence without changing anything.
    #[serde(default)]
    pub analyze_only: bool,
    /// Cap the layout pass so a nightly job makes steady progress instead of
    /// running until morning.
    #[serde(default)]
    pub max_layout_files: Option<usize>,
    /// Skip files below this many clusters in the layout pass.
    #[serde(default)]
    pub min_layout_clusters: Option<u64>,
}

impl OptimizeSettings {
    /// Is there anything to do at all?
    pub fn is_enabled(&self) -> bool {
        self.optimize_destination || self.optimize_source || self.mirror_layout || self.analyze_only
    }

    /// The settings for a pair of drives meant to stay identical.
    pub fn identical_drives() -> Self {
        OptimizeSettings {
            optimize_destination: true,
            optimize_source: false,
            mirror_layout: true,
            analyze_only: false,
            max_layout_files: None,
            min_layout_clusters: None,
        }
    }
}

/// Everything one optimization pass produced.
#[derive(Debug, Clone, Default)]
pub struct OptimizeReport {
    pub source: Option<media::VolumeInfo>,
    pub dest: Option<media::VolumeInfo>,
    pub source_optimize: Option<optimize::OptimizeResult>,
    pub dest_optimize: Option<optimize::OptimizeResult>,
    pub layout: Option<layout::LayoutReport>,
    /// Fragmentation on each side, when an analysis was requested.
    pub source_fragmentation: Option<extents::FragReport>,
    pub dest_fragmentation: Option<extents::FragReport>,
}

impl OptimizeReport {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        for (label, info) in [("source", &self.source), ("destination", &self.dest)] {
            if let Some(i) = info {
                s.push_str(&format!("{label:>12}: {}\n", i.describe()));
            }
        }
        if let Some(f) = &self.source_fragmentation {
            s.push_str("source fragmentation:\n");
            s.push_str(&f.describe());
        }
        if let Some(f) = &self.dest_fragmentation {
            s.push_str("destination fragmentation:\n");
            s.push_str(&f.describe());
        }
        for r in [&self.source_optimize, &self.dest_optimize]
            .into_iter()
            .flatten()
        {
            s.push_str(&r.summary());
        }
        if let Some(l) = &self.layout {
            s.push_str("layout:\n");
            s.push_str(&l.describe());
        }
        s
    }
}

/// Run the optimization stage of a job.
///
/// `files` are the relative paths present in both trees — normally taken from
/// the mirror's source scan, so the layout pass only considers files that
/// actually exist on both drives.
pub fn run(
    source_root: &Path,
    dest_root: &Path,
    files: &[RelPath],
    settings: &OptimizeSettings,
    cancel: &Cancel,
    progress: &mut dyn FnMut(&str),
) -> Result<OptimizeReport> {
    let mut report = OptimizeReport::default();
    if !settings.is_enabled() {
        return Ok(report);
    }

    progress("probing drives");
    report.source = media::probe(&volume_of(source_root)).ok();
    report.dest = media::probe(&volume_of(dest_root)).ok();

    if settings.analyze_only {
        progress("measuring fragmentation");
        let src_paths: Vec<_> = files.iter().map(|r| r.under(source_root)).collect();
        let dst_paths: Vec<_> = files.iter().map(|r| r.under(dest_root)).collect();
        report.source_fragmentation = Some(extents::analyze(src_paths.iter().map(|p| p.as_path())));
        report.dest_fragmentation = Some(extents::analyze(dst_paths.iter().map(|p| p.as_path())));
    }

    // Layout alignment runs *before* the OS optimizer. Defragmenting after
    // would undo the placement we just achieved: `defrag` consolidates each
    // volume on its own terms, with no knowledge that we were trying to make
    // the two drives match.
    if settings.mirror_layout {
        if let Some(dest) = &report.dest {
            progress("aligning layout");
            let opts = layout::LayoutOptions {
                dry_run: settings.analyze_only,
                max_files: settings.max_layout_files,
                min_clusters: settings
                    .min_layout_clusters
                    .unwrap_or(layout::LayoutOptions::default().min_clusters),
                ..Default::default()
            };
            report.layout = Some(layout::run(
                source_root,
                dest_root,
                dest,
                files,
                &opts,
                cancel,
                &mut |_| {},
            )?);
        }
    }

    let opt_opts = optimize::OptimizeOptions {
        dry_run: settings.analyze_only,
        ..Default::default()
    };
    if settings.optimize_source {
        if let Some(info) = &report.source {
            progress("optimizing source");
            report.source_optimize = Some(optimize::run(info, &opt_opts, cancel)?);
        }
    }
    if settings.optimize_destination {
        if let Some(info) = &report.dest {
            progress("optimizing destination");
            report.dest_optimize = Some(optimize::run(info, &opt_opts, cancel)?);
        }
    }

    Ok(report)
}

/// The volume a path lives on, in the form [`media::probe`] wants.
pub fn volume_of(path: &Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    {
        // `D:\data\x` -> `D:`. A UNC path has no local volume.
        let s = s.trim_start_matches(r"\\?\");
        if let Some(rest) = s.get(..2) {
            if rest.chars().nth(1) == Some(':') {
                return rest.to_string();
            }
        }
        String::new()
    }
    #[cfg(not(windows))]
    {
        s.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_doing_nothing() {
        // Most jobs are a plain copy; optimization must be opt-in.
        assert!(!OptimizeSettings::default().is_enabled());
    }

    #[test]
    fn the_identical_drives_preset_turns_on_layout_mirroring() {
        let s = OptimizeSettings::identical_drives();
        assert!(s.is_enabled());
        assert!(s.mirror_layout);
        assert!(s.optimize_destination);
    }

    #[test]
    fn a_disabled_settings_block_short_circuits_without_touching_a_drive() {
        let r = run(
            Path::new("D:/src"),
            Path::new("E:/dst"),
            &[],
            &OptimizeSettings::default(),
            &Cancel::none(),
            &mut |_| panic!("should not have done any work"),
        )
        .unwrap();
        assert!(r.source.is_none() && r.dest.is_none() && r.layout.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn volume_is_extracted_from_a_path() {
        assert_eq!(volume_of(Path::new(r"D:\data\photos")), "D:");
        assert_eq!(volume_of(Path::new(r"\\?\E:\x")), "E:");
        assert_eq!(volume_of(Path::new(r"\\nas\share\x")), "");
    }

    #[test]
    fn settings_round_trip_through_json() {
        let s = OptimizeSettings::identical_drives();
        let text = serde_json::to_string(&s).unwrap();
        let back: OptimizeSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.mirror_layout, s.mirror_layout);
        assert_eq!(back.optimize_destination, s.optimize_destination);

        // An older job file with no optimize block at all must still load.
        let bare: OptimizeSettings = serde_json::from_str("{}").unwrap();
        assert!(!bare.is_enabled());
    }
}
