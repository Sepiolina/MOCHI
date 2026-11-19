//! A synthetic sample job: multi-step, no archive I/O. It is no longer reachable
//! from the UI; it stays as the job runner's test fixture (progress, cancel,
//! argument limits in `jobs.rs` tests) and exercises the same
//! `mochi_core::job` interface every real job uses.

use std::time::Duration;

use mochi_core::job::JobContext;
use mochi_core::Result;

/// Most steps one self-test may take.
pub const MAX_STEPS: u32 = 1000;
/// Longest step, in milliseconds.
pub const MAX_STEP_MS: u32 = 1000;

/// The phase name in progress events.
pub const PHASE: &str = "self_test";

/// Run `steps` steps of `step_ms` milliseconds, reporting progress before
/// the first step and after each one, and checking cancellation before each
/// step (its only safe points). The caller has validated the bounds.
pub fn run(steps: u32, step_ms: u32, ctx: &JobContext<'_>) -> Result<String> {
    let total = u64::from(steps);
    ctx.report(PHASE, 0, Some(total));
    for done in 1..=total {
        ctx.check_cancelled()?;
        std::thread::sleep(Duration::from_millis(u64::from(step_ms)));
        ctx.report(PHASE, done, Some(total));
    }
    Ok(format!("self-test finished {steps} steps"))
}
