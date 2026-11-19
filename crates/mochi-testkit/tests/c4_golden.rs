//! C4 golden vectors (R8): recovery manifests, byte-exact against their
//! builders (canonical encoding makes that meaningful), each with the outcome
//! a correct reader must reach.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::manifest::Manifest;
use mochi_format::cbor::CborLimits;
use mochi_format::repr::StoredObject;
use mochi_format::Limits;
use mochi_testkit::fuzz::exercise_manifest;
use mochi_testkit::golden::{c4_manifest_vectors, render_c4_manifest, ManifestExpect};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c4")
}

#[test]
fn manifest_vectors_behave_as_expected() {
    for v in c4_manifest_vectors() {
        let got = Manifest::from_stored(
            &StoredObject::from_loaded(v.bytes.clone()),
            &Limits::default(),
            &CborLimits::default(),
        );
        match (&v.expect, got) {
            (ManifestExpect::Valid, Ok((m, _))) => {
                assert_eq!(
                    m.to_stored().unwrap().as_bytes(),
                    &v.bytes[..],
                    "{}: re-encoding differs",
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
        .expect("vectors.txt missing (run write_c4_golden_files deliberately)");
    assert_eq!(manifest, render_c4_manifest());
    let mut known = vec!["vectors.txt".to_string()];
    for v in c4_manifest_vectors() {
        let name = format!("{}.bin", v.name);
        assert_eq!(
            std::fs::read(dir().join(&name)).unwrap(),
            v.bytes,
            "{name} differs from its builder"
        );
        known.push(name);
    }
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(known.contains(&name), "unexpected golden file {name}");
    }
}

#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_c4_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    std::fs::write(dir().join("vectors.txt"), render_c4_manifest()).unwrap();
    for v in c4_manifest_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
    }
}

/// Stable-Rust stand-in for the `manifest_decode` and `cbor_decode` targets.
#[test]
fn fuzz_smoke_over_mutated_manifests() {
    let mut state = 0x5EED_0004u64;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for v in c4_manifest_vectors() {
        exercise_manifest(&v.bytes);
        mochi_testkit::fuzz::exercise_cbor(&v.bytes[8..]);
        for _ in 0..1000 {
            let mut b = v.bytes.clone();
            for _ in 0..(1 + next() % 3) {
                let i = (next() as usize) % b.len();
                match next() % 3 {
                    0 => b[i] ^= 1 << (next() % 8),
                    1 => b[i] = next() as u8,
                    _ => b.truncate(i.max(1)),
                }
            }
            exercise_manifest(&b);
            if b.len() > 8 {
                exercise_manifest(&b[8..]);
                mochi_testkit::fuzz::exercise_cbor(&b[8..]);
            }
        }
    }
}
