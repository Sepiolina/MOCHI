//! RAR 4 and RAR 5 through the real helper (UnRAR, streamed with `RAR_TEST`).
//! The archives are real ones written by RARLAB's rar, copied from the `unrar`
//! crate's test data (`fixtures/foreign/rar/PROVENANCE.txt`); the hostile ones
//! are byte mutations of them. No RAR writer is available to this repository,
//! so there is no fixture with a traversal name, a bomb, or a BLAKE2sp hash.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

fn rar(name: &str) -> std::path::PathBuf {
    fixture(&format!("rar/{name}"))
}

#[test]
fn rar4_lists_tests_and_reads_with_crc32() {
    let c = client();
    let p = rar("version.rar");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.format, ForeignFormat::Rar);
    assert!(l.complete);
    assert_eq!(names(&l), vec![b("VERSION")]);
    assert_eq!(l.entries[0].size, Some(11));
    assert!(l.entries[0].method.starts_with("RAR4"));
    assert_eq!(l.entries[0].mtime_unix, Some(1_438_968_068));
    assert_eq!(l.dimensions.recoverability, "UNSUPPORTED");
    let t = c.test(&p, None).unwrap();
    assert_eq!(t.entries[0].outcome, crc());
    assert_eq!(t.entries[0].outcome.describe(), "CRC32 matched");
    assert_eq!(t.bytes_read, 11);
    let (r, bytes) = read_all(&c, &p, 0, None);
    assert_eq!(r.outcome, crc());
    assert_eq!(bytes.len(), 11);
    let (_, gots) = read_many(&c, &p, &[0], None).unwrap();
    assert_eq!(gots[0].bytes, bytes);
}

#[test]
fn rar5_reports_which_hash_it_checked() {
    let c = client();
    let p = rar("solid.rar");
    let l = c.list(&p, None).unwrap();
    assert!(l.entries[0].method.starts_with("RAR5"));
    // This writer recorded CRC32; a BLAKE2sp entry would say "BLAKE2sp matched"
    // (no fixture: it needs a RAR writer).
    assert_eq!(c.test(&p, None).unwrap().entries[0].outcome, crc());
    assert_eq!(CheckSet::of(&[CheckKind::Blake2sp]).describe(), "BLAKE2sp");
    assert_eq!(
        EntryOutcome::Matched(CheckSet::of(&[CheckKind::Blake2sp])).describe(),
        "BLAKE2sp matched"
    );
    let u = c.list(&rar("unicode.rar"), None).unwrap();
    assert_eq!(u.entries[0].name, "te…―st✌".as_bytes());
}

#[test]
fn rar4_data_encryption_fails_closed() {
    let c = client();
    let p = rar("crypted.rar");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.entries[0].encryption, Encryption::Aes(128));
    assert_eq!(
        c.test(&p, None).unwrap().entries[0].outcome,
        EntryOutcome::PasswordRequired
    );
    let (_, g) = read_many(&c, &p, &[0], None).unwrap();
    assert_eq!(g[0].outcome, EntryOutcome::PasswordRequired);
    assert!(g[0].bytes.is_empty());
    // RAR 4 has no password check: a wrong password is a failed CRC32, and no
    // byte was delivered.
    let wrong = pw("not-unrar");
    let (_, g) = read_many(&c, &p, &[0], Some(&wrong)).unwrap();
    assert!(!g[0].outcome.is_clean(), "{:?}", g[0].outcome);
    assert!(
        g[0].bytes.is_empty(),
        "no partial output for a wrong password"
    );
    assert!(g[0].outcome.describe().contains("wrong password"));
    let ok = pw("unrar");
    let (_, g) = read_many(&c, &p, &[0], Some(&ok)).unwrap();
    assert_eq!(g[0].outcome, crc());
    assert_eq!(g[0].bytes, b"target\nCargo.lock\n");
}

#[test]
fn rar5_encrypted_names_ask_for_the_password_before_listing() {
    let c = client();
    let p = rar("comment-hpw-password.rar");
    assert_eq!(
        c.list(&p, None).unwrap_err().code,
        ErrorCode::PasswordRequired
    );
    assert_eq!(
        c.test(&p, None).unwrap_err().code,
        ErrorCode::PasswordRequired
    );
    assert_eq!(
        c.list(&p, Some(&pw("wrong"))).unwrap_err().code,
        ErrorCode::WrongPassword
    );
    let ok = pw("password");
    let l = c.list(&p, Some(&ok)).unwrap();
    assert_eq!(l.entries[0].encryption, Encryption::Aes(256));
    assert_eq!(c.test(&p, Some(&ok)).unwrap().entries[0].outcome, crc());
}

#[test]
fn a_multi_volume_set_with_a_missing_volume_is_incomplete_not_clean() {
    let c = client();
    let p = rar("archive.part1.rar");
    let l = c.list(&p, None).unwrap();
    assert!(!l.complete);
    assert!(l
        .stop_reason
        .as_deref()
        .unwrap()
        .contains("archive.part2.rar"));
    let t = c.test(&p, None).unwrap();
    assert!(!t.complete);
    let last = t.entries.last().unwrap();
    assert!(
        matches!(&last.outcome, EntryOutcome::ReadError(m) if m.contains("archive.part2.rar")),
        "{:?}",
        last.outcome
    );
    // The entries that fit in the first volume are fine.
    assert_eq!(t.entries[0].outcome, crc());
    assert!(t.problems() >= 1);
}

#[test]
fn a_later_volume_is_not_the_start_of_the_set() {
    let e = client().list(&rar("100M.part00002.rar"), None).unwrap_err();
    assert_eq!(e.code, ErrorCode::MissingVolume);
    assert!(e.message.contains("first volume"), "{}", e.message);
}

#[test]
fn corruption_is_reported_as_what_it_is() {
    let c = client();
    // These entries are compressed, so a damaged byte usually stops the
    // decoder before any CRC is computed: that is a read error ("no check
    // ran"), not a CRC verdict, and it is reported as such. When the check does
    // run and fails it is a mismatch naming the check.
    for name in ["crc_corrupt.rar", "rar5_corrupt.rar"] {
        let t = c.test(&rar(name), None).unwrap();
        match &t.entries[0].outcome {
            EntryOutcome::Mismatch {
                failed,
                wrong_password_possible: false,
            } => {
                assert!(failed.contains(CheckKind::Crc32))
            }
            EntryOutcome::ReadError(_) => {}
            o => panic!("{name}: {o:?}"),
        }
        assert_eq!(t.problems(), 1, "{name}");
        let (_, g) = read_many(&c, &rar(name), &[0], None).unwrap();
        assert!(!g[0].outcome.is_clean(), "{name}");
    }
    for name in ["truncated.rar", "rar5_truncated.rar"] {
        let t = c.test(&rar(name), None).unwrap();
        assert!(
            matches!(&t.entries[0].outcome, EntryOutcome::ReadError(m) if m.contains("ended after")),
            "{name}: {:?}",
            t.entries[0].outcome
        );
        let (_, g) = read_many(&c, &rar(name), &[0], None).unwrap();
        assert!(!g[0].outcome.is_clean());
    }
    // A header that fails its own check is never presented as a whole archive.
    let l = c.list(&rar("bad_header.rar"), None).unwrap();
    assert!(!l.complete);
}

#[test]
fn not_rar_and_unsupported_rar_variants_are_typed() {
    let c = client();
    assert_eq!(
        c.list(&fixture("fake.rar"), None).unwrap_err().code,
        ErrorCode::MalformedArchive
    );
    // Variants that still open: recovery record and locked flag read normally.
    for name in ["recovery-record.rar", "locked.rar"] {
        assert_eq!(c.test(&rar(name), None).unwrap().problems(), 0, "{name}");
    }
}

#[test]
fn an_entry_over_the_dictionary_limit_is_never_handed_to_unrar() {
    // Found by the fuzzer: skipping an entry of a solid archive makes UnRAR
    // allocate the entry's dictionary (2 GiB here) before any limit applies.
    let c = client();
    let p = rar("solid_huge_dict.rar");
    let l = c.list(&p, None).unwrap();
    assert!(!l.complete, "a listing that stopped must say so");
    let t = c.test(&p, None).unwrap();
    assert!(!t.complete || t.problems() > 0, "never a clean verdict");
}
