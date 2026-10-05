//! T15: adoption (spec Annex B.2 D10.7, §18.1; gate G2).
//!
//! Before publishing a checkpoint commit the writer re-reads both
//! representations from their serialized, hashed bytes and checks that each
//! one's authoritative state equals the source. A mismatch fails the commit:
//! no new head, the previous head stays valid, and the writer is not
//! poisoned. Plan: `docs/t16-t17-t15-plan.md` section 5.

use mochi_core::commit::Metadata;
use mochi_core::damage::{assess_damage, Effect, ObjectRole};
use mochi_core::publish::{
    check_checkpoint_representations, commit_history, open_at_footer, open_head, ArchiveWriter,
    AuditEvent, CheckpointPolicy, CheckpointTamper, ReadOptions, TailPolicy,
};
use mochi_core::status::Status;
use mochi_core::ErrorCode;
use mochi_testkit::archive::{read_state, test_options, Job, State};
use mochi_testkit::replay::{checkpoint_with_incomplete_snapshot, history, Step};
use mochi_testkit::{Fault, Op, SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn writer(s: &SimStorage) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), test_options()).unwrap()
}

fn commit(w: &mut ArchiveWriter<SimStorage>, st: &Step) {
    let job = Job::new();
    w.commit(st.tx.clone(), &job.ctx()).unwrap();
}

/// Commit `st` with `tamper` set and expect `code`; then show that nothing
/// changed and that the writer still works: the file is byte-identical, the
/// head is unchanged and still opens to `prev`, the roll-back is audited, and
/// the *same* transaction commits once the tamper is off.
fn assert_blocked(
    w: &mut ArchiveWriter<SimStorage>,
    s: &SimStorage,
    st: &Step,
    tamper: CheckpointTamper,
    code: ErrorCode,
    prev: Option<&State>,
) -> mochi_core::MochiError {
    let before = s.contents();
    let head_before = w.head_seq();
    w.set_checkpoint_tamper(Some(tamper));
    let job = Job::new();
    let e = w.commit(st.tx.clone(), &job.ctx()).unwrap_err();
    assert_eq!(e.code, code, "{tamper:?}: {e}");
    assert_eq!(s.contents(), before, "{tamper:?}: nothing was published");
    assert_eq!(w.head_seq(), head_before, "{tamper:?}: no new head");
    assert!(
        matches!(w.audit_log().last(), Some(AuditEvent::RolledBack { .. })),
        "{tamper:?}: the roll-back is audited"
    );
    if let Some(prev) = prev {
        let head = open_head(s, &opts()).unwrap();
        assert_eq!(Some(head.seq()), head_before);
        assert_eq!(
            &read_state(s, &head).unwrap(),
            prev,
            "{tamper:?}: previous head"
        );
    }
    // Not poisoned: the same transaction commits without the tamper.
    w.set_checkpoint_tamper(None);
    let out = w.commit(st.tx.clone(), &job.ctx()).unwrap();
    let head = open_head(s, &opts()).unwrap();
    assert_eq!(head.seq(), out.seq);
    assert_eq!(read_state(s, &head).unwrap(), st.after, "{tamper:?}: retry");
    e
}

/// **DoD.** A planted divergence in the snapshot manifest's attributes blocks
/// the head.
#[test]
fn t15_snapshot_attribute_divergence_blocks_the_head() {
    let steps = history();
    let s = SimStorage::new();
    let mut w = writer(&s);
    commit(&mut w, &steps[0]);
    commit(&mut w, &steps[1]);
    let e = assert_blocked(
        &mut w,
        &s,
        &steps[2],
        CheckpointTamper::SnapshotAttributes,
        ErrorCode::CheckpointMismatch,
        Some(&steps[1].after),
    );
    assert!(e.message.contains("snapshot manifest"), "{e}");
    assert!(e.message.contains("attributes"), "{e}");
}

#[test]
fn t15_snapshot_missing_entry_blocks_the_head() {
    let steps = history();
    let s = SimStorage::new();
    let mut w = writer(&s);
    commit(&mut w, &steps[0]);
    commit(&mut w, &steps[1]);
    let e = assert_blocked(
        &mut w,
        &s,
        &steps[2],
        CheckpointTamper::SnapshotOmitsEntry,
        ErrorCode::CheckpointMismatch,
        Some(&steps[1].after),
    );
    assert!(e.message.contains("snapshot manifest"), "{e}");
}

#[test]
fn t15_image_divergence_blocks_the_head() {
    let steps = history();
    let s = SimStorage::new();
    let mut w = writer(&s);
    commit(&mut w, &steps[0]);
    commit(&mut w, &steps[1]);
    let e = assert_blocked(
        &mut w,
        &s,
        &steps[2],
        CheckpointTamper::ImageOmitsLastOp,
        ErrorCode::CheckpointMismatch,
        Some(&steps[1].after),
    );
    assert!(e.message.contains("catalog image"), "{e}");
}

/// A first commit that fails adoption leaves an empty file, which is not an
/// archive (compare `a_cancelled_first_commit_leaves_no_descriptor_behind`).
#[test]
fn t15_first_commit_divergence_leaves_an_empty_file() {
    let steps = history();
    for tamper in [
        CheckpointTamper::SnapshotAttributes,
        CheckpointTamper::SnapshotOmitsEntry,
        CheckpointTamper::ImageOmitsLastOp,
    ] {
        let s = SimStorage::new();
        let mut w = writer(&s);
        assert_blocked(
            &mut w,
            &s,
            &steps[0],
            tamper,
            ErrorCode::CheckpointMismatch,
            None,
        );
    }
    // And the empty state is really empty after the block, before the retry.
    let s = SimStorage::new();
    let mut w = writer(&s);
    w.set_checkpoint_tamper(Some(CheckpointTamper::SnapshotAttributes));
    let job = Job::new();
    w.commit(steps[0].tx.clone(), &job.ctx()).unwrap_err();
    assert!(s.contents().is_empty());
    assert_eq!(w.head_seq(), None);
}

/// Delta commits write no checkpoint, so nothing is compared.
#[test]
fn t15_delta_commits_are_not_adopted() {
    let steps = history();
    let s = SimStorage::new();
    let mut w = writer(&s);
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    commit(&mut w, &steps[0]);
    for tamper in [
        CheckpointTamper::SnapshotAttributes,
        CheckpointTamper::SnapshotOmitsEntry,
        CheckpointTamper::ImageOmitsLastOp,
    ] {
        w.set_checkpoint_tamper(Some(tamper));
        let next = w.head_seq().unwrap() as usize + 1;
        commit(&mut w, &steps[next]);
    }
    w.set_checkpoint_tamper(None);
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.seq(), 3);
    assert_eq!(read_state(&s, &head).unwrap(), steps[3].after);
}

/// Under `Every(n)` exactly the checkpoint commits are adopted.
#[test]
fn t15_checkpoints_under_every_policy_are_adopted() {
    let steps = history();
    for n in [2u64, 3] {
        let s = SimStorage::new();
        let mut w = writer(&s);
        w.set_checkpoint_policy(CheckpointPolicy::Every(n)).unwrap();
        for (i, st) in steps.iter().enumerate() {
            let cp = i == 0 || (i as u64) % n == 0;
            let prev = i.checked_sub(1).map(|p| &steps[p].after);
            if cp {
                assert_blocked(
                    &mut w,
                    &s,
                    st,
                    CheckpointTamper::SnapshotAttributes,
                    ErrorCode::CheckpointMismatch,
                    prev,
                );
            } else {
                w.set_checkpoint_tamper(Some(CheckpointTamper::SnapshotAttributes));
                commit(&mut w, st);
                w.set_checkpoint_tamper(None);
            }
        }
        for (e, st) in commit_history(&s, &opts()).unwrap().iter().zip(&steps) {
            let o = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
            assert_eq!(read_state(&s, &o).unwrap(), st.after, "Every({n})");
        }
    }
}

/// Silent write corruption: the storage acknowledges bytes it stored wrong.
/// Only a re-read of what was written finds it.
#[test]
fn t15_silent_write_corruption_is_caught_on_reread() {
    let steps = history();
    // A clean run, to learn which appends are the snapshot and the image of
    // commit 2 (IDs are deterministic, so the same appends recur).
    let dry = SimStorage::new();
    let mut w = writer(&dry);
    for st in &steps[..3] {
        commit(&mut w, st);
    }
    let hist = commit_history(&dry, &opts()).unwrap();
    let Metadata::Checkpoint { image, snapshot } = hist[2].commit.metadata else {
        panic!("this writer emits checkpoints")
    };
    let appends: Vec<u64> = dry
        .trace()
        .into_iter()
        .filter_map(|op| match op {
            Op::Append { offset, .. } => Some(offset),
            _ => None,
        })
        .collect();
    let index_of = |offset: u64| appends.iter().position(|&o| o == offset).unwrap();

    for (what, r) in [("snapshot manifest", snapshot), ("catalog image", image)] {
        let s = SimStorage::with_faults([Fault::CorruptAppend {
            append_index: index_of(r.offset),
            at: (r.stored_len / 2) as usize,
            xor: 0x01,
        }]);
        let mut w = writer(&s);
        commit(&mut w, &steps[0]);
        commit(&mut w, &steps[1]);
        let before = s.contents();
        let job = Job::new();
        let e = w.commit(steps[2].tx.clone(), &job.ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "{what}: {e}");
        assert!(e.message.contains(what), "{what}: {e}");
        assert_eq!(s.contents(), before, "{what}: rolled back");
        let head = open_head(&s, &opts()).unwrap();
        assert_eq!(head.seq(), 1);
        assert_eq!(read_state(&s, &head).unwrap(), steps[1].after);
        // The fault was one append; the writer is not poisoned.
        commit(&mut w, &steps[2]);
        assert_eq!(open_head(&s, &opts()).unwrap().seq(), 2);
    }
}

/// The re-read uses the writer default limits, not the user's read limits.
#[test]
fn t15_user_read_limits_do_not_block_adoption() {
    let steps = history();
    let mut o = test_options();
    // Below the catalog image's frame, above everything the writer reads
    // through these limits for data objects.
    o.read.limits.max_frame_len = 30_000;
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), o).unwrap();
    for st in &steps[..3] {
        commit(&mut w, st);
    }
    let hist = commit_history(&s, &ReadOptions::default()).unwrap();
    let Metadata::Checkpoint { image, .. } = hist[2].commit.metadata else {
        panic!("checkpoint")
    };
    assert!(
        image.stored_len > 30_000,
        "the limit really is below the image ({} bytes)",
        image.stored_len
    );
    assert_eq!(hist.len(), 3);
}

/// What `verify` repeats (D10.7): a *published* checkpoint whose two
/// representations disagree. The forge is the Q24 one (a structurally valid
/// snapshot that omits a reachable version), which the real writer never
/// produces.
#[test]
fn t15_verify_reports_a_published_mismatch() {
    let (f, cp2, _) = checkpoint_with_incomplete_snapshot();
    let s = f.storage();

    let e = check_checkpoint_representations(&s, &cp2, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::CheckpointMismatch, "{e}");
    assert!(e.message.contains("d/a"), "names the divergence: {e}");

    let job = Job::new();
    let r = assess_damage(&s, &opts(), &job.ctx()).unwrap();
    let pairs: Vec<_> = r
        .objects
        .iter()
        .filter(|o| o.role == ObjectRole::CheckpointPair)
        .collect();
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[0].seq, 2);
    assert_eq!(pairs[0].error.code, ErrorCode::CheckpointMismatch);
    let range = r
        .ranges
        .iter()
        .find(|x| r.objects[x.cause].role == ObjectRole::CheckpointPair)
        .unwrap();
    assert_eq!(
        (range.first, range.last, range.effect),
        (2, 2, Effect::Degraded)
    );
    assert_eq!(r.commits[2].recoverability, Status::Degraded);
    assert_eq!(r.commits[0].recoverability, Status::Pass);
    assert_eq!(r.integrity(), Status::Fail);

    // Reads are unaffected, and Q24's append refusal is unchanged.
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.seq(), 2);
    let e = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(9000)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::RecordInvalid);
}

/// An undamaged archive passes the pair check at every checkpoint.
#[test]
fn t15_undamaged_checkpoints_pass_the_pair_check() {
    let s = SimStorage::new();
    let mut w = writer(&s);
    for st in &history() {
        commit(&mut w, st);
    }
    for e in commit_history(&s, &opts()).unwrap() {
        check_checkpoint_representations(&s, &e, &opts()).unwrap();
    }
    let job = Job::new();
    assert!(assess_damage(&s, &opts(), &job.ctx())
        .unwrap()
        .objects
        .is_empty());
}
