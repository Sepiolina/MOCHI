//! Automation exit codes (spec §23.2) and their precedence (Annex B.2 D15).
//!
//! The codes live here, not in `mochi-cli`, because a report carries its
//! exit code as a field (D15 "three results") and [`crate::report::Report`]
//! checks it. `mochi_cli::exit` re-exports this module, so there is still
//! one registry.
//!
//! # Precedence (D15)
//!
//! The first rule that matches wins:
//!
//! | Exit | Condition |
//! |---|---|
//! | 1 | Any reported dimension is `FAIL`, whether required or not |
//! | 3 | An operational error occurred |
//! | 4 | The policy result is `UNSUPPORTED` |
//! | 2 | The policy result is `DEGRADED`, `OVERDUE`, or `UNKNOWN` |
//! | 0 | Otherwise |
//!
//! A failure is evidence that something is wrong, so nothing masks it: not
//! an I/O error found later, and not the dimension being outside the policy
//! (§5.4, §20.3). Only the policy's required dimensions can produce 4 or 2;
//! an `UNSUPPORTED` or `UNKNOWN` dimension the policy does not require is
//! reported but does not change the exit code. The JSON report remains
//! authoritative (§23.2): automation should treat any non-zero code as "not
//! verified" and read the report for the full set of conditions.

use crate::status::Status;

/// Requested checks passed.
pub const OK: u8 = 0;
/// Verification or policy failure.
pub const FAILED: u8 = 1;
/// Degraded, unknown, or overdue required evidence.
pub const DEGRADED: u8 = 2;
/// Invocation, configuration, or operational error.
pub const ERROR: u8 = 3;
/// Unsupported required feature or verification capability.
pub const UNSUPPORTED: u8 = 4;

/// The D15 exit code of one run.
///
/// * `dimensions`: every reported dimension's status, required or not;
/// * `policy_result`: the rollup over the required dimensions only;
/// * `operational_error`: the run itself was compromised (I/O, cancellation,
///   bad invocation).
pub fn for_results(
    dimensions: impl IntoIterator<Item = Status>,
    policy_result: Status,
    operational_error: bool,
) -> u8 {
    if dimensions.into_iter().any(|s| s == Status::Fail) {
        return FAILED;
    }
    if operational_error {
        return ERROR;
    }
    match policy_result {
        // A required FAIL is a reported FAIL, caught above.
        Status::Fail => FAILED,
        Status::Unsupported => UNSUPPORTED,
        Status::Degraded | Status::Overdue | Status::Unknown => DEGRADED,
        Status::Pass => OK,
    }
}

/// Most important first; the same order as [`for_results`].
const PRECEDENCE: [u8; 5] = [FAILED, ERROR, UNSUPPORTED, DEGRADED, OK];

/// Combine the exit codes of several conditions that are already codes (for
/// example, several commands' errors) by the D15 precedence. No conditions
/// means `OK`. An unknown code is treated as `ERROR`, never silently dropped.
///
/// A run that has a report takes its code from [`for_results`]: whether a
/// dimension is required decides whether its `UNSUPPORTED` counts, and a code
/// has already lost that.
pub fn combine(codes: impl IntoIterator<Item = u8>) -> u8 {
    let mut best = OK;
    let rank = |c: u8| PRECEDENCE.iter().position(|p| *p == c);
    for c in codes {
        let c = if rank(c).is_some() { c } else { ERROR };
        if rank(c) < rank(best) {
            best = c;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::*;

    #[test]
    fn failure_is_never_masked() {
        assert_eq!(combine([OK, DEGRADED, UNSUPPORTED, ERROR, FAILED]), FAILED);
        assert_eq!(combine([FAILED, ERROR]), FAILED);
    }

    #[test]
    fn full_order() {
        assert_eq!(combine([]), OK);
        assert_eq!(combine([OK, DEGRADED]), DEGRADED);
        assert_eq!(combine([DEGRADED, UNSUPPORTED]), UNSUPPORTED);
        assert_eq!(combine([UNSUPPORTED, ERROR]), ERROR);
        assert_eq!(combine([ERROR, FAILED]), FAILED);
    }

    #[test]
    fn order_is_independent_of_input_order() {
        let all = [OK, FAILED, DEGRADED, ERROR, UNSUPPORTED];
        for i in 0..all.len() {
            let mut v = all.to_vec();
            v.rotate_left(i);
            assert_eq!(combine(v), FAILED);
        }
    }

    #[test]
    fn unknown_codes_count_as_error() {
        assert_eq!(combine([OK, 42]), ERROR);
        assert_eq!(combine([42, FAILED]), FAILED);
    }

    /// Each D15 rule, alone and against the rules below it.
    #[test]
    fn for_results_follows_the_d15_table() {
        // 1: any reported FAIL, even with an operational error and a policy
        // that does not require that dimension.
        assert_eq!(for_results([Fail, Pass], Pass, true), FAILED);
        assert_eq!(for_results([Fail], Unsupported, false), FAILED);
        // 3: an operational error beats the policy result.
        assert_eq!(for_results([Unsupported], Unsupported, true), ERROR);
        assert_eq!(for_results([Pass], Pass, true), ERROR);
        // 4, then 2, then 0, from the policy result only.
        assert_eq!(
            for_results([Unsupported, Unknown], Unsupported, false),
            UNSUPPORTED
        );
        for s in [Degraded, Overdue, Unknown] {
            assert_eq!(for_results([Unsupported, s], s, false), DEGRADED, "{s:?}");
        }
        assert_eq!(for_results([Unsupported, Unknown, Pass], Pass, false), OK);
        assert_eq!(for_results([], Pass, false), OK);
    }

    /// `combine` and `for_results` rank the codes the same way.
    #[test]
    fn both_functions_share_one_order() {
        let cases = [
            (vec![Fail], Unsupported, true),
            (vec![Pass], Unsupported, true),
            (vec![Pass], Unsupported, false),
            (vec![Pass], Degraded, false),
            (vec![Pass], Pass, false),
        ];
        let codes: Vec<u8> = cases
            .iter()
            .map(|(d, p, e)| for_results(d.clone(), *p, *e))
            .collect();
        assert_eq!(codes, PRECEDENCE);
    }
}
