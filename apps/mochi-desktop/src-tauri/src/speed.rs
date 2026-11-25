//! How many processor cores a write may use to compress (release 0.9.11),
//! and whether extraction flushes each file before naming it ("fast
//! extract").
//!
//! `mochi-core` reads, hashes and compresses a commit's content on worker
//! threads ahead of the writer (`WriterOptions::threads`); the archive is the
//! same bytes either way, so this is only a speed choice. On by default: all
//! cores (at most 8). Off: one core, which leaves the computer more
//! responsive during a long job. The choice is saved next to the crash-log
//! settings and read once at start, before any window can start a write
//! (the Explorer "Add to archive" verbs included).
//!
//! Fast extract is off by default. On, an extraction publishes each verified
//! file without first flushing its data to the device
//! (`RestoreOptions::skip_file_sync`): much faster for many small files,
//! especially on Windows, but after a power failure an extracted name may
//! hold incomplete content. Verification is unchanged either way.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use mochi_core::{ErrorCode, MochiError, Result};

const SETTINGS_FILE: &str = "compression-settings.json";

/// The saved choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Use every core (at most 8) to compress.
    pub all_cores: bool,
    /// Extract without a per-file flush. Absent in files saved before it
    /// existed, which means off.
    #[serde(default)]
    pub fast_extract: bool,
    /// Windows: count a successful folder flush on a local NTFS drive as
    /// saved when an archive is created (spec Annex B.2.20, D30). Absent in
    /// files saved before it existed, which means off (policy B).
    #[serde(default)]
    pub trust_ntfs_flush: bool,
    /// Remember each archive's last-seen head and the results of tests and
    /// commits in the state directory the command-line tool also uses (spec
    /// Annex B.2.22): freshness can then be checked, and the health summary
    /// outlives the session. Absent in files saved before it existed, which
    /// means on, as in the command-line tool.
    #[serde(default = "on")]
    pub local_history: bool,
    /// Adding to an archive another MOCHI program is writing to: wait up to
    /// [`WRITER_WAIT`] for it to finish instead of refusing at once (spec
    /// Annex B.2.23). Absent means off, the spec's fail-on-conflict default.
    #[serde(default)]
    pub wait_for_writer: bool,
}

/// How long an add waits for another writer when the setting is on.
pub const WRITER_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

fn on() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            all_cores: true,
            fast_extract: false,
            trust_ntfs_flush: false,
            local_history: true,
            wait_for_writer: false,
        }
    }
}

static ALL_CORES: AtomicBool = AtomicBool::new(true);
static FAST_EXTRACT: AtomicBool = AtomicBool::new(false);
static TRUST_NTFS_FLUSH: AtomicBool = AtomicBool::new(false);
static LOCAL_HISTORY: AtomicBool = AtomicBool::new(true);
static WAIT_FOR_WRITER: AtomicBool = AtomicBool::new(false);
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Read the saved choice from `dir` (a missing or unreadable file means the
/// default, all cores) and remember where to save it.
pub fn load(dir: PathBuf) {
    let saved = read(&dir);
    ALL_CORES.store(saved.all_cores, Ordering::Relaxed);
    FAST_EXTRACT.store(saved.fast_extract, Ordering::Relaxed);
    TRUST_NTFS_FLUSH.store(saved.trust_ntfs_flush, Ordering::Relaxed);
    LOCAL_HISTORY.store(saved.local_history, Ordering::Relaxed);
    WAIT_FOR_WRITER.store(saved.wait_for_writer, Ordering::Relaxed);
    let _ = DIR.set(dir);
}

fn read(dir: &Path) -> Settings {
    fs::read(dir.join(SETTINGS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or_default()
}

/// The current choice.
pub fn settings() -> Settings {
    Settings {
        all_cores: ALL_CORES.load(Ordering::Relaxed),
        fast_extract: FAST_EXTRACT.load(Ordering::Relaxed),
        trust_ntfs_flush: TRUST_NTFS_FLUSH.load(Ordering::Relaxed),
        local_history: LOCAL_HISTORY.load(Ordering::Relaxed),
        wait_for_writer: WAIT_FOR_WRITER.load(Ordering::Relaxed),
    }
}

/// Change and save the choices. They apply to jobs started from now on.
pub fn set(wanted: Settings) -> Result<Settings> {
    if let Some(dir) = DIR.get() {
        save(dir, wanted)?;
    }
    ALL_CORES.store(wanted.all_cores, Ordering::Relaxed);
    FAST_EXTRACT.store(wanted.fast_extract, Ordering::Relaxed);
    TRUST_NTFS_FLUSH.store(wanted.trust_ntfs_flush, Ordering::Relaxed);
    LOCAL_HISTORY.store(wanted.local_history, Ordering::Relaxed);
    WAIT_FOR_WRITER.store(wanted.wait_for_writer, Ordering::Relaxed);
    Ok(wanted)
}

fn save(dir: &Path, settings: Settings) -> Result<()> {
    let io = |what: &str, p: &Path, e: std::io::Error| {
        MochiError::new(ErrorCode::IoError, format!("{what} {}: {e}", p.display()))
    };
    fs::create_dir_all(dir).map_err(|e| io("creating the settings folder", dir, e))?;
    let json = serde_json::to_vec(&settings)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("saving the settings: {e}")))?;
    let path = dir.join(SETTINGS_FILE);
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    fs::write(&tmp, json).map_err(|e| io("saving the settings to", &tmp, e))?;
    fs::rename(&tmp, &path).map_err(|e| io("saving the settings to", &path, e))?;
    Ok(())
}

/// What an append passes as `WriterOptions::lock_wait` (Annex B.2.23).
pub fn lock_wait() -> Option<std::time::Duration> {
    WAIT_FOR_WRITER
        .load(Ordering::Relaxed)
        .then_some(WRITER_WAIT)
}

/// What a write passes as `WriterOptions::threads`.
pub fn writer_threads() -> Option<usize> {
    if ALL_CORES.load(Ordering::Relaxed) {
        None
    } else {
        Some(1)
    }
}

/// What a directory flush counts as on Windows when the app creates an
/// archive (D30): policy B unless the user turned the setting on.
pub fn windows_flush() -> mochi_core::storage::os::WindowsDirectoryFlush {
    if TRUST_NTFS_FLUSH.load(Ordering::Relaxed) {
        mochi_core::storage::os::WindowsDirectoryFlush::TrustLocalNtfs
    } else {
        mochi_core::storage::os::WindowsDirectoryFlush::Unconfirmed
    }
}

/// What an extraction passes as `skip_file_sync`.
pub fn skip_file_sync() -> bool {
    FAST_EXTRACT.load(Ordering::Relaxed)
}

/// The cores "all cores" means on this computer.
pub fn cores() -> usize {
    mochi_core::publish::default_writer_threads()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_survives_a_restart_and_a_damaged_file_means_all_cores() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), Settings::default());
        assert_eq!(
            Settings::default(),
            Settings {
                all_cores: true,
                fast_extract: false,
                trust_ntfs_flush: false,
                local_history: true,
                wait_for_writer: false,
            }
        );
        let changed = Settings {
            all_cores: false,
            fast_extract: true,
            trust_ntfs_flush: true,
            local_history: false,
            wait_for_writer: true,
        };
        save(dir.path(), changed).unwrap();
        assert_eq!(read(dir.path()), changed);
        fs::write(dir.path().join(SETTINGS_FILE), b"{not json").unwrap();
        assert_eq!(read(dir.path()), Settings::default());
    }

    #[test]
    fn a_file_saved_before_fast_extract_means_it_is_off() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(SETTINGS_FILE), br#"{"all_cores":false}"#).unwrap();
        assert_eq!(
            read(dir.path()),
            Settings {
                all_cores: false,
                fast_extract: false,
                trust_ntfs_flush: false,
                local_history: true,
                wait_for_writer: false,
            }
        );
    }

    /// D30: a file saved before the NTFS setting existed means policy B.
    #[test]
    fn a_file_saved_before_the_ntfs_setting_means_policy_b() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SETTINGS_FILE),
            br#"{"all_cores":true,"fast_extract":true}"#,
        )
        .unwrap();
        assert!(!read(dir.path()).trust_ntfs_flush);
        assert!(
            !Settings::default().trust_ntfs_flush,
            "the default is policy B"
        );
    }

    /// Annex B.2.22: a file saved before the setting existed keeps local
    /// history on, as the command-line tool has it by default.
    #[test]
    fn a_file_saved_before_local_history_means_it_is_on() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SETTINGS_FILE),
            br#"{"all_cores":true,"fast_extract":false,"trust_ntfs_flush":false}"#,
        )
        .unwrap();
        assert!(read(dir.path()).local_history);
        assert!(Settings::default().local_history);
    }
}
