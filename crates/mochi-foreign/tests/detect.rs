//! Variants and non-archives: each is a typed answer, not a guess (plan D8
//! rule 8) and never a crash.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

fn code(name: &str) -> ErrorCode {
    client().list(&fixture(name), None).unwrap_err().code
}

#[test]
fn split_zip_segments_are_unsupported() {
    // The first segment (.z01) starts with the spanning marker; the last
    // segment (.zip) has a non-zero disk number in its end record.
    assert_eq!(code("split.z01"), ErrorCode::UnsupportedSplitZip);
    assert_eq!(code("split_last.zip"), ErrorCode::UnsupportedSplitZip);
    assert!(ErrorCode::UnsupportedSplitZip.is_unsupported());
}

#[test]
fn self_extracting_executable_is_unsupported() {
    assert_eq!(code("sfx.exe"), ErrorCode::UnsupportedSfx);
}

#[test]
fn a_bare_7z_or_rar_signature_is_a_candidate_not_proof() {
    // Detected by signature, then rejected by the format's own header checks.
    assert_eq!(code("fake.7z"), ErrorCode::MalformedArchive);
    assert_eq!(code("fake.rar"), ErrorCode::MalformedArchive);
}

#[test]
fn a_gzipped_single_file_is_not_a_tar_gz() {
    assert_eq!(code("single.gz"), ErrorCode::UnsupportedFormat);
}

#[test]
fn garbage_and_empty_files_are_not_archives() {
    assert_eq!(code("garbage.bin"), ErrorCode::NotAnArchive);
    assert_eq!(code("empty.bin"), ErrorCode::NotAnArchive);
}

#[test]
fn a_missing_file_is_open_failed() {
    assert_eq!(code("does-not-exist.zip"), ErrorCode::OpenFailed);
}

#[test]
fn test_and_read_refuse_the_same_variants() {
    let c = client();
    assert_eq!(
        c.test(&fixture("split.z01"), None).unwrap_err().code,
        ErrorCode::UnsupportedSplitZip
    );
    assert_eq!(
        c.read_entry(&fixture("sfx.exe"), 0, None, &mut |_| Ok(()))
            .unwrap_err()
            .code,
        ErrorCode::UnsupportedSfx
    );
}
