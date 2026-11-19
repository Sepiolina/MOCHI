//! C12: the Redundancy profile (spec §7.4, §15, §22 step 6; Annex B.2.11
//! D21; R6).
//!
//! The exit criteria of plan C12: ≤ m lost shards per group rebuild
//! **bit-identically**; > m is an explicit unrecoverable result and never
//! wrong bytes; parity survives the loss of the catalog. The oracle is the
//! pristine file: a healed view must equal it byte for byte, and a repaired
//! archive must hold exactly the pristine snapshots.

use mochi_core::descriptor::Profile;
use mochi_core::parity::{scan, CommitParity, ParityScan};
use mochi_core::publish::{
    commit_history, locate_head, open_head, ArchiveWriter, CheckpointPolicy, ReadOptions,
    TailPolicy, TailState, Transaction, WriterOptions,
};
use mochi_core::repair::{apply, plan, Outcome, RepairOptions, StepStatus};
use mochi_core::report::Report;
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::storage::ReadStorage;
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::digest::{parity_frame_digest, StoredObjectBytes};
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_format::parity::{decode_frame, Layout};
use mochi_format::Limits;
use mochi_testkit::archive::{
    encrypted_options, keyed_read, padded_history, path, read_state, scripted_history,
    test_options, Job,
};
use mochi_testkit::{SeqIds, SimDir, SimStorage};

const OUT: &str = "repaired.mochi";

/// Incompressible bytes added per commit so that each commit spans several
/// parity shards although catalog images are compressed (see
/// `padded_history`). The test writer chunks finely, so a few KB per commit is
/// enough: more makes the catalog (one row per chunk) large again.
const PAD: usize = 4000;

fn profile(percent: u16) -> Profile {
    Profile {
        redundancy: Some(percent),
        ..Profile::default()
    }
}

fn options(percent: u16) -> WriterOptions {
    WriterOptions {
        profile: Some(profile(percent)),
        ..test_options()
    }
}

fn write_with(opts: WriterOptions) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(3)), opts).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    let job = Job::new();
    for st in padded_history(PAD) {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn write(percent: u16) -> SimStorage {
    write_with(options(percent))
}

fn all_bytes(src: &dyn ReadStorage) -> Vec<u8> {
    let mut b = vec![0u8; src.size().unwrap() as usize];
    src.read_exact_at(0, &mut b).unwrap();
    b
}

fn scan_of(s: &SimStorage) -> (ParityScan, Vec<u8>) {
    let (sc, view) = scan(s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    let bytes = all_bytes(&view);
    (sc, bytes)
}

fn layout(c: &CommitParity, percent: u16) -> Layout {
    Layout::compute(c.range_offset.unwrap(), c.range_len.unwrap(), percent).unwrap()
}

fn verify_at(s: &dyn ReadStorage, read: ReadOptions, level: VerificationLevel) -> Report {
    verify(
        s,
        &VerifyOptions {
            level,
            deep: true,
            read,
            ..VerifyOptions::default()
        },
        &Job::new().ctx(),
    )
    .report
}

fn verify_full(s: &dyn ReadStorage) -> Report {
    verify_at(s, ReadOptions::default(), VerificationLevel::Restoration)
}

fn dim(r: &Report, d: Dimension) -> Status {
    r.dimensions[&d]
}

fn codes(r: &Report) -> Vec<ErrorCode> {
    r.findings.iter().map(|f| f.code).collect()
}

fn damaged(s: &SimStorage, f: impl FnOnce(&mut Vec<u8>)) -> SimStorage {
    let mut b = s.contents();
    f(&mut b);
    SimStorage::from_bytes(b)
}

/// Flip one byte in each listed shard of commit `c`'s range.
fn hit_shards(b: &mut [u8], l: &Layout, shards: &[u64]) {
    for &i in shards {
        let (off, n) = l.shard_extent(i);
        assert!(n > 0, "shard {i} is virtual");
        b[(off + n / 2) as usize] ^= 0xA5;
    }
}

/// **Layout on the wire.** Every commit's parity frames sit between its
/// commit frame and its footer, protect [parent's footer, end of commit
/// frame) (commit 0 from offset 0, so the descriptor), carry the commit's
/// identity, and follow the layout rule for the descriptor's percentage.
#[test]
fn c12_parity_frames_sit_between_each_commit_and_its_footer() {
    let s = write(25);
    let bytes = s.contents();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    assert_eq!(history.len(), 3);
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert_eq!(head.descriptor.profile(), profile(25));
    for (n, e) in history.iter().enumerate() {
        let start = e.commit.parent.map_or(0, |p| p.footer_offset);
        let end = e.commit_offset + {
            let r = mochi_core::storage::StorageReader::new(&s).unwrap();
            mochi_format::footer::parse_footer_frame(&r, e.footer_offset)
                .unwrap()
                .commit_len
        };
        let l = Layout::compute(start, end - start, 25).unwrap();
        let flen = l.frame_len(0) as usize;
        assert_eq!(
            e.footer_offset - end,
            l.frame_count() * flen as u64,
            "commit {n}"
        );
        for j in 0..l.frame_count() as usize {
            let at = end as usize + j * flen;
            let fr = decode_frame(&bytes[at..at + flen], &[], &Limits::default()).unwrap();
            assert_eq!(fr.layout, l);
            assert_eq!(fr.envelope.identity.commit_sequence, e.commit.seq);
            assert_eq!(fr.envelope.identity.transaction_id, e.commit.transaction_id);
            assert_eq!(
                fr.envelope.identity.archive_id,
                *e.commit.archive_id.as_bytes()
            );
            assert_eq!(
                (fr.group, fr.parity_index),
                (j as u64 / l.parity_shards, j as u64 % l.parity_shards)
            );
        }
    }
    // A Core reader reads it as it reads any archive.
    let state = read_state(&s, &head).unwrap();
    assert_eq!(state, padded_history(PAD).last().unwrap().after);
    let r = verify_full(&s);
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert_eq!(dim(&r, Dimension::Recoverability), Status::Pass);
    assert!(r.scope.as_deref().unwrap().contains("Redundancy profile"));
    let (sc, healed) = scan_of(&s);
    assert_eq!(healed, bytes);
    assert_eq!(sc.declared_percent, Some(25));
    assert_eq!(sc.commits.len(), 3);
    assert!(sc.stopped.is_none() && sc.shards_damaged() == 0 && sc.frames_damaged() == 0);
}

/// **≤ m rebuild bit-identically**, in every commit at once: m shards of
/// each group, including the shards that hold the descriptor, a commit
/// record, a catalog image, and the parent's footer.
#[test]
fn c12_up_to_m_damaged_shards_per_group_rebuild_bit_identically() {
    let s = write(50);
    let pristine = s.contents();
    let (sc, _) = scan_of(&s);
    for c in &sc.commits {
        let l = layout(c, 50);
        assert!(l.parity_shards >= 2, "the fixture needs m ≥ 2: {l:?}");
    }
    // Each pattern: which data indices of group 0 to hit, in every commit.
    for pick in [vec![0u64, 1], vec![0, 3], vec![2, 3], vec![1]] {
        let bad = damaged(&s, |b| {
            for c in &sc.commits {
                let l = layout(c, 50);
                let real: Vec<u64> = pick
                    .iter()
                    .map(|j| l.shard_index(0, *j))
                    .filter(|i| *i < l.real_shards())
                    .collect();
                hit_shards(b, &l, &real);
            }
        });
        assert_ne!(bad.contents(), pristine);
        let (out, healed) = scan_of(&bad);
        assert_eq!(healed, pristine, "{pick:?}");
        assert!(out.shards_rebuilt() > 0 && out.beyond_capacity().next().is_none());

        // verify: the damage is a FAIL of integrity, and recoverable.
        let r = verify_full(&bad);
        assert_eq!(dim(&r, Dimension::Integrity), Status::Fail, "{pick:?}");
        assert_eq!(
            dim(&r, Dimension::Recoverability),
            Status::Degraded,
            "{pick:?}"
        );
        assert!(r
            .scope
            .as_deref()
            .unwrap()
            .contains("rebuilt archive verifies"));
        assert_eq!(r.exit_code, 1);
    }
}

/// **Repair writes what parity rebuilt**: complete, step 6 used, every
/// snapshot identical to the pristine one, and the new archive keeps the
/// recovery record and verifies clean.
#[test]
fn c12_repair_rebuilds_from_parity_and_keeps_the_recovery_record() {
    let s = write(50);
    let (sc, _) = scan_of(&s);
    let head = &sc.commits[0];
    let l = layout(head, 50);
    // The head's commit record is in its last real shard.
    let bad = damaged(&s, |b| hit_shards(b, &l, &[0, l.real_shards() - 1]));
    assert!(
        open_head(&bad, &ReadOptions::default()).is_err() || {
            // Whichever object the damage hit, the plain open must notice.
            verify_full(&bad).exit_code != 0
        }
    );
    let p = plan(&bad, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    let step6 = p.ladder.iter().find(|x| x.step == 6).unwrap();
    assert_eq!(step6.status, StepStatus::Used, "{step6:?}");
    assert!(p.damage_found);
    assert_eq!(p.parity.as_ref().unwrap().shards_rebuilt(), 2);
    // The plan survives JSON.
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(
        serde_json::from_str::<mochi_core::repair::RepairPlan>(&json).unwrap(),
        p
    );

    let mut dir = SimDir::new();
    let report = apply(
        &bad,
        &p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &ReadOptions::default(),
        &RepairOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(report.outcome, Outcome::Complete);
    let out = dir.file(OUT).unwrap();
    let new_head = open_head(&out, &ReadOptions::default()).unwrap();
    assert_eq!(new_head.descriptor.profile(), profile(50));
    let pristine_history = commit_history(&s, &ReadOptions::default()).unwrap();
    let new_history = commit_history(&out, &ReadOptions::default()).unwrap();
    assert_eq!(new_history.len(), pristine_history.len());
    for (a, b) in pristine_history.iter().zip(&new_history) {
        let pa = mochi_core::publish::open_at_footer(&s, a.footer_offset, &ReadOptions::default())
            .unwrap();
        let pb =
            mochi_core::publish::open_at_footer(&out, b.footer_offset, &ReadOptions::default())
                .unwrap();
        assert_eq!(read_state(&out, &pb).unwrap(), read_state(&s, &pa).unwrap());
    }
    let r = verify_full(&out);
    assert_eq!(r.exit_code, 0, "{r:?}");
    let (sc2, _) = scan_of(&out);
    assert_eq!(sc2.commits.len(), 3);
}

/// **> m is explicit and never wrong bytes.** One more damaged shard than a
/// group has parity: the group is reported (`PARITY_CAPACITY_EXCEEDED`,
/// recoverability `FAIL`), its bytes are left exactly as damaged (the healed
/// view equals the damaged file there), and repair leaves out what the
/// damage touches.
#[test]
fn c12_damage_beyond_parity_capacity_is_an_explicit_unrecoverable_result() {
    let s = write(25);
    let (sc, _) = scan_of(&s);
    let head = &sc.commits[0];
    let l = layout(head, 25);
    let m = l.parity_shards;
    assert!(l.real_shards() > m, "{l:?}");
    // m + 1 data shards of group 0, avoiding the last (the commit record),
    // so the head still opens and only file content is lost.
    let hit: Vec<u64> = (0..=m).map(|j| l.shard_index(0, j)).collect();
    assert!(hit.iter().all(|i| *i + 1 < l.real_shards()), "{l:?}");
    let bad = damaged(&s, |b| hit_shards(b, &l, &hit));
    let (out, healed) = scan_of(&bad);
    assert_eq!(healed, bad.contents(), "nothing in the group is rewritten");
    let losses: Vec<_> = out.beyond_capacity().collect();
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].0, head.seq);
    assert_eq!(losses[0].1.damaged_shards, m + 1);
    assert_eq!(losses[0].1.intact_parity, m);

    let r = verify_full(&bad);
    assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
    assert!(
        codes(&r).contains(&ErrorCode::ParityCapacityExceeded),
        "{r:?}"
    );
    assert_eq!(r.exit_code, 1);

    let p = plan(&bad, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_ne!(p.outcome, Outcome::Complete);
    let step6 = p.ladder.iter().find(|x| x.step == 6).unwrap();
    assert_eq!(step6.status, StepStatus::NotAvailable);
    assert!(
        step6.detail.contains("PARITY_CAPACITY_EXCEEDED"),
        "{step6:?}"
    );
    assert_eq!(p.parity.as_ref().unwrap().beyond_capacity().count(), 1);
}

/// **Parity survives the loss of the catalog**: the head's catalog image
/// zeroed entirely is rebuilt from parity, with no catalog consulted.
#[test]
fn c12_parity_survives_loss_of_the_catalog() {
    let s = write(100);
    let pristine = s.contents();
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    let mochi_core::commit::Metadata::Checkpoint { image, .. } = head.commit.metadata else {
        panic!("every commit of the fixture is a checkpoint");
    };
    let bad = damaged(&s, |b| {
        b[image.offset as usize..(image.offset + image.stored_len) as usize].fill(0);
    });
    let (_, healed) = scan_of(&bad);
    assert_eq!(healed, pristine);
    let p = plan(&bad, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(p.outcome, Outcome::Complete);
}

/// The descriptor and a footer that later commits' parity protects are
/// rebuilt too (commit 0 covers offset 0; commit n covers footer n − 1).
/// The head's own footer is in no range: damaged, it is not rebuilt, and
/// the walk says why it stopped.
#[test]
fn c12_descriptor_and_earlier_footers_are_protected() {
    let s = write(50);
    let pristine = s.contents();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    let footer0 = history
        .iter()
        .find(|e| e.commit.seq == 0)
        .unwrap()
        .footer_offset as usize;
    let bad = damaged(&s, |b| {
        b[20] ^= 0xFF; // the descriptor
        b[footer0 + 40] ^= 0xFF; // footer 0's digest
    });
    assert_ne!(
        verify_full(&bad).exit_code,
        0,
        "the damage is visible without parity"
    );
    let (_, healed) = scan_of(&bad);
    assert_eq!(healed, pristine);

    let head_footer = history
        .iter()
        .find(|e| e.commit.seq == 2)
        .unwrap()
        .footer_offset as usize;
    let bad = damaged(&s, |b| b[head_footer + 40] ^= 0xFF);
    let (sc, healed) = scan_of(&bad);
    assert_eq!(healed, bad.contents());
    assert!(
        sc.stopped.as_deref().unwrap().contains("does not validate"),
        "{sc:?}"
    );
}

/// **Damaged parity reduces protection, not integrity**: `PARITY_DAMAGED`,
/// integrity `PASS`, recoverability `DEGRADED`, exit 2.
#[test]
fn c12_damaged_parity_frame_is_degraded_protection() {
    let s = write(25);
    let (sc, _) = scan_of(&s);
    let c = &sc.commits[1];
    let l = layout(c, 25);
    let commit_end = l.range_offset + l.range_len;
    let bad = damaged(&s, |b| b[(commit_end + 300) as usize] ^= 1);
    let r = verify_full(&bad);
    assert_eq!(dim(&r, Dimension::Integrity), Status::Pass, "{r:?}");
    assert_eq!(dim(&r, Dimension::Recoverability), Status::Degraded);
    assert!(codes(&r).contains(&ErrorCode::ParityDamaged));
    assert_eq!(r.exit_code, 2);
    // Repair rewrites the archive with fresh parity: complete.
    let p = plan(&bad, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(p.outcome, Outcome::Complete);
    assert!(p.damage_found);
}

/// **Frames that verify but disagree with their commit** (a forged
/// identity, re-digested) are a profile violation, and nothing from them is
/// used.
#[test]
fn c12_parity_frames_with_another_identity_are_a_profile_violation() {
    let s = write(25);
    let (sc, _) = scan_of(&s);
    let c = &sc.commits[0];
    let l = layout(c, 25);
    let commit_end = (l.range_offset + l.range_len) as usize;
    let flen = l.frame_len(0) as usize;
    let bad = damaged(&s, |b| {
        for j in 0..l.frame_count() as usize {
            let f = &mut b[commit_end + j * flen..commit_end + (j + 1) * flen];
            f[8 + 56] ^= 1; // the envelope's commit sequence
            let at = flen - 32;
            let d = parity_frame_digest(StoredObjectBytes::new(&f[..at]));
            f[at..].copy_from_slice(d.as_bytes());
        }
    });
    let r = verify_full(&bad);
    assert!(codes(&r).contains(&ErrorCode::ProfileViolation), "{r:?}");
    assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
}

/// An interrupted commit (parity written, footer not) leaves a tail that is
/// eligible for truncation, as any interrupted write does (D14).
#[test]
fn c12_a_commit_cut_before_its_footer_is_an_uncommitted_tail() {
    let s = write(25);
    let b = s.contents();
    let cut = SimStorage::from_bytes(b[..b.len() - FOOTER_FRAME_LEN as usize].to_vec());
    let loc = locate_head(&cut, &Limits::default()).unwrap();
    assert!(
        matches!(loc.tail, TailState::Uncommitted { .. }),
        "{:?}",
        loc.tail
    );
    assert_eq!(loc.footer.fields.commit_sequence, 1);
}

/// The profile is fixed at creation: an append that asks to add, drop, or
/// resize the recovery record is refused, and the percentage is checked.
#[test]
fn c12_the_recovery_record_is_fixed_at_creation() {
    let s = write(10);
    for asked in [Profile::default(), profile(20)] {
        let e = ArchiveWriter::open_append(
            s.handle(),
            Box::new(SeqIds::new(5)),
            WriterOptions {
                profile: Some(asked),
                ..test_options()
            },
            TailPolicy::Refuse,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported, "{asked:?}");
    }
    let core = SimStorage::new();
    mochi_testkit::archive::build(core.clone(), 1, &scripted_history()[..1]).unwrap();
    let e = ArchiveWriter::open_append(
        core.handle(),
        Box::new(SeqIds::new(5)),
        options(10),
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported);
    for bad in [0u16, 101] {
        let e = ArchiveWriter::create(SimStorage::new(), Box::new(SeqIds::new(1)), options(bad))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad}");
    }
    // Appending keeps the profile without being asked.
    let (mut w, _) = ArchiveWriter::open_append(
        s.handle(),
        Box::new(SeqIds::new(5)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("late"), b"late".to_vec(), Default::default());
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let (sc, _) = scan_of(&s);
    assert_eq!(sc.commits.len(), 4);
    assert_eq!(verify_full(&s).exit_code, 0);
}

/// **Encrypted and redundant**: parity covers stored (sealed) bytes, so a
/// keyless run checks it and the healed view is exact; with the key,
/// everything verifies.
#[test]
fn c12_encrypted_archives_carry_parity_over_sealed_bytes() {
    let mut o = encrypted_options(&["pw"]);
    o.profile = Some(Profile {
        encrypted: true,
        redundancy: Some(50),
        ..Profile::default()
    });
    let s = write_with(o);
    let pristine = s.contents();
    let r = verify_at(&s, keyed_read(&["pw"]), VerificationLevel::Restoration);
    assert_eq!(r.exit_code, 0, "{r:?}");
    let (sc, _) = scan_of(&s);
    let l = layout(&sc.commits[0], 50);
    let bad = damaged(&s, |b| hit_shards(b, &l, &[0]));
    let (_, healed) = scan_of(&bad);
    assert_eq!(healed, pristine);
    let r = verify_at(
        &bad,
        ReadOptions::default(),
        VerificationLevel::StoredIntegrity,
    );
    assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
    assert_eq!(
        dim(&r, Dimension::Recoverability),
        Status::Degraded,
        "{r:?}"
    );
}

/// **TAR-compatible and redundant**: parity frames are skippable, so the
/// commits' streams are unchanged and the profile check passes.
#[test]
fn c12_tar_compatible_archives_carry_parity() {
    let s = write_with(WriterOptions {
        profile: Some(Profile {
            tar_compatible: true,
            redundancy: Some(10),
            ..Profile::default()
        }),
        ..test_options()
    });
    let r = verify_full(&s);
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert!(r.scope.as_deref().unwrap().contains("TAR stream"));
    assert!(r.scope.as_deref().unwrap().contains("Redundancy profile"));
}

/// A Core archive has no recovery record: verify says nothing about parity
/// and repair reports step 6 as not available, never as passed.
#[test]
fn c12_core_archives_have_no_parity_step() {
    let s = SimStorage::new();
    mochi_testkit::archive::build(s.clone(), 1, &scripted_history()).unwrap();
    let r = verify_full(&s);
    assert!(!r.scope.as_deref().unwrap().contains("Redundancy"));
    let p = plan(&s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    let step6 = p.ladder.iter().find(|x| x.step == 6).unwrap();
    assert_eq!(step6.status, StepStatus::NotAvailable);
    assert!(p.parity.is_none());
}

/// **Interleaving**: shard *i* is in group *i* mod *G*, so a contiguous
/// burst spreads over the groups. In a commit of ~3 MiB (two groups of 98
/// data shards, 10 parity shards each at 10 %), a burst over 20 consecutive
/// shards (10 per group) is rebuilt; 22 (11 in each group) is beyond capacity
/// in both groups and is reported for both, with nothing rewritten.
#[test]
fn c12_a_contiguous_burst_spreads_across_groups() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(3)),
        WriterOptions {
            chunk_size: Some(1 << 20),
            ..options(10)
        },
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(
        path("big"),
        mochi_testkit::deterministic_bytes(11, 3 << 20),
        Default::default(),
    );
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let pristine = s.contents();
    let (sc, _) = scan_of(&s);
    let l = layout(&sc.commits[0], 10);
    assert_eq!(
        (l.groups, l.data_shards, l.parity_shards),
        (2, 97, 10),
        "{l:?}"
    );

    let burst = |n: u64| {
        damaged(&s, |b| {
            let (start, _) = l.shard_extent(40);
            let (last, len) = l.shard_extent(40 + n - 1);
            b[start as usize..(last + len) as usize].fill(0x5A);
        })
    };
    let (out, healed) = scan_of(&burst(20));
    assert_eq!(healed, pristine);
    assert_eq!(out.shards_rebuilt(), 20);

    let bad = burst(22);
    let (out, healed) = scan_of(&bad);
    assert_eq!(healed, bad.contents());
    assert_eq!(out.beyond_capacity().count(), 2);
    let r = verify_full(&bad);
    assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
}

/// A commit's first parity frame damaged: in its payload (the header still
/// gives the frame length, so the next slot is tried) or in its magic (then
/// the region is searched for the parity magic). Either way the other frame
/// rebuilds a damaged data shard.
#[test]
fn c12_a_damaged_first_frame_does_not_hide_the_others() {
    let s = write(50);
    let pristine = s.contents();
    let (sc, _) = scan_of(&s);
    let c = &sc.commits[1];
    let l = layout(c, 50);
    assert_eq!(l.parity_shards, 2, "{l:?}");
    let commit_end = (l.range_offset + l.range_len) as usize;
    for at in [commit_end + 500, commit_end] {
        let bad = damaged(&s, |b| {
            b[at] ^= 0x10;
            hit_shards(b, &l, &[l.shard_index(0, 1)]);
        });
        let (out, healed) = scan_of(&bad);
        let mut want = pristine.clone();
        want[at] ^= 0x10; // parity bytes are not rebuilt
        assert_eq!(healed, want, "damage at {at}");
        assert_eq!(out.commits[1].frames_damaged, 1);
        assert_eq!(out.commits[1].shards_rebuilt, 1);
    }
}

/// Compaction keeps the source's recovery record (D21 item 11) and writes
/// fresh parity for the new archive.
#[test]
fn c12_compaction_keeps_the_recovery_record() {
    use mochi_core::compact::{compact, CompactOptions, Keep};
    let s = write(30);
    let (w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(2)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut dir = SimDir::new();
    compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    let out = dir.file(OUT).unwrap();
    let head = open_head(&out, &ReadOptions::default()).unwrap();
    assert_eq!(head.descriptor.profile(), profile(30));
    let (sc, _) = scan_of(&out);
    assert_eq!(sc.commits.len(), 3);
    assert!(sc.violations().next().is_none() && sc.frames_damaged() == 0);
    assert_eq!(verify_full(&out).exit_code, 0);
}

/// Re-encode every parity frame of the commit whose range is `l`, with
/// `edit` applied to the layout and the parity computed over the bytes the
/// new layout names: frames that are valid in themselves.
fn reforge(b: &mut [u8], l: &Layout, edit: impl Fn(&Layout) -> Layout, seq: u64) {
    use mochi_format::parity::{accumulate, data_shard_hash, encode_frame, ParityFrameInfo};
    let commit_end = (l.range_offset + l.range_len) as usize;
    let flen = l.frame_len(0) as usize;
    let first = decode_frame(&b[commit_end..commit_end + flen], &[], &Limits::default()).unwrap();
    let id = first.envelope.identity;
    assert_eq!(id.commit_sequence, seq);
    let nl = edit(l);
    assert_eq!(nl.frame_len(0) as usize, flen, "same frame length");
    assert_eq!(nl.frame_count(), l.frame_count(), "same frame count");
    let info = ParityFrameInfo {
        layout: nl,
        archive_id: id.archive_id,
        seq: id.commit_sequence,
        transaction_id: id.transaction_id,
        required_features: Vec::new(),
    };
    let s = nl.shard_size as usize;
    let mut out = Vec::new();
    for g in 0..nl.groups {
        let mut acc = vec![vec![0u8; s]; nl.parity_shards as usize];
        let mut hashes = Vec::new();
        for j in 0..nl.data_shards {
            let (off, n) = nl.shard_extent(nl.shard_index(g, j));
            let d = &b[off as usize..(off + n) as usize];
            hashes.push(data_shard_hash(d, &nl));
            accumulate(&mut acc, j, d).unwrap();
        }
        for (p, sh) in acc.iter().enumerate() {
            out.extend(
                encode_frame(
                    &info,
                    &[],
                    g,
                    p as u64,
                    &hashes,
                    sh,
                    &Limits::WRITER_DEFAULT,
                )
                .unwrap(),
            );
        }
    }
    b[commit_end..commit_end + out.len()].copy_from_slice(&out);
}

/// Frames that are valid in themselves but protect a range that does not
/// start at the parent's footer are a profile violation (D21 item 7).
#[test]
fn c12_parity_over_the_wrong_range_is_a_profile_violation() {
    let s = write(50);
    let (sc, _) = scan_of(&s);
    let c = &sc.commits[1];
    let l = layout(c, 50);
    let bad = damaged(&s, |b| {
        reforge(
            b,
            &l,
            |l| Layout::compute(l.range_offset + 64, l.range_len - 64, 50).unwrap(),
            1,
        )
    });
    let (out, _) = scan_of(&bad);
    assert!(
        out.commits[1]
            .violation
            .as_deref()
            .unwrap()
            .contains("parent's footer"),
        "{out:?}"
    );
    let r = verify_full(&bad);
    assert!(codes(&r).contains(&ErrorCode::ProfileViolation), "{r:?}");
}

/// **Never wrong bytes, even from forged parity.** Every frame of a group
/// re-digested with a parity shard that is not the code's: a damaged data
/// shard solved from it does not match its table entry, so nothing is
/// written there and the group is reported beyond capacity.
#[test]
fn c12_a_rebuilt_shard_that_misses_its_hash_is_never_used() {
    let s = write(50);
    let (sc, _) = scan_of(&s);
    let c = &sc.commits[0];
    let l = layout(c, 50);
    let commit_end = (l.range_offset + l.range_len) as usize;
    let flen = l.frame_len(0) as usize;
    let target = l.shard_index(0, 1);
    let bad = damaged(&s, |b| {
        for j in 0..l.frame_count() as usize {
            let f = &mut b[commit_end + j * flen..commit_end + (j + 1) * flen];
            // The parity shard starts after the envelope, fixed fields, and table.
            let at = 8 + 80 + 56 + 32 * l.data_shards as usize + 7;
            f[at] ^= 0x01;
            let end = flen - 32;
            let d = parity_frame_digest(StoredObjectBytes::new(&f[..end]));
            f[end..].copy_from_slice(d.as_bytes());
        }
        hit_shards(b, &l, &[target]);
    });
    let (out, healed) = scan_of(&bad);
    let (off, n) = l.shard_extent(target);
    assert_eq!(
        healed[off as usize..(off + n) as usize],
        bad.contents()[off as usize..(off + n) as usize],
        "the damaged shard is left as it was"
    );
    assert_eq!(out.commits[0].shards_rebuilt, 0);
    assert_eq!(out.beyond_capacity().count(), 1);
}
