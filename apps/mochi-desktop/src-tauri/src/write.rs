//! Writing from the desktop app (plan D2): staging what to add, and the jobs
//! that create an archive, add to one, and remove entries from its current
//! snapshot. Thin over `mochi-core` (AGENTS.md): the walk, names, and
//! attributes are `mochi_core::import`, the commit is `ArchiveWriter`; this
//! module keeps the user's selection and converts results. Independent of
//! Tauri (the window and the dialogs are wired in `lib.rs` and `commands.rs`),
//! so it is tested without a window.
//!
//! # The webview names no path
//!
//! Sources come from a native dialog shown by Rust, or from the window's drop
//! event, which Rust receives. They are kept here, and the webview sees
//! only display names ([`StagedView`]). A script in the webview therefore
//! cannot make the app read a file the user did not pick or drop, and the
//! app needs no filesystem or dialog permission.
//!
//! # Rules (delegated decisions, recorded in plan D2)
//!
//! * One staged set per window. A job copies it when it starts and, once
//!   its commit is published, removes exactly what it stored; a failed or
//!   cancelled job leaves it for another try.
//! * A commit is `LOCAL_COMMITTED` and nothing more (spec §5.3, §23.3 #6).
//! * A cancelled job leaves the previous head valid (spec §5.1): the
//!   writer publishes in one step at the end, and creation publishes a
//!   temporary file only on success.
//! * A new archive can also be a plain ZIP, `.tar`, `.tar.gz`, or `.tar.zst`
//!   (plan O31): the `ExportArchive` job hands the staged sources to
//!   `mochi-export` and reports what was written, never a commit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::descriptor::Profile;
use mochi_core::import::{
    import_recording, Import, ImportOptions, ImportedEntry, SourceKind, SourceMeta, SourceTree,
};
use mochi_core::job::JobContext;
use mochi_core::keys::KeySession;
use mochi_core::object::OsIds;
use mochi_core::publish::{
    ArchiveWriter, CommitOutcome, FileSource, PublishDurability, ReadOptions, TailRepair,
    Transaction, WriterOptions,
};
use mochi_core::sources::check_removable;
use mochi_core::storage::os::move_sources::{move_removable, MovedSources};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsSourceTree};
use mochi_core::{ErrorCode, MochiError, Result};
use tauri::DragDropEvent;

use crate::browse::{parse_key, Archives};
use crate::ipc::{
    ArchiveFormat, ArchiveHandle, CommitOperation, CommitReport, CompressionPreset, DropEvent,
    ExportReportView, IpcError, JobRequest, NameText, PathKey, Secret, SkippedView, StageOutcome,
    StagedItem, StagedKind, StagedView,
};

/// Most sources one staged set holds (a folder counts once).
pub const MAX_STAGED: usize = 1000;

/// Most entries one delete may name.
pub const MAX_DELETE_KEYS: usize = 10_000;

/// The phase name of a delete's progress events.
pub const DELETE_PHASE: &str = "delete";

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Only plain data lives behind these locks; a panic elsewhere cannot
    // leave it half-updated.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl CompressionPreset {
    /// The Zstandard level; `None` is the writer's default.
    pub fn zstd_level(self) -> Option<i32> {
        match self {
            CompressionPreset::Turbo => Some(-4),
            CompressionPreset::VeryFast => Some(-1),
            CompressionPreset::Fast => Some(1),
            CompressionPreset::Balanced => None,
            CompressionPreset::Maximum => Some(19),
            CompressionPreset::Ultra => Some(20),
            CompressionPreset::Extreme => Some(22),
        }
    }
}

// ---- the source tree --------------------------------------------------------

/// [`OsSourceTree`], paced in E2E builds so that a test can cancel a job
/// that would otherwise finish before the click (`MOCHI_E2E_PACE_MS`,
/// milliseconds per entry). Shipped builds never read the variable.
pub struct Tree {
    os: OsSourceTree,
}

impl Default for Tree {
    fn default() -> Self {
        Tree { os: OsSourceTree }
    }
}

impl SourceTree for Tree {
    type Node = PathBuf;

    fn meta(&self, node: &PathBuf) -> Result<SourceMeta> {
        #[cfg(feature = "e2e")]
        if let Some(ms) = std::env::var("MOCHI_E2E_PACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        self.os.meta(node)
    }

    fn children(&self, node: &PathBuf) -> Result<Vec<(Vec<u8>, PathBuf)>> {
        self.os.children(node)
    }

    fn source(&self, node: &PathBuf, len: u64) -> Result<Arc<dyn FileSource>> {
        self.os.source(node, len)
    }
}

// ---- staging ----------------------------------------------------------------

#[derive(Debug, Clone)]
struct Staged {
    name: ArchivePath,
    path: PathBuf,
    kind: SourceKind,
}

#[derive(Debug, Default)]
struct StagedSet {
    items: Vec<Staged>,
    destination: Option<PathBuf>,
    /// The format of the next new archive (plan O31).
    format: ArchiveFormat,
}

/// What the user has chosen to add, and where a new archive would go.
#[derive(Debug, Default)]
pub struct Staging {
    inner: Mutex<StagedSet>,
    /// A single `.mochi` file dropped on the window: opened on request, not
    /// staged (`on_drag_drop`). Only Rust ever holds the path.
    dropped_archive: Mutex<Option<PathBuf>>,
    /// The folder the last export was written into: what "Show in folder"
    /// opens (the webview names no path).
    last_export_folder: Mutex<Option<PathBuf>>,
}

fn view_of(set: &StagedSet) -> StagedView {
    StagedView {
        items: set
            .items
            .iter()
            .map(|s| StagedItem {
                name: NameText::of(s.name.as_stored()),
                kind: match s.kind {
                    SourceKind::File => StagedKind::File,
                    SourceKind::Directory => StagedKind::Folder,
                    SourceKind::Symlink | SourceKind::Other => StagedKind::Other,
                },
            })
            .collect(),
        destination: set
            .destination
            .as_ref()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        default_destination: default_destination(set).map(|p| p.display().to_string()),
        format: set.format,
    }
}

/// Where a new archive goes when the user did not choose: next to the first
/// staged item, named after it, never over an existing file (`name.mochi`,
/// then `name (2).mochi`, and so on; the extension is the chosen format's). `None` when there is nothing staged,
/// or the item has no usable parent folder or Unicode name; the save dialog
/// then has to be used. Nothing is created here.
fn default_destination(set: &StagedSet) -> Option<PathBuf> {
    if set.destination.is_some() {
        return None;
    }
    let first = set.items.first()?;
    let parent = first.path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let stem = first.path.file_stem()?.to_str()?;
    let ext = set.format.extension();
    (1u32..=99).find_map(|n| {
        let name = if n == 1 {
            format!("{stem}{ext}")
        } else {
            format!("{stem} ({n}){ext}")
        };
        let candidate = parent.join(name);
        (!candidate.exists()).then_some(candidate)
    })
}

fn stage_one(path: &Path) -> Result<Staged> {
    let tree = OsSourceTree;
    let (name, node) = tree.input(path)?;
    let meta = tree.meta(&node)?;
    Ok(Staged {
        name,
        path: node,
        kind: meta.kind,
    })
}

/// What a job takes from the staged set when it starts.
#[derive(Debug, Clone)]
pub struct Taken {
    items: Vec<Staged>,
    destination: Option<PathBuf>,
}

impl Staging {
    pub fn view(&self) -> StagedView {
        view_of(&lock(&self.inner))
    }

    /// Stage `paths`. A path that cannot be staged is reported and the rest
    /// still are: the same top-level name twice (two inputs would collide
    /// in the archive), a path with no name, one that cannot be read. A path
    /// already staged is ignored.
    pub fn add(&self, paths: &[PathBuf]) -> StageOutcome {
        let mut problems = Vec::new();
        let mut set = lock(&self.inner);
        for path in paths {
            if set.items.iter().any(|s| s.path == *path) {
                continue;
            }
            let staged = stage_one(path).and_then(|s| {
                if set.items.len() >= MAX_STAGED {
                    return Err(MochiError::new(
                        ErrorCode::LimitExceeded,
                        format!("at most {MAX_STAGED} items can be staged at once"),
                    ));
                }
                if let Some(other) = set.items.iter().find(|o| o.name == s.name) {
                    return Err(invalid(format!(
                        "{} would be stored under the same name as {}; add one of them \
                         in a separate step",
                        path.display(),
                        other.path.display()
                    )));
                }
                Ok(s)
            });
            match staged {
                Ok(s) => set.items.push(s),
                Err(e) => problems.push(IpcError::from(e)),
            }
        }
        StageOutcome {
            staged: view_of(&set),
            problems,
        }
    }

    /// Remove item `index` (as listed in the view). Out of range is
    /// `INVALID_ARGUMENT`.
    pub fn remove(&self, index: usize) -> Result<StagedView> {
        let mut set = lock(&self.inner);
        if index >= set.items.len() {
            return Err(invalid("no such staged item"));
        }
        set.items.remove(index);
        Ok(view_of(&set))
    }

    pub fn clear(&self) -> StagedView {
        let mut set = lock(&self.inner);
        let format = set.format;
        *set = StagedSet {
            format,
            ..StagedSet::default()
        };
        view_of(&set)
    }

    /// Choose the format of the next new archive. A destination already
    /// chosen keeps its folder and name and takes the format's extension.
    pub fn set_format(&self, format: ArchiveFormat) -> StagedView {
        let mut set = lock(&self.inner);
        set.format = format;
        if let Some(d) = set.destination.take() {
            set.destination = Some(format.path_for(&d));
        }
        view_of(&set)
    }

    /// The format of the next new archive.
    pub(crate) fn format(&self) -> ArchiveFormat {
        lock(&self.inner).format
    }

    /// The folder the last export went into, for "Show in folder".
    pub(crate) fn last_export_folder(&self) -> Option<PathBuf> {
        lock(&self.last_export_folder).clone()
    }

    /// The destination chosen with the save dialog, if any (D5 re-encryption
    /// writes there too).
    pub(crate) fn destination(&self) -> Option<PathBuf> {
        lock(&self.inner).destination.clone()
    }

    /// Forget the destination once something was written there, if it is
    /// still the one that was used.
    pub(crate) fn consume_destination(&self, used: &Path) {
        let mut set = lock(&self.inner);
        if set.destination.as_deref() == Some(used) {
            set.destination = None;
        }
    }

    /// Where the save dialog should start: the default destination.
    pub(crate) fn suggested_destination(&self) -> Option<PathBuf> {
        let set = lock(&self.inner);
        set.destination
            .clone()
            .or_else(|| default_destination(&set))
    }

    /// Remember a dropped `.mochi` file until the window asks to open it.
    fn set_dropped_archive(&self, path: PathBuf) {
        *lock(&self.dropped_archive) = Some(path);
    }

    /// The dropped archive, once.
    pub(crate) fn take_dropped_archive(&self) -> Option<PathBuf> {
        lock(&self.dropped_archive).take()
    }

    /// Stage the dropped archive file as an ordinary source (to add it to an
    /// archive) instead of opening it. Takes the held file, once. If it cannot
    /// be staged (for example another source has the same name) that is the
    /// error, and nothing is staged.
    pub(crate) fn stage_dropped_archive(&self) -> std::result::Result<StagedView, IpcError> {
        let path = self.take_dropped_archive().ok_or_else(|| IpcError {
            code: ErrorCode::InvalidArgument.as_str().to_owned(),
            message: "no dropped file is waiting".to_owned(),
        })?;
        let StageOutcome { staged, problems } = self.add(&[path]);
        match problems.into_iter().next() {
            Some(problem) => Err(problem),
            None => Ok(staged),
        }
    }

    pub fn set_destination(&self, path: PathBuf) -> StagedView {
        let mut set = lock(&self.inner);
        set.destination = Some(path);
        view_of(&set)
    }

    fn take(&self) -> Taken {
        let set = lock(&self.inner);
        Taken {
            items: set.items.clone(),
            destination: set
                .destination
                .clone()
                .or_else(|| default_destination(&set)),
        }
    }

    /// After a published commit: drop what it stored, and the destination
    /// if it was the one used. Anything staged since stays.
    fn consume(&self, taken: &Taken, used_destination: bool) {
        let mut set = lock(&self.inner);
        set.items
            .retain(|s| !taken.items.iter().any(|t| t.path == s.path));
        if used_destination && set.destination == taken.destination {
            set.destination = None;
        }
    }
}

// ---- drag and drop ----------------------------------------------------------

/// Where drop events go (a Tauri `Channel` in the app).
pub trait DropSink: Send + Sync + 'static {
    /// `false` once nobody listens.
    fn send(&self, event: DropEvent) -> bool;
}

/// The window's current listener for drop events, if any.
#[derive(Default)]
pub struct DropWatch {
    /// The listener and the announcement nobody has heard yet, under one lock:
    /// an announcement either reaches the listener that is there, or is kept
    /// for the next one, and `set` replays it and installs its sink in the
    /// same step, so an announcement made between the two cannot be stranded.
    state: Mutex<DropState>,
}

#[derive(Default)]
struct DropState {
    sink: Option<Arc<dyn DropSink>>,
    /// An announcement (D6) made while nobody listened, replayed to the next
    /// listener: an "Add to archive" from the shell can arrive before the
    /// window has started listening. The newest replaces an older one.
    waiting: Option<DropEvent>,
}

impl DropWatch {
    /// Listen with `sink`, replacing any earlier listener (a reloaded page).
    /// What was announced while nobody listened is delivered first; if the
    /// new sink is already gone it stays waiting for the next one.
    pub fn set(&self, sink: Arc<dyn DropSink>) {
        let mut state = lock(&self.state);
        if let Some(event) = state.waiting.take() {
            if !sink.send(event.clone()) {
                state.waiting = Some(event);
                state.sink = None;
                return;
            }
        }
        state.sink = Some(sink);
    }

    fn emit(&self, event: DropEvent) {
        let mut state = lock(&self.state);
        if let Some(sink) = state.sink.clone() {
            if !sink.send(event) {
                state.sink = None;
            }
        }
    }

    /// Tell the window that items were staged from outside it (as a drop
    /// would): the window opens its add panel. Held for the next listener if
    /// there is none.
    pub fn announce_staged(&self, outcome: StageOutcome) {
        let event = DropEvent::Dropped {
            staged: outcome.staged,
            problems: outcome.problems,
            from_shell: true,
        };
        let mut state = lock(&self.state);
        let delivered = state
            .sink
            .clone()
            .is_some_and(|sink| sink.send(event.clone()));
        if !delivered {
            state.sink = None;
            state.waiting = Some(event);
        }
    }
}

/// A single dropped file that is an archive: a `.mochi` file, or a ZIP, 7z,
/// RAR, tar, tar.gz or tar.zst file by its name (`route::dropped_archive`;
/// MOCHI wins by its magic). The window opens it, or stages it to add it to an
/// open archive, instead of adding it to a new archive. The name is only a
/// hint; opening checks the contents.
fn is_archive_file(path: &Path) -> bool {
    crate::route::dropped_archive(path).is_some()
}

/// Translate the window's drag-and-drop event: hovering lights the drop
/// target, a drop stages what was dropped. Nothing is read or written here.
pub fn on_drag_drop(event: &DragDropEvent, staging: &Staging, watch: &DropWatch) {
    match event {
        DragDropEvent::Enter { .. } => watch.emit(DropEvent::Hover { active: true }),
        DragDropEvent::Leave => watch.emit(DropEvent::Hover { active: false }),
        DragDropEvent::Drop { paths, .. } => {
            if let [only] = paths.as_slice() {
                if is_archive_file(only) {
                    staging.set_dropped_archive(only.clone());
                    watch.emit(DropEvent::Hover { active: false });
                    watch.emit(DropEvent::Archive {
                        name: only
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        foreign: crate::route::dropped_archive(only)
                            == Some(crate::route::Route::Foreign),
                    });
                    return;
                }
            }
            let StageOutcome { staged, problems } = staging.add(paths);
            watch.emit(DropEvent::Hover { active: false });
            watch.emit(DropEvent::Dropped {
                staged,
                problems,
                from_shell: false,
            });
        }
        _ => {}
    }
}

// ---- jobs -------------------------------------------------------------------

/// What the jobs need besides their request.
#[derive(Clone, Default)]
pub struct Services {
    pub archives: Arc<Archives>,
    pub staging: Arc<Staging>,
    /// The folder the user chose to extract into (D3).
    pub destination: Arc<crate::extract::Destination>,
    /// Foreign archives (D8).
    pub foreign: Arc<crate::foreign::Foreign>,
    /// The archive chosen for testing or repair, kept reports, and the
    /// repair's setup (D4).
    pub checks: Arc<crate::check::Checks>,
    /// The in-app updater (T13).
    pub updates: Arc<crate::update::Updates>,
    /// "Open the folder when done" (owner request 2026-11-10), shared with the compact
    /// window. `None` opens nothing (the services of tests that do not need it).
    pub after_extract: Option<Arc<crate::afterextract::AfterExtract>>,
    /// The window's MOCHI Find, for retrieving what it found (B.2.21). Set
    /// per window by `JobRegistry::services_for`.
    pub finder: Option<Arc<crate::find::Finder>>,
}

/// What a create asks for, gathered from the request.
struct CreateChoices<'a> {
    preset: CompressionPreset,
    tar_compatible: bool,
    passphrase: Option<&'a Secret>,
    recovery_percent: Option<u16>,
    /// Move the sources to the Bin after the commit (Annex B.2.23 item 3).
    delete_sources: bool,
}

/// What a finished job reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobDone {
    pub summary: String,
    pub commit: Option<Box<CommitReport>>,
    /// Set by the extract job (D3).
    pub extract: Option<Box<crate::ipc::ExtractReport>>,
    /// Set by the foreign-archive test job (D8).
    pub foreign_test: Option<Box<crate::ipc::ForeignTestReport>>,
    /// Set by the test and repair jobs (D4).
    pub check: Option<Box<crate::ipc::CheckResult>>,
    /// Set by the search job (D5).
    pub search: Option<Box<crate::ipc::SearchResultView>>,
    /// Set by the rekey jobs (D5).
    pub rekey: Option<Box<crate::ipc::RekeyReport>>,
    /// Set by the export job (plan O31).
    pub export: Option<Box<ExportReportView>>,
    /// Set by the virus scan job.
    pub scan: Option<Box<crate::scan::ScanReportView>>,
    /// Set by MOCHI Find's retrieve job (B.2.21).
    pub retrieve: Option<Box<crate::retrieve::RetrieveReportView>>,
}

impl JobDone {
    /// A result with only an empty summary (set what the job reports).
    pub fn empty() -> JobDone {
        JobDone {
            summary: String::new(),
            commit: None,
            extract: None,
            foreign_test: None,
            check: None,
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
        }
    }
}

impl Services {
    /// Refuse a request that cannot work, before a job starts.
    pub fn validate(&self, request: &JobRequest) -> Result<()> {
        match request {
            JobRequest::SelfTest { .. } => Ok(()),
            JobRequest::InstallUpdate { version } => self.updates.validate_install(version),
            JobRequest::TestForeign { .. } | JobRequest::ExtractForeign { .. } => {
                self.foreign.validate(&self.destination, request)
            }
            JobRequest::ScanArchive { .. } => crate::scan::validate(&self.archives, request),
            JobRequest::ExtractArchive { .. } => {
                crate::extract::validate(&self.archives, &self.destination, request)
            }
            JobRequest::RetrieveFound { .. } => {
                crate::retrieve::validate(self.finder.as_deref(), &self.destination, request)
            }
            JobRequest::TestArchive { .. }
            | JobRequest::PlanRepair { .. }
            | JobRequest::ApplyRepair { .. } => self.checks.validate(request),
            JobRequest::CreateArchive {
                tar_compatible,
                passphrase,
                recovery_percent,
                ..
            } => {
                if passphrase.is_some() && *tar_compatible {
                    return Err(invalid(
                        "an encrypted archive cannot also be readable with standard zstd + tar: \
                         the sealed objects are skipped by generic decoders (spec §7.3)",
                    ));
                }
                if let Some(secret) = passphrase {
                    // Refuse an empty or oversized passphrase before anything
                    // starts; the error never contains it.
                    secret.passphrase()?;
                }
                if let Some(p) = recovery_percent {
                    if !(1..=100).contains(p) {
                        return Err(invalid("a recovery record takes 1 to 100 percent"));
                    }
                }
                let t = self.staging.take();
                if t.items.is_empty() {
                    return Err(invalid("nothing is staged: choose files or a folder first"));
                }
                if t.destination.is_none() {
                    return Err(invalid("choose where to save the new archive first"));
                }
                Ok(())
            }
            JobRequest::ExportArchive { format, level } => {
                if format.export().is_none() {
                    return Err(invalid(
                        "ExportArchive writes ZIP, tar, tar.gz, and tar.zst; use CreateArchive for .mochi",
                    ));
                }
                if !format.offers(*level) {
                    return Err(invalid(
                        "ZIP and tar.gz offer the Fast, Balanced and Maximum levels only",
                    ));
                }
                let t = self.staging.take();
                if t.items.is_empty() {
                    return Err(invalid("nothing is staged: choose files or a folder first"));
                }
                if t.destination.is_none() {
                    return Err(invalid("choose where to save the new archive first"));
                }
                Ok(())
            }
            JobRequest::SearchArchive { .. } => crate::search::validate(&self.archives, request),
            JobRequest::AddPassphrase { .. }
            | JobRequest::RemovePassphrase { .. }
            | JobRequest::Reencrypt { .. } => {
                crate::rekey::validate(&self.archives, &self.staging, request)
            }
            JobRequest::AddToArchive { handle, .. } => {
                self.archives.path(*handle)?;
                if self.staging.take().items.is_empty() {
                    return Err(invalid("nothing is staged: choose files or a folder first"));
                }
                Ok(())
            }
            JobRequest::SetArchiveComment { handle, comment } => {
                let path = self.archives.path(*handle)?;
                if let Some(text) = comment {
                    mochi_core::commit::check_comment(text).map_err(invalid)?;
                    if crate::browse::head_encrypted(&path) {
                        return Err(MochiError::new(
                            ErrorCode::UnsupportedFeature,
                            "an Encrypted archive cannot have a comment: it would be readable \
                             without the passphrase",
                        ));
                    }
                }
                Ok(())
            }
            JobRequest::DeleteFromArchive { handle, keys } => {
                self.archives.path(*handle)?;
                if keys.is_empty() || keys.len() > MAX_DELETE_KEYS {
                    return Err(invalid(format!(
                        "a delete names 1 to {MAX_DELETE_KEYS} entries"
                    )));
                }
                keys.iter().try_for_each(|k| parse_key(k).map(|_| ()))
            }
        }
    }

    /// Run a write or extract job. Only the D2 and D3 requests; the caller
    /// routes the rest.
    pub fn run(&self, request: &JobRequest, ctx: &JobContext<'_>) -> Option<Result<JobDone>> {
        Some(match request {
            JobRequest::SelfTest { .. } => return None,
            JobRequest::InstallUpdate { version } => {
                self.updates
                    .run_install(version, ctx)
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
                    })
            }
            JobRequest::TestForeign { .. } => self.foreign.run(&self.destination, request, ctx),
            JobRequest::ExtractForeign { .. } => self
                .foreign
                .run(&self.destination, request, ctx)
                .map(|done| self.extracted(done)),
            JobRequest::ExtractArchive { .. } => {
                crate::extract::run(&self.archives, &self.destination, request, ctx)
                    .map(|done| self.extracted(done))
            }
            JobRequest::RetrieveFound { .. } => {
                crate::retrieve::run(self.finder.as_deref(), &self.destination, request, ctx)
                    .map(|done| self.extracted(done))
            }
            JobRequest::TestArchive { .. }
            | JobRequest::PlanRepair { .. }
            | JobRequest::ApplyRepair { .. } => self.checks.run(request, ctx),
            JobRequest::CreateArchive {
                preset,
                tar_compatible,
                passphrase,
                recovery_percent,
                delete_sources,
            } => self.create(
                CreateChoices {
                    preset: *preset,
                    tar_compatible: *tar_compatible,
                    passphrase: passphrase.as_ref(),
                    recovery_percent: *recovery_percent,
                    delete_sources: delete_sources.unwrap_or(false),
                },
                ctx,
            ),
            JobRequest::ExportArchive { format, level } => self.export(*format, *level, ctx),
            JobRequest::SearchArchive { .. } => crate::search::run(&self.archives, request, ctx),
            JobRequest::AddPassphrase { .. }
            | JobRequest::RemovePassphrase { .. }
            | JobRequest::Reencrypt { .. } => {
                crate::rekey::run(&self.archives, &self.staging, request, ctx)
            }
            JobRequest::ScanArchive { .. } => crate::scan::run(&self.archives, request, ctx),
            JobRequest::AddToArchive {
                handle,
                delete_sources,
            } => self.add(*handle, delete_sources.unwrap_or(false), ctx),
            JobRequest::DeleteFromArchive { handle, keys } => self.delete(*handle, keys, ctx),
            JobRequest::SetArchiveComment { handle, comment } => {
                self.set_comment(*handle, comment.as_deref(), ctx)
            }
        })
    }

    /// An extraction in an archive window ended without an error: open the folder it went
    /// into when the user wants that, and say so in its report. (The compact window's quick
    /// verbs do the same through their own host, `quick.rs`.)
    fn extracted(&self, mut done: JobDone) -> JobDone {
        if let (Some(after), Some(report), Some(folder)) = (
            self.after_extract.as_ref(),
            done.extract.as_mut(),
            self.destination.get(),
        ) {
            report.opened_folder = after.done(&folder);
        }
        if let (Some(after), Some(report), Some(folder)) = (
            self.after_extract.as_ref(),
            done.retrieve.as_mut(),
            self.destination.get(),
        ) {
            if report.files + report.directories > 0 {
                report.opened_folder = after.done(&folder);
            }
        }
        done
    }

    fn create(&self, choices: CreateChoices<'_>, ctx: &JobContext<'_>) -> Result<JobDone> {
        let CreateChoices {
            preset,
            tar_compatible,
            passphrase,
            recovery_percent,
            delete_sources,
        } = choices;
        // The key is opened before anything is created (D20): a bad
        // passphrase leaves no file and no partial output.
        let read = match passphrase {
            Some(secret) => ReadOptions {
                keys: Some(KeySession::single(secret.passphrase()?)),
                ..ReadOptions::default()
            },
            None => ReadOptions::default(),
        };
        let encrypted = passphrase.is_some();
        let taken = self.staging.take();
        let destination = taken
            .destination
            .clone()
            .ok_or_else(|| invalid("choose where to save the new archive first"))?;
        let (import, entries) = gather_recording(&taken, Transaction::new(), ctx)?;
        let (mut dir, name) = location(&destination)?;
        let opts = WriterOptions {
            read: read.clone(),
            zstd_level: preset.zstd_level(),
            record_time: true,
            threads: crate::speed::writer_threads(),
            profile: (tar_compatible || encrypted || recovery_percent.is_some()).then_some(
                Profile {
                    tar_compatible,
                    encrypted,
                    redundancy: recovery_percent,
                },
            ),
            ..WriterOptions::default()
        };
        let (w, outcome) = ArchiveWriter::create_in(
            &mut dir,
            &name,
            Box::new(OsIds),
            opts,
            import.transaction.clone(),
            ctx,
        )?;
        crate::history::committed(w.archive_id(), &outcome);
        // The commit is published; a failure to close does not undo it.
        let _ = w.close();
        self.staging.consume(&taken, true);
        // Annex B.2.23 item 3: only now, and only what reads back.
        let sources = delete_sources.then(|| move_sources(&destination, &read, &entries, ctx));
        // The new archive opens with the same key session, so the user lands
        // in it unlocked without typing the passphrase again.
        let opened = self.archives.open(&destination, read);
        Ok(done(
            CommitOperation::Create,
            &outcome,
            &import,
            0,
            opened,
            sources,
        ))
    }

    /// Write the staged sources as a plain ZIP or tar file (plan O31). The
    /// report says what was written; it is not a commit and not a verification.
    fn export(
        &self,
        format: ArchiveFormat,
        level: CompressionPreset,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let export_format = format
            .export()
            .ok_or_else(|| invalid("not a ZIP or tar format"))?;
        let taken = self.staging.take();
        let destination = format.path_for(
            &taken
                .destination
                .clone()
                .ok_or_else(|| invalid("choose where to save the new archive first"))?,
        );
        let inputs: Vec<(ArchivePath, PathBuf)> = taken
            .items
            .iter()
            .map(|s| (s.name.clone(), s.path.clone()))
            .collect();
        let report = mochi_export::export(
            &Tree::default(),
            &inputs,
            &destination,
            &mochi_export::Options {
                format: export_format,
                level: level.export_level(),
            },
            ctx,
        )?;
        self.staging.consume(&taken, true);
        if let Some(folder) = destination.parent() {
            *lock(&self.staging.last_export_folder) = Some(folder.to_path_buf());
        }
        Ok(export_done(format, &destination, &report))
    }

    fn add(
        &self,
        handle: ArchiveHandle,
        delete_sources: bool,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let taken = self.staging.take();
        let path = self.archives.path(handle)?;
        let (import, entries) = gather_recording(&taken, Transaction::new(), ctx)?;
        if import.transaction.is_empty() {
            return Err(invalid(
                "nothing to add: the staged items contain no files or folders that are stored",
            ));
        }
        let read = self.archives.read_options(handle)?;
        let outcome = commit_to(&path, read.clone(), import.transaction.clone(), ctx)?;
        self.staging.consume(&taken, false);
        let sources = delete_sources.then(|| move_sources(&path, &read, &entries, ctx));
        let opened = self.archives.reopen(handle);
        Ok(done(
            CommitOperation::Add,
            &outcome,
            &import,
            0,
            opened,
            sources,
        ))
    }

    fn delete(
        &self,
        handle: ArchiveHandle,
        keys: &[PathKey],
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let wanted = keys
            .iter()
            .map(|k| parse_key(k))
            .collect::<Result<Vec<_>>>()?;
        let path = self.archives.path(handle)?;
        let (mut dir, name) = location(&path)?;
        let (mut w, _) = ArchiveWriter::open_append_in(
            &mut dir,
            &name,
            Box::new(OsIds),
            writer_options(self.archives.read_options(handle)?),
            TailRepair::Refuse,
        )?;
        // Each named entry and everything under it, against the head the
        // writer holds under its lock; children before their parent.
        let snapshot = w.catalog().replay(None)?;
        let mut doomed = BTreeSet::new();
        for d in &wanted {
            if snapshot.get(d).is_none() {
                return Err(invalid(
                    "an entry you chose is not in the archive's current snapshot; \
                     nothing was committed",
                ));
            }
            doomed.extend(
                snapshot
                    .iter()
                    .map(|(p, _)| p)
                    .filter(|p| p.is_descendant_of(d))
                    .cloned(),
            );
            doomed.insert(d.clone());
        }
        let total = doomed.len() as u64;
        let mut tx = Transaction::new();
        for (i, p) in doomed.iter().rev().enumerate() {
            ctx.check_cancelled()?;
            tx.delete(p.clone());
            ctx.report(DELETE_PHASE, i as u64 + 1, Some(total));
        }
        let outcome = w.commit(tx, ctx)?;
        crate::history::committed(w.archive_id(), &outcome);
        let _ = w.close();
        let opened = self.archives.reopen(handle);
        let empty = Import {
            transaction: Transaction::new(),
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: Vec::new(),
        };
        Ok(done(
            CommitOperation::Delete,
            &outcome,
            &empty,
            total,
            opened,
            None,
        ))
    }
}

impl Services {
    /// Set or clear the archive comment as one new commit (D29). Nothing else
    /// changes; the writer checks the text and refuses an Encrypted archive
    /// before anything is written.
    fn set_comment(
        &self,
        handle: ArchiveHandle,
        comment: Option<&str>,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let path = self.archives.path(handle)?;
        let (mut dir, name) = location(&path)?;
        let (mut w, _) = ArchiveWriter::open_append_in(
            &mut dir,
            &name,
            Box::new(OsIds),
            writer_options(self.archives.read_options(handle)?),
            TailRepair::Refuse,
        )?;
        let mut tx = Transaction::new();
        match comment {
            Some(text) => tx.set_comment(text),
            None => tx.clear_comment(),
        };
        let outcome = w.commit(tx, ctx)?;
        crate::history::committed(w.archive_id(), &outcome);
        let _ = w.close();
        let opened = self.archives.reopen(handle);
        let empty = Import {
            transaction: Transaction::new(),
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: Vec::new(),
        };
        Ok(done(
            CommitOperation::Comment,
            &outcome,
            &empty,
            0,
            opened,
            None,
        ))
    }
}

/// Appending options: the open archive's read options, which carry the key
/// session of an Encrypted archive (D5). The profile is the archive's own.
fn writer_options(read: ReadOptions) -> WriterOptions {
    WriterOptions {
        read,
        record_time: true,
        threads: crate::speed::writer_threads(),
        // Add, Delete, Comment append: wait for another writer if the user
        // asked to (Annex B.2.23).
        lock_wait: crate::speed::lock_wait(),
        ..WriterOptions::default()
    }
}

/// Walk the staged sources into `tx`, recording where each imported entry
/// came from, for "Delete files after archiving" (Annex B.2.23 item 3).
/// Nothing is written.
fn gather_recording(
    taken: &Taken,
    tx: Transaction,
    ctx: &JobContext<'_>,
) -> Result<(Import, Vec<ImportedEntry<PathBuf>>)> {
    let named: Vec<(ArchivePath, PathBuf)> = taken
        .items
        .iter()
        .map(|s| (s.name.clone(), s.path.clone()))
        .collect();
    import_recording(&Tree::default(), &named, tx, &ImportOptions::default(), ctx)
}

/// What "Delete files after archiving" did.
#[derive(Debug, Default)]
struct SourcesMoved {
    moved: u64,
    kept: Vec<SkippedView>,
}

/// Annex B.2.23 item 3, after the commit is published: move to the Recycle
/// Bin or Trash each source `mochi_core::sources::check_removable` lets go
/// (its archived copy read back and the source still matches it), then each
/// folder it lets go once it is empty (`storage::os::move_sources`, shared
/// with the command line); keep, with the reason, everything else. Never
/// deletes outright, and never fails the job: the commit stands whatever
/// happens here.
fn move_sources(
    archive: &Path,
    read: &ReadOptions,
    entries: &[ImportedEntry<PathBuf>],
    ctx: &JobContext<'_>,
) -> SourcesMoved {
    let checked = OsReadStorage::open(archive)
        .map_err(MochiError::from)
        .and_then(|src| check_removable(&src, read, entries, ctx));
    let done = match checked {
        Ok(c) => move_removable(&c, crate::trashbin::set_aside().as_ref()),
        Err(e) => MovedSources::all_kept(
            entries.iter().map(|e| &e.path),
            &format!(
                "nothing was moved: the new archive could not be read back ({})",
                e.message
            ),
        ),
    };
    SourcesMoved {
        moved: done.moved,
        kept: done
            .kept
            .into_iter()
            .map(|(path, reason)| SkippedView {
                path: NameText::of(path.as_stored()),
                reason,
            })
            .collect(),
    }
}

/// The directory holding `archive` and its file name.
pub(crate) fn location(archive: &Path) -> Result<(OsDir, String)> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            invalid(format!(
                "{}: the archive's file name must be valid Unicode",
                archive.display()
            ))
        })?
        .to_owned();
    // What a directory flush counts as on Windows (D30): the user's setting.
    Ok((
        OsDir::containing(archive)?.with_windows_flush(crate::speed::windows_flush()),
        name,
    ))
}

/// Append `tx` to the archive at `path` as one commit. An uncommitted tail
/// is refused (truncating one is explicit, in the command-line tool).
fn commit_to(
    path: &Path,
    read: ReadOptions,
    tx: Transaction,
    ctx: &JobContext<'_>,
) -> Result<CommitOutcome> {
    let (mut dir, name) = location(path)?;
    let (mut w, _) = ArchiveWriter::open_append_in(
        &mut dir,
        &name,
        Box::new(OsIds),
        writer_options(read),
        TailRepair::Refuse,
    )?;
    let outcome = w.commit(tx, ctx)?;
    crate::history::committed(w.archive_id(), &outcome);
    // The commit is published; a failure to close does not undo it.
    let _ = w.close();
    Ok(outcome)
}

/// The result of an export: what was written (plan O31).
pub(crate) fn export_done(
    format: ArchiveFormat,
    destination: &Path,
    report: &mochi_export::Report,
) -> JobDone {
    let file_name = destination.file_name().unwrap_or(destination.as_os_str());
    let view = ExportReportView {
        format,
        name: NameText::of(file_name.to_string_lossy().as_bytes()),
        location: NameText::of(
            crate::help::for_shell(destination)
                .to_string_lossy()
                .as_bytes(),
        ),
        files: report.files,
        directories: report.directories,
        bytes: report.bytes,
        output_bytes: report.output_bytes,
        skipped: report
            .skipped
            .iter()
            .map(|s| SkippedView {
                path: NameText::of(s.path.as_bytes()),
                reason: s.reason.clone(),
            })
            .collect(),
        durability_note: report.durability_note.clone(),
    };
    JobDone {
        summary: format!(
            "wrote {} files and {} folders to {}",
            report.files,
            report.directories,
            file_name.to_string_lossy()
        ),
        commit: None,
        extract: None,
        foreign_test: None,
        check: None,
        search: None,
        rekey: None,
        export: Some(Box::new(view)),
        scan: None,
        retrieve: None,
    }
}

fn done(
    operation: CommitOperation,
    outcome: &CommitOutcome,
    import: &Import,
    removed: u64,
    opened: Result<crate::ipc::ArchiveSummary>,
    sources: Option<SourcesMoved>,
) -> JobDone {
    let sources = sources.unwrap_or_default();
    let (archive, reopen_error) = match opened {
        Ok(a) => (Some(a), None),
        Err(e) => (None, Some(IpcError::from(e))),
    };
    let commit = CommitReport {
        operation,
        status: outcome.status.as_str().to_owned(),
        seq: outcome.seq,
        commit_id: outcome.commit_id.to_hex(),
        files: import.files,
        directories: import.directories,
        bytes: import.bytes,
        removed,
        skipped: import
            .skipped
            .iter()
            .map(|s| SkippedView {
                path: NameText::of(s.path.as_stored()),
                reason: s.reason.clone(),
            })
            .collect(),
        sources_moved: sources.moved,
        sources_kept: sources.kept,
        durability_note: match &outcome.durability {
            PublishDurability::Durable => None,
            PublishDurability::DirectoryUnconfirmed(why) => Some(why.clone()),
        },
        archive,
        reopen_error,
    };
    JobDone {
        summary: format!(
            "committed commit {} ({})",
            outcome.seq,
            outcome.status.as_str()
        ),
        commit: Some(Box::new(commit)),
        extract: None,
        foreign_test: None,
        check: None,
        search: None,
        rekey: None,
        export: None,
        scan: None,
        retrieve: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use mochi_core::job::{CancellationToken, NullProgress, ProgressEvent, ProgressSink};
    use mochi_core::publish::open_head;
    use mochi_core::storage::os::OsReadStorage;
    use mochi_core::verify::{verify, VerifyOptions};
    use mochi_testkit::archive::{read_state, Content, State};

    use super::*;
    use crate::ipc::{JobEvent, JobOutcome};
    use crate::jobs::{EventSink, JobRegistry};

    fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// `src/` with a file, a folder holding two files, and a loose file.
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        out: PathBuf,
        svc: Services,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("src");
        put(&root, "docs/a.txt", b"alpha");
        put(&root, "docs/deep/b.txt", b"bravo bravo bravo");
        put(&root, "loose.bin", &[7u8; 100]);
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        Fixture {
            _tmp: tmp,
            root,
            out,
            svc: Services::default(),
        }
    }

    fn run(
        svc: &Services,
        request: &JobRequest,
        progress: &dyn ProgressSink,
        cancel: &CancellationToken,
    ) -> Result<JobDone> {
        svc.validate(request)?;
        let ctx = JobContext { progress, cancel };
        svc.run(request, &ctx).expect("a write request")
    }

    fn run_plain(svc: &Services, request: &JobRequest) -> Result<JobDone> {
        run(svc, request, &NullProgress, &CancellationToken::new())
    }

    fn create_request() -> JobRequest {
        JobRequest::CreateArchive {
            preset: CompressionPreset::Balanced,
            tar_compatible: false,
            passphrase: None,
            recovery_percent: None,
            delete_sources: None,
        }
    }

    fn stage_ok(svc: &Services, paths: &[PathBuf]) {
        let out = svc.staging.add(paths);
        assert!(out.problems.is_empty(), "{:?}", out.problems);
    }

    fn state_of(archive: &Path) -> State {
        let src = OsReadStorage::open(archive).unwrap();
        let opts = ReadOptions::default();
        let head = open_head(&src, &opts).unwrap();
        read_state(&src, &head).unwrap()
    }

    fn file(state: &State, name: &str) -> Option<Vec<u8>> {
        match state.get(name.as_bytes()) {
            Some(Content::File(b)) => Some(b.clone()),
            _ => None,
        }
    }

    fn assert_verifies(archive: &Path) {
        let src = OsReadStorage::open(archive).unwrap();
        let v = verify(
            &src,
            &VerifyOptions::default(),
            &JobContext {
                progress: &NullProgress,
                cancel: &CancellationToken::new(),
            },
        );
        assert!(v.head_is_anchorable(), "{:?}", v.report.dimensions);
    }

    /// Create `fx.root/docs` and `loose.bin` at `out/new.mochi`.
    fn created(fx: &Fixture) -> (PathBuf, JobDone) {
        let dest = fx.out.join("new.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest.clone());
        let done = run_plain(&fx.svc, &create_request()).unwrap();
        (dest, done)
    }

    // ---- staging ----

    #[test]
    fn staging_names_each_source_by_its_last_component() {
        let fx = fixture();
        let out = fx
            .svc
            .staging
            .add(&[fx.root.join("docs"), fx.root.join("loose.bin")]);
        assert!(out.problems.is_empty());
        let shown: Vec<_> = out
            .staged
            .items
            .iter()
            .map(|i| (i.name.text.as_str(), i.kind))
            .collect();
        assert_eq!(
            shown,
            [
                ("docs", StagedKind::Folder),
                ("loose.bin", StagedKind::File)
            ]
        );
        assert_eq!(out.staged.destination, None);
    }

    #[test]
    fn staging_the_same_path_twice_is_one_item_and_a_name_clash_is_reported() {
        let fx = fixture();
        let other = put(&fx.root, "elsewhere/docs/x.txt", b"x");
        let clash = other.parent().unwrap().to_path_buf(); // …/elsewhere/docs
        let out = fx.svc.staging.add(&[
            fx.root.join("docs"),
            fx.root.join("docs"),
            clash,
            fx.root.join("loose.bin"),
        ]);
        assert_eq!(
            out.staged.items.len(),
            2,
            "the duplicate and the clash left"
        );
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].code, "INVALID_ARGUMENT");
        assert!(out.problems[0].message.contains("same name"));
    }

    #[test]
    fn a_missing_source_is_reported_and_the_rest_are_staged() {
        let fx = fixture();
        let out = fx
            .svc
            .staging
            .add(&[fx.root.join("nope"), fx.root.join("loose.bin")]);
        assert_eq!(out.staged.items.len(), 1);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].code, "IO_ERROR");
    }

    #[test]
    fn staging_is_bounded() {
        let fx = fixture();
        let many = fx.root.join("many");
        let paths: Vec<PathBuf> = (0..=MAX_STAGED)
            .map(|i| put(&many, &format!("f{i}"), b""))
            .collect();
        let out = fx.svc.staging.add(&paths);
        assert_eq!(out.staged.items.len(), MAX_STAGED);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].code, "LIMIT_EXCEEDED");
    }

    #[test]
    fn items_can_be_removed_and_the_set_cleared() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(fx.out.join("x.mochi"));
        let v = fx.svc.staging.remove(0).unwrap();
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].name.text, "loose.bin");
        assert_eq!(
            fx.svc.staging.remove(5).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let v = fx.svc.staging.clear();
        assert!(v.items.is_empty());
        assert_eq!(v.destination, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_staged_name_is_escaped_text() {
        let fx = fixture();
        let p = put(&fx.root, "bad\u{1b}[31mname", b"x");
        let out = fx.svc.staging.add(&[p]);
        let name = &out.staged.items[0].name;
        assert_eq!(name.text, "bad\\u{1b}[31mname");
        assert!(!name.verbatim);
    }

    // ---- validation ----

    #[test]
    fn requests_that_cannot_work_are_refused_before_anything_starts() {
        let fx = fixture();
        let svc = &fx.svc;
        // Nothing staged.
        assert_eq!(
            svc.validate(&create_request()).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        stage_ok(svc, &[fx.root.join("loose.bin")]);
        // Staged, nowhere chosen: the default (next to the item) is used.
        svc.validate(&create_request()).unwrap();
        // An archive that is not open.
        let e = svc
            .validate(&JobRequest::AddToArchive {
                handle: 99,
                delete_sources: None,
            })
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        for keys in [
            vec![],
            vec!["zz".to_owned()],
            vec!["61".to_owned(); MAX_DELETE_KEYS + 1],
        ] {
            let e = svc
                .validate(&JobRequest::DeleteFromArchive { handle: 99, keys })
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument);
        }
    }

    #[test]
    fn a_delete_names_between_one_and_the_limit_of_entries() {
        let fx = fixture();
        let (_, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let delete = |keys: Vec<PathKey>| {
            fx.svc
                .validate(&JobRequest::DeleteFromArchive { handle, keys })
        };
        for keys in [vec![], vec!["61".to_owned(); MAX_DELETE_KEYS + 1]] {
            let e = delete(keys).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert!(e.message.contains("names 1 to"), "{}", e.message);
        }
        assert_eq!(
            delete(vec!["zz".to_owned()]).unwrap_err().message,
            "not a path key"
        );
        delete(vec![hex_key("loose.bin")]).unwrap();
        delete(vec!["61".to_owned(); MAX_DELETE_KEYS]).unwrap();
    }

    #[test]
    fn a_registry_refuses_a_create_with_nothing_staged_without_spawning() {
        let fx = fixture();
        let reg = Arc::new(JobRegistry::with_services(fx.svc.clone()));
        struct Never;
        impl EventSink for Never {
            fn send(&self, _: JobEvent) -> bool {
                panic!("no events for a refused job")
            }
        }
        let e = reg
            .start(
                create_request(),
                Arc::new(Never),
                Box::new(|_| panic!("must not spawn")),
            )
            .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT");
    }

    // ---- create ----

    #[test]
    fn create_commits_the_staged_sources_and_the_archive_verifies() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let state = state_of(&dest);
        assert_eq!(file(&state, "docs/a.txt").unwrap(), b"alpha");
        assert_eq!(
            file(&state, "docs/deep/b.txt").unwrap(),
            b"bravo bravo bravo"
        );
        assert_eq!(file(&state, "loose.bin").unwrap(), vec![7u8; 100]);
        assert!(matches!(state.get(&b"docs/deep"[..]), Some(Content::Dir)));
        assert_eq!(
            state.len(),
            5,
            "docs, docs/a.txt, docs/deep, docs/deep/b.txt, loose.bin"
        );
        assert_verifies(&dest);

        let c = done.commit.unwrap();
        assert_eq!(c.operation, CommitOperation::Create);
        assert_eq!(c.status, "LOCAL_COMMITTED");
        assert_eq!(
            (c.seq, c.files, c.directories, c.bytes),
            (0, 3, 2, 5 + 17 + 100)
        );
        assert!(c.skipped.is_empty());
        let a = c.archive.expect("the new archive is open for browsing");
        assert_eq!(a.file_name, "new.mochi");
        assert_eq!(a.commits.len(), 1);
        assert!(!a.profile.tar_compatible);
        assert_eq!(fx.svc.archives.path(a.handle).unwrap(), dest);
        assert!(done.summary.contains("LOCAL_COMMITTED"));
        // What was stored is no longer staged.
        assert!(fx.svc.staging.view().items.is_empty());
        assert_eq!(fx.svc.staging.view().destination, None);
    }

    #[test]
    fn the_tar_compatible_option_is_fixed_at_creation_and_shown_when_browsing() {
        let fx = fixture();
        let dest = fx.out.join("tar.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        fx.svc.staging.set_destination(dest.clone());
        let done = run_plain(
            &fx.svc,
            &JobRequest::CreateArchive {
                preset: CompressionPreset::Balanced,
                tar_compatible: true,
                passphrase: None,
                recovery_percent: None,
                delete_sources: None,
            },
        )
        .unwrap();
        let a = done.commit.unwrap().archive.unwrap();
        assert!(a.profile.tar_compatible);
        assert!(!a.profile.encrypted);
        assert_eq!(a.profile.redundancy_percent, None);
        assert_verifies(&dest);
        assert_eq!(file(&state_of(&dest), "docs/a.txt").unwrap(), b"alpha");

        // The default is not TAR-compatible (spec D4).
        let other = fixture();
        let (plain, _) = created(&other);
        let src = OsReadStorage::open(&plain).unwrap();
        let head = open_head(&src, &ReadOptions::default()).unwrap();
        assert!(!head.descriptor.profile().tar_compatible);
    }

    #[test]
    fn presets_map_to_distinct_levels_and_each_makes_a_valid_archive() {
        use CompressionPreset::*;
        let all = [Turbo, VeryFast, Fast, Balanced, Maximum, Ultra, Extreme];
        assert_eq!(
            all.map(CompressionPreset::zstd_level),
            [
                Some(-4),
                Some(-1),
                Some(1),
                None,
                Some(19),
                Some(20),
                Some(22)
            ]
        );
        // Words from a small vocabulary in a pseudo-random order: the longer
        // matches and larger window of level 19 pay off, level 1's do not.
        let words = [
            "archive", "snapshot", "commit", "chunk", "digest", "catalog", "frame", "parity",
        ];
        let mut x = 12345u32;
        let text: Vec<u8> = (0..200_000)
            .flat_map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let w = words[(x >> 24) as usize % words.len()];
                format!("{w} {} ", x >> 20).into_bytes()
            })
            .collect();
        let mut sizes = Vec::new();
        for preset in all {
            let fx = fixture();
            let src = put(&fx.root, "big.txt", &text);
            let dest = fx.out.join("p.mochi");
            stage_ok(&fx.svc, &[src]);
            fx.svc.staging.set_destination(dest.clone());
            run_plain(
                &fx.svc,
                &JobRequest::CreateArchive {
                    preset,
                    tar_compatible: false,
                    passphrase: None,
                    recovery_percent: None,
                    delete_sources: None,
                },
            )
            .unwrap();
            assert_verifies(&dest);
            assert_eq!(file(&state_of(&dest), "big.txt").unwrap(), text);
            sizes.push(std::fs::metadata(&dest).unwrap().len());
        }
        // Faster levels make bigger files where the gap is wide. Levels 19, 20
        // and 22 are compared only with level 1: zstd does not promise that a
        // higher level is never larger, and on this text 22 and 19 land within
        // a byte or two of each other (the archive also holds random IDs).
        let [turbo, _, fast, _, maximum, ultra, extreme] = sizes[..] else {
            panic!("one size per preset")
        };
        assert!(
            fast < turbo,
            "level 1 ({fast}) must beat level -4 ({turbo}) on repetitive text"
        );
        for (level, size) in [(19, maximum), (20, ultra), (22, extreme)] {
            assert!(
                size < fast,
                "level {level} ({size}) must beat level 1 ({fast}) on repetitive text"
            );
        }
    }

    #[test]
    fn create_never_replaces_an_existing_file() {
        let fx = fixture();
        let dest = put(&fx.out, "taken.mochi", b"not an archive");
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest.clone());
        let e = run_plain(&fx.svc, &create_request()).unwrap_err();
        assert_eq!(e.code, ErrorCode::DestinationExists);
        assert_eq!(std::fs::read(&dest).unwrap(), b"not an archive");
        assert_eq!(fx.svc.staging.view().items.len(), 1, "kept for another try");
    }

    #[test]
    fn a_missing_destination_folder_fails_without_writing() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        fx.svc
            .staging
            .set_destination(fx.out.join("no/such/dir/x.mochi"));
        assert!(run_plain(&fx.svc, &create_request()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_skipped_and_reported_not_followed() {
        let fx = fixture();
        std::os::unix::fs::symlink(fx.root.join("loose.bin"), fx.root.join("docs/link")).unwrap();
        let (dest, done) = created(&fx);
        let c = done.commit.unwrap();
        assert_eq!(c.skipped.len(), 1);
        assert_eq!(c.skipped[0].path.text, "docs/link");
        assert!(c.skipped[0].reason.contains("symbolic links"));
        assert!(!state_of(&dest).contains_key(&b"docs/link"[..]));
    }

    // ---- cancel ----

    /// Cancels the job once the import has read some content.
    struct CancelAfterFirstFile(CancellationToken);
    impl ProgressSink for CancelAfterFirstFile {
        fn report(&self, e: &ProgressEvent) {
            if e.phase == "import" && e.completed > 0 {
                self.0.cancel();
            }
        }
    }

    #[test]
    fn cancelling_a_create_leaves_no_file_and_keeps_the_staged_set() {
        let fx = fixture();
        let dest = fx.out.join("never.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest.clone());
        let token = CancellationToken::new();
        let sink = CancelAfterFirstFile(token.clone());
        let e = run(&fx.svc, &create_request(), &sink, &token).unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(
            std::fs::read_dir(&fx.out).unwrap().count(),
            0,
            "no archive and no temporary file"
        );
        assert_eq!(fx.svc.staging.view().items.len(), 2);
        assert_eq!(
            fx.svc.staging.view().destination.as_deref(),
            Some("never.mochi")
        );
    }

    #[test]
    fn a_job_cancelled_before_it_starts_changes_nothing() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let before = std::fs::read(&dest).unwrap();
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        let token = CancellationToken::new();
        token.cancel();
        let e = run(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
            &NullProgress,
            &token,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            before,
            "the head is unchanged"
        );
        let key = hex_key("loose.bin");
        let e = run(
            &fx.svc,
            &JobRequest::DeleteFromArchive {
                handle,
                keys: vec![key],
            },
            &NullProgress,
            &token,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }

    #[test]
    fn cancelling_an_add_midway_leaves_the_previous_head_valid() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let before = std::fs::read(&dest).unwrap();
        stage_ok(
            &fx.svc,
            &[
                put(&fx.root, "more/one.txt", b"1"),
                put(&fx.root, "more2/two.txt", b"2"),
            ],
        );
        let token = CancellationToken::new();
        let sink = CancelAfterFirstFile(token.clone());
        let e = run(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
            &sink,
            &token,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(std::fs::read(&dest).unwrap(), before);
        assert_verifies(&dest);
        assert_eq!(fx.svc.staging.view().items.len(), 2, "kept for another try");
    }

    // ---- add ----

    /// Annex B.2.23 item 3: with "Delete files after archiving", every
    /// source whose archived copy reads back goes to the Bin (never deleted)
    /// once the commit is published, folders too once empty; the archive
    /// holds it all, and the report counts what moved.
    #[test]
    fn b2_23_delete_after_archiving_moves_the_sources_to_the_bin() {
        let fx = fixture();
        let dest = fx.out.join("moved.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest.clone());
        let done = run_plain(
            &fx.svc,
            &JobRequest::CreateArchive {
                preset: CompressionPreset::Balanced,
                tar_compatible: false,
                passphrase: None,
                recovery_percent: None,
                delete_sources: Some(true),
            },
        )
        .unwrap();
        let c = done.commit.unwrap();
        assert_eq!(c.status, "LOCAL_COMMITTED");
        // docs, docs/deep, two files in them, and loose.bin.
        assert_eq!(c.sources_moved, 5, "{:?}", c.sources_kept);
        assert!(c.sources_kept.is_empty(), "{:?}", c.sources_kept);
        assert!(!fx.root.join("docs").exists());
        assert!(!fx.root.join("loose.bin").exists());
        // In the stand-in Bin, not gone.
        assert!(crate::trashbin::test_bin().exists());
        let state = state_of(&dest);
        assert_eq!(file(&state, "docs/a.txt").unwrap(), b"alpha");
        assert_eq!(
            file(&state, "docs/deep/b.txt").unwrap(),
            b"bravo bravo bravo"
        );
        assert_eq!(file(&state, "loose.bin").unwrap(), [7u8; 100]);
        assert_verifies(&dest);
    }

    /// Off (the default) nothing is moved. On, a folder that still holds
    /// something the archive did not take (a link, which is never stored) is
    /// kept with the reason, while its files go.
    #[cfg(unix)]
    #[test]
    fn b2_23_a_folder_holding_what_was_not_archived_is_kept() {
        let fx = fixture();
        let (_dest, done) = created(&fx);
        assert_eq!(done.commit.as_ref().unwrap().sources_moved, 0);
        assert!(fx.root.join("docs/a.txt").exists(), "off by default");
        let handle = done.commit.unwrap().archive.unwrap().handle;

        let keep = fx.root.join("keep");
        put(&keep, "x.txt", b"x");
        std::os::unix::fs::symlink("x.txt", keep.join("link")).unwrap();
        stage_ok(&fx.svc, std::slice::from_ref(&keep));
        let c = run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: Some(true),
            },
        )
        .unwrap()
        .commit
        .unwrap();
        assert_eq!(c.sources_moved, 1, "x.txt");
        assert!(!keep.join("x.txt").exists());
        assert!(
            keep.join("link").symlink_metadata().is_ok(),
            "the link stays"
        );
        assert_eq!(
            c.sources_kept
                .iter()
                .map(|k| (k.path.text.as_str(), k.reason.as_str()))
                .collect::<Vec<_>>(),
            [("keep", "it is not empty")]
        );
    }

    #[test]
    fn add_makes_a_new_commit_and_the_old_snapshot_is_unchanged() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        // Replace a file's content and add a new one.
        let changed = put(&fx.root, "loose.bin", b"changed");
        let fresh = put(&fx.root, "fresh.txt", b"fresh");
        stage_ok(&fx.svc, &[changed, fresh]);
        let done = run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
        )
        .unwrap();
        let c = done.commit.unwrap();
        assert_eq!(c.operation, CommitOperation::Add);
        assert_eq!(c.status, "LOCAL_COMMITTED");
        assert_eq!((c.seq, c.files, c.directories), (1, 2, 0));
        let a = c.archive.unwrap();
        assert_eq!(a.handle, handle, "the same handle, reopened");
        assert_eq!(a.commits.len(), 2);

        let state = state_of(&dest);
        assert_eq!(file(&state, "loose.bin").unwrap(), b"changed");
        assert_eq!(file(&state, "fresh.txt").unwrap(), b"fresh");
        assert_eq!(
            file(&state, "docs/a.txt").unwrap(),
            b"alpha",
            "untouched entries stay"
        );
        assert_verifies(&dest);

        // Snapshot 0 still shows the first commit's namespace.
        let snap0 = fx.svc.archives.snapshot(handle, 0).unwrap();
        assert_eq!(snap0.root_files, 1);
        let snap1 = fx.svc.archives.snapshot(handle, 1).unwrap();
        assert_eq!(snap1.root_files, 2);
        assert!(fx.svc.staging.view().items.is_empty());
    }

    /// D29: the comment is set, kept by a later add, cleared; each version
    /// keeps its own; the archive verifies; a bad text is refused up front.
    #[test]
    fn the_archive_comment_is_set_kept_and_cleared_as_commits() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let set = |c: Option<&str>| {
            run_plain(
                &fx.svc,
                &JobRequest::SetArchiveComment {
                    handle,
                    comment: c.map(str::to_owned),
                },
            )
        };
        let done = set(Some("Holiday photos\n\tday one")).unwrap();
        let report = done.commit.unwrap();
        assert_eq!(report.operation, CommitOperation::Comment);
        assert_eq!((report.files, report.removed), (0, 0));
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
        )
        .unwrap();
        set(None).unwrap();
        let info = |seq: u64| fx.svc.archives.info(handle, seq).unwrap().comment;
        assert_eq!(info(0), None);
        assert_eq!(info(1).as_deref(), Some("Holiday photos\n\tday one"));
        assert_eq!(info(2).as_deref(), Some("Holiday photos\n\tday one"));
        assert_eq!(info(3), None);
        assert_verifies(&dest);
        let before = std::fs::read(&dest).unwrap();
        for bad in ["", "a\r\nb"] {
            let e = fx
                .svc
                .validate(&JobRequest::SetArchiveComment {
                    handle,
                    comment: Some(bad.into()),
                })
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad:?}");
        }
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }

    #[test]
    fn add_keeps_a_tar_compatible_archive_tar_compatible() {
        let fx = fixture();
        let dest = fx.out.join("t.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        fx.svc.staging.set_destination(dest.clone());
        let done = run_plain(
            &fx.svc,
            &JobRequest::CreateArchive {
                preset: CompressionPreset::Fast,
                tar_compatible: true,
                passphrase: None,
                recovery_percent: None,
                delete_sources: None,
            },
        )
        .unwrap();
        let handle = done.commit.unwrap().archive.unwrap().handle;
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        let done = run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
        )
        .unwrap();
        assert!(done.commit.unwrap().archive.unwrap().profile.tar_compatible);
        assert_verifies(&dest);
    }

    #[test]
    fn an_add_of_nothing_storable_commits_nothing() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let before = std::fs::read(&dest).unwrap();
        // An empty staged set is refused at validation.
        assert!(run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None
            }
        )
        .is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn an_add_of_only_a_link_commits_nothing_and_says_why() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let before = std::fs::read(&dest).unwrap();
        let link = fx.root.join("onlylink");
        std::os::unix::fs::symlink(fx.root.join("loose.bin"), &link).unwrap();
        stage_ok(&fx.svc, &[link]);
        let e = run_plain(
            &fx.svc,
            &JobRequest::AddToArchive {
                handle,
                delete_sources: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.message.contains("nothing to add"));
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }

    // ---- delete ----

    fn hex_key(path: &str) -> PathKey {
        path.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn delete_removes_from_the_current_snapshot_and_history_keeps_it() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let done = run_plain(
            &fx.svc,
            &JobRequest::DeleteFromArchive {
                handle,
                keys: vec![hex_key("loose.bin")],
            },
        )
        .unwrap();
        let c = done.commit.unwrap();
        assert_eq!(c.operation, CommitOperation::Delete);
        assert_eq!(c.status, "LOCAL_COMMITTED");
        assert_eq!((c.seq, c.removed, c.files), (1, 1, 0));
        let a = c.archive.unwrap();
        assert_eq!(a.handle, handle);
        assert_eq!(a.commits.len(), 2);

        let state = state_of(&dest);
        assert!(!state.contains_key(&b"loose.bin"[..]));
        assert_eq!(state.len(), 4);
        assert_verifies(&dest);

        // The previous snapshot still has it, content included.
        let page0 = fx
            .svc
            .archives
            .children(handle, 0, None, false, 0, 100)
            .unwrap();
        assert!(page0.entries.iter().any(|e| e.name.text == "loose.bin"));
        let d = fx
            .svc
            .archives
            .details(handle, 0, &hex_key("loose.bin"))
            .unwrap();
        assert_eq!(d.size, "100");
        let page1 = fx
            .svc
            .archives
            .children(handle, 1, None, false, 0, 100)
            .unwrap();
        assert!(!page1.entries.iter().any(|e| e.name.text == "loose.bin"));
        // …and the bytes are still in the file: its size did not shrink.
        assert!(std::fs::metadata(&dest).unwrap().len() > 100);
    }

    #[test]
    fn deleting_a_folder_removes_everything_under_it_and_overlaps_are_fine() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let done = run_plain(
            &fx.svc,
            &JobRequest::DeleteFromArchive {
                handle,
                // A folder, something inside it, and the folder again.
                keys: vec![hex_key("docs"), hex_key("docs/deep/b.txt"), hex_key("docs")],
            },
        )
        .unwrap();
        assert_eq!(done.commit.unwrap().removed, 4);
        let state = state_of(&dest);
        assert_eq!(
            state.keys().map(|k| k.as_slice()).collect::<Vec<_>>(),
            [&b"loose.bin"[..]]
        );
        assert_verifies(&dest);
    }

    #[test]
    fn deleting_an_entry_the_snapshot_lacks_commits_nothing() {
        let fx = fixture();
        let (dest, done) = created(&fx);
        let handle = done.commit.unwrap().archive.unwrap().handle;
        let before = std::fs::read(&dest).unwrap();
        let e = run_plain(
            &fx.svc,
            &JobRequest::DeleteFromArchive {
                handle,
                keys: vec![hex_key("loose.bin"), hex_key("ghost.txt")],
            },
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(std::fs::read(&dest).unwrap(), before, "not even loose.bin");
    }

    // ---- as a job over the registry ----

    struct Collect(Mutex<mpsc::Sender<JobEvent>>, AtomicBool);
    impl EventSink for Collect {
        fn send(&self, e: JobEvent) -> bool {
            let _ = self.0.lock().unwrap().send(e);
            self.1.load(Ordering::SeqCst)
        }
    }

    #[test]
    fn a_create_streams_progress_and_one_finished_with_its_commit() {
        let fx = fixture();
        let dest = fx.out.join("job.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest.clone());
        let reg = Arc::new(JobRegistry::with_services(fx.svc.clone()));
        let (tx, rx) = mpsc::channel();
        let id = reg
            .start(
                create_request(),
                Arc::new(Collect(Mutex::new(tx), AtomicBool::new(true))),
                Box::new(|w| {
                    std::thread::spawn(w);
                }),
            )
            .unwrap();
        let mut events = Vec::new();
        loop {
            let e = rx.recv_timeout(Duration::from_secs(60)).unwrap();
            let end = matches!(e, JobEvent::Finished { .. });
            events.push(e);
            if end {
                break;
            }
        }
        assert_eq!(events[0], JobEvent::Started { job_id: id });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, JobEvent::Progress { phase, .. } if phase == "import")),
            "the import reports progress"
        );
        let JobEvent::Finished {
            outcome: JobOutcome::Completed { commit, .. },
            ..
        } = events.last().unwrap()
        else {
            panic!("not completed: {:?}", events.last())
        };
        assert_eq!(commit.as_ref().unwrap().status, "LOCAL_COMMITTED");
        assert!(dest.exists());
        assert_eq!(reg.running(), 0);
    }

    #[test]
    fn a_failed_create_finishes_failed_with_a_code() {
        let fx = fixture();
        let dest = put(&fx.out, "there.mochi", b"x");
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        fx.svc.staging.set_destination(dest);
        let reg = Arc::new(JobRegistry::with_services(fx.svc.clone()));
        let (tx, rx) = mpsc::channel();
        reg.start(
            create_request(),
            Arc::new(Collect(Mutex::new(tx), AtomicBool::new(true))),
            Box::new(|w| {
                std::thread::spawn(w);
            }),
        )
        .unwrap();
        loop {
            if let JobEvent::Finished { outcome, .. } =
                rx.recv_timeout(Duration::from_secs(60)).unwrap()
            {
                let JobOutcome::Failed { code, .. } = outcome else {
                    panic!("{outcome:?}")
                };
                assert_eq!(code, "DESTINATION_EXISTS");
                break;
            }
        }
    }

    // ---- drag and drop ----

    struct Drops(Mutex<Vec<DropEvent>>, AtomicBool);
    impl DropSink for Drops {
        fn send(&self, e: DropEvent) -> bool {
            self.0.lock().unwrap().push(e);
            self.1.load(Ordering::SeqCst)
        }
    }

    fn position() -> tauri::PhysicalPosition<f64> {
        tauri::PhysicalPosition::new(1.0, 2.0)
    }

    #[test]
    fn a_drop_stages_the_paths_in_rust_and_the_webview_hears_names_only() {
        let fx = fixture();
        let sink = Arc::new(Drops(Mutex::default(), AtomicBool::new(true)));
        let watch = DropWatch::default();
        watch.set(sink.clone());
        let paths = vec![fx.root.join("docs"), fx.root.join("missing")];
        on_drag_drop(
            &DragDropEvent::Enter {
                paths: paths.clone(),
                position: position(),
            },
            &fx.svc.staging,
            &watch,
        );
        on_drag_drop(
            &DragDropEvent::Drop {
                paths,
                position: position(),
            },
            &fx.svc.staging,
            &watch,
        );
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], DropEvent::Hover { active: true });
        assert_eq!(events[1], DropEvent::Hover { active: false });
        let DropEvent::Dropped {
            staged,
            problems,
            from_shell: false,
        } = &events[2]
        else {
            panic!("{:?}", events[2])
        };
        assert_eq!(staged.items.len(), 1);
        assert_eq!(staged.items[0].name.text, "docs");
        assert_eq!(problems.len(), 1, "the missing path is explained");
        assert_eq!(fx.svc.staging.view().items.len(), 1);
        // A drop is a stage: a later create uses it.
        on_drag_drop(&DragDropEvent::Leave, &fx.svc.staging, &watch);
        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&DropEvent::Hover { active: false })
        );
    }

    #[test]
    fn a_drop_with_no_listener_still_stages_and_a_gone_listener_is_forgotten() {
        let fx = fixture();
        let watch = DropWatch::default();
        let drop_of = |p: PathBuf| DragDropEvent::Drop {
            paths: vec![p],
            position: position(),
        };
        on_drag_drop(&drop_of(fx.root.join("docs")), &fx.svc.staging, &watch);
        assert_eq!(fx.svc.staging.view().items.len(), 1);

        let gone = Arc::new(Drops(Mutex::default(), AtomicBool::new(false)));
        watch.set(gone.clone());
        on_drag_drop(&drop_of(fx.root.join("loose.bin")), &fx.svc.staging, &watch);
        let seen = gone.0.lock().unwrap().len();
        assert_eq!(seen, 1, "the first send failed, so the sink was dropped");
        on_drag_drop(&drop_of(fx.root.join("docs")), &fx.svc.staging, &watch);
        assert_eq!(
            gone.0.lock().unwrap().len(),
            seen,
            "nothing more sent to it"
        );
    }

    #[test]
    fn staging_after_a_job_started_survives_that_jobs_commit() {
        let fx = fixture();
        let dest = fx.out.join("race.mochi");
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        fx.svc.staging.set_destination(dest);
        // The job copies the set when it starts; the user then drops more.
        let taken = fx.svc.staging.take();
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        fx.svc.staging.consume(&taken, true);
        let v = fx.svc.staging.view();
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].name.text, "loose.bin");
    }
    #[test]
    fn the_default_destination_is_next_to_the_first_item_and_never_over_a_file() {
        let fx = fixture();
        assert_eq!(
            fx.svc.staging.view().default_destination,
            None,
            "nothing staged"
        );
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        let want = fx.root.join("docs.mochi");
        assert_eq!(
            fx.svc.staging.view().default_destination,
            Some(want.display().to_string())
        );
        // An existing file is never the default: the next free name is.
        std::fs::write(&want, b"mine").unwrap();
        assert_eq!(
            fx.svc.staging.view().default_destination,
            Some(fx.root.join("docs (2).mochi").display().to_string())
        );
        // A destination the user chose replaces the default.
        fx.svc.staging.set_destination(fx.out.join("x.mochi"));
        assert_eq!(fx.svc.staging.view().default_destination, None);
    }

    #[test]
    fn a_create_with_no_chosen_destination_uses_the_default_and_keeps_existing_files() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        let taken_over = fx.root.join("docs.mochi");
        std::fs::write(&taken_over, b"not an archive").unwrap();
        run_plain(&fx.svc, &create_request()).unwrap();
        assert_eq!(std::fs::read(&taken_over).unwrap(), b"not an archive");
        let made = fx.root.join("docs (2).mochi");
        assert!(made.is_file());
        assert!(!state_of(&made).is_empty());
    }

    #[test]
    fn one_dropped_mochi_file_is_held_for_opening_not_staged() {
        let fx = fixture();
        let archive = fx.out.join("a.mochi");
        std::fs::write(&archive, b"x").unwrap();
        assert!(is_archive_file(&archive));
        assert!(!is_archive_file(&fx.root.join("loose.bin")));
        assert!(!is_archive_file(&fx.out.join("missing.mochi")));
        fx.svc.staging.set_dropped_archive(archive.clone());
        assert_eq!(fx.svc.staging.take_dropped_archive(), Some(archive));
        assert_eq!(fx.svc.staging.take_dropped_archive(), None, "once");
        assert!(fx.svc.staging.view().items.is_empty());
    }

    #[test]
    fn dropped_foreign_archives_are_held_and_flagged_and_can_be_staged_instead() {
        use crate::ipc::DropEvent;
        use std::sync::Mutex;
        struct Events(Mutex<Vec<DropEvent>>);
        impl DropSink for Events {
            fn send(&self, event: DropEvent) -> bool {
                self.0.lock().unwrap().push(event);
                true
            }
        }
        let fx = fixture();
        let zip = fx.out.join("pack.zip");
        std::fs::write(&zip, b"PK\x03\x04").unwrap();
        let tzst = fx.out.join("pack.TAR.ZST");
        std::fs::write(&tzst, b"x").unwrap();
        let mochi = fx.out.join("a.mochi");
        std::fs::write(&mochi, b"x").unwrap();
        // A lone .gz, a .docx and a folder are ordinary things to add.
        let gz = fx.out.join("notes.gz");
        std::fs::write(&gz, b"x").unwrap();
        let docx = fx.out.join("letter.docx");
        std::fs::write(&docx, b"PK\x03\x04").unwrap();
        assert!(is_archive_file(&zip) && is_archive_file(&tzst) && is_archive_file(&mochi));
        assert!(!is_archive_file(&gz) && !is_archive_file(&docx) && !is_archive_file(&fx.out));

        let events = Arc::new(Events(Mutex::new(Vec::new())));
        let watch = DropWatch::default();
        watch.set(events.clone());
        let drop = |paths: Vec<PathBuf>| DragDropEvent::Drop {
            paths,
            position: tauri::PhysicalPosition::new(0.0, 0.0),
        };
        let archive_events = |events: &Events| -> Vec<(String, bool)> {
            events
                .0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    DropEvent::Archive { name, foreign } => Some((name.clone(), *foreign)),
                    _ => None,
                })
                .collect()
        };
        on_drag_drop(&drop(vec![zip.clone()]), &fx.svc.staging, &watch);
        on_drag_drop(&drop(vec![mochi.clone()]), &fx.svc.staging, &watch);
        assert_eq!(
            archive_events(&events),
            vec![("pack.zip".to_owned(), true), ("a.mochi".to_owned(), false)]
        );
        // Two files at once, or an ordinary file, are staged as before.
        on_drag_drop(
            &drop(vec![zip.clone(), gz.clone()]),
            &fx.svc.staging,
            &watch,
        );
        on_drag_drop(&drop(vec![docx.clone()]), &fx.svc.staging, &watch);
        assert_eq!(archive_events(&events).len(), 2);
        assert_eq!(fx.svc.staging.view().items.len(), 3);
        fx.svc.staging.clear();

        // The held file (the last archive dropped) is staged instead of opened.
        on_drag_drop(&drop(vec![zip.clone()]), &fx.svc.staging, &watch);
        let staged = fx.svc.staging.stage_dropped_archive().unwrap();
        assert_eq!(staged.items.len(), 1);
        assert_eq!(staged.items[0].name.text, "pack.zip");
        assert_eq!(staged.items[0].kind, StagedKind::File);
        let e = fx.svc.staging.stage_dropped_archive().unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT", "once; nothing is waiting now");
        assert_eq!(fx.svc.staging.view().items.len(), 1);
    }

    // ---- export (plan O31): ZIP, tar, tar.gz, tar.zst ----

    fn export_request(format: ArchiveFormat) -> JobRequest {
        JobRequest::ExportArchive {
            format,
            level: CompressionPreset::Fast,
        }
    }

    const EXPORT_FORMATS: [ArchiveFormat; 4] = [
        ArchiveFormat::Zip,
        ArchiveFormat::Tar,
        ArchiveFormat::TarGz,
        ArchiveFormat::TarZst,
    ];

    /// The foreign-archive helper next to the test executable, built on demand.
    fn helper() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        let name = if cfg!(windows) {
            "mochi-foreign-helper.exe"
        } else {
            "mochi-foreign-helper"
        };
        let path = exe.parent().unwrap().parent().unwrap().join(name);
        if !path.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let ok = std::process::Command::new(cargo)
                .args(["build", "-p", "mochi-foreign"])
                .status()
                .unwrap()
                .success();
            assert!(
                ok && path.exists(),
                "could not build the helper at {path:?}"
            );
        }
        path
    }

    #[test]
    fn each_exported_format_is_read_back_by_the_foreign_reader_and_reports_plainly() {
        for format in EXPORT_FORMATS {
            let fx = fixture();
            // An empty folder and a Unicode name travel too.
            std::fs::create_dir_all(fx.root.join("docs/empty")).unwrap();
            put(&fx.root, "docs/Grüße ☃.txt", b"unicode");
            stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
            fx.svc.staging.set_format(format);
            // The user typed no extension: the format's is added.
            fx.svc.staging.set_destination(fx.out.join("pack"));
            let done = run_plain(&fx.svc, &export_request(format)).unwrap();
            let dest = fx.out.join(format!("pack{}", format.extension()));
            assert!(dest.is_file(), "{format:?}");
            assert_eq!(
                std::fs::read_dir(&fx.out).unwrap().count(),
                1,
                "no temporary file"
            );
            assert!(done.commit.is_none(), "an export is not a commit");
            let report = done.export.expect("an export report");
            assert_eq!(report.format, format);
            assert_eq!(report.name.text, format!("pack{}", format.extension()));
            assert_eq!((report.files, report.directories), (4, 3), "{format:?}");
            assert_eq!(report.bytes, 5 + 17 + 7 + 100);
            assert_eq!(report.output_bytes, std::fs::metadata(&dest).unwrap().len());
            assert!(report.skipped.is_empty());
            // The staged set is consumed, as after a create.
            assert!(fx.svc.staging.view().items.is_empty());
            assert_eq!(
                fx.svc.staging.last_export_folder().as_deref(),
                Some(fx.out.as_path())
            );

            // The existing reader (the isolated helper) lists it and tests it.
            let foreign = Arc::new(crate::foreign::Foreign::default());
            foreign.set_helper(helper());
            let summary = foreign
                .open(&dest, None)
                .expect("the foreign reader opens it");
            assert_eq!(summary.entries, 7, "{format:?}: 3 folders and 4 files");
            let page = foreign.children(summary.handle, None, 0, 100).unwrap();
            let names: Vec<_> = page.entries.iter().map(|e| e.name.text.clone()).collect();
            assert_eq!(names, ["docs", "loose.bin"], "{format:?}");
            let svc = Services {
                foreign: Arc::clone(&foreign),
                ..Services::default()
            };
            let tested = svc
                .run(
                    &JobRequest::TestForeign {
                        handle: summary.handle,
                    },
                    &JobContext {
                        progress: &NullProgress,
                        cancel: &CancellationToken::new(),
                    },
                )
                .unwrap()
                .unwrap();
            let t = tested.foreign_test.unwrap();
            assert_eq!(t.problems_total, 0, "{format:?}: {:?}", t.problems);
            assert!(t.complete);
        }
    }

    #[test]
    fn export_wording_is_the_summary_of_what_was_written_not_a_commit() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("loose.bin")]);
        fx.svc.staging.set_format(ArchiveFormat::Zip);
        let done = run_plain(&fx.svc, &export_request(ArchiveFormat::Zip)).unwrap();
        let text = done.summary.to_lowercase();
        for banned in [
            "local_committed",
            "commit",
            "safe",
            "backed up",
            "preserved",
            "verified",
        ] {
            assert!(!text.contains(banned), "{text}");
        }
    }

    #[test]
    fn export_never_replaces_a_file_and_the_staged_set_stays_for_another_try() {
        for format in EXPORT_FORMATS {
            let fx = fixture();
            stage_ok(&fx.svc, &[fx.root.join("docs")]);
            fx.svc.staging.set_format(format);
            let dest = fx.out.join(format!("mine{}", format.extension()));
            std::fs::write(&dest, b"precious").unwrap();
            fx.svc.staging.set_destination(dest.clone());
            let e = run_plain(&fx.svc, &export_request(format)).unwrap_err();
            assert_eq!(e.code, ErrorCode::DestinationExists, "{format:?}");
            assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
            assert_eq!(std::fs::read_dir(&fx.out).unwrap().count(), 1);
            assert_eq!(fx.svc.staging.view().items.len(), 1);
        }
    }

    #[test]
    fn a_cancelled_export_leaves_no_file_and_keeps_the_staged_set() {
        for format in EXPORT_FORMATS {
            let fx = fixture();
            stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
            fx.svc.staging.set_format(format);
            fx.svc.staging.set_destination(fx.out.join("never"));
            let token = CancellationToken::new();
            struct CancelOnWrite(CancellationToken);
            impl ProgressSink for CancelOnWrite {
                fn report(&self, e: &ProgressEvent) {
                    if e.phase == mochi_export::WRITE_PHASE {
                        self.0.cancel();
                    }
                }
            }
            let e = run(
                &fx.svc,
                &export_request(format),
                &CancelOnWrite(token.clone()),
                &token,
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::Cancelled, "{format:?}");
            assert_eq!(
                std::fs::read_dir(&fx.out).unwrap().count(),
                0,
                "{format:?}: no archive and no temporary file"
            );
            assert_eq!(fx.svc.staging.view().items.len(), 2);
            assert_eq!(fx.svc.staging.last_export_folder(), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_export_lists_what_it_could_not_represent() {
        let fx = fixture();
        std::os::unix::fs::symlink(fx.root.join("loose.bin"), fx.root.join("docs/link")).unwrap();
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        fx.svc.staging.set_format(ArchiveFormat::TarGz);
        fx.svc.staging.set_destination(fx.out.join("links"));
        let done = run_plain(&fx.svc, &export_request(ArchiveFormat::TarGz)).unwrap();
        let r = done.export.unwrap();
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].path.text, "docs/link");
        assert!(r.skipped[0].reason.contains("symbolic link"));
    }

    #[test]
    fn an_export_request_is_checked_before_a_job_starts() {
        let fx = fixture();
        // .mochi is created by CreateArchive, not exported.
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        let e = fx
            .svc
            .validate(&export_request(ArchiveFormat::Mochi))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        // Nothing staged.
        fx.svc.staging.clear();
        let e = fx
            .svc
            .validate(&export_request(ArchiveFormat::Zip))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        // The registry refuses it without spawning, like a create.
        let reg = Arc::new(JobRegistry::with_services(fx.svc.clone()));
        struct Never;
        impl EventSink for Never {
            fn send(&self, _: JobEvent) -> bool {
                panic!("no events for a refused job")
            }
        }
        let e = reg
            .start(
                export_request(ArchiveFormat::Zip),
                Arc::new(Never),
                Box::new(|_| panic!("must not spawn")),
            )
            .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn zip_and_tar_gz_offer_three_levels_and_refuse_the_other_four() {
        use CompressionPreset::*;
        let all = [Turbo, VeryFast, Fast, Balanced, Maximum, Ultra, Extreme];
        let offered = |f: ArchiveFormat| -> Vec<CompressionPreset> {
            all.into_iter().filter(|p| f.offers(*p)).collect()
        };
        for f in [ArchiveFormat::Zip, ArchiveFormat::TarGz] {
            assert_eq!(offered(f), [Fast, Balanced, Maximum], "{f:?}");
        }
        for f in [
            ArchiveFormat::Mochi,
            ArchiveFormat::TarZst,
            ArchiveFormat::Tar,
        ] {
            assert_eq!(offered(f), all, "{f:?}");
        }

        // A request for a level ZIP does not offer is refused before a job
        // starts, and the staged set is left for another try.
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        fx.svc.staging.set_destination(fx.out.join("z.zip"));
        for level in [Turbo, VeryFast, Ultra, Extreme] {
            let e = fx
                .svc
                .validate(&JobRequest::ExportArchive {
                    format: ArchiveFormat::Zip,
                    level,
                })
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{level:?}");
        }
        assert_eq!(fx.svc.staging.view().items.len(), 1);
    }

    #[test]
    fn an_export_streams_progress_and_one_finished_with_its_report() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("docs"), fx.root.join("loose.bin")]);
        fx.svc.staging.set_format(ArchiveFormat::Zip);
        fx.svc.staging.set_destination(fx.out.join("job"));
        let reg = Arc::new(JobRegistry::with_services(fx.svc.clone()));
        let (tx, rx) = mpsc::channel();
        reg.start(
            export_request(ArchiveFormat::Zip),
            Arc::new(Collect(Mutex::new(tx), AtomicBool::new(true))),
            Box::new(|w| {
                std::thread::spawn(w);
            }),
        )
        .unwrap();
        let mut events = Vec::new();
        loop {
            let e = rx.recv_timeout(Duration::from_secs(60)).unwrap();
            let end = matches!(e, JobEvent::Finished { .. });
            events.push(e);
            if end {
                break;
            }
        }
        assert!(events.iter().any(
            |e| matches!(e, JobEvent::Progress { phase, total: Some(_), .. } if phase == mochi_export::WRITE_PHASE)
        ));
        let JobEvent::Finished {
            outcome: JobOutcome::Completed { export, commit, .. },
            ..
        } = events.last().unwrap()
        else {
            panic!("{:?}", events.last())
        };
        assert!(commit.is_none());
        assert_eq!(export.as_ref().unwrap().format, ArchiveFormat::Zip);
        assert!(fx.out.join("job.zip").is_file());
    }

    // ---- the chosen format and the names that follow it ----

    #[test]
    fn the_default_name_and_a_chosen_destination_follow_the_format() {
        let fx = fixture();
        stage_ok(&fx.svc, &[fx.root.join("docs")]);
        let st = &fx.svc.staging;
        assert_eq!(st.view().format, ArchiveFormat::Mochi);
        for (format, ext) in [
            (ArchiveFormat::Zip, "zip"),
            (ArchiveFormat::TarGz, "tar.gz"),
            (ArchiveFormat::Mochi, "mochi"),
        ] {
            let v = st.set_format(format);
            assert_eq!(v.format, format);
            assert_eq!(
                v.default_destination,
                Some(fx.root.join(format!("docs.{ext}")).display().to_string())
            );
        }
        // A name taken for this format moves to the next free one.
        st.set_format(ArchiveFormat::Zip);
        std::fs::write(fx.root.join("docs.zip"), b"mine").unwrap();
        assert_eq!(
            st.view().default_destination,
            Some(fx.root.join("docs (2).zip").display().to_string())
        );
        // A chosen destination takes the new extension and keeps its folder.
        st.set_destination(fx.out.join("report.mochi"));
        let v = st.set_format(ArchiveFormat::TarZst);
        assert_eq!(v.destination.as_deref(), Some("report.tar.zst"));
        let v = st.set_format(ArchiveFormat::Tar);
        assert_eq!(v.destination.as_deref(), Some("report.tar"));
        // Clearing the selection keeps the format the user chose.
        assert_eq!(st.clear().format, ArchiveFormat::Tar);
    }

    // ---- announcements to a window that is not listening yet ----

    fn announce(watch: &DropWatch, name: &str) {
        let staging = Staging::default();
        let _ = name;
        watch.announce_staged(StageOutcome {
            staged: staging.view(),
            problems: vec![IpcError {
                code: "X".into(),
                message: name.to_owned(),
            }],
        });
    }

    fn announced(sink: &Drops) -> Vec<String> {
        sink.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                DropEvent::Dropped { problems, .. } => problems.first().map(|p| p.message.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_announcement_made_before_the_window_listens_is_replayed_once() {
        let watch = DropWatch::default();
        announce(&watch, "first");
        announce(&watch, "second"); // the newest replaces an older waiting one
        let sink = Arc::new(Drops(Mutex::default(), AtomicBool::new(true)));
        watch.set(sink.clone());
        assert_eq!(announced(&sink), ["second"]);
        // Not replayed to a reloaded page.
        let again = Arc::new(Drops(Mutex::default(), AtomicBool::new(true)));
        watch.set(again.clone());
        assert!(announced(&again).is_empty());
    }

    #[test]
    fn an_announcement_racing_a_listener_is_delivered_exactly_once_never_stranded() {
        // Many rounds of `set` against `announce_staged` on two threads: every
        // announcement ends up in a sink (the old one or the new one), and
        // nothing stays waiting once a listener is installed.
        for round in 0..200 {
            let watch = Arc::new(DropWatch::default());
            let sink = Arc::new(Drops(Mutex::default(), AtomicBool::new(true)));
            let w = Arc::clone(&watch);
            let announcer = std::thread::spawn(move || announce(&w, &format!("r{round}")));
            watch.set(sink.clone());
            announcer.join().unwrap();
            assert_eq!(announced(&sink), [format!("r{round}")], "round {round}");
            assert!(lock(&watch.state).waiting.is_none(), "round {round}");
        }
    }

    #[test]
    fn a_listener_that_is_already_gone_leaves_the_announcement_waiting() {
        let watch = DropWatch::default();
        announce(&watch, "kept");
        let dead = Arc::new(Drops(Mutex::default(), AtomicBool::new(false)));
        watch.set(dead.clone());
        let live = Arc::new(Drops(Mutex::default(), AtomicBool::new(true)));
        watch.set(live.clone());
        assert_eq!(announced(&live), ["kept"]);
    }
}
