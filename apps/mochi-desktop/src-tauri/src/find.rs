//! MOCHI Find: search the file names inside every `.mochi` archive under some
//! folders (plan D5 extension; spec §19.1 discovery, §23.3 #8 coverage, Annex
//! B.2.18 D28).
//!
//! # How it works
//!
//! The walk, the opening of each archive, the matching and the coverage are
//! [`mochi_core::find`], shared with `mochi find` (spec B.2.18): the two give
//! the same answers. This module adds what belongs to the window.
//!
//! * **Scopes.** The user picks folders (the shell's "Find in MOCHI archives",
//!   the desktop entry, or the in-app picker). Rust holds the paths; the window
//!   sees them as text to show and never sends one back.
//! * **Scan** (a job: [`mochi_core::job::JobContext`] progress and
//!   cancellation, forwarded over a `Channel` as [`FindEvent`]).
//!   [`Scan::run`] walks the folders for `*.mochi` files and opens each archive
//!   **read-only** ([`mochi_core::publish::open_head`], which hash-verifies the
//!   catalog image before it is opened, then a search of the head). No file
//!   content is read or hashed.
//! * **Cache.** Per-archive name lists live for the session, keyed by
//!   (path, size, modified time). A new query is answered from memory; a new
//!   scan re-reads only archives whose key changed. There is no on-disk cache.
//! * **Query** ([`Finder::query`], [`Matcher`]): a name substring
//!   (ASCII/Unicode case-insensitive, matched against the last path
//!   component), or a glob when the text has `*` or `?` (matched against the
//!   whole component). Text with a `/` is matched against the whole path
//!   inside the archive.
//! * **Coverage** (§23.3 #8). Every result carries what was searched and what
//!   was not: archives that are Encrypted (names cannot be read without the
//!   passphrase), damaged, unreadable, not MOCHI archives, in an unsupported
//!   format, or too large to hold; folders that could not be read; linked
//!   folders that were not followed; limits that stopped the walk; and whether
//!   the scan was stopped. Zero matches with any of those is **incomplete**,
//!   never "no matches" (`complete` is `false`).
//!
//! # Rules kept
//!
//! * Archives are untrusted: names cross as escaped [`NameText`]; nothing here
//!   is parsed except through `mochi_core`.
//! * Read-only: archives are opened with `OsReadStorage`, never for writing.
//! * No symbolic link or junction is followed (no loop is possible); depth,
//!   folder, archive and name counts are bounded.
//! * An Encrypted archive is never opened with a key here: Find has no
//!   passphrase, and says so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use mochi_core::find::{
    EntryFilter, Limit, MatchOptions, Outcome, Scan, ScanCache, Skip, Skipped, Stamp, TermSet,
    MAX_ENTRIES_TOTAL,
};
use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::ReadOptions;
use mochi_core::storage::os::find::{file_stamp, normalize_scopes};
use mochi_core::storage::os::OsFindTree;
use mochi_core::ErrorCode;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};
use ts_rs::TS;

use crate::browse::{hex, kind};
use crate::commands::Win;
use crate::ipc::{DialogText, EntryKindView, IpcError, NameText, OpenOutcome};
use crate::jobs::Spawner;
use crate::launch::Launches;

pub use mochi_core::find::{Limits, MAX_QUERY_BYTES, MAX_SCOPES, MAX_TERMS, PHASE};

/// Hits in one page that crosses IPC (the first page, unless asked otherwise).
/// Every match can be listed: the window asks for the next page as it
/// scrolls (B.2.21), and a retrieve can take every match without listing them.
pub const MAX_HITS_SHOWN: usize = 1_000;
/// The most hits one page may ask for.
pub const MAX_PAGE: usize = 10_000;
/// Archives listed by name in the coverage; the rest are counted.
pub const MAX_SKIPPED_SHOWN: usize = 200;
/// Fewest milliseconds between two progress events (the last is always sent).
const PROGRESS_EVERY: Duration = Duration::from_millis(120);

fn invalid(what: impl Into<String>) -> IpcError {
    IpcError {
        code: ErrorCode::InvalidArgument.as_str().to_owned(),
        message: what.into(),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Only plain data lives behind these locks; a panic elsewhere leaves it usable.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ---- the cache and one run -------------------------------------------------

#[derive(Default)]
struct CacheInner {
    map: HashMap<PathBuf, (Stamp, Outcome)>,
    entries: usize,
}

/// Name lists kept for the session, keyed by path and checked against size and
/// modified time. Failures that may be temporary (`Unreadable`) are not kept.
#[derive(Default)]
pub struct Cache {
    inner: Mutex<CacheInner>,
}

impl ScanCache for Cache {
    fn get(&self, path: &Path, stamp: &Stamp) -> Option<Outcome> {
        let c = lock(&self.inner);
        match c.map.get(path) {
            Some((s, o)) if s == stamp => Some(o.clone()),
            _ => None,
        }
    }

    fn put(&self, path: &Path, stamp: Stamp, outcome: &Outcome) {
        let n = match outcome {
            Outcome::Indexed(ix) => ix.len(),
            Outcome::Skipped(Skip::Unreadable { .. }) => return,
            Outcome::Skipped(_) => 0,
        };
        let mut c = lock(&self.inner);
        if let Some((_, Outcome::Indexed(old))) = c.map.remove(path) {
            c.entries = c.entries.saturating_sub(old.len());
        }
        // A cache is only a cache: when it would outgrow the budget it starts over.
        if c.entries.saturating_add(n) > MAX_ENTRIES_TOTAL {
            c.map.clear();
            c.entries = 0;
        }
        c.entries += n;
        c.map.insert(path.to_path_buf(), (stamp, outcome.clone()));
    }
}

impl Cache {
    /// Forget everything (the "Scan again" button).
    pub fn clear(&self) {
        let mut c = lock(&self.inner);
        c.map.clear();
        c.entries = 0;
    }

    /// Archives held.
    pub fn len(&self) -> usize {
        lock(&self.inner).map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A limit that stopped the walk early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum FindLimit {
    Depth,
    Folders,
    Archives,
}

impl From<Limit> for FindLimit {
    fn from(l: Limit) -> Self {
        match l {
            Limit::Depth => FindLimit::Depth,
            Limit::Folders => FindLimit::Folders,
            Limit::Archives => FindLimit::Archives,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum FindState {
    /// No scan has run.
    Idle,
    Running,
    Complete,
    /// Stopped by the user: what was found so far is searchable, and incomplete.
    Cancelled,
    Failed,
}

struct Status {
    state: FindState,
    failure: Option<String>,
}

/// One scan and what it found. The window refers to it by `id`.
pub struct RunState {
    id: u32,
    scan: Scan,
    /// Whether the scopes' subfolders were walked (the user's choice).
    subfolders: bool,
    status: Mutex<Status>,
}

impl RunState {
    fn new(id: u32, scopes: Vec<PathBuf>, subfolders: bool) -> Self {
        RunState {
            id,
            scan: Scan::new(scopes),
            subfolders,
            status: Mutex::new(Status {
                state: FindState::Running,
                failure: None,
            }),
        }
    }
}

// ---- what crosses IPC ------------------------------------------------------

/// A folder to search, as text to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindScopeView {
    pub index: u32,
    pub path: String,
}

/// One matching entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindHit {
    /// Which scan, and which archive of it: what `find_open_hit` takes.
    pub run: u32,
    pub archive: u32,
    /// The archive file on this computer, as text to show.
    pub archive_path: String,
    /// The archive file's modified time, milliseconds since 1970 (the archive's
    /// catalog records no per-entry time that Find reads).
    #[ts(type = "number | null")]
    pub archive_modified_ms: Option<i64>,
    /// The entry's full path inside the archive (archive-supplied: escaped text).
    pub path: NameText,
    /// The numbers of the names asked for that this entry matched (ascending;
    /// `[0]` for a single name).
    pub terms: Vec<u32>,
    /// The path's exact stored bytes, hex: for opening the entry's folder.
    pub key: String,
    pub kind: EntryKindView,
    /// Decimal string; `"0"` for a folder.
    pub size: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum FindSkipReason {
    NotMochi,
    Encrypted,
    Unsupported,
    Damaged,
    TooLarge,
    Unreadable,
}

/// An archive that could not be searched, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindSkippedView {
    pub archive: u32,
    pub path: String,
    pub reason: FindSkipReason,
    /// The stable error code, when `mochi_core` gave one.
    pub code: Option<String>,
    /// `mochi_core`'s sentence (it can quote archive content: escaped text).
    pub message: Option<NameText>,
}

/// What a scan covered and what it did not (spec §19.3, §23.3 #8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindCoverageView {
    pub run: u32,
    pub state: FindState,
    /// `true` only when the scan finished, every archive found was searched,
    /// every folder was read, nothing was left unfollowed, and no limit was hit:
    /// the only state in which "no matches" means none.
    pub complete: bool,
    #[ts(type = "number")]
    pub folders_visited: u64,
    #[ts(type = "number")]
    pub folders_unreadable: u64,
    /// Linked folders or archives that were not followed.
    #[ts(type = "number")]
    pub links_not_followed: u64,
    pub limit: Option<FindLimit>,
    #[ts(type = "number")]
    pub archives_found: u64,
    /// Archives whose names were searched.
    #[ts(type = "number")]
    pub archives_searched: u64,
    /// Archives found but not yet opened (a scan that is running or was stopped).
    #[ts(type = "number")]
    pub archives_pending: u64,
    /// Of `archives_searched`, how many came from the session's cache.
    #[ts(type = "number")]
    pub archives_from_cache: u64,
    #[ts(type = "number")]
    pub archives_skipped: u64,
    /// The first [`MAX_SKIPPED_SHOWN`] of them.
    pub skipped: Vec<FindSkippedView>,
    #[ts(type = "number")]
    pub names_searched: u64,
    pub scopes: Vec<String>,
    /// Whether the folders' subfolders were searched. `false` is the user's
    /// choice ("Search subfolders" off): only the folders themselves were
    /// walked, and that alone does not make the search incomplete.
    pub subfolders: bool,
    /// A failure that ended the scan, if one did.
    pub failure: Option<String>,
    /// Always `true`: only names are searched, never file content (§19.2).
    pub content_not_searched: bool,
}

/// Which entries a search may match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum FindEntryFilter {
    #[default]
    All,
    Files,
    Folders,
}

impl From<FindEntryFilter> for EntryFilter {
    fn from(f: FindEntryFilter) -> Self {
        match f {
            FindEntryFilter::All => EntryFilter::All,
            FindEntryFilter::Files => EntryFilter::Files,
            FindEntryFilter::Folders => EntryFilter::Folders,
        }
    }
}

/// A search for one or more names at once (spec Annex B.2.21): the names the
/// user typed or pasted, already split, and the options. The defaults are
/// `mochi find`'s: case ignored, part of a name, files and folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindQuery {
    /// 1 to [`MAX_TERMS`] names, each at most [`MAX_QUERY_BYTES`] bytes.
    pub terms: Vec<String>,
    #[serde(default)]
    pub match_case: bool,
    #[serde(default)]
    pub whole_name: bool,
    #[serde(default)]
    pub entries: FindEntryFilter,
    /// List only the matches of this name (its number in `terms`).
    #[serde(default)]
    #[ts(optional)]
    pub term: Option<u32>,
    /// The first match listed (0 by default): pages of the same query.
    #[serde(default)]
    #[ts(optional, type = "number")]
    pub offset: Option<u64>,
    /// Matches listed (default [`MAX_HITS_SHOWN`], at most [`MAX_PAGE`]).
    #[serde(default)]
    #[ts(optional)]
    pub limit: Option<u32>,
}

impl FindQuery {
    /// The same search, without the page or the one-name view: what is cached.
    fn search(&self) -> (Vec<String>, bool, bool, FindEntryFilter) {
        let texts = self
            .terms
            .iter()
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        (texts, self.match_case, self.whole_name, self.entries)
    }
}

/// One name asked for, and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindTermView {
    pub index: u32,
    /// The user's own text, trimmed.
    pub text: String,
    /// Entries it matched, shown or not.
    #[ts(type = "number")]
    pub hits: u64,
    /// Archives holding at least one of them.
    #[ts(type = "number")]
    pub archives: u64,
    /// The sizes of the files it matched, added up.
    #[ts(type = "number")]
    pub bytes: u64,
}

/// A page of a query's hits, what each name found, and the coverage they were
/// drawn from. Every match is reachable: `listed` says how many there are to page through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FindResultView {
    pub hits: Vec<FindHit>,
    /// All matching entries, shown or not (an entry matching two names is one).
    #[ts(type = "number")]
    pub total_hits: u64,
    /// Matches in the list being paged (`total_hits`, or one name's matches).
    #[ts(type = "number")]
    pub listed: u64,
    /// The position of `hits[0]` in that list.
    #[ts(type = "number")]
    pub offset: u64,
    /// The sizes of every matching file, added up.
    #[ts(type = "number")]
    pub total_bytes: u64,
    /// Archives holding at least one match.
    #[ts(type = "number")]
    pub archives_with_hits: u64,
    /// One per name asked for, in order. Empty when nothing was asked.
    pub terms: Vec<FindTermView>,
    pub coverage: FindCoverageView,
}

/// What the scan job tells the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum FindEvent {
    Started {
        run: u32,
    },
    /// Archives finished and, once the walk is over, the number found. The
    /// window then asks `find_query` again; the numbers are not the coverage.
    Progress {
        run: u32,
        #[ts(type = "number")]
        completed: u64,
        #[ts(type = "number | null")]
        total: Option<u64>,
    },
    Finished {
        run: u32,
        state: FindState,
    },
}

/// Where scan events go (a Tauri `Channel` in the app).
pub trait FindSink: Send + Sync + 'static {
    /// `false` once nobody listens (the scan is then cancelled).
    fn send(&self, event: FindEvent) -> bool;
}

impl FindSink for Channel<FindEvent> {
    fn send(&self, event: FindEvent) -> bool {
        Channel::send(self, event).is_ok()
    }
}

/// Forwards `mochi_core` progress as [`FindEvent::Progress`], at most one per
/// [`PROGRESS_EVERY`] (the last always goes).
struct Forward<'a> {
    run: u32,
    sink: &'a dyn FindSink,
    token: &'a CancellationToken,
    last: Mutex<Option<Instant>>,
}

impl ProgressSink for Forward<'_> {
    fn report(&self, event: &ProgressEvent) {
        let last_of_all = event.total == Some(event.completed);
        {
            let mut last = lock(&self.last);
            let now = Instant::now();
            if !last_of_all && last.is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY) {
                return;
            }
            *last = Some(now);
        }
        if !self.sink.send(FindEvent::Progress {
            run: self.run,
            completed: event.completed,
            total: event.total,
        }) {
            self.token.cancel();
        }
    }
}

// ---- the finder -------------------------------------------------------------

#[derive(Default)]
struct FinderInner {
    scopes: Vec<PathBuf>,
    run: Option<Arc<RunState>>,
    token: Option<CancellationToken>,
    last_run: u32,
}

/// One match of a query: the archive (an index into `Matches::found`), the
/// entry, and the names it matched.
struct Match {
    archive: usize,
    entry: usize,
    terms: Box<[u16]>,
}

/// Every match of the last query, kept so that its pages and a retrieve of
/// all of it do not match again. Valid for one scan in one state.
struct Matches {
    run: u32,
    /// Archives searched so far: a running scan adds more.
    searched: u64,
    search: (Vec<String>, bool, bool, FindEntryFilter),
    found: Vec<mochi_core::find::TermMatches>,
    all: Vec<Match>,
    terms: Vec<FindTermView>,
    bytes: u64,
}

/// Scope folders, the session's cache, and the latest scan.
#[derive(Default)]
pub struct Finder {
    inner: Mutex<FinderInner>,
    cache: Arc<Cache>,
    limits: Mutex<Option<Limits>>,
    last: Mutex<Option<Arc<Matches>>>,
}

fn scope_views(scopes: &[PathBuf]) -> Vec<FindScopeView> {
    scopes
        .iter()
        .enumerate()
        .map(|(i, p)| FindScopeView {
            index: u32::try_from(i).unwrap_or(u32::MAX),
            path: p.display().to_string(),
        })
        .collect()
}

/// An entry of a scanned archive, as the window asks to be shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundEntry {
    /// The exact stored path, hex.
    pub key: String,
    pub path: NameText,
    pub kind: EntryKindView,
}

impl Finder {
    /// A finder that shares `cache` with others (one per window).
    pub fn new(cache: Arc<Cache>) -> Finder {
        Finder {
            cache,
            ..Finder::default()
        }
    }

    /// A finder with other bounds (tests).
    pub fn with_limits(limits: Limits) -> Finder {
        Finder {
            limits: Mutex::new(Some(limits)),
            ..Finder::default()
        }
    }

    fn limits(&self) -> Limits {
        lock(&self.limits).clone().unwrap_or_default()
    }

    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    /// The entry `key` (a hit's `key`) of archive `archive` of scan `run`,
    /// checked against the names the scan read.
    pub fn entry_of(&self, run: u32, archive: u32, key: &str) -> Result<FoundEntry, IpcError> {
        let stored = crate::browse::parse_key(key)
            .map_err(IpcError::from)?
            .as_stored()
            .to_vec();
        let current = self
            .current()
            .filter(|r| r.id == run)
            .ok_or_else(|| invalid("that result belongs to an earlier scan; search again"))?;
        let index = match current.scan.record(archive as usize).map(|r| r.outcome) {
            Some(Some(Outcome::Indexed(ix))) => ix,
            _ => return Err(invalid("that archive is not in the scan")),
        };
        let entry = index
            .find(&stored)
            .ok_or_else(|| invalid("that entry is not in the archive"))?;
        Ok(FoundEntry {
            key: hex(&stored),
            path: NameText::of(&stored),
            kind: kind(entry.kind),
        })
    }

    pub fn scopes(&self) -> Vec<FindScopeView> {
        scope_views(&lock(&self.inner).scopes)
    }

    /// Replace the folders (what a shell launch does). Paths that are not
    /// existing absolute folders are left out; an error if none is left.
    pub fn set_scopes(&self, paths: Vec<PathBuf>) -> Result<Vec<FindScopeView>, IpcError> {
        let scopes = normalize_scopes(&paths, &[]);
        if scopes.is_empty() {
            return Err(invalid("none of the given paths is a folder"));
        }
        let mut st = lock(&self.inner);
        st.scopes = scopes.into_iter().take(MAX_SCOPES).collect();
        Ok(scope_views(&st.scopes))
    }

    /// Add folders to the list. Duplicates and folders inside one already
    /// listed are left out; a listed folder that lies inside a new one is replaced.
    pub fn add_scopes(&self, paths: Vec<PathBuf>) -> Result<Vec<FindScopeView>, IpcError> {
        let mut st = lock(&self.inner);
        let merged = normalize_scopes(&paths, &st.scopes);
        if merged.len() > MAX_SCOPES {
            return Err(invalid(format!(
                "at most {MAX_SCOPES} folders can be searched at once"
            )));
        }
        st.scopes = merged;
        Ok(scope_views(&st.scopes))
    }

    pub fn remove_scope(&self, index: usize) -> Vec<FindScopeView> {
        let mut st = lock(&self.inner);
        if index < st.scopes.len() {
            st.scopes.remove(index);
        }
        scope_views(&st.scopes)
    }

    /// Start a scan of the current folders. A scan still running is cancelled
    /// first. `fresh` forgets the cache. The work runs through `spawn`; events
    /// go to `sink`: `started`, progress, then exactly one `finished`.
    pub fn start(
        self: &Arc<Self>,
        fresh: bool,
        sink: Arc<dyn FindSink>,
        spawn: Box<Spawner>,
    ) -> Result<u32, IpcError> {
        self.start_with(fresh, true, sink, spawn)
    }

    /// [`Finder::start`], walking the folders' subfolders or not (`false`:
    /// only the archives directly in the chosen folders).
    pub fn start_with(
        self: &Arc<Self>,
        fresh: bool,
        subfolders: bool,
        sink: Arc<dyn FindSink>,
        spawn: Box<Spawner>,
    ) -> Result<u32, IpcError> {
        let (run, token) = {
            let mut st = lock(&self.inner);
            if st.scopes.is_empty() {
                return Err(invalid("choose at least one folder to search"));
            }
            let id = st.last_run.checked_add(1).ok_or_else(|| IpcError {
                code: ErrorCode::LimitExceeded.as_str().to_owned(),
                message: "this process has run the maximum number of scans; restart the app".into(),
            })?;
            st.last_run = id;
            if let Some(old) = st.token.take() {
                old.cancel();
            }
            let run = Arc::new(RunState::new(id, st.scopes.clone(), subfolders));
            let token = CancellationToken::new();
            st.run = Some(Arc::clone(&run));
            st.token = Some(token.clone());
            (run, token)
        };
        if fresh {
            self.cache.clear();
        }
        let id = run.id;
        let finder = Arc::clone(self);
        spawn(Box::new(move || finder.run_scan(&run, &token, &sink)));
        Ok(id)
    }

    fn run_scan(&self, run: &Arc<RunState>, token: &CancellationToken, sink: &Arc<dyn FindSink>) {
        struct Finish<'a> {
            run: &'a RunState,
            sink: &'a dyn FindSink,
            state: Option<FindState>,
        }
        impl Drop for Finish<'_> {
            fn drop(&mut self) {
                let state = self.state.take().unwrap_or(FindState::Failed);
                {
                    let mut status = lock(&self.run.status);
                    status.state = state.clone();
                    if state == FindState::Failed && status.failure.is_none() {
                        status.failure = Some("the scan stopped unexpectedly".to_owned());
                    }
                }
                self.sink.send(FindEvent::Finished {
                    run: self.run.id,
                    state,
                });
            }
        }
        let mut finish = Finish {
            run,
            sink: sink.as_ref(),
            state: None,
        };
        if !sink.send(FindEvent::Started { run: run.id }) {
            token.cancel();
        }
        let progress = Forward {
            run: run.id,
            sink: sink.as_ref(),
            token,
            last: Mutex::new(None),
        };
        let ctx = JobContext {
            progress: &progress,
            cancel: token,
        };
        let limits = Limits {
            subfolders: run.subfolders,
            ..self.limits()
        };
        let result = run.scan.run(
            &OsFindTree,
            &ReadOptions::default(),
            &limits,
            self.cache.as_ref(),
            &ctx,
        );
        finish.state = Some(match result {
            Ok(()) => FindState::Complete,
            Err(e) if e.code == ErrorCode::Cancelled => FindState::Cancelled,
            Err(e) => {
                lock(&run.status).failure = Some(e.message);
                FindState::Failed
            }
        });
        drop(finish);
        let mut st = lock(&self.inner);
        if st.run.as_ref().is_some_and(|r| r.id == run.id) {
            st.token = None;
        }
    }

    /// Stop the running scan. `false` if none is running.
    pub fn cancel(&self) -> bool {
        match &lock(&self.inner).token {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    fn current(&self) -> Option<Arc<RunState>> {
        lock(&self.inner).run.clone()
    }

    /// The coverage of the latest scan (idle if none has run).
    pub fn coverage(&self) -> FindCoverageView {
        match self.current() {
            Some(run) => coverage_of(&run),
            None => FindCoverageView {
                run: 0,
                state: FindState::Idle,
                complete: false,
                folders_visited: 0,
                folders_unreadable: 0,
                links_not_followed: 0,
                limit: None,
                archives_found: 0,
                archives_searched: 0,
                archives_pending: 0,
                archives_from_cache: 0,
                archives_skipped: 0,
                skipped: Vec::new(),
                names_searched: 0,
                scopes: scope_views(&lock(&self.inner).scopes)
                    .into_iter()
                    .map(|s| s.path)
                    .collect(),
                subfolders: true,
                failure: None,
                content_not_searched: true,
            },
        }
    }

    /// Entries of the latest scan whose names match `text`: the first page.
    /// Blocking: call it off the async runtime. Empty text matches nothing
    /// here (the window shows only the coverage); it is not a listing.
    /// Exactly `mochi find`'s rules.
    pub fn query(&self, text: &str) -> Result<FindResultView, IpcError> {
        if text.len() > MAX_QUERY_BYTES {
            return Err(invalid(format!(
                "a search takes at most {MAX_QUERY_BYTES} bytes of text"
            )));
        }
        let terms = if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![text.to_owned()]
        };
        self.query_many(&FindQuery {
            terms,
            match_case: false,
            whole_name: false,
            entries: FindEntryFilter::All,
            term: None,
            offset: None,
            limit: None,
        })
    }

    /// Every match of `q` in the latest scan (cached for the next page).
    /// `None` when nothing is asked or no scan ran.
    fn matches(&self, q: &FindQuery) -> Result<Option<Arc<Matches>>, IpcError> {
        if q.terms.len() > MAX_TERMS {
            return Err(invalid(format!(
                "a search takes at most {MAX_TERMS} names at once"
            )));
        }
        let search = q.search();
        let Some(run) = self.current() else {
            return Ok(None);
        };
        if search.0.is_empty() {
            return Ok(None);
        }
        let c = run.scan.coverage();
        let searched = c.archives_searched + c.skipped.len() as u64;
        if let Some(m) = lock(&self.last).as_ref() {
            if m.run == run.id && m.searched == searched && m.search == search {
                return Ok(Some(Arc::clone(m)));
            }
        }
        let options = MatchOptions {
            ignore_case: !search.1,
            whole_name: search.2,
        };
        let set = TermSet::new(&search.0, options, search.3.into()).map_err(IpcError::from)?;
        let found = run.scan.query_terms(&set, usize::MAX);
        let mut terms: Vec<FindTermView> = search
            .0
            .iter()
            .enumerate()
            .map(|(i, text)| FindTermView {
                index: u32::try_from(i).unwrap_or(u32::MAX),
                text: text.clone(),
                hits: 0,
                archives: 0,
                bytes: 0,
            })
            .collect();
        let mut all = Vec::new();
        let mut bytes = 0u64;
        for (a, m) in found.iter().enumerate() {
            for (t, &n) in m.per_term.iter().enumerate() {
                if let Some(v) = terms.get_mut(t) {
                    v.hits = v.hits.saturating_add(n);
                    v.archives += u64::from(n > 0);
                }
            }
            for (e, ts) in &m.entries {
                let size = m.names.get(*e).map_or(0, |x| x.size);
                bytes = bytes.saturating_add(size);
                for &t in ts {
                    if let Some(v) = terms.get_mut(usize::from(t)) {
                        v.bytes = v.bytes.saturating_add(size);
                    }
                }
                all.push(Match {
                    archive: a,
                    entry: *e,
                    terms: ts.clone().into_boxed_slice(),
                });
            }
        }
        let m = Arc::new(Matches {
            run: run.id,
            searched,
            search,
            found,
            all,
            terms,
            bytes,
        });
        *lock(&self.last) = Some(Arc::clone(&m));
        Ok(Some(m))
    }

    /// A page of the entries of the latest scan that match any of `q.terms`
    /// with `q`'s options (B.2.21), with what each name found. Blocking. No
    /// terms (or only blank ones) matches nothing; it is not a listing.
    pub fn query_many(&self, q: &FindQuery) -> Result<FindResultView, IpcError> {
        let coverage = self.coverage();
        let Some(m) = self.matches(q)? else {
            return Ok(FindResultView {
                hits: Vec::new(),
                total_hits: 0,
                listed: 0,
                offset: 0,
                total_bytes: 0,
                archives_with_hits: 0,
                terms: Vec::new(),
                coverage,
            });
        };
        let only = q.term.and_then(|t| u16::try_from(t).ok());
        if q.term.is_some_and(|t| t as usize >= m.terms.len()) {
            return Err(invalid("that name is not in the search"));
        }
        let listed: Vec<&Match> = m
            .all
            .iter()
            .filter(|x| only.is_none_or(|t| x.terms.contains(&t)))
            .collect();
        let offset = usize::try_from(q.offset.unwrap_or(0)).unwrap_or(usize::MAX);
        let limit = q
            .limit
            .map_or(MAX_HITS_SHOWN, |l| l as usize)
            .clamp(1, MAX_PAGE);
        let hits = listed
            .iter()
            .skip(offset)
            .take(limit)
            .filter_map(|x| {
                let a = m.found.get(x.archive)?;
                let entry = a.names.get(x.entry)?;
                Some(FindHit {
                    run: m.run,
                    archive: u32::try_from(a.index).unwrap_or(u32::MAX),
                    archive_path: a.path.display().to_string(),
                    archive_modified_ms: a.stamp.modified_ms(),
                    path: NameText::of(entry.path),
                    terms: x.terms.iter().map(|&t| u32::from(t)).collect(),
                    key: hex(entry.path),
                    kind: kind(entry.kind),
                    size: entry.size.to_string(),
                })
            })
            .collect();
        Ok(FindResultView {
            hits,
            total_hits: m.all.len() as u64,
            listed: listed.len() as u64,
            offset: offset.min(listed.len()) as u64,
            total_bytes: m.bytes,
            archives_with_hits: m.found.len() as u64,
            terms: m.terms.clone(),
            coverage,
        })
    }

    /// Every match of `q` (or of its one name, `q.term`) in scan `run`, by
    /// archive: what a retrieve of all matches takes (B.2.21).
    pub fn all_matches(
        &self,
        run: u32,
        q: &FindQuery,
    ) -> Result<Vec<(u32, Vec<mochi_core::catalog::path::ArchivePath>)>, IpcError> {
        if self.current_run() != Some(run) {
            return Err(invalid(
                "those results belong to an earlier scan; search again",
            ));
        }
        let Some(m) = self.matches(q)? else {
            return Ok(Vec::new());
        };
        let only = q.term.and_then(|t| u16::try_from(t).ok());
        let mut out: Vec<(u32, Vec<mochi_core::catalog::path::ArchivePath>)> = Vec::new();
        for x in m
            .all
            .iter()
            .filter(|x| only.is_none_or(|t| x.terms.contains(&t)))
        {
            let Some(a) = m.found.get(x.archive) else {
                continue;
            };
            let Some(e) = a.names.get(x.entry) else {
                continue;
            };
            let path = mochi_core::catalog::path::ArchivePath::from_stored(e.path)
                .map_err(IpcError::from)?;
            let n = u32::try_from(a.index).unwrap_or(u32::MAX);
            match out.last_mut() {
                Some((last, paths)) if *last == n => paths.push(path),
                _ => out.push((n, vec![path])),
            }
        }
        Ok(out)
    }

    /// The entries `keys` (hits' keys) of archive `archive` of scan `run`,
    /// each checked against the names the scan read (one lookup each).
    pub fn paths_in(
        &self,
        run: u32,
        archive: u32,
        keys: &[&str],
    ) -> Result<Vec<mochi_core::catalog::path::ArchivePath>, IpcError> {
        let current = self
            .current()
            .filter(|r| r.id == run)
            .ok_or_else(|| invalid("that result belongs to an earlier scan; search again"))?;
        let index = match current.scan.record(archive as usize).map(|r| r.outcome) {
            Some(Some(Outcome::Indexed(ix))) => ix,
            _ => return Err(invalid("that archive is not in the scan")),
        };
        let known: std::collections::HashSet<&[u8]> = index.iter().map(|e| e.path).collect();
        keys.iter()
            .map(|k| {
                let p = crate::browse::parse_key(k).map_err(IpcError::from)?;
                if !known.contains(p.as_stored()) {
                    return Err(invalid("that entry is not in the archive"));
                }
                Ok(p)
            })
            .collect()
    }

    /// The archive of a hit, if it is still the one that was scanned: the same
    /// scan, and the file's size and modified time unchanged.
    pub fn archive_for_open(&self, run: u32, archive: u32) -> Result<PathBuf, IpcError> {
        let (path, stamp) = self.record(run, archive)?;
        match file_stamp(&path) {
            Ok(Some(s)) if s == stamp => Ok(path),
            Ok(_) => Err(invalid(
                "this archive changed after it was scanned; scan again to find its current names",
            )),
            Err(e) => Err(IpcError {
                code: ErrorCode::IoError.as_str().to_owned(),
                message: format!("this archive cannot be read any more: {e}"),
            }),
        }
    }

    /// The archive file of a hit, as scanned (whether or not it changed since).
    pub fn archive_path(&self, run: u32, archive: u32) -> Result<PathBuf, IpcError> {
        self.record(run, archive).map(|(path, _)| path)
    }

    /// The number of the latest scan, if one ran.
    pub fn current_run(&self) -> Option<u32> {
        self.current().map(|r| r.id)
    }

    /// The folder that holds the archive of a hit.
    pub fn archive_folder(&self, run: u32, archive: u32) -> Result<PathBuf, IpcError> {
        let (path, _) = self.record(run, archive)?;
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| invalid("this archive's folder is not known"))
    }

    fn record(&self, run: u32, archive: u32) -> Result<(PathBuf, Stamp), IpcError> {
        let current = self
            .current()
            .filter(|r| r.id == run)
            .ok_or_else(|| invalid("that result belongs to an earlier scan; search again"))?;
        current
            .scan
            .record(archive as usize)
            .map(|r| (r.path, r.stamp))
            .ok_or_else(|| invalid("that archive is not in the scan"))
    }
}

fn skipped_view(s: &Skipped) -> FindSkippedView {
    let (reason, message) = match &s.skip {
        Skip::NotMochi => (FindSkipReason::NotMochi, None),
        Skip::Encrypted => (FindSkipReason::Encrypted, None),
        Skip::Unsupported { .. } => (FindSkipReason::Unsupported, None),
        Skip::Damaged { message, .. } => (
            FindSkipReason::Damaged,
            Some(NameText::of(message.as_bytes())),
        ),
        Skip::TooLarge => (FindSkipReason::TooLarge, None),
        Skip::Unreadable { message } => (
            FindSkipReason::Unreadable,
            Some(NameText::of(message.as_bytes())),
        ),
    };
    FindSkippedView {
        archive: u32::try_from(s.index).unwrap_or(u32::MAX),
        path: s.path.display().to_string(),
        reason,
        code: s.skip.code().map(|c| c.as_str().to_owned()),
        message,
    }
}

fn coverage_of(run: &RunState) -> FindCoverageView {
    let c = run.scan.coverage();
    let (state, failure) = {
        let status = lock(&run.status);
        (status.state.clone(), status.failure.clone())
    };
    FindCoverageView {
        run: run.id,
        complete: state == FindState::Complete && c.nothing_left_out(),
        state,
        folders_visited: c.folders_visited,
        folders_unreadable: c.folders_unreadable,
        links_not_followed: c.links_not_followed,
        limit: c.limit.map(FindLimit::from),
        archives_found: c.archives_found,
        archives_searched: c.archives_searched,
        archives_pending: c.archives_pending,
        archives_from_cache: c.archives_from_cache,
        archives_skipped: c.skipped.len() as u64,
        skipped: c
            .skipped
            .iter()
            .take(MAX_SKIPPED_SHOWN)
            .map(skipped_view)
            .collect(),
        names_searched: c.names_searched,
        scopes: run
            .scan
            .scopes()
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        subfolders: run.subfolders,
        failure,
        content_not_searched: true,
    }
}

// ---- commands ----------------------------------------------------------------

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, IpcError> + Send + 'static,
) -> Result<T, IpcError> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|_| IpcError {
            code: ErrorCode::Cancelled.as_str().to_owned(),
            message: "the operation stopped before it finished".to_owned(),
        })?
}

/// The folders to search (this window's own).
#[tauri::command]
pub fn find_scopes(win: Win) -> Vec<FindScopeView> {
    win.state.finder.scopes()
}

/// Ask for folders in the native dialog (in Rust) and add them. `null` if the
/// user cancelled.
#[tauri::command]
pub async fn find_choose_folders(
    app: AppHandle,
    text: Option<DialogText>,
    win: Win,
) -> Result<Option<Vec<FindScopeView>>, IpcError> {
    let finder = Arc::clone(&win.state.finder);
    let text = text.unwrap_or_default();
    blocking(move || match crate::picker::pick_folders(&app, &text) {
        None => Ok(None),
        Some(paths) => finder.add_scopes(paths).map(Some),
    })
    .await
}

/// Take a folder off the list.
#[tauri::command]
pub fn find_remove_scope(index: u32, win: Win) -> Vec<FindScopeView> {
    win.state.finder.remove_scope(index as usize)
}

/// Scan the folders. Returns the scan's number at once; events stream over
/// `on_event`. `fresh` forgets what the session cached. `subfolders`
/// (default `true`) walks the folders' subfolders too.
#[tauri::command]
pub async fn find_start(
    fresh: bool,
    subfolders: Option<bool>,
    on_event: Channel<FindEvent>,
    win: Win,
) -> Result<u32, IpcError> {
    win.state.finder.start_with(
        fresh,
        subfolders.unwrap_or(true),
        Arc::new(on_event),
        Box::new(|work| {
            tauri::async_runtime::spawn_blocking(work);
        }),
    )
}

/// Stop the scan. `false` if none is running.
#[tauri::command]
pub fn find_cancel(win: Win) -> bool {
    win.state.finder.cancel()
}

/// Names matching `text` in the latest scan, with its coverage.
#[tauri::command]
pub async fn find_query(text: String, win: Win) -> Result<FindResultView, IpcError> {
    let finder = Arc::clone(&win.state.finder);
    blocking(move || finder.query(&text)).await
}

/// Entries matching any of several names with the options chosen (B.2.21),
/// with what each name found and the coverage.
#[tauri::command]
pub async fn find_query_many(query: FindQuery, win: Win) -> Result<FindResultView, IpcError> {
    let finder = Arc::clone(&win.state.finder);
    blocking(move || finder.query_many(&query)).await
}

/// Ask for the folder to retrieve found files into (the native dialog, in
/// Rust); it becomes this window's extract folder. `None` if cancelled.
#[tauri::command]
pub async fn find_choose_retrieve_folder(
    app: AppHandle,
    text: Option<DialogText>,
    win: Win,
) -> Result<Option<String>, IpcError> {
    let destination = Arc::clone(&win.state.destination);
    let text = text.unwrap_or_default();
    blocking(move || {
        Ok(
            crate::picker::pick_extract_folder(&app, &text).map(|folder| {
                let shown = folder.display().to_string();
                destination.set(folder);
                shown
            }),
        )
    })
    .await
}

/// Open the archive of a hit (read-only) and show the entry `key` in it. The
/// archive opens where archives open (`Launches::open_hit_in_window`): in this
/// window if it is empty, in the window that already has it, else in a new
/// one. The answer is `Opened` for this window to show, or `Elsewhere`. The
/// window names no path.
#[tauri::command]
pub async fn find_open_hit(
    run: u32,
    archive: u32,
    key: String,
    win: Win,
    launches: State<'_, Arc<Launches>>,
) -> Result<OpenOutcome, IpcError> {
    let finder = Arc::clone(&win.state.finder);
    let launches = Arc::clone(&launches);
    let label = win.label.clone();
    blocking(move || {
        let path = finder.archive_for_open(run, archive)?;
        let entry = finder.entry_of(run, archive, &key)?;
        launches.open_hit_in_window(&label, &path, entry)
    })
    .await
}

/// Show the folder that holds the archive of a hit.
#[tauri::command]
pub async fn find_show_folder(run: u32, archive: u32, win: Win) -> Result<(), IpcError> {
    let finder = Arc::clone(&win.state.finder);
    blocking(move || crate::help::open_folder(&finder.archive_folder(run, archive)?)).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::SystemTime;

    use crate::browse::Archives;
    use mochi_core::find::Matcher;
    use mochi_core::manifest::Attributes;
    use mochi_core::publish::{ArchiveWriter, Transaction};
    use mochi_core::storage::os::OsStorage;
    use mochi_testkit::archive::{encrypted_options, path as apath, test_options, Job};
    use mochi_testkit::SeqIds;

    use super::*;

    /// Records events; `listening = false` simulates a closed window.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<FindEvent>>,
        deaf: AtomicBool,
    }

    impl FindSink for Recorder {
        fn send(&self, event: FindEvent) -> bool {
            lock(&self.events).push(event);
            !self.deaf.load(Ordering::SeqCst)
        }
    }

    static SEED: AtomicU32 = AtomicU32::new(100);

    fn write_archive(
        path: &Path,
        files: &[&str],
        opts: Option<mochi_core::publish::WriterOptions>,
    ) {
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(path).unwrap(),
            Box::new(SeqIds::new(u64::from(SEED.fetch_add(1, Ordering::SeqCst)))),
            opts.unwrap_or_else(test_options),
        )
        .unwrap();
        let mut tx = Transaction::new();
        for f in files {
            // Directories are explicit entries (spec §10.2): write each parent.
            let mut dir = String::new();
            for part in f.split('/').take(f.matches('/').count()) {
                if !dir.is_empty() {
                    dir.push('/');
                }
                dir.push_str(part);
                tx.put_dir(apath(&dir), Attributes::default());
            }
            tx.put_file(apath(f), f.as_bytes().to_vec(), Attributes::default());
        }
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
    }

    /// root/{a.mochi, deep/er/b.mochi, enc/secret.mochi (encrypted), bad.mochi
    /// (truncated), fake.mochi (not an archive), note.txt}
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("deep/er")).unwrap();
        std::fs::create_dir_all(r.join("enc")).unwrap();
        write_archive(
            &r.join("a.mochi"),
            &["Report.pdf", "docs/invoice.txt"],
            None,
        );
        write_archive(
            &r.join("deep/er/b.mochi"),
            &["photos/Holiday.JPG", "report-2.txt"],
            None,
        );
        write_archive(
            &r.join("enc/secret.mochi"),
            &["hidden-report.txt"],
            Some(encrypted_options(&["pw one"])),
        );
        write_archive(&r.join("good-for-bad.mochi"), &["x.txt"], None);
        let bytes = std::fs::read(r.join("good-for-bad.mochi")).unwrap();
        std::fs::remove_file(r.join("good-for-bad.mochi")).unwrap();
        std::fs::write(r.join("bad.mochi"), &bytes[..bytes.len() / 2]).unwrap();
        std::fs::write(r.join("fake.mochi"), b"this is not an archive at all").unwrap();
        std::fs::write(r.join("note.txt"), b"report").unwrap();
        dir
    }

    fn inline() -> Box<Spawner> {
        Box::new(|work| work())
    }

    fn scan_now(finder: &Arc<Finder>) -> (u32, Arc<Recorder>) {
        let sink = Arc::new(Recorder::default());
        let id = finder
            .start(false, Arc::clone(&sink) as Arc<dyn FindSink>, inline())
            .unwrap();
        (id, sink)
    }

    fn reasons(c: &FindCoverageView) -> Vec<(String, FindSkipReason)> {
        let mut v: Vec<(String, FindSkipReason)> = c
            .skipped
            .iter()
            .map(|s| {
                (
                    Path::new(&s.path)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    s.reason,
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    #[test]
    fn a_scan_finds_names_in_nested_archives_and_reports_what_it_could_not_search() {
        let dir = tree();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let (id, sink) = scan_now(&finder);

        let r = finder.query("report").unwrap();
        // Report.pdf (a), report-2.txt (b): the encrypted archive's name is not searchable.
        let names: Vec<&str> = r.hits.iter().map(|h| h.path.text.as_str()).collect();
        assert_eq!(names, ["Report.pdf", "report-2.txt"]);
        assert_eq!(r.total_hits, 2);
        assert!(r.hits.iter().all(|h| h.run == id));
        assert!(r.hits[0].archive_path.ends_with("a.mochi"));
        assert!(r.hits[1].archive_path.ends_with("b.mochi"));
        assert_eq!(r.hits[0].kind, EntryKindView::File);
        assert_eq!(r.hits[0].size, "10");

        let c = &r.coverage;
        assert_eq!(c.state, FindState::Complete);
        assert_eq!(c.archives_found, 5);
        assert_eq!(c.archives_searched, 2);
        assert_eq!(c.archives_skipped, 3);
        assert!(!c.complete, "skipped archives make the search incomplete");
        assert_eq!(
            reasons(c),
            [
                ("bad.mochi".to_owned(), FindSkipReason::Damaged),
                ("fake.mochi".to_owned(), FindSkipReason::NotMochi),
                ("secret.mochi".to_owned(), FindSkipReason::Encrypted),
            ]
        );
        assert!(c
            .skipped
            .iter()
            .all(|s| s.reason != FindSkipReason::Damaged || s.code.is_some()));
        assert!(c.content_not_searched);
        assert!(c.names_searched >= 6);

        let events = lock(&sink.events).clone();
        assert_eq!(events.first(), Some(&FindEvent::Started { run: id }));
        assert_eq!(
            events.last(),
            Some(&FindEvent::Finished {
                run: id,
                state: FindState::Complete
            })
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, FindEvent::Finished { .. }))
                .count(),
            1
        );
        assert!(events.iter().any(|e| matches!(
            e,
            FindEvent::Progress {
                total: Some(5),
                completed: 5,
                ..
            }
        )));
    }

    #[test]
    fn zero_matches_with_skipped_archives_is_incomplete_never_no_matches() {
        let dir = tree();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let r = finder.query("zzz-nothing-like-this").unwrap();
        assert_eq!(r.total_hits, 0);
        assert!(!r.coverage.complete);
        assert_eq!(r.coverage.archives_skipped, 3);
    }

    #[test]
    fn a_clean_scan_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        write_archive(&dir.path().join("only.mochi"), &["x/y.txt"], None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let none = finder.query("nope").unwrap();
        assert_eq!(none.total_hits, 0);
        assert!(
            none.coverage.complete,
            "a full scan with no match means none"
        );
        let some = finder.query("Y.TXT").unwrap();
        assert_eq!(some.total_hits, 1);
        assert_eq!(some.hits[0].path.text, "x/y.txt");
        // The folder `x` is a name too.
        let dirs = finder.query("x").unwrap();
        assert!(dirs.hits.iter().any(|h| h.kind == EntryKindView::Directory));
    }

    #[test]
    fn queries_match_names_globs_and_paths() {
        let dir = tree();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let paths = |q: &str| -> Vec<String> {
            finder
                .query(q)
                .unwrap()
                .hits
                .iter()
                .map(|h| h.path.text.clone())
                .collect()
        };
        assert_eq!(paths("holiday"), ["photos/Holiday.JPG"]);
        assert_eq!(paths("*.jpg"), ["photos/Holiday.JPG"]);
        assert_eq!(paths("report-?.txt"), ["report-2.txt"]);
        assert_eq!(paths("invoice.t*"), ["docs/invoice.txt"]);
        // A glob covers the whole name: "report" alone is a substring, "rep*" a prefix.
        assert_eq!(paths("rep*"), ["Report.pdf", "report-2.txt"]);
        assert!(paths("ep*").is_empty());
        // Text with a slash is matched against the path inside the archive.
        assert_eq!(paths("docs/inv"), ["docs/invoice.txt"]);
        assert!(paths("").is_empty(), "empty text is not a listing");
    }

    fn many(terms: &[&str]) -> FindQuery {
        FindQuery {
            terms: terms.iter().map(|t| (*t).to_owned()).collect(),
            ..FindQuery::default()
        }
    }

    #[test]
    fn a_list_of_names_says_what_each_found_and_which_found_nothing() {
        let dir = tree();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let r = finder
            .query_many(&many(&[
                "holiday.jpg",
                "  ",
                "invoice",
                "*.txt",
                "not-there.doc",
            ]))
            .unwrap();
        // Blank names are left out; the rest keep their order and numbers.
        let terms: Vec<(&str, u64, u64)> = r
            .terms
            .iter()
            .map(|t| (t.text.as_str(), t.hits, t.archives))
            .collect();
        assert_eq!(
            terms,
            [
                ("holiday.jpg", 1, 1),
                ("invoice", 1, 1),
                ("*.txt", 2, 2),
                ("not-there.doc", 0, 0),
            ]
        );
        // docs/invoice.txt matches two names and is one hit.
        assert_eq!(r.total_hits, 3);
        let inv = r
            .hits
            .iter()
            .find(|h| h.path.text == "docs/invoice.txt")
            .unwrap();
        assert_eq!(inv.terms, [1, 2]);
        // The skipped archives still make it incomplete.
        assert!(!r.coverage.complete);
        // One name is exactly `query`.
        assert_eq!(
            finder.query_many(&many(&["report"])).unwrap(),
            finder.query("report").unwrap()
        );
        assert!(finder.query_many(&many(&[])).unwrap().hits.is_empty());
        let over = vec!["a"; MAX_TERMS + 1];
        assert!(finder.query_many(&many(&over)).is_err());
    }

    #[test]
    fn match_case_whole_name_and_entry_kinds() {
        let dir = tree();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let paths = |q: FindQuery| -> Vec<String> {
            finder
                .query_many(&q)
                .unwrap()
                .hits
                .iter()
                .map(|h| h.path.text.clone())
                .collect()
        };
        assert_eq!(paths(many(&["report"])), ["Report.pdf", "report-2.txt"]);
        let cased = FindQuery {
            match_case: true,
            ..many(&["Report"])
        };
        assert_eq!(paths(cased), ["Report.pdf"]);
        let whole = FindQuery {
            whole_name: true,
            ..many(&["report.PDF", "report"])
        };
        assert_eq!(paths(whole), ["Report.pdf"]);
        let files = FindQuery {
            entries: FindEntryFilter::Files,
            ..many(&["docs", "photos", "holiday"])
        };
        assert_eq!(paths(files), ["photos/Holiday.JPG"]);
        let folders = FindQuery {
            entries: FindEntryFilter::Folders,
            ..many(&["docs", "photos", "holiday"])
        };
        assert_eq!(paths(folders), ["docs", "photos"]);
    }

    #[test]
    fn subfolders_off_searches_only_the_chosen_folders_and_is_still_complete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        write_archive(&dir.path().join("top.mochi"), &["a.txt"], None);
        write_archive(&dir.path().join("sub/deep.mochi"), &["b.txt"], None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        finder
            .start_with(false, false, Arc::new(Recorder::default()), inline())
            .unwrap();
        let r = finder.query_many(&many(&["a.txt", "b.txt"])).unwrap();
        assert_eq!(r.total_hits, 1);
        assert!(!r.coverage.subfolders);
        assert_eq!(r.coverage.archives_found, 1);
        assert!(r.coverage.complete, "not walking subfolders was asked for");
        scan_now(&finder);
        let r = finder.query_many(&many(&["a.txt", "b.txt"])).unwrap();
        assert_eq!((r.total_hits, r.coverage.subfolders), (2, true));
    }

    #[test]
    fn every_match_is_reachable_page_by_page_and_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<String> = (0..2_500).map(|i| format!("logs/app-{i:05}.log")).collect();
        let mut refs: Vec<&str> = files.iter().map(String::as_str).collect();
        refs.push("notes/readme.txt");
        write_archive(&dir.path().join("big.mochi"), &refs, None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let q = |offset: Option<u64>, limit: Option<u32>, term: Option<u32>| FindQuery {
            offset,
            limit,
            term,
            ..many(&["*.log", "readme"])
        };
        let first = finder.query_many(&q(None, None, None)).unwrap();
        assert_eq!(
            (first.hits.len(), first.total_hits, first.listed),
            (MAX_HITS_SHOWN, 2_501, 2_501)
        );
        assert_eq!(first.archives_with_hits, 1);
        // Pages cover every match once, in order.
        let mut seen: Vec<String> = Vec::new();
        let mut at = 0u64;
        loop {
            let page = finder.query_many(&q(Some(at), Some(1_000), None)).unwrap();
            assert_eq!(page.offset, at);
            if page.hits.is_empty() {
                break;
            }
            at += page.hits.len() as u64;
            seen.extend(page.hits.into_iter().map(|h| h.path.text));
        }
        assert_eq!(seen.len(), 2_501);
        let unique: std::collections::BTreeSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), 2_501);
        // One name's matches alone, and its size.
        let readme = finder.query_many(&q(None, None, Some(1))).unwrap();
        assert_eq!((readme.listed, readme.hits.len()), (1, 1));
        assert_eq!(readme.terms[1].bytes, "notes/readme.txt".len() as u64);
        assert_eq!(
            readme.total_bytes,
            readme.terms.iter().map(|t| t.bytes).sum::<u64>()
        );
        assert!(finder.query_many(&q(None, None, Some(5))).is_err());
        // A retrieve of all of it gets every path, grouped by archive.
        let run = first.hits[0].run;
        let all = finder.all_matches(run, &q(None, None, None)).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.len(), 2_501);
        assert_eq!(
            finder.all_matches(run, &q(None, None, Some(1))).unwrap()[0]
                .1
                .len(),
            1
        );
        assert!(finder.all_matches(run + 1, &q(None, None, None)).is_err());
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
    fn hostile_names_cross_as_escaped_text_with_exact_keys() {
        let dir = tempfile::tempdir().unwrap();
        let names = [
            "<img src=x onerror=alert(1)>.txt",
            "evil\u{202e}gpj.exe",
            "tab\tnew\nline.txt",
        ];
        write_archive(&dir.path().join("h.mochi"), &names, None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let r = finder.query("*").unwrap();
        assert_eq!(r.total_hits, 3);
        for h in &r.hits {
            assert!(
                !h.path.text.contains(['\n', '\t', '\u{202e}']),
                "{:?}",
                h.path
            );
            assert!(h.key.chars().all(|c| c.is_ascii_hexdigit()));
        }
        assert!(r.hits.iter().any(|h| !h.path.verbatim));
        assert!(r
            .hits
            .iter()
            .any(|h| h.path.text == "<img src=x onerror=alert(1)>.txt" && h.path.verbatim));
    }

    #[test]
    fn a_second_scan_uses_the_cache_until_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mochi");
        let b = dir.path().join("b.mochi");
        write_archive(&a, &["one.txt"], None);
        write_archive(&b, &["two.txt"], None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        assert_eq!(finder.coverage().archives_from_cache, 0);
        assert_eq!(finder.cache().len(), 2);

        scan_now(&finder);
        let c = finder.coverage();
        assert_eq!((c.archives_searched, c.archives_from_cache), (2, 2));
        assert_eq!(finder.query("two").unwrap().total_hits, 1);

        // A changed modified time is a different key: that archive is read again.
        let f = std::fs::OpenOptions::new().write(true).open(&a).unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        drop(f);
        scan_now(&finder);
        let c = finder.coverage();
        assert_eq!((c.archives_searched, c.archives_from_cache), (2, 1));

        // "Scan again" (fresh) forgets everything.
        let sink = Arc::new(Recorder::default());
        finder
            .start(true, sink as Arc<dyn FindSink>, inline())
            .unwrap();
        assert_eq!(finder.coverage().archives_from_cache, 0);
    }

    #[test]
    fn a_replaced_archive_is_read_again_and_its_old_hit_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mochi");
        write_archive(&a, &["old-name.txt"], None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let (id, _) = scan_now(&finder);
        let hit = finder.query("old-name").unwrap().hits.remove(0);
        assert_eq!(finder.archive_for_open(id, hit.archive).unwrap(), a);

        std::fs::remove_file(&a).unwrap();
        write_archive(&a, &["new-name-longer.txt"], None);
        let e = finder.archive_for_open(id, hit.archive).unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT");
        assert!(e.message.contains("changed"), "{}", e.message);

        scan_now(&finder);
        assert_eq!(finder.query("old-name").unwrap().total_hits, 0);
        assert_eq!(finder.query("new-name").unwrap().total_hits, 1);
        // A hit of the earlier scan is refused outright.
        assert!(finder.archive_for_open(id, hit.archive).is_err());
    }

    #[test]
    fn a_scan_can_be_cancelled_and_says_it_is_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..6 {
            write_archive(&dir.path().join(format!("{i}.mochi")), &["f.txt"], None);
        }
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        // A listener that is gone cancels the scan after its first event.
        let sink = Arc::new(Recorder::default());
        sink.deaf.store(true, Ordering::SeqCst);
        finder
            .start(false, Arc::clone(&sink) as Arc<dyn FindSink>, inline())
            .unwrap();
        let c = finder.coverage();
        assert_eq!(c.state, FindState::Cancelled);
        assert!(!c.complete);
        assert!(c.archives_searched < 6 || c.archives_pending > 0 || c.archives_found < 6);
        let events = lock(&sink.events).clone();
        assert_eq!(
            events.last(),
            Some(&FindEvent::Finished {
                run: c.run,
                state: FindState::Cancelled
            })
        );
        // The user's Cancel does the same while a scan runs in the background.
        assert!(!finder.cancel(), "nothing is running now");
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let sink2 = Arc::new(Recorder::default());
        finder
            .start(
                false,
                Arc::clone(&sink2) as Arc<dyn FindSink>,
                Box::new(move |work| {
                    let _ = tx.send(work);
                }),
            )
            .unwrap();
        assert!(finder.cancel(), "a started scan can be cancelled");
        rx.recv().unwrap()();
        assert_eq!(finder.coverage().state, FindState::Cancelled);
        let r = finder.query("f.txt").unwrap();
        assert!(!r.coverage.complete);
    }

    #[test]
    fn a_new_scan_cancels_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        write_archive(&dir.path().join("a.mochi"), &["f.txt"], None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let first_sink = Arc::new(Recorder::default());
        let tx2 = tx.clone();
        let first = finder
            .start(
                false,
                Arc::clone(&first_sink) as Arc<dyn FindSink>,
                Box::new(move |w| {
                    let _ = tx2.send(w);
                }),
            )
            .unwrap();
        let (second, _) = {
            let sink = Arc::new(Recorder::default());
            let id = finder
                .start(false, sink.clone() as Arc<dyn FindSink>, inline())
                .unwrap();
            (id, sink)
        };
        assert!(second > first);
        // The first scan's work runs late: it ends cancelled and changes nothing current.
        rx.recv().unwrap()();
        let events = lock(&first_sink.events).clone();
        assert_eq!(
            events.last(),
            Some(&FindEvent::Finished {
                run: first,
                state: FindState::Cancelled
            })
        );
        assert_eq!(finder.coverage().run, second);
        assert_eq!(finder.coverage().state, FindState::Complete);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_counted_not_followed_and_a_loop_cannot_hang_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        write_archive(&root.join("sub/a.mochi"), &["a-name.txt"], None);
        // A loop back to the root, and a link to a folder outside it.
        std::os::unix::fs::symlink(&root, root.join("sub/loop")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        write_archive(&outside.join("o.mochi"), &["outside-name.txt"], None);
        std::os::unix::fs::symlink(&outside, root.join("link-out")).unwrap();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![root]).unwrap();
        scan_now(&finder);
        assert_eq!(finder.query("a-name").unwrap().total_hits, 1);
        let r = finder.query("outside-name").unwrap();
        assert_eq!(r.total_hits, 0);
        assert_eq!(r.coverage.links_not_followed, 2);
        assert!(
            !r.coverage.complete,
            "unfollowed links are said, not hidden"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_is_counted_and_makes_the_search_incomplete() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        write_archive(&locked.join("hidden.mochi"), &["h.txt"], None);
        write_archive(&dir.path().join("ok.mochi"), &["ok.txt"], None);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let c = finder.coverage();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if c.folders_unreadable == 0 {
            // Running as root: permissions do not apply, nothing to assert.
            return;
        }
        assert_eq!(c.folders_unreadable, 1);
        assert!(!c.complete);
        assert_eq!(c.archives_searched, 1);
    }

    #[test]
    fn limits_stop_the_walk_and_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write_archive(&dir.path().join(format!("{i}.mochi")), &["f.txt"], None);
        }
        let finder = Arc::new(Finder::with_limits(Limits {
            archives: 3,
            workers: 2,
            ..Limits::default()
        }));
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let c = finder.coverage();
        assert_eq!(c.limit, Some(FindLimit::Archives));
        assert_eq!(c.archives_found, 3);
        assert!(!c.complete);

        let deep = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(deep.path().join("a/b/c")).unwrap();
        write_archive(&deep.path().join("a/b/c/d.mochi"), &["f.txt"], None);
        let finder = Arc::new(Finder::with_limits(Limits {
            depth: 1,
            ..Limits::default()
        }));
        finder.set_scopes(vec![deep.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let c = finder.coverage();
        assert_eq!(c.limit, Some(FindLimit::Depth));
        assert_eq!(c.archives_found, 0);
        assert!(!c.complete);
    }

    #[test]
    fn archives_that_do_not_fit_in_memory_are_too_large_not_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        write_archive(
            &dir.path().join("a.mochi"),
            &["1.txt", "2.txt", "3.txt"],
            None,
        );
        write_archive(
            &dir.path().join("b.mochi"),
            &["4.txt", "5.txt", "6.txt"],
            None,
        );
        let finder = Arc::new(Finder::with_limits(Limits {
            entries_total: 4,
            workers: 1,
            ..Limits::default()
        }));
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let c = finder.coverage();
        assert_eq!(c.archives_searched, 1);
        assert_eq!(c.skipped.len(), 1);
        assert_eq!(c.skipped[0].reason, FindSkipReason::TooLarge);
        assert!(!c.complete);
    }

    #[test]
    fn scopes_are_deduplicated_nested_ones_dropped_and_non_folders_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("inner");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        let finder = Finder::default();
        let v = finder
            .set_scopes(vec![
                inner.clone(),
                other.clone(),
                outer.clone(),
                outer.clone(),
                file,
            ])
            .unwrap();
        let shown: Vec<&str> = v.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(shown, [other.to_str().unwrap(), outer.to_str().unwrap()]);
        assert_eq!(v[1].index, 1);
        // Adding a folder inside a listed one changes nothing; removing works by position.
        let v = finder.add_scopes(vec![inner]).unwrap();
        assert_eq!(v.len(), 2);
        let v = finder.remove_scope(0);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, outer.display().to_string());
        assert!(finder
            .set_scopes(vec![
                PathBuf::from("relative/dir"),
                dir.path().join("missing")
            ])
            .is_err());
        // The earlier list is kept after a refusal.
        assert_eq!(finder.scopes().len(), 1);
        let none = Arc::new(Finder::default());
        let e = none
            .start(false, Arc::new(Recorder::default()), inline())
            .unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn a_query_before_any_scan_and_an_overlong_one_are_handled() {
        let finder = Finder::default();
        let r = finder.query("x").unwrap();
        assert_eq!(r.coverage.state, FindState::Idle);
        assert!(!r.coverage.complete);
        assert_eq!(r.total_hits, 0);
        let e = finder.query(&"a".repeat(MAX_QUERY_BYTES + 1)).unwrap_err();
        assert_eq!(e.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn hits_are_capped_but_counted() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<String> = (0..MAX_HITS_SHOWN + 25)
            .map(|i| format!("n{i:05}.txt"))
            .collect();
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        write_archive(&dir.path().join("many.mochi"), &refs, None);
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        scan_now(&finder);
        let r = finder.query("n0").unwrap();
        assert_eq!(r.hits.len(), MAX_HITS_SHOWN);
        assert_eq!(r.total_hits as usize, MAX_HITS_SHOWN + 25);
    }

    #[test]
    fn find_opens_the_archive_it_found_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mochi");
        write_archive(&a, &["x/y.txt"], None);
        let before = std::fs::read(&a).unwrap();
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let (id, _) = scan_now(&finder);
        let hit = finder.query("y.txt").unwrap().hits.remove(0);
        let path = finder.archive_for_open(id, hit.archive).unwrap();
        let archives = Archives::default();
        let OpenOutcome::Opened { archive } = archives.open_or_pend(&path).unwrap() else {
            panic!("expected an opened archive");
        };
        assert_eq!(archive.file_name, "a.mochi");
        assert_eq!(finder.archive_folder(id, hit.archive).unwrap(), dir.path());
        assert_eq!(
            std::fs::read(&a).unwrap(),
            before,
            "scanning writes nothing"
        );
    }

    /// Not a cost claim in the spec; a measurement for the report. Archives are made once, then
    /// the scan (cold: opens every archive), the same scan again (warm: the cache), and queries.
    /// `cargo test -p mochi-desktop --release find_scale -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark: builds many archives"]
    fn find_scale() {
        let archives = 60usize;
        let per = 2_000usize;
        let dir = tempfile::tempdir().unwrap();
        for a in 0..archives {
            let sub = dir.path().join(format!("d{}", a % 6));
            std::fs::create_dir_all(&sub).unwrap();
            let files: Vec<String> = (0..per)
                .map(|i| format!("dir{}/file-{a}-{i:05}.dat", i % 20))
                .collect();
            let refs: Vec<&str> = files.iter().map(String::as_str).collect();
            write_archive(&sub.join(format!("a{a}.mochi")), &refs, None);
        }
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![dir.path().to_path_buf()]).unwrap();
        let t = Instant::now();
        scan_now(&finder);
        let cold = t.elapsed();
        let t = Instant::now();
        scan_now(&finder);
        let warm = t.elapsed();
        let c = finder.coverage();
        let t = Instant::now();
        let r = finder.query("file-7-0123").unwrap();
        let q1 = t.elapsed();
        let t = Instant::now();
        let r2 = finder.query("*-00042.dat").unwrap();
        let q2 = t.elapsed();
        println!(
            "find_scale: {} archives, {} names; cold scan {cold:?}, warm scan {warm:?} ({} from cache); query {q1:?} ({} hits), glob {q2:?} ({} hits)",
            c.archives_searched, c.names_searched, c.archives_from_cache, r.total_hits, r2.total_hits
        );
        assert!(c.complete);
    }
}
