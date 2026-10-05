//! Stable error codes (plan §2.2, spec §20.5, §23.4).
//!
//! **This is the only place error codes are defined.** Reports, CLI output, and
//! UI messages all reuse [`ErrorCode`]. Adding a code is fine; renaming one or
//! changing its meaning is a breaking change (AGENTS.md). Final names are
//! ratification item R7; until then every code here is *draft*.

use std::fmt;

use serde::{Deserialize, Serialize};

use mochi_format::{ErrorClass, FormatError};

use crate::storage::StorageError;

/// A stable, registered error code. The wire form is `SCREAMING_SNAKE_CASE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// The underlying storage reported an I/O failure.
    IoError,
    /// A read or seek fell outside the object's bounds (checked before use).
    OutOfBounds,
    /// A configured resource limit (spec §8.5) was exceeded.
    LimitExceeded,
    /// The publication lock is held by another writer (spec §12.5).
    LockConflict,
    /// The caller supplied an invalid argument or configuration.
    InvalidArgument,
    /// The operation was cancelled through its cancellation token.
    Cancelled,
    /// A required feature or verification capability is not supported by this
    /// build; results are never partial or empty (spec §7.7, §26).
    UnsupportedFeature,
    /// A 1.0-scope operation that has not been implemented yet in this
    /// development build. Not a conforming end state.
    NotImplemented,
    /// A report contradicts the spec's reporting invariants (spec §5.4, §20.3).
    ReportInconsistent,
    /// Bytes are not a structurally valid frame (spec §8.5, §8.6). Added in C1.
    MalformedFrame,
    /// Input ended inside a structure; the tail may be an interrupted write
    /// (spec §12.2). Added in C1.
    Truncated,
    /// A footer failed validation (spec §8.4). Added in C1.
    FooterInvalid,
    /// A record envelope failed validation (spec §8.3). Added in C1.
    EnvelopeInvalid,
    /// Stored bytes do not match the recorded stored length or stored-object
    /// hash (spec §9.2, §20.1 "stored integrity"). Added in C2.
    StoredIntegrityFailed,
    /// An object's stored bytes verified, but decoding did not yield exactly
    /// the recorded length and chunk content hash (spec §9.3, §20.1 "content
    /// integrity"). Added in C2.
    ContentIntegrityFailed,
    /// An archive path violates the §10.4 representation rules. Added in C3.
    PathInvalid,
    /// File extents have a gap, overlap, out-of-range read, length mismatch,
    /// or other defect (spec §10.3). Added in C3.
    ExtentInvalid,
    /// Namespace operations leave an invalid snapshot, or delete a path that
    /// does not exist (spec §10.2). Added in C3.
    NamespaceInvalid,
    /// A catalog image or working catalog fails structural, schema, or
    /// relationship checks (spec §10.5). Added in C3.
    CatalogInvalid,
    /// An immutable ID already exists with different record content: this is
    /// corruption, not an update (spec §10.2). Added in C3.
    IdentityConflict,
    /// A canonical record (recovery manifest, commit body) is not canonical
    /// CBOR in the MOCHI subset, or violates its closed schema (spec D2).
    /// Added in C4.
    RecordInvalid,
    /// No valid committed footer was found anywhere in the archive (spec
    /// §8.4, §12.2): it was never committed, or every footer is damaged.
    /// Added in C5.
    NoValidHead,
    /// Bytes follow the last valid commit and are *eligible* for truncation
    /// (Annex B.2 D14: they look like an interrupted write; a conservative
    /// screen, not proof). Appending is refused until they are
    /// removed by an explicit, audited truncation. Added in C5.
    UncommittedTail,
    /// Bytes follow the last valid commit and are not eligible for
    /// truncation (Annex B.2 D14): they may hold a damaged later commit (spec §12.2, §22). Truncation is
    /// refused; this needs the repair workflow. Added in C5.
    TailUnresolved,
    /// A failure after the footer was appended (spec §12.2 steps 7–9): the
    /// commit may or may not be the durable head. Never reported as
    /// committed; reopen the archive to find out. Added in C5.
    CommitUnconfirmed,
    /// A writer refuses further work after a durability failure, because
    /// the state of unsynced bytes is no longer known. Reopen. Added in C5.
    WriterPoisoned,
    /// Creation found an existing file at the final name; nothing was
    /// replaced (spec Annex B.2 D13). Exit 3. Added in B.2.
    DestinationExists,
    /// A writer refused output that a default reader would reject (spec
    /// Annex B.2.3 writer default rule, D10 item 11). No head is published;
    /// the previous head stays the head. Exit 3. Added in B.2.
    CapacityExceeded,
    /// A checkpoint's image or snapshot manifest, re-read from its hashed
    /// bytes, disagrees with the source snapshot (D10 item 7). From a writer:
    /// no head, exit 3. As a `verify` finding it is a `FAIL` (exit 1) via the
    /// report, not via this code's exit mapping. Added in B.2.
    CheckpointMismatch,
    /// The archive descriptor is missing, damaged, or mismatched (D12).
    /// Interpretation and append are refused. Exit 1. Added in B.2.
    DescriptorInvalid,
    /// GC cannot rebuild retention state at the head because a manifest in
    /// the head's segment is missing or unverified (D10 item 10). Exit 1.
    /// Added in B.2.
    RetentionUnresolved,
    /// A tail-quarantine step failed, so truncation was blocked (D14).
    /// Exit 3. Added in B.2.
    QuarantineFailed,
    /// Directory durability is not `Confirmed`, so truncation is blocked
    /// unless explicitly waived (D14; Windows until gate G6). Exit 3.
    /// Added in B.2.
    DurabilityUnconfirmed,
    /// An in-place change to a creation-time constraint was requested,
    /// e.g. enabling encryption (D12). Write a new archive. Exit 4.
    /// Added in B.2.
    ProfileChangeUnsupported,
}

impl ErrorCode {
    /// Every registered code, in declaration order.
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::IoError,
        ErrorCode::OutOfBounds,
        ErrorCode::LimitExceeded,
        ErrorCode::LockConflict,
        ErrorCode::InvalidArgument,
        ErrorCode::Cancelled,
        ErrorCode::UnsupportedFeature,
        ErrorCode::NotImplemented,
        ErrorCode::ReportInconsistent,
        ErrorCode::MalformedFrame,
        ErrorCode::Truncated,
        ErrorCode::FooterInvalid,
        ErrorCode::EnvelopeInvalid,
        ErrorCode::StoredIntegrityFailed,
        ErrorCode::ContentIntegrityFailed,
        ErrorCode::PathInvalid,
        ErrorCode::ExtentInvalid,
        ErrorCode::NamespaceInvalid,
        ErrorCode::CatalogInvalid,
        ErrorCode::IdentityConflict,
        ErrorCode::RecordInvalid,
        ErrorCode::NoValidHead,
        ErrorCode::UncommittedTail,
        ErrorCode::TailUnresolved,
        ErrorCode::CommitUnconfirmed,
        ErrorCode::WriterPoisoned,
        ErrorCode::DestinationExists,
        ErrorCode::CapacityExceeded,
        ErrorCode::CheckpointMismatch,
        ErrorCode::DescriptorInvalid,
        ErrorCode::RetentionUnresolved,
        ErrorCode::QuarantineFailed,
        ErrorCode::DurabilityUnconfirmed,
        ErrorCode::ProfileChangeUnsupported,
    ];

    /// The stable string form. Independent of serde so it cannot drift silently;
    /// a test checks the two agree.
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::IoError => "IO_ERROR",
            ErrorCode::OutOfBounds => "OUT_OF_BOUNDS",
            ErrorCode::LimitExceeded => "LIMIT_EXCEEDED",
            ErrorCode::LockConflict => "LOCK_CONFLICT",
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::Cancelled => "CANCELLED",
            ErrorCode::UnsupportedFeature => "UNSUPPORTED_FEATURE",
            ErrorCode::NotImplemented => "NOT_IMPLEMENTED",
            ErrorCode::ReportInconsistent => "REPORT_INCONSISTENT",
            ErrorCode::MalformedFrame => "MALFORMED_FRAME",
            ErrorCode::Truncated => "TRUNCATED",
            ErrorCode::FooterInvalid => "FOOTER_INVALID",
            ErrorCode::EnvelopeInvalid => "ENVELOPE_INVALID",
            ErrorCode::StoredIntegrityFailed => "STORED_INTEGRITY_FAILED",
            ErrorCode::ContentIntegrityFailed => "CONTENT_INTEGRITY_FAILED",
            ErrorCode::PathInvalid => "PATH_INVALID",
            ErrorCode::ExtentInvalid => "EXTENT_INVALID",
            ErrorCode::NamespaceInvalid => "NAMESPACE_INVALID",
            ErrorCode::CatalogInvalid => "CATALOG_INVALID",
            ErrorCode::IdentityConflict => "IDENTITY_CONFLICT",
            ErrorCode::RecordInvalid => "RECORD_INVALID",
            ErrorCode::NoValidHead => "NO_VALID_HEAD",
            ErrorCode::UncommittedTail => "UNCOMMITTED_TAIL",
            ErrorCode::TailUnresolved => "TAIL_UNRESOLVED",
            ErrorCode::CommitUnconfirmed => "COMMIT_UNCONFIRMED",
            ErrorCode::WriterPoisoned => "WRITER_POISONED",
            ErrorCode::DestinationExists => "DESTINATION_EXISTS",
            ErrorCode::CapacityExceeded => "CAPACITY_EXCEEDED",
            ErrorCode::CheckpointMismatch => "CHECKPOINT_MISMATCH",
            ErrorCode::DescriptorInvalid => "DESCRIPTOR_INVALID",
            ErrorCode::RetentionUnresolved => "RETENTION_UNRESOLVED",
            ErrorCode::QuarantineFailed => "QUARANTINE_FAILED",
            ErrorCode::DurabilityUnconfirmed => "DURABILITY_UNCONFIRMED",
            ErrorCode::ProfileChangeUnsupported => "PROFILE_CHANGE_UNSUPPORTED",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error type for `mochi-core`: a stable code plus a human message.
///
/// Messages must never contain secrets (AGENTS.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MochiError {
    pub code: ErrorCode,
    pub message: String,
}

impl MochiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for MochiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for MochiError {}

impl From<StorageError> for MochiError {
    fn from(err: StorageError) -> Self {
        let code = match &err {
            StorageError::Io(_) => ErrorCode::IoError,
            StorageError::OutOfBounds { .. } => ErrorCode::OutOfBounds,
            StorageError::LockHeld => ErrorCode::LockConflict,
            StorageError::Unsupported(_) => ErrorCode::UnsupportedFeature,
        };
        MochiError::new(code, err.to_string())
    }
}

impl From<FormatError> for MochiError {
    fn from(err: FormatError) -> Self {
        let code = match err.class() {
            ErrorClass::Truncated => ErrorCode::Truncated,
            ErrorClass::Malformed => ErrorCode::MalformedFrame,
            ErrorClass::LimitExceeded => ErrorCode::LimitExceeded,
            ErrorClass::OutOfBounds => ErrorCode::OutOfBounds,
            ErrorClass::Footer => ErrorCode::FooterInvalid,
            ErrorClass::Envelope => ErrorCode::EnvelopeInvalid,
            ErrorClass::Unsupported => ErrorCode::UnsupportedFeature,
            ErrorClass::Source => ErrorCode::IoError,
            ErrorClass::InvalidArgument => ErrorCode::InvalidArgument,
            ErrorClass::ContentIntegrity => ErrorCode::ContentIntegrityFailed,
            ErrorClass::Record => ErrorCode::RecordInvalid,
            ErrorClass::CapacityExceeded => ErrorCode::CapacityExceeded,
        };
        MochiError::new(code, err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, MochiError>;

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Registry snapshot. Changing or removing a line here is a breaking change
    /// to the code registry; adding a code means appending a line deliberately.
    const REGISTRY_V0: &[&str] = &[
        "IO_ERROR",
        "OUT_OF_BOUNDS",
        "LIMIT_EXCEEDED",
        "LOCK_CONFLICT",
        "INVALID_ARGUMENT",
        "CANCELLED",
        "UNSUPPORTED_FEATURE",
        "NOT_IMPLEMENTED",
        "REPORT_INCONSISTENT",
        // Appended in C1.
        "MALFORMED_FRAME",
        "TRUNCATED",
        "FOOTER_INVALID",
        "ENVELOPE_INVALID",
        // Appended in C2.
        "STORED_INTEGRITY_FAILED",
        "CONTENT_INTEGRITY_FAILED",
        // Appended in C3.
        "PATH_INVALID",
        "EXTENT_INVALID",
        "NAMESPACE_INVALID",
        "CATALOG_INVALID",
        "IDENTITY_CONFLICT",
        // Appended in C4.
        "RECORD_INVALID",
        // Appended in C5.
        "NO_VALID_HEAD",
        "UNCOMMITTED_TAIL",
        "TAIL_UNRESOLVED",
        "COMMIT_UNCONFIRMED",
        "WRITER_POISONED",
        // Appended in the Annex B.2 batch (plan T6).
        "DESTINATION_EXISTS",
        "CAPACITY_EXCEEDED",
        "CHECKPOINT_MISMATCH",
        "DESCRIPTOR_INVALID",
        "RETENTION_UNRESOLVED",
        "QUARANTINE_FAILED",
        "DURABILITY_UNCONFIRMED",
        "PROFILE_CHANGE_UNSUPPORTED",
    ];

    #[test]
    fn registry_matches_snapshot() {
        let actual: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(actual, REGISTRY_V0);
    }

    #[test]
    fn codes_are_unique_and_screaming_snake() {
        let mut seen = HashSet::new();
        for code in ErrorCode::ALL {
            let s = code.as_str();
            assert!(seen.insert(s), "duplicate code {s}");
            assert!(
                s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                "{s} is not SCREAMING_SNAKE_CASE"
            );
        }
    }

    #[test]
    fn serde_form_equals_as_str() {
        for code in ErrorCode::ALL {
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, *code);
        }
    }

    #[test]
    fn every_format_error_class_maps_to_a_registered_code() {
        use mochi_format::error::{FooterFault, LimitKind};
        let cases = [
            (FormatError::Truncated { offset: 0 }, ErrorCode::Truncated),
            (
                FormatError::NotAFrame {
                    offset: 0,
                    magic: 0,
                },
                ErrorCode::MalformedFrame,
            ),
            (
                FormatError::LimitExceeded {
                    kind: LimitKind::FrameLength,
                    limit: 1,
                    actual: 2,
                },
                ErrorCode::LimitExceeded,
            ),
            (FormatError::Overflow { offset: 0 }, ErrorCode::OutOfBounds),
            (
                FormatError::Footer(FooterFault::DigestMismatch),
                ErrorCode::FooterInvalid,
            ),
            (
                FormatError::UnsupportedSchema { version: 9 },
                ErrorCode::UnsupportedFeature,
            ),
            (FormatError::Source("x".into()), ErrorCode::IoError),
            (
                FormatError::CapacityExceeded {
                    kind: mochi_format::error::CapacityKind::CborItems,
                    limit: 1,
                    actual: 2,
                },
                ErrorCode::CapacityExceeded,
            ),
            (
                FormatError::MisplacedFrame {
                    offset: 8,
                    magic: mochi_format::registry::ARCHIVE_DESCRIPTOR,
                },
                ErrorCode::MalformedFrame,
            ),
            (
                FormatError::UnsupportedEnvelopeVersion { version: 1 },
                ErrorCode::UnsupportedFeature,
            ),
            (
                FormatError::UnsupportedRequiredFeature { feature: 1 },
                ErrorCode::UnsupportedFeature,
            ),
        ];
        for (err, code) in cases {
            assert_eq!(MochiError::from(err).code, code);
        }
    }

    #[test]
    fn storage_errors_map_to_stable_codes() {
        let e: MochiError = StorageError::LockHeld.into();
        assert_eq!(e.code, ErrorCode::LockConflict);
        let e: MochiError = StorageError::OutOfBounds {
            offset: 1,
            len: 2,
            size: 0,
        }
        .into();
        assert_eq!(e.code, ErrorCode::OutOfBounds);
    }
}
