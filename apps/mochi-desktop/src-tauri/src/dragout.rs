//! Dragging entries out of an open archive into the operating system's file
//! manager (WinRAR-style): the user selects entries in the list and drags
//! them onto the desktop or a folder window.
//!
//! # How it works
//!
//! 1. The webview calls [`drag_out`] (a MOCHI archive) or [`drag_out_foreign`]
//!    (ZIP, 7z, RAR, tar) with the archive handle and the entry keys of the
//!    selection. It never names a path and never sees one.
//! 2. Rust checks the selection and its declared size (at most [`MAX_BYTES`]),
//!    makes a fresh, randomly named, owner-only folder under the app's cache
//!    directory (`<cache>/drag-out/<random>/`), and extracts into its `stage`
//!    folder through the **same code the Extract job runs**
//!    ([`extract::run`], [`Foreign::run`]): every byte is written by the C6
//!    restore engine, which never follows a link, never overwrites, and
//!    publishes a file only after its content verified (or, for a foreign
//!    format, after the format's own check). There is no second extractor.
//! 3. The engine keeps full archive paths, so each selected entry is then
//!    moved (a rename inside the same folder, not a copy) to the top level of
//!    the sibling `items` folder under its own name: `a/b/c.txt` becomes
//!    `items/c.txt`, a selected folder `a/b` becomes `items/b/...`. Two
//!    entries with one name get `name (2).ext`.
//! 4. The operating system's drag starts from the main window with the
//!    top-level items, through the `drag` crate (Apache-2.0 OR MIT, by
//!    CrabNebula; Windows: OLE `DoDragDrop`, Linux: a GTK drag source). The
//!    plugin `tauri-plugin-drag` is **not** used and no plugin permission is
//!    granted: the webview can only call the two commands below.
//!
//! # Clean-up
//!
//! A folder is deleted when the drag ends without a drop (cancelled or
//! refused), when anything fails before the drag starts, at start-up and at
//! exit when it is more than an hour old ([`sweep`]). A folder that was
//! dropped on stays until then: the file manager may still be copying from it
//! when the drop returns. The sweep touches only directories directly inside
//! `<cache>/drag-out/` whose name has the shape this module gives them, and
//! never follows a symbolic link, so it cannot be steered outside that folder.
//!
//! # Limits, said plainly
//!
//! * The size check uses the sizes the archive *declares*. A foreign archive
//!   can lie; the helper's own limits (zip-bomb ratios, totals) apply as for
//!   any extract.
//! * Extraction happens before the drag starts. A large selection takes time,
//!   and the operating system starts a drag only while the mouse button is
//!   still held, so the interface should start the call as the drag gesture
//!   begins and suggest Extract for a big selection.
//! * Any entry that cannot be extracted (a name the file system cannot hold,
//!   a failed check) refuses the whole drag with its reason: nothing partial
//!   is offered to the file manager. (A foreign archive's links and special
//!   files are skipped by design in every extract and do not refuse.)
//! * Linux (`drag` 2.1): the file URIs are not percent-encoded by the crate,
//!   so names containing `%`, `#` or `?` may be misread by a file manager.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::{ErrorCode, MochiError, Result};
use tauri::{AppHandle, Manager, State, WebviewWindow};

use crate::browse::{parse_key, Archives};
use crate::extract::{self, Destination, MAX_KEYS};
use crate::foreign::Foreign;
use crate::ipc::{ArchiveHandle, ConflictChoice, ExtractReport, IpcError, JobRequest, PathKey};

/// The folder under the app's cache directory that holds drag-out folders.
pub const FOLDER: &str = "drag-out";

/// Most declared bytes one drag-out may extract (8 GiB). Above it the call is
/// refused with `LIMIT_EXCEEDED` and the interface suggests Extract.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// A drag-out folder older than this is deleted by the sweep.
pub const MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// How long a command waits to learn whether the drag started. A drag that is
/// still running after this (Windows runs the whole drag inside the call) has
/// started; an immediate failure arrives well within it.
const START_WAIT: Duration = Duration::from_secs(2);

/// Names tried for one duplicated name (`name (2)` ... `name (999)`).
const MAX_DUPLICATES: u32 = 999;

/// The drag image: the app icon, small. Generic; it reveals nothing.
const DRAG_IMAGE: &[u8] = include_bytes!("../icons/32x32.png");

/// The extraction folder inside one drag-out folder (full archive paths).
const STAGE: &str = "stage";

/// The folder inside one drag-out folder whose entries are dragged.
const ITEMS: &str = "items";

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn io_error(what: &str, e: &std::io::Error) -> MochiError {
    MochiError::new(ErrorCode::IoError, format!("{what}: {e}"))
}

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

fn private_dir_builder() -> std::fs::DirBuilder {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut b = std::fs::DirBuilder::new();
        b.mode(0o700);
        b
    }
    #[cfg(not(unix))]
    std::fs::DirBuilder::new()
}

/// `<cache>/drag-out`, made (owner-only on Unix) if it is not there. It must
/// be a real directory, not a link.
pub fn ensure_base(cache: &Path) -> Result<PathBuf> {
    ensure_named_base(cache, FOLDER)
}

/// [`ensure_base`] for another folder of temporary folders under the cache
/// (the virus scan's, `scan`).
pub fn ensure_named_base(cache: &Path, folder: &str) -> Result<PathBuf> {
    let base = cache.join(folder);
    std::fs::create_dir_all(cache)
        .map_err(|e| io_error("could not prepare the cache folder", &e))?;
    match private_dir_builder().create(&base) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_error("could not make the drag-out folder", &e)),
    }
    let meta = std::fs::symlink_metadata(&base)
        .map_err(|e| io_error("could not check the drag-out folder", &e))?;
    if !meta.is_dir() {
        return Err(MochiError::new(
            ErrorCode::IoError,
            "the drag-out folder is not a plain folder",
        ));
    }
    Ok(base)
}

/// 32 lowercase hex digits from the operating system's randomness.
fn random_name() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("the operating system gave no randomness: {e}"),
        )
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `name` has the shape of a drag-out folder this module made.
fn is_folder_name(name: &str) -> bool {
    name.len() == 32
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One drag-out folder. Dropping it deletes the folder, unless [`keep`] was
/// called: every early return on the way to the drag cleans up after itself.
///
/// [`keep`]: DragFolder::keep
#[derive(Debug)]
pub struct DragFolder {
    root: PathBuf,
    keep: bool,
}

impl DragFolder {
    /// A new folder with a random name directly inside `base`, with its
    /// `stage` and `items` folders. `create_dir` semantics: a name that
    /// exists is never reused, and nothing is created through a link.
    pub fn create(base: &Path) -> Result<Self> {
        for _ in 0..8 {
            let root = base.join(random_name()?);
            match private_dir_builder().create(&root) {
                Ok(()) => {
                    let me = DragFolder { root, keep: false };
                    for sub in [STAGE, ITEMS] {
                        private_dir_builder()
                            .create(me.root.join(sub))
                            .map_err(|e| io_error("could not make a drag-out folder", &e))?;
                    }
                    return Ok(me);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_error("could not make a drag-out folder", &e)),
            }
        }
        Err(MochiError::new(
            ErrorCode::IoError,
            "could not find an unused drag-out folder name",
        ))
    }

    pub fn stage(&self) -> PathBuf {
        self.root.join(STAGE)
    }

    fn items(&self) -> PathBuf {
        self.root.join(ITEMS)
    }

    /// Stop the drop-time deletion; the folder is now owned by the drag and
    /// the sweep.
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl DragFolder {
    /// The folder itself.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for DragFolder {
    fn drop(&mut self) {
        if !self.keep {
            let _ = remove_tree(&self.root);
        }
    }
}

/// What a prepared drag holds: the folder and the paths to drag.
#[derive(Debug)]
pub struct Prepared {
    folder: DragFolder,
    items: Vec<PathBuf>,
}

impl Prepared {
    /// The top-level items to drag, in archive path order.
    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    /// The drag-out folder that holds them.
    pub fn root(&self) -> &Path {
        &self.folder.root
    }
}

/// Make a tree removable: Unix directories that lack owner write/search
/// permission (an archive may record `0555`) and Windows read-only entries.
/// Never follows a link.
fn relax(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    #[cfg(unix)]
    if meta.is_dir() {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700));
        }
    }
    #[cfg(windows)]
    if meta.permissions().readonly() {
        let mut p = meta.permissions();
        // Windows only: clears the read-only attribute (the Unix meaning of
        // `set_readonly(false)`, a world-writable file, does not apply).
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        let _ = std::fs::set_permissions(path, p);
    }
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                relax(&entry.path());
            }
        }
    }
}

/// Delete `path` and everything under it. A symbolic link is removed as a
/// link and never followed; read-only entries are made removable first.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return std::fs::remove_file(path).or_else(|_| std::fs::remove_dir(path));
    }
    relax(path);
    std::fs::remove_dir_all(path)
}

/// Delete the drag-out folders in `base` that are older than `max_age` at
/// `now`; returns how many. Only directories directly inside `base` whose
/// name is 32 lowercase hex digits are candidates. A symbolic link, a file,
/// or any other name is left exactly as it is, and `base` itself must be a
/// real directory.
pub fn sweep(base: &Path, max_age: Duration, now: SystemTime) -> usize {
    match std::fs::symlink_metadata(base) {
        Ok(m) if m.is_dir() => {}
        _ => return 0,
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_folder_name) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // `symlink_metadata`: a link is not a directory here.
        if !meta.is_dir() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age >= max_age);
        if old && remove_tree(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The sweep for the app's real cache folder.
fn sweep_cache(cache: &Path) -> usize {
    sweep(&cache.join(FOLDER), MAX_AGE, SystemTime::now())
}

/// Sweep at start-up, off the setup path.
pub fn sweep_in_background(app: &AppHandle) {
    if let Ok(cache) = app.path().app_cache_dir() {
        std::thread::spawn(move || {
            sweep_cache(&cache);
        });
    }
}

/// Sweep at exit (the process is going away; do it here).
pub fn sweep_on_exit(app: &AppHandle) {
    if let Ok(cache) = app.path().app_cache_dir() {
        sweep_cache(&cache);
    }
}

// ---------------------------------------------------------------------------
// The selection
// ---------------------------------------------------------------------------

/// The selection as archive paths: 1 to [`MAX_KEYS`] keys, each parsed, with
/// duplicates and entries under another selected folder dropped (the folder
/// brings them), in path order. These are the items that will be dragged.
fn top_level(keys: &[PathKey]) -> Result<Vec<ArchivePath>> {
    if keys.is_empty() {
        return Err(invalid("select at least one entry to drag"));
    }
    if keys.len() > MAX_KEYS {
        return Err(invalid(format!(
            "a drag names at most {MAX_KEYS} entries; use Extract instead"
        )));
    }
    let all = keys
        .iter()
        .map(|k| parse_key(k))
        .collect::<Result<HashSet<ArchivePath>>>()?;
    let mut tops: Vec<ArchivePath> = all
        .iter()
        .filter(|p| {
            let mut up = p.parent();
            while let Some(a) = up {
                if all.contains(&a) {
                    return false;
                }
                up = a.parent();
            }
            true
        })
        .cloned()
        .collect();
    tops.sort();
    Ok(tops)
}

/// Whether `path` is one of `set` or lies under one.
fn under_any(path: &ArchivePath, set: &HashSet<ArchivePath>) -> bool {
    let mut at = Some(path.clone());
    while let Some(p) = at {
        if set.contains(&p) {
            return true;
        }
        at = p.parent();
    }
    false
}

/// Refuse a selection whose declared size is above `max`.
fn check_size(bytes: u64, max: u64) -> Result<()> {
    if bytes > max {
        const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            format!(
                "the selection is about {:.1} GiB; dragging out is limited to {:.1} GiB. \
                 Use Extract instead",
                bytes as f64 / GIB,
                max as f64 / GIB
            ),
        ));
    }
    Ok(())
}

/// The bytes the selected files of snapshot `seq` declare, from the verified
/// catalog; `INVALID_ARGUMENT` for a key the snapshot lacks.
fn mochi_bytes(
    archives: &Archives,
    handle: ArchiveHandle,
    seq: u32,
    tops: &[ArchivePath],
) -> Result<u64> {
    let (_src, head, _opts) = archives.open_for_read(handle, u64::from(seq))?;
    let snapshot = head.catalog.replay(None)?;
    if let Some(missing) = tops.iter().find(|t| snapshot.get(t).is_none()) {
        return Err(invalid(format!(
            "no entry {:?} in this snapshot",
            String::from_utf8_lossy(missing.as_stored())
        )));
    }
    let versions = head.catalog.file_versions()?;
    let wanted: HashSet<ArchivePath> = tops.iter().cloned().collect();
    let mut total = 0u64;
    for (path, entry) in snapshot.iter() {
        if entry.kind != EntryKind::File || !under_any(path, &wanted) {
            continue;
        }
        let version = versions.get(&entry.version).ok_or_else(|| {
            MochiError::new(
                ErrorCode::CatalogInvalid,
                "the snapshot names a missing file version",
            )
        })?;
        total = total.saturating_add(version.logical_len);
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Extraction
// ---------------------------------------------------------------------------

/// The archive path's component as an operating system name.
fn os_name(component: &[u8]) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(OsStr::from_bytes(component).to_owned())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        mochi_core::catalog::path::utf16_from_wtf8(component)
            .map(|w| OsString::from_wide(&w))
            .ok_or_else(|| invalid("a name in the selection is not valid on this system"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        String::from_utf8(component.to_vec())
            .map(OsString::from)
            .map_err(|_| invalid("a name in the selection is not valid on this system"))
    }
}

/// Where the engine put `path` below `stage`.
fn stage_path(stage: &Path, path: &ArchivePath) -> Result<PathBuf> {
    let mut out = stage.to_path_buf();
    for component in path.components() {
        out.push(os_name(component)?);
    }
    Ok(out)
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`, ... if that is taken. A folder's
/// whole name is the stem.
fn unique_in(dir: &Path, name: &OsStr, is_dir: bool) -> Result<PathBuf> {
    let first = dir.join(name);
    if !exists(&first) {
        return Ok(first);
    }
    let as_path = Path::new(name);
    let (stem, ext) = if is_dir {
        (name.to_owned(), None)
    } else {
        (
            as_path.file_stem().unwrap_or(name).to_owned(),
            as_path.extension().map(OsStr::to_owned),
        )
    };
    for n in 2..=MAX_DUPLICATES {
        let mut candidate = stem.clone();
        candidate.push(format!(" ({n})"));
        if let Some(ext) = &ext {
            candidate.push(".");
            candidate.push(ext);
        }
        let path = dir.join(candidate);
        if !exists(&path) {
            return Ok(path);
        }
    }
    Err(MochiError::new(
        ErrorCode::LimitExceeded,
        "too many selected entries share one name",
    ))
}

/// Move each selected entry from its place below `stage` to the top level of
/// `items`, under its own name. Returns the new paths in selection order.
fn gather(folder: &DragFolder, tops: &[ArchivePath]) -> Result<Vec<PathBuf>> {
    let stage = folder.stage();
    let items = folder.items();
    let mut out = Vec::with_capacity(tops.len());
    for top in tops {
        let from = stage_path(&stage, top)?;
        let meta = std::fs::symlink_metadata(&from).map_err(|_| {
            invalid(format!(
                "{:?} was not extracted, so it cannot be dragged; use Extract to see why",
                String::from_utf8_lossy(top.as_stored())
            ))
        })?;
        if meta.file_type().is_symlink() {
            return Err(invalid("a link cannot be dragged"));
        }
        // An archive may record a folder as read-only; moving an entry out of
        // such a folder needs write access to it. These are folders this
        // call just made.
        let mut up = top.parent();
        while let Some(dir) = up {
            relax_one(&stage_path(&stage, &dir)?);
            up = dir.parent();
        }
        let name = os_name(top.components().last().unwrap_or_default())?;
        let to = unique_in(&items, &name, meta.is_dir())?;
        std::fs::rename(&from, &to)
            .map_err(|e| io_error("could not place a selected entry for dragging", &e))?;
        out.push(to);
    }
    Ok(out)
}

/// Give the owner access to one directory (no recursion).
fn relax_one(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::symlink_metadata(dir) {
            if meta.is_dir() {
                let mode = meta.permissions().mode();
                if mode & 0o700 != 0o700 {
                    let _ = std::fs::set_permissions(
                        dir,
                        std::fs::Permissions::from_mode(mode | 0o700),
                    );
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// `ErrorCode` for a code string of a report.
fn code_of(text: &str) -> ErrorCode {
    ErrorCode::ALL
        .iter()
        .copied()
        .find(|c| c.as_str() == text)
        .unwrap_or(ErrorCode::IoError)
}

/// Refuse a drag whose extraction left entries out. `skipped_ok` accepts the
/// kinds a foreign extract skips by design (links, special files:
/// `UNSUPPORTED_FEATURE`); anything else, or a report too long to see all of,
/// refuses, with the first reason.
fn require_complete(report: Option<&ExtractReport>, skipped_ok: bool) -> Result<()> {
    let report = report
        .ok_or_else(|| MochiError::new(ErrorCode::IoError, "the extraction gave no report"))?;
    let skipped = ErrorCode::UnsupportedFeature.as_str();
    let mut refusing = report
        .not_extracted
        .iter()
        .filter(|e| !(skipped_ok && e.code == skipped));
    let first = refusing.next();
    let unseen = report.not_extracted_total > report.not_extracted.len() as u64;
    if first.is_none() && !(unseen && skipped_ok) {
        return Ok(());
    }
    let (code, why) = first.map_or(
        (ErrorCode::IoError, "more than can be listed".to_owned()),
        |e| {
            (
                code_of(&e.code),
                format!("{}: {}", e.path.text, e.reason.text),
            )
        },
    );
    Err(MochiError::new(
        code,
        format!(
            "{} selected entr{} could not be extracted ({why}), so nothing was dragged. Use \
             Extract to choose a folder and see the full report",
            report.not_extracted_total,
            if report.not_extracted_total == 1 {
                "y"
            } else {
                "ies"
            }
        ),
    ))
}

/// Make the folder, run `extract` into its `stage`, and move the selection to
/// the top level. The folder is deleted again on any failure.
fn stage_and_gather(
    base: &Path,
    tops: &[ArchivePath],
    extract: impl FnOnce(&Destination) -> Result<()>,
) -> Result<Prepared> {
    let folder = DragFolder::create(base)?;
    let destination = Destination::default();
    destination.set(folder.stage());
    extract(&destination)?;
    let items = gather(&folder, tops)?;
    Ok(Prepared { folder, items })
}

/// Extract the selection of snapshot `seq` of an open MOCHI archive (unlocked
/// in its session, if Encrypted) into a fresh folder below `base` (from
/// [`ensure_base`]) and put the selected entries at its top level.
///
/// Errors, all before anything is left behind: `INVALID_ARGUMENT` (no keys,
/// too many, a key that does not parse or is not in the snapshot, an archive
/// that is not open), `LIMIT_EXCEEDED` (declared size above `max_bytes`), the
/// restore engine's own codes, and `IO_ERROR`.
pub fn prepare_mochi(
    archives: &Archives,
    base: &Path,
    handle: ArchiveHandle,
    seq: u32,
    keys: &[PathKey],
    max_bytes: u64,
) -> Result<Prepared> {
    let tops = top_level(keys)?;
    check_size(mochi_bytes(archives, handle, seq, &tops)?, max_bytes)?;
    let request = JobRequest::ExtractArchive {
        handle,
        seq,
        keys: keys.to_vec(),
        // Refuse before writing if a name cannot be held, rather than
        // offering part of the selection.
        on_conflict: ConflictChoice::Stop,
    };
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    stage_and_gather(base, &tops, |destination| {
        let done = extract::run(archives, destination, &request, &ctx)?;
        require_complete(done.extract.as_deref(), false)
    })
}

/// [`prepare_mochi`] for an open foreign archive (ZIP, 7z, RAR, tar): the
/// helper streams the entries into the same restore engine as Extract.
pub fn prepare_foreign(
    foreign: &Foreign,
    base: &Path,
    handle: ArchiveHandle,
    keys: &[PathKey],
    max_bytes: u64,
) -> Result<Prepared> {
    let tops = top_level(keys)?;
    check_size(foreign.declared_bytes(handle, keys)?, max_bytes)?;
    let request = JobRequest::ExtractForeign {
        handle,
        keys: keys.to_vec(),
        on_conflict: ConflictChoice::Stop,
    };
    let cancel = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    stage_and_gather(base, &tops, |destination| {
        let done = foreign.run(destination, &request, &ctx)?;
        require_complete(done.extract.as_deref(), true)
    })
}

// ---------------------------------------------------------------------------
// The operating system's drag
// ---------------------------------------------------------------------------

/// One drag at a time (extraction included). Released when dropped: on
/// Windows that is when the drag ends (the drag runs inside the call), on
/// Linux when the drag has started.
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Busy;

impl Busy {
    fn take() -> Result<Self> {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(MochiError::new(
                ErrorCode::LimitExceeded,
                "another drag is still in progress",
            ));
        }
        Ok(Busy)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Start the native drag from `window`. Linux needs the window's GTK
/// application window; Windows takes the window handle.
#[cfg(target_os = "linux")]
fn start_native(
    window: &WebviewWindow,
    item: drag::DragItem,
    image: drag::Image,
    on_end: impl Fn(drag::DragResult, drag::CursorPosition) + Send + 'static,
) -> drag::Result<()> {
    let gtk = window
        .gtk_window()
        .map_err(|_| drag::Error::UnsupportedWindowHandle)?;
    drag::start_drag(&gtk, item, image, on_end, drag::Options::default())
}

#[cfg(not(target_os = "linux"))]
fn start_native(
    window: &WebviewWindow,
    item: drag::DragItem,
    image: drag::Image,
    on_end: impl Fn(drag::DragResult, drag::CursorPosition) + Send + 'static,
) -> drag::Result<()> {
    drag::start_drag(window, item, image, on_end, drag::Options::default())
}

/// On the main thread: start the drag with the prepared items. A drag that
/// ends without a drop deletes its folder; one that fails to start does too.
fn drag_on_main(window: &WebviewWindow, mut prepared: Prepared) -> Result<()> {
    let root = prepared.root().to_path_buf();
    let item = drag::DragItem::Files(prepared.items().to_vec());
    let image = drag::Image::Raw(DRAG_IMAGE.to_vec());
    let on_end = move |result: drag::DragResult, _: drag::CursorPosition| {
        // A drop leaves the folder for the sweep: the file manager may still
        // be copying from it.
        if matches!(result, drag::DragResult::Cancel) {
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = remove_tree(&root);
            });
        }
    };
    match start_native(window, item, image, on_end) {
        Ok(()) => {
            prepared.folder.keep();
            Ok(())
        }
        // `prepared` drops here and deletes its folder.
        Err(e) => Err(MochiError::new(
            match e {
                drag::Error::UnsupportedWindowHandle => ErrorCode::UnsupportedFeature,
                _ => ErrorCode::IoError,
            },
            format!("the system could not start the drag: {e}"),
        )),
    }
}

/// Hand `prepared` to the main thread and wait until the drag has started or
/// failed. On Windows the system runs the whole drag inside the call, so a
/// call still running after [`START_WAIT`] means it started.
fn start(window: &WebviewWindow, prepared: Prepared, busy: Busy) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Result<()>>();
    let on_main = window.clone();
    window
        .run_on_main_thread(move || {
            let _busy = busy;
            let _ = tx.send(drag_on_main(&on_main, prepared));
        })
        .map_err(|_| {
            MochiError::new(
                ErrorCode::IoError,
                "the window is closing; nothing was dragged",
            )
        })?;
    match rx.recv_timeout(START_WAIT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Ok(()),
        Err(RecvTimeoutError::Disconnected) => Err(MochiError::new(
            ErrorCode::IoError,
            "the window closed before the drag started",
        )),
    }
}

/// Run `work` on the blocking pool and wait for it.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| IpcError {
            code: ErrorCode::Cancelled.as_str().to_owned(),
            message: "the operation stopped before it finished".to_owned(),
        })?
        .map_err(IpcError::from)
}

fn cache_dir(window: &WebviewWindow) -> std::result::Result<PathBuf, IpcError> {
    window.app_handle().path().app_cache_dir().map_err(|e| {
        IpcError::from(MochiError::new(
            ErrorCode::IoError,
            format!("the app's cache folder is not known: {e}"),
        ))
    })
}

/// Drag entries of snapshot `seq` of an open MOCHI archive out to the
/// operating system's file manager: extract them to a fresh temporary folder
/// that Rust makes (the webview names no path), then start the native drag
/// of the top-level items. Returns once the drag has started or failed.
///
/// Errors: `INVALID_ARGUMENT` (no keys, too many, a bad key, an archive that
/// is not open), `LIMIT_EXCEEDED` (more than 8 GiB declared, or a drag
/// already in progress), the restore engine's codes for an entry that cannot
/// be extracted, `UNSUPPORTED_FEATURE` / `IO_ERROR` if the system cannot start
/// the drag.
#[tauri::command]
pub async fn drag_out(
    window: WebviewWindow,
    handle: ArchiveHandle,
    seq: u32,
    keys: Vec<PathKey>,
    archives: State<'_, Arc<Archives>>,
) -> std::result::Result<(), IpcError> {
    let archives = Arc::clone(&archives);
    let cache = cache_dir(&window)?;
    blocking(move || {
        let busy = Busy::take()?;
        let base = ensure_base(&cache)?;
        let prepared = prepare_mochi(&archives, &base, handle, seq, &keys, MAX_BYTES)?;
        start(&window, prepared, busy)
    })
    .await
}

/// [`drag_out`] for an open foreign archive (ZIP, 7z, RAR, tar).
#[tauri::command]
pub async fn drag_out_foreign(
    window: WebviewWindow,
    handle: ArchiveHandle,
    keys: Vec<PathKey>,
    foreign: State<'_, Arc<Foreign>>,
) -> std::result::Result<(), IpcError> {
    let foreign = Arc::clone(&foreign);
    let cache = cache_dir(&window)?;
    blocking(move || {
        let busy = Busy::take()?;
        let base = ensure_base(&cache)?;
        let prepared = prepare_foreign(&foreign, &base, handle, &keys, MAX_BYTES)?;
        start(&window, prepared, busy)
    })
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::collections::BTreeMap;

    use mochi_core::manifest::Attributes;
    use mochi_core::publish::{ArchiveWriter, ReadOptions, Transaction};
    use mochi_core::storage::os::OsStorage;
    use mochi_testkit::archive::{encrypted_options, path as apath, test_options, Job};
    use mochi_testkit::SeqIds;

    use super::*;
    use crate::ipc::{OpenOutcome, Secret};

    const PASS: &str = "correct horse";

    fn hex(s: &str) -> String {
        s.bytes().map(|b| format!("{b:02x}")).collect()
    }

    fn keys(names: &[&str]) -> Vec<PathKey> {
        names.iter().map(|n| hex(n)).collect()
    }

    /// One commit: `top.txt`, `docs/{a.txt, deep/b.txt}`, `other/a.txt`.
    fn build(dir: &Path, name: &str, options: mochi_core::publish::WriterOptions) -> PathBuf {
        let path = dir.join(name);
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(4)),
            options,
        )
        .unwrap();
        let job = Job::new();
        let mut tx = Transaction::new();
        let a = Attributes::default();
        tx.put_dir(apath("docs"), a);
        tx.put_file(apath("docs/a.txt"), b"alpha".to_vec(), a);
        tx.put_dir(apath("docs/deep"), a);
        tx.put_file(apath("docs/deep/b.txt"), b"bravo".to_vec(), a);
        tx.put_dir(apath("other"), a);
        tx.put_file(apath("other/a.txt"), b"other alpha".to_vec(), a);
        tx.put_file(apath("top.txt"), b"top".to_vec(), a);
        w.commit(tx, &job.ctx()).unwrap();
        w.close().unwrap();
        path
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        archives: Archives,
        handle: ArchiveHandle,
    }

    fn fx() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let path = build(tmp.path(), "fixture.mochi", test_options());
        let base = ensure_base(&tmp.path().join("cache")).unwrap();
        let archives = Archives::default();
        let handle = archives.open(&path, ReadOptions::default()).unwrap().handle;
        Fx {
            _tmp: tmp,
            base,
            archives,
            handle,
        }
    }

    impl Fx {
        fn prepare(&self, names: &[&str]) -> Result<Prepared> {
            prepare_mochi(
                &self.archives,
                &self.base,
                self.handle,
                0,
                &keys(names),
                MAX_BYTES,
            )
        }

        fn folders_left(&self) -> usize {
            std::fs::read_dir(&self.base).unwrap().count()
        }
    }

    /// Every file below `root`, as relative path to bytes (`None`: folder).
    fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
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
        let mut out = BTreeMap::new();
        walk(root, root, &mut out);
        out
    }

    fn file(s: &str) -> Option<Vec<u8>> {
        Some(s.as_bytes().to_vec())
    }

    fn names_of(p: &Prepared) -> Vec<String> {
        p.items()
            .iter()
            .map(|i| i.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    // ---- extraction into the temporary folder: MOCHI ----

    #[test]
    fn a_single_file_lands_at_the_top_level_of_a_fresh_private_folder() {
        let f = fx();
        let p = f.prepare(&["top.txt"]).unwrap();
        assert_eq!(p.items().len(), 1);
        assert_eq!(std::fs::read(&p.items()[0]).unwrap(), b"top");
        assert_eq!(p.items()[0].parent().unwrap(), p.root().join(ITEMS));
        // Directly inside the base, under a random 32-hex name.
        assert_eq!(p.root().parent().unwrap(), f.base);
        assert!(is_folder_name(
            p.root().file_name().unwrap().to_str().unwrap()
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&f.base, p.root()] {
                let mode = std::fs::metadata(dir).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700, "{dir:?} is owner-only");
            }
        }
        // A second drag gets its own folder.
        let q = f.prepare(&["top.txt"]).unwrap();
        assert_ne!(p.root(), q.root());
        assert_eq!(f.folders_left(), 2);
    }

    #[test]
    fn a_nested_file_keeps_only_its_own_name_at_the_top() {
        let f = fx();
        let p = f.prepare(&["docs/deep/b.txt"]).unwrap();
        assert_eq!(names_of(&p), ["b.txt"]);
        assert_eq!(std::fs::read(&p.items()[0]).unwrap(), b"bravo");
        // What is offered to the file manager is exactly the one entry.
        let items = tree(&p.root().join(ITEMS));
        assert_eq!(items.len(), 1);
        assert_eq!(items["b.txt"], file("bravo"));
    }

    #[test]
    fn a_selected_folder_brings_its_subtree_under_its_own_name() {
        let f = fx();
        let p = f.prepare(&["docs/deep"]).unwrap();
        assert_eq!(names_of(&p), ["deep"]);
        let mut want = BTreeMap::new();
        want.insert("deep".to_owned(), None);
        want.insert("deep/b.txt".to_owned(), file("bravo"));
        assert_eq!(tree(&p.root().join(ITEMS)), want);
    }

    #[test]
    fn an_entry_under_a_selected_folder_and_repeats_are_not_dragged_twice() {
        let f = fx();
        let p = f
            .prepare(&["docs", "docs/a.txt", "docs/deep/b.txt", "docs", "top.txt"])
            .unwrap();
        assert_eq!(names_of(&p), ["docs", "top.txt"]);
        let items = tree(&p.root().join(ITEMS));
        assert_eq!(items["docs/a.txt"], file("alpha"));
        assert_eq!(items["docs/deep/b.txt"], file("bravo"));
        assert_eq!(items["top.txt"], file("top"));
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn two_entries_with_one_name_are_numbered_not_overwritten() {
        let f = fx();
        let p = f.prepare(&["docs/a.txt", "other/a.txt"]).unwrap();
        assert_eq!(names_of(&p), ["a.txt", "a (2).txt"]);
        assert_eq!(std::fs::read(&p.items()[0]).unwrap(), b"alpha");
        assert_eq!(std::fs::read(&p.items()[1]).unwrap(), b"other alpha");
    }

    #[test]
    fn the_archive_file_is_not_changed() {
        let f = fx();
        let archive = f._tmp.path().join("fixture.mochi");
        let before = std::fs::read(&archive).unwrap();
        f.prepare(&["docs"]).unwrap();
        assert_eq!(std::fs::read(&archive).unwrap(), before);
    }

    #[test]
    fn an_encrypted_archive_unlocked_in_its_session_drags_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = build(tmp.path(), "enc.mochi", encrypted_options(&[PASS]));
        let base = ensure_base(&tmp.path().join("cache")).unwrap();
        let archives = Archives::default();
        let pending = match archives.open_or_pend(&path).unwrap() {
            OpenOutcome::NeedsPassphrase { pending, .. } => pending,
            other => panic!("{other:?}"),
        };
        let handle = archives.unlock(pending, &Secret::new(PASS)).unwrap().handle;
        let p = prepare_mochi(
            &archives,
            &base,
            handle,
            0,
            &keys(&["docs/deep/b.txt"]),
            MAX_BYTES,
        )
        .unwrap();
        assert_eq!(names_of(&p), ["b.txt"]);
        assert_eq!(std::fs::read(&p.items()[0]).unwrap(), b"bravo");
        // The plaintext is in the temporary folder only; the archive file is
        // still sealed.
        let sealed = std::fs::read(&path).unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"bravo"));
    }

    // ---- refusals ----

    #[test]
    fn bad_input_is_refused_and_leaves_no_folder() {
        let f = fx();
        let too_many: Vec<PathKey> = (0..=MAX_KEYS).map(|i| hex(&format!("n{i}"))).collect();
        let cases: Vec<(&str, Vec<PathKey>)> = vec![
            ("no keys", vec![]),
            ("too many keys", too_many),
            ("not hex", vec!["zz".to_owned()]),
            ("odd length", vec!["abc".to_owned()]),
            ("empty key", vec![String::new()]),
            ("a path that escapes", vec![hex("../etc")]),
            ("not in the archive", keys(&["nope.txt"])),
            ("one good, one missing", keys(&["top.txt", "docs/zzz"])),
        ];
        for (what, ks) in cases {
            let e = prepare_mochi(&f.archives, &f.base, f.handle, 0, &ks, MAX_BYTES).unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{what}: {e}");
        }
        // An archive that is not open, and a snapshot it lacks.
        let e = prepare_mochi(
            &f.archives,
            &f.base,
            9999,
            0,
            &keys(&["top.txt"]),
            MAX_BYTES,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let e = prepare_mochi(
            &f.archives,
            &f.base,
            f.handle,
            77,
            &keys(&["top.txt"]),
            MAX_BYTES,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(f.folders_left(), 0, "a refusal leaves nothing behind");
    }

    #[test]
    fn a_selection_above_the_size_limit_is_refused_before_anything_is_written() {
        let f = fx();
        // docs = "alpha" + "bravo" = 10 bytes.
        let e = prepare_mochi(&f.archives, &f.base, f.handle, 0, &keys(&["docs"]), 9).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
        assert!(e.message.contains("Extract"), "{}", e.message);
        assert_eq!(f.folders_left(), 0);
        // Exactly at the limit is allowed.
        prepare_mochi(&f.archives, &f.base, f.handle, 0, &keys(&["docs"]), 10).unwrap();
        // The shipped limit.
        assert_eq!(MAX_BYTES, 8 * 1024 * 1024 * 1024);
        assert!(check_size(MAX_BYTES, MAX_BYTES).is_ok());
        assert_eq!(
            check_size(MAX_BYTES + 1, MAX_BYTES).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        assert_eq!(
            check_size(u64::MAX, MAX_BYTES).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn an_extraction_that_left_entries_out_refuses_the_whole_drag() {
        use crate::ipc::{NameText, NotExtractedView};
        let report = |codes: &[&str], total: u64| ExtractReport {
            destination: String::new(),
            seq: 0,
            files: 1,
            directories: 0,
            bytes: 0,
            not_extracted: codes
                .iter()
                .map(|c| NotExtractedView {
                    path: NameText::of(b"x"),
                    code: (*c).to_owned(),
                    reason: NameText::of(b"why"),
                })
                .collect(),
            not_extracted_total: total,
            attribute_notes: vec![],
            attributes_unavailable: None,
            durability_note: None,
            files_not_flushed: false,
            case_insensitive: false,
            opened_folder: false,
            replaced: 0,
            merged_folders: 0,
        };
        assert!(require_complete(Some(&report(&[], 0)), false).is_ok());
        let e =
            require_complete(Some(&report(&["CONTENT_INTEGRITY_FAILED"], 1)), false).unwrap_err();
        assert_eq!(e.code, ErrorCode::ContentIntegrityFailed);
        assert!(e.message.contains("nothing was dragged"));
        // Links skipped by design do not refuse a foreign drag, but do refuse
        // for MOCHI (which has none), and a real failure always refuses.
        let links = report(&["UNSUPPORTED_FEATURE"], 1);
        assert!(require_complete(Some(&links), true).is_ok());
        assert!(require_complete(Some(&links), false).is_err());
        let mixed = report(&["UNSUPPORTED_FEATURE", "NAME_UNSUPPORTED"], 2);
        assert_eq!(
            require_complete(Some(&mixed), true).unwrap_err().code,
            ErrorCode::NameUnsupported
        );
        // More failures than the report lists: cannot be sure, refuse.
        assert!(require_complete(Some(&report(&["UNSUPPORTED_FEATURE"], 5000)), true).is_err());
        assert!(require_complete(None, true).is_err());
    }

    #[test]
    fn a_failure_after_the_folder_exists_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let base = ensure_base(tmp.path()).unwrap();
        let tops = vec![ArchivePath::from_stored(b"x").unwrap()];
        // The extractor "succeeds" but the entry is not there.
        let e = stage_and_gather(&base, &tops, |_| Ok(())).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
        // The extractor fails.
        let e = stage_and_gather(&base, &tops, |_| Err(invalid("boom"))).unwrap_err();
        assert_eq!(e.message, "boom");
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
    }

    #[test]
    fn dropping_a_prepared_drag_deletes_its_folder_but_a_kept_one_stays() {
        let f = fx();
        let mut p = f.prepare(&["top.txt"]).unwrap();
        let root = p.root().to_path_buf();
        p.folder.keep();
        drop(p);
        assert!(
            root.exists(),
            "a kept folder is left to the drag and the sweep"
        );
        let p = f.prepare(&["top.txt"]).unwrap();
        let gone = p.root().to_path_buf();
        drop(p);
        assert!(!gone.exists());
    }

    // ---- clean-up ----

    fn old_folder(base: &Path) -> PathBuf {
        let p = base.join(random_name().unwrap());
        std::fs::create_dir(&p).unwrap();
        std::fs::write(p.join("file.txt"), b"x").unwrap();
        p
    }

    #[test]
    fn the_sweep_deletes_old_folders_only() {
        let tmp = tempfile::tempdir().unwrap();
        let base = ensure_base(tmp.path()).unwrap();
        let old = old_folder(&base);
        let stray_dir = base.join("not-one-of-ours");
        std::fs::create_dir(&stray_dir).unwrap();
        let stray_file = base.join(random_name().unwrap());
        std::fs::write(&stray_file, b"a file with a folder's name").unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"mine").unwrap();

        // Young folders stay.
        assert_eq!(sweep(&base, MAX_AGE, SystemTime::now()), 0);
        assert!(old.exists());
        // An hour and a second on, the folder goes; nothing else does.
        let later = SystemTime::now() + MAX_AGE + Duration::from_secs(1);
        assert_eq!(sweep(&base, MAX_AGE, later), 1);
        assert!(!old.exists());
        assert!(stray_dir.exists() && stray_file.exists() && outside.exists());
        assert!(base.exists(), "the base itself is never removed");
        // Nothing to do on a missing base.
        assert_eq!(sweep(&tmp.path().join("missing"), MAX_AGE, later), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_sweep_does_not_follow_symbolic_links() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let base = ensure_base(tmp.path()).unwrap();
        let precious = tmp.path().join("precious");
        std::fs::create_dir(&precious).unwrap();
        std::fs::write(precious.join("keep.txt"), b"keep").unwrap();
        let later = SystemTime::now() + 2 * MAX_AGE;

        // A link in place of a folder: left alone, target untouched.
        let link = base.join(random_name().unwrap());
        symlink(&precious, &link).unwrap();
        assert_eq!(sweep(&base, MAX_AGE, later), 0);
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(precious.join("keep.txt").exists());

        // A link inside an old folder: removed as a link, target untouched.
        let old = old_folder(&base);
        symlink(&precious, old.join("escape")).unwrap();
        symlink(precious.join("keep.txt"), old.join("escape.txt")).unwrap();
        assert_eq!(sweep(&base, MAX_AGE, later), 1);
        assert!(!old.exists());
        assert_eq!(std::fs::read(precious.join("keep.txt")).unwrap(), b"keep");

        // A base that is itself a link is not swept.
        let real = tmp.path().join("real-base");
        std::fs::create_dir(&real).unwrap();
        let victim = old_folder(&real);
        let via = tmp.path().join("via-link");
        symlink(&real, &via).unwrap();
        assert_eq!(sweep(&via, MAX_AGE, later), 0);
        assert!(victim.exists());
        // And `ensure_base` refuses a link in its place.
        let cache = tmp.path().join("cache2");
        std::fs::create_dir(&cache).unwrap();
        symlink(&real, cache.join(FOLDER)).unwrap();
        assert!(ensure_base(&cache).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_from_an_archive_is_still_removed_and_still_gathered() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("locked");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner/f.txt"), b"x").unwrap();
        std::fs::set_permissions(dir.join("inner"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        remove_tree(&dir).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn folder_names_are_recognised_exactly() {
        assert!(is_folder_name(&random_name().unwrap()));
        assert!(!is_folder_name(""));
        assert!(!is_folder_name(&"a".repeat(31)));
        assert!(!is_folder_name(&"a".repeat(33)));
        assert!(!is_folder_name(&"A".repeat(32)));
        assert!(!is_folder_name(&"g".repeat(32)));
        assert!(!is_folder_name(&format!("{}/..", "a".repeat(29))));
        assert_ne!(random_name().unwrap(), random_name().unwrap());
    }

    #[test]
    fn duplicate_names_keep_their_extension_and_folders_are_not_split() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.tar.gz"), b"").unwrap();
        std::fs::write(tmp.path().join(".rc"), b"").unwrap();
        std::fs::create_dir(tmp.path().join("v1.2")).unwrap();
        let name = |n: &str, d: bool| {
            unique_in(tmp.path(), OsStr::new(n), d)
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name("a.tar.gz", false), "a.tar (2).gz");
        assert_eq!(name(".rc", false), ".rc (2)");
        assert_eq!(name("v1.2", true), "v1.2 (2)");
        assert_eq!(name("fresh.txt", false), "fresh.txt");
    }

    // ---- foreign archives, through the real helper ----

    mod foreign_archives {
        use super::*;

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

        fn fixture(name: &str) -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../fixtures/foreign")
                .join(name)
        }

        fn open(name: &str) -> (Foreign, u32, tempfile::TempDir, PathBuf) {
            let foreign = Foreign::default();
            foreign.set_helper(helper());
            let handle = foreign.open(&fixture(name), None).unwrap().handle;
            let tmp = tempfile::tempdir().unwrap();
            let base = ensure_base(tmp.path()).unwrap();
            (foreign, handle, tmp, base)
        }

        #[test]
        fn a_nested_zip_entry_lands_at_the_top_level() {
            let (foreign, handle, _tmp, base) = open("basic_infozip.zip");
            let p =
                prepare_foreign(&foreign, &base, handle, &keys(&["sub/b.txt"]), MAX_BYTES).unwrap();
            assert_eq!(names_of(&p), ["b.txt"]);
            assert_eq!(std::fs::metadata(&p.items()[0]).unwrap().len(), 600);
            assert_eq!(tree(&p.root().join(ITEMS)).len(), 1);
        }

        #[test]
        fn a_zip_folder_and_a_file_together() {
            let (foreign, handle, _tmp, base) = open("basic_infozip.zip");
            let p = prepare_foreign(
                &foreign,
                &base,
                handle,
                &keys(&["sub", "sub/b.txt", "a.txt"]),
                MAX_BYTES,
            )
            .unwrap();
            assert_eq!(names_of(&p), ["a.txt", "sub"]);
            let items = tree(&p.root().join(ITEMS));
            assert!(items.contains_key("sub/b.txt"));
            assert!(items["a.txt"].is_some());
        }

        #[test]
        fn foreign_refusals_leave_no_folder() {
            let (foreign, handle, _tmp, base) = open("basic_infozip.zip");
            let cases: Vec<(&str, Vec<PathKey>, u32)> = vec![
                ("no keys", vec![], handle),
                ("bad key", vec!["xyz".to_owned()], handle),
                ("not in the archive", keys(&["nope"]), handle),
                ("not open", keys(&["a.txt"]), 4242),
            ];
            for (what, ks, h) in cases {
                let e = prepare_foreign(&foreign, &base, h, &ks, MAX_BYTES).unwrap_err();
                assert_eq!(e.code, ErrorCode::InvalidArgument, "{what}: {e}");
            }
            // sub/b.txt declares 600 bytes.
            let e = prepare_foreign(&foreign, &base, handle, &keys(&["sub"]), 599).unwrap_err();
            assert_eq!(e.code, ErrorCode::LimitExceeded);
            assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
            prepare_foreign(&foreign, &base, handle, &keys(&["sub"]), 600).unwrap();
        }

        #[test]
        fn a_name_the_extractor_refuses_cannot_be_dragged() {
            let (foreign, handle, _tmp, base) = open("traversal.zip");
            // The listing offers no key for a refused name, so the only
            // route is a hand-made key, which names nothing.
            let e = prepare_foreign(&foreign, &base, handle, &keys(&["../evil.txt"]), MAX_BYTES)
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument);
            assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
        }
    }
}
