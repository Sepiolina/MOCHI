//! The crash-report policy (plan D7): **opt-in, local, never sent**.
//!
//! There is no reporting endpoint, so nothing is uploaded and this build links
//! no network client for it. What exists is a local panic log that the user
//! turns on, reads, exports, and deletes:
//!
//! - **Off by default.** Until the user switches it on in the app's settings,
//!   a panic writes nothing (the previous panic hook still runs, so the
//!   console message is unchanged).
//! - **What a record holds** (always, once on): the time, the app version, the
//!   platform, the panicking thread's name, the source `file:line:column` of
//!   the panic, and a bounded backtrace of function names. These describe the
//!   program, not the user's data.
//! - **What it never holds**: passphrases, archive contents, and file or
//!   folder names. The panic *message* is the one place those could appear (an
//!   `expect` or `format!` that quotes a path), so it is left out unless the
//!   user also switches on "include the panic message", which says so.
//! - **Where it lives**: `crash.log` (plus one rotated `crash.log.1`, 256 KiB
//!   each at most) in the app's local data directory. The webview never names
//!   a path; it can ask Rust to show the text, to save a copy through the
//!   native save dialog, or to delete the log.
//! - **What it does not catch**: a crash that is not a Rust panic (a fault in
//!   the system webview, an out-of-memory kill, the OS ending the process)
//!   leaves no record here.
//!
//! The hook never panics itself and a failure to write is ignored: a crash
//! log must not make a crash worse.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::panic::{self, AssertUnwindSafe, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use mochi_core::{ErrorCode, MochiError, Result};
use serde::{Deserialize, Serialize};

/// The first line of every record; the entry count is a count of these.
pub const RECORD_MARKER: &str = "=== MOCHI crash record ===";

/// The live log file's size at which it is rotated.
pub const MAX_LOG_BYTES: u64 = 256 * 1024;

/// The most backtrace lines one record keeps.
const MAX_BACKTRACE_LINES: usize = 60;

/// The most characters of a panic message one record keeps (when opted in).
const MAX_MESSAGE_CHARS: usize = 500;

/// The most bytes `read` shows in the window.
pub const MAX_READ_BYTES: usize = 64 * 1024;

const LOG_FILE: &str = "crash.log";
const ROTATED_FILE: &str = "crash.log.1";
const SETTINGS_FILE: &str = "crash-report-settings.json";

/// The user's choices. Both default to off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Write a record when the app panics.
    pub enabled: bool,
    /// Also record the panic message, which can quote a file name.
    pub include_details: bool,
}

/// What the hook takes from a panic. Plain data, so it can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanicFacts {
    pub thread: String,
    pub location: Option<String>,
    pub message: Option<String>,
    pub backtrace: String,
}

/// A summary of the log on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogStatus {
    pub entries: u32,
    pub bytes: u32,
}

/// The crash log of this process: its directory and the live settings.
pub struct CrashLog {
    dir: PathBuf,
    enabled: AtomicBool,
    include_details: AtomicBool,
}

impl CrashLog {
    /// Open the log in `dir`, reading the saved settings. A missing or
    /// unreadable settings file means everything off.
    pub fn open(dir: PathBuf) -> Arc<Self> {
        let saved = fs::read(dir.join(SETTINGS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
            .unwrap_or_default();
        Arc::new(CrashLog {
            dir,
            enabled: AtomicBool::new(saved.enabled),
            include_details: AtomicBool::new(saved.enabled && saved.include_details),
        })
    }

    pub fn settings(&self) -> Settings {
        Settings {
            enabled: self.enabled.load(Ordering::Relaxed),
            include_details: self.include_details.load(Ordering::Relaxed),
        }
    }

    /// Change and save the settings. "Include details" without "enabled" is
    /// meaningless, so it is stored as off.
    pub fn set(&self, wanted: Settings) -> Result<Settings> {
        let settings = Settings {
            enabled: wanted.enabled,
            include_details: wanted.enabled && wanted.include_details,
        };
        fs::create_dir_all(&self.dir).map_err(io("creating the settings folder", &self.dir))?;
        let json = serde_json::to_vec(&settings).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("saving the settings: {e}"))
        })?;
        let path = self.dir.join(SETTINGS_FILE);
        let tmp = self.dir.join(format!("{SETTINGS_FILE}.tmp"));
        fs::write(&tmp, json).map_err(io("saving the settings", &tmp))?;
        fs::rename(&tmp, &path).map_err(io("saving the settings", &path))?;
        self.enabled.store(settings.enabled, Ordering::Relaxed);
        self.include_details
            .store(settings.include_details, Ordering::Relaxed);
        Ok(settings)
    }

    /// Append a record for `facts` if the log is on. Returns whether it wrote.
    pub fn record(&self, facts: &PanicFacts) -> std::io::Result<bool> {
        let settings = self.settings();
        if !settings.enabled {
            return Ok(false);
        }
        let text = render(facts, settings.include_details, unix_now());
        fs::create_dir_all(&self.dir)?;
        let live = self.dir.join(LOG_FILE);
        if fs::metadata(&live).is_ok_and(|m| m.len() >= MAX_LOG_BYTES) {
            // One generation is kept; the older one is replaced.
            let _ = fs::rename(&live, self.dir.join(ROTATED_FILE));
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&live)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        Ok(true)
    }

    /// The rotated log followed by the live one, oldest first.
    fn all_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for name in [ROTATED_FILE, LOG_FILE] {
            if let Ok(mut f) = fs::File::open(self.dir.join(name)) {
                let _ = f.read_to_end(&mut out);
            }
        }
        out
    }

    pub fn status(&self) -> LogStatus {
        let bytes = self.all_bytes();
        LogStatus {
            entries: count_entries(&bytes),
            bytes: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
        }
    }

    /// The last `max` bytes of the log as text, and whether older text was
    /// left out. Invalid UTF-8 (which the writer never produces) is replaced.
    pub fn read(&self, max: usize) -> (String, bool) {
        let bytes = self.all_bytes();
        let start = bytes.len().saturating_sub(max);
        (
            String::from_utf8_lossy(&bytes[start..]).into_owned(),
            start > 0,
        )
    }

    /// The whole log, for "Export".
    pub fn export_bytes(&self) -> Vec<u8> {
        self.all_bytes()
    }

    /// Delete both files. A log that is already gone is fine.
    pub fn clear(&self) -> Result<()> {
        for name in [LOG_FILE, ROTATED_FILE] {
            let path = self.dir.join(name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io("deleting the crash log", &path)(e)),
            }
        }
        Ok(())
    }
}

fn io<'a>(what: &'a str, path: &'a Path) -> impl FnOnce(std::io::Error) -> MochiError + 'a {
    move |e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("{what} ({}): {e}", path.display()),
        )
    }
}

fn count_entries(bytes: &[u8]) -> u32 {
    let text = String::from_utf8_lossy(bytes);
    let n = text.lines().filter(|l| *l == RECORD_MARKER).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `1970-01-01T00:00:00Z`-style UTC time from Unix seconds (Hinnant's civil
/// calendar algorithm; no time dependency is needed for one timestamp).
pub fn rfc3339_utc(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// One record as text. The message is included only when `details` is on, and
/// then escaped to one inert line and cut short.
pub fn render(facts: &PanicFacts, details: bool, unix_secs: u64) -> String {
    let mut out = String::new();
    out.push_str(RECORD_MARKER);
    out.push('\n');
    out.push_str(&format!("time: {}\n", rfc3339_utc(unix_secs)));
    out.push_str(&format!(
        "app: mochi-desktop {}\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!(
        "platform: {} {}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    out.push_str(&format!("thread: {}\n", one_line(&facts.thread)));
    out.push_str(&format!(
        "location: {}\n",
        facts.location.as_deref().unwrap_or("unknown")
    ));
    if details {
        let message = facts.message.as_deref().unwrap_or("(no text)");
        let escaped = mochi_core::text::escape(message.as_bytes());
        let cut: String = escaped.chars().take(MAX_MESSAGE_CHARS).collect();
        out.push_str(&format!("message: {cut}\n"));
    } else {
        out.push_str("message: not recorded (\"include the panic message\" is off)\n");
    }
    out.push_str("backtrace:\n");
    for line in facts.backtrace.lines().take(MAX_BACKTRACE_LINES) {
        out.push_str("  ");
        out.push_str(&one_line(line.trim_end()));
        out.push('\n');
    }
    out.push('\n');
    out
}

fn one_line(s: &str) -> String {
    mochi_core::text::escape(s.as_bytes())
}

fn facts_from(info: &PanicHookInfo<'_>) -> PanicFacts {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    PanicFacts {
        thread: std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned(),
        location: info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())),
        message,
        backtrace: std::backtrace::Backtrace::force_capture().to_string(),
    }
}

static ACTIVE: OnceLock<Arc<CrashLog>> = OnceLock::new();

/// Install the panic hook for `log`. The previous hook still runs. Only the
/// first call installs; later calls are ignored.
pub fn install(log: Arc<CrashLog>) {
    if ACTIVE.set(log).is_err() {
        return;
    }
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Some(log) = ACTIVE.get() {
            // Cheap, lock-free check first: when off, nothing else runs.
            if log.enabled.load(Ordering::Relaxed) {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                    let _ = log.record(&facts_from(info));
                }));
            }
        }
        previous(info);
    }));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn facts(message: &str) -> PanicFacts {
        PanicFacts {
            thread: "mochi-job".to_owned(),
            location: Some("apps/mochi-desktop/src-tauri/src/jobs.rs:10:5".to_owned()),
            message: Some(message.to_owned()),
            backtrace: "   0: mochi_desktop::jobs::run\n   1: std::rt::lang_start".to_owned(),
        }
    }

    fn on(dir: &Path, include_details: bool) -> Arc<CrashLog> {
        let log = CrashLog::open(dir.to_path_buf());
        log.set(Settings {
            enabled: true,
            include_details,
        })
        .unwrap();
        log
    }

    #[test]
    fn everything_is_off_until_the_user_turns_it_on() {
        let dir = tempfile::tempdir().unwrap();
        let log = CrashLog::open(dir.path().to_path_buf());
        assert_eq!(log.settings(), Settings::default());
        assert!(!log.settings().enabled && !log.settings().include_details);
        // A panic while it is off writes nothing, not even the directory.
        assert!(!log.record(&facts("boom")).unwrap());
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            0,
            "an off log creates no files"
        );
        assert_eq!(
            log.status(),
            LogStatus {
                entries: 0,
                bytes: 0
            }
        );
    }

    #[test]
    fn a_record_has_no_message_unless_the_user_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let log = on(dir.path(), false);
        let secret = "/home/me/taxes 2025/passphrase-hunter2.docx";
        assert!(log
            .record(&facts(&format!("cannot open {secret}")))
            .unwrap());
        let (text, truncated) = log.read(MAX_READ_BYTES);
        assert!(!truncated);
        assert!(text.starts_with(RECORD_MARKER));
        assert!(text.contains("jobs.rs:10:5"), "the source location is kept");
        assert!(
            text.contains("mochi_desktop::jobs::run"),
            "and the backtrace"
        );
        assert!(text.contains("message: not recorded"));
        for leak in ["taxes", "hunter2", "cannot open", "/home/me"] {
            assert!(!text.contains(leak), "{leak} must not be in the record");
        }
        assert_eq!(log.status().entries, 1);
    }

    #[test]
    fn with_the_message_opt_in_it_is_one_escaped_bounded_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = on(dir.path(), true);
        let long = "x".repeat(2_000);
        log.record(&facts("bad\nname\u{202e}.txt")).unwrap();
        log.record(&facts(&long)).unwrap();
        let (text, _) = log.read(MAX_READ_BYTES);
        let message_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("message: "))
            .collect();
        assert_eq!(message_lines.len(), 2);
        assert_eq!(message_lines[0], "message: bad\\u{a}name\\u{202e}.txt");
        assert!(message_lines[1].len() <= "message: ".len() + MAX_MESSAGE_CHARS);
        assert_eq!(log.status().entries, 2);
    }

    #[test]
    fn details_cannot_be_on_without_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = CrashLog::open(dir.path().to_path_buf());
        let stored = log
            .set(Settings {
                enabled: false,
                include_details: true,
            })
            .unwrap();
        assert_eq!(stored, Settings::default());
    }

    #[test]
    fn settings_survive_a_restart_and_a_damaged_file_means_off() {
        let dir = tempfile::tempdir().unwrap();
        on(dir.path(), true);
        let again = CrashLog::open(dir.path().to_path_buf());
        assert_eq!(
            again.settings(),
            Settings {
                enabled: true,
                include_details: true
            }
        );
        fs::write(dir.path().join(SETTINGS_FILE), b"{ not json").unwrap();
        assert_eq!(
            CrashLog::open(dir.path().to_path_buf()).settings(),
            Settings::default()
        );
        // A hand-edited file cannot switch details on without the log.
        fs::write(
            dir.path().join(SETTINGS_FILE),
            br#"{"enabled":false,"include_details":true}"#,
        )
        .unwrap();
        assert!(
            !CrashLog::open(dir.path().to_path_buf())
                .settings()
                .include_details
        );
    }

    #[test]
    fn the_log_is_bounded_by_rotation_and_can_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let log = on(dir.path(), false);
        // Fill the live file past the limit, then record again.
        fs::write(
            dir.path().join(LOG_FILE),
            vec![b'x'; MAX_LOG_BYTES as usize],
        )
        .unwrap();
        log.record(&facts("a")).unwrap();
        assert!(dir.path().join(ROTATED_FILE).exists());
        assert!(fs::metadata(dir.path().join(LOG_FILE)).unwrap().len() < MAX_LOG_BYTES);
        // Reading is windowed to the newest text and says so.
        let (text, truncated) = log.read(100);
        assert!(truncated);
        assert!(text.len() <= 100);
        assert_eq!(log.export_bytes().len(), log.status().bytes as usize);
        log.clear().unwrap();
        assert_eq!(
            log.status(),
            LogStatus {
                entries: 0,
                bytes: 0
            }
        );
        log.clear().unwrap();
        // The settings stay: deleting the log does not turn it off.
        assert!(log.settings().enabled);
    }

    #[cfg(unix)]
    #[test]
    fn the_log_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        on(dir.path(), false).record(&facts("a")).unwrap();
        let mode = fs::metadata(dir.path().join(LOG_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "no group or other access");
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn the_installed_hook_writes_only_when_on() {
        // The one test that installs the process-wide hook; it uses its own
        // directory and a unique marker, so another test's panic cannot
        // confuse it.
        let dir = tempfile::tempdir().unwrap();
        let log = CrashLog::open(dir.path().to_path_buf());
        install(Arc::clone(&log));
        // `install` is first-come; if another test got there first, this
        // process's hook points at that log and there is nothing to check.
        if !ACTIVE.get().is_some_and(|a| Arc::ptr_eq(a, &log)) {
            return;
        }
        let _ = panic::catch_unwind(|| panic!("marker-while-off"));
        assert_eq!(log.status().entries, 0);
        log.set(Settings {
            enabled: true,
            include_details: false,
        })
        .unwrap();
        let _ = panic::catch_unwind(|| panic!("marker-while-on"));
        let (text, _) = log.read(MAX_READ_BYTES);
        assert_eq!(log.status().entries, 1, "one record: {text}");
        assert!(!text.contains("marker-while-on"), "the message stays out");
        assert!(text.contains("crashlog.rs"), "the panic site is recorded");
    }
}
