//! ZIP through the real helper: listing, testing, reading, encryption, and the
//! exact wording of what was checked (plan D8 rules 6 and 7).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

#[test]
fn infozip_archive_lists_tests_and_reads() {
    let c = client();
    let p = fixture("basic_infozip.zip");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.format, ForeignFormat::Zip);
    assert!(l.complete && l.stop_reason.is_none() && l.breaches.is_empty());
    assert_eq!(names(&l), vec![b("a.txt"), b("empty/"), b("sub/b.txt")]);
    assert_eq!(l.entries[0].kind, EntryKind::File);
    assert_eq!(l.entries[1].kind, EntryKind::Directory);
    assert_eq!(l.entries[0].size, Some(6));
    assert_eq!(l.entries[2].size, Some(600));
    assert_eq!(l.entries[0].encryption, Encryption::None);
    assert_eq!(l.stream_check, StreamCheck::NotApplicable);
    // MOCHI's health dimensions do not exist for a foreign format.
    assert_eq!(l.dimensions.recoverability, "UNSUPPORTED");
    assert_eq!(l.dimensions.freshness, "UNSUPPORTED");
    assert_eq!(l.dimensions.retention, "UNSUPPORTED");

    let t = c.test(&p, None).unwrap();
    assert!(t.complete);
    let crc = EntryOutcome::Matched(CheckSet::of(&[CheckKind::Crc32]));
    assert_eq!(t.entries[0].outcome, crc);
    assert_eq!(t.entries[1].outcome, EntryOutcome::NoData);
    assert_eq!(t.entries[2].outcome, crc);
    assert_eq!(t.entries[0].outcome.describe(), "CRC32 matched");
    assert_eq!(t.problems(), 0);
    assert_eq!(t.bytes_read, 6 + 600);

    let (r, bytes) = read_all(&c, &p, 0, None);
    assert_eq!(r.outcome, crc);
    assert_eq!(bytes, b"hello\n");
    let (r, bytes) = read_all(&c, &p, 2, None);
    assert_eq!(r.outcome, crc);
    assert_eq!(bytes, b"second file\n".repeat(50));
}

#[test]
fn read_of_a_missing_entry_is_a_typed_error() {
    let e = client()
        .read_entry(&fixture("basic_infozip.zip"), 99, None, &mut |_| Ok(()))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::EntryNotFound);
}

#[test]
fn hostile_names_are_carried_exactly_and_never_judged() {
    let c = client();
    let p = fixture("traversal.zip");
    let l = c.list(&p, None).unwrap();
    let expect: Vec<Vec<u8>> = [
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
        "trailing.dot.",
        "日本語.txt",
    ]
    .iter()
    .map(|s| b(s))
    .collect();
    assert_eq!(
        names(&l),
        expect,
        "names must be the stored bytes, in order, unnormalized"
    );
    // The UTF-8 name carries the general-purpose bit; the ASCII ones need not.
    assert_eq!(l.entries[11].name_is_utf8_flag, Some(true));
    assert!(l.complete, "{:?}", l.stop_reason);
    let t = c.test(&p, None).unwrap();
    assert_eq!(
        t.problems(),
        0,
        "reading a hostile name is not an error; restoring it is the engine's job"
    );
}

#[test]
fn entries_sharing_a_name_are_reported_as_incomplete_not_hidden() {
    let c = client();
    let p = fixture("duplicate_names.zip");
    let l = c.list(&p, None).unwrap();
    // The zip library keeps one entry per name; the end record declares 3.
    assert_eq!(names(&l), vec![b("dup.txt"), b("other.txt")]);
    assert!(
        !l.complete,
        "a listing that lost an entry must not claim completeness"
    );
    assert!(l
        .stop_reason
        .as_deref()
        .is_some_and(|r| r.contains("declares 3 entries")));
    let t = c.test(&p, None).unwrap();
    assert!(!t.complete);
    // Pin the library's choice so an upgrade that changes it is noticed.
    let (_, bytes) = read_all(&c, &p, 0, None);
    assert_eq!(bytes, b"second");
}

#[test]
fn legacy_code_page_name_bytes_survive_untouched() {
    let l = client().list(&fixture("cp437_name.zip"), None).unwrap();
    assert_eq!(l.entries[0].name, b"caf\x82.txt".to_vec());
    assert_eq!(l.entries[0].name_is_utf8_flag, Some(false));
}

#[test]
fn symlink_entries_are_reported_as_symlinks_with_data_readable() {
    let c = client();
    let p = fixture("symlink.zip");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.entries[0].kind, EntryKind::Symlink);
    assert_eq!(l.entries[1].kind, EntryKind::Symlink);
    assert_eq!(l.entries[2].kind, EntryKind::File);
    assert_eq!(l.entries[3].kind, EntryKind::Directory);
    let (_, target) = read_all(&c, &p, 0, None);
    assert_eq!(
        target, b"../../etc/passwd",
        "the helper reports the target; it never follows it"
    );
}

#[test]
fn stored_entry_with_a_flipped_byte_reports_a_crc32_mismatch() {
    let t = client().test(&fixture("crc_corrupt.zip"), None).unwrap();
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Mismatch {
            failed: CheckSet::of(&[CheckKind::Crc32]),
            wrong_password_possible: false
        }
    );
    assert!(t.entries[0].outcome.describe().contains("did NOT match"));
    assert_eq!(t.problems(), 1);
}

#[test]
fn damaged_deflate_stream_is_not_reported_clean() {
    let t = client()
        .test(&fixture("deflate_corrupt.zip"), None)
        .unwrap();
    assert!(
        !t.entries[0].outcome.is_clean(),
        "{:?}",
        t.entries[0].outcome
    );
    assert!(matches!(
        t.entries[0].outcome,
        EntryOutcome::ReadError(_) | EntryOutcome::Mismatch { .. }
    ));
}

#[test]
fn truncated_zip_is_a_typed_malformed_error() {
    let e = client().list(&fixture("truncated.zip"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::MalformedArchive);
}

#[test]
fn zip64_extra_fields_read() {
    let t = client().test(&fixture("zip64_forced.zip"), None).unwrap();
    assert_eq!(t.entries[0].outcome.describe(), "CRC32 matched");
}

#[test]
fn unsupported_methods_are_reported_per_entry_not_as_failures_of_the_archive() {
    let c = client();
    for f in ["method_bzip2.zip", "method_lzma.zip"] {
        let l = c.list(&fixture(f), None).unwrap();
        assert_eq!(l.entries.len(), 1, "{f}");
        let t = c.test(&fixture(f), None).unwrap();
        assert!(
            matches!(t.entries[0].outcome, EntryOutcome::Unsupported(_)),
            "{f}: {:?}",
            t.entries[0].outcome
        );
        assert!(t.complete);
    }
}

// ---- encryption ---------------------------------------------------------------

fn pw(s: &str) -> Password {
    Password::new(s.as_bytes().to_vec())
}

#[test]
fn zipcrypto_is_labelled_weak_and_needs_the_password() {
    let c = client();
    let p = fixture("zipcrypto.zip");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.entries[0].encryption, Encryption::ZipCryptoWeak);
    assert!(l.entries[0].encryption.label().contains("weak"));
    // Listing needs no password (names are not encrypted in ZIP).
    assert_eq!(l.entries[0].name, b("secret.txt"));

    let t = c.test(&p, None).unwrap();
    assert_eq!(t.entries[0].outcome, EntryOutcome::PasswordRequired);

    let ok = c.test(&p, Some(&pw("secret"))).unwrap();
    assert_eq!(ok.entries[0].outcome.describe(), "CRC32 matched");
    let (r, bytes) = read_all(&c, &p, 0, Some(&pw("secret")));
    assert!(r.outcome.is_clean());
    assert_eq!(bytes, b"classified bytes\n".repeat(20));
}

#[test]
fn zipcrypto_wrong_password_fails_closed_with_no_output() {
    let c = client();
    let p = fixture("zipcrypto.zip");
    let t = c.test(&p, Some(&pw("not-the-password"))).unwrap();
    assert!(
        matches!(
            t.entries[0].outcome,
            EntryOutcome::WrongPassword
                | EntryOutcome::Mismatch {
                    wrong_password_possible: true,
                    ..
                }
        ),
        "{:?}",
        t.entries[0].outcome
    );
    assert_eq!(t.problems(), 1);
    // Read: not one byte reaches the sink.
    let mut calls = 0;
    let r = c
        .read_entry(&p, 0, Some(&pw("not-the-password")), &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert!(!r.outcome.is_clean());
    assert_eq!(calls, 0, "a wrong password must produce no partial output");
    assert_eq!(r.bytes_delivered, 0);
}

fn write_tmp(dir: &tempfile::TempDir, name: &str, data: &[u8]) -> std::path::PathBuf {
    let p = dir.path().join(name);
    std::fs::write(&p, data).unwrap();
    p
}

#[test]
fn aes_entries_report_the_authentication_code_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"aes protected payload\n".repeat(40);
    let p = write_tmp(&dir, "aes.zip", &aes_zip("pw-ok", &[("s.txt", &data)]));
    let c = client();

    let l = c.list(&p, None).unwrap();
    assert_eq!(l.entries[0].encryption, Encryption::Aes(256));
    // The inner method from the AE-x extra field, not AES method 99.
    assert_eq!(l.entries[0].method, "aes+deflate");

    assert_eq!(
        c.test(&p, None).unwrap().entries[0].outcome,
        EntryOutcome::PasswordRequired
    );

    let ok = c.test(&p, Some(&pw("pw-ok"))).unwrap();
    match &ok.entries[0].outcome {
        EntryOutcome::Matched(set) => {
            assert!(
                set.contains(CheckKind::AesAuthCode),
                "AES entries report the authentication code"
            );
            assert!(ok.entries[0]
                .outcome
                .describe()
                .contains("AES authentication code"));
        }
        other => panic!("{other:?}"),
    }
    let (r, bytes) = read_all(&c, &p, 0, Some(&pw("pw-ok")));
    assert!(r.outcome.is_clean());
    assert_eq!(bytes, data);

    // Wrong password: no bytes.
    let mut calls = 0;
    let r = c
        .read_entry(&p, 0, Some(&pw("pw-bad")), &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(r.outcome, EntryOutcome::WrongPassword);
    assert_eq!(calls, 0);
}

#[test]
fn aes_ciphertext_tampering_is_detected_and_nothing_is_delivered() {
    let dir = tempfile::tempdir().unwrap();
    // Incompressible and larger than the decoder's buffers, so output would
    // start flowing long before the end-of-entry authentication code is read.
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let data: Vec<u8> = (0..300_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let mut raw = aes_zip("pw-ok", &[("s.txt", &data)]);
    let mid = raw.len() / 2;
    raw[mid] ^= 0x55;
    let p = write_tmp(&dir, "aes_tampered.zip", &raw);
    let c = client();
    let mut calls = 0;
    let r = c
        .read_entry(&p, 0, Some(&pw("pw-ok")), &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert!(!r.outcome.is_clean(), "{:?}", r.outcome);
    assert_eq!(calls, 0, "tampered encrypted data is never delivered");
}

#[test]
fn aes_check_set_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_tmp(&dir, "aes.zip", &aes_zip("pw", &[("s.txt", b"abc")]));
    let t = client().test(&p, Some(&pw("pw"))).unwrap();
    // The zip crate writes AE-2 (no CRC32); the authentication code is the
    // only check, and the report must not claim a CRC32 that was not run.
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Matched(CheckSet::of(&[CheckKind::AesAuthCode]))
    );
    assert_eq!(
        t.entries[0].outcome.describe(),
        "AES authentication code matched"
    );
}
