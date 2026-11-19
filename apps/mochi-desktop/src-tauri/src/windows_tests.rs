//! Tests of the per-window state registry (`crate::windows`): windows open
//! archives independently, closing one releases only its own, an archive that
//! is already open is found again, and an empty window is reused.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mochi_core::manifest::Attributes;
use mochi_core::publish::{ArchiveWriter, ReadOptions, Transaction};
use mochi_core::storage::os::OsStorage;
use mochi_testkit::archive::{build, encrypted_options, scripted_history, Job};
use mochi_testkit::SeqIds;

use crate::browse::Archives;
use crate::foreign::Foreign;
use crate::ipc::{JobEvent, JobOutcome, JobRequest, OpenOutcome};
use crate::jobs::{EventSink, JobRegistry};
use crate::windows::*;

fn states() -> Arc<WindowStates> {
    Arc::new(WindowStates::new(
        Arc::new(Archives::default()),
        Arc::new(Foreign::default()),
        Arc::new(JobRegistry::default()),
    ))
}

fn archive(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(name);
    build(
        OsStorage::create_new(&path).unwrap(),
        7,
        &scripted_history(),
    )
    .unwrap();
    path
}

fn open_in(w: &WindowStates, label: &str, path: &Path) -> u32 {
    w.archives()
        .open_as(label, path, ReadOptions::default())
        .unwrap()
        .handle
}

struct Events(Mutex<Vec<JobEvent>>);

impl EventSink for Events {
    fn send(&self, event: JobEvent) -> bool {
        self.0.lock().unwrap().push(event);
        true
    }
}

impl Events {
    fn finished(&self) -> Option<JobOutcome> {
        self.0.lock().unwrap().iter().find_map(|e| match e {
            JobEvent::Finished { outcome, .. } => Some(outcome.clone()),
            _ => None,
        })
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A job that runs for five seconds unless it is cancelled.
fn slow_job(w: &WindowStates, jobs: &Arc<JobRegistry>, label: &str) -> Arc<Events> {
    let events = Arc::new(Events(Mutex::default()));
    let services = jobs.services_for(&w.state(label));
    jobs.start_for(
        label,
        services,
        JobRequest::SelfTest {
            steps: 1000,
            step_ms: 5,
        },
        events.clone(),
        Box::new(|work| {
            std::thread::spawn(work);
        }),
    )
    .unwrap();
    events
}

fn states_with_jobs() -> (Arc<WindowStates>, Arc<JobRegistry>) {
    let jobs = Arc::new(JobRegistry::default());
    let w = Arc::new(WindowStates::new(
        Arc::new(Archives::default()),
        Arc::new(Foreign::default()),
        Arc::clone(&jobs),
    ));
    (w, jobs)
}

#[test]
fn archive_labels_are_main_and_numbered_windows_only() {
    for ok in ["main", "win-1", "win-12"] {
        assert!(is_archive_label(ok), "{ok}");
    }
    for no in ["quick", "win-", "win-x", "win-1a", "main2", "", "WIN-1"] {
        assert!(!is_archive_label(no), "{no}");
    }
}

#[test]
fn placement_prefers_the_window_that_has_the_archive_then_an_empty_one() {
    let some = |s: &str| Some(s.to_owned());
    assert_eq!(place(some("a"), some("b")), Placement::Focus("a".into()));
    assert_eq!(place(some("a"), None), Placement::Focus("a".into()));
    assert_eq!(place(None, some("b")), Placement::Reuse("b".into()));
    assert_eq!(place(None, None), Placement::New);
}

#[test]
fn a_new_window_cascades_from_its_opener_and_wraps_at_the_screen_edge() {
    let monitor = ((0, 0), (1920, 1080));
    assert_eq!(cascade((100, 80), 32, monitor, (1000, 680)), (132, 112));
    // It would run off the right edge: back to the corner, one step in.
    assert_eq!(cascade((900, 80), 32, monitor, (1000, 680)), (32, 32));
    // Off the bottom edge.
    assert_eq!(cascade((100, 400), 32, monitor, (1000, 680)), (32, 32));
    // A second monitor to the left of the first.
    let left = ((-1920, 0), (1920, 1080));
    assert_eq!(cascade((-1900, 0), 32, left, (1000, 680)), (-1868, 32));
    assert_eq!(cascade((-1000, 0), 32, left, (1000, 680)), (-1888, 32));
    // A window bigger than the screen still gets a position.
    assert_eq!(cascade((0, 0), 32, monitor, (4000, 3000)), (32, 32));
}

#[test]
fn the_same_file_is_found_through_a_link_or_another_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let a = archive(&dir, "a.mochi");
    let b = archive(&dir, "b.mochi");
    assert!(same_file(&a, &a));
    assert!(!same_file(&a, &b));
    assert!(same_file(&a, &dir.path().join(".").join("a.mochi")));
    #[cfg(unix)]
    {
        let link = dir.path().join("link.mochi");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(same_file(&a, &link));
        let hard = dir.path().join("hard.mochi");
        std::fs::hard_link(&a, &hard).unwrap();
        assert!(same_file(&a, &hard));
    }
    // A file that does not exist is only the same as its own spelling.
    let missing = dir.path().join("missing.mochi");
    assert!(same_file(&missing, &missing));
    assert!(!same_file(&missing, &a));
}

#[test]
fn two_windows_open_archives_independently() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (archive(&dir, "a.mochi"), archive(&dir, "b.mochi"));
    let w = states();
    w.register("main");
    w.register("win-1");
    let ha = open_in(&w, "main", &pa);
    let hb = open_in(&w, "win-1", &pb);
    assert_ne!(ha, hb);
    assert_eq!(w.archives().owner_of(ha).as_deref(), Some("main"));
    assert_eq!(w.archives().owner_of(hb).as_deref(), Some("win-1"));
    assert_eq!(w.archives().handles_of("main"), [ha]);
    assert_eq!(w.archives().handles_of("win-1"), [hb]);
    // Each browses its own archive.
    assert!(w.archives().snapshot(ha, 1).is_ok());
    assert!(w.archives().snapshot(hb, 1).is_ok());
    assert_eq!(w.title("main"), "a.mochi - MOCHI");
    assert_eq!(w.title("win-1"), "b.mochi - MOCHI");
    assert_eq!(w.title("win-9"), "MOCHI");
}

#[test]
fn each_window_holds_its_own_limit_of_open_archives() {
    let dir = tempfile::tempdir().unwrap();
    let p = archive(&dir, "a.mochi");
    let w = states();
    for _ in 0..crate::browse::MAX_OPEN {
        open_in(&w, "main", &p);
    }
    let e = w
        .archives()
        .open_as("main", &p, ReadOptions::default())
        .unwrap_err();
    assert_eq!(e.code, mochi_core::ErrorCode::LimitExceeded);
    // Another window is not held back by it.
    open_in(&w, "win-1", &p);
}

#[test]
fn closing_a_window_releases_only_its_own_archives() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (archive(&dir, "a.mochi"), archive(&dir, "b.mochi"));
    let w = states();
    w.register("main");
    w.register("win-1");
    let ha = open_in(&w, "main", &pa);
    let hb = open_in(&w, "win-1", &pb);
    // An archive nobody claimed (the quick worker's) is nobody's to release.
    let loose = w
        .archives()
        .open(&pa, ReadOptions::default())
        .unwrap()
        .handle;
    let released = w.release("main");
    assert_eq!(released.archives, 1);
    assert!(
        w.archives().snapshot(ha, 1).is_err(),
        "main's archive is closed"
    );
    assert!(w.archives().snapshot(hb, 1).is_ok(), "win-1's is not");
    assert!(w.archives().snapshot(loose, 1).is_ok());
    assert!(!w.is_registered("main"));
    assert!(w.is_registered("win-1"));
    // The label can be used again for a window made later.
    assert_eq!(w.next_label(), "main");
}

#[test]
fn closing_a_window_cancels_its_jobs_and_no_other_window_s() {
    let (w, jobs) = states_with_jobs();
    w.register("main");
    w.register("win-1");
    let a = slow_job(&w, &jobs, "main");
    let b = slow_job(&w, &jobs, "win-1");
    assert_eq!(jobs.running_for("main"), 1);
    assert_eq!(jobs.running_for("win-1"), 1);
    let released = w.release("main");
    assert_eq!(released.jobs, 1);
    wait_until("main's job to stop", || a.finished().is_some());
    assert_eq!(a.finished(), Some(JobOutcome::Cancelled));
    assert!(b.finished().is_none(), "win-1's job keeps running");
    assert_eq!(jobs.running_for("win-1"), 1);
    w.release("win-1");
    wait_until("win-1's job to stop", || b.finished().is_some());
    assert_eq!(jobs.running(), 0);
}

#[test]
fn staged_sources_and_the_extract_folder_are_per_window() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"x").unwrap();
    let w = states();
    let (a, b) = (w.state("main"), w.state("win-1"));
    a.staging.add(std::slice::from_ref(&file));
    a.destination.set(dir.path().to_path_buf());
    assert_eq!(a.staging.view().items.len(), 1);
    assert!(b.staging.view().items.is_empty());
    assert!(b.destination.get().is_none());
    assert!(!Arc::ptr_eq(&a.checks, &b.checks));
    assert!(!Arc::ptr_eq(&a.drops, &b.drops));
    // The same window gets the same state back.
    assert!(Arc::ptr_eq(&a.staging, &w.state("main").staging));
    // Clearing one does not touch the other.
    b.staging.add(std::slice::from_ref(&file));
    a.staging.clear();
    assert_eq!(b.staging.view().items.len(), 1);
}

#[test]
fn jobs_work_on_their_own_window_s_staging() {
    let (w, jobs) = states_with_jobs();
    let (a, b) = (w.state("main"), w.state("win-1"));
    let sa = jobs.services_for(&a);
    let sb = jobs.services_for(&b);
    assert!(Arc::ptr_eq(&sa.staging, &a.staging));
    assert!(Arc::ptr_eq(&sb.staging, &b.staging));
    assert!(!Arc::ptr_eq(&sa.staging, &sb.staging));
    assert!(Arc::ptr_eq(&sa.archives, &sb.archives));
}

#[test]
fn an_archive_open_in_one_window_is_found_by_any_spelling_of_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (archive(&dir, "a.mochi"), archive(&dir, "b.mochi"));
    let w = states();
    w.register("main");
    w.register("win-1");
    open_in(&w, "main", &pa);
    open_in(&w, "win-1", &pb);
    assert_eq!(w.window_with_file(&pa).as_deref(), Some("main"));
    assert_eq!(
        w.window_with_file(&dir.path().join(".").join("b.mochi"))
            .as_deref(),
        Some("win-1")
    );
    assert_eq!(w.window_with_file(&dir.path().join("c.mochi")), None);
    // Asking again for a.mochi from win-1 focuses main; so does the shell.
    assert_eq!(
        w.decide(&pa, Some("win-1")),
        Placement::Focus("main".into())
    );
    assert_eq!(w.decide(&pa, None), Placement::Focus("main".into()));
    // A window that was closed no longer owns anything.
    w.release("main");
    assert_eq!(w.window_with_file(&pa), None);
}

#[test]
fn an_empty_window_is_reused_and_a_busy_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let pa = archive(&dir, "a.mochi");
    let pb = archive(&dir, "b.mochi");
    let (w, jobs) = states_with_jobs();
    w.register("main");
    // Empty: the user's open and the shell's both reuse it.
    assert_eq!(w.decide(&pa, Some("main")), Placement::Reuse("main".into()));
    assert_eq!(w.decide(&pa, None), Placement::Reuse("main".into()));
    // An archive on screen makes it busy.
    let h = open_in(&w, "main", &pa);
    assert!(!w.is_empty("main"));
    assert_eq!(w.decide(&pb, Some("main")), Placement::New);
    assert_eq!(w.decide(&pb, None), Placement::New);
    // Closing the archive (back to the start screen) frees it again.
    assert!(w.archives().close(h));
    assert!(w.is_empty("main"));
    assert_eq!(w.decide(&pb, Some("main")), Placement::Reuse("main".into()));
    // So does anything staged to add.
    let file = dir.path().join("note.txt");
    std::fs::write(&file, b"x").unwrap();
    w.state("main").staging.add(&[file]);
    assert!(!w.is_empty("main"));
    assert_eq!(w.decide(&pb, Some("main")), Placement::New);
    w.state("main").staging.clear();
    // And a running job.
    let job = slow_job(&w, &jobs, "main");
    assert!(!w.is_empty("main"));
    w.release("main");
    wait_until("the job to stop", || job.finished().is_some());
    // An unknown window is never "empty": it is not there to reuse.
    assert_eq!(w.decide(&pb, Some("win-7")), Placement::New);
}

#[test]
fn the_shell_reuses_the_empty_window_used_last() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (archive(&dir, "a.mochi"), archive(&dir, "b.mochi"));
    let w = states();
    for label in ["main", "win-1", "win-2"] {
        w.register(label);
    }
    open_in(&w, "win-2", &pa);
    // Most recently used first: win-2 (busy), win-1, main.
    assert_eq!(w.empty_window().as_deref(), Some("win-1"));
    w.touch("main");
    assert_eq!(w.empty_window().as_deref(), Some("main"));
    assert_eq!(w.decide(&pb, None), Placement::Reuse("main".into()));
}

#[test]
fn a_waiting_passphrase_belongs_to_its_window() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("enc.mochi");
    let mut writer = ArchiveWriter::create(
        OsStorage::create_new(&path).unwrap(),
        Box::new(SeqIds::new(3)),
        encrypted_options(&["correct horse"]),
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(
        mochi_testkit::archive::path("secret.txt"),
        b"x".to_vec(),
        Attributes::default(),
    );
    writer.commit(tx, &Job::new().ctx()).unwrap();
    writer.close().unwrap();
    let w = states();
    w.register("main");
    w.register("win-1");
    let outcome = w.archives().open_or_pend_as("win-1", &path).unwrap();
    let OpenOutcome::NeedsPassphrase { pending, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert!(!w.is_empty("win-1"));
    assert!(w.is_empty("main"));
    // Asking for the same file again goes to the window that is asking.
    assert_eq!(w.window_with_file(&path).as_deref(), Some("win-1"));
    assert_eq!(
        w.decide(&path, Some("main")),
        Placement::Focus("win-1".into())
    );
    // Closing win-1 forgets the wait.
    w.release("win-1");
    assert!(!w.archives().cancel_pending(pending), "already forgotten");
    assert_eq!(w.window_with_file(&path), None);
}

#[test]
fn labels_are_main_first_then_numbered_and_bounded() {
    let w = states();
    assert_eq!(w.next_label(), "main");
    w.register("main");
    assert_eq!(w.next_label(), "win-1");
    assert_eq!(w.next_label(), "win-2", "numbers are not handed out twice");
    w.register("win-2");
    assert!(w.has_room());
    for n in 3..=MAX_WINDOWS {
        w.register(&format!("win-{n}"));
    }
    assert_eq!(w.labels().len(), MAX_WINDOWS);
    assert!(!w.has_room());
    w.release("win-3");
    assert!(w.has_room());
}

#[test]
fn every_window_shares_one_recent_extract_folder_list() {
    let w = states();
    let dir = tempfile::tempdir().unwrap();
    w.state("main").destination.history.record(dir.path());
    let other = w.state("win-1");
    assert_eq!(
        other.destination.history.existing(),
        [dir.path().to_str().unwrap()]
    );
    assert!(Arc::ptr_eq(&other.destination.history, w.history()));
}
