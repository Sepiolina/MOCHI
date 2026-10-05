//! T11 + T12 through the real open paths (`open_head`, `open_at_footer`,
//! `open_append`), on archives the real writer produced under the test
//! checkpoint policies (review decision 14), plus forged commits appended on
//! top for the reject rows (`mochi_testkit::forge`).
//!
//! Rows of `docs/t11-t12-acceptance.md` covered here are named in each
//! test's doc comment. Not here yet: the oracle property test (amendment 4),
//! golden vectors, and the fuzz exerciser.

use std::cell::RefCell;
use std::collections::BTreeMap;

use mochi_core::catalog::namespace::FileVersionId;
use mochi_core::catalog::Catalog;
use mochi_core::commit::{CommitLink, Metadata};
use mochi_core::manifest::Attributes;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, read_snapshot, ArchiveWriter, CheckpointPolicy,
    HistoryEntry, OpenedHead, ReadOptions, TailPolicy, Transaction,
};
use mochi_core::ErrorCode;
use mochi_format::cbor::Value;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{path, read_state, test_options, Content, Job, State};
use mochi_testkit::forge::{self, empty_delta, link, rule_base, txid, Forge};
use mochi_testkit::replay::{
    append, attrs, checkpoint_with_incomplete_snapshot, flip, history, is_cp, open_append_err,
    snapshot_attrs, within, write, Step, Tracing,
};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};
use proptest::prelude::*;

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn code_at(s: &SimStorage, e: &HistoryEntry) -> Result<OpenedHead, ErrorCode> {
    open_at_footer(s, e.footer_offset, &opts()).map_err(|e| e.code)
}

// ---- decision 14: the test checkpoint policy ----------------------------------------

/// Decision 14: production default is `EveryCommit`; `Every(0)` is refused
/// and leaves the policy unchanged; commit 0 is a checkpoint under every
/// policy.
#[test]
fn checkpoint_policy_controls() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    assert_eq!(w.checkpoint_policy(), CheckpointPolicy::EveryCommit);
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let e = w
        .set_checkpoint_policy(CheckpointPolicy::Every(0))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(
        w.checkpoint_policy(),
        CheckpointPolicy::Never,
        "a refused policy changes nothing"
    );
    drop(w);

    for policy in [
        CheckpointPolicy::Never,
        CheckpointPolicy::Every(2),
        CheckpointPolicy::Every(7),
        CheckpointPolicy::EveryCommit,
    ] {
        let s = write(policy, &history()[..1]);
        let h = commit_history(&s, &opts()).unwrap();
        assert!(is_cp(&h[0]), "{policy:?}: commit 0 must be a checkpoint");
    }

    // A reopened writer starts at the production default again.
    let s = write(CheckpointPolicy::Never, &history()[..2]);
    let (w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(2)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    assert_eq!(w.checkpoint_policy(), CheckpointPolicy::EveryCommit);
}

fn expect_cp(policy: CheckpointPolicy, seq: u64) -> bool {
    seq == 0
        || match policy {
            CheckpointPolicy::EveryCommit => true,
            CheckpointPolicy::Never => false,
            CheckpointPolicy::Every(n) => seq.is_multiple_of(n),
        }
}

fn policy_strategy() -> impl Strategy<Value = CheckpointPolicy> {
    prop_oneof![
        Just(CheckpointPolicy::EveryCommit),
        Just(CheckpointPolicy::Never),
        (1u64..=8).prop_map(CheckpointPolicy::Every),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    /// T11 "Writer derivation": every delta's base equals the rule (the
    /// parent if it is a checkpoint, else the parent's base), across
    /// policies and across a reopen at an arbitrary commit, including
    /// reopening on a delta head; and every commit opens to its model state.
    #[test]
    fn writer_base_derivation_follows_the_rule(
        first in policy_strategy(),
        second in policy_strategy(),
        n in 1usize..=9,
        split in 0usize..=9,
    ) {
        let split = split.min(n).max(1);
        let mut steps = Vec::new();
        let mut state = State::new();
        for i in 0..n {
            let mut tx = Transaction::new();
            let p = format!("f{}", i % 4);
            let bytes = deterministic_bytes(i as u64, 20 + i);
            tx.put_file(path(&p), bytes.clone(), attrs(0o600 + i as u32, i as i64));
            state.insert(p.into_bytes(), Content::File(bytes));
            if i % 3 == 2 {
                tx.delete(path("f0"));
                state.remove(b"f0".as_slice());
            }
            steps.push(Step { tx, after: state.clone(), attrs: BTreeMap::new() });
        }
        let s = write(first, &steps[..split]);
        if split < n {
            append(&s, 99, second, &steps[split..]);
        }
        let h = commit_history(&s, &opts()).unwrap();
        prop_assert_eq!(h.len(), n);
        for (i, e) in h.iter().enumerate() {
            let policy = if i < split { first } else { second };
            prop_assert_eq!(is_cp(e), expect_cp(policy, i as u64), "commit {} under {:?}", i, policy);
            if let Metadata::Delta { base } = e.commit.metadata {
                prop_assert_eq!(base, rule_base(&h[i - 1]), "commit {}", i);
            }
            let opened = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
            prop_assert_eq!(read_state(&s, &opened).unwrap(), steps[i].after.clone());
        }
    }
}

// ---- opening every head under every policy --------------------------------------

/// Every commit, opened by footer and as the head, equals its model state
/// under every policy; its catalog materializes it; the segment base is the
/// one the rule names. A reader's replayed catalog is query-only, like a
/// checkpoint's.
#[test]
fn every_commit_opens_to_its_model_state_under_every_policy() {
    let steps = history();
    for policy in [
        CheckpointPolicy::EveryCommit,
        CheckpointPolicy::Never,
        CheckpointPolicy::Every(2),
        CheckpointPolicy::Every(3),
        CheckpointPolicy::Every(7),
    ] {
        let s = write(policy, &steps);
        let h = commit_history(&s, &opts()).unwrap();
        for (i, e) in h.iter().enumerate() {
            let mut opened = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
            assert_eq!(
                read_state(&s, &opened).unwrap(),
                steps[i].after,
                "{policy:?} commit {i}"
            );
            assert_eq!(opened.catalog.head_commit().unwrap(), Some(i as u64));
            let expected_base = match e.commit.metadata {
                Metadata::Checkpoint { .. } => i as u64,
                Metadata::Delta { base } => base.seq,
            };
            assert_eq!(opened.segment.base_seq, expected_base);
            assert_eq!(opened.segment.base_hint_mismatch, None);
            assert!(
                opened.catalog.set_meta("probe", b"x").is_err(),
                "{policy:?} commit {i}: a reader's catalog must refuse writes"
            );
        }
        let head = open_head(&s, &opts()).unwrap();
        assert_eq!(read_state(&s, &head).unwrap(), steps.last().unwrap().after);
    }
}

/// The old C5 test `a_delta_on_base_head_is_refused_until_replay_exists`,
/// inverted now that replay exists: a hand-built, valid delta on a
/// checkpoint opens, through the Q22 rule with a checkpoint as the
/// preceding commit (its key 6, never its snapshot).
#[test]
fn a_hand_built_delta_on_a_checkpoint_opens() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..2]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    let prev = h.last().unwrap();
    assert!(is_cp(prev));
    let m = f.append_manifest(&empty_delta(prev, txid(0x5A)));
    let forged = f.append_delta(prev, rule_base(prev), m, txid(0x5A));
    let fs = f.storage();
    let opened = open_head(&fs, &opts()).unwrap();
    assert_eq!(opened.seq(), forged.commit.seq);
    assert_eq!(opened.segment.base_seq, prev.commit.seq);
    assert_eq!(read_state(&fs, &opened).unwrap(), history()[1].after);
}

// ---- T11 rows, integration ------------------------------------------------------

/// cp0, d1 (Never) and the forge positioned after d1.
fn cp0_d1() -> (Forge, Vec<HistoryEntry>) {
    let s = write(CheckpointPolicy::Never, &history()[..2]);
    let f = Forge::new(s.contents());
    let h = f.history();
    assert!(is_cp(&h[0]) && !is_cp(&h[1]));
    (f, h)
}

fn open_err(f: &Forge) -> mochi_core::MochiError {
    open_head(&f.storage(), &opts()).expect_err("must not open")
}

/// T11: tampered base ID, the record otherwise self-consistent.
#[test]
fn t11_tampered_base_id_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let m = f.append_manifest(&empty_delta(&h[1], txid(1)));
    let mut base = rule_base(&h[1]);
    base.commit_id = mochi_format::digest::CommitId::from_bytes([0xAB; 32]);
    f.append_delta(&h[1], base, m, txid(1));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// T11: base off the ancestry: another archive's checkpoint at the same
/// sequence, with its real ID and footer offset.
#[test]
fn t11_base_off_the_ancestry_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let other = SimStorage::new();
    let mut w =
        ArchiveWriter::create(other.clone(), Box::new(SeqIds::new(500)), test_options()).unwrap();
    w.commit(history()[0].tx.clone(), &Job::new().ctx())
        .unwrap();
    w.close().unwrap();
    let other_cp0 = commit_history(&other, &opts()).unwrap().remove(0);
    assert_ne!(other_cp0.commit_id, h[0].commit_id);
    let m = f.append_manifest(&empty_delta(&h[1], txid(2)));
    f.append_delta(&h[1], link(&other_cp0), m, txid(2));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// T11: the named base is a delta.
#[test]
fn t11_base_is_a_delta_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let m = f.append_manifest(&empty_delta(&h[1], txid(3)));
    f.append_delta(&h[1], link(&h[1]), m, txid(3));
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::RecordInvalid);
    assert!(e.message.contains("delta"), "{e}");
}

/// T11: the base skips a later checkpoint.
#[test]
fn t11_base_skipping_a_later_checkpoint_is_record_invalid() {
    let s = write(CheckpointPolicy::Every(2), &history()[..3]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    assert!(is_cp(&h[2]));
    let m = f.append_manifest(&empty_delta(&h[2], txid(4)));
    f.append_delta(&h[2], link(&h[0]), m, txid(4));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// T11: an inconsistent segment: an intermediate delta names another base
/// while the head names the right one. The head alone is consistent.
#[test]
fn t11_inconsistent_segment_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let m2 = f.append_manifest(&empty_delta(&h[1], txid(5)));
    let mut wrong = rule_base(&h[1]);
    wrong.commit_id = mochi_format::digest::CommitId::from_bytes([0xCD; 32]);
    let c2 = f.append_delta(&h[1], wrong, m2, txid(5));
    let m3 = f.append_manifest(&empty_delta(&c2, txid(6)));
    f.append_delta(&c2, rule_base(&h[1]), m3, txid(6));
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::RecordInvalid);
    assert!(
        e.message.contains("commit 2"),
        "the intermediate delta is named: {e}"
    );
}

/// T11 / decision 16: a descriptor reference that differs within the
/// segment (at an intermediate commit; the head's own descriptor is fine).
#[test]
fn t11_descriptor_differing_within_the_segment_is_descriptor_invalid() {
    let (mut f, h) = cp0_d1();
    let m2 = f.append_manifest(&empty_delta(&h[1], txid(7)));
    let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), m2, txid(7));
    rec.descriptor.stored_hash = mochi_format::digest::StoredObjectHash::from_bytes([0x11; 32]);
    let c2 = f.append_commit(&rec);
    let m3 = f.append_manifest(&empty_delta(&c2, txid(8)));
    // `delta_record` copies c2's (wrong) reference; the head must carry the
    // real one, or it fails on its own descriptor and the segment rule is
    // never reached (T18 review: this test previously did exactly that).
    let mut head = forge::delta_record(&c2, rule_base(&h[1]), m3, txid(8));
    head.descriptor = h[1].commit.descriptor;
    f.append_commit(&head);
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::DescriptorInvalid);
    assert!(
        e.message
            .contains("commit 2 references a different archive descriptor"),
        "the segment rule refused it: {e}"
    );
}

/// T11 / decision 17: a wrong base hint on an otherwise valid delta opens
/// through its authenticated ancestry, and the mismatch is reported.
#[test]
fn t11_wrong_base_hint_opens_and_is_reported() {
    let (mut f, h) = cp0_d1();
    let m = f.append_manifest(&empty_delta(&h[1], txid(9)));
    let mut base = rule_base(&h[1]);
    let wrong = base.footer_offset + 1;
    base.footer_offset = wrong;
    f.append_delta(&h[1], base, m, txid(9));
    let fs = f.storage();
    let opened = open_head(&fs, &opts()).unwrap();
    assert_eq!(opened.segment.base_hint_mismatch, Some(wrong));
    assert_eq!(opened.segment.base_footer_offset, h[0].footer_offset);
    assert_eq!(read_state(&fs, &opened).unwrap(), history()[1].after);
}

/// Build cp0, d1, then c2 whose parent traversal offset (key 4) is
/// `bad_offset`, then a valid c3 on top: the walk from the head must cross
/// c2's broken offset.
fn broken_parent_offset(bad_offset: impl FnOnce(&[HistoryEntry]) -> u64, n: u8) -> Forge {
    let (mut f, h) = cp0_d1();
    let m2 = f.append_manifest(&empty_delta(&h[1], txid(n)));
    let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), m2, txid(n));
    rec.parent = Some(CommitLink {
        footer_offset: bad_offset(&h),
        ..rec.parent.unwrap()
    });
    let c2 = f.append_commit(&rec);
    let m3 = f.append_manifest(&empty_delta(&c2, txid(n + 1)));
    f.append_delta(&c2, rule_base(&h[1]), m3, txid(n + 1));
    f
}

/// Neither open path returns any state for the forged head: no search, no
/// fallback to an earlier head (D10.6 as amended).
fn assert_no_state(f: &Forge, code: ErrorCode) {
    let s = f.storage();
    assert_eq!(
        open_head(&s, &opts()).map(|_| ()).unwrap_err().code,
        code,
        "open_head"
    );
    let head_footer = s.contents().len() as u64 - FOOTER_FRAME_LEN;
    assert_eq!(
        open_at_footer(&s, head_footer, &opts())
            .map(|_| ())
            .unwrap_err()
            .code,
        code,
        "open_at_footer at the head"
    );
}

/// D10.6 (amended 2026-10-03, Q23/Q17): a parent traversal offset that
/// resolves to a valid footer whose commit is not the declared parent (here
/// commit 0's footer, for parent commit 1) is `RECORD_INVALID`.
#[test]
fn t11_parent_traversal_offset_to_another_footer_is_record_invalid() {
    let f = broken_parent_offset(|h| h[0].footer_offset, 10);
    assert_no_state(&f, ErrorCode::RecordInvalid);
}

/// D10.6 (amended 2026-10-03, Q23/Q17): a parent traversal offset that
/// does not resolve to a valid footer is `FOOTER_INVALID`; footer
/// validation precedes the ID comparison, and the offset is never repaired
/// by searching.
#[test]
fn t11_parent_traversal_offset_to_no_footer_is_footer_invalid() {
    let f = broken_parent_offset(|h| h[1].footer_offset - 3, 12);
    assert_no_state(&f, ErrorCode::FooterInvalid);
}

// ---- T12 rows, integration: parent link (Q22) ------------------------------------

/// Q22, checkpoint as the preceding commit: a link to the checkpoint's
/// snapshot (key 5) is `RECORD_INVALID`; the base snapshot is not an
/// alternative target. Control: its key 6 opens (`a_hand_built_...`).
#[test]
fn q22_link_to_the_base_snapshot_is_record_invalid() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..1]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    let Metadata::Checkpoint { snapshot, .. } = h[0].commit.metadata else {
        unreachable!()
    };
    let mut m = empty_delta(&h[0], txid(20));
    m.parent.as_mut().unwrap().delta_manifest_hash = snapshot.stored_hash;
    let r = f.append_manifest(&m);
    f.append_delta(&h[0], rule_base(&h[0]), r, txid(20));
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::RecordInvalid);
    assert!(e.message.contains("key 6"), "{e}");
}

/// Q22 mid-segment: the snapshot link at an intermediate commit, valid head.
#[test]
fn q22_snapshot_link_mid_segment_is_record_invalid() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..1]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    let Metadata::Checkpoint { snapshot, .. } = h[0].commit.metadata else {
        unreachable!()
    };
    let mut m1 = empty_delta(&h[0], txid(21));
    m1.parent.as_mut().unwrap().delta_manifest_hash = snapshot.stored_hash;
    let r1 = f.append_manifest(&m1);
    let c1 = f.append_delta(&h[0], rule_base(&h[0]), r1, txid(21));
    let r2 = f.append_manifest(&empty_delta(&c1, txid(22)));
    f.append_delta(&c1, rule_base(&h[0]), r2, txid(22));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// Q22: a link to another delta (the one before the preceding commit).
#[test]
fn q22_link_to_another_delta_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let mut m = empty_delta(&h[1], txid(23));
    m.parent.as_mut().unwrap().delta_manifest_hash = h[0].commit.delta_manifest.stored_hash;
    let r = f.append_manifest(&m);
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(23));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// Q22: parent sequence ≠ j − 1 (the encoder refuses this, so it is
/// edited in after encoding; the hash still names commit 1's key 6).
#[test]
fn q22_wrong_parent_sequence_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let r = f.append_manifest_edited(&empty_delta(&h[1], txid(24)), |v| {
        *forge::field(forge::field(v, 3), 0) = Value::Uint(0);
    });
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(24));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// Q22: no parent link at all on a delta after commit 0 (the encoder
/// refuses this, so it is edited in after encoding).
#[test]
fn q22_missing_parent_link_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let r = f.append_manifest_edited(&empty_delta(&h[1], txid(25)), |v| {
        *forge::field(v, 3) = Value::Null;
    });
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(25));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

// ---- T12 rows, integration: duplicate IDs, unknown op / feature -------------------

/// Decision 18 through the open path: a delta reintroducing a version that
/// exists in the base (an identical copy) is `RECORD_INVALID`.
#[test]
fn t12_reintroduced_version_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let fs = f.storage();
    let cp0 = open_at_footer(&fs, h[0].footer_offset, &opts()).unwrap();
    let snap = read_snapshot(&fs, &cp0, &opts()).unwrap();
    let mut m = empty_delta(&h[1], txid(30));
    m.file_versions.push(snap.file_versions[0].clone());
    let r = f.append_manifest(&m);
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(30));
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::RecordInvalid);
    assert!(e.message.contains("introduced"), "{e}");
}

/// A manifest with one PUT of an existing version (reuse by reference), at
/// commit `prev.seq + 1`. Valid as is; the two tests below break one field.
fn put_existing(f: &Forge, h: &[HistoryEntry], n: u8) -> mochi_core::manifest::Manifest {
    let fs = f.storage();
    let cp0 = open_at_footer(&fs, h[0].footer_offset, &opts()).unwrap();
    let snap = read_snapshot(&fs, &cp0, &opts()).unwrap();
    let file = snap
        .file_versions
        .iter()
        .find(|v| v.version.kind == mochi_core::catalog::namespace::EntryKind::File)
        .unwrap();
    let mut m = empty_delta(h.last().unwrap(), txid(n));
    m.ops
        .push(mochi_core::catalog::namespace::NamespaceOp::Put {
            path: path("zz"),
            version: file.version.id,
        });
    m
}

/// Control for the two tests below: reuse by reference opens.
#[test]
fn t12_put_of_an_existing_version_opens() {
    let (mut f, h) = cp0_d1();
    let m = put_existing(&f, &h, 31);
    let r = f.append_manifest(&m);
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(31));
    let fs = f.storage();
    let opened = open_head(&fs, &opts()).unwrap();
    assert!(read_state(&fs, &opened)
        .unwrap()
        .contains_key(b"zz".as_slice()));
}

/// Decision 19: an unknown operation kind under schema 1, mid-segment, is
/// `RECORD_INVALID`; nothing opens.
#[test]
fn t12_unknown_operation_kind_mid_segment_is_record_invalid() {
    let (mut f, h) = cp0_d1();
    let m = put_existing(&f, &h, 32);
    let r2 = f.append_manifest_edited(&m, |v| {
        let Value::Array(ops) = forge::field(v, 7) else {
            panic!()
        };
        let Value::Array(op) = &mut ops[0] else {
            panic!()
        };
        op[0] = Value::Uint(9);
    });
    let c2 = f.append_delta(&h[1], rule_base(&h[1]), r2, txid(32));
    let r3 = f.append_manifest(&empty_delta(&c2, txid(33)));
    f.append_delta(&c2, rule_base(&h[1]), r3, txid(33));
    assert_eq!(open_err(&f).code, ErrorCode::RecordInvalid);
}

/// An unknown required feature in a delta manifest mid-segment is
/// `UNSUPPORTED_FEATURE`; nothing opens.
#[test]
fn t12_unknown_required_feature_mid_segment_is_unsupported() {
    let (mut f, h) = cp0_d1();
    let r2 = f.append_manifest_edited(&empty_delta(&h[1], txid(34)), |v| {
        *forge::field(v, 9) = Value::Array(vec![Value::Uint(1)]);
    });
    let c2 = f.append_delta(&h[1], rule_base(&h[1]), r2, txid(34));
    let r3 = f.append_manifest(&empty_delta(&c2, txid(35)));
    f.append_delta(&c2, rule_base(&h[1]), r3, txid(35));
    assert_eq!(open_err(&f).code, ErrorCode::UnsupportedFeature);
}

// ---- T12 rows, integration: damage and no fallback ------------------------------

/// cp0 d1 d2 d3 cp4 d5 (Every(4)), six commits.
fn two_segments() -> (SimStorage, Vec<HistoryEntry>) {
    let s = write(CheckpointPolicy::Every(4), &history()[..6]);
    let h = commit_history(&s, &opts()).unwrap();
    let forms: Vec<bool> = h.iter().map(is_cp).collect();
    assert_eq!(forms, [true, false, false, false, true, false]);
    (s, h)
}

/// T12 "Damage in the segment": a flipped byte in delta manifest 2 fails
/// every head 2…3 with `STORED_INTEGRITY_FAILED`; heads 0, 1 and the heads
/// after the later checkpoint (4, 5) still open, to their model states.
#[test]
fn t12_damage_in_the_segment_scopes_to_its_heads() {
    let steps = history();
    let (s, h) = two_segments();
    let mut bytes = s.contents();
    let d2 = h[2].commit.delta_manifest;
    flip(&mut bytes, d2.offset + d2.stored_len / 2);
    let damaged = SimStorage::from_bytes(bytes);
    for (i, e) in h.iter().enumerate() {
        match code_at(&damaged, e) {
            Ok(opened) => {
                assert!([0, 1, 4, 5].contains(&i), "head {i} must not open");
                assert_eq!(read_state(&damaged, &opened).unwrap(), steps[i].after);
            }
            Err(code) => {
                assert!([2, 3].contains(&i), "head {i} must open, got {code:?}");
                assert_eq!(code, ErrorCode::StoredIntegrityFailed, "head {i}");
            }
        }
    }
    assert_eq!(open_head(&damaged, &opts()).unwrap().seq(), 5);
}

/// T12 "No fallback": with the head inside the damaged segment, `open_head`
/// returns an error, never an earlier state.
#[test]
fn t12_no_fallback_past_a_damaged_delta() {
    let s = write(CheckpointPolicy::Never, &history()[..4]);
    let h = commit_history(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    let d2 = h[2].commit.delta_manifest;
    flip(&mut bytes, d2.offset + d2.stored_len / 2);
    let damaged = SimStorage::from_bytes(bytes);
    let e = open_head(&damaged, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
}

// ---- T11 "No search": trace assertion --------------------------------------------

/// T11 "No search" (amendment 3): opening head *h* reads only the
/// descriptor; the segment's footers and commit records; delta manifests
/// *b*+1 … *h*; and *b*'s image. Not the base's own delta manifest (its
/// hash is compared, not read), not S(*b*) (D10.9), nothing of any other
/// commit, and nothing else.
#[test]
fn t11_opening_reads_only_the_segment() {
    let (s, h) = two_segments();
    for head in [3usize, 5] {
        let base = if head < 4 { 0 } else { 4 };
        let t = Tracing {
            inner: &s,
            reads: RefCell::new(Vec::new()),
        };
        let opened = open_at_footer(&t, h[head].footer_offset, &opts()).unwrap();
        assert_eq!(opened.segment.base_seq, base as u64);

        let d = h[head].commit.descriptor;
        let mut allowed = vec![(d.offset, d.offset + d.stored_len)];
        for e in &h[base..=head] {
            allowed.push((e.commit_offset, e.footer_offset + FOOTER_FRAME_LEN));
        }
        for e in &h[base + 1..=head] {
            let m = e.commit.delta_manifest;
            allowed.push((m.offset, m.offset + m.stored_len));
        }
        let Metadata::Checkpoint { image, snapshot } = h[base].commit.metadata else {
            unreachable!()
        };
        allowed.push((image.offset, image.offset + image.stored_len));

        let reads = t.reads.into_inner();
        assert!(!reads.is_empty());
        for r in &reads {
            assert!(
                within(*r, &allowed),
                "head {head}: read {r:?} outside the segment's inputs"
            );
        }
        let forbidden = [
            (snapshot.offset, snapshot.offset + snapshot.stored_len),
            (
                h[base].commit.delta_manifest.offset,
                h[base].commit.delta_manifest.offset + h[base].commit.delta_manifest.stored_len,
            ),
        ];
        for r in &reads {
            assert!(
                !forbidden.iter().any(|&(lo, hi)| r.0 < hi && lo < r.0 + r.1),
                "head {head}: read {r:?} touches S(b) or the base's delta manifest"
            );
        }
    }
}

// ---- decision 21: append on a delta head ------------------------------------------

/// T12 "Append on a delta head" (decision 21): reopening on a delta head
/// reconstructs promised attributes from S(*b*) plus the ordered deltas.
/// Checked against the transaction model at the next checkpoint's snapshot,
/// across delta → checkpoint, checkpoint → delta, and delta → delta
/// sessions; every state equals the model.
#[test]
fn append_on_a_delta_head_reconstructs_attributes() {
    let steps = history();
    // cp0 d1 d2 d3 (Never), one session.
    let s = write(CheckpointPolicy::Never, &steps[..4]);
    assert!(!is_cp(commit_history(&s, &opts()).unwrap().last().unwrap()));
    // Reopen on delta 3: commit 4 as a checkpoint (production default).
    append(&s, 1001, CheckpointPolicy::EveryCommit, &steps[4..5]);
    // Reopen on checkpoint 4: commit 5 as a delta.
    append(&s, 1002, CheckpointPolicy::Never, &steps[5..6]);
    // Reopen on delta 5: commit 6 as a checkpoint.
    append(&s, 1003, CheckpointPolicy::EveryCommit, &steps[6..7]);

    let h = commit_history(&s, &opts()).unwrap();
    let forms: Vec<bool> = h.iter().map(is_cp).collect();
    assert_eq!(forms, [true, false, false, false, true, false, true]);
    for (i, e) in h.iter().enumerate() {
        let opened = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
        assert_eq!(
            read_state(&s, &opened).unwrap(),
            steps[i].after,
            "commit {i}"
        );
        if is_cp(e) {
            assert_eq!(
                snapshot_attrs(&s, &opened),
                steps[i].attrs,
                "attributes at {i}"
            );
        }
    }
}

/// Delta → delta across a reopen, then a checkpoint in the same session:
/// the in-session attributes carried from a reconstructed start are right.
#[test]
fn append_delta_to_delta_then_checkpoint() {
    let steps = history();
    let s = write(CheckpointPolicy::Never, &steps[..3]);
    append(&s, 2001, CheckpointPolicy::Every(5), &steps[3..7]);
    let h = commit_history(&s, &opts()).unwrap();
    let forms: Vec<bool> = h.iter().map(is_cp).collect();
    assert_eq!(forms, [true, false, false, false, false, true, false]);
    let cp5 = open_at_footer(&s, h[5].footer_offset, &opts()).unwrap();
    assert_eq!(snapshot_attrs(&s, &cp5), steps[5].attrs);
    for (i, e) in h.iter().enumerate() {
        let opened = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
        assert_eq!(
            read_state(&s, &opened).unwrap(),
            steps[i].after,
            "commit {i}"
        );
    }
}

/// Decision 21: a damaged S(*b*) leaves reads of the delta head working
/// (D10.9) and refuses append with nothing published.
#[test]
fn a_damaged_base_snapshot_blocks_append_on_a_delta_head_only() {
    let steps = history();
    let s = write(CheckpointPolicy::Never, &steps[..3]);
    let h = commit_history(&s, &opts()).unwrap();
    let Metadata::Checkpoint { snapshot, .. } = h[0].commit.metadata else {
        unreachable!()
    };
    let mut bytes = s.contents();
    flip(&mut bytes, snapshot.offset + snapshot.stored_len / 2);
    let damaged = SimStorage::from_bytes(bytes.clone());

    let head = open_head(&damaged, &opts()).unwrap();
    assert_eq!(read_state(&damaged, &head).unwrap(), steps[2].after);

    let e = ArchiveWriter::open_append(
        damaged.clone(),
        Box::new(SeqIds::new(3001)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "{e}");
    assert!(e.message.contains("snapshot"), "{e}");
    assert_eq!(damaged.contents(), bytes, "nothing published");
}

// ---- checklist Q24: incomplete attributes (deliberately malformed input) ---------

/// Q24 on a checkpoint head: reads are unaffected (the snapshot is not read
/// to open, D10.9); append is refused at open, with nothing published.
/// Code provisional (`RECORD_INVALID`; see checklist Q24).
#[test]
fn q24_incomplete_snapshot_refuses_append_on_a_checkpoint_head() {
    let (f, _, missing) = checkpoint_with_incomplete_snapshot();
    let s = f.storage();
    let head = open_head(&s, &opts()).unwrap();
    let state = read_state(&s, &head).unwrap();
    assert_eq!(state, history()[1].after);
    assert!(
        state.contains_key(&missing),
        "the omitted version is still reachable"
    );

    let before = s.contents();
    let e = open_append_err(&s);
    assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");
    assert!(e.message.contains("promised attributes"), "{e}");
    assert_eq!(s.contents(), before, "nothing published");
}

/// Q24 on a delta head whose base has the incomplete snapshot: the same
/// refusal, from the S(b) + deltas reconstruction.
#[test]
fn q24_incomplete_base_snapshot_refuses_append_on_a_delta_head() {
    let (mut f, cp2, _) = checkpoint_with_incomplete_snapshot();
    let m = f.append_manifest(&empty_delta(&cp2, txid(40)));
    f.append_delta(&cp2, link(&cp2), m, txid(40));
    let s = f.storage();
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.segment.base_seq, cp2.commit.seq);
    assert_eq!(read_state(&s, &head).unwrap(), history()[1].after);

    let before = s.contents();
    let e = open_append_err(&s);
    assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");
    assert!(e.message.contains("promised attributes"), "{e}");
    assert_eq!(s.contents(), before, "nothing published");
}

// ---- checklist Q25: query-only enforcement on a replayed reader catalog ----------

/// One probe per public mutator of `Catalog`: `set_meta`, `insert_object`
/// (a new ID), `insert_file_version` (a new directory version), and
/// `append_commit` (an empty commit after the head). Each probe is valid
/// against the catalog's current state, so on writable state all four
/// succeed; that is the control showing a reader's refusals come from the
/// read-only enforcement, not from a malformed probe.
fn mutator_probes(cat: &mut Catalog) -> Vec<Result<(), ErrorCode>> {
    use mochi_core::catalog::namespace::{EntryKind, FileVersionId};
    use mochi_core::catalog::{Commit, FileVersion};
    use mochi_core::object::ObjectId;
    let head = cat.head_commit().unwrap().unwrap();
    let ids = cat.object_ids().unwrap();
    let mut record = cat.object(&ids[0]).unwrap().unwrap();
    record.id = ObjectId::from_bytes([0xEE; 32]);
    vec![
        cat.set_meta("probe", b"x").map_err(|e| e.code),
        cat.insert_object(&record, Some(0)).map_err(|e| e.code),
        cat.insert_file_version(
            &FileVersion {
                id: FileVersionId::from_bytes([0xEF; 32]),
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            &[],
        )
        .map_err(|e| e.code),
        cat.append_commit(&Commit {
            seq: head + 1,
            parent: Some(head),
            ops: Vec::new(),
        })
        .map(|_| ())
        .map_err(|e| e.code),
    ]
}

/// The reader's catalog refuses all four probes with `CATALOG_INVALID` and
/// is unchanged afterwards; a writable catalog with the same logical state
/// accepts all four.
fn assert_reader_refuses(h: &mut OpenedHead, who: &str) {
    let before = h.catalog.logical_dump().unwrap();
    let refused = mutator_probes(&mut h.catalog);
    assert_eq!(
        refused,
        vec![Err(ErrorCode::CatalogInvalid); 4],
        "{who}: every probe refused with CATALOG_INVALID"
    );
    assert_eq!(
        h.catalog.logical_dump().unwrap(),
        before,
        "{who}: unchanged"
    );

    // Control: the same logical state, writable.
    let mut writable = h.catalog.writable_copy_for_tests().unwrap();
    assert_eq!(
        writable.logical_dump().unwrap(),
        before,
        "{who}: control matches the reader"
    );
    assert_eq!(
        mutator_probes(&mut writable),
        vec![Ok(()); 4],
        "{who}: every probe succeeds on matching writable state"
    );
    assert_ne!(
        writable.logical_dump().unwrap(),
        before,
        "{who}: the control did write"
    );
}

/// Q25 (`PRAGMA query_only`, accepted as logical read-only enforcement, not
/// as equivalence with a read-only open): every public mutator is refused
/// with `CATALOG_INVALID` on a replayed reader catalog and on a checkpoint
/// reader's, and each reader is unchanged; the same probes succeed on
/// writable catalogs with matching state. The catalog's one connection is
/// private and no public API exposes it or runs SQL, so the pragma cannot
/// be turned off through the reader API. `publish` is compared for outcome
/// parity only: both readers return the same result (observed: both refuse,
/// because `publish` runs `VACUUM`, a write). This test makes no claim
/// about copies made from a reader.
#[test]
fn q25_replayed_reader_catalog_refuses_every_public_mutator() {
    let s = write(CheckpointPolicy::Never, &history()[..3]);
    let h = commit_history(&s, &opts()).unwrap();
    let mut cp = open_at_footer(&s, h[0].footer_offset, &opts()).unwrap();
    let mut replayed = open_at_footer(&s, h[2].footer_offset, &opts()).unwrap();
    assert!(is_cp(&h[0]) && !is_cp(&h[2]));
    assert_eq!(
        cp.catalog.publish().map(|_| ()).map_err(|e| e.code),
        replayed.catalog.publish().map(|_| ()).map_err(|e| e.code),
        "publish: outcome parity"
    );
    assert_reader_refuses(&mut cp, "checkpoint reader");
    assert_reader_refuses(&mut replayed, "replayed reader");
}

/// Q25 `Every(n)`: sequence origin 0, so commit s is a checkpoint iff
/// s mod n = 0; `Every(1)` is every commit; `Every(0)` is refused
/// (`checkpoint_policy_controls`).
#[test]
fn q25_every_n_counts_from_sequence_zero() {
    let forms = |p| -> Vec<bool> {
        commit_history(&write(p, &history()), &opts())
            .unwrap()
            .iter()
            .map(is_cp)
            .collect()
    };
    assert_eq!(
        forms(CheckpointPolicy::Every(3)),
        [true, false, false, true, false, false, true]
    );
    assert_eq!(forms(CheckpointPolicy::Every(1)), [true; 7]);
    assert_eq!(
        forms(CheckpointPolicy::Every(7)),
        [true, false, false, false, false, false, false]
    );
}

// ---- checklist Q26: reference validity through the open path ---------------------

/// Q26 (decided 2026-10-04): a delta whose PUT names a version that exists
/// neither in the catalog nor in the delta is `NAMESPACE_INVALID` through
/// `open_head`; nothing opens.
#[test]
fn q26_put_of_an_unknown_version_is_namespace_invalid() {
    let (mut f, h) = cp0_d1();
    let mut m = empty_delta(&h[1], txid(50));
    m.ops
        .push(mochi_core::catalog::namespace::NamespaceOp::Put {
            path: path("zz"),
            version: FileVersionId::from_bytes([0x99; 32]),
        });
    let r = f.append_manifest(&m);
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(50));
    let e = open_err(&f);
    assert_eq!(e.code, ErrorCode::NamespaceInvalid, "{e}");
}

// ---- checklist Q27: directory versions (decoder / encoder) -----------------------

/// A delta after `h[1]` introducing one new, valid directory version and
/// putting it at "q27".
fn delta_with_new_directory(h: &[HistoryEntry], n: u8) -> mochi_core::manifest::Manifest {
    use mochi_core::catalog::namespace::{EntryKind, NamespaceOp};
    let mut m = empty_delta(&h[1], txid(n));
    let id = FileVersionId::from_bytes([n; 32]);
    m.file_versions
        .push(mochi_core::manifest::FileVersionEntry {
            version: mochi_core::catalog::FileVersion {
                id,
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            extents: Vec::new(),
            attributes: Attributes::default(),
        });
    m.ops.push(NamespaceOp::Put {
        path: path("q27"),
        version: id,
    });
    m
}

/// Q27 control: the valid directory opens, so the faults below are the only
/// difference.
#[test]
fn q27_control_a_valid_new_directory_opens() {
    let (mut f, h) = cp0_d1();
    let r = f.append_manifest(&delta_with_new_directory(&h, 0x71));
    f.append_delta(&h[1], rule_base(&h[1]), r, txid(0x71));
    let s = f.storage();
    let head = open_head(&s, &opts()).unwrap();
    assert!(read_state(&s, &head)
        .unwrap()
        .contains_key(b"q27".as_slice()));
}

/// Q27 [delegated 2026-10-04]: the encoder refuses a directory version with
/// a length or extents, so a writer cannot emit what the reader rejects.
#[test]
fn q27_encoder_refuses_a_malformed_directory() {
    let (f, h) = cp0_d1();
    let _ = f;
    let mut m = delta_with_new_directory(&h, 0x72);
    m.file_versions[0].version.logical_len = 3;
    let e = m.to_stored().unwrap_err();
    assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");
}

/// Q27 [delegated 2026-10-04]: through the open path, a directory version
/// with a nonzero length, or with an extent, is a schema violation
/// (`RECORD_INVALID`) found by the decoder; nothing opens. The CBOR is
/// edited after encoding (the encoder refuses it).
#[test]
fn q27_malformed_directory_is_record_invalid_through_open() {
    for (what, edit) in [("nonzero length", 0u8), ("an extent", 1u8)] {
        let (mut f, h) = cp0_d1();
        let m = delta_with_new_directory(&h, 0x73 + edit);
        let r = f.append_manifest_edited(&m, |v| {
            let Value::Array(fvs) = forge::field(v, 6) else {
                panic!()
            };
            let fv = &mut fvs[0];
            if edit == 0 {
                *forge::field(fv, 2) = Value::Uint(3);
            } else {
                *forge::field(fv, 4) = Value::Array(vec![Value::Array(vec![
                    Value::Uint(0),
                    Value::Uint(1),
                    Value::Null,
                    Value::Uint(0),
                ])]);
            }
        });
        f.append_delta(&h[1], rule_base(&h[1]), r, txid(0x73 + edit));
        let e = open_err(&f);
        assert_eq!(e.code, ErrorCode::RecordInvalid, "{what}: {e}");
    }
}
