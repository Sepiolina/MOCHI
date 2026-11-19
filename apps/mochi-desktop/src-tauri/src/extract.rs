//! Extracting from the desktop app (plan D3): the folder the user chose, the
//! plan that shows what would collide before anything is written, and the job
//! that restores. Thin over `mochi-core` (AGENTS.md "No format logic in the UI
//! or in Tauri commands"): every byte that reaches the disk is written by the
//! C6 restore engine (`mochi_core::restore`), which rejects unsupported
//! names, never follows a link, never overwrites, and publishes a file only
//! after its chunks and content hash verified. This module keeps the chosen
//! folder, calls the engine, and converts its report. Independent of Tauri
//! (the dialog is shown by `commands.rs`), so it is tested without a window.
//!
//! # The webview names no path
//!
//! The folder comes from a native dialog shown by Rust. The webview sees its
//! display text and the plan, and asks for a job by archive handle, snapshot,
//! and entry keys. A script in the webview cannot make the app write
//! anywhere the user did not pick, and the app needs no `fs` or `dialog`
//! permission.
//!
//! # Rules (delegated decisions, recorded in plan D3)
//!
//! * **Collisions.** The engine has exactly one policy: an entry whose name
//!   exists in the folder, or that the folder treats as another entry's name,
//!   is skipped, and nothing is overwritten or renamed. [`plan`] shows the
//!   list before anything is written; the job either stops
//!   ([`ConflictChoice::Stop`]) or skips them ([`ConflictChoice::SkipConflicts`]).
//!   Keep-both and overwrite are not offered: the engine lacks them, and the
//!   app does not invent them (plan D3 "Gap").
//! * **Setuid and setgid are never restored** (plan O6: only on explicit
//!   request, and the app makes no request), and the report says so.
//! * **Cancellation.** The engine discards the file it is writing and stops;
//!   files already published stay, each complete and verified. Directories
//!   made so far keep their private creation modes, because attributes are
//!   applied at the end. The outcome is `cancelled` and carries no counts.
//! * **An extraction is not a verification** (spec §23.3). The report speaks
//!   only of what it extracted.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::JobContext;
use mochi_core::restore::{
    preflight_selected, restore_selected, ExceptionKind, OnExisting, Preflight, RestoreOptions,
    RestoreReport,
};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{AttributeKind, CaseBehavior, DirectoryDurability};
use mochi_core::{ErrorCode, MochiError, Result};

use crate::browse::{parse_key, Archives};
use crate::ipc::{
    AttributeNote, ConflictChoice, ConflictView, ConflictWhy, ExtractPlan, ExtractReport,
    JobRequest, NameText, NotExtractedView, PathKey,
};
use crate::write::JobDone;

/// Most entries one extract may name.
pub const MAX_KEYS: usize = 10_000;

/// Conflicts a plan lists (the total is always given).
pub const MAX_CONFLICTS_SHOWN: usize = 200;

/// Entries a report lists as not extracted (the total is always given).
pub const MAX_NOT_EXTRACTED_SHOWN: usize = 1_000;

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Only a path lives behind this lock; a panic elsewhere cannot leave it
    // half-updated.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The folder the user chose to extract into, kept in Rust.
#[derive(Debug, Default)]
pub struct Destination {
    chosen: Mutex<Option<PathBuf>>,
    /// The folders a plan made, deepest first, until something is extracted into them:
    /// the new folder named after the archive (`plan_default`, `make_new_in`) and the
    /// folders "Extract to..." created because the typed folder did not exist yet
    /// (`with_missing_created`). Closing the plan removes them again while they are empty.
    made: Mutex<Vec<PathBuf>>,
    /// The folder to remember after a successful extraction when it is not
    /// the chosen one itself: the parent a new folder was made in. `set`
    /// clears it (an explicit choice is remembered as chosen).
    record_under: Mutex<Option<PathBuf>>,
    /// The folders extracted into lately ("Extract to..." offers them).
    /// Shared by every window (`WindowStates::history`).
    pub history: Arc<crate::folders::FolderHistory>,
}

impl Destination {
    /// A destination that records into `history` (shared between windows).
    pub fn with_history(history: Arc<crate::folders::FolderHistory>) -> Self {
        Destination {
            history,
            ..Destination::default()
        }
    }

    pub fn get(&self) -> Option<PathBuf> {
        lock(&self.chosen).clone()
    }

    pub fn set(&self, folder: PathBuf) {
        *lock(&self.chosen) = Some(folder);
        *lock(&self.record_under) = None;
    }

    /// Remember `parent` rather than the chosen folder when this extraction
    /// succeeds (the chosen folder is a new one made inside `parent`).
    pub fn record_under(&self, parent: PathBuf) {
        *lock(&self.record_under) = Some(parent);
    }

    /// An extraction into the chosen folder succeeded: remember where.
    pub fn record_success(&self) {
        let parent = lock(&self.record_under).take();
        if let Some(folder) = parent.or_else(|| self.get()) {
            self.history.record(&folder);
        }
    }

    /// Remove the folders a plan made, deepest first, while each is empty. A folder with
    /// anything in it is never touched, and neither is any folder above it.
    pub fn discard_made(&self) {
        let made = std::mem::take(&mut *lock(&self.made));
        for folder in made {
            if std::fs::remove_dir(&folder).is_err() {
                break;
            }
            let mut chosen = lock(&self.chosen);
            if chosen.as_deref() == Some(folder.as_path()) {
                *chosen = None;
            }
        }
    }

    /// Also tidy up `folders` (deepest first, above anything made already) if the plan is
    /// closed unused.
    fn also_made(&self, folders: Vec<PathBuf>) {
        lock(&self.made).extend(folders);
    }
}

/// The folder an extraction goes to when the user does not choose: a new
/// folder next to the archive, named after it (`name`, then `name (2)`, ...).
/// Never an existing folder, so nothing in it can collide.
fn default_folder(archive: &Path) -> Result<PathBuf> {
    let parent = archive
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| invalid("this archive's folder is not known; choose a folder instead"))?;
    let stem = archive
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("this archive's name cannot be used for a folder; choose one"))?;
    (1u32..=99)
        .map(|n| {
            if n == 1 {
                parent.join(stem)
            } else {
                parent.join(format!("{stem} ({n})"))
            }
        })
        .find(|c| !c.exists())
        .ok_or_else(|| invalid("too many folders with this name already exist; choose one"))
}

/// The name of the folder an archive extracts into: its file name without
/// the extension, and without a `.tar` before a compression suffix
/// (`photos.tar.gz` gives `photos`). `None` when nothing usable is left.
pub fn folder_stem(file_name: &str) -> Option<String> {
    let path = Path::new(file_name);
    let mut stem = path.file_stem()?.to_str()?.to_owned();
    if stem.len() > 4 && stem.to_ascii_lowercase().ends_with(".tar") {
        stem.truncate(stem.len() - 4);
    }
    let stem = stem.trim().to_owned();
    (!stem.is_empty() && stem != "." && stem != "..").then_some(stem)
}

/// The `n`th candidate name for a folder: `name`, `name (2)`, `name (3)`, ...
fn numbered(stem: &str, n: u32) -> String {
    if n <= 1 {
        stem.to_owned()
    } else {
        format!("{stem} ({n})")
    }
}

/// Most numbered folders tried before giving up.
const MAX_NUMBERED: u32 = 999;

/// Make a new, empty folder `parent/stem`, or `parent/stem (2)`, `(3)`, ...
/// when that name is taken. The folder is made with `create_dir`, which fails
/// if the name exists, so an existing folder or file is never reused and the
/// check and the creation cannot be raced.
pub fn create_unique_dir(parent: &Path, stem: &str) -> Result<PathBuf> {
    for n in 1..=MAX_NUMBERED {
        let candidate = parent.join(numbered(stem, n));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(MochiError::new(
                    ErrorCode::IoError,
                    format!("could not create the folder {}: {e}", candidate.display()),
                ))
            }
        }
    }
    Err(invalid("too many folders with this name already exist"))
}

impl Destination {
    /// Make a new folder named after `archive` inside `parent`,
    /// remember it as the folder to tidy up if nothing is extracted into it
    /// (`discard_made`). An earlier unused one of this kind is removed first.
    pub fn make_new_in(&self, parent: &Path, archive: &Path) -> Result<PathBuf> {
        self.discard_made();
        let stem = archive
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(folder_stem)
            .ok_or_else(|| invalid("this archive's name cannot be used for a folder"))?;
        let folder = create_unique_dir(parent, &stem)?;
        *lock(&self.made) = vec![folder.clone()];
        Ok(folder)
    }
}

/// The name the next new folder for `stem` in `parent` would get: `stem`, else
/// `stem (2)`, ... (what [`create_unique_dir`] would make now). For a preview only: the
/// folder is made later, with `create_dir`, which never reuses a name that appeared meanwhile.
pub fn next_free_name(parent: &Path, stem: &str) -> Option<String> {
    (1..=MAX_NUMBERED)
        .map(|n| numbered(stem, n))
        .find(|name| !parent.join(name).exists())
}

/// Create `folder` and any folders above it that do not exist yet, top down, each with
/// `create_dir` (a name that appeared meanwhile is used only if it is a folder, and is not
/// counted as ours). Returns the folders this call made, deepest first.
pub fn create_missing(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut at = Some(folder);
    while let Some(p) = at {
        if p.exists() {
            break;
        }
        missing.push(p.to_path_buf());
        at = p.parent().filter(|q| !q.as_os_str().is_empty());
    }
    let mut made = Vec::new();
    for p in missing.iter().rev() {
        match std::fs::create_dir(p) {
            Ok(()) => made.push(p.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && p.is_dir() => {}
            Err(e) => {
                remove_empty(&made);
                return Err(MochiError::new(
                    ErrorCode::IoError,
                    format!("could not create the folder {}: {e}", p.display()),
                ));
            }
        }
    }
    made.reverse();
    Ok(made)
}

/// Remove folders made by [`create_missing`], given in the order they were made (top
/// down), deepest first, while each is empty.
fn remove_empty(made_top_down: &[PathBuf]) {
    for p in made_top_down.iter().rev() {
        if std::fs::remove_dir(p).is_err() {
            break;
        }
    }
}

/// "Extract to..." into `folder`, which may not exist yet: create it (and the folders above
/// it that are missing), then run `plan`. The folders created are tidied up with the plan
/// (`Destination::discard_made`) if nothing is extracted into them, and at once if the plan
/// fails. An earlier unused plan's folders are removed first.
pub fn with_missing_created<T>(
    destination: &Destination,
    folder: &Path,
    plan: impl FnOnce() -> Result<T>,
) -> Result<T> {
    destination.discard_made();
    let created = create_missing(folder)?;
    match plan() {
        Ok(p) => {
            destination.also_made(created);
            Ok(p)
        }
        Err(e) => {
            destination.discard_made();
            let mut top_down = created;
            top_down.reverse();
            remove_empty(&top_down);
            Err(e)
        }
    }
}

/// Plan an extraction of a MOCHI archive into a new folder named after it in
/// `parent` (the folder the user chose), made now. The caller starts the extraction itself;
/// the folder is removed again if the plan fails.
pub fn plan_in_new_folder(
    archives: &Archives,
    destination: &Destination,
    parent: &Path,
    handle: u32,
    seq: u32,
    keys: &[PathKey],
) -> Result<ExtractPlan> {
    let folder = destination.make_new_in(parent, &archives.path(handle)?)?;
    match plan(archives, destination, Some(folder), handle, seq, keys) {
        Ok(p) => {
            destination.record_under(parent.to_path_buf());
            Ok(p)
        }
        Err(e) => {
            destination.discard_made();
            Err(e)
        }
    }
}

/// Plan an extraction into a new folder next to the archive, which is made
/// now (an extraction needs its folder to exist) and removed again if the
/// plan is closed unused (`Destination::discard_made`). An earlier unused
/// folder of this kind is removed first.
pub fn plan_default(
    archives: &Archives,
    destination: &Destination,
    handle: u32,
    seq: u32,
    keys: &[PathKey],
) -> Result<ExtractPlan> {
    destination.discard_made();
    let folder = default_folder(&archives.path(handle)?)?;
    std::fs::create_dir(&folder).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("could not create the folder {}: {e}", folder.display()),
        )
    })?;
    *lock(&destination.made) = vec![folder.clone()];
    let parent = folder.parent().map(Path::to_path_buf);
    match plan(archives, destination, Some(folder), handle, seq, keys) {
        Ok(p) => {
            if let Some(parent) = parent {
                destination.record_under(parent);
            }
            Ok(p)
        }
        Err(e) => {
            destination.discard_made();
            Err(e)
        }
    }
}

fn selection(keys: &[PathKey]) -> Result<Vec<ArchivePath>> {
    if keys.len() > MAX_KEYS {
        return Err(invalid(format!(
            "an extract names at most {MAX_KEYS} entries"
        )));
    }
    keys.iter().map(|k| parse_key(k)).collect()
}

/// `text` as one escaped line: engine and OS messages can carry archive
/// bytes, and the UI shows them as text only.
fn escaped(text: &str) -> NameText {
    NameText::of(text.as_bytes())
}

/// Why an entry was not extracted, as one sentence.
fn reason(kind: &ExceptionKind) -> NameText {
    match kind {
        ExceptionKind::Collision => NameText {
            text: "an entry with this name is already in the folder, or the folder treats the \
                   name as the same as another entry's; nothing was overwritten"
                .to_owned(),
            verbatim: true,
        },
        ExceptionKind::UnsupportedName(issue) => NameText {
            text: format!("this folder's file system cannot hold the name: {issue}"),
            verbatim: true,
        },
        ExceptionKind::ParentNotRestored { .. } => NameText {
            text: "skipped because the folder it belongs to was not extracted".to_owned(),
            verbatim: true,
        },
        ExceptionKind::Integrity { message, .. } => escaped(&format!(
            "the content failed its check against the archive's hashes and was not written: \
             {message}"
        )),
        ExceptionKind::Failed { message, .. } => escaped(message),
    }
}

fn attribute_name(kind: AttributeKind) -> &'static str {
    match kind {
        AttributeKind::Mtime => "modification time",
        AttributeKind::Mode => "permissions",
        AttributeKind::SetId => "setuid and setgid bits",
        AttributeKind::Ownership => "owner and group",
        AttributeKind::ReadOnly => "read-only bit",
        AttributeKind::HiddenOrSystem => "hidden and system bits",
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Refuse a request that cannot work, before a job starts.
pub fn validate(
    archives: &Archives,
    destination: &Destination,
    request: &JobRequest,
) -> Result<()> {
    let JobRequest::ExtractArchive {
        handle, seq, keys, ..
    } = request
    else {
        return Err(invalid("not an extract request"));
    };
    archives.path(*handle)?;
    let _ = seq; // an unknown snapshot is refused when the job opens it
    selection(keys)?;
    if destination.get().is_none() {
        return Err(invalid("choose the folder to extract into first"));
    }
    Ok(())
}

/// What extracting `keys` of snapshot `seq` would meet in the folder, found
/// before anything is written. `picked` is a folder the user just chose (it
/// becomes the destination once the plan succeeds); `None` re-checks the one
/// chosen earlier.
///
/// The folder must be one the engine accepts as a root (on Linux, owned by
/// the user and not writable by others): otherwise the plan fails with the
/// engine's error and nothing was written.
pub fn plan(
    archives: &Archives,
    destination: &Destination,
    picked: Option<PathBuf>,
    handle: u32,
    seq: u32,
    keys: &[PathKey],
) -> Result<ExtractPlan> {
    let folder = match &picked {
        Some(p) => p.clone(),
        None => destination
            .get()
            .ok_or_else(|| invalid("choose the folder to extract into first"))?,
    };
    let wanted = selection(keys)?;
    let (_src, head, _opts) = archives.open_for_read(handle, u64::from(seq))?;
    let mut root = OsRestoreDir::open(&folder)?;
    // Looked at as "Replace" would: which folders exist and would be added to,
    // which files exist (what each choice keeps or replaces), and what cannot
    // be extracted here whatever is chosen. Nothing is written.
    let (pre, entries) = preflight_selected(&head, &wanted, &mut root, OnExisting::Replace)?;
    if let Some(p) = picked {
        destination.set(p);
    }
    Ok(plan_view(&folder, &pre, entries))
}

/// The preflight as IPC text: the first conflicts in path order, the total,
/// and the folder's case behaviour.
fn plan_view(folder: &Path, pre: &Preflight, entries: u64) -> ExtractPlan {
    let conflicts = pre
        .exceptions
        .iter()
        .take(MAX_CONFLICTS_SHOWN)
        .map(|(path, kind)| ConflictView {
            path: NameText::of(path.as_stored()),
            code: kind.code().as_str().to_owned(),
            reason: reason(kind).text,
            why: conflict_why(pre, path, kind),
        })
        .collect();
    ExtractPlan {
        destination: folder.display().to_string(),
        entries: u32::try_from(entries).unwrap_or(u32::MAX),
        existing: pre
            .replaced
            .iter()
            .take(MAX_CONFLICTS_SHOWN)
            .map(|p| NameText::of(p.as_stored()))
            .collect(),
        existing_total: count(pre.replaced.len()),
        merged_folders: count(pre.merged.len()),
        conflicts,
        conflicts_total: count(pre.exceptions.len()),
        case_insensitive: pre.case_behavior == CaseBehavior::Insensitive,
    }
}

/// Why an entry the preflight refused cannot be extracted, as a kind the window
/// words (owner decision 2026-11-11). Shared with foreign archives.
pub(crate) fn conflict_why(
    pre: &Preflight,
    path: &mochi_core::catalog::path::ArchivePath,
    kind: &ExceptionKind,
) -> ConflictWhy {
    match kind {
        ExceptionKind::UnsupportedName(_) => ConflictWhy::NameNotAllowed,
        ExceptionKind::Collision if pre.not_entered.contains(path) => ConflictWhy::FolderNotEntered,
        ExceptionKind::Collision if pre.same_name_in_archive.contains(path) => {
            ConflictWhy::SameNameInArchive
        }
        ExceptionKind::Collision => ConflictWhy::OtherKind,
        _ => ConflictWhy::Other,
    }
}

/// E2E builds only: slows the restore per entry (`MOCHI_E2E_PACE_MS`) so
/// that a spec can cancel one deterministically. Shipped builds never read
/// the variable.
#[cfg(feature = "e2e")]
pub(crate) struct Paced<'a>(pub(crate) &'a dyn mochi_core::job::ProgressSink);

#[cfg(feature = "e2e")]
impl mochi_core::job::ProgressSink for Paced<'_> {
    fn report(&self, event: &mochi_core::job::ProgressEvent) {
        self.0.report(event);
        if let Some(ms) = std::env::var("MOCHI_E2E_PACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// Run an extract job. A job that ends without error (entries it skipped are
/// in its report) is remembered in the recent folders; a failed or cancelled
/// one is not.
pub fn run(
    archives: &Archives,
    destination: &Destination,
    request: &JobRequest,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    let done = run_job(archives, destination, request, ctx)?;
    destination.record_success();
    Ok(done)
}

fn run_job(
    archives: &Archives,
    destination: &Destination,
    request: &JobRequest,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    let JobRequest::ExtractArchive {
        handle,
        seq,
        keys,
        on_conflict,
    } = request
    else {
        return Err(invalid("not an extract request"));
    };
    let wanted = selection(keys)?;
    let folder = destination
        .get()
        .ok_or_else(|| invalid("choose the folder to extract into first"))?;
    let (src, head, opts) = archives.open_for_read(*handle, u64::from(*seq))?;
    let root = OsRestoreDir::open(&folder)?.with_set_aside(crate::trashbin::set_aside());
    let options = RestoreOptions {
        restore_setid: false,
        refuse_on_preflight_exceptions: *on_conflict == ConflictChoice::Stop,
        skip_file_sync: crate::speed::skip_file_sync(),
        on_existing: on_conflict.on_existing(),
    };
    #[cfg(feature = "e2e")]
    let paced = Paced(ctx.progress);
    #[cfg(feature = "e2e")]
    let ctx = &JobContext {
        progress: &paced,
        cancel: ctx.cancel,
    };
    let report = restore_selected(&src, &head, &wanted, root, &options, &opts, ctx)?;
    let view = report_view(&report, &folder, *seq);
    let summary = format!(
        "extracted {} file{} and {} folder{} to {}{}",
        view.files,
        if view.files == 1 { "" } else { "s" },
        view.directories,
        if view.directories == 1 { "" } else { "s" },
        view.destination,
        if view.not_extracted_total == 0 {
            String::new()
        } else {
            format!("; {} not extracted", view.not_extracted_total)
        }
    );
    Ok(JobDone {
        summary,
        commit: None,
        extract: Some(Box::new(view)),
        foreign_test: None,
        check: None,
        search: None,
        rekey: None,
        export: None,
        scan: None,
        retrieve: None,
    })
}

/// The engine's report as IPC text (archive strings escaped).
pub fn report_view(report: &RestoreReport, folder: &Path, seq: u32) -> ExtractReport {
    let not_extracted = report
        .exceptions
        .iter()
        .take(MAX_NOT_EXTRACTED_SHOWN)
        .map(|e| NotExtractedView {
            path: NameText::of(e.path.as_stored()),
            code: e.kind.code().as_str().to_owned(),
            reason: reason(&e.kind),
        })
        .collect();
    let mut by_kind: std::collections::BTreeMap<AttributeKind, (u64, &_)> =
        std::collections::BTreeMap::new();
    for e in &report.attribute_exceptions {
        by_kind.entry(e.issue.attribute).or_insert((0, e)).0 += 1;
    }
    let attribute_notes = by_kind
        .into_iter()
        .map(|(kind, (count, first))| AttributeNote {
            attribute: attribute_name(kind).to_owned(),
            count,
            first_path: NameText::of(first.path.as_stored()),
            reason: escaped(&first.issue.reason),
        })
        .collect();
    ExtractReport {
        destination: folder.display().to_string(),
        seq,
        files: report.files,
        directories: report.directories,
        bytes: report.bytes,
        not_extracted,
        not_extracted_total: report.exceptions.len() as u64,
        attribute_notes,
        attributes_unavailable: report.attributes_unavailable.as_deref().map(escaped),
        durability_note: match &report.directory_durability {
            DirectoryDurability::Confirmed => None,
            DirectoryDurability::Unconfirmed(why) => Some(why.clone()),
        },
        files_not_flushed: !report.file_data_synced,
        case_insensitive: report.case_behavior == CaseBehavior::Insensitive,
        opened_folder: false,
        replaced: report.replaced,
        merged_folders: report.merged_directories,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use mochi_core::job::{CancellationToken, NullProgress, ProgressEvent, ProgressSink};
    use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
    use mochi_core::publish::{ArchiveWriter, ReadOptions, Transaction};
    use mochi_core::restore::{AttributeException, RestoreException};
    use mochi_core::storage::os::OsStorage;
    use mochi_core::storage::AttributeIssue;
    use mochi_testkit::archive::{test_options, Job};
    use mochi_testkit::SeqIds;

    use super::*;
    use crate::ipc::{JobEvent, JobOutcome};
    use crate::jobs::{EventSink, JobRegistry};
    use crate::write::Services;

    type Tree = BTreeMap<String, Option<Vec<u8>>>;

    const HOSTILE: &str = "<img src=x onerror=alert(1)>";
    const BIDI: &str = "rtl\u{202E}gpj.exe";

    fn attrs(mode: u32) -> Attributes {
        Attributes {
            posix: Some(PosixAttributes {
                mode,
                uid: 1000,
                gid: 1000,
            }),
            windows: None,
            mtime: Some(Mtime {
                secs: 1_700_000_000,
                nanos: 0,
            }),
        }
    }

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    /// Two commits. The head holds: `docs/`, `docs/a.txt` ("alpha v2"),
    /// `docs/deep/`, `docs/deep/b.txt`, `top.txt`, `tool` (setuid), and
    /// names that are markup and bidi text. Commit 0 had `docs/a.txt` as
    /// "alpha v1".
    fn archive(dir: &Path) -> PathBuf {
        let path = dir.join("fixture.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(9)),
            test_options(),
        )
        .unwrap();
        let job = Job::new();
        let mut tx = Transaction::new();
        tx.put_dir(p("docs"), attrs(0o755));
        tx.put_file(p("docs/a.txt"), b"alpha v1".to_vec(), attrs(0o644));
        tx.put_dir(p("docs/deep"), attrs(0o755));
        tx.put_file(p("docs/deep/b.txt"), b"bravo".to_vec(), attrs(0o644));
        tx.put_file(p("top.txt"), b"top".to_vec(), attrs(0o640));
        tx.put_file(p("tool"), b"#!tool".to_vec(), attrs(0o4755));
        tx.put_file(p(HOSTILE), b"markup".to_vec(), attrs(0o644));
        tx.put_file(p(BIDI), b"bidi".to_vec(), attrs(0o644));
        w.commit(tx, &job.ctx()).unwrap();
        let mut tx = Transaction::new();
        tx.put_file(p("docs/a.txt"), b"alpha v2".to_vec(), attrs(0o644));
        w.commit(tx, &job.ctx()).unwrap();
        w.close().unwrap();
        path
    }

    struct Fx {
        tmp: tempfile::TempDir,
        archive: PathBuf,
        dest: PathBuf,
        archives: Arc<Archives>,
        destination: Arc<Destination>,
        handle: u32,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let archive = archive(tmp.path());
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        let archives = Arc::new(Archives::default());
        let handle = archives
            .open(&archive, ReadOptions::default())
            .unwrap()
            .handle;
        let destination = Arc::new(Destination::default());
        destination.set(dest.clone());
        Fx {
            tmp,
            archive,
            dest,
            archives,
            destination,
            handle,
        }
    }

    fn walk(base: &Path, dir: &Path, out: &mut Tree) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if e.file_type().unwrap().is_dir() {
                out.insert(rel, None);
                walk(base, &e.path(), out);
            } else {
                out.insert(rel, Some(std::fs::read(e.path()).unwrap()));
            }
        }
    }

    fn tree(root: &Path) -> Tree {
        let mut out = Tree::new();
        walk(root, root, &mut out);
        out
    }

    fn file(s: &str) -> Option<Vec<u8>> {
        Some(s.as_bytes().to_vec())
    }

    /// The head's namespace as the destination can hold it: Windows cannot
    /// hold the markup name.
    fn head_tree() -> Tree {
        let mut t = Tree::new();
        t.insert("docs".into(), None);
        t.insert("docs/a.txt".into(), file("alpha v2"));
        t.insert("docs/deep".into(), None);
        t.insert("docs/deep/b.txt".into(), file("bravo"));
        t.insert("top.txt".into(), file("top"));
        t.insert("tool".into(), file("#!tool"));
        t.insert(BIDI.into(), file("bidi"));
        if !cfg!(windows) {
            t.insert(HOSTILE.into(), file("markup"));
        }
        t
    }

    /// The choice for extracting the whole head: Windows cannot hold the
    /// markup name, so there the job must be told to skip it (`Stop` would
    /// refuse before writing, which `stopping_on_a_conflict…` covers).
    fn whole() -> ConflictChoice {
        if cfg!(windows) {
            ConflictChoice::SkipConflicts
        } else {
            ConflictChoice::Stop
        }
    }

    fn request(f: &Fx, seq: u32, keys: &[&str], on_conflict: ConflictChoice) -> JobRequest {
        JobRequest::ExtractArchive {
            handle: f.handle,
            seq,
            keys: keys.iter().map(|k| hex(k.as_bytes())).collect(),
            on_conflict,
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn go(f: &Fx, req: &JobRequest) -> Result<JobDone> {
        validate(&f.archives, &f.destination, req)?;
        let cancel = CancellationToken::new();
        run(
            &f.archives,
            &f.destination,
            req,
            &JobContext {
                progress: &NullProgress,
                cancel: &cancel,
            },
        )
    }

    fn report(done: JobDone) -> ExtractReport {
        *done.extract.expect("an extract report")
    }

    fn no_temporaries(t: &Tree) {
        assert!(t.keys().all(|k| !k.contains(".mochi-restore.")), "{t:?}");
    }

    // ---- extract all, a snapshot, a selection ----

    #[test]
    fn extract_all_writes_the_head_and_leaves_the_archive_untouched() {
        let f = fx();
        let before = std::fs::read(&f.archive).unwrap();
        let r = report(go(&f, &request(&f, 1, &[], whole())).unwrap());
        let got = tree(&f.dest);
        assert_eq!(got, head_tree());
        no_temporaries(&got);
        let files = head_tree().values().filter(|v| v.is_some()).count() as u64;
        assert_eq!((r.files, r.directories), (files, 2));
        assert_eq!(
            r.bytes,
            head_tree()
                .values()
                .flatten()
                .map(|b| b.len() as u64)
                .sum::<u64>()
        );
        assert_eq!(r.not_extracted_total, u64::from(cfg!(windows)));
        assert_eq!(std::fs::read(&f.archive).unwrap(), before, "read-only");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let m = std::fs::metadata(f.dest.join("top.txt")).unwrap();
            assert_eq!(m.permissions().mode() & 0o7777, 0o640);
            assert_eq!(m.mtime(), 1_700_000_000);
        }
    }

    /// The windows case: the markup name is reported, as text.
    #[test]
    fn a_name_the_folder_cannot_hold_is_reported_not_renamed() {
        let f = fx();
        let r = report(go(&f, &request(&f, 1, &[], ConflictChoice::SkipConflicts)).unwrap());
        if cfg!(windows) {
            let e = r
                .not_extracted
                .iter()
                .find(|e| e.path.text == HOSTILE)
                .expect("the markup name is listed");
            assert_eq!(e.code, "NAME_UNSUPPORTED");
            assert!(e.path.verbatim, "markup is shown as it is, as text");
            assert_eq!(r.not_extracted_total, 1);
        } else {
            assert!(r.not_extracted.is_empty());
            assert!(f.dest.join(HOSTILE).exists());
        }
    }

    #[test]
    fn the_snapshot_on_screen_is_the_one_extracted() {
        let f = fx();
        go(&f, &request(&f, 0, &[], whole())).unwrap();
        assert_eq!(tree(&f.dest)["docs/a.txt"], file("alpha v1"));
        let g = fx();
        go(&g, &request(&g, 1, &[], whole())).unwrap();
        assert_eq!(tree(&g.dest)["docs/a.txt"], file("alpha v2"));
    }

    #[test]
    fn a_selected_file_comes_with_its_folders_and_nothing_else() {
        let f = fx();
        let r = report(
            go(
                &f,
                &request(&f, 1, &["docs/deep/b.txt"], ConflictChoice::Stop),
            )
            .unwrap(),
        );
        let mut want = Tree::new();
        want.insert("docs".into(), None);
        want.insert("docs/deep".into(), None);
        want.insert("docs/deep/b.txt".into(), file("bravo"));
        assert_eq!(tree(&f.dest), want);
        assert_eq!((r.files, r.directories), (1, 2));
    }

    #[test]
    fn a_selected_folder_brings_its_subtree_and_several_selections_combine() {
        let f = fx();
        go(&f, &request(&f, 1, &["docs"], ConflictChoice::Stop)).unwrap();
        let want: Tree = head_tree()
            .into_iter()
            .filter(|(k, _)| k.starts_with("docs"))
            .collect();
        assert_eq!(tree(&f.dest), want);

        let g = fx();
        go(
            &g,
            &request(&g, 1, &["top.txt", "tool"], ConflictChoice::Stop),
        )
        .unwrap();
        let want: Tree = head_tree()
            .into_iter()
            .filter(|(k, _)| k == "top.txt" || k == "tool")
            .collect();
        assert_eq!(tree(&g.dest), want);
    }

    #[test]
    fn an_entry_the_snapshot_lacks_is_refused_before_anything_is_written() {
        let f = fx();
        let e = go(&f, &request(&f, 1, &["nope"], ConflictChoice::Stop)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(tree(&f.dest).is_empty());
        let e = plan(
            &f.archives,
            &f.destination,
            None,
            f.handle,
            1,
            &[hex(b"nope")],
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let e = go(&f, &request(&f, 9, &[], ConflictChoice::Stop)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "no commit 9");
    }

    // ---- collisions ----

    /// An existing `top.txt`, an existing folder `docs`, and an existing
    /// file named like the bidi entry.
    fn crowded(f: &Fx) {
        std::fs::write(f.dest.join("top.txt"), b"mine").unwrap();
        std::fs::create_dir(f.dest.join("docs")).unwrap();
        std::fs::write(f.dest.join("docs/keep.txt"), b"keep").unwrap();
        std::fs::write(f.dest.join(BIDI), b"mine too").unwrap();
    }

    /// Owner decision 2026-11-11: the plan says which files already exist
    /// (kept by "Add only new files", moved to the Bin by "Replace existing
    /// files"), which folders would be added to, and what cannot be extracted
    /// here whatever is chosen. Planning writes nothing.
    #[test]
    fn the_plan_lists_existing_files_merged_folders_and_conflicts_and_writes_nothing() {
        let f = fx();
        crowded(&f);
        let before = tree(&f.dest);
        let plan = plan(&f.archives, &f.destination, None, f.handle, 1, &[]).unwrap();
        assert_eq!(tree(&f.dest), before, "the plan wrote nothing");
        let existing: Vec<&str> = plan.existing.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(existing, ["rtl\\u{202e}gpj.exe", "top.txt"]);
        assert_eq!((plan.existing_total, plan.merged_folders), (2, 1));
        // Only Windows has something that cannot be extracted at all.
        assert_eq!(plan.conflicts_total, u32::from(cfg!(windows)));
        if cfg!(windows) {
            assert_eq!(plan.conflicts[0].path.text, HOSTILE);
            assert_eq!(plan.conflicts[0].code, "NAME_UNSUPPORTED");
            assert_eq!(plan.conflicts[0].why, ConflictWhy::NameNotAllowed);
        }
        assert_eq!(plan.entries, 8);
        assert_eq!(plan.destination, f.dest.display().to_string());
    }

    /// "Add only new files": the existing folder is added to, every existing
    /// file keeps its bytes, and nothing goes to the Bin.
    #[test]
    fn add_new_merges_into_existing_folders_and_keeps_existing_files() {
        let f = fx();
        crowded(&f);
        let done = report(go(&f, &request(&f, 1, &[], ConflictChoice::AddNew)).unwrap());
        assert_eq!(std::fs::read(f.dest.join("top.txt")).unwrap(), b"mine");
        assert_eq!(
            std::fs::read(f.dest.join("docs/keep.txt")).unwrap(),
            b"keep"
        );
        assert_eq!(
            std::fs::read(f.dest.join("docs/a.txt")).unwrap(),
            b"alpha v2"
        );
        assert_eq!(
            std::fs::read(f.dest.join("docs/deep/b.txt")).unwrap(),
            b"bravo"
        );
        assert_eq!((done.replaced, done.merged_folders), (0, 1));
        assert_eq!(done.not_extracted_total, 2 + u64::from(cfg!(windows)));
        no_temporaries(&tree(&f.dest));
    }

    /// "Replace existing files": each existing file goes to the Bin (the test
    /// stand-in) with its bytes, the archive's takes its name, and a file the
    /// archive does not have is never touched.
    #[test]
    fn replace_moves_existing_files_to_the_bin() {
        let f = fx();
        crowded(&f);
        let bin = crate::trashbin::test_bin();
        let done = report(go(&f, &request(&f, 1, &[], ConflictChoice::Replace)).unwrap());
        assert_eq!(std::fs::read(f.dest.join("top.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(f.dest.join(BIDI)).unwrap(), b"bidi");
        assert_eq!(
            std::fs::read(f.dest.join("docs/keep.txt")).unwrap(),
            b"keep"
        );
        assert_eq!((done.replaced, done.merged_folders), (2, 1));
        assert_eq!(done.not_extracted_total, u64::from(cfg!(windows)));
        // The old files are in the bin, under their own names (other tests
        // may have put theirs there too: look for this test's bytes).
        let binned: Vec<Vec<u8>> = std::fs::read_dir(&bin)
            .unwrap()
            .map(|e| std::fs::read(e.unwrap().path()).unwrap())
            .collect();
        assert!(binned.contains(&b"mine".to_vec()));
        assert!(binned.contains(&b"mine too".to_vec()));
        no_temporaries(&tree(&f.dest));
    }

    #[test]
    fn stopping_on_a_conflict_writes_nothing_and_keeps_every_existing_byte() {
        let f = fx();
        crowded(&f);
        let before = tree(&f.dest);
        let e = go(&f, &request(&f, 1, &[], ConflictChoice::Stop)).unwrap_err();
        // The first conflict in path order: on Windows, the markup name.
        let first = if cfg!(windows) {
            ErrorCode::NameUnsupported
        } else {
            ErrorCode::NameCollision
        };
        assert_eq!(e.code, first);
        assert!(e.message.contains("nothing was written"), "{e}");
        assert_eq!(tree(&f.dest), before);
    }

    #[test]
    fn skipping_conflicts_extracts_the_rest_overwrites_nothing_and_reports_each() {
        let f = fx();
        crowded(&f);
        let r = report(go(&f, &request(&f, 1, &[], ConflictChoice::SkipConflicts)).unwrap());
        let got = tree(&f.dest);
        assert_eq!(got["top.txt"], file("mine"), "never overwritten");
        assert_eq!(got["docs/keep.txt"], file("keep"));
        assert_eq!(got[BIDI], file("mine too"));
        assert!(
            !got.contains_key("docs/a.txt"),
            "a skipped folder's subtree is skipped"
        );
        assert_eq!(got["tool"], file("#!tool"), "the rest is extracted");
        no_temporaries(&got);
        let by_path: BTreeMap<&str, &NotExtractedView> = r
            .not_extracted
            .iter()
            .map(|e| (e.path.text.as_str(), e))
            .collect();
        assert_eq!(by_path["top.txt"].code, "NAME_COLLISION");
        assert_eq!(by_path["docs"].code, "NAME_COLLISION");
        // Entries inside the skipped folder are skipped with its cause.
        assert_eq!(by_path["docs/a.txt"].code, "NAME_COLLISION");
        assert!(by_path["docs/a.txt"].reason.text.contains("not extracted"));
        assert_eq!(
            r.not_extracted_total,
            r.not_extracted.len() as u64,
            "all listed (fewer than the cap)"
        );
    }

    /// Archive text in the report is escaped: the bidi override is not
    /// shown raw, and is marked as escaped.
    #[test]
    fn names_in_the_report_are_escaped_text() {
        let f = fx();
        crowded(&f);
        let r = report(go(&f, &request(&f, 1, &[], ConflictChoice::SkipConflicts)).unwrap());
        let e = r
            .not_extracted
            .iter()
            .find(|e| e.path.text.starts_with("rtl"))
            .unwrap();
        assert_eq!(e.path.text, "rtl\\u{202e}gpj.exe");
        assert!(!e.path.verbatim);
        assert!(!e.path.text.contains('\u{202E}'));
    }

    // ---- attributes ----

    #[cfg(unix)]
    #[test]
    fn setuid_is_never_restored_and_the_report_says_so() {
        use std::os::unix::fs::PermissionsExt;
        let f = fx();
        let r = report(go(&f, &request(&f, 1, &["tool"], ConflictChoice::Stop)).unwrap());
        let mode = std::fs::metadata(f.dest.join("tool"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7000, 0, "no setuid bit: {mode:o}");
        assert_eq!(mode & 0o777, 0o755);
        let note = r
            .attribute_notes
            .iter()
            .find(|n| n.attribute == "setuid and setgid bits")
            .expect("reported");
        assert_eq!(note.count, 1);
        assert_eq!(note.first_path.text, "tool");
    }

    // ---- cancellation ----

    /// Cancels once the restore has handled `n` entries.
    struct CancelAfter(CancellationToken, u64);
    impl ProgressSink for CancelAfter {
        fn report(&self, e: &ProgressEvent) {
            if e.phase == "restore" && e.completed >= self.1 {
                self.0.cancel();
            }
        }
    }

    #[test]
    fn a_cancelled_extraction_leaves_only_complete_files() {
        let f = fx();
        let token = CancellationToken::new();
        let sink = CancelAfter(token.clone(), 4);
        let req = request(&f, 1, &[], whole());
        validate(&f.archives, &f.destination, &req).unwrap();
        let e = run(
            &f.archives,
            &f.destination,
            &req,
            &JobContext {
                progress: &sink,
                cancel: &token,
            },
        )
        .map(|_| ())
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        let got = tree(&f.dest);
        no_temporaries(&got);
        assert!(!got.is_empty(), "the entries before the cancel stay");
        let all = head_tree();
        assert!(got.len() < all.len(), "stopped early: {got:?}");
        for (k, v) in &got {
            assert_eq!(all.get(k), Some(v), "{k} is complete, not partial");
        }
    }

    // ---- the registry ----

    struct Collect(
        std::sync::Mutex<mpsc::Sender<JobEvent>>,
        Arc<JobRegistry>,
        u64,
    );
    impl EventSink for Collect {
        fn send(&self, e: JobEvent) -> bool {
            if let JobEvent::Progress {
                job_id,
                phase,
                completed,
                ..
            } = &e
            {
                if phase == "restore" && *completed >= self.2 {
                    self.1.cancel(*job_id);
                }
            }
            let _ = self.0.lock().unwrap().send(e);
            true
        }
    }

    fn registry(f: &Fx) -> Arc<JobRegistry> {
        Arc::new(JobRegistry::with_services(Services {
            archives: Arc::clone(&f.archives),
            staging: Arc::default(),
            destination: Arc::clone(&f.destination),
            foreign: Arc::default(),
            checks: Arc::default(),
            updates: Arc::default(),
            after_extract: None,
            finder: None,
        }))
    }

    fn finish(reg: &Arc<JobRegistry>, req: JobRequest, cancel_at: u64) -> (u32, Vec<JobEvent>) {
        let (tx, rx) = mpsc::channel();
        let id = reg
            .start(
                req,
                Arc::new(Collect(
                    std::sync::Mutex::new(tx),
                    Arc::clone(reg),
                    cancel_at,
                )),
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
                return (id, events);
            }
        }
    }

    #[test]
    fn an_extract_streams_progress_and_one_finished_with_its_report() {
        let f = fx();
        let reg = registry(&f);
        let (id, events) = finish(&reg, request(&f, 1, &[], whole()), u64::MAX);
        assert_eq!(events[0], JobEvent::Started { job_id: id });
        let totals: Vec<(u64, Option<u64>)> = events
            .iter()
            .filter_map(|e| match e {
                JobEvent::Progress {
                    phase,
                    completed,
                    total,
                    ..
                } if phase == "restore" => Some((*completed, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(totals.first(), Some(&(0, Some(8))));
        assert_eq!(totals.last(), Some(&(8, Some(8))));
        let finished: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, JobEvent::Finished { .. }))
            .collect();
        assert_eq!(finished.len(), 1);
        let JobEvent::Finished {
            outcome: JobOutcome::Completed {
                extract, commit, ..
            },
            ..
        } = finished[0]
        else {
            panic!("completed: {finished:?}")
        };
        assert!(commit.is_none());
        assert_eq!(extract.as_ref().unwrap().directories, 2);
        assert_eq!(tree(&f.dest), head_tree());
    }

    /// Owner request 2026-11-10: an extraction in an archive window opens its folder when
    /// done, unless "Open the folder when done" is off, and the report says which.
    #[test]
    fn a_finished_extraction_opens_its_folder_unless_that_is_turned_off() {
        let f = fx();
        let after = Arc::new(crate::afterextract::AfterExtract::default());
        let opened = Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
        let log = Arc::clone(&opened);
        after.set_opener(Arc::new(move |p: &Path| {
            log.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }));
        let reg = Arc::new(JobRegistry::with_services(Services {
            archives: Arc::clone(&f.archives),
            destination: Arc::clone(&f.destination),
            after_extract: Some(Arc::clone(&after)),
            ..Services::default()
        }));
        let opened_in = |events: &[JobEvent]| match events.last() {
            Some(JobEvent::Finished {
                outcome: JobOutcome::Completed { extract, .. },
                ..
            }) => extract.as_ref().unwrap().opened_folder,
            other => panic!("completed: {other:?}"),
        };
        let (_, events) = finish(&reg, request(&f, 1, &["top.txt"], whole()), u64::MAX);
        assert!(opened_in(&events));
        assert_eq!(*opened.lock().unwrap(), vec![f.dest.clone()]);

        after.set(false).unwrap();
        let again = f.tmp.path().join("again");
        std::fs::create_dir(&again).unwrap();
        f.destination.set(again);
        let (_, events) = finish(&reg, request(&f, 1, &["top.txt"], whole()), u64::MAX);
        assert!(!opened_in(&events));
        assert_eq!(opened.lock().unwrap().len(), 1);

        // A cancelled extraction opens nothing, whatever the choice.
        after.set(true).unwrap();
        let third = f.tmp.path().join("third");
        std::fs::create_dir(&third).unwrap();
        f.destination.set(third);
        // Cancelled on the first forwarded event (`cancelling_over_the_registry...`).
        let (_, events) = finish(&reg, request(&f, 1, &[], whole()), 0);
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Cancelled,
                ..
            })
        ));
        assert_eq!(opened.lock().unwrap().len(), 1);
    }

    /// Owner request 2026-11-10: "Extract to..." into a folder that does not exist yet
    /// creates it (and the folders above it), and tidies them up again if unused.
    #[test]
    fn a_folder_that_does_not_exist_is_created_and_removed_again_if_unused() {
        let f = fx();
        let root = f.tmp.path().join("new");
        let deeper = root.join("deeper");
        let d = Destination::default();
        with_missing_created(&d, &deeper, || {
            plan(&f.archives, &d, Some(deeper.clone()), f.handle, 1, &[])
        })
        .unwrap();
        assert!(deeper.is_dir());
        assert_eq!(d.get(), Some(deeper.clone()));
        // The plan is closed unused: both folders go, the one above them stays.
        d.discard_made();
        assert!(!root.exists());
        assert!(f.tmp.path().is_dir());
        assert_eq!(d.get(), None);

        // With a new folder named after the archive inside it: three levels, all tidied up.
        with_missing_created(&d, &deeper, || {
            plan_in_new_folder(&f.archives, &d, &deeper, f.handle, 1, &[])
        })
        .unwrap();
        let named = d.get().unwrap();
        assert_eq!(named.parent(), Some(deeper.as_path()));
        d.discard_made();
        assert!(!root.exists());
    }

    #[test]
    fn a_created_folder_is_kept_once_something_is_extracted_into_it() {
        let f = fx();
        let made = f.tmp.path().join("made").join("here");
        with_missing_created(&f.destination, &made, || {
            plan(
                &f.archives,
                &f.destination,
                Some(made.clone()),
                f.handle,
                1,
                &[],
            )
        })
        .unwrap();
        report(go(&f, &request(&f, 1, &["top.txt"], whole())).unwrap());
        assert_eq!(std::fs::read(made.join("top.txt")).unwrap(), b"top");
        // A later plan tidies only what is empty: this folder and the one above stay.
        f.destination.discard_made();
        assert!(made.join("top.txt").is_file());
    }

    #[test]
    fn a_failing_plan_removes_the_folders_it_created_and_a_file_in_the_way_is_refused() {
        let f = fx();
        let root = f.tmp.path().join("fresh");
        let d = Destination::default();
        let r = with_missing_created(&d, &root.join("x"), || {
            plan(
                &f.archives,
                &d,
                Some(root.join("x")),
                f.handle,
                1,
                &[hex(b"nope")],
            )
        });
        assert!(r.is_err());
        assert!(!root.exists());
        // A file where a folder would go: refused, and nothing is created.
        let file = f.tmp.path().join("a-file");
        std::fs::write(&file, b"mine").unwrap();
        assert!(crate::folders::folder_to_extract_into(file.to_str().unwrap()).is_err());
        assert!(create_missing(&file.join("below")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"mine");
        // An existing folder is used as it is: nothing is created.
        assert!(create_missing(&f.dest).unwrap().is_empty());
    }

    #[test]
    fn cancelling_over_the_registry_reports_cancelled_and_leaves_no_partial_file() {
        let f = fx();
        let reg = registry(&f);
        // Cancel on the first forwarded event: within one phase the
        // forwarder sends at most one event per 50 ms, so a later count may
        // never be sent for a job this small. Cancelling mid-way is
        // `a_cancelled_extraction_leaves_only_complete_files`.
        let (_, events) = finish(&reg, request(&f, 1, &[], whole()), 0);
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Cancelled,
                ..
            })
        ));
        let got = tree(&f.dest);
        no_temporaries(&got);
        let all = head_tree();
        for (k, v) in &got {
            assert_eq!(all.get(k), Some(v), "{k}");
        }
        assert!(got.len() < all.len());
        // The registry is free again, and the archive is still open.
        assert_eq!(reg.running(), 0);
        assert!(f.archives.children(f.handle, 1, None, false, 0, 10).is_ok());
    }

    // ---- requests the app refuses ----

    #[test]
    fn requests_that_cannot_work_are_refused_before_a_job_starts() {
        let f = fx();
        let none = Destination::default();
        let req = request(&f, 1, &[], ConflictChoice::Stop);
        let e = validate(&f.archives, &none, &req).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "no folder chosen");
        let closed = JobRequest::ExtractArchive {
            handle: 999,
            seq: 1,
            keys: vec![],
            on_conflict: ConflictChoice::Stop,
        };
        assert!(validate(&f.archives, &f.destination, &closed).is_err());
        let bad_key = JobRequest::ExtractArchive {
            handle: f.handle,
            seq: 1,
            keys: vec!["not hex".into()],
            on_conflict: ConflictChoice::Stop,
        };
        assert!(validate(&f.archives, &f.destination, &bad_key).is_err());
        let many = JobRequest::ExtractArchive {
            handle: f.handle,
            seq: 1,
            keys: vec![hex(b"a"); MAX_KEYS + 1],
            on_conflict: ConflictChoice::Stop,
        };
        assert!(validate(&f.archives, &f.destination, &many).is_err());
        let e = plan(&f.archives, &none, None, f.handle, 1, &[]).unwrap_err();
        assert_eq!(
            e.code,
            ErrorCode::InvalidArgument,
            "nothing chosen to re-check"
        );
        assert!(tree(&f.dest).is_empty());
    }

    #[test]
    fn a_chosen_folder_is_remembered_only_when_the_plan_succeeds() {
        let f = fx();
        let other = f.tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let d = Destination::default();
        plan(&f.archives, &d, Some(other.clone()), f.handle, 1, &[]).unwrap();
        assert_eq!(d.get(), Some(other));
        // A failing plan keeps the earlier choice.
        let missing = f.tmp.path().join("missing");
        assert!(plan(&f.archives, &d, Some(missing), f.handle, 1, &[hex(b"nope")]).is_err());
        assert_eq!(d.get(), Some(f.tmp.path().join("other")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_folder_other_users_can_modify_is_refused_before_anything_is_written() {
        use std::os::unix::fs::PermissionsExt;
        let f = fx();
        std::fs::set_permissions(&f.dest, std::fs::Permissions::from_mode(0o777)).unwrap();
        let e = plan(&f.archives, &f.destination, None, f.handle, 1, &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{e}");
        let e = go(&f, &request(&f, 1, &[], ConflictChoice::Stop)).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{e}");
        assert!(tree(&f.dest).is_empty());
    }

    #[test]
    fn open_for_read_gives_each_snapshot_and_refuses_unknown_ones() {
        let f = fx();
        let (_, head, _) = f.archives.open_for_read(f.handle, 0).unwrap();
        assert_eq!(head.seq(), 0);
        let (_, head, _) = f.archives.open_for_read(f.handle, 1).unwrap();
        assert_eq!(head.seq(), 1);
        assert_eq!(
            f.archives
                .open_for_read(f.handle, 5)
                .map(|_| ())
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        f.archives.close(f.handle);
        assert!(f.archives.open_for_read(f.handle, 1).is_err());
    }

    // ---- the report ----

    #[test]
    fn a_long_report_is_capped_but_counts_everything() {
        let mut r = RestoreReport {
            files: 3,
            directories: 1,
            bytes: 10,
            exceptions: Vec::new(),
            directory_durability: DirectoryDurability::Unconfirmed("no sync".into()),
            file_data_synced: false,
            attributes_unavailable: Some("bad\u{1b}[31m manifest".into()),
            attribute_exceptions: Vec::new(),
            case_behavior: CaseBehavior::Insensitive,
            merged_directories: 0,
            replaced: 0,
        };
        for i in 0..MAX_NOT_EXTRACTED_SHOWN + 5 {
            r.exceptions.push(RestoreException {
                path: p(&format!("f{i:05}")),
                kind: ExceptionKind::Failed {
                    code: ErrorCode::IoError,
                    message: "disk\nfull".into(),
                },
            });
        }
        for path in ["a", "b"] {
            r.attribute_exceptions.push(AttributeException {
                path: p(path),
                issue: AttributeIssue {
                    attribute: AttributeKind::Ownership,
                    reason: "needs privilege".into(),
                },
            });
        }
        let v = report_view(&r, Path::new("/x"), 4);
        assert_eq!(v.not_extracted.len(), MAX_NOT_EXTRACTED_SHOWN);
        assert_eq!(v.not_extracted_total, (MAX_NOT_EXTRACTED_SHOWN + 5) as u64);
        assert_eq!(v.not_extracted[0].code, "IO_ERROR");
        assert_eq!(v.not_extracted[0].reason.text, "disk\\u{a}full");
        assert!(!v.not_extracted[0].reason.verbatim);
        assert_eq!(v.attribute_notes.len(), 1, "one note per kind");
        assert_eq!(v.attribute_notes[0].count, 2);
        assert_eq!(v.attribute_notes[0].first_path.text, "a");
        assert_eq!(v.durability_note.as_deref(), Some("no sync"));
        assert!(v.files_not_flushed);
        assert!(v.case_insensitive);
        let why = v.attributes_unavailable.unwrap();
        assert!(!why.text.contains('\u{1b}') && !why.verbatim);
        assert_eq!(v.seq, 4);
    }

    #[test]
    fn a_plan_says_when_the_folder_folds_case() {
        let mut pre = Preflight {
            case_behavior: CaseBehavior::Insensitive,
            exceptions: BTreeMap::new(),
            merged: Default::default(),
            replaced: Default::default(),
            not_entered: Default::default(),
            same_name_in_archive: Default::default(),
        };
        pre.exceptions.insert(p("Readme"), ExceptionKind::Collision);
        let v = plan_view(Path::new("/x"), &pre, 7);
        assert!(v.case_insensitive);
        assert_eq!((v.entries, v.conflicts_total), (7, 1));
        assert_eq!(v.conflicts[0].code, "NAME_COLLISION");
        pre.case_behavior = CaseBehavior::Sensitive;
        assert!(!plan_view(Path::new("/x"), &pre, 7).case_insensitive);
    }

    #[test]
    fn a_plan_lists_at_most_the_cap_but_counts_every_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("many.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(3)),
            test_options(),
        )
        .unwrap();
        let n = MAX_CONFLICTS_SHOWN + 30;
        let mut tx = Transaction::new();
        let dest = tmp.path().join("out");
        std::fs::create_dir(&dest).unwrap();
        for i in 0..n {
            tx.put_file(p(&format!("f{i:04}")), vec![1], attrs(0o644));
            std::fs::write(dest.join(format!("f{i:04}")), b"mine").unwrap();
        }
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
        let archives = Archives::default();
        let handle = archives.open(&path, ReadOptions::default()).unwrap().handle;
        let d = Destination::default();
        d.set(dest);
        let plan = plan(&archives, &d, None, handle, 0, &[]).unwrap();
        // Every name exists as a file: listed as existing (at most the cap),
        // counted in full.
        assert_eq!(plan.existing.len(), MAX_CONFLICTS_SHOWN);
        assert_eq!(plan.existing_total as usize, n);
        assert_eq!(plan.conflicts_total, 0);
        assert_eq!(plan.entries as usize, n);
        assert_eq!(plan.existing[0].text, "f0000", "in path order");
    }
    #[test]
    fn the_default_folder_is_new_next_to_the_archive_and_tidied_when_unused() {
        let f = fx();
        let made = f
            .archive
            .parent()
            .unwrap()
            .join(f.archive.file_stem().unwrap());
        let plan = plan_default(&f.archives, &f.destination, f.handle, 1, &[]).unwrap();
        assert!(made.is_dir());
        assert_eq!(plan.destination, made.display().to_string());
        // Nothing already there can collide; on Windows the markup name is
        // unsupported in any folder (as the other plan tests expect).
        assert_eq!(
            plan.conflicts_total,
            u32::from(cfg!(windows)),
            "a new folder has nothing to collide with"
        );
        assert_eq!(f.destination.get(), Some(made.clone()));
        f.destination.discard_made();
        assert!(!made.exists(), "an unused default folder is removed");
        assert_eq!(f.destination.get(), None);
    }

    #[test]
    fn the_default_folder_never_reuses_an_existing_one_and_is_never_removed_when_used() {
        let f = fx();
        let base = f
            .archive
            .parent()
            .unwrap()
            .join(f.archive.file_stem().unwrap());
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("mine.txt"), b"mine").unwrap();
        let second = PathBuf::from(format!("{} (2)", base.display()));
        plan_default(&f.archives, &f.destination, f.handle, 1, &[]).unwrap();
        assert!(second.is_dir());
        assert_eq!(f.destination.get(), Some(second.clone()));
        std::fs::write(second.join("extracted.txt"), b"x").unwrap();
        f.destination.discard_made();
        assert!(
            second.join("extracted.txt").is_file(),
            "a folder with files stays"
        );
        assert_eq!(std::fs::read(base.join("mine.txt")).unwrap(), b"mine");
    }

    #[test]
    fn folder_names_drop_the_extension_and_a_tar_before_a_compression_suffix() {
        assert_eq!(folder_stem("photos.mochi").as_deref(), Some("photos"));
        assert_eq!(folder_stem("photos.tar.gz").as_deref(), Some("photos"));
        assert_eq!(folder_stem("Photos.TAR.zst").as_deref(), Some("Photos"));
        assert_eq!(folder_stem("a.b.zip").as_deref(), Some("a.b"));
        assert_eq!(folder_stem("noext").as_deref(), Some("noext"));
        assert_eq!(folder_stem(""), None);
        assert_eq!(folder_stem(".."), None);
    }

    #[test]
    fn a_new_desktop_folder_is_numbered_when_the_name_is_taken_and_nothing_is_reused() {
        let desktop = tempfile::tempdir().unwrap();
        let archive = Path::new("/somewhere/photos.tar.gz");
        let d = Destination::default();
        let first = d.make_new_in(desktop.path(), archive).unwrap();
        assert_eq!(first, desktop.path().join("photos"));
        std::fs::write(first.join("mine.txt"), b"mine").unwrap();
        let second = d.make_new_in(desktop.path(), archive).unwrap();
        assert_eq!(second, desktop.path().join("photos (2)"));
        // The unused second folder is tidied up when the next one is made, so
        // its number is free again; the used first one stays.
        let third = d.make_new_in(desktop.path(), archive).unwrap();
        assert_eq!(third, desktop.path().join("photos (2)"));
        assert_eq!(std::fs::read(first.join("mine.txt")).unwrap(), b"mine");
        // A file with the name counts as taken too.
        std::fs::write(desktop.path().join("photos (3)"), b"file").unwrap();
        assert_eq!(
            create_unique_dir(desktop.path(), "photos").unwrap(),
            desktop.path().join("photos (4)")
        );
    }

    #[test]
    fn a_missing_desktop_is_an_error_not_a_created_path() {
        let desktop = tempfile::tempdir().unwrap();
        let gone = desktop.path().join("no-such-desktop");
        let e = create_unique_dir(&gone, "x").unwrap_err();
        assert_eq!(e.code, ErrorCode::IoError);
        assert!(!gone.exists());
    }

    #[test]
    fn the_desktop_plan_extracts_into_a_new_folder_named_after_the_archive() {
        let f = fx();
        let desktop = tempfile::tempdir().unwrap();
        let plan = plan_in_new_folder(
            &f.archives,
            &f.destination,
            desktop.path(),
            f.handle,
            1,
            &[],
        )
        .unwrap();
        let made = desktop.path().join("fixture");
        assert!(made.is_dir());
        assert_eq!(plan.destination, made.display().to_string());
        assert_eq!(f.destination.get(), Some(made));
    }

    // ---- "Extract to...": the recent folders ----

    #[test]
    fn a_successful_extract_into_a_new_folder_records_the_folder_it_was_made_in() {
        let f = fx();
        let parent = tempfile::tempdir().unwrap();
        plan_in_new_folder(&f.archives, &f.destination, parent.path(), f.handle, 1, &[]).unwrap();
        // Planning remembers nothing; an extraction does.
        assert!(f.destination.history.existing().is_empty());
        go(&f, &request(&f, 1, &[], whole())).unwrap();
        assert_eq!(
            f.destination.history.existing(),
            [parent.path().to_str().unwrap()]
        );
    }

    #[test]
    fn extracting_into_the_folder_itself_records_that_folder() {
        let f = fx();
        plan(
            &f.archives,
            &f.destination,
            Some(f.dest.clone()),
            f.handle,
            1,
            &[],
        )
        .unwrap();
        go(&f, &request(&f, 1, &[], whole())).unwrap();
        assert_eq!(f.destination.history.existing(), [f.dest.to_str().unwrap()]);
    }

    #[test]
    fn a_failed_or_refused_extract_records_nothing() {
        let f = fx();
        plan(
            &f.archives,
            &f.destination,
            Some(f.dest.clone()),
            f.handle,
            1,
            &[],
        )
        .unwrap();
        // An unknown snapshot fails when the job opens it.
        assert!(go(&f, &request(&f, 99, &[], whole())).is_err());
        assert!(f.destination.history.existing().is_empty());
    }

    #[test]
    fn extracting_twice_into_one_folder_reports_the_collisions_and_overwrites_nothing() {
        let f = fx();
        plan(
            &f.archives,
            &f.destination,
            Some(f.dest.clone()),
            f.handle,
            1,
            &[],
        )
        .unwrap();
        go(&f, &request(&f, 1, &[], ConflictChoice::SkipConflicts)).unwrap();
        let first = tree(&f.dest);
        std::fs::write(f.dest.join("top.txt"), b"edited since").unwrap();
        let again = plan(&f.archives, &f.destination, None, f.handle, 1, &[]).unwrap();
        assert!(again.existing_total > 0);
        assert!(again.existing.iter().any(|n| n.text == "top.txt"));
        let r = report(go(&f, &request(&f, 1, &[], ConflictChoice::SkipConflicts)).unwrap());
        assert!(r.not_extracted_total > 0);
        assert_eq!(
            std::fs::read(f.dest.join("top.txt")).unwrap(),
            b"edited since"
        );
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            tree(&f.dest).keys().collect::<Vec<_>>()
        );
        // Stopping instead of skipping refuses before writing anything.
        assert!(go(&f, &request(&f, 1, &[], ConflictChoice::Stop)).is_err());
    }
}
