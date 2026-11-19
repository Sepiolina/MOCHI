//! C8, ladder step 5: independent copies (spec §22; Annex B.2.12 D22;
//! `mochi_core::copies`).
//!
//! The oracle is a pristine copy of the source, read with the test kit's own
//! reassembly (`archive::read_state`), as in `c8_repair.rs`: every repaired
//! commit must hold exactly the pristine snapshot's entries minus the ones
//! the plan lists as omitted, with the same version IDs and attributes. The
//! expected donations are written from the damage placed in the fixture, not
//! from the planner's output.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::copies::{CopyMatch, CopySource, CopyStatus, DonationKind, Trust};
use mochi_core::descriptor::Profile;
use mochi_core::publish::{
    commit_history, open_at_footer, promised_attributes, ArchiveWriter, CheckpointPolicy,
    HistoryEntry, OpenedHead, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::repair::{
    apply, apply_with_copies, plan, plan_with_copies, Outcome, RepairOptions, RepairPlan,
    StepStatus, PLAN_FORMAT,
};
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_format::parity::Layout;
use mochi_testkit::archive::{
    encrypted_options, keyed_read, path, read_state, test_options, Job, State,
};
use mochi_testkit::replay::{attrs, flip, FailAfter, FailingReads};
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const OUT: &str = "repaired.mochi";

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// One commit per `(name, seed)`: the file `name` with 150 bytes derived from
/// `seed`. Commit 0 is the only checkpoint.
fn build_with(
    ids: u64,
    files: &[(&str, u64)],
    len: usize,
    options: WriterOptions,
    read: &ReadOptions,
) -> SimStorage {
    let _ = read;
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(ids)), options).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
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
    build_with(1, files, 150, test_options(), &opts())
}

const SIX: [(&str, u64); 6] = [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5), ("f", 6)];

fn history(s: &SimStorage) -> Vec<HistoryEntry> {
    commit_history(s, &opts()).unwrap()
}

fn opened_all(s: &SimStorage, read: &ReadOptions) -> Vec<OpenedHead> {
    commit_history(s, read)
        .unwrap()
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, read).unwrap())
        .collect()
}

/// Where the first chunk of `name` in commit `seq` lies.
fn chunk_range(s: &SimStorage, read: &ReadOptions, seq: u64, name: &str) -> (u64, u64) {
    let h = &opened_all(s, read)[seq as usize];
    let snap = h.catalog.replay(None).unwrap();
    let entry = snap.get(&path(name)).unwrap();
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
    (at, r.stored_len)
}

fn damaged(s: &SimStorage, f: impl FnOnce(&mut Vec<u8>)) -> SimStorage {
    let mut b = s.contents();
    f(&mut b);
    SimStorage::from_bytes(b)
}

/// Flip the middle of the first chunk of `name` in commit `seq`.
fn hit_chunk(s: &SimStorage, seq: u64, name: &str) -> (SimStorage, (u64, u64)) {
    let r = chunk_range(s, &opts(), seq, name);
    (damaged(s, |b| flip(b, r.0 + r.1 / 2)), r)
}

fn copy<'a>(label: &str, s: &'a SimStorage) -> CopySource<'a> {
    CopySource {
        label: label.to_owned(),
        storage: s,
    }
}

fn plan_with(s: &SimStorage, copies: &[CopySource<'_>]) -> RepairPlan {
    plan_with_copies(s, copies, &opts(), &Job::new().ctx()).unwrap()
}

fn run(
    s: &SimStorage,
    copies: &[CopySource<'_>],
    p: &RepairPlan,
) -> (SimDir, mochi_core::Result<mochi_core::repair::RepairReport>) {
    let mut dir = SimDir::new();
    let r = apply_with_copies(
        s,
        copies,
        p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions::default(),
        &Job::new().ctx(),
    );
    (dir, r)
}

fn ladder(p: &RepairPlan) -> Vec<StepStatus> {
    p.ladder.iter().map(|s| s.status).collect()
}

fn step5(p: &RepairPlan) -> StepStatus {
    p.ladder.iter().find(|l| l.step == 5).unwrap().status
}

fn without(mut st: State, omitted: &BTreeSet<Vec<u8>>) -> State {
    st.retain(|k, _| !omitted.contains(k));
    st
}

/// The repaired archive holds `kept` pristine snapshots, in order, each minus
/// `omitted` (by pristine sequence), with identical version IDs and promised
/// attributes.
fn assert_repairs(
    pristine: &SimStorage,
    out: &SimStorage,
    read: &ReadOptions,
    kept: &[u64],
    omitted: &BTreeMap<u64, BTreeSet<Vec<u8>>>,
) {
    let source = opened_all(pristine, read);
    let new = opened_all(out, read);
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
        let sa = promised_attributes(pristine, a, read).unwrap();
        for (v, attr) in promised_attributes(out, b, read).unwrap() {
            assert_eq!(sa.get(&v), Some(&attr), "attributes kept");
        }
    }
}

fn all(n: u64) -> Vec<u64> {
    (0..n).collect()
}

fn names(list: &[&str]) -> BTreeSet<Vec<u8>> {
    list.iter().map(|n| n.as_bytes().to_vec()).collect()
}

// ---- 1 ---------------------------------------------------------------------

/// **A damaged chunk with an intact copy**: without the copy the version is
/// left out (partial); with it the plan is complete, exactly the chunk's frame
/// is donated, and the repaired archive reproduces every pristine snapshot.
#[test]
fn copy_1_a_damaged_chunk_is_donated_and_the_repair_is_complete() {
    let pristine = build(&SIX);
    let (bad, chunk) = hit_chunk(&pristine, 2, "c");
    let good = pristine.clone();

    let without_copy = plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(without_copy.outcome, Outcome::Partial);
    assert_eq!(step5(&without_copy), StepStatus::NotAvailable);
    assert_eq!(without_copy.ladder[4].detail, "no --copy given");
    assert!(without_copy.copies.is_empty() && without_copy.donations.is_empty());

    let copies = [copy("good.mochi", &good)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.format, PLAN_FORMAT);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.exit_code, 0);
    assert_eq!(step5(&p), StepStatus::Used);
    assert_eq!(p.donations.len(), 1, "{:#?}", p.donations);
    let d = &p.donations[0];
    assert_eq!((d.offset, d.len), chunk);
    assert_eq!(d.kind, DonationKind::DataObject);
    assert_eq!(d.copy, 0);
    let c = &p.copies[0];
    assert_eq!(c.path, "good.mochi");
    assert_eq!(c.len, good.contents().len() as u64);
    assert_eq!(c.matched, Some(CopyMatch::IdenticalDescriptor));
    assert_eq!(c.agreed_through, Some(5));
    assert_eq!((c.diverged_at, c.later_commits), (None, 0));
    assert_eq!((c.frames_donated, c.bytes_donated), (1, chunk.1));
    assert_eq!(c.status, CopyStatus::Used);
    assert!(p.damage_found);
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Chain));

    // JSON round trip, `match` spelled as the plan format says.
    let json = serde_json::to_string(&p).unwrap();
    assert!(
        json.contains("\"match\":\"identical_descriptor\""),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<RepairPlan>(&json).unwrap(), p);

    let (dir, r) = run(&bad, &copies, &p);
    let report = r.unwrap();
    assert_eq!(report.outcome, Outcome::Complete);
    assert_eq!(report.donations, p.donations);
    assert_eq!(report.reverification.exit_code, 0);
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 2 ---------------------------------------------------------------------

/// **Head commit frame and footer destroyed, copy intact**: the head is
/// recovered, labelled as confirmed by the copy, and exactly the commit frame
/// and the footer are donated. With the head's delta manifest destroyed as
/// well, the manifest is donated too (verified by the hash the commit names).
#[test]
fn copy_2_a_destroyed_head_is_recovered_and_labelled_copy_confirmed() {
    let pristine = build(&SIX);
    let head = history(&pristine).pop().unwrap();
    let footer_end = head.footer_offset + FOOTER_FRAME_LEN;
    let bad = damaged(&pristine, |b| {
        flip(b, head.commit_offset + 20);
        flip(b, head.footer_offset + 30);
    });
    // Control: the source alone has lost the head.
    let alone = plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(alone.head.as_ref().unwrap().seq, 4);
    assert_eq!(alone.outcome, Outcome::Partial);
    assert_eq!(alone.tail.as_ref().unwrap().state, "unresolved");

    let copies = [copy("good.mochi", &pristine)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.head.as_ref().unwrap().seq, 5);
    assert_eq!(p.head.as_ref().unwrap().found_by, "eof");
    let kinds: Vec<DonationKind> = p.donations.iter().map(|d| d.kind).collect();
    assert_eq!(kinds, [DonationKind::CommitRecord, DonationKind::Footer]);
    assert_eq!(p.donations[0].offset, head.commit_offset);
    assert_eq!(p.donations[1].offset + p.donations[1].len, footer_end);
    assert_eq!(p.copies[0].agreed_through, Some(5));
    let trust: Vec<(u64, Trust)> = p.snapshots.iter().map(|s| (s.seq, s.trust)).collect();
    assert_eq!(trust.len(), 6);
    assert_eq!(trust[5], (5, Trust::CopyConfirmed));
    assert!(trust[..5].iter().all(|(_, t)| *t == Trust::Chain));
    assert!(p.copies[0].detail.contains("confirmed by this copy alone"));

    let (dir, r) = run(&bad, &copies, &p);
    let report = r.unwrap();
    assert_eq!(report.commits[5].trust, Trust::CopyConfirmed);
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );

    // The head's delta manifest destroyed too.
    let manifest = head.commit.delta_manifest;
    let worse = damaged(&bad, |b| flip(b, manifest.offset + manifest.stored_len / 2));
    let p = plan_with(&worse, &copies);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    let kinds: Vec<DonationKind> = p.donations.iter().map(|d| d.kind).collect();
    assert_eq!(
        kinds,
        [
            DonationKind::DeltaManifest,
            DonationKind::CommitRecord,
            DonationKind::Footer
        ]
    );
    let (dir, r) = run(&worse, &copies, &p);
    r.unwrap();
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 3 ---------------------------------------------------------------------

/// Two archives that share commits 0–3 and append different commit 4 (and the
/// copy a commit 5): the copy has **diverged**. `len` sets the new files'
/// size in the source and the copy: equal sizes put both commit 4 footers at
/// one offset, different sizes at different ones; the copy is diverged either
/// way.
fn fork(source_len: usize, copy_len: usize) -> (SimStorage, SimStorage) {
    let shared = [("a", 1), ("b", 2), ("c", 3), ("d", 4)];
    let extend = |tail: &[(&str, u64)], len: usize| {
        let s = build(&shared);
        let (mut w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(SeqIds::new(40)),
            test_options(),
            mochi_core::publish::TailPolicy::Refuse,
        )
        .unwrap();
        for (name, seed) in tail {
            let mut tx = Transaction::new();
            tx.put_file(
                path(name),
                deterministic_bytes(*seed, len),
                attrs(0o644, *seed as i64),
            );
            w.commit(tx, &Job::new().ctx()).unwrap();
        }
        w.close().unwrap();
        s
    };
    (
        extend(&[("g", 7)], source_len),
        extend(&[("g", 8), ("h", 9)], copy_len),
    )
}

#[test]
fn copy_3_a_diverged_copy_donates_only_at_or_below_the_shared_commit() {
    for (len, copy_len) in [(150, 150), (150, 100)] {
        let (pristine, other) = fork(len, copy_len);
        // Damage the source in a shared commit (1) and in its own commit 4.
        let shared_chunk = chunk_range(&pristine, &opts(), 1, "b");
        let own_chunk = chunk_range(&pristine, &opts(), 4, "g");
        let bad = damaged(&pristine, |b| {
            flip(b, shared_chunk.0 + shared_chunk.1 / 2);
            flip(b, own_chunk.0 + own_chunk.1 / 2);
        });
        let copies = [copy("other.mochi", &other)];
        let p = plan_with(&bad, &copies);
        let c = &p.copies[0];
        assert_eq!(c.status, CopyStatus::Used, "{len}: {c:#?}");
        assert_eq!(c.matched, Some(CopyMatch::IdenticalDescriptor));
        assert_eq!(c.agreed_through, Some(3), "{len}");
        assert_eq!(c.diverged_at, Some(4), "{len}");
        // Only the shared commit's chunk is donated; nothing above commit 3.
        assert_eq!(p.donations.len(), 1, "{len}: {:#?}", p.donations);
        assert_eq!(
            (p.donations[0].offset, p.donations[0].len),
            shared_chunk,
            "{len}"
        );
        let c3_end = history(&pristine)[3].footer_offset + FOOTER_FRAME_LEN;
        assert!(p.donations.iter().all(|d| d.offset + d.len <= c3_end));
        // The source's own damaged chunk is left out, as without the copy.
        assert_eq!(p.outcome, Outcome::Partial, "{len}");
        assert_eq!(p.omitted.len(), 1);
        assert_eq!(p.omitted[0].path, "g");
        let (dir, r) = run(&bad, &copies, &p);
        r.unwrap();
        assert_repairs(
            &pristine,
            &dir.file(OUT).unwrap(),
            &opts(),
            &all(5),
            &BTreeMap::from([(4, names(&["g"]))]),
        );
    }
}

// ---- 4 ---------------------------------------------------------------------

/// **A copy longer than the source**: its later commits are reported and
/// never used.
#[test]
fn copy_4_commits_beyond_the_sources_length_are_reported_never_used() {
    let longer = build(&[
        ("a", 1),
        ("b", 2),
        ("c", 3),
        ("d", 4),
        ("e", 5),
        ("f", 6),
        ("g", 7),
        ("h", 8),
    ]);
    let source = build(&SIX);
    assert!(longer.contents().starts_with(&source.contents()));
    let (bad, chunk) = hit_chunk(&source, 2, "c");
    let copies = [copy("longer.mochi", &longer)];
    let p = plan_with(&bad, &copies);
    let c = &p.copies[0];
    assert_eq!(c.later_commits, 2, "{c:#?}");
    assert_eq!(c.agreed_through, Some(5));
    assert_eq!(c.status, CopyStatus::Used);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.snapshots.len(), 6, "nothing beyond the source's head");
    assert_eq!(p.head.as_ref().unwrap().seq, 5);
    assert_eq!(p.donations.len(), 1);
    assert_eq!((p.donations[0].offset, p.donations[0].len), chunk);
    assert!(c.detail.contains("2 later commits"), "{}", c.detail);
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &source,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 5 ---------------------------------------------------------------------

/// **A shorter (older) copy** donates for its own commits only.
#[test]
fn copy_5_an_older_copy_donates_for_its_commits_only() {
    let source = build(&SIX);
    let older = build(&SIX[..3]);
    assert!(source.contents().starts_with(&older.contents()));
    let early = chunk_range(&source, &opts(), 1, "b");
    let late = chunk_range(&source, &opts(), 4, "e");
    let bad = damaged(&source, |b| {
        flip(b, early.0 + early.1 / 2);
        flip(b, late.0 + late.1 / 2);
    });
    let copies = [copy("older.mochi", &older)];
    let p = plan_with(&bad, &copies);
    let c = &p.copies[0];
    assert_eq!(c.agreed_through, Some(2));
    assert_eq!(c.later_commits, 0);
    assert_eq!(c.status, CopyStatus::Used);
    assert_eq!(p.donations.len(), 1);
    assert_eq!((p.donations[0].offset, p.donations[0].len), early);
    assert_eq!(p.outcome, Outcome::Partial);
    assert_eq!(p.omitted.len(), 1);
    assert_eq!(p.omitted[0].path, "e");
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &source,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::from([(4, names(&["e"])), (5, names(&["e"]))]),
    );
}

// ---- 6 ---------------------------------------------------------------------

/// **A copy damaged in the same place** supplies nothing: partial, and the
/// copy says it could not supply what the source lacks. A copy damaged
/// identically is byte-identical there, so it is not needed (the ladder still
/// says nothing was supplied for the damage that remains).
#[test]
fn copy_6_a_copy_damaged_in_the_same_place_supplies_nothing() {
    let pristine = build(&SIX);
    let (at, len) = chunk_range(&pristine, &opts(), 2, "c");
    let bad = damaged(&pristine, |b| flip(b, at + len / 2));

    let also_bad = damaged(&pristine, |b| flip(b, at + len / 2 + 1));
    let copies = [copy("also-bad.mochi", &also_bad)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.outcome, Outcome::Partial);
    assert!(p.donations.is_empty());
    assert_eq!(p.copies[0].status, CopyStatus::NotAvailable);
    assert!(
        p.copies[0].detail.contains("1 damaged frame"),
        "{}",
        p.copies[0].detail
    );
    assert_eq!(step5(&p), StepStatus::NotAvailable);
    assert_eq!(p.omitted.len(), 1);

    let same_flip = damaged(&pristine, |b| flip(b, at + len / 2));
    let copies = [copy("same.mochi", &same_flip)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.outcome, Outcome::Partial);
    assert!(p.donations.is_empty());
    assert_eq!(p.copies[0].status, CopyStatus::NotNeeded);
    assert_eq!(step5(&p), StepStatus::NotAvailable);
    assert!(p.ladder[4].detail.contains("supplied nothing"));
}

// ---- 7 ---------------------------------------------------------------------

/// **Not a copy** (another archive): never used.
#[test]
fn copy_7_another_archive_is_not_a_copy() {
    let pristine = build(&SIX);
    let (bad, _) = hit_chunk(&pristine, 2, "c");
    // Same files, other IDs: a different archive ID in its descriptor.
    let other = build_with(77, &SIX, 150, test_options(), &opts());
    assert_ne!(other.contents()[..64], pristine.contents()[..64]);
    let copies = [copy("other.mochi", &other)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.copies[0].matched, Some(CopyMatch::NotACopy));
    assert_eq!(p.copies[0].status, CopyStatus::NotAvailable);
    assert_eq!(p.copies[0].agreed_through, None);
    assert!(p.donations.is_empty());
    assert_eq!(step5(&p), StepStatus::NotAvailable);
    assert_eq!(p.outcome, Outcome::Partial);
    // An empty file and a one-byte file are not copies either, and not fatal.
    let empty = SimStorage::new();
    let tiny = SimStorage::from_bytes(vec![1]);
    let copies = [copy("empty", &empty), copy("tiny", &tiny)];
    let p = plan_with(&bad, &copies);
    assert!(p
        .copies
        .iter()
        .all(|c| c.matched == Some(CopyMatch::NotACopy)));
}

/// The source's descriptor destroyed: a copy whose descriptor hashes to the
/// one a self-verifying source commit records is a copy (match
/// `descriptor_hash`), and donates the descriptor.
#[test]
fn copy_7b_a_destroyed_descriptor_is_donated_by_hash() {
    let pristine = build(&SIX);
    let bad = damaged(&pristine, |b| flip(b, 30));
    let alone = plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(alone.outcome, Outcome::NothingRecoverable);
    let copies = [copy("good.mochi", &pristine)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.copies[0].matched, Some(CopyMatch::DescriptorHash));
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.donations.len(), 1);
    assert_eq!(p.donations[0].kind, DonationKind::Descriptor);
    assert_eq!(p.donations[0].offset, 0);
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 8 ---------------------------------------------------------------------

/// **A forged copy** never donates: here the copy holds, where the source's
/// chunk is damaged, a well-formed chunk of the same length that belongs
/// elsewhere. It does not hash to what the agreed structure expects.
#[test]
fn copy_8_a_forged_chunk_is_never_donated() {
    let pristine = build(&SIX);
    let (at, len) = chunk_range(&pristine, &opts(), 2, "c");
    let (other_at, other_len) = chunk_range(&pristine, &opts(), 3, "d");
    assert_eq!(len, other_len, "the fixture needs equal chunk lengths");
    let bad = damaged(&pristine, |b| flip(b, at + len / 2));
    let forged = damaged(&pristine, |b| {
        let replacement = b[other_at as usize..(other_at + other_len) as usize].to_vec();
        b[at as usize..(at + len) as usize].copy_from_slice(&replacement);
    });
    let copies = [copy("forged.mochi", &forged)];
    let p = plan_with(&bad, &copies);
    assert!(p.donations.is_empty(), "{:#?}", p.donations);
    assert_eq!(p.copies[0].status, CopyStatus::NotAvailable);
    assert_eq!(p.outcome, Outcome::Partial);
    assert_eq!(p.omitted.len(), 1);
    assert_eq!(p.omitted[0].path, "c");
}

// ---- first copy wins -------------------------------------------------------

/// **Argument order decides**: with two copies that could both donate the
/// frame, the first donates, and the second finds the source's frame good.
/// With the first damaged, the second donates.
#[test]
fn copy_first_in_argument_order_wins() {
    let pristine = build(&SIX);
    let (at, len) = chunk_range(&pristine, &opts(), 2, "c");
    let bad = damaged(&pristine, |b| flip(b, at + len / 2));
    let one = pristine.clone();
    let two = pristine.clone();
    let copies = [copy("one", &one), copy("two", &two)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.donations.len(), 1);
    assert_eq!(p.donations[0].copy, 0);
    assert_eq!(p.copies[0].status, CopyStatus::Used);
    assert_eq!(p.copies[1].status, CopyStatus::NotNeeded);
    assert_eq!(
        (p.copies[1].frames_donated, p.copies[1].bytes_donated),
        (0, 0)
    );

    let also_bad = damaged(&pristine, |b| flip(b, at + len / 2 + 1));
    let copies = [copy("damaged", &also_bad), copy("good", &two)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.donations.len(), 1);
    assert_eq!(p.donations[0].copy, 1);
    assert_eq!(p.copies[0].status, CopyStatus::NotAvailable);
    assert_eq!(p.copies[1].status, CopyStatus::Used);
    assert_eq!(p.outcome, Outcome::Complete);
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 9 ---------------------------------------------------------------------

/// A Redundancy archive: three commits of one 200 KB file each, 20 % parity.
fn redundant() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(3)),
        WriterOptions {
            chunk_size: Some(16 << 10),
            profile: Some(Profile {
                redundancy: Some(20),
                ..Profile::default()
            }),
            ..test_options()
        },
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    for (name, seed) in [("a", 1u64), ("b", 2), ("c", 3)] {
        let mut tx = Transaction::new();
        tx.put_file(
            path(name),
            deterministic_bytes(seed, 200_000),
            attrs(0o644, seed as i64),
        );
        w.commit(tx, &Job::new().ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn layout_of(s: &SimStorage, seq: u64) -> Layout {
    let h = history(s);
    let e = &h[seq as usize];
    let start = e.commit.parent.map_or(0, |p| p.footer_offset);
    Layout::compute(start, e.commit_offset + commit_len(s, e) - start, 20).unwrap()
}

fn commit_len(s: &SimStorage, e: &HistoryEntry) -> u64 {
    let r = mochi_core::storage::StorageReader::new(s).unwrap();
    mochi_format::footer::parse_footer_frame(&r, e.footer_offset)
        .unwrap()
        .commit_len
}

fn hit_shards(b: &mut [u8], l: &Layout, shards: &[u64]) {
    for &i in shards {
        let (off, n) = l.shard_extent(i);
        assert!(n > 0, "shard {i} is virtual");
        b[(off + n / 2) as usize] ^= 0xA5;
    }
}

/// **Parity first, then the copy.** Commit 1 has more damaged shards in a
/// group than parity covers; commit 2 has one that parity rebuilds. Parity
/// rebuilds the second and leaves the first as damaged; the copy covers the
/// first. The repair is complete, and the ladder shows steps 6 and 5 used.
#[test]
fn copy_9_parity_runs_first_and_the_copy_covers_what_parity_cannot() {
    let pristine = redundant();
    let l1 = layout_of(&pristine, 1);
    let l2 = layout_of(&pristine, 2);
    assert_eq!(l1.groups, 1, "{l1:?}");
    let m = l1.parity_shards;
    assert!(l1.real_shards() >= m + 3, "{l1:?}");
    // m + 1 shards of group 0, avoiding the first (the parent's footer) and
    // the last (the commit record).
    let beyond: Vec<u64> = (1..=m + 1).map(|j| l1.shard_index(0, j)).collect();
    assert!(beyond.iter().all(|i| *i + 1 < l1.real_shards()));
    let bad = damaged(&pristine, |b| {
        hit_shards(b, &l1, &beyond);
        hit_shards(b, &l2, &[l2.shard_index(0, 1)]);
    });

    // Without the copy: parity rebuilt one shard, and the other group's loss
    // leaves something out.
    let alone = plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(alone.parity.as_ref().unwrap().beyond_capacity().count(), 1);
    assert_ne!(alone.outcome, Outcome::Complete);

    let copies = [copy("good.mochi", &pristine)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    let st: BTreeMap<u8, StepStatus> = p.ladder.iter().map(|l| (l.step, l.status)).collect();
    assert_eq!(st[&6], StepStatus::Used, "{:?}", p.ladder);
    assert_eq!(st[&5], StepStatus::Used, "{:?}", p.ladder);
    assert!(!p.donations.is_empty());
    // Every donation lies in a hit shard of commit 1, none in commit 2's
    // (parity rebuilt that one).
    let in_hit = |off: u64, len: u64| {
        beyond.iter().any(|i| {
            let (o, n) = l1.shard_extent(*i);
            off < o + n && o < off + len
        })
    };
    assert!(p.donations.iter().all(|d| in_hit(d.offset, d.len)));
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(3),
        &BTreeMap::new(),
    );
}

// ---- 10 --------------------------------------------------------------------

/// **Encrypted source and copy.** Planning keyless works where it does today
/// (nothing is claimed complete, nothing leaks); planning and applying with
/// the passphrase donate the sealed frame by its stored hash and re-seal.
#[test]
fn copy_10_encrypted_archives_donate_sealed_frames() {
    let read = keyed_read(&["pw"]);
    let pristine = build_with(5, &SIX, 150, encrypted_options(&["pw"]), &read);
    let (at, len) = chunk_range(&pristine, &read, 2, "c");
    let bad = damaged(&pristine, |b| flip(b, at + len / 2));
    let copies = [copy("good.mochi", &pristine)];

    // Keyless: planning does not fail on the key, and claims nothing.
    let keyless = plan_with_copies(&bad, &copies, &opts(), &Job::new().ctx());
    if let Ok(p) = keyless {
        assert_ne!(p.outcome, Outcome::Complete);
    }

    let p = plan_with_copies(&bad, &copies, &read, &Job::new().ctx()).unwrap();
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.donations.len(), 1);
    assert_eq!((p.donations[0].offset, p.donations[0].len), (at, len));
    let mut dir = SimDir::new();
    let report = apply_with_copies(
        &bad,
        &copies,
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
    assert_eq!(report.outcome, Outcome::Complete);
    assert_eq!(report.reverification.exit_code, 0);
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &read,
        &all(6),
        &BTreeMap::new(),
    );
}

// ---- 11 --------------------------------------------------------------------

/// **Copies (and the source) are untouched** by `plan` and by `apply`.
#[test]
fn copy_11_the_copies_and_the_source_are_never_written() {
    let pristine = build(&SIX);
    let (bad, _) = hit_chunk(&pristine, 2, "c");
    let good = pristine.clone();
    let (before_copy, before_src) = (good.contents(), bad.contents());
    let copies = [copy("good.mochi", &good)];
    let p = plan_with(&bad, &copies);
    assert_eq!(good.contents(), before_copy);
    assert_eq!(bad.contents(), before_src);
    let (_dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_eq!(good.contents(), before_copy);
    assert_eq!(bad.contents(), before_src);
    assert!(!good.is_halted());
}

// ---- 12 --------------------------------------------------------------------

/// **Apply refuses**: a different copy list, the copies in another order, a
/// copy edited between plan and apply, and a v1 plan. Nothing is written.
#[test]
fn copy_12_apply_refuses_a_plan_that_does_not_match_its_copies() {
    let pristine = build(&SIX);
    let (bad, chunk) = hit_chunk(&pristine, 2, "c");
    let one = pristine.clone();
    let two = pristine.clone();
    let planned = [copy("one", &one)];
    let p = plan_with(&bad, &planned);
    assert_eq!(p.outcome, Outcome::Complete);

    let refused = |copies: &[CopySource<'_>], p: &RepairPlan| {
        let (dir, r) = run(&bad, copies, p);
        assert!(dir.names().is_empty(), "nothing was written");
        r.unwrap_err()
    };
    // No copies at all, and a second copy.
    assert_eq!(refused(&[], &p).code, ErrorCode::InvalidArgument);
    let more = [copy("one", &one), copy("two", &two)];
    assert_eq!(refused(&more, &p).code, ErrorCode::InvalidArgument);
    // Another spelling of the same copy is another copy list.
    assert_eq!(
        refused(&[copy("./one", &one)], &p).code,
        ErrorCode::InvalidArgument
    );
    // Order matters for two copies.
    let p2 = plan_with(&bad, &more);
    let swapped = [copy("two", &two), copy("one", &one)];
    assert_eq!(refused(&swapped, &p2).code, ErrorCode::InvalidArgument);
    // The copy edited between plan and apply: the donated chunk is gone.
    let edited = damaged(&pristine, |b| flip(b, chunk.0 + chunk.1 / 2 + 3));
    assert_eq!(
        refused(&[copy("one", &edited)], &p).code,
        ErrorCode::InvalidArgument
    );
    // A v1 plan (as an older build wrote it: no copies, donations, trust).
    let mut v: serde_json::Value =
        serde_json::to_value(plan(&bad, &opts(), &Job::new().ctx()).unwrap()).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.insert("format".into(), "mochi-repair-plan-v1".into());
    obj.remove("copies");
    obj.remove("donations");
    for s in obj["snapshots"].as_array_mut().unwrap() {
        s.as_object_mut().unwrap().remove("trust");
    }
    let v1: RepairPlan = serde_json::from_value(v).unwrap();
    let e = refused(&[], &v1);
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(
        e.message.contains("format") && e.message.contains("mochi-repair-plan-v1"),
        "{}",
        e.message
    );
    // Through the old entry points too.
    let mut dir = SimDir::new();
    let e = apply(
        &bad,
        &v1,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(dir.names().is_empty());
    // The matching call still works.
    let (_dir, r) = run(&bad, &planned, &p);
    r.unwrap();
}

// ---- 13 --------------------------------------------------------------------

/// **An I/O error on a copy** disables that copy and is reported; the plan
/// still completes (with another copy, the second donates). An error on the
/// source is still fatal.
#[test]
fn copy_13_an_io_error_on_a_copy_disables_it_and_is_not_fatal() {
    let pristine = build(&SIX);
    let (bad, chunk) = hit_chunk(&pristine, 2, "c");
    let good = pristine.clone();
    for fail_from in [0, 100, chunk.0, chunk.0 + chunk.1 / 2] {
        let failing = FailingReads {
            inner: &good,
            fail_from,
        };
        let copies = [CopySource {
            label: "failing".into(),
            storage: &failing,
        }];
        let p = plan_with(&bad, &copies);
        let c = &p.copies[0];
        assert_eq!(c.status, CopyStatus::Unreadable, "{fail_from}: {c:#?}");
        assert!(c.detail.contains("injected read failure"), "{}", c.detail);
        assert!(p.donations.is_empty(), "{fail_from}");
        assert_eq!(c.frames_donated, 0);
        assert_eq!(p.outcome, Outcome::Partial);
        assert_eq!(step5(&p), StepStatus::NotAvailable);
    }
    // Failing late (after the donated chunk was read) takes the donation back.
    let failing = FailingReads {
        inner: &good,
        fail_from: good.contents().len() as u64 - 1,
    };
    let copies = [
        CopySource {
            label: "failing".into(),
            storage: &failing,
        },
        copy("good", &good),
    ];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.copies[0].status, CopyStatus::Unreadable);
    assert_eq!(p.copies[1].status, CopyStatus::Used);
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
    assert_eq!(p.donations.len(), 1);
    assert_eq!(p.donations[0].copy, 1);
    let (dir, r) = run(&bad, &copies, &p);
    r.unwrap();
    assert_repairs(
        &pristine,
        &dir.file(OUT).unwrap(),
        &opts(),
        &all(6),
        &BTreeMap::new(),
    );

    // The source failing is not a copy's problem.
    let src = FailingReads {
        inner: &bad,
        fail_from: 100,
    };
    let e = plan_with_copies(&src, &[copy("good", &good)], &opts(), &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::IoError);
}

// ---- trust is not borrowed -------------------------------------------------

/// A copy has no say about commits the source holds: where the source's own
/// chain is intact and its frames verify, a copy with different bytes
/// elsewhere changes nothing.
#[test]
fn copy_an_intact_source_needs_nothing_and_changes_nothing() {
    let source = build(&SIX);
    let (other, _) = fork(150, 150);
    let longer = build_with(1, &SIX, 150, test_options(), &opts());
    let copies = [copy("same.mochi", &longer), copy("fork.mochi", &other)];
    let p = plan_with(&source, &copies);
    let alone = plan(&source, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.outcome, Outcome::Complete);
    assert!(p.donations.is_empty());
    assert!(!p.damage_found);
    assert_eq!(step5(&p), StepStatus::NotNeeded);
    assert_eq!(p.snapshots, alone.snapshots);
    assert_eq!(p.copies[0].status, CopyStatus::NotNeeded);
    assert_eq!(
        ladder(&p)
            .iter()
            .zip(ladder(&alone))
            .filter(|(a, b)| *a != b)
            .count(),
        1,
        "only step 5 differs"
    );
}

/// **A disk that dies at every possible moment.** However many reads a
/// failing copy serves before it fails, planning returns, the copy is either
/// used (it finished) or unreadable with none of its donations kept, and with
/// a good copy behind it the repair is complete and identical to the oracle.
#[test]
fn copy_13b_a_copy_failing_at_any_read_gives_back_what_it_donated() {
    let pristine = build(&SIX);
    let (bad, _) = hit_chunk(&pristine, 2, "c");
    let good = pristine.clone();
    // Reads one copy serves when nothing fails.
    let probe = FailAfter::new(&good, usize::MAX);
    let p = plan_with(&bad, &[copy_of(&probe)]);
    assert_eq!(p.copies[0].status, CopyStatus::Used);
    let total = probe.served.get();
    assert!(total > 10, "{total}");
    let mut late_failures = 0;
    // Every read for a short run; a stride keeps a long one affordable.
    let stride = (total / 80).max(1);
    for n in (0..=total).step_by(stride).chain([total - 1, total - 2]) {
        let failing = FailAfter::new(&good, n);
        let copies = [copy_of(&failing), copy("good", &good)];
        let p = plan_with(&bad, &copies);
        match p.copies[0].status {
            CopyStatus::Unreadable => {
                assert_eq!(p.copies[0].frames_donated, 0, "{n}");
                assert!(p.donations.iter().all(|d| d.copy == 1), "{n}");
                late_failures += 1;
            }
            CopyStatus::Used => assert_eq!(p.donations[0].copy, 0, "{n}"),
            other => panic!("{n}: {other:?}"),
        }
        assert_eq!(p.outcome, Outcome::Complete, "{n}: {p:#?}");
        assert_eq!(p.donations.len(), 1, "{n}");
        if n % (stride * 8) != 0 {
            continue;
        }
        // Apply sees the same disk fail at the same read.
        let again = FailAfter::new(&good, n);
        let (dir, r) = run(&bad, &[copy_of(&again), copy("good", &good)], &p);
        r.unwrap();
        assert_repairs(
            &pristine,
            &dir.file(OUT).unwrap(),
            &opts(),
            &all(6),
            &BTreeMap::new(),
        );
    }
    assert!(late_failures > 0);
}

fn copy_of<'a>(s: &'a FailAfter<'a>) -> CopySource<'a> {
    CopySource {
        label: "failing".into(),
        storage: s,
    }
}

// ---- the agreement rule, literally -----------------------------------------

/// **A copy does not rebuild a commit the source cannot show**, unless that
/// commit extends the source's latest one (D22 item 3): a destroyed footer in
/// the middle is not agreed, so neither it nor its commit frame is donated and
/// the chain stays broken. The commits above it still agree by (i), and the
/// objects they reference still come from the copy.
#[test]
fn copy_14_a_damaged_middle_commit_is_not_confirmed_by_a_copy() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    let middle = &h[2];
    let late = chunk_range(&pristine, &opts(), 4, "e");
    let bad = damaged(&pristine, |b| {
        flip(b, middle.footer_offset + 30);
        flip(b, late.0 + late.1 / 2);
    });
    let copies = [copy("good.mochi", &pristine)];
    let p = plan_with(&bad, &copies);
    let c = &p.copies[0];
    assert_eq!(c.agreed_through, Some(5), "{c:#?}");
    assert!(
        c.detail.contains("commit 2 cannot be agreed"),
        "{}",
        c.detail
    );
    assert!(!c.detail.contains("confirmed by this copy alone"), "{c:#?}");
    // The late chunk is donated; commit 2's footer is not.
    let kinds: Vec<(DonationKind, u64)> = p.donations.iter().map(|d| (d.kind, d.offset)).collect();
    assert_eq!(kinds, [(DonationKind::DataObject, late.0)], "{kinds:?}");
    // The chain cannot be followed through commit 2's footer, so the head
    // cannot be opened, exactly as without the copy.
    assert_ne!(p.outcome, Outcome::Complete);
    assert!(p.snapshots.iter().all(|s| s.trust == Trust::Chain));
    let alone = plan(&bad, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.snapshots, alone.snapshots);
    assert_eq!(p.lost_snapshots, alone.lost_snapshots);
}

/// **Chaining goes through a commit the copy holds.** The source lost its
/// newest commit; the copy has it, but its own commit 4 (the one the newest
/// names as its parent, and the source's head) is damaged, so the copy's chain
/// stops above it. The copy then holds no commit that agrees with the source,
/// and the newest commit is not confirmed by a bare parent link (D22 item 3
/// (ii) wants an *agreed* commit).
#[test]
fn copy_15_a_commit_that_chains_to_nothing_the_copy_agrees_on_is_not_confirmed() {
    let pristine = build(&SIX);
    let h = history(&pristine);
    let bad = damaged(&pristine, |b| {
        flip(b, h[5].commit_offset + 20);
        flip(b, h[5].footer_offset + 30);
    });
    let no_head = damaged(&pristine, |b| flip(b, h[4].footer_offset + 30));
    let copies = [copy("no-head.mochi", &no_head)];
    let p = plan_with(&bad, &copies);
    let c = &p.copies[0];
    assert_eq!(c.agreed_through, None, "{c:#?}");
    assert_eq!(c.status, CopyStatus::NotAvailable);
    assert!(p.donations.is_empty(), "{:#?}", p.donations);
    assert_eq!(p.head.as_ref().unwrap().seq, 4);
    assert_eq!(p.outcome, Outcome::Partial);
    // The control: a copy that holds commit 4 does confirm the newest.
    let copies = [copy("good.mochi", &pristine)];
    let p = plan_with(&bad, &copies);
    assert_eq!(p.copies[0].agreed_through, Some(5));
    assert_eq!(p.outcome, Outcome::Complete, "{p:#?}");
}

/// **Cost.** Comparing in blocks means a copy costs one more read of the
/// source (the comparison) plus the frames that touch a differing block, not a
/// hash of every frame on top of it (D22 item 4): a damaged chunk in a large
/// archive, and a copy that is otherwise identical.
#[test]
fn copy_16_a_copy_costs_about_one_more_read_of_the_source() {
    use std::cell::RefCell;
    let big = build_with(
        1,
        &[("a", 1), ("b", 2), ("c", 3)],
        300_000,
        WriterOptions {
            chunk_size: Some(16 << 10),
            ..test_options()
        },
        &opts(),
    );
    let len = big.contents().len() as u64;
    let (bad, chunk) = hit_chunk(&big, 1, "b");
    // The baseline is a plan of the undamaged archive (every version is read
    // there too), so the difference is the copy pass alone.
    let bytes_read = |with_copy: bool| -> u64 {
        let t = mochi_testkit::replay::Tracing {
            inner: if with_copy { &bad } else { &big },
            reads: RefCell::new(Vec::new()),
        };
        let good = big.clone();
        let copies = [copy("good", &good)];
        let p = if with_copy {
            plan_with_copies(&t, &copies, &opts(), &Job::new().ctx()).unwrap()
        } else {
            plan(&t, &opts(), &Job::new().ctx()).unwrap()
        };
        assert_eq!(p.outcome, Outcome::Complete);
        if with_copy {
            assert_eq!(p.donations.len(), 1);
            assert_eq!((p.donations[0].offset, p.donations[0].len), chunk);
        }
        let n = t.reads.borrow().iter().map(|r| r.1).sum();
        n
    };
    let extra = bytes_read(true).saturating_sub(bytes_read(false));
    assert!(
        extra <= len + len / 4,
        "the copy pass read {extra} more bytes of a {len}-byte source"
    );
}
