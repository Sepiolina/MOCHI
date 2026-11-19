//! Foreign archives in the desktop app (plan D8): ZIP, 7z, RAR, tar, tar.gz,
//! tar.zst, opened **read-only** to browse, test, and extract. Thin over two crates
//! (AGENTS.md "No format logic in the UI or in Tauri commands"):
//!
//! * `mochi-foreign` parses the archive in an isolated helper process. This
//!   crate links no parser (it names `mochi-foreign` with
//!   `default-features = false`; `ci/check-invariants.sh` checks that) and
//!   treats everything the helper says as untrusted text.
//! * `mochi_core::restore_foreign` judges every name and writes every byte,
//!   with the C6 engine's protections. Nothing here creates a file.
//!
//! # Wording (plan D8 rule 6, spec §23.3)
//!
//! A foreign "test" reports the checks that ran ("CRC32 matched"), never
//! MOCHI's verification vocabulary, and recoverability, freshness and
//! retention are `UNSUPPORTED`. An extraction says what it extracted and what
//! it did not; it never says an archive is verified.
//!
//! # Passwords
//!
//! A password crosses IPC once, in the call that opens the archive. Rust keeps
//! it (zeroized on drop) in the open session so that testing and extracting
//! need not ask again; the webview never holds it past the call, and it is
//! never written anywhere. The helper receives it in its request frame, never
//! as an argument.
//!
//! # The webview names no path
//!
//! The archive and the destination folder come from native dialogs shown from
//! Rust. A file that needs a password is remembered (`Foreign::pending`) so
//! the retry does not have to name it.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::JobContext;
use mochi_core::restore::ExceptionKind;
use mochi_core::restore_foreign::{
    plan_foreign, FileFailure, ForeignEntry, ForeignKind, ForeignRestoreOptions, ForeignRestorer,
    ForeignTree, RefusalReason, TreeRow,
};
use mochi_core::storage::os::OsRestoreDir;
use mochi_core::storage::{AttributeKind, CaseBehavior, DirectoryDurability};
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_foreign::{
    default_helper_path, EntryInfo, EntryKind, EntryOutcome, ForeignClient, ForeignError,
    ForeignFormat, Password, ReadManySink, StreamCheck, FOREIGN_DIMENSIONS,
};

use crate::browse::parse_key;
use crate::extract::{Destination, MAX_CONFLICTS_SHOWN, MAX_NOT_EXTRACTED_SHOWN};
use crate::ipc::{
    AttributeNote, CheckCount, ConflictChoice, ConflictView, ConflictWhy, ExtractPlan,
    ExtractReport, ForeignArchiveSummary, ForeignChildrenPage, ForeignDimensionsView,
    ForeignExtractPlan, ForeignHit, ForeignKindView, ForeignProblem, ForeignRow,
    ForeignSearchResult, ForeignTestReport, IpcError, JobRequest, NameText, NotExtractedView,
    PathKey,
};
use crate::write::JobDone;

/// Foreign archives one process keeps open at once.
pub const MAX_OPEN: usize = 4;

/// Foreign archives all windows together keep open (each window is held to
/// [`MAX_OPEN`]).
pub const MAX_OPEN_TOTAL: usize = 16;

use crate::browse::UNOWNED;

/// Most entries one extract may name.
pub const MAX_KEYS: usize = 10_000;

/// Problems a test report lists (the total is always given).
pub const MAX_PROBLEMS_SHOWN: usize = 1_000;

/// Hits a search returns (the total is always given).
pub const MAX_SEARCH_HITS: usize = 1_000;

/// The longest search text accepted, in bytes (as for a MOCHI search).
pub const MAX_SEARCH_QUERY_BYTES: usize = crate::search::MAX_QUERY_BYTES;

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A helper failure as the IPC error the UI shows. The code is the foreign
/// crate's stable code (`WRONG_PASSWORD`, `HELPER_CRASHED`, ...).
pub fn ipc_error(e: &ForeignError) -> IpcError {
    IpcError {
        code: e.code.as_str().to_owned(),
        message: e.message.clone(),
    }
}

fn mochi_error(e: &ForeignError) -> MochiError {
    let code = match e.code {
        mochi_foreign::ErrorCode::Cancelled => ErrorCode::Cancelled,
        c if c.is_unsupported() => ErrorCode::UnsupportedFeature,
        _ => ErrorCode::IoError,
    };
    MochiError::new(code, format!("{}: {}", e.code.as_str(), e.message))
}

/// One open foreign archive. The listing is kept as the helper reported it.
struct Session {
    path: PathBuf,
    entries: Vec<EntryInfo>,
    tree: ForeignTree,
    password: Option<Password>,
}

#[derive(Default)]
struct Registry {
    last: u32,
    open: HashMap<u32, Arc<Session>>,
    /// The window (its label) each open archive belongs to.
    owners: HashMap<u32, String>,
    /// A file a window picked that needs a password, kept so the retry (which
    /// carries the password) does not have to name it. One per window.
    pending: HashMap<String, PathBuf>,
}

/// The foreign archives this process has open, by handle.
#[derive(Default)]
pub struct Foreign {
    inner: Mutex<Registry>,
    /// Where the helper binary is, when not next to the executable (tests).
    helper: Mutex<Option<PathBuf>>,
    /// The recent-archives list a successful open is recorded in.
    recents: OnceLock<Arc<crate::recent::Recents>>,
}

fn checks_text(f: ForeignFormat) -> &'static str {
    match f {
        ForeignFormat::Zip => {
            "CRC32 per entry (AES-encrypted entries: the AES authentication code)"
        }
        ForeignFormat::SevenZ => "CRC32 per entry",
        ForeignFormat::Rar => "CRC32 or BLAKE2sp per entry, as each entry records",
        ForeignFormat::Tar => "no per-entry check",
        ForeignFormat::TarGz => "gzip CRC-32 over the whole stream; no per-entry check",
        ForeignFormat::TarZst => {
            "zstd frame checksum, when the archive has one; no per-entry check"
        }
    }
}

fn dimensions() -> ForeignDimensionsView {
    ForeignDimensionsView {
        recoverability: FOREIGN_DIMENSIONS.recoverability.to_owned(),
        freshness: FOREIGN_DIMENSIONS.freshness.to_owned(),
        retention: FOREIGN_DIMENSIONS.retention.to_owned(),
    }
}

fn kind_of(k: EntryKind) -> ForeignKind {
    match k {
        EntryKind::File => ForeignKind::File,
        EntryKind::Directory => ForeignKind::Directory,
        EntryKind::Symlink => ForeignKind::Symlink,
        EntryKind::HardLink => ForeignKind::HardLink,
        EntryKind::Other => ForeignKind::Other,
    }
}

fn core_entries(entries: &[EntryInfo]) -> Vec<ForeignEntry> {
    entries
        .iter()
        .map(|e| ForeignEntry {
            index: e.index,
            name: e.name.clone(),
            kind: kind_of(e.kind),
            size: e.size,
            mtime_unix: e.mtime_unix,
        })
        .collect()
}

fn selection(keys: &[PathKey]) -> Result<Vec<ArchivePath>> {
    if keys.len() > MAX_KEYS {
        return Err(invalid(format!(
            "an extract names at most {MAX_KEYS} entries"
        )));
    }
    keys.iter().map(|k| parse_key(k)).collect()
}

fn escaped(text: &str) -> NameText {
    NameText::of(text.as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Foreign {
    /// Use this helper binary instead of the one next to the executable.
    #[cfg(any(test, feature = "e2e"))]
    pub fn set_helper(&self, path: PathBuf) {
        *lock(&self.helper) = Some(path);
    }

    /// Record the foreign archives opened from now on in `recents` (once).
    pub fn set_recents(&self, recents: Arc<crate::recent::Recents>) {
        let _ = self.recents.set(recents);
    }

    fn helper_path(&self) -> PathBuf {
        lock(&self.helper)
            .clone()
            .unwrap_or_else(default_helper_path)
    }

    fn client(&self) -> ForeignClient {
        ForeignClient::new(self.helper_path())
    }

    /// The file of the open foreign archive `handle`.
    pub fn path(&self, handle: u32) -> Result<PathBuf> {
        Ok(self.session(handle)?.path.clone())
    }

    fn session(&self, handle: u32) -> Result<Arc<Session>> {
        lock(&self.inner)
            .open
            .get(&handle)
            .cloned()
            .ok_or_else(|| invalid("this archive is not open"))
    }

    /// The archive file a retry should use (set when a password was needed).
    pub fn pending(&self) -> Option<PathBuf> {
        self.pending_for(UNOWNED)
    }

    /// [`Foreign::pending`] for one window.
    pub fn pending_for(&self, owner: &str) -> Option<PathBuf> {
        lock(&self.inner).pending.get(owner).cloned()
    }

    pub fn open_count(&self) -> usize {
        lock(&self.inner).open.len()
    }

    /// The window an open archive belongs to.
    pub fn owner_of(&self, handle: u32) -> Option<String> {
        lock(&self.inner).owners.get(&handle).cloned()
    }

    /// Give an open archive to the window `owner`. `false` if it is not open.
    pub fn assign(&self, handle: u32, owner: &str) -> bool {
        let mut g = lock(&self.inner);
        if !g.open.contains_key(&handle) {
            return false;
        }
        g.owners.insert(handle, owner.to_owned());
        true
    }

    /// Whether the window has a foreign archive open or one waiting for a
    /// password.
    pub fn holds_any(&self, owner: &str) -> bool {
        let g = lock(&self.inner);
        g.owners.values().any(|o| o.as_str() == owner) || g.pending.contains_key(owner)
    }

    /// The file names of a window's open foreign archives, oldest first.
    pub fn file_names_of(&self, owner: &str) -> Vec<String> {
        let g = lock(&self.inner);
        let mut handles: Vec<u32> = g
            .owners
            .iter()
            .filter(|(_, o)| o.as_str() == owner)
            .map(|(h, _)| *h)
            .collect();
        handles.sort_unstable();
        handles
            .into_iter()
            .filter_map(|h| g.open.get(&h))
            .filter_map(|s| s.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect()
    }

    /// The window that has `path` open or is asking for its password.
    pub fn owner_of_file(&self, path: &Path) -> Option<String> {
        let g = lock(&self.inner);
        for (h, s) in &g.open {
            if crate::windows::same_file(&s.path, path) {
                return g.owners.get(h).cloned();
            }
        }
        g.pending
            .iter()
            .find(|(_, p)| crate::windows::same_file(p, path))
            .map(|(o, _)| o.clone())
    }

    /// Close every foreign archive of one window (forgetting passwords) and
    /// its waiting file. Other windows are untouched.
    pub fn close_owned_by(&self, owner: &str) -> usize {
        let mut g = lock(&self.inner);
        let handles: Vec<u32> = g
            .owners
            .iter()
            .filter(|(_, o)| o.as_str() == owner)
            .map(|(h, _)| *h)
            .collect();
        for h in &handles {
            g.open.remove(h);
            g.owners.remove(h);
        }
        g.pending.remove(owner);
        handles.len()
    }

    /// List `path` and keep it open. `password` is used for the listing (names
    /// may be encrypted) and kept for later reads. On `PASSWORD_REQUIRED` or
    /// `WRONG_PASSWORD` the path is remembered for the retry.
    pub fn open(
        &self,
        path: &Path,
        password: Option<Vec<u8>>,
    ) -> std::result::Result<ForeignArchiveSummary, IpcError> {
        self.open_as(UNOWNED, path, password)
    }

    /// [`Foreign::open`] for the window `owner`.
    pub fn open_as(
        &self,
        owner: &str,
        path: &Path,
        password: Option<Vec<u8>>,
    ) -> std::result::Result<ForeignArchiveSummary, IpcError> {
        {
            let g = lock(&self.inner);
            let mine = g.owners.values().filter(|o| o.as_str() == owner).count();
            if mine >= MAX_OPEN || g.open.len() >= MAX_OPEN_TOTAL {
                return Err(IpcError {
                    code: ErrorCode::LimitExceeded.as_str().to_owned(),
                    message: format!("at most {MAX_OPEN} archives can be open; close one first"),
                });
            }
        }
        let password = password.map(Password::new);
        let listing = self.client().list(path, password.as_ref());
        let listing = match listing {
            Ok(l) => l,
            Err(e) => {
                let mut g = lock(&self.inner);
                if matches!(
                    e.code,
                    mochi_foreign::ErrorCode::PasswordRequired
                        | mochi_foreign::ErrorCode::WrongPassword
                ) {
                    g.pending.insert(owner.to_owned(), path.to_path_buf());
                } else {
                    g.pending.remove(owner);
                }
                return Err(ipc_error(&e));
            }
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tree = ForeignTree::build(&core_entries(&listing.entries));
        let notes = listing
            .breaches
            .iter()
            .map(|b| {
                format!(
                    "{}: limit {}, observed {}{}",
                    b.kind.as_str(),
                    b.limit,
                    b.observed,
                    b.entry.map_or(String::new(), |e| format!(" (entry {e})"))
                )
            })
            .take(20)
            .collect();
        let weak = listing
            .entries
            .iter()
            .any(|e| e.encryption == mochi_foreign::Encryption::ZipCryptoWeak);
        if let Some(recents) = self.recents.get() {
            recents.record(path);
        }
        let mut g = lock(&self.inner);
        g.pending.remove(owner);
        g.last = g.last.wrapping_add(1);
        let handle = g.last;
        g.owners.insert(handle, owner.to_owned());
        let summary = ForeignArchiveSummary {
            handle,
            file_name,
            format: listing.format.label().to_owned(),
            entries: listing.entries.len() as u64,
            complete: listing.complete,
            stop_reason: listing.stop_reason.as_deref().map(escaped),
            notes,
            checks: checks_text(listing.format).to_owned(),
            opened_with_password: password.is_some(),
            weak_encryption: weak,
            dimensions: dimensions(),
        };
        g.open.insert(
            handle,
            Arc::new(Session {
                path: path.to_path_buf(),
                entries: listing.entries,
                tree,
                password,
            }),
        );
        Ok(summary)
    }

    pub fn close(&self, handle: u32) -> bool {
        let mut g = lock(&self.inner);
        g.owners.remove(&handle);
        g.open.remove(&handle).is_some()
    }

    /// One page of a directory's children (`dir` `None` for the root).
    pub fn children(
        &self,
        handle: u32,
        dir: Option<&str>,
        offset: u32,
        limit: u32,
    ) -> Result<ForeignChildrenPage> {
        let s = self.session(handle)?;
        let dir = dir.map(parse_key).transpose()?;
        if let Some(d) = &dir {
            if s.tree.get(d).is_none() {
                return Err(invalid("no such folder in this archive"));
            }
        }
        let limit = limit.clamp(1, 1000) as usize;
        let rows = s.tree.children(dir.as_ref());
        let total = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        let entries = rows
            .iter()
            .skip(offset as usize)
            .take(limit)
            .map(|r| row(&s, r))
            .collect();
        Ok(ForeignChildrenPage {
            total,
            offset,
            entries,
        })
    }

    /// Entries (folders and files) whose full path contains `query` as a byte
    /// substring, in path order. Names only: nothing is read from the archive.
    /// `ignore_case` folds ASCII letters only. An empty query finds nothing.
    /// Each hit carries the row `children` would return for it (same key), the
    /// full escaped path, and its folder's key.
    pub fn search(
        &self,
        handle: u32,
        query: &str,
        ignore_case: bool,
    ) -> Result<ForeignSearchResult> {
        if query.len() > MAX_SEARCH_QUERY_BYTES {
            return Err(invalid(format!(
                "a search takes at most {MAX_SEARCH_QUERY_BYTES} bytes of text"
            )));
        }
        let s = self.session(handle)?;
        let needle = query.as_bytes();
        if needle.is_empty() {
            return Ok(ForeignSearchResult {
                hits: Vec::new(),
                total_hits: 0,
            });
        }
        let found = |hay: &[u8]| {
            if ignore_case {
                hay.windows(needle.len())
                    .any(|w| w.eq_ignore_ascii_case(needle))
            } else {
                hay.windows(needle.len()).any(|w| w == needle)
            }
        };
        let mut hits = Vec::new();
        let mut total: u32 = 0;
        for r in s.tree.rows() {
            // A refused name has no path: its raw name is what is shown.
            let full: &[u8] = r.path.as_ref().map_or(&r.name, |p| p.as_stored());
            if !found(full) {
                continue;
            }
            total = total.saturating_add(1);
            if hits.len() < MAX_SEARCH_HITS {
                hits.push(ForeignHit {
                    row: row(&s, r),
                    path: NameText::of(full),
                    parent: r
                        .path
                        .as_ref()
                        .and_then(|p| p.parent())
                        .map(|p| hex(p.as_stored())),
                });
            }
        }
        Ok(ForeignSearchResult {
            hits,
            total_hits: total,
        })
    }

    /// What extracting `keys` would meet in the folder, before anything is
    /// written. `picked` is a folder the user just chose.
    pub fn plan(
        &self,
        destination: &Destination,
        picked: Option<PathBuf>,
        handle: u32,
        keys: &[PathKey],
    ) -> Result<ForeignExtractPlan> {
        let s = self.session(handle)?;
        let folder = match &picked {
            Some(p) => p.clone(),
            None => destination
                .get()
                .ok_or_else(|| invalid("choose the folder to extract into first"))?,
        };
        let wanted = selection(keys)?;
        let mut root = OsRestoreDir::open(&folder)?;
        // Looked at as "Replace" would (see `extract::plan`). Nothing is written.
        let plan = plan_foreign(
            &core_entries(&s.entries),
            &wanted,
            &mut root,
            mochi_core::restore::OnExisting::Replace,
        )?;
        if let Some(p) = picked {
            destination.set(p);
        }
        let mut conflicts: Vec<ConflictView> = Vec::new();
        let mut push = |path: NameText, code: ErrorCode, reason: String, why: ConflictWhy| {
            conflicts.push(ConflictView {
                path,
                code: code.as_str().to_owned(),
                reason,
                why,
            });
        };
        for r in plan
            .refusals
            .iter()
            .filter(|r| r.reason.code() == ErrorCode::NameUnsupported)
        {
            push(
                NameText::of(&r.name),
                r.reason.code(),
                r.reason.describe(),
                ConflictWhy::NameNotAllowed,
            );
        }
        for (path, kind) in &plan.preflight.exceptions {
            push(
                NameText::of(path.as_stored()),
                kind.code(),
                reason(kind).text,
                crate::extract::conflict_why(&plan.preflight, path, kind),
            );
        }
        for d in &plan.duplicates {
            push(
                NameText::of(d.as_stored()),
                ErrorCode::NameCollision,
                "the archive holds this path more than once; only the first is extracted"
                    .to_owned(),
                ConflictWhy::SameNameInArchive,
            );
        }
        let total = conflicts.len();
        conflicts.truncate(MAX_CONFLICTS_SHOWN);
        let skipped_kinds = plan
            .refusals
            .iter()
            .filter(|r| r.reason.code() != ErrorCode::NameUnsupported)
            .count();
        Ok(ForeignExtractPlan {
            plan: ExtractPlan {
                destination: folder.display().to_string(),
                entries: u32::try_from(plan.items).unwrap_or(u32::MAX),
                existing: plan
                    .preflight
                    .replaced
                    .iter()
                    .take(MAX_CONFLICTS_SHOWN)
                    .map(|p| NameText::of(p.as_stored()))
                    .collect(),
                existing_total: u32::try_from(plan.preflight.replaced.len()).unwrap_or(u32::MAX),
                merged_folders: u32::try_from(plan.preflight.merged.len()).unwrap_or(u32::MAX),
                conflicts,
                conflicts_total: u32::try_from(total).unwrap_or(u32::MAX),
                case_insensitive: plan.preflight.case_behavior == CaseBehavior::Insensitive,
            },
            skipped_kinds: u32::try_from(skipped_kinds).unwrap_or(u32::MAX),
        })
    }

    /// Plan extracting `keys` into a new folder named after the archive in
    /// `parent` (the folder the user chose), made now. The folder is removed again if the
    /// plan fails.
    pub fn plan_in_new_folder(
        &self,
        destination: &Destination,
        parent: &Path,
        handle: u32,
        keys: &[PathKey],
    ) -> Result<ForeignExtractPlan> {
        let s = self.session(handle)?;
        let folder = destination.make_new_in(parent, &s.path)?;
        match self.plan(destination, Some(folder), handle, keys) {
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

    /// Refuse a request that cannot work, before a job starts.
    pub fn validate(&self, destination: &Destination, request: &JobRequest) -> Result<()> {
        match request {
            JobRequest::TestForeign { handle } => self.session(*handle).map(|_| ()),
            JobRequest::ExtractForeign { handle, keys, .. } => {
                self.session(*handle)?;
                selection(keys)?;
                if destination.get().is_none() {
                    return Err(invalid("choose the folder to extract into first"));
                }
                Ok(())
            }
            _ => Err(invalid("not a foreign-archive request")),
        }
    }

    /// Run a foreign-archive job.
    pub fn run(
        &self,
        destination: &Destination,
        request: &JobRequest,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        match request {
            JobRequest::TestForeign { handle } => self.test(*handle, ctx),
            JobRequest::ExtractForeign {
                handle,
                keys,
                on_conflict,
            } => {
                let done = self.extract(destination, *handle, keys, *on_conflict, ctx)?;
                destination.record_success();
                Ok(done)
            }
            _ => Err(invalid("not a foreign-archive request")),
        }
    }

    fn job_client(&self, ctx: &JobContext<'_>) -> ForeignClient {
        self.client().with_cancel(ctx.cancel.flag())
    }

    fn test(&self, handle: u32, ctx: &JobContext<'_>) -> Result<JobDone> {
        let s = self.session(handle)?;
        let total = s.entries.len() as u64;
        ctx.report("foreign_test", 0, Some(total));
        let client = self.job_client(ctx);
        let report = client
            .test_with_progress(&s.path, s.password.as_ref(), &mut |n| {
                ctx.report("foreign_test", n, Some(total));
            })
            .map_err(|e| mochi_error(&e))?;
        ctx.check_cancelled()?;
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut problems = Vec::new();
        let mut problems_total = 0u64;
        for e in &report.entries {
            *counts.entry(e.outcome.describe()).or_default() += 1;
            if !e.outcome.is_clean() {
                problems_total += 1;
                if problems.len() < MAX_PROBLEMS_SHOWN {
                    problems.push(ForeignProblem {
                        path: NameText::of(&e.info.name),
                        what: escaped(&e.outcome.describe()),
                    });
                }
            }
        }
        let stream_failed = matches!(report.stream_check, StreamCheck::Failed(_));
        if stream_failed {
            problems_total += 1;
        }
        let checks: Vec<CheckCount> = counts
            .into_iter()
            .map(|(what, count)| CheckCount { what, count })
            .collect();
        let summary = format!(
            "read {} entr{} of the {} archive; {} problem{}{}. This reports the checks the \
             format has; it is not a verification of the archive",
            report.entries.len(),
            if report.entries.len() == 1 {
                "y"
            } else {
                "ies"
            },
            report.format.label(),
            problems_total,
            if problems_total == 1 { "" } else { "s" },
            if report.complete {
                String::new()
            } else {
                "; the run stopped early, so it covers only part of the archive".to_owned()
            }
        );
        let view = ForeignTestReport {
            format: report.format.label().to_owned(),
            entries: report.entries.len() as u64,
            checks,
            problems,
            problems_total,
            stream_check: report.stream_check.describe(),
            complete: report.complete,
            stop_reason: report.stop_reason.as_deref().map(escaped),
            bytes_read: report.bytes_read,
            dimensions: dimensions(),
        };
        Ok(JobDone {
            summary,
            commit: None,
            extract: None,
            foreign_test: Some(Box::new(view)),
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
            check: None,
        })
    }

    fn extract(
        &self,
        destination: &Destination,
        handle: u32,
        keys: &[PathKey],
        on_conflict: ConflictChoice,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let s = self.session(handle)?;
        let wanted = selection(keys)?;
        let folder = destination
            .get()
            .ok_or_else(|| invalid("choose the folder to extract into first"))?;
        let root = OsRestoreDir::open(&folder)?.with_set_aside(crate::trashbin::set_aside());
        let entries = core_entries(&s.entries);
        let options = ForeignRestoreOptions {
            refuse_on_exceptions: on_conflict == ConflictChoice::Stop,
            skip_file_sync: crate::speed::skip_file_sync(),
            on_existing: on_conflict.on_existing(),
        };
        let mut restorer = ForeignRestorer::new(&entries, &wanted, root, options, ctx)?;
        let indices = restorer.wanted();
        let mut seen: BTreeMap<String, u64> = BTreeMap::new();
        let mut stopped: Option<String> = None;
        if !indices.is_empty() {
            let client = self.job_client(ctx);
            let mut sink = RestoreSink {
                restorer: &mut restorer,
                ctx,
                seen: &mut seen,
                error: None,
            };
            let outcome = client.read_many(&s.path, &indices, s.password.as_ref(), &mut sink);
            let sink_error = sink.error.take();
            match outcome {
                Ok(r) => {
                    if !r.complete {
                        stopped = Some(
                            r.stop_reason
                                .unwrap_or_else(|| "the reader stopped early".to_owned()),
                        );
                    }
                }
                Err(e) => {
                    // Cancellation wins over whatever the sink or the helper
                    // reported: the file being written is discarded and the
                    // job ends as cancelled.
                    if ctx.cancel.is_cancelled() || e.code == mochi_foreign::ErrorCode::Cancelled {
                        restorer.abandon("cancelled");
                        return Err(MochiError::new(
                            ErrorCode::Cancelled,
                            "extraction cancelled",
                        ));
                    }
                    if let Some(me) = sink_error {
                        restorer.abandon("the destination refused the write");
                        return Err(me);
                    }
                    stopped = Some(format!("{}: {}", e.code.as_str(), e.message));
                }
            }
        }
        ctx.check_cancelled()?;
        let why = stopped
            .clone()
            .unwrap_or_else(|| "the archive did not deliver it".to_owned());
        let done = restorer.finish(&why)?;
        let mut view = report_view(&done.report, &folder);
        for r in &done.refusals {
            view.not_extracted_total += 1;
            if view.not_extracted.len() < MAX_NOT_EXTRACTED_SHOWN {
                view.not_extracted.push(NotExtractedView {
                    path: NameText::of(&r.name),
                    code: r.reason.code().as_str().to_owned(),
                    reason: escaped(&format!("not extracted: {}", r.reason.describe())),
                });
            }
        }
        let checks = seen
            .iter()
            .map(|(what, n)| format!("{what} ({n})"))
            .collect::<Vec<_>>()
            .join("; ");
        let summary = format!(
            "{}extracted {} file{} and {} folder{} to {}{}{}",
            stopped
                .as_ref()
                .map_or(String::new(), |w| format!("stopped early ({w}); ")),
            view.files,
            if view.files == 1 { "" } else { "s" },
            view.directories,
            if view.directories == 1 { "" } else { "s" },
            view.destination,
            if view.not_extracted_total == 0 {
                String::new()
            } else {
                format!("; {} not extracted", view.not_extracted_total)
            },
            if checks.is_empty() {
                String::new()
            } else {
                format!(". What each file was checked with: {checks}. This is not a verification")
            }
        );
        Ok(JobDone {
            summary,
            commit: None,
            extract: Some(Box::new(view)),
            foreign_test: None,
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
            check: None,
        })
    }
}

/// Feeds what the helper streams into the restorer, one file at a time.
struct RestoreSink<'a, 'c, 'j> {
    restorer: &'a mut ForeignRestorer<'c, 'j, OsRestoreDir>,
    ctx: &'a JobContext<'j>,
    /// How each delivered file ended, in the format's own words, with counts.
    seen: &'a mut BTreeMap<String, u64>,
    /// An error from the destination, to report as itself.
    error: Option<MochiError>,
}

impl RestoreSink<'_, '_, '_> {
    fn fail(&mut self, e: MochiError) -> io::Error {
        let msg = e.to_string();
        self.error = Some(e);
        io::Error::other(msg)
    }
}

impl ReadManySink for RestoreSink<'_, '_, '_> {
    fn begin(&mut self, info: &EntryInfo) -> io::Result<()> {
        self.restorer
            .begin_file(info.index)
            .map_err(|e| self.fail(e))
    }

    fn data(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Cancellation is also noticed here, between chunks of a big file.
        if self.ctx.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        self.restorer.write(bytes).map_err(|e| self.fail(e))
    }

    fn finish(&mut self, _info: &EntryInfo, outcome: &EntryOutcome) -> io::Result<()> {
        *self.seen.entry(outcome.describe()).or_default() += 1;
        let verdict = if outcome.is_clean() {
            Ok(())
        } else if matches!(outcome, EntryOutcome::Mismatch { .. }) {
            Err(FileFailure::CheckFailed(outcome.describe()))
        } else {
            Err(FileFailure::Unreadable(outcome.describe()))
        };
        self.restorer.end_file(verdict).map_err(|e| self.fail(e))
    }
}

fn kind_view(k: ForeignKind) -> ForeignKindView {
    match k {
        ForeignKind::File => ForeignKindView::File,
        ForeignKind::Directory => ForeignKindView::Directory,
        ForeignKind::Symlink | ForeignKind::HardLink => ForeignKindView::Link,
        ForeignKind::Other => ForeignKindView::Special,
    }
}

fn row(s: &Session, r: &TreeRow) -> ForeignRow {
    let info = r
        .index
        .and_then(|i| s.entries.get(usize::try_from(i).ok()?));
    let encryption = info
        .filter(|e| e.encryption != mochi_foreign::Encryption::None)
        .map(|e| e.encryption.label());
    ForeignRow {
        key: r.path.as_ref().map(|p| hex(p.as_stored())),
        name: NameText::of(&r.name),
        kind: kind_view(r.kind),
        size: r.size.map(|n| n.to_string()),
        child_dirs: r.child_dirs,
        child_files: r.child_files,
        encryption,
        note: r
            .refused
            .as_ref()
            .map(|why: &RefusalReason| format!("will not be extracted: {}", why.describe())),
    }
}

/// Why an entry was not extracted, in foreign wording.
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
            "the format's own check did not pass, so the file was not written: {message}"
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

fn report_view(report: &mochi_core::restore::RestoreReport, folder: &Path) -> ExtractReport {
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
    let mut by_kind: BTreeMap<AttributeKind, (u64, &_)> = BTreeMap::new();
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
        seq: 0,
        files: report.files,
        directories: report.directories,
        bytes: report.bytes,
        not_extracted,
        not_extracted_total: report.exceptions.len() as u64,
        attribute_notes,
        attributes_unavailable: None,
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

impl Foreign {
    /// Drag-out (`dragout`): the bytes the helper declares for the files at
    /// or under `keys`, before anything is extracted. Declared sizes are the
    /// archive's own claim, so this bounds the work only as far as the
    /// helper's limits hold it to them. `INVALID_ARGUMENT` for a key the
    /// listing lacks.
    pub fn declared_bytes(&self, handle: u32, keys: &[PathKey]) -> Result<u64> {
        let s = self.session(handle)?;
        let wanted = selection(keys)?;
        if let Some(missing) = wanted.iter().find(|w| s.tree.get(w).is_none()) {
            return Err(invalid(format!(
                "no entry {:?} in this archive",
                String::from_utf8_lossy(missing.as_stored())
            )));
        }
        let wanted: std::collections::HashSet<ArchivePath> = wanted.into_iter().collect();
        let mut total = 0u64;
        for e in s.entries.iter().filter(|e| e.kind == EntryKind::File) {
            let Ok(Some(path)) = mochi_core::restore_foreign::parse_name(&e.name) else {
                continue;
            };
            let mut at = Some(path);
            while let Some(p) = at {
                if wanted.contains(&p) {
                    total = total.saturating_add(e.size.unwrap_or(0));
                    break;
                }
                at = p.parent();
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    //! The desktop adapter against the real helper binary: listing, paging,
    //! passwords, the foreign test report's wording, and extraction through
    //! the core engine, including the hostile fixtures.
    use mochi_core::job::{CancellationToken, NullProgress};
    use mochi_testkit::archive::Job;

    use super::*;

    const BANNED: [&str; 5] = ["backed up", "backup", "safe", "preserved", "secure"];

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/foreign")
    }

    /// The helper binary next to the test executable; built on demand when
    /// only this package's tests were asked for.
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

    fn service() -> Foreign {
        let f = Foreign::default();
        f.set_helper(helper());
        f
    }

    fn fx(name: &str) -> PathBuf {
        fixtures().join(name)
    }

    fn hex_key(s: &str) -> String {
        hex(s.as_bytes())
    }

    struct Dest {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
        destination: Destination,
    }

    fn dest() -> Dest {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("out");
        std::fs::create_dir(&dir).unwrap();
        let destination = Destination::default();
        destination.set(dir.clone());
        Dest {
            _tmp: tmp,
            dir,
            destination,
        }
    }

    fn names(page: &ForeignChildrenPage) -> Vec<String> {
        page.entries.iter().map(|e| e.name.text.clone()).collect()
    }

    fn run(f: &Foreign, d: &Dest, req: &JobRequest) -> std::result::Result<JobDone, MochiError> {
        f.validate(&d.destination, req)?;
        let job = Job::new();
        f.run(&d.destination, req, &job.ctx())
    }

    fn extract(handle: u32, keys: &[&str], c: ConflictChoice) -> JobRequest {
        JobRequest::ExtractForeign {
            handle,
            keys: keys.iter().map(|k| hex_key(k)).collect(),
            on_conflict: c,
        }
    }

    #[test]
    fn a_zip_opens_read_only_with_the_foreign_labels_and_pages() {
        let f = service();
        let s = f.open(&fx("basic_infozip.zip"), None).unwrap();
        assert_eq!(s.format, "ZIP");
        assert_eq!(s.entries, 3);
        assert!(s.complete && s.stop_reason.is_none());
        assert!(s.checks.contains("CRC32"));
        // Recoverability, freshness and retention do not exist for ZIP.
        assert_eq!(s.dimensions.recoverability, "UNSUPPORTED");
        assert_eq!(s.dimensions.freshness, "UNSUPPORTED");
        assert_eq!(s.dimensions.retention, "UNSUPPORTED");
        let root = f.children(s.handle, None, 0, 100).unwrap();
        assert_eq!(names(&root), vec!["empty", "sub", "a.txt"]);
        assert_eq!(root.entries[1].child_files, 1);
        let sub = f
            .children(
                s.handle,
                Some(&root.entries[1].key.clone().unwrap()),
                0,
                100,
            )
            .unwrap();
        assert_eq!(names(&sub), vec!["b.txt"]);
        assert_eq!(sub.entries[0].size.as_deref(), Some("600"));
        // Paging.
        let p = f.children(s.handle, None, 1, 1).unwrap();
        assert_eq!((p.total, p.entries.len()), (3, 1));
        assert!(f.children(s.handle, Some(&hex_key("nope")), 0, 10).is_err());
        assert!(f.children(99, None, 0, 10).is_err());
        assert!(f.close(s.handle) && !f.close(s.handle));
    }

    #[test]
    fn hostile_names_are_listed_as_text_and_marked_not_extractable() {
        let f = service();
        let s = f.open(&fx("traversal.zip"), None).unwrap();
        let root = f.children(s.handle, None, 0, 1000).unwrap();
        let evil = root
            .entries
            .iter()
            .find(|e| e.name.text.contains("evil.txt"))
            .unwrap();
        assert!(evil.key.is_none(), "a refused name cannot be selected");
        assert!(evil
            .note
            .as_deref()
            .unwrap()
            .contains("will not be extracted"));
    }

    #[test]
    fn a_password_is_asked_for_before_listing_and_a_wrong_one_is_refused() {
        let f = service();
        let p = fx("7z/enc_names.7z");
        let e = f.open(&p, None).unwrap_err();
        assert_eq!(e.code, "PASSWORD_REQUIRED");
        assert_eq!(
            f.pending().as_deref(),
            Some(p.as_path()),
            "kept for the retry"
        );
        assert_eq!(f.open_count(), 0, "nothing is open");
        let e = f.open(&p, Some(b"nope".to_vec())).unwrap_err();
        assert_eq!(e.code, "WRONG_PASSWORD");
        assert!(f.pending().is_some());
        let s = f.open(&p, Some(b"SECRET".to_vec())).unwrap();
        assert!(s.opened_with_password);
        assert!(f.pending().is_none());
        assert_eq!(s.format, "7z");
        // Not found / malformed answers do not leave a pending file.
        assert_eq!(
            f.open(&fx("garbage.bin"), None).unwrap_err().code,
            "NOT_AN_ARCHIVE"
        );
        assert!(f.pending().is_none());
    }

    #[test]
    fn a_test_job_names_the_checks_and_never_uses_verification_wording() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("7z/crc_corrupt.7z"), None).unwrap();
        let done = run(&f, &d, &JobRequest::TestForeign { handle: s.handle }).unwrap();
        let r = done.foreign_test.unwrap();
        assert_eq!(r.format, "7z");
        assert_eq!(r.problems_total, 1);
        assert_eq!(r.problems.len(), 1);
        assert!(r.problems[0].what.text.contains("CRC32 did NOT match"));
        assert!(r
            .checks
            .iter()
            .any(|c| c.what == "CRC32 matched" && c.count >= 1));
        assert_eq!(r.dimensions.recoverability, "UNSUPPORTED");
        assert!(done.summary.contains("not a verification"));
        let text = format!("{} {:?}", done.summary, r).to_lowercase();
        for w in BANNED {
            assert!(!text.contains(w), "banned wording {w:?} in {text}");
        }
        for w in ["pass", "verified", "healthy", "intact"] {
            // "passed" appears nowhere either: only the check's own words.
            assert!(
                !done.summary.to_lowercase().contains(w),
                "{w}: {}",
                done.summary
            );
        }
    }

    #[test]
    fn a_tar_gz_test_reports_the_stream_check_and_a_bad_trailer() {
        let f = service();
        let d = dest();
        let ok = f.open(&fx("basic.tar.gz"), None).unwrap();
        let r = run(&f, &d, &JobRequest::TestForeign { handle: ok.handle })
            .unwrap()
            .foreign_test
            .unwrap();
        assert!(
            r.stream_check.contains("gzip stream CRC32 matched"),
            "{}",
            r.stream_check
        );
        assert!(r
            .checks
            .iter()
            .any(|c| c.what.contains("no per-entry check")));
        let bad = f.open(&fx("gz_crc_corrupt.tar.gz"), None).unwrap();
        let r = run(&f, &d, &JobRequest::TestForeign { handle: bad.handle })
            .unwrap()
            .foreign_test
            .unwrap();
        assert!(r.stream_check.contains("failed"), "{}", r.stream_check);
        assert!(r.problems_total >= 1);
    }

    #[test]
    fn extracting_a_7z_writes_identical_bytes_through_the_core_engine() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("7z/basic.7z"), None).unwrap();
        let plan = f.plan(&d.destination, None, s.handle, &[]).unwrap();
        assert!(plan.plan.conflicts.is_empty());
        let done = run(&f, &d, &extract(s.handle, &[], ConflictChoice::Stop)).unwrap();
        let r = done.extract.unwrap();
        assert_eq!(r.not_extracted_total, 0);
        assert_eq!(r.files, 3); // a.txt, sub/b.txt, zero.txt
        assert_eq!(std::fs::read(d.dir.join("a.txt")).unwrap(), b"hello\n");
        assert_eq!(
            std::fs::read(d.dir.join("sub/b.txt")).unwrap(),
            "second file\n".repeat(50).as_bytes()
        );
        assert!(d.dir.join("empty").is_dir());
        assert!(done.summary.contains("CRC32 matched"), "{}", done.summary);
        assert!(done.summary.contains("not a verification"));
        // A second run collides with everything and writes nothing new.
        let again = run(
            &f,
            &d,
            &extract(s.handle, &[], ConflictChoice::SkipConflicts),
        )
        .unwrap();
        assert!(again.extract.unwrap().not_extracted_total > 0);
        assert_eq!(std::fs::read(d.dir.join("a.txt")).unwrap(), b"hello\n");
    }

    #[test]
    fn a_selection_extracts_only_that_subtree() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("basic_infozip.zip"), None).unwrap();
        run(&f, &d, &extract(s.handle, &["sub"], ConflictChoice::Stop)).unwrap();
        assert!(d.dir.join("sub/b.txt").exists());
        assert!(!d.dir.join("a.txt").exists());
    }

    #[test]
    fn hostile_archives_write_nothing_outside_the_folder_and_say_what_was_skipped() {
        for name in ["traversal.zip", "traversal.tar", "7z/traversal.7z"] {
            let f = service();
            let d = dest();
            let parent = d.dir.parent().unwrap().to_path_buf();
            let before: Vec<_> = std::fs::read_dir(&parent)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            let s = f.open(&fx(name), None).unwrap();
            let plan = f.plan(&d.destination, None, s.handle, &[]).unwrap();
            assert!(
                plan.plan.conflicts_total >= 1,
                "{name}: the plan names what is refused"
            );
            let done = run(
                &f,
                &d,
                &extract(s.handle, &[], ConflictChoice::SkipConflicts),
            )
            .unwrap();
            let r = done.extract.unwrap();
            assert!(r.not_extracted_total >= 1, "{name}");
            let after: Vec<_> = std::fs::read_dir(&parent)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(before, after, "{name}: nothing appeared beside the folder");
            for e in &r.not_extracted {
                if e.path.text.contains("evil.txt") {
                    assert!(
                        e.reason.text.contains("not extracted"),
                        "{name}: {}",
                        e.reason.text
                    );
                }
            }
            // Stop refuses before writing anything when a name is refused.
            let d2 = dest();
            let stop = run(&f, &d2, &extract(s.handle, &[], ConflictChoice::Stop));
            assert!(stop.is_err(), "{name}");
            assert_eq!(std::fs::read_dir(&d2.dir).unwrap().count(), 0, "{name}");
        }
    }

    #[test]
    fn links_in_a_tar_are_skipped_and_counted_never_created() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("traversal.tar"), None).unwrap();
        let plan = f.plan(&d.destination, None, s.handle, &[]).unwrap();
        assert!(plan.skipped_kinds >= 2, "symlink, hard link, device, fifo");
        run(
            &f,
            &d,
            &extract(s.handle, &[], ConflictChoice::SkipConflicts),
        )
        .unwrap();
        fn any_link(p: &Path) -> bool {
            std::fs::read_dir(p).unwrap().any(|e| {
                let e = e.unwrap();
                let t = e.file_type().unwrap();
                t.is_symlink() || (t.is_dir() && any_link(&e.path()))
            })
        }
        assert!(!any_link(&d.dir));
    }

    #[test]
    fn a_wrong_password_extracts_no_encrypted_file_and_no_partial_output() {
        let f = service();
        let d = dest();
        let s = f
            .open(&fx("7z/enc_data.7z"), Some(b"not-it".to_vec()))
            .unwrap();
        let done = run(
            &f,
            &d,
            &extract(s.handle, &[], ConflictChoice::SkipConflicts),
        )
        .unwrap();
        let r = done.extract.unwrap();
        // The two encrypted files are reported; only the empty one (no data,
        // so nothing to decrypt) exists, and no temporary file is left.
        assert_eq!(r.not_extracted_total, 2, "{:?}", r.not_extracted);
        assert_eq!(r.files, 1);
        assert!(!d.dir.join("a.txt").exists() && !d.dir.join("sub/b.txt").exists());
        fn files(p: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(p).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    files(&e.path(), out);
                } else {
                    out.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        let mut left = Vec::new();
        files(&d.dir, &mut left);
        assert_eq!(left, vec!["zero.txt".to_string()], "{left:?}");
        // The right password extracts.
        let d2 = dest();
        let ok = f
            .open(&fx("7z/enc_data.7z"), Some(b"SECRET".to_vec()))
            .unwrap();
        let done = run(&f, &d2, &extract(ok.handle, &[], ConflictChoice::Stop)).unwrap();
        assert_eq!(done.extract.unwrap().files, 3);
    }

    #[test]
    fn a_corrupt_entry_is_not_written_and_is_reported_in_the_formats_words() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("7z/crc_corrupt.7z"), None).unwrap();
        let done = run(
            &f,
            &d,
            &extract(s.handle, &[], ConflictChoice::SkipConflicts),
        )
        .unwrap();
        let r = done.extract.unwrap();
        assert_eq!(r.not_extracted_total, 1);
        assert!(r.not_extracted[0]
            .reason
            .text
            .contains("CRC32 did NOT match"));
        assert!(!d.dir.join("sub/b.txt").exists());
        assert!(d.dir.join("a.txt").exists());
    }

    #[test]
    fn a_rar_extracts_with_its_check_named() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("rar/version.rar"), None).unwrap();
        assert_eq!(s.format, "RAR");
        let done = run(&f, &d, &extract(s.handle, &[], ConflictChoice::Stop)).unwrap();
        assert_eq!(done.extract.unwrap().files, 1);
        assert!(done.summary.contains("CRC32 matched"));
    }

    #[test]
    fn cancelling_stops_the_job_with_nothing_half_written() {
        let f = service();
        let d = dest();
        let s = f.open(&fx("7z/basic.7z"), None).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let progress = NullProgress;
        let ctx = JobContext {
            progress: &progress,
            cancel: &cancel,
        };
        let e = f
            .run(
                &d.destination,
                &extract(s.handle, &[], ConflictChoice::Stop),
                &ctx,
            )
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::Cancelled);
        let e = f
            .run(
                &d.destination,
                &JobRequest::TestForeign { handle: s.handle },
                &ctx,
            )
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(std::fs::read_dir(&d.dir).unwrap().count(), 0);
    }

    #[test]
    fn requests_are_validated_before_a_job_starts() {
        let f = service();
        let d = Dest {
            destination: Destination::default(),
            ..dest()
        };
        let s = f.open(&fx("basic.tar"), None).unwrap();
        assert!(f
            .validate(&d.destination, &JobRequest::TestForeign { handle: 99 })
            .is_err());
        assert!(f
            .validate(
                &Destination::default(),
                &extract(s.handle, &[], ConflictChoice::Stop)
            )
            .is_err());
        assert!(f
            .validate(
                &d.destination,
                &JobRequest::ExtractForeign {
                    handle: s.handle,
                    keys: vec!["zz".into()],
                    on_conflict: ConflictChoice::Stop
                }
            )
            .is_err());
    }

    #[test]
    fn zipcrypto_is_flagged_weak_and_the_open_limit_holds() {
        let f = service();
        let s = f.open(&fx("zipcrypto.zip"), None).unwrap();
        assert!(s.weak_encryption);
        let mut handles = vec![s.handle];
        for _ in 1..MAX_OPEN {
            handles.push(f.open(&fx("basic.tar"), None).unwrap().handle);
        }
        let e = f.open(&fx("basic.tar"), None).unwrap_err();
        assert_eq!(e.code, "LIMIT_EXCEEDED");
        assert!(f.close(handles[0]));
        assert!(f.open(&fx("basic.tar"), None).is_ok());
    }

    #[test]
    fn a_tar_zst_opens_with_its_own_label_and_honest_checks() {
        let f = service();
        let s = f.open(&fx("basic.tar.zst"), None).unwrap();
        assert_eq!(s.format, "tar.zst");
        assert_eq!(s.entries, 3);
        assert!(s.complete);
        assert!(s.checks.contains("zstd frame checksum") && s.checks.contains("no per-entry"));
        for banned in BANNED {
            assert!(!s.checks.contains(banned), "{banned}");
        }
        let root = f.children(s.handle, None, 0, 100).unwrap();
        assert_eq!(names(&root), vec!["d", "a.txt"]);
    }

    #[test]
    fn recent_foreign_archives_are_recorded_once_opened() {
        let f = service();
        let recents = Arc::new(crate::recent::Recents::default());
        f.set_recents(Arc::clone(&recents));
        // A failed open records nothing.
        assert!(f.open(&fx("garbage.bin"), None).is_err());
        assert!(recents.view().is_empty());
        f.open(&fx("basic.tar"), None).unwrap();
        assert_eq!(recents.view()[0].name, "basic.tar");
    }

    fn hit_paths(r: &ForeignSearchResult) -> Vec<String> {
        r.hits.iter().map(|h| h.path.text.clone()).collect()
    }

    #[test]
    fn a_search_finds_entries_by_full_path_with_the_listing_rows_and_folder_keys() {
        let f = service();
        let s = f.open(&fx("basic_infozip.zip"), None).unwrap();
        let r = f.search(s.handle, "b.txt", false).unwrap();
        assert_eq!(
            (r.total_hits, hit_paths(&r)),
            (1, vec!["sub/b.txt".to_owned()])
        );
        let hit = &r.hits[0];
        assert_eq!(hit.parent, Some(hex_key("sub")));
        // The row is exactly what foreign_children returns for the entry.
        let sub = f.children(s.handle, hit.parent.as_deref(), 0, 100).unwrap();
        assert_eq!(sub.entries, vec![hit.row.clone()]);
        assert_eq!(hit.row.key, Some(hex_key("sub/b.txt")));
        // A match in a folder name finds the folder and what is in it, in
        // path order; a root-level entry has no parent.
        let r = f.search(s.handle, "sub", false).unwrap();
        assert_eq!(hit_paths(&r), vec!["sub", "sub/b.txt"]);
        assert_eq!(r.hits[0].parent, None);
        assert_eq!(r.hits[0].row.key, Some(hex_key("sub")));
        assert_eq!(r.hits[0].row.child_files, 1);
        // A path separator is part of the path being searched.
        assert_eq!(
            hit_paths(&f.search(s.handle, "ub/b", false).unwrap()),
            vec!["sub/b.txt"]
        );
        // Case: exact unless asked, and ASCII only.
        assert_eq!(f.search(s.handle, "SUB", false).unwrap().total_hits, 0);
        assert_eq!(f.search(s.handle, "SUB", true).unwrap().total_hits, 2);
        // An empty query finds nothing; nothing matching is an empty result.
        let none = f.search(s.handle, "", true).unwrap();
        assert_eq!((none.total_hits, none.hits.len()), (0, 0));
        assert_eq!(f.search(s.handle, "zzz", true).unwrap().total_hits, 0);
        // Bad requests are refused.
        assert!(f.search(99, "a", false).is_err());
        assert!(f
            .search(s.handle, &"a".repeat(MAX_SEARCH_QUERY_BYTES + 1), false)
            .is_err());
    }

    #[test]
    fn a_search_returns_refused_names_unselectable() {
        let f = service();
        let s = f.open(&fx("traversal.zip"), None).unwrap();
        let r = f.search(s.handle, "evil", false).unwrap();
        assert!(r.total_hits >= 1);
        let evil = r
            .hits
            .iter()
            .find(|h| h.path.text.contains("evil.txt"))
            .unwrap();
        assert!(evil.row.key.is_none() && evil.parent.is_none());
        assert!(evil.row.note.is_some());
        // The same row the root listing shows for it.
        let root = f.children(s.handle, None, 0, 1000).unwrap();
        assert!(root.entries.contains(&evil.row));
    }

    #[test]
    fn a_search_returns_at_most_a_thousand_hits_and_counts_them_all() {
        // A tar of 1,100 empty files, written by hand (ustar headers).
        fn header(name: &str) -> [u8; 512] {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..107].copy_from_slice(b"0000644");
            h[108..115].copy_from_slice(b"0000000");
            h[116..123].copy_from_slice(b"0000000");
            h[124..135].copy_from_slice(b"00000000000");
            h[136..147].copy_from_slice(b"00000000000");
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
            h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            h[155] = b' ';
            h
        }
        let mut tar = Vec::new();
        for i in 0..1100 {
            tar.extend_from_slice(&header(&format!("many/file-{i:04}.dat")));
        }
        tar.extend_from_slice(&[0u8; 1024]);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("many.tar");
        std::fs::write(&p, tar).unwrap();
        let f = service();
        let s = f.open(&p, None).unwrap();
        assert_eq!(s.entries, 1100);
        let r = f.search(s.handle, "file-", false).unwrap();
        assert_eq!((r.hits.len(), r.total_hits), (MAX_SEARCH_HITS, 1100));
        assert_eq!(r.hits[0].path.text, "many/file-0000.dat");
        assert_eq!(r.hits[999].path.text, "many/file-0999.dat");
        // The folder is a hit of its own when the text matches it.
        let r = f.search(s.handle, "many", true).unwrap();
        assert_eq!(r.total_hits, 1101);
        assert_eq!(r.hits[0].path.text, "many");
    }

    #[test]
    fn a_search_never_reads_file_contents() {
        // basic.tar's b.txt holds "second file"; its name does not.
        let f = service();
        let s = f.open(&fx("basic.tar"), None).unwrap();
        assert_eq!(
            f.search(s.handle, "second file", true).unwrap().total_hits,
            0
        );
        assert_eq!(f.search(s.handle, "hello", true).unwrap().total_hits, 0);
        assert_eq!(f.search(s.handle, "A.TXT", true).unwrap().total_hits, 1);
    }
}
