//! C6 race-resistant restoration on Linux (owner decision 2026-10-06; plan
//! C6 "Open: `OsRestoreDir` is path-based"). `OsRestoreDir` works through
//! directory descriptors it holds, never through paths, so swapping a
//! directory for a symbolic link redirects nothing; nothing is ever opened
//! through a symbolic link; and a root other users could modify is refused
//! before anything is written.

#![cfg(target_os = "linux")]

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{open_head, ArchiveWriter, ReadOptions, Transaction};
use mochi_core::restore::{restore, RestoreOptions};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{CaseBehavior, RestoreDir, Storage, StorageError};
use mochi_core::{ErrorCode, MochiError};
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::{SeqIds, SimStorage};

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn is_root() -> bool {
    std::fs::metadata("/proc/self").unwrap().uid() == 0
}

/// A symbolic link planted where restoration creates an entry is a
/// collision: it is not followed, and its target is untouched.
#[test]
fn c6_planted_symlinks_are_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim"), b"precious").unwrap();
    symlink(outside.path(), root.path().join("d")).unwrap();
    symlink(outside.path().join("victim"), root.path().join("f")).unwrap();

    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    assert!(matches!(
        dir.create_dir(b"d"),
        Err(StorageError::Exists { .. })
    ));
    assert!(matches!(
        dir.create_file(b"f"),
        Err(StorageError::Exists { .. })
    ));
    assert!(dir.entry_exists(b"d").unwrap());
    // Attributes are never applied through a link either.
    let a = Attributes {
        posix: Some(PosixAttributes {
            mode: 0o777,
            uid: 0,
            gid: 0,
        }),
        windows: None,
        mtime: Some(Mtime { secs: 0, nanos: 0 }),
    };
    let before = std::fs::metadata(outside.path().join("victim")).unwrap();
    let issues = dir.apply_attributes(b"f", EntryKind::File, &a);
    assert!(!issues.is_empty());
    let after = std::fs::metadata(outside.path().join("victim")).unwrap();
    assert_eq!(before.mode(), after.mode());
    assert_eq!(before.mtime(), after.mtime());
    assert_eq!(names(outside.path()), ["victim"]);
    assert_eq!(
        std::fs::read(outside.path().join("victim")).unwrap(),
        b"precious"
    );
}

/// **The race itself.** After a directory is created, something renames it
/// away and puts a symbolic link to another directory in its place. Later
/// writes still land in the directory restoration created (now under its
/// new name), never through the link. A path-based implementation writes
/// into the link's target here.
#[test]
fn c6_a_directory_swapped_for_a_symlink_redirects_nothing() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    let mut sub = dir.create_dir(b"sub").unwrap();

    std::fs::rename(root.path().join("sub"), root.path().join("moved")).unwrap();
    symlink(outside.path(), root.path().join("sub")).unwrap();

    let mut f = sub.create_file(b".tmp").unwrap();
    f.append(b"restored bytes").unwrap();
    f.sync_data().unwrap();
    sub.publish_no_replace(b".tmp", b"file").unwrap();
    drop(f);
    let mut inner = sub.create_dir(b"inner").unwrap();
    inner.sync_directory().unwrap();
    sub.sync_directory().unwrap();

    assert!(
        names(outside.path()).is_empty(),
        "{:?}",
        names(outside.path())
    );
    assert_eq!(names(&root.path().join("moved")), ["file", "inner"]);
    assert_eq!(
        std::fs::read(root.path().join("moved/file")).unwrap(),
        b"restored bytes"
    );
}

/// Until attributes are applied, everything restoration creates is private
/// to the restoring user (`0700` directories, `0600` files), so no other
/// user can rename entries while they are written.
#[test]
fn c6_entries_are_private_until_attributes_apply() {
    let root = tempfile::tempdir().unwrap();
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    let mut sub = dir.create_dir(b"d").unwrap();
    let f = sub.create_file(b"f").unwrap();
    drop(f);
    let mode = |p: &str| std::fs::metadata(root.path().join(p)).unwrap().mode() & 0o7777;
    assert_eq!(mode("d"), 0o700);
    assert_eq!(mode("d/f"), 0o600);
}

/// A root other users could modify is refused before anything is written:
/// group- or world-writable without the sticky bit, or (as root) owned by
/// another user. Sticky world-writable roots (`/tmp`) are accepted.
#[test]
fn c6_a_root_others_can_modify_is_refused() {
    let refused = |p: &Path| {
        let e = MochiError::from(OsRestoreDir::open(p).unwrap_err());
        assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{e}");
        assert!(e.message.contains("Nothing was written"), "{e}");
        assert!(names(p).is_empty());
    };
    let root = tempfile::tempdir().unwrap();
    for m in [0o777, 0o775, 0o757] {
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(m)).unwrap();
        refused(root.path());
    }
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
    OsRestoreDir::open(root.path()).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    OsRestoreDir::open(root.path()).unwrap();
    if is_root() {
        std::os::unix::fs::chown(root.path(), Some(1000), Some(1000)).unwrap();
        refused(root.path());
    }
}

/// The case probe finds this filesystem case-sensitive and leaves nothing
/// behind. (Insensitive detection needs a case-folding filesystem; see the
/// checklist's C6 evidence.)
#[test]
fn c6_case_detection_on_the_real_filesystem() {
    let root = tempfile::tempdir().unwrap();
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    assert_eq!(dir.case_behavior().unwrap(), CaseBehavior::Sensitive);
    assert!(names(root.path()).is_empty());
    if let Ok(d) = std::env::var("MOCHI_CASEFOLD_DIR") {
        let mut dir = OsRestoreDir::open(&d).unwrap();
        assert_eq!(dir.case_behavior().unwrap(), CaseBehavior::Insensitive);
    }
}

/// A whole restore, with the root's existing entries checked in preflight:
/// a symbolic link already at a top-level name is reported as a collision
/// and its target is untouched.
#[test]
fn c6_restore_reports_a_planted_top_level_symlink() {
    let s = SimStorage::new();
    let mut tx = Transaction::new();
    tx.put_file(path("a"), b"archive a".to_vec(), Attributes::default());
    tx.put_file(path("b"), b"archive b".to_vec(), Attributes::default());
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    let head = open_head(&s, &ReadOptions::default()).unwrap();

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("victim"), b"precious").unwrap();
    symlink(outside.path().join("victim"), root.path().join("a")).unwrap();
    let r = restore(
        &s,
        &head,
        None,
        OsRestoreDir::open(root.path()).unwrap(),
        &RestoreOptions::default(),
        &ReadOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(r.exceptions.len(), 1, "{:?}", r.exceptions);
    assert_eq!(r.exceptions[0].kind.code(), ErrorCode::NameCollision);
    assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"archive b");
    assert_eq!(
        std::fs::read(outside.path().join("victim")).unwrap(),
        b"precious"
    );
    // Attributes::default() has no POSIX attributes: §10.4.1 defaults.
    let mode = std::fs::metadata(root.path().join("b")).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

/// **Insensitive detection and preflight on a real case-folding
/// filesystem** (ext4 casefold). Needs `MOCHI_CASEFOLD_DIR`: an empty
/// directory with the casefold attribute (`chattr +F`), owned by the user
/// running the test. CI job `casefold` provides one and runs this test with
/// `--ignored`; it fails, rather than passes, without the directory.
#[test]
#[ignore = "needs MOCHI_CASEFOLD_DIR on ext4 casefold (CI job `casefold`)"]
fn c6_casefold_destination() {
    let d = std::path::PathBuf::from(
        std::env::var("MOCHI_CASEFOLD_DIR").expect("MOCHI_CASEFOLD_DIR is not set"),
    );
    assert!(names(&d).is_empty(), "{:?}", names(&d));
    let mut dir = OsRestoreDir::open(&d).unwrap();
    assert_eq!(dir.case_behavior().unwrap(), CaseBehavior::Insensitive);
    assert!(names(&d).is_empty(), "the probe left {:?}", names(&d));

    let s = SimStorage::new();
    let mut tx = Transaction::new();
    tx.put_file(path("Readme"), b"upper".to_vec(), Attributes::default());
    tx.put_file(path("existing"), b"archive".to_vec(), Attributes::default());
    tx.put_file(path("readme"), b"lower".to_vec(), Attributes::default());
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    std::fs::write(d.join("EXISTING"), b"already here").unwrap();
    let go = |refuse: bool| {
        restore(
            &s,
            &head,
            None,
            OsRestoreDir::open(&d).unwrap(),
            &RestoreOptions {
                refuse_on_preflight_exceptions: refuse,
                ..RestoreOptions::default()
            },
            &ReadOptions::default(),
            &Job::new().ctx(),
        )
    };
    let e = go(true).unwrap_err();
    assert_eq!(e.code, ErrorCode::NameCollision, "{e}");
    assert_eq!(names(&d), ["EXISTING"]);

    let r = go(false).unwrap();
    assert_eq!(r.case_behavior, CaseBehavior::Insensitive);
    let mut collided: Vec<String> = r
        .exceptions
        .iter()
        .map(|e| {
            assert_eq!(e.kind.code(), ErrorCode::NameCollision);
            String::from_utf8_lossy(e.path.as_stored()).into_owned()
        })
        .collect();
    collided.sort();
    assert_eq!(collided, ["existing", "readme"]);
    assert_eq!(names(&d), ["EXISTING", "Readme"]);
    assert_eq!(std::fs::read(d.join("readme")).unwrap(), b"upper");
    assert_eq!(std::fs::read(d.join("existing")).unwrap(), b"already here");
}
