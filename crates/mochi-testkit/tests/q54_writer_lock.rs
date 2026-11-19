//! Q54 (owner decision 2026-10-06; spec Annex B D17, B.2.7) as amended by
//! the owner decision of 2026-10-20 (D27, B.2.17): the publication lock is an
//! OS lock **on the archive itself**, and no lock file is ever created.
//!
//! On the real filesystem: writers are excluded through every alias,
//! hard links included; readers, which take no lock, read the committed
//! snapshot while a writer is in the middle of an append (on Windows that
//! failed before the lock moved to one byte beyond the data: error 33); no
//! `.mochi-lock` file appears; and legacy lock files that builds up to 0.9.2
//! left behind are removed by a writer, under the rules of B.2.17 item 5.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::{open_head, phase, ArchiveWriter, ReadOptions, TailPolicy};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsStorage, LEGACY_LOCK_FILE_SUFFIX};
use mochi_core::storage::Storage;
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
    writer_seeded(path, 9)
}

/// [`writer`] with its own ID seed, for tests with more than one committing
/// writer (object IDs must not repeat).
fn writer_seeded(path: &Path, seed: u64) -> mochi_core::Result<ArchiveWriter<OsStorage>> {
    ArchiveWriter::open_append(
        OsStorage::open_existing(path)?,
        Box::new(SeqIds::new(seed)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|(w, _)| w)
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every alias of one archive reaches one lock: `.` and `..` segments, a
/// symbolic link (Unix), a hard link (every platform: the lock belongs to the
/// file), and a different case on a case-insensitive filesystem (Windows).
/// The holder is excluded through all of them, and the next writer gets the
/// lock once the holder closes.
#[test]
fn q54_a_writer_excludes_every_alias() {
    let (tmp, file, _) = archive(2);
    std::fs::create_dir(tmp.path().join("sub")).unwrap();
    let mut aliases = vec![
        file.clone(),
        tmp.path().join(".").join(NAME),
        tmp.path().join("sub").join("..").join(NAME),
    ];
    #[cfg(unix)]
    {
        let link = tmp.path().join("sub").join("link.mochi");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        aliases.push(link);
    }
    let hard = tmp.path().join("sub").join("hard.mochi");
    std::fs::hard_link(&file, &hard).unwrap();
    aliases.push(hard);
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

/// **Windows (B.2.17 item 2).** Two hard-link names of one archive: the lock
/// taken through either name excludes the other. B.2.7 had recorded this as
/// a gap; a byte-range lock belongs to the file, not to the name.
#[cfg(windows)]
#[test]
fn q54_windows_hard_links_share_the_lock() {
    let (tmp, file, _) = archive(2);
    let other = tmp.path().join("other.mochi");
    std::fs::hard_link(&file, &other).unwrap();
    for (held, refused) in [(&file, &other), (&other, &file)] {
        let holder = writer(held).unwrap();
        let e = writer(refused).map(|_| ()).unwrap_err();
        assert_eq!(e.code, ErrorCode::LockConflict, "{e}");
        holder.close().unwrap();
    }
}

/// The lock is held on the archive and nothing else: no lock file is created
/// by create, append, close, or a failed lock, on any platform. Someone else
/// holding the archive's OS lock excludes writers, and dropping a writer
/// without closing it releases the lock too.
#[test]
fn q54_no_lock_file_is_ever_created() {
    let tmp = tempfile::tempdir().unwrap();
    let w = create(tmp.path(), NAME, 7).unwrap();
    assert_eq!(names_in(tmp.path()), [NAME]);
    w.close().unwrap();
    assert_eq!(names_in(tmp.path()), [NAME]);

    let file = tmp.path().join(NAME);
    let steps = scripted_history();
    let mut w = writer_seeded(&file, 11).unwrap();
    w.commit(steps[1].tx.clone(), &Job::new().ctx()).unwrap();
    assert_eq!(names_in(tmp.path()), [NAME]);
    w.close().unwrap();

    // Someone else holding the OS lock on the archive excludes writers.
    let mut other = OsStorage::open_existing(&file).unwrap();
    other.try_lock_exclusive().unwrap();
    let e = writer(&file).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    other.unlock().unwrap();
    assert_eq!(names_in(tmp.path()), [NAME]);

    // Dropping a writer without closing it releases the lock too.
    let mut w = writer(&file).unwrap();
    w.commit(steps[2].tx.clone(), &Job::new().ctx()).unwrap();
    drop(w);
    writer(&file).unwrap().close().unwrap();
    assert_eq!(names_in(tmp.path()), [NAME]);
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

/// Creation (D13) locks the temporary file before it is published, and the
/// lock is on the open handle, so it carries across the rename: a second
/// writer is refused while the creating writer lives, and a reader can read
/// the new archive meanwhile. A name that is already taken (here by an archive
/// another writer holds) is `DESTINATION_EXISTS`: nothing is replaced, nothing
/// is left behind.
#[test]
fn q54_a_created_archive_is_locked_and_readable() {
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
    assert_eq!(names_in(tmp.path()), [NAME]);

    // The name is taken by an archive another writer holds: nothing is
    // created or changed, and the holder keeps its lock.
    let before = std::fs::read(&file).unwrap();
    let e = ArchiveWriter::<OsStorage>::create_in(
        &mut dir,
        NAME,
        Box::new(SeqIds::new(8)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists, "{e}");
    assert_eq!(
        names_in(tmp.path()),
        [NAME],
        "the temporary file was removed"
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
    let e = writer(&file).map(|_| ()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    w.close().unwrap();
    writer(&file).unwrap().close().unwrap();
}

// ---- Legacy lock files (owner decision 2026-10-20; spec B.2.17 item 5) ------

/// `<name>.mochi-lock` in `dir`.
fn legacy_of(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{LEGACY_LOCK_FILE_SUFFIX}"))
}

/// An empty legacy lock file for `name`, as builds up to 0.9.2 left it.
fn legacy(dir: &Path, name: &str) -> PathBuf {
    let p = legacy_of(dir, name);
    std::fs::File::create(&p).unwrap();
    p
}

/// Creates `name` in `dir` through the D13 path and returns its writer.
fn create(dir: &Path, name: &str, seed: u64) -> mochi_core::Result<ArchiveWriter<OsStorage>> {
    let steps = scripted_history();
    ArchiveWriter::<OsStorage>::create_in(
        &mut OsDir::open(dir).unwrap(),
        name,
        Box::new(SeqIds::new(seed)),
        test_options(),
        steps[0].tx.clone(),
        &Job::new().ctx(),
    )
    .map(|(w, _)| w)
}

/// Trigger a: a writer that opens an existing archive for appending removes
/// the empty legacy lock files beside it, orphan or not, including its own
/// archive's.
#[test]
fn legacy_lock_files_are_removed_when_a_writer_appends() {
    let (tmp, file, _) = archive(2);
    let gone = legacy(tmp.path(), "gone.mochi");
    let own = legacy(tmp.path(), NAME);
    let w = writer(&file).unwrap();
    assert!(!gone.exists(), "an orphan goes");
    assert!(!own.exists(), "so does the archive's own, held by nobody");
    assert_eq!(names_in(tmp.path()), [NAME]);
    w.close().unwrap();
}

/// Trigger b: creating an archive does the same, once it is published and
/// while its creator holds the lock.
#[test]
fn legacy_lock_files_are_removed_when_an_archive_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let gone = legacy(tmp.path(), "gone.mochi");
    let same_name = legacy(tmp.path(), NAME);
    let w = create(tmp.path(), NAME, 7).unwrap();
    assert!(!gone.exists() && !same_name.exists());
    assert_eq!(names_in(tmp.path()), [NAME]);
    w.close().unwrap();
}

/// Nothing sweeps on read-only use: opening and reading leave every legacy
/// lock file where it is.
#[test]
fn read_only_use_removes_nothing() {
    let (tmp, file, steps) = archive(2);
    let gone = legacy(tmp.path(), "gone.mochi");
    let own = legacy(tmp.path(), NAME);
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &ReadOptions::default()).unwrap();
    assert_eq!(read_state(&r, &head).unwrap(), steps[1].after);
    assert!(gone.exists() && own.exists());
}

/// What the sweep keeps, whatever else it finds: a lock file held by another
/// handle (the way an old build holds it), one with bytes, a link, a
/// directory, anything that is not named like a lock file, and anything in a
/// sub-directory. Released, the held one goes with the next writer.
#[test]
fn only_unheld_empty_regular_legacy_files_go() {
    let (tmp, file, _) = archive(2);
    let dir = tmp.path();

    // Archive present or absent does not matter.
    let present = legacy(dir, NAME);
    std::fs::write(dir.join("kept.mochi"), b"x").unwrap();
    let with_archive = legacy(dir, "kept.mochi");
    let orphan = legacy(dir, "orphan.mochi");

    // Held by someone else, with `File::try_lock` as old builds took it.
    let held = legacy(dir, "held.mochi");
    let holder = std::fs::File::open(&held).unwrap();
    holder.try_lock().unwrap();

    // Not what a legacy lock file is: bytes, a link, a directory, other names.
    let bytes = legacy_of(dir, "bytes.mochi");
    std::fs::write(&bytes, b"user data").unwrap();
    #[cfg(unix)]
    let link = {
        let link = legacy_of(dir, "link.mochi");
        std::os::unix::fs::symlink(&bytes, &link).unwrap();
        link
    };
    let as_dir = legacy_of(dir, "dir.mochi");
    std::fs::create_dir(&as_dir).unwrap();
    let bare = dir.join(LEGACY_LOCK_FILE_SUFFIX);
    std::fs::File::create(&bare).unwrap();
    let other = dir.join("notes.txt");
    std::fs::File::create(&other).unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    let nested = legacy(&dir.join("sub"), "deep.mochi");

    writer(&file).unwrap().close().unwrap();
    assert!(!present.exists() && !with_archive.exists() && !orphan.exists());
    #[cfg(unix)]
    let links = [&link];
    #[cfg(not(unix))]
    let links: [&PathBuf; 0] = [];
    for p in [&held, &bytes, &as_dir, &bare, &other, &nested]
        .into_iter()
        .chain(links)
    {
        assert!(p.symlink_metadata().is_ok(), "{} was removed", p.display());
    }
    assert!(dir.join("kept.mochi").exists());

    // Released, the held one is like any other.
    drop(holder);
    writer(&file).unwrap().close().unwrap();
    assert!(!held.exists());
    assert!(nested.exists() && bytes.exists());
}

/// Cleanup never fails the user's operation: a sweep that cannot remove
/// (read-only directory), cannot read the directory, or meets odd entries
/// leaves the writer working.
#[cfg(unix)]
#[test]
fn a_failing_sweep_never_fails_the_operation() {
    use mochi_core::storage::os::sweep_legacy_lock_files;
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(sweep_legacy_lock_files(Path::new("/nonexistent/mochi")), 0);

    let (tmp, file, steps) = archive(2);
    let gone = legacy(tmp.path(), "gone.mochi");
    let mode = std::fs::metadata(tmp.path()).unwrap().permissions().mode();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores directory permissions, so only assert the failure path
    // where it really is one.
    let removable = std::fs::File::create(tmp.path().join("probe")).is_ok();
    let mut w = writer(&file).unwrap();
    w.commit(steps[2].tx.clone(), &Job::new().ctx()).unwrap();
    w.close().unwrap();
    std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode)).unwrap();
    if !removable {
        assert!(gone.exists(), "nothing could be removed");
    }
}

/// Creators racing for one name, with a legacy lock file around for the
/// sweeps that all of them run, end with exactly one winner; every loser is
/// told the name exists and leaves the winner's archive and lock alone.
#[test]
fn creators_racing_end_with_one_winner() {
    let tmp = tempfile::tempdir().unwrap();
    for round in 0..30 {
        let name = format!("r{round}.mochi");
        let (dir, name) = (tmp.path(), &name);
        legacy(dir, name);
        let outcomes: Vec<_> = std::thread::scope(|s| {
            let creators: Vec<_> = (0..4u64)
                .map(|i| s.spawn(move || create(dir, name, 100 + i).map(|w| w.close().unwrap())))
                .collect();
            creators.into_iter().map(|c| c.join().unwrap()).collect()
        });
        let won = outcomes.iter().filter(|o| o.is_ok()).count();
        assert_eq!(won, 1, "round {round}: {outcomes:?}");
        for e in outcomes.iter().filter_map(|o| o.as_ref().err()) {
            assert_eq!(e.code, ErrorCode::DestinationExists, "round {round}: {e}");
        }
        // The winner's archive is intact and nothing else of this round is
        // left (the other creators' temporary files were removed).
        let r = OsReadStorage::open(dir.join(name)).unwrap();
        open_head(&r, &ReadOptions::default()).unwrap();
    }
    assert_eq!(names_in(tmp.path()).len(), 30);
}

/// A FIFO named like a lock file does not block a writer: the sweep neither
/// opens it blocking nor removes it (spec B.2.17 item 5).
#[cfg(unix)]
#[test]
fn a_fifo_named_like_a_lock_file_does_not_block_a_writer() {
    let (tmp, file, _) = archive(2);
    let fifo = tmp.path().join("x.mochi-lock");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.is_ok_and(|s| s.success()), "mkfifo");
    let (tx, rx) = std::sync::mpsc::channel();
    let f = file.clone();
    std::thread::spawn(move || {
        let _ = tx.send(writer(&f).map(|w| w.close().unwrap()).map_err(|e| e.code));
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(30)),
        Ok(Ok(())),
        "the writer was blocked or failed"
    );
    assert!(fifo.symlink_metadata().is_ok(), "the FIFO is kept");
}

/// **Windows, any filesystem.** CI formats FAT32, exFAT and ReFS volumes and
/// NTFS is the runner's own; for each it sets `MOCHI_LOCK_TEST_DIR` to a
/// directory on that volume (`.github/workflows/ci.yml`, "Writer lock on other
/// filesystems"). On such a volume: create an archive, append, read while a
/// writer holds the lock, and a second writer gets `LOCK_CONFLICT`. This
/// checks that a one-byte `LockFileEx` at offset 2^62 works there (spec
/// B.2.17 item 2); without the variable (an ordinary `cargo test`) there is
/// nothing to point at and the test returns at once, because the CI step
/// always sets it.
#[cfg(windows)]
#[test]
fn q54_windows_writer_lock_on_the_volume_named_by_the_environment() {
    let Some(root) = std::env::var_os("MOCHI_LOCK_TEST_DIR") else {
        eprintln!("MOCHI_LOCK_TEST_DIR is not set; the per-filesystem lock check is run by CI");
        return;
    };
    let dir = PathBuf::from(root);
    eprintln!("writer lock check in {}", dir.display());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(NAME);
    let steps = scripted_history();
    let read = |want: &Step, seq: u64| {
        let r = OsReadStorage::open(&file).unwrap();
        let head = open_head(&r, &ReadOptions::default()).unwrap();
        assert_eq!(head.seq(), seq);
        assert_eq!(read_state(&r, &head).unwrap(), want.after);
    };

    // Create: the temporary file is locked, renamed, and stays locked.
    let w = create(&dir, NAME, 7).unwrap();
    assert_eq!(
        writer(&file).map(|_| ()).unwrap_err().code,
        ErrorCode::LockConflict
    );
    read(&steps[0], 0);
    w.close().unwrap();

    // Append while a reader reads and a second writer is refused.
    let mut w = writer_seeded(&file, 11).unwrap();
    assert_eq!(
        writer(&file).map(|_| ()).unwrap_err().code,
        ErrorCode::LockConflict
    );
    read(&steps[0], 0);
    w.commit(steps[1].tx.clone(), &Job::new().ctx()).unwrap();
    read(&steps[1], 1);
    assert_eq!(
        writer(&file).map(|_| ()).unwrap_err().code,
        ErrorCode::LockConflict
    );
    w.close().unwrap();
    writer(&file).unwrap().close().unwrap();
}

/// **Windows.** A lock file 0.9.2 created is hidden; it is removed like any
/// other. And the writer lock is a byte range: a whole-file lock by another
/// handle (what `std` takes) is refused while a writer holds the archive, yet
/// that handle reads the committed archive.
#[cfg(windows)]
#[test]
fn windows_hidden_legacy_lock_file_is_removed_and_the_lock_does_not_block_reads() {
    use std::os::windows::fs::OpenOptionsExt;
    let (tmp, file, steps) = archive(2);
    let hidden = legacy_of(tmp.path(), NAME);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .attributes(0x2)
        .open(&hidden)
        .unwrap();
    let mut w = writer(&file).unwrap();
    assert!(!hidden.exists());
    w.commit(steps[2].tx.clone(), &Job::new().ctx()).unwrap();

    let other = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&file)
        .unwrap();
    assert!(matches!(
        other.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &ReadOptions::default()).unwrap();
    assert_eq!(read_state(&r, &head).unwrap(), steps[2].after);
    w.close().unwrap();
}
