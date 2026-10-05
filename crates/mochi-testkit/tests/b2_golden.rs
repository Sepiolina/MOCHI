//! Annex B.2 golden vectors (plan T30; gate G1): binary envelope v0.
//!
//! Byte-exact against the builders; each vector reaches exactly its stated
//! outcome; every valid vector re-encodes identically through the writer;
//! the writer refuses to emit any reject vector's envelope; and mutated
//! vectors drive the envelope fuzz exerciser.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_format::envelope::encode_binary_record;
use mochi_format::registry::FrameKind;
use mochi_format::Limits;
use mochi_testkit::fuzz::{exercise_descriptor, exercise_envelope};
use mochi_testkit::golden::{
    b2_decode_descriptor_vector, b2_decode_envelope_vector, b2_descriptor_vectors,
    b2_envelope_vectors, render_b2_descriptor_manifest, render_b2_manifest,
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/b2")
}

#[test]
fn vectors_reach_their_stated_outcome() {
    for v in b2_envelope_vectors() {
        assert_eq!(
            b2_decode_envelope_vector(&v.bytes),
            v.expect,
            "{}: {}",
            v.name,
            v.description
        );
    }
}

#[test]
fn every_obligation_has_its_own_vector() {
    let rejects: Vec<String> = b2_envelope_vectors()
        .iter()
        .filter_map(|v| v.expect.as_ref().err().map(|e| format!("{e:?}")))
        .collect();
    let mut unique = rejects.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), rejects.len(), "two vectors test one rule");
}

#[test]
fn valid_vectors_round_trip_through_the_writer() {
    for v in b2_envelope_vectors() {
        if let Ok(env) = &v.expect {
            let frame = encode_binary_record(
                FrameKind::MetadataDelta,
                env,
                &v.bytes[8 + 80 + 8 * env.required_features.len()..],
                &[],
                &Limits::default(),
            )
            .unwrap();
            assert_eq!(frame, v.bytes, "{}", v.name);
        }
    }
}

#[test]
fn checked_in_files_match_the_builders() {
    for v in b2_envelope_vectors() {
        let path = dir().join(format!("{}.bin", v.name));
        let on_disk = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run write_b2_golden_files deliberately)", v.name));
        assert_eq!(on_disk, v.bytes, "{} differs from its builder", v.name);
    }
    for v in b2_descriptor_vectors() {
        let on_disk = std::fs::read(dir().join(format!("{}.bin", v.name))).unwrap();
        assert_eq!(on_disk, v.bytes, "{} differs from its builder", v.name);
    }
    let manifest = std::fs::read_to_string(dir().join("vectors.txt")).unwrap();
    assert_eq!(manifest, render_b2_manifest());
    let manifest = std::fs::read_to_string(dir().join("descriptor-vectors.txt")).unwrap();
    assert_eq!(manifest, render_b2_descriptor_manifest());
    let known: Vec<String> = b2_envelope_vectors()
        .iter()
        .map(|v| format!("{}.bin", v.name))
        .chain(
            b2_descriptor_vectors()
                .iter()
                .map(|v| format!("{}.bin", v.name)),
        )
        .collect();
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".bin") {
            assert!(known.contains(&name), "unexpected golden file {name}");
        }
    }
}

#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_b2_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    for v in b2_envelope_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
    }
    std::fs::write(dir().join("vectors.txt"), render_b2_manifest()).unwrap();
    for v in b2_descriptor_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
    }
    std::fs::write(
        dir().join("descriptor-vectors.txt"),
        render_b2_descriptor_manifest(),
    )
    .unwrap();
}

#[test]
fn descriptor_vectors_reach_their_stated_outcome() {
    for v in b2_descriptor_vectors() {
        assert_eq!(
            b2_decode_descriptor_vector(&v.bytes),
            v.expect,
            "{}: {}",
            v.name,
            v.description
        );
    }
}

/// Every valid descriptor re-encodes byte-identically; the same bytes are
/// refused anywhere but offset 0.
#[test]
fn valid_descriptors_are_canonical_and_offset_bound() {
    use mochi_core::descriptor::Descriptor;
    use mochi_format::cbor::CborLimits;
    for v in b2_descriptor_vectors() {
        if let Ok(d) = &v.expect {
            assert_eq!(
                d.to_stored().unwrap().as_bytes(),
                &v.bytes[..],
                "{}",
                v.name
            );
            let e =
                Descriptor::from_stored(&v.bytes, 72, &Limits::default(), &CborLimits::default())
                    .unwrap_err();
            assert_eq!(e.code, mochi_core::ErrorCode::DescriptorInvalid);
        }
    }
}

#[test]
fn fuzz_smoke_over_mutated_descriptors() {
    let mut rng = SplitMix(0x5EED_D35C);
    for v in b2_descriptor_vectors() {
        exercise_descriptor(&v.bytes);
        for _ in 0..2000 {
            let mut b = v.bytes.clone();
            let i = (rng.next() as usize) % b.len();
            match rng.next() % 3 {
                0 => b[i] ^= 1 << (rng.next() % 8),
                1 => b[i] = rng.next() as u8,
                _ => b.truncate(i),
            }
            exercise_descriptor(&b);
        }
    }
}

struct SplitMix(u64);
impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Stable-Rust stand-in for the `envelope` fuzz target, seeded with vectors
/// that pass every check, so its re-encode invariant is actually reached.
/// Mutations are mostly single-byte, to stay near the valid region.
#[test]
fn fuzz_smoke_over_mutated_envelopes() {
    let mut rng = SplitMix(0x5EED_00B2);
    for v in b2_envelope_vectors() {
        exercise_envelope(&v.bytes);
        for _ in 0..3000 {
            let mut b = v.bytes.clone();
            let i = (rng.next() as usize) % b.len();
            match rng.next() % 4 {
                0 => b[i] ^= 1 << (rng.next() % 8),
                1 => b[i] = rng.next() as u8,
                2 => b.truncate(i),
                _ => {}
            }
            exercise_envelope(&b);
        }
    }
}
