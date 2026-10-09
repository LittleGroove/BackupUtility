//! Driving the operating system's own optimizer.
//!
//! Windows ships `defrag.exe`, the command-line face of Optimize Drives. It
//! already knows how to move files safely while the volume is mounted and in
//! use, and it is the same engine the built-in maintenance task runs. Writing
//! our own general-purpose defragmenter would be strictly worse, so this module
//! decides *which* operation to run — the part that needs the media detection
//! in [`super::media`] — and then hands off.
//!
//! What we deliberately do *not* do is parse `defrag /A` output to measure
//! fragmentation. That text is localized, so parsing it would silently return
//! nothing on a non-English system. [`super::extents`] computes the same
//! numbers from the cluster maps directly, which is exact and language-neutral.

use super::media::{self, MediaKind, OptimizeOp, VolumeInfo};
use crate::file::Cancel;
use crate::{BkError, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How to run an optimization pass.
#[derive(Debug, Clone)]
pub struct OptimizeOptions {
    /// Force a specific operation. `None` picks one from the detected media,
    /// which is almost always what you want.
    pub op: Option<OptimizeOp>,
    /// Report the command that would run, but do not run it.
    pub dry_run: bool,
    /// Give up after this long. A full defrag of a large spinning disk can run
    /// for hours, so the default is generous.
    pub timeout: Option<Duration>,
    /// Run at low I/O priority so a scheduled optimization does not make the
    /// machine unusable.
    pub low_priority: bool,
    /// Permit a defragmentation pass on media we could not positively identify
    /// as spinning. Off by default — see [`super::media`].
    pub allow_defrag_on_unknown_media: bool,
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        OptimizeOptions {
            op: None,
            dry_run: false,
            timeout: Some(Duration::from_secs(6 * 60 * 60)),
            low_priority: true,
            allow_defrag_on_unknown_media: false,
        }
    }
}

/// What an optimization pass did.
#[derive(Debug, Clone)]
pub struct OptimizeResult {
    pub volume: String,
    pub media: MediaKind,
    /// The operation actually chosen.
    pub op: OptimizeOp,
    /// The command line, so a user can see exactly what was run.
    pub command: String,
    /// False for a dry run, or when the chosen operation was `AnalyzeOnly`
    /// and nothing needed doing.
    pub ran: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub elapsed: Duration,
    /// Set when we declined to do what was asked, and why.
    pub refused: Option<String>,
}

impl OptimizeResult {
    pub fn succeeded(&self) -> bool {
        self.refused.is_none() && (!self.ran || self.exit_code == Some(0))
    }

    pub fn summary(&self) -> String {
        let mut s = format!("{}  [{}]  {}\n", self.volume, self.media, self.op);
        if let Some(why) = &self.refused {
            s.push_str(&format!("  refused: {why}\n"));
            return s;
        }
        s.push_str(&format!("  command: {}\n", self.command));
        if self.ran {
            s.push_str(&format!(
                "  exit {} in {}\n",
                self.exit_code.map(|c| c.to_string()).unwrap_or("?".into()),
                crate::timefmt::duration(self.elapsed.as_secs())
            ));
        } else {
            s.push_str("  not run (dry run)\n");
        }
        s
    }
}

/// Decide what to do and do it.
pub fn run(info: &VolumeInfo, opts: &OptimizeOptions, cancel: &Cancel) -> Result<OptimizeResult> {
    let chosen = opts.op.unwrap_or_else(|| media::recommended_op(info));

    // Guard the one genuinely damaging mistake: defragmenting solid state.
    // An explicit `op` does not override this, because "I asked for it" is not
    // a good enough reason to burn an SSD's write endurance by accident.
    if chosen == OptimizeOp::Defrag {
        let refuse = match info.kind {
            MediaKind::Ssd => Some(
                "this is a solid-state drive; defragmenting it costs write \
                 endurance and gains nothing — use retrim instead"
                    .to_string(),
            ),
            MediaKind::Unknown if !opts.allow_defrag_on_unknown_media => Some(
                "could not confirm this is a spinning disk (USB enclosures \
                 often hide it); pass allow_defrag_on_unknown_media to \
                 override"
                    .to_string(),
            ),
            _ => None,
        };
        if let Some(why) = refuse {
            return Ok(OptimizeResult {
                volume: info.volume.clone(),
                media: info.kind,
                op: chosen,
                command: String::new(),
                ran: false,
                exit_code: None,
                output: String::new(),
                elapsed: Duration::ZERO,
                refused: Some(why),
            });
        }
    }

    let (program, args) = command_for(&info.volume, chosen, opts);
    let command = format!("{program} {}", args.join(" "));

    if opts.dry_run || !chosen.modifies_the_drive() && chosen != OptimizeOp::AnalyzeOnly {
        return Ok(OptimizeResult {
            volume: info.volume.clone(),
            media: info.kind,
            op: chosen,
            command,
            ran: false,
            exit_code: None,
            output: String::new(),
            elapsed: Duration::ZERO,
            refused: None,
        });
    }

    // Every one of these needs Administrator; failing early with a clear
    // message beats a cryptic "Access is denied" from the child process.
    crate::file::sys::require_elevation(&format!("optimizing {}", info.volume))?;

    let started = Instant::now();
    let (exit_code, output) = spawn_and_wait(&program, &args, opts, cancel)?;

    Ok(OptimizeResult {
        volume: info.volume.clone(),
        media: info.kind,
        op: chosen,
        command,
        ran: true,
        exit_code,
        output,
        elapsed: started.elapsed(),
        refused: None,
    })
}

/// Build the command line for an operation.
#[cfg(windows)]
fn command_for(volume: &str, op: OptimizeOp, opts: &OptimizeOptions) -> (String, Vec<String>) {
    let target = media::volume_root(volume)
        .trim_end_matches('\\')
        .to_string();
    let mut args = vec![target];
    match op {
        // /D is the traditional file-consolidation defrag.
        OptimizeOp::Defrag => args.push("/D".into()),
        // /L re-issues TRIM for all free space.
        OptimizeOp::Retrim => args.push("/L".into()),
        // /O lets Windows apply the right operation for the media itself.
        OptimizeOp::Auto => args.push("/O".into()),
        // /A analyzes and reports without moving anything.
        OptimizeOp::AnalyzeOnly => args.push("/A".into()),
    }
    args.push("/V".into()); // verbose, so the output is worth keeping
    if opts.low_priority && op != OptimizeOp::AnalyzeOnly {
        // /H would run at *normal* priority; omitting it keeps the default
        // low priority, which is what a background maintenance job wants.
    }
    ("defrag.exe".into(), args)
}

/// The Linux equivalents.
///
/// `fstrim` is in util-linux and is the direct analogue of a Windows retrim.
/// `e4defrag` ships with e2fsprogs and is ext4-only; XFS and Btrfs have their
/// own tools, which is why the defrag path here is narrower than on Windows.
#[cfg(not(windows))]
fn command_for(volume: &str, op: OptimizeOp, _opts: &OptimizeOptions) -> (String, Vec<String>) {
    let target = volume.to_string();
    match op {
        OptimizeOp::Retrim | OptimizeOp::Auto => ("fstrim".into(), vec!["-v".into(), target]),
        OptimizeOp::Defrag => ("e4defrag".into(), vec![target]),
        OptimizeOp::AnalyzeOnly => ("e4defrag".into(), vec!["-c".into(), target]),
    }
}

/// Run the child, honouring cancellation and the timeout.
fn spawn_and_wait(
    program: &str,
    args: &[String],
    opts: &OptimizeOptions,
    cancel: &Cancel,
) -> Result<(Option<i32>, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            BkError::Other(anyhow::anyhow!(
                "could not start {program}: {e} — is it installed and on PATH?"
            ))
        })?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = child
                    .wait_with_output()
                    .map(collect_output)
                    .unwrap_or_default();
                return Ok((status.code(), out));
            }
            Ok(None) => {}
            Err(e) => return Err(BkError::Io(e)),
        }

        // A defrag can run for hours, so "stop" has to actually stop it.
        // Killing defrag.exe mid-pass is safe: every move it makes is a
        // completed NTFS transaction, so the volume is never left inconsistent.
        if cancel.is_cancelled() {
            let _ = child.kill();
            let out = child
                .wait_with_output()
                .map(collect_output)
                .unwrap_or_default();
            return Ok((None, format!("{out}\n[cancelled]")));
        }
        if let Some(limit) = opts.timeout {
            if started.elapsed() > limit {
                let _ = child.kill();
                let out = child
                    .wait_with_output()
                    .map(collect_output)
                    .unwrap_or_default();
                return Ok((None, format!("{out}\n[timed out after {limit:?}]")));
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn collect_output(o: std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr);
    if !err.trim().is_empty() {
        s.push('\n');
        s.push_str(&err);
    }
    s.trim().to_string()
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
            total_clusters: Some(1000),
            free_clusters: Some(500),
        }
    }

    #[test]
    fn defragmenting_an_ssd_is_refused_even_when_asked_for() {
        // Explicitly requesting it is not enough. The whole point of the
        // media detection is to stop this happening by accident, and an
        // operator typing the wrong flag is exactly that accident.
        let opts = OptimizeOptions {
            op: Some(OptimizeOp::Defrag),
            ..Default::default()
        };
        let r = run(&info(MediaKind::Ssd, true), &opts, &Cancel::none()).unwrap();
        assert!(!r.ran);
        assert!(r.refused.unwrap().contains("solid-state"));
    }

    #[test]
    fn defragmenting_unidentified_media_is_refused_unless_overridden() {
        let opts = OptimizeOptions {
            op: Some(OptimizeOp::Defrag),
            ..Default::default()
        };
        let r = run(&info(MediaKind::Unknown, false), &opts, &Cancel::none()).unwrap();
        assert!(r.refused.is_some(), "should decline without confirmation");

        let forced = OptimizeOptions {
            op: Some(OptimizeOp::Defrag),
            allow_defrag_on_unknown_media: true,
            dry_run: true,
            ..Default::default()
        };
        let r = run(&info(MediaKind::Unknown, false), &forced, &Cancel::none()).unwrap();
        assert!(
            r.refused.is_none(),
            "an explicit override should be honoured"
        );
    }

    #[test]
    fn a_dry_run_reports_the_command_without_executing_it() {
        let opts = OptimizeOptions {
            dry_run: true,
            ..Default::default()
        };
        let r = run(&info(MediaKind::Hdd, false), &opts, &Cancel::none()).unwrap();
        assert!(!r.ran);
        assert_eq!(r.op, OptimizeOp::Defrag);
        assert!(r.succeeded());
        #[cfg(windows)]
        {
            assert!(r.command.contains("defrag.exe"), "{}", r.command);
            assert!(r.command.contains("D:"), "{}", r.command);
            assert!(r.command.contains("/D"), "{}", r.command);
        }
    }

    #[test]
    fn the_operation_follows_the_media_when_not_forced() {
        let opts = OptimizeOptions {
            dry_run: true,
            ..Default::default()
        };
        let ssd = run(&info(MediaKind::Ssd, true), &opts, &Cancel::none()).unwrap();
        assert_eq!(ssd.op, OptimizeOp::Retrim);
        #[cfg(windows)]
        assert!(ssd.command.contains("/L"), "{}", ssd.command);

        let hdd = run(&info(MediaKind::Hdd, false), &opts, &Cancel::none()).unwrap();
        assert_eq!(hdd.op, OptimizeOp::Defrag);

        let unknown = run(&info(MediaKind::Unknown, true), &opts, &Cancel::none()).unwrap();
        assert_eq!(unknown.op, OptimizeOp::Auto, "let Windows decide");
        #[cfg(windows)]
        assert!(unknown.command.contains("/O"), "{}", unknown.command);
    }

    #[test]
    fn an_ssd_without_trim_has_nothing_safe_to_do() {
        let opts = OptimizeOptions {
            dry_run: true,
            ..Default::default()
        };
        let r = run(&info(MediaKind::Ssd, false), &opts, &Cancel::none()).unwrap();
        assert_eq!(r.op, OptimizeOp::AnalyzeOnly);
        assert!(r.succeeded());
    }

    #[cfg(windows)]
    #[test]
    fn command_lines_are_built_for_every_operation() {
        let o = OptimizeOptions::default();
        for (op, flag) in [
            (OptimizeOp::Defrag, "/D"),
            (OptimizeOp::Retrim, "/L"),
            (OptimizeOp::Auto, "/O"),
            (OptimizeOp::AnalyzeOnly, "/A"),
        ] {
            let (prog, args) = command_for("D:", op, &o);
            assert_eq!(prog, "defrag.exe");
            assert_eq!(args[0], "D:", "the drive comes first");
            assert!(
                args.contains(&flag.to_string()),
                "{op:?} should pass {flag}"
            );
        }
    }
}
