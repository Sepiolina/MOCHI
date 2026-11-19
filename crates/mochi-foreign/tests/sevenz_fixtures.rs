//! 7z through the real helper: listing, testing, reading, AES, split volumes,
//! and the hostile cases (plan D8 rules 5-8). Ordinary archives are written by
//! the real 7-Zip command line, hostile-name ones by py7zr
//! (`fixtures/foreign/gen_7z.py`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

fn sz(name: &str) -> std::path::PathBuf {
    fixture(&format!("7z/{name}"))
}

fn index_of(l: &ListReport, name: &str) -> u64 {
    l.entries
        .iter()
        .find(|e| e.name == name.as_bytes())
        .unwrap_or_else(|| panic!("no entry {name}"))
        .index
}

const B_TXT: &str = "second file\n";

#[test]
fn basic_archive_lists_tests_and_reads_with_crc32() {
    let c = client();
    let p = sz("basic.7z");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.format, ForeignFormat::SevenZ);
    assert!(l.complete && l.breaches.is_empty());
    let a = index_of(&l, "a.txt");
    let b = index_of(&l, "sub/b.txt");
    let e = index_of(&l, "empty");
    let z = index_of(&l, "zero.txt");
    assert_eq!(l.entries[e as usize].kind, EntryKind::Directory);
    assert_eq!(l.entries[b as usize].size, Some(600));
    assert_eq!(l.entries[a as usize].encryption, Encryption::None);
    assert!(l.entries[a as usize].method.contains("LZMA2"));
    assert!(
        l.entries[a as usize].unix_mode.is_some(),
        "7-Zip stores the Unix mode"
    );
    assert!(l.entries[a as usize].mtime_unix == Some(1_577_836_800));
    assert_eq!(l.stream_check, StreamCheck::NotApplicable);
    for d in [
        l.dimensions.recoverability,
        l.dimensions.freshness,
        l.dimensions.retention,
    ] {
        assert_eq!(d, "UNSUPPORTED");
    }

    let t = c.test(&p, None).unwrap();
    assert!(t.complete && t.problems() == 0);
    assert_eq!(t.entries[a as usize].outcome, crc());
    assert_eq!(t.entries[a as usize].outcome.describe(), "CRC32 matched");
    assert_eq!(t.entries[b as usize].outcome, crc());
    assert_eq!(t.entries[e as usize].outcome, EntryOutcome::NoData);
    assert_eq!(t.entries[z as usize].outcome, EntryOutcome::NoData);
    assert_eq!(t.bytes_read, 606);

    // Whole archive in one helper run, and one entry out of a solid block.
    let all: Vec<u64> = (0..l.entries.len() as u64).collect();
    let (r, gots) = read_many(&c, &p, &all, None).unwrap();
    assert!(r.complete && r.delivered == all.len() as u64);
    assert_eq!(got(&gots, a).bytes, b"hello\n");
    assert_eq!(got(&gots, a).outcome, crc());
    assert_eq!(got(&gots, b).bytes, B_TXT.repeat(50).as_bytes());
    let (_, one) = read_many(&c, &p, &[b], None).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].bytes, B_TXT.repeat(50).as_bytes());
    // The single-entry operation works on 7z too.
    let (rr, bytes) = read_all(&c, &p, a, None);
    assert_eq!(rr.outcome, crc());
    assert_eq!(bytes, b"hello\n");
}

#[test]
fn every_supported_method_reads_back_identically() {
    let c = client();
    for name in ["basic", "nonsolid", "copy", "ppmd", "bzip2", "lzma_d16m"] {
        let p = sz(&format!("{name}.7z"));
        let l = c.list(&p, None).unwrap();
        let b = index_of(&l, "sub/b.txt");
        let (r, gots) = read_many(&c, &p, &[b], None).unwrap();
        assert!(r.complete, "{name}");
        assert_eq!(got(&gots, b).bytes, B_TXT.repeat(50).as_bytes(), "{name}");
        assert_eq!(got(&gots, b).outcome, crc(), "{name}");
    }
}

#[test]
fn a_header_that_claims_a_two_gib_dictionary_is_refused_not_allocated() {
    // Also pins the parse of the coder properties out of the library's Debug
    // form: if a library update changes it, this stops failing the right way.
    let c = client();
    let p = sz("lzma_huge_dict.7z");
    let t = c.test(&p, None).unwrap();
    let l = c.list(&p, None).unwrap();
    let b = index_of(&l, "sub/b.txt");
    match &t.entries[b as usize].outcome {
        EntryOutcome::Unsupported(why) => assert!(why.contains("2048 MiB"), "{why}"),
        o => panic!("expected unsupported, got {o:?}"),
    }
    // The 16 MiB archive made the same way is fine.
    assert_eq!(c.test(&sz("lzma_d16m.7z"), None).unwrap().problems(), 0);
}

#[test]
fn data_encryption_labels_requires_a_password_and_fails_closed() {
    let c = client();
    let p = sz("enc_data.7z");
    // Names are not encrypted here, so listing needs no password.
    let l = c.list(&p, None).unwrap();
    let b = index_of(&l, "sub/b.txt");
    assert_eq!(l.entries[b as usize].encryption, Encryption::Aes(256));
    assert_eq!(l.entries[b as usize].encryption.label(), "AES-256");

    let t = c.test(&p, None).unwrap();
    assert_eq!(
        t.entries[b as usize].outcome,
        EntryOutcome::PasswordRequired
    );
    let (_, none) = read_many(&c, &p, &[b], None).unwrap();
    assert_eq!(none[0].outcome, EntryOutcome::PasswordRequired);
    assert!(none[0].bytes.is_empty());

    // Wrong password: nothing is delivered, nothing is called a match.
    let wrong = pw("not-the-password");
    let t = c.test(&p, Some(&wrong)).unwrap();
    assert!(t
        .entries
        .iter()
        .all(|e| !matches!(e.outcome, EntryOutcome::Matched(_))));
    let all: Vec<u64> = (0..l.entries.len() as u64).collect();
    let (r, gots) = read_many(&c, &p, &all, Some(&wrong)).unwrap();
    for g in &gots {
        assert!(
            !matches!(g.outcome, EntryOutcome::Matched(_)),
            "{:?}",
            g.outcome
        );
        if l.entries[g.info.index as usize].encryption != Encryption::None {
            assert!(g.bytes.is_empty(), "no partial output for a wrong password");
        }
    }
    let _ = r;

    // Right password.
    let ok = pw("SECRET");
    let t = c.test(&p, Some(&ok)).unwrap();
    assert_eq!(t.entries[b as usize].outcome, crc());
    let (_, gots) = read_many(&c, &p, &[b], Some(&ok)).unwrap();
    assert_eq!(gots[0].bytes, B_TXT.repeat(50).as_bytes());
}

#[test]
fn encrypted_names_ask_for_the_password_before_listing() {
    let c = client();
    let p = sz("enc_names.7z");
    assert_eq!(
        c.list(&p, None).unwrap_err().code,
        ErrorCode::PasswordRequired
    );
    assert_eq!(
        c.test(&p, None).unwrap_err().code,
        ErrorCode::PasswordRequired
    );
    assert_eq!(
        read_many(&c, &p, &[0], None).err().unwrap().code,
        ErrorCode::PasswordRequired
    );
    let wrong = pw("nope");
    assert_eq!(
        c.list(&p, Some(&wrong)).unwrap_err().code,
        ErrorCode::WrongPassword
    );
    let ok = pw("SECRET");
    let l = c.list(&p, Some(&ok)).unwrap();
    assert!(index_of(&l, "a.txt") < l.entries.len() as u64);
    assert_eq!(c.test(&p, Some(&ok)).unwrap().problems(), 0);
    // A password that is not text cannot be a 7z password.
    let bad = Password::new(vec![0xff, 0xfe]);
    assert_eq!(
        c.list(&p, Some(&bad)).unwrap_err().code,
        ErrorCode::BadRequest
    );
}

#[test]
fn split_volumes_read_as_one_stream() {
    let c = client();
    let first = sz("split.7z.001");
    let l = c.list(&first, None).unwrap();
    assert_eq!(l.format, ForeignFormat::SevenZ);
    assert!(l.complete);
    let n = index_of(&l, "noise.bin");
    assert_eq!(l.entries[n as usize].size, Some(900));
    assert_eq!(c.test(&first, None).unwrap().problems(), 0);
    let (_, gots) = read_many(&c, &first, &[n], None).unwrap();
    assert_eq!(gots[0].bytes.len(), 900);
    assert_eq!(gots[0].outcome, crc());
    // A later volume is not the start of the set.
    let e = c.list(&sz("split.7z.002"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotAnArchive);
    assert!(e.message.contains("first volume"), "{}", e.message);
}

#[test]
fn a_split_set_with_a_missing_volume_says_so() {
    let c = client();
    // .001 and .003 only: the gap ends the set after one volume.
    for dir in ["gap", "short"] {
        let e = c
            .list(&sz(&format!("{dir}/split.7z.001")), None)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::MissingVolume, "{dir}: {e}");
        assert!(e.message.contains("volume"), "{}", e.message);
        assert_eq!(
            c.test(&sz(&format!("{dir}/split.7z.001")), None)
                .unwrap_err()
                .code,
            ErrorCode::MissingVolume
        );
    }
}

#[test]
fn a_corrupt_entry_fails_its_crc_and_only_its_crc() {
    let c = client();
    let p = sz("crc_corrupt.7z");
    let l = c.list(&p, None).unwrap();
    let a = index_of(&l, "a.txt");
    let b = index_of(&l, "sub/b.txt");
    let t = c.test(&p, None).unwrap();
    assert_eq!(t.entries[a as usize].outcome, crc());
    match &t.entries[b as usize].outcome {
        EntryOutcome::Mismatch {
            failed,
            wrong_password_possible: false,
        } => assert!(failed.contains(CheckKind::Crc32)),
        o => panic!("{o:?}"),
    }
    assert_eq!(t.problems(), 1);
    assert!(t.entries[b as usize]
        .outcome
        .describe()
        .contains("did NOT match"));
    // A read reports the same, so the caller discards what it received.
    let (_, gots) = read_many(&c, &p, &[a, b], None).unwrap();
    assert_eq!(got(&gots, a).outcome, crc());
    assert!(!got(&gots, b).outcome.is_clean());
}

#[test]
fn damaged_headers_and_truncation_are_typed_errors() {
    let c = client();
    for name in ["truncated.7z", "bad_start_crc.7z", "bad_header.7z"] {
        let e = c.list(&sz(name), None).unwrap_err();
        assert_eq!(e.code, ErrorCode::MalformedArchive, "{name}: {e}");
        assert_eq!(
            c.test(&sz(name), None).unwrap_err().code,
            ErrorCode::MalformedArchive
        );
    }
    let e = c.list(&sz("truncated.7z"), None).unwrap_err();
    assert!(e.message.contains("truncated"), "{}", e.message);
}

#[test]
fn bombs_are_stopped_by_the_configured_limits() {
    // Ratio: 8 MiB of zeros in ~1 KiB, against the default 200:1.
    let c = client();
    let t = c.test(&sz("bomb_ratio.7z"), None).unwrap();
    assert!(!t.complete);
    assert!(
        t.breaches.iter().any(|b| b.kind == BreachKind::Ratio),
        "{:?}",
        t.breaches
    );
    assert!(t
        .entries
        .iter()
        .any(|e| matches!(e.outcome, EntryOutcome::Breach(BreachKind::Ratio))));
    assert!(t.bytes_read < 8 << 20, "stopped early: {}", t.bytes_read);

    // Per-entry size (declared): refused without decoding.
    let small = client_with(Limits {
        max_entry_size: 1 << 20,
        max_ratio: 1_000_000,
        ..Limits::default()
    });
    let t = small.test(&sz("bomb_ratio.7z"), None).unwrap();
    assert!(t.breaches.iter().any(|b| b.kind == BreachKind::EntrySize));
    assert_eq!(t.bytes_read, 0);

    // Total output.
    let tot = client_with(Limits {
        max_total_output: 1 << 20,
        max_ratio: 1_000_000,
        ..Limits::default()
    });
    let t = tot.test(&sz("bomb_ratio.7z"), None).unwrap();
    assert!(t.breaches.iter().any(|b| b.kind == BreachKind::TotalOutput));
    assert!(!t.complete);

    // Entry count.
    let few = client_with(Limits {
        max_entries: 100,
        ..Limits::default()
    });
    let l = few.list(&sz("bomb_count.7z"), None).unwrap();
    assert_eq!(l.entries.len(), 100);
    assert!(!l.complete);
    assert!(l.breaches.iter().any(|b| b.kind == BreachKind::EntryCount));

    // Name length: not carried, never truncated.
    let names = client_with(Limits {
        max_name_len: 256,
        ..Limits::default()
    });
    let l = names.list(&sz("long_name.7z"), None).unwrap();
    assert!(l.entries[0].name.is_empty());
    assert!(l.entries[0].error.is_some());
    assert!(l.breaches.iter().any(|b| b.kind == BreachKind::NameLength));
}

#[test]
fn hostile_names_are_carried_exactly_and_never_judged() {
    let c = client();
    let p = sz("traversal.7z");
    let l = c.list(&p, None).unwrap();
    let want = [
        "../evil.txt",
        "/abs/path.txt",
        "a/../../b.txt",
        "C:\\win.txt",
        "dir\\back.txt",
        "CON",
        "nul.txt",
        "file.txt:stream",
        "A.txt",
        "a.txt",
        "trail.",
        "ok/fine.txt",
    ];
    let have: Vec<Vec<u8>> = names(&l);
    for w in want {
        assert!(have.contains(&b(w)), "missing {w:?} in {have:?}");
    }
    // Every entry still tests clean: the reader judges data, not names.
    assert_eq!(c.test(&p, None).unwrap().problems(), 0);
}

#[test]
fn read_many_validates_and_dedups_its_request() {
    let c = client();
    let p = sz("basic.7z");
    let e = read_many(&c, &p, &[0, 999], None).err().unwrap();
    assert_eq!(e.code, ErrorCode::EntryNotFound);
    let l = c.list(&p, None).unwrap();
    let a = index_of(&l, "a.txt");
    let (r, gots) = read_many(&c, &p, &[a, a, a], None).unwrap();
    assert_eq!(r.delivered, 1);
    assert_eq!(gots.len(), 1);
}

#[test]
fn a_wrong_password_on_stored_data_delivers_no_garbage() {
    // `enc_copy.7z` is encrypted but not compressed: with the wrong password
    // the decrypted bytes are noise that no decoder rejects, so they stream
    // out of the reader and only the CRC32 at the end says they are wrong.
    // The helper holds them until then, so the caller receives nothing.
    let c = client();
    let p = sz("enc_copy.7z");
    let l = c.list(&p, None).unwrap();
    let b = index_of(&l, "sub/b.txt");
    let wrong = pw("not-the-password");
    let (_, gots) = read_many(&c, &p, &[b], Some(&wrong)).unwrap();
    assert!(
        matches!(
            gots[0].outcome,
            EntryOutcome::Mismatch {
                wrong_password_possible: true,
                ..
            }
        ),
        "{:?}",
        gots[0].outcome
    );
    assert!(
        gots[0].bytes.is_empty(),
        "{} bytes of noise were delivered",
        gots[0].bytes.len()
    );
    let ok = pw("SECRET");
    let (_, gots) = read_many(&c, &p, &[b], Some(&ok)).unwrap();
    assert_eq!(gots[0].bytes, B_TXT.repeat(50).as_bytes());
}

#[test]
fn a_header_claiming_absurd_counts_is_malformed_not_a_crash() {
    // Found by the fuzzer: sevenz-rust2 sizes a vector from the file count and
    // panics on capacity overflow; that is reported as a malformed archive.
    let c = client();
    let e = c.list(&sz("header_huge_count.7z"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::MalformedArchive, "{e}");
}

#[test]
fn a_zeroed_start_header_is_refused_promptly_not_scanned_for_headers() {
    // Audit F16, found by the fuzzer: with a zero start-header CRC and 20 zero
    // bytes, sevenz-rust2 scans backwards for header-looking bytes and parses
    // each candidate; on this input the scan never finished. The start header
    // is checked before the library sees the file.
    let c = client();
    let t = std::time::Instant::now();
    let e = c
        .list(&sz("hang_zeroed_start_header.7z"), None)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::MalformedArchive, "{e}");
    assert!(
        t.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        t.elapsed()
    );
}
