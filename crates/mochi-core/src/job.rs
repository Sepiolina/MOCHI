//! The one jobs layer (plan §2.2, spec §23.4).
//!
//! Long operations take a [`JobContext`] (a progress sink plus a cancellation
//! token) and return a typed report. The CLI renders reports as text or JSON;
//! the desktop app streams the same progress over a Tauri `Channel`. There is
//! deliberately no second progress mechanism (AGENTS.md).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::error::{ErrorCode, MochiError, Result};

/// Cooperative cancellation, and pausing at the same safe points. Cheap to
/// clone; all clones share one state.
///
/// Cancelling before publication step 7 leaves the previous head valid
/// (spec §5.1, plan C5); jobs must call [`CancellationToken::check`] at safe
/// points only. A paused job waits inside `check` until it is resumed or
/// cancelled, so it stops only where it could also have been cancelled.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    pause: Arc<PauseGate>,
}

/// Whether the job should wait at its next safe point; cancelling wakes it.
#[derive(Debug, Default)]
struct PauseGate {
    paused: Mutex<bool>,
    changed: Condvar,
}

impl PauseGate {
    fn lock(&self) -> MutexGuard<'_, bool> {
        // The guarded value is a single flag: still consistent after a panic.
        self.paused.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn set(&self, paused: bool) {
        *self.lock() = paused;
        self.changed.notify_all();
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // Take the gate's lock so a job about to wait sees the flag or the wake-up.
        drop(self.pause.lock());
        self.pause.changed.notify_all();
    }

    /// The shared flag itself, for a collaborator outside this crate that
    /// must notice cancellation on its own thread (the foreign-format helper
    /// supervisor kills its child process when it is set).
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Ask the job to wait at its next safe point ([`CancellationToken::check`]).
    pub fn pause(&self) {
        self.pause.set(true);
    }

    /// Let a paused job go on.
    pub fn resume(&self) {
        self.pause.set(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.pause.lock()
    }

    /// `Err(CANCELLED)` if cancellation was requested. While the job is
    /// paused, waits here until it is resumed or cancelled.
    pub fn check(&self) -> Result<()> {
        let mut paused = self.pause.lock();
        while *paused && !self.is_cancelled() {
            paused = self
                .pause
                .changed
                .wait(paused)
                .unwrap_or_else(|p| p.into_inner());
        }
        drop(paused);
        if self.is_cancelled() {
            Err(MochiError::new(ErrorCode::Cancelled, "operation cancelled"))
        } else {
            Ok(())
        }
    }
}

/// One progress update. `total` is `None` when it is not yet known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub phase: &'static str,
    pub completed: u64,
    pub total: Option<u64>,
}

/// Receives progress. Implementations must be cheap and must not block the job.
pub trait ProgressSink: Send + Sync {
    fn report(&self, event: &ProgressEvent);
}

/// Discards progress.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProgress;

impl ProgressSink for NullProgress {
    fn report(&self, _event: &ProgressEvent) {}
}

/// Everything a job needs from its caller.
pub struct JobContext<'a> {
    pub progress: &'a dyn ProgressSink,
    pub cancel: &'a CancellationToken,
}

impl JobContext<'_> {
    pub fn report(&self, phase: &'static str, completed: u64, total: Option<u64>) {
        self.progress.report(&ProgressEvent {
            phase,
            completed,
            total,
        });
    }

    pub fn check_cancelled(&self) -> Result<()> {
        self.cancel.check()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Recorder(Mutex<Vec<ProgressEvent>>);

    impl ProgressSink for Recorder {
        fn report(&self, event: &ProgressEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    #[test]
    fn clones_share_cancellation() {
        let a = CancellationToken::new();
        let b = a.clone();
        assert!(b.check().is_ok());
        a.cancel();
        assert!(b.is_cancelled());
        assert_eq!(b.check().unwrap_err().code, ErrorCode::Cancelled);
    }

    #[test]
    fn a_paused_job_waits_at_its_next_check_until_resumed() {
        let token = CancellationToken::new();
        token.pause();
        assert!(token.is_paused());
        let job = token.clone();
        let passed = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&passed);
        let worker = std::thread::spawn(move || {
            let r = job.check();
            seen.store(true, Ordering::SeqCst);
            r
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !passed.load(Ordering::SeqCst),
            "a paused job went past check"
        );
        token.resume();
        assert!(worker.join().unwrap().is_ok());
        assert!(!token.is_paused());
    }

    #[test]
    fn cancelling_wakes_a_paused_job_with_cancelled() {
        let token = CancellationToken::new();
        token.pause();
        let job = token.clone();
        let worker = std::thread::spawn(move || job.check());
        std::thread::sleep(std::time::Duration::from_millis(20));
        token.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err().code,
            ErrorCode::Cancelled
        );
    }

    #[test]
    fn context_forwards_progress_and_cancellation() {
        let rec = Recorder(Mutex::new(Vec::new()));
        let token = CancellationToken::new();
        let ctx = JobContext {
            progress: &rec,
            cancel: &token,
        };
        ctx.report("scan", 1, Some(4));
        ctx.report("scan", 2, None);
        assert!(ctx.check_cancelled().is_ok());
        token.cancel();
        assert!(ctx.check_cancelled().is_err());
        let events = rec.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].total, Some(4));
        assert_eq!(events[1].total, None);
    }

    #[test]
    fn null_progress_is_a_valid_sink() {
        let token = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &token,
        };
        ctx.report("x", 0, None);
    }
}
