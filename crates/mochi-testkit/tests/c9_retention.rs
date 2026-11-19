//! C9: retention state (spec §16.3, §18.3, Annex B D18).
//!
//! Retention is manifest state: delta manifests carry the operations,
//! snapshot manifests the complete state, and readers rebuild it from S(*b*)
//! plus the segment's deltas (D10.10). The expectations come from the
//! operations each test performs, checked against what independent readers
//! rebuild from the stored archive: `segment_state`, baseline recovery,
//! and the decoded manifests themselves.

use std::collections::{BTreeMap, BTreeSet};

use mochi_core::commit::Metadata;
use mochi_core::manifest::{ManifestKind, RETENTION_SCHEMA_VERSION, SCHEMA_VERSION};
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, read_snapshot, recover_baseline_at_footer,
    segment_state, ArchiveWriter, CheckpointPolicy, CheckpointTamper, ReadOptions, TailPolicy,
    Transaction,
};
use mochi_core::retention::RetentionState;
use mochi_core::ErrorCode;
use mochi_format::cbor::CborLimits;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn create(s: &SimStorage, seed: u64) -> ArchiveWriter<SimStorage> {
    let mut w =
        ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(seed)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    w
}

fn reopen(s: &SimStorage, seed: u64) -> ArchiveWriter<SimStorage> {
    let mut w = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0;
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    w
}

fn file(i: u64) -> Transaction {
    let mut tx = Transaction::new();
    tx.put_file(
        path(&format!("f{i}")),
        format!("content {i}").into_bytes(),
        attrs(0o644, 0),
    );
    tx
}

fn run(w: &mut ArchiveWriter<SimStorage>, tx: &Transaction) -> mochi_core::Result<u64> {
    w.commit(tx.clone(), &Job::new().ctx()).map(|o| o.seq)
}

fn state(expired: &[u64], holds: &[(&str, u64)]) -> RetentionState {
    RetentionState {
        expired: expired.iter().copied().collect(),
        holds: holds
            .iter()
            .map(|(l, s)| (l.as_bytes().to_vec(), *s))
            .collect::<BTreeMap<_, _>>(),
    }
}

/// What a fresh reader rebuilds at the head.
fn rebuilt(s: &SimStorage) -> RetentionState {
    let head = open_head(s, &opts()).unwrap();
    segment_state(s, &head, &opts()).unwrap().retention
}

/// **D18.** Operations carried by deltas, across a forced checkpoint and a
/// writer reopen, rebuild to the expected state; the checkpoint's snapshot
/// manifest carries the complete state; roots follow from it.
#[test]
fn c9_retention_survives_deltas_checkpoints_and_reopening() {
    let s = SimStorage::new();
    let mut w = create(&s, 1);
    for i in 0..3 {
        run(&mut w, &file(i)).unwrap(); // 0, 1, 2
    }
    let mut tx = file(3);
    tx.expire(0).hold(b"legal", 1).expire(1);
    run(&mut w, &tx).unwrap(); // 3: delta
    assert_eq!(rebuilt(&s), state(&[0, 1], &[("legal", 1)]));

    w.request_checkpoint();
    run(&mut w, &file(4)).unwrap(); // 4: checkpoint carrying the state
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    assert!(head.commit.metadata.is_checkpoint());
    let snap = read_snapshot(&s, &head, &opts()).unwrap();
    assert_eq!(snap.retention, state(&[0, 1], &[("legal", 1)]));

    // Reopened, the writer knows the hold: releasing it is valid.
    let mut w = reopen(&s, 2);
    let mut tx = Transaction::new();
    tx.release(b"legal").expire(2);
    run(&mut w, &tx).unwrap(); // 5: retention only, no namespace change
    w.close().unwrap();
    let r = rebuilt(&s);
    assert_eq!(r, state(&[0, 1, 2], &[]));
    assert_eq!(r.roots(5), BTreeSet::from([3, 4, 5]));
}

/// **G2: a hold added after the last checkpoint survives.** It is only in a
/// delta manifest, yet a reopened writer, a fresh reader, and baseline
/// recovery from S(*b*) all see it, and a later checkpoint carries it.
#[test]
fn c9_a_hold_added_after_the_last_checkpoint_survives() {
    let s = SimStorage::new();
    let mut w = create(&s, 3);
    run(&mut w, &file(0)).unwrap(); // 0: checkpoint
    run(&mut w, &file(1)).unwrap(); // 1: delta
    let mut tx = file(2);
    tx.hold(b"case-17", 1).expire(1);
    run(&mut w, &tx).unwrap(); // 2: delta with the hold
    w.close().unwrap();

    let want = state(&[1], &[("case-17", 1)]);
    assert_eq!(rebuilt(&s), want);
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.segment.base_seq, 0);
    let footer = head.location.footer.footer_offset;
    let baseline = recover_baseline_at_footer(&s, footer, &opts()).unwrap();
    assert_eq!(baseline.retention, want);
    assert!(want.roots(2).contains(&1), "held, so still a root");

    let mut w = reopen(&s, 4);
    w.request_checkpoint();
    run(&mut w, &file(3)).unwrap(); // 3: checkpoint
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_snapshot(&s, &head, &opts()).unwrap().retention, want);
}

/// Invalid operations are the caller's error (`INVALID_ARGUMENT`), found
/// before anything is written: the archive is byte-identical and the writer
/// keeps working.
#[test]
fn c9_invalid_retention_operations_write_nothing() {
    let s = SimStorage::new();
    let mut w = create(&s, 5);
    run(&mut w, &file(0)).unwrap();
    let mut tx = Transaction::new();
    tx.hold(b"h", 0);
    run(&mut w, &tx).unwrap(); // 1
    let before = s.contents();
    let mut cases: Vec<Transaction> = Vec::new();
    for build in [
        |t: &mut Transaction| {
            t.expire(2);
        },
        |t: &mut Transaction| {
            t.expire(3);
        },
        |t: &mut Transaction| {
            t.hold(b"h", 0);
        },
        |t: &mut Transaction| {
            t.hold(b"", 0);
        },
        |t: &mut Transaction| {
            t.hold(b"x", 3);
        },
        |t: &mut Transaction| {
            t.release(b"nobody");
        },
        |t: &mut Transaction| {
            t.expire(0).expire(0);
        },
    ] {
        let mut tx = file(9);
        build(&mut tx);
        cases.push(tx);
    }
    for tx in &cases {
        let e = run(&mut w, tx).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{tx:?}: {e}");
        assert_eq!(s.contents(), before, "{tx:?}");
    }
    let mut tx = Transaction::new();
    tx.release(b"h").expire(0);
    assert_eq!(run(&mut w, &tx).unwrap(), 2);
}

/// Schema 2 is used exactly when a manifest carries retention data: a
/// delta with operations and a snapshot with a non-empty state. Every
/// other manifest keeps schema 1, so archives without retention are
/// unchanged on the wire.
#[test]
fn c9_schema_2_only_where_there_is_retention_data() {
    let s = SimStorage::new();
    let mut w = create(&s, 6);
    run(&mut w, &file(0)).unwrap(); // 0: checkpoint, no retention
    let mut tx = file(1);
    tx.hold(b"h", 0);
    run(&mut w, &tx).unwrap(); // 1: delta with an operation
    run(&mut w, &file(2)).unwrap(); // 2: delta without one
    w.request_checkpoint();
    run(&mut w, &file(3)).unwrap(); // 3: checkpoint with a state
    w.close().unwrap();

    let schema = |r: &mochi_core::commit::ObjectRef| {
        let mut buf = vec![0u8; usize::try_from(r.stored_len).unwrap()];
        mochi_core::storage::ReadStorage::read_exact_at(&s, r.offset, &mut buf).unwrap();
        let (m, _) = mochi_core::manifest::Manifest::from_stored(
            &mochi_format::repr::StoredObject::from_loaded(buf),
            &opts().limits,
            &CborLimits::default(),
        )
        .unwrap();
        (m.kind, m.schema_version())
    };
    let mut got = Vec::new();
    for h in commit_history(&s, &opts()).unwrap() {
        got.push(schema(&h.commit.delta_manifest));
        if let Metadata::Checkpoint { snapshot, .. } = h.commit.metadata {
            got.push(schema(&snapshot));
        }
    }
    use ManifestKind::{Delta, Snapshot};
    assert_eq!(
        got,
        [
            (Delta, SCHEMA_VERSION),
            (Snapshot, SCHEMA_VERSION),
            (Delta, RETENTION_SCHEMA_VERSION),
            (Delta, SCHEMA_VERSION),
            (Delta, SCHEMA_VERSION),
            (Snapshot, RETENTION_SCHEMA_VERSION),
        ]
    );
    // Every commit still opens at its own footer.
    for h in commit_history(&s, &opts()).unwrap() {
        open_at_footer(&s, h.footer_offset, &opts()).unwrap();
    }
}

/// **D10.7 adoption covers retention.** A snapshot manifest whose retention
/// state differs from the writer's is `CHECKPOINT_MISMATCH`: no new head,
/// the unpublished bytes are rolled back. Both directions: holds dropped,
/// and an expiry invented.
#[test]
fn c9_adoption_refuses_a_snapshot_with_the_wrong_retention() {
    for with_hold in [true, false] {
        let s = SimStorage::new();
        let mut w = create(&s, 7);
        run(&mut w, &file(0)).unwrap();
        if with_hold {
            let mut tx = Transaction::new();
            tx.hold(b"h", 0);
            run(&mut w, &tx).unwrap();
        } else {
            run(&mut w, &file(1)).unwrap();
        }
        let before = s.contents();
        w.set_checkpoint_tamper(Some(CheckpointTamper::SnapshotRetention));
        w.request_checkpoint();
        let e = run(&mut w, &file(2)).unwrap_err();
        assert_eq!(e.code, ErrorCode::CheckpointMismatch, "{e}");
        assert!(
            e.message.contains("retention")
                || e.message.contains("expired")
                || e.message.contains("hold"),
            "{e}"
        );
        assert_eq!(s.contents(), before);
        assert_eq!(open_head(&s, &opts()).unwrap().seq(), 1);
    }
}
