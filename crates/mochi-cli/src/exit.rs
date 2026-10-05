//! Automation exit codes (spec §23.2).
//!
//! # Precedence (plan §9, O13, decided)
//!
//! When one run meets several conditions, the exit code is the first that
//! applies in this order. The JSON report remains authoritative (§23.2).
//!
//! 1. **`1` failure** — definite evidence that something is wrong. Nothing may
//!    mask it: an overall result never hides a failing dimension (§5.4,
//!    §20.3), and a failure found before an I/O error is still a failure.
//! 2. **`3` error** — the run itself was compromised (bad invocation, I/O,
//!    cancellation), so any remaining "pass" is not trustworthy.
//! 3. **`4` unsupported** — a *required* check could not be performed at all.
//! 4. **`2` degraded** — checks ran, but evidence is partial, unknown, or
//!    overdue.
//! 5. **`0` ok** — only when every requested check passed.
//!
//! Automation should therefore treat any non-zero code as "not verified", and
//! read the report for the full set of conditions.

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

/// Most important first; see the module note.
const PRECEDENCE: [u8; 5] = [FAILED, ERROR, UNSUPPORTED, DEGRADED, OK];

/// Combine the exit codes of several conditions by the documented precedence.
/// No conditions means `OK`. An unknown code is treated as `ERROR`, never
/// silently dropped.
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
}
