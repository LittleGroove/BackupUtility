//! Registering jobs to run unattended.
//!
//! ## Why XML rather than `schtasks` flags
//! The obvious approach — `schtasks /Create /SC DAILY /ST 03:00` — cannot
//! express the settings that decide whether an overnight backup is actually
//! useful:
//!
//! * **`StartWhenAvailable`** — run the job late if the machine was off at 3am.
//!   Without it, a server that was down overnight simply skips that night's
//!   backup and nobody finds out until they need it.
//! * **`DisallowStartIfOnBatteries` / `StopIfGoingOnBatteries`** — both default
//!   to *true*, so on a laptop a scheduled backup silently never runs, and one
//!   already running is killed the moment the charger comes out.
//! * **`ExecutionTimeLimit`** — defaults to 72 hours; a multi-terabyte mirror
//!   should not be shot in the head partway.
//! * **`MultipleInstancesPolicy`** — stop a slow nightly run and the next one
//!   colliding on the same destination.
//!
//! All of those need `/Create /XML`, so this module builds the task document
//! itself. The format is Task Scheduler schema 1.2, which every Windows 10 and
//! 11 build accepts.
//!
//! ## Elevation
//! `RunLevel=HighestAvailable` plus a SYSTEM principal means the task runs
//! elevated at its trigger time with **no interactive UAC prompt** — which is
//! the whole point of a 3am backup. Registering the task itself needs
//! Administrator; running it afterwards does not.

use crate::job::{Frequency, Job, Schedule};
use crate::{BkError, Result};
use std::path::Path;
#[cfg(not(windows))]
use std::path::PathBuf;
use std::process::Command;

/// The well-known SID for `NT AUTHORITY\SYSTEM`. Used rather than the literal
/// name because that name is localized.
const SYSTEM_SID: &str = "S-1-5-18";

/// Build the command line a scheduled task runs.
pub fn action_for(job: &Job, engine_exe: &Path) -> (String, String) {
    (
        engine_exe.to_string_lossy().into_owned(),
        format!("run --job \"{}\"", job.name),
    )
}

// ===========================================================================
// Task XML
// ===========================================================================

/// Build the Task Scheduler XML for a job.
///
/// Pure and deterministic, so the exact document that will be registered can
/// be inspected and tested without touching the system.
pub fn task_xml(job: &Job, engine_exe: &Path, jobs_dir: Option<&Path>) -> Result<String> {
    let sched = job.schedule.as_ref().ok_or_else(|| {
        BkError::Other(anyhow::anyhow!("job '{}' has no schedule", job.name))
    })?;

    let (command, mut args) = action_for(job, engine_exe);
    if let Some(dir) = jobs_dir {
        args.push_str(&format!(" --jobs-dir \"{}\"", dir.display()));
    }

    let principal = if sched.run_as_system {
        format!("    <UserId>{SYSTEM_SID}</UserId>\n    <RunLevel>HighestAvailable</RunLevel>")
    } else {
        // InteractiveToken means the task only fires while the registering
        // user is logged on. That is a real limitation on a server, which is
        // why `run_as_system` is the recommended setting.
        "    <LogonType>InteractiveToken</LogonType>\n    <RunLevel>HighestAvailable</RunLevel>"
            .to_string()
    };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>BackupUtility</Author>
    <Description>{description}</Description>
    <URI>\{uri}</URI>
  </RegistrationInfo>
  <Triggers>
{triggers}
  </Triggers>
  <Principals>
    <Principal id="Author">
{principal}
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>{no_battery_start}</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>{stop_on_battery}</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>{catch_up}</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>{enabled}</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        description = escape(
            job.description
                .as_deref()
                .unwrap_or("Scheduled backup (BackupUtility)")
        ),
        uri = escape(&job.task_name()),
        triggers = triggers_xml(sched)?,
        principal = principal,
        // Both of these default to *true* in Task Scheduler, which silently
        // prevents a laptop from ever backing itself up.
        no_battery_start = sched.only_on_ac_power,
        stop_on_battery = sched.only_on_ac_power,
        catch_up = sched.run_if_missed,
        enabled = !job.disabled,
        command = escape(&command),
        args = escape(&args),
    );
    Ok(xml)
}

fn triggers_xml(s: &Schedule) -> Result<String> {
    // A start boundary in the past is legal and makes the document
    // deterministic: Task Scheduler computes the next occurrence from it
    // rather than us having to bake in today's date.
    let boundary = format!("2020-01-01T{}:00", time_or_default(&s.at));

    Ok(match s.frequency {
        Frequency::Hourly => format!(
            r#"    <CalendarTrigger>
      <StartBoundary>{boundary}</StartBoundary>
      <Enabled>true</Enabled>
      <Repetition>
        <Interval>PT1H</Interval>
        <Duration>P1D</Duration>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>"#
        ),
        Frequency::Daily => format!(
            r#"    <CalendarTrigger>
      <StartBoundary>{boundary}</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>"#
        ),
        Frequency::Weekly => {
            let day = weekday_element(s.day.as_deref().unwrap_or("SUN"))?;
            format!(
                r#"    <CalendarTrigger>
      <StartBoundary>{boundary}</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByWeek>
        <DaysOfWeek>
          <{day} />
        </DaysOfWeek>
        <WeeksInterval>1</WeeksInterval>
      </ScheduleByWeek>
    </CalendarTrigger>"#
            )
        }
        Frequency::OnLogon => r#"    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>"#
            .to_string(),
        Frequency::OnStart => r#"    <BootTrigger>
      <Enabled>true</Enabled>
      <Delay>PT2M</Delay>
    </BootTrigger>"#
            .to_string(),
    })
}

fn time_or_default(at: &str) -> String {
    let ok = at.len() == 5
        && at.as_bytes().get(2) == Some(&b':')
        && at[..2].parse::<u32>().is_ok_and(|h| h < 24)
        && at[3..].parse::<u32>().is_ok_and(|m| m < 60);
    if ok {
        at.to_string()
    } else {
        "03:00".to_string()
    }
}

fn weekday_element(day: &str) -> Result<&'static str> {
    Ok(match day.trim().to_ascii_uppercase().as_str() {
        "SUN" | "SUNDAY" => "Sunday",
        "MON" | "MONDAY" => "Monday",
        "TUE" | "TUESDAY" => "Tuesday",
        "WED" | "WEDNESDAY" => "Wednesday",
        "THU" | "THURSDAY" => "Thursday",
        "FRI" | "FRIDAY" => "Friday",
        "SAT" | "SATURDAY" => "Saturday",
        other => {
            return Err(BkError::Other(anyhow::anyhow!(
                "'{other}' is not a day of the week"
            )))
        }
    })
}

/// Escape text for inclusion in XML content.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ===========================================================================
// Windows registration
// ===========================================================================

/// Register (or replace) the scheduled task for a job.
///
/// A job with no schedule has any stale task removed instead, so turning a
/// schedule off in the GUI actually stops the job running.
#[cfg(windows)]
pub fn register(job: &Job, engine_exe: &Path, jobs_dir: Option<&Path>) -> Result<()> {
    if job.schedule.is_none() {
        let _ = unregister(job);
        return Ok(());
    }
    crate::file::sys::require_elevation("registering a scheduled task")?;

    let xml = task_xml(job, engine_exe, jobs_dir)?;

    // `schtasks /XML` wants a file, and is happiest with UTF-16LE + BOM —
    // which is also what the `encoding="UTF-16"` declaration promises.
    let tmp = std::env::temp_dir().join(format!("bku_task_{}.xml", sanitize(&job.name)));
    std::fs::write(&tmp, utf16le_with_bom(&xml))?;

    let out = Command::new("schtasks")
        .args([
            "/Create",
            "/TN",
            &job.task_name(),
            "/XML",
            &tmp.to_string_lossy(),
            "/F",
        ])
        .output()?;
    let _ = std::fs::remove_file(&tmp);

    if !out.status.success() {
        return Err(BkError::Other(anyhow::anyhow!(
            "schtasks /Create failed for '{}': {}{}",
            job.name,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Encode as UTF-16LE with a byte-order mark.
fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in s.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Remove a job's scheduled task. Absent is success.
#[cfg(windows)]
pub fn unregister(job: &Job) -> Result<()> {
    let _ = Command::new("schtasks")
        .args(["/Delete", "/TN", &job.task_name(), "/F"])
        .output()?;
    Ok(())
}

/// Trigger a registered task immediately.
#[cfg(windows)]
pub fn run_now(job: &Job) -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/Run", "/TN", &job.task_name()])
        .output()?;
    if !out.status.success() {
        return Err(BkError::Other(anyhow::anyhow!(
            "schtasks /Run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Is a task registered for this job?
#[cfg(windows)]
pub fn is_registered(job: &Job) -> bool {
    Command::new("schtasks")
        .args(["/Query", "/TN", &job.task_name()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ===========================================================================
// Linux
// ===========================================================================

/// The systemd service unit for a job.
///
/// systemd is the right target rather than cron: it gives us the same
/// properties the Windows XML does — `Persistent=true` is `StartWhenAvailable`,
/// and the service can be niced and I/O-throttled so a nightly mirror does not
/// make the machine unusable.
pub fn systemd_service(job: &Job, engine_exe: &Path, jobs_dir: Option<&Path>) -> String {
    let (command, mut args) = action_for(job, engine_exe);
    if let Some(dir) = jobs_dir {
        args.push_str(&format!(" --jobs-dir \"{}\"", dir.display()));
    }
    format!(
        "[Unit]\n\
         Description=BackupUtility job {name}\n\
         After=local-fs.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={command} {args}\n\
         Nice=10\n\
         IOSchedulingClass=idle\n\
         # Raw device access and reading other users' files both need root.\n\
         User=root\n",
        name = job.name,
        command = command,
        args = args,
    )
}

/// The systemd timer unit for a job.
pub fn systemd_timer(job: &Job) -> Result<String> {
    let sched = job.schedule.as_ref().ok_or_else(|| {
        BkError::Other(anyhow::anyhow!("job '{}' has no schedule", job.name))
    })?;
    let on_calendar = match sched.frequency {
        Frequency::Hourly => "hourly".to_string(),
        Frequency::Daily => format!("*-*-* {}:00", time_or_default(&sched.at)),
        Frequency::Weekly => format!(
            "{} *-*-* {}:00",
            &weekday_element(sched.day.as_deref().unwrap_or("SUN"))?[..3],
            time_or_default(&sched.at)
        ),
        Frequency::OnStart | Frequency::OnLogon => "".to_string(),
    };

    let trigger = if on_calendar.is_empty() {
        // A boot trigger is a delay after startup rather than a calendar time.
        "OnBootSec=2min\n".to_string()
    } else {
        format!("OnCalendar={on_calendar}\n")
    };

    Ok(format!(
        "[Unit]\n\
         Description=BackupUtility timer for {name}\n\
         \n\
         [Timer]\n\
         {trigger}\
         # Run a missed job once the machine is back, rather than skipping it.\n\
         Persistent=true\n\
         # Spread load if several jobs share a trigger time.\n\
         RandomizedDelaySec=5min\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
        name = job.name,
        trigger = trigger,
    ))
}

/// The crontab line for a job, for systems without systemd.
pub fn crontab_line(job: &Job, engine_exe: &Path) -> Result<String> {
    let sched = job.schedule.as_ref().ok_or_else(|| {
        BkError::Other(anyhow::anyhow!("job '{}' has no schedule", job.name))
    })?;
    let at = time_or_default(&sched.at);
    let (h, m) = at.split_once(':').unwrap_or(("3", "00"));
    let (h, m) = (h.trim_start_matches('0'), m.trim_start_matches('0'));
    let (h, m) = (if h.is_empty() { "0" } else { h }, if m.is_empty() { "0" } else { m });

    let spec = match sched.frequency {
        Frequency::Hourly => format!("{m} * * * *"),
        Frequency::Daily => format!("{m} {h} * * *"),
        Frequency::Weekly => {
            let dow = match sched.day.as_deref().unwrap_or("SUN").to_ascii_uppercase().as_str() {
                s if s.starts_with("SUN") => 0,
                s if s.starts_with("MON") => 1,
                s if s.starts_with("TUE") => 2,
                s if s.starts_with("WED") => 3,
                s if s.starts_with("THU") => 4,
                s if s.starts_with("FRI") => 5,
                _ => 6,
            };
            format!("{m} {h} * * {dow}")
        }
        Frequency::OnStart | Frequency::OnLogon => "@reboot".to_string(),
    };
    let (command, args) = action_for(job, engine_exe);
    Ok(format!("{spec} {command} {args}\n"))
}

/// Write the unit files and enable the timer.
#[cfg(not(windows))]
pub fn register(job: &Job, engine_exe: &Path, jobs_dir: Option<&Path>) -> Result<()> {
    if job.schedule.is_none() {
        let _ = unregister(job);
        return Ok(());
    }
    crate::file::sys::require_elevation("installing a systemd timer")?;

    let unit_dir = PathBuf::from("/etc/systemd/system");
    let stem = format!("bku-{}", sanitize(&job.name));
    std::fs::write(
        unit_dir.join(format!("{stem}.service")),
        systemd_service(job, engine_exe, jobs_dir),
    )?;
    std::fs::write(unit_dir.join(format!("{stem}.timer")), systemd_timer(job)?)?;

    let _ = Command::new("systemctl").arg("daemon-reload").output()?;
    let out = Command::new("systemctl")
        .args(["enable", "--now", &format!("{stem}.timer")])
        .output()?;
    if !out.status.success() {
        return Err(BkError::Other(anyhow::anyhow!(
            "systemctl enable failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn unregister(job: &Job) -> Result<()> {
    let stem = format!("bku-{}", sanitize(&job.name));
    let _ = Command::new("systemctl")
        .args(["disable", "--now", &format!("{stem}.timer")])
        .output();
    for ext in ["service", "timer"] {
        let _ = std::fs::remove_file(format!("/etc/systemd/system/{stem}.{ext}"));
    }
    let _ = Command::new("systemctl").arg("daemon-reload").output();
    Ok(())
}

#[cfg(not(windows))]
pub fn run_now(job: &Job) -> Result<()> {
    let stem = format!("bku-{}", sanitize(&job.name));
    let out = Command::new("systemctl")
        .args(["start", &format!("{stem}.service")])
        .output()?;
    if !out.status.success() {
        return Err(BkError::Other(anyhow::anyhow!(
            "systemctl start failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn is_registered(job: &Job) -> bool {
    let stem = format!("bku-{}", sanitize(&job.name));
    PathBuf::from(format!("/etc/systemd/system/{stem}.timer")).exists()
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Frequency;

    fn scheduled(freq: Frequency) -> Job {
        let mut job = Job::mirror("nightly", r"D:\", r"E:\");
        job.schedule = Some(Schedule {
            frequency: freq,
            at: "03:30".into(),
            day: Some("SUN".into()),
            run_as_system: true,
            run_if_missed: true,
            only_on_ac_power: false,
        });
        job
    }

    fn xml_of(job: &Job) -> String {
        task_xml(job, Path::new(r"C:\Tools\bkengine.exe"), None).unwrap()
    }

    #[test]
    fn the_task_runs_elevated_as_system_without_a_uac_prompt() {
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("<RunLevel>HighestAvailable</RunLevel>"), "{x}");
        assert!(x.contains(SYSTEM_SID), "should run as SYSTEM");
    }

    #[test]
    fn a_missed_run_is_caught_up_rather_than_skipped() {
        // Without StartWhenAvailable, a server that was off at 3am silently
        // misses that night's backup entirely.
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("<StartWhenAvailable>true</StartWhenAvailable>"), "{x}");
    }

    #[test]
    fn battery_settings_default_to_letting_the_job_run() {
        // Task Scheduler defaults both of these to true, which stops a laptop
        // ever backing itself up and kills a running job when unplugged.
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(x.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));

        let mut job = scheduled(Frequency::Daily);
        job.schedule.as_mut().unwrap().only_on_ac_power = true;
        let x = xml_of(&job);
        assert!(x.contains("<DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries>"));
    }

    #[test]
    fn a_long_mirror_is_not_killed_by_the_default_time_limit() {
        // PT0S means "no limit"; the Task Scheduler default is 72 hours.
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"), "{x}");
    }

    #[test]
    fn a_slow_run_does_not_collide_with_the_next_trigger() {
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
    }

    #[test]
    fn each_frequency_produces_the_right_trigger() {
        assert!(xml_of(&scheduled(Frequency::Daily)).contains("<ScheduleByDay>"));
        assert!(xml_of(&scheduled(Frequency::Weekly)).contains("<Sunday />"));
        assert!(xml_of(&scheduled(Frequency::Hourly)).contains("<Interval>PT1H</Interval>"));
        assert!(xml_of(&scheduled(Frequency::OnStart)).contains("<BootTrigger>"));
        assert!(xml_of(&scheduled(Frequency::OnLogon)).contains("<LogonTrigger>"));
    }

    #[test]
    fn the_scheduled_time_lands_in_the_start_boundary() {
        let x = xml_of(&scheduled(Frequency::Daily));
        assert!(x.contains("T03:30:00"), "{x}");
    }

    #[test]
    fn a_nonsense_time_falls_back_rather_than_producing_invalid_xml() {
        // Validation catches this earlier, but a job file edited by hand can
        // still reach here, and an unparseable task is worse than a 3am one.
        let mut job = scheduled(Frequency::Daily);
        job.schedule.as_mut().unwrap().at = "tea time".into();
        assert!(xml_of(&job).contains("T03:00:00"));
        assert_eq!(time_or_default("24:00"), "03:00");
        assert_eq!(time_or_default("07:45"), "07:45");
    }

    #[test]
    fn a_disabled_job_registers_a_disabled_task() {
        let mut job = scheduled(Frequency::Daily);
        job.disabled = true;
        assert!(xml_of(&job).contains("<Enabled>false</Enabled>"));
    }

    #[test]
    fn xml_special_characters_in_paths_are_escaped() {
        // A path with an ampersand would otherwise produce a document Task
        // Scheduler rejects outright.
        let mut job = scheduled(Frequency::Daily);
        job.name = "R&D <nightly>".into();
        job.description = Some("Backs up \"R&D\"".into());
        let x = task_xml(&job, Path::new(r"C:\Program Files\A&B\bkengine.exe"), None).unwrap();
        assert!(x.contains("A&amp;B"), "{x}");
        assert!(x.contains("R&amp;D &lt;nightly&gt;"), "{x}");
        assert!(!x.contains("A&B"));
    }

    #[test]
    fn the_action_invokes_the_engine_with_the_job_name() {
        let job = scheduled(Frequency::Daily);
        let x = task_xml(
            &job,
            Path::new(r"C:\Tools\bkengine.exe"),
            Some(Path::new(r"C:\ProgramData\BackupUtility\jobs")),
        )
        .unwrap();
        assert!(x.contains(r"C:\Tools\bkengine.exe"), "{x}");
        assert!(x.contains("run --job"), "{x}");
        assert!(x.contains("--jobs-dir"), "{x}");
    }

    #[test]
    fn an_unscheduled_job_has_no_xml_to_produce() {
        let job = Job::mirror("manual", r"D:\", r"E:\");
        assert!(task_xml(&job, Path::new("x"), None).is_err());
    }

    #[test]
    fn the_utf16_encoding_matches_what_the_declaration_promises() {
        let bytes = utf16le_with_bom("<a/>");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "little-endian BOM");
        assert_eq!(&bytes[2..4], &[b'<', 0]);
        assert_eq!(bytes.len(), 2 + 4 * 2);
    }

    #[test]
    fn systemd_units_carry_the_same_guarantees_as_the_windows_task() {
        let job = scheduled(Frequency::Daily);
        let timer = systemd_timer(&job).unwrap();
        // Persistent=true is the systemd spelling of StartWhenAvailable.
        assert!(timer.contains("Persistent=true"), "{timer}");
        assert!(timer.contains("OnCalendar=*-*-* 03:30:00"), "{timer}");

        let service = systemd_service(&job, Path::new("/usr/local/bin/bkengine"), None);
        assert!(service.contains("ExecStart=/usr/local/bin/bkengine run --job"));
        assert!(service.contains("User=root"));
        assert!(service.contains("IOSchedulingClass=idle"));
    }

    #[test]
    fn systemd_covers_every_frequency() {
        assert!(systemd_timer(&scheduled(Frequency::Hourly)).unwrap().contains("OnCalendar=hourly"));
        assert!(systemd_timer(&scheduled(Frequency::Weekly)).unwrap().contains("Sun *-*-* 03:30"));
        assert!(systemd_timer(&scheduled(Frequency::OnStart)).unwrap().contains("OnBootSec=2min"));
    }

    #[test]
    fn crontab_lines_are_well_formed() {
        let engine = Path::new("/usr/local/bin/bkengine");
        let daily = crontab_line(&scheduled(Frequency::Daily), engine).unwrap();
        assert!(daily.starts_with("30 3 * * * "), "{daily}");

        let weekly = crontab_line(&scheduled(Frequency::Weekly), engine).unwrap();
        assert!(weekly.starts_with("30 3 * * 0 "), "{weekly}");

        let hourly = crontab_line(&scheduled(Frequency::Hourly), engine).unwrap();
        assert!(hourly.starts_with("30 * * * * "), "{hourly}");

        let boot = crontab_line(&scheduled(Frequency::OnStart), engine).unwrap();
        assert!(boot.starts_with("@reboot "), "{boot}");
    }

    #[test]
    fn unknown_weekdays_are_rejected_rather_than_silently_becoming_sunday() {
        let mut job = scheduled(Frequency::Weekly);
        job.schedule.as_mut().unwrap().day = Some("Fooday".into());
        assert!(task_xml(&job, Path::new("x"), None).is_err());

        assert_eq!(weekday_element("wed").unwrap(), "Wednesday");
        assert_eq!(weekday_element("SATURDAY").unwrap(), "Saturday");
    }
}
