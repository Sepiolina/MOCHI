//! Finding names across archives: the engine of `mochi find` and of the
//! desktop app's MOCHI Find (spec Annex B.2.18, D28; §19.1 discovery, §19.3
//! and §23.3 #8 coverage).
//!
//! # What it does
//!
//! * **Walk** ([`walk`]). The scope folders are walked through a
//!   [`FindTree`] for files named `*.mochi` (any case). **No symbolic link or
//!   junction is followed**, so no loop is possible; a linked folder or a
//!   linked `.mochi` file is counted (`links_not_followed`). Depth, folders
//!   and archives are bounded ([`Limits`]); a bound that stops the walk is
//!   reported ([`Limit`]). A folder that cannot be listed is counted.
//! * **Open** ([`index_archive`]). Each archive is opened **read-only**
//!   through [`open_head`], which hash-verifies the catalog image before it is
//!   opened, and its head snapshot is listed with [`search`] (an empty
//!   query). No file content is read or hashed: Find is not verification.
//!   Every failure becomes the archive's [`Skip`] reason; only cancellation
//!   is an error.
//! * **Scan** ([`Scan::run`], a job). One thread walks, a bounded pool opens
//!   archives as they are found. Progress phase [`PHASE`]: `completed` is
//!   archives finished, `total` the archives found once the walk is over. A
//!   caller may supply a [`ScanCache`] (the desktop keeps one per session,
//!   keyed by path, size and modified time); the CLI uses [`NoCache`].
//! * **Query** ([`Matcher`]): case-insensitive substring of the last path
//!   component, or a glob when the text has `*` or `?` (matched against the
//!   whole component); text with a `/` is matched against the whole path
//!   inside the archive. Names match as stored bytes (O24); the only
//!   normalization is case folding, applied to the comparison only.
//!   [`Matcher::with_options`] adds the desktop's *Match case* and *Whole
//!   name only*, and [`TermSet`] matches a list of names at once, saying
//!   which each entry matched (spec Annex B.2.21, D31). `mochi find` uses the
//!   defaults, which are the rules above.
//! * **Coverage** ([`Coverage`]). What was searched and what was not:
//!   archives skipped with their reason, folders that could not be read,
//!   links not followed, the limit hit, archives still pending. Zero matches
//!   while [`Coverage::nothing_left_out`] is `false` is never "no matches".
//!
//! # What it never does
//!
//! * It never writes: archives are opened through [`FindTree::open`], which
//!   returns a read-only handle, and nothing is cached on disk.
//! * It never opens an Encrypted archive with a key: the read options' keys
//!   are dropped, so such an archive is [`Skip::Encrypted`].
//! * It never reads a draft archive (`UNSUPPORTED_FEATURE`, [`Skip::Unsupported`]).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use mochi_format::registry::ARCHIVE_DESCRIPTOR;

use crate::catalog::namespace::EntryKind;
use crate::descriptor::DraftPolicy;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::{CancellationToken, JobContext, NullProgress};
use crate::publish::{open_head, ReadOptions};
use crate::search::{search, Query, SnapshotScope};
use crate::storage::{ReadStorage, StorageError};

/// Progress phase of a scan.
pub const PHASE: &str = "find_scan";

/// Deepest folder level walked below a scope.
pub const MAX_DEPTH: usize = 48;
/// Most folders walked in one scan.
pub const MAX_FOLDERS: u64 = 200_000;
/// Most archives found in one scan.
pub const MAX_ARCHIVES: usize = 20_000;
/// Most names one archive may hold for Find (a bigger one is "too large").
pub const MAX_ENTRIES_PER_ARCHIVE: usize = 1_000_000;
/// Most names one scan holds in memory across all archives.
pub const MAX_ENTRIES_TOTAL: usize = 3_000_000;
/// Most scope folders.
pub const MAX_SCOPES: usize = 64;
/// Most workers opening archives at once.
pub const MAX_WORKERS: usize = 8;
/// The longest query accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 1_024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Only plain data lives behind these locks; a panic elsewhere leaves it usable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- the tree ------------------------------------------------------------------

/// What identifies one version of an archive file: its size and modified time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

impl Stamp {
    /// The modified time in milliseconds since 1970, when it is known.
    pub fn modified_ms(&self) -> Option<i64> {
        let d = self.mtime?.duration_since(UNIX_EPOCH).ok()?;
        i64::try_from(d.as_millis()).ok()
    }
}

/// One directory entry as the walk sees it (links not followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeKind {
    Folder,
    File,
    /// A symbolic link or a junction; `to_folder` when its target is a folder.
    Link {
        to_folder: bool,
    },
    /// A device, FIFO, socket, or anything else.
    Other,
    /// The entry's type could not be read.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub kind: TreeKind,
}

/// The folders Find walks and the archives it opens. The OS implementation is
/// `storage::os::OsFindTree`; tests may supply another.
pub trait FindTree: Sync {
    /// A read-only archive handle.
    type Archive: ReadStorage;

    /// The entries of `dir`, any order, without following links. `None` when
    /// the folder cannot be listed.
    fn list(&self, dir: &Path) -> Option<Vec<TreeEntry>>;

    /// The size and modified time of the file at `path` (not following a link).
    fn stamp(&self, path: &Path) -> Option<Stamp>;

    /// Open the file at `path` for reading only.
    fn open(&self, path: &Path) -> std::result::Result<Self::Archive, StorageError>;
}

/// Whether a file name is an archive's for Find: extension `mochi`, any case.
pub fn is_archive_name(name: &OsStr) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mochi"))
}

// ---- one archive ---------------------------------------------------------------

struct Slot {
    start: u32,
    len: u32,
    size: u64,
    dir: bool,
}

/// One entry of an [`ArchiveIndex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry<'a> {
    /// The exact stored path bytes.
    pub path: &'a [u8],
    /// Logical size; 0 for a folder.
    pub size: u64,
    pub kind: EntryKind,
}

/// The names of one archive's head snapshot: one byte arena and one slot per
/// entry, in the catalog's path order.
#[derive(Default)]
pub struct ArchiveIndex {
    names: Vec<u8>,
    slots: Vec<Slot>,
}

impl ArchiveIndex {
    fn push(&mut self, path: &[u8], size: u64, dir: bool) -> bool {
        let (Ok(start), Ok(len)) = (u32::try_from(self.names.len()), u32::try_from(path.len()))
        else {
            return false;
        };
        if start.checked_add(len).is_none() {
            return false;
        }
        self.names.extend_from_slice(path);
        self.slots.push(Slot {
            start,
            len,
            size,
            dir,
        });
        true
    }

    /// Number of entries (files and folders).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn path(&self, slot: &Slot) -> &[u8] {
        let start = slot.start as usize;
        self.names
            .get(start..start + slot.len as usize)
            .unwrap_or_default()
    }

    fn view<'a>(&'a self, slot: &Slot) -> IndexEntry<'a> {
        IndexEntry {
            path: self.path(slot),
            size: slot.size,
            kind: if slot.dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
        }
    }

    /// Entry number `i`.
    pub fn get(&self, i: usize) -> Option<IndexEntry<'_>> {
        self.slots.get(i).map(|s| self.view(s))
    }

    /// The entry whose stored path is exactly `path`.
    pub fn find(&self, path: &[u8]) -> Option<IndexEntry<'_>> {
        self.slots
            .iter()
            .find(|s| self.path(s) == path)
            .map(|s| self.view(s))
    }

    /// Every entry, in path order.
    pub fn iter(&self) -> impl Iterator<Item = IndexEntry<'_>> {
        self.slots.iter().map(|s| self.view(s))
    }
}

/// Why an archive could not be searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// The file does not start like a MOCHI archive.
    NotMochi,
    /// Encrypted: names are sealed without the passphrase, which Find never has.
    Encrypted,
    /// A draft or newer format this build does not read.
    Unsupported { code: ErrorCode, message: String },
    /// Damaged, truncated, or otherwise refused by `mochi_core`.
    Damaged { code: ErrorCode, message: String },
    /// Too many names to hold in memory.
    TooLarge,
    /// The file could not be opened or read.
    Unreadable { message: String },
}

impl Skip {
    /// The reason as a stable lower-case word (JSON, the desktop's IPC).
    pub fn reason(&self) -> &'static str {
        match self {
            Skip::NotMochi => "not_mochi",
            Skip::Encrypted => "encrypted",
            Skip::Unsupported { .. } => "unsupported",
            Skip::Damaged { .. } => "damaged",
            Skip::TooLarge => "too_large",
            Skip::Unreadable { .. } => "unreadable",
        }
    }

    /// The stable error code, when `mochi_core` gave one.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Skip::Unsupported { code, .. } | Skip::Damaged { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// `mochi_core`'s sentence, when there is one. It can quote archive
    /// content: escape it before showing it.
    pub fn message(&self) -> Option<&str> {
        match self {
            Skip::Unsupported { message, .. }
            | Skip::Damaged { message, .. }
            | Skip::Unreadable { message } => Some(message),
            _ => None,
        }
    }
}

/// What opening one archive gave.
#[derive(Clone)]
pub enum Outcome {
    Indexed(Arc<ArchiveIndex>),
    Skipped(Skip),
}

/// The read options Find uses: the caller's limits, never a key, never a draft.
fn find_options(opts: &ReadOptions) -> ReadOptions {
    ReadOptions {
        keys: None,
        drafts: DraftPolicy::default(),
        ..opts.clone()
    }
}

/// Open one archive read-only and list the names of its head snapshot.
/// Only cancellation is an `Err`; every other failure is the archive's
/// [`Skip`]. Keys in `opts` are ignored: Find has no passphrase.
pub fn index_archive(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let opts = find_options(opts);
    let mut magic = [0u8; 4];
    match src.read_exact_at(0, &mut magic) {
        Ok(()) if magic == ARCHIVE_DESCRIPTOR.to_le_bytes() => {}
        Ok(()) | Err(StorageError::OutOfBounds { .. }) => {
            return Ok(Outcome::Skipped(Skip::NotMochi))
        }
        Err(e) => {
            return Ok(Outcome::Skipped(Skip::Unreadable {
                message: MochiError::from(e).message,
            }))
        }
    }
    let skip = |e: MochiError| match e.code {
        ErrorCode::Cancelled => Err(e),
        ErrorCode::KeyUnavailable => Ok(Outcome::Skipped(Skip::Encrypted)),
        ErrorCode::UnsupportedFeature => Ok(Outcome::Skipped(Skip::Unsupported {
            code: e.code,
            message: e.message,
        })),
        ErrorCode::IoError => Ok(Outcome::Skipped(Skip::Unreadable { message: e.message })),
        code => Ok(Outcome::Skipped(Skip::Damaged {
            code,
            message: e.message,
        })),
    };
    let head = match open_head(src, &opts) {
        Ok(h) => h,
        Err(e) => return skip(e),
    };
    if head.descriptor.encrypted() && head.unlocked.is_none() {
        return Ok(Outcome::Skipped(Skip::Encrypted));
    }
    let ctx = JobContext {
        progress: &NullProgress,
        cancel,
    };
    let result = match search(
        src,
        &head,
        SnapshotScope::Head,
        &Query::default(),
        &opts,
        &ctx,
    ) {
        Ok(r) => r,
        Err(e) => return skip(e),
    };
    if !result.coverage.complete() {
        let reason = result
            .coverage
            .unavailable
            .first()
            .map(|u| u.error.clone())
            .unwrap_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "the head snapshot was not covered",
                )
            });
        return skip(reason);
    }
    if result.hits.len() > MAX_ENTRIES_PER_ARCHIVE {
        return Ok(Outcome::Skipped(Skip::TooLarge));
    }
    let mut index = ArchiveIndex::default();
    index.slots.reserve(result.hits.len());
    for h in &result.hits {
        let dir = h.kind == EntryKind::Directory;
        if !index.push(h.path.as_stored(), h.logical_len, dir) {
            return Ok(Outcome::Skipped(Skip::TooLarge));
        }
    }
    Ok(Outcome::Indexed(Arc::new(index)))
}

// ---- the walk ------------------------------------------------------------------

/// A limit that stopped the walk early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    Depth,
    Folders,
    Archives,
}

impl Limit {
    pub fn as_str(self) -> &'static str {
        match self {
            Limit::Depth => "depth",
            Limit::Folders => "folders",
            Limit::Archives => "archives",
        }
    }
}

/// Bounds of a scan (tests lower them).
#[derive(Debug, Clone)]
pub struct Limits {
    /// Whether the subfolders of a scope are walked at all. `false` is the
    /// user's choice to search only the chosen folders themselves (desktop
    /// Find, B.2.21): it is not a bound, so no [`Limit`] is reported for it.
    pub subfolders: bool,
    pub depth: usize,
    pub folders: u64,
    pub archives: usize,
    pub entries_total: usize,
    pub workers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, usize::from);
        Limits {
            subfolders: true,
            depth: MAX_DEPTH,
            folders: MAX_FOLDERS,
            archives: MAX_ARCHIVES,
            entries_total: MAX_ENTRIES_TOTAL,
            workers: cpus.clamp(2, MAX_WORKERS),
        }
    }
}

/// What a walk covered.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkStats {
    pub folders: u64,
    /// Folders that could not be listed, and entries whose type or size
    /// could not be read.
    pub unreadable: u64,
    /// Linked folders and linked `.mochi` files, not followed.
    pub links: u64,
    pub limit: Option<Limit>,
}

/// Walk `scopes` depth first, calling `on_archive` for each `.mochi` file:
/// a folder's archives in name order, then its subfolders in name order.
/// `on_archive` returns `false` to stop the walk. Links are counted, never
/// followed. Cancellation stops the walk between folders.
pub fn walk<T: FindTree + ?Sized>(
    tree: &T,
    scopes: &[PathBuf],
    limits: &Limits,
    cancel: &CancellationToken,
    on_archive: &mut dyn FnMut(PathBuf, Stamp) -> bool,
) -> WalkStats {
    let mut stats = WalkStats::default();
    let mut archives_found = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = scopes.iter().rev().map(|p| (p.clone(), 0)).collect();
    'dirs: while let Some((dir, depth)) = stack.pop() {
        if cancel.is_cancelled() {
            break;
        }
        if stats.folders >= limits.folders {
            stats.limit = Some(Limit::Folders);
            break;
        }
        stats.folders += 1;
        let Some(entries) = tree.list(&dir) else {
            stats.unreadable += 1;
            continue;
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut archives: Vec<(PathBuf, Stamp)> = Vec::new();
        for e in entries {
            let named = e.path.file_name().is_some_and(is_archive_name);
            match e.kind {
                TreeKind::Unreadable => stats.unreadable += 1,
                TreeKind::Link { to_folder } => {
                    // Not followed: it cannot loop, and the coverage says so.
                    if named || to_folder {
                        stats.links += 1;
                    }
                }
                TreeKind::Folder => {
                    if !limits.subfolders {
                        // Not asked for: the scope is the folder itself.
                    } else if depth >= limits.depth {
                        stats.limit = Some(Limit::Depth);
                    } else {
                        subdirs.push(e.path);
                    }
                }
                TreeKind::File if named => match tree.stamp(&e.path) {
                    Some(stamp) => archives.push((e.path, stamp)),
                    None => stats.unreadable += 1,
                },
                TreeKind::File | TreeKind::Other => {}
            }
        }
        archives.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, stamp) in archives {
            if archives_found >= limits.archives {
                stats.limit = Some(Limit::Archives);
                break 'dirs;
            }
            archives_found += 1;
            if !on_archive(path, stamp) {
                break 'dirs;
            }
        }
        subdirs.sort();
        stack.extend(subdirs.into_iter().rev().map(|p| (p, depth + 1)));
    }
    stats
}

// ---- a scan ---------------------------------------------------------------------

/// Name lists kept between scans by a caller. `get` answers only for an
/// unchanged stamp.
pub trait ScanCache: Sync {
    fn get(&self, path: &Path, stamp: &Stamp) -> Option<Outcome>;
    fn put(&self, path: &Path, stamp: Stamp, outcome: &Outcome);
}

/// No cache: every archive is opened (the CLI).
pub struct NoCache;

impl ScanCache for NoCache {
    fn get(&self, _: &Path, _: &Stamp) -> Option<Outcome> {
        None
    }
    fn put(&self, _: &Path, _: Stamp, _: &Outcome) {}
}

/// One archive the walk found.
#[derive(Clone)]
pub struct Record {
    pub path: PathBuf,
    pub stamp: Stamp,
    /// `None` while the archive waits to be opened.
    pub outcome: Option<Outcome>,
}

#[derive(Default)]
struct ScanInner {
    records: Vec<Record>,
    walk: WalkStats,
    from_cache: u64,
}

/// One scan of some folders, and what it found.
pub struct Scan {
    scopes: Vec<PathBuf>,
    inner: Mutex<ScanInner>,
    found: AtomicUsize,
    done: AtomicUsize,
    held: AtomicUsize,
    walk_done: AtomicBool,
}

/// An archive that was not searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The archive's number in the scan.
    pub index: usize,
    pub path: PathBuf,
    pub skip: Skip,
}

/// What a scan covered and what it did not (spec §19.3, §23.3 #8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub folders_visited: u64,
    pub folders_unreadable: u64,
    pub links_not_followed: u64,
    pub limit: Option<Limit>,
    pub archives_found: u64,
    pub archives_searched: u64,
    /// Found but not opened (a scan that is running or was stopped).
    pub archives_pending: u64,
    /// Of `archives_searched`, how many came from the cache.
    pub archives_from_cache: u64,
    /// Every archive not searched, in the order found.
    pub skipped: Vec<Skipped>,
    pub names_searched: u64,
}

impl Coverage {
    /// Every archive found was searched, every folder was read, nothing was
    /// left unfollowed, and no limit was hit. With a scan that finished (was
    /// not cancelled), this is the only state in which "no matches" means none.
    pub fn nothing_left_out(&self) -> bool {
        self.archives_pending == 0
            && self.skipped.is_empty()
            && self.folders_unreadable == 0
            && self.links_not_followed == 0
            && self.limit.is_none()
    }
}

/// The matches of a query in one archive.
pub struct ArchiveMatches {
    /// The archive's number in the scan.
    pub index: usize,
    pub path: PathBuf,
    pub stamp: Stamp,
    pub names: Arc<ArchiveIndex>,
    /// All matches in this archive.
    pub count: u64,
    /// The first matches (at most the cap asked for), as entry numbers in `names`.
    pub entries: Vec<usize>,
}

impl Scan {
    pub fn new(scopes: Vec<PathBuf>) -> Scan {
        Scan {
            scopes,
            inner: Mutex::new(ScanInner::default()),
            found: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            walk_done: AtomicBool::new(false),
        }
    }

    pub fn scopes(&self) -> &[PathBuf] {
        &self.scopes
    }

    /// Walk the scopes and open every archive found: one walking thread,
    /// `limits.workers` opening archives. `Err(CANCELLED)` if cancelled; what
    /// was found stays in the scan. Keys in `opts` are ignored.
    pub fn run<T: FindTree>(
        &self,
        tree: &T,
        opts: &ReadOptions,
        limits: &Limits,
        cache: &dyn ScanCache,
        ctx: &JobContext<'_>,
    ) -> Result<()> {
        let opts = find_options(opts);
        let (tx, rx) = sync_channel::<usize>(256);
        let rx = Mutex::new(rx);
        std::thread::scope(|s| {
            for _ in 0..limits.workers.max(1) {
                s.spawn(|| loop {
                    let next = lock(&rx).recv();
                    let Ok(idx) = next else { return };
                    if self.process(tree, &opts, cache, limits, ctx, idx).is_err() {
                        // Cancelled: drain without working so the walker never blocks.
                        while lock(&rx).recv().is_ok() {}
                        return;
                    }
                });
            }
            let stats = walk(
                tree,
                &self.scopes,
                limits,
                ctx.cancel,
                &mut |path, stamp| {
                    let idx = {
                        let mut inner = lock(&self.inner);
                        inner.records.push(Record {
                            path,
                            stamp,
                            outcome: None,
                        });
                        inner.records.len() - 1
                    };
                    self.found.fetch_add(1, Ordering::SeqCst);
                    ctx.report(PHASE, self.done.load(Ordering::SeqCst) as u64, None);
                    tx.send(idx).is_ok()
                },
            );
            lock(&self.inner).walk = stats;
            self.walk_done.store(true, Ordering::SeqCst);
            drop(tx);
        });
        // The walk is over even if it was cut short: report the final count.
        let total = self.found.load(Ordering::SeqCst) as u64;
        ctx.report(PHASE, self.done.load(Ordering::SeqCst) as u64, Some(total));
        ctx.check_cancelled()
    }

    /// Open one found archive (from the cache when its stamp is unchanged)
    /// and store the outcome.
    fn process<T: FindTree>(
        &self,
        tree: &T,
        opts: &ReadOptions,
        cache: &dyn ScanCache,
        limits: &Limits,
        ctx: &JobContext<'_>,
        idx: usize,
    ) -> Result<()> {
        ctx.check_cancelled()?;
        let (path, stamp) = {
            let inner = lock(&self.inner);
            let Some(r) = inner.records.get(idx) else {
                return Ok(());
            };
            (r.path.clone(), r.stamp)
        };
        let (mut outcome, cached) = match cache.get(&path, &stamp) {
            Some(o) => (o, true),
            None => {
                let o = match tree.open(&path) {
                    Ok(src) => index_archive(&src, opts, ctx.cancel)?,
                    Err(e) => Outcome::Skipped(Skip::Unreadable {
                        message: MochiError::from(e).message,
                    }),
                };
                (o, false)
            }
        };
        if let Outcome::Indexed(ix) = &outcome {
            let n = ix.len();
            if self.held.fetch_add(n, Ordering::SeqCst).saturating_add(n) > limits.entries_total {
                self.held.fetch_sub(n, Ordering::SeqCst);
                outcome = Outcome::Skipped(Skip::TooLarge);
            }
        }
        if !cached {
            cache.put(&path, stamp, &outcome);
        }
        {
            let mut inner = lock(&self.inner);
            if cached {
                inner.from_cache += 1;
            }
            if let Some(r) = inner.records.get_mut(idx) {
                r.outcome = Some(outcome);
            }
        }
        let done = self.done.fetch_add(1, Ordering::SeqCst) + 1;
        let total = self
            .walk_done
            .load(Ordering::SeqCst)
            .then(|| self.found.load(Ordering::SeqCst) as u64);
        ctx.report(PHASE, done as u64, total);
        Ok(())
    }

    /// Archive number `index` of the scan.
    pub fn record(&self, index: usize) -> Option<Record> {
        lock(&self.inner).records.get(index).cloned()
    }

    /// What the scan has covered so far.
    pub fn coverage(&self) -> Coverage {
        let inner = lock(&self.inner);
        let mut searched = 0u64;
        let mut pending = 0u64;
        let mut names = 0u64;
        let mut skipped = Vec::new();
        for (i, r) in inner.records.iter().enumerate() {
            match &r.outcome {
                None => pending += 1,
                Some(Outcome::Indexed(ix)) => {
                    searched += 1;
                    names = names.saturating_add(ix.len() as u64);
                }
                Some(Outcome::Skipped(s)) => skipped.push(Skipped {
                    index: i,
                    path: r.path.clone(),
                    skip: s.clone(),
                }),
            }
        }
        Coverage {
            folders_visited: inner.walk.folders,
            folders_unreadable: inner.walk.unreadable,
            links_not_followed: inner.walk.links,
            limit: inner.walk.limit,
            archives_found: inner.records.len() as u64,
            archives_searched: searched,
            archives_pending: pending,
            archives_from_cache: inner.from_cache,
            skipped,
            names_searched: names,
        }
    }

    /// The matches of `matcher` in every searched archive, in archive path
    /// order, keeping the first `cap` entry numbers per archive (all are
    /// counted). Archives without a match are left out. Large sets are
    /// matched on several threads.
    pub fn query(&self, matcher: &Matcher, cap: usize) -> Vec<ArchiveMatches> {
        let sets = self.indexed_sets();
        let per_archive = par_map(&sets, |ix| matches_in(ix, matcher, cap));
        sets.into_iter()
            .zip(per_archive)
            .filter(|(_, (count, _))| *count > 0)
            .map(
                |((index, path, stamp, names), (count, entries))| ArchiveMatches {
                    index,
                    path,
                    stamp,
                    names,
                    count,
                    entries,
                },
            )
            .collect()
    }

    /// The matches of every term of `terms` in every searched archive, in
    /// archive path order: per archive, the first `cap` matching entries with
    /// the terms each matched, and every term's count (all are counted).
    /// Archives without a match are left out.
    pub fn query_terms(&self, terms: &TermSet, cap: usize) -> Vec<TermMatches> {
        let sets = self.indexed_sets();
        let per_archive = par_map(&sets, |ix| terms_in(ix, terms, cap));
        sets.into_iter()
            .zip(per_archive)
            .filter(|(_, m)| m.count > 0)
            .map(|((index, path, stamp, names), m)| TermMatches {
                index,
                path,
                stamp,
                names,
                count: m.count,
                entries: m.entries,
                per_term: m.per_term,
            })
            .collect()
    }

    /// Every searched archive's names, in archive path order.
    fn indexed_sets(&self) -> Vec<IndexedSet> {
        let mut sets: Vec<IndexedSet> = {
            let inner = lock(&self.inner);
            inner
                .records
                .iter()
                .enumerate()
                .filter_map(|(i, r)| match &r.outcome {
                    Some(Outcome::Indexed(ix)) => {
                        Some((i, r.path.clone(), r.stamp, Arc::clone(ix)))
                    }
                    _ => None,
                })
                .collect()
        };
        sets.sort_by(|a, b| a.1.cmp(&b.1));
        sets
    }
}

/// One searched archive: its number, path, stamp and names.
type IndexedSet = (usize, PathBuf, Stamp, Arc<ArchiveIndex>);

/// The matches of a [`TermSet`] in one archive.
pub struct TermMatches {
    /// The archive's number in the scan.
    pub index: usize,
    pub path: PathBuf,
    pub stamp: Stamp,
    pub names: Arc<ArchiveIndex>,
    /// Entries that matched at least one term (an entry is counted once).
    pub count: u64,
    /// The first matching entries (at most the cap asked for): the entry
    /// number in `names` and the numbers of the terms it matched, ascending.
    pub entries: Vec<(usize, Vec<u16>)>,
    /// For each term, in order, how many entries of this archive it matched.
    pub per_term: Vec<u64>,
}

/// The matches in one archive: how many, and the first few as entry numbers.
type Matched = (u64, Vec<usize>);

/// (matches, the first `cap` of them as entry numbers) in one archive.
fn matches_in(ix: &ArchiveIndex, m: &Matcher, cap: usize) -> Matched {
    let mut count = 0u64;
    let mut first = Vec::new();
    for (i, slot) in ix.slots.iter().enumerate() {
        if m.matches(ix.path(slot)) {
            count += 1;
            if first.len() < cap {
                first.push(i);
            }
        }
    }
    (count, first)
}

/// Matched entries of one archive for a [`TermSet`].
#[derive(Clone, Default)]
struct TermsMatched {
    count: u64,
    entries: Vec<(usize, Vec<u16>)>,
    per_term: Vec<u64>,
}

fn terms_in(ix: &ArchiveIndex, terms: &TermSet, cap: usize) -> TermsMatched {
    let mut out = TermsMatched {
        per_term: vec![0; terms.len()],
        ..TermsMatched::default()
    };
    let mut hit = Vec::new();
    for (i, slot) in ix.slots.iter().enumerate() {
        terms.matching(ix.view(slot), &mut hit);
        if hit.is_empty() {
            continue;
        }
        out.count += 1;
        for &t in &hit {
            if let Some(c) = out.per_term.get_mut(usize::from(t)) {
                *c += 1;
            }
        }
        if out.entries.len() < cap {
            out.entries.push((i, std::mem::take(&mut hit)));
        }
    }
    out
}

/// `f` over every set's names; large sets on several threads. One result
/// per set, in order.
fn par_map<R, F>(sets: &[IndexedSet], f: F) -> Vec<R>
where
    R: Send + Clone + Default,
    F: Fn(&ArchiveIndex) -> R + Sync,
{
    let names: usize = sets.iter().map(|s| s.3.len()).sum();
    let workers = if names < 100_000 {
        1
    } else {
        std::thread::available_parallelism()
            .map_or(2, usize::from)
            .clamp(1, MAX_WORKERS)
    };
    if workers == 1 {
        return sets.iter().map(|s| f(&s.3)).collect();
    }
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new(vec![None; sets.len()]);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(set) = sets.get(i) else { return };
                let r = f(&set.3);
                if let Some(slot) = lock(&out).get_mut(i) {
                    *slot = Some(r);
                }
            });
        }
    });
    out.into_inner()
        .unwrap_or_else(|p| p.into_inner())
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect()
}

// ---- matching ----------------------------------------------------------------------

/// What a query asks for. Names match as bytes of the stored path (O24); the
/// only normalization is case folding, applied to the comparison only.
#[derive(Debug, Clone)]
pub struct Matcher {
    kind: MatcherKind,
    ignore_case: bool,
    whole_path: bool,
}

#[derive(Debug, Clone)]
enum MatcherKind {
    Any,
    Substring {
        raw: Vec<u8>,
        lower: String,
    },
    /// The whole name (or path), equal after [`fold`] when case is ignored.
    Exact {
        raw: Vec<u8>,
        folded: Vec<u8>,
    },
    Glob(Vec<char>),
}

/// How a query compares names (desktop Find's options, B.2.21). The
/// default is what `mochi find` does: case ignored, part of a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchOptions {
    /// Compare with case folded (the default) or byte for byte.
    pub ignore_case: bool,
    /// Text without `*` or `?` must equal the whole name (or, with a `/`, the
    /// whole path) instead of being part of it. A glob covers the whole name
    /// either way.
    pub whole_name: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        MatchOptions {
            ignore_case: true,
            whole_name: false,
        }
    }
}

/// `bytes` with case folded as the matcher folds it: Unicode lower-casing
/// per character for UTF-8, ASCII folding for bytes that are not UTF-8.
fn fold(bytes: &[u8]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_lowercase().into_bytes(),
        Err(_) => bytes.to_ascii_lowercase(),
    }
}

/// The last path component of a stored path.
fn last_component(path: &[u8]) -> &[u8] {
    path.rsplit(|&b| b == b'/').next().unwrap_or(path)
}

fn chars_of(bytes: &[u8], ignore_case: bool) -> Vec<char> {
    match std::str::from_utf8(bytes) {
        Ok(s) if ignore_case => s.chars().flat_map(char::to_lowercase).collect(),
        Ok(s) => s.chars().collect(),
        Err(_) => bytes
            .iter()
            .map(|&b| {
                if ignore_case {
                    char::from(b.to_ascii_lowercase())
                } else {
                    char::from(b)
                }
            })
            .collect(),
    }
}

/// `*` matches any run, `?` any one character; the pattern covers the whole text.
fn glob(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while let Some(&tc) = t.get(ti) {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                mark = ti;
                pi += 1;
            }
            Some(&c) if c == '?' || c == tc => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some(s) => {
                    pi = s + 1;
                    mark += 1;
                    ti = mark;
                }
                None => return false,
            },
        }
    }
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

impl Matcher {
    /// `text` is the user's query, trimmed. Empty matches everything.
    pub fn new(text: &str, ignore_case: bool) -> Matcher {
        Matcher::with_options(
            text,
            MatchOptions {
                ignore_case,
                whole_name: false,
            },
        )
    }

    /// [`Matcher::new`] with every option. `Matcher::with_options(t,
    /// MatchOptions::default())` is exactly `Matcher::new(t, true)`.
    pub fn with_options(text: &str, options: MatchOptions) -> Matcher {
        let MatchOptions {
            ignore_case,
            whole_name,
        } = options;
        let text = text.trim();
        let whole_path = text.contains('/');
        let kind = if text.is_empty() {
            MatcherKind::Any
        } else if text.contains(['*', '?']) {
            MatcherKind::Glob(chars_of(text.as_bytes(), ignore_case))
        } else if whole_name {
            MatcherKind::Exact {
                raw: text.as_bytes().to_vec(),
                folded: fold(text.as_bytes()),
            }
        } else {
            MatcherKind::Substring {
                raw: text.as_bytes().to_vec(),
                lower: text.to_lowercase(),
            }
        };
        Matcher {
            kind,
            ignore_case,
            whole_path,
        }
    }

    /// Whether the query is a glob (it has `*` or `?`).
    pub fn is_glob(&self) -> bool {
        matches!(self.kind, MatcherKind::Glob(_))
    }

    /// Whether the query is matched against the whole path (it has a `/`).
    pub fn is_whole_path(&self) -> bool {
        self.whole_path
    }

    /// Whether the stored path `path` matches.
    pub fn matches(&self, path: &[u8]) -> bool {
        let target = if self.whole_path {
            path
        } else {
            last_component(path)
        };
        match &self.kind {
            MatcherKind::Any => true,
            MatcherKind::Exact { raw, folded } => {
                if self.ignore_case {
                    fold(target) == *folded
                } else {
                    target == raw.as_slice()
                }
            }
            MatcherKind::Glob(p) => glob(p, &chars_of(target, self.ignore_case)),
            MatcherKind::Substring { raw, lower } => {
                if !self.ignore_case {
                    return contains(target, raw, false);
                }
                if lower.is_ascii() {
                    return contains(target, lower.as_bytes(), true);
                }
                match std::str::from_utf8(target) {
                    Ok(s) => s.to_lowercase().contains(lower.as_str()),
                    Err(_) => contains(target, raw, false),
                }
            }
        }
    }
}

/// Most terms one [`TermSet`] holds (desktop Find's pasted list, B.2.21).
pub const MAX_TERMS: usize = 1_000;

/// Which entries a [`TermSet`] may match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EntryFilter {
    /// Files and folders (what `mochi find` matches).
    #[default]
    All,
    Files,
    Folders,
}

impl EntryFilter {
    fn admits(self, kind: EntryKind) -> bool {
        match self {
            EntryFilter::All => true,
            EntryFilter::Files => kind != EntryKind::Directory,
            EntryFilter::Folders => kind == EntryKind::Directory,
        }
    }
}

/// Several queries at once (desktop Find's list of names, B.2.21): an entry
/// matches the set when it matches any term, and the set says which. Each
/// term follows the [`Matcher`] rules with the same [`MatchOptions`]. Terms
/// that must equal a whole name are looked up by name instead of compared one
/// by one; the answer is the same.
#[derive(Debug, Clone)]
pub struct TermSet {
    terms: Vec<Matcher>,
    /// Whole-name terms (no `*`, `?` or `/`) by the name they equal (folded
    /// when case is ignored): their term numbers.
    by_name: std::collections::HashMap<Vec<u8>, Vec<u16>>,
    /// Every other term's number.
    each: Vec<u16>,
    ignore_case: bool,
    filter: EntryFilter,
}

impl TermSet {
    /// `terms` are the user's texts, each trimmed; empty ones are refused, as
    /// are more than [`MAX_TERMS`] and a term over [`MAX_QUERY_BYTES`].
    pub fn new(terms: &[String], options: MatchOptions, filter: EntryFilter) -> Result<TermSet> {
        if terms.is_empty() || terms.len() > MAX_TERMS {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!("a search takes 1 to {MAX_TERMS} names"),
            ));
        }
        let mut set = TermSet {
            terms: Vec::with_capacity(terms.len()),
            by_name: std::collections::HashMap::new(),
            each: Vec::new(),
            ignore_case: options.ignore_case,
            filter,
        };
        for (i, text) in terms.iter().enumerate() {
            let text = text.trim();
            if text.is_empty() {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "an empty name is not a search; leave it out",
                ));
            }
            if text.len() > MAX_QUERY_BYTES {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    format!("a name to find takes at most {MAX_QUERY_BYTES} bytes"),
                ));
            }
            // MAX_TERMS fits in u16.
            let n = u16::try_from(i).unwrap_or(u16::MAX);
            let m = Matcher::with_options(text, options);
            match &m.kind {
                MatcherKind::Exact { raw, folded } if !m.whole_path => {
                    let key = if options.ignore_case { folded } else { raw };
                    set.by_name.entry(key.clone()).or_default().push(n);
                }
                _ => set.each.push(n),
            }
            set.terms.push(m);
        }
        Ok(set)
    }

    /// Number of terms.
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Term number `i`.
    pub fn term(&self, i: usize) -> Option<&Matcher> {
        self.terms.get(i)
    }

    /// The numbers of the terms `entry` matches, ascending, into `out`
    /// (cleared first). Empty when the filter leaves the entry out.
    pub fn matching(&self, entry: IndexEntry<'_>, out: &mut Vec<u16>) {
        out.clear();
        if !self.filter.admits(entry.kind) {
            return;
        }
        if !self.by_name.is_empty() {
            let name = last_component(entry.path);
            let hit = if self.ignore_case {
                self.by_name.get(&fold(name))
            } else {
                self.by_name.get(name)
            };
            if let Some(ns) = hit {
                out.extend_from_slice(ns);
            }
        }
        for &n in &self.each {
            if self
                .terms
                .get(usize::from(n))
                .is_some_and(|m| m.matches(entry.path))
            {
                out.push(n);
            }
        }
        out.sort_unstable();
    }
}

fn contains(hay: &[u8], needle: &[u8], fold: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| {
        if fold {
            w.eq_ignore_ascii_case(needle)
        } else {
            w == needle
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct MemStorage(Vec<u8>);

    impl ReadStorage for MemStorage {
        fn size(&self) -> std::result::Result<u64, StorageError> {
            Ok(self.0.len() as u64)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, StorageError> {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
    }

    #[test]
    fn matcher_folds_case_for_unicode_and_leaves_bytes_that_are_not_text_alone() {
        let m = Matcher::new("ÉCOLE", true);
        assert!(m.matches("docs/école.txt".as_bytes()));
        assert!(!Matcher::new("ÉCOLE", false).matches("école.txt".as_bytes()));
        let raw = Matcher::new("a?", true);
        assert!(raw.matches(b"dir/A\xff"));
        assert!(Matcher::new("*", true).matches(b""));
        assert!(!Matcher::new("a?c", true).matches(b"ac"));
        assert!(Matcher::new("*a*b*", true).matches(b"xxAyyBzz"));
    }

    #[test]
    fn matcher_kinds() {
        // A substring of the last component only.
        assert!(Matcher::new("report", true).matches(b"docs/Q3-REPORT.pdf"));
        assert!(!Matcher::new("docs", true).matches(b"docs/a.txt"));
        // A glob covers the whole last component.
        assert!(Matcher::new("*.pdf", true).matches(b"x/y/a.PDF"));
        assert!(!Matcher::new("ep*", true).matches(b"report.txt"));
        // A slash: the whole path.
        let p = Matcher::new("docs/inv", true);
        assert!(p.is_whole_path() && !p.is_glob());
        assert!(p.matches(b"docs/invoice.txt"));
        assert!(!p.matches(b"other/invoice.txt"));
        assert!(Matcher::new("docs/*.txt", true).matches(b"docs/invoice.txt"));
        assert!(Matcher::new("", true).matches(b"anything"));
    }

    /// An in-memory tree: folders, files (archive bytes), links.
    #[derive(Default)]
    struct Mem {
        dirs: BTreeMap<PathBuf, Vec<TreeEntry>>,
        files: BTreeMap<PathBuf, Vec<u8>>,
        unreadable: Vec<PathBuf>,
    }

    impl Mem {
        fn dir(&mut self, p: &str) {
            let p = PathBuf::from(p);
            if let Some(parent) = p.parent().filter(|q| !q.as_os_str().is_empty()) {
                if let Some(v) = self.dirs.get_mut(parent) {
                    v.push(TreeEntry {
                        path: p.clone(),
                        kind: TreeKind::Folder,
                    });
                }
            }
            self.dirs.entry(p).or_default();
        }
        fn entry(&mut self, p: &str, kind: TreeKind) {
            let p = PathBuf::from(p);
            let parent = p.parent().map(Path::to_path_buf).unwrap_or_default();
            self.dirs
                .entry(parent)
                .or_default()
                .push(TreeEntry { path: p, kind });
        }
        fn file(&mut self, p: &str, bytes: &[u8]) {
            self.entry(p, TreeKind::File);
            self.files.insert(PathBuf::from(p), bytes.to_vec());
        }
    }

    impl FindTree for Mem {
        type Archive = MemStorage;
        fn list(&self, dir: &Path) -> Option<Vec<TreeEntry>> {
            if self.unreadable.iter().any(|u| u == dir) {
                return None;
            }
            self.dirs.get(dir).cloned()
        }
        fn stamp(&self, path: &Path) -> Option<Stamp> {
            self.files.get(path).map(|b| Stamp {
                size: b.len() as u64,
                mtime: None,
            })
        }
        fn open(&self, path: &Path) -> std::result::Result<MemStorage, StorageError> {
            self.files
                .get(path)
                .map(|b| MemStorage(b.clone()))
                .ok_or_else(|| StorageError::Io(std::io::Error::other("gone")))
        }
    }

    fn ctx_run(tree: &Mem, scopes: &[&str], limits: &Limits) -> (Scan, Result<()>) {
        let scan = Scan::new(scopes.iter().map(PathBuf::from).collect());
        let cancel = CancellationToken::new();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &cancel,
        };
        let r = scan.run(tree, &ReadOptions::default(), limits, &NoCache, &ctx);
        (scan, r)
    }

    #[test]
    fn the_walk_counts_links_unreadable_folders_and_files_that_are_not_archives() {
        let mut t = Mem::default();
        t.dir("/r");
        t.dir("/r/sub");
        t.dir("/r/locked");
        t.unreadable.push(PathBuf::from("/r/locked"));
        t.file("/r/fake.MOCHI", b"not an archive");
        t.file("/r/tiny.mochi", b"ab");
        t.file("/r/note.txt", b"x");
        t.entry("/r/link-dir", TreeKind::Link { to_folder: true });
        t.entry("/r/link.mochi", TreeKind::Link { to_folder: false });
        t.entry("/r/link-file", TreeKind::Link { to_folder: false });
        t.entry("/r/odd", TreeKind::Unreadable);
        let (scan, r) = ctx_run(&t, &["/r"], &Limits::default());
        r.unwrap();
        let c = scan.coverage();
        assert_eq!(c.folders_visited, 3);
        assert_eq!(
            c.folders_unreadable, 2,
            "the locked folder and the odd entry"
        );
        assert_eq!(c.links_not_followed, 2);
        assert_eq!(c.archives_found, 2);
        assert_eq!(c.archives_searched, 0);
        assert!(c.skipped.iter().all(|s| s.skip == Skip::NotMochi));
        assert!(!c.nothing_left_out());
    }

    #[test]
    fn limits_stop_the_walk() {
        let mut t = Mem::default();
        t.dir("/r");
        for i in 0..5 {
            t.file(&format!("/r/{i}.mochi"), b"x");
        }
        let (scan, _) = ctx_run(
            &t,
            &["/r"],
            &Limits {
                archives: 3,
                ..Limits::default()
            },
        );
        let c = scan.coverage();
        assert_eq!((c.archives_found, c.limit), (3, Some(Limit::Archives)));

        let mut t = Mem::default();
        t.dir("/d");
        t.dir("/d/a");
        t.dir("/d/a/b");
        let (scan, _) = ctx_run(
            &t,
            &["/d"],
            &Limits {
                depth: 1,
                ..Limits::default()
            },
        );
        assert_eq!(scan.coverage().limit, Some(Limit::Depth));
        let (scan, _) = ctx_run(
            &t,
            &["/d"],
            &Limits {
                folders: 2,
                ..Limits::default()
            },
        );
        let c = scan.coverage();
        assert_eq!((c.folders_visited, c.limit), (2, Some(Limit::Folders)));
    }

    #[test]
    fn subfolders_off_walks_only_the_scopes_and_reports_no_limit() {
        let mut t = Mem::default();
        t.dir("/r");
        t.dir("/r/sub");
        t.file("/r/top.mochi", b"x");
        t.file("/r/sub/deep.mochi", b"x");
        let (scan, _) = ctx_run(
            &t,
            &["/r"],
            &Limits {
                subfolders: false,
                ..Limits::default()
            },
        );
        let c = scan.coverage();
        assert_eq!((c.folders_visited, c.archives_found), (1, 1));
        assert_eq!(c.limit, None, "a choice, not a bound");
        assert_eq!(scan.record(0).unwrap().path, PathBuf::from("/r/top.mochi"));
        let (scan, _) = ctx_run(&t, &["/r"], &Limits::default());
        assert_eq!(scan.coverage().archives_found, 2);
    }

    #[test]
    fn match_options_whole_name_and_case() {
        let exact = MatchOptions {
            ignore_case: true,
            whole_name: true,
        };
        let cased = MatchOptions {
            ignore_case: false,
            whole_name: false,
        };
        // The default options are `mochi find`'s.
        assert_eq!(
            MatchOptions::default(),
            MatchOptions {
                ignore_case: true,
                whole_name: false
            }
        );
        assert!(Matcher::with_options("report.pdf", exact).matches(b"a/REPORT.PDF"));
        assert!(!Matcher::with_options("report", exact).matches(b"a/report.pdf"));
        assert!(Matcher::with_options("École.txt", exact).matches("x/école.TXT".as_bytes()));
        assert!(Matcher::with_options("docs/a.txt", exact).matches(b"DOCS/A.txt"));
        assert!(!Matcher::with_options("docs/a.txt", exact).matches(b"x/docs/a.txt"));
        // A glob covers the whole name with or without the option.
        assert!(Matcher::with_options("*.pdf", exact).matches(b"a.PDF"));
        // Match case compares bytes.
        assert!(!Matcher::with_options("Report", cased).matches(b"report.pdf"));
        assert!(Matcher::with_options("Report", cased).matches(b"Q3 Report.pdf"));
        assert!(!Matcher::with_options("*.PDF", cased).matches(b"a.pdf"));
        let exact_cased = MatchOptions {
            ignore_case: false,
            whole_name: true,
        };
        assert!(Matcher::with_options("a.txt", exact_cased).matches(b"d/a.txt"));
        assert!(!Matcher::with_options("a.txt", exact_cased).matches(b"d/A.txt"));
        // Bytes that are not UTF-8 fold as ASCII.
        assert!(!Matcher::with_options("ab", exact).matches(b"x/AB\xff"));
        assert!(Matcher::with_options("ab?", exact).matches(b"x/AB\xff"));
        assert!(Matcher::with_options(
            "ab",
            MatchOptions {
                ignore_case: true,
                whole_name: false
            }
        )
        .matches(b"AB\xff"));
    }

    fn index_of(paths: &[(&str, bool)]) -> ArchiveIndex {
        let mut ix = ArchiveIndex::default();
        for (p, dir) in paths {
            assert!(ix.push(p.as_bytes(), 1, *dir));
        }
        ix
    }

    #[test]
    fn a_term_set_says_which_terms_each_entry_matched() {
        let ix = index_of(&[
            ("docs", true),
            ("docs/Invoice-1.pdf", false),
            ("docs/invoice-2.PDF", false),
            ("photos", true),
            ("photos/Holiday.jpg", false),
            ("report.txt", false),
        ]);
        let terms: Vec<String> = ["invoice-2.pdf", "holiday", "*.pdf", "missing.doc", "docs"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let opts = MatchOptions::default();
        let set = TermSet::new(&terms, opts, EntryFilter::All).unwrap();
        let m = terms_in(&ix, &set, 100);
        let got: Vec<(&str, Vec<u16>)> = m
            .entries
            .iter()
            .map(|(i, t)| {
                (
                    std::str::from_utf8(ix.get(*i).unwrap().path).unwrap(),
                    t.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("docs", vec![4]),
                ("docs/Invoice-1.pdf", vec![2]),
                ("docs/invoice-2.PDF", vec![0, 2]),
                ("photos/Holiday.jpg", vec![1]),
            ]
        );
        assert_eq!(m.count, 4, "an entry that matches two terms is one hit");
        assert_eq!(m.per_term, [1, 1, 2, 0, 1]);

        // Files only leaves the folder out; whole name finds exact names by lookup.
        let exact = MatchOptions {
            ignore_case: true,
            whole_name: true,
        };
        let set = TermSet::new(&terms, exact, EntryFilter::Files).unwrap();
        let m = terms_in(&ix, &set, 100);
        assert_eq!(m.per_term, [1, 0, 2, 0, 0]);
        let folders = TermSet::new(&terms, exact, EntryFilter::Folders).unwrap();
        assert_eq!(terms_in(&ix, &folders, 100).per_term, [0, 0, 0, 0, 1]);
        // The cap keeps the first entries; every match is still counted.
        let m = terms_in(
            &ix,
            &TermSet::new(&terms, opts, EntryFilter::All).unwrap(),
            1,
        );
        assert_eq!((m.entries.len(), m.count), (1, 4));
    }

    #[test]
    fn the_name_lookup_gives_the_same_answer_as_matching_each_term() {
        let names = [
            "a/Report.PDF",
            "b/report.pdf",
            "École.txt",
            "école.txt",
            "dir/",
            "dir/sub/Deep.TXT",
            "plain",
            "Ǆ.txt",
            "ǆ.txt",
        ];
        let mut ix = ArchiveIndex::default();
        for n in names {
            ix.push(n.as_bytes(), 0, false);
        }
        ix.push(b"x/AB\xff", 0, false);
        let terms: Vec<String> = [
            "report.pdf",
            "ÉCOLE.TXT",
            "deep.txt",
            "PLAIN",
            "ab",
            "ǅ.txt",
            "nothing",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        for ignore_case in [true, false] {
            let opts = MatchOptions {
                ignore_case,
                whole_name: true,
            };
            let set = TermSet::new(&terms, opts, EntryFilter::All).unwrap();
            let mut out = Vec::new();
            for e in ix.iter() {
                set.matching(e, &mut out);
                let linear: Vec<u16> = (0..terms.len())
                    .filter(|&i| Matcher::with_options(&terms[i], opts).matches(e.path))
                    .map(|i| u16::try_from(i).unwrap())
                    .collect();
                assert_eq!(out, linear, "{:?} ignore_case={ignore_case}", e.path);
            }
        }
    }

    #[test]
    fn a_term_set_refuses_what_is_not_a_search() {
        let o = MatchOptions::default();
        assert!(TermSet::new(&[], o, EntryFilter::All).is_err());
        assert!(TermSet::new(&["  ".to_owned()], o, EntryFilter::All).is_err());
        assert!(TermSet::new(&["x".repeat(MAX_QUERY_BYTES + 1)], o, EntryFilter::All).is_err());
        let many = vec!["a".to_owned(); MAX_TERMS + 1];
        assert!(TermSet::new(&many, o, EntryFilter::All).is_err());
        let most = vec!["a".to_owned(); MAX_TERMS];
        assert_eq!(
            TermSet::new(&most, o, EntryFilter::All).unwrap().len(),
            MAX_TERMS
        );
    }

    #[test]
    fn a_cancelled_scan_is_an_error_and_keeps_what_it_found() {
        let mut t = Mem::default();
        t.dir("/r");
        t.file("/r/a.mochi", b"x");
        let scan = Scan::new(vec![PathBuf::from("/r")]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &cancel,
        };
        let e = scan
            .run(
                &t,
                &ReadOptions::default(),
                &Limits::default(),
                &NoCache,
                &ctx,
            )
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(scan.coverage().folders_visited, 0);
    }

    #[test]
    fn skip_words_are_stable() {
        let all = [
            Skip::NotMochi,
            Skip::Encrypted,
            Skip::Unsupported {
                code: ErrorCode::UnsupportedFeature,
                message: "m".into(),
            },
            Skip::Damaged {
                code: ErrorCode::Truncated,
                message: "m".into(),
            },
            Skip::TooLarge,
            Skip::Unreadable {
                message: "m".into(),
            },
        ];
        let words: Vec<&str> = all.iter().map(Skip::reason).collect();
        assert_eq!(
            words,
            [
                "not_mochi",
                "encrypted",
                "unsupported",
                "damaged",
                "too_large",
                "unreadable"
            ]
        );
        assert_eq!(all[3].code(), Some(ErrorCode::Truncated));
        assert_eq!(all[0].message(), None);
    }
}
