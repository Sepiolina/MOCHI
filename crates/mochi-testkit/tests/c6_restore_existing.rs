//! Restoring into a destination that already has some of the names (owner
//! decision 2026-11-11; spec §10.4 forbids only *silent* overwriting):
//! `OnExisting::Keep` (the default, unchanged), `Merge` (enter existing
//! folders, add what they lack, keep existing files) and `Replace` (as Merge,
//! and an existing regular file goes to the Recycle Bin / Trash, after the
//! restored file has been verified, never before). Nothing else is ever
//! replaced: a file where the archive has a folder or the reverse, a link,
//! or a folder that may not be entered stays a `NAME_COLLISION`.

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::manifest::{Attributes, Mtime};
use mochi_core::publish::{open_head, ArchiveWriter, ReadOptions, Transaction};
use mochi_core::restore::{
    preflight_selected, restore, ExceptionKind, OnExisting, RestoreOptions, RestoreReport,
};
use mochi_core::storage::RestoreDir;
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::{SeqIds, SimStorage, SimTree};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// An archive with one commit: `None` is a directory, `Some` a file. Every
/// entry carries an mtime, so applied attributes can be seen.
fn archive_with(entries: &[(&str, Option<&[u8]>)]) -> SimStorage {
    let s = SimStorage::new();
    let mut tx = Transaction::new();
    let a = Attributes {
        posix: None,
        windows: None,
        mtime: Some(Mtime {
            secs: 1_700_000_000,
            nanos: 0,
        }),
    };
    for (p, c) in entries {
        match c {
            None => tx.put_dir(path(p), a),
            Some(b) => tx.put_file(path(p), b.to_vec(), a),
        };
    }
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    s
}

fn run<D: RestoreDir>(s: &SimStorage, root: D, on_existing: OnExisting) -> RestoreReport {
    let head = open_head(s, &opts()).unwrap();
    restore(
        s,
        &head,
        None,
        root,
        &RestoreOptions {
            on_existing,
            ..RestoreOptions::default()
        },
        &opts(),
        &Job::new().ctx(),
    )
    .unwrap()
}

fn exception(r: &RestoreReport, p: &str) -> Option<ExceptionKind> {
    r.exceptions
        .iter()
        .find(|e| e.path.as_stored() == p.as_bytes())
        .map(|e| e.kind.clone())
}

fn no_temporaries(paths: &[String]) {
    assert!(
        paths.iter().all(|p| !p.contains(".mochi-restore.")),
        "{paths:?}"
    );
}

/// The archive used by most tests: a folder with two files, a top file.
fn docs() -> SimStorage {
    archive_with(&[
        ("docs", None),
        ("docs/a.txt", Some(b"archive a")),
        ("docs/new.txt", Some(b"archive new")),
        ("top.txt", Some(b"archive top")),
    ])
}

/// A destination that already has the folder and some of the files.
fn crowded() -> SimTree {
    let tree = SimTree::new();
    tree.insert_file("docs/a.txt", b"mine a");
    tree.insert_file("docs/notes.txt", b"my notes");
    tree.insert_file("top.txt", b"mine top");
    tree
}

/// Keep (the default) is unchanged: an existing folder is a collision and
/// nothing under it is written.
#[test]
fn keep_is_unchanged_an_existing_folder_blocks_its_subtree() {
    let tree = crowded();
    let r = run(&docs(), tree.clone(), OnExisting::Keep);
    assert_eq!(exception(&r, "docs"), Some(ExceptionKind::Collision));
    assert_eq!(
        exception(&r, "docs/new.txt"),
        Some(ExceptionKind::ParentNotRestored {
            cause: ErrorCode::NameCollision
        })
    );
    assert_eq!(exception(&r, "top.txt"), Some(ExceptionKind::Collision));
    assert!(tree.file(b"docs/new.txt").is_none());
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"mine a");
    assert_eq!((r.files, r.merged_directories, r.replaced), (0, 0, 0));
    assert!(tree.set_aside_files().is_empty());
}

/// Merge adds what the folder lacks and keeps every existing file and its
/// bytes; the folder that was entered is not counted as restored and gets
/// none of the archive's attributes (it was the user's).
#[test]
fn merge_adds_only_what_is_missing_and_keeps_existing_files() {
    let tree = crowded();
    let r = run(&docs(), tree.clone(), OnExisting::Merge);
    assert_eq!(tree.file(b"docs/new.txt").unwrap(), b"archive new");
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"mine a");
    assert_eq!(tree.file(b"docs/notes.txt").unwrap(), b"my notes");
    assert_eq!(tree.file(b"top.txt").unwrap(), b"mine top");
    assert_eq!(exception(&r, "docs/a.txt"), Some(ExceptionKind::Collision));
    assert_eq!(exception(&r, "top.txt"), Some(ExceptionKind::Collision));
    assert_eq!(exception(&r, "docs"), None);
    assert_eq!(
        (r.files, r.directories, r.merged_directories, r.replaced),
        (1, 0, 1, 0)
    );
    assert!(
        tree.attributes(b"docs").is_none(),
        "the user's folder is not touched"
    );
    assert!(tree.attributes(b"docs/new.txt").is_some());
    assert!(tree.set_aside_files().is_empty());
    no_temporaries(&tree.paths());
}

/// Replace moves each existing regular file to the Bin (its bytes go with
/// it) and puts the archive's in its place; a file the user has that the
/// archive does not is never touched.
#[test]
fn replace_moves_existing_files_to_the_bin_and_writes_the_archive_s() {
    let tree = crowded();
    let r = run(&docs(), tree.clone(), OnExisting::Replace);
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"archive a");
    assert_eq!(tree.file(b"docs/new.txt").unwrap(), b"archive new");
    assert_eq!(tree.file(b"top.txt").unwrap(), b"archive top");
    assert_eq!(tree.file(b"docs/notes.txt").unwrap(), b"my notes");
    let mut binned = tree.set_aside_files();
    binned.sort();
    assert_eq!(
        binned,
        [
            ("docs/a.txt".to_owned(), b"mine a".to_vec()),
            ("top.txt".to_owned(), b"mine top".to_vec()),
        ]
    );
    assert_eq!((r.files, r.merged_directories, r.replaced), (3, 1, 2));
    no_temporaries(&tree.paths());
}

/// A file where the archive has a folder, a folder where it has a file, and
/// anything that is neither (a link, a device) are never entered or
/// replaced, in any mode.
#[test]
fn a_different_kind_or_a_link_is_never_replaced_or_entered() {
    let s = archive_with(&[
        ("was_file", None),
        ("was_file/x", Some(b"x")),
        ("was_dir", Some(b"file")),
        ("link", None),
        ("link/y", Some(b"y")),
        ("special", Some(b"z")),
    ]);
    for mode in [OnExisting::Merge, OnExisting::Replace] {
        let tree = SimTree::new();
        tree.insert_file("was_file", b"mine");
        tree.insert_dir("was_dir");
        tree.insert_other("link");
        tree.insert_other("special");
        let r = run(&s, tree.clone(), mode);
        for p in ["was_file", "was_dir", "link", "special"] {
            assert_eq!(
                exception(&r, p),
                Some(ExceptionKind::Collision),
                "{mode:?} {p}"
            );
        }
        assert_eq!(tree.file(b"was_file").unwrap(), b"mine");
        assert!(tree.file(b"link/y").is_none());
        assert!(tree.set_aside_files().is_empty(), "{mode:?}");
        assert_eq!(r.files, 0, "{mode:?}");
    }
}

/// A folder the backend will not enter (on Linux, one other users can
/// modify) is a collision, and its subtree is skipped. The preflight says
/// which collisions are such a folder, and which are two names of the
/// archive's own that the destination treats as one, so the window can say why.
#[test]
fn a_folder_that_may_not_be_entered_is_a_collision() {
    let s = archive_with(&[("docs", None), ("x", Some(b"x")), ("X", Some(b"X"))]);
    let head = open_head(&s, &opts()).unwrap();
    let tree = SimTree::new().case_insensitive();
    tree.insert_dir("docs");
    tree.close_dir("docs");
    let mut root = tree.clone();
    let (pre, _) = preflight_selected(&head, &[], &mut root, OnExisting::Replace).unwrap();
    let only = |set: &std::collections::BTreeSet<mochi_core::catalog::path::ArchivePath>| {
        set.iter()
            .map(|p| String::from_utf8_lossy(p.as_stored()).into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(only(&pre.not_entered), ["docs"]);
    // Byte order puts "X" first, so "x" is the one that collides.
    assert_eq!(only(&pre.same_name_in_archive), ["x"]);
    assert_eq!(pre.exceptions.len(), 2);

    let tree = crowded();
    tree.close_dir("docs");
    let r = run(&docs(), tree.clone(), OnExisting::Replace);
    assert_eq!(exception(&r, "docs"), Some(ExceptionKind::Collision));
    assert!(matches!(
        exception(&r, "docs/a.txt"),
        Some(ExceptionKind::ParentNotRestored { .. })
    ));
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"mine a");
    assert_eq!(tree.set_aside_files().len(), 1, "only top.txt");
}

/// If the Bin is not available, the existing file keeps its name and its
/// bytes, the restored copy is discarded, and the report says why.
#[test]
fn when_the_bin_fails_the_existing_file_is_kept() {
    let tree = crowded();
    tree.fail_set_aside();
    let r = run(&docs(), tree.clone(), OnExisting::Replace);
    assert_eq!(tree.file(b"docs/a.txt").unwrap(), b"mine a");
    assert_eq!(tree.file(b"top.txt").unwrap(), b"mine top");
    assert_eq!(tree.file(b"docs/new.txt").unwrap(), b"archive new");
    match exception(&r, "top.txt") {
        Some(ExceptionKind::Failed { code, message }) => {
            assert_eq!(code, ErrorCode::IoError);
            assert!(message.contains("so it was kept"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.replaced, 0);
    no_temporaries(&tree.paths());
}

/// The restored file is verified before anything is moved: a damaged file
/// in the archive leaves the user's file where it was.
#[test]
fn a_damaged_file_never_costs_the_user_their_file() {
    let s = archive_with(&[("big", Some(&[7u8; 300_000]))]);
    let head = open_head(&s, &opts()).unwrap();
    let (_, extents) = {
        let snap = head.catalog.replay(None).unwrap();
        let e = snap.get(&path("big")).unwrap();
        head.catalog.file_version(&e.version).unwrap().unwrap()
    };
    let ExtentSource::Chunk { chunk, .. } = extents[0].source else {
        panic!("a chunk")
    };
    let at = head.catalog.object_location(&chunk).unwrap().unwrap();
    let mut bytes = s.contents();
    bytes[at as usize + 20] ^= 0x10;
    let damaged = SimStorage::from_bytes(bytes);
    let tree = SimTree::new();
    tree.insert_file("big", b"mine");
    let r = run(&damaged, tree.clone(), OnExisting::Replace);
    assert!(matches!(
        exception(&r, "big"),
        Some(ExceptionKind::Integrity { .. })
    ));
    assert_eq!(tree.file(b"big").unwrap(), b"mine");
    assert!(tree.set_aside_files().is_empty());
    no_temporaries(&tree.paths());
}

/// The preflight says what each mode would do, and writes nothing: with
/// Replace, which folders are entered and which files would go to the Bin;
/// with Keep, the same names are collisions.
#[test]
fn the_preflight_reports_merges_and_replacements_without_writing() {
    let s = docs();
    let head = open_head(&s, &opts()).unwrap();
    let tree = crowded();
    let before = tree.paths();
    let mut root = tree.clone();
    let (pre, total) = preflight_selected(&head, &[], &mut root, OnExisting::Replace).unwrap();
    assert_eq!(total, 4);
    assert_eq!(tree.paths(), before, "nothing written");
    let names = |set: &std::collections::BTreeSet<_>| -> Vec<String> {
        set.iter()
            .map(|p: &mochi_core::catalog::path::ArchivePath| {
                String::from_utf8_lossy(p.as_stored()).into_owned()
            })
            .collect()
    };
    assert_eq!(names(&pre.merged), ["docs"]);
    assert_eq!(names(&pre.replaced), ["docs/a.txt", "top.txt"]);
    assert!(pre.exceptions.is_empty());

    let (keep, _) = preflight_selected(&head, &[], &mut root, OnExisting::Keep).unwrap();
    assert!(keep.merged.is_empty() && keep.replaced.is_empty());
    assert_eq!(keep.exceptions.len(), 2, "docs and top.txt");
}

/// On a case-insensitive destination the folder and the file are found
/// under the destination's own spelling.
#[test]
fn case_insensitive_destinations_merge_and_replace_under_their_own_spelling() {
    let tree = SimTree::new().case_insensitive();
    tree.insert_file("DOCS/A.TXT", b"mine a");
    let r = run(&docs(), tree.clone(), OnExisting::Replace);
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(r.merged_directories, 1);
    assert_eq!(
        tree.set_aside_files(),
        [("DOCS/A.TXT".to_owned(), b"mine a".to_vec())]
    );
    assert_eq!(tree.file(b"DOCS/a.txt").unwrap(), b"archive a");
}

/// On the real filesystem (Linux): the existing folder is entered, an
/// existing file goes to the place given (a stand-in for the Trash), a
/// symbolic link to a folder is never entered, and a folder other users can
/// modify is not entered either. Without a place given, nothing is replaced.
#[cfg(target_os = "linux")]
#[test]
fn on_linux_merge_and_replace_go_through_the_held_directory() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use mochi_core::storage::os::OsRestoreDir;

    let s = archive_with(&[
        ("docs", None),
        ("docs/a.txt", Some(b"archive a")),
        ("docs/new.txt", Some(b"archive new")),
        ("linked", None),
        ("linked/x", Some(b"x")),
        ("open", None),
        ("open/y", Some(b"y")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/a.txt"), b"mine a").unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), root.join("linked")).unwrap();
    std::fs::create_dir(root.join("open")).unwrap();
    std::fs::set_permissions(root.join("open"), std::fs::Permissions::from_mode(0o777)).unwrap();

    // Without a place for existing files, Replace keeps them.
    let r = run(&s, OsRestoreDir::open(root).unwrap(), OnExisting::Replace);
    assert_eq!(std::fs::read(root.join("docs/a.txt")).unwrap(), b"mine a");
    assert!(matches!(
        exception(&r, "docs/a.txt"),
        Some(ExceptionKind::Failed { .. })
    ));
    std::fs::remove_file(root.join("docs/new.txt")).unwrap();

    let bin_dir = bin.path().to_path_buf();
    let to_bin: Arc<mochi_core::storage::os::SetAside> = Arc::new(move |p: &std::path::Path| {
        let name = p.file_name().unwrap();
        std::fs::rename(p, bin_dir.join(name))
    });
    let r = run(
        &s,
        OsRestoreDir::open(root).unwrap().with_set_aside(to_bin),
        OnExisting::Replace,
    );
    assert_eq!(
        std::fs::read(root.join("docs/a.txt")).unwrap(),
        b"archive a"
    );
    assert_eq!(
        std::fs::read(root.join("docs/new.txt")).unwrap(),
        b"archive new"
    );
    assert_eq!(std::fs::read(bin.path().join("a.txt")).unwrap(), b"mine a");
    // The link is never followed: nothing appears where it points.
    assert_eq!(exception(&r, "linked"), Some(ExceptionKind::Collision));
    assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    // A folder other users can modify is not entered.
    assert_eq!(exception(&r, "open"), Some(ExceptionKind::Collision));
    assert!(!root.join("open/y").exists());
    assert_eq!((r.merged_directories, r.replaced), (1, 1));
}
