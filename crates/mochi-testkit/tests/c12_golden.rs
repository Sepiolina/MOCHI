//! C12 golden vectors (spec Annex B.2.11 D21; ratification artifact R6).
//!
//! * `c12/valid-parity-frame.bin` and `c12/reject-parity-*.bin`: the R6
//!   vectors' frames as files (fuzz seeds, and byte-exact against
//!   `docs/ratification/R6-vectors/vectors.json`).
//! * `c12/valid-archive-recovery-record.mochi`: the scripted 3-commit history
//!   with a 50 % recovery record. It **must keep verifying** clean, and its
//!   parity must keep decoding. Like the C5 archives it is not compared with
//!   a fresh build (it contains zstd and SQLite output).
//! * `c12/damaged-within-capacity.mochi`: that archive with two data shards
//!   of commit 0 (one holds the descriptor) and two of the head damaged: the
//!   parity scan must rebuild it to the valid archive **byte for byte**.
//! * `c12/damaged-beyond-capacity.mochi`: three data shards of the head's
//!   one group damaged, one more than its parity: reported, never rebuilt.
//!
//! Frozen when first written (`write_c12_golden_files` refuses to overwrite a
//! file that differs).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::descriptor::Profile;
use mochi_core::parity::scan;
use mochi_core::publish::{ArchiveWriter, CheckpointPolicy, ReadOptions, WriterOptions};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::storage::ReadStorage;
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::parity::{decode_frame, Layout};
use mochi_format::Limits;
use mochi_testkit::archive::{scripted_history, test_options, Job};
use mochi_testkit::fuzz::{exercise_all, exercise_parity_frame};
use mochi_testkit::{SeqIds, SimStorage};

const ARCHIVE: &str = "valid-archive-recovery-record.mochi";
const WITHIN: &str = "damaged-within-capacity.mochi";
const BEYOND: &str = "damaged-beyond-capacity.mochi";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c12")
}

fn vectors() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../docs/ratification/R6-vectors/vectors.json"
    ))
    .unwrap()
}

fn hex(s: &serde_json::Value) -> Vec<u8> {
    let s = s.as_str().unwrap();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// The R6 frames as named files.
fn frame_files() -> Vec<(String, Vec<u8>)> {
    let v = vectors();
    let mut out = vec![(
        "valid-parity-frame.bin".to_owned(),
        hex(&v["small_commit"]["frames"][0]),
    )];
    for r in v["rejects"].as_array().unwrap() {
        let name: String = r["name"]
            .as_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        out.push((format!("reject-parity-{name}.bin"), hex(&r["frame"])));
    }
    out
}

fn build() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(12)),
        WriterOptions {
            profile: Some(Profile {
                redundancy: Some(50),
                ..Profile::default()
            }),
            ..test_options()
        },
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    let job = Job::new();
    for st in scripted_history() {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

/// Each commit's layout, head first, from a clean scan.
fn layouts(s: &SimStorage) -> Vec<Layout> {
    let (sc, _) = scan(s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    sc.commits
        .iter()
        .map(|c| Layout::compute(c.range_offset.unwrap(), c.range_len.unwrap(), 50).unwrap())
        .collect()
}

fn hit(b: &mut [u8], l: &Layout, data_indices: &[u64]) {
    for j in data_indices {
        let (off, n) = l.shard_extent(l.shard_index(0, *j));
        assert!(n > 0);
        b[(off + n / 3) as usize] ^= 0x3C;
    }
}

fn damaged_files(valid: &SimStorage) -> Vec<(String, Vec<u8>)> {
    let ls = layouts(valid);
    let (head, first) = (ls[0], ls[ls.len() - 1]);
    let mut within = valid.contents();
    hit(&mut within, &first, &[0, 1]);
    hit(&mut within, &head, &[1, 2]);
    let mut beyond = valid.contents();
    hit(&mut beyond, &head, &[0, 1, 2]);
    vec![(WITHIN.into(), within), (BEYOND.into(), beyond)]
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn all_bytes(src: &dyn ReadStorage) -> Vec<u8> {
    let mut b = vec![0u8; src.size().unwrap() as usize];
    src.read_exact_at(0, &mut b).unwrap();
    b
}

fn full_verify(s: &SimStorage) -> mochi_core::report::Report {
    verify(
        s,
        &VerifyOptions {
            level: VerificationLevel::Restoration,
            deep: true,
            ..VerifyOptions::default()
        },
        &Job::new().ctx(),
    )
    .report
}

#[test]
fn frame_files_are_the_r6_vectors() {
    for (name, bytes) in frame_files() {
        assert_eq!(read(&name), bytes, "{name}");
        let r = decode_frame(&bytes, &[1], &Limits::default());
        assert_eq!(r.is_ok(), name.starts_with("valid-"), "{name}: {r:?}");
    }
}

#[test]
fn the_archive_keeps_verifying_and_its_parity_keeps_decoding() {
    let s = SimStorage::from_bytes(read(ARCHIVE));
    let r = full_verify(&s);
    assert_eq!(r.exit_code, 0, "{r:?}");
    assert!(r.scope.as_deref().unwrap().contains("Redundancy profile"));
    let (sc, _) = scan(&s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(sc.commits.len(), 3);
    assert!(sc.stopped.is_none());
    assert_eq!(sc.declared_percent, Some(50));
}

#[test]
fn damage_within_capacity_rebuilds_to_the_valid_archive() {
    let valid = read(ARCHIVE);
    let s = SimStorage::from_bytes(read(WITHIN));
    let (sc, view) = scan(&s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(all_bytes(&view), valid);
    assert_eq!(sc.shards_rebuilt(), 4);
    let r = full_verify(&s);
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Fail);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Degraded);
}

#[test]
fn damage_beyond_capacity_is_reported_and_left_alone() {
    let bytes = read(BEYOND);
    let s = SimStorage::from_bytes(bytes.clone());
    let (sc, view) = scan(&s, &ReadOptions::default(), &Job::new().ctx()).unwrap();
    assert_eq!(all_bytes(&view), bytes);
    assert_eq!(sc.beyond_capacity().count(), 1);
    let r = full_verify(&s);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Fail);
    assert!(r
        .findings
        .iter()
        .any(|f| f.code == ErrorCode::ParityCapacityExceeded));
}

/// The fuzz exercisers over every byte flip of the valid frame and over
/// truncations of the archive (C12's share of the deterministic fuzz smoke).
#[test]
fn fuzz_smoke_over_mutated_parity_inputs() {
    let frame = read("valid-parity-frame.bin");
    exercise_parity_frame(&frame);
    for i in 0..frame.len() {
        let mut f = frame.clone();
        f[i] ^= 0x81;
        exercise_parity_frame(&f);
    }
    for (_, f) in frame_files() {
        exercise_all(&f);
    }
    let archive = read(WITHIN);
    for cut in [
        archive.len(),
        archive.len() - 1,
        archive.len() - 72,
        archive.len() / 2,
    ] {
        exercise_all(&archive[..cut]);
    }
}

#[test]
#[ignore = "writes fixtures/golden/c12; run once, deliberately"]
fn write_c12_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    let valid = match std::fs::read(dir().join(ARCHIVE)) {
        Ok(b) => SimStorage::from_bytes(b), // frozen
        Err(_) => build(),
    };
    let mut all = frame_files();
    all.push((ARCHIVE.into(), valid.contents()));
    all.extend(damaged_files(&valid));
    for (name, bytes) in all {
        let path = dir().join(&name);
        if let Ok(old) = std::fs::read(&path) {
            if old == bytes {
                continue;
            }
            panic!("{name} exists and differs: fixtures are frozen");
        }
        std::fs::write(&path, bytes).unwrap();
    }
}
