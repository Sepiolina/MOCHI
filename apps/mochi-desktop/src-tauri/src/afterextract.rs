//! What happens after an extraction (owner requests 2026-11-09 and 2026-11-10): the folder
//! extracted into opens in a new file-manager window when the job is done, unless the user
//! turned "Open the folder when done" off.
//!
//! One choice for the whole app. The compact window of the shell's quick verbs
//! (`quick.rs`) and the archive windows' Extract and "Extract to..." (`write::Services::run`)
//! read and change the same value, so the check box says the same thing wherever it is shown.
//! It is saved in `quick-settings.json` in the app's local data folder (the name it had when
//! only the compact window used it) and read once at start.
//!
//! Opening is best effort: a file manager that cannot be started leaves the "Show in folder"
//! button, which says why. Only an extraction that ended without an error opens anything; a
//! failed or cancelled one does not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use mochi_core::{ErrorCode, MochiError, Result};
use serde::{Deserialize, Serialize};

/// Where the choice is saved (app local data folder).
pub const SETTINGS_FILE: &str = "quick-settings.json";

/// Opens a folder in the file manager (the app's `help::open_folder`; a recorder in tests).
pub type Opener = dyn Fn(&Path) -> std::io::Result<()> + Send + Sync;

/// The saved choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Settings {
    /// Open the folder extracted into when the job is done. On by default.
    #[serde(default = "yes")]
    open_folder_when_done: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            open_folder_when_done: true,
        }
    }
}

/// "Open the folder when done", shared by every window.
#[derive(Default)]
pub struct AfterExtract {
    /// The user turned it off (so the default, `false`, means on).
    keep_closed: AtomicBool,
    /// Where the choice is saved; unset in tests that do not save it.
    dir: OnceLock<PathBuf>,
    /// How a folder is opened; unset means nothing is opened (tests that do not care).
    opener: OnceLock<Arc<Opener>>,
}

impl AfterExtract {
    /// Read the saved choice from `dir` (a missing or damaged file means on) and save there
    /// from now on. Once, at start-up.
    pub fn load(&self, dir: PathBuf) {
        let saved: Settings = std::fs::read(dir.join(SETTINGS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        self.keep_closed
            .store(!saved.open_folder_when_done, Ordering::Relaxed);
        let _ = self.dir.set(dir);
    }

    /// Use `opener` to open folders (once, at start-up).
    pub fn set_opener(&self, opener: Arc<Opener>) {
        let _ = self.opener.set(opener);
    }

    /// Whether the folder extracted into opens when the job is done.
    pub fn open_when_done(&self) -> bool {
        !self.keep_closed.load(Ordering::Relaxed)
    }

    /// Turn it on or off, and save it. It applies to a job still running too.
    pub fn set(&self, open: bool) -> Result<bool> {
        if let Some(dir) = self.dir.get() {
            save(
                dir,
                Settings {
                    open_folder_when_done: open,
                },
            )?;
        }
        self.keep_closed.store(!open, Ordering::Relaxed);
        Ok(open)
    }

    /// An extraction into `folder` is done: open it if the user wants that. `true` if it was
    /// opened. Read now, not when the job began: the box may have changed meanwhile.
    pub fn done(&self, folder: &Path) -> bool {
        if !self.open_when_done() {
            return false;
        }
        match self.opener.get() {
            Some(open) => open(folder).is_ok(),
            None => false,
        }
    }
}

fn save(dir: &Path, settings: Settings) -> Result<()> {
    let io = |what: &str, p: &Path, e: std::io::Error| {
        MochiError::new(ErrorCode::IoError, format!("{what} {}: {e}", p.display()))
    };
    std::fs::create_dir_all(dir).map_err(|e| io("creating the settings folder", dir, e))?;
    let json = serde_json::to_vec(&settings)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("saving the settings: {e}")))?;
    let path = dir.join(SETTINGS_FILE);
    let tmp = dir.join(format!("{SETTINGS_FILE}.tmp"));
    std::fs::write(&tmp, json).map_err(|e| io("saving the settings to", &tmp, e))?;
    std::fs::rename(&tmp, &path).map_err(|e| io("saving the settings to", &path, e))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn recording() -> (AfterExtract, Arc<Mutex<Vec<PathBuf>>>) {
        let a = AfterExtract::default();
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        a.set_opener(Arc::new(move |p: &Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }));
        (a, log)
    }

    #[test]
    fn on_by_default_saved_when_changed_and_a_damaged_file_means_on() {
        let dir = tempfile::tempdir().unwrap();
        let (a, log) = recording();
        a.load(dir.path().to_path_buf());
        assert!(a.open_when_done());
        assert!(a.done(Path::new("/x")));
        assert!(!a.set(false).unwrap());
        assert!(!a.done(Path::new("/y")));
        assert_eq!(*log.lock().unwrap(), vec![PathBuf::from("/x")]);
        let again = AfterExtract::default();
        again.load(dir.path().to_path_buf());
        assert!(!again.open_when_done());
        std::fs::write(dir.path().join(SETTINGS_FILE), b"{oops").unwrap();
        let third = AfterExtract::default();
        third.load(dir.path().to_path_buf());
        assert!(third.open_when_done());
    }

    #[test]
    fn without_an_opener_nothing_is_opened() {
        assert!(!AfterExtract::default().done(Path::new("/x")));
    }
}
