//! Spec Annex B.2.23 item 3, "Delete files after archiving": which sources
//! `mochi_core::sources::check_removable` lets go, on real files.
//!
//! A source may go only when its archived copy reads back and hashes to the
//! recorded value and the source still hashes to that same value. A file
//! changed after it was archived, or whose archived copy is damaged, is kept,
//! and so is every folder above it. Nothing here removes anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::import::{import_recording, ImportOptions, ImportedEntry, SourceKind};
use mochi_core::publish::{open_head, ArchiveWriter, ReadOptions, Transaction};
use mochi_core::sources::{check_removable, Verdict};
use mochi_core::storage::os::{OsReadStorage, OsSourceTree, OsStorage};
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::SeqIds;

/// `src/` with `top.txt` and `dir/{a,b}.txt`, archived as `src`.
fn setup() -> (tempfile::TempDir, PathBuf, Vec<ImportedEntry<PathBuf>>) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(src.join("dir")).unwrap();
    fs::write(src.join("top.txt"), vec![1u8; 3000]).unwrap();
    fs::write(src.join("dir/a.txt"), vec![2u8; 300]).unwrap();
    fs::write(src.join("dir/b.txt"), b"bee").unwrap();
    let job = Job::new();
    let (import, entries) = import_recording(
        &OsSourceTree,
        &[(path("src"), src.clone())],
        Transaction::new(),
        &ImportOptions::default(),
        &job.ctx(),
    )
    .unwrap();
    let archive = tmp.path().join("a.mochi");
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(&archive).unwrap(),
        Box::new(SeqIds::new(1)),
        test_options(),
    )
    .unwrap();
    w.commit(import.transaction, &job.ctx()).unwrap();
    w.close().unwrap();
    (tmp, archive, entries)
}

fn verdicts(archive: &Path, entries: &[ImportedEntry<PathBuf>]) -> Vec<(String, Verdict)> {
    let s = OsReadStorage::open(archive).unwrap();
    check_removable(&s, &ReadOptions::default(), entries, &Job::new().ctx())
        .unwrap()
        .into_iter()
        .map(|c| {
            (
                String::from_utf8_lossy(c.path.as_stored()).into_owned(),
                c.verdict,
            )
        })
        .collect()
}

fn verdict_of<'a>(v: &'a [(String, Verdict)], p: &str) -> &'a Verdict {
    &v.iter().find(|(x, _)| x == p).unwrap().1
}

#[test]
fn b2_23_unchanged_sources_may_go_and_the_import_recorded_each_of_them() {
    let (_tmp, archive, entries) = setup();
    let kinds: Vec<(String, SourceKind)> = entries
        .iter()
        .map(|e| {
            (
                String::from_utf8_lossy(e.path.as_stored()).into_owned(),
                e.kind,
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("src".to_owned(), SourceKind::Directory),
            ("src/dir".to_owned(), SourceKind::Directory),
            ("src/dir/a.txt".to_owned(), SourceKind::File),
            ("src/dir/b.txt".to_owned(), SourceKind::File),
            ("src/top.txt".to_owned(), SourceKind::File),
        ]
    );
    for (p, v) in verdicts(&archive, &entries) {
        assert_eq!(v, Verdict::Remove, "{p}");
    }
    // It decides only: every source is still there.
    assert!(entries.iter().all(|e| e.node.exists()));
}

#[test]
fn b2_23_a_file_changed_after_archiving_is_kept_with_its_folders() {
    let (_tmp, archive, entries) = setup();
    // Same length, other bytes: only the hash can tell.
    fs::write(entries[2].node.clone(), vec![3u8; 300]).unwrap();
    let v = verdicts(&archive, &entries);
    match verdict_of(&v, "src/dir/a.txt") {
        Verdict::Keep(why) => assert!(why.contains("changed"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(verdict_of(&v, "src/dir/b.txt"), &Verdict::Remove);
    assert_eq!(verdict_of(&v, "src/top.txt"), &Verdict::Remove);
    for dir in ["src", "src/dir"] {
        assert!(
            matches!(verdict_of(&v, dir), Verdict::Keep(_)),
            "{dir} must be kept: something in it is"
        );
    }
}

#[test]
fn b2_23_a_file_whose_archived_copy_is_damaged_is_kept() {
    let (_tmp, archive, entries) = setup();
    // Flip a byte in the middle of top.txt's first chunk.
    let at = {
        let s = OsReadStorage::open(&archive).unwrap();
        let h = open_head(&s, &ReadOptions::default()).unwrap();
        let snap = h.catalog.replay(None).unwrap();
        let e = snap.get(&path("src/top.txt")).unwrap();
        let (_, extents) = h.catalog.file_version(&e.version).unwrap().unwrap();
        let chunk = extents
            .iter()
            .find_map(|x| match x.source {
                ExtentSource::Chunk { chunk, .. } => Some(chunk),
                ExtentSource::Hole => None,
            })
            .unwrap();
        let r = h.catalog.object(&chunk).unwrap().unwrap();
        h.catalog.object_location(&chunk).unwrap().unwrap() + r.stored_len / 2
    };
    let mut bytes = fs::read(&archive).unwrap();
    bytes[usize::try_from(at).unwrap()] ^= 0x40;
    fs::write(&archive, bytes).unwrap();
    let v = verdicts(&archive, &entries);
    match verdict_of(&v, "src/top.txt") {
        Verdict::Keep(why) => assert!(why.contains("did not read back"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(verdict_of(&v, "src"), Verdict::Keep(_)));
    assert_eq!(verdict_of(&v, "src/dir/a.txt"), &Verdict::Remove);
    assert_eq!(verdict_of(&v, "src/dir"), &Verdict::Remove);
}
