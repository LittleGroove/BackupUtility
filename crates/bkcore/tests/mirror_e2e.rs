//! End-to-end tests for the file mirror: real trees, real disks, real
//! metadata.
//!
//! The unit tests prove the planner's logic in isolation. These prove the
//! whole pipeline — scan, plan, save state, copy, commit — actually produces a
//! destination that matches the source, and keeps matching across repeat runs.

use bkcore::file::attic::AtticPolicy;
use bkcore::file::copy::CopyOptions;
use bkcore::file::filter::Filter;
use bkcore::file::mirror::{self, MirrorJob};
use bkcore::file::plan::{CompareBy, MirrorMode};
use bkcore::file::state::{RunStatus, SaveState};
use bkcore::file::Cancel;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Set a file's modification time. `File::open` gives a read-only handle,
/// which Windows refuses to let us stamp, so this opens for write.
fn set_mtime(p: &Path, when: std::time::SystemTime) {
    fs::OpenOptions::new()
        .write(true)
        .open(p)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

/// A disposable directory tree.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("bku_e2e_{name}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Sandbox { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        p
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.root.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Every file under `root`, as `relative path -> contents`. The comparison
/// used to prove two trees are the same.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            // Our own bookkeeping is not part of the mirrored data.
            if rel.starts_with(".bku-") {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => {
                    out.insert(format!("{rel}/"), Vec::new());
                    stack.push(p);
                }
                Ok(_) => {
                    out.insert(rel, fs::read(&p).unwrap_or_default());
                }
                Err(_) => {}
            }
        }
    }
    out
}

/// A job wired for testing: temp state root, deterministic worker count.
fn job(name: &str, src: &Path, dst: &Path, state: &Path) -> MirrorJob {
    MirrorJob {
        workers: 2,
        state_root: state.to_path_buf(),
        filter: Filter::permissive(),
        // Verify every byte we write: these tests are about correctness, not
        // throughput, and a silent corruption would otherwise pass.
        copy: CopyOptions {
            verify: true,
            ..CopyOptions::default()
        },
        ..MirrorJob::new(name, src, dst)
    }
}

fn run(j: &MirrorJob) -> bkcore::file::mirror::MirrorReport {
    mirror::run(j, &Cancel::none(), &mut |_| {}).expect("mirror run failed")
}

/// Build a representative source tree: nested directories, a range of file
/// sizes including empty and multi-buffer, and awkward names.
fn populate(src: &Sandbox) {
    src.write("readme.txt", b"top level");
    src.write("empty.bin", b"");
    src.write("docs/notes.md", b"# notes\n");
    src.write("docs/deep/nested/thing.txt", b"buried");
    src.mkdir("emptydir");
    src.write("data/big.bin", &vec![0xABu8; 5 * 1024 * 1024]);
    src.write("data/name with spaces.txt", b"spaces are legal");
    src.write("data/unicode-\u{00e9}\u{00e8}.txt", "accented".as_bytes());
}

#[test]
fn a_full_mirror_reproduces_the_source_tree() {
    let src = Sandbox::new("full_src");
    let dst = Sandbox::new("full_dst");
    let st = Sandbox::new("full_state");
    populate(&src);

    let report = run(&job("full", &src.root, &dst.root, &st.root));

    assert!(report.succeeded(), "{}", report.summary());
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(report.actions_failed, 0);
    assert_eq!(
        snapshot(&src.root),
        snapshot(&dst.root),
        "the destination should be an exact copy of the source"
    );
    // The 5 MiB file alone should dominate the byte count.
    assert!(report.bytes_copied >= 5 * 1024 * 1024);
    assert_eq!(report.files_copied, 7);
}

#[test]
fn a_second_run_copies_nothing() {
    // The property that makes a nightly job viable: an unchanged source must
    // cost a scan, not a re-transfer.
    let src = Sandbox::new("delta_src");
    let dst = Sandbox::new("delta_dst");
    let st = Sandbox::new("delta_state");
    populate(&src);
    let j = job("delta", &src.root, &dst.root, &st.root);

    let first = run(&j);
    assert!(first.bytes_copied > 5 * 1024 * 1024);

    let second = run(&j);
    assert_eq!(second.bytes_copied, 0, "{}", second.summary());
    assert_eq!(second.files_copied, 0);
    assert_eq!(second.plan_stats.files_to_copy, 0);
    assert_eq!(second.plan_stats.files_to_update, 0);
    assert!(second.plan_stats.unchanged >= 7);
    assert_eq!(snapshot(&src.root), snapshot(&dst.root));
}

#[test]
fn only_the_changed_file_is_transferred() {
    let src = Sandbox::new("chg_src");
    let dst = Sandbox::new("chg_dst");
    let st = Sandbox::new("chg_state");
    populate(&src);
    let j = job("chg", &src.root, &dst.root, &st.root);
    run(&j);

    src.write("docs/notes.md", b"# notes\nrewritten, and longer than before\n");
    let second = run(&j);

    assert_eq!(second.plan_stats.files_to_update, 1);
    assert_eq!(second.plan_stats.files_to_copy, 0);
    assert!(
        second.bytes_copied < 1000,
        "only the small changed file should move, got {} bytes",
        second.bytes_copied
    );
    assert_eq!(snapshot(&src.root), snapshot(&dst.root));
}

#[test]
fn a_new_file_is_picked_up_on_the_next_run() {
    let src = Sandbox::new("add_src");
    let dst = Sandbox::new("add_dst");
    let st = Sandbox::new("add_state");
    populate(&src);
    let j = job("add", &src.root, &dst.root, &st.root);
    run(&j);

    src.write("docs/brand-new.txt", b"added later");
    let second = run(&j);

    assert_eq!(second.plan_stats.files_to_copy, 1);
    assert_eq!(
        fs::read(dst.path("docs/brand-new.txt")).unwrap(),
        b"added later"
    );
}

#[test]
fn additive_mode_keeps_files_the_source_deleted() {
    let src = Sandbox::new("keep_src");
    let dst = Sandbox::new("keep_dst");
    let st = Sandbox::new("keep_state");
    populate(&src);
    let j = job("keep", &src.root, &dst.root, &st.root);
    run(&j);

    fs::remove_file(src.path("readme.txt")).unwrap();
    let second = run(&j);

    assert_eq!(second.plan_stats.to_remove, 0);
    assert!(
        dst.path("readme.txt").exists(),
        "additive mode must never remove anything"
    );
}

#[test]
fn delete_mode_removes_files_the_source_deleted() {
    let src = Sandbox::new("del_src");
    let dst = Sandbox::new("del_dst");
    let st = Sandbox::new("del_state");
    populate(&src);
    let mut j = job("del", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Delete;
    run(&j);

    fs::remove_file(src.path("readme.txt")).unwrap();
    fs::remove_dir_all(src.path("docs/deep")).unwrap();
    let second = run(&j);

    assert!(!dst.path("readme.txt").exists());
    assert!(!dst.path("docs/deep").exists(), "and the directory too");
    assert!(second.plan_stats.to_remove >= 3);
    assert_eq!(
        snapshot(&src.root),
        snapshot(&dst.root),
        "delete mode should leave the two trees identical"
    );
}

#[test]
fn attic_mode_preserves_what_it_displaces() {
    let src = Sandbox::new("attic_src");
    let dst = Sandbox::new("attic_dst");
    let st = Sandbox::new("attic_state");
    populate(&src);
    let mut j = job("attic", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Attic;
    run(&j);

    fs::remove_file(src.path("readme.txt")).unwrap();
    let second = run(&j);

    assert!(
        !dst.path("readme.txt").exists(),
        "it should be gone from the mirror proper"
    );
    let attic = second.attic.expect("attic mode should report a folder");
    assert_eq!(
        fs::read(attic.join("readme.txt")).unwrap(),
        b"top level",
        "but fully recoverable from the attic"
    );
    assert_eq!(second.plan_stats.to_attic, 1);
}

#[test]
fn a_dry_run_changes_nothing_but_reports_everything() {
    let src = Sandbox::new("dry_src");
    let dst = Sandbox::new("dry_dst");
    let st = Sandbox::new("dry_state");
    populate(&src);
    let mut j = job("dry", &src.root, &dst.root, &st.root);
    j.dry_run = true;

    let report = run(&j);

    assert!(report.dry_run);
    assert_eq!(report.plan_stats.files_to_copy, 7);
    assert!(report.plan_stats.bytes_to_copy > 5 * 1024 * 1024);
    assert_eq!(report.bytes_copied, 0);
    assert!(
        snapshot(&dst.root).is_empty(),
        "a dry run must not write a single file"
    );
}

#[test]
fn cancelling_during_the_scan_leaves_nothing_to_resume() {
    // There is no plan yet, so there is nothing a later run could pick up.
    // The correct outcome is a clean cancellation, not a half-saved state.
    let src = Sandbox::new("earlycancel_src");
    let dst = Sandbox::new("earlycancel_dst");
    let st = Sandbox::new("earlycancel_state");
    populate(&src);
    let j = job("earlycancel", &src.root, &dst.root, &st.root);

    let cancel = Cancel::new();
    cancel.cancel();
    let report = mirror::run(&j, &cancel, &mut |_| {}).unwrap();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(snapshot(&dst.root).is_empty());
    assert!(
        SaveState::load(&st.root, "earlycancel").unwrap().is_none(),
        "no plan was committed, so no state should exist"
    );
}

#[test]
fn an_interrupted_run_resumes_instead_of_restarting() {
    let src = Sandbox::new("resume_src");
    let dst = Sandbox::new("resume_dst");
    let st = Sandbox::new("resume_state");
    populate(&src);
    let j = job("resume", &src.root, &dst.root, &st.root);

    // Interrupt the moment the plan is committed but before work begins —
    // the same position a power cut early in a run would leave us in.
    let cancel = Cancel::new();
    let trigger = cancel.clone();
    let first = mirror::run(&j, &cancel, &mut |e| {
        if matches!(e, mirror::Event::Planned(_)) {
            trigger.cancel();
        }
    })
    .unwrap();
    assert_eq!(first.status, RunStatus::Cancelled);
    assert!(
        SaveState::load(&st.root, "resume").unwrap().is_some(),
        "the plan should have been committed before execution started"
    );

    // The next run picks the saved plan back up rather than starting over.
    let second = run(&j);
    assert!(second.resumed, "should have continued the interrupted run");
    assert!(second.succeeded(), "{}", second.summary());
    assert_eq!(snapshot(&src.root), snapshot(&dst.root));

    // A third run sees a finished job, so it plans afresh and finds no work.
    let third = run(&j);
    assert!(!third.resumed);
    assert_eq!(third.bytes_copied, 0);
}

#[test]
fn a_resumed_run_skips_the_actions_the_first_attempt_completed() {
    // The point of the save state is not to redo finished work. Proving that
    // needs a way to tell "skipped because the journal says so" apart from
    // "skipped because the file already matches", so this deletes files from
    // the destination after they were recorded as done: a resume must leave
    // them missing, because it believes they are already handled.
    let src = Sandbox::new("skip_src");
    let dst = Sandbox::new("skip_dst");
    let st = Sandbox::new("skip_state");
    populate(&src);
    let j = job("skip", &src.root, &dst.root, &st.root);

    run(&j);

    // Reopen the completed state and put it back into the interrupted state.
    let (mut state, _actions) = SaveState::load(&st.root, "skip").unwrap().unwrap();
    let done_before = state.done_count();
    assert!(done_before > 0);
    state.set_status(RunStatus::Running, 0).unwrap();
    drop(state);

    fs::remove_file(dst.path("readme.txt")).unwrap();
    let resumed = run(&j);

    assert!(resumed.resumed);
    assert_eq!(resumed.bytes_copied, 0, "every action was already journalled");
    assert!(
        !dst.path("readme.txt").exists(),
        "a resumed run must not redo work the journal says is finished"
    );

    // And once that run completes, a fresh plan notices the gap and repairs it.
    let repair = run(&j);
    assert!(!repair.resumed);
    assert_eq!(repair.plan_stats.files_to_copy, 1);
    assert_eq!(snapshot(&src.root), snapshot(&dst.root));
}

#[test]
fn content_comparison_skips_files_that_are_already_identical() {
    let src = Sandbox::new("hash_src");
    let dst = Sandbox::new("hash_dst");
    let st = Sandbox::new("hash_state");
    populate(&src);
    let mut j = job("hash", &src.root, &dst.root, &st.root);
    j.compare = CompareBy::Content;

    run(&j);
    let second = run(&j);

    // Content mode plans every file as a candidate, then proves by hashing
    // that nothing needs to move.
    assert!(second.plan_stats.files_to_update >= 7);
    assert_eq!(second.bytes_copied, 0, "{}", second.summary());
    assert!(second.content_matches >= 7);
}

#[test]
fn content_comparison_catches_a_same_size_same_time_edit() {
    // The case size+mtime comparison cannot see: a file rewritten in place
    // with identical length and its timestamp put back.
    let src = Sandbox::new("sneaky_src");
    let dst = Sandbox::new("sneaky_dst");
    let st = Sandbox::new("sneaky_state");
    src.write("ledger.dat", b"AAAAAAAAAA");
    let mut j = job("sneaky", &src.root, &dst.root, &st.root);
    run(&j);

    // Same length, and we restore the original timestamp.
    let before = fs::metadata(src.path("ledger.dat")).unwrap().modified().unwrap();
    src.write("ledger.dat", b"BBBBBBBBBB");
    set_mtime(&src.path("ledger.dat"), before);

    let quick = run(&j);
    assert_eq!(
        quick.plan_stats.unchanged, 1,
        "size+mtime cannot detect this, by design"
    );

    j.compare = CompareBy::Content;
    let thorough = run(&j);
    assert_eq!(thorough.files_copied, 1, "hashing does detect it");
    assert_eq!(fs::read(dst.path("ledger.dat")).unwrap(), b"BBBBBBBBBB");
}

#[test]
fn excluded_paths_are_neither_copied_nor_deleted_from_the_destination() {
    let src = Sandbox::new("excl_src");
    let dst = Sandbox::new("excl_dst");
    let st = Sandbox::new("excl_state");
    populate(&src);
    src.write("cache/huge.tmp", b"should not travel");

    let mut j = job("excl", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Delete;
    j.filter = Filter {
        exclude: vec!["cache/".into(), "*.tmp".into()],
        ..Filter::permissive()
    };
    // Something the destination has that the filter also excludes: delete mode
    // must not treat it as an "extra" and remove it, because the source scan
    // never looked for it.
    fs::create_dir_all(dst.path("cache")).unwrap();
    fs::write(dst.path("cache/local.tmp"), b"pre-existing").unwrap();

    run(&j);

    assert!(!dst.path("cache/huge.tmp").exists(), "excluded, so not copied");
    assert!(
        dst.path("cache/local.tmp").exists(),
        "excluded, so not deleted either"
    );
}

#[test]
fn deeply_nested_paths_past_max_path_are_handled() {
    let src = Sandbox::new("long_src");
    let dst = Sandbox::new("long_dst");
    let st = Sandbox::new("long_state");
    // ~40 segments of 20 chars is well past the 260-character MAX_PATH limit.
    let mut rel = String::new();
    for i in 0..40 {
        rel.push_str(&format!("directory_number_{i:02}/"));
    }
    src.write(&format!("{rel}buried.txt"), b"found me");

    let report = run(&job("long", &src.root, &dst.root, &st.root));

    assert!(report.succeeded(), "{}", report.summary());
    assert_eq!(
        fs::read(dst.path(&format!("{rel}buried.txt"))).unwrap(),
        b"found me"
    );
}

#[test]
fn timestamps_survive_the_mirror() {
    let src = Sandbox::new("ts_src");
    let dst = Sandbox::new("ts_dst");
    let st = Sandbox::new("ts_state");
    src.write("aged.txt", b"old file");
    // Backdate it so a copy that ignored timestamps would be obvious.
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    set_mtime(&src.path("aged.txt"), when);

    run(&job("ts", &src.root, &dst.root, &st.root));

    let got = fs::metadata(dst.path("aged.txt")).unwrap().modified().unwrap();
    let want = fs::metadata(src.path("aged.txt")).unwrap().modified().unwrap();
    assert_eq!(got, want, "the mirrored file should carry the source's mtime");
}

#[test]
fn a_destination_inside_the_source_is_refused() {
    // Without this guard the job copies the backup into itself, recursing
    // until the drive fills.
    let src = Sandbox::new("self_src");
    let st = Sandbox::new("self_state");
    src.mkdir("backup");
    let j = job("self", &src.root, &src.path("backup"), &st.root);

    let err = mirror::validate(&j).unwrap_err().to_string();
    assert!(err.contains("inside the source"), "{err}");
}

#[test]
fn a_source_inside_the_destination_is_refused() {
    let dst = Sandbox::new("rev_dst");
    let st = Sandbox::new("rev_state");
    dst.mkdir("data");
    let j = job("rev", &dst.path("data"), &dst.root, &st.root);

    let err = mirror::validate(&j).unwrap_err().to_string();
    assert!(err.contains("inside the destination"), "{err}");
}

#[test]
fn a_missing_source_fails_before_anything_is_written() {
    let dst = Sandbox::new("nosrc_dst");
    let st = Sandbox::new("nosrc_state");
    let missing = std::env::temp_dir().join("bku_e2e_definitely_missing");
    let _ = fs::remove_dir_all(&missing);
    let j = job("nosrc", &missing, &dst.root, &st.root);

    let err = mirror::run(&j, &Cancel::none(), &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains("does not exist"), "{err}");
}

#[test]
fn a_file_that_becomes_a_directory_is_replaced_correctly() {
    let src = Sandbox::new("kind_src");
    let dst = Sandbox::new("kind_dst");
    let st = Sandbox::new("kind_state");
    src.write("thing", b"i am a file");
    let mut j = job("kind", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Delete;
    run(&j);
    assert!(dst.path("thing").is_file());

    // Now the same name is a directory in the source.
    fs::remove_file(src.path("thing")).unwrap();
    src.write("thing/inner.txt", b"i am a directory now");

    let second = run(&j);
    assert!(second.succeeded(), "{}", second.summary());
    assert!(dst.path("thing").is_dir(), "the file should have been replaced");
    assert_eq!(
        fs::read(dst.path("thing/inner.txt")).unwrap(),
        b"i am a directory now"
    );
}

#[test]
fn progress_events_reach_the_caller() {
    let src = Sandbox::new("evt_src");
    let dst = Sandbox::new("evt_dst");
    let st = Sandbox::new("evt_state");
    populate(&src);

    let (mut scanned, mut planned, mut progressed) = (false, false, false);
    mirror::run(
        &job("evt", &src.root, &dst.root, &st.root),
        &Cancel::none(),
        &mut |e| match e {
            mirror::Event::ScanningSource(_) => scanned = true,
            mirror::Event::Planned(p) => {
                planned = true;
                assert!(p.len() > 0);
            }
            mirror::Event::Progress(p) => {
                progressed = true;
                assert!(p.percent() >= 0.0);
            }
            _ => {}
        },
    )
    .unwrap();

    assert!(scanned && planned && progressed, "a front-end needs all three");
}

#[cfg(windows)]
#[test]
fn readonly_and_hidden_files_mirror_and_re_mirror() {
    // Read-only files are the classic thing that breaks a mirror on the
    // *second* run, when it tries to overwrite what it faithfully copied.
    let src = Sandbox::new("ro_src");
    let dst = Sandbox::new("ro_dst");
    let st = Sandbox::new("ro_state");
    let f = src.write("locked.txt", b"v1");
    let mut perms = fs::metadata(&f).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&f, perms).unwrap();

    let j = job("ro", &src.root, &dst.root, &st.root);
    run(&j);
    assert!(
        fs::metadata(dst.path("locked.txt")).unwrap().permissions().readonly(),
        "the read-only attribute should have been mirrored"
    );

    // Change the source and re-run: overwriting the read-only copy must work.
    let mut perms = fs::metadata(&f).unwrap().permissions();
    perms.set_readonly(false);
    fs::set_permissions(&f, perms).unwrap();
    fs::write(&f, b"v2 is longer").unwrap();
    let mut perms = fs::metadata(&f).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&f, perms).unwrap();

    let second = run(&j);
    assert!(second.succeeded(), "{}", second.summary());
    assert_eq!(fs::read(dst.path("locked.txt")).unwrap(), b"v2 is longer");
}


// ===========================================================================
// Run logs, attic retention, free space, and verification
// ===========================================================================

#[test]
fn every_run_leaves_a_log_describing_what_it_did() {
    // A scheduled job writes to a console nobody watches. Without this file
    // there is no way to answer "what happened at 3am?".
    let src = Sandbox::new("log_src");
    let dst = Sandbox::new("log_dst");
    let st = Sandbox::new("log_state");
    populate(&src);

    let report = run(&job("logjob", &src.root, &dst.root, &st.root));

    let log_path = report.log_path.expect("a run should leave a log");
    assert!(log_path.exists(), "{}", log_path.display());
    let text = fs::read_to_string(&log_path).unwrap();

    assert!(text.contains("logjob"), "the log should name the job:\n{text}");
    assert!(text.contains("files to copy"), "and record the plan:\n{text}");
    assert!(text.contains("status"), "and the outcome:\n{text}");
    assert!(text.contains("finished"), "and when it ended:\n{text}");
}

#[test]
fn a_refused_run_still_explains_itself_in_the_log() {
    // The failure mode this guards: a misconfigured scheduled job that fails
    // instantly every night and leaves no trace of why.
    let dst = Sandbox::new("refuse_dst");
    let st = Sandbox::new("refuse_state");
    let missing = std::env::temp_dir().join("bku_e2e_refuse_missing_source");
    let _ = fs::remove_dir_all(&missing);

    let j = job("refused", &missing, &dst.root, &st.root);
    assert!(mirror::run(&j, &Cancel::none(), &mut |_| {}).is_err());

    let logs = bkcore::file::state::list_logs(&st.root, "refused").unwrap();
    assert_eq!(logs.len(), 1, "the refusal should still have been logged");
    let text = fs::read_to_string(&logs[0]).unwrap();
    assert!(text.contains("REFUSED"), "{text}");
    assert!(text.contains("does not exist"), "{text}");
}

#[test]
fn attic_folders_are_pruned_to_the_configured_limit() {
    // Without this the attic grows on every run and is eventually what fills
    // the spare drive.
    let src = Sandbox::new("prune_src");
    let dst = Sandbox::new("prune_dst");
    let st = Sandbox::new("prune_state");
    for n in ["a", "b", "c", "d"] {
        src.write(&format!("{n}.txt"), b"content");
    }
    let mut j = job("prunejob", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Attic;
    j.attic_policy = AtticPolicy {
        keep_runs: Some(2),
        max_age_days: None,
        max_bytes: None,
    };
    run(&j);

    // Each subsequent run displaces one more file, so each creates a folder.
    for n in ["a", "b", "c"] {
        fs::remove_file(src.path(&format!("{n}.txt"))).unwrap();
        run(&j);
    }

    let folders = bkcore::file::attic::list(&dst.root.join(".bku-attic"), false).unwrap();
    assert!(
        folders.len() <= 2,
        "expected at most 2 attic folders, found {}: {:?}",
        folders.len(),
        folders.iter().map(|f| &f.stamp).collect::<Vec<_>>()
    );
}

#[test]
fn attic_folders_are_kept_when_no_limit_is_set() {
    let src = Sandbox::new("keepall_src");
    let dst = Sandbox::new("keepall_dst");
    let st = Sandbox::new("keepall_state");
    for n in ["a", "b", "c"] {
        src.write(&format!("{n}.txt"), b"content");
    }
    let mut j = job("keepall", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Attic;
    j.attic_policy = AtticPolicy::keep_everything();
    run(&j);
    for n in ["a", "b"] {
        fs::remove_file(src.path(&format!("{n}.txt"))).unwrap();
        run(&j);
    }

    let folders = bkcore::file::attic::list(&dst.root.join(".bku-attic"), false).unwrap();
    assert_eq!(folders.len(), 2, "nothing should have been pruned");
}

#[test]
fn two_runs_in_the_same_second_do_not_share_an_attic_folder() {
    // Both runs happen well within one second, so without disambiguation the
    // second would overwrite what the first put in the attic.
    let src = Sandbox::new("clash_src");
    let dst = Sandbox::new("clash_dst");
    let st = Sandbox::new("clash_state");
    src.write("first.txt", b"first version");
    src.write("second.txt", b"second version");
    let mut j = job("clash", &src.root, &dst.root, &st.root);
    j.mode = MirrorMode::Attic;
    j.attic_policy = AtticPolicy::keep_everything();
    run(&j);

    fs::remove_file(src.path("first.txt")).unwrap();
    run(&j);
    fs::remove_file(src.path("second.txt")).unwrap();
    run(&j);

    let folders = bkcore::file::attic::list(&dst.root.join(".bku-attic"), false).unwrap();
    assert_eq!(folders.len(), 2, "each run needs its own folder");
    // And both displaced files must still be recoverable.
    let all: Vec<String> = folders
        .iter()
        .flat_map(|f| fs::read_dir(&f.path).unwrap())
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(all.contains(&"first.txt".to_string()), "{all:?}");
    assert!(all.contains(&"second.txt".to_string()), "{all:?}");
}

#[test]
fn verification_confirms_a_good_mirror_and_catches_a_corrupted_one() {
    let src = Sandbox::new("vfy_src");
    let dst = Sandbox::new("vfy_dst");
    let st = Sandbox::new("vfy_state");
    populate(&src);
    run(&job("vfy", &src.root, &dst.root, &st.root));

    let check = |deep: bool| {
        bkcore::file::verify::compare(
            &src.root,
            &dst.root,
            &Filter::permissive(),
            &bkcore::file::walk::ScanOptions::default(),
            deep,
            2,
            &Cancel::none(),
            &mut |_, _| {},
        )
        .unwrap()
    };

    let clean = check(true);
    assert!(clean.matched(), "a fresh mirror should verify:\n{}", clean.describe());
    assert!(clean.bytes_checked >= 5 * 1024 * 1024);

    // Corrupt one byte of the destination, keeping size and mtime identical —
    // exactly what bit rot looks like.
    let victim = dst.path("data/big.bin");
    let when = fs::metadata(&victim).unwrap().modified().unwrap();
    let mut bytes = fs::read(&victim).unwrap();
    bytes[1000] ^= 0xFF;
    fs::write(&victim, &bytes).unwrap();
    set_mtime(&victim, when);

    let shallow = check(false);
    assert!(shallow.matched(), "size + time cannot see this, by design");

    let deep = check(true);
    assert!(!deep.matched(), "hashing must catch it");
    assert_eq!(deep.differences_total, 1);
}

#[test]
fn a_run_that_would_not_fit_is_refused_before_it_starts() {
    // The check is arithmetic over the plan and the volume's free space, so
    // this exercises the decision rather than needing an actually-full disk.
    assert_eq!(mirror::shortfall(1000, 400, 0), Some(600));
    assert_eq!(mirror::shortfall(1000, 400, 600), None, "removals free space");

    // And a real run on a volume with room must not be blocked by it.
    let src = Sandbox::new("space_src");
    let dst = Sandbox::new("space_dst");
    let st = Sandbox::new("space_state");
    populate(&src);
    let j = job("space", &src.root, &dst.root, &st.root);
    assert!(j.check_free_space, "the check should be on by default");
    let report = run(&j);
    assert!(report.succeeded(), "{}", report.summary());
}

#[test]
fn the_file_list_is_only_collected_when_something_needs_it() {
    // On a multi-terabyte drive this is millions of strings, so an ordinary
    // job must not pay for it.
    let src = Sandbox::new("flist_src");
    let dst = Sandbox::new("flist_dst");
    let st = Sandbox::new("flist_state");
    populate(&src);

    let mut j = job("flist", &src.root, &dst.root, &st.root);
    assert!(run(&j).file_list.is_empty(), "not requested, not collected");

    j.report_file_list = true;
    let report = run(&j);
    assert_eq!(report.file_list.len(), 7, "every file, no directories");
}
