//! Decompression-bomb limits: total output, per-entry size, entry count,
//! compression ratio, name length. Each breach is reported *by entry* and the
//! operation ends cleanly (plan D8 rule 5).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

fn unlimited() -> Limits {
    Limits {
        max_total_output: u64::MAX,
        max_entry_size: u64::MAX,
        max_entries: u64::MAX,
        max_ratio: u64::MAX,
        ratio_min_output: 0,
        max_name_len: u32::MAX,
    }
}

fn has(breaches: &[Breach], kind: BreachKind, entry: Option<u64>) -> bool {
    breaches.iter().any(|b| b.kind == kind && b.entry == entry)
}

// ---- ratio ---------------------------------------------------------------------

#[test]
fn zip_ratio_bomb_is_stopped_at_its_entry() {
    let c = client(); // default ratio limit: 200
    let t = c.test(&fixture("bomb_ratio.zip"), None).unwrap();
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::Ratio)
    );
    assert!(has(&t.breaches, BreachKind::Ratio, Some(0)));
    assert!(
        t.bytes_read < 1 << 20,
        "caught from the header, before decompressing: {}",
        t.bytes_read
    );
}

#[test]
fn zip_header_that_lies_about_its_compressed_size_is_still_caught() {
    // Patch the central directory's compressed size of the 8 MiB-of-zeros
    // entry to 4 MiB: the per-entry ratio (a claim) now passes, and only the
    // archive-wide ratio against the real file length can stop it.
    let mut raw = std::fs::read(fixture("bomb_ratio.zip")).unwrap();
    let cd = raw.windows(4).rposition(|w| w == b"PK\x01\x02").unwrap();
    raw[cd + 20..cd + 24].copy_from_slice(&(4u32 << 20).to_le_bytes());
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("lying.zip");
    std::fs::write(&p, &raw).unwrap();
    let t = client().test(&p, None).unwrap();
    assert!(!t.complete);
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::Ratio)
    );
    assert!(has(&t.breaches, BreachKind::Ratio, Some(0)));
    assert!(
        t.bytes_read < 4 << 20,
        "stopped early, read {}",
        t.bytes_read
    );
}

#[test]
fn tar_gz_ratio_bomb_is_stopped_while_decompressing() {
    let t = client().test(&fixture("bomb_ratio.tar.gz"), None).unwrap();
    assert!(!t.complete);
    assert!(
        has(&t.breaches, BreachKind::Ratio, Some(0)),
        "{:?}",
        t.breaches
    );
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::Ratio)
    );
    assert_eq!(
        t.stream_check,
        StreamCheck::NotReached,
        "the trailer was never reached"
    );
    // Listing is bounded the same way: it must decompress to list.
    let l = client().list(&fixture("bomb_ratio.tar.gz"), None).unwrap();
    assert!(!l.complete);
    assert!(has(&l.breaches, BreachKind::Ratio, Some(0)));
}

#[test]
fn a_small_file_of_zeros_is_not_a_bomb() {
    // ratio_min_output keeps tiny, highly compressible files out of the check.
    let l = Limits {
        max_ratio: 2,
        ratio_min_output: 1 << 30,
        ..Limits::default()
    };
    let t = client_with(l)
        .test(&fixture("bomb_ratio.zip"), None)
        .unwrap();
    assert_eq!(t.problems(), 0, "{:?}", t.entries[0].outcome);
}

// ---- entry count ----------------------------------------------------------------

#[test]
fn entry_count_limit_stops_listing_in_every_format() {
    let l = Limits {
        max_entries: 100,
        ..unlimited()
    };
    for f in ["bomb_count.zip", "bomb_count.tar", "bomb_count.tar.gz"] {
        let c = client_with(l);
        let r = c.list(&fixture(f), None).unwrap();
        assert_eq!(r.entries.len(), 100, "{f}");
        assert!(!r.complete, "{f}");
        assert!(
            has(&r.breaches, BreachKind::EntryCount, None),
            "{f}: {:?}",
            r.breaches
        );
        assert!(
            r.breaches
                .iter()
                .any(|b| b.limit == 100 && b.observed > 100),
            "{f}"
        );
        let t = c.test(&fixture(f), None).unwrap();
        assert_eq!(t.entries.len(), 100, "{f}");
        assert!(!t.complete, "{f}");
    }
}

#[test]
fn archives_under_the_entry_limit_are_untouched() {
    let l = Limits {
        max_entries: 300,
        ..unlimited()
    };
    for f in ["bomb_count.zip", "bomb_count.tar", "bomb_count.tar.gz"] {
        let r = client_with(l).list(&fixture(f), None).unwrap();
        assert_eq!(r.entries.len(), 300, "{f}");
        assert!(r.complete && r.breaches.is_empty(), "{f}");
    }
}

// ---- per-entry size --------------------------------------------------------------

#[test]
fn entry_size_limit_names_the_entry_and_the_run_continues() {
    let l = Limits {
        max_entry_size: 1 << 20,
        ..unlimited()
    };
    // ZIP: the second entry here is fine, the 8 MiB one is not.
    let t = client_with(l)
        .test(&fixture("bomb_ratio.zip"), None)
        .unwrap();
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::EntrySize)
    );
    assert!(t.breaches.iter().any(|b| b.kind == BreachKind::EntrySize
        && b.entry == Some(0)
        && b.limit == 1 << 20
        && b.observed == 8 << 20));
    assert!(t.complete, "an entry-size breach skips that entry only");
    // tar.gz
    let t = client_with(l)
        .test(&fixture("bomb_ratio.tar.gz"), None)
        .unwrap();
    assert_eq!(
        t.entries[0].outcome,
        EntryOutcome::Breach(BreachKind::EntrySize)
    );
    assert!(has(&t.breaches, BreachKind::EntrySize, Some(0)));
    // The rest of this archive is intact entries, which still read.
    let t = client_with(l).test(&fixture("basic.tar"), None).unwrap();
    assert_eq!(t.problems(), 0);
}

// ---- total output ----------------------------------------------------------------

#[test]
fn total_output_limit_stops_the_operation_at_the_entry_that_crossed_it() {
    // bomb_total.zip: six entries of 600 KiB; a 1 MiB total allows one and a bit.
    let l = Limits {
        max_total_output: 1 << 20,
        ..unlimited()
    };
    let t = client_with(l)
        .test(&fixture("bomb_total.zip"), None)
        .unwrap();
    assert!(!t.complete);
    assert_eq!(t.entries.len(), 2, "entries after the breach are not run");
    assert!(t.entries[0].outcome.is_clean());
    assert_eq!(
        t.entries[1].outcome,
        EntryOutcome::Breach(BreachKind::TotalOutput)
    );
    assert!(has(&t.breaches, BreachKind::TotalOutput, Some(1)));

    // A read counts against the same total.
    let r = client_with(Limits {
        max_total_output: 1000,
        ..unlimited()
    })
    .read_entry(&fixture("bomb_total.zip"), 0, None, &mut |_| Ok(()))
    .unwrap();
    assert_eq!(r.outcome, EntryOutcome::Breach(BreachKind::TotalOutput));

    // tar.gz: the stream is cut and its trailer is never reached.
    let t = client_with(l)
        .test(&fixture("bomb_ratio.tar.gz"), None)
        .unwrap();
    assert!(!t.complete);
    assert!(
        has(&t.breaches, BreachKind::TotalOutput, Some(0)),
        "{:?}",
        t.breaches
    );
    assert_eq!(t.stream_check, StreamCheck::NotReached);
}

// ---- name length ------------------------------------------------------------------

#[test]
fn over_long_names_are_a_breach_never_a_truncation() {
    let l = Limits {
        max_name_len: 256,
        ..unlimited()
    };
    for (f, len) in [("long_name.zip", 1004u64), ("longname_gnu.tar", 304)] {
        let r = client_with(l).list(&fixture(f), None).unwrap();
        assert!(r.entries[0].name.is_empty(), "{f}: the name is not carried");
        assert!(r.entries[0].error.is_some(), "{f}");
        assert!(
            r.breaches.iter().any(|b| b.kind == BreachKind::NameLength
                && b.entry == Some(0)
                && b.observed == len
                && b.limit == 256),
            "{f}: {:?}",
            r.breaches
        );
    }
}

#[test]
fn default_limits_do_not_disturb_ordinary_archives() {
    let c = client();
    for f in [
        "basic_infozip.zip",
        "basic.tar",
        "basic.tar.gz",
        "traversal.zip",
        "traversal.tar",
    ] {
        let r = c.test(&fixture(f), None).unwrap();
        assert!(r.complete && r.breaches.is_empty(), "{f}");
    }
}
