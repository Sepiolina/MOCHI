//! In-app updates (plan D7, T13). The update check, the download, the
//! signature check and the install all happen in Rust: the webview is never
//! given the updater plugin's own permissions and makes no network request
//! (the CSP is unchanged). It sees only the adapters below, each granted by
//! name in `capabilities/main.json`.
//!
//! Privacy and consent:
//! - Nothing is requested until the user presses "Check for updates", or has
//!   turned on the opt-in "check when the app starts" setting (off by default;
//!   the refusal of an automatic check while it is off is made here, not in the
//!   UI).
//! - Nothing is downloaded or installed until the user confirms the version
//!   they were shown; `install_update` names that version and a stale
//!   confirmation is refused.
//! - The endpoint, the public key and the release page are fixed in
//!   `tauri.conf.json` and this file; the webview supplies no URL.
//! - The plugin refuses an update whose signature does not verify against the
//!   public key in the configuration, and an update signed for a different
//!   version than the one announced (`requireSignedVersion`). A refused
//!   download is never installed. An unsigned installer is not "verified" or
//!   "trusted" by the operating system; this signature is what authenticates an
//!   update.
//! - Release notes come from a remote server: they are bounded, stripped of
//!   control characters, and shown as text only.
//!
//! The decision logic is independent of the plugin: [`Backend`] is the seam,
//! and the tests use a fake that performs no network I/O.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use mochi_core::job::JobContext;
use mochi_core::{ErrorCode, MochiError, Result as CoreResult};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where a user downloads a release by hand (the Ubuntu fallback, and the page
/// the Settings text points at). Fixed: never taken from the update manifest.
pub const RELEASES_PAGE: &str = "https://github.com/immerh8/MOCHI/releases/latest";

/// The most release-note bytes passed to the window.
pub const MAX_NOTES_BYTES: usize = 16 * 1024;

/// The longest version string accepted from the server.
const MAX_VERSION_BYTES: usize = 64;

/// The phase name in the install job's progress events.
pub const PHASE_DOWNLOAD: &str = "update_download";

const SETTINGS_FILE: &str = "update-settings.json";

/// How this build can apply an update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum UpdateInstallMode {
    /// The app downloads, verifies and installs the update itself (the NSIS
    /// setup on Windows; the `.deb` on Ubuntu through the system's package
    /// tool, which asks for administrator permission).
    InPlace,
    /// The app only announces the version; the user downloads it from the
    /// release page. Used for any build that was not installed from one of the
    /// two packages (a development build, a copied binary).
    Manual,
}

/// The package the running binary came from, as far as the updater cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Package {
    Nsis,
    Deb,
    /// Anything else, or unknown (no bundle marker in the binary).
    Other,
}

/// Whether the plugin can install in place for `package`. Pure: the answer
/// depends on how the app was installed, nothing else (AppImage is not a 1.0
/// target, decision O11; there is no MSI).
pub fn install_mode(package: Package) -> UpdateInstallMode {
    match package {
        Package::Nsis | Package::Deb => UpdateInstallMode::InPlace,
        Package::Other => UpdateInstallMode::Manual,
    }
}

/// What the Settings page shows and sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct UpdateSettingsView {
    /// Check once when the app starts. Off unless the user turned it on.
    pub check_on_start: bool,
    pub current_version: String,
    pub install_mode: UpdateInstallMode,
    /// The page to download a release from by hand.
    pub download_page: String,
    /// The install asks the operating system for administrator permission
    /// (Ubuntu's package tool).
    pub needs_admin_prompt: bool,
}

/// A newer version the server announced, ready for the user to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct UpdateOffer {
    pub version: String,
    pub current_version: String,
    /// Release notes as plain text, bounded and without control characters.
    #[ts(optional)]
    pub notes: Option<String>,
    pub notes_truncated: bool,
    /// The announced publish date, as the server sent it, when it parsed.
    #[ts(optional)]
    pub date: Option<String>,
    pub install_mode: UpdateInstallMode,
    pub download_page: String,
    pub needs_admin_prompt: bool,
}

/// Why a check or an install did not happen. The window words each kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum UpdateFailureKind {
    /// The update server could not be reached.
    Offline,
    /// The server answered, but not with a usable release.
    Server,
    /// The update's signature did not verify: it was refused.
    Signature,
    /// The download finished but the install did not.
    Install,
    /// The update service is not available in this build.
    Unavailable,
    Other,
}

/// The result of "Check for updates".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "result", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum UpdateCheckView {
    UpToDate {
        current_version: String,
    },
    Available {
        offer: UpdateOffer,
    },
    /// An automatic check was asked for while the setting is off: nothing was
    /// requested.
    Skipped,
    Failed {
        kind: UpdateFailureKind,
        /// A short plain-text description of what went wrong.
        detail: String,
    },
}

/// A failure from the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: UpdateFailureKind,
    pub detail: String,
    pub cancelled: bool,
}

impl Failure {
    fn new(kind: UpdateFailureKind, detail: impl Into<String>) -> Self {
        Failure {
            kind,
            detail: bounded_line(&detail.into(), 200),
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Failure {
            cancelled: true,
            ..Failure::new(UpdateFailureKind::Other, "cancelled")
        }
    }
}

/// What the backend learned about the newest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// The seam to the updater plugin. Blocking, object-safe, and free of Tauri
/// types so that the tests need no network and no window.
pub trait Backend: Send + Sync {
    /// The package the app was installed from.
    fn package(&self) -> Package;
    /// The running version.
    fn current_version(&self) -> String;
    /// Ask the server for a newer version. The backend remembers it for
    /// [`Backend::install`].
    fn check(&self) -> std::result::Result<Option<Candidate>, Failure>;
    /// Download the remembered update `version`, verify its signature against
    /// the configured public key, then install it. `progress` gets
    /// (bytes so far, total when known); `cancelled` is polled during the
    /// download. A signature that does not verify is an `Err` with kind
    /// `Signature`, and nothing is installed.
    fn install(
        &self,
        version: &str,
        progress: &dyn Fn(u64, Option<u64>),
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<(), Failure>;
}

#[derive(Debug, Default)]
struct State {
    dir: Option<PathBuf>,
    check_on_start: bool,
    /// The version of the offer the user was last shown.
    offered: Option<String>,
}

/// The update service: settings, the decision logic, and the one backend.
#[derive(Default)]
pub struct Updates {
    state: Mutex<State>,
    backend: OnceLock<Arc<dyn Backend>>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    check_on_start: bool,
}

impl Updates {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Bind the real backend and the settings folder (once, at startup). The
    /// settings are read here; a missing or damaged file means off.
    pub fn configure(&self, dir: PathBuf, backend: Arc<dyn Backend>) {
        let _ = self.backend.set(backend);
        let mut state = self.lock();
        state.check_on_start = read_stored(&dir.join(SETTINGS_FILE));
        state.dir = Some(dir);
    }

    pub fn settings(&self) -> UpdateSettingsView {
        let check_on_start = self.lock().check_on_start;
        let (current_version, package) = match self.backend.get() {
            Some(b) => (b.current_version(), b.package()),
            None => (env!("CARGO_PKG_VERSION").to_owned(), Package::Other),
        };
        UpdateSettingsView {
            check_on_start,
            current_version,
            install_mode: install_mode(package),
            download_page: RELEASES_PAGE.to_owned(),
            needs_admin_prompt: package == Package::Deb,
        }
    }

    /// Turn the startup check on or off and remember it.
    pub fn set_check_on_start(&self, on: bool) -> CoreResult<UpdateSettingsView> {
        let dir = self.lock().dir.clone();
        if let Some(dir) = dir {
            write_stored(&dir.join(SETTINGS_FILE), on)?;
        }
        self.lock().check_on_start = on;
        Ok(self.settings())
    }

    /// Ask the server for a newer version. An `automatic` check while the
    /// setting is off requests nothing.
    pub fn check(&self, automatic: bool) -> UpdateCheckView {
        if automatic && !self.lock().check_on_start {
            return UpdateCheckView::Skipped;
        }
        let Some(backend) = self.backend.get() else {
            return UpdateCheckView::Failed {
                kind: UpdateFailureKind::Unavailable,
                detail: "the update service is not running".to_owned(),
            };
        };
        match backend.check() {
            Err(f) => {
                self.lock().offered = None;
                UpdateCheckView::Failed {
                    kind: f.kind,
                    detail: f.detail,
                }
            }
            Ok(None) => {
                self.lock().offered = None;
                UpdateCheckView::UpToDate {
                    current_version: backend.current_version(),
                }
            }
            Ok(Some(c)) => match offer(&c, backend.as_ref()) {
                Some(offer) => {
                    self.lock().offered = Some(offer.version.clone());
                    UpdateCheckView::Available { offer }
                }
                None => {
                    self.lock().offered = None;
                    UpdateCheckView::Failed {
                        kind: UpdateFailureKind::Server,
                        detail: "the server announced a version this app cannot read".to_owned(),
                    }
                }
            },
        }
    }

    /// Refuse an install that does not name the offer the user was shown, or
    /// that this build cannot apply in place.
    pub fn validate_install(&self, version: &str) -> CoreResult<()> {
        let Some(backend) = self.backend.get() else {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "the update service is not running",
            ));
        };
        if install_mode(backend.package()) != UpdateInstallMode::InPlace {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                format!(
                    "this copy of MOCHI was not installed from a MOCHI package, so it cannot \
                     update itself; download the new version from {RELEASES_PAGE}"
                ),
            ));
        }
        if self.lock().offered.as_deref() != Some(version) {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "that version is not the one on offer; check for updates again",
            ));
        }
        Ok(())
    }

    /// The install job: download, verify, install. Reports download progress;
    /// stops at the next poll when cancelled (once the installer has been
    /// handed the verified package it can no longer be stopped).
    pub fn run_install(&self, version: &str, ctx: &JobContext<'_>) -> CoreResult<String> {
        self.validate_install(version)?;
        let Some(backend) = self.backend.get() else {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "the update service is not running",
            ));
        };
        ctx.report(PHASE_DOWNLOAD, 0, None);
        let result = backend.install(
            version,
            &|done, total| ctx.report(PHASE_DOWNLOAD, done, total),
            &|| ctx.check_cancelled().is_err(),
        );
        match result {
            Ok(()) => {
                self.lock().offered = None;
                Ok(format!("update to version {version} installed"))
            }
            Err(f) if f.cancelled => Err(MochiError::new(ErrorCode::Cancelled, "cancelled")),
            Err(f) => Err(MochiError::new(job_code(f.kind), failure_message(&f))),
        }
    }
}

/// The registered code a failed install job reports. Updates have no codes of
/// their own: the message says which step failed.
fn job_code(kind: UpdateFailureKind) -> ErrorCode {
    match kind {
        UpdateFailureKind::Unavailable => ErrorCode::UnsupportedFeature,
        _ => ErrorCode::IoError,
    }
}

fn failure_message(f: &Failure) -> String {
    match f.kind {
        UpdateFailureKind::Signature => {
            "The update's signature did not verify, so it was refused and nothing was installed."
                .to_owned()
        }
        UpdateFailureKind::Offline => {
            "The update server could not be reached. Nothing was installed.".to_owned()
        }
        UpdateFailureKind::Server => format!(
            "The update server did not return a usable download. Nothing was installed. ({})",
            f.detail
        ),
        UpdateFailureKind::Install => format!(
            "The update was downloaded and its signature verified, but it was not installed. ({})",
            f.detail
        ),
        UpdateFailureKind::Unavailable | UpdateFailureKind::Other => {
            format!("The update was not installed. ({})", f.detail)
        }
    }
}

fn offer(c: &Candidate, backend: &dyn Backend) -> Option<UpdateOffer> {
    let version = c.version.trim();
    if version.is_empty()
        || version.len() > MAX_VERSION_BYTES
        || !version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
    {
        return None;
    }
    let (notes, notes_truncated) = match &c.notes {
        Some(n) => {
            let (text, cut) = plain_notes(n);
            ((!text.is_empty()).then_some(text), cut)
        }
        None => (None, false),
    };
    let package = backend.package();
    Some(UpdateOffer {
        version: version.to_owned(),
        current_version: backend.current_version(),
        notes,
        notes_truncated,
        date: c.date.as_deref().map(|d| bounded_line(d, 64)),
        install_mode: install_mode(package),
        download_page: RELEASES_PAGE.to_owned(),
        needs_admin_prompt: package == Package::Deb,
    })
}

/// Release notes as safe plain text: at most [`MAX_NOTES_BYTES`], line breaks
/// kept, every other control character (and bidirectional overrides, which
/// can disguise text) removed. Returns whether it was cut.
pub fn plain_notes(raw: &str) -> (String, bool) {
    let mut out = String::new();
    let mut cut = false;
    for ch in raw.chars() {
        let keep = match ch {
            '\n' | '\t' => ch,
            '\r' => continue,
            c if c.is_control() => continue,
            // Bidirectional embeddings, overrides and isolates.
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' => continue,
            c => c,
        };
        if out.len() + keep.len_utf8() > MAX_NOTES_BYTES {
            cut = true;
            break;
        }
        out.push(keep);
    }
    (out.trim().to_owned(), cut)
}

/// One line of at most `max` characters, control characters replaced.
fn bounded_line(s: &str, max: usize) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

fn read_stored(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Stored>(&b).ok())
        .is_some_and(|s| s.check_on_start)
}

fn write_stored(path: &Path, on: bool) -> CoreResult<()> {
    let io =
        |what: &str, e: std::io::Error| MochiError::new(ErrorCode::IoError, format!("{what}: {e}"));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io("creating the settings folder", e))?;
    }
    let json = serde_json::to_vec(&Stored { check_on_start: on })
        .map_err(|e| MochiError::new(ErrorCode::IoError, e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| io("saving the update settings", e))?;
    std::fs::rename(&tmp, path).map_err(|e| io("saving the update settings", e))
}

// ---- The real backend: tauri-plugin-updater ---------------------------------

/// Classify an updater error. Signature problems are kept apart from network
/// problems so the window never reports a refused update as "offline".
pub fn classify(error: &tauri_plugin_updater::Error) -> UpdateFailureKind {
    use tauri_plugin_updater::Error as E;
    match error {
        E::Minisign(_)
        | E::Base64(_)
        | E::SignatureUtf8(_)
        | E::SignedVersionMismatch { .. }
        | E::MissingSignedVersion => UpdateFailureKind::Signature,
        E::Reqwest(e) if e.is_connect() || e.is_timeout() => UpdateFailureKind::Offline,
        E::Reqwest(_)
        | E::Network(_)
        | E::ReleaseNotFound
        | E::Serialization(_)
        | E::Semver(_)
        | E::TargetNotFound(_)
        | E::TargetsNotFound(_)
        | E::UnsupportedArch
        | E::UnsupportedOs => UpdateFailureKind::Server,
        E::AuthenticationFailed
        | E::DebInstallFailed
        | E::PackageInstallFailed
        | E::InvalidUpdaterFormat
        | E::Io(_)
        | E::TempDirNotFound
        | E::TempDirNotOnSameMountPoint
        | E::PreviousAppNotRestored(_)
        | E::FailedToDetermineExtractPath
        | E::BinaryNotFoundInArchive => UpdateFailureKind::Install,
        _ => UpdateFailureKind::Other,
    }
}

fn from_plugin(error: &tauri_plugin_updater::Error) -> Failure {
    Failure::new(classify(error), error.to_string())
}

/// The package this binary was bundled as, from the marker `tauri build`
/// writes into it.
fn running_package() -> Package {
    use tauri::utils::config::BundleType;
    use tauri::utils::platform::bundle_type;
    match bundle_type() {
        Some(BundleType::Nsis) => Package::Nsis,
        Some(BundleType::Deb) => Package::Deb,
        _ => Package::Other,
    }
}

/// The updater plugin, driven from Rust.
pub struct PluginBackend {
    app: tauri::AppHandle,
    pending: Mutex<Option<tauri_plugin_updater::Update>>,
}

impl PluginBackend {
    pub fn new(app: tauri::AppHandle) -> Self {
        PluginBackend {
            app,
            pending: Mutex::new(None),
        }
    }
}

impl Backend for PluginBackend {
    fn package(&self) -> Package {
        running_package()
    }

    fn current_version(&self) -> String {
        self.app.package_info().version.to_string()
    }

    fn check(&self) -> std::result::Result<Option<Candidate>, Failure> {
        use tauri_plugin_updater::UpdaterExt;
        let updater = self.app.updater().map_err(|e| from_plugin(&e))?;
        let found = tauri::async_runtime::block_on(updater.check()).map_err(|e| from_plugin(&e))?;
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        *pending = found.clone();
        Ok(found.map(|u| Candidate {
            version: u.version,
            notes: u.body,
            date: u.date.map(|d| d.to_string()),
        }))
    }

    fn install(
        &self,
        version: &str,
        progress: &dyn Fn(u64, Option<u64>),
        cancelled: &dyn Fn() -> bool,
    ) -> std::result::Result<(), Failure> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::time::Duration;

        let update = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .filter(|u| u.version == version)
            .ok_or_else(|| {
                Failure::new(
                    UpdateFailureKind::Other,
                    "that version is not the one on offer",
                )
            })?;

        #[derive(Default)]
        struct Shared {
            done: AtomicBool,
            bytes: AtomicU64,
            /// 0 when the server did not say.
            total: AtomicU64,
            result: Mutex<Option<std::result::Result<Vec<u8>, tauri_plugin_updater::Error>>>,
        }
        let shared = Arc::new(Shared::default());
        let task = {
            let shared = Arc::clone(&shared);
            let update = update.clone();
            tauri::async_runtime::spawn(async move {
                let r = update
                    .download(
                        |n, total| {
                            shared.bytes.fetch_add(n as u64, Ordering::Relaxed);
                            shared.total.store(total.unwrap_or(0), Ordering::Relaxed);
                        },
                        || {},
                    )
                    .await;
                *shared.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
                shared.done.store(true, Ordering::Release);
            })
        };
        while !shared.done.load(Ordering::Acquire) {
            if cancelled() {
                task.abort();
                return Err(Failure::cancelled());
            }
            let total = shared.total.load(Ordering::Relaxed);
            progress(
                shared.bytes.load(Ordering::Relaxed),
                (total > 0).then_some(total),
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        // The signature was verified inside `download`: an `Err` here means
        // nothing was handed to an installer.
        let bytes = shared
            .result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .ok_or_else(|| Failure::new(UpdateFailureKind::Other, "the download ended early"))?
            .map_err(|e| from_plugin(&e))?;
        // Past this point the installer owns the process (on Windows it
        // ends the app), so a cancellation is no longer honoured.
        update.install(bytes).map_err(|e| from_plugin(&e))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use mochi_core::job::{CancellationToken, ProgressEvent, ProgressSink};

    use super::*;

    /// A backend that never touches the network.
    struct Fake {
        package: Package,
        found: std::result::Result<Option<Candidate>, Failure>,
        installs: AtomicUsize,
        checks: AtomicUsize,
        install_result: std::result::Result<(), Failure>,
    }

    impl Fake {
        fn new(package: Package, found: std::result::Result<Option<Candidate>, Failure>) -> Self {
            Fake {
                package,
                found,
                installs: AtomicUsize::new(0),
                checks: AtomicUsize::new(0),
                install_result: Ok(()),
            }
        }
    }

    impl Backend for Fake {
        fn package(&self) -> Package {
            self.package
        }
        fn current_version(&self) -> String {
            "0.1.0".to_owned()
        }
        fn check(&self) -> std::result::Result<Option<Candidate>, Failure> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            self.found.clone()
        }
        fn install(
            &self,
            _version: &str,
            progress: &dyn Fn(u64, Option<u64>),
            cancelled: &dyn Fn() -> bool,
        ) -> std::result::Result<(), Failure> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            if cancelled() {
                return Err(Failure::cancelled());
            }
            progress(10, Some(20));
            self.install_result.clone()
        }
    }

    fn candidate(version: &str, notes: Option<&str>) -> Candidate {
        Candidate {
            version: version.to_owned(),
            notes: notes.map(str::to_owned),
            date: None,
        }
    }

    fn service(fake: Fake, dir: &Path) -> (Updates, Arc<Fake>) {
        let fake = Arc::new(fake);
        let updates = Updates::default();
        updates.configure(dir.to_path_buf(), fake.clone());
        (updates, fake)
    }

    struct NoProgress;
    impl ProgressSink for NoProgress {
        fn report(&self, _event: &ProgressEvent) {}
    }

    fn run(updates: &Updates, version: &str, token: &CancellationToken) -> CoreResult<String> {
        let ctx = JobContext {
            progress: &NoProgress,
            cancel: token,
        };
        updates.run_install(version, &ctx)
    }

    #[test]
    fn install_modes_follow_the_package() {
        assert_eq!(install_mode(Package::Nsis), UpdateInstallMode::InPlace);
        assert_eq!(install_mode(Package::Deb), UpdateInstallMode::InPlace);
        assert_eq!(install_mode(Package::Other), UpdateInstallMode::Manual);
    }

    #[test]
    fn the_startup_check_is_off_until_turned_on_and_an_automatic_check_requests_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (u, fake) = service(Fake::new(Package::Deb, Ok(None)), dir.path());
        assert!(!u.settings().check_on_start);
        assert_eq!(u.check(true), UpdateCheckView::Skipped);
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0, "no request was made");
        // The explicit action still works with the setting off.
        assert!(matches!(u.check(false), UpdateCheckView::UpToDate { .. }));
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
        u.set_check_on_start(true).unwrap();
        assert!(matches!(u.check(true), UpdateCheckView::UpToDate { .. }));
        assert_eq!(fake.checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_setting_survives_a_restart_and_a_damaged_file_means_off() {
        let dir = tempfile::tempdir().unwrap();
        let (u, _) = service(Fake::new(Package::Deb, Ok(None)), dir.path());
        u.set_check_on_start(true).unwrap();
        let (again, _) = service(Fake::new(Package::Deb, Ok(None)), dir.path());
        assert!(again.settings().check_on_start);
        std::fs::write(dir.path().join(SETTINGS_FILE), b"{ not json").unwrap();
        let (damaged, _) = service(Fake::new(Package::Deb, Ok(None)), dir.path());
        assert!(!damaged.settings().check_on_start);
    }

    #[test]
    fn an_unconfigured_service_checks_nothing() {
        let u = Updates::default();
        assert!(!u.settings().check_on_start);
        assert!(matches!(
            u.check(false),
            UpdateCheckView::Failed {
                kind: UpdateFailureKind::Unavailable,
                ..
            }
        ));
        assert!(u.validate_install("1.0.0").is_err());
    }

    #[test]
    fn an_offer_carries_the_version_and_plain_text_notes() {
        let dir = tempfile::tempdir().unwrap();
        let notes = "<b>Fixes</b>\r\n- one\u{0007}\n<script>alert(1)</script>\u{202E}";
        let (u, _) = service(
            Fake::new(Package::Nsis, Ok(Some(candidate("0.2.0", Some(notes))))),
            dir.path(),
        );
        let UpdateCheckView::Available { offer } = u.check(false) else {
            panic!("expected an offer");
        };
        assert_eq!(offer.version, "0.2.0");
        assert_eq!(offer.current_version, "0.1.0");
        assert_eq!(offer.install_mode, UpdateInstallMode::InPlace);
        assert!(!offer.needs_admin_prompt);
        // The markup is kept as the literal text it is; the window renders it
        // as text. Control and bidirectional characters are gone.
        assert_eq!(
            offer.notes.as_deref(),
            Some("<b>Fixes</b>\n- one\n<script>alert(1)</script>")
        );
        assert_eq!(offer.download_page, RELEASES_PAGE);
    }

    #[test]
    fn huge_notes_are_cut_and_say_so() {
        let (text, cut) = plain_notes(&"a".repeat(MAX_NOTES_BYTES * 2));
        assert!(cut);
        assert_eq!(text.len(), MAX_NOTES_BYTES);
        let (text, cut) = plain_notes("short");
        assert!(!cut);
        assert_eq!(text, "short");
    }

    #[test]
    fn a_hostile_version_string_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for v in ["", "1.0.0 <img src=x>", "v\n1", &"9".repeat(200)] {
            let (u, _) = service(
                Fake::new(Package::Deb, Ok(Some(candidate(v, None)))),
                dir.path(),
            );
            assert!(
                matches!(
                    u.check(false),
                    UpdateCheckView::Failed {
                        kind: UpdateFailureKind::Server,
                        ..
                    }
                ),
                "{v:?}"
            );
        }
    }

    #[test]
    fn a_failed_check_is_a_failure_with_its_kind_never_up_to_date() {
        let dir = tempfile::tempdir().unwrap();
        let (u, _) = service(
            Fake::new(
                Package::Deb,
                Err(Failure::new(UpdateFailureKind::Offline, "no route")),
            ),
            dir.path(),
        );
        assert_eq!(
            u.check(false),
            UpdateCheckView::Failed {
                kind: UpdateFailureKind::Offline,
                detail: "no route".to_owned()
            }
        );
    }

    #[test]
    fn nothing_installs_without_the_version_the_user_was_shown() {
        let dir = tempfile::tempdir().unwrap();
        let (u, fake) = service(
            Fake::new(Package::Deb, Ok(Some(candidate("0.2.0", None)))),
            dir.path(),
        );
        let token = CancellationToken::new();
        // Before any check.
        assert!(run(&u, "0.2.0", &token).is_err());
        u.check(false);
        // A different version than the one on offer.
        assert!(run(&u, "9.9.9", &token).is_err());
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
        assert!(run(&u, "0.2.0", &token).is_ok());
        assert_eq!(fake.installs.load(Ordering::SeqCst), 1);
        // The offer is spent.
        assert!(run(&u, "0.2.0", &token).is_err());
    }

    #[test]
    fn a_build_not_installed_from_a_package_does_not_install_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (u, fake) = service(
            Fake::new(Package::Other, Ok(Some(candidate("0.2.0", None)))),
            dir.path(),
        );
        let UpdateCheckView::Available { offer } = u.check(false) else {
            panic!("expected an offer");
        };
        assert_eq!(offer.install_mode, UpdateInstallMode::Manual);
        let err = run(&u, "0.2.0", &CancellationToken::new()).unwrap_err();
        assert_eq!(err.code, ErrorCode::UnsupportedFeature);
        assert!(err.message.contains(RELEASES_PAGE));
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_signature_failure_refuses_the_update_in_plain_words() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = Fake::new(Package::Nsis, Ok(Some(candidate("0.2.0", None))));
        fake.install_result = Err(Failure::new(UpdateFailureKind::Signature, "bad"));
        let (u, _) = service(fake, dir.path());
        u.check(false);
        let err = run(&u, "0.2.0", &CancellationToken::new()).unwrap_err();
        assert!(err.message.contains("refused"), "{}", err.message);
        assert!(err.message.contains("nothing was installed"));
        for word in ["verified!", "trusted", "safe", "secure"] {
            assert!(!err.message.contains(word), "{word}");
        }
    }

    #[test]
    fn cancelling_stops_the_install_with_the_cancelled_code() {
        let dir = tempfile::tempdir().unwrap();
        let (u, _) = service(
            Fake::new(Package::Deb, Ok(Some(candidate("0.2.0", None)))),
            dir.path(),
        );
        u.check(false);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            run(&u, "0.2.0", &token).unwrap_err().code,
            ErrorCode::Cancelled
        );
    }

    #[test]
    fn plugin_errors_are_classified_and_signatures_are_not_called_offline() {
        use tauri_plugin_updater::Error as E;
        assert_eq!(
            classify(&E::SignatureUtf8("x".into())),
            UpdateFailureKind::Signature
        );
        assert_eq!(
            classify(&E::SignedVersionMismatch {
                signed: "1".into(),
                announced: "2".into()
            }),
            UpdateFailureKind::Signature
        );
        assert_eq!(
            classify(&E::MissingSignedVersion),
            UpdateFailureKind::Signature
        );
        assert_eq!(classify(&E::ReleaseNotFound), UpdateFailureKind::Server);
        assert_eq!(
            classify(&E::Network("status 404".into())),
            UpdateFailureKind::Server
        );
        assert_eq!(
            classify(&E::AuthenticationFailed),
            UpdateFailureKind::Install
        );
        assert_eq!(
            classify(&E::PackageInstallFailed),
            UpdateFailureKind::Install
        );
    }

    #[test]
    fn failure_text_is_one_bounded_line() {
        let f = Failure::new(UpdateFailureKind::Other, format!("a\nb{}", "x".repeat(500)));
        assert!(!f.detail.contains('\n'));
        assert!(f.detail.chars().count() <= 200);
    }
}
