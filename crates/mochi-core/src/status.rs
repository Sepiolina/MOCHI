//! Health statuses and dimensions (spec §20.3, §20.4).

use serde::{Deserialize, Serialize};

/// The only permitted status values (spec §20.4). AGENTS.md: use no others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    /// Required checks completed successfully within policy.
    Pass,
    /// A required check found a violation.
    Fail,
    /// Some capability remains, but a required protection is reduced.
    Degraded,
    /// Evidence is insufficient or unavailable.
    Unknown,
    /// Required evidence is older than policy permits.
    Overdue,
    /// The implementation cannot perform the required check.
    Unsupported,
}

impl Status {
    /// Only `PASS` may ever be rendered with success styling (spec §23.3 #2).
    /// A skipped, unsupported, incomplete, or overdue check is never `PASS`.
    pub const fn is_pass(self) -> bool {
        matches!(self, Status::Pass)
    }
}

/// Health dimensions reported separately (spec §20.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Durability,
    Integrity,
    Recoverability,
    Searchability,
    Freshness,
    RetentionCompliance,
    KeyAvailability,
}

impl Dimension {
    pub const ALL: [Dimension; 7] = [
        Dimension::Durability,
        Dimension::Integrity,
        Dimension::Recoverability,
        Dimension::Searchability,
        Dimension::Freshness,
        Dimension::RetentionCompliance,
        Dimension::KeyAvailability,
    ];
}

/// Verification levels (spec §20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationLevel {
    /// Preservation profile only; `UNSUPPORTED` in 1.0.
    Inventory,
    Structural,
    Referential,
    StoredIntegrity,
    ContentIntegrity,
    Restoration,
    /// Optional in 1.0 (plan C13).
    Search,
    /// Preservation profile only; `UNSUPPORTED` in 1.0.
    DisasterRecovery,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pass_is_pass() {
        for s in [
            Status::Fail,
            Status::Degraded,
            Status::Unknown,
            Status::Overdue,
            Status::Unsupported,
        ] {
            assert!(!s.is_pass(), "{s:?} must never look like success");
        }
        assert!(Status::Pass.is_pass());
    }

    #[test]
    fn status_wire_forms_match_spec_table() {
        let forms: Vec<String> = [
            Status::Pass,
            Status::Fail,
            Status::Degraded,
            Status::Unknown,
            Status::Overdue,
            Status::Unsupported,
        ]
        .iter()
        .map(|s| serde_json::to_string(s).unwrap())
        .collect();
        assert_eq!(
            forms,
            [
                "\"PASS\"",
                "\"FAIL\"",
                "\"DEGRADED\"",
                "\"UNKNOWN\"",
                "\"OVERDUE\"",
                "\"UNSUPPORTED\""
            ]
        );
    }

    #[test]
    fn dimension_names_match_spec_20_3() {
        let names: Vec<String> = Dimension::ALL
            .iter()
            .map(|d| serde_json::to_string(d).unwrap().replace('"', ""))
            .collect();
        assert_eq!(
            names,
            [
                "durability",
                "integrity",
                "recoverability",
                "searchability",
                "freshness",
                "retention_compliance",
                "key_availability"
            ]
        );
    }
}
