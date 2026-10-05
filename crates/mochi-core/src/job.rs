//! The one jobs layer (plan §2.2, spec §23.4).
//!
//! Long operations take a [`JobContext`] (a progress sink plus a cancellation
//! token) and return a typed report. The CLI renders reports as text or JSON;
//! the desktop app streams the same progress over a Tauri `Channel`. There is
//! deliberately no second progress mechanism (AGENTS.md).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::{ErrorCode, MochiError, Result};

/// Cooperative cancellation. Cheap to clone; all clones share one flag.
///
/// Cancelling before publication step 7 leaves the previous head valid
/// (spec §5.1, plan C5); jobs must call [`CancellationToken::check`] at safe
/// points only.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(CANCELLED)` if cancellation was requested.
    pub fn check(&self) -> Result<()> {
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
