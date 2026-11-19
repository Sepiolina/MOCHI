//! Retrieve what MOCHI Find found (spec Annex B.2.21; owner request
//! 2026-11-17): extract the chosen hits of the window's latest scan, from as
//! many archives as they came from, into one folder the user chose.
//!
//! Thin over the extract path (AGENTS.md "No format logic in the UI or in
//! Tauri commands"):
//!
//! * **The webview names no path.** It names the scan number, each archive's
//!   position in that scan and each entry's key. Rust checks every key against
//!   the names the scan read ([`Finder::entry_of`]) and that the archive file
//!   still has the size and modified time it had when it was scanned
//!   ([`Finder::archive_for_open`]); an archive that changed is reported and
//!   left alone, never guessed at. The folder comes from the native dialog
//!   (`find_choose_retrieve_folder`) and is the window's extract folder.
//! * **Read-only archives.** Each archive is opened with `OsReadStorage` and
//!   `open_head` (the catalog image hash-verified first), as Find opens it.
//!   Find never has a passphrase, so an Encrypted archive never has hits.
//! * **Every byte is written by the C6 restore engine**
//!   (`mochi_core::restore::restore_selected`): unsupported names are
//!   refused, links are never followed, and a file is published only after its
//!   chunks and content hash verified. The folders above each entry come with it.
//! * **Collisions.** Several archives can hold the same name. Only the two
//!   choices that make sense across archives are taken: *add only new files*
//!   (an existing file is kept and the entry skipped, and reported) and
//!   *replace* (the existing file goes to the Recycle Bin / Trash once the new
//!   one has been checked). With [`RetrieveLayout::PerArchive`] every archive
//!   gets a new folder of its own (`name`, `name (2)`, … never reused), so
//!   archives cannot collide with each other.
//! * **Not a verification** (spec §23.3): the report says what was extracted.
//! * **Cancellation** stops at the engine's next safe point; what was
//!   extracted stays, each file complete and verified.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::{JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::{open_head, ReadOptions};
use mochi_core::restore::{restore_selected, RestoreOptions, RESTORE_PHASE};
use mochi_core::storage::os::{OsReadStorage, OsRestoreDir};
use mochi_core::{ErrorCode, MochiError, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::browse::parse_key;
use crate::extract::{create_unique_dir, folder_stem, report_view, Destination};
use crate::find::Finder;
use crate::ipc::{ConflictChoice, IpcError, JobRequest, NameText, NotExtractedView, PathKey};
use crate::write::JobDone;

/// Most entries one retrieve may name: no more than a scan can hold, so in
/// practice every match. A retrieve of every match names none (`all`).
pub const MAX_ITEMS: usize = mochi_core::find::MAX_ENTRIES_TOTAL;
/// Entries listed per archive as not extracted (the total is always given).
pub const MAX_NOT_EXTRACTED_PER_ARCHIVE: usize = 100;

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn from_ipc(e: IpcError) -> String {
    e.message
}

/// One hit to retrieve: the archive's position in the scan and the entry's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct FoundItem {
    pub archive: u32,
    pub key: PathKey,
}

/// Where the retrieved entries go inside the chosen folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RetrieveLayout {
    /// A new folder per archive, named after it (`photos`, `photos (2)`, …),
    /// holding the entries at their paths inside the archive.
    PerArchive,
    /// All into the chosen folder, at their paths inside their archives.
    Together,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub enum RetrieveArchiveState {
    /// Extracted (entries it skipped are listed).
    Extracted,
    /// The archive file changed or went away after the scan: nothing was
    /// taken from it. Scan again to find its current names.
    Changed,
    /// It could not be opened or extracted from (`message` says why).
    Failed,
}

/// What happened with one archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RetrieveArchiveView {
    pub archive_path: String,
    /// The folder its entries went into (`None` when nothing was attempted).
    pub folder: Option<String>,
    pub state: RetrieveArchiveState,
    /// Why it was changed or failed (may quote archive content: escaped).
    pub message: Option<NameText>,
    /// Entries asked for from this archive.
    pub asked: u32,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub directories: u64,
    #[ts(type = "number")]
    pub bytes: u64,
    pub not_extracted: Vec<NotExtractedView>,
    #[ts(type = "number")]
    pub not_extracted_total: u64,
}

/// What a retrieve did, archive by archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../ui/src/ipc/generated/")]
pub struct RetrieveReportView {
    pub destination: String,
    pub layout: RetrieveLayout,
    #[ts(type = "number")]
    pub files: u64,
    #[ts(type = "number")]
    pub directories: u64,
    #[ts(type = "number")]
    pub bytes: u64,
    /// Entries not extracted, over every archive.
    #[ts(type = "number")]
    pub not_extracted_total: u64,
    /// Archives changed since the scan, or that failed.
    pub archives_not_done: u32,
    pub archives: Vec<RetrieveArchiveView>,
    /// Files were not flushed to disk one by one (Settings, Performance).
    pub files_not_flushed: bool,
    /// The folder was opened in the file manager ("Open the folder when done").
    pub opened_folder: bool,
}

/// The request's parts, or why it is not a retrieve.
struct Parts<'a> {
    run: u32,
    items: &'a [FoundItem],
    all: Option<&'a crate::find::FindQuery>,
    layout: RetrieveLayout,
    on_conflict: ConflictChoice,
}

fn parts(request: &JobRequest) -> Result<Parts<'_>> {
    let JobRequest::RetrieveFound {
        run,
        items,
        all,
        layout,
        on_conflict,
    } = request
    else {
        return Err(invalid("not a retrieve request"));
    };
    Ok(Parts {
        run: *run,
        items,
        all: all.as_ref(),
        layout: *layout,
        on_conflict: *on_conflict,
    })
}

/// Refuse a request that cannot work, before a job starts.
pub fn validate(
    finder: Option<&Finder>,
    destination: &Destination,
    request: &JobRequest,
) -> Result<()> {
    let Parts {
        run,
        items,
        all,
        on_conflict,
        ..
    } = parts(request)?;
    let finder = finder.ok_or_else(|| invalid("MOCHI Find is not available here"))?;
    match all {
        Some(q) => {
            if !items.is_empty() {
                return Err(invalid(
                    "a retrieve names found entries or asks for every match, not both",
                ));
            }
            if q.terms.iter().all(|t| t.trim().is_empty()) {
                return Err(invalid(
                    "a retrieve of every match needs the search it matches",
                ));
            }
        }
        None => {
            if items.is_empty() || items.len() > MAX_ITEMS {
                return Err(invalid(format!(
                    "a retrieve names 1 to {MAX_ITEMS} found entries"
                )));
            }
        }
    }
    if !matches!(
        on_conflict,
        ConflictChoice::AddNew | ConflictChoice::Replace
    ) {
        return Err(invalid(
            "retrieving from several archives either adds only new files or replaces existing ones",
        ));
    }
    if finder.current_run() != Some(run) {
        return Err(invalid(
            "those results belong to an earlier scan; search again",
        ));
    }
    items
        .iter()
        .try_for_each(|i| parse_key(&i.key).map(|_| ()))?;
    if destination.get().is_none() {
        return Err(invalid("choose the folder to retrieve into first"));
    }
    Ok(())
}

/// What to take from one archive.
enum Wanted<'a> {
    /// Hits' keys, checked against the scan before use.
    Keys(Vec<&'a str>),
    /// Paths from the scan's own names (every match of a search).
    Paths(Vec<ArchivePath>),
}

impl Wanted<'_> {
    fn len(&self) -> usize {
        match self {
            Wanted::Keys(k) => k.len(),
            Wanted::Paths(p) => p.len(),
        }
    }
}

/// Reports one restore's progress as part of the whole: entries done before
/// this archive plus the engine's own count, out of every entry asked for.
struct Overall<'a> {
    inner: &'a dyn ProgressSink,
    before: u64,
    total: u64,
}

impl ProgressSink for Overall<'_> {
    fn report(&self, event: &ProgressEvent) {
        if event.phase != RESTORE_PHASE {
            return;
        }
        self.inner.report(&ProgressEvent {
            phase: RESTORE_PHASE,
            completed: self.before.saturating_add(event.completed).min(self.total),
            total: Some(self.total),
        });
    }
}

fn not_done(
    path: &Path,
    state: RetrieveArchiveState,
    message: String,
    asked: usize,
) -> RetrieveArchiveView {
    RetrieveArchiveView {
        archive_path: path.display().to_string(),
        folder: None,
        state,
        message: Some(NameText::of(message.as_bytes())),
        asked: u32::try_from(asked).unwrap_or(u32::MAX),
        files: 0,
        directories: 0,
        bytes: 0,
        not_extracted: Vec::new(),
        not_extracted_total: 0,
    }
}

/// Run a retrieve job.
pub fn run(
    finder: Option<&Finder>,
    destination: &Destination,
    request: &JobRequest,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    validate(finder, destination, request)?;
    let finder = finder.ok_or_else(|| invalid("MOCHI Find is not available here"))?;
    let Parts {
        run,
        items,
        all,
        layout,
        on_conflict,
    } = parts(request)?;
    let dest = destination
        .get()
        .ok_or_else(|| invalid("choose the folder to retrieve into first"))?;

    // Archive by archive, in scan order; each entry once. Ticked hits are
    // checked against the names the scan read; every match of a search comes
    // from those names already.
    let mut by_archive: BTreeMap<u32, Wanted<'_>> = BTreeMap::new();
    match all {
        Some(q) => {
            for (archive, paths) in finder.all_matches(run, q).map_err(|e| invalid(e.message))? {
                by_archive.insert(archive, Wanted::Paths(paths));
            }
        }
        None => {
            let mut keys: BTreeMap<u32, std::collections::BTreeSet<&str>> = BTreeMap::new();
            for item in items {
                keys.entry(item.archive)
                    .or_default()
                    .insert(item.key.as_str());
            }
            for (archive, k) in keys {
                by_archive.insert(archive, Wanted::Keys(k.into_iter().collect()));
            }
        }
    }
    if by_archive.is_empty() {
        return Err(invalid(
            "nothing matches that search any more; search again",
        ));
    }
    let total: u64 = by_archive.values().map(|w| w.len() as u64).sum();
    ctx.report(RESTORE_PHASE, 0, Some(total));

    let options = RestoreOptions {
        restore_setid: false,
        refuse_on_preflight_exceptions: false,
        skip_file_sync: crate::speed::skip_file_sync(),
        on_existing: on_conflict.on_existing(),
    };
    let mut views = Vec::new();
    let mut before = 0u64;
    let mut files_not_flushed = false;
    for (archive, want) in &by_archive {
        ctx.check_cancelled()?;
        let asked = want.len();
        let done_before = before;
        before = before.saturating_add(asked as u64);
        let path = match finder.archive_for_open(run, *archive) {
            Ok(p) => p,
            Err(e) => {
                let shown = finder
                    .archive_path(run, *archive)
                    .unwrap_or_else(|_| PathBuf::from("?"));
                views.push(not_done(
                    &shown,
                    RetrieveArchiveState::Changed,
                    from_ipc(e),
                    asked,
                ));
                continue;
            }
        };
        let wanted: std::result::Result<Vec<ArchivePath>, String> = match want {
            Wanted::Paths(p) => Ok(p.clone()),
            Wanted::Keys(k) => finder.paths_in(run, *archive, k).map_err(from_ipc),
        };
        let wanted = match wanted {
            Ok(w) => w,
            Err(m) => {
                views.push(not_done(&path, RetrieveArchiveState::Failed, m, asked));
                continue;
            }
        };
        let opts = ReadOptions::default();
        let opened = OsReadStorage::open(&path)
            .map_err(MochiError::from)
            .and_then(|src| open_head(&src, &opts).map(|head| (src, head)));
        let (src, head) = match opened {
            Ok(o) => o,
            Err(e) => {
                views.push(not_done(
                    &path,
                    RetrieveArchiveState::Failed,
                    e.message,
                    asked,
                ));
                continue;
            }
        };
        let (folder, made) = match layout {
            RetrieveLayout::Together => (dest.clone(), false),
            RetrieveLayout::PerArchive => {
                let stem = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(folder_stem)
                    .unwrap_or_else(|| "archive".to_owned());
                match create_unique_dir(&dest, &stem) {
                    Ok(f) => (f, true),
                    Err(e) => {
                        views.push(not_done(
                            &path,
                            RetrieveArchiveState::Failed,
                            e.message,
                            asked,
                        ));
                        continue;
                    }
                }
            }
        };
        let root = match OsRestoreDir::open(&folder) {
            Ok(r) => r.with_set_aside(crate::trashbin::set_aside()),
            Err(e) => {
                if made {
                    let _ = std::fs::remove_dir(&folder);
                }
                views.push(not_done(
                    &path,
                    RetrieveArchiveState::Failed,
                    MochiError::from(e).message,
                    asked,
                ));
                continue;
            }
        };
        let overall = Overall {
            inner: ctx.progress,
            before: done_before,
            total,
        };
        let inner = JobContext {
            progress: &overall,
            cancel: ctx.cancel,
        };
        match restore_selected(&src, &head, &wanted, root, &options, &opts, &inner) {
            Ok(report) => {
                let v = report_view(&report, &folder, 0);
                files_not_flushed |= v.files_not_flushed;
                views.push(RetrieveArchiveView {
                    archive_path: path.display().to_string(),
                    folder: Some(folder.display().to_string()),
                    state: RetrieveArchiveState::Extracted,
                    message: None,
                    asked: u32::try_from(asked).unwrap_or(u32::MAX),
                    files: v.files,
                    directories: v.directories,
                    bytes: v.bytes,
                    not_extracted: v
                        .not_extracted
                        .into_iter()
                        .take(MAX_NOT_EXTRACTED_PER_ARCHIVE)
                        .collect(),
                    not_extracted_total: v.not_extracted_total,
                });
            }
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) => {
                if made {
                    // Only while still empty: never removes anything extracted.
                    let _ = std::fs::remove_dir(&folder);
                }
                views.push(not_done(
                    &path,
                    RetrieveArchiveState::Failed,
                    e.message,
                    asked,
                ));
            }
        }
    }
    ctx.report(RESTORE_PHASE, total, Some(total));

    let sum = |f: fn(&RetrieveArchiveView) -> u64| views.iter().map(f).sum::<u64>();
    let report = RetrieveReportView {
        destination: dest.display().to_string(),
        layout,
        files: sum(|v| v.files),
        directories: sum(|v| v.directories),
        bytes: sum(|v| v.bytes),
        not_extracted_total: sum(|v| v.not_extracted_total),
        archives_not_done: u32::try_from(
            views
                .iter()
                .filter(|v| v.state != RetrieveArchiveState::Extracted)
                .count(),
        )
        .unwrap_or(u32::MAX),
        archives: views,
        files_not_flushed,
        opened_folder: false,
    };
    if report.files + report.directories > 0 {
        destination.record_success();
    }
    let summary = format!(
        "retrieved {} file{} from {} archive{} to {}{}",
        report.files,
        if report.files == 1 { "" } else { "s" },
        by_archive.len(),
        if by_archive.len() == 1 { "" } else { "s" },
        report.destination,
        if report.not_extracted_total == 0 {
            String::new()
        } else {
            format!("; {} not extracted", report.not_extracted_total)
        }
    );
    Ok(JobDone {
        summary,
        retrieve: Some(Box::new(report)),
        ..JobDone::empty()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use mochi_core::manifest::Attributes;
    use mochi_core::publish::{ArchiveWriter, Transaction};
    use mochi_core::storage::os::OsStorage;
    use mochi_testkit::archive::{path as apath, test_options, Job};
    use mochi_testkit::SeqIds;

    use super::*;
    use crate::find::{FindEntryFilter, FindEvent, FindQuery, FindSink};

    static SEED: AtomicU64 = AtomicU64::new(9_000);

    struct Quiet;
    impl FindSink for Quiet {
        fn send(&self, _: FindEvent) -> bool {
            true
        }
    }

    fn write_archive(path: &Path, files: &[(&str, &[u8])]) {
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(path).unwrap(),
            Box::new(SeqIds::new(SEED.fetch_add(1, Ordering::SeqCst))),
            test_options(),
        )
        .unwrap();
        let mut tx = Transaction::new();
        for (f, bytes) in files {
            let parts: Vec<&str> = f.split('/').collect();
            for i in 1..parts.len() {
                tx.put_dir(apath(&parts[..i].join("/")), Attributes::default());
            }
            tx.put_file(apath(f), bytes.to_vec(), Attributes::default());
        }
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
    }

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
        out: PathBuf,
        finder: Arc<Finder>,
        destination: Destination,
    }

    /// root/{alpha.mochi, beta.mochi}: both hold `docs/report.txt`.
    fn fixture() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("archives");
        let out = dir.path().join("out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        write_archive(
            &root.join("alpha.mochi"),
            &[("docs/report.txt", b"from alpha"), ("keep/a.bin", b"aaa")],
        );
        write_archive(
            &root.join("beta.mochi"),
            &[("docs/report.txt", b"from beta"), ("pics/cat.jpg", b"meow")],
        );
        let finder = Arc::new(Finder::default());
        finder.set_scopes(vec![root.clone()]).unwrap();
        finder
            .start(false, Arc::new(Quiet), Box::new(|work| work()))
            .unwrap();
        let destination = Destination::default();
        destination.set(out.clone());
        Fx {
            _dir: dir,
            root,
            out,
            finder,
            destination,
        }
    }

    fn found(fx: &Fx, names: &[&str]) -> (u32, Vec<FoundItem>) {
        let r = fx
            .finder
            .query_many(&FindQuery {
                terms: names.iter().map(|n| (*n).to_owned()).collect(),
                match_case: false,
                whole_name: true,
                entries: FindEntryFilter::Files,
                ..FindQuery::default()
            })
            .unwrap();
        let run = r.hits.first().map_or(0, |h| h.run);
        let items = r
            .hits
            .iter()
            .map(|h| FoundItem {
                archive: h.archive,
                key: h.key.clone(),
            })
            .collect();
        (run, items)
    }

    fn request(
        run: u32,
        items: Vec<FoundItem>,
        layout: RetrieveLayout,
        on: ConflictChoice,
    ) -> JobRequest {
        JobRequest::RetrieveFound {
            run,
            items,
            all: None,
            layout,
            on_conflict: on,
        }
    }

    fn go(fx: &Fx, req: &JobRequest) -> RetrieveReportView {
        *run(Some(&fx.finder), &fx.destination, req, &Job::new().ctx())
            .unwrap()
            .retrieve
            .unwrap()
    }

    #[test]
    fn one_folder_per_archive_keeps_same_named_files_apart() {
        let fx = fixture();
        let (run, items) = found(&fx, &["report.txt", "cat.jpg"]);
        assert_eq!(items.len(), 3);
        let r = go(
            &fx,
            &request(
                run,
                items,
                RetrieveLayout::PerArchive,
                ConflictChoice::AddNew,
            ),
        );
        assert_eq!(
            (r.files, r.not_extracted_total, r.archives_not_done),
            (3, 0, 0)
        );
        assert_eq!(
            std::fs::read(fx.out.join("alpha/docs/report.txt")).unwrap(),
            b"from alpha"
        );
        assert_eq!(
            std::fs::read(fx.out.join("beta/docs/report.txt")).unwrap(),
            b"from beta"
        );
        assert_eq!(
            std::fs::read(fx.out.join("beta/pics/cat.jpg")).unwrap(),
            b"meow"
        );
        // Only what was asked for: not the rest of the archive.
        assert!(!fx.out.join("alpha/keep").exists());
        assert!(r
            .archives
            .iter()
            .all(|a| a.state == RetrieveArchiveState::Extracted));

        // Again: new folders, never the ones already there.
        let (run, items) = found(&fx, &["report.txt"]);
        go(
            &fx,
            &request(
                run,
                items,
                RetrieveLayout::PerArchive,
                ConflictChoice::AddNew,
            ),
        );
        assert!(fx.out.join("alpha (2)/docs/report.txt").is_file());
    }

    #[test]
    fn together_keeps_the_first_file_and_reports_the_second() {
        let fx = fixture();
        let (run, items) = found(&fx, &["report.txt"]);
        let r = go(
            &fx,
            &request(run, items, RetrieveLayout::Together, ConflictChoice::AddNew),
        );
        assert_eq!((r.files, r.not_extracted_total), (1, 1));
        assert_eq!(
            std::fs::read(fx.out.join("docs/report.txt")).unwrap(),
            b"from alpha"
        );
        let beta = r
            .archives
            .iter()
            .find(|a| a.archive_path.ends_with("beta.mochi"))
            .unwrap();
        assert_eq!(beta.not_extracted_total, 1);
        assert_eq!(beta.not_extracted[0].path.text, "docs/report.txt");
    }

    #[test]
    fn an_archive_that_changed_after_the_scan_is_left_alone() {
        let fx = fixture();
        let (run, items) = found(&fx, &["report.txt"]);
        // beta is replaced by another archive after the scan.
        std::fs::remove_file(fx.root.join("beta.mochi")).unwrap();
        write_archive(
            &fx.root.join("beta.mochi"),
            &[("docs/report.txt", b"newer and longer")],
        );
        let r = go(
            &fx,
            &request(
                run,
                items,
                RetrieveLayout::PerArchive,
                ConflictChoice::AddNew,
            ),
        );
        assert_eq!((r.files, r.archives_not_done), (1, 1));
        let beta = r
            .archives
            .iter()
            .find(|a| a.archive_path.ends_with("beta.mochi"))
            .unwrap();
        assert_eq!(beta.state, RetrieveArchiveState::Changed);
        assert!(beta.folder.is_none());
        assert!(!fx.out.join("beta").exists());
    }

    #[test]
    fn requests_that_cannot_work_are_refused_before_anything_is_written() {
        let fx = fixture();
        let (run, items) = found(&fx, &["report.txt"]);
        let check = |req: &JobRequest| validate(Some(&fx.finder), &fx.destination, req);
        assert!(check(&request(
            run,
            Vec::new(),
            RetrieveLayout::Together,
            ConflictChoice::AddNew
        ))
        .is_err());
        for on in [ConflictChoice::Stop, ConflictChoice::SkipConflicts] {
            assert!(check(&request(run, items.clone(), RetrieveLayout::Together, on)).is_err());
        }
        assert!(check(&request(
            run + 1,
            items.clone(),
            RetrieveLayout::Together,
            ConflictChoice::AddNew
        ))
        .is_err());
        let bad_key = vec![FoundItem {
            archive: 0,
            key: "zz".into(),
        }];
        assert!(check(&request(
            run,
            bad_key,
            RetrieveLayout::Together,
            ConflictChoice::AddNew
        ))
        .is_err());
        assert!(validate(
            None,
            &fx.destination,
            &request(
                run,
                items.clone(),
                RetrieveLayout::Together,
                ConflictChoice::AddNew
            )
        )
        .is_err());
        assert!(validate(
            Some(&fx.finder),
            &Destination::default(),
            &request(run, items, RetrieveLayout::Together, ConflictChoice::AddNew)
        )
        .is_err());
        assert_eq!(std::fs::read_dir(&fx.out).unwrap().count(), 0);
    }

    #[test]
    fn an_entry_the_scan_did_not_read_is_not_extracted() {
        let fx = fixture();
        let (run, mut items) = found(&fx, &["cat.jpg"]);
        // A key that is valid hex but names nothing the scan read in that archive.
        items.push(FoundItem {
            archive: items[0].archive,
            key: crate::browse::hex(b"keep/a.bin"),
        });
        let r = go(
            &fx,
            &request(run, items, RetrieveLayout::Together, ConflictChoice::AddNew),
        );
        assert_eq!(r.files, 0);
        assert_eq!(r.archives[0].state, RetrieveArchiveState::Failed);
        assert!(!fx.out.join("pics").exists());
    }

    #[test]
    fn a_cancelled_retrieve_is_cancelled() {
        let fx = fixture();
        let (scan, items) = found(&fx, &["report.txt"]);
        let job = Job::new();
        job.cancel.cancel();
        let e = super::run(
            Some(&fx.finder),
            &fx.destination,
            &request(
                scan,
                items,
                RetrieveLayout::PerArchive,
                ConflictChoice::AddNew,
            ),
            &job.ctx(),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled);
    }

    #[test]
    fn every_match_of_a_search_is_retrieved_without_listing_it() {
        let fx = fixture();
        let (run, _) = found(&fx, &["report.txt"]);
        let query = FindQuery {
            terms: vec!["*.txt".into(), "cat.jpg".into()],
            ..FindQuery::default()
        };
        let req = JobRequest::RetrieveFound {
            run,
            items: Vec::new(),
            all: Some(query.clone()),
            layout: RetrieveLayout::PerArchive,
            on_conflict: ConflictChoice::AddNew,
        };
        let r = go(&fx, &req);
        assert_eq!((r.files, r.archives_not_done), (3, 0));
        assert!(fx.out.join("alpha/docs/report.txt").is_file());
        assert!(fx.out.join("beta/docs/report.txt").is_file());
        assert!(fx.out.join("beta/pics/cat.jpg").is_file());
        assert!(!fx.out.join("alpha/keep").exists());
        // One name of it alone.
        let only = JobRequest::RetrieveFound {
            run,
            items: Vec::new(),
            all: Some(FindQuery {
                term: Some(1),
                ..query
            }),
            layout: RetrieveLayout::Together,
            on_conflict: ConflictChoice::AddNew,
        };
        let r = go(&fx, &only);
        assert_eq!(r.files, 1);
        assert!(fx.out.join("pics/cat.jpg").is_file());
        assert!(!fx.out.join("docs").exists());
        // Items and all at once, or an empty search, are refused.
        let both = JobRequest::RetrieveFound {
            run,
            items: vec![FoundItem {
                archive: 0,
                key: crate::browse::hex(b"docs/report.txt"),
            }],
            all: Some(FindQuery {
                terms: vec!["x".into()],
                ..FindQuery::default()
            }),
            layout: RetrieveLayout::Together,
            on_conflict: ConflictChoice::AddNew,
        };
        assert!(validate(Some(&fx.finder), &fx.destination, &both).is_err());
        let empty = JobRequest::RetrieveFound {
            run,
            items: Vec::new(),
            all: Some(FindQuery::default()),
            layout: RetrieveLayout::Together,
            on_conflict: ConflictChoice::AddNew,
        };
        assert!(validate(Some(&fx.finder), &fx.destination, &empty).is_err());
    }
}
