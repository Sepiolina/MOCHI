//! Errors for the framing layer.
//!
//! `mochi-format` cannot depend on `mochi-core`, so it does not use the stable
//! `ErrorCode` registry directly. Every [`FormatError`] reports an
//! [`ErrorClass`], and `mochi-core` maps each class to its registered code.

use std::fmt;

/// Coarse classification that `mochi-core` maps onto registered error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Input ended before a structure was complete.
    Truncated,
    /// Bytes are structurally invalid.
    Malformed,
    /// A configured limit (spec §8.5) was exceeded.
    LimitExceeded,
    /// Archive-derived integer arithmetic overflowed or a range was out of bounds.
    OutOfBounds,
    /// The footer failed validation (spec §8.4).
    Footer,
    /// The record envelope failed validation (spec §8.3).
    Envelope,
    /// A required feature or schema is not supported by this build.
    Unsupported,
    /// The byte source failed.
    Source,
    /// The caller asked for something the writer must refuse.
    InvalidArgument,
    /// Decoding produced something other than the declared content: wrong
    /// length, a decoder failure, or a size field that disagrees with the
    /// record (spec §9.3, §20.1 "content integrity"). Added in C2.
    ContentIntegrity,
    /// A canonical record (manifest, commit body) is not canonical CBOR in
    /// the MOCHI subset, or violates its schema. The frame around it may be
    /// fine. Added in C4.
    Record,
    /// A **writer** refused to emit something a default reader would reject
    /// (spec Annex B.2.3 writer default rule; B.2.2 `CAPACITY_EXCEEDED`).
    /// Never raised while reading: readers report [`ErrorClass::LimitExceeded`].
    CapacityExceeded,
    /// An operation needs the archive's data key and none was supplied, or
    /// none of the supplied passphrases opens a key envelope (spec Annex
    /// B.2.10 item 9). Not evidence of damage.
    KeyUnavailable,
}

/// Which configurable limit was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    SkippablePayload,
    FrameLength,
    BlocksPerFrame,
    WindowSize,
    CommitFrameLength,
    /// Decoded length of one object (C2): bounds decompression output.
    DecodedObjectLength,
    /// Required features listed by one record (spec Annex B.2.3).
    RequiredFeatures,
    /// Payload of a binary-enveloped record, e.g. a catalog image (B.2.3:
    /// *S* − 592).
    ImagePayload,
    /// Argon2id memory a key envelope declares, in KiB (D20 item 1).
    KdfMemory,
    /// Argon2id passes (iterations) a key envelope declares.
    KdfIterations,
    /// Argon2id lanes a key envelope declares.
    KdfLanes,
    /// Key envelopes one commit lists (D20 item 3).
    KeyEnvelopes,
}

/// Which bound a writer would have exceeded (spec Annex B.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapacityKind {
    /// A reader limit on frames ([`LimitKind`]).
    Frame(LimitKind),
    /// Total CBOR data items, counted as the decoder counts them.
    CborItems,
    /// CBOR nesting depth, as the decoder measures it.
    CborDepth,
    /// A binary-envelope payload larger than its default budget.
    EnvelopePayload,
}

/// Why a footer was rejected (spec §8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FooterFault {
    /// The preceding 8-byte skippable header is not the footer header.
    HeaderMismatch,
    /// The payload does not start with the footer magic.
    BadPayloadMagic,
    /// The commit-frame range overflows, is empty, or does not lie before the footer.
    CommitRangeInvalid,
    /// The bytes at the commit offset are not a skippable frame of the stated length.
    CommitFrameMalformed,
    /// The recomputed digest differs from the stored one.
    DigestMismatch,
}

/// Why a record envelope was rejected (spec §8.3; Annex B.2 D11).
///
/// One variant per validation obligation, so a reject test can name the
/// obligation it exercises. Unknown schema/envelope versions and unknown
/// required features are not faults here: they are
/// [`ErrorClass::Unsupported`] (exit 4), as D11 says "refuse".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeFault {
    /// Fewer bytes than the fixed header, or than the header length declares.
    TooShort,
    /// Record type: the frame kind is not the kind the referencing field
    /// expects.
    RecordTypeMismatch,
    /// The containing frame kind never carries a binary envelope.
    KindHasNoEnvelope,
    /// The header-length field is not 80 + 8·n.
    HeaderLengthMismatch,
    /// More required features than the binary envelope v0 allows (n ≤ 64).
    FeatureCountOutOfRange,
    /// Required features: the list is not strictly increasing.
    FeaturesNotIncreasing,
    /// Payload encoding: the value is not registered.
    UnknownPayloadEncoding,
    /// Payload encoding: the payload lacks that encoding's signature.
    EncodingSignatureMismatch,
    /// Schema version: for a catalog image, the header's record schema
    /// version differs from the image's SQLite `user_version`.
    SchemaVersionMismatch,
    /// Payload length: the header's value is not the frame payload minus the
    /// header.
    PayloadLengthMismatch,
    /// Archive identity differs from the referencing commit's archive ID.
    ArchiveIdMismatch,
    /// Identity: commit sequence differs from the referencing commit's.
    SequenceMismatch,
    /// Identity: transaction ID differs from the referencing commit's.
    TransactionIdMismatch,
}

/// Why an object failed to encode or decode (spec §9.1, §9.3). Added in C2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecFault {
    /// Stored bytes are a frame, but not a Zstandard data frame.
    NotADataFrame,
    /// Stored bytes hold more than one frame, or trailing bytes after one.
    NotSingleFrame,
    /// A data object lacks `Frame_Content_Size`, which MOCHI requires (O21).
    MissingContentSize,
    /// A data object lacks the frame checksum, which MOCHI requires (O21).
    MissingChecksum,
    /// The frame's `Dictionary_ID` disagrees with the record's dictionary
    /// dependency (O21: the record is authoritative; the frame field must
    /// equal the dictionary's own embedded ID, or 0 when there is none).
    DictionaryMismatch { frame_dictionary_id: u32 },
    /// `Frame_Content_Size` disagrees with the record's decoded length.
    DeclaredSizeMismatch { declared: u64, expected: u64 },
    /// The decoder rejected the frame (bad block, checksum, window, ...).
    DecodeFailed,
    /// The decoder produced a different number of bytes than the record says.
    /// `actual_at_least` is exact when shorter, a lower bound when longer.
    DecodedLengthMismatch { expected: u64, actual_at_least: u64 },
    /// The encoder failed; not archive-derived.
    EncoderFailed,
}

/// Why sealing, opening, or wrapping failed (spec Annex B.2.10, D20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealFault {
    /// A sealed-object payload shorter than header plus tag.
    TooShort,
    /// The authentication tag did not verify: the bytes are not what was
    /// sealed under this key, archive, and binding. Whether that means a wrong
    /// key or a modified object is decided by the caller, which knows whether
    /// the stored-object hash already verified.
    Authentication,
    /// The sealed header names a key other than the one in use.
    KeyIdMismatch,
    /// A sealed-object version this build does not know.
    UnknownVersion { version: u16 },
    /// A suite this build does not know.
    UnknownSuite { suite: u16 },
    /// An object kind this build does not know.
    UnknownKind { kind: u32 },
    /// A KDF or Argon2 version this build does not know.
    UnknownKdf,
    /// KDF parameters below Argon2's own minimums (m < 8·p, t < 1, p < 1).
    BadKdfParameters,
    /// A sealed object of a kind other than the one the reader expected.
    KindMismatch,
    /// The operation needs the data key and the caller supplied none.
    NoKey,
    /// The operating-system random source failed.
    RandomFailed,
    /// The AEAD or KDF primitive failed for a reason not archive-derived.
    PrimitiveFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The input ended at `offset`, where more bytes were required.
    Truncated {
        offset: u64,
    },
    /// The magic at `offset` is neither a Zstandard data frame nor a skippable frame.
    NotAFrame {
        offset: u64,
        magic: u32,
    },
    /// Frame-header descriptor has the reserved bit set (RFC 8878).
    ReservedHeaderBit {
        offset: u64,
    },
    /// Block type 3 (reserved, invalid; spec §8.6).
    ReservedBlockType {
        offset: u64,
    },
    /// `Block_Size` exceeds `Block_Maximum_Size` for this frame.
    BlockTooLarge {
        offset: u64,
        size: u32,
        max: u64,
    },
    /// A configured limit was exceeded.
    LimitExceeded {
        kind: LimitKind,
        limit: u64,
        actual: u64,
    },
    /// Archive-derived arithmetic overflowed.
    Overflow {
        offset: u64,
    },
    Footer(FooterFault),
    Envelope(EnvelopeFault),
    /// A frame of a kind restricted to one offset found elsewhere: an archive
    /// descriptor not at offset 0 (spec Annex B.2 D12).
    MisplacedFrame {
        offset: u64,
        magic: u32,
    },
    /// A record requires a feature this build does not know (D11: refuse).
    UnsupportedRequiredFeature {
        feature: u64,
    },
    /// An envelope used a schema version this build does not know.
    UnsupportedSchema {
        version: u16,
    },
    /// A binary envelope of a version this build does not know (D11: refuse).
    UnsupportedEnvelopeVersion {
        version: u16,
    },
    /// A writer refused output that a default reader would reject (B.2.3).
    CapacityExceeded {
        kind: CapacityKind,
        limit: u64,
        actual: u64,
    },
    /// The byte source failed (message is never archive content).
    Source(String),
    /// A writer was asked to emit a frame it must refuse.
    CannotWrite(&'static str),
    /// A record payload is larger than a skippable frame can carry (spec §8.3).
    PayloadTooLarge {
        len: u64,
    },
    /// Encoding or decoding an object failed (C2).
    Codec(CodecFault),
    /// A capability this build does not have yet; never silently degraded.
    Unsupported(&'static str),
    /// Input is not canonical CBOR in the MOCHI subset (C4, spec D2).
    Cbor(crate::cbor::CborFault),
    /// Canonical CBOR, but not a valid instance of the expected schema:
    /// a wrong type, a missing required key, or an unknown key (C4).
    Schema(String),
    /// A caller asked for something the encoder must refuse (not
    /// archive-derived), e.g. an unsorted map.
    InvalidArgument(&'static str),
    /// Sealing, opening, or key wrapping failed (D20).
    Seal(SealFault),
}

impl FormatError {
    pub fn class(&self) -> ErrorClass {
        match self {
            FormatError::Truncated { .. } => ErrorClass::Truncated,
            FormatError::NotAFrame { .. }
            | FormatError::ReservedHeaderBit { .. }
            | FormatError::ReservedBlockType { .. }
            | FormatError::BlockTooLarge { .. }
            | FormatError::MisplacedFrame { .. } => ErrorClass::Malformed,
            FormatError::LimitExceeded { .. } => ErrorClass::LimitExceeded,
            FormatError::Overflow { .. } => ErrorClass::OutOfBounds,
            FormatError::Footer(_) => ErrorClass::Footer,
            FormatError::Envelope(_) => ErrorClass::Envelope,
            FormatError::UnsupportedRequiredFeature { .. }
            | FormatError::UnsupportedSchema { .. }
            | FormatError::UnsupportedEnvelopeVersion { .. }
            | FormatError::Unsupported(_) => ErrorClass::Unsupported,
            FormatError::CapacityExceeded { .. } => ErrorClass::CapacityExceeded,
            FormatError::Cbor(f) => match f {
                crate::cbor::CborFault::Truncated | crate::cbor::CborFault::LengthExceedsInput => {
                    ErrorClass::Truncated
                }
                crate::cbor::CborFault::TooDeep | crate::cbor::CborFault::TooManyItems => {
                    ErrorClass::LimitExceeded
                }
                _ => ErrorClass::Record,
            },
            FormatError::Schema(_) => ErrorClass::Record,
            FormatError::InvalidArgument(_) => ErrorClass::InvalidArgument,
            FormatError::Codec(fault) => match fault {
                CodecFault::NotADataFrame
                | CodecFault::NotSingleFrame
                | CodecFault::MissingContentSize
                | CodecFault::MissingChecksum => ErrorClass::Malformed,
                CodecFault::DictionaryMismatch { .. }
                | CodecFault::DeclaredSizeMismatch { .. }
                | CodecFault::DecodeFailed
                | CodecFault::DecodedLengthMismatch { .. } => ErrorClass::ContentIntegrity,
                CodecFault::EncoderFailed => ErrorClass::InvalidArgument,
            },
            FormatError::Source(_) => ErrorClass::Source,
            FormatError::CannotWrite(_) | FormatError::PayloadTooLarge { .. } => {
                ErrorClass::InvalidArgument
            }
            FormatError::Seal(fault) => match fault {
                SealFault::TooShort => ErrorClass::Malformed,
                SealFault::Authentication => ErrorClass::ContentIntegrity,
                SealFault::KeyIdMismatch
                | SealFault::KindMismatch
                | SealFault::BadKdfParameters => ErrorClass::Record,
                SealFault::NoKey => ErrorClass::KeyUnavailable,
                SealFault::UnknownVersion { .. }
                | SealFault::UnknownSuite { .. }
                | SealFault::UnknownKind { .. }
                | SealFault::UnknownKdf => ErrorClass::Unsupported,
                SealFault::RandomFailed => ErrorClass::Source,
                SealFault::PrimitiveFailed => ErrorClass::InvalidArgument,
            },
        }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Truncated { offset } => write!(f, "input truncated at offset {offset}"),
            FormatError::NotAFrame { offset, magic } => {
                write!(f, "no frame magic at offset {offset} (found {magic:#010x})")
            }
            FormatError::ReservedHeaderBit { offset } => {
                write!(f, "reserved frame-header bit set in frame at {offset}")
            }
            FormatError::ReservedBlockType { offset } => {
                write!(f, "reserved block type at offset {offset}")
            }
            FormatError::BlockTooLarge { offset, size, max } => {
                write!(
                    f,
                    "block at {offset} declares {size} bytes, maximum is {max}"
                )
            }
            FormatError::LimitExceeded {
                kind,
                limit,
                actual,
            } => write!(f, "limit {kind:?} exceeded: {actual} > {limit}"),
            FormatError::Overflow { offset } => {
                write!(f, "integer overflow while walking frame at {offset}")
            }
            FormatError::Footer(fault) => write!(f, "footer rejected: {fault:?}"),
            FormatError::Envelope(fault) => write!(f, "envelope rejected: {fault:?}"),
            FormatError::MisplacedFrame { offset, magic } => write!(
                f,
                "frame {magic:#010x} at offset {offset} is only valid at its required offset"
            ),
            FormatError::UnsupportedRequiredFeature { feature } => {
                write!(
                    f,
                    "record requires feature {feature}, which this build does not support"
                )
            }
            FormatError::UnsupportedSchema { version } => {
                write!(f, "unsupported record schema version {version}")
            }
            FormatError::UnsupportedEnvelopeVersion { version } => {
                write!(f, "unsupported binary envelope version {version}")
            }
            FormatError::CapacityExceeded {
                kind,
                limit,
                actual,
            } => write!(
                f,
                "refusing to write {kind:?} of {actual}: a default reader accepts at most {limit}"
            ),
            FormatError::Source(msg) => write!(f, "byte source failed: {msg}"),
            FormatError::CannotWrite(why) => write!(f, "refusing to write frame: {why}"),
            FormatError::PayloadTooLarge { len } => {
                write!(f, "payload of {len} bytes exceeds the 32-bit frame size")
            }
            FormatError::Codec(fault) => write!(f, "object codec: {fault:?}"),
            FormatError::Unsupported(what) => write!(f, "not supported by this build: {what}"),
            FormatError::Cbor(fault) => write!(f, "not canonical MOCHI CBOR: {fault:?}"),
            FormatError::Schema(msg) => write!(f, "schema violation: {msg}"),
            FormatError::InvalidArgument(msg) => write!(f, "invalid argument: {msg}"),
            FormatError::Seal(fault) => write!(f, "sealed object or key envelope: {fault:?}"),
        }
    }
}

impl std::error::Error for FormatError {}

pub type Result<T> = std::result::Result<T, FormatError>;
