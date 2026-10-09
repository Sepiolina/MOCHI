//! T18: descriptor failure behaviour (spec Annex B.2 D12; gate G1).
//!
//! A missing, damaged, or mismatched descriptor permits head discovery,
//! commit validation, and diagnostics; it refuses interpretation and append
//! (`DESCRIPTOR_INVALID`), before append writes anything; and damage
//! assessment reports it as `FAIL`. A profile is fixed at creation: asking
//! an append for another one is `PROFILE_CHANGE_UNSUPPORTED` (exit 4, checked
//! in `mochi-cli/tests/profile_change.rs`).

use mochi_core::damage::{assess_damage, Effect, ObjectRole, Readability};
use mochi_core::descriptor::{Descriptor, Profile};
use mochi_core::publish::{
    commit_history, locate_head, open_at_footer, open_head, ArchiveWriter, CheckpointPolicy,
    HistoryEntry, ReadOptions, TailPolicy, TailState, WriterOptions,
};
use mochi_core::status::Status;
use mochi_core::ErrorCode;
use mochi_format::digest::stored_object_hash;
use mochi_testkit::archive::{read_state, test_options, Job};
use mochi_testkit::forge::{self, empty_delta, rule_base, txid, Forge};
use mochi_testkit::replay::{damage, history, history_long, is_cp, write, Step};
use mochi_testkit::{SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// cp0 d1 d2 | cp3 d4 d5 | cp6 d7 d8 (`Every(3)`).
fn fixture() -> (SimStorage, Vec<HistoryEntry>, Vec<Step>) {
    let steps = history_long();
    let s = write(CheckpointPolicy::Every(3), &steps);
    let h = commit_history(&s, &opts()).unwrap();
    assert_eq!(h.len(), 9);
    (s, h, steps)
}

fn with_profile(profile: Option<Profile>) -> WriterOptions {
    WriterOptions {
        profile,
        ..test_options()
    }
}

fn append_err(s: &SimStorage, o: WriterOptions, tail: TailPolicy) -> mochi_core::MochiError {
    ArchiveWriter::open_append(s.clone(), Box::new(SeqIds::new(9000)), o, tail)
        .map(|_| ())
        .expect_err("append must be refused")
}

/// Append a complete frame that is not a footer after the head: an
/// uncommitted tail of one frame (a copy of the head's delta manifest).
fn add_uncommitted_tail(bytes: &mut Vec<u8>, head: &HistoryEntry) -> u64 {
    let r = head.commit.delta_manifest;
    let frame = bytes[r.offset as usize..(r.offset + r.stored_len) as usize].to_vec();
    bytes.extend_from_slice(&frame);
    r.stored_len
}

const ENCRYPT: Profile = Profile {
    tar_compatible: false,
    encrypted: true,
};
const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
};

// ---- damaged and missing --------------------------------------------------------------

/// **Checklist DoD (damaged).** D12 "Permitted": head discovery, footer and
/// commit validation, and diagnostics; "Reported: FAIL"; "Refused":
/// interpretation of every commit, and append.
#[test]
fn t18_damaged_descriptor_permits_discovery_and_diagnostics_only() {
    let (s, h, _) = fixture();
    let clean_head = locate_head(&s, &opts().limits).unwrap();
    let mut bytes = s.contents();
    damage(&mut bytes, h[0].commit.descriptor);
    let d = SimStorage::from_bytes(bytes.clone());

    // Permitted.
    let loc = locate_head(&d, &opts().limits).unwrap();
    assert_eq!(loc.footer, clean_head.footer);
    assert!(loc.tail.is_clean());
    let dh = commit_history(&d, &opts()).unwrap();
    let ids = |v: &[HistoryEntry]| v.iter().map(|e| e.commit_id).collect::<Vec<_>>();
    assert_eq!(ids(&dh), ids(&h), "commit validation is unaffected");

    // Refused: interpretation of every commit.
    assert_eq!(
        open_head(&d, &opts()).unwrap_err().code,
        ErrorCode::DescriptorInvalid
    );
    for e in &h {
        let err = open_at_footer(&d, e.footer_offset, &opts()).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::DescriptorInvalid,
            "commit {}",
            e.commit.seq
        );
    }

    // Reported: FAIL, one finding covering every commit.
    let job = Job::new();
    let r = assess_damage(&d, &opts(), &job.ctx()).unwrap();
    assert_eq!(r.objects.len(), 1, "{:?}", r.objects);
    let o = &r.objects[0];
    assert_eq!(
        (o.seq, o.role, o.offset, o.error.code),
        (0, ObjectRole::Descriptor, 0, ErrorCode::DescriptorInvalid)
    );
    let ranges: Vec<_> = r
        .ranges
        .iter()
        .map(|x| (x.first, x.last, x.effect))
        .collect();
    assert_eq!(ranges, [(0, 8, Effect::Unreadable)]);
    for c in &r.commits {
        assert_eq!(
            c.readable,
            Readability::Unreadable {
                code: ErrorCode::DescriptorInvalid
            },
            "commit {}",
            c.seq
        );
        assert_eq!(c.recoverability, Status::Fail);
        assert_eq!(c.causes, [0]);
    }
    assert_eq!(r.integrity(), Status::Fail);
    assert_eq!(r.recoverability(), Status::Fail);
    assert_eq!(r.head_recoverability(), Status::Fail);
    let f = r.findings();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].code, ErrorCode::DescriptorInvalid);
    let a = f[0].affected.as_ref().unwrap();
    assert_eq!((a.first, a.last), (0, 8));

    // Refused: append, and nothing was written.
    let e = append_err(&d, test_options(), TailPolicy::Refuse);
    assert_eq!(e.code, ErrorCode::DescriptorInvalid);
    assert_eq!(d.contents(), bytes);
}

/// D12 "missing": the descriptor frame's magic is gone. Discovery still
/// works; interpretation, append, and assessment behave as for damage.
#[test]
fn t18_missing_descriptor() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    bytes[..4].copy_from_slice(&[0; 4]);
    let d = SimStorage::from_bytes(bytes.clone());
    assert!(locate_head(&d, &opts().limits).unwrap().tail.is_clean());
    assert_eq!(commit_history(&d, &opts()).unwrap().len(), h.len());
    assert_eq!(
        open_head(&d, &opts()).unwrap_err().code,
        ErrorCode::DescriptorInvalid
    );
    assert_eq!(
        append_err(&d, test_options(), TailPolicy::Refuse).code,
        ErrorCode::DescriptorInvalid
    );
    let job = Job::new();
    let r = assess_damage(&d, &opts(), &job.ctx()).unwrap();
    assert_eq!(r.objects.len(), 1);
    assert_eq!(r.objects[0].role, ObjectRole::Descriptor);
    assert_eq!(r.integrity(), Status::Fail);
    assert_eq!(r.recoverability(), Status::Fail);
    assert_eq!(d.contents(), bytes);
}

/// Append refuses a bad descriptor **before** removing an uncommitted tail:
/// a refused append writes nothing (D12, D14). Control: with the descriptor
/// intact, the same request removes the tail.
#[test]
fn t18_append_refusal_precedes_tail_truncation() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    let committed = bytes.len() as u64;
    let tail = add_uncommitted_tail(&mut bytes, &h[8]);

    let control = SimStorage::from_bytes(bytes.clone());
    assert!(matches!(
        locate_head(&control, &opts().limits).unwrap().tail,
        TailState::Uncommitted { .. }
    ));
    let (_, t) = ArchiveWriter::open_append(
        control.clone(),
        Box::new(SeqIds::new(9000)),
        test_options(),
        TailPolicy::TruncateWithoutQuarantine,
    )
    .unwrap();
    assert_eq!(t.unwrap().removed_len, tail);
    assert_eq!(control.contents().len() as u64, committed);

    damage(&mut bytes, h[0].commit.descriptor);
    let d = SimStorage::from_bytes(bytes.clone());
    let e = append_err(&d, test_options(), TailPolicy::TruncateWithoutQuarantine);
    assert_eq!(e.code, ErrorCode::DescriptorInvalid);
    assert_eq!(d.contents(), bytes, "the tail was not truncated");
}

/// Descriptor damage dominates other damage for interpretation (opening
/// reads the descriptor first); every object is still reported with its
/// own range.
#[test]
fn t18_descriptor_damage_with_other_damage() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, h[0].commit.descriptor);
    damage(&mut bytes, h[4].commit.delta_manifest);
    let d = SimStorage::from_bytes(bytes);
    let job = Job::new();
    let r = assess_damage(&d, &opts(), &job.ctx()).unwrap();
    let objs: Vec<_> = r.objects.iter().map(|o| (o.seq, o.role)).collect();
    assert_eq!(
        objs,
        [(0, ObjectRole::Descriptor), (4, ObjectRole::DeltaManifest)]
    );
    let ranges: Vec<_> = r
        .ranges
        .iter()
        .map(|x| (x.first, x.last, x.effect))
        .collect();
    assert_eq!(
        ranges,
        [(0, 8, Effect::Unreadable), (4, 5, Effect::Unreadable)]
    );
    for (c, e) in r.commits.iter().zip(&h) {
        assert_eq!(
            c.readable,
            Readability::Unreadable {
                code: ErrorCode::DescriptorInvalid
            }
        );
        assert_eq!(
            open_at_footer(&d, e.footer_offset, &opts())
                .unwrap_err()
                .code,
            ErrorCode::DescriptorInvalid
        );
    }
}

/// An undamaged archive: the descriptor is checked once, not per commit,
/// and reports nothing.
#[test]
fn t18_intact_descriptor_is_checked_once_and_passes() {
    let (s, _, steps) = fixture();
    let job = Job::new();
    let r = assess_damage(&s, &opts(), &job.ctx()).unwrap();
    assert!(r.objects.is_empty());
    assert_eq!(r.integrity(), Status::Pass);
    assert_eq!(r.objects_checked, 9 + 2 * 3 + 1);
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_state(&s, &head).unwrap(), steps[8].after);
}

// ---- mismatched -----------------------------------------------------------------------

/// cp0, d1 (real), then forged d2 referencing a different descriptor (its
/// hash changed), then d3 on the same base referencing the real one.
fn mismatched_segment() -> (SimStorage, Vec<HistoryEntry>) {
    let s = write(CheckpointPolicy::Never, &history()[..2]);
    let mut f = Forge::new(s.contents());
    let h = f.history();
    assert!(is_cp(&h[0]) && !is_cp(&h[1]));
    let m2 = f.append_manifest(&empty_delta(&h[1], txid(7)));
    let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), m2, txid(7));
    rec.descriptor.stored_hash = mochi_format::digest::StoredObjectHash::from_bytes([0x11; 32]);
    let c2 = f.append_commit(&rec);
    let m3 = f.append_manifest(&empty_delta(&c2, txid(8)));
    // `delta_record` copies the previous commit's descriptor reference; d3
    // must reference the real one, or it fails on its own reference and the
    // segment rule is never exercised.
    let mut rec3 = forge::delta_record(&c2, rule_base(&h[1]), m3, txid(8));
    rec3.descriptor = h[1].commit.descriptor;
    f.append_commit(&rec3);
    let fs = f.storage();
    let h = commit_history(&fs, &opts()).unwrap();
    assert_eq!(h.len(), 4);
    assert_eq!(h[3].commit.descriptor, h[0].commit.descriptor);
    assert_ne!(h[2].commit.descriptor, h[0].commit.descriptor);
    (fs, h)
}

/// **Checklist DoD (mismatched).** A commit that references another
/// descriptor is refused, and so is every later commit of its segment
/// (D10.6), even though the head's own reference is valid. The earlier
/// commits stay readable, and the assessment says exactly that.
#[test]
fn t18_mismatched_descriptor_within_a_segment() {
    let (s, h) = mismatched_segment();
    assert!(open_at_footer(&s, h[0].footer_offset, &opts()).is_ok());
    assert!(open_at_footer(&s, h[1].footer_offset, &opts()).is_ok());
    for e in &h[2..] {
        assert_eq!(
            open_at_footer(&s, e.footer_offset, &opts())
                .unwrap_err()
                .code,
            ErrorCode::DescriptorInvalid,
            "commit {}",
            e.commit.seq
        );
    }
    let job = Job::new();
    let r = assess_damage(&s, &opts(), &job.ctx()).unwrap();
    let objs: Vec<_> = r.objects.iter().map(|o| (o.seq, o.role)).collect();
    assert_eq!(objs, [(2, ObjectRole::Descriptor)]);
    let ranges: Vec<_> = r
        .ranges
        .iter()
        .map(|x| (x.first, x.last, x.effect))
        .collect();
    assert_eq!(ranges, [(2, 3, Effect::Unreadable)]);
    let readable: Vec<_> = r.commits.iter().map(|c| c.readable.clone()).collect();
    let bad = Readability::Unreadable {
        code: ErrorCode::DescriptorInvalid,
    };
    assert_eq!(
        readable,
        [Readability::Image, Readability::Image, bad.clone(), bad]
    );
    assert_eq!(r.integrity(), Status::Fail);
    assert_eq!(r.head_recoverability(), Status::Fail);
    // Two distinct references were checked.
    assert_eq!(r.objects_checked, 4 + 2 + 2);
    assert_eq!(
        append_err(&s, test_options(), TailPolicy::Refuse).code,
        ErrorCode::DescriptorInvalid
    );
}

/// A commit whose archive ID differs from the descriptor's is mismatched
/// (D12): refused, and reported against that commit.
#[test]
fn t18_descriptor_naming_another_archive() {
    let s = write(CheckpointPolicy::EveryCommit, &history()[..1]);
    let h = commit_history(&s, &opts()).unwrap();
    // Replace the descriptor with one for another archive, same length, and
    // re-point commit 0 at it.
    let other = Descriptor::new(mochi_core::object::ArchiveId::from_bytes([0x5A; 32]), false)
        .to_stored()
        .unwrap();
    let f = rewrite_commit0_descriptor(&s, &h[0], other.as_bytes());
    let fs = f.storage();
    assert_eq!(
        open_head(&fs, &opts()).unwrap_err().code,
        ErrorCode::DescriptorInvalid
    );
    let job = Job::new();
    let r = assess_damage(&fs, &opts(), &job.ctx()).unwrap();
    assert_eq!(r.objects.len(), 1);
    assert_eq!(r.objects[0].role, ObjectRole::Descriptor);
    assert!(
        r.objects[0].error.message.contains("archive"),
        "{}",
        r.objects[0].error
    );
}

// ---- profiles -------------------------------------------------------------------------

/// Commit 0 of `s` re-published against the descriptor bytes `desc` (same
/// length as the original), so the head is consistent with it.
fn rewrite_commit0_descriptor(s: &SimStorage, e0: &HistoryEntry, desc: &[u8]) -> Forge {
    assert_eq!(e0.commit.seq, 0);
    let mut bytes = s.contents();
    let r = e0.commit.descriptor;
    assert_eq!(r.offset, 0);
    assert_eq!(desc.len() as u64, r.stored_len, "same-length descriptor");
    bytes[..desc.len()].copy_from_slice(desc);
    bytes.truncate(e0.commit_offset as usize);
    let mut f = Forge::new(bytes);
    let mut rec = e0.commit.clone();
    rec.descriptor.stored_hash =
        stored_object_hash(mochi_format::repr::StoredObject::from_loaded(desc.to_vec()).view());
    f.append_commit(&rec);
    f
}

/// A one-commit archive created TAR-compatible by the writer (D19).
fn tar_archive() -> (SimStorage, Vec<Step>) {
    let steps = history()[..1].to_vec();
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), with_profile(Some(TAR)))
        .unwrap();
    w.commit(steps[0].tx.clone(), &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let o = open_head(&s, &opts()).unwrap();
    assert!(o.descriptor.tar_compatible);
    assert_eq!(o.descriptor.profile(), TAR);
    (s, steps)
}

/// **Checklist DoD (profile).** Enabling encryption in place is
/// `PROFILE_CHANGE_UNSUPPORTED` and writes nothing; the same append with the
/// archive's own profile, or none, proceeds.
#[test]
fn t18_enabling_encryption_in_place_is_refused() {
    let (s, h, steps) = fixture();
    let before = s.contents();
    for tail in [TailPolicy::Refuse, TailPolicy::TruncateWithoutQuarantine] {
        let e = append_err(&s, with_profile(Some(ENCRYPT)), tail);
        assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported, "{e}");
        assert!(e.message.contains("Encrypted"), "{e}");
        assert_eq!(s.contents(), before);
    }
    // Both changes at once are named.
    let both = Profile {
        tar_compatible: true,
        encrypted: true,
    };
    let e = append_err(&s, with_profile(Some(both)), TailPolicy::Refuse);
    assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported);
    assert!(
        e.message.contains("Encrypted") && e.message.contains("TAR"),
        "{e}"
    );

    // With an uncommitted tail, the refusal comes first and keeps the tail.
    let mut bytes = before.clone();
    add_uncommitted_tail(&mut bytes, &h[8]);
    let t = SimStorage::from_bytes(bytes.clone());
    let e = append_err(
        &t,
        with_profile(Some(ENCRYPT)),
        TailPolicy::TruncateWithoutQuarantine,
    );
    assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported);
    assert_eq!(t.contents(), bytes);

    // The lock was released, and the archive's own profile is accepted.
    for p in [None, Some(Profile::default())] {
        let (w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(SeqIds::new(9100)),
            with_profile(p),
            TailPolicy::Refuse,
        )
        .unwrap();
        drop(w);
    }
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_state(&s, &head).unwrap(), steps[8].after);
}

/// A profile this build cannot write (Encrypted) is refused at creation as
/// unsupported (exit 4), not as a profile change, and nothing is written.
/// TAR compatibility is writable (D19).
#[test]
fn t18_create_with_an_unusable_profile_request_is_refused_and_leaves_nothing() {
    let s = SimStorage::new();
    let e = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        with_profile(Some(ENCRYPT)),
    )
    .map(|_| ())
    .unwrap_err();
    // The Encrypted profile is writable (D20), but only with a passphrase to
    // wrap its data key; and never together with the TAR constraint.
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(s.contents().is_empty());
    let both = Profile {
        tar_compatible: true,
        encrypted: true,
    };
    let e = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        with_profile(Some(both)),
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(s.contents().is_empty());
    // The lock was released.
    ArchiveWriter::create(s, Box::new(SeqIds::new(1)), test_options()).unwrap();
    for p in [Profile::default(), TAR] {
        ArchiveWriter::create(
            SimStorage::new(),
            Box::new(SeqIds::new(1)),
            with_profile(Some(p)),
        )
        .unwrap();
    }
}

/// On a TAR-compatible archive: turning TAR compatibility off is a profile
/// change (refused before a tail is truncated); appending with the archive's
/// own profile, or none, proceeds. Reading is unaffected.
#[test]
fn t18_tar_archive_profile_change_and_append() {
    let (s, steps) = tar_archive();
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(read_state(&s, &head).unwrap(), steps[0].after);
    let core = Profile::default();
    let before = s.contents();
    assert_eq!(
        append_err(&s, with_profile(Some(core)), TailPolicy::Refuse).code,
        ErrorCode::ProfileChangeUnsupported
    );
    assert_eq!(s.contents(), before);
    for p in [None, Some(TAR)] {
        let (w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(SeqIds::new(9100)),
            with_profile(p),
            TailPolicy::Refuse,
        )
        .unwrap();
        drop(w);
    }
    let h = commit_history(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    add_uncommitted_tail(&mut bytes, &h[0]);
    let t = SimStorage::from_bytes(bytes.clone());
    let e = append_err(
        &t,
        with_profile(Some(core)),
        TailPolicy::TruncateWithoutQuarantine,
    );
    assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported);
    assert_eq!(t.contents(), bytes, "the tail was not truncated");
}

// ---- fuzz exerciser -------------------------------------------------------------------

/// The `archive_open` exerciser (which runs the assessment and checks its
/// descriptor verdict against opening) accepts every T18 fixture.
#[test]
fn t18_fixtures_pass_the_archive_open_exerciser() {
    let (s, h, _) = fixture();
    let mut damaged = s.contents();
    damage(&mut damaged, h[0].commit.descriptor);
    let mut missing = s.contents();
    missing[..4].copy_from_slice(&[0; 4]);
    let (mismatch, _) = mismatched_segment();
    let (tar, _) = tar_archive();
    for bytes in [
        s.contents(),
        damaged,
        missing,
        mismatch.contents(),
        tar.contents(),
    ] {
        mochi_testkit::fuzz::exercise_archive_open(&bytes);
    }
}
