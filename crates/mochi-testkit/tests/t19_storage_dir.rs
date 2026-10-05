//! T19: the directory operations behind creation (D13) and quarantine (D14),
//! gate G7: create an exclusive sibling file, publish it without replacing,
//! remove a temporary file only under its lock.
//!
//! One conformance suite runs against the real `OsDir` (in a temporary
//! directory) and the simulated `SimDir`; fault injection then covers each
//! operation on `SimDir`.

use std::path::Path;

use mochi_core::storage::os::OsDir;
use mochi_core::storage::{
    DirectoryDurability, ReadStorage, RemoveOutcome, Storage, StorageDir, StorageError,
};
use mochi_testkit::{is_halted, CrashMode, DirFault, DirOp, SimDir, SimStorage};

fn bytes_of(s: &impl ReadStorage) -> Vec<u8> {
    let mut v = vec![0; s.size().unwrap() as usize];
    s.read_exact_at(0, &mut v).unwrap();
    v
}

fn exists(e: &StorageError, want: &str) -> bool {
    matches!(e, StorageError::Exists { name } if name == want)
}

/// The conformance suite. `read` returns the bytes currently at a name.
fn conformance<D: StorageDir>(mut dir: D, read: impl Fn(&str) -> Option<Vec<u8>>) {
    // Exclusive creation, locked.
    let mut t = dir.create_exclusive("a.tmp").unwrap();
    t.append(b"hello").unwrap();
    t.sync_data().unwrap();
    assert!(exists(
        &dir.create_exclusive("a.tmp").map(|_| ()).unwrap_err(),
        "a.tmp"
    ));
    assert_eq!(
        dir.remove_if_unlocked("a.tmp").unwrap(),
        RemoveOutcome::Locked
    );
    assert_eq!(read("a.tmp").as_deref(), Some(&b"hello"[..]));

    // Publication without replacing: the old name is gone, the new one holds
    // the bytes, the handle still works.
    dir.publish_no_replace("a.tmp", "a").unwrap();
    assert_eq!(read("a.tmp"), None);
    assert_eq!(read("a").as_deref(), Some(&b"hello"[..]));
    t.append(b"!").unwrap();
    assert_eq!(read("a").as_deref(), Some(&b"hello!"[..]));
    assert_eq!(
        dir.remove_if_unlocked("a.tmp").unwrap(),
        RemoveOutcome::Missing
    );
    assert!(exists(
        &dir.create_exclusive("a").map(|_| ()).unwrap_err(),
        "a"
    ));

    // Never replaces: an existing destination and the source are untouched.
    let mut u = dir.create_exclusive("b.tmp").unwrap();
    u.append(b"other").unwrap();
    let e = dir.publish_no_replace("b.tmp", "a").unwrap_err();
    assert!(exists(&e, "a"), "{e}");
    assert_eq!(read("a").as_deref(), Some(&b"hello!"[..]));
    assert_eq!(read("b.tmp").as_deref(), Some(&b"other"[..]));

    // Removing one's own locked temporary file.
    dir.discard(u, "b.tmp").unwrap();
    assert_eq!(read("b.tmp"), None);

    // Cleanup removes only what it can lock.
    let mut v = dir.create_exclusive("c.tmp").unwrap();
    assert_eq!(
        dir.remove_if_unlocked("c.tmp").unwrap(),
        RemoveOutcome::Locked
    );
    assert!(read("c.tmp").is_some());
    v.unlock().unwrap();
    drop(v);
    assert_eq!(
        dir.remove_if_unlocked("c.tmp").unwrap(),
        RemoveOutcome::Removed
    );
    assert_eq!(read("c.tmp"), None);
    assert_eq!(
        dir.remove_if_unlocked("c.tmp").unwrap(),
        RemoveOutcome::Missing
    );

    // Names, not paths.
    for bad in ["", ".", "..", "x/y", "x\\y", "x\0y"] {
        let invalid = |e: StorageError| matches!(e, StorageError::InvalidName { .. });
        assert!(
            invalid(dir.create_exclusive(bad).map(|_| ()).unwrap_err()),
            "{bad:?}"
        );
        assert!(
            invalid(dir.publish_no_replace("a", bad).unwrap_err()),
            "{bad:?}"
        );
        assert!(invalid(dir.remove_if_unlocked(bad).unwrap_err()), "{bad:?}");
    }

    let d = dir.sync_directory().unwrap();
    if cfg!(windows) {
        assert!(matches!(d, DirectoryDurability::Unconfirmed(_)));
    }
    drop(t);
}

#[test]
fn t19_conformance_os_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let read = move |name: &str| std::fs::read(Path::new(&root).join(name)).ok();
    conformance(OsDir::open(tmp.path()).unwrap(), read);
}

#[test]
fn t19_conformance_sim_dir() {
    let dir = SimDir::new();
    let view = dir.clone();
    conformance(dir, move |name| view.file(name).map(|f| f.contents()));
}

#[test]
fn t19_os_dir_rejects_a_file_path() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("f");
    std::fs::write(&file, b"x").unwrap();
    assert!(OsDir::open(&file).is_err());
    assert!(OsDir::containing(&file).is_ok());
}

/// Two handles on one OS directory: an exclusive create races to one
/// winner; the loser gets `Exists` and must not delete the winner's file.
#[test]
fn t19_os_dir_two_creators_one_winner() {
    let tmp = tempfile::tempdir().unwrap();
    let mut a = OsDir::open(tmp.path()).unwrap();
    let mut b = OsDir::open(tmp.path()).unwrap();
    let mut w = a.create_exclusive("x.tmp").unwrap();
    w.append(b"winner").unwrap();
    assert!(exists(
        &b.create_exclusive("x.tmp").map(|_| ()).unwrap_err(),
        "x.tmp"
    ));
    assert_eq!(
        b.remove_if_unlocked("x.tmp").unwrap(),
        RemoveOutcome::Locked
    );
    assert_eq!(std::fs::read(tmp.path().join("x.tmp")).unwrap(), b"winner");
}

// ---- fault injection (SimDir) -------------------------------------------------------

fn halted(e: &StorageError) -> bool {
    is_halted(e)
}

#[test]
fn t19_failed_create_creates_nothing() {
    let mut d = SimDir::with_faults([DirFault::FailCreate { index: 0 }]);
    assert!(d.create_exclusive("a.tmp").is_err());
    assert!(d.names().is_empty());
    d.create_exclusive("a.tmp").unwrap();
    assert_eq!(d.names(), ["a.tmp"]);
}

#[test]
fn t19_halt_before_each_operation_changes_nothing() {
    // Operation indices: 0 create, 1 publish, 2 sync, 3 create, 4 discard.
    for halt_at in 0..5 {
        let mut d = SimDir::with_faults([DirFault::HaltBeforeOp { index: halt_at }]);
        let mut steps: Vec<(Vec<String>, Vec<String>)> = Vec::new();
        let mut run = || -> Result<(), StorageError> {
            let mut t = d.create_exclusive("a.tmp")?;
            t.append(b"data").unwrap();
            t.sync_data().unwrap();
            steps.push((d.names(), d.durable_names()));
            d.publish_no_replace("a.tmp", "a")?;
            steps.push((d.names(), d.durable_names()));
            d.sync_directory()?;
            steps.push((d.names(), d.durable_names()));
            let u = d.create_exclusive("b.tmp")?;
            steps.push((d.names(), d.durable_names()));
            d.discard(u, "b.tmp")?;
            Ok(())
        };
        let e = run().unwrap_err();
        assert!(halted(&e), "halt at {halt_at}: {e}");
        assert!(d.is_halted());
        // Nothing after the halt took effect: state equals the last snapshot.
        let (names, durable) = steps.last().cloned().unwrap_or_default();
        assert_eq!(d.names(), names, "halt at {halt_at}");
        assert_eq!(d.durable_names(), durable, "halt at {halt_at}");
        assert!(halted(&d.sync_directory().unwrap_err()));
    }
}

#[test]
fn t19_failed_publish_changes_nothing() {
    let mut d = SimDir::with_faults([DirFault::FailPublish { index: 0 }]);
    let mut t = d.create_exclusive("a.tmp").unwrap();
    t.append(b"data").unwrap();
    assert!(d.publish_no_replace("a.tmp", "a").is_err());
    assert_eq!(d.names(), ["a.tmp"]);
    d.publish_no_replace("a.tmp", "a").unwrap();
    assert_eq!(d.names(), ["a"]);
}

/// A crash between link and unlink leaves both names on the same bytes;
/// after the crash nobody holds the temporary file's lock, so cleanup
/// removes the temporary name and the published file survives intact.
#[test]
fn t19_crash_between_link_and_unlink() {
    let mut d = SimDir::with_faults([DirFault::HaltBetweenLinkAndUnlink { index: 0 }]);
    let mut t = d.create_exclusive("a.tmp").unwrap();
    t.append(b"payload").unwrap();
    t.sync_data().unwrap();
    let e = d.publish_no_replace("a.tmp", "a").unwrap_err();
    assert!(halted(&e));
    assert_eq!(d.names(), ["a", "a.tmp"]);
    assert!(d.file("a").unwrap().same_file(&d.file("a.tmp").unwrap()));
    assert_eq!(
        d.trace(),
        [
            DirOp::Create {
                name: "a.tmp".into()
            },
            DirOp::Link {
                from: "a.tmp".into(),
                to: "a".into()
            },
        ]
    );

    let mut after = d.crash_image(CrashMode::KeepAll);
    assert_eq!(after.names(), ["a", "a.tmp"]);
    assert_eq!(
        after.remove_if_unlocked("a.tmp").unwrap(),
        RemoveOutcome::Removed
    );
    assert_eq!(after.names(), ["a"]);
    assert_eq!(after.file("a").unwrap().contents(), b"payload");
}

#[test]
fn t19_failed_removal_removes_nothing() {
    let mut d = SimDir::with_faults([
        DirFault::FailRemove { index: 0 },
        DirFault::FailRemove { index: 1 },
    ]);
    let t = d.create_exclusive("a.tmp").unwrap();
    assert!(d.discard(t.clone(), "a.tmp").is_err());
    assert_eq!(d.names(), ["a.tmp"]);
    let mut t2 = t.clone();
    t2.unlock().unwrap();
    assert!(d.remove_if_unlocked("a.tmp").is_err());
    assert_eq!(d.names(), ["a.tmp"]);
    // The failed attempt released its probe lock: the next one succeeds.
    assert_eq!(
        d.remove_if_unlocked("a.tmp").unwrap(),
        RemoveOutcome::Removed
    );
}

#[test]
fn t19_discard_refuses_another_file() {
    let mut d = SimDir::new();
    let _a = d.create_exclusive("a.tmp").unwrap();
    let b = d.create_exclusive("b.tmp").unwrap();
    assert!(d.discard(b, "a.tmp").is_err());
    assert_eq!(d.names(), ["a.tmp", "b.tmp"]);
}

/// A published name survives power loss only after a confirmed directory
/// sync; a failed or unconfirmed one persists nothing.
#[test]
fn t19_directory_durability() {
    let mut d = SimDir::with_faults([
        DirFault::FailSyncDirectory { index: 0 },
        DirFault::UnconfirmedSyncDirectory { index: 1 },
    ]);
    let mut t = d.create_exclusive("a.tmp").unwrap();
    t.append(b"payload").unwrap();
    t.sync_data().unwrap();
    d.publish_no_replace("a.tmp", "a").unwrap();
    assert!(d.crash_image(CrashMode::SyncedOnly).names().is_empty());
    assert!(d.crash_image(CrashMode::KeepAll).names() == ["a"]);

    assert!(d.sync_directory().is_err());
    assert!(d.durable_names().is_empty());
    assert!(matches!(
        d.sync_directory().unwrap(),
        DirectoryDurability::Unconfirmed(_)
    ));
    assert!(d.durable_names().is_empty());
    assert_eq!(d.sync_directory().unwrap(), DirectoryDurability::Confirmed);
    let after = d.crash_image(CrashMode::SyncedOnly);
    assert_eq!(after.names(), ["a"]);
    assert_eq!(after.file("a").unwrap().contents(), b"payload");
}

/// Unsynced bytes of a published file follow the file's own model.
#[test]
fn t19_file_bytes_follow_their_own_sync() {
    let mut d = SimDir::new();
    let mut t = d.create_exclusive("a.tmp").unwrap();
    t.append(b"synced").unwrap();
    t.sync_data().unwrap();
    t.append(b"+unsynced").unwrap();
    d.publish_no_replace("a.tmp", "a").unwrap();
    d.sync_directory().unwrap();
    let after = d.crash_image(CrashMode::SyncedOnly);
    assert_eq!(bytes_of(&after.file("a").unwrap()), b"synced");
    let fixture = SimStorage::from_bytes(b"old".to_vec());
    after.insert("old", fixture);
    assert_eq!(after.durable_names(), ["a", "old"]);
}
