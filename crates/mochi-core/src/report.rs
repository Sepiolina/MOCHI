//! Report schema **v1** (spec §20.5; plan C7). Still a draft: the final
//! schema is ratification item R7. v1 adds [`Report::freshness_anchor`]
//! (spec Annex B.1 D8: "the report names the anchor used") to v0, and the
//! timestamps it carries are D15 [`crate::timestamp::Timestamp`] strings.
//! The field reference is `docs/report-schema-v1.md`.
//!
//! The shape carries the reporting invariants, so that nothing built on top
//! of it can start out lying:
//!
//! * a new report starts with every dimension `UNKNOWN` (never `PASS`);
//! * [`Report::validate`] rejects an overall status that hides a failing
//!   dimension, or a `PASS` that contradicts skipped/failed evidence.
//!
//! # Three results (D15, T26)
//!
//! A report carries three results in distinct fields, which
//! [`Report::conclude`] computes and [`Report::validate`] re-derives:
//!
//! * **evidence**: each dimension's status (`dimensions`) and their rollup
//!   (`overall_status`), ordered `FAIL` > `UNSUPPORTED` > `DEGRADED` >
//!   `OVERDUE` > `UNKNOWN` > `PASS` ([`Status::rollup`]);
//! * **policy result** (`policy_result`): the same rollup over the policy's
//!   required dimensions only, with the policy listed (`policy`);
//! * **exit code** (`exit_code`), by the D15 precedence
//!   ([`crate::exit::for_results`]).
//!
//! Freshness is required only when the user supplied an expected head, the
//! local history holds this archive ID, or freshness was explicitly requested
//! ([`FreshnessBasis`]). On first sight it is not required, so its `UNKNOWN`
//! does not stop exit 0.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::ErrorCode;
use crate::status::{Dimension, Status, VerificationLevel};

pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Which freshness anchor a report was judged against (spec Annex B.1 D8).
/// Serialized as `none`, `user`, or `local-history`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FreshnessAnchorKind {
    /// No anchor: freshness is `UNKNOWN`.
    #[default]
    None,
    /// An expected head supplied by the user.
    User,
    /// The head this client last saw for the archive ID.
    LocalHistory,
}

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

/// Why freshness is or is not required (Annex B.2 D15). Each field records
/// one of the three conditions; any one makes freshness required.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessBasis {
    /// The user supplied an expected head.
    pub expected_head_supplied: bool,
    /// The local history holds this archive ID.
    pub archive_in_local_history: bool,
    /// Freshness was explicitly requested.
    pub requested: bool,
}

impl FreshnessBasis {
    /// D15: freshness is required only when one of the conditions holds.
    pub const fn required(&self) -> bool {
        self.expected_head_supplied || self.archive_in_local_history || self.requested
    }
}

/// The verification policy a report was judged against (D15: "with the
/// policy listed"). Whether freshness is required is derived from
/// [`FreshnessBasis`], never chosen separately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The required dimensions, in [`Dimension`] order.
    pub required: BTreeSet<Dimension>,
    pub freshness: FreshnessBasis,
}

impl Policy {
    /// A policy requiring `required`. Naming [`Dimension::Freshness`] there is
    /// an explicit request for it (D15's third condition) and is recorded as
    /// such; otherwise freshness is required exactly when `freshness` says so.
    pub fn new(
        required: impl IntoIterator<Item = Dimension>,
        mut freshness: FreshnessBasis,
    ) -> Self {
        let mut required: BTreeSet<Dimension> = required.into_iter().collect();
        if required.contains(&Dimension::Freshness) {
            freshness.requested = true;
        }
        if freshness.required() {
            required.insert(Dimension::Freshness);
        }
        Self {
            required,
            freshness,
        }
    }

    /// The D15 policy result: the rollup over the required dimensions only.
    /// A required dimension with no reported status counts as `UNKNOWN`; a
    /// policy that requires nothing is `UNKNOWN` ([`Status::rollup`]).
    pub fn result(&self, dimensions: &BTreeMap<Dimension, Status>) -> Status {
        Status::rollup(
            self.required
                .iter()
                .map(|d| dimensions.get(d).copied().unwrap_or(Status::Unknown)),
        )
    }
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
    /// The anchor `expected_head` came from (D8). `none` exactly when
    /// `expected_head` is absent.
    #[serde(default)]
    pub freshness_anchor: FreshnessAnchorKind,
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
    /// D15 evidence rollup over every reported dimension.
    pub overall_status: Status,
    /// The policy `policy_result` was judged against.
    pub policy: Policy,
    /// D15 policy result: the rollup over `policy.required` only.
    pub policy_result: Status,
    /// The run itself was compromised (I/O, cancellation, bad invocation).
    pub operational_error: bool,
    /// D15 exit code ([`crate::exit::for_results`]).
    pub exit_code: u8,
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
    /// `overall_status` is not the D15 rollup of `dimensions`.
    EvidenceRollupMismatch {
        expected: Status,
        found: Status,
    },
    /// `policy_result` is not the D15 rollup of the required dimensions.
    PolicyResultMismatch {
        expected: Status,
        found: Status,
    },
    /// `exit_code` is not the D15 exit code of these results.
    ExitCodeMismatch {
        expected: u8,
        found: u8,
    },
    /// Freshness is required, or not, against what the basis says (D15).
    FreshnessRequirementMismatch {
        basis_requires: bool,
    },
    /// An expected head without a named anchor, or an anchor without an
    /// expected head (D8: the report names the anchor used).
    FreshnessAnchorMismatch,
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
            Self::EvidenceRollupMismatch { expected, found } => write!(
                f,
                "overall status {found:?} is not the rollup of the dimensions ({expected:?})"
            ),
            Self::PolicyResultMismatch { expected, found } => write!(
                f,
                "policy result {found:?} is not the rollup of the required dimensions \
                 ({expected:?})"
            ),
            Self::ExitCodeMismatch { expected, found } => {
                write!(
                    f,
                    "exit code {found} does not follow from the results ({expected})"
                )
            }
            Self::FreshnessRequirementMismatch { basis_requires } => write!(
                f,
                "freshness is {}required, but its basis says it is {}",
                if *basis_requires { "not " } else { "" },
                if *basis_requires {
                    "required"
                } else {
                    "not required"
                }
            ),
            Self::FreshnessAnchorMismatch => write!(
                f,
                "the expected head and the named freshness anchor disagree"
            ),
        }
    }
}

impl Report {
    /// A report with no evidence yet: every dimension and the overall status are
    /// `UNKNOWN`, the policy requires nothing (so its result is `UNKNOWN` too),
    /// and the exit code is 2. Checks must earn `PASS`; it is never the default.
    pub fn new(level: VerificationLevel) -> Self {
        let mut r = Self {
            schema_version: REPORT_SCHEMA_VERSION,
            archive_id: None,
            checked_commit: None,
            expected_head: None,
            freshness_anchor: FreshnessAnchorKind::None,
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
            policy: Policy::default(),
            policy_result: Status::Unknown,
            operational_error: false,
            exit_code: crate::exit::DEGRADED,
        };
        r.conclude(false);
        r
    }

    /// Compute the three D15 results from `dimensions` and `policy`: the
    /// evidence rollup, the policy result, and the exit code. Call it after
    /// the last dimension is set; `operational_error` says whether the run
    /// itself was compromised.
    pub fn conclude(&mut self, operational_error: bool) {
        self.overall_status = Status::rollup(self.dimensions.values().copied());
        self.policy_result = self.policy.result(&self.dimensions);
        self.operational_error = operational_error;
        self.exit_code = crate::exit::for_results(
            self.dimensions.values().copied(),
            self.policy_result,
            operational_error,
        );
    }

    /// Check the reporting invariants. Returns every violation found.
    ///
    /// Besides refusing reports that lie, it re-derives the three D15 results
    /// and refuses any that differ from what [`Report::conclude`] computes.
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

        if self.expected_head.is_some() != (self.freshness_anchor != FreshnessAnchorKind::None) {
            out.push(ReportViolation::FreshnessAnchorMismatch);
        }

        let evidence = Status::rollup(self.dimensions.values().copied());
        if self.overall_status != evidence {
            out.push(ReportViolation::EvidenceRollupMismatch {
                expected: evidence,
                found: self.overall_status,
            });
        }
        let basis_requires = self.policy.freshness.required();
        if self.policy.required.contains(&Dimension::Freshness) != basis_requires {
            out.push(ReportViolation::FreshnessRequirementMismatch { basis_requires });
        }
        let policy = self.policy.result(&self.dimensions);
        if self.policy_result != policy {
            out.push(ReportViolation::PolicyResultMismatch {
                expected: policy,
                found: self.policy_result,
            });
        }
        let exit = crate::exit::for_results(
            self.dimensions.values().copied(),
            self.policy_result,
            self.operational_error,
        );
        if self.exit_code != exit {
            out.push(ReportViolation::ExitCodeMismatch {
                expected: exit,
                found: self.exit_code,
            });
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
        r.freshness_anchor = FreshnessAnchorKind::User;
        r
    }

    #[test]
    fn new_report_is_all_unknown_and_valid() {
        let r = Report::new(VerificationLevel::Structural);
        assert_eq!(r.overall_status, Status::Unknown);
        assert_eq!(r.dimensions.len(), Dimension::ALL.len());
        assert!(r.dimensions.values().all(|s| *s == Status::Unknown));
        assert_eq!(r.schema_version, 1);
        assert_eq!(r.freshness_anchor, FreshnessAnchorKind::None);
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
        r.conclude(false);
        assert_eq!(r.overall_status, Status::Fail);
        assert!(r.validate().is_ok());
        r.overall_status = Status::Degraded;
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::FailingDimensionConcealed {
            dimension: Dimension::Integrity
        }));
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
        r.freshness_anchor = FreshnessAnchorKind::None;
        let v = r.validate().unwrap_err();
        assert!(v.contains(&ReportViolation::FreshnessPassWithoutExpectedHead));
    }

    /// D8: an expected head always names its anchor, and an anchor always
    /// comes with the head it supplied.
    #[test]
    fn the_anchor_and_the_expected_head_go_together() {
        let mut r = Report::new(VerificationLevel::Structural);
        r.expected_head = Some("head".into());
        assert!(r
            .validate()
            .unwrap_err()
            .contains(&ReportViolation::FreshnessAnchorMismatch));
        r.expected_head = None;
        r.freshness_anchor = FreshnessAnchorKind::LocalHistory;
        assert!(r
            .validate()
            .unwrap_err()
            .contains(&ReportViolation::FreshnessAnchorMismatch));
        let json = serde_json::to_string(&FreshnessAnchorKind::LocalHistory).unwrap();
        assert_eq!(json, "\"local-history\"");
    }

    /// A required dimension the map does not mention has no evidence: it
    /// counts as `UNKNOWN`, never as a pass.
    #[test]
    fn a_missing_required_dimension_is_unknown() {
        let p = Policy::new(
            [Dimension::Integrity, Dimension::Durability],
            FreshnessBasis::default(),
        );
        let only_integrity: BTreeMap<_, _> = [(Dimension::Integrity, Status::Pass)].into();
        assert_eq!(p.result(&only_integrity), Status::Unknown);
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let mut r = Report::new(VerificationLevel::Structural);
        r.schema_version = 0;
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
