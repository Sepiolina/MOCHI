//! T22: archive creation by the D13 mechanism (spec Annex B.2 D13; gate G7).
//!
//! The first commit goes to an exclusively created, locked temporary file,
//! is synced, published at the final name without replacing, and then the
//! directory is flushed. `DESTINATION_EXISTS` when the name is taken.

use std::sync::Mutex;

use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::{
    commit_history, open_head, phase, temporary_name, ArchiveWriter, CommitOutcome,
    PublishDurability, ReadOptions,
};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::storage::os::{OsDir, OsStorage, LOCK_FILE_SUFFIX};
use mochi_core::storage::{RemoveOutcome, StorageDir};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{read_state, scripted_history, test_options, Job};
use mochi_testkit::{CrashMode, DirFault, DirOp, Fault, Op, SeqIds, SimDir, SimStorage};

const NAME: &str = "a.mochi";

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn create_sim(
    dir: &mut SimDir,
    seed: u64,
) -> mochi_core::Result<(ArchiveWriter<SimStorage>, CommitOutcome)> {
    let steps = scripted_history();
    ArchiveWriter::create_in(
        dir,
        NAME,
        Box::new(SeqIds::new(seed)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
}

/// The archive at `NAME` opens to the first scripted state.
fn assert_complete(file: &SimStorage) {
    let head = open_head(file, &opts()).unwrap();
    assert_eq!(head.seq(), 0);
    assert_eq!(
        read_state(file, &head).unwrap(),
        scripted_history()[0].after
    );
}

fn temp_names(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| n.ends_with(".mochi-tmp"))
        .cloned()
        .collect()
}

#[test]
fn t22_creation_publishes_then_flushes() {
    let mut dir = SimDir::new();
    let (mut w, outcome) = create_sim(&mut dir, 7).unwrap();
    assert_eq!(outcome.seq, 0);
    assert_eq!(outcome.durability, PublishDurability::Durable);
    assert_eq!(dir.names(), [NAME]);
    assert_eq!(dir.durable_names(), [NAME]);
    let file = dir.file(NAME).unwrap();
    assert_complete(&file);

    // Order: create the temporary file, publish, then flush the directory
    // once. The temporary file itself never asked for a directory flush.
    let trace = dir.trace();
    let temp = match &trace[0] {
        DirOp::Create { name } => name.clone(),
        other => panic!("first op {other:?}"),
    };
    assert!(
        temp.starts_with(".a.mochi.") && temp.ends_with(".mochi-tmp"),
        "{temp}"
    );
    assert_eq!(
        trace[1..],
        [
            DirOp::Link {
                from: temp.clone(),
                to: NAME.into()
            },
            DirOp::Unlink { name: temp.clone() },
            DirOp::SyncDirectory,
        ]
    );
    assert!(!file.trace().contains(&Op::SyncDirectory));
    assert!(file.trace().contains(&Op::SyncData));

    // The writer holds the archive and keeps appending.
    assert_eq!(dir.remove_if_unlocked_probe(NAME), RemoveOutcome::Locked);
    let job = Job::new();
    w.commit(scripted_history()[1].tx.clone(), &job.ctx())
        .unwrap();
    assert_eq!(commit_history(&file, &opts()).unwrap().len(), 2);
}

#[test]
fn t22_creation_on_the_os() {
    let tmp = tempfile::tempdir().unwrap();
    let mut dir = OsDir::open(tmp.path()).unwrap();
    let steps = scripted_history();
    let (w, outcome) = ArchiveWriter::<OsStorage>::create_in(
        &mut dir,
        NAME,
        Box::new(SeqIds::new(7)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(outcome.seq, 0);
    if cfg!(target_os = "linux") {
        assert_eq!(outcome.durability, PublishDurability::Durable);
    }
    w.close().unwrap();
    let mut names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    // The writer lock file stays after the writer closes (Q54: existence is
    // not ownership, so it is never removed), and it holds no bytes.
    let lock = format!("{NAME}{LOCK_FILE_SUFFIX}");
    assert_eq!(names, [NAME.to_string(), lock.clone()]);
    assert_eq!(std::fs::metadata(tmp.path().join(lock)).unwrap().len(), 0);
    let r = OsReadStorage::open(tmp.path().join(NAME)).unwrap();
    let head = open_head(&r, &opts()).unwrap();
    assert_eq!(read_state(&r, &head).unwrap(), steps[0].after);
}

/// **DESTINATION_EXISTS**: the existing file is untouched and no temporary
/// file is left behind.
#[test]
fn t22_an_existing_destination_is_never_replaced() {
    let mut dir = SimDir::new();
    dir.insert(NAME, SimStorage::from_bytes(b"precious".to_vec()));
    let e = create_sim(&mut dir, 7).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists, "{e}");
    assert_eq!(dir.names(), [NAME]);
    assert_eq!(dir.file(NAME).unwrap().contents(), b"precious");

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(NAME), b"precious").unwrap();
    let mut os = OsDir::open(tmp.path()).unwrap();
    let e = ArchiveWriter::<OsStorage>::create_in(
        &mut os,
        NAME,
        Box::new(SeqIds::new(7)),
        test_options(),
        scripted_history()[0].tx.clone(),
        &Job::new().ctx(),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(std::fs::read(tmp.path().join(NAME)).unwrap(), b"precious");
    // No temporary file is left. The final name's lock file (Q54), taken
    // before publication was attempted, may stay: it is empty and unlocked.
    let lock = tmp.path().join(format!("{NAME}{LOCK_FILE_SUFFIX}"));
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 2);
    let held = std::fs::File::open(&lock).unwrap();
    held.try_lock().unwrap();
}

/// D13 outcomes for the directory flush.
#[test]
fn t22_directory_flush_outcomes() {
    let mut dir = SimDir::with_faults([DirFault::UnconfirmedSyncDirectory { index: 0 }]);
    let (_w, outcome) = create_sim(&mut dir, 7).unwrap();
    assert!(matches!(
        outcome.durability,
        PublishDurability::DirectoryUnconfirmed(_)
    ));
    assert_eq!(outcome.status.as_str(), "LOCAL_COMMITTED");

    let mut dir = SimDir::with_faults([DirFault::FailSyncDirectory { index: 0 }]);
    let e = create_sim(&mut dir, 7).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::CommitUnconfirmed, "{e}");
    // Left in place, complete.
    assert_eq!(dir.names(), [NAME]);
    assert_complete(&dir.file(NAME).unwrap());
}

/// A commit that fails inside the temporary file (here, its first data
/// sync) publishes nothing and removes the temporary file.
#[test]
fn t22_failed_commit_creates_nothing() {
    let dir = SimDir::new();
    dir.add_next_file_fault(Fault::FailSync { sync_index: 0 });
    let mut d = dir.clone();
    let e = create_sim(&mut d, 7).map(|_| ()).unwrap_err();
    assert!(e.message.contains("nothing was created"), "{e}");
    assert!(dir.names().is_empty(), "{:?}", dir.names());
}

/// How many mutations a successful creation makes to its file and to the
/// directory: the crash points.
fn crash_points() -> (usize, usize) {
    let mut dir = SimDir::new();
    let (w, _) = create_sim(&mut dir, 7).unwrap();
    let file = dir.file(NAME).unwrap();
    let mutations = file
        .trace()
        .iter()
        .filter(|o| !matches!(o, Op::Lock | Op::Unlock))
        .count();
    drop(w);
    (mutations, 3) // create, publish, sync directory
}

/// After a crash and cleanup, the directory holds either nothing or the
/// complete first commit at `NAME`.
fn check_after_crash(dir: &SimDir, mode: CrashMode, what: &str) {
    let mut after = dir.crash_image(mode);
    for t in temp_names(&after.names()) {
        assert_eq!(
            after.remove_if_unlocked(&t).unwrap(),
            RemoveOutcome::Removed,
            "{what}: a crash releases every lock"
        );
    }
    match after.names().as_slice() {
        [] => {}
        [n] if n == NAME => assert_complete(&after.file(NAME).unwrap()),
        other => panic!("{what} {mode:?}: unexpected names {other:?}"),
    }
}

/// **Checklist DoD (crash at every step).** A halt before every mutation of
/// the temporary file, before every directory operation, and between link
/// and unlink; each under a process kill and a power loss.
#[test]
fn t22_a_crash_at_every_step_leaves_nothing_or_the_whole_archive() {
    let (file_mutations, dir_ops) = crash_points();
    assert!(file_mutations >= 5, "{file_mutations}");
    let modes = [CrashMode::KeepAll, CrashMode::SyncedOnly];
    let mut published = 0;
    for k in 0..file_mutations {
        let dir = SimDir::new();
        dir.add_next_file_fault(Fault::HaltBeforeMutation { mutation_index: k });
        let mut d = dir.clone();
        assert!(create_sim(&mut d, 7).is_err(), "file mutation {k}");
        assert!(
            dir.names().iter().all(|n| n != NAME),
            "nothing published before the commit"
        );
        for m in modes {
            check_after_crash(&dir, m, &format!("file mutation {k}"));
        }
    }
    for k in 0..dir_ops {
        let dir = SimDir::with_faults([DirFault::HaltBeforeOp { index: k }]);
        let mut d = dir.clone();
        assert!(create_sim(&mut d, 7).is_err(), "dir op {k}");
        published += usize::from(dir.names().iter().any(|n| n == NAME));
        for m in modes {
            check_after_crash(&dir, m, &format!("dir op {k}"));
        }
    }
    assert_eq!(
        published, 1,
        "only a halt before the directory flush leaves it published"
    );

    let dir = SimDir::with_faults([DirFault::HaltBetweenLinkAndUnlink { index: 0 }]);
    let mut d = dir.clone();
    assert!(create_sim(&mut d, 7).is_err());
    assert_eq!(temp_names(&dir.names()).len(), 1);
    check_after_crash(&dir, CrashMode::KeepAll, "between link and unlink");
    // Power loss before any directory flush: neither name survives.
    assert!(dir.crash_image(CrashMode::SyncedOnly).names().is_empty());
}

/// Runs a second creator the moment the first reports `phase`.
struct Interleave {
    at: &'static str,
    run: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl ProgressSink for Interleave {
    fn report(&self, e: &ProgressEvent) {
        if e.phase == self.at {
            if let Some(f) = self.run.lock().unwrap().take() {
                f();
            }
        }
    }
}

/// **Checklist DoD (two creators).** B runs entirely while A is about to
/// publish, so both temporary files exist at once. Exactly one wins; the
/// loser gets `DESTINATION_EXISTS` and removes only its own temporary file.
#[test]
fn t22_two_concurrent_creators_one_winner() {
    let dir = SimDir::new();
    let b_dir = dir.clone();
    let b_result = std::sync::Arc::new(Mutex::new(None));
    let b_out = b_result.clone();
    let sink = Interleave {
        at: phase::PUBLISH,
        run: Mutex::new(Some(Box::new(move || {
            let mut d = b_dir.clone();
            assert_eq!(temp_names(&d.names()).len(), 1, "A's temporary file exists");
            let r = create_sim(&mut d, 99).map(|(w, o)| {
                drop(w);
                o.seq
            });
            *b_out.lock().unwrap() = Some(r.map_err(|e| e.code));
        }))),
    };
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &sink,
        cancel: &cancel,
    };
    let mut a_dir = dir.clone();
    let a = ArchiveWriter::create_in(
        &mut a_dir,
        NAME,
        Box::new(SeqIds::new(7)),
        test_options(),
        scripted_history()[0].tx.clone(),
        &ctx,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(b_result.lock().unwrap().clone(), Some(Ok(0)), "B won");
    assert_eq!(a.code, ErrorCode::DestinationExists);
    assert_eq!(
        dir.names(),
        [NAME],
        "A removed its own temporary file, nothing else"
    );
    assert_complete(&dir.file(NAME).unwrap());
    // B's archive, not A's: B's temporary name was the one published.
    let b_temp = temporary_name(
        NAME,
        &mochi_core::object::IdSource::next_id(&mut SeqIds::new(99)).unwrap(),
    );
    assert!(dir.trace().contains(&DirOp::Link {
        from: b_temp,
        to: NAME.into()
    }));
}

trait Probe {
    fn remove_if_unlocked_probe(&self, name: &str) -> RemoveOutcome;
}

impl Probe for SimDir {
    /// `remove_if_unlocked` on a clone, which reports `Locked` without
    /// changing anything when the lock is held.
    fn remove_if_unlocked_probe(&self, name: &str) -> RemoveOutcome {
        self.clone().remove_if_unlocked(name).unwrap()
    }
}
