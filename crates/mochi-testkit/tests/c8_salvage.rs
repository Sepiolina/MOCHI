//! C8, ladder step 7: the bounded salvage scan (spec §22, §22.1, §5.1; Annex
//! B.2.13 D23; `mochi_core::salvage`).
//!
//! The oracle is a pristine copy of the source, read with the test kit's own
//! reassembly (`archive::read_state`), as in `c8_repair.rs`: every recovered
//! snapshot must hold exactly the pristine snapshot's entries, with the same
//! version IDs and promised attributes. What each test expects to be
//! confirmed or left out is written from the damage placed in the fixture,
//! not from the scan's output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use mochi_core::copies::Trust;
use mochi_core::descriptor::Profile;
use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::object::ArchiveId;
use mochi_core::publish::{
    commit_history, open_at_footer, promised_attributes, ArchiveWriter, CheckpointPolicy,
    HistoryEntry, OpenedHead, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::repair::{
    apply_with_salvage, plan_with_salvage, Outcome, RepairOptions, RepairPlan, RepairReport,
    StepStatus,
};
use mochi_core::salvage::SalvageOptions;
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{path, read_state, test_options, Job};
use mochi_testkit::replay::{attrs, flip};
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const OUT: &str = "salvaged.mochi";

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn salvage() -> SalvageOptions {
    SalvageOptions::default()
}

/// One commit per `(name, seed)`: the file `name` with `len` bytes derived
/// from `seed`.
fn build_with(
    ids: u64,
    files: &[(&str, u64)],
    len: usize,
    options: WriterOptions,
    policy: CheckpointPolicy,
) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(ids)), options).unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    let job = Job::new();
    for (name, seed) in files {
        let mut tx = Transaction::new();
        tx.put_file(
            path(name),
            deterministic_bytes(*seed, len),
            attrs(0o644, *seed as i64),
        );
        w.commit(tx, &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn build(files: &[(&str, u64)]) -> SimStorage {
    build_with(1, files, 150, test_options(), CheckpointPolicy::Never)
}

const THREE: [(&str, u64); 3] = [("a", 1), ("b", 2), ("c", 3)];
const SIX: [(&str, u64); 6] = [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5), ("f", 6)];

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
    let mut b = s.contents();
    f(&mut b);
    SimStorage::from_bytes(b)
}

fn plan_s(s: &SimStorage) -> RepairPlan {
    plan_with_salvage(s, &[], Some(&salvage()), &opts(), &Job::new().ctx()).unwrap()
}

fn plan_o(s: &SimStorage, sv: &SalvageOptions, o: &ReadOptions) -> mochi_core::Result<RepairPlan> {
    plan_with_salvage(s, &[], Some(sv), o, &Job::new().ctx())
}

fn step7(p: &RepairPlan) -> StepStatus {
    p.ladder.iter().find(|l| l.step == 7).unwrap().status
}

fn recovered(p: &RepairPlan) -> Vec<u64> {
    p.snapshots.iter().map(|s| s.seq).collect()
}

fn run(
    s: &SimStorage,
    sv: &SalvageOptions,
    p: &RepairPlan,
    accept_retention_loss: bool,
) -> (SimDir, mochi_core::Result<RepairReport>) {
    run_with(s, sv, p, accept_retention_loss, &opts())
}

fn run_with(
    s: &SimStorage,
    sv: &SalvageOptions,
    p: &RepairPlan,
    accept_retention_loss: bool,
    read: &ReadOptions,
) -> (SimDir, mochi_core::Result<RepairReport>) {
    let mut dir = SimDir::new();
    let r = apply_with_salvage(
        s,
        &[],
        Some(sv),
        p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        read,
        &RepairOptions {
            accept_retention_loss,
            ..RepairOptions::default()
        },
        &Job::new().ctx(),
    );
    (dir, r)
}

/// The repaired archive holds exactly the pristine snapshots `kept`, in
/// order, each with identical contents, version IDs, and promised
/// attributes.
fn assert_repairs(pristine: &SimStorage, out: &SimStorage, kept: &[u64]) {
    let source = opened_all(pristine);
    let new = opened_all(out);
    assert_eq!(new.len(), kept.len(), "one commit per recovered snapshot");
    for (i, r) in kept.iter().enumerate() {
        let (a, b) = (&source[*r as usize], &new[i]);
        assert_eq!(
            read_state(out, b).unwrap(),
            read_state(pristine, a).unwrap(),
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
    }
}

fn all(n: u64) -> Vec<u64> {
    (0..n).collect()
}

/// Flip one byte in the payload of the footer of every commit in `which`.
fn break_footers(s: &SimStorage, which: &[usize]) -> SimStorage {
    let h = history(s);
    damaged(s, |b| {
        for i in which {
            flip(b, h[*i].footer_offset + 30);
        }
    })
}

fn commit_ids(p: &RepairPlan) -> Vec<(u64, Option<String>)> {
    p.snapshots
        .iter()
        .map(|s| (s.seq, s.commit_id.clone()))
        .collect()
}

// ---- 1 ---------------------------------------------------------------------

/// **Every footer has one flipped byte; there is no head.** The commits below
/// the top are confirmed by their children's parent links and recovered; the
/// top commit has neither a footer nor a child, so it is listed as
/// unconfirmed and never recovered (§5.1). The plan is partial.
#[test]
fn salvage_1_every_footer_flipped_recovers_all_but_the_unconfirmed_head() {
    let pristine = build(&SIX);
    let bad = break_footers(&pristine, &[0, 1, 2, 3, 4, 5]);

    // Control: without --salvage there is nothing.
    let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(plain.outcome, Outcome::NothingRecoverable);
    assert_eq!(step7(&plain), StepStatus::NotAttempted);
    assert!(plain.ladder[6].detail.contains("--salvage"), "{plain:#?}");

    let p = plan_s(&bad);
    assert_eq!(p.outcome, Outcome::Partial, "{p:#?}");
    assert_eq!(p.exit_code, 2);
    assert_eq!(step7(&p), StepStatus::Used, "{:#?}", p.ladder);
    assert_eq!(recovered(&p), all(5));
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Salvage));
    let sv = p.salvage.as_ref().unwrap();
    let h = history(&pristine);
    assert_eq!(sv.unconfirmed.len(), 1);
    assert_eq!(sv.unconfirmed[0].seq, 5);
    assert_eq!(sv.unconfirmed[0].offset, h[5].commit_offset);
    assert!(sv.unconfirmed[0].by.contains("§5.1"));
    assert_eq!(sv.confirmed.len(), 5);
    assert!(sv.confirmed.iter().all(|c| c.by == "link"));
    assert_eq!(sv.footers_regenerated.len(), 5);
    assert_eq!(sv.segments.len(), 1);
    assert_eq!((sv.segments[0].top_seq, sv.segments[0].bottom_seq), (4, 0));
    assert!(!sv.limit_reached);
    assert_eq!(sv.foreign_commits, 0);
    assert!(p.head.is_none(), "salvage never invents a trusted head");

    // Apply: retention is unresolved without a trusted head.
    let (_, refused) = run(&bad, &salvage(), &p, false);
    assert_eq!(refused.unwrap_err().code, ErrorCode::RetentionUnresolved);
    let (dir, r) = run(&bad, &salvage(), &p, true);
    let report = r.unwrap();
    assert_eq!(report.outcome, Outcome::Partial);
    assert_eq!(report.exit_code, 2);
    assert!(report.retention_loss_accepted);
    assert!(report.commits.iter().all(|c| c.trust == Trust::Salvage));
    assert_eq!(report.reverification.exit_code, 0);
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(5));
}

// ---- 10 --------------------------------------------------------------------

/// `--salvage` on a clean archive: step 7 is not needed, nothing changes, and
/// the plan equals the one made without it except for step 7's wording.
#[test]
fn salvage_10_a_clean_archive_does_not_need_it() {
    let pristine = build(&SIX);
    let p = plan_s(&pristine);
    assert_eq!(step7(&p), StepStatus::NotNeeded);
    assert_eq!(p.outcome, Outcome::Complete);
    assert_eq!(p.exit_code, 0);
    assert!(p.salvage.is_none());
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Chain));
    let plain = mochi_core::repair::plan(&pristine, &opts(), &Job::new().ctx()).unwrap();
    let mut a = p.clone();
    let mut b = plain.clone();
    a.ladder.remove(6);
    b.ladder.remove(6);
    assert_eq!(a, b);
    let (dir, r) = run(&pristine, &salvage(), &p, false);
    assert_eq!(r.unwrap().outcome, Outcome::Complete);
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(6));
}

// ---- helpers for the cases below -------------------------------------------------------

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn archive_id(s: &SimStorage) -> ArchiveId {
    history(s)[0].commit.archive_id
}

/// The descriptor frame's length: it fills `[0, len)`.
fn wipe_descriptor(s: &SimStorage) -> SimStorage {
    let len = history(s)[0].commit.descriptor.stored_len as usize;
    damaged(s, |b| b[..len].fill(0))
}

fn profile_of(out: &SimStorage, read: &ReadOptions) -> Profile {
    mochi_core::publish::open_head(out, read)
        .unwrap()
        .descriptor
        .profile()
}

/// Destroy the parity frames of commit 0 as well as the descriptor, so that
/// nothing can rebuild the descriptor (a Redundancy archive heals it
/// otherwise).
fn wipe_parity_of_first_commit(s: &SimStorage) -> SimStorage {
    let h = history(s);
    let r = mochi_core::storage::StorageReader::new(s).unwrap();
    let commit_len = mochi_format::footer::parse_footer_frame(&r, h[0].footer_offset)
        .unwrap()
        .commit_len;
    let (from, to) = (h[0].commit_offset + commit_len, h[0].footer_offset);
    assert!(to > from, "the first commit carries parity frames");
    damaged(s, |b| b[from as usize..to as usize].fill(0xEE))
}

/// Three commits (`a`, `b`, `c`) under `profile`, each file `len` bytes in
/// `chunk`-byte chunks.
fn build_profile(profile: Profile, len: usize, chunk: u64) -> SimStorage {
    build_with(
        1,
        &THREE,
        len,
        WriterOptions {
            profile: Some(profile),
            chunk_size: Some(chunk),
            ..test_options()
        },
        CheckpointPolicy::Never,
    )
}

// ---- 2 ---------------------------------------------------------------------

/// **Descriptor destroyed, nothing to restore it** (no parity, no copy): the
/// footers are intact, so the head is trusted, but no catalog opens. Salvage
/// re-encodes the descriptor from the archive ID and the profile choices and
/// accepts the one whose hash equals what the commits record. Everything is
/// then recovered, labelled `salvage`, and the plan is partial. A TAR-compatible
/// source is listed as having lost that constraint, and the new archive is
/// not TAR-compatible. A Redundancy archive keeps its percentage.
#[test]
fn salvage_2_a_destroyed_descriptor_is_re_encoded_by_its_hash() {
    let core = build_profile(Profile::default(), 150, 64);
    let tar = build_profile(
        Profile {
            tar_compatible: true,
            ..Profile::default()
        },
        150,
        64,
    );
    let redundant = build_profile(
        Profile {
            redundancy: Some(20),
            ..Profile::default()
        },
        2000,
        512,
    );
    for (name, pristine, percent, tar_lost) in [
        ("core", &core, None, false),
        ("tar", &tar, None, true),
        ("redundancy", &redundant, Some(20u16), false),
    ] {
        let bad = if percent.is_some() {
            wipe_descriptor(&wipe_parity_of_first_commit(pristine))
        } else {
            wipe_descriptor(pristine)
        };
        // Control: nothing without --salvage, and the head is found.
        let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
        assert_eq!(plain.outcome, Outcome::NothingRecoverable, "{name}");
        assert!(plain.head.is_some(), "{name}");
        assert!(plain.ladder[6].detail.contains("--salvage"), "{name}");

        let p = plan_s(&bad);
        assert_eq!(p.outcome, Outcome::Partial, "{name}: {p:#?}");
        assert_eq!(step7(&p), StepStatus::Used, "{name}");
        assert_eq!(recovered(&p), all(3), "{name}");
        assert!(
            p.snapshots.iter().all(|s| s.trust == Trust::Salvage),
            "{name}"
        );
        let inf = p
            .salvage
            .as_ref()
            .unwrap()
            .profile_inference
            .clone()
            .unwrap();
        assert!(inf.descriptor_reconstructed, "{name}: {inf:?}");
        assert!(!inf.encrypted, "{name}");
        assert_eq!(inf.redundancy_percent, percent, "{name}");
        assert_eq!(
            inf.profile_constraints_lost,
            if tar_lost {
                vec!["tar_compatible"]
            } else {
                vec![]
            },
            "{name}"
        );
        assert!(
            inf.writer_parameters
                .starts_with("recorded in a catalog image"),
            "{name}: {}",
            inf.writer_parameters
        );
        assert_eq!(p.salvage.as_ref().unwrap().archive_id_source, "commits");

        // The head is trusted, so its retention carries over without a waiver.
        let (dir, r) = run(&bad, &salvage(), &p, false);
        let report = r.unwrap();
        assert_eq!(report.outcome, Outcome::Partial, "{name}");
        assert_eq!(
            report.profile_constraints_lost, inf.profile_constraints_lost,
            "{name}"
        );
        let out = dir.file(OUT).unwrap();
        assert_repairs(pristine, &out, &all(3));
        let got = profile_of(&out, &opts());
        assert!(!got.tar_compatible, "{name}: TAR is not carried (D12)");
        assert_eq!(got.redundancy, percent, "{name}");
    }
}

/// An Encrypted archive's descriptor is re-encoded too (the Encrypted profile
/// follows required feature 1 in the commits), but its commits cannot be read
/// without the passphrase: a missing one is `KEY_UNAVAILABLE`, never an empty
/// result.
#[test]
fn salvage_2b_an_encrypted_archive_needs_its_passphrase() {
    let read = mochi_testkit::archive::keyed_read(&["pw"]);
    let pristine = build_with(
        5,
        &THREE,
        150,
        mochi_testkit::archive::encrypted_options(&["pw"]),
        CheckpointPolicy::Never,
    );
    let bad = wipe_descriptor(&pristine);
    let keyless = plan_o(&bad, &salvage(), &opts());
    assert_eq!(keyless.unwrap_err().code, ErrorCode::KeyUnavailable);

    let p = plan_o(&bad, &salvage(), &read).unwrap();
    assert_eq!(p.outcome, Outcome::Partial, "{p:#?}");
    assert_eq!(recovered(&p), all(3));
    let inf = p
        .salvage
        .as_ref()
        .unwrap()
        .profile_inference
        .clone()
        .unwrap();
    assert!(inf.descriptor_reconstructed && inf.encrypted);
    let mut dir = SimDir::new();
    let report = apply_with_salvage(
        &bad,
        &[],
        Some(&salvage()),
        &p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &read,
        &RepairOptions {
            kdf: Some(mochi_testkit::archive::TEST_KDF),
            ..RepairOptions::default()
        },
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(report.outcome, Outcome::Partial);
    let out = dir.file(OUT).unwrap();
    assert!(profile_of(&out, &read).encrypted);
    let source = opened_all_with(&pristine, &read);
    let new = opened_all_with(&out, &read);
    assert_eq!(new.len(), 3);
    for i in 0..3 {
        assert_eq!(
            read_state(&out, &new[i]).unwrap(),
            read_state(&pristine, &source[i]).unwrap()
        );
    }
}

fn opened_all_with(s: &SimStorage, read: &ReadOptions) -> Vec<OpenedHead> {
    commit_history(s, read)
        .unwrap()
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, read).unwrap())
        .collect()
}

// ---- 3 ---------------------------------------------------------------------

/// **A middle commit frame destroyed, and the later catalogs damaged**: the
/// chain from the head breaks at commit 3 and nothing above it opens. The
/// older segment (commits 0–2, which still have their footers) is a segment
/// of its own, opened like a head: its catalog, a checkpoint, recovers
/// snapshots 0–2 from its own image. The lost ones are listed.
#[test]
fn salvage_3_the_older_segment_is_recovered_from_its_own_catalogs() {
    let pristine = build_with(1, &SIX, 150, test_options(), CheckpointPolicy::EveryCommit);
    let h = history(&pristine);
    let bad = damaged(&pristine, |b| {
        flip(b, h[3].commit_offset + 20);
        for e in [&h[4], &h[5]] {
            let (image, snapshot) = mochi_testkit::replay::checkpoint_refs(e);
            mochi_testkit::replay::damage(b, image);
            mochi_testkit::replay::damage(b, snapshot);
        }
    });
    let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(plain.outcome, Outcome::NothingRecoverable, "{plain:#?}");
    assert!(plain.chain_break.is_some());

    let p = plan_s(&bad);
    assert_eq!(p.outcome, Outcome::Partial, "{p:#?}");
    assert_eq!(step7(&p), StepStatus::Used);
    assert_eq!(recovered(&p), all(3));
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Salvage));
    let lost: Vec<u64> = p.lost_snapshots.iter().map(|l| l.seq).collect();
    assert_eq!(lost, [3, 4, 5]);
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.segments.len(), 2, "{sv:#?}");
    assert_eq!(sv.segments[1].top_seq, 2, "the older segment is opened too");
    assert_eq!(sv.segments[1].bottom_seq, 0);
    assert_eq!((sv.segments[0].top_seq, sv.segments[0].bottom_seq), (5, 4));
    // The head is trusted, but it cannot be opened: retention is unresolved.
    let (_, refused) = run(&bad, &salvage(), &p, false);
    assert_eq!(refused.unwrap_err().code, ErrorCode::RetentionUnresolved);
    let (dir, r) = run(&bad, &salvage(), &p, true);
    r.unwrap();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(3));
}

// ---- 4 ---------------------------------------------------------------------

/// **An archive stored inside the archive.** The outer archive's bytes are
/// followed by a byte copy of an earlier state of itself, behind a header
/// that no longer walks (the data frame that held the copy had its header
/// destroyed), so the resynchronization scans *inside* the copy. Its commits,
/// footers, and data frames are all found, and all carry the offsets of the
/// original, not of where they lie: nothing is recovered from the copy, the
/// copy's commits are rejected, and the outer archive's snapshots are
/// unchanged and still trusted.
#[test]
fn salvage_4_an_archive_inside_the_archive_is_not_the_archive() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    // The copy is a byte copy of the state through commit 3, cut inside its
    // last footer (a copy that ended in a whole footer would make the file
    // end in a footer that validates: that is freshness, spec §5.7).
    let copy_len = (h[3].footer_offset + FOOTER_FRAME_LEN - 20) as usize;
    let copy = pristine.contents()[..copy_len].to_vec();
    let mut b = pristine.contents();
    // Break the outer head's footer so the file does not end in a footer.
    flip(&mut b, h[5].footer_offset + 30);
    let outer_len = b.len();
    b.extend_from_slice(&[0u8; 8]); // the destroyed frame header
    let copy_at = b.len() as u64;
    b.extend_from_slice(&copy);
    let bad = SimStorage::from_bytes(b);

    let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(
        recovered(&plain),
        all(5),
        "the outer chain, through commit 4"
    );
    let p = plan_s(&bad);
    assert_eq!(p.outcome, Outcome::Partial);
    // Commit 5 is the unconfirmed head of the outer archive; the copy adds
    // nothing.
    assert_eq!(recovered(&p), all(5));
    assert!(
        p.snapshots.iter().all(|s| s.trust == Trust::Chain),
        "{p:#?}"
    );
    let sv = p.salvage.as_ref().unwrap();
    assert!(sv.resyncs >= 1);
    // Every commit of the copy was found, none confirmed.
    assert!(sv.confirmed.iter().all(|c| c.offset < outer_len as u64));
    let in_copy = |o: u64| o >= copy_at;
    let listed: Vec<u64> = sv
        .rejected
        .iter()
        .chain(&sv.unconfirmed)
        .filter(|c| in_copy(c.offset))
        .map(|c| c.seq)
        .collect();
    assert_eq!(
        listed.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([0, 1, 2, 3]),
        "{sv:#?}"
    );
    // Commits 0-2 of the copy are named by the copy's own children with the
    // original footer offsets, which they do not follow: rejected.
    assert!(sv.rejected_count >= 3, "{sv:#?}");
    assert!(sv
        .unconfirmed
        .iter()
        .any(|c| c.seq == 5 && c.offset == h[5].commit_offset));
    // The copy's data frames are orphans (nothing recovered references them).
    assert!(sv.orphan_data_objects > 0);
    let (dir, r) = run(&bad, &salvage(), &p, false);
    r.unwrap();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(5));
}

// ---- 5 ---------------------------------------------------------------------

/// **Two archives, one after the other.** With the first archive's
/// descriptor intact the second's commits are foreign and ignored. With the
/// descriptor destroyed nothing says which archive is meant:
/// `ARCHIVE_ID_AMBIGUOUS`, naming both. `--archive-id` picks the first (and
/// recovers it, re-encoding its descriptor); picking the second recovers
/// nothing, because its offsets are those of a file that begins where it does
/// not.
#[test]
fn salvage_5_two_archives_are_ambiguous_until_one_is_chosen() {
    let a = build(&THREE);
    let b2 = build_with(
        40,
        &[("x", 7), ("y", 8)],
        150,
        test_options(),
        CheckpointPolicy::Never,
    );
    let id_a = archive_id(&a);
    let id_b = archive_id(&b2);
    assert_ne!(id_a, id_b);
    let mut bytes = a.contents();
    bytes.extend_from_slice(&b2.contents());
    let both = SimStorage::from_bytes(bytes);

    // Descriptor intact: B is foreign.
    let p = plan_s(&both);
    assert_eq!(recovered(&p), all(3), "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.archive_id_source, "descriptor");
    assert_eq!(sv.foreign_commits, 2);
    assert_eq!(sv.archive_id.as_deref(), Some(id_a.to_hex().as_str()));
    assert_eq!(p.outcome, Outcome::Partial, "the tail is not clean");

    // Descriptor destroyed.
    let bad = wipe_descriptor(&both);
    let e = plan_o(&bad, &salvage(), &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::ArchiveIdAmbiguous);
    assert!(e.message.contains(&id_a.to_hex()), "{}", e.message);
    assert!(e.message.contains(&id_b.to_hex()), "{}", e.message);
    assert_eq!(
        mochi_core::verify::classify(e.code),
        mochi_core::verify::ErrorClass::Operational
    );

    let pick_a = SalvageOptions {
        archive_id: Some(id_a),
    };
    let p = plan_o(&bad, &pick_a, &opts()).unwrap();
    assert_eq!(recovered(&p), all(3), "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.archive_id_source, "option");
    assert_eq!(sv.foreign_commits, 2);
    assert!(
        sv.profile_inference
            .as_ref()
            .unwrap()
            .descriptor_reconstructed
    );
    let (dir, r) = run(&bad, &pick_a, &p, true);
    r.unwrap();
    assert_repairs(&a, &dir.file(OUT).unwrap(), &all(3));
    // The plan is bound to the choice.
    let (_, wrong) = run(&bad, &salvage(), &p, true);
    assert_eq!(wrong.unwrap_err().code, ErrorCode::ArchiveIdAmbiguous);

    let pick_b = SalvageOptions {
        archive_id: Some(id_b),
    };
    let p = plan_o(&bad, &pick_b, &opts()).unwrap();
    assert_eq!(p.outcome, Outcome::NothingRecoverable, "{p:#?}");
    assert!(p.salvage.as_ref().unwrap().rejected_count > 0);

    // A request that disagrees with a readable descriptor is refused.
    let e = plan_o(
        &both,
        &SalvageOptions {
            archive_id: Some(id_b),
        },
        &opts(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

// ---- 6 ---------------------------------------------------------------------

/// **An interrupted publication**: the commit frame was written and the
/// footer was not (or only part of it). Nothing confirms the commit, so it
/// is never recovered (§5.1); it is listed with its sequence, ID, and offset.
#[test]
fn salvage_6_an_interrupted_publication_is_never_recovered() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    for cut in [
        h[5].footer_offset,      // no footer at all
        h[5].footer_offset + 30, // a footer cut short
        h[5].footer_offset + 71, // one byte short
    ] {
        let prefix = SimStorage::from_bytes(pristine.contents()[..cut as usize].to_vec());
        let p = plan_s(&prefix);
        assert_eq!(recovered(&p), all(5), "cut at {cut}");
        assert_eq!(p.outcome, Outcome::Partial, "cut at {cut}");
        assert!(p.snapshots.iter().all(|s| s.trust == Trust::Chain));
        assert_eq!(step7(&p), StepStatus::NotAvailable, "cut at {cut}");
        let sv = p.salvage.as_ref().unwrap();
        assert_eq!(sv.unconfirmed.len(), 1);
        let u = &sv.unconfirmed[0];
        assert_eq!(
            (u.seq, u.commit_id.as_str(), u.offset),
            (
                5,
                hex(h[5].commit_id.as_bytes()).as_str(),
                h[5].commit_offset
            )
        );
        // The same plan, with and without --salvage, recovers the same.
        let plain = mochi_core::repair::plan(&prefix, &opts(), &Job::new().ctx()).unwrap();
        assert_eq!(commit_ids(&plain), commit_ids(&p));
        // Its data objects are counted, never placed in a namespace.
        assert!(sv.orphan_data_objects > 0 && sv.orphan_bytes > 0);
        let (dir, r) = run(&prefix, &salvage(), &p, false);
        r.unwrap();
        assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(5));
    }
}

// ---- 7 ---------------------------------------------------------------------

/// **Contradictory commits at one sequence**: a forged commit 1 with another
/// transaction ID (so another commit ID) follows an archive whose commit 1
/// and 2 have lost their footers. Neither commit 1 is used, so nothing above
/// commit 0 is recovered, and both are listed.
#[test]
fn salvage_7_contradictory_commits_recover_neither() {
    let pristine = build(&THREE);
    let broken = break_footers(&pristine, &[1, 2]);
    let h = history(&pristine);
    let mut forge = mochi_testkit::forge::Forge::new(broken.contents());
    let mut record = h[1].commit.clone();
    record.transaction_id = mochi_testkit::forge::txid(7);
    let forged = forge.append_commit(&record);
    assert_ne!(forged.commit_id, h[1].commit_id);
    // The forged commit has no footer either.
    flip(&mut forge.bytes, forged.footer_offset + 30);
    let bad = forge.storage();

    let p = plan_s(&bad);
    assert_eq!(recovered(&p), [0], "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.contradictory.len(), 1);
    let c = &sv.contradictory[0];
    assert_eq!(c.seq, 1);
    let ids: BTreeSet<&str> = c.commits.iter().map(|x| x.commit_id.as_str()).collect();
    assert_eq!(
        ids,
        BTreeSet::from([
            hex(h[1].commit_id.as_bytes()).as_str(),
            hex(forged.commit_id.as_bytes()).as_str()
        ])
    );
    // Commit 2 names commit 1 and so confirms nothing; it is unconfirmed.
    assert!(sv.confirmed.iter().all(|c| c.seq == 0));
    assert!(sv.unconfirmed.iter().any(|u| u.seq == 2));
    assert!(sv.segments.iter().all(|s| s.top_seq == 0));
}

// ---- 8 ---------------------------------------------------------------------

fn limited(n: u64) -> ReadOptions {
    let mut o = opts();
    o.limits.max_salvage_candidates = n;
    o
}

/// **The candidate limit.** Set low, the scan stops, step 7 is `incomplete`
/// (never a pass), the plan is partial, and the limit in force is reported by
/// `verify` as `LIMIT_NOT_DEFAULT`. One candidate more than it needs, it
/// finishes; exactly the number it needs, it finishes; one fewer, it stops.
#[test]
fn salvage_8_the_candidate_limit_stops_the_scan_and_says_so() {
    let pristine = build(&SIX);
    let bad = break_footers(&pristine, &[3, 4, 5]);
    let full = plan_o(&bad, &salvage(), &opts()).unwrap();
    assert_eq!(step7(&full), StepStatus::Used);
    assert_eq!(recovered(&full), all(5));
    let needed = full.salvage.as_ref().unwrap().candidates;
    assert!(needed > 7, "{needed}");

    for (limit, reached) in [(needed + 1, false), (needed, false), (needed - 1, true)] {
        let p = plan_o(&bad, &salvage(), &limited(limit)).unwrap();
        let sv = p.salvage.as_ref().unwrap();
        assert_eq!(sv.limit_reached, reached, "limit {limit}");
        assert_eq!(sv.candidate_limit, limit);
        assert_eq!(
            step7(&p),
            if reached {
                StepStatus::Incomplete
            } else {
                StepStatus::Used
            },
            "limit {limit}"
        );
    }

    // Stopped early: only part of the file was examined; the trusted chain
    // (commits 0-2) is intact, the rest is not recovered, and the outcome is
    // partial.
    let p = plan_o(&bad, &salvage(), &limited(7)).unwrap();
    let sv = p.salvage.as_ref().unwrap();
    assert!(sv.limit_reached && sv.candidates == 7);
    assert_eq!(step7(&p), StepStatus::Incomplete);
    assert!(p.ladder[6].detail.contains("max-salvage-candidates=7"));
    assert_eq!(recovered(&p), all(3));
    assert_eq!(p.outcome, Outcome::Partial);
    assert_eq!(p.exit_code, 2);
    // A scan that stopped at the limit is never a pass, however much it kept.
    let (dir, r) = run_with(&bad, &salvage(), &p, false, &limited(7));
    assert_eq!(r.unwrap().outcome, Outcome::Partial);
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(3));

    let v = mochi_core::verify::verify(
        &bad,
        &mochi_core::verify::VerifyOptions {
            read: limited(7),
            ..Default::default()
        },
        &Job::new().ctx(),
    );
    let f = v
        .report
        .findings
        .iter()
        .find(|f| f.code == ErrorCode::LimitNotDefault)
        .expect("LIMIT_NOT_DEFAULT");
    assert_eq!(f.observed.as_deref(), Some("7"));
    assert_eq!(f.expected.as_deref(), Some("1048576"));
    assert!(f
        .message
        .as_deref()
        .unwrap()
        .contains("max-salvage-candidates"));
}

// ---- 9 ---------------------------------------------------------------------

/// **Truncation at every byte** of a small three-commit fixture, salvaged
/// (§5.1 under salvage; the property behind the whole step): at *every*
/// length the recovered snapshots are exactly the commits whose footer is
/// whole, each with the pristine commit's ID, no snapshot is ever one that
/// was not committed in the prefix, and no unconfirmed commit is recovered.
/// The repair of a sample of lengths (every footer boundary, and a stride)
/// is written, verified, and compared with the pristine snapshots.
#[test]
fn salvage_9_truncation_at_every_byte_never_recovers_an_uncommitted_commit() {
    let pristine = build_with(
        1,
        &[("a", 1), ("b", 2), ("c", 3)],
        100,
        WriterOptions {
            chunk_size: Some(128),
            ..test_options()
        },
        CheckpointPolicy::Never,
    );
    let bytes = pristine.contents();
    let h = history(&pristine);
    let ends: Vec<u64> = h
        .iter()
        .map(|e| e.footer_offset + FOOTER_FRAME_LEN)
        .collect();
    let opened = opened_all(&pristine);
    let entries: Vec<usize> = opened
        .iter()
        .map(|o| read_state(&pristine, o).unwrap().len())
        .collect();
    assert_eq!(entries, [1, 2, 3]);
    let total = bytes.len() as u64;
    assert_eq!(*ends.last().unwrap(), total);

    let sampled = |l: u64| {
        ends.iter().any(|e| l.abs_diff(*e) <= 2)
            || h.iter().any(|e| l.abs_diff(e.commit_offset) <= 2)
            || l.is_multiple_of(997)
    };
    let check = |l: u64| -> bool {
        let prefix = SimStorage::from_bytes(bytes[..l as usize].to_vec());
        let p = plan_s(&prefix);
        let k = ends.iter().rposition(|e| *e <= l);
        let want: Vec<u64> = k.map_or(Vec::new(), |k| all(k as u64 + 1));
        assert_eq!(recovered(&p), want, "length {l}");
        // Never an uncommitted commit, never anything but the trusted chain
        // (a truncated file has no footer salvage could add).
        for s in &p.snapshots {
            assert_eq!(s.trust, Trust::Chain, "length {l}");
            assert_eq!(
                s.commit_id.as_deref(),
                Some(hex(h[s.seq as usize].commit_id.as_bytes()).as_str()),
                "length {l}: snapshot {} is the pristine commit",
                s.seq
            );
            assert_eq!(s.entries as usize, entries[s.seq as usize], "length {l}");
            assert_eq!(s.omitted, 0, "length {l}");
        }
        if let Some(sv) = &p.salvage {
            let recovered_ids: BTreeSet<&str> = p
                .snapshots
                .iter()
                .filter_map(|s| s.commit_id.as_deref())
                .collect();
            for u in &sv.unconfirmed {
                assert!(
                    !recovered_ids.contains(u.commit_id.as_str()),
                    "length {l}: unconfirmed commit {} was recovered",
                    u.seq
                );
            }
        }
        match k {
            Some(_) if ends.contains(&l) => {
                // Exactly at a footer: a clean archive of k + 1 commits.
                assert_eq!(p.outcome, Outcome::Complete, "length {l}");
                assert_eq!(step7(&p), StepStatus::NotNeeded, "length {l}");
            }
            Some(_) => assert_eq!(p.outcome, Outcome::Partial, "length {l}"),
            None => assert_eq!(p.outcome, Outcome::NothingRecoverable, "length {l}"),
        }
        if sampled(l) && k.is_some() {
            let (dir, r) = run(&prefix, &salvage(), &p, false);
            let report = r.unwrap();
            assert_eq!(report.commits.len(), want.len(), "length {l}");
            assert_repairs(&pristine, &dir.file(OUT).unwrap(), &want);
            return true;
        }
        false
    };
    // Every length, spread over the cores (the SQLite opens dominate).
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let repaired: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads as u64)
            .map(|t| {
                let check = &check;
                scope.spawn(move || {
                    (0..=total)
                        .filter(|l| l % threads as u64 == t)
                        .filter(|l| check(*l))
                        .count()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert!(repaired > 10, "{repaired}");
}

// ---- 11 --------------------------------------------------------------------

struct CancelAt {
    token: CancellationToken,
    after: usize,
    seen: std::sync::atomic::AtomicUsize,
    scan_reports: std::sync::atomic::AtomicUsize,
}

impl ProgressSink for CancelAt {
    fn report(&self, e: &ProgressEvent) {
        if e.phase == "repair-salvage" {
            let n = self
                .scan_reports
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if n >= self.after {
                self.token.cancel();
            }
        }
        self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// **Cancellation mid-scan**: the scan checks its token once per MiB it
/// reads, so a job cancelled in the middle of a multi-MiB file stops there
/// with `CANCELLED` and no plan; nothing is written.
#[test]
fn salvage_11_cancellation_stops_the_scan() {
    let pristine = build_with(
        1,
        &[("big", 5)],
        6 << 20,
        WriterOptions {
            chunk_size: Some(1 << 20),
            ..test_options()
        },
        CheckpointPolicy::Never,
    );
    let bad = break_footers(&pristine, &[0]);
    // Control: it runs to the end.
    let p = plan_s(&bad);
    assert_eq!(step7(&p), StepStatus::NotAvailable, "{:#?}", p.ladder);
    // Cancelled at the second progress report of the scan phase.
    let token = CancellationToken::new();
    let sink = CancelAt {
        token: token.clone(),
        after: 2,
        seen: Default::default(),
        scan_reports: Default::default(),
    };
    let ctx = JobContext {
        progress: &sink,
        cancel: &token,
    };
    let e = plan_with_salvage(&bad, &[], Some(&salvage()), &opts(), &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    // It stopped in the scan, not after it: well short of the whole file.
    let scanned = sink.scan_reports.load(std::sync::atomic::Ordering::SeqCst);
    assert!((2..=4).contains(&scanned), "{scanned}");
}

// ---- more ------------------------------------------------------------------

/// **Resynchronization**: a data frame whose header no longer walks, in the
/// middle of an archive whose last footer is also damaged. The forward walk
/// stops there (so the trusted chain ends before it); the scan resynchronizes
/// at the next frame, confirms the later commits by their own footers, and
/// recovers them. The version whose chunk lost its header is left out, and
/// the snapshots the trusted chain recovered keep their trust label.
#[test]
fn salvage_12_a_destroyed_frame_header_is_resynchronized_past() {
    let pristine = build(&SIX);
    // The first chunk of "c" (added by commit 2).
    let h = opened_all(&pristine);
    let c = {
        let snap = h[2].catalog.replay(None).unwrap();
        let e = snap.get(&path("c")).unwrap();
        let (_, extents) = h[2].catalog.file_version(&e.version).unwrap().unwrap();
        let chunk = extents
            .iter()
            .find_map(|x| match x.source {
                mochi_core::catalog::extent::ExtentSource::Chunk { chunk, .. } => Some(chunk),
                _ => None,
            })
            .unwrap();
        h[2].catalog.object_location(&chunk).unwrap().unwrap()
    };
    let hist = history(&pristine);
    let bad = damaged(&pristine, |b| {
        b[c as usize..c as usize + 8].fill(0);
        flip(b, hist[5].footer_offset + 30);
    });
    let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(recovered(&plain), [0, 1], "{plain:#?}");

    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(5), "{p:#?}");
    let trust: Vec<Trust> = p.snapshots.iter().map(|s| s.trust).collect();
    assert_eq!(
        trust,
        [
            Trust::Chain,
            Trust::Chain,
            Trust::Salvage,
            Trust::Salvage,
            Trust::Salvage
        ]
    );
    let sv = p.salvage.as_ref().unwrap();
    assert!(sv.resyncs >= 1);
    assert_eq!(sv.unconfirmed.len(), 1);
    assert_eq!(sv.unconfirmed[0].seq, 5);
    assert_eq!(p.outcome, Outcome::Partial);
    // "c" lost its first chunk: it is left out of snapshots 2-4, nothing else.
    assert_eq!(p.omitted.len(), 1);
    assert_eq!(p.omitted[0].path, "c");
    assert_eq!(p.omitted[0].snapshots, [2, 3, 4]);
    let (dir, r) = run(&bad, &salvage(), &p, false);
    r.unwrap();
    let out = dir.file(OUT).unwrap();
    let new = opened_all(&out);
    assert_eq!(new.len(), 5);
    for (i, o) in new.iter().enumerate() {
        let mut want = read_state(&pristine, &h[i]).unwrap();
        want.remove(&b"c"[..]);
        assert_eq!(read_state(&out, o).unwrap(), want, "snapshot {i}");
    }
}

/// A Redundancy archive keeps its parity frames between the commit and the
/// footer, so the regenerated footer does not follow the commit directly.
#[test]
fn salvage_13_footers_are_regenerated_after_parity_frames() {
    let pristine = build_profile(
        Profile {
            redundancy: Some(20),
            ..Profile::default()
        },
        2000,
        512,
    );
    let bad = break_footers(&pristine, &[0, 1, 2]);
    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(2), "{p:#?}");
    assert_eq!(p.salvage.as_ref().unwrap().footers_regenerated.len(), 2);
    let (dir, r) = run(&bad, &salvage(), &p, true);
    r.unwrap();
    let out = dir.file(OUT).unwrap();
    assert_repairs(&pristine, &out, &all(2));
    assert_eq!(profile_of(&out, &opts()).redundancy, Some(20));
}

/// A plan is bound to the options it was made with: it is refused when
/// applied with different salvage options, and a plan made without salvage
/// is not the plan salvage makes.
#[test]
fn salvage_14_apply_refuses_a_plan_made_with_other_options() {
    let pristine = build(&SIX);
    let bad = break_footers(&pristine, &[0, 1, 2, 3, 4, 5]);
    let with = plan_s(&bad);
    let without = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_ne!(with, without);
    // Applying the salvage plan without --salvage re-plans to something else.
    let mut dir = SimDir::new();
    let e = mochi_core::repair::apply(
        &bad,
        &with,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions {
            accept_retention_loss: true,
            ..RepairOptions::default()
        },
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(dir.file(OUT).is_none());
    // And the other way round: nothing is recoverable without salvage.
    let (dir, r) = run(&bad, &salvage(), &without, true);
    assert_eq!(r.unwrap_err().code, ErrorCode::InvalidArgument);
    assert!(dir.file(OUT).is_none());
    // The plan survives a round trip through JSON, as `apply` reads it.
    let json = serde_json::to_string(&with).unwrap();
    assert_eq!(serde_json::from_str::<RepairPlan>(&json).unwrap(), with);
    assert!(json.contains("\"trust\":\"salvage\""));
    assert!(json.contains("\"status\":\"used\""));
}

/// The orphans of the unconfirmed head are counted exactly, from the
/// pristine archive's own catalog, and appear in no recovered namespace.
#[test]
fn salvage_15_orphan_data_objects_are_counted_and_never_placed() {
    let pristine = build(&SIX);
    let bad = break_footers(&pristine, &[5]);
    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(5), "{p:#?}");
    let o = opened_all(&pristine);
    let head = &o[5];
    let snap = head.catalog.replay(None).unwrap();
    let e = snap.get(&path("f")).unwrap();
    let (_, extents) = head.catalog.file_version(&e.version).unwrap().unwrap();
    let (mut count, mut bytes) = (0u64, 0u64);
    for x in &extents {
        if let mochi_core::catalog::extent::ExtentSource::Chunk { chunk, .. } = x.source {
            count += 1;
            bytes += head.catalog.object(&chunk).unwrap().unwrap().stored_len;
        }
    }
    assert!(count >= 3);
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!((sv.orphan_data_objects, sv.orphan_bytes), (count, bytes));
    let (dir, r) = run(&bad, &salvage(), &p, false);
    r.unwrap();
    let out = dir.file(OUT).unwrap();
    let last = opened_all(&out).pop().unwrap();
    assert!(!read_state(&out, &last).unwrap().contains_key(&b"f"[..]));
}

// ---- fuzz smoke ------------------------------------------------------------------------

/// Stable-Rust stand-in for the `salvage_scan` fuzz target (nightly
/// libFuzzer runs it in CI): the exerciser's invariants hold over a real
/// archive, every prefix on a stride, and one-byte, run, and splice mutations
/// of it, with and without damage to the footers.
#[test]
fn salvage_fuzz_smoke_over_mutated_archives() {
    use mochi_testkit::fuzz::exercise_salvage_scan;
    let pristine = build(&THREE);
    let bytes = pristine.contents();
    exercise_salvage_scan(&bytes);
    exercise_salvage_scan(&break_footers(&pristine, &[0, 1, 2]).contents());
    exercise_salvage_scan(&wipe_descriptor(&pristine).contents());
    for l in (0..bytes.len()).step_by(211) {
        exercise_salvage_scan(&bytes[..l]);
    }
    let mut x = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for round in 0..120 {
        let mut b = bytes.clone();
        match round % 4 {
            0 => {
                let at = (next() as usize) % b.len();
                b[at] ^= 1 << (next() % 8);
            }
            1 => {
                let at = (next() as usize) % b.len();
                let n = ((next() % 64) as usize).min(b.len() - at);
                b[at..at + n].fill(0);
            }
            2 => {
                // Splice a copy of an earlier stretch over a later one.
                let n = 1 + (next() as usize) % 2048;
                let from = (next() as usize) % b.len().saturating_sub(n).max(1);
                let to = (next() as usize) % b.len().saturating_sub(n).max(1);
                let piece = b[from..(from + n).min(b.len())].to_vec();
                b[to..to + piece.len()].copy_from_slice(&piece);
            }
            _ => {
                // Two copies end to end.
                let copy = b.clone();
                b.extend_from_slice(&copy);
            }
        }
        exercise_salvage_scan(&b);
    }
    // The golden vectors the archive_open target is seeded with.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c5");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten().take(12) {
            if e.path().extension().is_some_and(|x| x == "mochi") {
                let b = std::fs::read(e.path()).unwrap();
                exercise_salvage_scan(&b);
                if b.len() > 200 {
                    let mut c = b.clone();
                    let at = c.len() - 40;
                    c[at] ^= 0x40;
                    exercise_salvage_scan(&c);
                }
            }
        }
    }
}

// ---- more cases ------------------------------------------------------------------------

/// A valid Zstandard frame holding `payload` in one raw block (what zstd
/// writes for incompressible input), without checksum.
fn raw_zstd_frame(payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() < 1 << 17);
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD];
    // Single segment, 4-byte content size, no checksum, no dictionary.
    f.push(0b1010_0000);
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    // Last block, raw, size.
    let header = 1u32 | ((payload.len() as u32) << 3);
    f.extend_from_slice(&header.to_le_bytes()[..3]);
    f.extend_from_slice(payload);
    f
}

/// **An archive stored inside a data frame that still walks** is never
/// scanned: the walk does not resynchronize inside a frame that walked
/// validly (D23 item 2). The copy's commits, footers, and frames are not
/// even kept, so nothing about it is reported and the outer snapshots are
/// the trusted chain's.
#[test]
fn salvage_4b_a_valid_frame_holding_an_archive_is_not_scanned_inside() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    // Cut inside the last footer: a copy ending in a whole footer would make
    // the file end in one that validates (freshness, spec §5.7).
    let copy = pristine.contents()[..(h[3].footer_offset + 20) as usize].to_vec();
    let mut b = pristine.contents();
    flip(&mut b, h[5].footer_offset + 30);
    b.extend_from_slice(&raw_zstd_frame(&copy));
    let bad = SimStorage::from_bytes(b);
    // The frame walks as a data frame.
    let r = mochi_core::storage::StorageReader::new(&bad).unwrap();
    let span = mochi_format::frame::walk_frame(
        &r,
        pristine.contents().len() as u64,
        &mochi_format::Limits::default(),
    )
    .unwrap();
    assert_eq!(span.kind, mochi_format::registry::FrameKind::ZstdData);

    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(5), "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.resyncs, 0, "nothing failed to walk");
    assert_eq!(sv.kept_commits, 6, "only the outer archive's commits");
    assert_eq!(sv.kept_footers, 5);
    assert_eq!((sv.rejected_count, sv.foreign_commits), (0, 0));
    assert_eq!(sv.unconfirmed.len(), 1);
    assert_eq!(sv.unconfirmed[0].seq, 5);
    // The frame is one orphan data object, counted whole.
    assert_eq!(
        sv.orphan_data_objects,
        1 + 3,
        "the head's chunks and the frame"
    );
}

/// **One damaged footer in the middle**: the head is trusted, the chain
/// breaks at the damaged footer, and every catalog below the head needs it.
/// The footer is regenerated from the next commit's parent link, everything
/// is recovered, retention carries over without a waiver, and **the plan is
/// still partial**: a repair that used step 7 is never complete (D23 item 9).
#[test]
fn salvage_16_one_damaged_footer_in_the_middle_is_bridged_but_partial() {
    let pristine = build(&SIX);
    let bad = break_footers(&pristine, &[2]);
    let plain = mochi_core::repair::plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(plain.outcome, Outcome::NothingRecoverable, "{plain:#?}");
    assert!(plain.chain_break.is_some());
    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(6), "{p:#?}");
    assert!(p.lost_snapshots.is_empty() && p.omitted.is_empty());
    assert!(
        matches!(
            p.retention,
            Some(mochi_core::repair::RetentionPlan::Carried { .. })
        ),
        "the head is trusted: {:?}",
        p.retention
    );
    assert_eq!(p.tail.as_ref().unwrap().state, "clean");
    assert_eq!(step7(&p), StepStatus::Used);
    assert_eq!(p.outcome, Outcome::Partial, "salvage is always partial");
    assert_eq!(p.exit_code, 2);
    assert!(p.chain_break.is_some(), "the trusted chain's break is kept");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(
        sv.footers_regenerated,
        [history(&pristine)[2].footer_offset]
    );
    let (dir, r) = run(&bad, &salvage(), &p, false);
    assert_eq!(r.unwrap().outcome, Outcome::Partial);
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(6));
}

/// A child's parent link names where the parent's footer is; a footer is
/// regenerated there only if nothing that walked validly lies in the way. A
/// forged child that points into the middle of the parent's own (damaged)
/// footer confirms nothing and changes no byte.
#[test]
fn salvage_17_a_link_into_a_valid_frame_confirms_nothing() {
    let pristine = build(&[("a", 1), ("b", 2)]);
    let broken = break_footers(&pristine, &[1]);
    let h = history(&pristine);
    let mut forge = mochi_testkit::forge::Forge::new(broken.contents());
    // Room between the parent's footer and the forged commit, so that the
    // position check alone cannot refuse the link.
    let filler = mochi_format::frame::encode_skippable_frame(
        mochi_format::registry::FrameKind::RecoveryManifest,
        &[7u8; 100],
    )
    .unwrap();
    forge.append_object(&mochi_format::repr::StoredObject::from_loaded(filler));
    let mut record = h[1].commit.clone();
    record.seq = 2;
    record.transaction_id = mochi_testkit::forge::txid(9);
    record.parent = Some(mochi_core::commit::CommitLink {
        commit_id: h[1].commit_id,
        seq: 1,
        // Inside the parent's footer frame, not at its start.
        footer_offset: h[1].footer_offset + 8,
    });
    let forged = forge.append_commit(&record);
    // The forged commit has no footer either.
    flip(&mut forge.bytes, forged.footer_offset + 30);
    let bad = forge.storage();
    let p = plan_s(&bad);
    assert_eq!(recovered(&p), [0], "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert!(sv.footers_regenerated.is_empty(), "{sv:#?}");
    // Commit 1 is named by a link that cannot be right: rejected; commit 2
    // has no footer and no child: unconfirmed.
    assert!(sv.rejected.iter().any(|u| u.seq == 1), "{sv:#?}");
    assert!(sv.unconfirmed.iter().any(|u| u.seq == 2), "{sv:#?}");
}

/// **The position test is what decides between an archive and a copy of it.**
/// A byte copy of the state through commit 3, cut inside its last footer,
/// follows an archive whose commit 2 lost its footer. Commit 3 names commit 2
/// by ID and by the footer offset of the original; both the real commit 2 and
/// its copy carry that ID, but only the real one ends before that footer. The
/// link resolves to it, its footer is regenerated, and the whole history is
/// recovered. If the copy were as good a parent as the real commit, the link
/// would be ambiguous and commit 2 would stay unconfirmed.
#[test]
fn salvage_18_a_copy_does_not_compete_with_the_real_parent() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    let broken = break_footers(&pristine, &[2]);
    let mut b = broken.contents();
    let copy = b[..(h[3].footer_offset + 20) as usize].to_vec();
    b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&copy);
    let bad = SimStorage::from_bytes(b);
    let p = plan_s(&bad);
    assert_eq!(recovered(&p), all(6), "{p:#?}");
    let sv = p.salvage.as_ref().unwrap();
    assert_eq!(sv.footers_regenerated, [h[2].footer_offset]);
    assert!(sv
        .confirmed
        .iter()
        .all(|c| c.offset < h[5].footer_offset + 72));
    assert!(
        sv.rejected.iter().any(|c| c.seq == 2),
        "the copy's commit 2"
    );
    let (dir, r) = run(&bad, &salvage(), &p, false);
    r.unwrap();
    assert_repairs(&pristine, &dir.file(OUT).unwrap(), &all(6));
}

/// **A walk back from a confirmed commit stays inside what the scan
/// confirmed** (D23 items 5, 11(c); found by the `salvage_scan` fuzz target,
/// CI run 38452009022). In a Redundancy archive whose head footer is broken,
/// the last parity frame before commit 0's footer loses its magic and, four
/// bytes on, a reserved frame begins that walks validly and ends where that
/// footer ends. Commit 0's footer bytes are intact, but they lie inside a
/// frame that walked validly, so commit 1's link does not name commit 0 and
/// commit 0 is rejected. Commit 1 is confirmed by its own footer and its
/// catalog still supplies snapshot 0, as a catalog does below a chain break:
/// without a commit record. Before the fix, the survey re-walked the parent
/// links by footer alone and took commit 0's record (and its manifests) from
/// the commit the scan had rejected.
#[test]
fn salvage_19_a_salvaged_chain_never_walks_past_what_the_scan_confirmed() {
    use mochi_format::frame::walk_frame;
    use mochi_format::registry::{RESERVED_NO_CORE_INTERPRETATION, SKIPPABLE_HEADER_LEN};
    let pristine = build_profile(
        Profile {
            redundancy: Some(50),
            ..Profile::default()
        },
        2000,
        512,
    );
    let h = history(&pristine);
    let limits = opts().limits;
    // The last frame between commit 0's frame and its footer: a parity frame.
    let r = mochi_core::storage::StorageReader::new(&pristine).unwrap();
    let mut at = h[0].commit_offset;
    let mut last = None;
    while at < h[0].footer_offset {
        let span = walk_frame(&r, at, &limits).unwrap();
        last = Some(span);
        at = span.offset + span.len;
    }
    let last = last.unwrap();
    assert_eq!(
        last.kind,
        mochi_format::registry::FrameKind::ParityObject,
        "commit 0 carries parity frames"
    );
    let cover_end = h[0].footer_offset + FOOTER_FRAME_LEN;
    let cover = last.offset + 4;
    let payload = cover_end - cover - SKIPPABLE_HEADER_LEN as u64;
    let bad = SimStorage::from_bytes({
        let mut b = break_footers(&pristine, &[2]).contents();
        let x = last.offset as usize;
        b[x..x + 4].fill(0);
        b[x + 4..x + 8].copy_from_slice(&RESERVED_NO_CORE_INTERPRETATION.to_le_bytes());
        b[x + 8..x + 12].copy_from_slice(&(payload as u32).to_le_bytes());
        b
    });

    let p = plan_s(&bad);
    let sv = p.salvage.as_ref().unwrap();
    let id = |i: usize| hex(h[i].commit_id.as_bytes());
    assert!(p.head.is_none(), "{p:#?}");
    assert!(
        sv.confirmed
            .iter()
            .any(|c| c.seq == 1 && c.commit_id == id(1)),
        "{sv:#?}"
    );
    assert!(
        !sv.confirmed.iter().any(|c| c.seq == 0),
        "commit 0's footer lies inside a frame that walked validly: {sv:#?}"
    );
    assert!(sv.rejected.iter().any(|c| c.seq == 0), "{sv:#?}");
    assert!(sv.unconfirmed.iter().any(|c| c.seq == 2), "{sv:#?}");
    assert_eq!(recovered(&p), all(2), "{p:#?}");
    assert_eq!(
        commit_ids(&p),
        [(0, None), (1, Some(id(1)))],
        "snapshot 0 comes from commit 1's catalog, without commit 0's record"
    );
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Salvage));
    assert_ne!(p.outcome, Outcome::Complete);
    mochi_testkit::fuzz::exercise_salvage_scan(&bad.contents());

    let (dir, r) = run(&bad, &salvage(), &p, true);
    let report = r.unwrap();
    let out = dir.file(OUT).unwrap();
    assert_eq!(opened_all(&out).len(), 2, "{report:#?}");
}

/// **The trusted head's chain, below its break, stays inside what the scan
/// confirmed** (D23 items 5, 11(c); found by the `salvage_scan` fuzz target,
/// CI run 38952433601). The same defect as `salvage_19`, reached from an
/// intact head: the ordinary walk breaks below the head (commit 1's footer is
/// damaged), salvage regenerates that footer, and the walk then went on to
/// commit 0, whose footer lies where the scan refused it (rejected). The
/// snapshot it holds must carry no commit record. Run on the fuzz input
/// (`crash-s1-a`, a Redundancy archive, kept whole: `fuzz tmin` shrinks it
/// by only a few percent), because parity heals a hand-built version of the
/// damage before salvage is needed.
#[test]
fn salvage_20_the_trusted_heads_chain_never_walks_past_what_the_scan_confirmed() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/fuzz-regressions/salvage_scan/crash-s1-a");
    let bad = SimStorage::from_bytes(std::fs::read(path).unwrap());
    let p = plan_s(&bad);
    let sv = p.salvage.as_ref().expect("the scan ran");
    assert_eq!(p.head.as_ref().map(|h| h.seq), Some(2), "{p:#?}");
    assert!(sv.rejected.iter().any(|c| c.seq == 0), "{sv:#?}");
    assert!(!sv.confirmed.iter().any(|c| c.seq == 0), "{sv:#?}");
    let ids = commit_ids(&p);
    assert_eq!(
        ids.iter().find(|(s, _)| *s == 0).unwrap().1,
        None,
        "{ids:?}"
    );
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Salvage));
    assert_ne!(p.outcome, Outcome::Complete);
    mochi_testkit::fuzz::exercise_salvage_scan(&bad.contents());
}

/// **A commit the ordinary walk reached from the trusted head keeps its
/// record when only the descriptor needed the scan** (D23 items 4, 5;
/// found by the `salvage_scan` fuzz target, CI run 42501819636,
/// `crash-0e608cfb…`, kept whole). The ordinary walk reaches commits 2 and 1
/// from the head and breaks below them, but no catalog opens until the scan
/// re-encodes the descriptor. The scan's own frame walk never finds commit
/// 1 (it resyncs past it), so commit 1 is neither confirmed nor listed. The
/// trusted chain from the break up is kept as the ordinary walk found it,
/// so snapshot 1 carries commit 1's record and is labelled salvage because
/// it needed the regenerated descriptor. The exerciser's oracle had allowed
/// only the head itself; it now allows the head's chain down to its break.
#[test]
fn salvage_21_the_trusted_heads_chain_down_to_its_break_keeps_its_records() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../fixtures/fuzz-regressions/salvage_scan/crash-0e608cfbfda3fdf1358c3d1e2a0749b75a9700ce",
    );
    let bad = SimStorage::from_bytes(std::fs::read(path).unwrap());
    let p = plan_s(&bad);
    let sv = p.salvage.as_ref().expect("the scan ran");
    assert_eq!(p.head.as_ref().map(|h| h.seq), Some(2), "{p:#?}");
    assert_eq!(
        p.chain_break.as_ref().map(|b| b.first_trusted_seq),
        Some(1),
        "{p:#?}"
    );
    let profile = sv.profile_inference.as_ref().expect("descriptor evidence");
    assert!(profile.descriptor_reconstructed, "{sv:#?}");
    let listed = |seq: u64| {
        sv.confirmed
            .iter()
            .chain(&sv.unconfirmed)
            .chain(&sv.rejected)
            .any(|c| c.seq == seq)
    };
    assert!(!listed(1), "the scan never saw commit 1: {sv:#?}");
    let ids = commit_ids(&p);
    assert_eq!(ids.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [0, 1, 2]);
    assert!(ids.iter().all(|(_, id)| id.is_some()), "{ids:?}");
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Salvage));
    assert_ne!(p.outcome, Outcome::Complete);
    mochi_testkit::fuzz::exercise_salvage_scan(&bad.contents());
}

/// The input the `salvage_scan` fuzz target found in CI (run 38452009022,
/// `crash-599502c9…`), kept whole: no nightly toolchain was at hand to
/// minimize it. See `salvage_19` for the same case built by hand. The
/// `crash-s1-*` inputs are from CI run 38952433601 (see `salvage_20`), and
/// `crash-0e608cfb…` from CI run 42501819636 (see `salvage_21`).
#[test]
fn salvage_fuzz_regression_inputs() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/fuzz-regressions/salvage_scan");
    let mut n = 0;
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().extension().is_none() {
            mochi_testkit::fuzz::exercise_salvage_scan(&std::fs::read(e.path()).unwrap());
            n += 1;
        }
    }
    assert!(n >= 1);
}
