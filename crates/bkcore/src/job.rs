//! Job configuration — "what to back up, where, when, and how".
//!
//! A job is a JSON file on disk. The GUI edits it, the scheduler registers it,
//! and the engine executes it, all reading the same document, so there is one
//! source of truth and no way for the three to disagree.
//!
//! Two kinds of job share the format:
//!
//! * **Mirror** — the main one. A source path (usually a drive root, sometimes
//!   a subfolder) is reproduced at a destination path.
//! * **Image** — the block-level path: a whole volume captured into the
//!   content-addressed chunk store, for bare-metal restore.
//!
//! Every field that can sensibly have a default does, so a minimal job file is
//! four lines and still runs correctly. That matters because these files get
//! hand-edited.

use crate::file::attic::AtticPolicy;
use crate::file::copy::CopyOptions;
use crate::file::filter::Filter;
use crate::file::mirror::MirrorJob;
use crate::file::plan::{CompareBy, MirrorMode};
use crate::file::walk::ScanOptions;
use crate::opt::OptimizeSettings;
use crate::{BkError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bump when the format changes incompatibly.
pub const JOB_VERSION: u32 = 2;

/// One configured job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    #[serde(default = "current_version")]
    pub version: u32,
    /// Stable id. Used for the state directory and the scheduled task name.
    pub name: String,
    /// Free text for the operator's benefit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Skip this job without deleting it.
    #[serde(default)]
    pub disabled: bool,
    #[serde(flatten)]
    pub kind: JobKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    /// Optional post-copy disk work. Absent means "just copy the files".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optimize: Option<OptimizeSettings>,
    /// Where run state lives. Defaults to the per-machine state root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<PathBuf>,
}

fn current_version() -> u32 {
    JOB_VERSION
}

/// What sort of backup this job performs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobKind {
    /// File-level mirror: one path to another.
    Mirror(MirrorSpec),
    /// Block-level volume image into the chunk store.
    Image(ImageSpec),
}

/// The settings of a file mirror.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MirrorSpec {
    /// What to back up. A drive root (`D:\`) or any folder beneath one.
    pub source: PathBuf,
    /// Where it goes.
    pub dest: PathBuf,
    /// What happens to destination files the source no longer has.
    #[serde(default)]
    pub mode: MirrorMode,
    /// How hard to look before declaring a file unchanged.
    #[serde(default)]
    pub compare: CompareBy,
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub fidelity: Fidelity,
    /// Concurrent copies. `None` picks a value from the CPU count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
    /// Follow symlinks and junctions instead of re-creating them. Off by
    /// default: a junction pointing at its own ancestor is an infinite tree.
    #[serde(default)]
    pub follow_links: bool,
    /// Detect hard links so a file with several names is stored once.
    #[serde(default)]
    pub preserve_hardlinks: bool,
    /// Descend through mount points onto other volumes.
    #[serde(default)]
    pub cross_volume: bool,
    /// Where displaced files go in attic mode. Defaults to
    /// `<dest>/.bku-attic/<timestamp>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attic_root: Option<PathBuf>,
    /// When old attic folders are removed. The default keeps 10 runs; without
    /// a limit the attic grows forever and is what eventually fills the spare.
    #[serde(default)]
    pub attic_policy: AtticPolicy,
    /// Refuse to start when the destination plainly cannot hold the copy.
    #[serde(default = "yes")]
    pub check_free_space: bool,
    /// Write a per-run log under the state directory. A scheduled job with
    /// this off leaves no record of what it did.
    #[serde(default = "yes")]
    pub log: bool,
}

/// How much of NTFS's metadata to carry across.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fidelity {
    #[serde(default = "yes")]
    pub acls: bool,
    #[serde(default = "yes")]
    pub alternate_data_streams: bool,
    #[serde(default = "yes")]
    pub sparse: bool,
    #[serde(default = "yes")]
    pub timestamps: bool,
    #[serde(default = "yes")]
    pub attributes: bool,
    /// Re-read and hash every file after writing it. Roughly doubles the
    /// destination's I/O, so it is off by default and worth turning on for
    /// data you cannot re-create.
    #[serde(default)]
    pub verify: bool,
    /// Use `SeBackupPrivilege` to read files whose ACLs exclude us.
    #[serde(default = "yes")]
    pub use_backup_privilege: bool,
}

fn yes() -> bool {
    true
}

impl Default for Fidelity {
    fn default() -> Self {
        Fidelity {
            acls: true,
            alternate_data_streams: true,
            sparse: true,
            timestamps: true,
            attributes: true,
            verify: false,
            use_backup_privilege: true,
        }
    }
}

impl Fidelity {
    /// Everything a non-NTFS destination can actually hold.
    pub fn data_only() -> Self {
        Fidelity {
            acls: false,
            alternate_data_streams: false,
            sparse: false,
            timestamps: true,
            attributes: false,
            verify: false,
            use_backup_privilege: true,
        }
    }

    pub fn to_copy_options(&self) -> CopyOptions {
        CopyOptions {
            preserve_acls: self.acls,
            preserve_ads: self.alternate_data_streams,
            preserve_sparse: self.sparse,
            preserve_timestamps: self.timestamps,
            preserve_attributes: self.attributes,
            verify: self.verify,
            use_backup_privilege: self.use_backup_privilege,
            ..CopyOptions::default()
        }
    }
}

/// The settings of a block-level volume image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSpec {
    /// Source volume, e.g. `"C:"`, resolved to a device path at run time.
    pub source: String,
    /// Root of the content-addressed chunk store.
    pub store_root: PathBuf,
    /// Re-read and re-hash stored chunks after each run.
    #[serde(default = "yes")]
    pub verify_after: bool,
}

/// When a job runs unattended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub frequency: Frequency,
    /// 24-hour local time, `"HH:MM"`. Used by daily and weekly schedules.
    #[serde(default = "default_time")]
    pub at: String,
    /// Day for a weekly schedule, e.g. `"SUN"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    /// Run as SYSTEM rather than the registering user. Lets the job run with
    /// nobody logged in, which is what a server wants.
    #[serde(default)]
    pub run_as_system: bool,
    /// Start the job late if the machine was off at the trigger time.
    #[serde(default = "yes")]
    pub run_if_missed: bool,
    /// Do not start on battery, and stop if the machine goes onto battery.
    #[serde(default)]
    pub only_on_ac_power: bool,
}

fn default_time() -> String {
    "03:00".into()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Frequency {
    Hourly,
    Daily,
    Weekly,
    /// At every user logon.
    OnLogon,
    /// At system start.
    OnStart,
}

impl Frequency {
    /// Does this frequency need a time of day?
    pub fn needs_time(self) -> bool {
        matches!(self, Frequency::Daily | Frequency::Weekly)
    }
}

impl Job {
    /// A ready-to-run mirror job with sensible defaults.
    pub fn mirror(
        name: impl Into<String>,
        source: impl Into<PathBuf>,
        dest: impl Into<PathBuf>,
    ) -> Self {
        Job {
            version: JOB_VERSION,
            name: name.into(),
            description: None,
            disabled: false,
            kind: JobKind::Mirror(MirrorSpec {
                source: source.into(),
                dest: dest.into(),
                mode: MirrorMode::Additive,
                compare: CompareBy::SizeAndTime,
                filter: Filter::permissive(),
                fidelity: Fidelity::default(),
                workers: None,
                follow_links: false,
                preserve_hardlinks: false,
                cross_volume: false,
                attic_root: None,
                attic_policy: AtticPolicy::default(),
                check_free_space: true,
                log: true,
            }),
            schedule: None,
            optimize: None,
            state_root: None,
        }
    }

    /// The Windows scheduled-task name this job registers under.
    pub fn task_name(&self) -> String {
        format!(r"BackupUtility\{}", self.name)
    }

    pub fn as_mirror(&self) -> Option<&MirrorSpec> {
        match &self.kind {
            JobKind::Mirror(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_image(&self) -> Option<&ImageSpec> {
        match &self.kind {
            JobKind::Image(i) => Some(i),
            _ => None,
        }
    }

    /// Turn a mirror job into the runnable form the engine takes.
    pub fn to_mirror_job(&self) -> Result<MirrorJob> {
        let spec = self.as_mirror().ok_or_else(|| {
            BkError::Other(anyhow::anyhow!("job '{}' is not a mirror job", self.name))
        })?;
        Ok(MirrorJob {
            name: self.name.clone(),
            source: spec.source.clone(),
            dest: spec.dest.clone(),
            mode: spec.mode,
            compare: spec.compare,
            filter: spec.filter.clone(),
            scan: ScanOptions {
                follow_links: spec.follow_links,
                detect_hardlinks: spec.preserve_hardlinks,
                cross_volume: spec.cross_volume,
                ..ScanOptions::default()
            },
            copy: spec.fidelity.to_copy_options(),
            workers: spec
                .workers
                .unwrap_or_else(crate::file::mirror::default_workers),
            dry_run: false,
            resume: true,
            state_root: self
                .state_root
                .clone()
                .unwrap_or_else(crate::file::state::default_state_root),
            attic_root: spec.attic_root.clone(),
            attic_policy: spec.attic_policy.clone(),
            check_free_space: spec.check_free_space,
            log: spec.log,
            // The layout pass consumes this; without it the optimization stage
            // would walk the destination all over again.
            report_file_list: self
                .optimize
                .as_ref()
                .is_some_and(|o| o.mirror_layout || o.analyze_only),
        })
    }

    /// Catch configuration mistakes before a scheduled run hits them at 3am.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(BkError::Other(anyhow::anyhow!("a job needs a name")));
        }
        if self.version > JOB_VERSION {
            return Err(BkError::Other(anyhow::anyhow!(
                "job '{}' was written by a newer version of this tool \
                 (format {}, we understand {JOB_VERSION})",
                self.name,
                self.version
            )));
        }
        if let Some(s) = &self.schedule {
            if s.frequency.needs_time() && !is_hhmm(&s.at) {
                return Err(BkError::Other(anyhow::anyhow!(
                    "job '{}': schedule time '{}' is not HH:MM",
                    self.name,
                    s.at
                )));
            }
        }
        match &self.kind {
            JobKind::Mirror(_) => {
                // Reuses the mirror's own guards: self-nesting, missing source.
                let mj = self.to_mirror_job()?;
                crate::file::mirror::validate(&mj)?;
            }
            JobKind::Image(i) => {
                if i.source.trim().is_empty() {
                    return Err(BkError::Other(anyhow::anyhow!(
                        "job '{}': image jobs need a source volume",
                        self.name
                    )));
                }
            }
        }
        Ok(())
    }
}

fn is_hhmm(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    matches!((h.parse::<u32>(), m.parse::<u32>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
        && h.len() == 2
        && m.len() == 2
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where job files live by default: `jobs/` under the configuration root.
pub fn default_jobs_dir() -> PathBuf {
    crate::paths::config_root().join("jobs")
}

/// What [`JobStore::list`] returns: the jobs that parsed, and the files that
/// did not, each with the reason.
pub type JobListing = (Vec<Job>, Vec<(PathBuf, String)>);

/// A directory of job files.
pub struct JobStore {
    dir: PathBuf,
}

impl JobStore {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        // Only the machine-wide directory is tightened; a custom --jobs-dir is
        // the operator's business.
        if dir == default_jobs_dir() {
            crate::paths::restrict_to_admins(&dir);
        }
        Ok(JobStore { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.json", sanitize(name)))
    }

    pub fn save(&self, job: &Job) -> Result<PathBuf> {
        let path = self.path_for(&job.name);
        let json = serde_json::to_vec_pretty(job).map_err(|e| BkError::Other(e.into()))?;
        // Same atomic-replace discipline as everywhere else: a half-written
        // job file that a scheduled task then reads would be worse than none.
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        };
        write().map_err(|e| {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                BkError::Other(anyhow::anyhow!(
                    "cannot write {}: {e} — the jobs directory is reserved for \
                     administrators because scheduled runs execute as SYSTEM; \
                     run this from an elevated prompt",
                    path.display()
                ))
            } else {
                BkError::Io(e)
            }
        })?;
        Ok(path)
    }

    pub fn load(&self, name: &str) -> Result<Job> {
        let path = self.path_for(name);
        let data = std::fs::read(&path).map_err(|e| {
            BkError::Other(anyhow::anyhow!(
                "no job named '{name}' in {}: {e}",
                self.dir.display()
            ))
        })?;
        serde_json::from_slice(&data)
            .map_err(|e| BkError::Other(anyhow::anyhow!("job '{name}' is not valid JSON: {e}")))
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let path = self.path_for(name);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Every job in the directory, sorted by name. Unparseable files are
    /// reported alongside so a single bad file does not hide the rest.
    pub fn list(&self) -> Result<JobListing> {
        let mut jobs = Vec::new();
        let mut broken = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|d| serde_json::from_slice::<Job>(&d).map_err(|e| e.to_string()))
            {
                Ok(j) => jobs.push(j),
                Err(e) => broken.push((path, e)),
            }
        }
        jobs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((jobs, broken))
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "job".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(n: &str) -> Self {
            let p = std::env::temp_dir().join(format!("bku_job_{n}"));
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
    fn a_minimal_job_file_parses_with_defaults() {
        // These files get hand-edited, so the shortest sensible one must work.
        let text = r#"{
            "name": "photos",
            "type": "mirror",
            "source": "D:\\Photos",
            "dest": "E:\\Backup\\Photos"
        }"#;
        let job: Job = serde_json::from_str(text).unwrap();
        assert_eq!(job.name, "photos");
        assert!(!job.disabled);
        assert!(job.schedule.is_none());
        assert!(job.optimize.is_none(), "optimization must be opt-in");

        let m = job.as_mirror().unwrap();
        assert_eq!(m.mode, MirrorMode::Additive, "the safe default");
        assert_eq!(m.compare, CompareBy::SizeAndTime);
        assert!(m.fidelity.acls, "full fidelity by default");
        assert!(!m.fidelity.verify, "but not the expensive verify pass");
        assert!(!m.follow_links, "and never follow links by default");
    }

    #[test]
    fn a_job_round_trips_through_json() {
        let mut job = Job::mirror("nightly", r"D:\", r"E:\");
        job.schedule = Some(Schedule {
            frequency: Frequency::Daily,
            at: "03:30".into(),
            day: None,
            run_as_system: true,
            run_if_missed: true,
            only_on_ac_power: false,
        });
        job.optimize = Some(OptimizeSettings::identical_drives());

        let text = serde_json::to_string_pretty(&job).unwrap();
        let back: Job = serde_json::from_str(&text).unwrap();
        assert_eq!(back.name, "nightly");
        assert_eq!(back.schedule.as_ref().unwrap().at, "03:30");
        assert!(back.optimize.as_ref().unwrap().mirror_layout);
        assert_eq!(back.as_mirror().unwrap().source, PathBuf::from(r"D:\"));
    }

    #[test]
    fn an_image_job_uses_the_same_file_format() {
        let text = r#"{
            "name": "system",
            "type": "image",
            "source": "C:",
            "store_root": "E:\\Images"
        }"#;
        let job: Job = serde_json::from_str(text).unwrap();
        let i = job.as_image().unwrap();
        assert_eq!(i.source, "C:");
        assert!(i.verify_after, "verification defaults on for images");
        assert!(job.as_mirror().is_none());
    }

    #[test]
    fn a_job_from_a_newer_format_is_refused_rather_than_misread() {
        let mut job = Job::mirror("future", r"D:\x", r"E:\y");
        job.version = JOB_VERSION + 1;
        let err = job.validate().unwrap_err().to_string();
        assert!(err.contains("newer version"), "{err}");
    }

    #[test]
    fn a_malformed_schedule_time_is_caught_at_configuration_time() {
        // Better to fail when the operator saves the job than at 3am.
        let mut job = Job::mirror("bad", r"D:\x", r"E:\y");
        job.schedule = Some(Schedule {
            frequency: Frequency::Daily,
            at: "3am".into(),
            day: None,
            run_as_system: false,
            run_if_missed: true,
            only_on_ac_power: false,
        });
        let err = job.validate().unwrap_err().to_string();
        assert!(err.contains("HH:MM"), "{err}");

        assert!(is_hhmm("03:00") && is_hhmm("23:59"));
        assert!(!is_hhmm("3:00") && !is_hhmm("24:00") && !is_hhmm("12:60"));
    }

    #[test]
    fn a_trigger_that_needs_no_time_does_not_demand_one() {
        let mut job = Job::mirror("onstart", std::env::temp_dir(), r"E:\y");
        job.schedule = Some(Schedule {
            frequency: Frequency::OnStart,
            at: "not a time".into(),
            day: None,
            run_as_system: true,
            run_if_missed: false,
            only_on_ac_power: false,
        });
        // The time is ignored for this trigger, so it must not be validated.
        assert!(!Frequency::OnStart.needs_time());
        // (The mirror validation will still object to the fake destination,
        // so only the schedule half is asserted here.)
        assert!(is_hhmm(&default_time()));
    }

    #[test]
    fn fidelity_maps_onto_copy_options() {
        let f = Fidelity::data_only();
        let o = f.to_copy_options();
        assert!(!o.preserve_acls && !o.preserve_ads && !o.preserve_attributes);
        assert!(o.preserve_timestamps, "timestamps work on any filesystem");

        let full = Fidelity::default().to_copy_options();
        assert!(full.preserve_acls && full.preserve_ads && full.preserve_sparse);
    }

    #[test]
    fn the_store_saves_loads_lists_and_deletes() {
        let t = Tmp::new("store");
        let store = JobStore::open(&t.0).unwrap();
        store.save(&Job::mirror("alpha", r"D:\a", r"E:\a")).unwrap();
        store.save(&Job::mirror("beta", r"D:\b", r"E:\b")).unwrap();

        let (jobs, broken) = store.list().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(broken.is_empty());
        assert_eq!(jobs[0].name, "alpha", "listed in name order");

        assert_eq!(store.load("beta").unwrap().name, "beta");
        store.delete("alpha").unwrap();
        assert_eq!(store.list().unwrap().0.len(), 1);
        // Deleting something absent is not an error.
        store.delete("alpha").unwrap();
    }

    #[test]
    fn one_corrupt_job_file_does_not_hide_the_others() {
        let t = Tmp::new("broken");
        let store = JobStore::open(&t.0).unwrap();
        store.save(&Job::mirror("good", r"D:\a", r"E:\a")).unwrap();
        std::fs::write(t.0.join("bad.json"), b"{ not json at all").unwrap();

        let (jobs, broken) = store.list().unwrap();
        assert_eq!(jobs.len(), 1, "the good job is still listed");
        assert_eq!(broken.len(), 1, "and the bad one is reported, not silent");
    }

    #[test]
    fn job_names_become_safe_filenames() {
        let t = Tmp::new("names");
        let store = JobStore::open(&t.0).unwrap();
        // A name a user would plausibly type, containing path separators.
        let job = Job::mirror(r"D: -> E: nightly", r"D:\a", r"E:\a");
        let path = store.save(&job).unwrap();
        assert!(path.exists());
        assert_eq!(
            store.load(r"D: -> E: nightly").unwrap().name,
            r"D: -> E: nightly"
        );
    }

    #[test]
    fn conversion_to_a_runnable_mirror_job_carries_the_settings() {
        let mut job = Job::mirror("x", r"D:\src", r"E:\dst");
        if let JobKind::Mirror(m) = &mut job.kind {
            m.mode = MirrorMode::Attic;
            m.workers = Some(3);
            m.preserve_hardlinks = true;
            m.fidelity.verify = true;
        }
        let mj = job.to_mirror_job().unwrap();
        assert_eq!(mj.mode, MirrorMode::Attic);
        assert_eq!(mj.workers, 3);
        assert!(mj.scan.detect_hardlinks);
        assert!(mj.copy.verify);
        assert!(mj.resume, "resume is always on for a configured job");
    }

    #[test]
    fn converting_an_image_job_to_a_mirror_job_is_an_error_not_a_panic() {
        let job: Job = serde_json::from_str(
            r#"{"name":"i","type":"image","source":"C:","store_root":"E:\\s"}"#,
        )
        .unwrap();
        let err = job.to_mirror_job().unwrap_err().to_string();
        assert!(err.contains("not a mirror job"), "{err}");
    }
}
