"""Error codes (R7 section 6) and the verification class of each."""
from __future__ import annotations

# code -> class: 'violation' | 'unsupported' | 'operational'   (R7 table, column 3)
CODE_CLASS = {
    "IO_ERROR": "operational",
    "OUT_OF_BOUNDS": "operational",
    "LIMIT_EXCEEDED": "operational",
    "LOCK_CONFLICT": "operational",
    "INVALID_ARGUMENT": "operational",
    "CANCELLED": "operational",
    "UNSUPPORTED_FEATURE": "unsupported",
    "NOT_IMPLEMENTED": "operational",
    "REPORT_INCONSISTENT": "operational",
    "MALFORMED_FRAME": "violation",
    "TRUNCATED": "violation",
    "FOOTER_INVALID": "violation",
    "ENVELOPE_INVALID": "violation",
    "STORED_INTEGRITY_FAILED": "violation",
    "CONTENT_INTEGRITY_FAILED": "violation",
    "PATH_INVALID": "violation",
    "EXTENT_INVALID": "violation",
    "NAMESPACE_INVALID": "violation",
    "CATALOG_INVALID": "violation",
    "IDENTITY_CONFLICT": "violation",
    "RECORD_INVALID": "violation",
    "NO_VALID_HEAD": "violation",
    "UNCOMMITTED_TAIL": "operational",
    "TAIL_UNRESOLVED": "violation",
    "COMMIT_UNCONFIRMED": "operational",
    "WRITER_POISONED": "operational",
    "DESTINATION_EXISTS": "operational",
    "CAPACITY_EXCEEDED": "operational",
    "CHECKPOINT_MISMATCH": "violation",
    "DESCRIPTOR_INVALID": "violation",
    "RETENTION_UNRESOLVED": "violation",
    "QUARANTINE_FAILED": "operational",
    "DURABILITY_UNCONFIRMED": "operational",
    "PROFILE_CHANGE_UNSUPPORTED": "unsupported",
    "NAME_COLLISION": "operational",
    "NAME_UNSUPPORTED": "operational",
    "ATTRIBUTE_NOT_RESTORED": "operational",
    "REFERENCE_INVALID": "violation",
    "FRESHNESS_FAILED": "violation",
    "PROFILE_VIOLATION": "violation",
    "KEY_UNAVAILABLE": "operational",
    "KDF_COST_BELOW_DEFAULT": "operational",
    "LIMIT_NOT_DEFAULT": "operational",
    "PARITY_DAMAGED": "violation",
    "PARITY_CAPACITY_EXCEEDED": "violation",
    "ARCHIVE_ID_AMBIGUOUS": "operational",
}


class MochiError(Exception):
    """An error carrying a stable code and, optionally, the stored-object hash
    (hex) it concerns."""

    def __init__(self, code: str, message: str = "", obj: str | None = None):
        super().__init__(f"{code}: {message}" if message else code)
        assert code in CODE_CLASS, code
        self.code = code
        self.message = message
        self.obj = obj


def is_violation(code: str) -> bool:
    return CODE_CLASS[code] == "violation"
