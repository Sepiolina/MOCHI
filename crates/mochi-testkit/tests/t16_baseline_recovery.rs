//! T16: baseline recovery (spec Annex B.2 D10.8; gate G2).
//!
//! Recovery from the checkpoint at *b* starts from snapshot manifest S(*b*)
//! and applies the delta manifests after *b*. It needs commit *b*'s record,
//! and **neither SQLite, nor delta manifest *b*, nor any earlier manifest**.
//!
//! "Equal" below always means: the recovered catalog's authoritative state at
//! each commit from *b* to the head equals the state of the same commit
//! opened from the **undamaged** original, and the recovered promised
//! attributes equal the transaction model's. Plan: `docs/t16-t17-t15-plan.md`
//! section 3.

use mochi_core::catalog::namespace::FileVersionId;
use mochi_core::catalog::{Commit, FileVersion, SegmentApplier};
use mochi_core::commit::Metadata;
use mochi_core::manifest::Manifest;
use mochi_core::publish::{
    commit_history, open_at_footer, read_snapshot, recover_baseline_at_footer,
    recover_with_trusted_head, BaselineRecovery, CheckpointPolicy, HistoryEntry, ReadOptions,
};
use mochi_core::recovery::{catalog_from_snapshot, RecoveryScope};
use mochi_core::state::AuthoritativeState;
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::forge::{empty_delta, link, rule_base, txid, Forge};
use mochi_testkit::replay::{
    attrs_by_path, checkpoint_refs, damage, history, history_10, history_long, range, wipe, within,
    write, Step, Tracing,
};
use mochi_testkit::SimStorage;

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// cp0 d1 d2 d3 **cp4** d5 d6 d7: head 7, base 4.
fn fixture_a() -> (SimStorage, Vec<HistoryEntry>, Vec<Step>) {
    let steps: Vec<Step> = history_long().into_iter().take(8).collect();
    let s = write(CheckpointPolicy::Every(4), &steps);
    let h = commit_history(&s, &opts()).unwrap();
    assert_eq!(h.len(), 8);
    (s, h, steps)
}

fn state_at(src: &SimStorage, e: &HistoryEntry) -> AuthoritativeState {
    let o = open_at_footer(src, e.footer_offset, &opts()).unwrap();
    AuthoritativeState::from_catalog(&o.catalog, e.commit.seq).unwrap()
}

/// The recovery's states from its base to the head equal the original's, and
/// its attributes equal the model's.
fn assert_recovered(
    rec: &BaselineRecovery,
    original: &SimStorage,
    hist: &[HistoryEntry],
    steps: &[Step],
) {
    let (b, h) = (rec.segment.base_seq, rec.head_seq);
    for s in b..=h {
        let got = AuthoritativeState::from_catalog(&rec.catalog, s).unwrap();
        let want = state_at(original, &hist[s as usize]);
        assert_eq!(
            got.differences(&want, 8),
            Vec::<String>::new(),
            "state at commit {s} (base {b}, head {h})"
        );
    }
    assert_eq!(rec.catalog.head_commit().unwrap(), Some(h));
    let ns = rec.catalog.replay(Some(h)).unwrap();
    assert_eq!(
        attrs_by_path(&ns, &rec.attributes),
        steps[h as usize].attrs,
        "promised attributes at the head"
    );
}

fn recover_at(src: &SimStorage, e: &HistoryEntry) -> mochi_core::Result<BaselineRecovery> {
    recover_baseline_at_footer(src, e.footer_offset, &opts())
}

// ---- catalog_from_snapshot --------------------------------------------------------

/// A catalog built from S(c) has the same authoritative state as checkpoint
/// c's image, materializes commit c, and starts a segment at c.
#[test]
fn catalog_from_snapshot_matches_the_image() {
    let steps = history_long();
    let s = write(CheckpointPolicy::Every(3), &steps);
    for e in commit_history(&s, &opts()).unwrap() {
        if !e.commit.metadata.is_checkpoint() {
            continue;
        }
        let c = e.commit.seq;
        let opened = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
        let snap = read_snapshot(&s, &opened, &opts()).unwrap();
        let cat = catalog_from_snapshot(&snap).unwrap();
        assert_eq!(cat.head_commit().unwrap(), Some(c));
        assert_eq!(
            AuthoritativeState::from_catalog(&cat, c).unwrap(),
            AuthoritativeState::from_catalog(&opened.catalog, c).unwrap(),
            "checkpoint {c}"
        );
        // It can start a segment at c.
        assert_eq!(SegmentApplier::new(cat).unwrap().head(), c);
    }
}

#[test]
fn catalog_from_snapshot_refuses_a_delta() {
    let s = write(CheckpointPolicy::Never, &history()[..2]);
    let h = commit_history(&s, &opts()).unwrap();
    let delta: Manifest = empty_delta(&h[1], txid(1));
    let e = catalog_from_snapshot(&delta).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

// ---- the DoD rows ------------------------------------------------------------------

/// **Checklist DoD.** Recovery succeeds with every image deleted, delta *b*
/// deleted, and all earlier manifests deleted.
#[test]
fn t16_recovers_with_images_delta_b_and_earlier_manifests_destroyed() {
    let (s, h, steps) = fixture_a();
    let mut bytes = s.contents();
    let (image0, snap0) = checkpoint_refs(&h[0]);
    let (image4, _) = checkpoint_refs(&h[4]);
    wipe(&mut bytes, image0);
    wipe(&mut bytes, image4);
    wipe(&mut bytes, snap0);
    wipe(&mut bytes, h[4].commit.delta_manifest); // delta b
    for e in &h[..4] {
        wipe(&mut bytes, e.commit.delta_manifest); // every earlier delta
    }
    let damaged = SimStorage::from_bytes(bytes);

    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();
    // The manifest chain cannot reach the head any more ...
    if let Ok(c) = &rec.chain {
        assert_ne!(c.snapshot_range.map(|r| r.1), Some(7));
    }
    // ... baseline recovery does.
    let b = rec.baseline.as_ref().expect("attempted").as_ref().unwrap();
    assert_eq!(b.segment.base_seq, 4);
    assert_recovered(b, &s, &h, &steps);
    assert_eq!(rec.scope_for(7), RecoveryScope::SnapshotRecovery);
    assert_eq!(rec.scope_for(5), RecoveryScope::HistoricalRecovery);
    assert_eq!(rec.scope_for(4), RecoveryScope::HistoricalRecovery);
    assert!(!matches!(
        rec.scope_for(2),
        RecoveryScope::SnapshotRecovery | RecoveryScope::HistoricalRecovery
    ));
    let a = rec.head_attributes().unwrap();
    assert_eq!(
        attrs_by_path(&rec.head_catalog().unwrap().replay(Some(7)).unwrap(), a),
        steps[7].attrs
    );
}

/// Nothing before *b* is needed: every byte of commits 0–3 (objects, commit
/// records, footers) is zeroed, leaving only the descriptor.
#[test]
fn t16_recovers_with_everything_before_b_destroyed() {
    let (s, h, steps) = fixture_a();
    let mut bytes = s.contents();
    let d = h[0].commit.descriptor;
    let from = (d.offset + d.stored_len) as usize;
    let to = (h[3].footer_offset + FOOTER_FRAME_LEN) as usize;
    bytes[from..to].fill(0);
    let damaged = SimStorage::from_bytes(bytes);
    let rec = recover_at(&damaged, &h[7]).unwrap();
    assert_eq!(rec.segment.base_seq, 4);
    assert_recovered(&rec, &s, &h, &steps);
}

/// D10.8's input list, as reads: the descriptor, commit records and footers
/// *b* … *h*, S(*b*), and deltas *b*+1 … *h*. Not image *b*, not delta *b*,
/// nothing of commits 0 to 3.
#[test]
fn t16_baseline_reads_only_b_record_s_b_and_later_deltas() {
    let (s, h, _) = fixture_a();
    let t = Tracing {
        inner: &s,
        reads: Default::default(),
    };
    recover_at_traced(&t, &h[7]);

    let d = h[0].commit.descriptor;
    let mut allowed = vec![range(d)];
    for e in &h[4..=7] {
        allowed.push((e.commit_offset, e.footer_offset + FOOTER_FRAME_LEN));
    }
    let (image4, snap4) = checkpoint_refs(&h[4]);
    allowed.push(range(snap4));
    for e in &h[5..=7] {
        allowed.push(range(e.commit.delta_manifest));
    }
    let forbidden = [
        range(image4),
        range(h[4].commit.delta_manifest),
        (
            d.offset + d.stored_len,
            h[3].footer_offset + FOOTER_FRAME_LEN,
        ),
    ];
    let reads = t.reads.into_inner();
    assert!(!reads.is_empty());
    for r in &reads {
        assert!(within(*r, &allowed), "read {r:?} outside D10.8's inputs");
        assert!(
            !forbidden.iter().any(|&(lo, hi)| r.0 < hi && lo < r.0 + r.1),
            "read {r:?} touches image b, delta b, or commits before b"
        );
    }
    assert!(
        reads.iter().any(|r| within(*r, &[range(snap4)])),
        "S(b) was read"
    );
}

fn recover_at_traced(t: &Tracing<'_>, e: &HistoryEntry) {
    recover_baseline_at_footer(t, e.footer_offset, &opts()).unwrap();
}

/// A checkpoint head (*b* = *h*) recovers from its snapshot alone.
#[test]
fn t16_checkpoint_head_recovers_from_its_snapshot_alone() {
    let (s, h, steps) = fixture_a();
    let mut bytes = s.contents();
    wipe(&mut bytes, checkpoint_refs(&h[4]).0);
    wipe(&mut bytes, h[4].commit.delta_manifest);
    let damaged = SimStorage::from_bytes(bytes);
    let rec = recover_at(&damaged, &h[4]).unwrap();
    assert_eq!(rec.segment.base_seq, 4);
    assert_recovered(&rec, &s, &h, &steps);
}

/// Undamaged control: for every head under every policy, baseline recovery
/// equals what opening gives, and the model.
#[test]
fn t16_matches_open_for_every_head_under_every_policy() {
    let steps = history();
    for policy in [
        CheckpointPolicy::EveryCommit,
        CheckpointPolicy::Never,
        CheckpointPolicy::Every(2),
        CheckpointPolicy::Every(3),
    ] {
        let s = write(policy, &steps);
        let h = commit_history(&s, &opts()).unwrap();
        for e in &h {
            let rec = recover_at(&s, e)
                .unwrap_or_else(|err| panic!("{policy:?} head {}: {err}", e.commit.seq));
            assert_recovered(&rec, &s, &h, &steps);
        }
    }
}

// ---- refusals: no earlier checkpoint, no partial result -----------------------------

/// A damaged S(*b*) is refused; baseline recovery never retries from an
/// earlier checkpoint (D10.4, D10.6).
#[test]
fn t16_damaged_snapshot_refuses_without_trying_an_earlier_checkpoint() {
    let (s, h, _) = fixture_a();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[4]).1);
    let damaged = SimStorage::from_bytes(bytes.clone());
    assert_eq!(
        recover_at(&damaged, &h[7]).unwrap_err().code,
        ErrorCode::StoredIntegrityFailed
    );
    // The delta chain is intact, so the trusted recovery reaches the head
    // without baseline recovery.
    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();
    assert_eq!(
        rec.chain.as_ref().unwrap().snapshot_range.map(|r| r.1),
        Some(7)
    );
    assert!(rec.baseline.is_none());

    // With delta 2 gone too the chain breaks; baseline is attempted and
    // refuses, and no snapshot of the head is claimed.
    wipe(&mut bytes, h[2].commit.delta_manifest);
    let damaged = SimStorage::from_bytes(bytes);
    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();
    let e = rec
        .baseline
        .as_ref()
        .expect("attempted")
        .as_ref()
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    assert_ne!(rec.scope_for(7), RecoveryScope::SnapshotRecovery);
    assert!(rec.head_catalog().is_none());
}

#[test]
fn t16_damaged_delta_in_segment_refuses() {
    let (s, h, _) = fixture_a();
    let mut bytes = s.contents();
    damage(&mut bytes, h[6].commit.delta_manifest);
    let damaged = SimStorage::from_bytes(bytes);
    assert_eq!(
        recover_at(&damaged, &h[7]).unwrap_err().code,
        ErrorCode::StoredIntegrityFailed
    );
}

/// Delta *b*+1's parent link is checked against commit *b*'s record (its key
/// 6), not against S(*b*) (review Q22).
#[test]
fn t16_first_delta_link_is_checked_against_commit_b_record() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..1]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    let Metadata::Checkpoint { snapshot, .. } = h[0].commit.metadata else {
        unreachable!()
    };
    let mut m = empty_delta(&h[0], txid(20));
    m.parent.as_mut().unwrap().delta_manifest_hash = snapshot.stored_hash;
    let r = f.append_manifest(&m);
    let d1 = f.append_delta(&h[0], rule_base(&h[0]), r, txid(20));
    let e = recover_baseline_at_footer(&f.storage(), d1.footer_offset, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::RecordInvalid);
    assert!(e.message.contains("key 6"), "{e}");

    // Control: the correct link recovers.
    let mut g = Forge::new(s.contents());
    let m = empty_delta(&h[0], txid(21));
    let r = g.append_manifest(&m);
    let d1 = g.append_delta(&h[0], rule_base(&h[0]), r, txid(21));
    let rec = recover_baseline_at_footer(&g.storage(), d1.footer_offset, &opts()).unwrap();
    assert_eq!(rec.head_seq, 1);
    assert_eq!(rec.segment.base_seq, 0);
}

/// A checkpoint commit whose snapshot reference names *another* commit's
/// snapshot (by its correct hash) is refused by D11 identity binding.
#[test]
fn t16_snapshot_bound_to_another_commit_is_refused() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..2]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    let mut rec = h[1].commit.clone();
    rec.seq = 2;
    rec.transaction_id = txid(30);
    rec.parent = Some(link(&h[1]));
    let forged = f.append_commit(&rec);
    let e = recover_baseline_at_footer(&f.storage(), forged.footer_offset, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::EnvelopeInvalid, "{e}");
}

/// Baseline recovery trusts S(*b*) as published. A snapshot that is
/// internally consistent but disagrees with its image (the Q24 forge) is
/// *not* detected here: that disagreement is the adoption check's (T15,
/// `check_checkpoint_representations`). What recovery returns is what the
/// snapshot says.
#[test]
fn t16_a_snapshot_that_disagrees_with_its_image_recovers_what_the_snapshot_says() {
    let (f, cp2, missing) = mochi_testkit::replay::checkpoint_with_incomplete_snapshot();
    let s = f.storage();
    let rec = recover_baseline_at_footer(&s, cp2.footer_offset, &opts()).unwrap();
    let ns = rec.catalog.replay(Some(2)).unwrap();
    assert!(
        ns.iter().all(|(p, _)| p.as_stored() != missing.as_slice()),
        "the snapshot omits the file, so recovery does too"
    );
    let opened = open_at_footer(&s, cp2.footer_offset, &opts()).unwrap();
    let image_ns = opened.catalog.replay(Some(2)).unwrap();
    assert!(image_ns
        .iter()
        .any(|(p, _)| p.as_stored() == missing.as_slice()));
}

#[test]
fn t16_bad_descriptor_refuses() {
    let (s, h, _) = fixture_a();
    let mut bytes = s.contents();
    damage(&mut bytes, h[0].commit.descriptor);
    let damaged = SimStorage::from_bytes(bytes);
    assert_eq!(
        recover_at(&damaged, &h[7]).unwrap_err().code,
        ErrorCode::DescriptorInvalid
    );
    // The same through the trusted entry point: the head is still located,
    // baseline recovery refuses.
    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();
    if let Some(b) = &rec.baseline {
        assert_eq!(b.as_ref().unwrap_err().code, ErrorCode::DescriptorInvalid);
    }
}

// ---- the trusted entry point --------------------------------------------------------

/// The C4 case on a real archive: a missing delta before a checkpoint breaks
/// the manifest chain, and commit records bridge it.
#[test]
fn t16_missing_delta_before_a_checkpoint_is_bridged_by_commit_records() {
    let steps = history_10();
    let s = write(CheckpointPolicy::Every(6), &steps);
    let h = commit_history(&s, &opts()).unwrap();
    assert_eq!(h.len(), 10);
    let mut bytes = s.contents();
    wipe(&mut bytes, h[3].commit.delta_manifest);
    let damaged = SimStorage::from_bytes(bytes);
    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();

    let chain = rec.chain.as_ref().unwrap();
    assert_eq!(chain.chain_broken_at, Some(4));
    assert_eq!(chain.snapshot_range, None);

    let b = rec.baseline.as_ref().expect("attempted").as_ref().unwrap();
    assert_eq!(b.segment.base_seq, 6);
    assert_recovered(b, &s, &h, &steps);
    assert_eq!(rec.scope_for(9), RecoveryScope::SnapshotRecovery);
    assert_eq!(rec.scope_for(7), RecoveryScope::HistoricalRecovery);
    assert_eq!(rec.scope_for(3), RecoveryScope::FileRecovery);
}

#[test]
fn t16_baseline_not_attempted_when_the_chain_reaches_the_head() {
    let (s, _, _) = fixture_a();
    let rec = recover_with_trusted_head(&s, &opts()).unwrap();
    assert!(rec.baseline.is_none());
    assert_eq!(rec.head_seq, 7);
    assert!(rec.head_catalog().is_some());
    assert_eq!(rec.scope_for(7), RecoveryScope::SnapshotRecovery);
    assert_eq!(rec.scope_for(0), RecoveryScope::HistoricalRecovery);
}

/// The recovered catalog is query-only, like a reader's.
#[test]
fn t16_recovered_catalog_is_query_only() {
    use mochi_core::catalog::namespace::EntryKind;
    let (s, h, _) = fixture_a();
    let rec = recover_at(&s, &h[7]).unwrap();
    let mut cat = rec.catalog;
    let head = cat.head_commit().unwrap().unwrap();
    assert_eq!(
        cat.set_meta("probe", b"x").unwrap_err().code,
        ErrorCode::CatalogInvalid
    );
    assert_eq!(
        cat.insert_file_version(
            &FileVersion {
                id: FileVersionId::from_bytes([0xEF; 32]),
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            &[],
        )
        .unwrap_err()
        .code,
        ErrorCode::CatalogInvalid
    );
    assert_eq!(
        cat.append_commit(&Commit {
            seq: head + 1,
            parent: Some(head),
            ops: Vec::new(),
        })
        .unwrap_err()
        .code,
        ErrorCode::CatalogInvalid
    );
}
