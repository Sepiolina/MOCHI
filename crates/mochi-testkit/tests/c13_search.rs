//! C13 (required part): essential file discovery (spec §19.1, §19.3).
//!
//! Expectations come from the operations each test performs, never from
//! what `search` returns: which paths each commit put or deleted, and plain
//! BLAKE3 of the content written (the file-content hash, O20). Every
//! retained snapshot is discoverable by path, file version, content hash,
//! and snapshot; zero results under partial coverage are never reported as
//! complete; and discovery survives the loss of a catalog image by
//! rebuilding from recovery manifests.

use std::collections::BTreeSet;

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::publish::{
    commit_history, open_head, ArchiveWriter, CatalogSource, CheckpointPolicy, ReadOptions,
    TailPolicy, Transaction,
};
use mochi_core::search::{search, FullText, PathMatch, Query, SearchResult, SnapshotScope};
use mochi_core::ErrorCode;
use mochi_format::digest::FileContentHash;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::replay::{attrs, checkpoint_refs, damage};
use mochi_testkit::{SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn hash(content: &[u8]) -> FileContentHash {
    FileContentHash::from_bytes(*blake3::hash(content).as_bytes())
}

const INVOICE: &[u8] = b"invoice 2024: 42 units";
const NOTES_0: &[u8] = b"first notes";
const NOTES_1: &[u8] = b"second notes, longer than the first";

/// Commits, each a checkpoint only where noted:
/// * 0: `docs/`, `docs/Invoice-2024.pdf` (INVOICE), `docs/notes.txt` (NOTES_0)
/// * 1: `docs/notes.txt` replaced (NOTES_1); `copy.pdf` (INVOICE again)
/// * 2: `docs/Invoice-2024.pdf` deleted
/// * 3: checkpoint (forced), no namespace change
/// * 4: `late.txt` (NOTES_0)
fn fixture(s: &SimStorage) {
    let mut w =
        ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(13)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut tx = Transaction::new();
    tx.put_dir(path("docs"), attrs(0o755, 0))
        .put_file(
            path("docs/Invoice-2024.pdf"),
            INVOICE.to_vec(),
            attrs(0o644, 0),
        )
        .put_file(path("docs/notes.txt"), NOTES_0.to_vec(), attrs(0o644, 0));
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("docs/notes.txt"), NOTES_1.to_vec(), attrs(0o644, 1))
        .put_file(path("copy.pdf"), INVOICE.to_vec(), attrs(0o644, 1));
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    tx.delete(path("docs/Invoice-2024.pdf"));
    w.commit(tx, &job.ctx()).unwrap();
    w.request_checkpoint();
    w.commit(Transaction::new(), &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("late.txt"), NOTES_0.to_vec(), attrs(0o644, 4));
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();
}

fn run(s: &SimStorage, scope: SnapshotScope, q: &Query) -> mochi_core::Result<SearchResult> {
    let head = open_head(s, &opts())?;
    search(s, &head, scope, q, &opts(), &Job::new().ctx())
}

fn name(n: &str) -> Query {
    Query {
        name: n.as_bytes().to_vec(),
        ..Query::default()
    }
}

/// (seq, path) of every hit.
fn found(r: &SearchResult) -> Vec<(u64, String)> {
    r.hits
        .iter()
        .map(|h| {
            (
                h.seq,
                String::from_utf8(h.path.as_stored().to_vec()).unwrap(),
            )
        })
        .collect()
}

fn pairs(v: &[(u64, &str)]) -> Vec<(u64, String)> {
    v.iter().map(|(s, p)| (*s, p.to_string())).collect()
}

/// **DoD (§19.1): discovery by path, at the head and across snapshots.**
/// A name deleted at the head is absent there with complete coverage, and
/// found in every snapshot that held it.
#[test]
fn c13_search_by_name_at_head_and_across_snapshots() {
    let s = SimStorage::new();
    fixture(&s);

    let r = run(&s, SnapshotScope::Head, &name("Invoice")).unwrap();
    assert!(r.hits.is_empty());
    assert!(
        r.coverage.complete(),
        "zero hits here are a complete answer"
    );
    assert_eq!(r.coverage.requested, vec![4]);
    assert_eq!(r.coverage.indexed_seq, 4);
    assert_eq!(r.coverage.full_text, FullText::NotBuilt);

    let r = run(&s, SnapshotScope::All, &name("Invoice")).unwrap();
    assert_eq!(
        found(&r),
        pairs(&[(0, "docs/Invoice-2024.pdf"), (1, "docs/Invoice-2024.pdf")])
    );
    assert!(r.coverage.complete());
    assert_eq!(r.coverage.searched, vec![0, 1, 2, 3, 4]);
    assert_eq!(r.coverage.entries_examined, 3 + 4 + 3 + 3 + 4);
    assert!(r.coverage.opened_separately.is_empty());

    let r = run(&s, SnapshotScope::Commit(1), &name(".pdf")).unwrap();
    assert_eq!(
        found(&r),
        pairs(&[(1, "copy.pdf"), (1, "docs/Invoice-2024.pdf")])
    );
    let h = &r.hits[1];
    assert_eq!(h.kind, EntryKind::File);
    assert_eq!(h.logical_len, INVOICE.len() as u64);
    assert_eq!(h.content_hash, Some(hash(INVOICE)));
}

/// Search normalization is ASCII case folding only, and results carry the
/// exact stored bytes (§10.4).
#[test]
fn c13_case_folding_never_alters_identity() {
    let s = SimStorage::new();
    fixture(&s);
    let mut q = name("invoice");
    assert!(run(&s, SnapshotScope::Commit(0), &q)
        .unwrap()
        .hits
        .is_empty());
    q.ascii_case_insensitive = true;
    let r = run(&s, SnapshotScope::Commit(0), &q).unwrap();
    assert_eq!(found(&r), pairs(&[(0, "docs/Invoice-2024.pdf")]));
}

/// Path criteria: exact, subtree, and entry kind, combined with AND.
#[test]
fn c13_path_and_kind_criteria() {
    let s = SimStorage::new();
    fixture(&s);
    let q = Query {
        path: Some(PathMatch::Under(path("docs"))),
        ..Query::default()
    };
    let r = run(&s, SnapshotScope::Commit(0), &q).unwrap();
    assert_eq!(
        found(&r),
        pairs(&[
            (0, "docs"),
            (0, "docs/Invoice-2024.pdf"),
            (0, "docs/notes.txt")
        ])
    );
    let q = Query {
        path: Some(PathMatch::Under(path("docs"))),
        kind: Some(EntryKind::Directory),
        ..Query::default()
    };
    assert_eq!(
        found(&run(&s, SnapshotScope::All, &q).unwrap()),
        pairs(&[
            (0, "docs"),
            (1, "docs"),
            (2, "docs"),
            (3, "docs"),
            (4, "docs")
        ])
    );
    // The history of one path: discovery by identity until plan O29.
    let q = Query {
        path: Some(PathMatch::Exact(path("docs/notes.txt"))),
        ..Query::default()
    };
    let r = run(&s, SnapshotScope::All, &q).unwrap();
    let hashes: Vec<_> = r.hits.iter().map(|h| (h.seq, h.content_hash)).collect();
    assert_eq!(
        hashes,
        vec![
            (0, Some(hash(NOTES_0))),
            (1, Some(hash(NOTES_1))),
            (2, Some(hash(NOTES_1))),
            (3, Some(hash(NOTES_1))),
            (4, Some(hash(NOTES_1))),
        ]
    );
    // `Exact` does not match a path that merely starts with it.
    let q = Query {
        path: Some(PathMatch::Exact(path("docs/notes"))),
        ..Query::default()
    };
    assert!(run(&s, SnapshotScope::All, &q).unwrap().hits.is_empty());
}

/// **DoD (§19.1): discovery by file version and by content.** A version is
/// found in exactly the snapshots that hold it; a content hash finds every
/// path with that content, under any name.
#[test]
fn c13_search_by_version_and_content_hash() {
    let s = SimStorage::new();
    fixture(&s);
    // The version of notes.txt that commit 0 wrote, as a client learns it.
    let first = run(&s, SnapshotScope::Commit(0), &name("notes")).unwrap();
    let v0 = first.hits[0].version;
    let q = Query {
        version: Some(v0),
        ..Query::default()
    };
    assert_eq!(
        found(&run(&s, SnapshotScope::All, &q).unwrap()),
        pairs(&[(0, "docs/notes.txt")])
    );

    let q = Query {
        content_hash: Some(hash(INVOICE)),
        ..Query::default()
    };
    assert_eq!(
        found(&run(&s, SnapshotScope::All, &q).unwrap()),
        pairs(&[
            (0, "docs/Invoice-2024.pdf"),
            (1, "copy.pdf"),
            (1, "docs/Invoice-2024.pdf"),
            (2, "copy.pdf"),
            (3, "copy.pdf"),
            (4, "copy.pdf"),
        ])
    );
    // NOTES_0 under two names, in disjoint snapshots.
    let q = Query {
        content_hash: Some(hash(NOTES_0)),
        ..Query::default()
    };
    assert_eq!(
        found(&run(&s, SnapshotScope::All, &q).unwrap()),
        pairs(&[(0, "docs/notes.txt"), (4, "late.txt")])
    );
    // A hash nothing has.
    let q = Query {
        content_hash: Some(hash(b"never written")),
        ..Query::default()
    };
    let r = run(&s, SnapshotScope::All, &q).unwrap();
    assert!(r.hits.is_empty() && r.coverage.complete());
}

/// **DoD (§19.1): every retained snapshot, and only those.** Expired
/// snapshots leave the retained scope; a held one stays in it.
#[test]
fn c13_retained_scope_follows_retention() {
    let s = SimStorage::new();
    fixture(&s);
    let mut w = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(14)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0;
    let mut tx = Transaction::new();
    tx.expire(0).expire(1).expire(2).hold(b"audit", 1);
    w.commit(tx, &Job::new().ctx()).unwrap(); // 5
    w.close().unwrap();

    let r = run(&s, SnapshotScope::Retained, &name("Invoice")).unwrap();
    assert_eq!(r.coverage.requested, vec![1, 3, 4, 5]);
    assert_eq!(found(&r), pairs(&[(1, "docs/Invoice-2024.pdf")]));
    assert!(r.coverage.complete());
    // Expired snapshots are still in the file until a GC rewrite.
    let r = run(&s, SnapshotScope::All, &name("Invoice")).unwrap();
    assert_eq!(r.coverage.requested, (0..=5).collect::<Vec<_>>());
    assert_eq!(found(&r).len(), 2);
}

/// **Fault row "index deleted" for discovery (§19.1, §24.2): the catalog
/// image is damaged.** The head's catalog is rebuilt from the checkpoint's
/// snapshot manifest; snapshots before that checkpoint are opened at their
/// own footers. The answers are the same as from the intact archive.
#[test]
fn c13_discovery_rebuilds_from_manifests_when_an_image_is_damaged() {
    let s = SimStorage::new();
    fixture(&s);
    let intact = run(&s, SnapshotScope::All, &Query::default()).unwrap();

    let h = commit_history(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).0);
    let damaged = SimStorage::from_bytes(bytes);

    let r = run(&damaged, SnapshotScope::All, &Query::default()).unwrap();
    assert!(matches!(
        r.coverage.catalog_source,
        CatalogSource::SnapshotManifest { .. }
    ));
    assert_eq!(r.coverage.opened_separately, vec![0, 1, 2]);
    assert!(r.coverage.complete());
    assert_eq!(found(&r), found(&intact));
    assert_eq!(r.hits, intact.hits);
}

/// **§19.3: zero results with partial coverage are not "no match".** Every
/// image is damaged, and the first segment's snapshot manifest too, so
/// snapshots 0 to 2 cannot be read at all: a search for a file that only
/// they held returns nothing, and says coverage is partial and why.
#[test]
fn c13_zero_hits_under_partial_coverage_are_reported_partial() {
    let s = SimStorage::new();
    fixture(&s);
    let h = commit_history(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    let (img0, snap0) = checkpoint_refs(&h[0]);
    damage(&mut bytes, img0);
    damage(&mut bytes, snap0);
    damage(&mut bytes, checkpoint_refs(&h[3]).0);
    let damaged = SimStorage::from_bytes(bytes);

    let r = run(&damaged, SnapshotScope::All, &name("Invoice")).unwrap();
    assert!(r.hits.is_empty());
    assert!(!r.coverage.complete());
    assert_eq!(r.coverage.searched, vec![3, 4]);
    let missing: Vec<u64> = r.coverage.unavailable.iter().map(|u| u.seq).collect();
    assert_eq!(missing, vec![0, 1, 2]);
    for u in &r.coverage.unavailable {
        assert_eq!(u.error.code, ErrorCode::StoredIntegrityFailed, "{u:?}");
    }
    // The head alone is still complete.
    assert!(run(&damaged, SnapshotScope::Head, &name("Invoice"))
        .unwrap()
        .coverage
        .complete());
}

/// The retained scope needs the retention state; with the manifest it is
/// rebuilt from damaged, the search refuses instead of guessing a scope
/// (D10.10). Other scopes still answer.
#[test]
fn c13_retained_scope_refuses_unresolved_retention() {
    let s = SimStorage::new();
    fixture(&s);
    let h = commit_history(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).1);
    let damaged = SimStorage::from_bytes(bytes);
    let e = run(&damaged, SnapshotScope::Retained, &Query::default()).unwrap_err();
    assert_eq!(e.code, ErrorCode::RetentionUnresolved);
    let r = run(&damaged, SnapshotScope::All, &name("late")).unwrap();
    assert_eq!(found(&r), pairs(&[(4, "late.txt")]));
    assert!(r.coverage.complete());
}

#[test]
fn c13_commit_after_head_is_invalid_and_cancellation_is_honoured() {
    let s = SimStorage::new();
    fixture(&s);
    let e = run(&s, SnapshotScope::Commit(5), &Query::default()).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);

    let head = open_head(&s, &opts()).unwrap();
    let job = Job::new();
    job.cancel.cancel();
    let e = search(
        &s,
        &head,
        SnapshotScope::All,
        &Query::default(),
        &opts(),
        &job.ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
}

/// Hostile names are found as bytes and come back byte for byte.
#[test]
fn c13_hostile_and_non_utf8_names_round_trip() {
    let s = SimStorage::new();
    let mut w =
        ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(15)), test_options()).unwrap();
    let names: [&[u8]; 3] = [b"<img src=x onerror=alert(1)>", b"bad\xff", b"\x1b[31mred"];
    let mut tx = Transaction::new();
    for n in names {
        tx.put_file(
            mochi_core::catalog::path::ArchivePath::from_stored(n).unwrap(),
            n.to_vec(),
            attrs(0o644, 0),
        );
    }
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    for n in names {
        let q = Query {
            name: n.to_vec(),
            ..Query::default()
        };
        let r = run(&s, SnapshotScope::Head, &q).unwrap();
        let got: BTreeSet<&[u8]> = r.hits.iter().map(|h| h.path.as_stored()).collect();
        assert_eq!(got, BTreeSet::from([n]));
        assert_eq!(r.hits[0].content_hash, Some(hash(n)));
    }
}
