//! Catalog image records (spec Annex B.2.2 "Binary envelope v0"; D11; plan T10).
//!
//! A checkpoint's catalog image is stored as one `METADATA_DELTA` skippable
//! frame whose payload is the binary envelope v0 header followed by the
//! SQLite image. The envelope codec, and every D11 obligation except the
//! integrity scope, live in `mochi_format::envelope`; this module only fixes
//! what is catalog-specific and maps it into core:
//!
//! | Envelope field | Catalog image |
//! |---|---|
//! | Record type | frame kind `METADATA_DELTA` (commit key 5, checkpoint form, key 1) |
//! | Record schema version | [`IMAGE_RECORD_SCHEMA`] = the catalog's SQLite `user_version` |
//! | Payload encoding | 0, SQLite 3 image |
//! | Required features | none known in this build ([`KNOWN_IMAGE_FEATURES`]) |
//! | Identity | archive ID, sequence, transaction ID of the checkpoint commit |
//! | Payload budget | *S* − 592 (B.2.3), 268,434,864 bytes at the defaults |
//!
//! **Integrity scope.** [`decode_image_record`] requires that the caller has
//! already checked the stored-object hash recorded by the referencing commit.
//! That ordering is load-bearing twice over: SQLite has no page checksums
//! (C3), and the envelope's identity fields are only meaningful once the
//! bytes are bound to the commit by hash (D11).
//!
//! **Writer limits.** The encoder always applies the reader defaults
//! ([`Limits::WRITER_DEFAULT`], B.2.3 "writer default rule"), whatever limits
//! the writer was configured to *read* with. An image over budget is
//! `CAPACITY_EXCEEDED` and nothing is published (D10.11). The opt-in path
//! (`create --exceed-default-limits`, plan T29) is deferred (Q10; spec
//! Annex B, D16), so no writer exceeds the defaults.
//!
//! **Effective capacity.** A published catalog is a whole number of 4096-byte
//! pages (§10.5), and the budget is not a multiple of 4096. The largest image
//! that passes is 65,535 pages (268,431,360 bytes); 65,536 pages is exactly
//! *S* and is refused. See the checklist's T10 note.

use mochi_format::envelope::{
    decode_binary_record, encode_binary_record, BinaryEnvelope, EnvelopeRules, PayloadEncoding,
    RecordIdentity,
};
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;
use mochi_format::Limits;

use crate::catalog::schema;
use crate::error::Result;
use crate::publish::skippable_payload;

/// The record schema version of a catalog image envelope: the catalog's
/// SQLite `user_version`, which the envelope carries as a u16 (B.2.2).
pub const IMAGE_RECORD_SCHEMA: u16 = {
    assert!(schema::SCHEMA_VERSION >= 0 && schema::SCHEMA_VERSION <= u16::MAX as i32);
    schema::SCHEMA_VERSION as u16
};

/// Image record schema versions this build reads.
pub const IMAGE_SCHEMA_VERSIONS: &[u16] = &[IMAGE_RECORD_SCHEMA];

/// Required features this build understands on a catalog image. None in
/// 1.0 Core; unknown features are refused (D11).
pub const KNOWN_IMAGE_FEATURES: &[u64] = &[];

/// The reader rules for a field that references a catalog image.
pub const IMAGE_RULES: EnvelopeRules<'static> = EnvelopeRules {
    kind: FrameKind::MetadataDelta,
    schema_versions: IMAGE_SCHEMA_VERSIONS,
    known_features: KNOWN_IMAGE_FEATURES,
};

/// Wrap a published SQLite image for the checkpoint commit with `identity`,
/// as one complete stored frame, under the reader defaults.
///
/// Errors: `CAPACITY_EXCEEDED` if the image exceeds the B.2.3 budget or the
/// frame limits; `ENVELOPE_INVALID` if `image` does not start with the SQLite
/// signature or its `user_version` is not [`IMAGE_RECORD_SCHEMA`].
pub fn encode_image_record(image: &[u8], identity: RecordIdentity) -> Result<StoredObject> {
    let envelope = BinaryEnvelope {
        record_schema_version: IMAGE_RECORD_SCHEMA,
        encoding: PayloadEncoding::SqliteImage,
        identity,
        required_features: Vec::new(),
    };
    let frame = encode_binary_record(
        FrameKind::MetadataDelta,
        &envelope,
        image,
        KNOWN_IMAGE_FEATURES,
        &Limits::WRITER_DEFAULT,
    )?;
    Ok(StoredObject::from_loaded(frame))
}

/// The SQLite image inside a catalog image record, after every D11
/// obligation: frame kind, envelope version, header and payload lengths,
/// the payload budget under `limits`, encoding and signature, required
/// features, schema version = `user_version`, and identity = `expected`.
///
/// **Precondition:** `stored` has been verified against the stored-object
/// hash its commit records. Nothing here can detect a substituted image that
/// carries the right identity.
///
/// Errors carry the format layer's codes: `MALFORMED_FRAME` for anything but
/// exactly one `METADATA_DELTA` frame, `ENVELOPE_INVALID` for an envelope
/// fault (including an identity mismatch), `UNSUPPORTED_FEATURE` for an
/// unknown envelope version, schema version, or required feature, and
/// `LIMIT_EXCEEDED` for an image over the reader's budget.
pub fn decode_image_record<'a>(
    stored: &'a StoredObject,
    expected: &RecordIdentity,
    limits: &Limits,
) -> Result<&'a [u8]> {
    let payload = skippable_payload(stored, FrameKind::MetadataDelta, limits)?;
    let candidate = decode_binary_record(
        mochi_format::registry::METADATA_DELTA,
        payload,
        &IMAGE_RULES,
        limits,
    )?;
    let (_, image) = candidate.bind(expected)?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::error::ErrorCode;
    use mochi_format::registry::SKIPPABLE_HEADER_LEN;

    fn identity(seq: u64) -> RecordIdentity {
        RecordIdentity {
            archive_id: [7; 32],
            commit_sequence: seq,
            transaction_id: [9; 16],
        }
    }

    fn real_image() -> Vec<u8> {
        Catalog::new_working()
            .unwrap()
            .publish()
            .unwrap()
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn round_trip_and_header_layout() {
        let img = real_image();
        let stored = encode_image_record(&img, identity(3)).unwrap();
        let b = stored.as_bytes();
        // 8-byte skippable header, then the 80-byte envelope (n = 0).
        assert_eq!(b.len(), SKIPPABLE_HEADER_LEN + 80 + img.len());
        let e = &b[SKIPPABLE_HEADER_LEN..];
        assert_eq!(&e[0..4], &80u32.to_le_bytes()); // header length
        assert_eq!(&e[4..6], &0u16.to_le_bytes()); // envelope version
        assert_eq!(&e[6..8], &IMAGE_RECORD_SCHEMA.to_le_bytes()); // = user_version
        assert_eq!(&e[8..12], &0u32.to_le_bytes()); // SQLite image
        assert_eq!(&e[12..16], &0u32.to_le_bytes()); // n
        assert_eq!(&e[16..24], &(img.len() as u64).to_le_bytes());
        assert_eq!(&e[24..56], &[7; 32]);
        assert_eq!(&e[56..64], &3u64.to_le_bytes());
        assert_eq!(&e[64..80], &[9; 16]);
        assert_eq!(&e[80..], &img[..]);

        let back = decode_image_record(&stored, &identity(3), &Limits::default()).unwrap();
        assert_eq!(back, &img[..]);
        Catalog::open_image(back, &Default::default()).unwrap();
    }

    /// D11 identity: an image bound to another commit is refused, field by
    /// field, even though (by precondition) its hash matched.
    #[test]
    fn an_image_of_another_commit_is_refused() {
        let stored = encode_image_record(&real_image(), identity(3)).unwrap();
        let wrong = [
            RecordIdentity {
                archive_id: [8; 32],
                ..identity(3)
            },
            identity(4),
            RecordIdentity {
                transaction_id: [1; 16],
                ..identity(3)
            },
        ];
        for w in wrong {
            let e = decode_image_record(&stored, &w, &Limits::default()).unwrap_err();
            assert_eq!(e.code, ErrorCode::EnvelopeInvalid, "{w:?}");
        }
    }

    /// A pre-T10 archive stored the SQLite image bare. Its first bytes,
    /// read as an envelope, give envelope version 0x6574 ("te"): refused as
    /// unsupported, never handed to SQLite.
    #[test]
    fn a_bare_image_is_refused() {
        let img = real_image();
        let bare = StoredObject::from_loaded(
            mochi_format::frame::encode_skippable_frame_within(
                FrameKind::MetadataDelta,
                &img,
                &Limits::WRITER_DEFAULT,
            )
            .unwrap(),
        );
        let e = decode_image_record(&bare, &identity(0), &Limits::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature);
    }

    #[test]
    fn a_non_image_frame_is_refused() {
        let stored = encode_image_record(&real_image(), identity(0)).unwrap();
        let mut b = stored.as_bytes().to_vec();
        b[0..4].copy_from_slice(&mochi_format::registry::RECOVERY_MANIFEST.to_le_bytes());
        let e = decode_image_record(
            &StoredObject::from_loaded(b),
            &identity(0),
            &Limits::default(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::MalformedFrame);
    }

    #[test]
    fn the_writer_refuses_a_payload_that_is_not_a_catalog_of_this_schema() {
        let mut img = real_image();
        img[63] = 1; // user_version = 1, not IMAGE_RECORD_SCHEMA
        assert_eq!(
            encode_image_record(&img, identity(0)).unwrap_err().code,
            ErrorCode::EnvelopeInvalid
        );
        assert_eq!(
            encode_image_record(b"not sqlite", identity(0))
                .unwrap_err()
                .code,
            ErrorCode::EnvelopeInvalid
        );
    }
}
