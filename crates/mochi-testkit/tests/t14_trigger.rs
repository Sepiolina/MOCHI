//! T14: the checkpoint trigger (spec Annex B.2.3; gate G3).
//!
//! A commit is a checkpoint when Δ ≥ α·max(*B*, *F*): Δ is the stored bytes
//! of the delta manifests, commit records, and footers since the base, *B*
//! the base's image plus snapshot manifest. Commit 0 always is; a forced
//! checkpoint resets the base. The oracle here recomputes Δ and *B* from the
//! published archive bytes (history entries and object references), never
//! from the writer's own counters.

use mochi_core::commit::Metadata;
use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::phase;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, ArchiveWriter, CheckpointPolicy, CheckpointTrigger,
    HistoryEntry, ReadOptions, TailPolicy, Transaction, WriterOptions, DEFAULT_CHECKPOINT_FLOOR,
};
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{path, read_state, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

/// Cancels the job when `phase` is first reported.
struct CancelAt {
    phase: &'static str,
    cancel: CancellationToken,
}

impl ProgressSink for CancelAt {
    fn report(&self, event: &ProgressEvent) {
        if event.phase == self.phase {
            self.cancel.cancel();
        }
    }
}

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn trigger(num: u64, den: u64, floor: u64) -> CheckpointTrigger {
    CheckpointTrigger::new(num, den, floor).unwrap()
}

fn with_trigger(t: CheckpointTrigger) -> WriterOptions {
    WriterOptions {
        checkpoint_trigger: Some(t),
        ..test_options()
    }
}

/// Commit `i`: one new small file, so every commit has a delta manifest
/// of similar size.
fn small_tx(i: u64) -> Transaction {
    let mut tx = Transaction::new();
    tx.put_file(
        path(&format!("f{i:04}")),
        deterministic_bytes(i, 100),
        attrs(0o644, i as i64),
    );
    tx
}

/// Write commits `range` with `t` in one session (fresh ID seed per
/// session, so IDs never repeat across sessions).
fn session(s: &SimStorage, t: CheckpointTrigger, range: std::ops::Range<u64>, seed: u64) {
    let job = Job::new();
    let ids = Box::new(SeqIds::new(seed));
    let mut w = if s.contents().is_empty() {
        ArchiveWriter::create(s.clone(), ids, with_trigger(t)).unwrap()
    } else {
        ArchiveWriter::open_append(s.clone(), ids, with_trigger(t), TailPolicy::Refuse)
            .unwrap()
            .0
    };
    for i in range {
        w.commit(small_tx(i), &job.ctx()).unwrap();
    }
    w.close().unwrap();
}

/// Δ contribution of one delta commit, from the archive's bytes: its delta
/// manifest, its commit record (commit offset to footer), and its footer.
fn delta_of(e: &HistoryEntry) -> u64 {
    e.commit.delta_manifest.stored_len + (e.footer_offset - e.commit_offset) + FOOTER_FRAME_LEN
}

fn base_of(e: &HistoryEntry) -> u64 {
    match e.commit.metadata {
        Metadata::Checkpoint { image, snapshot } => image.stored_len + snapshot.stored_len,
        Metadata::Delta { .. } => panic!("commit {} is not a checkpoint", e.commit.seq),
    }
}

/// For each commit: (is a checkpoint, what B.2.3 says it must be), with
/// `forced` naming the commits a checkpoint was requested for.
fn oracle(h: &[HistoryEntry], t: CheckpointTrigger, forced: &[u64]) -> Vec<(bool, bool)> {
    let (num, den) = t.alpha();
    let mut delta = 0u64;
    let mut base = 0u64;
    let mut out = Vec::new();
    for e in h {
        let is_cp = e.commit.metadata.is_checkpoint();
        let seq = e.commit.seq;
        let threshold = u128::from(base.max(t.floor())) * u128::from(num);
        let want =
            seq == 0 || forced.contains(&seq) || u128::from(delta) * u128::from(den) >= threshold;
        out.push((is_cp, want));
        if is_cp {
            delta = 0;
            base = base_of(e);
        } else {
            delta += delta_of(e);
        }
    }
    out
}

fn forms(s: &SimStorage) -> Vec<bool> {
    commit_history(s, &opts())
        .unwrap()
        .iter()
        .map(|e| e.commit.metadata.is_checkpoint())
        .collect()
}

// ---- the rule ---------------------------------------------------------------------------

/// The comparison is exact at the threshold, for whole and fractional α,
/// with *B* or *F* the larger, and never overflows.
#[test]
fn t14_rule_is_exact_at_the_threshold() {
    let t = trigger(1, 1, 100);
    assert!(!t.requires_checkpoint(99, 0));
    assert!(t.requires_checkpoint(100, 0));
    assert!(!t.requires_checkpoint(149, 150), "B above F decides");
    assert!(t.requires_checkpoint(150, 150));
    // α = 3/2, F = 10: threshold 15.
    let t = trigger(3, 2, 10);
    assert!(!t.requires_checkpoint(14, 0));
    assert!(t.requires_checkpoint(15, 0));
    // α = 1/3, F = 10: threshold 10/3, so Δ = 4 is the first that reaches it.
    let t = trigger(1, 3, 10);
    assert!(!t.requires_checkpoint(3, 0));
    assert!(t.requires_checkpoint(4, 0));
    // Extremes: no overflow, no panic.
    let t = trigger(u64::MAX, 1, u64::MAX);
    assert!(!t.requires_checkpoint(u64::MAX, u64::MAX));
    let t = trigger(1, u64::MAX, u64::MAX);
    assert!(t.requires_checkpoint(1, u64::MAX));
    assert!(!t.requires_checkpoint(0, u64::MAX));
    // F = 0 is allowed: the base alone decides.
    let t = trigger(1, 1, 0);
    assert!(t.requires_checkpoint(10, 10));
    assert!(!t.requires_checkpoint(9, 10));
}

#[test]
fn t14_alpha_must_be_positive_and_the_defaults_are_provisional_values() {
    for (n, d) in [(0, 1), (1, 0), (0, 0)] {
        assert_eq!(
            CheckpointTrigger::new(n, d, 1).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
    let t = CheckpointTrigger::default();
    assert_eq!(t.alpha(), (1, 1));
    assert_eq!(t.floor(), 1 << 20);
    assert_eq!(DEFAULT_CHECKPOINT_FLOOR, 1 << 20);
    let s = SimStorage::new();
    let w = ArchiveWriter::create(s, Box::new(SeqIds::new(1)), test_options()).unwrap();
    assert_eq!(w.checkpoint_policy(), CheckpointPolicy::Trigger(t));
}

// ---- the writer ------------------------------------------------------------------------

/// Over 40 commits with a trigger that fires every few commits, every
/// commit's form is what the rule says, computed from the archive bytes.
/// Every commit opens, and the head holds every file.
#[test]
fn t14_every_commit_follows_the_rule() {
    let t = trigger(1, 8, 0);
    let s = SimStorage::new();
    session(&s, t, 0..40, 1);
    let h = commit_history(&s, &opts()).unwrap();
    let got = oracle(&h, t, &[]);
    for (i, (is_cp, want)) in got.iter().enumerate() {
        assert_eq!(is_cp, want, "commit {i}");
    }
    let cps = got.iter().filter(|(c, _)| *c).count();
    assert!(
        (3..=20).contains(&cps),
        "{cps} checkpoints: the test needs both forms"
    );
    for e in &h {
        open_at_footer(&s, e.footer_offset, &opts()).unwrap();
    }
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_state(&s, &head).unwrap().len(), 40);
}

/// **Checklist DoD (threshold).** With α·*F* set to exactly the Δ of
/// commits 1 … 5, commit 6 is a checkpoint; one byte more and it is a
/// delta. α = 1/1000 makes α·*F* exact while *F* stays above *B*.
#[test]
fn t14_trigger_fires_exactly_at_the_threshold() {
    // Measure Δ(1 … 5) with a trigger that never fires.
    let probe = SimStorage::new();
    session(&probe, trigger(1, 1, u64::MAX), 0..7, 1);
    let h = commit_history(&probe, &opts()).unwrap();
    assert_eq!(
        forms(&probe),
        [true, false, false, false, false, false, false]
    );
    let d5: u64 = h[1..=5].iter().map(delta_of).sum();
    assert!(
        1000 * d5 > base_of(&h[0]),
        "F must exceed B for α·F to decide"
    );

    let at = SimStorage::new();
    session(&at, trigger(1, 1000, 1000 * d5), 0..7, 1);
    assert_eq!(forms(&at), [true, false, false, false, false, false, true]);

    let over = SimStorage::new();
    session(&over, trigger(1, 1000, 1000 * (d5 + 1)), 0..7, 1);
    assert_eq!(
        forms(&over),
        [true, false, false, false, false, false, false]
    );
    // The archives are identical up to commit 6: only the decision differs.
    let h_at = commit_history(&at, &opts()).unwrap();
    assert_eq!(
        at.contents()[..h_at[5].footer_offset as usize],
        over.contents()[..h_at[5].footer_offset as usize]
    );
}

/// **Checklist DoD (generated bytes).** Data objects and checkpoint output
/// do not count toward Δ: commits of 2 MiB files under the default 1 MiB
/// floor stay deltas, though each adds more than *F* to the file; and after
/// a checkpoint Δ is 0 again whatever its image and snapshot weigh. Chunks
/// are 1 MiB here: with the 64-byte test chunks a 2 MiB file has 32,768
/// chunk entries, and that delta manifest is metadata that rightly counts.
#[test]
fn t14_generated_bytes_do_not_count() {
    let s = SimStorage::new();
    let job = Job::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        WriterOptions {
            chunk_size: Some(1 << 20),
            ..with_trigger(CheckpointTrigger::default())
        },
    )
    .unwrap();
    for i in 0..3u64 {
        let mut tx = Transaction::new();
        tx.put_file(
            path(&format!("big{i}")),
            deterministic_bytes(100 + i, 2 << 20),
            attrs(0o644, 1),
        );
        let before = s.contents().len() as u64;
        w.commit(tx, &job.ctx()).unwrap();
        assert!(s.contents().len() as u64 - before > DEFAULT_CHECKPOINT_FLOOR);
    }
    let (delta, base) = w.trigger_accounting().unwrap();
    let h = commit_history(&s, &opts()).unwrap();
    assert_eq!(forms(&s), [true, false, false]);
    assert_eq!(delta, h[1..].iter().map(delta_of).sum::<u64>());
    assert!(
        delta < DEFAULT_CHECKPOINT_FLOOR,
        "Δ {delta} counts data bytes"
    );
    assert_eq!(base, base_of(&h[0]));

    // A forced checkpoint resets Δ; its own image and snapshot are B, not Δ.
    w.request_checkpoint();
    w.commit(small_tx(9), &job.ctx()).unwrap();
    let h = commit_history(&s, &opts()).unwrap();
    assert!(h[3].commit.metadata.is_checkpoint());
    assert_eq!(w.trigger_accounting(), Some((0, base_of(&h[3]))));
}

/// Commit 0 is a checkpoint even when nothing could trigger one.
#[test]
fn t14_commit_zero_is_always_a_checkpoint() {
    let s = SimStorage::new();
    session(&s, trigger(u64::MAX, 1, u64::MAX), 0..3, 1);
    assert_eq!(forms(&s), [true, false, false]);
}

/// **Checklist DoD (forced).** A requested checkpoint is written next and
/// resets the base: later deltas name it. A request survives a commit that
/// fails (here, cancelled) and is used by the next published one.
#[test]
fn t14_a_forced_checkpoint_resets_the_base() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        with_trigger(trigger(1, 1, u64::MAX)),
    )
    .unwrap();
    let job = Job::new();
    for i in 0..3 {
        w.commit(small_tx(i), &job.ctx()).unwrap();
    }
    w.request_checkpoint();
    assert!(w.checkpoint_requested());
    // Cancel once the writer is writing the requested checkpoint, i.e.
    // after it has acted on the request: the commit fails and rolls back.
    let cancel = CancellationToken::new();
    let sink = CancelAt {
        phase: phase::CHECKPOINT,
        cancel: cancel.clone(),
    };
    let ctx = JobContext {
        progress: &sink,
        cancel: &cancel,
    };
    let len = s.contents().len();
    let e = w.commit(small_tx(3), &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(cancel.is_cancelled(), "the checkpoint phase was reached");
    assert_eq!(s.contents().len(), len, "rolled back");
    assert!(
        w.checkpoint_requested(),
        "a failed commit does not use up the request"
    );
    w.commit(small_tx(3), &job.ctx()).unwrap();
    assert!(!w.checkpoint_requested());
    for i in 4..6 {
        w.commit(small_tx(i), &job.ctx()).unwrap();
    }
    w.close().unwrap();

    let h = commit_history(&s, &opts()).unwrap();
    assert_eq!(forms(&s), [true, false, false, true, false, false]);
    for e in &h[4..] {
        let Metadata::Delta { base } = e.commit.metadata else {
            panic!("delta expected")
        };
        assert_eq!((base.seq, base.commit_id), (3, h[3].commit_id));
    }
    for (is_cp, want) in oracle(&h, trigger(1, 1, u64::MAX), &[3]) {
        assert_eq!(is_cp, want);
    }
}

/// Reopening rebuilds Δ and *B* from the segment, so a history written in
/// several sessions follows the rule as if written in one, whether a
/// session ends on a delta or on a checkpoint.
#[test]
fn t14_accounting_survives_reopen() {
    let t = trigger(1, 8, 0);
    let one = SimStorage::new();
    session(&one, t, 0..30, 1);
    let one_forms = forms(&one);
    // Split points on a delta and on a checkpoint head.
    let first_delta = one_forms.iter().skip(1).position(|c| !c).unwrap() + 1;
    let later_cp = one_forms.iter().skip(1).position(|c| *c).unwrap() + 1;
    for split in [first_delta as u64 + 1, later_cp as u64 + 1, 17] {
        let s = SimStorage::new();
        session(&s, t, 0..split, 1);
        let before = {
            let w = ArchiveWriter::open_append(
                s.clone(),
                Box::new(SeqIds::new(2)),
                with_trigger(t),
                TailPolicy::Refuse,
            )
            .unwrap()
            .0;
            w.trigger_accounting().unwrap()
        };
        let h = commit_history(&s, &opts()).unwrap();
        let last_cp = h
            .iter()
            .rposition(|e| e.commit.metadata.is_checkpoint())
            .unwrap();
        let want_delta: u64 = h[last_cp + 1..].iter().map(delta_of).sum();
        assert_eq!(before, (want_delta, base_of(&h[last_cp])), "split {split}");
        session(&s, t, split..30, 1000 + split);
        let h = commit_history(&s, &opts()).unwrap();
        for (i, (is_cp, want)) in oracle(&h, t, &[]).into_iter().enumerate() {
            assert_eq!(is_cp, want, "split {split}, commit {i}");
        }
        assert_eq!(forms(&s), one_forms, "split {split}");
    }
}

/// The point of T14: under the default trigger, small commits stay deltas,
/// so metadata no longer grows with a full checkpoint per commit. Compared
/// with every-commit checkpoints on the same 30 commits.
#[test]
fn t14_default_trigger_avoids_a_checkpoint_per_commit() {
    let s = SimStorage::new();
    session(&s, CheckpointTrigger::default(), 0..30, 1);
    assert_eq!(forms(&s).iter().filter(|c| **c).count(), 1);

    let every = SimStorage::new();
    let job = Job::new();
    let mut w =
        ArchiveWriter::create(every.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    for i in 0..30 {
        w.commit(small_tx(i), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    let (a, b) = (s.contents().len(), every.contents().len());
    assert!(a * 5 < b, "trigger {a} bytes, every commit {b} bytes");
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_state(&s, &head).unwrap().len(), 30);
}
