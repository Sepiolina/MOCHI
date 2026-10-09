//! The representation pipeline (spec §9.1), stage by stage.
//!
//! Encode: [`encode`] (compress) → [`protect`] (AEAD, if any) →
//! [`frame_object`] (record framing). Decode runs the stages in reverse inside
//! [`decode_object`], with every length checked against the caller's
//! expectation and the §8.5 limits before anything is allocated or trusted.
//!
//! **What exists.** One encoding (a single Zstandard data frame) and two
//! protection modes. For an unprotected object the encoded plaintext, stored
//! payload, and stored object are byte-identical, but they remain distinct
//! types, and each stage still checks what it is handed. A protected object
//! is *sealed* (spec Annex B.2.10, D20): the encoded plaintext, which is the
//! Zstandard frame, becomes the plaintext of one
//! [`crate::registry::ENCRYPTED_OBJECT`] frame ([`crate::seal`]); only
//! [`encode_object_sealed`] and [`decode_object_sealed`] do that, because
//! sealing needs a key, an archive, and the object's ID. Dictionary
//! resolution arrives with C8/C9.
//!
//! **Data-object profile (plan §9, O21, decided).** Every MOCHI data object is
//! exactly one Zstandard data frame that:
//!
//! * carries `Frame_Content_Size`: structural verification and catalog-less
//!   salvage (§22) can learn a chunk's decoded length without decompressing;
//! * carries the frame checksum: without a catalog, it is the only content
//!   check a salvage scan has for an unencrypted chunk (it stays supplementary
//!   to the chunk content hash, §8.5);
//! * has `Dictionary_ID` equal to the dictionary's own embedded ID, or 0 with
//!   no dictionary. The *record's* dependency is authoritative; the 32-bit
//!   frame field is a salvage hint and a consistency check only, because it
//!   is neither unique nor collision-resistant.
//!
//! "Compression disabled" is not a separate encoding: it is the same frame
//! made of raw blocks. An encoding names the decoder a reader needs, not the
//! writer's effort; one decode path keeps the checksum and content size for
//! every object. Stock `zstd` accepts all of this, so the TAR profile (§7.2)
//! is unaffected.
//!
//! Decoding does not verify digests; that is the object verifier's job in
//! `mochi-core`, which knows the record. This module guarantees only that the
//! decoded bytes are exactly the declared length and came from exactly one
//! structurally valid frame.

use std::io::Read;

use crate::error::{CodecFault, FormatError, LimitKind, Result, SealFault};
use crate::frame::{walk_frame, DataFrameInfo, FrameDetail};
use crate::limits::Limits;
use crate::registry::FrameKind;
use crate::repr::{DecodedBytes, EncodedPlaintext, StoredObject, StoredPayload};
use crate::seal::{open_payload, seal_payload, sealed_frame_payload, SealContext, SealTarget};
use crate::secret::Random;

/// How decoded bytes become encoded plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// Exactly one Zstandard data frame with `Frame_Content_Size` and the
    /// frame checksum present (module note, O21). Includes raw-block frames:
    /// "no compression" is a writer preset, not an encoding.
    ZstdFrame,
}

/// How encoded plaintext becomes a stored payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protection {
    /// Stored payload is the encoded plaintext.
    None,
    /// Authenticated encryption in a [`crate::registry::ENCRYPTED_OBJECT`]
    /// frame (Annex B.2.10, D20). Needs a key: [`encode_object_sealed`] and
    /// [`decode_object_sealed`]; the keyless entry points refuse it.
    Aead,
}

/// Writer parameters for [`encode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeParams {
    pub encoding: Encoding,
    /// Zstandard compression level. Recorded per archive by the caller
    /// (spec §13: chunking parameters must be recorded).
    pub level: i32,
}

impl Default for EncodeParams {
    fn default() -> Self {
        EncodeParams {
            encoding: Encoding::ZstdFrame,
            level: 3,
        }
    }
}

/// Stage 1: compress logical content into encoded plaintext.
pub fn encode(content: &DecodedBytes, params: &EncodeParams) -> Result<EncodedPlaintext> {
    match params.encoding {
        Encoding::ZstdFrame => {
            use zstd::zstd_safe::CParameter;
            let fail = |_| FormatError::Codec(CodecFault::EncoderFailed);
            let mut c = zstd::bulk::Compressor::new(params.level).map_err(fail)?;
            c.set_parameter(CParameter::ChecksumFlag(true))
                .map_err(fail)?;
            c.set_parameter(CParameter::ContentSizeFlag(true))
                .map_err(fail)?;
            c.set_parameter(CParameter::DictIdFlag(false))
                .map_err(fail)?;
            let out = c.compress(content.as_bytes()).map_err(fail)?;
            Ok(EncodedPlaintext::from_codec(out))
        }
    }
}

/// Stage 2: protect encoded plaintext. [`Protection::Aead`] needs a key and
/// is refused here with [`SealFault::NoKey`]; use [`protect_sealed`].
pub fn protect(plaintext: EncodedPlaintext, protection: Protection) -> Result<StoredPayload> {
    match protection {
        Protection::None => Ok(StoredPayload::from_codec(plaintext.into_inner())),
        Protection::Aead => Err(FormatError::Seal(SealFault::NoKey)),
    }
}

/// Stage 2 for [`Protection::Aead`]: seal the encoded plaintext (the
/// Zstandard frame) for the chunk `object_id` under `ctx`, with a fresh random
/// nonce. The stored payload is the sealed header, ciphertext, and tag.
pub fn protect_sealed(
    plaintext: EncodedPlaintext,
    ctx: &SealContext<'_>,
    object_id: &[u8; 32],
    rng: &mut dyn Random,
) -> Result<StoredPayload> {
    let bytes = plaintext.into_inner();
    let target = SealTarget::Chunk {
        object_id: *object_id,
    };
    Ok(StoredPayload::from_codec(seal_payload(
        ctx, &target, &bytes, rng,
    )?))
}

/// Stage 3 for [`Protection::Aead`]: frame a sealed payload as one
/// `0x184D2A59` skippable frame within `limits`.
pub fn frame_sealed(payload: StoredPayload, limits: &Limits) -> Result<StoredObject> {
    let frame = crate::frame::encode_skippable_frame_within(
        FrameKind::EncryptedObject,
        &payload.into_inner(),
        limits,
    )?;
    Ok(StoredObject::from_codec(frame))
}

/// Stage 3: frame a stored payload as stored object bytes.
///
/// For an unprotected Zstandard object the data frame *is* the framing, so
/// this checks that the payload is exactly one structurally valid data frame
/// (our walker must agree with libzstd's output) and relabels it.
pub fn frame_object(
    payload: StoredPayload,
    protection: Protection,
    limits: &Limits,
) -> Result<StoredObject> {
    match protection {
        Protection::None => {
            let bytes = payload.into_inner();
            single_data_frame(&bytes, limits)?;
            Ok(StoredObject::from_codec(bytes))
        }
        Protection::Aead => Err(FormatError::Seal(SealFault::NoKey)),
    }
}

/// All three encode stages, unprotected. [`Protection::Aead`] is refused with
/// [`SealFault::NoKey`]: see [`encode_object_sealed`].
pub fn encode_object(
    content: &DecodedBytes,
    params: &EncodeParams,
    protection: Protection,
    limits: &Limits,
) -> Result<StoredObject> {
    if content.len() > limits.max_decoded_object_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::DecodedObjectLength,
            limit: limits.max_decoded_object_len,
            actual: content.len(),
        });
    }
    let plaintext = encode(content, params)?;
    let payload = protect(plaintext, protection)?;
    frame_object(payload, protection, limits)
}

/// All three encode stages with [`Protection::Aead`]: compress, seal for the
/// chunk `object_id` under `ctx`, frame (spec Annex B.2.10 item 5). The
/// content limit and the frame limits are the same as for an unprotected
/// object.
pub fn encode_object_sealed(
    content: &DecodedBytes,
    params: &EncodeParams,
    ctx: &SealContext<'_>,
    object_id: &[u8; 32],
    rng: &mut dyn Random,
    limits: &Limits,
) -> Result<StoredObject> {
    if content.len() > limits.max_decoded_object_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::DecodedObjectLength,
            limit: limits.max_decoded_object_len,
            actual: content.len(),
        });
    }
    let plaintext = encode(content, params)?;
    let payload = protect_sealed(plaintext, ctx, object_id, rng)?;
    frame_sealed(payload, limits)
}

/// Seal an existing Zstandard data frame (one frame, walked structurally) for
/// the chunk `object_id` under `ctx`: the second half of a rewrite, which has
/// the frame but not the decoded bytes.
pub fn seal_data_frame(
    frame: &[u8],
    ctx: &SealContext<'_>,
    object_id: &[u8; 32],
    rng: &mut dyn Random,
    limits: &Limits,
) -> Result<StoredObject> {
    single_data_frame(frame, limits)?;
    let target = SealTarget::Chunk {
        object_id: *object_id,
    };
    let payload = StoredPayload::from_codec(seal_payload(ctx, &target, frame, rng)?);
    frame_sealed(payload, limits)
}

/// Open a sealed chunk and hand back its Zstandard frame, without
/// decompressing it: what a rewrite carries from one archive to another
/// (it re-seals the same frame under the new key and archive). The sealed
/// header, key ID, and tag are checked; the frame is checked to be one
/// structurally valid data frame.
pub fn unseal_object(
    stored: &StoredObject,
    ctx: &SealContext<'_>,
    object_id: &[u8; 32],
    limits: &Limits,
) -> Result<Vec<u8>> {
    let payload = sealed_frame_payload(stored, limits)?;
    let target = SealTarget::Chunk {
        object_id: *object_id,
    };
    let plaintext = open_payload(ctx, &target, payload)?;
    single_data_frame(&plaintext, limits)?;
    Ok(plaintext)
}

/// `bytes` must be exactly one Zstandard data frame, walked structurally.
fn single_data_frame(bytes: &[u8], limits: &Limits) -> Result<DataFrameInfo> {
    let span = walk_frame(bytes, 0, limits)?;
    if span.kind != FrameKind::ZstdData {
        return Err(FormatError::Codec(CodecFault::NotADataFrame));
    }
    if span.len != bytes.len() as u64 {
        return Err(FormatError::Codec(CodecFault::NotSingleFrame));
    }
    match span.detail {
        FrameDetail::Data(info) => Ok(info),
        FrameDetail::Skippable { .. } => Err(FormatError::Codec(CodecFault::NotADataFrame)),
    }
}

/// Decode stored object bytes back to exactly `expected_len` decoded bytes.
///
/// `expected_len` comes from the object record; it bounds allocation, and any
/// disagreement — with `Frame_Content_Size`, or with what the decoder actually
/// produces — is a content-integrity failure, never a truncated or padded
/// success (spec §9.3: a matching hash does not replace length validation).
pub fn decode_object(
    stored: &StoredObject,
    protection: Protection,
    expected_len: u64,
    limits: &Limits,
) -> Result<DecodedBytes> {
    match protection {
        // A sealed object needs a key: see [`decode_object_sealed`].
        Protection::Aead => Err(FormatError::Seal(SealFault::NoKey)),
        Protection::None => decode_data_frame(stored.as_bytes(), expected_len, limits),
    }
}

/// Decode a sealed chunk: unframe, open under `ctx` for the chunk `object_id`
/// (a tag failure is [`SealFault::Authentication`], reported as content
/// integrity), then the same bounded decompression as an unprotected object.
pub fn decode_object_sealed(
    stored: &StoredObject,
    ctx: &SealContext<'_>,
    object_id: &[u8; 32],
    expected_len: u64,
    limits: &Limits,
) -> Result<DecodedBytes> {
    if expected_len > limits.max_decoded_object_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::DecodedObjectLength,
            limit: limits.max_decoded_object_len,
            actual: expected_len,
        });
    }
    let plaintext = unseal_object(stored, ctx, object_id, limits)?;
    decode_data_frame(&plaintext, expected_len, limits)
}

/// Stages shared by both protection modes: `plaintext` must be one Zstandard
/// data frame meeting the O21 profile, whose decoded length is `expected_len`.
fn decode_data_frame(plaintext: &[u8], expected_len: u64, limits: &Limits) -> Result<DecodedBytes> {
    if expected_len > limits.max_decoded_object_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::DecodedObjectLength,
            limit: limits.max_decoded_object_len,
            actual: expected_len,
        });
    }
    // Stage 3⁻¹: unframe.
    let info = single_data_frame(plaintext, limits)?;

    // Profile checks (O21), then record agreement, then bounded decompression.
    let Some(declared) = info.declared_content_size else {
        return Err(FormatError::Codec(CodecFault::MissingContentSize));
    };
    if !info.has_checksum {
        return Err(FormatError::Codec(CodecFault::MissingChecksum));
    }
    // This codec is only reached for records with no dictionary dependency
    // (the object verifier refuses dependencies until C8/C9), so any declared
    // dictionary disagrees with the record.
    if info.dictionary_id != 0 {
        return Err(FormatError::Codec(CodecFault::DictionaryMismatch {
            frame_dictionary_id: info.dictionary_id,
        }));
    }
    if declared != expected_len {
        return Err(FormatError::Codec(CodecFault::DeclaredSizeMismatch {
            declared,
            expected: expected_len,
        }));
    }
    let decode_failed = |_| FormatError::Codec(CodecFault::DecodeFailed);
    let decoder = zstd::stream::read::Decoder::with_buffer(plaintext).map_err(decode_failed)?;
    let mut decoder = decoder.single_frame();
    decoder
        .window_log_max(window_log_for(limits.max_window_size))
        .map_err(decode_failed)?;

    // Read at most one byte more than expected, so overlong output is caught
    // without being fully materialized.
    let cap = expected_len.saturating_add(1);
    let initial = usize::try_from(expected_len.min(1 << 20)).unwrap_or(1 << 20);
    let mut out = Vec::with_capacity(initial);
    decoder
        .take(cap)
        .read_to_end(&mut out)
        .map_err(decode_failed)?;
    if out.len() as u64 != expected_len {
        return Err(FormatError::Codec(CodecFault::DecodedLengthMismatch {
            expected: expected_len,
            actual_at_least: out.len() as u64,
        }));
    }
    Ok(DecodedBytes::new(out))
}

/// Smallest `n` with `2^n >= max`, clamped to libzstd's accepted range.
fn window_log_for(max: u64) -> u32 {
    let n = if max <= 1 {
        0
    } else {
        64 - (max - 1).leading_zeros()
    };
    n.clamp(10, 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorClass;

    fn roundtrip(content: &[u8], level: i32) {
        let limits = Limits::default();
        let d = DecodedBytes::new(content.to_vec());
        let params = EncodeParams {
            level,
            ..EncodeParams::default()
        };
        let stored = encode_object(&d, &params, Protection::None, &limits).unwrap();
        let back = decode_object(&stored, Protection::None, d.len(), &limits).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn roundtrips_small_empty_and_compressible() {
        roundtrip(b"", 3);
        roundtrip(b"hello", 1);
        roundtrip(&[b'A'; 100_000], 3);
        roundtrip(&(0..=255u8).cycle().take(300_000).collect::<Vec<_>>(), -5);
    }

    #[test]
    fn writer_emits_checksum_and_content_size() {
        let limits = Limits::default();
        let d = DecodedBytes::new(b"payload".to_vec());
        let s = encode_object(&d, &EncodeParams::default(), Protection::None, &limits).unwrap();
        let info = single_data_frame(s.as_bytes(), &limits).unwrap();
        assert!(info.has_checksum);
        assert_eq!(info.declared_content_size, Some(7));
        assert_eq!(info.dictionary_id, 0);
    }

    #[test]
    fn wrong_expected_length_is_content_integrity_not_success() {
        let limits = Limits::default();
        let d = DecodedBytes::new(b"payload".to_vec());
        let s = encode_object(&d, &EncodeParams::default(), Protection::None, &limits).unwrap();
        for wrong in [0, 6, 8, 1000] {
            let e = decode_object(&s, Protection::None, wrong, &limits).unwrap_err();
            assert_eq!(e.class(), ErrorClass::ContentIntegrity, "{wrong}: {e}");
        }
    }

    #[test]
    fn trailing_bytes_or_second_frame_are_rejected() {
        let limits = Limits::default();
        let d = DecodedBytes::new(b"payload".to_vec());
        let s = encode_object(&d, &EncodeParams::default(), Protection::None, &limits).unwrap();
        let mut two = s.as_bytes().to_vec();
        two.extend_from_slice(s.as_bytes());
        let e = decode_object(
            &StoredObject::from_loaded(two),
            Protection::None,
            14,
            &limits,
        )
        .unwrap_err();
        assert_eq!(e, FormatError::Codec(CodecFault::NotSingleFrame));
    }

    #[test]
    fn a_protected_object_is_refused_without_a_key_never_stored_plain() {
        let limits = Limits::default();
        let d = DecodedBytes::new(b"x".to_vec());
        let e = encode_object(&d, &EncodeParams::default(), Protection::Aead, &limits).unwrap_err();
        assert_eq!(e, FormatError::Seal(SealFault::NoKey));
        assert_eq!(e.class(), ErrorClass::KeyUnavailable);
        let stored =
            encode_object(&d, &EncodeParams::default(), Protection::None, &limits).unwrap();
        let e = decode_object(&stored, Protection::Aead, 1, &limits).unwrap_err();
        assert_eq!(e.class(), ErrorClass::KeyUnavailable);
    }

    fn ctx(key: &crate::secret::DataKey) -> SealContext<'_> {
        SealContext {
            key,
            key_id: crate::seal::KeyId::from_bytes([9; 16]),
            archive_id: [3; 32],
        }
    }

    #[test]
    fn a_sealed_chunk_round_trips_and_is_bound_to_its_object_id() {
        let limits = Limits::default();
        let key = crate::secret::DataKey::from_bytes([5; 32]);
        let content = DecodedBytes::new(b"sealed chunk content ".repeat(100));
        let id = [1u8; 32];
        let stored = encode_object_sealed(
            &content,
            &EncodeParams::default(),
            &ctx(&key),
            &id,
            &mut crate::secret::OsRandom,
            &limits,
        )
        .unwrap();
        // One ENCRYPTED_OBJECT frame whose ciphertext is not the plain frame.
        assert_eq!(
            &stored.as_bytes()[..4],
            &crate::registry::ENCRYPTED_OBJECT.to_le_bytes()
        );
        let plain = encode_object(
            &content,
            &EncodeParams::default(),
            Protection::None,
            &limits,
        )
        .unwrap();
        assert!(!stored
            .as_bytes()
            .windows(plain.as_bytes().len().min(32))
            .any(|w| w == &plain.as_bytes()[..plain.as_bytes().len().min(32)]));
        let back = decode_object_sealed(&stored, &ctx(&key), &id, content.len(), &limits).unwrap();
        assert_eq!(back, content);
        // The Zstandard frame comes out unchanged, for a rewrite to re-seal.
        assert_eq!(
            unseal_object(&stored, &ctx(&key), &id, &limits).unwrap(),
            plain.as_bytes()
        );
        // Another object ID, key, or archive fails as content integrity.
        for bad in [
            decode_object_sealed(&stored, &ctx(&key), &[2u8; 32], content.len(), &limits),
            decode_object_sealed(
                &stored,
                &ctx(&crate::secret::DataKey::from_bytes([6; 32])),
                &id,
                content.len(),
                &limits,
            ),
        ] {
            assert_eq!(bad.unwrap_err().class(), ErrorClass::ContentIntegrity);
        }
        // The recorded decoded length is still checked after opening.
        assert_eq!(
            decode_object_sealed(&stored, &ctx(&key), &id, content.len() + 1, &limits)
                .unwrap_err()
                .class(),
            ErrorClass::ContentIntegrity
        );
        // The decoded-length limit applies before anything is opened.
        let tight = Limits {
            max_decoded_object_len: 4,
            ..limits
        };
        assert!(matches!(
            decode_object_sealed(&stored, &ctx(&key), &id, 5, &tight),
            Err(FormatError::LimitExceeded { .. })
        ));
        assert!(matches!(
            encode_object_sealed(
                &content,
                &EncodeParams::default(),
                &ctx(&key),
                &id,
                &mut crate::secret::OsRandom,
                &tight
            ),
            Err(FormatError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn decoded_length_limit_applies_both_ways() {
        let tight = Limits {
            max_decoded_object_len: 4,
            ..Limits::default()
        };
        let d = DecodedBytes::new(b"12345".to_vec());
        assert_eq!(
            encode_object(&d, &EncodeParams::default(), Protection::None, &tight)
                .unwrap_err()
                .class(),
            ErrorClass::LimitExceeded
        );
        let s = encode_object(
            &d,
            &EncodeParams::default(),
            Protection::None,
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(
            decode_object(&s, Protection::None, 5, &tight)
                .unwrap_err()
                .class(),
            ErrorClass::LimitExceeded
        );
    }

    #[test]
    fn window_log_rounds_up_and_clamps() {
        assert_eq!(window_log_for(0), 10);
        assert_eq!(window_log_for(1024), 10);
        assert_eq!(window_log_for(1025), 11);
        assert_eq!(window_log_for(128 << 20), 27);
        assert_eq!(window_log_for((128 << 20) + 1), 28);
        assert_eq!(window_log_for(u64::MAX), 31);
    }
}
