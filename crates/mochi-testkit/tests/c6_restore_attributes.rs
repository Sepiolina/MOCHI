//! C6 attribute restoration (plan O6; spec §10.4.1: "Restoration MUST
//! report unrestorable attributes as exceptions").

use mochi_core::commit::Metadata;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{
    commit_history, open_head, promised_attributes, ArchiveWriter, ReadOptions, Transaction,
};
use mochi_core::report::Severity;
use mochi_core::restore::{restore, RestoreOptions, RestoreReport};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{AttributeKind, RestoreDir};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{build, path, scripted_history, test_options, Content, Job};
use mochi_testkit::{SeqIds, SimStorage, SimTree};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn posix(mode: u32) -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode,
            uid: 1000,
            gid: 1000,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 1_600_000_000,
            nanos: 123_456_789,
        }),
    }
}

fn archive_with(entries: &[(&str, Option<&[u8]>, Attributes)]) -> SimStorage {
    let s = SimStorage::new();
    let mut tx = Transaction::new();
    for (p, c, a) in entries {
        match c {
            None => tx.put_dir(path(p), *a),
            Some(b) => tx.put_file(path(p), b.to_vec(), *a),
        };
    }
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    s
}

fn run<D: RestoreDir>(s: &SimStorage, root: D, options: RestoreOptions) -> RestoreReport {
    let head = open_head(s, &opts()).unwrap();
    restore(s, &head, None, root, &options, &opts(), &Job::new().ctx()).unwrap()
}

/// Every restored entry gets exactly its promised attributes, and
/// directories get theirs after their contents, deepest first.
#[test]
fn c6_attributes_are_applied_and_directories_last() {
    let s = SimStorage::new();
    build(s.clone(), 31, &scripted_history()).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let promised = promised_attributes(&s, &head, &opts()).unwrap();
    let tree = SimTree::new();
    let r = run(&s, tree.clone(), RestoreOptions::default());
    assert!(r.complete() && r.attributes_complete(), "{r:?}");
    let snap = head.catalog.replay(None).unwrap();
    for (p, e) in snap.iter() {
        assert_eq!(
            tree.attributes(p.as_stored()),
            Some(promised[&e.version]),
            "{p:?}"
        );
    }
    let order = tree.applied_order();
    let at = |p: &str| order.iter().position(|x| x == p).unwrap();
    assert!(at("docs") > at("docs/a.txt") && at("docs") > at("docs/empty"));
}

/// Setuid and setgid are removed unless requested, and the removal is
/// reported (plan O6).
#[test]
fn c6_setid_only_on_request() {
    let s = archive_with(&[
        ("tool", Some(b"#!/bin/sh"), posix(0o4755)),
        ("group", Some(b"g"), posix(0o2750)),
        ("sticky", None, posix(0o1777)),
    ]);
    let tree = SimTree::new();
    let r = run(&s, tree.clone(), RestoreOptions::default());
    let mode = |t: &SimTree, p: &[u8]| t.attributes(p).unwrap().posix.unwrap().mode;
    assert_eq!(mode(&tree, b"tool"), 0o755);
    assert_eq!(mode(&tree, b"group"), 0o750);
    assert_eq!(
        mode(&tree, b"sticky"),
        0o1777,
        "the sticky bit is not setid"
    );
    let setid: Vec<_> = r
        .attribute_exceptions
        .iter()
        .filter(|e| e.issue.attribute == AttributeKind::SetId)
        .collect();
    assert_eq!(setid.len(), 2);

    let tree = SimTree::new();
    let r = run(
        &s,
        tree.clone(),
        RestoreOptions {
            restore_setid: true,
        },
    );
    assert!(r.attributes_complete(), "{:?}", r.attribute_exceptions);
    assert_eq!(mode(&tree, b"tool"), 0o4755);
    assert_eq!(mode(&tree, b"group"), 0o2750);
}

/// An attribute the destination refuses is an exception on every entry it
/// affects, summarised as one finding per kind with a count.
#[test]
fn c6_refused_attributes_are_reported_once_per_kind() {
    let s = SimStorage::new();
    build(s.clone(), 32, &scripted_history()).unwrap();
    let tree = SimTree::new().unprivileged();
    let r = run(&s, tree.clone(), RestoreOptions::default());
    assert!(r.complete());
    let entries = scripted_history()[2].after.len();
    assert_eq!(r.attribute_exceptions.len(), entries);
    assert!(r
        .attribute_exceptions
        .iter()
        .all(|e| e.issue.attribute == AttributeKind::Ownership));
    let f = r.findings();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].code, ErrorCode::AttributeNotRestored);
    assert_eq!(f[0].severity, Severity::Warning);
    assert!(
        f[0].message
            .as_ref()
            .unwrap()
            .contains(&format!("{entries} entries")),
        "{f:?}"
    );
}

/// A damaged snapshot manifest makes attributes unavailable; content is
/// still restored and verified, and the report says what was not done.
#[test]
fn c6_attributes_unavailable_content_still_restored() {
    let s = SimStorage::new();
    build(s.clone(), 33, &scripted_history()).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let base = commit_history(&s, &opts())
        .unwrap()
        .into_iter()
        .find(|h| h.commit.seq == head.segment.base_seq)
        .unwrap();
    let Metadata::Checkpoint { snapshot, .. } = base.commit.metadata else {
        panic!("the segment base is a checkpoint")
    };
    let mut bytes = s.contents();
    bytes[(snapshot.offset + snapshot.stored_len / 2) as usize] ^= 0x01;
    let damaged = SimStorage::from_bytes(bytes);
    let tree = SimTree::new();
    let r = run(&damaged, tree.clone(), RestoreOptions::default());
    assert!(r.complete(), "{:?}", r.exceptions);
    assert!(r.attributes_unavailable.is_some());
    assert!(!r.attributes_complete());
    for (k, c) in &scripted_history()[2].after {
        if let Content::File(b) = c {
            assert_eq!(tree.file(k).as_ref(), Some(b));
        }
        assert!(tree.attributes(k).is_none(), "nothing guessed");
    }
    let f = r.findings();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].code, ErrorCode::AttributeNotRestored);
}

/// On the real filesystem: the time (to the nanosecond where the filesystem
/// keeps it) and, on Unix, the mode. Ownership applies when privileged and
/// is reported otherwise; on Windows a POSIX owner and mode are reported.
#[test]
fn c6_os_attributes() {
    let s = archive_with(&[
        ("d", None, posix(0o750)),
        ("d/f", Some(b"content"), posix(0o640)),
        ("ro", Some(b"read only"), posix(0o444)),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let r = run(
        &s,
        OsRestoreDir::open(dir.path()).unwrap(),
        RestoreOptions::default(),
    );
    assert!(r.complete(), "{:?}", r.exceptions);
    let want = std::time::UNIX_EPOCH + std::time::Duration::new(1_600_000_000, 123_456_789);
    for p in ["d", "d/f", "ro"] {
        let m = std::fs::metadata(dir.path().join(p)).unwrap();
        let got = m.modified().unwrap();
        let diff = if got > want {
            got.duration_since(want).unwrap()
        } else {
            want.duration_since(got).unwrap()
        };
        assert!(diff < std::time::Duration::from_millis(1), "{p}: {got:?}");
    }
    let kinds: Vec<AttributeKind> = r
        .attribute_exceptions
        .iter()
        .map(|e| e.issue.attribute)
        .collect();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = |p: &str| {
            std::fs::metadata(dir.path().join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!((mode("d"), mode("d/f"), mode("ro")), (0o750, 0o640, 0o444));
        let root = std::fs::metadata(dir.path()).unwrap().uid() == 0;
        if root {
            assert!(kinds.is_empty(), "{kinds:?}");
            assert_eq!(
                std::fs::metadata(dir.path().join("d/f")).unwrap().uid(),
                1000
            );
        } else {
            assert!(
                kinds.iter().all(|k| *k == AttributeKind::Ownership),
                "{kinds:?}"
            );
            assert_eq!(kinds.len(), 3);
        }
    }
    #[cfg(windows)]
    {
        assert!(std::fs::metadata(dir.path().join("ro"))
            .unwrap()
            .permissions()
            .readonly());
        assert!(!std::fs::metadata(dir.path().join("d/f"))
            .unwrap()
            .permissions()
            .readonly());
        assert!(kinds
            .iter()
            .all(|k| matches!(k, AttributeKind::Ownership | AttributeKind::Mode)));
        assert_eq!(kinds.len(), 6);
    }
}

/// Windows-authored attributes restored on Unix: read-only clears the
/// write bits, hidden has no equivalent and is reported, and the time is
/// set.
#[cfg(unix)]
#[test]
fn c6_os_windows_authored_on_unix() {
    use mochi_core::manifest::{WINDOWS_ARCHIVE, WINDOWS_HIDDEN, WINDOWS_READONLY};
    use std::os::unix::fs::PermissionsExt;
    let win = |bits: u32| Attributes {
        posix: None,
        windows: Some(bits),
        mtime: Some(Mtime {
            secs: 1_500_000_000,
            nanos: 0,
        }),
    };
    let s = archive_with(&[
        ("plain", Some(b"p"), win(WINDOWS_ARCHIVE)),
        ("ro", Some(b"r"), win(WINDOWS_READONLY | WINDOWS_ARCHIVE)),
        ("hidden", Some(b"h"), win(WINDOWS_HIDDEN)),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let r = run(
        &s,
        OsRestoreDir::open(dir.path()).unwrap(),
        RestoreOptions::default(),
    );
    let mode = |p: &str| {
        std::fs::metadata(dir.path().join(p))
            .unwrap()
            .permissions()
            .mode()
    };
    assert_eq!(mode("ro") & 0o222, 0);
    assert_ne!(mode("plain") & 0o200, 0);
    let kinds: Vec<(String, AttributeKind)> = r
        .attribute_exceptions
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.path.as_stored()).into_owned(),
                e.issue.attribute,
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [("hidden".to_string(), AttributeKind::HiddenOrSystem)]
    );
    let t = std::fs::metadata(dir.path().join("plain"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        t,
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000)
    );
}

/// Nested directories get their attributes deepest first.
#[test]
fn c6_nested_directories_deepest_first() {
    let s = archive_with(&[
        ("a", None, posix(0o755)),
        ("a/b", None, posix(0o755)),
        ("a/b/c", None, posix(0o700)),
        ("a/b/c/f", Some(b"f"), posix(0o600)),
    ]);
    let tree = SimTree::new();
    run(&s, tree.clone(), RestoreOptions::default());
    assert_eq!(tree.applied_order(), ["a/b/c/f", "a/b/c", "a/b", "a"]);
}

/// A delta head: attributes of versions introduced after the checkpoint
/// come from the delta manifests.
#[test]
fn c6_attributes_from_delta_manifests() {
    let s = archive_with(&[("base", Some(b"b"), posix(0o644))]);
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(2)),
        test_options(),
        mochi_core::publish::TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("later"), b"l".to_vec(), posix(0o600));
    w.commit(tx, &Job::new().ctx()).unwrap();
    drop(w);
    let head = open_head(&s, &opts()).unwrap();
    assert!(!head.commit.metadata.is_checkpoint(), "a delta head");
    let tree = SimTree::new();
    let r = run(&s, tree.clone(), RestoreOptions::default());
    assert!(r.attributes_complete(), "{r:?}");
    assert_eq!(
        tree.attributes(b"later").unwrap().posix.unwrap().mode,
        0o600
    );
    assert_eq!(tree.attributes(b"base").unwrap().posix.unwrap().mode, 0o644);
}
