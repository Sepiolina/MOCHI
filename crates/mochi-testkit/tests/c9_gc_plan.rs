//! C9: GC planning (spec §18.3, Annex B D10.10, D18; `mochi_core::gc`).
//!
//! The oracle marks independently of the planner: one `Catalog::replay` per
//! root (the planner walks the history once), and roots computed from the
//! operations each test performs.

use std::collections::BTreeSet;

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::commit::Metadata;
use mochi_core::gc::plan;
use mochi_core::publish::{open_head, ArchiveWriter, CheckpointPolicy, ReadOptions, Transaction};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(path(p), deterministic_bytes(seed, 150), attrs(0o644, 0));
}

/// 0: a=1, b=2. 1: a=3. 2: c=4, delete b. 3: whatever `last` adds.
fn history(s: &SimStorage, last: impl FnOnce(&mut Transaction)) {
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1);
    put(&mut tx, "b", 2);
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 3);
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "c", 4);
    tx.delete(path("b"));
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    last(&mut tx);
    if !tx.is_empty() {
        w.commit(tx, &job.ctx()).unwrap();
    }
    w.close().unwrap();
}

/// Chunks reachable from `roots`, one replay per root.
fn oracle_chunks(s: &SimStorage, roots: &[u64]) -> BTreeSet<String> {
    let head = open_head(s, &opts()).unwrap();
    let mut out = BTreeSet::new();
    for r in roots {
        for (_, e) in head.catalog.replay(Some(*r)).unwrap().iter() {
            let (_, extents) = head.catalog.file_version(&e.version).unwrap().unwrap();
            for x in extents {
                if let ExtentSource::Chunk { chunk, .. } = x.source {
                    out.insert(chunk.to_hex());
                }
            }
        }
    }
    out
}

fn all_chunks(s: &SimStorage) -> BTreeSet<String> {
    let head = open_head(s, &opts()).unwrap();
    head.catalog
        .object_ids()
        .unwrap()
        .iter()
        .map(|i| i.to_hex())
        .collect()
}

/// Without retention operations every commit is a root and nothing is
/// collectable. Planning is read-only.
#[test]
fn c9_nothing_is_collectable_by_default() {
    let s = SimStorage::new();
    history(&s, |_| {});
    let before = s.contents();
    let p = plan(&s, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(s.contents(), before, "planning writes nothing");
    assert_eq!(p.roots, [0, 1, 2]);
    assert!(!p.collects_anything());
    assert_eq!(p.retained.chunks as usize, all_chunks(&s).len());
}

/// **§18.3 marking.** Expiring snapshots 0 and 1 leaves 2 and 3 as roots.
/// Collectable is exactly what only 0 and 1 reach: `a` as commit 0 wrote it
/// (seed 1) and `b` (seed 2), each snapshot with its reason. Everything 2
/// and 3 reach is retained, whenever it was written: `a` as commit 1 wrote
/// it is still at 2.
#[test]
fn c9_expired_snapshots_are_collectable_and_explained() {
    let s = SimStorage::new();
    history(&s, |tx| {
        tx.expire(0).expire(1);
    });
    let p = plan(&s, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.roots, [2, 3]);
    assert_eq!(p.expired, [0, 1]);
    let seqs: Vec<u64> = p.collectable_snapshots.iter().map(|c| c.seq).collect();
    assert_eq!(seqs, [0, 1]);
    assert!(p
        .collectable_snapshots
        .iter()
        .all(|c| c.reason.contains("expired") && c.commit_id.len() == 64));

    let kept = oracle_chunks(&s, &[2, 3]);
    let want: BTreeSet<String> = all_chunks(&s).difference(&kept).cloned().collect();
    assert!(!want.is_empty());
    let got: BTreeSet<String> = p.collectable_chunks.iter().cloned().collect();
    assert_eq!(got, want);
    assert_eq!(p.retained.chunks as usize, kept.len());
    // a@0 and b@0: the two versions only expired snapshots reach.
    assert_eq!(p.collectable.file_versions, 2);
    assert_eq!(p.collectable_versions.len(), 2);
    assert!(p.collectable.stored_bytes > 0);
}

/// **Fault-matrix row "retention expires under legal hold".** An expired
/// snapshot under a hold stays a root and nothing it reaches is
/// collectable; once released, it is.
#[test]
fn c9_a_legal_hold_protects_an_expired_snapshot() {
    let s = SimStorage::new();
    history(&s, |tx| {
        tx.hold(b"litigation", 0).expire(0);
    });
    let p = plan(&s, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.roots, [0, 1, 2, 3]);
    assert!(!p.collects_anything());
    assert_eq!(p.holds.len(), 1);
    assert_eq!(
        (p.holds[0].label.as_str(), p.holds[0].seq),
        ("litigation", 0)
    );

    // The same archive after a release: 0 is collectable.
    let mut w = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(2)),
        test_options(),
        mochi_core::publish::TailPolicy::Refuse,
    )
    .unwrap()
    .0;
    let mut tx = Transaction::new();
    tx.release(b"litigation");
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let p = plan(&s, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.roots, [1, 2, 3, 4]);
    let kept = oracle_chunks(&s, &[1, 2, 3, 4]);
    let want: BTreeSet<String> = all_chunks(&s).difference(&kept).cloned().collect();
    assert_eq!(
        p.collectable_chunks
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>(),
        want
    );
}

/// **G2: GC refuses when retention state is unresolved (D10.10).** A
/// damaged snapshot manifest at the segment base leaves reads working
/// (D10.9: the image serves), but retention cannot be rebuilt, so planning
/// is `RETENTION_UNRESOLVED` and never falls back to an older checkpoint.
#[test]
fn c9_gc_refuses_when_retention_is_unresolved() {
    let s = SimStorage::new();
    history(&s, |tx| {
        tx.hold(b"h", 1);
    });
    // Force a checkpoint at the head, then damage its snapshot manifest.
    let mut w = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(3)),
        test_options(),
        mochi_core::publish::TailPolicy::Refuse,
    )
    .unwrap()
    .0;
    w.request_checkpoint();
    let mut tx = Transaction::new();
    tx.expire(0);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let Metadata::Checkpoint { snapshot, .. } = head.commit.metadata else {
        panic!("the head is a checkpoint");
    };
    let mut bytes = s.contents();
    bytes[snapshot.offset as usize + 30] ^= 0x01;
    let damaged = SimStorage::from_bytes(bytes);

    let head = open_head(&damaged, &opts()).expect("reads continue (D10.9)");
    assert_eq!(head.seq(), 4);
    let e = plan(&damaged, &opts(), &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::RetentionUnresolved, "{e}");
}
