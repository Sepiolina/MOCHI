//! Parent side: spawn the helper, supervise it, and turn everything that can go
//! wrong with it into a typed [`ForeignError`]. A crash, panic, hang, flood, or
//! protocol violation in the helper never takes the caller down.
//!
//! The helper is treated as untrusted too: frame sizes, counts, and ordering
//! are validated here, not assumed.

use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use crate::protocol::{
    kind, path_to_bytes, read_frame, Done, FrameError, Message, Request, PROTOCOL_VERSION,
};
use crate::types::*;

/// Limits on the helper *process* (as opposed to [`Limits`], which bound what
/// it produces from an archive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunLimits {
    /// Wall-clock time for one operation; the helper is killed after this.
    pub wall_time: Duration,
    /// Memory limit for the helper. On Unix the helper applies it to itself
    /// (`RLIMIT_AS`); on Windows the parent puts it in a Job Object
    /// (`jobobject.rs`). `memory_limit_enforced()` says whether it is applied.
    pub memory_bytes: Option<u64>,
    /// Total non-data bytes (entry listings, results) accepted from the helper.
    pub max_metadata_bytes: u64,
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            wall_time: Duration::from_secs(600),
            memory_bytes: Some(2 << 30),
            max_metadata_bytes: 256 << 20,
        }
    }
}

impl RunLimits {
    /// Whether `memory_bytes` is actually applied on this platform.
    pub const fn memory_limit_enforced() -> bool {
        cfg!(any(unix, windows))
    }
}

/// Where the helper binary lives by default: next to the running executable.
pub fn default_helper_path() -> PathBuf {
    let name = if cfg!(windows) {
        "mochi-foreign-helper.exe"
    } else {
        "mochi-foreign-helper"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

#[derive(Debug)]
pub struct ListReport {
    pub format: ForeignFormat,
    pub entries: Vec<EntryInfo>,
    pub breaches: Vec<Breach>,
    /// False when listing stopped early (limit, truncation); `stop_reason`
    /// says why. A partial listing is labelled partial, never complete.
    pub complete: bool,
    pub stop_reason: Option<String>,
    /// tar.gz only: the gzip trailer check, reached because listing reads the
    /// whole stream.
    pub stream_check: StreamCheck,
    pub dimensions: ForeignDimensions,
}

#[derive(Debug)]
pub struct TestEntry {
    pub info: EntryInfo,
    pub outcome: EntryOutcome,
}

#[derive(Debug)]
pub struct TestReport {
    pub format: ForeignFormat,
    pub entries: Vec<TestEntry>,
    pub breaches: Vec<Breach>,
    pub complete: bool,
    pub stop_reason: Option<String>,
    /// For `.tar.gz`, a `Failed` stream check means no entry is vouched for,
    /// whatever its own outcome says.
    pub stream_check: StreamCheck,
    pub bytes_read: u64,
    pub dimensions: ForeignDimensions,
}

impl TestReport {
    /// Entries whose outcome is not a clean read, plus a failed stream.
    pub fn problems(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.outcome.is_clean())
            .count()
            + usize::from(matches!(self.stream_check, StreamCheck::Failed(_)))
    }
}

#[derive(Debug)]
pub struct ReadReport {
    pub outcome: EntryOutcome,
    pub bytes_delivered: u64,
    pub breaches: Vec<Breach>,
    pub stream_check: StreamCheck,
}

/// What [`ForeignClient::read_many`] reports besides what it gave the sink.
#[derive(Debug)]
pub struct ReadManyReport {
    pub format: ForeignFormat,
    /// Entries the helper delivered (each requested entry exactly once).
    pub delivered: u64,
    pub breaches: Vec<Breach>,
    pub complete: bool,
    pub stop_reason: Option<String>,
    /// For `.tar.gz`, a failed stream check vouches for no entry.
    pub stream_check: StreamCheck,
}

/// Receives the entries of [`ForeignClient::read_many`], one at a time.
pub trait ReadManySink {
    /// An entry is about to be delivered.
    fn begin(&mut self, info: &EntryInfo) -> io::Result<()>;
    /// Bytes of the current entry.
    fn data(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// The current entry ended with `outcome`. Anything other than a clean
    /// outcome means the bytes just delivered must be discarded.
    fn finish(&mut self, info: &EntryInfo, outcome: &EntryOutcome) -> io::Result<()>;
}

#[derive(Clone)]
pub struct ForeignClient {
    helper: PathBuf,
    run: RunLimits,
    limits: Limits,
    /// Set by the caller to stop the operation: the helper is killed.
    cancel: Option<Arc<AtomicBool>>,
}

/// How the exchange with the helper ended, before it is mapped to an error.
enum DriveEnd {
    Finished,
    /// The helper (or a violation we detected) ended the exchange with this.
    Failed(ForeignError),
    /// stdout closed or broke before `Done`.
    StreamEnded,
    OutputLimit,
}

fn lock(c: &Mutex<Child>) -> MutexGuard<'_, Child> {
    c.lock().unwrap_or_else(PoisonError::into_inner)
}

fn reap(child: &Mutex<Child>, kill: bool) -> Option<ExitStatus> {
    if kill {
        let _ = lock(child).kill();
    }
    loop {
        match lock(child).try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => {}
            Err(_) => return None,
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn describe_exit(status: Option<ExitStatus>, memory_limited: bool) -> String {
    let Some(s) = status else {
        return "the helper's exit status could not be read".to_string();
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = s.signal() {
            let name = match sig {
                6 => "SIGABRT",
                9 => "SIGKILL",
                11 => "SIGSEGV",
                24 => "SIGXCPU",
                _ => "",
            };
            let mut t = format!("the helper was terminated by signal {sig}");
            if !name.is_empty() {
                t.push_str(&format!(" ({name})"));
            }
            if memory_limited && matches!(sig, 6 | 9 | 11) {
                t.push_str("; a memory-limit breach is the likely cause");
            }
            if sig == 24 {
                t.push_str("; the CPU-time backstop was reached");
            }
            return t;
        }
    }
    // On Windows a Job Object memory breach makes the helper's allocation fail
    // and Rust abort it: a non-zero status with no signal to name.
    let _ = memory_limited;
    #[cfg(windows)]
    if memory_limited && s.code().is_some_and(|c| c != 0) {
        return format!(
            "the helper exited with status {}; a memory-limit breach is a possible cause",
            s.code().unwrap_or(-1)
        );
    }
    match s.code() {
        Some(c) => format!("the helper exited with status {c}"),
        None => "the helper ended abnormally".to_string(),
    }
}

impl std::fmt::Debug for ForeignClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignClient")
            .field("helper", &self.helper)
            .field("run", &self.run)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ForeignClient {
    pub fn new(helper: impl Into<PathBuf>) -> Self {
        ForeignClient {
            helper: helper.into(),
            run: RunLimits::default(),
            limits: Limits::default(),
            cancel: None,
        }
    }

    /// Stop the helper (and fail with `CANCELLED`) soon after `flag` is set.
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_run_limits(mut self, run: RunLimits) -> Self {
        self.run = run;
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// List entries. Reads no entry data (except through a `.tar.gz`'s
    /// decompressor, see [`ListReport::stream_check`]).
    pub fn list(
        &self,
        archive: &Path,
        password: Option<&Password>,
    ) -> Result<ListReport, ForeignError> {
        let mut c = Collector::new(Op::List, self.limits);
        self.exchange(archive, Op::List, 0, Vec::new(), password, &mut |m| {
            c.push(m)
        })?;
        c.into_list()
    }

    /// Stream every entry through its decompressor, run the format's own
    /// check, discard the bytes.
    pub fn test(
        &self,
        archive: &Path,
        password: Option<&Password>,
    ) -> Result<TestReport, ForeignError> {
        self.test_with_progress(archive, password, &mut |_| {})
    }

    /// [`test`](Self::test), telling `progress` how many entries have
    /// finished so far (called on this thread, once per entry).
    pub fn test_with_progress(
        &self,
        archive: &Path,
        password: Option<&Password>,
        progress: &mut dyn FnMut(u64),
    ) -> Result<TestReport, ForeignError> {
        let mut c = Collector::new(Op::Test, self.limits);
        let mut finished = 0u64;
        self.exchange(archive, Op::Test, 0, Vec::new(), password, &mut |m| {
            if matches!(m, Message::Result { .. }) {
                finished += 1;
                progress(finished);
            }
            c.push(m)
        })?;
        c.into_test()
    }

    /// Stream one entry's bytes to `sink`. Encrypted ZIP entries are delivered
    /// only after their authentication passed; for any other entry, bytes
    /// already delivered must be discarded when the outcome is not clean.
    pub fn read_entry(
        &self,
        archive: &Path,
        index: u64,
        password: Option<&Password>,
        sink: &mut dyn FnMut(&[u8]) -> io::Result<()>,
    ) -> Result<ReadReport, ForeignError> {
        let mut c = Collector::new(Op::Read, self.limits);
        self.exchange(archive, Op::Read, index, Vec::new(), password, &mut |m| {
            if let Message::Data(b) = &m {
                sink(b).map_err(|e| {
                    ForeignError::new(ErrorCode::SinkFailed, format!("the byte sink failed: {e}"))
                })?;
            }
            c.push(m)
        })?;
        c.into_read()
    }

    /// Stream the named entries in **one** helper run, so a solid archive is
    /// decoded once rather than once per entry. The helper delivers entries in
    /// the order the archive stores them (not the order of `indices`); `sink`
    /// is told when each begins, receives its bytes, and is told how it ended.
    /// Bytes delivered for an entry whose outcome is not clean must be
    /// discarded by the sink (the sink's `finish` is where it learns that).
    /// Encrypted entries are delivered only after their authentication passed.
    pub fn read_many(
        &self,
        archive: &Path,
        indices: &[u64],
        password: Option<&Password>,
        sink: &mut dyn ReadManySink,
    ) -> Result<ReadManyReport, ForeignError> {
        let mut want: Vec<u64> = indices.to_vec();
        want.sort_unstable();
        want.dedup();
        if want.len() > crate::protocol::MAX_READ_INDICES {
            return Err(ForeignError::new(
                ErrorCode::BadRequest,
                "too many entries requested in one read",
            ));
        }
        let mut c = Collector::new_many(self.limits, want.clone());
        let sink_failed = |e: io::Error| {
            ForeignError::new(ErrorCode::SinkFailed, format!("the byte sink failed: {e}"))
        };
        self.exchange(archive, Op::ReadMany, 0, want, password, &mut |m| {
            match &m {
                Message::Entry(info) => {
                    c.push(m.clone())?;
                    return sink.begin(info).map_err(sink_failed);
                }
                Message::Data(b) => {
                    c.push(m.clone())?;
                    return sink.data(b).map_err(sink_failed);
                }
                Message::Result { index, outcome } => {
                    let info = c.current_entry().cloned();
                    c.push(m.clone())?;
                    return match info {
                        Some(info) if info.index == *index => {
                            sink.finish(&info, outcome).map_err(sink_failed)
                        }
                        _ => Err(Collector::bad("result does not match the pending entry")),
                    };
                }
                _ => {}
            }
            c.push(m)
        })?;
        c.into_read_many()
    }

    fn exchange(
        &self,
        archive: &Path,
        op: Op,
        read_index: u64,
        read_indices: Vec<u64>,
        password: Option<&Password>,
        on_msg: &mut dyn FnMut(Message) -> Result<(), ForeignError>,
    ) -> Result<(), ForeignError> {
        let memory_limit = if RunLimits::memory_limit_enforced() {
            self.run.memory_bytes
        } else {
            None
        };
        let req = Request {
            op,
            read_index,
            read_indices,
            limits: self.limits,
            memory_limit,
            cpu_seconds: Some(self.run.wall_time.as_secs().saturating_add(5)),
            archive_path: path_to_bytes(archive),
            password: password.cloned(),
        };
        // On Windows the limit is the parent's to apply (the helper has no
        // RLIMIT_AS), so the request does not carry it.
        #[cfg(windows)]
        let req = Request {
            memory_limit: None,
            ..req
        };
        let mut cmd = Command::new(&self.helper);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // The helper is a console-subsystem binary; without this flag every
        // spawn from the (windowless) desktop app flashes a console window.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| {
            ForeignError::new(
                ErrorCode::HelperSpawnFailed,
                format!("cannot start the helper: {}", e.kind()),
            )
        })?;
        // Confine the helper before it is sent anything. A limit that was
        // asked for and cannot be applied is an error, never a silent skip.
        #[cfg(windows)]
        let _job = match memory_limit {
            None => None,
            Some(bytes) => {
                let applied = crate::jobobject::Job::with_memory_limit(bytes).and_then(|job| {
                    job.assign(&child)?;
                    Ok(job)
                });
                match applied {
                    Ok(job) => Some(job),
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ForeignError::new(
                            ErrorCode::HelperSpawnFailed,
                            format!("cannot apply the helper's memory limit: {e}"),
                        ));
                    }
                }
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ForeignError::new(
                ErrorCode::HelperSpawnFailed,
                "the helper's pipes are unavailable",
            ));
        };
        let child = Arc::new(Mutex::new(child));
        let timed_out = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = {
            let child = Arc::clone(&child);
            let timed_out = Arc::clone(&timed_out);
            let cancelled = Arc::clone(&cancelled);
            let cancel = self.cancel.clone();
            let wall = self.run.wall_time;
            thread::spawn(move || {
                let start = std::time::Instant::now();
                loop {
                    let slice = Duration::from_millis(25);
                    match done_rx.recv_timeout(slice) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                        cancelled.store(true, Ordering::SeqCst);
                        let _ = lock(&child).kill();
                        return;
                    }
                    if start.elapsed() >= wall {
                        timed_out.store(true, Ordering::SeqCst);
                        let _ = lock(&child).kill();
                        return;
                    }
                }
            })
        };
        let end = self.drive(stdin, BufReader::new(stdout), &req, on_msg);

        let result = match end {
            DriveEnd::Finished => {
                // stdin was dropped inside `drive`; the helper exits on its own.
                let status = reap(&child, false);
                if timed_out.load(Ordering::SeqCst) {
                    Err(self.timeout_error())
                } else if status.is_some_and(|s| s.success()) {
                    Ok(())
                } else {
                    Err(ForeignError::new(
                        ErrorCode::HelperCrashed,
                        format!(
                            "{} after reporting completion",
                            describe_exit(status, memory_limit.is_some())
                        ),
                    ))
                }
            }
            DriveEnd::Failed(e) => {
                let _ = reap(&child, true);
                Err(e)
            }
            DriveEnd::OutputLimit => {
                let _ = reap(&child, true);
                Err(ForeignError::new(
                    ErrorCode::HelperOutputLimit,
                    format!(
                        "the helper produced more than {} bytes of listing data and was stopped",
                        self.run.max_metadata_bytes
                    ),
                ))
            }
            DriveEnd::StreamEnded => {
                let status = reap(&child, true);
                if timed_out.load(Ordering::SeqCst) {
                    Err(self.timeout_error())
                } else if status.is_some_and(|s| s.success()) {
                    Err(ForeignError::new(
                        ErrorCode::HelperProtocol,
                        "the helper exited without completing the operation",
                    ))
                } else {
                    Err(ForeignError::new(
                        ErrorCode::HelperCrashed,
                        describe_exit(status, memory_limit.is_some()),
                    ))
                }
            }
        };
        let _ = done_tx.send(());
        let _ = watchdog.join();
        if cancelled.load(Ordering::SeqCst) {
            return Err(ForeignError::new(
                ErrorCode::Cancelled,
                "the operation was cancelled and the helper was stopped",
            ));
        }
        result
    }

    fn timeout_error(&self) -> ForeignError {
        ForeignError::new(
            ErrorCode::HelperTimeout,
            format!(
                "the helper did not finish within {} s and was stopped",
                self.run.wall_time.as_secs()
            ),
        )
    }

    fn drive(
        &self,
        mut stdin: impl Write,
        mut stdout: impl io::Read,
        req: &Request,
        on_msg: &mut dyn FnMut(Message) -> Result<(), ForeignError>,
    ) -> DriveEnd {
        let proto = |m: &str| DriveEnd::Failed(ForeignError::new(ErrorCode::HelperProtocol, m));
        // Hello first, so a version mismatch is caught before any request.
        match read_frame(&mut stdout) {
            Ok(Some((kind::HELLO, body))) => match Message::decode(kind::HELLO, &body) {
                Ok(Message::Hello { version, .. }) if version == PROTOCOL_VERSION => {}
                Ok(Message::Hello { version, .. }) => {
                    return DriveEnd::Failed(ForeignError::new(
                        ErrorCode::HelperProtocol,
                        format!("the helper speaks protocol {version}, this client speaks {PROTOCOL_VERSION}"),
                    ))
                }
                _ => return proto("malformed hello"),
            },
            Ok(Some(_)) => return proto("the first frame was not a hello"),
            Ok(None) | Err(FrameError::Io(_)) => return DriveEnd::StreamEnded,
            Err(FrameError::Protocol(e)) => return proto(e.0),
        }
        let body = req.encode();
        if crate::protocol::write_frame(&mut stdin, kind::REQUEST, &body)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return DriveEnd::StreamEnded;
        }
        drop(stdin);

        let mut meta_bytes: u64 = 0;
        let mut data_bytes: u64 = 0;
        let mut total_data: u64 = 0;
        loop {
            let (k, body) = match read_frame(&mut stdout) {
                Ok(Some(f)) => f,
                Ok(None) | Err(FrameError::Io(_)) => return DriveEnd::StreamEnded,
                Err(FrameError::Protocol(e)) => return proto(e.0),
            };
            let frame_bytes = body.len() as u64 + 5;
            if k == kind::DATA {
                data_bytes = data_bytes.saturating_add(body.len() as u64);
                total_data = total_data.saturating_add(body.len() as u64);
                if !req.op.is_read() {
                    return proto("entry data outside a read operation");
                }
                if data_bytes > self.limits.max_entry_size
                    || total_data > self.limits.max_total_output
                {
                    return DriveEnd::OutputLimit;
                }
            } else {
                if k == kind::ENTRY {
                    data_bytes = 0;
                }
                meta_bytes = meta_bytes.saturating_add(frame_bytes);
                if meta_bytes > self.run.max_metadata_bytes {
                    return DriveEnd::OutputLimit;
                }
            }
            let msg = match Message::decode(k, &body) {
                Ok(m) => m,
                Err(e) => return proto(e.0),
            };
            match msg {
                Message::Hello { .. } => return proto("a second hello"),
                Message::Error { code, message } => {
                    return DriveEnd::Failed(ForeignError::new(code, message))
                }
                Message::Done(d) => {
                    return match on_msg(Message::Done(d)) {
                        Ok(()) => DriveEnd::Finished,
                        Err(e) => DriveEnd::Failed(e),
                    };
                }
                other => {
                    if let Err(e) = on_msg(other) {
                        return DriveEnd::Failed(e);
                    }
                }
            }
        }
    }
}

/// Validates ordering and counts, and assembles the reports.
struct Collector {
    op: Op,
    limits: Limits,
    format: Option<ForeignFormat>,
    entries: Vec<TestEntry>,
    pending_result: bool,
    breaches: Vec<Breach>,
    stream_check: Option<StreamCheck>,
    read_outcome: Option<EntryOutcome>,
    data_bytes: u64,
    done: Option<Done>,
    /// `ReadMany`: the indices asked for, sorted; and the ones delivered.
    wanted: Vec<u64>,
    delivered: Vec<u64>,
    current: Option<EntryInfo>,
}

impl Collector {
    fn new(op: Op, limits: Limits) -> Self {
        Collector {
            op,
            limits,
            format: None,
            entries: Vec::new(),
            pending_result: false,
            breaches: Vec::new(),
            stream_check: None,
            read_outcome: None,
            data_bytes: 0,
            done: None,
            wanted: Vec::new(),
            delivered: Vec::new(),
            current: None,
        }
    }

    fn new_many(limits: Limits, wanted: Vec<u64>) -> Self {
        let mut c = Collector::new(Op::ReadMany, limits);
        c.wanted = wanted;
        c
    }

    fn current_entry(&self) -> Option<&EntryInfo> {
        self.current.as_ref()
    }

    fn bad(m: &str) -> ForeignError {
        ForeignError::new(ErrorCode::HelperProtocol, m)
    }

    fn push(&mut self, m: Message) -> Result<(), ForeignError> {
        if self.done.is_some() {
            return Err(Self::bad("message after done"));
        }
        match m {
            Message::Format(f) => {
                if self.format.replace(f).is_some() {
                    return Err(Self::bad("format announced twice"));
                }
            }
            _ if self.format.is_none() => return Err(Self::bad("message before format")),
            Message::Entry(info) if self.op == Op::ReadMany => {
                if self.current.is_some() {
                    return Err(Self::bad("entry before the previous result"));
                }
                if self.wanted.binary_search(&info.index).is_err() {
                    return Err(Self::bad("an entry that was not requested"));
                }
                if self.delivered.contains(&info.index) {
                    return Err(Self::bad("an entry delivered twice"));
                }
                if info.name.len() as u64 > u64::from(self.limits.max_name_len) {
                    return Err(Self::bad("entry name over the limit"));
                }
                self.delivered.push(info.index);
                self.current = Some(info);
            }
            Message::Entry(info) => {
                if self.op == Op::Read {
                    return Err(Self::bad("entry listing during a read"));
                }
                if self.pending_result {
                    return Err(Self::bad("entry before the previous result"));
                }
                if info.index != self.entries.len() as u64 {
                    return Err(Self::bad("entry index out of sequence"));
                }
                if self.entries.len() as u64 >= self.limits.max_entries {
                    return Err(Self::bad("more entries than the limit allows"));
                }
                if info.name.len() as u64 > u64::from(self.limits.max_name_len) {
                    return Err(Self::bad("entry name over the limit"));
                }
                self.pending_result = self.op == Op::Test;
                self.entries.push(TestEntry {
                    info,
                    outcome: EntryOutcome::NotRun,
                });
            }
            Message::Result { index, outcome } => match self.op {
                Op::Test => {
                    let Some(last) = self.entries.last_mut() else {
                        return Err(Self::bad("result without an entry"));
                    };
                    if !self.pending_result || last.info.index != index {
                        return Err(Self::bad("result does not match the pending entry"));
                    }
                    last.outcome = outcome;
                    self.pending_result = false;
                }
                Op::Read => {
                    if self.read_outcome.replace(outcome).is_some() {
                        return Err(Self::bad("two results for one read"));
                    }
                }
                Op::ReadMany => match self.current.take() {
                    Some(e) if e.index == index => {}
                    _ => return Err(Self::bad("result does not match the pending entry")),
                },
                Op::List => return Err(Self::bad("result during a listing")),
            },
            Message::Data(b) => {
                if !self.op.is_read() {
                    return Err(Self::bad("data outside a read"));
                }
                if self.op == Op::ReadMany && self.current.is_none() {
                    return Err(Self::bad("data outside an entry"));
                }
                self.data_bytes = self.data_bytes.saturating_add(b.len() as u64);
            }
            Message::StreamCheck(s) => {
                if self.stream_check.replace(s).is_some() {
                    return Err(Self::bad("stream check twice"));
                }
            }
            Message::Breach(b) => {
                if self.breaches.len() < 100_000 {
                    self.breaches.push(b);
                }
            }
            Message::Done(d) => self.done = Some(d),
            Message::Hello { .. } | Message::Error { .. } => {
                return Err(Self::bad("unexpected message"))
            }
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(ForeignFormat, Done, StreamCheck), ForeignError> {
        let format = self
            .format
            .ok_or_else(|| Self::bad("done without a format"))?;
        let done = self
            .done
            .take()
            .ok_or_else(|| Self::bad("no done message"))?;
        Ok((
            format,
            done,
            self.stream_check.take().unwrap_or(StreamCheck::NotReached),
        ))
    }

    fn into_list(mut self) -> Result<ListReport, ForeignError> {
        let (format, done, stream_check) = self.finish()?;
        Ok(ListReport {
            format,
            entries: self.entries.into_iter().map(|e| e.info).collect(),
            breaches: self.breaches,
            complete: done.complete,
            stop_reason: done.stop_reason,
            stream_check,
            dimensions: FOREIGN_DIMENSIONS,
        })
    }

    fn into_test(mut self) -> Result<TestReport, ForeignError> {
        let (format, done, stream_check) = self.finish()?;
        Ok(TestReport {
            format,
            entries: self.entries,
            breaches: self.breaches,
            complete: done.complete,
            stop_reason: done.stop_reason,
            stream_check,
            bytes_read: done.bytes_out,
            dimensions: FOREIGN_DIMENSIONS,
        })
    }

    fn into_read_many(mut self) -> Result<ReadManyReport, ForeignError> {
        let (format, done, stream_check) = self.finish()?;
        if self.current.is_some() {
            return Err(Self::bad("an entry without a result"));
        }
        if done.complete && self.delivered.len() != self.wanted.len() {
            return Err(Self::bad("fewer entries delivered than requested"));
        }
        Ok(ReadManyReport {
            format,
            delivered: self.delivered.len() as u64,
            breaches: self.breaches,
            complete: done.complete,
            stop_reason: done.stop_reason,
            stream_check,
        })
    }

    fn into_read(mut self) -> Result<ReadReport, ForeignError> {
        let (_, _, stream_check) = self.finish()?;
        let outcome = self
            .read_outcome
            .take()
            .ok_or_else(|| Self::bad("no result for the entry"))?;
        Ok(ReadReport {
            outcome,
            bytes_delivered: self.data_bytes,
            breaches: self.breaches,
            stream_check,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! The collector is the parent's check on a helper it does not trust:
    //! ordering and counts are validated, not assumed.
    use super::*;

    fn entry(index: u64) -> EntryInfo {
        EntryInfo {
            index,
            name: b"n".to_vec(),
            kind: EntryKind::File,
            size: Some(1),
            compressed_size: None,
            encryption: Encryption::None,
            method: "tar".into(),
            link_target: None,
            unix_mode: None,
            mtime_unix: None,
            name_is_utf8_flag: None,
            error: None,
        }
    }

    fn done() -> Message {
        Message::Done(Done {
            complete: true,
            entries: 0,
            bytes_out: 0,
            stop_reason: None,
        })
    }

    fn rejects(op: Op, msgs: Vec<Message>) -> bool {
        let mut c = Collector::new(op, Limits::default());
        msgs.into_iter().any(|m| c.push(m).is_err())
    }

    fn fmt() -> Message {
        Message::Format(ForeignFormat::Tar)
    }

    #[test]
    fn a_well_ordered_test_run_is_accepted() {
        let mut c = Collector::new(Op::Test, Limits::default());
        for m in [
            fmt(),
            Message::Entry(entry(0)),
            Message::Result {
                index: 0,
                outcome: EntryOutcome::NoEntryCheck,
            },
            Message::Entry(entry(1)),
            Message::Result {
                index: 1,
                outcome: EntryOutcome::NoData,
            },
            Message::StreamCheck(StreamCheck::NotApplicable),
            done(),
        ] {
            c.push(m).unwrap();
        }
        assert_eq!(c.into_test().unwrap().entries.len(), 2);
    }

    #[test]
    fn messages_before_the_format_or_a_second_format_are_rejected() {
        assert!(rejects(Op::List, vec![Message::Entry(entry(0))]));
        assert!(rejects(Op::List, vec![fmt(), fmt()]));
    }

    #[test]
    fn entry_indices_must_be_sequential() {
        assert!(rejects(Op::List, vec![fmt(), Message::Entry(entry(1))]));
        assert!(rejects(
            Op::List,
            vec![fmt(), Message::Entry(entry(0)), Message::Entry(entry(2))]
        ));
    }

    #[test]
    fn results_must_match_the_pending_entry() {
        let r = |i| Message::Result {
            index: i,
            outcome: EntryOutcome::NoData,
        };
        assert!(
            rejects(Op::Test, vec![fmt(), r(0)]),
            "result without an entry"
        );
        assert!(
            rejects(Op::Test, vec![fmt(), Message::Entry(entry(0)), r(5)]),
            "wrong index"
        );
        assert!(
            rejects(Op::Test, vec![fmt(), Message::Entry(entry(0)), r(0), r(0)]),
            "second result"
        );
        assert!(
            rejects(
                Op::Test,
                vec![fmt(), Message::Entry(entry(0)), Message::Entry(entry(1))]
            ),
            "entry before the previous result"
        );
        assert!(
            rejects(Op::List, vec![fmt(), r(0)]),
            "results do not belong in a listing"
        );
    }

    #[test]
    fn data_only_belongs_to_a_read() {
        assert!(rejects(Op::List, vec![fmt(), Message::Data(vec![1])]));
        assert!(rejects(Op::Test, vec![fmt(), Message::Data(vec![1])]));
        let mut c = Collector::new(Op::Read, Limits::default());
        c.push(fmt()).unwrap();
        c.push(Message::Data(vec![1, 2])).unwrap();
        c.push(Message::Result {
            index: 0,
            outcome: EntryOutcome::NoEntryCheck,
        })
        .unwrap();
        c.push(done()).unwrap();
        assert_eq!(c.into_read().unwrap().bytes_delivered, 2);
        assert!(
            rejects(Op::Read, vec![fmt(), Message::Entry(entry(0))]),
            "no listing during a read"
        );
    }

    #[test]
    fn nothing_may_follow_done_and_a_read_needs_its_result() {
        assert!(rejects(
            Op::List,
            vec![
                fmt(),
                done(),
                Message::Breach(Breach {
                    kind: BreachKind::Ratio,
                    entry: None,
                    limit: 1,
                    observed: 2
                })
            ]
        ));
        let mut c = Collector::new(Op::Read, Limits::default());
        c.push(fmt()).unwrap();
        c.push(done()).unwrap();
        assert!(
            c.into_read().is_err(),
            "a read without a result is a protocol error"
        );
    }

    #[test]
    fn the_entry_count_and_name_limits_bind_the_helper_too() {
        let mut c = Collector::new(
            Op::List,
            Limits {
                max_entries: 2,
                ..Limits::default()
            },
        );
        c.push(fmt()).unwrap();
        c.push(Message::Entry(entry(0))).unwrap();
        c.push(Message::Entry(entry(1))).unwrap();
        assert!(c.push(Message::Entry(entry(2))).is_err());
        let mut long = entry(0);
        long.name = vec![b'a'; 11];
        let mut c = Collector::new(
            Op::List,
            Limits {
                max_name_len: 10,
                ..Limits::default()
            },
        );
        c.push(fmt()).unwrap();
        assert!(c.push(Message::Entry(long)).is_err());
    }

    #[test]
    fn a_stream_check_twice_or_a_missing_done_is_rejected() {
        assert!(rejects(
            Op::List,
            vec![
                fmt(),
                Message::StreamCheck(StreamCheck::NotApplicable),
                Message::StreamCheck(StreamCheck::NotApplicable)
            ]
        ));
        let mut c = Collector::new(Op::List, Limits::default());
        c.push(fmt()).unwrap();
        assert!(c.into_list().is_err());
    }

    fn many(wanted: &[u64]) -> Collector {
        Collector::new_many(Limits::default(), wanted.to_vec())
    }

    fn result(i: u64) -> Message {
        Message::Result {
            index: i,
            outcome: EntryOutcome::NoEntryCheck,
        }
    }

    #[test]
    fn read_many_accepts_any_order_of_the_requested_set() {
        // Blocks of a 7z archive are decoded in an order of their own.
        let mut c = many(&[1, 4, 7]);
        for m in [
            fmt(),
            Message::Entry(entry(7)),
            Message::Data(vec![1]),
            result(7),
            Message::Entry(entry(1)),
            result(1),
            Message::Entry(entry(4)),
            Message::Data(vec![2, 3]),
            result(4),
            Message::StreamCheck(StreamCheck::NotApplicable),
            done(),
        ] {
            c.push(m).unwrap();
        }
        assert_eq!(c.into_read_many().unwrap().delivered, 3);
    }

    #[test]
    fn read_many_rejects_what_the_parent_did_not_ask_for_or_got_twice() {
        let ok = |m: Vec<Message>| !rejects_many(&[1, 4], m);
        assert!(!ok(vec![fmt(), Message::Entry(entry(2))]), "not requested");
        assert!(
            !ok(vec![
                fmt(),
                Message::Entry(entry(1)),
                result(1),
                Message::Entry(entry(1))
            ]),
            "twice"
        );
        assert!(
            !ok(vec![
                fmt(),
                Message::Entry(entry(1)),
                Message::Entry(entry(4))
            ]),
            "entry before the previous result"
        );
        assert!(
            !ok(vec![fmt(), Message::Data(vec![1])]),
            "data outside an entry"
        );
        assert!(
            !ok(vec![fmt(), Message::Entry(entry(1)), result(4)]),
            "wrong result"
        );
        assert!(!ok(vec![fmt(), result(1)]), "result without an entry");
        assert!(ok(vec![
            fmt(),
            Message::Entry(entry(1)),
            Message::Data(vec![1]),
            result(1)
        ]));
    }

    fn rejects_many(wanted: &[u64], msgs: Vec<Message>) -> bool {
        let mut c = many(wanted);
        msgs.into_iter().any(|m| c.push(m).is_err())
    }

    #[test]
    fn read_many_needs_every_requested_entry_unless_the_run_stopped_early() {
        let mut c = many(&[1, 4]);
        for m in [fmt(), Message::Entry(entry(1)), result(1), done()] {
            c.push(m).unwrap();
        }
        assert!(c.into_read_many().is_err(), "an entry went missing");
        let mut c = many(&[1, 4]);
        for m in [
            fmt(),
            Message::Entry(entry(1)),
            result(1),
            Message::Done(Done {
                complete: false,
                entries: 1,
                bytes_out: 0,
                stop_reason: Some("limit".into()),
            }),
        ] {
            c.push(m).unwrap();
        }
        assert_eq!(
            c.into_read_many().unwrap().delivered,
            1,
            "an early stop is allowed to deliver fewer"
        );
        let mut c = many(&[1]);
        for m in [fmt(), Message::Entry(entry(1)), done()] {
            c.push(m).unwrap();
        }
        assert!(c.into_read_many().is_err(), "an entry without its result");
    }
}
