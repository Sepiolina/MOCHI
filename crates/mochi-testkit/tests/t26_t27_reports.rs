//! T26 (three results in the report) and T27 (exit precedence): spec Annex
//! B.2 D15; gate G9.

use mochi_core::exit;
use mochi_core::report::{FreshnessBasis, Policy, Report, ReportViolation};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use serde_json::Value;

use Dimension::*;
use Status::*;

/// A report whose dimensions are `set` (every other one `UNKNOWN`), judged
/// against `policy`, concluded with `operational_error`.
fn report(set: &[(Dimension, Status)], policy: Policy, operational_error: bool) -> Report {
    let mut r = Report::new(VerificationLevel::StoredIntegrity);
    for (d, s) in set {
        r.dimensions.insert(*d, *s);
    }
    r.policy = policy;
    r.conclude(operational_error);
    r.validate().unwrap();
    r
}

fn first_sight() -> FreshnessBasis {
    FreshnessBasis::default()
}

/// **G9 matrix.** First-sight `UNKNOWN` freshness exits 0, and each mixed
/// outcome gets its D15 exit code.
#[test]
fn g9_mixed_outcome_matrix() {
    // First sight: no expected head, no local history, not requested, so
    // freshness is not required and its UNKNOWN does not stop exit 0.
    let p = Policy::new([Durability, Integrity], first_sight());
    assert!(!p.required.contains(&Freshness));
    let r = report(&[(Durability, Pass), (Integrity, Pass)], p, false);
    assert_eq!(r.dimensions[&Freshness], Unknown);
    assert_eq!(r.overall_status, Unknown, "the evidence still says UNKNOWN");
    assert_eq!(r.policy_result, Pass);
    assert_eq!(r.exit_code, exit::OK);

    type Case = (
        &'static str,
        Vec<(Dimension, Status)>,
        Vec<Dimension>,
        bool,
        u8,
    );
    let cases: Vec<Case> = vec![
        (
            "FAIL with an I/O error",
            vec![(Integrity, Fail)],
            vec![Integrity],
            true,
            exit::FAILED,
        ),
        (
            "FAIL in a dimension that is not required",
            vec![(Integrity, Pass), (Searchability, Fail)],
            vec![Integrity],
            false,
            exit::FAILED,
        ),
        (
            "required UNSUPPORTED with an I/O error",
            vec![(KeyAvailability, Unsupported)],
            vec![KeyAvailability],
            true,
            exit::ERROR,
        ),
        (
            "required UNSUPPORTED with a required UNKNOWN",
            vec![(KeyAvailability, Unsupported), (Recoverability, Unknown)],
            vec![KeyAvailability, Recoverability],
            false,
            exit::UNSUPPORTED,
        ),
        (
            "UNSUPPORTED not required, with a required DEGRADED",
            vec![(KeyAvailability, Unsupported), (Durability, Degraded)],
            vec![Durability],
            false,
            exit::DEGRADED,
        ),
    ];
    for (what, set, required, io, want) in cases {
        let r = report(&set, Policy::new(required, first_sight()), io);
        assert_eq!(r.exit_code, want, "{what}");
        // The evidence rollup never hides a FAIL, required or not.
        if set.iter().any(|(_, s)| *s == Fail) {
            assert_eq!(r.overall_status, Fail, "{what}");
        }
    }
}

/// **D15 freshness.** Required exactly when the user supplied an expected
/// head, the local history holds the archive ID, or it was requested; naming
/// it in the policy is a request.
#[test]
fn t26_freshness_requirement_is_derived() {
    let bases = [
        FreshnessBasis {
            expected_head_supplied: true,
            ..Default::default()
        },
        FreshnessBasis {
            archive_in_local_history: true,
            ..Default::default()
        },
        FreshnessBasis {
            requested: true,
            ..Default::default()
        },
    ];
    for b in bases {
        assert!(b.required(), "{b:?}");
        let p = Policy::new([Integrity], b);
        assert!(p.required.contains(&Freshness), "{b:?}");
        // Required and UNKNOWN: exit 2, not 0.
        let r = report(&[(Integrity, Pass)], p, false);
        assert_eq!(r.policy_result, Unknown);
        assert_eq!(r.exit_code, exit::DEGRADED);
    }
    assert!(!first_sight().required());

    let p = Policy::new([Integrity, Freshness], first_sight());
    assert!(p.freshness.requested, "naming freshness requests it");
    assert!(p.required.contains(&Freshness));

    // A policy that requires freshness against its basis is refused.
    let mut r = report(
        &[(Integrity, Pass)],
        Policy::new([Integrity], first_sight()),
        false,
    );
    r.policy.required.insert(Freshness);
    r.conclude(false);
    assert!(r
        .validate()
        .unwrap_err()
        .contains(&ReportViolation::FreshnessRequirementMismatch {
            basis_requires: false
        }));
}

/// **T26 DoD (JSON schema test).** The three results are separate JSON
/// fields with fixed names and forms, the policy is listed, and the JSON
/// round-trips.
#[test]
fn t26_json_carries_three_separate_results() {
    let r = report(
        &[(Integrity, Pass), (KeyAvailability, Unsupported)],
        Policy::new(
            [Integrity],
            FreshnessBasis {
                archive_in_local_history: true,
                ..Default::default()
            },
        ),
        false,
    );
    let v: Value = serde_json::to_value(&r).unwrap();
    let o = v.as_object().unwrap();
    for k in [
        "dimensions",
        "overall_status",
        "policy",
        "policy_result",
        "operational_error",
        "exit_code",
    ] {
        assert!(o.contains_key(k), "missing {k}");
    }
    assert_eq!(v["overall_status"], "UNSUPPORTED", "evidence: worst of all");
    assert_eq!(v["policy_result"], "UNKNOWN", "policy: freshness required");
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["operational_error"], false);
    assert_eq!(
        v["policy"]["required"],
        serde_json::json!(["integrity", "freshness"])
    );
    assert_eq!(
        v["policy"]["freshness"],
        serde_json::json!({
            "expected_head_supplied": false,
            "archive_in_local_history": true,
            "requested": false
        })
    );
    let back: Report = serde_json::from_value(v).unwrap();
    assert_eq!(back, r);
    back.validate().unwrap();
}

/// Each result is checked against the others: a report that states a result
/// that does not follow is refused.
#[test]
fn t26_inconsistent_results_are_refused() {
    let base = report(
        &[(Integrity, Pass), (Searchability, Fail)],
        Policy::new([Integrity], first_sight()),
        false,
    );
    assert_eq!(base.exit_code, exit::FAILED);

    let mut r = base.clone();
    r.overall_status = Unsupported;
    assert!(r
        .validate()
        .unwrap_err()
        .contains(&ReportViolation::EvidenceRollupMismatch {
            expected: Fail,
            found: Unsupported
        }));

    let mut r = base.clone();
    r.policy_result = Unknown;
    assert!(r
        .validate()
        .unwrap_err()
        .contains(&ReportViolation::PolicyResultMismatch {
            expected: Pass,
            found: Unknown
        }));

    // The non-required FAIL must not be turned into a clean exit.
    let mut r = base.clone();
    r.exit_code = exit::OK;
    assert!(r
        .validate()
        .unwrap_err()
        .contains(&ReportViolation::ExitCodeMismatch {
            expected: exit::FAILED,
            found: exit::OK
        }));

    // An operational error recorded but not reflected in the code.
    let mut r = report(
        &[(Integrity, Pass)],
        Policy::new([Integrity], first_sight()),
        false,
    );
    assert_eq!(r.exit_code, exit::OK);
    r.operational_error = true;
    assert!(r
        .validate()
        .unwrap_err()
        .contains(&ReportViolation::ExitCodeMismatch {
            expected: exit::ERROR,
            found: exit::OK
        }));
}

/// A policy that requires nothing has nothing that earned a pass: its result
/// is `UNKNOWN` and the run exits 2, never 0.
#[test]
fn t26_an_empty_policy_never_passes() {
    // Every dimension that can pass without an expected head (§5.7) passes.
    let all: Vec<_> = Dimension::ALL
        .iter()
        .filter(|d| **d != Freshness)
        .map(|d| (*d, Pass))
        .collect();
    let r = report(&all, Policy::new([], first_sight()), false);
    assert_eq!(r.overall_status, Unknown);
    assert_eq!(r.policy_result, Unknown);
    assert_eq!(r.exit_code, exit::DEGRADED);
}
