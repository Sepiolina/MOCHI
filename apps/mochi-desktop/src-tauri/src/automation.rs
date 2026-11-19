//! WebDriver support for test builds (feature `e2e`, the E2E job only).
//!
//! On Windows, msedgedriver attaches to a WebView2 app by setting
//! `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` (remote debugging) and
//! `WEBVIEW2_USER_DATA_FOLDER` (where it then looks for `DevToolsActivePort`).
//! Tauri's webview layer always passes its own browser arguments and data
//! directory to WebView2, and those win, so the driver never finds the port
//! (CI: "DevToolsActivePort file doesn't exist"). With `e2e`, the main
//! window passes both through. Shipped builds never enable the feature, so
//! an environment variable can never open a debugging port in them.
//!
//! WebDriver cannot drive a native file dialog, so with `e2e` the open
//! dialog first takes paths from `MOCHI_E2E_PICK` (a platform path list, one
//! path per "Open archive" click, in order; `crate::picker`). Shipped builds
//! never read the variable. D2's flows need different picks in each spec file
//! (a new app per file, so the queue starts over): `MOCHI_E2E_PICK_FILE` names
//! a text file with one path per line, which a spec rewrites before its
//! clicks; its lines are used first, then `MOCHI_E2E_PICK`. `MOCHI_E2E_PACE_MS`
//! slows the walk of a create or add, and an extraction (D3), by that many
//! milliseconds per entry, so that a spec can cancel one deterministically
//! (`crate::write::Tree`, `crate::extract`). The folder dialog of an extraction
//! takes its path from the same pick queue.
//!
//! On Windows tauri-driver passes a session's `args` to WebView2, not to the
//! app, so a spec cannot start the app with a shell verb. With `e2e`, a start
//! with no arguments reads them from `MOCHI_E2E_ARGS_FILE` (one per line) and
//! empties the file, so only that one start sees them (`launch_args`).

use tauri::{Manager, WebviewWindowBuilder, Wry};

/// The arguments Tauri's webview layer uses when none are given (wry
/// `webview2::create_environment`), kept when ours are added.
#[cfg(all(windows, feature = "e2e"))]
const DEFAULT_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

#[cfg(all(windows, feature = "e2e"))]
pub fn configure<'a, M: Manager<Wry>>(
    mut builder: WebviewWindowBuilder<'a, Wry, M>,
) -> WebviewWindowBuilder<'a, Wry, M> {
    if let Some(extra) = std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS") {
        let extra = extra.to_string_lossy();
        builder = builder.additional_browser_args(&format!("{DEFAULT_ARGS} {extra}"));
    }
    if let Some(folder) = std::env::var_os("WEBVIEW2_USER_DATA_FOLDER") {
        builder = builder.data_directory(folder.into());
    }
    builder
}

#[cfg(not(all(windows, feature = "e2e")))]
pub fn configure<'a, M: Manager<Wry>>(
    builder: WebviewWindowBuilder<'a, Wry, M>,
) -> WebviewWindowBuilder<'a, Wry, M> {
    builder
}

/// The first path in the `MOCHI_E2E_PICK_FILE` file, which is removed from it.
#[cfg(feature = "e2e")]
fn next_file_pick() -> Option<std::path::PathBuf> {
    let file = std::path::PathBuf::from(std::env::var_os("MOCHI_E2E_PICK_FILE")?);
    let text = std::fs::read_to_string(&file).ok()?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?.to_owned();
    let rest: Vec<&str> = lines.collect();
    std::fs::write(&file, rest.join("\n")).ok()?;
    Some(std::path::PathBuf::from(first))
}

/// The next path for a dialog: from `MOCHI_E2E_PICK_FILE`, else from
/// `MOCHI_E2E_PICK`, if any remain.
#[cfg(feature = "e2e")]
pub fn next_pick() -> Option<std::path::PathBuf> {
    if let Some(p) = next_file_pick() {
        return Some(p);
    }
    use std::collections::VecDeque;
    use std::sync::Mutex;
    static QUEUE: Mutex<Option<VecDeque<std::path::PathBuf>>> = Mutex::new(None);
    let mut q = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    q.get_or_insert_with(|| {
        std::env::var_os("MOCHI_E2E_PICK")
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default()
    })
    .pop_front()
}

/// The arguments of a start with none on its command line, from
/// `MOCHI_E2E_ARGS_FILE` (one per line), which is emptied: one start only.
#[cfg(feature = "e2e")]
pub fn launch_args() -> Vec<std::ffi::OsString> {
    let Some(file) = std::env::var_os("MOCHI_E2E_ARGS_FILE") else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Vec::new();
    };
    let _ = std::fs::write(&file, "");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(std::ffi::OsString::from)
        .collect()
}

/// Shipped builds take their arguments from the command line only.
#[cfg(not(feature = "e2e"))]
pub fn launch_args() -> Vec<std::ffi::OsString> {
    Vec::new()
}
