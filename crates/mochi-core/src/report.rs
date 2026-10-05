//! Report schema **v0** (spec §20.5). Draft: the final schema is ratification
//! item R7 and replaces this in phase C7 (schema v1).
//!
//! What v0 does provide is the *shape* and the reporting invariants, so that
//! nothing built on top of it can start out lying:
//!
//! * a new report starts with every dimension `UNKNOWN` (never `PASS`);
//! * [`Report::validate`] rejects an overall status that hides a failing
//!   dimension, or a `PASS` that contradicts skipped/failed evidence.
//!
//! Timestamps follow Annex B.2 D15 ([`crate::timestamp::Timestamp`], T28);
//! the v0 fields still hold them as strings until C7's schema v1. Exit-code
//! precedence (D15) is T27.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::status::{Dimension, Status, VerificationLevel};

pub const REPORT_SCHEMA_VERSION: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
    pub spec_revision: String,
    pub wire_generation: u32,
    /// Always the draft label until the §28 gates pass.
    pub format_status: String,
}

impl ToolInfo {
    /// Describes this build of `mochi-core`.
    pub fn current() -> Self {
        Self {
            name: crate::TOOL_NAME.to_owned(),
            version: crate::TOOL_VERSION.to_owned(),
            spec_revision: mochi_format::version::SPEC_REVISION.to_owned(),
            wire_generation: mochi_format::version::WIRE_GENERATION,
            format_status: mochi_format::version::FORMAT_STATUS.to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// An inclusive range of commit sequences (Annex B.2 D10.9: "verification
/// names the affected sequence range").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeqRange {
    pub first: u64,
    pub last: u64,
}

/// A structured finding carrying a stable code (spec §20.5, §23.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub code: ErrorCode,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<String>,
    /// The commits this finding affects, when it is about stored objects of
    /// the history (review decision Q35; an input to ratification item R7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<SeqRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedItem {
    pub item: String,
    pub reason: String,
}

/// Objects and bytes checked versus expected, plus evidence age (spec §20.5).
/// `None` means "not measured", which is different from zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub expected_objects: Option<u64>,
    pub checked_objects: Option<u64>,
    pub expected_bytes: Option<u64>,
    pub checked_bytes: Option<u64>,
    pub evidence_age_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub archive_id: Option<String>,
    pub checked_commit: Option<String>,
    /// Independently recorded expected head (spec §5.7). Absent means freshness
    /// cannot be `PASS` (Annex B D8 default).
    pub expected_head: Option<String>,
    pub tool: ToolInfo,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub level: VerificationLevel,
    /// Human-readable scope; sampling must say so (spec §21).
    pub scope: Option<String>,
    pub coverage: Coverage,
    pub skipped: Vec<SkippedItem>,
    pub findings: Vec<Finding>,
    /// Repair actions, only if a repair was separately performed (spec §20.5).
    pub repair_actions: Vec<String>,
    pub dimensions: BTreeMap<Dimension, Status>,
    pub overall_status: Status,
}

/// A way in which a report breaks the spec's reporting invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportViolation {
    WrongSchemaVersion {
        found: u32,
    },
    /// `PASS` overall while a dimension is not `PASS` (spec §20.3).
    PassHidesDimension {
        dimension: Dimension,
        status: Status,
    },
    /// A `FAIL` dimension is concealed by a non-`FAIL` overall status (spec §20.3).
    FailingDimensionConcealed {
        dimension: Dimension,
    },
    /// `PASS` overall despite an error-severity finding (spec §5.4).
    PassWithErrorFinding {
        code: ErrorCode,
    },
    /// `PASS` overall despite skipped items (spec §5.4).
    PassWithSkippedItems {
        count: usize,
    },
    /// Freshness `PASS` with no expected head to compare against (spec §5.7).
    FreshnessPassWithoutExpectedHead,
}

impl fmt::Display for ReportViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSchemaVersion { found } => write!(
                f,
                "report schema version {found}, expected {REPORT_SCHEMA_VERSION}"
            ),
            Self::PassHidesDimension { dimension, status } => {
                write!(f, "overall PASS hides {dimension:?} = {status:?}")
            }
            Self::FailingDimensionConcealed { dimension } => {
                write!(f, "overall status conceals failing dimension {dimension:?}")
            }
            Self::PassWithErrorFinding { code } => {
                write!(f, "overall PASS despite error finding {code}")
            }
            Self::PassWithSkippedItems { count } => {
                write!(f, "overall PASS despite {count} skipped item(s)")
            }
            Self::FreshnessPassWithoutExpectedHead => {
                write!(f, "freshness PASS without an expected head")
            }
        }
    }
}

impl Report {
    /// A report with no evidence yet: every dimension and the overall status are
    /// `UNKNOWN`. Checks must earn `PASS`; it is never the default.
    pub fn new(level: VerificationLevel) -> Self {
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            archive_id: None,
            checked_commit: None,
            expected_head: None,
            tool: ToolInfo::current(),
            started_at: None,
            completed_at: None,
            level,
            scope: None,
            coverage: Coverage::default(),
            skipped: Vec::new(),
            findings: Vec::new(),
            repair_actions: Vec::new(),
            dimensions: Dimension::ALL
                .iter()
                .map(|d| (*d, Status::Unknown))
                .collect(),
            overall_status: Status::Unknown,
        }
    }

    /// Check the reporting invariants. Returns every violation found.
    ///
    /// This does not *compute* an overall status: how `FAIL`, `UNSUPPORTED`,
    /// `DEGRADED`, etc. combine into an exit code is unspecified (plan §9, O13)
    /// and lands with C7. It only refuses reports that lie.
    pub fn validate(&self) -> Result<(), Vec<ReportViolation>> {
        let mut out = Vec::new();

        if self.schema_version != REPORT_SCHEMA_VERSION {
            out.push(ReportViolation::WrongSchemaVersion {
                found: self.schema_version,
            });
        }

        for (dimension, status) in &self.dimensions {
            if *status == Status::Fail && self.overall_status != Status::Fail {
                out.push(ReportViolation::FailingDimensionConcealed {
                    dimension: *dimension,
                });
            }
            if self.overall_status.is_pass() && !status.is_pass() {
                out.push(ReportViolation::PassHidesDimension {
                    dimension: *dimension,
                    status: *status,
                });
            }
        }

        if self.overall_status.is_pass() {
            if let Some(f) = self.findings.iter().find(|f| f.severity == Severity::Error) {
                out.push(ReportViolation::PassWithErrorFinding { code: f.code });
            }
            if !self.skipped.is_empty() {
                out.push(ReportViolation::PassWithSkippedItems {
                    count: self.skipped.len(),
                });
            }
        }

        if self.dimensions.get(&Dimension::Freshness) == Some(&Status::Pass)
            && self.expected_head.is_none()
        {
            out.push(ReportViolation::FreshnessPassWithoutExpectedHead);
        }

        if out.is_empty() {
            Ok(())
        } else {
            Err(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_pass(mut r: Report) -> Report {
        for s in r.dimensions.values_mut() {
            *s = Status::Pass;
        }
        r.overall_status = Status::Pass;
        r.expected_head = Some("head".into());
        r
    }

    #[test]
    fn new_report_is_all_unknown_and_valid() {
        let r = Report::new(VerificationLevel::Structural);
        assert_eq!(r.overall_status, Status::Unknown);
        assert_eq!(r.dimensions.len(), Dimension::ALL.len());
        assert!(r.dimensions.values().all(|s| *s == Status::Unknown));
        assert_eq!(r.schema_version, 0);
        assert!(r.validate().is_ok());
    }

    #[test]
    fn tool_info_carries_the_draft_label() {
        let t = ToolInfo::current();
        assert_eq!(t.format_status, "experimental / draft-compatible");
        assert_eq!(t.spec_revision, "2.0");
    }

    #[test]
    fn genuine_all_pass_is_valid() {
        let r = all_pass(Report::new(VerificationLevel::StoredIntegrity));
        assert!(r.validate().is_ok());
    }

    #[test]
    fn pass_cannot_hide_a_non_pass_dimension() {
        let mut r = all_pass(Report::new(VerificationLevel::StoredIntegrity));
        r.dimensions
            .insert(Dimension::Recoverability, Status::Overdue);
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::PassHidesDimension {
            dimension: Dimension::Recoverability,
            status: Status::Overdue
        }));
    }

    #[test]
    fn overall_cannot_conceal_a_failing_dimension() {
        let mut r = Report::new(VerificationLevel::Structural);
        r.dimensions.insert(Dimension::Integrity, Status::Fail);
        r.overall_status = Status::Degraded;
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::FailingDimensionConcealed {
            dimension: Dimension::Integrity
        }));
        r.overall_status = Status::Fail;
        assert!(r.validate().is_ok());
    }

    #[test]
    fn pass_with_skips_or_error_findings_is_rejected() {
        let mut r = all_pass(Report::new(VerificationLevel::StoredIntegrity));
        r.skipped.push(SkippedItem {
            item: "obj-7".into(),
            reason: "unreadable".into(),
        });
        r.findings.push(Finding {
            code: ErrorCode::IoError,
            severity: Severity::Error,
            message: None,
            expected: None,
            observed: None,
            affected: None,
        });
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::PassWithSkippedItems { count: 1 }));
        assert!(v.contains(&ReportViolation::PassWithErrorFinding {
            code: ErrorCode::IoError
        }));
    }

    #[test]
    fn freshness_pass_requires_an_expected_head() {
        let mut r = all_pass(Report::new(VerificationLevel::Structural));
        r.expected_head = None;
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::FreshnessPassWithoutExpectedHead));
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let mut r = Report::new(VerificationLevel::Structural);
        r.schema_version = 1;
        assert!(r.validate().is_err());
    }

    #[test]
    fn json_round_trip_preserves_the_report() {
        let mut r = Report::new(VerificationLevel::Referential);
        r.archive_id = Some("example".into());
        r.findings.push(Finding {
            code: ErrorCode::UnsupportedFeature,
            severity: Severity::Warning,
            message: Some("m".into()),
            expected: Some("3".into()),
            observed: Some("2".into()),
            affected: None,
        });
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"overall_status\":\"UNKNOWN\""));
        assert!(json.contains("\"code\":\"UNSUPPORTED_FEATURE\""));
        // A finding without a range serializes exactly as it did before the
        // field existed (review decision Q35).
        assert!(!json.contains("affected"));
        let back: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn an_affected_range_round_trips_and_old_json_still_parses() {
        let f = Finding {
            code: ErrorCode::StoredIntegrityFailed,
            severity: Severity::Error,
            message: None,
            expected: None,
            observed: None,
            affected: Some(SeqRange { first: 3, last: 5 }),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"affected\":{\"first\":3,\"last\":5}"));
        assert_eq!(serde_json::from_str::<Finding>(&json).unwrap(), f);
        let old = r#"{"code":"IO_ERROR","severity":"error"}"#;
        assert_eq!(serde_json::from_str::<Finding>(old).unwrap().affected, None);
    }
}
