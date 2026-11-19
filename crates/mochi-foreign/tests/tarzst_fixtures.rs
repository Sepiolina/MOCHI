//! tar.zst: what is and is not checked, hostile streams, and detection. The
//! archives are built here with libzstd (the `zstd` dev-dependency) and the
//! `tar` writer; the reader under test is the pure-Rust `ruzstd` decoder, so
//! the two sides are independent implementations of Zstandard.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;
use std::io::Write;
use std::path::PathBuf;

fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(1_577_836_800);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        b.append_data(&mut h, name, *data).unwrap();
    }
    b.into_inner().unwrap()
}

fn zst(raw: &[u8], checksum: bool) -> Vec<u8> {
    let mut e = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    e.include_checksum(checksum).unwrap();
    e.write_all(raw).unwrap();
    e.finish().unwrap()
}

fn basic_tar() -> Vec<u8> {
    tar_of(&[
        ("a.txt", b"hello\n"),
        ("d/b.txt", &b"second file\n".repeat(50)),
    ])
}

/// Keeps the temporary directory alive with the path.
struct Tmp(tempfile::TempDir, PathBuf);

fn write(name: &str, bytes: &[u8]) -> Tmp {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    Tmp(dir, p)
}

impl Tmp {
    fn path(&self) -> &std::path::Path {
        let _ = &self.0;
        &self.1
    }
}

fn matched() -> StreamCheck {
    StreamCheck::Matched(CheckSet::of(&[CheckKind::ZstdFrameChecksum]))
}

#[test]
fn tar_zst_lists_tests_and_reads_with_the_frame_checksum() {
    let t = write("basic.tar.zst", &zst(&basic_tar(), true));
    let c = client();
    let l = c.list(t.path(), None).unwrap();
    assert_eq!(l.format, ForeignFormat::TarZst);
    assert!(l.complete);
    assert_eq!(names(&l), vec![b("a.txt"), b("d/b.txt")]);
    assert_eq!(l.entries[1].size, Some(600));
    assert_eq!(l.entries[0].mtime_unix, Some(1_577_836_800));
    // Listing reads the stream to its end, so the frame checksum is available.
    assert_eq!(l.stream_check, matched());
    assert_eq!(l.stream_check.describe(), "zstd frame checksum matched");

    let r = c.test(t.path(), None).unwrap();
    assert!(only_kinds(&r, |o| matches!(o, EntryOutcome::NoEntryCheck)));
    assert_eq!(r.stream_check, matched());
    assert_eq!(r.problems(), 0);
    assert_eq!(r.bytes_read, 6 + 600);

    let (rep, bytes) = read_all(&c, t.path(), 1, None);
    assert_eq!(rep.outcome, EntryOutcome::NoEntryCheck);
    assert_eq!(bytes, b"second file\n".repeat(50));

    let (_, got) = read_many(&c, t.path(), &[0, 1], None).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].bytes, b"hello\n");
}

#[test]
fn a_frame_without_a_checksum_is_not_credited_with_one() {
    let t = write("nochk.tar.zst", &zst(&basic_tar(), false));
    let l = client().list(t.path(), None).unwrap();
    assert_eq!(l.format, ForeignFormat::TarZst);
    assert!(l.complete);
    assert_eq!(l.stream_check, StreamCheck::NotApplicable);
}

#[test]
fn concatenated_and_skippable_frames_are_read_as_one_stream() {
    let raw = basic_tar();
    // Split on a block boundary: tar entries, then the end-of-archive blocks.
    let cut = 512;
    let mut file = zst(&raw[..cut], true);
    // A skippable frame (magic bytes 50 2A 4D 18, low nibble free) carrying 5
    // arbitrary bytes.
    file.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18]);
    file.extend_from_slice(&5u32.to_le_bytes());
    file.extend_from_slice(b"extra");
    file.extend_from_slice(&zst(&raw[cut..], true));
    let t = write("multi.tar.zst", &file);
    let c = client();
    let l = c.list(t.path(), None).unwrap();
    assert!(l.complete, "{:?}", l.stop_reason);
    assert_eq!(names(&l), vec![b("a.txt"), b("d/b.txt")]);
    assert_eq!(l.stream_check, matched());
    let (_, bytes) = read_all(&c, t.path(), 0, None);
    assert_eq!(bytes, b"hello\n");
}

#[test]
fn a_wrong_frame_checksum_fails_the_stream_not_the_entries() {
    let mut f = zst(&basic_tar(), true);
    let n = f.len();
    f[n - 1] ^= 0xFF;
    let t = write("badsum.tar.zst", &f);
    let r = client().test(t.path(), None).unwrap();
    assert!(
        matches!(&r.stream_check, StreamCheck::Failed(m) if m.contains("checksum")),
        "{:?}",
        r.stream_check
    );
    // The entries were all read; the stream failure is what vouches for none.
    assert!(only_kinds(&r, |o| matches!(o, EntryOutcome::NoEntryCheck)));
}

#[test]
fn a_truncated_stream_is_a_typed_incomplete_result() {
    // Random data, longer than the 2 MiB window libzstd uses at level 3: the
    // decoder holds a window of output back until the frame ends or the window
    // is exceeded, so the cut has to fall after that for the tar header to be
    // seen at all.
    let mut x: u64 = 7;
    let data: Vec<u8> = (0..3 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    let f = zst(&tar_of(&[("rand.bin", &data)]), true);
    let t = write("trunc.tar.zst", &f[..f.len() * 9 / 10]);
    let c = client();
    let l = c.list(t.path(), None).unwrap();
    assert!(!l.complete);
    assert!(l.stop_reason.is_some());
    assert!(matches!(l.stream_check, StreamCheck::Failed(_)));
    let r = c.test(t.path(), None).unwrap();
    assert!(!r.complete);
    assert!(r.problems() >= 1);

    // Cut before the first tar header could be read: damaged, not a guess.
    let short = write("short.tar.zst", &f[..f.len() / 10]);
    assert_eq!(
        c.list(short.path(), None).unwrap_err().code,
        ErrorCode::MalformedArchive
    );
}

#[test]
fn damage_inside_the_compressed_data_never_panics_the_helper() {
    let mut x: u64 = 99;
    let data: Vec<u8> = (0..30_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    let f = zst(&tar_of(&[("rand.bin", &data)]), true);
    let c = client();
    for at in [f.len() / 3, f.len() / 2, f.len() - 9] {
        let mut g = f.clone();
        g[at] ^= 0x5A;
        let t = write("flip.tar.zst", &g);
        match c.test(t.path(), None) {
            // Either the block structure broke (a read error / failed stream)
            // or only literal bytes changed and the frame checksum caught it.
            Ok(r) => assert!(
                r.problems() >= 1 || matches!(r.stream_check, StreamCheck::Failed(_)),
                "flip at {at} went unnoticed: {:?}",
                r.stream_check
            ),
            Err(e) => assert!(e.code == ErrorCode::MalformedArchive, "{e:?}"),
        }
    }
}

#[test]
fn a_zstd_bomb_is_a_limit_breach_mid_stream() {
    // 64 MiB of zeros in a few KiB of zstd.
    let zeros = vec![0u8; 64 << 20];
    let f = zst(&tar_of(&[("zeros.bin", &zeros)]), true);
    assert!(f.len() < 64 << 10, "{}", f.len());
    let t = write("bomb.tar.zst", &f);

    // Default limits: the compression-ratio limit trips while decoding.
    let r = client().test(t.path(), None).unwrap();
    assert!(!r.complete);
    assert!(
        r.breaches
            .iter()
            .any(|b| b.kind == BreachKind::Ratio && b.entry == Some(0)),
        "{:?}",
        r.breaches
    );
    assert_eq!(
        r.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::Ratio)
    );
    assert_eq!(r.stream_check, StreamCheck::NotReached);
    let l = client().list(t.path(), None).unwrap();
    assert!(!l.complete);

    // Ratio off, total-output budget small: that limit trips instead.
    let c = client_with(Limits {
        max_total_output: 4 << 20,
        max_ratio: u64::MAX,
        ..Limits::default()
    });
    let r = c.test(t.path(), None).unwrap();
    assert!(!r.complete);
    assert!(
        r.breaches.iter().any(|b| b.kind == BreachKind::TotalOutput),
        "{:?}",
        r.breaches
    );
    assert!(r.bytes_read <= (4 << 20) + (1 << 20), "{}", r.bytes_read);
}

#[test]
fn a_zstd_file_that_is_not_a_tar_is_unsupported_not_a_guess() {
    let t = write("single.zst", &zst(b"just a compressed text file\n", true));
    let e = client().list(t.path(), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFormat);
    assert!(e.message.contains("zstd"), "{}", e.message);
}

#[test]
fn a_frame_asking_for_a_huge_window_is_refused() {
    // magic, descriptor (no single-segment, no checksum), window descriptor
    // 0xF8 = exponent 31 (2 GiB+), then nothing.
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0xF8];
    f.extend_from_slice(&[0u8; 32]);
    let t = write("window.tar.zst", &f);
    let e = client().list(t.path(), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFormat);
    assert!(e.message.contains("window"), "{}", e.message);
}

#[test]
fn a_renamed_copy_is_still_a_tar_zst() {
    let t = write("whatever.bin", &zst(&basic_tar(), true));
    assert_eq!(
        client().list(t.path(), None).unwrap().format,
        ForeignFormat::TarZst
    );
}

#[test]
fn the_committed_seed_lists() {
    let l = client().list(&fixture("basic.tar.zst"), None).unwrap();
    assert_eq!(l.format, ForeignFormat::TarZst);
    assert_eq!(names(&l), vec![b("d/"), b("a.txt"), b("d/b.txt")]);
    assert_eq!(l.stream_check, matched());
}
