//! tar, tar.gz: what is and is not checked, and exact names (plan D8 rule 6).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

#[test]
fn plain_tar_has_no_per_entry_check_and_says_so() {
    let c = client();
    let p = fixture("basic.tar");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.format, ForeignFormat::Tar);
    assert!(l.complete);
    assert_eq!(names(&l), vec![b("d/"), b("a.txt"), b("d/b.txt")]);
    assert_eq!(l.entries[0].kind, EntryKind::Directory);
    assert_eq!(l.entries[1].kind, EntryKind::File);
    assert_eq!(l.entries[2].size, Some(600));
    assert_eq!(l.entries[1].mtime_unix, Some(1_577_836_800));
    assert_eq!(l.entries[1].unix_mode, Some(0o644));
    assert_eq!(l.stream_check, StreamCheck::NotApplicable);

    let t = c.test(&p, None).unwrap();
    assert_eq!(t.entries[0].outcome, EntryOutcome::NoData);
    assert_eq!(t.entries[1].outcome, EntryOutcome::NoEntryCheck);
    assert_eq!(t.entries[2].outcome, EntryOutcome::NoEntryCheck);
    assert_eq!(
        t.entries[1].outcome.describe(),
        "read without error; this format has no per-entry check"
    );
    assert!(
        only_kinds(&t, |o| !matches!(o, EntryOutcome::Matched(_))),
        "tar never reports a matched check"
    );
    assert_eq!(t.stream_check, StreamCheck::NotApplicable);
    assert_eq!(t.bytes_read, 6 + 600);

    let (r, bytes) = read_all(&c, &p, 1, None);
    assert_eq!(r.outcome, EntryOutcome::NoEntryCheck);
    assert_eq!(bytes, b"hello\n");
}

#[test]
fn tar_gz_has_only_the_whole_stream_gzip_crc() {
    let c = client();
    let p = fixture("basic.tar.gz");
    let l = c.list(&p, None).unwrap();
    assert_eq!(l.format, ForeignFormat::TarGz);
    assert_eq!(names(&l), vec![b("d/"), b("a.txt"), b("d/b.txt")]);
    // Listing reads the stream to its end, so the trailer check is available.
    assert_eq!(
        l.stream_check,
        StreamCheck::Matched(CheckSet::of(&[CheckKind::GzipStreamCrc32]))
    );
    assert_eq!(l.stream_check.describe(), "gzip stream CRC32 matched");

    let t = c.test(&p, None).unwrap();
    // No entry is ever credited with a CRC of its own.
    assert!(only_kinds(&t, |o| matches!(
        o,
        EntryOutcome::NoData | EntryOutcome::NoEntryCheck
    )));
    assert_eq!(
        t.stream_check,
        StreamCheck::Matched(CheckSet::of(&[CheckKind::GzipStreamCrc32]))
    );
    assert_eq!(t.problems(), 0);

    let (r, bytes) = read_all(&c, &p, 2, None);
    assert_eq!(r.outcome, EntryOutcome::NoEntryCheck);
    assert_eq!(bytes, b"second file\n".repeat(50));
}

#[test]
fn damaged_gzip_trailer_fails_the_stream_not_the_entries() {
    let t = client()
        .test(&fixture("gz_crc_corrupt.tar.gz"), None)
        .unwrap();
    assert!(
        matches!(t.stream_check, StreamCheck::Failed(_)),
        "{:?}",
        t.stream_check
    );
    // The entries individually read fine; only the stream check speaks, and it
    // counts as a problem so a UI cannot show the archive as clean.
    assert!(t.problems() >= 1);
    assert!(t.stream_check.describe().starts_with("stream check failed"));
}

#[test]
fn truncated_tar_gz_is_reported_incomplete_with_a_failed_stream() {
    let c = client();
    let l = c.list(&fixture("truncated.tar.gz"), None).unwrap();
    assert!(!l.complete);
    assert!(l.stop_reason.is_some());
    assert!(matches!(l.stream_check, StreamCheck::Failed(_)));
    let t = c.test(&fixture("truncated.tar.gz"), None).unwrap();
    assert!(!t.complete);
    assert!(t.problems() >= 1);
}

#[test]
fn truncated_plain_tar_is_reported_incomplete() {
    let c = client();
    let l = c.list(&fixture("truncated.tar"), None).unwrap();
    assert!(
        !l.complete,
        "a tar cut off mid-entry must not list as complete"
    );
    assert!(l
        .stop_reason
        .as_deref()
        .is_some_and(|r| r.contains("truncated")));
    let t = c.test(&fixture("truncated.tar"), None).unwrap();
    assert!(!t.complete);
    assert!(t
        .entries
        .iter()
        .any(|e| matches!(e.outcome, EntryOutcome::ReadError(_))));
}

#[test]
fn tar_hostile_entries_are_carried_as_found() {
    let c = client();
    let p = fixture("traversal.tar");
    let l = c.list(&p, None).unwrap();
    assert!(l.complete);
    let expect: Vec<(&str, EntryKind)> = vec![
        ("../evil.txt", EntryKind::File),
        ("/abs/path.txt", EntryKind::File),
        ("a/../../b.txt", EntryKind::File),
        ("link_out", EntryKind::Symlink),
        ("hard", EntryKind::HardLink),
        ("dev_null", EntryKind::Other),
        ("fifo", EntryKind::Other),
        ("CON", EntryKind::File),
        ("Dup.txt", EntryKind::File),
        ("dup.txt", EntryKind::File),
    ];
    let got: Vec<(Vec<u8>, EntryKind)> =
        l.entries.iter().map(|e| (e.name.clone(), e.kind)).collect();
    let want: Vec<(Vec<u8>, EntryKind)> = expect.iter().map(|(n, k)| (b(n), *k)).collect();
    assert_eq!(got, want);
    assert_eq!(
        l.entries[3].link_target.as_deref(),
        Some(&b"../../etc/passwd"[..])
    );
    assert_eq!(l.entries[4].link_target.as_deref(), Some(&b"a.txt"[..]));
    // Links, devices and FIFOs have no data stream to test.
    let t = c.test(&p, None).unwrap();
    for i in [3, 4, 5, 6] {
        assert_eq!(t.entries[i].outcome, EntryOutcome::NoData, "entry {i}");
    }
    // Both same-named entries (and the case variants) are individually addressable.
    let (_, one) = read_all(&c, &p, 8, None);
    let (_, two) = read_all(&c, &p, 9, None);
    assert_eq!((one, two), (b"1".to_vec(), b"2".to_vec()));
}

#[test]
fn long_names_from_pax_and_gnu_headers_are_complete() {
    let c = client();
    let pax = c.list(&fixture("pax.tar"), None).unwrap();
    assert_eq!(
        pax.entries[0].name,
        format!("{}/{}.txt", "p".repeat(150), "q".repeat(150)).into_bytes()
    );
    let gnu = c.list(&fixture("longname_gnu.tar"), None).unwrap();
    assert_eq!(
        gnu.entries[0].name,
        format!("{}.txt", "g".repeat(300)).into_bytes()
    );
}

#[test]
fn read_of_a_missing_tar_entry_is_a_typed_error() {
    let e = client()
        .read_entry(&fixture("basic.tar"), 50, None, &mut |_| Ok(()))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::EntryNotFound);
}

#[test]
fn tgz_is_the_same_format_as_tar_gz() {
    // Detection is by content: a renamed copy is still a tar.gz.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("renamed.tgz");
    std::fs::copy(fixture("basic.tar.gz"), &p).unwrap();
    assert_eq!(
        client().list(&p, None).unwrap().format,
        ForeignFormat::TarGz
    );
}
