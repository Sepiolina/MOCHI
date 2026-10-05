//! The representation pipeline (spec §9.1), stage by stage.
//!
//! Encode: [`encode`] (compress) → [`protect`] (AEAD, if any) →
//! [`frame_object`] (record framing). Decode runs the stages in reverse inside
//! [`decode_object`], with every length checked against the caller's
//! expectation and the §8.5 limits before anything is allocated or trusted.
//!
//! **What exists in C2.** One encoding (a single Zstandard data frame) and one
//! protection mode (none). For an unprotected object the encoded plaintext,
//! stored payload, and stored object are byte-identical, but they remain
//! distinct types, and each stage still checks what it is handed. Encryption
//! (the [`crate::registry::ENCRYPTED_OBJECT`] envelope, spec §14) is C11 and blocked on R5;
//! dictionary resolution arrives with C8/C9.
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

use crate::error::{CodecFault, FormatError, LimitKind, Result};
use crate::frame::{walk_frame, DataFrameInfo, FrameDetail};
use crate::limits::Limits;
use crate::registry::FrameKind;
use crate::repr::{DecodedBytes, EncodedPlaintext, StoredObject, StoredPayload};

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
    /// Authenticated encryption in a [`crate::registry::ENCRYPTED_OBJECT`] envelope. C11; blocked on R5.
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

/// Stage 2: protect encoded plaintext. Only [`Protection::None`] exists in C2.
pub fn protect(plaintext: EncodedPlaintext, protection: Protection) -> Result<StoredPayload> {
    match protection {
        Protection::None => Ok(StoredPayload::from_codec(plaintext.into_inner())),
        Protection::Aead => Err(FormatError::Unsupported(
            "encrypted objects (spec §14; plan C11, blocked on R5)",
        )),
    }
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
        Protection::Aead => Err(FormatError::Unsupported(
            "encrypted objects (spec §14; plan C11, blocked on R5)",
        )),
    }
}

/// All three encode stages.
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
    if expected_len > limits.max_decoded_object_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::DecodedObjectLength,
            limit: limits.max_decoded_object_len,
            actual: expected_len,
        });
    }
    // Stage 3⁻¹: unframe.
    let (payload, info) = match protection {
        Protection::None => {
            let info = single_data_frame(stored.as_bytes(), limits)?;
            (stored.as_bytes(), info)
        }
        Protection::Aead => {
            return Err(FormatError::Unsupported(
                "encrypted objects (spec §14; plan C11, blocked on R5)",
            ))
        }
    };
    // Stage 2⁻¹: unprotect (identity for Protection::None).
    let plaintext = payload;

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
    fn encryption_is_unsupported_not_silently_plain() {
        let limits = Limits::default();
        let d = DecodedBytes::new(b"x".to_vec());
        let e = encode_object(&d, &EncodeParams::default(), Protection::Aead, &limits).unwrap_err();
        assert_eq!(e.class(), ErrorClass::Unsupported);
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
