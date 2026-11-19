//! Archive browsing (plan D1): opened archives, their snapshots, and paged
//! listings, as IPC types. Thin over `mochi-core` (AGENTS.md): opening,
//! verification, history, retention, and the paged index
//! ([`mochi_core::browse::SnapshotIndex`]) all live there; this module keeps
//! sessions and converts results. It is independent of Tauri, so it is tested
//! without a window.
//!
//! Reading only: archives are opened through `OsReadStorage` (spec §20.2,
//! AGENTS.md "Verification is read-only"), and nothing here writes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use mochi_core::browse::{BrowseEntry, Children, SnapshotIndex, MAX_PAGE};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::manifest::{
    Attributes, WINDOWS_ARCHIVE, WINDOWS_HIDDEN, WINDOWS_READONLY, WINDOWS_SYSTEM,
};
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, promised_attributes, recorded_writer_parameters,
    segment_state, CatalogSource, HistoryEntry, OpenedHead, ReadOptions,
};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::timestamp::Timestamp;
use mochi_core::{ErrorCode, MochiError, Result};

use crate::ipc::{
    ArchiveHandle, ArchiveInfoView, ArchiveSummary, AttributesView, CatalogSourceView,
    ChildrenPage, CommitView, EntryDetails, EntryKindView, EntryRow, IpcError, NameText,
    OpenOutcome, PathKey, PendingId, PosixView, ProfileView, Secret, SnapshotView,
};
use mochi_core::keys::KeySession;
use mochi_core::publish::{locate_head, read_commit};

/// Archives one process keeps open at once. The UI shows one and closes it
/// before opening the next; the margin covers a reload that lost a handle.
pub const MAX_OPEN: usize = 4;

/// Archives all windows together keep open (each window is held to
/// [`MAX_OPEN`]); several archive windows are the point of the multi-window
/// design, so the process-wide cap is larger than one window's.
pub const MAX_OPEN_TOTAL: usize = 16;

/// The owner of an archive that no window claimed (a test, or a handle the
/// quick worker holds). Closing a window never touches it.
pub const UNOWNED: &str = "";

/// Snapshots kept open per archive (the head and the one being looked at).
const SNAPSHOTS_KEPT: usize = 2;

/// One opened snapshot: its verified commit, its index, and its promised
/// attributes once something asked for them.
struct View {
    seq: u64,
    head: OpenedHead,
    index: SnapshotIndex,
    attributes: Option<std::result::Result<BTreeMap<FileVersionId, Attributes>, MochiError>>,
}

impl View {
    fn open(head: OpenedHead) -> Result<Self> {
        let index = SnapshotIndex::build(&head.catalog)?;
        // The index counts in u32 (`SnapshotIndex::build` refuses more).
        Ok(View {
            seq: head.seq(),
            head,
            index,
            attributes: None,
        })
    }
}

struct Session {
    /// The file this session reads (D2 appends to it through this path).
    path: PathBuf,
    src: OsReadStorage,
    opts: ReadOptions,
    history: Vec<HistoryEntry>,
    /// Most recently used last.
    views: VecDeque<View>,
    /// The key envelope this session's passphrase unlocked (D5); `None` for
    /// a Core archive.
    unlocked_envelope: Option<[u8; 16]>,
}

impl Session {
    /// The opened snapshot `seq`, opening it if it is not kept, with the
    /// storage and options it was opened through.
    fn view(&mut self, seq: u64) -> Result<(&mut View, &OsReadStorage, &ReadOptions)> {
        let Session {
            src,
            opts,
            history,
            views,
            ..
        } = self;
        if let Some(i) = views.iter().position(|v| v.seq == seq) {
            if let Some(v) = views.remove(i) {
                views.push_back(v);
            }
        } else {
            let entry = history
                .iter()
                .find(|e| e.commit.seq == seq)
                .ok_or_else(|| invalid(format!("this archive has no commit {seq}")))?;
            let view = View::open(open_at_footer(src, entry.footer_offset, opts)?)?;
            if views.len() >= SNAPSHOTS_KEPT {
                views.pop_front();
            }
            views.push_back(view);
        }
        // Both branches leave the requested view last; `views` is not empty.
        let last = views.len() - 1;
        Ok((&mut views[last], src, opts))
    }
}

/// The archives this process has open, by handle.
#[derive(Default)]
pub struct Archives {
    inner: Mutex<Registry>,
    /// Told of every archive opened, for the start screen's recent list.
    recents: OnceLock<Arc<crate::recent::Recents>>,
}

#[derive(Default)]
struct Registry {
    last: ArchiveHandle,
    open: HashMap<ArchiveHandle, Arc<Mutex<Session>>>,
    last_pending: PendingId,
    /// The window (its label) each open archive belongs to. Closing a window
    /// closes exactly its archives.
    owners: HashMap<ArchiveHandle, String>,
    /// Encrypted archives waiting for a passphrase (D5), oldest first, with
    /// the window that asked. Only the path is kept: the webview never names
    /// it, and nothing secret is held before the passphrase arrives.
    pending: VecDeque<(PendingId, PathBuf, String)>,
}

/// Archives that may wait for a passphrase at once (the oldest is dropped).
pub const MAX_PENDING: usize = 4;

/// The message of a passphrase that did not open an Encrypted archive. It is
/// `KEY_UNAVAILABLE`, an operational answer: not evidence of damage (D20).
pub const WRONG_PASSPHRASE: &str = "the passphrase did not open this archive";

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned lock means a thread panicked while holding it. Sessions are
    // read-only caches; the next request rebuilds whatever it needs.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A [`PathKey`] back to an archive path. Never trusted: it is checked as
/// hex and as a path, and a path the snapshot lacks is refused later.
pub fn parse_key(key: &str) -> Result<ArchivePath> {
    let bad = || invalid("not a path key");
    let b = key.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(2) {
        return Err(bad());
    }
    let digit = |c: u8| (c as char).to_digit(16).ok_or_else(bad);
    let bytes = b
        .chunks_exact(2)
        .map(|p| u8::try_from(digit(p[0])? * 16 + digit(p[1])?).map_err(|_| bad()))
        .collect::<Result<Vec<u8>>>()?;
    ArchivePath::from_stored(&bytes).map_err(|_| bad())
}

/// A passphrase that opens none of the head's key envelopes is
/// `KEY_UNAVAILABLE` with one fixed message: never a statement about damage,
/// and never anything about the passphrase itself.
fn wrong_passphrase(e: MochiError) -> MochiError {
    if e.code == ErrorCode::KeyUnavailable {
        MochiError::new(ErrorCode::KeyUnavailable, WRONG_PASSPHRASE)
    } else {
        e
    }
}

/// Whether the head commit record, read without a key, says the archive is
/// Encrypted. `false` when the head cannot be read: opening reports that.
pub fn head_encrypted(path: &Path) -> bool {
    let Ok(src) = OsReadStorage::open(path) else {
        return false;
    };
    let read = ReadOptions::default();
    locate_head(&src, &read.limits)
        .ok()
        .and_then(|loc| read_commit(&src, &loc.footer, &read).ok())
        .is_some_and(|(c, _)| c.encrypted())
}

pub(crate) fn kind(k: EntryKind) -> EntryKindView {
    match k {
        EntryKind::File => EntryKindView::File,
        EntryKind::Directory => EntryKindView::Directory,
    }
}

fn count(n: u64) -> u32 {
    // Bounded by the entry count, which `SnapshotIndex::build` keeps in u32.
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn row(e: &BrowseEntry) -> EntryRow {
    EntryRow {
        key: hex(e.path.as_stored()),
        name: NameText::of(e.name()),
        kind: kind(e.kind),
        size: e.logical_len.to_string(),
        child_dirs: count(e.child_dirs),
        child_files: count(e.child_files),
    }
}

fn time(secs: i64, nanos: u32) -> Option<String> {
    Timestamp::from_unix(secs, nanos)
        .ok()
        .map(|t| t.to_string())
}

fn attributes_view(a: &Attributes) -> AttributesView {
    AttributesView::Recorded {
        posix: a.posix.map(|p| PosixView {
            mode: p.mode,
            uid: p.uid,
            gid: p.gid,
        }),
        windows: a.windows.map(|bits| {
            [
                (WINDOWS_READONLY, "read-only"),
                (WINDOWS_HIDDEN, "hidden"),
                (WINDOWS_SYSTEM, "system"),
                (WINDOWS_ARCHIVE, "archive"),
            ]
            .iter()
            .filter(|(bit, _)| bits & bit != 0)
            .map(|(_, name)| (*name).to_owned())
            .collect()
        }),
        mtime: a.mtime.and_then(|m| time(m.secs, m.nanos)),
    }
}

impl Archives {
    fn session(&self, handle: ArchiveHandle) -> Result<Arc<Mutex<Session>>> {
        lock(&self.inner)
            .open
            .get(&handle)
            .cloned()
            .ok_or_else(|| invalid("that archive is not open (it was closed)"))
    }

    /// Archives open now.
    pub fn open_count(&self) -> usize {
        lock(&self.inner).open.len()
    }

    /// Open the archive at `path` read-only: its history (every commit
    /// record, chained back to commit 0), its head fully verified as every
    /// open is, and retention at the head.
    pub fn open(&self, path: &Path, opts: ReadOptions) -> Result<ArchiveSummary> {
        self.open_as(UNOWNED, path, opts)
    }

    /// [`Archives::open`] for the window `owner`: the archive is released when
    /// that window closes, and counts against that window's [`MAX_OPEN`].
    pub fn open_as(&self, owner: &str, path: &Path, opts: ReadOptions) -> Result<ArchiveSummary> {
        {
            let reg = lock(&self.inner);
            let mine = reg.owners.values().filter(|o| o.as_str() == owner).count();
            if mine >= MAX_OPEN {
                return Err(MochiError::new(
                    ErrorCode::LimitExceeded,
                    format!("at most {MAX_OPEN} archives can be open at once; close one first"),
                ));
            }
            if reg.open.len() >= MAX_OPEN_TOTAL {
                return Err(MochiError::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "at most {MAX_OPEN_TOTAL} archives can be open in all windows; close one first"
                    ),
                ));
            }
        }
        let (summary, session) = Self::load(path, opts)?;
        let mut reg = lock(&self.inner);
        let handle = reg.last.checked_add(1).ok_or_else(|| {
            MochiError::new(
                ErrorCode::LimitExceeded,
                "this process has opened the maximum number of archives; restart the app",
            )
        })?;
        reg.last = handle;
        reg.open.insert(handle, Arc::new(Mutex::new(session)));
        reg.owners.insert(handle, owner.to_owned());
        drop(reg);
        if let Some(recents) = self.recents.get() {
            recents.record(path);
        }
        Ok(ArchiveSummary { handle, ..summary })
    }

    /// Keep `recents` informed of the archives opened from now on (once).
    pub fn set_recents(&self, recents: Arc<crate::recent::Recents>) {
        let _ = self.recents.set(recents);
    }

    /// Open the archive at `path` if it needs no passphrase; an Encrypted one
    /// (its head commit record says so without a key) waits in Rust for
    /// `unlock` and the webview is told only its file name (D5).
    pub fn open_or_pend(&self, path: &Path) -> Result<OpenOutcome> {
        self.open_or_pend_as(UNOWNED, path)
    }

    /// [`Archives::open_or_pend`] for the window `owner`; a waiting passphrase
    /// belongs to that window too.
    pub fn open_or_pend_as(&self, owner: &str, path: &Path) -> Result<OpenOutcome> {
        if head_encrypted(path) {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut reg = lock(&self.inner);
            let pending = reg.last_pending.checked_add(1).ok_or_else(|| {
                MochiError::new(
                    ErrorCode::LimitExceeded,
                    "this process has opened the maximum number of archives; restart the app",
                )
            })?;
            reg.last_pending = pending;
            if reg.pending.len() >= MAX_PENDING {
                reg.pending.pop_front();
            }
            reg.pending
                .push_back((pending, path.to_path_buf(), owner.to_owned()));
            return Ok(OpenOutcome::NeedsPassphrase { pending, file_name });
        }
        self.open_as(owner, path, ReadOptions::default())
            .map(|archive| OpenOutcome::Opened { archive })
    }

    /// Open the waiting archive `pending` with the passphrase. The passphrase
    /// goes into a [`KeySession`] (zeroizing) and is not kept anywhere else.
    /// A wrong one is `KEY_UNAVAILABLE` and the archive keeps waiting; any
    /// other outcome ends the wait.
    pub fn unlock(&self, pending: PendingId, passphrase: &Secret) -> Result<ArchiveSummary> {
        let (path, owner) = lock(&self.inner)
            .pending
            .iter()
            .find(|(id, _, _)| *id == pending)
            .map(|(_, p, o)| (p.clone(), o.clone()))
            .ok_or_else(|| invalid("that archive is no longer waiting for a passphrase"))?;
        let keys = KeySession::single(passphrase.passphrase()?);
        let opened = self.open_as(
            &owner,
            &path,
            ReadOptions {
                keys: Some(keys),
                ..ReadOptions::default()
            },
        );
        match &opened {
            Err(e) if e.code == ErrorCode::KeyUnavailable => {}
            _ => lock(&self.inner)
                .pending
                .retain(|(id, _, _)| *id != pending),
        }
        opened
    }

    /// Stop waiting for a passphrase. `false` if it was not waiting.
    pub fn cancel_pending(&self, pending: PendingId) -> bool {
        let mut reg = lock(&self.inner);
        let before = reg.pending.len();
        reg.pending.retain(|(id, _, _)| *id != pending);
        reg.pending.len() != before
    }

    /// The window an open archive belongs to (`""` if none claimed it).
    pub fn owner_of(&self, handle: ArchiveHandle) -> Option<String> {
        lock(&self.inner).owners.get(&handle).cloned()
    }

    /// Give an open archive to the window `owner`. `false` if it is not open.
    pub fn assign(&self, handle: ArchiveHandle, owner: &str) -> bool {
        let mut reg = lock(&self.inner);
        if !reg.open.contains_key(&handle) {
            return false;
        }
        reg.owners.insert(handle, owner.to_owned());
        true
    }

    /// The open archives of one window, oldest first.
    pub fn handles_of(&self, owner: &str) -> Vec<ArchiveHandle> {
        let reg = lock(&self.inner);
        let mut v: Vec<_> = reg
            .owners
            .iter()
            .filter(|(_, o)| o.as_str() == owner)
            .map(|(h, _)| *h)
            .collect();
        v.sort_unstable();
        v
    }

    /// Whether the window has an archive open or one waiting for its
    /// passphrase.
    pub fn holds_any(&self, owner: &str) -> bool {
        let reg = lock(&self.inner);
        reg.owners.values().any(|o| o.as_str() == owner)
            || reg.pending.iter().any(|(_, _, o)| o.as_str() == owner)
    }

    /// The file names of a window's open archives, oldest first (for its
    /// title).
    pub fn file_names_of(&self, owner: &str) -> Vec<String> {
        self.handles_of(owner)
            .into_iter()
            .filter_map(|h| self.path(h).ok())
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect()
    }

    /// The window that has `path` open, or is waiting for its passphrase (the
    /// same file, whatever name or link it was reached by).
    pub fn owner_of_file(&self, path: &Path) -> Option<String> {
        let (sessions, waiting): (Vec<_>, Vec<_>) = {
            let reg = lock(&self.inner);
            (
                reg.open
                    .iter()
                    .map(|(h, s)| (Arc::clone(s), reg.owners.get(h).cloned()))
                    .collect(),
                reg.pending
                    .iter()
                    .map(|(_, p, o)| (p.clone(), o.clone()))
                    .collect(),
            )
        };
        for (session, owner) in sessions {
            let open_path = lock(&session).path.clone();
            if crate::windows::same_file(&open_path, path) {
                return owner;
            }
        }
        waiting
            .into_iter()
            .find(|(p, _)| crate::windows::same_file(p, path))
            .map(|(_, o)| o)
    }

    /// The handle of the open archive that is the file `path` (not one that
    /// is only waiting for a passphrase), if any.
    pub fn handle_of_file(&self, path: &Path) -> Option<ArchiveHandle> {
        let sessions: Vec<_> = {
            let reg = lock(&self.inner);
            reg.open.iter().map(|(h, s)| (*h, Arc::clone(s))).collect()
        };
        sessions
            .into_iter()
            .find(|(_, s)| crate::windows::same_file(&lock(s).path, path))
            .map(|(h, _)| h)
    }

    /// Close every archive of one window and forget its waiting passphrases.
    /// Returns how many archives were open. Other windows are untouched.
    pub fn close_owned_by(&self, owner: &str) -> usize {
        let mut reg = lock(&self.inner);
        let handles: Vec<_> = reg
            .owners
            .iter()
            .filter(|(_, o)| o.as_str() == owner)
            .map(|(h, _)| *h)
            .collect();
        for h in &handles {
            reg.open.remove(h);
            reg.owners.remove(h);
        }
        reg.pending.retain(|(_, _, o)| o.as_str() != owner);
        handles.len()
    }

    /// Encrypted archives waiting for a passphrase.
    pub fn pending_count(&self) -> usize {
        lock(&self.inner).pending.len()
    }

    /// The read options an open archive was opened with, including the key
    /// session of an Encrypted one: what a write job needs to append (D5).
    pub fn read_options(&self, handle: ArchiveHandle) -> Result<ReadOptions> {
        Ok(lock(&*self.session(handle)?).opts.clone())
    }

    /// The key envelope this session's passphrase unlocked.
    pub fn unlocked_envelope(&self, handle: ArchiveHandle) -> Result<Option<[u8; 16]>> {
        Ok(lock(&*self.session(handle)?).unlocked_envelope)
    }

    /// The file an open archive was opened from.
    pub fn path(&self, handle: ArchiveHandle) -> Result<PathBuf> {
        Ok(lock(&*self.session(handle)?).path.clone())
    }

    /// A private reader and the verified commit `seq` of an open archive, for
    /// a job that reads for a long time (extraction, D3). It opens the file
    /// again instead of borrowing the session's, so browsing is not blocked
    /// while the job runs.
    pub fn open_for_read(
        &self,
        handle: ArchiveHandle,
        seq: u64,
    ) -> Result<(OsReadStorage, OpenedHead, ReadOptions)> {
        let (path, opts, footer_offset) = {
            let session = self.session(handle)?;
            let s = lock(&session);
            let entry = s
                .history
                .iter()
                .find(|e| e.commit.seq == seq)
                .ok_or_else(|| invalid(format!("this archive has no commit {seq}")))?;
            (s.path.clone(), s.opts.clone(), entry.footer_offset)
        };
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let head = open_at_footer(&src, footer_offset, &opts)?;
        Ok((src, head, opts))
    }

    /// Read the archive at the same path again, under the same handle: after
    /// a commit, so the timeline and snapshots show it. The old session is
    /// replaced only once the new one has fully opened.
    pub fn reopen(&self, handle: ArchiveHandle) -> Result<ArchiveSummary> {
        let (path, opts) = {
            let session = self.session(handle)?;
            let s = lock(&session);
            (s.path.clone(), s.opts.clone())
        };
        let (summary, session) = Self::load(&path, opts)?;
        let mut reg = lock(&self.inner);
        match reg.open.get_mut(&handle) {
            Some(slot) => *slot = Arc::new(Mutex::new(session)),
            None => return Err(invalid("that archive is not open (it was closed)")),
        }
        Ok(ArchiveSummary { handle, ..summary })
    }

    /// Read `path` into a session and its summary (handle 0 until
    /// registered).
    fn load(path: &Path, opts: ReadOptions) -> Result<(ArchiveSummary, Session)> {
        let src = OsReadStorage::open(path).map_err(MochiError::from)?;
        let history = commit_history(&src, &opts)?;
        let head = open_head(&src, &opts).map_err(wrong_passphrase)?;
        if head.descriptor.encrypted() && head.unlocked.is_none() {
            return Err(MochiError::new(ErrorCode::KeyUnavailable, WRONG_PASSPHRASE));
        }
        let unlocked_envelope = head.unlocked.as_ref().map(|u| u.envelope_id);
        let retention = segment_state(&src, &head, &opts).map(|s| s.retention);
        let head_seq = head.seq();
        let roots = retention.as_ref().ok().map(|r| r.roots(head_seq));
        let commits = history
            .iter()
            .map(|e| {
                let seq = e.commit.seq;
                CommitView {
                    seq,
                    commit_id: e.commit_id.to_hex(),
                    time: e.commit.time.and_then(|t| time(t.secs, t.nanos)),
                    checkpoint: e.commit.metadata.is_checkpoint(),
                    retained: roots.as_ref().map(|r| r.contains(&seq)),
                    expired: retention.as_ref().ok().map(|r| r.expired.contains(&seq)),
                    holds: retention
                        .as_ref()
                        .map(|r| {
                            r.holds
                                .iter()
                                .filter(|(_, s)| **s == seq)
                                .map(|(label, _)| NameText::of(label))
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect();
        let profile = head.descriptor.profile();
        let summary = ArchiveSummary {
            handle: 0,
            file_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            archive_id: head.commit.archive_id.to_hex(),
            profile: ProfileView {
                tar_compatible: profile.tar_compatible,
                encrypted: profile.encrypted,
                redundancy_percent: profile.redundancy,
            },
            commits,
            retention_error: retention.err().map(IpcError::from),
        };
        let mut views = VecDeque::new();
        views.push_back(View::open(head)?);
        let session = Session {
            path: path.to_path_buf(),
            src,
            opts,
            history,
            views,
            unlocked_envelope,
        };
        Ok((summary, session))
    }

    /// Close an archive. `false` if it was not open.
    pub fn close(&self, handle: ArchiveHandle) -> bool {
        let mut reg = lock(&self.inner);
        reg.owners.remove(&handle);
        reg.open.remove(&handle).is_some()
    }

    /// Open (or reuse) snapshot `seq` of an open archive.
    pub fn snapshot(&self, handle: ArchiveHandle, seq: u64) -> Result<SnapshotView> {
        let session = self.session(handle)?;
        let mut s = lock(&session);
        let (v, _, _) = s.view(seq)?;
        let (root_dirs, root_files) = v.index.counts(None)?;
        Ok(SnapshotView {
            seq: v.seq,
            commit_id: v.head.commit_id.to_hex(),
            entries: count(v.index.len() as u64),
            root_dirs: count(root_dirs),
            root_files: count(root_files),
            catalog_source: match &v.head.catalog_source {
                CatalogSource::Image => CatalogSourceView::Image,
                CatalogSource::SnapshotManifest { image_error } => {
                    CatalogSourceView::SnapshotManifest {
                        image_error: IpcError::from(image_error.clone()),
                    }
                }
            },
        })
    }

    /// The Info window's facts for snapshot `seq`: its totals
    /// ([`SnapshotIndex::totals`]) and the writer's recorded parameters, read
    /// from the snapshot's catalog; and the archive file's size on disk.
    pub fn info(&self, handle: ArchiveHandle, seq: u64) -> Result<ArchiveInfoView> {
        let session = self.session(handle)?;
        let mut s = lock(&session);
        let path = s.path.clone();
        let (v, _, _) = s.view(seq)?;
        let totals = v.index.totals(&v.head.catalog)?;
        let params = recorded_writer_parameters(&v.head.catalog)?;
        let seq = v.seq;
        let comment = v.head.commit.comment.clone();
        drop(s);
        Ok(ArchiveInfoView {
            seq,
            folders: totals.folders.to_string(),
            files: totals.files.to_string(),
            total_size: totals.logical_bytes.to_string(),
            packed_size: totals.stored_bytes.to_string(),
            chunks: totals.chunks.to_string(),
            file_size: std::fs::metadata(&path).ok().map(|m| m.len().to_string()),
            chunk_size: params.map(|(size, _)| size.to_string()),
            zstd_level: params.map(|(_, level)| level),
            comment,
        })
    }

    /// Children `offset .. offset + limit` of `dir` (the root for `None`) in
    /// snapshot `seq`; subdirectories only with `directories_only`.
    pub fn children(
        &self,
        handle: ArchiveHandle,
        seq: u64,
        dir: Option<&str>,
        directories_only: bool,
        offset: u32,
        limit: u32,
    ) -> Result<ChildrenPage> {
        let dir = dir.map(parse_key).transpose()?;
        let limit = usize::try_from(limit).unwrap_or(MAX_PAGE + 1);
        let session = self.session(handle)?;
        let mut s = lock(&session);
        let (v, _, _) = s.view(seq)?;
        let which = if directories_only {
            Children::Directories
        } else {
            Children::All
        };
        let page = v
            .index
            .children(dir.as_ref(), which, u64::from(offset), limit)?;
        Ok(ChildrenPage {
            total: count(page.total),
            offset,
            entries: page.entries.into_iter().map(row).collect(),
        })
    }

    /// Details of the entry at `key` in snapshot `seq`. Attributes are read
    /// from the snapshot's manifests on first use; if they cannot be, they
    /// are reported unavailable, never guessed.
    pub fn details(&self, handle: ArchiveHandle, seq: u64, key: &PathKey) -> Result<EntryDetails> {
        let path = parse_key(key)?;
        let session = self.session(handle)?;
        let mut s = lock(&session);
        let (v, src, opts) = s.view(seq)?;
        let e = v
            .index
            .entry(&path)
            .ok_or_else(|| invalid("no such entry in this snapshot"))?
            .clone();
        let attributes = {
            let head = &v.head;
            let all = v
                .attributes
                .get_or_insert_with(|| promised_attributes(src, head, opts));
            match all {
                Ok(map) => match map.get(&e.version) {
                    Some(a) => attributes_view(a),
                    None => AttributesView::Unavailable {
                        error: IpcError {
                            code: ErrorCode::RecordInvalid.as_str().to_owned(),
                            message: "the manifests record no attributes for this version"
                                .to_owned(),
                        },
                    },
                },
                Err(err) => AttributesView::Unavailable {
                    error: IpcError::from(err.clone()),
                },
            }
        };
        Ok(EntryDetails {
            key: key.clone(),
            path: NameText::of(e.path.as_stored()),
            kind: kind(e.kind),
            size: e.logical_len.to_string(),
            content_hash: e.content_hash.map(|h| h.to_hex()),
            file_version_id: hex(e.version.as_bytes()),
            attributes,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use mochi_core::job::JobContext;
    use mochi_core::manifest::{Mtime, PosixAttributes};
    use mochi_core::publish::{ArchiveWriter, CheckpointPolicy, Transaction};
    use mochi_core::storage::os::OsStorage;
    use mochi_testkit::archive::{build, encrypted_options, scripted_history, test_options, Job};
    use mochi_testkit::SeqIds;

    use super::*;

    fn scripted(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("scripted.mochi");
        build(
            OsStorage::create_new(&path).unwrap(),
            7,
            &scripted_history(),
        )
        .unwrap();
        path
    }

    fn key(s: &[u8]) -> String {
        hex(s)
    }

    fn names(page: &ChildrenPage) -> Vec<String> {
        page.entries.iter().map(|e| e.name.text.clone()).collect()
    }

    #[test]
    fn an_archive_opens_with_its_history_and_head_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let archives = Archives::default();
        let s = archives
            .open(&scripted(&dir), ReadOptions::default())
            .unwrap();
        assert_eq!(s.file_name, "scripted.mochi");
        let seqs: Vec<u64> = s.commits.iter().map(|c| c.seq).collect();
        assert_eq!(seqs, [0, 1, 2]);
        assert!(s.commits.iter().all(|c| c.checkpoint));
        assert!(s.retention_error.is_none());
        assert!(s.commits.iter().all(|c| c.retained == Some(true)));
        assert_eq!(
            s.profile,
            ProfileView {
                tar_compatible: false,
                encrypted: false,
                redundancy_percent: None
            }
        );

        // The head, against the scripted model (independent of the catalog).
        let model = scripted_history().last().unwrap().after.clone();
        let snap = archives.snapshot(s.handle, 2).unwrap();
        assert_eq!(snap.entries as usize, model.len());
        assert_eq!(snap.catalog_source, CatalogSourceView::Image);
        let root = archives.children(s.handle, 2, None, false, 0, 100).unwrap();
        let mut expected_root: Vec<String> = model
            .keys()
            .filter(|k| !k.contains(&b'/'))
            .map(|k| String::from_utf8(k.clone()).unwrap())
            .collect();
        expected_root.sort();
        let mut got = names(&root);
        got.sort();
        assert_eq!(got, expected_root);
        assert_eq!(root.entries[0].kind, EntryKindView::Directory, "dirs first");
        assert_eq!(root.total as usize, expected_root.len());

        // Info: every entry is a folder or a file; the chunks fit in the file,
        // and the writer's chunk size and level were recorded.
        let info = archives.info(s.handle, 2).unwrap();
        let n = |v: &str| v.parse::<u64>().unwrap();
        assert_eq!(info.seq, 2);
        assert_eq!(n(&info.folders) + n(&info.files), u64::from(snap.entries));
        assert!(n(&info.packed_size) > 0);
        assert!(n(&info.packed_size) < n(info.file_size.as_deref().unwrap()));
        assert!(info.chunk_size.is_some() && info.zstd_level.is_some());
        assert!(archives.info(s.handle + 99, 2).is_err(), "a closed handle");
    }

    #[test]
    fn earlier_snapshots_show_their_own_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let archives = Archives::default();
        let s = archives
            .open(&scripted(&dir), ReadOptions::default())
            .unwrap();
        let steps = scripted_history();
        for (seq, step) in steps.iter().enumerate() {
            let seq = seq as u64;
            let mut got = Vec::new();
            // Walk the whole tree through pages of 2, as the UI would.
            let mut dirs = vec![None::<String>];
            while let Some(d) = dirs.pop() {
                let mut offset = 0;
                loop {
                    let page = archives
                        .children(s.handle, seq, d.as_deref(), false, offset, 2)
                        .unwrap();
                    if page.entries.is_empty() {
                        break;
                    }
                    offset += page.entries.len() as u32;
                    for e in page.entries {
                        if e.kind == EntryKindView::Directory {
                            dirs.push(Some(e.key.clone()));
                        }
                        got.push(parse_key(&e.key).unwrap().as_stored().to_vec());
                    }
                }
            }
            got.sort();
            let want: Vec<Vec<u8>> = step.after.keys().cloned().collect();
            assert_eq!(got, want, "snapshot {seq}");
        }
    }

    #[test]
    fn details_carry_the_recorded_hash_and_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let archives = Archives::default();
        let s = archives
            .open(&scripted(&dir), ReadOptions::default())
            .unwrap();
        let model = scripted_history().last().unwrap().after.clone();
        let hostile = b"<img src=x onerror=alert(1)>";
        let d = archives.details(s.handle, 2, &key(hostile)).unwrap();
        assert_eq!(d.path.text, "<img src=x onerror=alert(1)>");
        assert!(d.path.verbatim);
        let mochi_testkit::archive::Content::File(bytes) = &model[hostile.as_slice()] else {
            panic!("a file in the model")
        };
        assert_eq!(d.size, bytes.len().to_string());
        assert_eq!(
            d.content_hash.as_deref(),
            Some(blake3::hash(bytes).to_hex().as_str()),
            "plain BLAKE3 of the content (O20), computed here independently"
        );
        let AttributesView::Recorded { posix, mtime, .. } = d.attributes else {
            panic!("attributes recorded: {:?}", d.attributes)
        };
        assert_eq!(
            posix,
            Some(PosixView {
                mode: 0o644,
                uid: 1000,
                gid: 1000
            })
        );
        assert!(mtime.is_some());
        let docs = archives.details(s.handle, 2, &key(b"docs")).unwrap();
        assert_eq!(docs.kind, EntryKindView::Directory);
        assert_eq!(docs.content_hash, None);
    }

    #[test]
    fn untrusted_requests_are_refused_with_invalid_argument() {
        let dir = tempfile::tempdir().unwrap();
        let archives = Archives::default();
        let s = archives
            .open(&scripted(&dir), ReadOptions::default())
            .unwrap();
        let h = s.handle;
        let code = |r: Result<ChildrenPage>| r.unwrap_err().code;
        assert_eq!(
            code(archives.children(h, 9, None, false, 0, 10)),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(archives.children(h, 2, None, false, 0, 0)),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            code(archives.children(h, 2, None, false, 0, MAX_PAGE as u32 + 1)),
            ErrorCode::InvalidArgument
        );
        for bad in ["", "zz", "6", "2f", "646f6373", "big"] {
            let r = archives.children(h, 2, Some(bad), false, 0, 10);
            // "646f6373" is `docs`, a valid directory: the one that succeeds.
            if bad == "646f6373" {
                assert!(r.is_ok());
            } else {
                assert_eq!(code(r), ErrorCode::InvalidArgument, "{bad:?}");
            }
        }
        // A file is not a directory; an absent path is not an entry.
        assert_eq!(
            code(archives.children(h, 2, Some(&key(b"big")), false, 0, 10)),
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            archives.details(h, 2, &key(b"nope")).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert!(archives.close(h));
        assert!(!archives.close(h));
        assert_eq!(
            code(archives.children(h, 2, None, false, 0, 10)),
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn open_archives_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = scripted(&dir);
        let archives = Archives::default();
        for _ in 0..MAX_OPEN {
            archives.open(&path, ReadOptions::default()).unwrap();
        }
        let e = archives.open(&path, ReadOptions::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
        assert_eq!(archives.open_count(), MAX_OPEN);
    }

    #[test]
    fn files_that_are_not_archives_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not.mochi");
        std::fs::write(&path, b"this is not an archive").unwrap();
        let archives = Archives::default();
        assert!(archives.open(&path, ReadOptions::default()).is_err());
        assert!(archives
            .open(&dir.path().join("missing.mochi"), ReadOptions::default())
            .is_err());
        assert_eq!(archives.open_count(), 0);
    }

    #[test]
    fn an_encrypted_archive_needs_a_passphrase_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("enc.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(3)),
            encrypted_options(&["correct horse"]),
        )
        .unwrap();
        let job = Job::new();
        let mut tx = Transaction::new();
        tx.put_file(
            mochi_testkit::archive::path("secret.txt"),
            b"x".to_vec(),
            Attributes::default(),
        );
        w.commit(tx, &job.ctx()).unwrap();
        w.close().unwrap();
        let e = Archives::default()
            .open(&path, ReadOptions::default())
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::KeyUnavailable);
    }

    /// Names that are not UTF-8, and control and bidi characters, cross as
    /// an escaped rendering plus an exact key that round-trips.
    #[test]
    fn hostile_names_are_escaped_and_keys_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(5)),
            test_options(),
        )
        .unwrap();
        let attrs = Attributes {
            posix: Some(PosixAttributes {
                mode: 0o600,
                uid: 0,
                gid: 0,
            }),
            windows: Some(WINDOWS_HIDDEN | WINDOWS_READONLY),
            mtime: Some(Mtime {
                secs: 1_700_000_000,
                nanos: 0,
            }),
        };
        let raw: [&[u8]; 4] = [
            b"bad\xff\xfename",
            b"esc\x1b[31mred",
            "rtl\u{202E}gpj.exe".as_bytes(),
            b"line\nbreak",
        ];
        let mut tx = Transaction::new();
        for r in raw {
            tx.put_file(ArchivePath::from_stored(r).unwrap(), r.to_vec(), attrs);
        }
        let job = Job::new();
        w.commit(tx, &job.ctx()).unwrap();
        w.close().unwrap();
        let archives = Archives::default();
        let s = archives.open(&path, ReadOptions::default()).unwrap();
        let page = archives.children(s.handle, 0, None, false, 0, 10).unwrap();
        assert_eq!(page.total, 4);
        for e in &page.entries {
            assert!(!e.name.verbatim, "{:?} was escaped", e.name.text);
            assert!(!e.name.text.chars().any(char::is_control));
            let stored = parse_key(&e.key).unwrap();
            assert!(raw.contains(&stored.as_stored()));
            let d = archives.details(s.handle, 0, &e.key).unwrap();
            assert_eq!(d.size, stored.as_stored().len().to_string());
            let AttributesView::Recorded { windows, .. } = d.attributes else {
                panic!("recorded")
            };
            assert_eq!(
                windows,
                Some(vec!["read-only".to_owned(), "hidden".to_owned()])
            );
        }
        assert_eq!(
            names(&page),
            [
                "bad\\xff\\xfename",
                "esc\\u{1b}[31mred",
                "line\\u{a}break",
                "rtl\\u{202e}gpj.exe"
            ]
        );
    }

    /// Plan D1 exit, library side: a 100,000-entry snapshot is browsed a
    /// page at a time; no page exceeds the limit, and every entry is reached.
    #[test]
    fn a_hundred_thousand_entries_browse_in_bounded_pages() {
        const FILES: u32 = 100_000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(11)),
            test_options(),
        )
        .unwrap();
        w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
            .unwrap();
        let mut tx = Transaction::new();
        tx.put_dir(mochi_testkit::archive::path("many"), Attributes::default());
        for i in 0..FILES {
            tx.put_file(
                ArchivePath::from_stored(format!("many/f{i:06}").as_bytes()).unwrap(),
                Vec::new(),
                Attributes::default(),
            );
        }
        let job = Job::new();
        let ctx: JobContext<'_> = job.ctx();
        w.commit(tx, &ctx).unwrap();
        w.close().unwrap();

        let archives = Archives::default();
        let s = archives.open(&path, ReadOptions::default()).unwrap();
        let snap = archives.snapshot(s.handle, 0).unwrap();
        assert_eq!(snap.entries, FILES + 1);
        let many = key(b"many");
        let mut seen = 0u32;
        let mut last: Option<String> = None;
        loop {
            let page = archives
                .children(s.handle, 0, Some(&many), false, seen, MAX_PAGE as u32)
                .unwrap();
            assert_eq!(page.total, FILES);
            assert!(page.entries.len() <= MAX_PAGE);
            if page.entries.is_empty() {
                break;
            }
            for e in &page.entries {
                assert!(last.as_deref() < Some(e.name.text.as_str()), "ordered");
                last = Some(e.name.text.clone());
            }
            seen += page.entries.len() as u32;
        }
        assert_eq!(seen, FILES);
        // A page deep in the directory is as small as the first.
        let deep = archives
            .children(s.handle, 0, Some(&many), false, 99_990, 50)
            .unwrap();
        assert_eq!(deep.entries.len(), 10);
        assert_eq!(deep.entries[0].name.text, "f099990");
    }
}
