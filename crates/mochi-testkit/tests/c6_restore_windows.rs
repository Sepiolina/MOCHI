//! C6 restoration on Windows (owner delegation 2026-10-06; see
//! `docs/c6-restore-platforms.md`). `OsRestoreDir` pins every directory it
//! holds (no `FILE_SHARE_DELETE`), so a directory in the restored tree
//! cannot be renamed away and replaced while restoration resolves names
//! through it, and it never opens through a reparse point.

#![cfg(windows)]

use std::path::Path;

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::manifest::{Attributes, Mtime};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{CaseBehavior, RestoreDir, Storage, StorageError};

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// **The race, prevented.** While restoration holds a directory, it cannot
/// be renamed or deleted, so it cannot be swapped for a junction or a
/// link; the same holds for the root. Once restoration lets go, both work.
#[test]
fn c6_windows_held_directories_cannot_be_moved_or_deleted() {
    let root = tempfile::tempdir().unwrap();
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    let mut sub = dir.create_dir(b"sub").unwrap();
    let empty = sub.create_dir(b"empty").unwrap();

    assert!(std::fs::rename(root.path().join("sub"), root.path().join("moved")).is_err());
    assert!(std::fs::remove_dir(root.path().join("sub/empty")).is_err());
    let elsewhere = tempfile::tempdir().unwrap();
    assert!(std::fs::rename(root.path(), elsewhere.path().join("root")).is_err());

    let mut f = sub.create_file(b".tmp").unwrap();
    f.append(b"restored bytes").unwrap();
    f.sync_data().unwrap();
    sub.publish_no_replace(b".tmp", b"file").unwrap();
    drop(f);
    assert_eq!(names(&root.path().join("sub")), ["empty", "file"]);

    drop((empty, sub, dir));
    std::fs::remove_dir(root.path().join("sub/empty")).unwrap();
    std::fs::rename(root.path().join("sub"), root.path().join("moved")).unwrap();
}

/// NTFS is case-insensitive: detected by the probe, which leaves nothing.
#[test]
fn c6_windows_case_detection() {
    let root = tempfile::tempdir().unwrap();
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    assert_eq!(dir.case_behavior().unwrap(), CaseBehavior::Insensitive);
    assert!(names(root.path()).is_empty());
    std::fs::write(root.path().join("Existing"), b"x").unwrap();
    assert!(dir.entry_exists(b"EXISTING").unwrap());
    assert!(!dir.entry_exists(b"absent").unwrap());
}

/// Planted symbolic links are collisions, never followed, and attributes
/// are never applied through them. Creating a symbolic link needs a
/// privilege (or developer mode); without it this part cannot be set up
/// and says so, while the rest of the suite still runs.
#[test]
fn c6_windows_planted_links_are_never_followed() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim");
    std::fs::write(&victim, b"precious").unwrap();
    let file_link = std::os::windows::fs::symlink_file(&victim, root.path().join("f"));
    let dir_link = std::os::windows::fs::symlink_dir(outside.path(), root.path().join("d"));
    if file_link.is_err() || dir_link.is_err() {
        eprintln!("symbolic links need a privilege here; planted-link checks not run");
        return;
    }
    let mut dir = OsRestoreDir::open(root.path()).unwrap();
    assert!(matches!(
        dir.create_file(b"f"),
        Err(StorageError::Exists { .. })
    ));
    assert!(matches!(
        dir.create_dir(b"d"),
        Err(StorageError::Exists { .. })
    ));
    assert!(dir.entry_exists(b"d").unwrap());
    let before = std::fs::metadata(&victim).unwrap().modified().unwrap();
    let a = Attributes {
        posix: None,
        windows: Some(mochi_core::manifest::WINDOWS_READONLY),
        mtime: Some(Mtime { secs: 0, nanos: 0 }),
    };
    let issues = dir.apply_attributes(b"f", EntryKind::File, &a);
    assert!(!issues.is_empty());
    let m = std::fs::metadata(&victim).unwrap();
    assert_eq!(m.modified().unwrap(), before);
    assert!(!m.permissions().readonly());
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert_eq!(names(outside.path()), ["victim"]);
    // A root that is itself a link is refused.
    assert!(OsRestoreDir::open(root.path().join("d")).is_err());
}
