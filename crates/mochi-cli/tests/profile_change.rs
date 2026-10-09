//! T18: enabling encryption in place exits 4 (spec Annex B.2 D12: "there is
//! no in-place conversion to the Encrypted profile in 1.0; a request for one
//! exits 4"). `append` has no CLI surface yet (C14, T29), so the request is
//! made through `mochi-core` and its error mapped by the CLI's own
//! `exit_code_for`, the function every command's error goes through.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_cli::{exit, exit_code_for};
use mochi_core::descriptor::Profile;
use mochi_core::publish::{ArchiveWriter, TailPolicy, WriterOptions};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{build, scripted_history, test_options};
use mochi_testkit::{SeqIds, SimStorage};

#[test]
fn enabling_encryption_in_place_exits_4() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let before = s.contents();
    let opts = WriterOptions {
        profile: Some(Profile {
            tar_compatible: false,
            encrypted: true,
        }),
        ..test_options()
    };
    let e = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(100)),
        opts,
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::ProfileChangeUnsupported, "{e}");
    assert_eq!(exit_code_for(e.code), exit::UNSUPPORTED);
    assert_eq!(exit::UNSUPPORTED, 4);
    assert_eq!(s.contents(), before, "a refused append writes nothing");
}

/// Creating an Encrypted archive needs a passphrase (C11, D20 item 9): without
/// one the core refuses before a byte is written, an operational error.
#[test]
fn creating_an_encrypted_archive_without_a_passphrase_is_refused() {
    let opts = WriterOptions {
        profile: Some(Profile {
            tar_compatible: false,
            encrypted: true,
        }),
        ..test_options()
    };
    let s = SimStorage::new();
    let e = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), opts)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(exit_code_for(e.code), exit::ERROR);
    assert!(s.contents().is_empty(), "nothing was written");
}
