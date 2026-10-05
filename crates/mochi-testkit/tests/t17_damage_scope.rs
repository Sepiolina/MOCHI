//! T17: damage scope (spec Annex B.2 D10.9; gate G2).
//!
//! The damage matrix runs over deltas, images, and snapshot manifests across
//! three segments, against an **independent oracle** written from the D10.9
//! text and the fixture's known commit forms. The oracle does not call
//! `mochi_core::damage` or any open-path logic. Plan:
//! `docs/t16-t17-t15-plan.md` section 4.

#![allow(clippy::needless_range_loop)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use mochi_core::damage::{assess_damage, Effect, Readability};
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, ArchiveWriter, CatalogSource, CheckpointPolicy,
    HistoryEntry, ReadOptions, TailPolicy,
};
use mochi_core::report::{Report, SeqRange};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::ErrorCode;
use mochi_format::cbor::Value;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{read_state, test_options, Job};
use mochi_testkit::forge::{self, Forge};
use mochi_testkit::replay::{
    checkpoint_refs, damage, history_long, range, within, write, Step, Tracing,
};
use mochi_testkit::{SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

// ---- the oracle ----------------------------------------------------------------------

/// cp0 d1 d2 | cp3 d4 d5 | cp6 d7 d8 (`Every(3)`).
const CP: [bool; 9] = [true, false, false, true, false, false, true, false, false];

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Obj {
    Delta(u64),
    Image(u64),
    Snap(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Expect {
    Image,
    Snapshot,
}

fn base(h: u64) -> u64 {
    (0..=h).rev().find(|&s| CP[s as usize]).unwrap()
}

/// The last commit of the segment that starts at or contains `x`.
fn end(x: u64) -> u64 {
    (x + 1..9).find(|&s| CP[s as usize]).map_or(8, |s| s - 1)
}

type Damaged = BTreeSet<Obj>;

/// D10.9: what opening commit `h` does. Every damage here is a changed byte,
/// i.e. stored damage.
fn expected_open(h: u64, d: &Damaged) -> Result<Expect, ErrorCode> {
    let b = base(h);
    // A damaged delta in the segment (b, h] breaks it. The checkpoint's own
    // delta (b itself) is in no segment.
    if (b + 1..=h).any(|j| d.contains(&Obj::Delta(j))) {
        return Err(ErrorCode::StoredIntegrityFailed);
    }
    if !d.contains(&Obj::Image(b)) {
        Ok(Expect::Image)
    } else if !d.contains(&Obj::Snap(b)) {
        Ok(Expect::Snapshot)
    } else {
        Err(ErrorCode::StoredIntegrityFailed)
    }
}

fn expected_recoverability(h: u64, d: &Damaged) -> Status {
    match expected_open(h, d) {
        Err(_) => Status::Fail,
        Ok(Expect::Snapshot) => Status::Degraded,
        Ok(Expect::Image) if d.contains(&Obj::Snap(base(h))) => Status::Degraded,
        Ok(Expect::Image) => Status::Pass,
    }
}

fn expected_ranges(d: &Damaged) -> Vec<(u64, u64, Effect)> {
    let mut out: Vec<_> = d
        .iter()
        .map(|o| match *o {
            Obj::Delta(j) if CP[j as usize] => (j, j, Effect::NoReadAffected),
            Obj::Delta(j) => (j, end(j), Effect::Unreadable),
            Obj::Image(c) if !d.contains(&Obj::Snap(c)) => (c, end(c), Effect::ReadsFromSnapshot),
            Obj::Image(c) => (c, end(c), Effect::Unreadable),
            Obj::Snap(c) if !d.contains(&Obj::Image(c)) => (c, end(c), Effect::Degraded),
            Obj::Snap(c) => (c, end(c), Effect::Unreadable),
        })
        .collect();
    out.sort_by_key(|r| (r.0, r.1, format!("{:?}", r.2)));
    out
}

fn fixture() -> (SimStorage, Vec<HistoryEntry>, Vec<Step>) {
    let steps = history_long();
    let s = write(CheckpointPolicy::Every(3), &steps);
    let h = commit_history(&s, &opts()).unwrap();
    let forms: Vec<bool> = h
        .iter()
        .map(|e| e.commit.metadata.is_checkpoint())
        .collect();
    assert_eq!(forms, CP);
    (s, h, steps)
}

fn apply(bytes: &mut [u8], h: &[HistoryEntry], d: &Damaged) {
    for o in d {
        match *o {
            Obj::Delta(j) => damage(bytes, h[j as usize].commit.delta_manifest),
            Obj::Image(c) => damage(bytes, checkpoint_refs(&h[c as usize]).0),
            Obj::Snap(c) => damage(bytes, checkpoint_refs(&h[c as usize]).1),
        }
    }
}

/// Mismatches between the oracle and the code for one damage set, over every
/// head. Each line names the case, the head, and what each side said.
fn check_case(s: &SimStorage, h: &[HistoryEntry], steps: &[Step], d: &Damaged) -> Vec<String> {
    let mut bytes = s.contents();
    apply(&mut bytes, h, d);
    let damaged = SimStorage::from_bytes(bytes);
    let mut bad = Vec::new();
    let job = Job::new();
    let report = assess_damage(&damaged, &opts(), &job.ctx()).unwrap();

    for (i, e) in h.iter().enumerate() {
        let hh = i as u64;
        let want = expected_open(hh, d);
        match (open_at_footer(&damaged, e.footer_offset, &opts()), want) {
            (Ok(o), Ok(w)) => {
                let src = match o.catalog_source {
                    CatalogSource::Image => Expect::Image,
                    CatalogSource::SnapshotManifest { .. } => Expect::Snapshot,
                };
                if src != w {
                    bad.push(format!("{d:?} head {hh}: opened from {src:?}, want {w:?}"));
                }
                match read_state(&damaged, &o) {
                    Ok(st) if st == steps[i].after => {}
                    other => bad.push(format!("{d:?} head {hh}: state differs ({other:?})")),
                }
            }
            (Err(e), Err(code)) => {
                if e.code != code {
                    bad.push(format!(
                        "{d:?} head {hh}: error {:?}, want {code:?}",
                        e.code
                    ));
                }
            }
            (got, want) => bad.push(format!(
                "{d:?} head {hh}: open {:?}, oracle {want:?}",
                got.map(|_| "Ok")
            )),
        }
        let scope = &report.commits[i];
        let readable_ok = match (&scope.readable, want) {
            (Readability::Image, Ok(Expect::Image)) => true,
            (Readability::SnapshotRebuild, Ok(Expect::Snapshot)) => true,
            (Readability::Unreadable { code }, Err(c)) => *code == c,
            _ => false,
        };
        if !readable_ok {
            bad.push(format!(
                "{d:?} head {hh}: assessment says {:?}, oracle {want:?}",
                scope.readable
            ));
        }
        let want_rec = expected_recoverability(hh, d);
        if scope.recoverability != want_rec {
            bad.push(format!(
                "{d:?} head {hh}: recoverability {:?}, oracle {want_rec:?}",
                scope.recoverability
            ));
        }
    }

    let mut got: Vec<_> = report
        .ranges
        .iter()
        .map(|r| (r.first, r.last, r.effect))
        .collect();
    got.sort_by_key(|r| (r.0, r.1, format!("{:?}", r.2)));
    if got != expected_ranges(d) {
        bad.push(format!(
            "{d:?}: ranges {got:?}, oracle {:?}",
            expected_ranges(d)
        ));
    }
    if report.objects.len() != d.len() {
        bad.push(format!("{d:?}: {} objects reported", report.objects.len()));
    }
    let want_integrity = if d.is_empty() {
        Status::Pass
    } else {
        Status::Fail
    };
    if report.integrity() != want_integrity {
        bad.push(format!("{d:?}: integrity {:?}", report.integrity()));
    }
    let worst = (0..9u64)
        .map(|x| expected_recoverability(x, d))
        .fold(Status::Pass, |a, b| match (a, b) {
            (Status::Fail, _) | (_, Status::Fail) => Status::Fail,
            (Status::Degraded, _) | (_, Status::Degraded) => Status::Degraded,
            _ => Status::Pass,
        });
    if report.recoverability() != worst {
        bad.push(format!(
            "{d:?}: archive recoverability {:?}",
            report.recoverability()
        ));
    }
    if report.head_recoverability() != expected_recoverability(8, d) {
        bad.push(format!(
            "{d:?}: head recoverability {:?}",
            report.head_recoverability()
        ));
    }
    bad
}

/// **Checklist DoD.** 18 single-segment damage cases against D10.9, over
/// every head.
#[test]
fn t17_damage_matrix_matches_d10_9() {
    let (s, h, steps) = fixture();
    let mut cases: Vec<Damaged> = Vec::new();
    for j in 0..9 {
        cases.push([Obj::Delta(j)].into());
    }
    for c in [0, 3, 6] {
        cases.push([Obj::Image(c)].into());
        cases.push([Obj::Snap(c)].into());
        cases.push([Obj::Image(c), Obj::Snap(c)].into());
    }
    assert_eq!(cases.len(), 9 + 9);
    let mut bad = Vec::new();
    for d in &cases {
        bad.extend(check_case(&s, &h, &steps, d));
    }
    assert!(
        bad.is_empty(),
        "damage matrix mismatches:\n{}",
        bad.join("\n")
    );
}

/// Damage in more than one segment at once.
#[test]
fn t17_two_damages_in_different_segments() {
    let (s, h, steps) = fixture();
    let mut bad = Vec::new();
    for d in [
        Damaged::from([Obj::Delta(1), Obj::Snap(6)]),
        Damaged::from([Obj::Image(0), Obj::Delta(4)]),
        Damaged::from([Obj::Image(3), Obj::Snap(3), Obj::Delta(7)]),
        Damaged::from([Obj::Snap(0), Obj::Snap(3), Obj::Snap(6)]),
    ] {
        bad.extend(check_case(&s, &h, &steps, &d));
    }
    assert!(bad.is_empty(), "mismatches:\n{}", bad.join("\n"));
}

#[test]
fn t17_undamaged_control() {
    let (s, h, steps) = fixture();
    assert!(check_case(&s, &h, &steps, &Damaged::new()).is_empty());
    let job = Job::new();
    let r = assess_damage(&s, &opts(), &job.ctx()).unwrap();
    assert!(r.objects.is_empty() && r.ranges.is_empty());
    assert!(r.commits.iter().all(|c| c.readable == Readability::Image
        && c.recoverability == Status::Pass
        && c.causes.is_empty()));
    assert_eq!(r.integrity(), Status::Pass);
    assert_eq!(r.recoverability(), Status::Pass);
    assert_eq!(r.head_recoverability(), Status::Pass);
    // 9 delta manifests, a snapshot and an image for each of 3 checkpoints,
    // and the one descriptor every commit references (T18).
    assert_eq!(r.objects_checked, 9 + 2 * 3 + 1);
    assert_eq!(r.head_seq, 8);
}

// ---- named D10.9 clauses -------------------------------------------------------------

#[test]
fn t17_damaged_snapshot_reads_continue_degraded() {
    let (s, h, steps) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).1);
    let damaged = SimStorage::from_bytes(bytes);
    for i in 3..=5 {
        let o = open_at_footer(&damaged, h[i].footer_offset, &opts()).unwrap();
        assert_eq!(o.catalog_source, CatalogSource::Image);
        assert_eq!(read_state(&damaged, &o).unwrap(), steps[i].after);
    }
    let job = Job::new();
    let r = assess_damage(&damaged, &opts(), &job.ctx()).unwrap();
    for (i, c) in r.commits.iter().enumerate() {
        let want = if (3..=5).contains(&i) {
            Status::Degraded
        } else {
            Status::Pass
        };
        assert_eq!(c.recoverability, want, "commit {i}");
    }
    assert_eq!(r.ranges.len(), 1);
    assert_eq!(
        (r.ranges[0].first, r.ranges[0].last, r.ranges[0].effect),
        (3, 5, Effect::Degraded)
    );

    // Reported honestly: a report built from it passes validation, and its
    // overall status is not PASS (an object failed its check).
    let findings = r.findings();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].affected, Some(SeqRange { first: 3, last: 5 }));
    assert_eq!(findings[0].code, ErrorCode::StoredIntegrityFailed);
    let mut report = Report::new(VerificationLevel::StoredIntegrity);
    report.findings = findings;
    report
        .dimensions
        .insert(Dimension::Integrity, r.integrity());
    report
        .dimensions
        .insert(Dimension::Recoverability, r.recoverability());
    report.overall_status = Status::Fail;
    assert_eq!(report.dimensions[&Dimension::Integrity], Status::Fail);
    assert_eq!(
        report.dimensions[&Dimension::Recoverability],
        Status::Degraded
    );
    report.validate().unwrap();
    report.overall_status = Status::Pass;
    assert!(report.validate().is_err(), "PASS must not hide this");
}

#[test]
fn t17_damaged_image_reads_rebuild_from_snapshot() {
    let (s, h, steps) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).0);
    let damaged = SimStorage::from_bytes(bytes);
    for i in 3..=5 {
        let o = open_at_footer(&damaged, h[i].footer_offset, &opts()).unwrap();
        match &o.catalog_source {
            CatalogSource::SnapshotManifest { image_error } => {
                assert_eq!(image_error.code, ErrorCode::StoredIntegrityFailed)
            }
            other => panic!("head {i}: {other:?}"),
        }
        assert_eq!(read_state(&damaged, &o).unwrap(), steps[i].after);
    }
    // Head 2 still reads from image 0's segment.
    let o = open_at_footer(&damaged, h[2].footer_offset, &opts()).unwrap();
    assert_eq!(o.catalog_source, CatalogSource::Image);
}

/// Q31: a checkpoint's own delta manifest is in no replay segment.
#[test]
fn t17_checkpoint_own_delta_damage_does_not_affect_reads() {
    let (s, h, steps) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, h[3].commit.delta_manifest);
    let damaged = SimStorage::from_bytes(bytes.clone());

    let o = open_at_footer(&damaged, h[3].footer_offset, &opts()).unwrap();
    assert!(o.manifest.is_none());
    assert_eq!(
        o.manifest_error.as_ref().map(|e| e.code),
        Some(ErrorCode::StoredIntegrityFailed)
    );
    assert_eq!(read_state(&damaged, &o).unwrap(), steps[3].after);
    for i in [4, 5] {
        let o = open_at_footer(&damaged, h[i].footer_offset, &opts()).unwrap();
        assert!(o.manifest.is_some() && o.manifest_error.is_none());
        assert_eq!(read_state(&damaged, &o).unwrap(), steps[i].after);
    }

    let job = Job::new();
    let r = assess_damage(&damaged, &opts(), &job.ctx()).unwrap();
    assert_eq!(
        r.ranges
            .iter()
            .map(|x| (x.first, x.last, x.effect))
            .collect::<Vec<_>>(),
        [(3, 3, Effect::NoReadAffected)]
    );
    assert!(r.commits.iter().all(|c| c.recoverability == Status::Pass));
    assert_eq!(r.integrity(), Status::Fail, "the object finding stays");

    // Append is not so tolerant (Q34): refused, nothing written.
    let upto = (h[3].footer_offset + FOOTER_FRAME_LEN) as usize;
    let at3 = SimStorage::from_bytes(bytes[..upto].to_vec());
    let e = ArchiveWriter::open_append(
        at3.clone(),
        Box::new(SeqIds::new(5000)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    assert_eq!(at3.contents(), bytes[..upto]);
}

/// A hash-valid object that is *invalid* is not damage (Q31/Q32): its own
/// failure is refused as before, and nothing stands in for it.
#[test]
fn t17_invalid_record_is_not_treated_as_damage() {
    // A real checkpoint 1 whose own delta manifest is replaced by one with an
    // unknown required feature: hash-valid, so STORED_INTEGRITY_FAILED is
    // not what it is.
    let steps = history_long();
    let s = write(CheckpointPolicy::EveryCommit, &steps[..2]);
    let h = commit_history(&s, &opts()).unwrap();
    let real = open_at_footer(&s, h[1].footer_offset, &opts()).unwrap();
    let delta = real.manifest.clone().unwrap();
    let mut f = Forge::new(s.contents());
    f.bytes.truncate(h[1].commit_offset as usize);
    let r = f.append_manifest_edited(&delta, |v| {
        *forge::field(v, 9) = Value::Array(vec![Value::Uint(1)]);
    });
    let mut rec = h[1].commit.clone();
    rec.delta_manifest = r;
    let forged = f.append_commit(&rec);
    let fs = f.storage();

    let e = open_at_footer(&fs, forged.footer_offset, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFeature, "no tolerance: {e}");
    // Commit 0 is untouched.
    assert!(open_at_footer(&fs, h[0].footer_offset, &opts()).is_ok());

    let job = Job::new();
    let rep = assess_damage(&fs, &opts(), &job.ctx()).unwrap();
    assert_eq!(rep.objects.len(), 1);
    assert_eq!(rep.objects[0].error.code, ErrorCode::UnsupportedFeature);
    assert_eq!(
        rep.commits[1].readable,
        Readability::Unreadable {
            code: ErrorCode::UnsupportedFeature
        }
    );
    assert_eq!(rep.commits[1].recoverability, Status::Fail);
    assert_eq!(rep.commits[0].recoverability, Status::Pass);
    assert_eq!(
        (
            rep.ranges[0].first,
            rep.ranges[0].last,
            rep.ranges[0].effect
        ),
        (1, 1, Effect::Unreadable)
    );
}

/// The frozen bare-image archive's images are refused at the envelope as
/// `UNSUPPORTED_FEATURE` (checklist Q12). That is not stored damage, so the
/// intact snapshot manifest must not stand in for it.
#[test]
fn t17_unsupported_image_does_not_fall_back() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden/c5/reject-archive-bare-image.mochi");
    let s = SimStorage::from_bytes(std::fs::read(path).unwrap());
    assert_eq!(
        open_head(&s, &opts()).unwrap_err().code,
        ErrorCode::UnsupportedFeature
    );
    let job = Job::new();
    let r = assess_damage(&s, &opts(), &job.ctx()).unwrap();
    let head = r.commits.last().unwrap();
    assert_eq!(
        head.readable,
        Readability::Unreadable {
            code: ErrorCode::UnsupportedFeature
        }
    );
    assert!(r
        .ranges
        .iter()
        .all(|x| x.effect == Effect::Unreadable && x.effect != Effect::ReadsFromSnapshot));
}

/// With the image and snapshot both gone, no earlier checkpoint is offered.
#[test]
fn t17_no_fallback_to_an_earlier_checkpoint() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).0);
    damage(&mut bytes, checkpoint_refs(&h[3]).1);
    let damaged = SimStorage::from_bytes(bytes);
    for i in 3..=5 {
        let e = open_at_footer(&damaged, h[i].footer_offset, &opts()).unwrap_err();
        assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "head {i}");
        assert!(
            e.message.contains("snapshot manifest could not stand in"),
            "names both failures: {e}"
        );
    }
    // Earlier and later segments are unaffected.
    for i in [0, 1, 2, 6, 7, 8] {
        assert!(open_at_footer(&damaged, h[i].footer_offset, &opts()).is_ok());
    }
}

#[test]
fn t17_append_refused_when_base_image_damaged() {
    let (s, h, steps) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[6]).0);
    let damaged = SimStorage::from_bytes(bytes.clone());
    let o = open_head(&damaged, &opts()).unwrap();
    assert_eq!(o.seq(), 8);
    assert!(matches!(
        o.catalog_source,
        CatalogSource::SnapshotManifest { .. }
    ));
    assert_eq!(read_state(&damaged, &o).unwrap(), steps[8].after);
    let e = ArchiveWriter::open_append(
        damaged.clone(),
        Box::new(SeqIds::new(6000)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    assert_eq!(damaged.contents(), bytes, "nothing written");
}

/// The snapshot manifest is read only after the image fails.
#[test]
fn t17_fallback_reads_s_b_only_after_the_image_fails() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    let (image3, snap3) = checkpoint_refs(&h[3]);
    damage(&mut bytes, image3);
    let damaged = SimStorage::from_bytes(bytes);
    let t = Tracing {
        inner: &damaged,
        reads: Default::default(),
    };
    open_at_footer(&t, h[5].footer_offset, &opts()).unwrap();

    let mut allowed = vec![range(h[0].commit.descriptor)];
    for e in &h[3..=5] {
        allowed.push((e.commit_offset, e.footer_offset + FOOTER_FRAME_LEN));
    }
    for e in &h[4..=5] {
        allowed.push(range(e.commit.delta_manifest));
    }
    allowed.push(range(image3));
    allowed.push(range(snap3));
    let reads = t.reads.into_inner();
    for r in &reads {
        assert!(
            within(*r, &allowed),
            "read {r:?} outside the segment's inputs"
        );
    }
    let first = |(lo, hi): (u64, u64)| reads.iter().position(|r| r.0 < hi && lo < r.0 + r.1);
    let (i_img, i_snap) = (first(range(image3)), first(range(snap3)));
    assert!(i_img.is_some() && i_snap.is_some());
    assert!(i_img < i_snap, "S(b) is read after the image, not before");
    // And nothing of the base's own delta manifest.
    assert!(first(range(h[3].commit.delta_manifest)).is_none());
}

#[test]
fn t17_assessment_is_read_only_and_cancellable() {
    let (s, h, _) = fixture();
    let mut bytes = s.contents();
    damage(&mut bytes, checkpoint_refs(&h[3]).1);
    let damaged = SimStorage::from_bytes(bytes);
    let before = blake3::hash(&damaged.contents());
    let job = Job::new();
    assess_damage(&damaged, &opts(), &job.ctx()).unwrap();
    assert_eq!(blake3::hash(&damaged.contents()), before);

    let cancelled = Job::new();
    cancelled.cancel.cancel();
    let e = assess_damage(&damaged, &opts(), &cancelled.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
}
