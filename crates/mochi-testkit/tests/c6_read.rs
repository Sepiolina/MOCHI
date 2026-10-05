//! C6 read path, first slice: `mochi_core::read::{list, read_file}` (plan
//! C6 "Open … snapshot selection by commit", `list`, `get`; spec §9.3,
//! §10.3, §20.1).
//!
//! The expected results come from the scripted history's independent model
//! and from `archive::read_state`, a separate reassembly in the test kit, so
//! the reader is not checked against itself.

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::catalog::namespace::{EntryKind, NamespaceOp};
use mochi_core::catalog::FileVersion;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::manifest::Attributes;
use mochi_core::publish::{commit_history, open_at_footer, open_head, ReadOptions};
use mochi_core::read::{list, read_file, read_file_in, FileRead};
use mochi_core::ErrorCode;
use mochi_format::digest::{file_content_hash, FileContentHash};
use mochi_format::repr::DecodedSlice;
use mochi_testkit::archive::{build, path, read_state, scripted_history, Content, Job};
use mochi_testkit::history::HistoryWriter;
use mochi_testkit::SimStorage;

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn archive() -> SimStorage {
    let s = SimStorage::new();
    build(s.clone(), 11, &scripted_history()).unwrap();
    s
}

fn hash(bytes: &[u8]) -> FileContentHash {
    file_content_hash(DecodedSlice::from_logical(bytes))
}

fn get(
    s: &SimStorage,
    head: &mochi_core::publish::OpenedHead,
    p: &str,
) -> mochi_core::Result<(Vec<u8>, FileRead)> {
    let mut out = Vec::new();
    let r = read_file(s, head, &path(p), &mut out, &opts(), &Job::new().ctx())?;
    Ok((out, r))
}

/// **C6.** `list` gives every entry of the head with kind, length, and
/// hash; `read_file` returns each file's exact bytes. Both agree with the
/// model and with the test kit's own reassembly.
#[test]
fn c6_list_and_read_the_head() {
    let s = archive();
    let head = open_head(&s, &opts()).unwrap();
    let model = scripted_history()[2].after.clone();
    assert_eq!(read_state(&s, &head).unwrap(), model, "oracle");

    let listed = list(&head, None).unwrap();
    let names: Vec<Vec<u8>> = listed.iter().map(|e| e.path.as_stored().to_vec()).collect();
    assert_eq!(names, model.keys().cloned().collect::<Vec<_>>());
    for e in &listed {
        match &model[e.path.as_stored()] {
            Content::Dir => {
                assert_eq!(e.kind, EntryKind::Directory);
                assert_eq!(e.content_hash, None);
            }
            Content::File(bytes) => {
                assert_eq!(e.kind, EntryKind::File);
                assert_eq!(e.logical_len, bytes.len() as u64);
                assert_eq!(e.content_hash, Some(hash(bytes)));
                let p = String::from_utf8(e.path.as_stored().to_vec()).unwrap();
                let (out, r) = get(&s, &head, &p).unwrap();
                assert_eq!(&out, bytes, "{p}");
                assert_eq!(r.content_hash, hash(bytes));
                assert_eq!(r.logical_len, bytes.len() as u64);
                assert_eq!(r.hole_bytes, 0);
            }
        }
    }
    // The empty file has no chunks; a multi-chunk file loads several.
    assert_eq!(get(&s, &head, "docs/empty").unwrap().1.chunks_loaded, 0);
    assert!(get(&s, &head, "big").unwrap().1.chunks_loaded > 1);
}

/// `list` under a directory: the directory and its descendants only, by
/// whole path components.
#[test]
fn c6_list_under_a_directory() {
    let s = archive();
    let head = open_head(&s, &opts()).unwrap();
    let under: Vec<String> = list(&head, Some(&path("docs")))
        .unwrap()
        .iter()
        .map(|e| String::from_utf8(e.path.as_stored().to_vec()).unwrap())
        .collect();
    assert_eq!(under, ["docs", "docs/a.txt", "docs/empty"]);
    assert!(list(&head, Some(&path("nowhere"))).unwrap().is_empty());
}

/// **C6 snapshot selection.** An earlier commit, opened at its own footer,
/// reads as it was: the replaced file's old bytes, and the renamed and
/// deleted entries in their old places.
#[test]
fn c6_read_an_earlier_commit() {
    let s = archive();
    let history = commit_history(&s, &opts()).unwrap();
    for (i, step) in scripted_history().iter().enumerate() {
        let entry = history.iter().find(|h| h.commit.seq == i as u64).unwrap();
        let at = open_at_footer(&s, entry.footer_offset, &opts()).unwrap();
        let listed: Vec<Vec<u8>> = list(&at, None)
            .unwrap()
            .iter()
            .map(|e| e.path.as_stored().to_vec())
            .collect();
        assert_eq!(
            listed,
            step.after.keys().cloned().collect::<Vec<_>>(),
            "{i}"
        );
        for (k, c) in &step.after {
            if let Content::File(bytes) = c {
                let p = String::from_utf8(k.clone()).unwrap();
                assert_eq!(&get(&s, &at, &p).unwrap().0, bytes, "commit {i}: {p}");
            }
        }
    }
}

/// Asking for an absent path or a directory is an invocation error, and
/// nothing is written.
#[test]
fn c6_absent_path_and_directory_are_refused() {
    let s = archive();
    let head = open_head(&s, &opts()).unwrap();
    for p in ["nowhere", "docs/c.bin", "docs"] {
        let mut out = Vec::new();
        let e = read_file(&s, &head, &path(p), &mut out, &opts(), &Job::new().ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{p}: {e}");
        assert!(out.is_empty(), "{p}");
    }
}

/// A damaged chunk is a stored-integrity failure, never silently wrong
/// bytes; files that do not use it still read.
#[test]
fn c6_a_damaged_chunk_fails_the_read() {
    let s = archive();
    let head = open_head(&s, &opts()).unwrap();
    let (version, extents) = {
        let snap = head.catalog.replay(None).unwrap();
        let entry = snap.get(&path("big")).unwrap();
        head.catalog.file_version(&entry.version).unwrap().unwrap()
    };
    assert!(version.logical_len > 0);
    let ExtentSource::Chunk { chunk, .. } = extents[1].source else {
        panic!("big's second extent is a chunk");
    };
    let at = head.catalog.object_location(&chunk).unwrap().unwrap();
    let mut bytes = s.contents();
    bytes[at as usize + 20] ^= 0x01;
    let damaged = SimStorage::from_bytes(bytes);
    let head = open_head(&damaged, &opts()).unwrap();
    let e = get(&damaged, &head, "big").unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "{e}");
    let model = &scripted_history()[2].after;
    let Content::File(a) = &model[b"docs/a.txt".as_slice()] else {
        panic!()
    };
    assert_eq!(&get(&damaged, &head, "docs/a.txt").unwrap().0, a);
}

/// Cancellation is honoured before the first extent: nothing is written.
#[test]
fn c6_cancellation() {
    let s = archive();
    let head = open_head(&s, &opts()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let mut out = Vec::new();
    let e = read_file(&s, &head, &path("big"), &mut out, &opts(), &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(out.is_empty());
}

/// Sparse files (§9.3): holes come out as zeros and count towards the
/// file-content hash, which covers the whole logical stream.
#[test]
fn c6_sparse_file_reads_holes_as_zeros() {
    let mut w = HistoryWriter::new(3).unwrap();
    let content = b"leading data".to_vec();
    let sparse = w.file(&content, 100_000, Attributes::default()).unwrap();
    let all_hole = w.file(b"", 70_000, Attributes::default()).unwrap();
    w.commit(
        vec![
            NamespaceOp::Put {
                path: path("sparse"),
                version: sparse,
            },
            NamespaceOp::Put {
                path: path("hole-only"),
                version: all_hole,
            },
        ],
        false,
    )
    .unwrap();
    let src = SimStorage::from_bytes(w.bytes.clone());
    let read = |p: &str| {
        let mut out = Vec::new();
        read_file_in(
            &src,
            &w.catalog,
            &path(p),
            &mut out,
            &opts(),
            &Job::new().ctx(),
        )
        .map(|r| (out, r))
    };
    let (out, r) = read("sparse").unwrap();
    let mut want = content.clone();
    want.resize(content.len() + 100_000, 0);
    assert_eq!(out, want);
    assert_eq!(r.hole_bytes, 100_000);
    assert_eq!(r.content_hash, hash(&want));
    let (out, r) = read("hole-only").unwrap();
    assert_eq!(out, vec![0u8; 70_000]);
    assert_eq!((r.hole_bytes, r.chunks_loaded), (70_000, 0));
}

/// A file whose chunks are all intact but whose bytes do not match its
/// recorded file-content hash is a content-integrity failure, reported
/// after streaming (the module note: output is unverified until `Ok`).
#[test]
fn c6_a_file_content_hash_mismatch_is_detected() {
    let mut w = HistoryWriter::new(4).unwrap();
    let good = w
        .file(b"the real content", 0, Attributes::default())
        .unwrap();
    let (_, extents) = w.catalog.file_version(&good).unwrap().unwrap();
    let forged = mochi_core::catalog::namespace::FileVersionId::from_bytes([0xF0; 32]);
    w.catalog
        .insert_file_version(
            &FileVersion {
                id: forged,
                kind: EntryKind::File,
                logical_len: 16,
                content_hash: Some(hash(b"something else!!")),
            },
            &extents,
        )
        .unwrap();
    w.commit(
        vec![
            NamespaceOp::Put {
                path: path("good"),
                version: good,
            },
            NamespaceOp::Put {
                path: path("forged"),
                version: forged,
            },
        ],
        false,
    )
    .unwrap();
    let src = SimStorage::from_bytes(w.bytes.clone());
    let mut out = Vec::new();
    let ok = read_file_in(
        &src,
        &w.catalog,
        &path("good"),
        &mut out,
        &opts(),
        &Job::new().ctx(),
    );
    assert_eq!(ok.unwrap().content_hash, hash(b"the real content"));
    let mut out = Vec::new();
    let e = read_file_in(
        &src,
        &w.catalog,
        &path("forged"),
        &mut out,
        &opts(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ContentIntegrityFailed, "{e}");
    assert_eq!(out, b"the real content", "streamed before the verdict");
}
