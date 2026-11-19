//! C2 golden vectors (R8): object decode outcomes and digest known answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_format::codec::{decode_object, Protection};
use mochi_format::repr::StoredObject;
use mochi_format::Limits;
use mochi_testkit::fuzz::exercise_object_decode;
use mochi_testkit::golden::{
    c2_digest_vectors, c2_object_vectors, render_c2_manifest, ObjectExpect,
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c2")
}

#[test]
fn object_vectors_behave_as_expected() {
    let limits = Limits::default();
    for v in c2_object_vectors() {
        let stored = StoredObject::from_loaded(v.bytes.clone());
        let got = decode_object(&stored, Protection::None, v.expected_len, &limits);
        match (&v.expect, got) {
            (ObjectExpect::Decodes(want), Ok(d)) => {
                assert_eq!(d.as_bytes(), &want[..], "{}", v.name)
            }
            (ObjectExpect::Rejected(class), Err(e)) => {
                assert_eq!(e.class(), *class, "{}: {e}", v.name)
            }
            (want, got) => panic!("{}: expected {want:?}, got {got:?}", v.name),
        }
    }
}

/// Recompute every digest known answer from the separator strings written out
/// literally here, not from the library's constants, so an accidental change
/// to a constant fails this test as well as the file comparison.
#[test]
fn digest_vectors_match_literal_construction() {
    let literal = |scope: &str| -> &'static [u8] {
        match scope {
            "file-content" => b"", // O20: unseparated, b3sum-equal
            "chunk-content" => b"MOCHI2-CHUNK-CONTENT\0",
            "stored-object" => b"MOCHI2-STORED-OBJECT\0",
            "dictionary" => b"MOCHI2-DICTIONARY\0",
            "commit-id" => b"MOCHI2-COMMIT-ID\0",
            other => panic!("unknown scope {other}"),
        }
    };
    let vectors = c2_digest_vectors();
    assert_eq!(vectors.len(), 15);
    for v in vectors {
        let mut input = literal(v.scope).to_vec();
        input.extend_from_slice(&v.input);
        assert_eq!(
            *blake3::hash(&input).as_bytes(),
            v.digest,
            "{} over {} bytes",
            v.scope,
            v.input.len()
        );
    }
}

#[test]
fn checked_in_files_match_the_builders() {
    let manifest = std::fs::read_to_string(dir().join("vectors.txt"))
        .expect("vectors.txt missing (run write_c2_golden_files deliberately)");
    assert_eq!(
        manifest,
        render_c2_manifest(),
        "vectors.txt differs from its builder"
    );
    let mut known = vec!["vectors.txt".to_string()];
    for v in c2_object_vectors() {
        let name = format!("{}.bin", v.name);
        let on_disk = std::fs::read(dir().join(&name))
            .unwrap_or_else(|e| panic!("{name}: {e} (run write_c2_golden_files deliberately)"));
        assert_eq!(on_disk, v.bytes, "{name} differs from its builder");
        known.push(name);
    }
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(known.contains(&name), "unexpected golden file {name}");
    }
}

/// Deliberate, manual regeneration. Not run in CI. After running, review
/// `git diff --stat fixtures/golden/c2` and explain each changed file.
#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_c2_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    std::fs::write(dir().join("vectors.txt"), render_c2_manifest()).unwrap();
    for v in c2_object_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
    }
}

/// Stable-Rust stand-in for the `object_decode` fuzz target.
#[test]
fn fuzz_smoke_over_mutated_objects() {
    let mut state = 0x5EED_0002u64;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for v in c2_object_vectors() {
        exercise_object_decode(&v.bytes);
        for _ in 0..2000 {
            let mut b = v.bytes.clone();
            for _ in 0..(1 + next() % 3) {
                if b.is_empty() {
                    break;
                }
                let i = (next() as usize) % b.len();
                match next() % 3 {
                    0 => b[i] ^= 1 << (next() % 8),
                    1 => b[i] = next() as u8,
                    _ => b.truncate(i),
                }
            }
            exercise_object_decode(&b);
        }
    }
}
