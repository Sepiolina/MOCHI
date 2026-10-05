//! Record envelopes (spec §8.3 as amended by Annex B.2 D11).
//!
//! **DRAFT** (ratification item R2). An envelope has two encodings:
//!
//! * **Deterministic-CBOR records** (commit, manifest, descriptor) carry the
//!   envelope as required keys in the record body. Their decoders live in
//!   `mochi-core`; they call the shared obligation checks in this module
//!   ([`check_required_features`], [`RecordIdentity::check`]) so the two
//!   encodings cannot drift apart.
//! * **Opaque payloads** (catalog images now; dictionaries and parity later)
//!   carry the **binary envelope v0** below at the start of the skippable
//!   payload.
//!
//! # Binary envelope v0 (Annex B.2.2)
//!
//! Integers unsigned little-endian; every byte is defined.
//!
//! | Offset | Size | Field | Rule |
//! |---|---|---|---|
//! | 0 | 4 | Header length | = 80 + 8·n |
//! | 4 | 2 | Envelope version | = 0 |
//! | 6 | 2 | Record schema version | catalog image: = SQLite `user_version` |
//! | 8 | 4 | Payload encoding | 0 = SQLite 3 image; others unregistered |
//! | 12 | 4 | n = required-feature count | ≤ 64 |
//! | 16 | 8 | Payload length | = frame payload − header length |
//! | 24 | 32 | Archive ID | |
//! | 56 | 8 | Commit sequence | |
//! | 64 | 16 | Transaction ID | |
//! | 80 | 8·n | Required features | u64, strictly increasing |
//!
//! # D11 obligations and where each is enforced
//!
//! | §8.3 field | Here |
//! |---|---|
//! | Record type | [`decode_binary_record`]: frame kind = [`EnvelopeRules::kind`] |
//! | Schema version | envelope version = 0; record schema version ∈ [`EnvelopeRules::schema_versions`]; for SQLite, = `user_version` |
//! | Required features | [`check_required_features`] (both encodings) |
//! | Archive identity, Identity | [`RecordIdentity::check`] (both encodings) |
//! | Payload encoding | registered value and its signature |
//! | Payload length | header field = frame payload − header |
//! | Integrity scope | **not here**: the referencing record's stored-object hash, checked by `mochi-core` before this decoder runs. A record found by scanning is a [`Candidate`] until bound. |
//!
//! Footers have their own fixed layout (§8.4) and never carry an envelope.
//! No envelope contains the ID of the commit that references it: the commit
//! ID covers the object's hash, so that would be circular (D11).

use crate::error::{CapacityKind, EnvelopeFault, FormatError, LimitKind, Result};
use crate::frame::encode_skippable_frame_within;
use crate::limits::{Limits, DEFAULT_REQUIRED_FEATURES, MAX_BINARY_ENVELOPE_HEADER};
use crate::registry::{self, FrameKind};

/// Fixed part of the binary envelope v0 header.
pub const BINARY_ENVELOPE_FIXED_LEN: usize = 80;

/// The only binary envelope version this build reads or writes.
pub const BINARY_ENVELOPE_VERSION: u16 = 0;

/// Binary envelope v0 wire rule: n ≤ 64 (B.2.2). Independent of any reader
/// limit: raising [`Limits::max_required_features`] does not admit a longer
/// v0 header. See the module note on the open question this leaves.
pub const BINARY_ENVELOPE_MAX_FEATURES: u64 = DEFAULT_REQUIRED_FEATURES;

/// Offset of SQLite's `user_version` in a database header (big-endian i32).
const SQLITE_USER_VERSION_AT: usize = 60;

/// Registered payload encodings (B.2.2). Unregistered values are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadEncoding {
    /// 0: a SQLite 3 database image; payload starts `SQLite format 3\0`.
    SqliteImage,
}

impl PayloadEncoding {
    pub const fn code(self) -> u32 {
        match self {
            PayloadEncoding::SqliteImage => 0,
        }
    }

    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(PayloadEncoding::SqliteImage),
            _ => None,
        }
    }

    /// The bytes every payload of this encoding must start with.
    pub const fn signature(self) -> &'static [u8] {
        match self {
            PayloadEncoding::SqliteImage => &registry::SQLITE_IMAGE_SIGNATURE,
        }
    }
}

/// Frame kinds that carry a binary envelope. Only catalog images exist in
/// this build; dictionaries (C8) and parity (C12) join when their payload
/// encodings are registered.
pub const fn carries_binary_envelope(kind: FrameKind) -> bool {
    matches!(kind, FrameKind::MetadataDelta)
}

/// The identity a record must share with the commit that references it
/// (D11 "Archive identity" and "Identity"). Used by both encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordIdentity {
    pub archive_id: [u8; 32],
    pub commit_sequence: u64,
    pub transaction_id: [u8; 16],
}

impl RecordIdentity {
    /// Check that a record's identity equals the referencing commit's. Each
    /// field has its own fault; the archive ID is checked first because a
    /// record from another archive is the most informative diagnosis.
    pub fn check(&self, expected: &RecordIdentity) -> Result<()> {
        if self.archive_id != expected.archive_id {
            return Err(FormatError::Envelope(EnvelopeFault::ArchiveIdMismatch));
        }
        if self.commit_sequence != expected.commit_sequence {
            return Err(FormatError::Envelope(EnvelopeFault::SequenceMismatch));
        }
        if self.transaction_id != expected.transaction_id {
            return Err(FormatError::Envelope(EnvelopeFault::TransactionIdMismatch));
        }
        Ok(())
    }
}

/// The D11 required-features obligation, for both encodings, in this order:
/// the count limit (bounds work before anything else), strictly increasing,
/// then every entry known. Unknown features are refused, never ignored.
pub fn check_required_features(features: &[u64], known: &[u64], limits: &Limits) -> Result<()> {
    let n = features.len() as u64;
    if n > limits.max_required_features {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::RequiredFeatures,
            limit: limits.max_required_features,
            actual: n,
        });
    }
    if features.windows(2).any(|w| w[0] >= w[1]) {
        return Err(FormatError::Envelope(EnvelopeFault::FeaturesNotIncreasing));
    }
    if let Some(&feature) = features.iter().find(|f| !known.contains(f)) {
        return Err(FormatError::UnsupportedRequiredFeature { feature });
    }
    Ok(())
}

/// What a caller knows about the record it is about to decode: the frame
/// kind its referencing field names, and what this build supports.
#[derive(Debug, Clone, Copy)]
pub struct EnvelopeRules<'a> {
    pub kind: FrameKind,
    /// Record schema versions this build understands for `kind`.
    pub schema_versions: &'a [u16],
    /// Required features this build understands. Empty in Core today.
    pub known_features: &'a [u64],
}

/// A decoded binary envelope header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryEnvelope {
    pub record_schema_version: u16,
    pub encoding: PayloadEncoding,
    pub identity: RecordIdentity,
    pub required_features: Vec<u64>,
}

impl BinaryEnvelope {
    /// 80 + 8·n, or `None` if that overflows (n is archive-derived).
    fn header_len_for(n: u64) -> Option<u64> {
        n.checked_mul(8)?
            .checked_add(BINARY_ENVELOPE_FIXED_LEN as u64)
    }
}

/// An envelope that passed every intrinsic check but is not yet bound to a
/// referencing commit. Its identity is unchecked; nothing may interpret the
/// payload until [`Candidate::bind`] succeeds.
#[derive(Debug)]
pub struct Candidate<'a> {
    envelope: BinaryEnvelope,
    payload: &'a [u8],
}

impl<'a> Candidate<'a> {
    /// Fields that are safe to inspect before binding (for diagnostics and
    /// salvage reports). They are untrusted.
    pub fn unbound_envelope(&self) -> &BinaryEnvelope {
        &self.envelope
    }

    /// Bind to the referencing commit: the D11 identity obligations.
    /// The integrity-scope obligation (the referencing record's stored-object
    /// hash) must already have been checked by the caller.
    pub fn bind(self, expected: &RecordIdentity) -> Result<(BinaryEnvelope, &'a [u8])> {
        self.envelope.identity.check(expected)?;
        Ok((self.envelope, self.payload))
    }
}

/// Payload budget for a binary-enveloped record (B.2.3 "Image payload"):
/// *S* − 592, the skippable payload limit minus the largest v0 header, so
/// the budget does not depend on how many features a record lists. With
/// default limits this is [`crate::limits::DEFAULT_IMAGE_PAYLOAD`].
pub fn image_payload_budget(limits: &Limits) -> u64 {
    limits
        .max_skippable_payload
        .saturating_sub(MAX_BINARY_ENVELOPE_HEADER)
}

/// Encode one complete skippable frame: binary envelope v0 + `payload`.
///
/// The writer refuses anything a reader with `writer_limits` would refuse:
/// the same obligations as [`decode_binary_record`], plus the payload budget
/// ([`image_payload_budget`]) and the frame limits, as
/// [`FormatError::CapacityExceeded`].
pub fn encode_binary_record(
    kind: FrameKind,
    envelope: &BinaryEnvelope,
    payload: &[u8],
    known_features: &[u64],
    writer_limits: &Limits,
) -> Result<Vec<u8>> {
    if !carries_binary_envelope(kind) {
        return Err(FormatError::Envelope(EnvelopeFault::KindHasNoEnvelope));
    }
    let n = envelope.required_features.len() as u64;
    if n > BINARY_ENVELOPE_MAX_FEATURES {
        return Err(FormatError::Envelope(EnvelopeFault::FeatureCountOutOfRange));
    }
    check_required_features(&envelope.required_features, known_features, writer_limits)?;
    check_payload_encoding(envelope, payload)?;
    let budget = image_payload_budget(writer_limits);
    let payload_len = payload.len() as u64;
    if payload_len > budget {
        return Err(FormatError::CapacityExceeded {
            kind: CapacityKind::EnvelopePayload,
            limit: budget,
            actual: payload_len,
        });
    }
    // n ≤ 64, so neither the header length nor the u32 casts can overflow.
    let header_len = BinaryEnvelope::header_len_for(n)
        .ok_or(FormatError::Envelope(EnvelopeFault::FeatureCountOutOfRange))?;

    let mut body = Vec::with_capacity(header_len as usize + payload.len());
    body.extend_from_slice(&(header_len as u32).to_le_bytes());
    body.extend_from_slice(&BINARY_ENVELOPE_VERSION.to_le_bytes());
    body.extend_from_slice(&envelope.record_schema_version.to_le_bytes());
    body.extend_from_slice(&envelope.encoding.code().to_le_bytes());
    body.extend_from_slice(&(n as u32).to_le_bytes());
    body.extend_from_slice(&payload_len.to_le_bytes());
    body.extend_from_slice(&envelope.identity.archive_id);
    body.extend_from_slice(&envelope.identity.commit_sequence.to_le_bytes());
    body.extend_from_slice(&envelope.identity.transaction_id);
    for f in &envelope.required_features {
        body.extend_from_slice(&f.to_le_bytes());
    }
    debug_assert_eq!(body.len() as u64, header_len);
    body.extend_from_slice(payload);
    encode_skippable_frame_within(kind, &body, writer_limits)
}

/// Payload-encoding and (for SQLite) schema-version obligations, shared by
/// encoder and decoder.
fn check_payload_encoding(envelope: &BinaryEnvelope, payload: &[u8]) -> Result<()> {
    if !payload.starts_with(envelope.encoding.signature()) {
        return Err(FormatError::Envelope(
            EnvelopeFault::EncodingSignatureMismatch,
        ));
    }
    match envelope.encoding {
        PayloadEncoding::SqliteImage => {
            // A SQLite header is 100 bytes; user_version is at 60..64.
            let uv = payload
                .get(SQLITE_USER_VERSION_AT..SQLITE_USER_VERSION_AT + 4)
                .ok_or(FormatError::Envelope(EnvelopeFault::TooShort))?;
            let user_version = i32::from_be_bytes([uv[0], uv[1], uv[2], uv[3]]);
            if i64::from(user_version) != i64::from(envelope.record_schema_version) {
                return Err(FormatError::Envelope(EnvelopeFault::SchemaVersionMismatch));
            }
        }
    }
    Ok(())
}

/// Decode the payload of a skippable frame whose magic is `frame_magic`,
/// checking every intrinsic D11 obligation. The result is a [`Candidate`]:
/// its identity is unchecked until [`Candidate::bind`].
///
/// Order: kind, fixed-header presence, envelope version, feature count (a
/// wire rule, before the header length is computed from it), header length,
/// payload length, payload budget, encoding, features, schema version,
/// signature. Each
/// failure has its own fault; unknown versions and features are
/// `Unsupported`.
pub fn decode_binary_record<'a>(
    frame_magic: u32,
    frame_payload: &'a [u8],
    rules: &EnvelopeRules<'_>,
    limits: &Limits,
) -> Result<Candidate<'a>> {
    let kind = FrameKind::from_magic(frame_magic)
        .filter(|k| carries_binary_envelope(*k))
        .ok_or(FormatError::Envelope(EnvelopeFault::KindHasNoEnvelope))?;
    if kind != rules.kind {
        return Err(FormatError::Envelope(EnvelopeFault::RecordTypeMismatch));
    }
    let fixed = frame_payload
        .get(..BINARY_ENVELOPE_FIXED_LEN)
        .ok_or(FormatError::Envelope(EnvelopeFault::TooShort))?;
    let u16_at = |i: usize| u16::from_le_bytes([fixed[i], fixed[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([fixed[i], fixed[i + 1], fixed[i + 2], fixed[i + 3]]);
    let u64_at = |i: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&fixed[i..i + 8]);
        u64::from_le_bytes(b)
    };

    let version = u16_at(4);
    if version != BINARY_ENVELOPE_VERSION {
        return Err(FormatError::UnsupportedEnvelopeVersion { version });
    }
    let n = u64::from(u32_at(12));
    if n > BINARY_ENVELOPE_MAX_FEATURES {
        return Err(FormatError::Envelope(EnvelopeFault::FeatureCountOutOfRange));
    }
    let header_len = BinaryEnvelope::header_len_for(n)
        .ok_or(FormatError::Envelope(EnvelopeFault::FeatureCountOutOfRange))?;
    if u64::from(u32_at(0)) != header_len {
        return Err(FormatError::Envelope(EnvelopeFault::HeaderLengthMismatch));
    }
    let header_len = usize::try_from(header_len)
        .map_err(|_| FormatError::Envelope(EnvelopeFault::HeaderLengthMismatch))?;
    let (head, payload) = frame_payload
        .split_at_checked(header_len)
        .ok_or(FormatError::Envelope(EnvelopeFault::TooShort))?;
    if u64_at(16) != payload.len() as u64 {
        return Err(FormatError::Envelope(EnvelopeFault::PayloadLengthMismatch));
    }
    let budget = image_payload_budget(limits);
    if payload.len() as u64 > budget {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::ImagePayload,
            limit: budget,
            actual: payload.len() as u64,
        });
    }
    let encoding = PayloadEncoding::from_code(u32_at(8))
        .ok_or(FormatError::Envelope(EnvelopeFault::UnknownPayloadEncoding))?;

    let required_features: Vec<u64> = head[BINARY_ENVELOPE_FIXED_LEN..]
        .chunks_exact(8)
        .map(|c| {
            let mut b = [0u8; 8];
            b.copy_from_slice(c);
            u64::from_le_bytes(b)
        })
        .collect();
    check_required_features(&required_features, rules.known_features, limits)?;

    let record_schema_version = u16_at(6);
    if !rules.schema_versions.contains(&record_schema_version) {
        return Err(FormatError::UnsupportedSchema {
            version: record_schema_version,
        });
    }

    let mut archive_id = [0u8; 32];
    archive_id.copy_from_slice(&fixed[24..56]);
    let mut transaction_id = [0u8; 16];
    transaction_id.copy_from_slice(&fixed[64..80]);
    let envelope = BinaryEnvelope {
        record_schema_version,
        encoding,
        identity: RecordIdentity {
            archive_id,
            commit_sequence: u64_at(56),
            transaction_id,
        },
        required_features,
    };
    check_payload_encoding(&envelope, payload)?;
    Ok(Candidate { envelope, payload })
}

/// Encode a deterministic-CBOR record (commit, manifest, descriptor) as one
/// complete skippable frame, refusing anything a default reader would reject:
/// CBOR items and depth ([`crate::cbor::encode_within`]), then the frame
/// limits ([`encode_skippable_frame_within`]). The CBOR-native envelope keys
/// themselves are the schema's business, checked by its encoder in
/// `mochi-core` with [`check_required_features`] and [`RecordIdentity`].
pub fn encode_cbor_record(
    kind: FrameKind,
    value: &crate::cbor::Value,
    cbor_limits: &crate::cbor::CborLimits,
    writer_limits: &Limits,
) -> Result<Vec<u8>> {
    if carries_binary_envelope(kind) || !kind.is_skippable() || kind == FrameKind::CommitFooter {
        return Err(FormatError::CannotWrite(
            "frame kind does not carry a CBOR-native envelope",
        ));
    }
    let payload = crate::cbor::encode_within(value, cbor_limits)?;
    encode_skippable_frame_within(kind, &payload, writer_limits)
}
