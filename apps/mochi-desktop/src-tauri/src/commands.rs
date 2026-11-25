//! Tauri commands: thin adapters over the job runner and `mochi-core`
//! (AGENTS.md). Archive work runs on the blocking pool, never on the async
//! runtime.

use std::sync::Arc;

use serde::Deserialize;
use ts_rs::TS;

use tauri::ipc::{Channel, CommandArg, CommandItem, InvokeError};
use tauri::{AppHandle, Manager, Runtime, State};

use crate::browse::Archives;
use crate::check::SubjectPick;
use crate::crashlog::{CrashLog, Settings, MAX_READ_BYTES};
use crate::folders::ExtractWizardSetup;
use crate::foreign::Foreign;
use crate::ipc::{
    ArchiveFormat, ArchiveHandle, ArchiveSummary, ChildrenPage, CrashLogView, CrashSettingsView,
    DialogText, DropEvent, EntryDetails, EnvelopeList, ExtractPlan, ForeignArchiveSummary,
    ForeignChildrenPage, ForeignExtractPlan, ForeignSearchResult, HealthView, IpcError, JobEvent,
    JobId, JobRequest, OpenOutcome, PathKey, PendingId, RepairSetupAction, RepairSetupView,
    ReportId, Secret, ShowTarget, SnapshotView, SourcePick, SpeedSettingsView, StageOutcome,
    StagedView, SubjectId, SubjectSource, SubjectView,
};
use crate::jobs::{EventSink, JobRegistry};
use crate::launch::{LaunchEvent, LaunchSink, Launches};
use crate::quick::{
    Quick, QuickAddRequest, QuickEvent, QuickFolder, QuickLocation, QuickPool, QuickSink,
};
use crate::recent::{RecentView, Recents};
use crate::update::{UpdateCheckView, UpdateSettingsView, Updates};
use crate::windows::{WindowState, WindowStates};
use crate::write::DropSink;

impl DropSink for Channel<DropEvent> {
    fn send(&self, event: DropEvent) -> bool {
        Channel::send(self, event).is_ok()
    }
}

impl LaunchSink for Channel<LaunchEvent> {
    fn send(&self, event: LaunchEvent) -> bool {
        Channel::send(self, event).is_ok()
    }
}

impl QuickSink for Channel<QuickEvent> {
    fn send(&self, event: QuickEvent) -> bool {
        Channel::send(self, event).is_ok()
    }
}

impl EventSink for Channel<JobEvent> {
    fn send(&self, event: JobEvent) -> bool {
        Channel::send(self, event).is_ok()
    }
}

/// The window a command was called from, and that window's own state
/// (`crate::windows`): its staged sources, drop listener, extract folder and
/// Test and Repair tool. The label comes from Tauri, not from the page, so a
/// window cannot name another window's state.
pub struct Win {
    pub label: String,
    pub state: Arc<WindowState>,
}

impl<'de, R: Runtime> CommandArg<'de, R> for Win {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let webview = command.message.webview();
        let label = webview.label().to_owned();
        let states = webview.state::<Arc<WindowStates>>();
        Ok(Win {
            state: states.state(&label),
            label,
        })
    }
}

/// Forwards a job's events, and sets the window's title again when the job
/// finishes (a job can create the archive the window now shows).
struct TitleSink {
    events: Channel<JobEvent>,
    launches: Arc<Launches>,
    label: String,
}

impl EventSink for TitleSink {
    fn send(&self, event: JobEvent) -> bool {
        let finished = matches!(event, JobEvent::Finished { .. });
        let sent = self.events.send(event).is_ok();
        if finished {
            self.launches.refresh_title(&self.label);
        }
        sent
    }
}

/// Start a job. Returns its ID at once; events stream over `on_event`.
/// CPU-bound or blocking work runs on the blocking pool, never on the async
/// runtime. The job belongs to the calling window: it works on that window's
/// staged sources and destination, and closing the window cancels it.
#[tauri::command]
pub async fn start_job(
    request: JobRequest,
    on_event: Channel<JobEvent>,
    jobs: State<'_, Arc<JobRegistry>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<JobId, IpcError> {
    let services = jobs.services_for(&win.state);
    let sink = TitleSink {
        events: on_event,
        launches: Arc::clone(&launches),
        label: win.label.clone(),
    };
    jobs.start_for(
        &win.label,
        services,
        request,
        Arc::new(sink),
        Box::new(|work| {
            tauri::async_runtime::spawn_blocking(work);
        }),
    )
}

/// Request cancellation of a running job. `false` if it is not running.
#[tauri::command]
pub fn cancel_job(job_id: JobId, jobs: State<'_, Arc<JobRegistry>>) -> bool {
    jobs.cancel(job_id)
}

/// Pause (`paused: true`) or resume a running job. `false` if it is not
/// running. The job waits at its next safe point; cancel still ends it.
#[tauri::command]
pub fn pause_job(job_id: JobId, paused: bool, jobs: State<'_, Arc<JobRegistry>>) -> bool {
    jobs.set_paused(job_id, paused)
}

/// Run `work` on the blocking pool and wait for it.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> mochi_core::Result<T> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| IpcError {
            code: mochi_core::ErrorCode::Cancelled.as_str().to_owned(),
            message: "the operation stopped before it finished".to_owned(),
        })?
        .map_err(IpcError::from)
}

/// Run `work` on the blocking pool and wait for it (the work reports an
/// [`IpcError`] itself).
async fn blocking_ipc<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| IpcError {
            code: mochi_core::ErrorCode::Cancelled.as_str().to_owned(),
            message: "the operation stopped before it finished".to_owned(),
        })?
}

/// Ask the user for an archive in the native open dialog (in Rust; the
/// webview never names a path) and open it read-only: a MOCHI archive, or a
/// ZIP, 7z, RAR, tar, tar.gz or tar.zst archive (`crate::route`; a file that
/// starts like a MOCHI archive is one whatever its name). An Encrypted MOCHI
/// archive waits in Rust for `unlock_archive` (D5); a foreign archive that
/// needs a password is `foreign_password` and is retried with
/// `open_foreign_archive`. `null` if the user cancelled the dialog. A window that already shows an
/// archive (or runs a job) does not give it up: the archive opens in a new
/// window and the answer is `elsewhere` (one that already has it open is
/// brought forward instead).
#[tauri::command]
pub async fn open_archive(
    app: AppHandle,
    text: Option<DialogText>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<Option<OpenOutcome>, IpcError> {
    let launches = Arc::clone(&launches);
    let text = text.unwrap_or_default();
    blocking_ipc(move || match crate::picker::pick_archive(&app, &text) {
        None => Ok(None),
        Some(path) => launches.open_in_window(&win.label, &path).map(Some),
    })
    .await
}

/// The archives opened lately, for the start screen (names only).
#[tauri::command]
pub fn recent_archives(recents: State<'_, Arc<Recents>>) -> Vec<RecentView> {
    recents.view()
}

/// Empty the recent list.
#[tauri::command]
pub fn forget_recent_archives(recents: State<'_, Arc<Recents>>) {
    recents.clear();
}

/// Open entry `index` of the recent list, a MOCHI or a foreign archive
/// (both are recorded). The window names no path.
#[tauri::command]
pub async fn open_recent(
    index: u32,
    recents: State<'_, Arc<Recents>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<OpenOutcome, IpcError> {
    let recents = Arc::clone(&recents);
    let launches = Arc::clone(&launches);
    blocking_ipc(move || {
        let path = usize::try_from(index)
            .ok()
            .and_then(|i| recents.get(i))
            .ok_or_else(|| IpcError {
                code: mochi_core::ErrorCode::InvalidArgument.as_str().to_owned(),
                message: "that archive is no longer in the recent list".to_owned(),
            })?;
        launches.open_in_window(&win.label, &path)
    })
    .await
}

/// Open the archive file just dropped on the window (`DropEvent::Archive`): a
/// `.mochi` file or a foreign archive, routed as `open_archive` routes. `null`
/// if there is none waiting.
#[tauri::command]
pub async fn open_dropped_archive(
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<Option<OpenOutcome>, IpcError> {
    let launches = Arc::clone(&launches);
    blocking_ipc(move || match win.state.staging.take_dropped_archive() {
        None => Ok(None),
        Some(path) => launches.open_in_window(&win.label, &path).map(Some),
    })
    .await
}

/// Stage the archive file just dropped on the window as an ordinary source to
/// add, instead of opening it (the window uses this when a MOCHI archive is
/// open and the user drops, say, a ZIP to store it). The file is the one Rust
/// holds for `DropEvent::Archive`; the window names no path. An error if none
/// is waiting or it cannot be staged.
#[tauri::command]
pub async fn stage_dropped_archive(win: Win) -> Result<StagedView, IpcError> {
    blocking_ipc(move || win.state.staging.stage_dropped_archive()).await
}

/// Show a folder in the operating system's file manager.
#[tauri::command]
pub async fn show_in_folder(
    what: ShowTarget,
    archives: State<'_, Arc<Archives>>,
    win: Win,
) -> Result<(), IpcError> {
    let archives = Arc::clone(&archives);
    let destination = Arc::clone(&win.state.destination);
    let staging = Arc::clone(&win.state.staging);
    tauri::async_runtime::spawn_blocking(move || {
        let folder = match what {
            ShowTarget::ExtractDestination => destination.get().ok_or_else(|| {
                mochi_core::MochiError::new(
                    mochi_core::ErrorCode::InvalidArgument,
                    "no folder has been chosen yet",
                )
            }),
            ShowTarget::LastExport => staging.last_export_folder().ok_or_else(|| {
                mochi_core::MochiError::new(
                    mochi_core::ErrorCode::InvalidArgument,
                    "no archive has been written yet",
                )
            }),
            ShowTarget::Archive { handle } => archives.path(handle).and_then(|p| {
                p.parent().map(std::path::Path::to_path_buf).ok_or_else(|| {
                    mochi_core::MochiError::new(
                        mochi_core::ErrorCode::InvalidArgument,
                        "this archive's folder is not known",
                    )
                })
            }),
        }
        .map_err(IpcError::from)?;
        crate::help::open_folder(&folder)
    })
    .await
    .map_err(|_| IpcError {
        code: mochi_core::ErrorCode::Cancelled.as_str().to_owned(),
        message: "the operation stopped before it finished".to_owned(),
    })?
}

/// Open the Encrypted archive that is waiting for `pending` with the
/// passphrase (D5). The passphrase crosses IPC here, once; it goes into a
/// zeroizing key session in Rust and is not returned, logged, or kept
/// anywhere else. A wrong passphrase is `KEY_UNAVAILABLE` ("the passphrase
/// did not open this archive"), never a statement about the archive.
#[tauri::command]
pub async fn unlock_archive(
    pending: PendingId,
    passphrase: Secret,
    archives: State<'_, Arc<Archives>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<ArchiveSummary, IpcError> {
    let archives = Arc::clone(&archives);
    let launches = Arc::clone(&launches);
    blocking(move || {
        let opened = archives.unlock(pending, &passphrase);
        launches.refresh_title(&win.label);
        opened
    })
    .await
}

/// Stop waiting for a passphrase. `false` if it was not waiting.
#[tauri::command]
pub fn cancel_unlock(pending: PendingId, archives: State<'_, Arc<Archives>>) -> bool {
    archives.cancel_pending(pending)
}

/// The head's key envelopes of an open Encrypted archive (D5; `mochi rekey
/// --list`, which needs no key).
#[tauri::command]
pub async fn list_envelopes(
    handle: ArchiveHandle,
    archives: State<'_, Arc<Archives>>,
) -> Result<EnvelopeList, IpcError> {
    let archives = Arc::clone(&archives);
    blocking(move || crate::rekey::envelopes(&archives, handle)).await
}

/// Give the chosen Encrypted archive its passphrase for testing and repair
/// (D5). The passphrase crosses here, once, into a zeroizing key session kept
/// in Rust. A wrong one is `KEY_UNAVAILABLE` and changes nothing.
#[tauri::command]
pub async fn unlock_check_subject(
    subject: SubjectId,
    passphrase: Secret,
    win: Win,
) -> Result<SubjectView, IpcError> {
    let checks = Arc::clone(&win.state.checks);
    blocking(move || checks.unlock_subject(subject, &passphrase)).await
}

/// Close an archive. `false` if it was not open. On the blocking pool:
/// closing a file on a network drive can take a moment.
#[tauri::command]
pub async fn close_archive(
    handle: ArchiveHandle,
    archives: State<'_, Arc<Archives>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<bool, IpcError> {
    let archives = Arc::clone(&archives);
    let launches = Arc::clone(&launches);
    blocking(move || {
        let closed = archives.close(handle);
        launches.refresh_title(&win.label);
        Ok(closed)
    })
    .await
}

/// Open (or reuse) one snapshot of an open archive.
#[tauri::command]
pub async fn open_snapshot(
    handle: ArchiveHandle,
    seq: u32,
    archives: State<'_, Arc<Archives>>,
) -> Result<SnapshotView, IpcError> {
    let archives = Arc::clone(&archives);
    blocking(move || archives.snapshot(handle, u64::from(seq))).await
}

/// The archive Info window: snapshot `seq`'s totals and recorded settings.
#[tauri::command]
pub async fn archive_info(
    handle: ArchiveHandle,
    seq: u32,
    archives: State<'_, Arc<Archives>>,
) -> Result<crate::ipc::ArchiveInfoView, IpcError> {
    let archives = Arc::clone(&archives);
    blocking(move || archives.info(handle, u64::from(seq))).await
}

/// The virus scan window's starting point: the scanners found on this
/// computer and the choice used last.
#[tauri::command]
pub async fn scan_setup() -> Result<crate::scan::ScanSetup, IpcError> {
    blocking(|| Ok(crate::scan::setup())).await
}

/// The virus scan window's Browse: the native open dialog, shown by Rust.
/// Returns the chosen program's path as text for the window's field, or
/// `null` if the user cancelled.
#[tauri::command]
pub async fn pick_scanner_program(
    app: AppHandle,
    text: Option<DialogText>,
) -> Result<Option<String>, IpcError> {
    let text = text.unwrap_or_default();
    blocking(move || {
        Ok(crate::picker::pick_scanner_program(&app, &text)
            .map(|p| p.to_string_lossy().into_owned()))
    })
    .await
}

/// One page of a directory's children (`dir` null for the root).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn list_children(
    handle: ArchiveHandle,
    seq: u32,
    dir: Option<PathKey>,
    directories_only: bool,
    offset: u32,
    limit: u32,
    archives: State<'_, Arc<Archives>>,
) -> Result<ChildrenPage, IpcError> {
    let archives = Arc::clone(&archives);
    blocking(move || {
        archives.children(
            handle,
            u64::from(seq),
            dir.as_deref(),
            directories_only,
            offset,
            limit,
        )
    })
    .await
}

/// Details of one entry.
#[tauri::command]
pub async fn entry_details(
    handle: ArchiveHandle,
    seq: u32,
    key: PathKey,
    archives: State<'_, Arc<Archives>>,
) -> Result<EntryDetails, IpcError> {
    let archives = Arc::clone(&archives);
    blocking(move || archives.details(handle, u64::from(seq), &key)).await
}

/// Listen for items dropped on the window (D2). Rust receives the window's
/// drop event and stages the paths itself; the webview hears only names.
#[tauri::command]
pub fn watch_drops(on_drop: Channel<DropEvent>, win: Win) {
    win.state.drops.set(Arc::new(on_drop));
}

/// Listen for what the shell asked the app to do (D6): open an archive,
/// extract one, add files. Rust receives the arguments, opens or stages
/// itself, and the webview hears that an archive was opened or why a launch
/// failed. Anything that arrived before this call is delivered now.
#[tauri::command]
pub fn watch_launches(
    on_launch: Channel<LaunchEvent>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) {
    launches.watch(&win.label, Arc::new(on_launch));
}

/// The compact window a command was called from (`quick`, `quick-2`, ...;
/// one per job, `quick::QuickPool`). The label comes from Tauri, not from the
/// page, so a compact window reaches its own job only.
pub struct QuickWin {
    label: String,
    pool: Arc<QuickPool>,
}

impl QuickWin {
    fn quick(&self) -> Result<Arc<Quick>, IpcError> {
        self.pool.of(&self.label).map_err(IpcError::from)
    }
}

impl<'de, R: Runtime> CommandArg<'de, R> for QuickWin {
    fn from_command(command: CommandItem<'de, R>) -> Result<Self, InvokeError> {
        let webview = command.message.webview();
        Ok(QuickWin {
            label: webview.label().to_owned(),
            pool: Arc::clone(&webview.state::<Arc<QuickPool>>()),
        })
    }
}

/// Listen for the compact window's job (D6 quick verbs): progress, a request
/// for a passphrase, and the result. Anything that arrived before this call
/// is delivered now. Only a compact window may call it, and it hears its own job.
#[tauri::command]
pub fn watch_quick(on_event: Channel<QuickEvent>, win: QuickWin) -> Result<(), IpcError> {
    win.quick()?.watch(Arc::new(on_event));
    Ok(())
}

/// The passphrase the compact window asked for. It goes straight to the
/// waiting job and is not kept.
#[tauri::command]
pub fn quick_passphrase(passphrase: Secret, win: QuickWin) -> Result<(), IpcError> {
    win.quick()?.passphrase(passphrase).map_err(IpcError::from)
}

/// Stop the compact window's job.
#[tauri::command]
pub fn quick_cancel(win: QuickWin) {
    if let Ok(q) = win.quick() {
        q.cancel();
    }
}

/// Pause (`paused: true`) or resume the compact window's job.
#[tauri::command]
pub fn quick_pause(paused: bool, win: QuickWin) -> bool {
    win.quick().is_ok_and(|q| q.set_paused(paused))
}

/// Open the folder the compact window's job wrote into, in the file manager.
/// The webview names no path: Rust remembers it.
#[tauri::command]
pub async fn quick_show_folder(win: QuickWin) -> Result<(), IpcError> {
    let quick = win.quick()?;
    blocking(move || quick.show_folder()).await
}

/// The compact window's "Open the folder when done" check box: on or off,
/// saved for the next extraction (and the running one, if it has not finished).
/// Only a compact window may call it.
#[tauri::command]
pub fn quick_set_open_folder(open: bool, win: QuickWin) -> Result<bool, IpcError> {
    win.pool
        .set_open_folder_when_done(open)
        .map_err(IpcError::from)
}

/// Rename the folder the last "Extract to new folder" made. The webview
/// sends one folder name, never a path; Rust renames in the same parent and
/// never replaces a name that exists. Only a compact window may call it. On
/// the blocking pool: the folder may be on a slow drive.
#[tauri::command]
pub async fn quick_rename_folder(name: String, win: QuickWin) -> Result<QuickFolder, IpcError> {
    let quick = win.quick()?;
    blocking(move || quick.rename_folder(&name)).await
}

/// Move the folder the last "Extract to new folder" made: the folder dialog
/// is shown from Rust and the folder keeps its name there. `null` if the user
/// cancelled. Only a compact window may call it.
#[tauri::command]
pub async fn quick_move_folder(
    app: AppHandle,
    text: Option<DialogText>,
    win: QuickWin,
) -> Result<Option<QuickFolder>, IpcError> {
    let quick = win.quick()?;
    let text = text.unwrap_or_default();
    blocking(
        move || match crate::picker::pick_extract_folder(&app, &text) {
            Some(parent) => quick.move_folder(&parent).map(Some),
            None => Ok(None),
        },
    )
    .await
}

/// The Add form's Browse…: show the save dialog (in Rust) for the format the
/// form is set to, and keep the folder it returns. The webview learns only the
/// folder's display text and the file name. `null` if the user cancelled.
/// Only a compact window may call it.
#[tauri::command]
pub async fn quick_choose_location(
    app: AppHandle,
    name: String,
    format: ArchiveFormat,
    text: Option<DialogText>,
    win: QuickWin,
) -> Result<Option<QuickLocation>, IpcError> {
    let quick = win.quick()?;
    let text = text.unwrap_or_default();
    blocking(move || {
        let suggested = quick.suggested_location(&name, format)?;
        match crate::picker::pick_destination(&app, Some(&suggested), format, &text) {
            Some(path) => quick.set_location(&path, format).map(Some),
            None => Ok(None),
        }
    })
    .await
}

/// OK on the Add form. The request is checked in Rust; a refusal (a name that
/// exists, a bad name, options the format does not have) is returned and the
/// form stays open. The passphrase, if any, crosses IPC here, once. Only a
/// compact window may call it. On the blocking pool: it looks whether the name
/// exists, and the folder may be on a slow drive.
#[tauri::command]
pub async fn quick_add_submit(request: QuickAddRequest, win: QuickWin) -> Result<(), IpcError> {
    let quick = win.quick()?;
    blocking(move || quick.submit_add(request)).await
}

/// Close the compact window that asks.
#[tauri::command]
pub fn quick_close(win: QuickWin) {
    if let Some(host) = win.pool.host() {
        host.close_quick(&win.label);
    }
}

/// What is staged to add.
#[tauri::command]
pub fn staged_sources(win: Win) -> StagedView {
    win.state.staging.view()
}

/// Show the native dialog for files or a folder (in Rust; the webview never
/// names a path) and stage the choice. Cancelling changes nothing.
#[tauri::command]
pub async fn stage_sources(
    app: AppHandle,
    pick: SourcePick,
    text: Option<DialogText>,
    win: Win,
) -> Result<StageOutcome, IpcError> {
    let staging = Arc::clone(&win.state.staging);
    let text = text.unwrap_or_default();
    blocking(move || {
        Ok(match crate::picker::pick_sources(&app, pick, &text) {
            Some(paths) => staging.add(&paths),
            None => StageOutcome {
                staged: staging.view(),
                problems: Vec::new(),
            },
        })
    })
    .await
}

/// Take item `index` out of the staged set.
#[tauri::command]
pub fn unstage_source(index: u32, win: Win) -> Result<StagedView, IpcError> {
    let index = usize::try_from(index).unwrap_or(usize::MAX);
    win.state.staging.remove(index).map_err(IpcError::from)
}

/// Empty the staged set and forget the chosen destination.
#[tauri::command]
pub fn clear_staged(win: Win) -> StagedView {
    win.state.staging.clear()
}

/// Show the save dialog for a new archive (in Rust) and remember the choice.
/// `null` if the user cancelled the dialog (an earlier choice is not reused).
#[tauri::command]
pub async fn choose_destination(
    app: AppHandle,
    text: Option<DialogText>,
    win: Win,
) -> Result<Option<StagedView>, IpcError> {
    let staging = Arc::clone(&win.state.staging);
    let text = text.unwrap_or_default();
    blocking(move || {
        let suggested = staging.suggested_destination();
        let format = staging.format();
        Ok(
            crate::picker::pick_destination(&app, suggested.as_deref(), format, &text)
                .map(|path| staging.set_destination(path)),
        )
    })
    .await
}

/// Choose the format of the next new archive (plan O31). The names the window
/// shows (the default and a destination already chosen) follow its extension.
/// MOCHI-only options are the window's to show or hide; the jobs refuse a
/// combination that does not exist.
#[tauri::command]
pub fn set_archive_format(format: ArchiveFormat, win: Win) -> StagedView {
    win.state.staging.set_format(format)
}

/// Plan an extraction (D3): optionally show the native folder dialog (in
/// Rust; the webview never names a path), then say what extracting `keys` of
/// snapshot `seq` would meet there, before anything is written. `null` if
/// the user cancelled the dialog. With `pick` false the folder chosen earlier
/// is checked again.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn plan_extract(
    app: AppHandle,
    handle: ArchiveHandle,
    seq: u32,
    keys: Vec<PathKey>,
    pick: bool,
    text: Option<DialogText>,
    archives: State<'_, Arc<Archives>>,
    win: Win,
) -> Result<Option<ExtractPlan>, IpcError> {
    let text = text.unwrap_or_default();
    let archives = Arc::clone(&archives);
    let destination = Arc::clone(&win.state.destination);
    blocking(move || {
        let picked = if pick {
            match crate::picker::pick_extract_folder(&app, &text) {
                Some(folder) => {
                    // The unused new folder of the default plan is not needed now.
                    destination.discard_made();
                    Some(folder)
                }
                None => return Ok(None),
            }
        } else {
            None
        };
        crate::extract::plan(&archives, &destination, picked, handle, seq, &keys).map(Some)
    })
    .await
}

/// Plan an extraction into a new folder next to the archive, named after it
/// (the default of "Extract"): no dialog. The folder is made now, and removed
/// again by `discard_extract_folder` if the plan is closed unused. Nothing is
/// extracted.
#[tauri::command]
pub async fn plan_extract_default(
    handle: ArchiveHandle,
    seq: u32,
    keys: Vec<PathKey>,
    archives: State<'_, Arc<Archives>>,
    win: Win,
) -> Result<ExtractPlan, IpcError> {
    let archives = Arc::clone(&archives);
    let destination = Arc::clone(&win.state.destination);
    blocking(move || crate::extract::plan_default(&archives, &destination, handle, seq, &keys))
        .await
}

/// What the "Extract to..." window opens with: the folder Settings chose (the
/// user's Desktop from Tauri's path resolver by default), and the recent folders
/// that still exist. Async, on the blocking pool: the recent folders are looked
/// at, and one on a network drive can be slow to answer (each gets
/// `folders::EXISTS_DEADLINE`); a synchronous command would run on the event
/// loop and stall every window meanwhile (owner report 2026-11-12).
#[tauri::command]
pub async fn extract_wizard_setup(
    app: AppHandle,
    win: Win,
    quick: State<'_, Arc<QuickPool>>,
) -> Result<ExtractWizardSetup, IpcError> {
    let open_folder = quick.open_folder_when_done();
    blocking(move || {
        let desktop = app.path().desktop_dir().ok().filter(|d| d.is_dir());
        Ok(win
            .state
            .destination
            .history
            .setup(desktop.as_deref(), open_folder))
    })
    .await
}

/// Settings, Extraction: where "Extract to..." starts and "Open the folder
/// when done". Nothing is looked at on disk.
#[tauri::command]
pub fn get_extract_settings(
    win: Win,
    quick: State<'_, Arc<QuickPool>>,
) -> crate::folders::ExtractSettingsView {
    let (start, custom) = win.state.destination.history.start();
    crate::folders::ExtractSettingsView {
        start,
        custom,
        open_folder: quick.open_folder_when_done(),
    }
}

/// Settings, Extraction: choose where "Extract to..." starts. `custom` is the
/// folder text for `custom` (validated in Rust as any folder text is; it need
/// not exist yet), `null` to keep the folder chosen before. Saved in the app's
/// config folder; async because it writes that small file.
#[tauri::command]
pub async fn set_extract_settings(
    start: crate::folders::ExtractStart,
    custom: Option<String>,
    win: Win,
    quick: State<'_, Arc<QuickPool>>,
) -> Result<crate::folders::ExtractSettingsView, IpcError> {
    let open_folder = quick.open_folder_when_done();
    blocking(move || {
        let (start, custom) = win
            .state
            .destination
            .history
            .set_start(start, custom.as_deref())?;
        Ok(crate::folders::ExtractSettingsView {
            start,
            custom,
            open_folder,
        })
    })
    .await
}

/// "Open the folder when done" from an archive window's "Extract to..." (owner
/// request 2026-11-10): the same saved choice as the compact window's check box.
#[tauri::command]
pub fn set_open_folder_when_done(
    open: bool,
    quick: State<'_, Arc<QuickPool>>,
) -> Result<bool, IpcError> {
    quick
        .set_open_folder_when_done(open)
        .map_err(IpcError::from)
}

/// Which archive a preview is for: its folder is named after it.
#[derive(Debug, Clone, Copy, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum PreviewArchive {
    Mochi { handle: ArchiveHandle },
    Foreign { handle: ArchiveHandle },
}

/// What the folder typed into "Extract to..." would do (owner request 2026-11-10): it
/// exists, or will be created; with a new folder, the name it will get (numbered when the
/// archive's own name is taken there). Read-only: nothing is created or written.
#[tauri::command]
pub async fn preview_extract_to(
    folder: String,
    new_folder: bool,
    archive: PreviewArchive,
    archives: State<'_, Arc<Archives>>,
    foreign: State<'_, Arc<Foreign>>,
) -> Result<crate::folders::ExtractToPreview, IpcError> {
    let archives = Arc::clone(&archives);
    let foreign = Arc::clone(&foreign);
    blocking(move || {
        let path = match archive {
            PreviewArchive::Mochi { handle } => archives.path(handle),
            PreviewArchive::Foreign { handle } => foreign.path(handle),
        };
        let name = path
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned));
        Ok(crate::folders::preview(
            &folder,
            new_folder,
            name.as_deref(),
        ))
    })
    .await
}

/// Show the folder dialog for "Extract to..." (in Rust), starting in `start`
/// when that is an existing folder, and return the folder's text. `null` if
/// the user cancelled.
#[tauri::command]
pub async fn browse_extract_folder(
    app: AppHandle,
    start: Option<String>,
    text: Option<DialogText>,
) -> Result<Option<String>, IpcError> {
    let text = text.unwrap_or_default();
    blocking(move || {
        let start = start
            .as_deref()
            .and_then(|s| crate::folders::existing_folder(s).ok());
        Ok(
            crate::picker::pick_extract_folder_in(&app, start.as_deref(), &text)
                .and_then(|p| p.to_str().map(str::to_owned)),
        )
    })
    .await
}

/// "Extract to..." (WinRAR's window): plan extracting `keys` (empty:
/// everything) into `folder`, a folder the user typed or chose (created now,
/// with any missing folders above it, if it does not exist yet; removed again
/// while empty if the plan is closed unused: owner request 2026-11-10), or,
/// with `new_folder`, into a new folder named after the archive inside it
/// (`name`, `name (2)`, ...; never an existing one, made now). The folder text
/// is validated in Rust (`folders::folder_to_extract_into`). The webview starts the
/// extraction itself with `start_job`, which writes into the folder Rust
/// remembers; entries that collide are listed in the plan and never
/// overwritten. Nothing is extracted here.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn plan_extract_to(
    handle: ArchiveHandle,
    seq: u32,
    keys: Vec<PathKey>,
    folder: String,
    new_folder: bool,
    archives: State<'_, Arc<Archives>>,
    win: Win,
) -> Result<ExtractPlan, IpcError> {
    let archives = Arc::clone(&archives);
    let destination = Arc::clone(&win.state.destination);
    blocking(move || {
        let (folder, _) = crate::folders::folder_to_extract_into(&folder)?;
        crate::extract::with_missing_created(&destination, &folder, || {
            if new_folder {
                crate::extract::plan_in_new_folder(
                    &archives,
                    &destination,
                    &folder,
                    handle,
                    seq,
                    &keys,
                )
            } else {
                crate::extract::plan(
                    &archives,
                    &destination,
                    Some(folder.clone()),
                    handle,
                    seq,
                    &keys,
                )
            }
        })
    })
    .await
}

/// `plan_extract_to` for an open ZIP, 7z, RAR, or tar archive.
#[tauri::command]
pub async fn plan_extract_foreign_to(
    handle: ArchiveHandle,
    keys: Vec<PathKey>,
    folder: String,
    new_folder: bool,
    foreign: State<'_, Arc<Foreign>>,
    win: Win,
) -> Result<ForeignExtractPlan, IpcError> {
    let foreign = Arc::clone(&foreign);
    let destination = Arc::clone(&win.state.destination);
    blocking(move || {
        let (folder, _) = crate::folders::folder_to_extract_into(&folder)?;
        crate::extract::with_missing_created(&destination, &folder, || {
            if new_folder {
                foreign.plan_in_new_folder(&destination, &folder, handle, &keys)
            } else {
                foreign.plan(&destination, Some(folder.clone()), handle, &keys)
            }
        })
    })
    .await
}

/// Remove the new folder a default plan made, if nothing was extracted into it.
/// On the blocking pool: the folder may be on a slow drive.
#[tauri::command]
pub async fn discard_extract_folder(win: Win) -> Result<(), IpcError> {
    blocking(move || {
        win.state.destination.discard_made();
        Ok(())
    })
    .await
}

/// Choose the archive to test, check, or repair (D4): the native open dialog
/// (in Rust; the webview never names a path), the file of an archive open in
/// the browser, or the new archive the last repair wrote. `null` if the user
/// cancelled the dialog. Nothing is read yet.
#[tauri::command]
pub async fn choose_check_subject(
    app: AppHandle,
    from: SubjectSource,
    text: Option<DialogText>,
    archives: State<'_, Arc<Archives>>,
    win: Win,
) -> Result<Option<SubjectView>, IpcError> {
    let text = text.unwrap_or_default();
    let archives = Arc::clone(&archives);
    let checks = Arc::clone(&win.state.checks);
    blocking(move || {
        let pick = match from {
            SubjectSource::Dialog => match crate::picker::pick_check_subject(&app, &text) {
                Some(path) => SubjectPick::Path(path),
                None => return Ok(None),
            },
            SubjectSource::OpenArchive { handle } => SubjectPick::OpenArchive(handle),
            SubjectSource::RepairOutput => SubjectPick::RepairOutput,
        };
        checks.choose_subject(&archives, pick).map(Some)
    })
    .await
}

/// The health summary of the chosen archive (D4; `mochi health`): recorded
/// evidence judged at the current head. Runs no check and writes nothing.
#[tauri::command]
pub async fn health_report(subject: SubjectId, win: Win) -> Result<HealthView, IpcError> {
    let checks = Arc::clone(&win.state.checks);
    blocking(move || checks.health(subject)).await
}

/// Export a report (D4): the native save dialog (in Rust), then the exact
/// report v1 JSON that core produced, written to a new file. Returns the
/// file's name, or `null` if the user cancelled the dialog.
#[tauri::command]
pub async fn export_report(
    app: AppHandle,
    report: ReportId,
    text: Option<DialogText>,
    win: Win,
) -> Result<Option<String>, IpcError> {
    let checks = Arc::clone(&win.state.checks);
    let text = text.unwrap_or_default();
    blocking(move || {
        let (suggested, json) = checks.report_json(report)?;
        let Some(path) = crate::picker::pick_report_destination(&app, &suggested, &text) else {
            return Ok(None);
        };
        crate::check::write_new_file(&path, json.as_bytes())?;
        Ok(Some(
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ))
    })
    .await
}

/// Change a repair's setup (D4): add a copy or choose the new file in a
/// native dialog (in Rust), remove a copy, or just report the setup.
#[tauri::command]
pub async fn repair_setup(
    app: AppHandle,
    action: RepairSetupAction,
    text: Option<DialogText>,
    win: Win,
) -> Result<RepairSetupView, IpcError> {
    let checks = Arc::clone(&win.state.checks);
    let text = text.unwrap_or_default();
    blocking(move || {
        let picked = match action {
            RepairSetupAction::AddCopy => crate::picker::pick_copy(&app, &text),
            RepairSetupAction::ChooseOutput => crate::picker::pick_repair_output(&app, &text),
            _ => None,
        };
        checks.setup(action, picked)
    })
    .await
}

/// Open a ZIP, 7z, RAR, tar, tar.gz or tar.zst archive read-only (D8). Rust shows the
/// native dialog, so the webview names no path. `retry` re-uses the file whose
/// password was asked for last (kept in Rust) instead of showing the dialog
/// again; `password` is used for this call and kept in the open session only.
/// `null` if the user cancelled the dialog. A password is never an argument to
/// the helper and never logged.
#[tauri::command]
pub async fn open_foreign_archive(
    app: AppHandle,
    retry: bool,
    password: Option<String>,
    text: Option<DialogText>,
    foreign: State<'_, Arc<Foreign>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<Option<ForeignArchiveSummary>, IpcError> {
    let foreign = Arc::clone(&foreign);
    let launches = Arc::clone(&launches);
    let text = text.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        let path = if retry {
            foreign.pending_for(&win.label).ok_or_else(|| IpcError {
                code: mochi_core::ErrorCode::InvalidArgument.as_str().to_owned(),
                message: "there is no archive waiting for a password".to_owned(),
            })?
        } else {
            match crate::picker::pick_foreign_archive(&app, &text) {
                Some(p) => p,
                None => return Ok(None),
            }
        };
        let opened = launches.while_opening(&win.label, &path, || {
            foreign
                .open_as(&win.label, &path, password.map(String::into_bytes))
                .map(Some)
        });
        launches.refresh_title(&win.label);
        opened
    })
    .await
    .map_err(|_| IpcError {
        code: mochi_core::ErrorCode::Cancelled.as_str().to_owned(),
        message: "the operation stopped before it finished".to_owned(),
    })?
}

/// Close a foreign archive (and forget its password). `false` if not open.
#[tauri::command]
pub async fn close_foreign_archive(
    handle: ArchiveHandle,
    foreign: State<'_, Arc<Foreign>>,
    launches: State<'_, Arc<Launches>>,
    win: Win,
) -> Result<bool, IpcError> {
    // On the blocking pool, as every other archive call: never the event loop.
    let foreign = Arc::clone(&foreign);
    let launches = Arc::clone(&launches);
    blocking(move || {
        let closed = foreign.close(handle);
        launches.refresh_title(&win.label);
        Ok(closed)
    })
    .await
}

/// One page of a foreign archive's directory (`dir` null for the root).
#[tauri::command]
pub async fn foreign_children(
    handle: ArchiveHandle,
    dir: Option<PathKey>,
    offset: u32,
    limit: u32,
    foreign: State<'_, Arc<Foreign>>,
) -> Result<ForeignChildrenPage, IpcError> {
    let foreign = Arc::clone(&foreign);
    blocking(move || foreign.children(handle, dir.as_deref(), offset, limit)).await
}

/// Search the names of every entry of an open foreign archive for `query`
/// (a byte substring of the full path; ASCII case folded when `ignore_case`).
/// Names only, never contents. At most 1000 hits come back; the total is
/// always given.
#[tauri::command]
pub async fn foreign_search(
    handle: ArchiveHandle,
    query: String,
    ignore_case: bool,
    foreign: State<'_, Arc<Foreign>>,
) -> Result<ForeignSearchResult, IpcError> {
    let foreign = Arc::clone(&foreign);
    blocking(move || foreign.search(handle, &query, ignore_case)).await
}

/// Plan extracting `keys` of a foreign archive: optionally show the native
/// folder dialog (in Rust), then say what would collide or be skipped, before
/// anything is written. `null` if the user cancelled the dialog.
#[tauri::command]
pub async fn plan_extract_foreign(
    app: AppHandle,
    handle: ArchiveHandle,
    keys: Vec<PathKey>,
    pick: bool,
    text: Option<DialogText>,
    foreign: State<'_, Arc<Foreign>>,
    win: Win,
) -> Result<Option<ForeignExtractPlan>, IpcError> {
    let text = text.unwrap_or_default();
    let foreign = Arc::clone(&foreign);
    let destination = Arc::clone(&win.state.destination);
    blocking(move || {
        let picked = if pick {
            match crate::picker::pick_extract_folder(&app, &text) {
                Some(folder) => Some(folder),
                None => return Ok(None),
            }
        } else {
            None
        };
        foreign.plan(&destination, picked, handle, &keys).map(Some)
    })
    .await
}

/// T13: the update settings (the opt-in start-up check, off by default) and
/// what this build can do with an update.
#[tauri::command]
pub fn get_update_settings(updates: State<'_, Arc<Updates>>) -> UpdateSettingsView {
    updates.settings()
}

/// T13: turn the start-up check on or off.
#[tauri::command]
pub async fn set_update_settings(
    check_on_start: bool,
    updates: State<'_, Arc<Updates>>,
) -> Result<UpdateSettingsView, IpcError> {
    let updates = Arc::clone(&updates);
    blocking(move || updates.set_check_on_start(check_on_start)).await
}

/// T13: ask the update server whether a newer version exists. The request is
/// made here, in Rust, to the endpoint fixed in `tauri.conf.json`.
/// `automatic` marks the check made when the app starts: Rust makes no
/// request for it unless the user turned the setting on. Nothing is
/// downloaded; the install is the `install_update` job, after the user
/// confirms the version shown. The start-up check is the first window's: a
/// window opened later for another archive makes none (one request per start).
#[tauri::command]
pub async fn check_for_update(
    automatic: bool,
    updates: State<'_, Arc<Updates>>,
    win: Win,
) -> Result<UpdateCheckView, IpcError> {
    if automatic && win.label != crate::windows::MAIN_LABEL {
        return Ok(UpdateCheckView::Skipped);
    }
    let updates = Arc::clone(&updates);
    blocking(move || Ok(updates.check(automatic))).await
}

// ---- D7: the opt-in crash log -------------------------------------------------

fn crash_view(log: &CrashLog) -> CrashSettingsView {
    let settings = log.settings();
    let status = log.status();
    CrashSettingsView {
        enabled: settings.enabled,
        include_details: settings.include_details,
        entries: status.entries,
        bytes: status.bytes,
    }
}

/// The crash-log switches and what the log holds (D7). Both switches start off.
#[tauri::command]
pub async fn get_crash_settings(
    log: State<'_, Arc<CrashLog>>,
) -> Result<CrashSettingsView, IpcError> {
    let log = Arc::clone(&log);
    blocking(move || Ok(crash_view(&log))).await
}

/// Change the crash-log switches (D7). Nothing is sent anywhere either way.
#[tauri::command]
pub async fn set_crash_settings(
    enabled: bool,
    include_details: bool,
    log: State<'_, Arc<CrashLog>>,
) -> Result<CrashSettingsView, IpcError> {
    let log = Arc::clone(&log);
    blocking(move || {
        log.set(Settings {
            enabled,
            include_details,
        })?;
        Ok(crash_view(&log))
    })
    .await
}

fn speed_view() -> SpeedSettingsView {
    SpeedSettingsView {
        all_cores: crate::speed::settings().all_cores,
        fast_extract: crate::speed::settings().fast_extract,
        cores: u32::try_from(crate::speed::cores()).unwrap_or(u32::MAX),
        trust_ntfs_flush: crate::speed::settings().trust_ntfs_flush,
        local_history: crate::speed::settings().local_history,
        wait_for_writer: crate::speed::settings().wait_for_writer,
        windows: cfg!(windows),
    }
}

/// Whether writes compress on every processor core (0.9.11). On by default.
#[tauri::command]
pub async fn get_speed_settings() -> Result<SpeedSettingsView, IpcError> {
    Ok(speed_view())
}

/// Change them; they apply to jobs started from now on. The archive written
/// is the same either way; fast extract trades power-failure durability of
/// extracted files for speed (`speed` module docs).
#[tauri::command]
pub async fn set_speed_settings(
    all_cores: bool,
    fast_extract: bool,
    trust_ntfs_flush: Option<bool>,
    local_history: Option<bool>,
    wait_for_writer: Option<bool>,
) -> Result<SpeedSettingsView, IpcError> {
    blocking(move || {
        // Absent keeps the saved choice (D30, Windows only in effect).
        let trust_ntfs_flush =
            trust_ntfs_flush.unwrap_or(crate::speed::settings().trust_ntfs_flush);
        // Absent keeps the saved choice (Annex B.2.22).
        let local_history = local_history.unwrap_or(crate::speed::settings().local_history);
        // Absent keeps the saved choice (Annex B.2.23).
        let wait_for_writer = wait_for_writer.unwrap_or(crate::speed::settings().wait_for_writer);
        crate::speed::set(crate::speed::Settings {
            all_cores,
            fast_extract,
            trust_ntfs_flush,
            local_history,
            wait_for_writer,
        })?;
        Ok(speed_view())
    })
    .await
}

/// The newest part of the local crash log, to read in the window (D7).
#[tauri::command]
pub async fn read_crash_log(log: State<'_, Arc<CrashLog>>) -> Result<CrashLogView, IpcError> {
    let log = Arc::clone(&log);
    blocking(move || {
        let (text, truncated) = log.read(MAX_READ_BYTES);
        Ok(CrashLogView {
            text,
            truncated,
            entries: log.status().entries,
        })
    })
    .await
}

/// Save a copy of the crash log to a new file the user picks (D7). Returns
/// the file's name, or `null` if the user cancelled the dialog.
#[tauri::command]
pub async fn export_crash_log(
    app: AppHandle,
    text: Option<DialogText>,
    log: State<'_, Arc<CrashLog>>,
) -> Result<Option<String>, IpcError> {
    let log = Arc::clone(&log);
    let text = text.unwrap_or_default();
    blocking(move || {
        let Some(path) = crate::picker::pick_crash_log_destination(&app, &text) else {
            return Ok(None);
        };
        crate::check::write_new_file(&path, &log.export_bytes())?;
        Ok(Some(
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ))
    })
    .await
}

/// Delete the local crash log (D7). The switches stay as they are.
#[tauri::command]
pub async fn clear_crash_log(log: State<'_, Arc<CrashLog>>) -> Result<CrashSettingsView, IpcError> {
    let log = Arc::clone(&log);
    blocking(move || {
        log.clear()?;
        Ok(crash_view(&log))
    })
    .await
}

/// Help, User guide (audit F8): open the bundled guide with the operating system's handler.
/// The webview names no path: Rust resolves the installed resource (`help`).
#[tauri::command]
pub async fn open_user_guide(app: AppHandle) -> Result<(), IpcError> {
    tauri::async_runtime::spawn_blocking(move || crate::help::open_user_guide_for(&app))
        .await
        .map_err(|_| IpcError {
            code: mochi_core::ErrorCode::Cancelled.as_str().to_owned(),
            message: "the operation stopped before it finished".to_owned(),
        })?
}
