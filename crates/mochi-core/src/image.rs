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
//! | Payload encoding | written: 2, Zstandard-compressed SQLite 3 image; read: 2, and 0 (draft 2, uncompressed SQLite 3 image) |
//! | Required features | none in Core; exactly `[1]` in the Encrypted profile (D20 item 4) |
//! | Identity | archive ID, sequence, transaction ID of the checkpoint commit |
//! | Payload budget | *S* − 592 (B.2.3), 268,434,864 bytes at the defaults: the stored payload, and also the decoded image of encoding 2 |
//!
//! **Integrity scope.** [`decode_image_record`] requires that the caller has
//! already checked the stored-object hash recorded by the referencing commit.
//! That ordering is load-bearing twice over: SQLite has no page checksums
//! (C3), and the envelope's identity fields are only meaningful once the
//! bytes are bound to the commit by hash (D11).
//!
//! **Compression.** A 1.0 writer always writes encoding 2: one Zstandard frame
//! (level [`mochi_format::envelope::IMAGE_ZSTD_LEVEL`], `Frame_Content_Size`
//! and checksum) around the SQLite image. Hashes stay on the *stored* frame
//! bytes and are checked before this module runs; decompression is bounded by
//! the declared size before allocation. The Encrypted profile seals the same
//! payload (header and compressed image); compress-then-seal, so the AEAD
//! tag, not the compressor, is what authenticates it.
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

use std::borrow::Cow;

use mochi_format::envelope::{
    compress_image, decode_binary_record, decode_image, encode_binary_record, BinaryEnvelope,
    EnvelopeRules, PayloadEncoding, RecordIdentity,
};
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;
use mochi_format::seal::FEATURE_ENCRYPTED;
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

/// Required features this build understands on a catalog image: the
/// Encrypted profile's (D20 item 4). Unknown features are refused (D11).
pub const KNOWN_IMAGE_FEATURES: &[u64] = &[FEATURE_ENCRYPTED];

/// The reader rules for a field that references a catalog image.
pub const IMAGE_RULES: EnvelopeRules<'static> = EnvelopeRules {
    kind: FrameKind::MetadataDelta,
    schema_versions: IMAGE_SCHEMA_VERSIONS,
    known_features: KNOWN_IMAGE_FEATURES,
};

/// Wrap a published SQLite image for the checkpoint commit with `identity`,
/// as one complete stored frame, under the reader defaults.
///
/// `encrypted` lists the Encrypted profile's required feature (D20); the
/// writer of that profile seals the frame's *payload* ([`image_payload`]) and
/// stores it as a sealed object instead of this frame.
///
/// Errors: `CAPACITY_EXCEEDED` if the image exceeds the B.2.3 budget or the
/// frame limits; `ENVELOPE_INVALID` if `image` does not start with the SQLite
/// signature or its `user_version` is not [`IMAGE_RECORD_SCHEMA`].
pub fn encode_image_record(
    image: &[u8],
    identity: RecordIdentity,
    encrypted: bool,
) -> Result<StoredObject> {
    if crate::descriptor::new_archive_draft().is_some() {
        // The test control that builds draft-2 sources for `mochi migrate`
        // (`set_legacy_draft_for_tests`) also writes draft 2's image form.
        return encode_image_record_draft2(image, identity, encrypted);
    }
    let envelope = BinaryEnvelope {
        record_schema_version: IMAGE_RECORD_SCHEMA,
        encoding: PayloadEncoding::ZstdSqliteImage,
        identity,
        required_features: if encrypted {
            vec![FEATURE_ENCRYPTED]
        } else {
            Vec::new()
        },
    };
    let compressed = compress_image(&envelope, image, &Limits::WRITER_DEFAULT)?;
    let frame = encode_binary_record(
        FrameKind::MetadataDelta,
        &envelope,
        &compressed,
        KNOWN_IMAGE_FEATURES,
        &Limits::WRITER_DEFAULT,
    )?;
    Ok(StoredObject::from_loaded(frame))
}

/// A draft-2 image record (payload encoding 0, uncompressed). No 1.0 writer
/// calls this; it exists so tests can build the archives `mochi migrate`
/// must still read and the readers must still accept.
pub fn encode_image_record_draft2(
    image: &[u8],
    identity: RecordIdentity,
    encrypted: bool,
) -> Result<StoredObject> {
    let envelope = BinaryEnvelope {
        record_schema_version: IMAGE_RECORD_SCHEMA,
        encoding: PayloadEncoding::SqliteImage,
        identity,
        required_features: if encrypted {
            vec![FEATURE_ENCRYPTED]
        } else {
            Vec::new()
        },
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
) -> Result<Cow<'a, [u8]>> {
    let payload = skippable_payload(stored, FrameKind::MetadataDelta, limits)?;
    decode_image_payload(payload, expected, limits, false)
}

/// The payload of an image frame: the binary envelope header and the SQLite
/// image. This is the plaintext a sealed image carries (D20 item 5).
pub fn image_payload(frame: &StoredObject) -> Result<&[u8]> {
    frame
        .as_bytes()
        .get(mochi_format::registry::SKIPPABLE_HEADER_LEN..)
        .ok_or_else(|| {
            crate::error::MochiError::new(
                crate::error::ErrorCode::MalformedFrame,
                "image frame shorter than its header",
            )
        })
}

/// [`decode_image_record`] for a payload already extracted (or opened from a
/// sealed object): every D11 obligation, and the required features equal
/// `[1]` exactly when `encrypted`, else none.
pub fn decode_image_payload<'a>(
    payload: &'a [u8],
    expected: &RecordIdentity,
    limits: &Limits,
    encrypted: bool,
) -> Result<Cow<'a, [u8]>> {
    let candidate = decode_binary_record(
        mochi_format::registry::METADATA_DELTA,
        payload,
        &IMAGE_RULES,
        limits,
    )?;
    let (envelope, payload) = candidate.bind(expected)?;
    let want: &[u64] = if encrypted { &[FEATURE_ENCRYPTED] } else { &[] };
    if envelope.required_features != want {
        return Err(crate::error::MochiError::new(
            crate::error::ErrorCode::RecordInvalid,
            "the image's required features disagree with the archive's profile \
             (the Encrypted feature is listed exactly in an Encrypted archive)",
        ));
    }
    Ok(decode_image(&envelope, payload, limits)?)
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
        let stored = encode_image_record(&img, identity(3), false).unwrap();
        let b = stored.as_bytes();
        // 8-byte skippable header, then the 80-byte envelope (n = 0).
        let e = &b[SKIPPABLE_HEADER_LEN..];
        let compressed = &e[80..];
        assert_eq!(b.len(), SKIPPABLE_HEADER_LEN + 80 + compressed.len());
        assert!(
            compressed.len() < img.len() / 4,
            "a catalog image compresses"
        );
        assert_eq!(&e[0..4], &80u32.to_le_bytes()); // header length
        assert_eq!(&e[4..6], &0u16.to_le_bytes()); // envelope version
        assert_eq!(&e[6..8], &IMAGE_RECORD_SCHEMA.to_le_bytes()); // = user_version
        assert_eq!(&e[8..12], &2u32.to_le_bytes()); // Zstandard-compressed SQLite image
        assert_eq!(&e[12..16], &0u32.to_le_bytes()); // n
        assert_eq!(&e[16..24], &(compressed.len() as u64).to_le_bytes());
        assert_eq!(&e[24..56], &[7; 32]);
        assert_eq!(&e[56..64], &3u64.to_le_bytes());
        assert_eq!(&e[64..80], &[9; 16]);
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);

        let back = decode_image_record(&stored, &identity(3), &Limits::default()).unwrap();
        assert_eq!(&back[..], &img[..]);
        Catalog::open_image(&back, &Default::default()).unwrap();
    }

    /// Writing is deterministic: same image, same bytes.
    #[test]
    fn the_writer_is_deterministic() {
        let img = real_image();
        let a = encode_image_record(&img, identity(3), false).unwrap();
        let b = encode_image_record(&img, identity(3), false).unwrap();
        assert_eq!(a.as_bytes(), b.as_bytes());
    }

    /// Draft 2 wrote encoding 0 (an uncompressed image); readers still accept it.
    #[test]
    fn a_draft_2_uncompressed_image_is_still_read() {
        let img = real_image();
        let stored = encode_image_record_draft2(&img, identity(3), false).unwrap();
        let b = stored.as_bytes();
        assert_eq!(b.len(), SKIPPABLE_HEADER_LEN + 80 + img.len());
        assert_eq!(
            &b[SKIPPABLE_HEADER_LEN + 8..SKIPPABLE_HEADER_LEN + 12],
            &0u32.to_le_bytes()
        );
        let back = decode_image_record(&stored, &identity(3), &Limits::default()).unwrap();
        assert!(matches!(back, Cow::Borrowed(_)));
        assert_eq!(&back[..], &img[..]);
    }

    /// Encoding 2 with a valid Zstandard frame of something that is not a
    /// SQLite image is `ENVELOPE_INVALID`; a damaged frame is an error, never
    /// a panic and never handed to SQLite. (The stored hash catches damage
    /// before this runs, `c5_publication`; these are the second line.)
    #[test]
    fn a_compressed_payload_that_is_not_an_image_is_refused() {
        let envelope = BinaryEnvelope {
            record_schema_version: IMAGE_RECORD_SCHEMA,
            encoding: PayloadEncoding::ZstdSqliteImage,
            identity: identity(3),
            required_features: Vec::new(),
        };
        let frame_of = |payload: &[u8]| {
            StoredObject::from_loaded(
                encode_binary_record(
                    FrameKind::MetadataDelta,
                    &envelope,
                    payload,
                    KNOWN_IMAGE_FEATURES,
                    &Limits::WRITER_DEFAULT,
                )
                .unwrap(),
            )
        };
        let text = mochi_format::codec::compress_frame(&vec![b'x'; 8192], 3).unwrap();
        let e =
            decode_image_record(&frame_of(&text), &identity(3), &Limits::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::EnvelopeInvalid, "{e}");

        let good = encode_image_record(&real_image(), identity(3), false).unwrap();
        let mut b = good.as_bytes().to_vec();
        let mid = SKIPPABLE_HEADER_LEN + 80 + (b.len() - SKIPPABLE_HEADER_LEN - 80) / 2;
        b[mid] ^= 0x08;
        assert!(decode_image_record(
            &StoredObject::from_loaded(b),
            &identity(3),
            &Limits::default()
        )
        .is_err());
    }

    /// D11 identity: an image bound to another commit is refused, field by
    /// field, even though (by precondition) its hash matched.
    #[test]
    fn an_image_of_another_commit_is_refused() {
        let stored = encode_image_record(&real_image(), identity(3), false).unwrap();
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
        let stored = encode_image_record(&real_image(), identity(0), false).unwrap();
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

    /// The pre-envelope form (a bare SQLite image in a metadata frame) is
    /// refused at the envelope, never handed to SQLite (T10, Q12).
    #[test]
    fn a_bare_image_is_unsupported_not_damage() {
        let mut frame = mochi_format::registry::METADATA_DELTA
            .to_le_bytes()
            .to_vec();
        let img = real_image();
        frame.extend_from_slice(&(img.len() as u32).to_le_bytes());
        frame.extend_from_slice(&img);
        let e = decode_image_record(
            &StoredObject::from_loaded(frame),
            &identity(0),
            &Limits::default(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature);
    }

    #[test]
    fn the_writer_refuses_a_payload_that_is_not_a_catalog_of_this_schema() {
        let mut img = real_image();
        img[63] = 0; // user_version = 0, the pre-freeze draft, not IMAGE_RECORD_SCHEMA
        assert_eq!(
            encode_image_record(&img, identity(0), false)
                .unwrap_err()
                .code,
            ErrorCode::EnvelopeInvalid
        );
        assert_eq!(
            encode_image_record(b"not sqlite", identity(0), false)
                .unwrap_err()
                .code,
            ErrorCode::EnvelopeInvalid
        );
    }
}
