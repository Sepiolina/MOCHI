//! Help, User guide (audit F8): open the user guide that the installer bundles.
//!
//! The guide is `docs/user/` converted to static HTML at build time
//! (`packaging/guide.py`) and installed as a resource, `guide/index.html`. It is
//! written for the `mochi` command line, and says so in a banner on every page.
//!
//! Least privilege (AGENTS.md "Tauri v2 rules"): the webview supplies **no path and no
//! URL**. Rust resolves the bundled resource, checks that it is a file, and hands it to the
//! operating system's handler for `.html`. The page it opens is local and offline (no
//! script, no remote content), and the app's CSP is unchanged. The webview is granted one
//! permission for this, `allow-open-user-guide`, for this one command.
//!
//! The actual open sits behind [`Opener`] so that tests (and, in `e2e` builds only, the
//! E2E job) can record the path instead of launching a program.

use std::io;
use std::path::{Path, PathBuf};

use mochi_core::ErrorCode;
use tauri::{AppHandle, Manager};

use crate::ipc::IpcError;

/// Where the guide's first page is, under the app's resource directory. The installers put
/// the guide there (`packaging/build-installer.sh`) and the smoke checks assert it.
pub const GUIDE_RESOURCE: &str = "guide/index.html";

/// Opens a file with the operating system's handler for its type.
pub trait Opener {
    fn open(&self, path: &Path) -> io::Result<()>;
}

/// The real thing: `ShellExecute` on Windows, `xdg-open` on Ubuntu.
pub struct OsOpener;

impl Opener for OsOpener {
    fn open(&self, path: &Path) -> io::Result<()> {
        open::that(path)
    }
}

/// E2E builds only: append the path to a file instead of opening anything, so the job
/// shows which path the Help action opened without needing a browser on the runner.
#[cfg(feature = "e2e")]
pub struct RecordingOpener(pub PathBuf);

#[cfg(feature = "e2e")]
impl Opener for RecordingOpener {
    fn open(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)?;
        writeln!(log, "{}", path.display())
    }
}

/// `\\?\C:\x` is how Windows canonical paths come back from the resource resolver; the shell
/// handlers do not all accept that form, so drop the prefix for a drive path. A `\\?\UNC\`
/// path is left alone.
pub fn for_shell(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// Open the guide at `path` (already resolved by the caller). Refuses, without opening
/// anything, when the build does not carry the guide.
pub fn open_guide_at(path: Option<PathBuf>, opener: &dyn Opener) -> Result<PathBuf, IpcError> {
    let Some(path) = path.map(|p| for_shell(&p)).filter(|p| p.is_file()) else {
        return Err(IpcError {
            code: ErrorCode::IoError.as_str().to_owned(),
            message: "The user guide is not part of this build. It is installed with the \
                      MOCHI installer."
                .to_owned(),
        });
    };
    opener.open(&path).map_err(|e| IpcError {
        code: ErrorCode::IoError.as_str().to_owned(),
        message: format!("No program on this computer could open the user guide: {e}"),
    })?;
    Ok(path)
}

/// Show `folder` in the operating system's file manager ("Show in folder").
/// Rust resolved the folder; the webview named nothing. E2E builds append it
/// to `MOCHI_E2E_FOLDER_LOG` (not the guide's log) instead of opening a window.
pub fn open_folder(folder: &Path) -> Result<(), IpcError> {
    let folder = for_shell(folder);
    if !folder.is_dir() {
        return Err(IpcError {
            code: ErrorCode::IoError.as_str().to_owned(),
            message: "That folder is not there any more.".to_owned(),
        });
    }
    #[cfg(feature = "e2e")]
    if let Some(log) = std::env::var_os("MOCHI_E2E_FOLDER_LOG") {
        return RecordingOpener(PathBuf::from(log))
            .open(&folder)
            .map_err(|e| IpcError {
                code: ErrorCode::IoError.as_str().to_owned(),
                message: format!("Could not record the folder: {e}"),
            });
    }
    OsOpener.open(&folder).map_err(|e| IpcError {
        code: ErrorCode::IoError.as_str().to_owned(),
        message: format!("No program on this computer could show the folder: {e}"),
    })
}

/// The bundled guide's first page, as the app resolves it.
pub fn resolve_guide(app: &AppHandle) -> Option<PathBuf> {
    #[cfg(feature = "e2e")]
    if let Some(dir) = std::env::var_os("MOCHI_E2E_GUIDE_DIR") {
        return Some(PathBuf::from(dir).join("index.html"));
    }
    app.path()
        .resolve(GUIDE_RESOURCE, tauri::path::BaseDirectory::Resource)
        .ok()
}

/// Open the bundled user guide with the operating system's handler.
pub fn open_user_guide_for(app: &AppHandle) -> Result<(), IpcError> {
    let path = resolve_guide(app);
    #[cfg(feature = "e2e")]
    if let Some(log) = std::env::var_os("MOCHI_E2E_OPEN_LOG") {
        return open_guide_at(path, &RecordingOpener(PathBuf::from(log))).map(|_| ());
    }
    open_guide_at(path, &OsOpener).map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct Recorder {
        opened: RefCell<Vec<PathBuf>>,
        fail: bool,
    }

    impl Opener for Recorder {
        fn open(&self, path: &Path) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no handler"));
            }
            self.opened.borrow_mut().push(path.to_path_buf());
            Ok(())
        }
    }

    #[test]
    fn the_resource_path_is_the_one_the_installers_ship() {
        // packaging/build-installer.sh bundles the generated directory as `guide`, and the
        // installer smoke checks look for this file under the install's resource directory.
        assert_eq!(GUIDE_RESOURCE, "guide/index.html");
    }

    #[test]
    fn the_help_action_opens_exactly_the_bundled_path() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("guide").join("index.html");
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        std::fs::write(&index, "<!doctype html>").unwrap();
        let opener = Recorder::default();
        let opened = open_guide_at(Some(index.clone()), &opener).unwrap();
        assert_eq!(opened, index);
        assert_eq!(*opener.opened.borrow(), vec![index]);
    }

    #[test]
    fn a_build_without_the_guide_opens_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let opener = Recorder::default();
        for missing in [
            None,
            Some(dir.path().join("guide").join("index.html")),
            Some(dir.path().to_path_buf()),
        ] {
            let e = open_guide_at(missing, &opener).unwrap_err();
            assert_eq!(e.code, "IO_ERROR");
            assert!(
                e.message.contains("not part of this build"),
                "{}",
                e.message
            );
        }
        assert!(opener.opened.borrow().is_empty());
    }

    #[test]
    fn a_missing_handler_is_an_error_not_a_silent_success() {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.html");
        std::fs::write(&index, "x").unwrap();
        let opener = Recorder {
            fail: true,
            ..Recorder::default()
        };
        let e = open_guide_at(Some(index), &opener).unwrap_err();
        assert_eq!(e.code, "IO_ERROR");
        assert!(
            e.message.contains("could open the user guide"),
            "{}",
            e.message
        );
    }

    #[test]
    fn windows_verbatim_prefixes_are_dropped_for_drive_paths_only() {
        assert_eq!(
            for_shell(Path::new(r"\\?\C:\Program Files\MOCHI\guide\index.html")),
            PathBuf::from(r"C:\Program Files\MOCHI\guide\index.html")
        );
        assert_eq!(
            for_shell(Path::new(r"\\?\UNC\server\share\guide\index.html")),
            PathBuf::from(r"\\?\UNC\server\share\guide\index.html")
        );
        assert_eq!(
            for_shell(Path::new("/usr/lib/MOCHI/guide/index.html")),
            PathBuf::from("/usr/lib/MOCHI/guide/index.html")
        );
    }
}
