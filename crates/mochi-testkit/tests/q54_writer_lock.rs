//! Q54 (owner decision 2026-10-06; spec Annex B D17, B.2.7): the publication
//! lock is an OS lock on a separate lock file, `<archive>.mochi-lock`.
//!
//! On the real filesystem: writers are excluded through every alias the
//! lock-file name resolves; the lock file's existence is not ownership; and
//! readers, which take no lock, read the committed snapshot while a writer
//! is in the middle of an append. On Windows that last one failed before
//! (error 33: the writer's lock covered the archive itself).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::{open_head, phase, ArchiveWriter, ReadOptions, TailPolicy};
use mochi_core::storage::os::{lock_file_path, OsReadStorage, OsStorage, LOCK_FILE_SUFFIX};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{build, read_state, scripted_history, test_options, Job, Step};
use mochi_testkit::SeqIds;

const NAME: &str = "a.mochi";

/// A directory holding an archive with the first `n` scripted commits.
fn archive(n: usize) -> (tempfile::TempDir, PathBuf, Vec<Step>) {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join(NAME);
    let steps = scripted_history();
    build(OsStorage::create_new(&file).unwrap(), 7, &steps[..n]).unwrap();
    (tmp, file, steps)
}

fn writer(path: &Path) -> mochi_core::Result<ArchiveWriter<OsStorage>> {
    ArchiveWriter::open_append(
        OsStorage::open_existing(path)?,
        Box::new(SeqIds::new(9)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|(w, _)| w)
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join(format!("{NAME}{LOCK_FILE_SUFFIX}"))
}

/// Every alias the lock-file name resolves reaches one lock: `.` and `..`
/// segments, a symbolic link (Unix), a hard link (Unix, through the
/// archive's own `flock`), and a different case on a case-insensitive
/// filesystem (Windows). The holder is excluded through all of them, and
/// the next writer gets the lock once the holder closes.
#[test]
fn q54_a_writer_excludes_every_alias() {
    let (tmp, file, _) = archive(2);
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    let mut aliases = vec![
        file.clone(),
        tmp.path().join(".").join(NAME),
        tmp.path().join("sub").join("..").join(NAME),
    ];
    for alias in &aliases {
        assert_eq!(
            lock_file_path(alias).unwrap(),
            lock_file_path(&file).unwrap()
        );
    }
    #[cfg(unix)]
    {
        let link = tmp.path().join("sub").join("link.mochi");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(
            lock_file_path(&link).unwrap(),
            lock_file_path(&file).unwrap()
        );
        aliases.push(link);
        let hard = tmp.path().join("sub").join("hard.mochi");
        std::fs::hard_link(&file, &hard).unwrap();
        aliases.push(hard);
    }
    #[cfg(windows)]
    aliases.push(tmp.path().join(NAME.to_uppercase()));

    let holder = writer(&file).unwrap();
    for alias in &aliases {
        let e = writer(alias).map(|_| ()).unwrap_err();
        assert_eq!(e.code, ErrorCode::LockConflict, "{}: {e}", alias.display());
    }
    holder.close().unwrap();
    for alias in &aliases {
        writer(alias).unwrap().close().unwrap();
    }
}

/// The lock file's existence is not ownership: a leftover lock file nobody
/// holds does not block a writer, and closing a writer leaves the file in
/// place, empty. Only the OS lock on it excludes.
#[test]
fn q54_lock_file_existence_is_not_ownership() {
    let (tmp, file, steps) = archive(2);
    let lock = lock_path(tmp.path());
    assert!(lock.exists(), "the creating writer's lock file stays");
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);

    // A missing lock file is simply created again.
    std::fs::remove_file(&lock).unwrap();
    let w = writer(&file).unwrap();
    w.close().unwrap();
    assert!(lock.exists(), "unlocking never removes the lock file");
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);

    // Someone else holding the OS lock on the lock file excludes writers.
    let other = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    other.try_lock().unwrap();
    let e = writer(&file).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    other.unlock().unwrap();

    // Dropping a writer without closing it releases the lock too.
    let mut w = writer(&file).unwrap();
    w.commit(steps[2].tx.clone(), &Job::new().ctx()).unwrap();
    drop(w);
    writer(&file).unwrap().close().unwrap();
    assert_eq!(std::fs::metadata(&lock).unwrap().len(), 0);
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &ReadOptions::default()).unwrap();
    assert_eq!(read_state(&r, &head).unwrap(), steps[2].after);
}

/// What a reader saw at one point of the writer's commit.
#[derive(Debug)]
struct Seen {
    phase: &'static str,
    seq: u64,
    state_matches: bool,
    second_writer: Option<ErrorCode>,
}

/// At each phase, opens the archive read-only (no lock), reads the head and
/// every file, and tries a second writer.
struct ReadDuringAppend<'a> {
    file: &'a Path,
    before: &'a Step,
    after: &'a Step,
    seen: Mutex<Vec<Seen>>,
}

impl ProgressSink for ReadDuringAppend<'_> {
    fn report(&self, e: &ProgressEvent) {
        let r = OsReadStorage::open(self.file).unwrap();
        let head = open_head(&r, &ReadOptions::default())
            .unwrap_or_else(|err| panic!("{}: reader failed: {err}", e.phase));
        let state = read_state(&r, &head)
            .unwrap_or_else(|err| panic!("{}: reading files failed: {err}", e.phase));
        let want = if head.seq() == 1 {
            &self.before.after
        } else {
            &self.after.after
        };
        let second_writer = writer(self.file).map(|_| ()).err().map(|e| e.code);
        self.seen.lock().unwrap().push(Seen {
            phase: e.phase,
            seq: head.seq(),
            state_matches: state == *want,
            second_writer,
        });
    }
}

/// **Q54 evidence.** While a writer appends (content written, checkpoint
/// and commit record written, footer about to be appended and about to be
/// synced), a reader without any lock opens the archive and reads the last
/// committed snapshot exactly; a second writer is refused throughout. Before
/// the footer is appended the reader sees the previous head; once it is
/// appended, the new one. Never anything in between.
#[test]
fn q54_readers_see_the_committed_snapshot_during_an_append() {
    let (_tmp, file, steps) = archive(2);
    let mut w = writer(&file).unwrap();

    // Between commits, with the lock held: readers are not blocked.
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &ReadOptions::default()).unwrap();
    assert_eq!(head.seq(), 1);
    assert_eq!(read_state(&r, &head).unwrap(), steps[1].after);

    let sink = ReadDuringAppend {
        file: &file,
        before: &steps[1],
        after: &steps[2],
        seen: Mutex::new(Vec::new()),
    };
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &sink,
        cancel: &cancel,
    };
    w.commit(steps[2].tx.clone(), &ctx).unwrap();
    let seen = sink.seen.into_inner().unwrap();

    for p in [
        phase::CONTENT,
        phase::COMMIT_RECORD,
        phase::FOOTER,
        phase::SYNC_FOOTER,
    ] {
        assert!(seen.iter().any(|s| s.phase == p), "phase {p} not observed");
    }
    let mut footer_seen = false;
    for s in &seen {
        assert!(s.state_matches, "{s:?}");
        assert_eq!(s.second_writer, Some(ErrorCode::LockConflict), "{s:?}");
        footer_seen |= s.phase == phase::SYNC_FOOTER;
        // The footer is appended after FOOTER and before SYNC_FOOTER.
        let want = if footer_seen { 2 } else { 1 };
        assert_eq!(s.seq, want, "{s:?}");
    }
    w.close().unwrap();
}

/// Creation (D13) takes the final name's lock file before the archive
/// becomes visible, and keeps it: a second writer is refused while the
/// creating writer lives, and a reader can read the new archive meanwhile
/// (on Windows the temporary file's own lock is released at publication).
#[test]
fn q54_a_created_archive_is_locked_and_readable() {
    use mochi_core::storage::os::OsDir;
    let tmp = tempfile::tempdir().unwrap();
    let mut dir = OsDir::open(tmp.path()).unwrap();
    let steps = scripted_history();
    let (w, _) = ArchiveWriter::<OsStorage>::create_in(
        &mut dir,
        NAME,
        Box::new(SeqIds::new(7)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
    .unwrap();
    let file = tmp.path().join(NAME);
    let e = writer(&file).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &ReadOptions::default()).unwrap();
    assert_eq!(read_state(&r, &head).unwrap(), steps[0].after);

    // Another process holding the final name's lock: nothing is created.
    let other_name = "b.mochi";
    let lock = tmp.path().join(format!("{other_name}{LOCK_FILE_SUFFIX}"));
    let held = std::fs::File::create(&lock).unwrap();
    held.try_lock().unwrap();
    let e = ArchiveWriter::<OsStorage>::create_in(
        &mut dir,
        other_name,
        Box::new(SeqIds::new(8)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict, "{e}");
    assert!(!tmp.path().join(other_name).exists());
    let leftovers = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter(|e| {
            let n = e.as_ref().unwrap().file_name();
            n.to_string_lossy().ends_with(".mochi-tmp")
        })
        .count();
    assert_eq!(leftovers, 0, "the temporary file was removed");
    drop(held);
    w.close().unwrap();
    writer(&file).unwrap().close().unwrap();
}
