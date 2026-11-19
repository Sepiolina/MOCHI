//! A stable-Rust exerciser run over the corpus, as `tests/fuzz_smoke.rs`-style
//! tests elsewhere in the repo: every committed fixture, each truncation, and
//! seeded byte flips go through every exerciser in `mochi_foreign::fuzz`. The
//! cargo-fuzz targets (`fuzz/fuzz_targets/foreign_*.rs`) call the same
//! functions. A panic in any of them is a finding.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::fuzz::*;

fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    let mut dirs = vec![fixture_dir()];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().unwrap().is_dir() {
                dirs.push(e.path());
                continue;
            }
            if name.ends_with(".py") || name.ends_with(".md") || name.ends_with(".txt") {
                continue;
            }
            v.push((name, std::fs::read(e.path()).unwrap()));
        }
    }
    v.sort();
    assert!(v.len() > 30, "fixture corpus went missing: {}", v.len());
    v
}

fn all(data: &[u8]) {
    exercise_zip_listing(data);
    exercise_tar_listing(data);
    exercise_targz_listing(data);
    exercise_tarzst_listing(data);
    exercise_sevenz_listing(data);
    exercise_detect(data);
    exercise_protocol(data);
    // UnRAR reads files, not memory: write the input to a temporary file.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("in.rar");
    std::fs::write(&path, data).unwrap();
    exercise_rar_file(&path);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn every_fixture_survives_every_exerciser() {
    for (name, data) in corpus() {
        eprintln!("{name}");
        all(&data);
    }
}

#[test]
fn truncations_of_every_fixture_survive() {
    for (name, data) in corpus() {
        if data.len() > 70_000 {
            continue;
        }
        let step = (data.len() / 40).max(1);
        for cut in (0..data.len()).step_by(step) {
            all(&data[..cut]);
        }
        let _ = name;
    }
}

#[test]
fn seeded_byte_flips_survive() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (name, data) in corpus() {
        if data.is_empty() || data.len() > 70_000 {
            continue;
        }
        for _ in 0..25 {
            let mut m = data.clone();
            for _ in 0..1 + (rng.next() % 4) {
                let i = (rng.next() % m.len() as u64) as usize;
                m[i] ^= 1 << (rng.next() % 8);
            }
            all(&m);
        }
        let _ = name;
    }
}

#[test]
fn random_bytes_survive() {
    let mut rng = Rng(42);
    for len in [0usize, 1, 3, 22, 100, 512, 1024, 4096] {
        let d: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        all(&d);
    }
}

#[test]
fn a_zero_length_central_directory_and_giant_counts_survive() {
    // EOCD claiming 65535 entries at offset 0 of an otherwise empty file.
    let mut z = vec![b'P', b'K', 5, 6, 0, 0, 0, 0];
    z.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
    z.extend_from_slice(&[0xff; 4]);
    z.extend_from_slice(&[0, 0, 0, 0]);
    all(&z);
}
