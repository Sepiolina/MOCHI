//! C6 restore engine (plan C6 exit: fault-matrix row "path traversal or
//! naming collision"; spec §10.4, §23.3 #7): nothing is overwritten or
//! merged, names are never altered, only verified files appear.

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::manifest::Attributes;
use mochi_core::publish::{open_head, ArchiveWriter, ReadOptions, Transaction};
use mochi_core::report::Severity;
use mochi_core::restore::{restore, ExceptionKind, RestoreOptions, RestoreReport};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{CaseBehavior, DirectoryDurability, NameIssue, RestoreDir};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{build, path, scripted_history, test_options, Content, Job};
use mochi_testkit::{SeqIds, SimStorage, SimTree};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// An archive with one commit: `None` is a directory, `Some` a file.
fn archive_with(entries: &[(&str, Option<&[u8]>)]) -> SimStorage {
    let s = SimStorage::new();
    let mut tx = Transaction::new();
    for (p, c) in entries {
        match c {
            None => tx.put_dir(path(p), Attributes::default()),
            Some(b) => tx.put_file(path(p), b.to_vec(), Attributes::default()),
        };
    }
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.commit(tx, &Job::new().ctx()).unwrap();
    s
}

fn scripted() -> SimStorage {
    let s = SimStorage::new();
    build(s.clone(), 21, &scripted_history()).unwrap();
    s
}

fn run<D: RestoreDir>(s: &SimStorage, under: Option<&str>, root: D) -> RestoreReport {
    let head = open_head(s, &opts()).unwrap();
    let under = under.map(path);
    restore(
        s,
        &head,
        under.as_ref(),
        root,
        &RestoreOptions::default(),
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

/// The whole head restores exactly, with every file verified, and the
/// report says attributes were not restored.
#[test]
fn c6_restore_the_head() {
    let s = scripted();
    let tree = SimTree::new();
    let r = run(&s, None, tree.clone());
    assert!(r.complete(), "{:?}", r.exceptions);
    let model = &scripted_history()[2].after;
    let mut files = 0;
    for (k, c) in model {
        match c {
            Content::Dir => assert!(tree.is_dir(k)),
            Content::File(b) => {
                files += 1;
                assert_eq!(tree.file(k).as_ref(), Some(b));
            }
        }
    }
    assert_eq!(r.files, files);
    assert_eq!(r.directories, (model.len() - files as usize) as u64);
    assert_eq!(tree.paths().len(), model.len());
    assert_eq!(r.directory_durability, DirectoryDurability::Confirmed);
    assert!(r.attributes_complete(), "{:?}", r.attribute_exceptions);
    assert!(r.findings().is_empty());
}

/// **C6 exit (naming collision).** On a case-insensitive destination, the
/// second of two names that differ only in case is a collision: the first
/// keeps its bytes, nothing is merged, and a colliding directory's subtree
/// is skipped with the collision as its cause.
#[test]
fn c6_case_insensitive_collisions_never_overwrite_or_merge() {
    let s = archive_with(&[
        ("Docs", None),
        ("Docs/x", Some(b"upper dir file")),
        ("Readme", Some(b"upper")),
        ("docs", None),
        ("docs/y", Some(b"lower dir file")),
        ("readme", Some(b"lower")),
    ]);
    let tree = SimTree::new().case_insensitive();
    let r = run(&s, None, tree.clone());
    assert_eq!(tree.file(b"Readme").unwrap(), b"upper");
    assert_eq!(tree.file(b"Docs/x").unwrap(), b"upper dir file");
    assert_eq!(exception(&r, "readme"), Some(ExceptionKind::Collision));
    assert_eq!(exception(&r, "docs"), Some(ExceptionKind::Collision));
    assert_eq!(
        exception(&r, "docs/y"),
        Some(ExceptionKind::ParentNotRestored {
            cause: ErrorCode::NameCollision
        })
    );
    assert_eq!(tree.paths(), ["Docs", "Docs/x", "Readme"]);
    assert_eq!((r.files, r.directories), (2, 1));
    let codes: Vec<ErrorCode> = r.findings().iter().map(|f| f.code).collect();
    assert_eq!(
        codes
            .iter()
            .filter(|c| **c == ErrorCode::NameCollision)
            .count(),
        3
    );
}

fn refusing(s: &SimStorage, root: SimTree) -> mochi_core::MochiError {
    let head = open_head(s, &opts()).unwrap();
    restore(
        s,
        &head,
        None,
        root,
        &RestoreOptions {
            refuse_on_preflight_exceptions: true,
            ..RestoreOptions::default()
        },
        &opts(),
        &Job::new().ctx(),
    )
    .unwrap_err()
}

/// **C6 (collision preflight).** The destination's case behaviour is
/// detected and every collision is known before anything is written: asked
/// to refuse on any, the restore ends with `NAME_COLLISION` and the
/// destination is untouched. The same archive into a case-sensitive
/// destination has no collision and restores in full.
#[test]
fn c6_preflight_finds_collisions_before_writing() {
    let s = archive_with(&[
        ("Docs", None),
        ("Docs/x", Some(b"upper dir file")),
        ("docs", None),
        ("docs/y", Some(b"lower dir file")),
        ("z", Some(b"zed")),
    ]);
    let tree = SimTree::new().case_insensitive();
    let e = refusing(&s, tree.clone());
    assert_eq!(e.code, ErrorCode::NameCollision, "{e}");
    assert!(e.message.contains("nothing was written"), "{e}");
    assert!(tree.paths().is_empty(), "{:?}", tree.paths());

    let r = run(&s, None, tree.clone());
    assert_eq!(r.case_behavior, CaseBehavior::Insensitive);
    assert_eq!(exception(&r, "docs"), Some(ExceptionKind::Collision));
    assert_eq!(tree.paths(), ["Docs", "Docs/x", "z"]);

    let sensitive = SimTree::new();
    let r = run(&s, None, sensitive.clone());
    assert_eq!(r.case_behavior, CaseBehavior::Sensitive);
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(sensitive.paths().len(), 5);
}

/// The preflight also sees what is already at the destination, through the
/// destination's own case rule, and names the destination cannot hold.
#[test]
fn c6_preflight_sees_existing_entries_and_unsupported_names() {
    let s = archive_with(&[("readme", Some(b"archive")), ("b", Some(b"bee"))]);
    let tree = SimTree::new().case_insensitive();
    tree.insert_file("README", b"already here");
    let e = refusing(&s, tree.clone());
    assert_eq!(e.code, ErrorCode::NameCollision, "{e}");
    assert_eq!(tree.paths(), ["README"]);
    assert_eq!(tree.file(b"README").unwrap(), b"already here");

    let s = archive_with(&[("ok", Some(b"fine")), ("CON", Some(b"device"))]);
    let tree = SimTree::new().windows_rules();
    let e = refusing(&s, tree.clone());
    assert_eq!(e.code, ErrorCode::NameUnsupported, "{e}");
    assert!(tree.paths().is_empty());
}

/// Existing destination content is a collision too, and keeps its bytes.
#[test]
fn c6_existing_destination_content_is_kept() {
    let s = archive_with(&[("a", Some(b"from the archive")), ("b", Some(b"bee"))]);
    let tree = SimTree::new();
    tree.insert_file("a", b"already here");
    let r = run(&s, None, tree.clone());
    assert_eq!(tree.file(b"a").unwrap(), b"already here");
    assert_eq!(tree.file(b"b").unwrap(), b"bee");
    assert_eq!(exception(&r, "a"), Some(ExceptionKind::Collision));
    no_temporaries(&tree.paths());
}

/// **C6 (unsupported names).** Under Windows rules, reserved, forbidden,
/// and trailing-dot names are reported, not renamed, and an unsupported
/// directory's subtree is skipped with that cause.
#[test]
fn c6_unsupported_names_are_reported_not_renamed() {
    let s = archive_with(&[
        ("CON", Some(b"device")),
        ("aux.tar.gz", Some(b"device too")),
        ("dir.", None),
        ("dir./inner", Some(b"inner")),
        ("ok.txt", Some(b"fine")),
        ("<img src=x onerror=alert(1)>", Some(b"hostile name")),
        ("q?", Some(b"question")),
    ]);
    let tree = SimTree::new().windows_rules();
    let r = run(&s, None, tree.clone());
    assert_eq!(tree.paths(), ["ok.txt"]);
    let issue = |p: &str| match exception(&r, p) {
        Some(ExceptionKind::UnsupportedName(i)) => i,
        other => panic!("{p}: {other:?}"),
    };
    assert_eq!(issue("CON"), NameIssue::Reserved);
    assert_eq!(issue("aux.tar.gz"), NameIssue::Reserved);
    assert_eq!(issue("dir."), NameIssue::TrailingDotOrSpace);
    assert_eq!(
        issue("<img src=x onerror=alert(1)>"),
        NameIssue::IllegalCharacter(b'<')
    );
    assert_eq!(issue("q?"), NameIssue::IllegalCharacter(b'?'));
    assert_eq!(
        exception(&r, "dir./inner"),
        Some(ExceptionKind::ParentNotRestored {
            cause: ErrorCode::NameUnsupported
        })
    );
}

/// An archive file whose name has the engine's temporary-name form is
/// restored under its own name; the engine uses another temporary name.
#[test]
fn c6_an_entry_named_like_a_temporary_file() {
    let s = archive_with(&[
        (".mochi-restore.1.tmp", Some(b"looks temporary")),
        (".mochi-restore.3.tmp", Some(b"so does this")),
        ("z", Some(b"zed")),
    ]);
    let tree = SimTree::new();
    let r = run(&s, None, tree.clone());
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(
        tree.file(b".mochi-restore.1.tmp").unwrap(),
        b"looks temporary"
    );
    assert_eq!(tree.file(b".mochi-restore.3.tmp").unwrap(), b"so does this");
    assert_eq!(tree.file(b"z").unwrap(), b"zed");
    assert_eq!(tree.paths().len(), 3);
}

/// A file with a damaged chunk is absent, never partly present; the other
/// files restore, and no temporary file is left.
#[test]
fn c6_a_damaged_file_is_absent_not_partial() {
    let s = scripted();
    let head = open_head(&s, &opts()).unwrap();
    let (_, extents) = {
        let snap = head.catalog.replay(None).unwrap();
        let e = snap.get(&path("big")).unwrap();
        head.catalog.file_version(&e.version).unwrap().unwrap()
    };
    let ExtentSource::Chunk { chunk, .. } = extents[2].source else {
        panic!("a chunk")
    };
    let at = head.catalog.object_location(&chunk).unwrap().unwrap();
    let mut bytes = s.contents();
    bytes[at as usize + 20] ^= 0x10;
    let damaged = SimStorage::from_bytes(bytes);
    let tree = SimTree::new();
    let r = run(&damaged, None, tree.clone());
    assert!(tree.file(b"big").is_none());
    match exception(&r, "big") {
        Some(ExceptionKind::Integrity { code, .. }) => {
            assert_eq!(code, ErrorCode::StoredIntegrityFailed)
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(r.exceptions.len(), 1);
    no_temporaries(&tree.paths());
    let f = r.findings();
    assert!(f
        .iter()
        .any(|x| x.code == ErrorCode::StoredIntegrityFailed && x.severity == Severity::Error));
}

/// Restoring one subtree creates it and the directories above it only.
#[test]
fn c6_restore_a_subtree() {
    let s = scripted();
    let tree = SimTree::new();
    let r = run(&s, Some("docs"), tree.clone());
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(tree.paths(), ["docs", "docs/a.txt", "docs/empty"]);
    let head = open_head(&s, &opts()).unwrap();
    let e = restore(
        &s,
        &head,
        Some(&path("missing")),
        SimTree::new(),
        &RestoreOptions::default(),
        &opts(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

/// Cancellation ends the job with `CANCELLED` and leaves no temporary file.
#[test]
fn c6_restore_cancellation() {
    let s = scripted();
    let head = open_head(&s, &opts()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let tree = SimTree::new();
    let e = restore(
        &s,
        &head,
        None,
        tree.clone(),
        &RestoreOptions::default(),
        &opts(),
        &ctx,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(tree.paths().is_empty());
}

/// On the real filesystem: the head restores; a second restore into the same
/// directory collides everywhere and changes nothing, including a file the
/// user edited in between. Linux only: elsewhere restoration to the
/// filesystem is refused ([`c6_os_restore_is_refused_where_unsupported`]);
/// Windows naming rules are covered on `SimTree`.
#[cfg(target_os = "linux")]
#[test]
fn c6_os_restore_and_restore_again() {
    let s = scripted();
    let dir = tempfile::tempdir().unwrap();
    let r = run(&s, None, OsRestoreDir::open(dir.path()).unwrap());
    let model = &scripted_history()[2].after;
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(r.directory_durability, DirectoryDurability::Confirmed);
    assert_eq!(r.case_behavior, CaseBehavior::Sensitive);
    for (k, c) in model {
        let p = String::from_utf8(k.clone()).unwrap();
        let on_disk = dir.path().join(&p);
        match c {
            Content::Dir => assert!(on_disk.is_dir(), "{p}"),
            Content::File(b) => assert_eq!(&std::fs::read(&on_disk).unwrap(), b, "{p}"),
        }
    }

    std::fs::write(dir.path().join("big"), b"edited locally").unwrap();
    let r = run(&s, None, OsRestoreDir::open(dir.path()).unwrap());
    assert_eq!((r.files, r.directories), (0, 0));
    assert_eq!(
        std::fs::read(dir.path().join("big")).unwrap(),
        b"edited locally"
    );
    assert_eq!(exception(&r, "big"), Some(ExceptionKind::Collision));
    assert_eq!(
        exception(&r, "docs/a.txt"),
        Some(ExceptionKind::ParentNotRestored {
            cause: ErrorCode::NameCollision
        })
    );
    let left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    no_temporaries(&left);
}

/// Where race-resistant restoration is not implemented (every platform but
/// Linux; Windows included), opening a restore destination fails clearly,
/// `UNSUPPORTED_FEATURE` (exit 4), and nothing is written.
#[cfg(not(target_os = "linux"))]
#[test]
fn c6_os_restore_is_refused_where_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let e = mochi_core::MochiError::from(OsRestoreDir::open(dir.path()).unwrap_err());
    assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{e}");
    assert!(e.message.contains("Nothing was written"), "{e}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// A nested subtree brings the directories above it, and nothing beside it.
#[test]
fn c6_restore_a_nested_subtree() {
    let s = archive_with(&[
        ("a", None),
        ("a/b", None),
        ("a/b/c", Some(b"deep")),
        ("a/other", Some(b"beside")),
        ("z", Some(b"elsewhere")),
    ]);
    let tree = SimTree::new();
    let r = run(&s, Some("a/b"), tree.clone());
    assert!(r.complete(), "{:?}", r.exceptions);
    assert_eq!(tree.paths(), ["a", "a/b", "a/b/c"]);
    assert_eq!(tree.file(b"a/b/c").unwrap(), b"deep");
}

/// Cancellation is checked before every entry, directories included: a
/// cancelled job creates nothing.
#[test]
fn c6_restore_cancellation_before_a_directory() {
    let s = archive_with(&[("a", None), ("a/f", Some(b"f"))]);
    let head = open_head(&s, &opts()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let tree = SimTree::new();
    let e = restore(
        &s,
        &head,
        None,
        tree.clone(),
        &RestoreOptions::default(),
        &opts(),
        &ctx,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(tree.paths().is_empty(), "{:?}", tree.paths());
}
