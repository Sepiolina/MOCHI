//! C8: recovery and repair (spec §22, §22.1, §22.2; `mochi_core::repair`).
//!
//! The oracle is a pristine copy of the source, read with the test kit's
//! own reassembly (`archive::read_state`): each repaired commit must hold
//! exactly the pristine snapshot's entries minus the ones the plan lists
//! as omitted, with identical content, version IDs, and promised
//! attributes. The damage is placed with the fixture's known layout; the
//! expected outcome of each case is written from the module's rules, not
//! from the planner's output.

use std::collections::{BTreeMap, BTreeSet};

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::manifest::Mtime;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, promised_attributes, segment_state, ArchiveWriter,
    CheckpointPolicy, HistoryEntry, OpenedHead, ReadOptions, TailPolicy, Transaction,
};
use mochi_core::repair::{
    apply, plan, Outcome, RepairOptions, RepairPlan, RetentionPlan, StepStatus,
};
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{path, read_state, test_options, Job, State};
use mochi_testkit::replay::{attrs, checkpoint_refs, damage, flip};
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const OUT: &str = "repaired.mochi";

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn put(tx: &mut Transaction, p: &str, seed: u64, mode: u32) {
    tx.put_file(
        path(p),
        deterministic_bytes(seed, 150),
        attrs(mode, seed as i64),
    );
}

/// Nine commits, checkpoints at 0 and 3 only (segments 0–2 and 3–8):
/// a directory, a replacement, a rename, a deletion, retention (1–4
/// expired, 2 held as "audit"), and commit times.
fn source() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut n = 0i64;
    let mut c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        n += 1;
        tx.at(Mtime {
            secs: 1_700_000_000 + n,
            nanos: 3,
        });
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1, 0o644);
        put(t, "b", 2, 0o600);
    }); // 0: checkpoint
    c(&mut w, &|t| put(t, "d/a", 3, 0o640)); // 1
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    w.request_checkpoint();
    c(&mut w, &|t| put(t, "c", 4, 0o644)); // 3: checkpoint
    c(&mut w, &|t| {
        t.delete(path("c"));
    }); // 4
    c(&mut w, &|t| put(t, "e", 5, 0o644)); // 5
    c(&mut w, &|t| {
        t.expire(1).expire(2).expire(3).expire(4).hold(b"audit", 2);
    }); // 6
    c(&mut w, &|t| put(t, "d/a", 6, 0o644)); // 7
    c(&mut w, &|t| put(t, "f", 7, 0o644)); // 8
    w.close().unwrap();
    s
}

fn history(s: &SimStorage) -> Vec<HistoryEntry> {
    commit_history(s, &opts()).unwrap()
}

fn opened_all(s: &SimStorage) -> Vec<OpenedHead> {
    history(s)
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, &opts()).unwrap())
        .collect()
}

fn damaged(s: &SimStorage, f: impl FnOnce(&mut Vec<u8>)) -> SimStorage {
    let mut bytes = s.contents();
    f(&mut bytes);
    SimStorage::from_bytes(bytes)
}

/// Flip a byte in the first chunk of the file at `p` in commit `seq`.
fn damage_chunk(s: &SimStorage, seq: u64, p: &str, bytes: &mut [u8]) {
    let h = &opened_all(s)[seq as usize];
    let snap = h.catalog.replay(None).unwrap();
    let entry = snap.get(&path(p)).unwrap();
    let (_, extents) = h.catalog.file_version(&entry.version).unwrap().unwrap();
    let chunk = extents
        .iter()
        .find_map(|e| match e.source {
            ExtentSource::Chunk { chunk, .. } => Some(chunk),
            ExtentSource::Hole => None,
        })
        .unwrap();
    let r = h.catalog.object(&chunk).unwrap().unwrap();
    let at = h.catalog.object_location(&chunk).unwrap().unwrap();
    flip(bytes, at + r.stored_len / 2);
}

fn plan_of(s: &SimStorage) -> RepairPlan {
    plan(s, &opts(), &Job::new().ctx()).unwrap()
}

fn run(
    s: &SimStorage,
    p: &RepairPlan,
    o: &RepairOptions,
) -> (SimDir, mochi_core::Result<mochi_core::repair::RepairReport>) {
    let mut dir = SimDir::new();
    let r = apply(
        s,
        p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        o,
        &Job::new().ctx(),
    );
    (dir, r)
}

fn without(mut st: State, omitted: &BTreeSet<Vec<u8>>) -> State {
    st.retain(|k, _| !omitted.contains(k));
    st
}

fn ladder(p: &RepairPlan) -> Vec<StepStatus> {
    p.ladder.iter().map(|s| s.status).collect()
}

/// The repaired archive holds `kept` source snapshots, in order: each the
/// pristine snapshot minus `omitted` (by sequence), with the same version
/// IDs, promised attributes, and (when the commit record was trusted) time.
fn assert_repairs(
    pristine: &SimStorage,
    out: &SimStorage,
    kept: &[u64],
    omitted: &BTreeMap<u64, BTreeSet<Vec<u8>>>,
) {
    let source = opened_all(pristine);
    let new = opened_all(out);
    assert_eq!(new.len(), kept.len(), "one commit per recovered snapshot");
    let none = BTreeSet::new();
    for (i, r) in kept.iter().enumerate() {
        let (a, b) = (&source[*r as usize], &new[i]);
        let gone = omitted.get(r).unwrap_or(&none);
        assert_eq!(
            read_state(out, b).unwrap(),
            without(read_state(pristine, a).unwrap(), gone),
            "new {i} vs source {r}"
        );
        let src_snap = a.catalog.replay(None).unwrap();
        for (p, e) in b.catalog.replay(None).unwrap().iter() {
            assert_eq!(
                src_snap.get(p).unwrap().version,
                e.version,
                "version ID kept"
            );
        }
        let sa = promised_attributes(pristine, a, &opts()).unwrap();
        for (v, attr) in promised_attributes(out, b, &opts()).unwrap() {
            assert_eq!(sa.get(&v), Some(&attr), "attributes kept");
        }
        assert_eq!(b.commit.time, a.commit.time);
    }
}

/// **Clean archive.** Every snapshot recovered from the head's image; the
/// ladder stops at step 1 and names steps 5–7 as not attempted, never as
/// passed; apply reproduces every snapshot, carries retention, verifies the
/// result, and leaves the source as it was. The plan survives JSON.
#[test]
fn c8_clean_archive_plans_complete_and_reproduces_every_snapshot() {
    let src = source();
    let before = src.contents();
    let p = plan_of(&src);
    assert_eq!(p.outcome, Outcome::Complete);
    assert_eq!(p.exit_code, 0);
    assert!(!p.damage_found);
    assert_eq!(
        ladder(&p),
        [
            StepStatus::Used,
            StepStatus::NotNeeded,
            StepStatus::Used,
            StepStatus::NotNeeded,
            StepStatus::NotAttempted,
            StepStatus::NotAttempted,
            StepStatus::NotAttempted,
        ]
    );
    assert_eq!(p.snapshots.len(), 9);
    assert!(p.snapshots.iter().all(|s| s.opened_at == 8));
    assert!(p.lost_snapshots.is_empty() && p.omitted.is_empty());
    assert_eq!(p.writer_parameters.unwrap().chunk_size, 64);

    // Deterministic, and survives JSON.
    assert_eq!(plan_of(&src), p);
    let json = serde_json::to_string(&p).unwrap();
    let back: RepairPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);

    let (dir, r) = run(&src, &back, &RepairOptions::default());
    let r = r.unwrap();
    assert_eq!(src.contents(), before, "the source is never written");
    assert_eq!(r.outcome, Outcome::Complete);
    assert_eq!(r.exit_code, 0);
    assert!(r.source_kept);
    assert_eq!(r.reverification.exit_code, 0);
    assert_eq!(dir.names(), [OUT]);
    let out = dir.file(OUT).unwrap();
    let all: Vec<u64> = (0..=8).collect();
    assert_repairs(&src, &out, &all, &BTreeMap::new());
    assert_ne!(Some(r.new_archive_id), r.source_archive_id);
    let ret = |s: &SimStorage| {
        let h = open_head(s, &opts()).unwrap();
        segment_state(s, &h, &opts()).unwrap().retention
    };
    assert_eq!(ret(&out), ret(&src), "identity mapping when all are kept");
}

/// **DoD fault row: corrupted latest footer.** The head is the latest valid
/// footer found by scanning (step 2); the last commit is past it, in an
/// unexamined tail, so the repair is **partial** (exit 2) though every
/// snapshot up to 7 is recovered whole.
#[test]
fn c8_corrupted_latest_footer_uses_the_previous_footer_and_is_partial() {
    let pristine = source();
    let src = damaged(&pristine, |b| {
        let at = b.len() as u64 - FOOTER_FRAME_LEN / 2;
        flip(b, at);
    });
    let p = plan_of(&src);
    let head = p.head.clone().unwrap();
    assert_eq!(head.seq, 7);
    assert_eq!(head.found_by, "scan");
    assert_eq!(
        ladder(&p)[..2],
        [StepStatus::NotAvailable, StepStatus::Used]
    );
    assert_ne!(p.tail.as_ref().unwrap().state, "clean");
    assert!(p.damage_found);
    assert_eq!(p.outcome, Outcome::Partial);
    assert_eq!(p.exit_code, 2);
    assert_eq!(p.snapshots.len(), 8);

    let (dir, r) = run(&src, &p, &RepairOptions::default());
    let r = r.unwrap();
    assert_eq!(r.outcome, Outcome::Partial);
    assert_eq!(r.exit_code, 2);
    let kept: Vec<u64> = (0..=7).collect();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &kept, &BTreeMap::new());
}

/// **Unrecoverable content.** A damaged chunk of file `e` (introduced at
/// 5) and one of `b` (introduced at 0, renamed to `d/b` at 2, the same
/// version): each version is left out of every snapshot that holds it,
/// listed with its path, version, snapshots, and the stored-integrity
/// code; everything else is reproduced exactly. Partial, exit 2.
#[test]
fn c8_damaged_chunks_leave_out_exactly_their_versions() {
    let pristine = source();
    let src = damaged(&pristine, |b| {
        damage_chunk(&pristine, 5, "e", b);
        damage_chunk(&pristine, 0, "b", b);
    });
    let p = plan_of(&src);
    assert_eq!(p.outcome, Outcome::Partial);
    assert!(p.lost_snapshots.is_empty());
    let listed: Vec<(String, Vec<u64>, ErrorCode)> = p
        .omitted
        .iter()
        .map(|o| (o.path.clone(), o.snapshots.clone(), o.reason.code))
        .collect();
    let sif = ErrorCode::StoredIntegrityFailed;
    assert_eq!(
        listed,
        [
            ("b".to_string(), vec![0, 1], sif),
            ("d/b".to_string(), (2..=8).collect(), sif),
            ("e".to_string(), (5..=8).collect(), sif),
        ]
    );
    assert_eq!(p.omitted[0].version_id, p.omitted[1].version_id, "a rename");

    let (dir, r) = run(&src, &p, &RepairOptions::default());
    let r = r.unwrap();
    assert_eq!((r.outcome, r.exit_code), (Outcome::Partial, 2));
    assert_eq!(r.omitted, p.omitted);
    let mut omitted: BTreeMap<u64, BTreeSet<Vec<u8>>> = BTreeMap::new();
    for o in &p.omitted {
        for s in &o.snapshots {
            omitted
                .entry(*s)
                .or_default()
                .insert(o.path.as_bytes().to_vec());
        }
    }
    let all: Vec<u64> = (0..=8).collect();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all, &omitted);
}

/// **Step 4: recovery manifests.** The head segment's image (checkpoint 3)
/// is damaged: the head's catalog is rebuilt from snapshot manifest S(3)
/// and holds snapshots 3–8 only; 0–2 come from opening commit 2 (image 0).
/// Nothing is lost, so the repair is complete.
#[test]
fn c8_damaged_head_image_is_rebuilt_from_its_snapshot_manifest() {
    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| damage(b, checkpoint_refs(&h[3]).0));
    let p = plan_of(&src);
    assert_eq!(ladder(&p)[3], StepStatus::Used);
    let at: Vec<(u64, u64)> = p.snapshots.iter().map(|s| (s.seq, s.opened_at)).collect();
    assert_eq!(
        at,
        [
            (0, 2),
            (1, 2),
            (2, 2),
            (3, 8),
            (4, 8),
            (5, 8),
            (6, 8),
            (7, 8),
            (8, 8)
        ]
    );
    assert!(p.damage_found);
    assert_eq!(p.outcome, Outcome::Complete);
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    assert_eq!(r.unwrap().outcome, Outcome::Complete);
    let all: Vec<u64> = (0..=8).collect();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all, &BTreeMap::new());
}

/// **Step 3: metadata chains.** Delta manifest 1 is damaged, so commits 1
/// and 2 cannot be opened on their own (D10.9), but the head's catalog
/// holds their history, and S(3) still records the attributes of the
/// version delta 1 introduced (`d/a`, reachable at 3). Complete; the
/// failed manifest is listed.
#[test]
fn c8_damaged_delta_is_covered_by_a_later_catalog() {
    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| damage(b, h[1].commit.delta_manifest));
    assert!(open_at_footer(&src, h[1].footer_offset, &opts()).is_err());
    let p = plan_of(&src);
    assert_eq!(p.manifest_failures.len(), 1);
    assert_eq!(
        (
            p.manifest_failures[0].seq,
            p.manifest_failures[0].kind.as_str()
        ),
        (1, "delta")
    );
    assert!(p.damage_found);
    assert_eq!(p.outcome, Outcome::Complete);
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    assert_eq!(r.unwrap().outcome, Outcome::Complete);
    let all: Vec<u64> = (0..=8).collect();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all, &BTreeMap::new());
}

/// **Lost head and retention.** Delta manifest 5 breaks segment 3–8 from 5
/// on: snapshots 5–8 are lost (each with its open error), the head among
/// them, so retention cannot be rebuilt. Apply refuses without the waiver
/// and writes nothing; with it, the repair holds 0–4, no holds and nothing
/// expired, and says so.
#[test]
fn c8_a_lost_head_needs_the_retention_waiver() {
    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| damage(b, h[5].commit.delta_manifest));
    let p = plan_of(&src);
    let lost: Vec<(u64, ErrorCode)> = p
        .lost_snapshots
        .iter()
        .map(|l| (l.seq, l.reason.code))
        .collect();
    let sif = ErrorCode::StoredIntegrityFailed;
    assert_eq!(lost, [(5, sif), (6, sif), (7, sif), (8, sif)]);
    assert!(p.lost_snapshots.iter().all(|l| l.commit_id.is_some()));
    assert!(matches!(
        p.retention,
        Some(RetentionPlan::Unresolved { .. })
    ));
    assert_eq!(p.outcome, Outcome::Partial);

    let (dir, r) = run(&src, &p, &RepairOptions::default());
    assert_eq!(r.unwrap_err().code, ErrorCode::RetentionUnresolved);
    assert!(dir.names().is_empty(), "nothing written");

    let waive = RepairOptions {
        accept_retention_loss: true,
        ..RepairOptions::default()
    };
    let (dir, r) = run(&src, &p, &waive);
    let r = r.unwrap();
    assert!(r.retention_loss_accepted);
    assert_eq!((r.outcome, r.exit_code), (Outcome::Partial, 2));
    let out = dir.file(OUT).unwrap();
    assert_repairs(&pristine, &out, &[0, 1, 2, 3, 4], &BTreeMap::new());
    let head = open_head(&out, &opts()).unwrap();
    let ret = segment_state(&out, &head, &opts()).unwrap().retention;
    assert!(ret.holds.is_empty() && ret.expired.is_empty());
}

/// **Retention carried over a lost snapshot.** With the source's head
/// intact, a damaged chunk does not lose snapshots, but the plan maps the
/// hold on 2 and the expiry of 1–4 onto the new commits.
#[test]
fn c8_retention_is_carried_to_the_new_commits() {
    let pristine = source();
    let src = damaged(&pristine, |b| damage_chunk(&pristine, 8, "f", b));
    let p = plan_of(&src);
    match p.retention.as_ref().unwrap() {
        RetentionPlan::Carried { holds, expired } => {
            assert_eq!(holds.len(), 1);
            assert_eq!((holds[0].label.as_str(), holds[0].seq), ("audit", 2));
            assert_eq!(holds[0].new_seq, Some(2));
            assert_eq!(expired, &[1, 2, 3, 4]);
        }
        other => panic!("{other:?}"),
    }
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    r.unwrap();
    let out = dir.file(OUT).unwrap();
    let head = open_head(&out, &opts()).unwrap();
    let ret = segment_state(&out, &head, &opts()).unwrap().retention;
    assert_eq!(ret.expired, BTreeSet::from([1, 2, 3, 4]));
    assert_eq!(ret.holds.get(b"audit".as_slice()), Some(&2));
}

/// **The plan is what was approved.** A commit appended after planning, or
/// an edited plan, is refused before anything is written.
#[test]
fn c8_a_stale_or_edited_plan_is_refused() {
    let src = source();
    let p = plan_of(&src);

    let mut edited = p.clone();
    edited.omitted.clear();
    edited.snapshots.pop();
    let (dir, r) = run(&src, &edited, &RepairOptions::default());
    assert_eq!(r.unwrap_err().code, ErrorCode::InvalidArgument);
    assert!(dir.names().is_empty());

    let (mut w, _) = ArchiveWriter::open_append(
        src.clone(),
        Box::new(SeqIds::new(5)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "g", 8, 0o644);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    assert_eq!(r.unwrap_err().code, ErrorCode::InvalidArgument);
    assert!(dir.names().is_empty());
}

/// **Nothing recoverable.** Without any valid footer there is no head
/// (`NO_VALID_HEAD`); with the only descriptor damaged every commit
/// refuses interpretation (D12). Both plans say so with exit 1, and apply
/// writes nothing.
#[test]
fn c8_nothing_recoverable_writes_nothing() {
    let garbage = SimStorage::from_bytes(deterministic_bytes(1, 4096));
    let p = plan_of(&garbage);
    assert_eq!((p.outcome, p.exit_code), (Outcome::NothingRecoverable, 1));
    assert!(p.head.is_none());
    assert_eq!(ladder(&p)[..2], [StepStatus::NotAvailable; 2]);
    let (dir, r) = run(&garbage, &p, &RepairOptions::default());
    assert_eq!(r.unwrap_err().code, ErrorCode::NoValidHead);
    assert!(dir.names().is_empty());

    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| damage(b, h[0].commit.descriptor));
    let p = plan_of(&src);
    assert_eq!(p.outcome, Outcome::NothingRecoverable);
    assert_eq!(p.lost_snapshots.len(), 9);
    assert!(p
        .lost_snapshots
        .iter()
        .all(|l| l.reason.code == ErrorCode::DescriptorInvalid));
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    assert_eq!(r.unwrap_err().code, ErrorCode::RecordInvalid);
    assert!(dir.names().is_empty());
}

/// **Publication.** An existing destination is never replaced, and a
/// cancelled repair leaves nothing behind.
#[test]
fn c8_never_replaces_and_cancellation_leaves_nothing() {
    let src = source();
    let p = plan_of(&src);
    let mut dir = SimDir::new();
    let existing = SimStorage::from_bytes(b"keep me".to_vec());
    dir.insert(OUT, existing);
    let e = apply(
        &src,
        &p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(dir.file(OUT).unwrap().contents(), b"keep me");

    let job = Job::new();
    job.cancel.cancel();
    let mut dir = SimDir::new();
    let e = apply(
        &src,
        &p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions::default(),
        &job.ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(dir.names().is_empty());
}

/// **Attributes are never invented.** Delta 1 introduced `d/a` (seed 3);
/// with delta 1 *and* snapshot manifest S(3) damaged, no verified manifest
/// records that version's promised attributes, so it is left out of every
/// snapshot that holds it (1–6), though its content is intact.
/// (S(3) is the head segment's base, so retention is unresolved too.)
#[test]
fn c8_a_version_without_verified_attributes_is_left_out() {
    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| {
        damage(b, h[1].commit.delta_manifest);
        damage(b, checkpoint_refs(&h[3]).1);
    });
    let p = plan_of(&src);
    assert_eq!(p.omitted.len(), 1);
    let o = &p.omitted[0];
    assert_eq!(o.path, "d/a");
    assert_eq!(o.snapshots, (1..=6).collect::<Vec<_>>());
    assert_eq!(o.reason.code, ErrorCode::RecordInvalid);
    assert_eq!(p.manifest_failures.len(), 2);
    assert_eq!(p.outcome, Outcome::Partial);
    // S(3) is also the head segment's base, so the head's retention cannot
    // be rebuilt either (D10.10).
    assert!(matches!(
        p.retention,
        Some(RetentionPlan::Unresolved { .. })
    ));
    let waive = RepairOptions {
        accept_retention_loss: true,
        ..RepairOptions::default()
    };
    let (dir, r) = run(&src, &p, &waive);
    r.unwrap();
    let omitted: BTreeMap<u64, BTreeSet<Vec<u8>>> = (1..=6)
        .map(|s| (s, BTreeSet::from([b"d/a".to_vec()])))
        .collect();
    let all: Vec<u64> = (0..=8).collect();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all, &omitted);
}

/// **Trust flows from the head (§22.1).** Commit 2's record is damaged, so
/// the chain from the head breaks below commit 3. Snapshots 0–2 still come
/// from the head's catalog (image 3 holds their history), but their commit
/// records are not trusted: no commit IDs or times, and the version only
/// snapshot 0 holds (`d/a`, seed 1) has its attributes only in delta 0, a
/// manifest of an untrusted commit, so it is left out. Every version
/// reachable at checkpoint 3 keeps its attributes from S(3).
#[test]
fn c8_a_chain_break_trusts_only_what_the_head_reaches() {
    let pristine = source();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| {
        flip(b, h[2].commit_offset + 20);
    });
    let p = plan_of(&src);
    let br = p.chain_break.as_ref().unwrap();
    assert_eq!(br.first_trusted_seq, 3);
    assert!(p.lost_snapshots.is_empty());
    let ids: Vec<bool> = p.snapshots.iter().map(|s| s.commit_id.is_some()).collect();
    assert_eq!(
        ids,
        [false, false, false, true, true, true, true, true, true]
    );
    let listed: Vec<(&str, &[u64])> = p
        .omitted
        .iter()
        .map(|o| (o.path.as_str(), o.snapshots.as_slice()))
        .collect();
    assert_eq!(listed, [("d/a", &[0u64][..])]);
    assert_eq!(p.outcome, Outcome::Partial);

    let (dir, r) = run(&src, &p, &RepairOptions::default());
    let r = r.unwrap();
    assert!(r.commits[..3].iter().all(|c| c.source_commit_id.is_none()));
    let out = dir.file(OUT).unwrap();
    let new = opened_all(&out);
    assert!(new[..3].iter().all(|o| o.commit.time.is_none()));
    // Content and IDs as in the pristine source, minus d/a at 0.
    let source = opened_all(&pristine);
    for i in 0..9 {
        let mut want = read_state(&pristine, &source[i]).unwrap();
        if i == 0 {
            want.remove(b"d/a".as_slice());
        }
        assert_eq!(read_state(&out, &new[i]).unwrap(), want, "commit {i}");
    }
}

/// **Retention across lost snapshots.** Image 3 and delta 1 are damaged:
/// the head reads from S(3) (snapshots 3–8), and commits 1 and 2 cannot be
/// opened on their own, so they are lost. The hold on 2 is lost with its
/// snapshot (listed, never moved to another one); the expiry of 3 and 4
/// and a hold on 5 (added by commit 9) move to their new sequences 1, 2,
/// and 3.
#[test]
fn c8_retention_follows_the_new_sequence_and_lost_holds_are_listed() {
    let pristine = source();
    let (mut w, _) = ArchiveWriter::open_append(
        pristine.clone(),
        Box::new(SeqIds::new(5)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.hold(b"later", 5);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let h = history(&pristine);
    let src = damaged(&pristine, |b| {
        damage(b, h[1].commit.delta_manifest);
        damage(b, checkpoint_refs(&h[3]).0);
    });
    let p = plan_of(&src);
    let lost: Vec<u64> = p.lost_snapshots.iter().map(|l| l.seq).collect();
    assert_eq!(lost, [1, 2]);
    match p.retention.as_ref().unwrap() {
        RetentionPlan::Carried { holds, expired, .. } => {
            let mapped: Vec<(&str, u64, Option<u64>)> = holds
                .iter()
                .map(|x| (x.label.as_str(), x.seq, x.new_seq))
                .collect();
            assert_eq!(mapped, [("audit", 2, None), ("later", 5, Some(3))]);
            assert_eq!(expired, &[3, 4]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(p.outcome, Outcome::Partial);
    let (dir, r) = run(&src, &p, &RepairOptions::default());
    r.unwrap();
    let out = dir.file(OUT).unwrap();
    assert_repairs(&pristine, &out, &[0, 3, 4, 5, 6, 7, 8, 9], &BTreeMap::new());
    let head = open_head(&out, &opts()).unwrap();
    let ret = segment_state(&out, &head, &opts()).unwrap().retention;
    assert_eq!(ret.expired, BTreeSet::from([1, 2]));
    assert_eq!(ret.holds.len(), 1);
    assert_eq!(ret.holds.get(b"later".as_slice()), Some(&3));
}

/// **Contradictory metadata is rejected, not resolved (§22.1).** A forged
/// commit 9 (well-formed and hash-bound, so trusted by the chain) re-states
/// the version of `b` with other promised attributes. Opening 9 fails
/// (D10.4: a version is introduced once), so snapshot 9 is lost; and since
/// two verified manifests now disagree about that version, it is left out
/// of every snapshot, under both of its paths, rather than taking either.
#[test]
fn c8_manifests_that_disagree_about_a_version_reject_it() {
    use mochi_core::manifest::FileVersionEntry;
    use mochi_testkit::forge::{empty_delta, rule_base, txid, Forge};

    let pristine = source();
    let mut f = Forge::new(pristine.contents());
    let h = f.history();
    let head = &h[8];
    let o = open_at_footer(&pristine, head.footer_offset, &opts()).unwrap();
    let v = o
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("d/b"))
        .unwrap()
        .version;
    let (version, extents) = o.catalog.file_version(&v).unwrap().unwrap();
    let mut m = empty_delta(head, txid(0x77));
    m.file_versions.push(FileVersionEntry {
        version,
        extents,
        attributes: attrs(0o777, 99),
    });
    let r = f.append_manifest(&m);
    f.append_delta(head, rule_base(head), r, txid(0x77));
    let src = f.storage();

    let p = plan_of(&src);
    let lost: Vec<u64> = p.lost_snapshots.iter().map(|l| l.seq).collect();
    assert_eq!(lost, [9]);
    let listed: Vec<(&str, ErrorCode)> = p
        .omitted
        .iter()
        .map(|o| (o.path.as_str(), o.reason.code))
        .collect();
    assert_eq!(
        listed,
        [
            ("b", ErrorCode::RecordInvalid),
            ("d/b", ErrorCode::RecordInvalid)
        ]
    );
    assert!(p.omitted[0].reason.message.contains("disagree"));
}

/// Forge commit 9 on `source()` that puts `/dep` with one chunk. With
/// `needs_dictionary`, the chunk's record declares a dependency this build
/// cannot satisfy; otherwise the chunk is ordinary. The bytes are intact
/// either way.
fn with_dependency_commit(needs_dictionary: bool) -> SimStorage {
    use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
    use mochi_core::catalog::FileVersion;
    use mochi_core::manifest::{ChunkEntry, FileVersionEntry};
    use mochi_core::object::{build_object, Dependency, ObjectId};
    use mochi_format::codec::{EncodeParams, Protection};
    use mochi_format::digest::file_content_hash;
    use mochi_format::repr::{DecodedBytes, DecodedSlice};
    use mochi_format::Limits;
    use mochi_testkit::forge::{empty_delta, rule_base, txid, Forge};

    let content = deterministic_bytes(77, 150);
    let mut f = Forge::new(source().contents());
    let head = f.history()[8].clone();
    let mut obj = build_object(
        &DecodedBytes::new(content.clone()),
        &EncodeParams::default(),
        Protection::None,
        &mut SeqIds::new(0x5000),
        &Limits::default(),
    )
    .unwrap();
    if needs_dictionary {
        obj.record
            .dependencies
            .push(Dependency::Dictionary(ObjectId::from_bytes([9; 32])));
    }
    let at = f.append_object(&obj.stored).offset;
    let version = FileVersion {
        id: FileVersionId::from_bytes([0xD7; 32]),
        kind: EntryKind::File,
        logical_len: content.len() as u64,
        content_hash: Some(file_content_hash(DecodedSlice::from_logical(&content))),
    };
    let mut m = empty_delta(&head, txid(0x78));
    m.chunks.push(ChunkEntry {
        record: obj.record.clone(),
        location: Some(at),
    });
    m.file_versions.push(FileVersionEntry {
        version,
        extents: vec![mochi_core::catalog::extent::Extent {
            ordinal: 0,
            logical_offset: 0,
            length: content.len() as u64,
            source: ExtentSource::Chunk {
                chunk: obj.record.id,
                chunk_offset: 0,
            },
        }],
        attributes: attrs(0o644, 8),
    });
    m.ops.push(NamespaceOp::Put {
        path: path("dep"),
        version: FileVersionId::from_bytes([0xD7; 32]),
    });
    m.canonicalize();
    let r = f.append_manifest(&m);
    f.append_delta(&head, rule_base(&head), r, txid(0x78));
    f.storage()
}

/// **Intact content this build cannot decode is refused, not omitted
/// [delegated 2026-10-08, K4].** A forged commit 9 introduces a chunk whose
/// record declares a dictionary dependency (`UNSUPPORTED_FEATURE` on read).
/// `plan` fails with that code; leaving the file out would be silent loss
/// dressed as damage (§7.7, §26). Control: the same commit without the
/// dependency plans cleanly with nothing omitted, so the refusal is the
/// dependency's and nothing else's.
#[test]
fn c8_undecodable_content_refuses_the_plan() {
    let control = with_dependency_commit(false);
    let p = plan_of(&control);
    assert!(p.omitted.is_empty(), "{:?}", p.omitted);
    assert!(p.lost_snapshots.is_empty(), "{:?}", p.lost_snapshots);

    let s = with_dependency_commit(true);
    let e = plan(&s, &opts(), &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{e:?}");
}
