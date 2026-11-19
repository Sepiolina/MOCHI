//! The job runner: bridges `mochi_core::job` (a progress sink and a
//! cancellation token) to an event stream, which the app sends over a Tauri
//! `Channel` (AGENTS.md: no second progress mechanism).
//!
//! It is independent of Tauri so that it is tested without a window: events
//! go to an [`EventSink`], and work starts through a spawner the caller
//! supplies (`tauri::async_runtime::spawn_blocking` in the app, a thread in
//! tests). Every job emits `started`, then progress, then exactly one
//! `finished`, also when it is cancelled, fails, or unwinds.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::{ErrorCode, MochiError};

use crate::ipc::{IpcError, JobEvent, JobId, JobOutcome, JobRequest};
use crate::write::{JobDone, Services};

/// Where a job's events go. `send` returns `false` once nobody is listening
/// (the window was closed or reloaded); the runner then cancels the job.
pub trait EventSink: Send + Sync + 'static {
    fn send(&self, event: JobEvent) -> bool;
}

/// Starts a unit of blocking work off the async runtime.
pub type Spawner = dyn FnOnce(Box<dyn FnOnce() + Send + 'static>);

/// The jobs that are running, by ID, with their cancellation tokens.
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<Registry>,
    /// What the write jobs (D2) work on.
    services: Services,
}

#[derive(Debug, Default)]
struct Registry {
    last_id: JobId,
    /// Each running job's token and the window (its label) that started it.
    running: HashMap<JobId, (CancellationToken, String)>,
}

impl JobRegistry {
    /// A registry whose write jobs work on `services`.
    pub fn with_services(services: Services) -> Self {
        JobRegistry {
            inner: Mutex::default(),
            services,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Registry> {
        // A poisoned lock only means another thread panicked while holding
        // it; the map itself is still consistent (every update is a single
        // insert or remove).
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Validate `request`, register a new job, and start it with `spawn`.
    /// Returns as soon as the job is started; its events go to `sink`.
    pub fn start(
        self: &Arc<Self>,
        request: JobRequest,
        sink: Arc<dyn EventSink>,
        spawn: Box<Spawner>,
    ) -> Result<JobId, IpcError> {
        self.start_for(
            crate::browse::UNOWNED,
            self.services.clone(),
            request,
            sink,
            spawn,
        )
    }

    /// The services a window's jobs work on: the shared ones (archives,
    /// foreign archives, updater) and that window's own staging, destination
    /// and Test and Repair state.
    pub fn services_for(&self, window: &crate::windows::WindowState) -> Services {
        Services {
            staging: Arc::clone(&window.staging),
            destination: Arc::clone(&window.destination),
            checks: Arc::clone(&window.checks),
            finder: Some(Arc::clone(&window.finder)),
            ..self.services.clone()
        }
    }

    /// [`JobRegistry::start`] for the window `owner`, working on `services`
    /// (`services_for`). Closing the window cancels the job.
    pub fn start_for(
        self: &Arc<Self>,
        owner: &str,
        services: Services,
        request: JobRequest,
        sink: Arc<dyn EventSink>,
        spawn: Box<Spawner>,
    ) -> Result<JobId, IpcError> {
        validate(&request, &services)?;
        let token = CancellationToken::new();
        let id = {
            let mut reg = self.lock();
            let id = reg.last_id.checked_add(1).ok_or_else(|| {
                IpcError::from(MochiError::new(
                    ErrorCode::LimitExceeded,
                    "this process has run the maximum number of jobs; restart the app",
                ))
            })?;
            reg.last_id = id;
            reg.running.insert(id, (token.clone(), owner.to_owned()));
            id
        };
        let registry = Arc::clone(self);
        spawn(Box::new(move || {
            run_job(&registry, &services, id, &request, sink, &token);
        }));
        Ok(id)
    }

    /// Request cancellation. `false` if no such job is running (it never
    /// existed or has already finished). The job stops at its next safe
    /// point and then reports `cancelled`.
    pub fn cancel(&self, id: JobId) -> bool {
        match self.lock().running.get(&id) {
            Some((token, _)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Pause (`true`) or resume (`false`) a running job. `false` if no such
    /// job is running. A paused job stops at its next safe point, the same
    /// points where it could be cancelled (`CancellationToken::check`), and
    /// waits there; cancelling it, or closing its window, still ends it.
    pub fn set_paused(&self, id: JobId, paused: bool) -> bool {
        match self.lock().running.get(&id) {
            Some((token, _)) => {
                if paused {
                    token.pause();
                } else {
                    token.resume();
                }
                true
            }
            None => false,
        }
    }

    /// Request cancellation of every job a window started (it was closed).
    /// Returns how many were running. Other windows' jobs are untouched.
    pub fn cancel_owned(&self, owner: &str) -> usize {
        let reg = self.lock();
        let mut n = 0;
        for (token, o) in reg.running.values() {
            if o == owner {
                token.cancel();
                n += 1;
            }
        }
        n
    }

    /// Number of jobs a window has running.
    pub fn running_for(&self, owner: &str) -> usize {
        self.lock()
            .running
            .values()
            .filter(|(_, o)| o == owner)
            .count()
    }

    /// Number of jobs still running.
    pub fn running(&self) -> usize {
        self.lock().running.len()
    }

    fn finish(&self, id: JobId) {
        self.lock().running.remove(&id);
    }
}

fn validate(request: &JobRequest, services: &Services) -> Result<(), IpcError> {
    match request {
        JobRequest::CreateArchive { .. }
        | JobRequest::AddToArchive { .. }
        | JobRequest::ExportArchive { .. }
        | JobRequest::DeleteFromArchive { .. }
        | JobRequest::ExtractArchive { .. }
        | JobRequest::RetrieveFound { .. }
        | JobRequest::TestForeign { .. }
        | JobRequest::ExtractForeign { .. }
        | JobRequest::ScanArchive { .. }
        | JobRequest::SetArchiveComment { .. }
        | JobRequest::InstallUpdate { .. } => services.validate(request)?,
        JobRequest::TestArchive { .. }
        | JobRequest::PlanRepair { .. }
        | JobRequest::ApplyRepair { .. }
        | JobRequest::SearchArchive { .. }
        | JobRequest::AddPassphrase { .. }
        | JobRequest::RemovePassphrase { .. }
        | JobRequest::Reencrypt { .. } => services.validate(request)?,
        JobRequest::SelfTest { steps, step_ms } => {
            if !(1..=crate::self_test::MAX_STEPS).contains(steps)
                || *step_ms > crate::self_test::MAX_STEP_MS
            {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "self-test takes 1 to {} steps of at most {} ms",
                        crate::self_test::MAX_STEPS,
                        crate::self_test::MAX_STEP_MS
                    ),
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Forwards `mochi-core` progress as `progress` events. If the listener is
/// gone, cancels the job: nobody can see it any more.
struct ForwardProgress<'a> {
    id: JobId,
    sink: &'a dyn EventSink,
    token: &'a CancellationToken,
    /// The phase and time of the last event sent (see [`MIN_PROGRESS_GAP`]).
    last: Mutex<Option<(&'static str, Instant)>>,
}

/// Progress is a display, not a log: core reports once per object, file or
/// path (tens of thousands for a big archive), and each event crosses the IPC
/// bridge and re-renders the page. Within one phase at most one event goes
/// out per this interval. A phase's first event and its last (`completed`
/// reaching `total`) always go out, so a phase change or completion is never
/// delayed or lost, and the job's `finished` event follows the last one.
const MIN_PROGRESS_GAP: Duration = Duration::from_millis(50);

impl ProgressSink for ForwardProgress<'_> {
    fn report(&self, event: &ProgressEvent) {
        let now = Instant::now();
        {
            let mut last = self.last.lock().unwrap_or_else(|p| p.into_inner());
            let finishes = event.total.is_some_and(|t| event.completed >= t);
            let due = match *last {
                Some((phase, at)) if phase == event.phase => {
                    now.duration_since(at) >= MIN_PROGRESS_GAP
                }
                _ => true,
            };
            if !(due || finishes) {
                return;
            }
            *last = Some((event.phase, now));
        }
        let sent = self.sink.send(JobEvent::Progress {
            job_id: self.id,
            phase: event.phase.to_owned(),
            completed: event.completed,
            total: event.total,
        });
        if !sent {
            self.token.cancel();
        }
    }
}

/// Sends `finished` exactly once: with the job's outcome, or `aborted` if
/// the job unwound before producing one. The job leaves the registry
/// first, so `cancel` after `finished` reports `false`.
struct Finisher<'a> {
    registry: &'a JobRegistry,
    id: JobId,
    sink: &'a dyn EventSink,
    outcome: Option<JobOutcome>,
}

impl Drop for Finisher<'_> {
    fn drop(&mut self) {
        self.registry.finish(self.id);
        let outcome = self.outcome.take().unwrap_or_else(|| JobOutcome::Aborted {
            message: "the job stopped unexpectedly without a result".into(),
        });
        self.sink.send(JobEvent::Finished {
            job_id: self.id,
            outcome,
        });
    }
}

fn run_job(
    registry: &JobRegistry,
    services: &Services,
    id: JobId,
    request: &JobRequest,
    sink: Arc<dyn EventSink>,
    token: &CancellationToken,
) {
    let mut finisher = Finisher {
        registry,
        id,
        sink: sink.as_ref(),
        outcome: None,
    };
    if !sink.send(JobEvent::Started { job_id: id }) {
        token.cancel();
    }
    let progress = ForwardProgress {
        id,
        sink: sink.as_ref(),
        token,
        last: Mutex::new(None),
    };
    let ctx = JobContext {
        progress: &progress,
        cancel: token,
    };
    let result = match request {
        JobRequest::SelfTest { steps, step_ms } => crate::self_test::run(*steps, *step_ms, &ctx)
            .map(|summary| JobDone {
                summary,
                commit: None,
                extract: None,
                foreign_test: None,
                check: None,
                search: None,
                rekey: None,
                export: None,
                scan: None,
                retrieve: None,
            }),
        write => services.run(write, &ctx).unwrap_or_else(|| {
            Err(MochiError::new(
                ErrorCode::NotImplemented,
                "this job kind is not routed",
            ))
        }),
    };
    finisher.outcome = Some(match result {
        Ok(JobDone {
            summary,
            commit,
            extract,
            foreign_test,
            check,
            search,
            rekey,
            export,
            scan,
            retrieve,
        }) => JobOutcome::Completed {
            summary,
            commit,
            extract,
            foreign_test,
            check,
            search,
            rekey,
            export,
            scan,
            retrieve,
        },
        Err(e) if e.code == ErrorCode::Cancelled => JobOutcome::Cancelled,
        Err(e) => JobOutcome::Failed {
            code: e.code.as_str().to_owned(),
            message: e.message,
        },
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Collects events and lets the test wait for them.
    struct Recorder {
        tx: Mutex<mpsc::Sender<JobEvent>>,
        listening: std::sync::atomic::AtomicBool,
    }

    impl EventSink for Recorder {
        fn send(&self, event: JobEvent) -> bool {
            let _ = self.tx.lock().unwrap().send(event);
            self.listening.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn recorder() -> (Arc<Recorder>, mpsc::Receiver<JobEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Recorder {
                tx: Mutex::new(tx),
                listening: std::sync::atomic::AtomicBool::new(true),
            }),
            rx,
        )
    }

    fn thread_spawner() -> Box<Spawner> {
        Box::new(|work| {
            std::thread::spawn(work);
        })
    }

    /// Every event up to and including `finished`.
    fn until_finished(rx: &mpsc::Receiver<JobEvent>) -> Vec<JobEvent> {
        let mut events = Vec::new();
        loop {
            let e = rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let done = matches!(e, JobEvent::Finished { .. });
            events.push(e);
            if done {
                return events;
            }
        }
    }

    #[test]
    fn a_job_reports_started_progress_and_one_finished() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        let id = reg
            .start(
                JobRequest::SelfTest {
                    steps: 4,
                    step_ms: 0,
                },
                sink,
                thread_spawner(),
            )
            .unwrap();
        let events = until_finished(&rx);
        assert_eq!(events[0], JobEvent::Started { job_id: id });
        let progress: Vec<(u64, Option<u64>)> = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress {
                    completed, total, ..
                } => Some((*completed, *total)),
                _ => None,
            })
            .collect();
        // Progress is throttled (`MIN_PROGRESS_GAP`): intermediate steps may
        // be dropped, but the first and the last of the phase never are.
        assert_eq!(progress.first(), Some(&(0, Some(4))), "{progress:?}");
        assert_eq!(progress.last(), Some(&(4, Some(4))), "{progress:?}");
        assert!(
            progress.windows(2).all(|w| w[0].0 <= w[1].0),
            "{progress:?}"
        );
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Completed { .. },
                ..
            })
        ));
        assert_eq!(reg.running(), 0);
        assert!(!reg.cancel(id), "a finished job cannot be cancelled");
        assert!(rx.try_recv().is_err(), "nothing after finished");
    }

    #[test]
    fn a_paused_job_makes_no_progress_until_resumed() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        let id = reg
            .start(
                JobRequest::SelfTest {
                    steps: 20,
                    step_ms: 10,
                },
                sink,
                thread_spawner(),
            )
            .unwrap();
        loop {
            if let JobEvent::Progress { completed: 2.., .. } =
                rx.recv_timeout(Duration::from_secs(20)).unwrap()
            {
                break;
            }
        }
        assert!(reg.set_paused(id, true));
        // At most the step under way finishes; then nothing until resumed.
        std::thread::sleep(Duration::from_millis(100));
        while rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(200));
        assert!(rx.try_recv().is_err(), "a paused job reported progress");
        assert_eq!(reg.running(), 1);
        assert!(reg.set_paused(id, false));
        let rest = until_finished(&rx);
        assert!(matches!(
            rest.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Completed { .. },
                ..
            })
        ));
        assert!(!reg.set_paused(id, true), "a finished job cannot be paused");
    }

    #[test]
    fn cancelling_a_paused_job_reports_cancelled() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        let id = reg
            .start(
                JobRequest::SelfTest {
                    steps: 1000,
                    step_ms: 5,
                },
                sink,
                thread_spawner(),
            )
            .unwrap();
        assert!(reg.set_paused(id, true));
        std::thread::sleep(Duration::from_millis(50));
        assert!(reg.cancel(id));
        let rest = until_finished(&rx);
        let JobEvent::Finished { outcome, .. } = rest.last().unwrap() else {
            panic!("no finished event")
        };
        assert_eq!(*outcome, JobOutcome::Cancelled);
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn cancellation_stops_the_job_and_reports_cancelled() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        let id = reg
            .start(
                JobRequest::SelfTest {
                    steps: 1000,
                    step_ms: 20,
                },
                sink,
                thread_spawner(),
            )
            .unwrap();
        // Wait for real progress, then cancel.
        loop {
            if let JobEvent::Progress { completed: 2.., .. } =
                rx.recv_timeout(Duration::from_secs(20)).unwrap()
            {
                break;
            }
        }
        assert!(reg.cancel(id));
        let rest = until_finished(&rx);
        let JobEvent::Finished { outcome, .. } = rest.last().unwrap() else {
            panic!("no finished event")
        };
        assert_eq!(*outcome, JobOutcome::Cancelled);
        let last_step = rest
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress { completed, .. } => Some(*completed),
                _ => None,
            })
            .max()
            .unwrap_or(2);
        assert!(last_step < 1000, "stopped early, at step {last_step}");
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn a_flood_of_progress_is_thinned_but_phase_edges_survive() {
        let (sink, rx) = recorder();
        let token = CancellationToken::new();
        let progress = ForwardProgress {
            id: JobId::default(),
            sink: sink.as_ref(),
            token: &token,
            last: Mutex::new(None),
        };
        let ctx = JobContext {
            progress: &progress,
            cancel: &token,
        };
        // 10 000 objects in a tight loop, then a second phase.
        for i in 0..=10_000u64 {
            ctx.report("verify-objects", i, Some(10_000));
        }
        ctx.report("verify-files", 0, Some(2));
        ctx.report("verify-files", 1, Some(2));
        ctx.report("verify-files", 2, Some(2));
        let sent: Vec<(String, u64)> = rx
            .try_iter()
            .filter_map(|e| match e {
                JobEvent::Progress {
                    phase, completed, ..
                } => Some((phase, completed)),
                _ => None,
            })
            .collect();
        assert!(sent.len() < 100, "{} events for 10 000 reports", sent.len());
        assert_eq!(sent[0], ("verify-objects".to_owned(), 0), "phase start");
        assert!(
            sent.contains(&("verify-objects".to_owned(), 10_000)),
            "phase end"
        );
        assert!(
            sent.contains(&("verify-files".to_owned(), 0)),
            "next phase start"
        );
        assert_eq!(sent.last(), Some(&("verify-files".to_owned(), 2)));
        assert!(!token.is_cancelled());
    }

    #[test]
    fn a_vanished_listener_cancels_the_job() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        sink.listening
            .store(false, std::sync::atomic::Ordering::SeqCst);
        reg.start(
            JobRequest::SelfTest {
                steps: 1000,
                step_ms: 20,
            },
            sink,
            thread_spawner(),
        )
        .unwrap();
        let events = until_finished(&rx);
        let JobEvent::Finished { outcome, .. } = events.last().unwrap() else {
            panic!("no finished event")
        };
        assert_eq!(*outcome, JobOutcome::Cancelled);
        assert!(events.len() < 10, "{} events", events.len());
    }

    #[test]
    fn invalid_requests_are_refused_before_anything_starts() {
        let reg = Arc::new(JobRegistry::default());
        for request in [
            JobRequest::SelfTest {
                steps: 0,
                step_ms: 1,
            },
            JobRequest::SelfTest {
                steps: crate::self_test::MAX_STEPS + 1,
                step_ms: 1,
            },
            JobRequest::SelfTest {
                steps: 1,
                step_ms: crate::self_test::MAX_STEP_MS + 1,
            },
        ] {
            let (sink, rx) = recorder();
            let e = reg
                .start(
                    request.clone(),
                    sink,
                    Box::new(|_| panic!("must not spawn")),
                )
                .unwrap_err();
            assert_eq!(e.code, "INVALID_ARGUMENT", "{request:?}");
            assert!(rx.try_recv().is_err(), "no events for a refused job");
        }
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn an_unwinding_job_reports_aborted() {
        let reg = Arc::new(JobRegistry::default());
        let (sink, rx) = recorder();
        // Unwind inside the job's thread after it has started.
        let id = {
            let mut r = reg.lock();
            r.last_id += 1;
            let id = r.last_id;
            r.running
                .insert(id, (CancellationToken::new(), String::new()));
            id
        };
        let sink2: Arc<dyn EventSink> = sink;
        let reg2 = Arc::clone(&reg);
        let handle = std::thread::spawn(move || {
            let _finisher = Finisher {
                registry: &reg2,
                id,
                sink: sink2.as_ref(),
                outcome: None,
            };
            panic!("simulated fault");
        });
        assert!(handle.join().is_err());
        let events = until_finished(&rx);
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Aborted { .. },
                ..
            })
        ));
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn ids_are_unique() {
        let reg = Arc::new(JobRegistry::default());
        let mut ids = std::collections::BTreeSet::new();
        for _ in 0..20 {
            let (sink, _rx) = recorder();
            let id = reg
                .start(
                    JobRequest::SelfTest {
                        steps: 1,
                        step_ms: 0,
                    },
                    sink,
                    thread_spawner(),
                )
                .unwrap();
            assert!(ids.insert(id));
        }
    }
}
