//! C5 golden vectors (R8).
//!
//! * `c5/*.bin`: commit records, byte-exact against their builders
//!   (canonical encoding makes that meaningful), each with its outcome.
//! * `c5/valid-archive-3-commits.mochi`: a whole schema-1 archive that
//!   **must keep opening** to the scripted states at every commit. Like the C3 catalog
//!   image it is not compared with a fresh build byte for byte: it contains
//!   zstd and SQLite output, which may change between library versions
//!   without any format change. Losing the ability to read it would be one.
//! * `c5/reject-archive-legacy-v0.mochi`: the schema-0 archive checked in at
//!   C5, frozen (never regenerated). It must keep being **refused as legacy**
//!   (§26: identify legacy formats explicitly), never misread as damage.
//! * `c5/reject-archive-bare-image.mochi`: the schema-1 archive checked in at
//!   T8/T9, before T10, frozen. Its images are bare SQLite, not binary
//!   envelope v0 (Annex B.2.2). Head discovery and the commit chain still
//!   work; opening is refused at the envelope, before SQLite sees the image.
//! * `c5/valid-archive-delta-segment.mochi`, `c5/valid-archive-wrong-base-hint.mochi`,
//!   and `c5/reject-archive-*.mochi` other than the two above: the T11/T12
//!   archive vectors (`mochi_testkit::golden::c5_archive_vectors`), listed in
//!   `vectors.txt` with their expected outcome. Frozen when first written
//!   (`write_c5_archive_vectors` refuses to overwrite); both the frozen
//!   files and fresh builds must meet the listed expectation.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::commit::CommitRecord;
use mochi_core::publish::{commit_history, open_at_footer, open_head, ReadOptions};
use mochi_format::cbor::CborLimits;
use mochi_format::Limits;
use mochi_testkit::archive::{build, read_state, scripted_history};
use mochi_testkit::fuzz::{exercise_archive_open, exercise_commit, ArchiveOpenOutcome};
use mochi_testkit::golden::{
    c5_archive_vectors, c5_commit_vectors, c5_segment_history, render_c5_manifest, ArchiveExpect,
    ManifestExpect,
};
use mochi_testkit::SimStorage;

const ARCHIVE: &str = "valid-archive-3-commits.mochi";
const LEGACY_ARCHIVE: &str = "reject-archive-legacy-v0.mochi";
const BARE_IMAGE_ARCHIVE: &str = "reject-archive-bare-image.mochi";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c5")
}

#[test]
fn commit_vectors_behave_as_expected() {
    for v in c5_commit_vectors() {
        let got = CommitRecord::from_stored(&v.bytes, &Limits::default(), &CborLimits::default());
        match (&v.expect, got) {
            (ManifestExpect::Valid, Ok((r, _))) => {
                assert_eq!(
                    r.to_stored().unwrap().0.as_bytes(),
                    &v.bytes[..],
                    "{}",
                    v.name
                )
            }
            (ManifestExpect::Rejected(code), Err(e)) => {
                assert_eq!(e.code.as_str(), *code, "{}: {}", v.name, e.message)
            }
            (want, got) => panic!("{}: expected {want:?}, got {got:?}", v.name),
        }
    }
}

#[test]
fn checked_in_files_match_the_builders() {
    let manifest = std::fs::read_to_string(dir().join("vectors.txt"))
        .expect("vectors.txt missing (run write_c5_golden_files deliberately)");
    assert_eq!(manifest, render_c5_manifest());
    let mut known = vec![
        "vectors.txt".to_string(),
        ARCHIVE.to_string(),
        LEGACY_ARCHIVE.to_string(),
        BARE_IMAGE_ARCHIVE.to_string(),
    ];
    for v in c5_commit_vectors() {
        let name = format!("{}.bin", v.name);
        assert_eq!(std::fs::read(dir().join(&name)).unwrap(), v.bytes, "{name}");
        known.push(name);
    }
    // Archive vectors are frozen and checked by behaviour, not bytes.
    for v in c5_archive_vectors() {
        known.push(format!("{}.mochi", v.name));
    }
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(known.contains(&name), "unexpected golden file {name}");
    }
}

#[test]
fn the_checked_in_archive_keeps_opening_at_every_commit() {
    let bytes = std::fs::read(dir().join(ARCHIVE)).expect("archive fixture missing");
    let s = SimStorage::from_bytes(bytes);
    let steps = scripted_history();
    let opts = ReadOptions::default();
    let head = open_head(&s, &opts).unwrap();
    assert_eq!(head.seq(), 2);
    let history = commit_history(&s, &opts).unwrap();
    assert_eq!(history.len(), 3);
    for (i, h) in history.iter().enumerate() {
        let opened = open_at_footer(&s, h.footer_offset, &opts).unwrap();
        assert_eq!(
            read_state(&s, &opened).unwrap(),
            steps[i].after,
            "commit {i}"
        );
    }
}

#[test]
fn the_checked_in_archive_is_schema_1_with_a_descriptor_and_checkpoints() {
    use mochi_core::publish::read_snapshot;
    let s = SimStorage::from_bytes(std::fs::read(dir().join(ARCHIVE)).unwrap());
    let opts = ReadOptions::default();
    for h in commit_history(&s, &opts).unwrap() {
        assert!(h.commit.metadata.is_checkpoint());
        assert_eq!(h.commit.descriptor.offset, 0);
        let opened = open_at_footer(&s, h.footer_offset, &opts).unwrap();
        assert_eq!(opened.descriptor.archive_id, h.commit.archive_id);
        assert!(!opened.descriptor.tar_compatible);
        let snap = read_snapshot(&s, &opened, &opts).unwrap();
        assert_eq!(snap.identity(), h.commit.identity());
    }
}

/// §26: the schema-0 archive is refused, explicitly as legacy, with
/// UNSUPPORTED_FEATURE (exit 4). Head discovery still works on it.
#[test]
fn the_legacy_v0_archive_is_refused_as_legacy() {
    let s = SimStorage::from_bytes(std::fs::read(dir().join(LEGACY_ARCHIVE)).unwrap());
    let opts = ReadOptions::default();
    let loc = mochi_core::publish::locate_head(&s, &opts.limits).unwrap();
    assert!(loc.tail.is_clean());
    let e = open_head(&s, &opts).unwrap_err();
    assert_eq!(e.code, mochi_core::ErrorCode::UnsupportedFeature, "{e}");
    assert!(e.message.contains("legacy"), "{}", e.message);
}

/// T10: an image that is not in a binary envelope is never handed to
/// SQLite. Read as an envelope, a bare image's bytes 4..6 ("te") give
/// version 0x6574: unknown envelope version, refused as unsupported (D11
/// "Schema version: unknown version: refuse"). Commit records are unaffected.
#[test]
fn the_pre_envelope_archive_is_refused_at_the_image() {
    let s = SimStorage::from_bytes(std::fs::read(dir().join(BARE_IMAGE_ARCHIVE)).unwrap());
    let opts = ReadOptions::default();
    let loc = mochi_core::publish::locate_head(&s, &opts.limits).unwrap();
    assert!(loc.tail.is_clean());
    let history = commit_history(&s, &opts).unwrap();
    assert_eq!(history.len(), 3);
    for h in &history {
        let e = open_at_footer(&s, h.footer_offset, &opts).unwrap_err();
        assert_eq!(e.code, mochi_core::ErrorCode::UnsupportedFeature, "{e}");
        assert!(
            e.message.contains("envelope version 25972"),
            "{}",
            e.message
        );
    }
}

fn check_archive(name: &str, bytes: Vec<u8>, expect: &ArchiveExpect) {
    let s = SimStorage::from_bytes(bytes);
    let opts = ReadOptions::default();
    match expect {
        ArchiveExpect::Opens => {
            open_head(&s, &opts).unwrap_or_else(|e| panic!("{name}: must open: {e}"));
            for h in commit_history(&s, &opts).unwrap() {
                open_at_footer(&s, h.footer_offset, &opts)
                    .unwrap_or_else(|e| panic!("{name}: commit {}: {e}", h.commit.seq));
            }
        }
        ArchiveExpect::Rejected(code) => {
            let e = open_head(&s, &opts)
                .map(|_| ())
                .expect_err(&format!("{name}: must not open"));
            assert_eq!(e.code.as_str(), *code, "{name}: {e}");
        }
    }
}

/// T11/T12 archive vectors: the frozen file and a fresh build both meet
/// the expectation listed in `vectors.txt`.
#[test]
fn archive_vectors_behave_as_expected() {
    for v in c5_archive_vectors() {
        let file = format!("{}.mochi", v.name);
        let frozen = std::fs::read(dir().join(&file)).unwrap_or_else(|_| {
            panic!("{file} missing (run write_c5_archive_vectors deliberately)")
        });
        check_archive(&format!("{file} (frozen)"), frozen, &v.expect);
        check_archive(&format!("{file} (fresh build)"), v.bytes, &v.expect);
    }
}

/// The delta-segment vector: checkpoint, three deltas, checkpoint, one
/// delta, each commit opening to the scripted state, each delta's segment
/// base the one the rule names.
#[test]
fn the_delta_segment_archive_opens_every_commit_through_its_segment() {
    use mochi_core::commit::Metadata;
    let s = SimStorage::from_bytes(
        std::fs::read(dir().join("valid-archive-delta-segment.mochi")).unwrap(),
    );
    let opts = ReadOptions::default();
    let steps = c5_segment_history();
    let history = commit_history(&s, &opts).unwrap();
    let kinds: Vec<bool> = history
        .iter()
        .map(|h| h.commit.metadata.is_checkpoint())
        .collect();
    assert_eq!(kinds, [true, false, false, false, true, false]);
    for (i, h) in history.iter().enumerate() {
        let opened = open_at_footer(&s, h.footer_offset, &opts).unwrap();
        assert_eq!(
            read_state(&s, &opened).unwrap(),
            steps[i].after,
            "commit {i}"
        );
        let base = match h.commit.metadata {
            Metadata::Checkpoint { .. } => i as u64,
            Metadata::Delta { base } => base.seq,
        };
        assert_eq!(opened.segment.base_seq, base, "commit {i}");
        assert_eq!(base, if i < 4 { 0 } else { 4 }, "commit {i}");
    }
}

/// The wrong-base-hint vector (decision 17): opens, the mismatch reported.
#[test]
fn the_wrong_base_hint_archive_opens_and_reports_the_hint() {
    let s = SimStorage::from_bytes(
        std::fs::read(dir().join("valid-archive-wrong-base-hint.mochi")).unwrap(),
    );
    let opts = ReadOptions::default();
    let head = open_head(&s, &opts).unwrap();
    assert_eq!(head.seq(), 2);
    assert_eq!(head.segment.base_seq, 0);
    let real = commit_history(&s, &opts).unwrap()[0].footer_offset;
    assert_eq!(head.segment.base_footer_offset, real);
    assert_eq!(head.segment.base_hint_mismatch, Some(real + 1));
    assert_eq!(
        read_state(&s, &head).unwrap(),
        c5_segment_history()[1].after
    );
}

/// Writes the T11/T12 archive vectors that do not exist yet, and
/// `vectors.txt`. Never overwrites: an existing archive file is left
/// untouched (`create_new`; existing vectors are never regenerated,
/// acceptance "Fixtures"), and a new one is created only if absent.
#[test]
#[ignore = "writes new fixtures; run by hand and review the diff"]
fn write_c5_archive_vectors() {
    use std::io::Write;
    for v in c5_archive_vectors() {
        let path = dir().join(format!("{}.mochi", v.name));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => f.write_all(&v.bytes).unwrap(),
            // Frozen: an existing vector is never rewritten.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
    std::fs::write(dir().join("vectors.txt"), render_c5_manifest()).unwrap();
}

#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_c5_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    std::fs::write(dir().join("vectors.txt"), render_c5_manifest()).unwrap();
    for v in c5_commit_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
    }
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    std::fs::write(dir().join(ARCHIVE), s.contents()).unwrap();
}

/// Stable-Rust stand-in for the `commit_decode` and `archive_open` targets.
#[test]
fn fuzz_smoke_over_mutated_inputs() {
    let mut state = 0x5EED_0005u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut seeds: Vec<Vec<u8>> = c5_commit_vectors().into_iter().map(|v| v.bytes).collect();
    let small = SimStorage::new();
    build(small.clone(), 7, &scripted_history()[..1]).unwrap();
    let archive = small.contents();
    for seed in &seeds {
        for _ in 0..300 {
            let mut d = seed.clone();
            for _ in 0..(1 + next() % 4) {
                let i = (next() as usize) % d.len();
                d[i] ^= 1 << (next() % 8);
            }
            exercise_commit(&d);
            exercise_commit(&d[8.min(d.len())..]);
        }
    }
    // Archive mutations: bit flips anywhere, plus truncations, on a
    // checkpoint-only archive and on the T11/T12 delta-segment vector (so
    // `exercise_archive_open`'s segment assertions run on delta heads).
    let segment = std::fs::read(dir().join("valid-archive-delta-segment.mochi")).unwrap();
    // Every T11/T12 archive vector, unmutated, with its outcome asserted:
    // both valid vectors have delta heads, so they must come back
    // `OpenedDelta`, which proves the exerciser's segment assertions ran
    // (and held); every reject must be refused. This runs in CI's test job.
    let mut deltas_checked = 0;
    for v in c5_archive_vectors() {
        let bytes = std::fs::read(dir().join(format!("{}.mochi", v.name))).unwrap();
        let got = exercise_archive_open(&bytes);
        let want = match v.expect {
            ArchiveExpect::Opens => ArchiveOpenOutcome::OpenedDelta,
            ArchiveExpect::Rejected(_) => ArchiveOpenOutcome::Refused,
        };
        assert_eq!(got, want, "{}", v.name);
        if got == ArchiveOpenOutcome::OpenedDelta {
            deltas_checked += 1;
        }
    }
    assert!(
        deltas_checked >= 2,
        "segment assertions reached on {deltas_checked} delta heads"
    );
    for archive in [&archive, &segment] {
        for _ in 0..300 {
            let mut d = archive.clone();
            let i = (next() as usize) % d.len();
            d[i] ^= 1 << (next() % 8);
            exercise_archive_open(&d);
            let cut = (next() as usize) % d.len();
            exercise_archive_open(&d[..cut]);
        }
    }
    seeds.clear();
}

/// G1 (T30): the D11 binding vectors fail on the rule they name, not on an
/// earlier one. Each frozen file's refusal message names its fault.
#[test]
fn binding_vectors_fail_on_their_own_rule() {
    for (name, fragment) in [
        (
            "reject-archive-segment-descriptor-differs-head-valid",
            "commit 2 references a different archive descriptor",
        ),
        ("reject-archive-manifest-archive-id", "ArchiveIdMismatch"),
        (
            "reject-archive-manifest-transaction-id",
            "TransactionIdMismatch",
        ),
        ("reject-archive-manifest-sequence", "SequenceMismatch"),
        ("reject-archive-manifest-hash", "delta manifest at offset"),
        (
            "reject-archive-descriptor-hash",
            "archive descriptor at offset 0: stored-object hash mismatch",
        ),
        (
            "reject-archive-descriptor-archive-id",
            "names a different archive",
        ),
    ] {
        let s = SimStorage::from_bytes(std::fs::read(dir().join(format!("{name}.mochi"))).unwrap());
        let e = open_head(&s, &ReadOptions::default())
            .map(|_| ())
            .unwrap_err();
        assert!(e.message.contains(fragment), "{name}: {e}");
    }
}
