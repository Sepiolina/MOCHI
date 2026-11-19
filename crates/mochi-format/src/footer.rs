//! The framed footer (spec §8.4).
//!
//! A footer is a skippable frame: an 8-byte header (`COMMIT_FOOTER`, size 64)
//! followed by a 64-byte payload:
//!
//! | Payload offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | magic `MOCHI2\0\0` |
//! | 8 | 8 | commit-frame offset (from byte zero of the archive) |
//! | 16 | 8 | commit-frame stored length (header + payload) |
//! | 24 | 8 | commit sequence |
//! | 32 | 32 | footer digest |
//!
//! Integers are unsigned little-endian. A reader that finds 64 plausible bytes
//! at EOF **must also validate the preceding skippable header** (spec §8.4),
//! so validation here always reads all 72 bytes.
//!
//! Validation is keyless: the digest covers *stored* commit-frame bytes, so it
//! works on encrypted archives too.
//!
//! Assumption recorded for ratification (plan §9, O17): the commit frame must
//! lie entirely before the footer that names it. The spec says physical order
//! may vary but does not allow a footer to precede the commit it commits to.

use crate::digest::{FooterDigest, StoredObjectBytes};
use crate::error::{FooterFault, FormatError, Result};
use crate::frame::{walk_frame, FrameDetail};
use crate::limits::Limits;
use crate::registry::{self, FrameKind};
use crate::source::{read_array, read_exact, ReadAt};

/// Total footer frame length: 8-byte skippable header + 64-byte payload.
pub const FOOTER_FRAME_LEN: u64 =
    (registry::SKIPPABLE_HEADER_LEN + registry::FOOTER_PAYLOAD_LEN) as u64;

/// Fields decoded from a footer payload. Untrusted until validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FooterFields {
    pub commit_offset: u64,
    pub commit_len: u64,
    pub commit_sequence: u64,
    pub digest: [u8; 32],
}

/// A footer that passed every check in [`validate_footer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedFooter {
    pub footer_offset: u64,
    pub fields: FooterFields,
    /// Kind of the stored commit frame. Structural only: whether this kind is
    /// acceptable for a commit is decided above this layer.
    pub commit_kind: FrameKind,
}

/// Build the 72-byte footer frame for a stored commit frame placed at
/// `commit_offset`.
pub fn encode_footer_frame(
    commit_offset: u64,
    commit_sequence: u64,
    commit_frame: &[u8],
) -> [u8; FOOTER_FRAME_LEN as usize] {
    let mut out = [0u8; FOOTER_FRAME_LEN as usize];
    out[0..4].copy_from_slice(&registry::COMMIT_FOOTER.to_le_bytes());
    out[4..8].copy_from_slice(&(registry::FOOTER_PAYLOAD_LEN as u32).to_le_bytes());
    let p = registry::SKIPPABLE_HEADER_LEN;
    out[p..p + 8].copy_from_slice(&registry::FOOTER_MAGIC);
    out[p + 8..p + 16].copy_from_slice(&commit_offset.to_le_bytes());
    out[p + 16..p + 24].copy_from_slice(&(commit_frame.len() as u64).to_le_bytes());
    out[p + 24..p + 32].copy_from_slice(&commit_sequence.to_le_bytes());
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&out[p..p + 32]);
    let digest = crate::digest::footer_digest(&prefix, StoredObjectBytes::new(commit_frame));
    out[p + 32..p + 64].copy_from_slice(&digest);
    out
}

/// Read and structurally decode the footer frame at `footer_offset` without
/// checking the digest or the commit frame.
pub fn parse_footer_frame(
    src: &(impl ReadAt + ?Sized),
    footer_offset: u64,
) -> Result<FooterFields> {
    let header = read_array::<8>(src, footer_offset)?;
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let size = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if magic != registry::COMMIT_FOOTER || size as usize != registry::FOOTER_PAYLOAD_LEN {
        return Err(FormatError::Footer(FooterFault::HeaderMismatch));
    }
    let payload_at = footer_offset
        .checked_add(registry::SKIPPABLE_HEADER_LEN as u64)
        .ok_or(FormatError::Overflow {
            offset: footer_offset,
        })?;
    let payload = read_array::<{ registry::FOOTER_PAYLOAD_LEN }>(src, payload_at)?;
    if payload[0..8] != registry::FOOTER_MAGIC {
        return Err(FormatError::Footer(FooterFault::BadPayloadMagic));
    }
    let u64_at = |i: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&payload[i..i + 8]);
        u64::from_le_bytes(b)
    };
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&payload[32..64]);
    Ok(FooterFields {
        commit_offset: u64_at(8),
        commit_len: u64_at(16),
        commit_sequence: u64_at(24),
        digest,
    })
}

/// Fully validate the footer at `footer_offset` (spec §8.4).
///
/// Checks, in order: the preceding skippable header, the payload magic, that
/// the named commit range is non-empty, overflow-free and entirely before the
/// footer, that a skippable frame of exactly the stated length sits there, and
/// finally the domain-separated BLAKE3 digest over payload bytes 0–31 plus the
/// exact stored commit-frame bytes.
pub fn validate_footer(
    src: &(impl ReadAt + ?Sized),
    footer_offset: u64,
    limits: &Limits,
) -> Result<ValidatedFooter> {
    let fields = parse_footer_frame(src, footer_offset)?;
    let bad_range = FormatError::Footer(FooterFault::CommitRangeInvalid);

    if fields.commit_len < registry::SKIPPABLE_HEADER_LEN as u64 {
        return Err(bad_range);
    }
    let commit_end = fields
        .commit_offset
        .checked_add(fields.commit_len)
        .ok_or_else(|| bad_range.clone())?;
    if commit_end > footer_offset {
        return Err(bad_range);
    }
    if fields.commit_len > limits.max_commit_frame_len {
        return Err(FormatError::LimitExceeded {
            kind: crate::error::LimitKind::CommitFrameLength,
            limit: limits.max_commit_frame_len,
            actual: fields.commit_len,
        });
    }

    // The bytes at the offset must be a skippable frame of exactly this length.
    let malformed = FormatError::Footer(FooterFault::CommitFrameMalformed);
    let span = match walk_frame(src, fields.commit_offset, limits) {
        Ok(span) => span,
        Err(e @ FormatError::Source(_)) => return Err(e),
        Err(_) => return Err(malformed),
    };
    if span.len != fields.commit_len || !matches!(span.detail, FrameDetail::Skippable { .. }) {
        return Err(malformed);
    }

    // Stream the stored commit-frame bytes through the digest.
    let p = registry::SKIPPABLE_HEADER_LEN as u64;
    let prefix_bytes = read_array::<32>(src, footer_offset + p)?;
    let mut hasher = FooterDigest::new(&prefix_bytes);
    let mut buf = [0u8; 16 * 1024];
    let mut at = fields.commit_offset;
    while at < commit_end {
        let n = usize::try_from((commit_end - at).min(buf.len() as u64))
            .map_err(|_| FormatError::Overflow { offset: at })?;
        read_exact(src, at, &mut buf[..n])?;
        hasher.update(StoredObjectBytes::new(&buf[..n]));
        at += n as u64;
    }
    if hasher.finalize() != fields.digest {
        return Err(FormatError::Footer(FooterFault::DigestMismatch));
    }

    Ok(ValidatedFooter {
        footer_offset,
        fields,
        commit_kind: span.kind,
    })
}

/// Validate the footer at a clean committed EOF: the final 72 bytes.
///
/// Reading only the last 64 bytes is permitted by the spec, but the preceding
/// header must be validated as well, so this reads and checks all 72.
pub fn validate_footer_at_eof(
    src: &(impl ReadAt + ?Sized),
    limits: &Limits,
) -> Result<ValidatedFooter> {
    let len = src.len();
    let footer_offset = len
        .checked_sub(FOOTER_FRAME_LEN)
        .ok_or(FormatError::Truncated { offset: len })?;
    validate_footer(src, footer_offset, limits)
}
