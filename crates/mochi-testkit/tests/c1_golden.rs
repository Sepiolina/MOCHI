//! C1 golden vectors (R8) and fuzz smoke runs.

use std::path::PathBuf;

use mochi_format::footer::validate_footer_at_eof;
use mochi_format::frame::Frames;
use mochi_format::{ErrorClass, Limits};
use mochi_testkit::fuzz::exercise_all;
use mochi_testkit::golden::{c1_vectors, Expect, Vector};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c1")
}

fn walk(bytes: &[u8]) -> (Vec<u64>, Option<ErrorClass>) {
    let mut lens = Vec::new();
    for item in Frames::new(bytes, 0, Limits::default()) {
        match item {
            Ok(span) => lens.push(span.len),
            Err(e) => return (lens, Some(e.class())),
        }
    }
    (lens, None)
}

fn check(v: &Vector) {
    let limits = Limits::default();
    let (lens, err) = walk(&v.bytes);
    match &v.expect {
        Expect::Frames(want) => {
            assert_eq!((&lens, err), (want, None), "{}", v.name);
        }
        Expect::Footer {
            lens: want,
            commit_offset,
            sequence,
        } => {
            assert_eq!((&lens, err), (want, None), "{}", v.name);
            let f = validate_footer_at_eof(&v.bytes[..], &limits).unwrap();
            assert_eq!(f.fields.commit_offset, *commit_offset, "{}", v.name);
            assert_eq!(f.fields.commit_sequence, *sequence, "{}", v.name);
        }
        Expect::FramesThenError(want, class) => {
            assert_eq!((&lens, err), (want, Some(*class)), "{}", v.name);
        }
        Expect::FooterRejected { lens: want, class } => {
            assert_eq!((&lens, err), (want, None), "{}", v.name);
            let e = validate_footer_at_eof(&v.bytes[..], &limits).unwrap_err();
            assert_eq!(e.class(), *class, "{}: {e}", v.name);
        }
    }
}

#[test]
fn vectors_behave_as_expected() {
    for v in c1_vectors() {
        check(&v);
    }
}

#[test]
fn checked_in_files_match_the_builders() {
    for v in c1_vectors() {
        let path = dir().join(format!("{}.bin", v.name));
        let on_disk = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (run write_c1_golden_files deliberately)", v.name));
        assert_eq!(on_disk, v.bytes, "{} differs from its builder", v.name);
    }
    // No stray files: every .bin on disk is a known vector.
    let known: Vec<String> = c1_vectors()
        .iter()
        .map(|v| format!("{}.bin", v.name))
        .collect();
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".bin") {
            assert!(known.contains(&name), "unexpected golden file {name}");
        }
    }
}

/// Deliberate, manual regeneration. Not run in CI. After running, review
/// `git diff --stat fixtures/golden/c1` and explain each changed file.
#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_c1_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    for v in c1_vectors() {
        std::fs::write(dir().join(format!("{}.bin", v.name)), &v.bytes).unwrap();
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

/// Stable-Rust stand-in for a fuzz run: deterministic mutations of every
/// vector through the same exercisers the cargo-fuzz targets use.
#[test]
fn fuzz_smoke_over_mutated_vectors() {
    let mut rng = SplitMix(0x5EED_0001);
    for v in c1_vectors() {
        exercise_all(&v.bytes);
        for _ in 0..1500 {
            let mut b = v.bytes.clone();
            if b.is_empty() {
                continue;
            }
            for _ in 0..(1 + rng.next() % 4) {
                let i = (rng.next() as usize) % b.len();
                match rng.next() % 3 {
                    0 => b[i] ^= 1 << (rng.next() % 8),
                    1 => b[i] = rng.next() as u8,
                    _ => b[i] = 0xFF,
                }
            }
            match rng.next() % 4 {
                0 => b.truncate((rng.next() as usize) % (b.len() + 1)),
                1 => b.extend_from_slice(&v.bytes[..(rng.next() as usize) % v.bytes.len()]),
                _ => {}
            }
            exercise_all(&b);
        }
    }
}
