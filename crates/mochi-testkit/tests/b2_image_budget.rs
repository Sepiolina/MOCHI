//! Plan T10 (gate G4): the catalog image budget at its **real** default,
//! *S* − 592 = 268,434,864 bytes (spec Annex B.2.3), through the core
//! functions the writer and `open_head` call (`mochi_core::image`).
//!
//! * an image of exactly the budget is written and read back;
//! * one byte over is refused by the writer with `CAPACITY_EXCEEDED`;
//! * one byte over, in a frame that still fits *S* (possible because a
//!   header with n = 0 is 512 bytes shorter than the largest), is refused by
//!   a default reader with `LIMIT_EXCEEDED`.
//!
//! **This is an envelope/limit boundary test with synthetic image bytes, not
//! a valid-catalog round trip.** The payloads carry only the SQLite signature
//! and `user_version`: the budget is enforced on byte counts before SQLite
//! sees anything, and a catalog of exactly the budget cannot exist (it is
//! not a whole number of 4096-byte pages). Valid-catalog coverage is in the
//! c3 and c5 tests. The last test pins what the budget means for real
//! catalogs: 65,535 pages (checklist question 11).
//!
//! Own test binary so its ~0.8 GB peak does not overlap other tests'
//! test binaries (cargo runs test binaries one at a time); the second test
//! allocates nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_core::image::{decode_image_record, encode_image_record, IMAGE_RECORD_SCHEMA};
use mochi_core::ErrorCode;
use mochi_format::envelope::{
    encode_binary_record, BinaryEnvelope, PayloadEncoding, RecordIdentity,
};
use mochi_format::limits::{DEFAULT_IMAGE_PAYLOAD, DEFAULT_SKIPPABLE_PAYLOAD};
use mochi_format::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::Limits;

const PAGE: u64 = 4096;

fn identity() -> RecordIdentity {
    RecordIdentity {
        archive_id: [0xA5; 32],
        commit_sequence: 41,
        transaction_id: [0x5A; 16],
    }
}

/// A zeroed payload of `len` bytes that passes the envelope's signature and
/// `user_version` checks (user_version 0 = zero bytes at 60..64).
fn sqlite_shaped(len: u64) -> Vec<u8> {
    assert_eq!(
        IMAGE_RECORD_SCHEMA, 0,
        "payload helper assumes user_version 0"
    );
    let mut v = vec![0u8; len as usize];
    v[..16].copy_from_slice(b"SQLite format 3\0");
    v
}

#[test]
fn image_budget_boundary_with_synthetic_image_bytes() {
    assert_eq!(DEFAULT_IMAGE_PAYLOAD, 268_434_864);
    assert_eq!(Limits::WRITER_DEFAULT, Limits::default());

    // Exactly the budget: written, and read back by a default reader.
    let mut payload = sqlite_shaped(DEFAULT_IMAGE_PAYLOAD);
    let stored = encode_image_record(&payload, identity()).unwrap();
    assert_eq!(
        stored.len(),
        SKIPPABLE_HEADER_LEN as u64 + 80 + DEFAULT_IMAGE_PAYLOAD
    );
    let back = decode_image_record(&stored, &identity(), &Limits::default()).unwrap();
    assert_eq!(back.len() as u64, DEFAULT_IMAGE_PAYLOAD);
    drop(stored);

    // One byte over: the writer refuses; nothing is emitted.
    payload.push(0);
    let e = encode_image_record(&payload, identity()).unwrap_err();
    assert_eq!(e.code, ErrorCode::CapacityExceeded, "{e}");

    // One byte over, written by a writer with raised limits: the frame
    // payload (80 + budget + 1) still fits S, and a default reader refuses
    // it on the image budget.
    let raised = Limits {
        max_skippable_payload: DEFAULT_SKIPPABLE_PAYLOAD + PAGE,
        ..Limits::default()
    };
    let frame = encode_binary_record(
        FrameKind::MetadataDelta,
        &BinaryEnvelope {
            record_schema_version: IMAGE_RECORD_SCHEMA,
            encoding: PayloadEncoding::SqliteImage,
            identity: identity(),
            required_features: Vec::new(),
        },
        &payload,
        &[],
        &raised,
    )
    .unwrap();
    drop(payload);
    assert!(frame.len() as u64 - SKIPPABLE_HEADER_LEN as u64 <= DEFAULT_SKIPPABLE_PAYLOAD);
    let over = StoredObject::from_loaded(frame);
    let e = decode_image_record(&over, &identity(), &Limits::default()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LimitExceeded, "{e}");
    // ... and a reader that raised its limit explicitly accepts it.
    assert!(decode_image_record(&over, &identity(), &raised).is_ok());
}

/// The budget is not page-aligned, so the largest catalog a default writer
/// can publish is 65,535 pages; 65,536 pages is exactly S. The 592-byte
/// header reserve therefore costs exactly one page of catalog capacity.
#[test]
fn effective_catalog_capacity_is_65535_pages() {
    let max_pages = DEFAULT_IMAGE_PAYLOAD / PAGE;
    assert_eq!(max_pages, 65_535);
    assert_eq!((max_pages + 1) * PAGE, DEFAULT_SKIPPABLE_PAYLOAD);
    assert!((max_pages + 1) * PAGE > DEFAULT_IMAGE_PAYLOAD);
    assert_eq!(
        mochi_core::catalog::CatalogLimits::default().max_image_len,
        DEFAULT_IMAGE_PAYLOAD
    );
}
