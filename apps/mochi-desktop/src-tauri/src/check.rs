//! Test, health, and repair from the desktop app (plan D4). Thin over
//! `mochi-core` (AGENTS.md "No format logic in the UI or in Tauri commands"):
//! verification is `mochi_core::verify::verify`, the health summary is
//! `mochi_core::health::assess`, and repair is `mochi_core::repair::plan_*`
//! then `apply_*`, called as `mochi verify`/`fsck`, `mochi health`, and
//! `mochi repair plan|apply` call them. This module keeps the chosen files,
//! the reports for export, and the approved plan, and converts core's typed
//! results for display. It is independent of Tauri (the dialogs are shown by
//! `commands.rs`), so it is tested without a window.
//!
//! # The webview names no path
//!
//! The archive to test or repair, each independent copy, the new archive a
//! repair writes, and where a report is exported are chosen in native dialogs
//! shown by Rust and kept here. The webview refers to them by ID.
//!
//! # Rules (delegated decisions, recorded in plan D4)
//!
//! * **Read-only.** "Test archive", the health summary, and a repair plan
//!   open the archive through `OsReadStorage` (spec §20.2, §23.3 #3). Only
//!   `apply` writes, and only a new file chosen in a save dialog that does not
//!   exist yet; the source is never written (§22.2).
//! * **The report is core's.** A report that fails `Report::validate` is
//!   never shown (`REPORT_INCONSISTENT`, as the CLI does). Its JSON is
//!   rendered once, exactly as `mochi verify --json` prints it, and "Export
//!   report JSON" writes those bytes unchanged.
//! * **The plan is the approval.** `apply` takes the plan the user reviewed,
//!   by ID, with the copies and salvage options it was made with; core plans
//!   again and refuses any difference. Changing the archive or the copies
//!   drops the plan.
//! * **Freshness from the shared head history** (spec Annex B.2.22, which
//!   replaces B.3 item 5). With "Remember archives on this computer" on (the
//!   default), a test is anchored at the head the app or the command-line
//!   tool last saw on this computer (D8: `PASS` if the history still holds
//!   it, `FAIL` on a rollback or substitution, `UNKNOWN` at first sight), and
//!   a test in which nothing failed records the head, as `mochi verify` does.
//!   Off, freshness has no anchor and is `UNKNOWN`, never `PASS`.
//! * **Evidence is shared, or this session's.** With local history on, each
//!   completed test (and each commit the app publishes, `history::committed`)
//!   is appended to the evidence log `mochi health` reads, and the health
//!   summary reads that log: the command-line tool's evidence counts here and
//!   the reverse. Off, each test is recorded in memory only and nothing is
//!   saved.
//! * **Encrypted archives (D20, D5).** The subject says whether it is
//!   Encrypted (the head commit record, read without a key) and whether Rust
//!   holds a key for it. The passphrase crosses IPC once, in `unlock`, and is
//!   kept only as a zeroizing `KeySession` in the subject (or taken from the
//!   browser's open archive, which already holds it). A test with a key runs
//!   every level; without one it runs keylessly (stored integrity; never
//!   `PASS` overall). A repair needs the key and is refused with
//!   `KEY_UNAVAILABLE` before anything is read.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use mochi_core::copies::CopySource;
use mochi_core::health::{
    self, CurrentHead, EvidenceCommand, EvidenceHead, EvidenceRecord, Inputs,
};
use mochi_core::job::JobContext;
use mochi_core::object::{ArchiveId, OsIds};
use mochi_core::publish::{commit_history, locate_head, open_head, read_commit, ReadOptions};
use mochi_core::repair::{
    self, LostSnapshot, OmittedEntry, Outcome, RepairOptions, RepairPlan, RepairReport,
    RetentionPlan,
};
use mochi_core::report::Report;
use mochi_core::salvage::SalvageOptions;
use mochi_core::status::{Status, VerificationLevel};
use mochi_core::storage::os::{OsDir, OsReadStorage};
use mochi_core::timestamp::Timestamp;
use mochi_core::verify::{
    judge_freshness_of, verify, FreshnessAnchor, FreshnessJudgement, VerifyOptions,
};
use mochi_core::{ErrorCode, MochiError, Result};
use serde::Serialize;

use crate::browse::Archives;
use crate::ipc::Secret;
use crate::ipc::{
    ArchiveHandle, CheckLevel, CheckResult, CopyView, CoverageView, DimensionView, FindingView,
    HealthView, JobRequest, LadderStepView, LostSnapshotView, NameText, OmittedView, ParityView,
    PlanId, PlannedSnapshotView, RepairOutcomeView, RepairPlanView, RepairResultView,
    RepairSetupAction, RepairSetupView, RepairedCommitView, ReportId, ReportKind, ReportNoteView,
    ReportView, RetentionView, ReverificationView, SalvageView, SeqRangeView, StatusView,
    SubjectId, SubjectView,
};
use crate::write::JobDone;

/// Reports kept for export (the most recent ones).
pub const REPORTS_KEPT: usize = 16;

/// Evidence records kept per archive in memory (the most recent ones).
pub const EVIDENCE_KEPT: usize = 64;

/// Independent copies one repair may name.
pub const MAX_COPIES: usize = 8;

/// Entries a plan or a repair result lists as left out (the total is always
/// given).
pub const MAX_OMITTED_SHOWN: usize = 1_000;

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every update below is a handful of assignments made while holding the
    // lock; a panic elsewhere cannot leave the state half-updated.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The file's name for display (the user's own name, not archive data).
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A core enum as its serialized name (`integrity`, `restoration`, …).
fn name_of<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn text(s: &str) -> NameText {
    NameText::of(s.as_bytes())
}

/// The archive chosen for testing, health, or repair.
#[derive(Debug, Clone)]
struct Subject {
    id: SubjectId,
    path: PathBuf,
    /// The head commit says Encrypted.
    encrypted: bool,
    /// The key session that opened it (D5), if one did. Zeroizing; never
    /// printed (`KeySession` has no `Debug` text of its contents).
    keys: Option<std::sync::Arc<mochi_core::keys::KeySession>>,
}

fn subject_view(s: &Subject) -> SubjectView {
    SubjectView {
        id: s.id,
        file_name: file_name(&s.path),
        encrypted: s.encrypted,
        unlocked: s.encrypted && s.keys.is_some(),
    }
}

impl Subject {
    /// The read options a check or repair of this subject uses.
    fn read(&self) -> ReadOptions {
        ReadOptions {
            keys: self.keys.clone(),
            ..ReadOptions::default()
        }
    }
}

/// A plan the user can approve, with what it was made from.
#[derive(Debug, Clone)]
struct StoredPlan {
    id: PlanId,
    subject: SubjectId,
    plan: RepairPlan,
    copies: Vec<PathBuf>,
    salvage: Option<SalvageOptions>,
}

#[derive(Debug, Default)]
struct State {
    last_subject: SubjectId,
    subject: Option<Subject>,
    last_report: ReportId,
    /// (ID, suggested file name, exact JSON), oldest first.
    reports: VecDeque<(ReportId, String, String)>,
    /// This session's evidence, by archive ID (hex).
    evidence: HashMap<String, VecDeque<EvidenceRecord>>,
    copies: Vec<PathBuf>,
    output: Option<PathBuf>,
    last_plan: PlanId,
    plan: Option<StoredPlan>,
    /// The new archive the last repair wrote.
    repaired: Option<PathBuf>,
}

/// Everything D4 keeps between calls.
#[derive(Debug, Default)]
pub struct Checks {
    inner: Mutex<State>,
}

/// Where `choose_check_subject` takes the archive from, once resolved.
pub enum SubjectPick {
    /// A path the user chose in a native dialog.
    Path(PathBuf),
    OpenArchive(ArchiveHandle),
    RepairOutput,
}

impl Checks {
    fn lock(&self) -> MutexGuard<'_, State> {
        lock(&self.inner)
    }

    /// Choose the archive to test, check, or repair. A new choice drops the
    /// repair's copies, new file, and plan, which belonged to the old one.
    pub fn choose_subject(&self, archives: &Archives, pick: SubjectPick) -> Result<SubjectView> {
        let mut keys = None;
        let path = match pick {
            SubjectPick::Path(p) => p,
            SubjectPick::OpenArchive(handle) => {
                // The browser's session already holds the key (D5).
                keys = archives.read_options(handle)?.keys;
                archives.path(handle)?
            }
            SubjectPick::RepairOutput => self
                .lock()
                .repaired
                .clone()
                .ok_or_else(|| invalid("no repair has written a new archive yet"))?,
        };
        let meta = std::fs::metadata(&path)
            .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{}: {e}", path.display())))?;
        if !meta.is_file() {
            return Err(invalid(format!("{} is not a file", path.display())));
        }
        let mut s = self.lock();
        let id = s
            .last_subject
            .checked_add(1)
            .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "restart the app"))?;
        s.last_subject = id;
        s.copies.clear();
        s.output = None;
        s.plan = None;
        let subject = Subject {
            id,
            encrypted: crate::browse::head_encrypted(&path),
            path,
            keys,
        };
        let view = subject_view(&subject);
        s.subject = Some(subject);
        Ok(view)
    }

    /// Open the chosen Encrypted archive with `passphrase` and keep the key
    /// in Rust (D5). A wrong passphrase is `KEY_UNAVAILABLE` and changes
    /// nothing; the archive is not described as damaged.
    pub fn unlock_subject(&self, id: SubjectId, passphrase: &Secret) -> Result<SubjectView> {
        let (path, encrypted) = {
            let s = self.lock();
            match &s.subject {
                Some(x) if x.id == id => (x.path.clone(), x.encrypted),
                _ => {
                    return Err(invalid(
                        "that archive is no longer the one chosen; choose it again",
                    ))
                }
            }
        };
        if !encrypted {
            return Err(invalid(
                "this archive is not encrypted: it has no passphrase",
            ));
        }
        let keys = mochi_core::keys::KeySession::single(passphrase.passphrase()?);
        let read = ReadOptions {
            keys: Some(keys.clone()),
            ..ReadOptions::default()
        };
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let head = open_head(&src, &read).map_err(|e| {
            if e.code == ErrorCode::KeyUnavailable {
                MochiError::new(ErrorCode::KeyUnavailable, crate::browse::WRONG_PASSPHRASE)
            } else {
                e
            }
        })?;
        if head.unlocked.is_none() {
            return Err(MochiError::new(
                ErrorCode::KeyUnavailable,
                crate::browse::WRONG_PASSPHRASE,
            ));
        }
        let mut s = self.lock();
        match &mut s.subject {
            Some(x) if x.id == id => {
                x.keys = Some(keys);
                Ok(subject_view(x))
            }
            _ => Err(invalid(
                "that archive is no longer the one chosen; choose it again",
            )),
        }
    }

    /// The chosen archive, if `id` is still the chosen one.
    fn subject_of(&self, id: SubjectId) -> Result<Subject> {
        match &self.lock().subject {
            Some(s) if s.id == id => Ok(s.clone()),
            _ => Err(invalid(
                "that archive is no longer the one chosen; choose it again",
            )),
        }
    }

    /// The chosen archive's path, if `id` is still the chosen one.
    fn subject(&self, id: SubjectId) -> Result<PathBuf> {
        self.subject_of(id).map(|s| s.path)
    }

    /// Change the repair's copies or new file. `picked` is what the dialog
    /// returned for `AddCopy` and `ChooseOutput` (`None`: the user cancelled
    /// it, and nothing changes).
    pub fn setup(
        &self,
        action: RepairSetupAction,
        picked: Option<PathBuf>,
    ) -> Result<RepairSetupView> {
        let mut s = self.lock();
        let subject = s
            .subject
            .clone()
            .ok_or_else(|| invalid("choose the archive to repair first"))?;
        match (action, picked) {
            (RepairSetupAction::Show, _)
            | (RepairSetupAction::AddCopy | RepairSetupAction::ChooseOutput, None) => {}
            (RepairSetupAction::AddCopy, Some(copy)) => {
                if s.copies.len() >= MAX_COPIES {
                    return Err(invalid(format!(
                        "a repair takes at most {MAX_COPIES} copies"
                    )));
                }
                if same_file(&copy, &subject.path) {
                    return Err(invalid(format!(
                        "{} is the archive being repaired; a copy is a different file",
                        file_name(&copy)
                    )));
                }
                if s.copies.iter().any(|c| same_file(c, &copy)) {
                    return Err(invalid(format!("{} is already a copy", file_name(&copy))));
                }
                s.copies.push(copy);
                s.plan = None;
            }
            (RepairSetupAction::RemoveCopy { index }, _) => {
                let i = usize::try_from(index).unwrap_or(usize::MAX);
                if i >= s.copies.len() {
                    return Err(invalid("there is no such copy"));
                }
                s.copies.remove(i);
                s.plan = None;
            }
            (RepairSetupAction::ClearCopies, _) => {
                if !s.copies.is_empty() {
                    s.copies.clear();
                    s.plan = None;
                }
            }
            (RepairSetupAction::ChooseOutput, Some(out)) => {
                let out = crate::picker::with_extension(out);
                if std::fs::symlink_metadata(&out).is_ok() {
                    return Err(MochiError::new(
                        ErrorCode::DestinationExists,
                        format!(
                            "{} exists; a repair writes a new file and never replaces one. \
                             Choose a new name",
                            file_name(&out)
                        ),
                    ));
                }
                s.output = Some(out);
            }
        }
        Ok(setup_view(&s))
    }

    /// The health summary of the chosen archive (`mochi health` with the
    /// default policy): this session's evidence, judged at the current head.
    /// Reads only the head's commit record; runs no check.
    pub fn health(&self, subject: SubjectId) -> Result<HealthView> {
        let path = self.subject(subject)?;
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let read = ReadOptions::default();
        let loc = locate_head(&src, &read.limits)?;
        let (commit, commit_id) = read_commit(&src, &loc.footer, &read)?;
        let archive_id = commit.archive_id.to_hex();
        let current = CurrentHead {
            archive_id: archive_id.clone(),
            seq: commit.seq,
            commit_id: commit_id.to_hex(),
        };
        let store = crate::history::store();
        // The shared log and head history (Annex B.2.22), or, with local
        // history off, this session's records and no anchor.
        let (records, unreadable, seen) = match &store {
            Some(st) => {
                let ev = st.read_evidence(&commit.archive_id).unwrap_or_default();
                let seen = st.get(&commit.archive_id).ok().flatten();
                (ev.records, ev.unreadable, seen)
            }
            None => (
                self.lock()
                    .evidence
                    .get(&archive_id)
                    .map(|r| r.iter().cloned().collect())
                    .unwrap_or_default(),
                0,
                None,
            ),
        };
        let mut freshness = FreshnessJudgement {
            status: Status::Unknown,
            findings: Vec::new(),
        };
        let mut freshness_note = None;
        if let Some(h) = seen {
            match commit_history(&src, &read) {
                Ok(history) => {
                    freshness = judge_freshness_of(
                        &FreshnessAnchor::LocalHistory {
                            seq: h.seq,
                            commit_id: h.commit_id,
                        },
                        &history,
                    );
                }
                Err(e) => {
                    freshness_note = Some(format!(
                        "the commit history could not be read, so the head cannot be compared \
                         with the one last seen: {}",
                        e.message
                    ));
                }
            }
        }
        let report = health::assess(
            &Inputs {
                evidence: &records,
                unreadable_evidence: unreadable,
                evidence_unavailable: None,
                current: &current,
                anchor: seen.map(|h| health::AnchorRef {
                    seq: h.seq,
                    commit_id: h.commit_id.to_hex(),
                }),
                freshness,
                freshness_note,
                now: Timestamp::now()?,
            },
            &health::HealthPolicy::default(),
        );
        let report = self.keep(ReportKind::Health, &path, false, false, report)?;
        Ok(HealthView {
            report,
            records: u32::try_from(records.len()).unwrap_or(u32::MAX),
        })
    }

    /// A kept report: a suggested file name and the exact JSON.
    pub fn report_json(&self, id: ReportId) -> Result<(String, String)> {
        self.lock()
            .reports
            .iter()
            .find(|(r, _, _)| *r == id)
            .map(|(_, name, json)| (name.clone(), json.clone()))
            .ok_or_else(|| {
                invalid("that report is no longer kept; run the test or open the health view again")
            })
    }

    /// Validate `report`, keep its JSON for export, and convert it.
    fn keep(
        &self,
        kind: ReportKind,
        path: &Path,
        deep: bool,
        keyless: bool,
        report: Report,
    ) -> Result<ReportView> {
        if let Err(violations) = report.validate() {
            let msg: Vec<String> = violations.iter().map(ToString::to_string).collect();
            return Err(MochiError::new(
                ErrorCode::ReportInconsistent,
                format!("internal: the report is inconsistent: {}", msg.join("; ")),
            ));
        }
        let json = report_json(&report)?;
        let stem = path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_owned());
        let suggested = match kind {
            ReportKind::Test => format!("{stem}-test-report.json"),
            ReportKind::Health => format!("{stem}-health-report.json"),
        };
        let id = {
            let mut s = self.lock();
            let id = s
                .last_report
                .checked_add(1)
                .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "restart the app"))?;
            s.last_report = id;
            if s.reports.len() >= REPORTS_KEPT {
                s.reports.pop_front();
            }
            s.reports.push_back((id, suggested, json.clone()));
            id
        };
        Ok(report_view(
            id,
            kind,
            file_name(path),
            deep,
            keyless,
            &report,
            json,
        ))
    }

    fn record(&self, archive_id: String, record: Option<EvidenceRecord>) {
        let Some(record) = record else { return };
        let mut s = self.lock();
        let log = s.evidence.entry(archive_id).or_default();
        if log.len() >= EVIDENCE_KEPT {
            log.pop_front();
        }
        log.push_back(record);
    }

    /// Refuse a D4 request that cannot work, before a job starts.
    pub fn validate(&self, request: &JobRequest) -> Result<()> {
        match request {
            JobRequest::TestArchive { subject, .. } => self.subject(*subject).map(|_| ()),
            JobRequest::PlanRepair {
                subject,
                salvage,
                archive_id,
            } => {
                self.subject(*subject)?;
                salvage_options(*salvage, archive_id.as_deref()).map(|_| ())
            }
            JobRequest::ApplyRepair { subject, plan, .. } => {
                self.subject(*subject)?;
                let s = self.lock();
                match &s.plan {
                    Some(p) if p.id == *plan && p.subject == *subject => {}
                    _ => {
                        return Err(invalid(
                            "that plan is no longer current (the archive or the copies \
                             changed): plan again",
                        ))
                    }
                }
                if s.output.is_none() {
                    return Err(invalid("choose where to save the new archive first"));
                }
                Ok(())
            }
            _ => Err(invalid("not a test or repair request")),
        }
    }

    /// Run a D4 job.
    pub fn run(&self, request: &JobRequest, ctx: &JobContext<'_>) -> Result<JobDone> {
        #[cfg(feature = "e2e")]
        let paced = crate::extract::Paced(ctx.progress);
        #[cfg(feature = "e2e")]
        let ctx = &JobContext {
            progress: &paced,
            cancel: ctx.cancel,
        };
        match request {
            JobRequest::TestArchive {
                subject,
                level,
                deep,
            } => self.test(*subject, *level, *deep, ctx),
            JobRequest::PlanRepair {
                subject,
                salvage,
                archive_id,
            } => self.plan(*subject, *salvage, archive_id.as_deref(), ctx),
            JobRequest::ApplyRepair {
                subject,
                plan,
                accept_retention_loss,
            } => self.apply(*subject, *plan, *accept_retention_loss, ctx),
            _ => Err(invalid("not a test or repair request")),
        }
    }

    /// "Test archive": `mochi verify` (or `fsck`), read-only.
    fn test(
        &self,
        subject: SubjectId,
        level: CheckLevel,
        deep: bool,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let chosen = self.subject_of(subject)?;
        let path = chosen.path.clone();
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let read = chosen.read();
        let store = crate::history::store();
        // The head last seen on this computer, by the app or the CLI (D8).
        let anchor = crate::history::anchor(
            store.as_ref(),
            crate::history::claimed_archive_id(&src, &read),
        );
        let opts = VerifyOptions {
            level: verification_level(level),
            // With the passphrase every level runs; without one an Encrypted
            // archive is checked keylessly (D20 item 12).
            read,
            anchor,
            require_freshness: false,
            deep,
        };
        let v = verify(&src, &opts, ctx);
        if ctx.cancel.is_cancelled() && v.report.operational_error {
            return Err(MochiError::new(ErrorCode::Cancelled, "cancelled"));
        }
        let keyless = head_encrypted(&src, &opts.read) && chosen.keys.is_none();
        let report = self.keep(ReportKind::Test, &path, deep, keyless, v.report.clone())?;
        if v.head_is_anchorable() {
            if let Some(h) = v.head {
                crate::history::remember(store.as_ref(), h.archive_id, h.seq, h.commit_id);
            }
        }
        if let Some(h) = v.head {
            let command = if deep {
                EvidenceCommand::Fsck
            } else {
                EvidenceCommand::Verify
            };
            let head = EvidenceHead {
                seq: h.seq,
                commit_id: h.commit_id.to_hex(),
            };
            // As the CLI records it: every completed run with a verified head,
            // failures included (a failure must outlive the run that found it).
            let record = EvidenceRecord::from_report(command, &v.report, head);
            match (&store, &record) {
                (Some(_), Some(r)) => {
                    crate::history::record_evidence(store.as_ref(), &h.archive_id, r);
                }
                _ => self.record(h.archive_id.to_hex(), record),
            }
        }
        let summary = format!(
            "tested {}: overall {}, exit {}",
            report.file_name,
            name_of(&report.overall_status),
            report.exit_code
        );
        Ok(JobDone {
            summary,
            commit: None,
            extract: None,
            check: Some(Box::new(CheckResult::Test { report })),
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
            foreign_test: None,
        })
    }

    /// `mochi repair plan`: read-only.
    fn plan(
        &self,
        subject: SubjectId,
        salvage: bool,
        archive_id: Option<&str>,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let chosen = self.subject_of(subject)?;
        let path = chosen.path.clone();
        let salvage = salvage_options(salvage, archive_id)?;
        let copies = self.lock().copies.clone();
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let read = chosen.read();
        require_key(&src, &read)?;
        let opened = open_copies(&copies)?;
        let plan =
            repair::plan_with_salvage(&src, &copy_sources(&opened), salvage.as_ref(), &read, ctx)?;
        let id = {
            let mut s = self.lock();
            // The setup may have changed while the survey ran.
            if s.subject.as_ref().map(|x| x.id) != Some(subject) || s.copies != copies {
                return Err(invalid(
                    "the archive or the copies changed while planning: plan again",
                ));
            }
            let id = s
                .last_plan
                .checked_add(1)
                .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "restart the app"))?;
            s.last_plan = id;
            s.plan = Some(StoredPlan {
                id,
                subject,
                plan: plan.clone(),
                copies: copies.clone(),
                salvage,
            });
            id
        };
        let view = plan_view(id, &path, &plan, &copies);
        let summary = format!(
            "planned a repair of {}: {}",
            view.file_name,
            name_of(&plan.outcome)
        );
        Ok(JobDone {
            summary,
            commit: None,
            extract: None,
            check: Some(Box::new(CheckResult::Plan { plan: view })),
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
            foreign_test: None,
        })
    }

    /// `mochi repair apply`: the approved plan into the new file.
    fn apply(
        &self,
        subject: SubjectId,
        plan_id: PlanId,
        accept_retention_loss: bool,
        ctx: &JobContext<'_>,
    ) -> Result<JobDone> {
        let chosen = self.subject_of(subject)?;
        let path = chosen.path.clone();
        let (stored, output) = {
            let s = self.lock();
            let stored = match &s.plan {
                Some(p) if p.id == plan_id && p.subject == subject => p.clone(),
                _ => {
                    return Err(invalid(
                        "that plan is no longer current (the archive or the copies changed): \
                         plan again",
                    ))
                }
            };
            let output = s
                .output
                .clone()
                .ok_or_else(|| invalid("choose where to save the new archive first"))?;
            (stored, output)
        };
        if std::fs::symlink_metadata(&output).is_ok() {
            return Err(MochiError::new(
                ErrorCode::DestinationExists,
                format!(
                    "{} exists; a repair writes a new file and never replaces one. Nothing was \
                     written",
                    file_name(&output)
                ),
            ));
        }
        let name = output
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| invalid("the new archive's file name must be valid Unicode"))?
            .to_owned();
        let mut dir = OsDir::containing(&output)?.with_windows_flush(crate::speed::windows_flush());
        // The source is only read: repair never writes to it (§22.2).
        let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
        let read = chosen.read();
        require_key(&src, &read)?;
        let opened = open_copies(&stored.copies)?;
        let options = RepairOptions {
            accept_retention_loss,
            ..RepairOptions::default()
        };
        let report = repair::apply_with_salvage(
            &src,
            &copy_sources(&opened),
            stored.salvage.as_ref(),
            &stored.plan,
            &mut dir,
            &name,
            Box::new(OsIds),
            &read,
            &options,
            ctx,
        )?;
        // The new archive's head anchors a later test of it (D8), and its
        // publication is durability evidence (Annex B.2.22), as the CLI does.
        if let Ok(out) = OsReadStorage::open(&output) {
            if let Ok(h) = open_head(&out, &read) {
                let durability = match &report.durability_unconfirmed {
                    None => mochi_core::publish::PublishDurability::Durable,
                    Some(why) => {
                        mochi_core::publish::PublishDurability::DirectoryUnconfirmed(why.clone())
                    }
                };
                crate::history::committed_head(
                    h.commit.archive_id,
                    h.seq(),
                    h.commit_id,
                    &durability,
                );
            }
        }
        {
            let mut s = self.lock();
            s.repaired = Some(output.clone());
            s.output = None;
            if s.plan.as_ref().map(|p| p.id) == Some(plan_id) {
                s.plan = None;
            }
        }
        let view = result_view(&output, &report);
        let summary = format!(
            "wrote {}: {}, re-verification {}",
            view.file_name,
            name_of(&report.outcome),
            name_of(&report.reverification.overall_status)
        );
        Ok(JobDone {
            summary,
            commit: None,
            extract: None,
            check: Some(Box::new(CheckResult::Repair { result: view })),
            search: None,
            rekey: None,
            export: None,
            scan: None,
            retrieve: None,
            foreign_test: None,
        })
    }
}

/// Write `bytes` to a new file at `path`, never replacing one (as the CLI's
/// `--output`), and flush it.
pub fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::AlreadyExists {
                ErrorCode::DestinationExists
            } else {
                ErrorCode::IoError
            };
            MochiError::new(
                code,
                format!(
                    "{}: {e}; a report is written to a new file and never replaces one",
                    path.display()
                ),
            )
        })?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{}: {e}", path.display())))
}

/// The report as `mochi verify --json` prints it: one line, then a newline.
pub fn report_json(report: &Report) -> Result<String> {
    let value = serde_json::to_value(report)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("rendering the report: {e}")))?;
    Ok(format!("{value}\n"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn setup_view(s: &State) -> RepairSetupView {
    RepairSetupView {
        copies: s.copies.iter().map(|c| file_name(c)).collect(),
        output: s.output.as_deref().map(file_name),
    }
}

fn verification_level(l: CheckLevel) -> VerificationLevel {
    match l {
        CheckLevel::Structural => VerificationLevel::Structural,
        CheckLevel::Referential => VerificationLevel::Referential,
        CheckLevel::StoredIntegrity => VerificationLevel::StoredIntegrity,
        CheckLevel::ContentIntegrity => VerificationLevel::ContentIntegrity,
        CheckLevel::Restoration => VerificationLevel::Restoration,
    }
}

/// `--salvage [--archive-id HEX]` as the CLI parses it.
fn salvage_options(salvage: bool, archive_id: Option<&str>) -> Result<Option<SalvageOptions>> {
    let archive_id = archive_id.map(str::trim).filter(|h| !h.is_empty());
    if !salvage {
        if archive_id.is_some() {
            return Err(invalid("an archive ID is used only by the salvage scan"));
        }
        return Ok(None);
    }
    let archive_id = match archive_id {
        None => None,
        Some(h) => {
            let bytes: Option<Vec<u8>> = (h.len() == 64 && h.is_ascii())
                .then(|| {
                    (0..64)
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok())
                        .collect()
                })
                .flatten();
            let bytes: [u8; 32] = bytes
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| invalid("the archive ID must be 64 hexadecimal digits"))?;
            Some(ArchiveId::from_bytes(bytes))
        }
    };
    Ok(Some(SalvageOptions { archive_id }))
}

/// Whether the head commit, as located, says the archive is Encrypted.
/// `false` when the head cannot be read (the check itself reports that).
fn head_encrypted(src: &OsReadStorage, read: &ReadOptions) -> bool {
    locate_head(src, &read.limits)
        .ok()
        .and_then(|loc| read_commit(src, &loc.footer, read).ok())
        .is_some_and(|(c, _)| c.encrypted())
}

/// A repair of an Encrypted archive needs the passphrase (D20): it opens
/// every sealed object and seals it again under the new archive's key. Refuse
/// before reading anything else when none was entered. An archive whose head
/// cannot be read is left to the repair itself.
fn require_key(src: &OsReadStorage, read: &ReadOptions) -> Result<()> {
    if read.keys.is_none() && head_encrypted(src, read) {
        return Err(MochiError::new(
            ErrorCode::KeyUnavailable,
            "this archive is encrypted: a repair opens every sealed object and seals it \
             again, so it needs the passphrase. Enter the passphrase for this archive, then \
             plan again. Nothing was read or written",
        ));
    }
    Ok(())
}

fn open_copies(paths: &[PathBuf]) -> Result<Vec<(String, OsReadStorage)>> {
    paths
        .iter()
        .map(|p| {
            OsReadStorage::open(p)
                .map(|s| (file_name(p), s))
                .map_err(MochiError::from)
        })
        .collect()
}

fn copy_sources(opened: &[(String, OsReadStorage)]) -> Vec<CopySource<'_>> {
    opened
        .iter()
        .map(|(label, storage)| CopySource {
            label: label.clone(),
            storage,
        })
        .collect()
}

// ---- conversions ------------------------------------------------------------

pub fn status_view(s: Status) -> StatusView {
    match s {
        Status::Pass => StatusView::Pass,
        Status::Fail => StatusView::Fail,
        Status::Degraded => StatusView::Degraded,
        Status::Unknown => StatusView::Unknown,
        Status::Overdue => StatusView::Overdue,
        Status::Unsupported => StatusView::Unsupported,
    }
}

fn decimal(n: Option<u64>) -> Option<String> {
    n.map(|n| n.to_string())
}

/// The report for display. Messages and reasons are core's text, which can
/// quote archive data, so they cross escaped.
pub fn report_view(
    report_id: ReportId,
    kind: ReportKind,
    file_name: String,
    deep: bool,
    keyless: bool,
    r: &Report,
    json: String,
) -> ReportView {
    ReportView {
        report_id,
        kind,
        file_name,
        deep,
        keyless,
        schema_version: r.schema_version,
        archive_id: r.archive_id.clone(),
        checked_commit: r.checked_commit.clone(),
        expected_head: r.expected_head.clone(),
        freshness_anchor: name_of(&r.freshness_anchor),
        started_at: r.started_at.clone(),
        completed_at: r.completed_at.clone(),
        level: name_of(&r.level),
        scope: r.scope.as_deref().map(text),
        dimensions: r
            .dimensions
            .iter()
            .map(|(d, s)| DimensionView {
                dimension: name_of(d),
                status: status_view(*s),
                required: r.policy.required.contains(d),
            })
            .collect(),
        overall_status: status_view(r.overall_status),
        policy_result: status_view(r.policy_result),
        exit_code: r.exit_code,
        operational_error: r.operational_error,
        coverage: CoverageView {
            expected_objects: decimal(r.coverage.expected_objects),
            checked_objects: decimal(r.coverage.checked_objects),
            expected_bytes: decimal(r.coverage.expected_bytes),
            checked_bytes: decimal(r.coverage.checked_bytes),
            evidence_age_seconds: decimal(r.coverage.evidence_age_seconds),
        },
        findings: r
            .findings
            .iter()
            .map(|f| FindingView {
                code: f.code.as_str().to_owned(),
                severity: name_of(&f.severity),
                message: f.message.as_deref().map(text),
                expected: f.expected.as_deref().map(text),
                observed: f.observed.as_deref().map(text),
                affected: f.affected.map(|a| SeqRangeView {
                    first: a.first,
                    last: a.last,
                }),
            })
            .collect(),
        skipped: r
            .skipped
            .iter()
            .map(|s| ReportNoteView {
                item: text(&s.item),
                reason: text(&s.reason),
            })
            .collect(),
        json,
    }
}

fn outcome_view(o: Outcome) -> RepairOutcomeView {
    match o {
        Outcome::Complete => RepairOutcomeView::Complete,
        Outcome::Partial => RepairOutcomeView::Partial,
        Outcome::NothingRecoverable => RepairOutcomeView::NothingRecoverable,
    }
}

fn lost_view(l: &LostSnapshot) -> LostSnapshotView {
    LostSnapshotView {
        seq: l.seq,
        commit_id: l.commit_id.clone(),
        code: l.reason.code.as_str().to_owned(),
        reason: text(&l.reason.message),
    }
}

/// The stored path from its exact hex, escaped; the lossy `path` only if the
/// hex is malformed (it never is from core).
fn omitted_view(o: &OmittedEntry) -> OmittedView {
    let bytes: Option<Vec<u8>> = o
        .path_hex
        .len()
        .is_multiple_of(2)
        .then(|| {
            (0..o.path_hex.len())
                .step_by(2)
                .map(|i| {
                    o.path_hex
                        .get(i..i + 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                })
                .collect()
        })
        .flatten();
    OmittedView {
        path: bytes.map_or_else(|| text(&o.path), |b| NameText::of(&b)),
        kind: o.kind.clone(),
        snapshots: o.snapshots.clone(),
        code: o.reason.code.as_str().to_owned(),
        reason: text(&o.reason.message),
    }
}

fn salvage_used(snapshots: impl IntoIterator<Item = mochi_core::copies::Trust>) -> bool {
    snapshots
        .into_iter()
        .any(|t| t == mochi_core::copies::Trust::Salvage)
}

pub fn plan_view(id: PlanId, path: &Path, p: &RepairPlan, copies: &[PathBuf]) -> RepairPlanView {
    RepairPlanView {
        plan_id: id,
        file_name: file_name(path),
        outcome: outcome_view(p.outcome),
        exit_code: p.exit_code,
        archive_id: p.archive_id.clone(),
        head_found_by: p.head.as_ref().map(|h| h.found_by.clone()),
        head_seq: p.head.as_ref().map(|h| h.seq),
        tail_state: p.tail.as_ref().map(|t| t.state.clone()),
        tail_len: p.tail.as_ref().map(|t| t.len.to_string()),
        chain_break_at: p.chain_break.as_ref().map(|c| c.first_trusted_seq),
        chain_break_reason: p
            .chain_break
            .as_ref()
            .map(|c| text(&format!("{}: {}", c.reason.code.as_str(), c.reason.message))),
        ladder: p
            .ladder
            .iter()
            .map(|l| LadderStepView {
                step: l.step,
                name: l.name.clone(),
                status: name_of(&l.status),
                detail: text(&l.detail),
            })
            .collect(),
        snapshots: p
            .snapshots
            .iter()
            .map(|s| PlannedSnapshotView {
                seq: s.seq,
                commit_id: s.commit_id.clone(),
                entries: s.entries.to_string(),
                omitted: s.omitted.to_string(),
                catalog: name_of(&s.catalog),
                trust: name_of(&s.trust),
            })
            .collect(),
        lost: p.lost_snapshots.iter().map(lost_view).collect(),
        omitted: p
            .omitted
            .iter()
            .take(MAX_OMITTED_SHOWN)
            .map(omitted_view)
            .collect(),
        omitted_total: p.omitted.len().to_string(),
        manifest_failures: u32::try_from(p.manifest_failures.len()).unwrap_or(u32::MAX),
        retention: p.retention.as_ref().map(retention_view),
        parity: p.parity.as_ref().map(|s| ParityView {
            declared_percent: s.declared_percent,
            shards_damaged: s.shards_damaged().to_string(),
            shards_rebuilt: s.shards_rebuilt().to_string(),
            groups_beyond_capacity: s.beyond_capacity().count().to_string(),
        }),
        copies: p
            .copies
            .iter()
            .enumerate()
            .map(|(i, c)| CopyView {
                file_name: copies
                    .get(i)
                    .map_or_else(|| c.path.clone(), |p| file_name(p)),
                status: name_of(&c.status),
                frames_donated: c.frames_donated.to_string(),
                detail: text(&c.detail),
            })
            .collect(),
        donations: p.donations.len().to_string(),
        salvage: p.salvage.as_ref().map(|v| SalvageView {
            confirmed_commits: v.confirmed.len().to_string(),
            unconfirmed_commits: v.unconfirmed_count.to_string(),
            orphan_data_objects: v.orphan_data_objects.to_string(),
            limit_reached: v.limit_reached,
        }),
        salvage_used: salvage_used(p.snapshots.iter().map(|s| s.trust)),
        damage_found: p.damage_found,
    }
}

fn retention_view(r: &RetentionPlan) -> RetentionView {
    match r {
        RetentionPlan::Carried { holds, expired } => RetentionView::Carried {
            holds: u32::try_from(holds.len()).unwrap_or(u32::MAX),
            holds_lost: u32::try_from(holds.iter().filter(|h| h.new_seq.is_none()).count())
                .unwrap_or(u32::MAX),
            expired: u32::try_from(expired.len()).unwrap_or(u32::MAX),
        },
        RetentionPlan::Unresolved { reason } => RetentionView::Unresolved {
            code: reason.code.as_str().to_owned(),
            reason: text(&reason.message),
        },
    }
}

pub fn result_view(output: &Path, r: &RepairReport) -> RepairResultView {
    RepairResultView {
        outcome: outcome_view(r.outcome),
        exit_code: r.exit_code,
        file_name: file_name(output),
        new_archive_id: r.new_archive_id.clone(),
        new_len: r.new_len.to_string(),
        commits: r
            .commits
            .iter()
            .map(|c| RepairedCommitView {
                source_seq: c.source_seq,
                new_seq: c.new_seq,
                entries: c.entries.to_string(),
                omitted: c.omitted.to_string(),
                trust: name_of(&c.trust),
            })
            .collect(),
        lost: r.lost_snapshots.iter().map(lost_view).collect(),
        omitted: r
            .omitted
            .iter()
            .take(MAX_OMITTED_SHOWN)
            .map(omitted_view)
            .collect(),
        omitted_total: r.omitted.len().to_string(),
        profile_constraints_lost: r.profile_constraints_lost.clone(),
        retention_loss_accepted: r.retention_loss_accepted,
        reverification: ReverificationView {
            level: r.reverification.level.clone(),
            overall_status: status_view(r.reverification.overall_status),
            policy_result: status_view(r.reverification.policy_result),
            exit_code: r.reverification.exit_code,
            findings: r.reverification.findings.to_string(),
        },
        durability_note: r.durability_unconfirmed.clone(),
        source_kept: r.source_kept,
        salvage_used: salvage_used(r.commits.iter().map(|c| c.trust)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use mochi_core::job::{CancellationToken, NullProgress};
    use mochi_core::manifest::Attributes;
    use mochi_core::publish::{open_head, ArchiveWriter, Transaction};
    use mochi_core::storage::os::OsStorage;
    use mochi_testkit::archive::{
        build, encrypted_options, first_chunk_middle, path as apath, read_state, scripted_history,
        Job, State,
    };
    use mochi_testkit::SeqIds;

    use super::*;
    use crate::ipc::{JobEvent, JobOutcome};
    use crate::jobs::{EventSink, JobRegistry};
    use crate::write::Services;

    struct Fx {
        dir: tempfile::TempDir,
        checks: Checks,
        archives: Archives,
    }

    fn fx() -> Fx {
        Fx {
            dir: tempfile::tempdir().unwrap(),
            checks: Checks::default(),
            archives: Archives::default(),
        }
    }

    impl Fx {
        fn at(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// The scripted three-commit history (`big` spans several chunks;
        /// a markup name at the root).
        fn good(&self, name: &str) -> PathBuf {
            let path = self.at(name);
            build(
                OsStorage::create_new(&path).unwrap(),
                7,
                &scripted_history(),
            )
            .unwrap();
            path
        }

        /// A copy of `good` with one byte of `big`'s first chunk flipped.
        fn damaged(&self, good: &Path, name: &str) -> PathBuf {
            let src = OsReadStorage::open(good).unwrap();
            let at = first_chunk_middle(&open_head(&src, &ReadOptions::default()).unwrap(), "big")
                .unwrap();
            let mut bytes = std::fs::read(good).unwrap();
            bytes[usize::try_from(at).unwrap()] ^= 0x01;
            let path = self.at(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }

        fn choose(&self, path: &Path) -> SubjectId {
            self.checks
                .choose_subject(&self.archives, SubjectPick::Path(path.to_path_buf()))
                .unwrap()
                .id
        }

        fn run(&self, req: &JobRequest) -> Result<JobDone> {
            self.checks.validate(req)?;
            let job = Job::new();
            self.checks.run(req, &job.ctx())
        }

        fn test(&self, subject: SubjectId, level: CheckLevel, deep: bool) -> ReportView {
            match self.run(&JobRequest::TestArchive {
                subject,
                level,
                deep,
            }) {
                Ok(JobDone { check: Some(c), .. }) => match *c {
                    CheckResult::Test { report } => report,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        }

        fn plan(&self, subject: SubjectId, salvage: bool) -> RepairPlanView {
            match self.run(&JobRequest::PlanRepair {
                subject,
                salvage,
                archive_id: None,
            }) {
                Ok(JobDone { check: Some(c), .. }) => match *c {
                    CheckResult::Plan { plan } => plan,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            }
        }

        fn apply(&self, subject: SubjectId, plan: PlanId) -> Result<RepairResultView> {
            self.run(&JobRequest::ApplyRepair {
                subject,
                plan,
                accept_retention_loss: false,
            })
            .map(|d| match *d.check.unwrap() {
                CheckResult::Repair { result } => result,
                other => panic!("{other:?}"),
            })
        }

        fn output(&self, name: &str) -> RepairSetupView {
            self.checks
                .setup(RepairSetupAction::ChooseOutput, Some(self.at(name)))
                .unwrap()
        }
    }

    fn status(r: &ReportView, d: &str) -> StatusView {
        r.dimensions
            .iter()
            .find(|x| x.dimension == d)
            .unwrap_or_else(|| panic!("no {d}"))
            .status
    }

    fn listing(dir: &Path) -> Vec<std::ffi::OsString> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        v.sort();
        v
    }

    fn hash(path: &Path) -> blake3::Hash {
        blake3::hash(&std::fs::read(path).unwrap())
    }

    /// The report core's `verify` produces for `path`, as the CLI renders it,
    /// with the run's times removed: the independent oracle.
    fn core_json(path: &Path, level: VerificationLevel, deep: bool) -> serde_json::Value {
        let src = OsReadStorage::open(path).unwrap();
        let v = verify(
            &src,
            &VerifyOptions {
                level,
                deep,
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        );
        without_times(&serde_json::from_str(&report_json(&v.report).unwrap()).unwrap())
    }

    fn without_times(v: &serde_json::Value) -> serde_json::Value {
        let mut v = v.clone();
        let m = v.as_object_mut().unwrap();
        assert!(m.remove("started_at").unwrap().is_string());
        assert!(m.remove("completed_at").unwrap().is_string());
        v
    }

    fn state_of(path: &Path) -> State {
        let src = OsReadStorage::open(path).unwrap();
        read_state(&src, &open_head(&src, &ReadOptions::default()).unwrap()).unwrap()
    }

    #[test]
    fn a_good_archive_tests_as_core_reports_it_and_is_not_changed() {
        let f = fx();
        let good = f.good("good.mochi");
        let before = hash(&good);
        let id = f.choose(&good);
        let r = f.test(id, CheckLevel::Restoration, false);
        assert_eq!(status(&r, "integrity"), StatusView::Pass);
        assert_eq!(status(&r, "recoverability"), StatusView::Pass);
        assert_eq!(status(&r, "freshness"), StatusView::Unknown);
        assert_eq!(r.freshness_anchor, "none");
        assert_ne!(
            r.overall_status,
            StatusView::Pass,
            "durability is never observed"
        );
        assert_eq!((r.exit_code, r.level.as_str()), (0, "restoration"));
        assert!(!r.deep && !r.keyless);
        assert_eq!(r.file_name, "good.mochi");
        assert!(r.json.ends_with('\n') && !r.json.trim_end().contains('\n'));
        let ours: serde_json::Value = serde_json::from_str(&r.json).unwrap();
        assert_eq!(
            without_times(&ours),
            core_json(&good, VerificationLevel::Restoration, false)
        );
        assert_eq!(hash(&good), before, "testing is read-only");
    }

    #[test]
    fn deep_is_fsck_and_every_level_is_core_s() {
        let f = fx();
        let good = f.good("good.mochi");
        let id = f.choose(&good);
        let r = f.test(id, CheckLevel::Restoration, true);
        assert!(r.deep);
        let ours: serde_json::Value = serde_json::from_str(&r.json).unwrap();
        assert_eq!(
            without_times(&ours),
            core_json(&good, VerificationLevel::Restoration, true)
        );
        for (l, core) in [
            (CheckLevel::Structural, VerificationLevel::Structural),
            (CheckLevel::Referential, VerificationLevel::Referential),
            (
                CheckLevel::StoredIntegrity,
                VerificationLevel::StoredIntegrity,
            ),
            (
                CheckLevel::ContentIntegrity,
                VerificationLevel::ContentIntegrity,
            ),
        ] {
            let r = f.test(id, l, false);
            let ours: serde_json::Value = serde_json::from_str(&r.json).unwrap();
            assert_eq!(without_times(&ours), core_json(&good, core, false), "{l:?}");
        }
        // Above stored integrity, integrity is never PASS.
        let r = f.test(id, CheckLevel::Structural, false);
        assert_eq!(status(&r, "integrity"), StatusView::Unknown);
        assert_eq!(r.exit_code, 2);
    }

    #[test]
    fn a_corrupted_archive_fails_and_the_overall_status_shows_it() {
        let f = fx();
        let good = f.good("good.mochi");
        let bad = f.damaged(&good, "bad.mochi");
        let before = hash(&bad);
        let id = f.choose(&bad);
        let r = f.test(id, CheckLevel::Restoration, false);
        assert_eq!(status(&r, "integrity"), StatusView::Fail);
        assert_eq!(r.overall_status, StatusView::Fail);
        assert_eq!(r.policy_result, StatusView::Fail);
        assert_eq!(r.exit_code, 1);
        assert!(r
            .findings
            .iter()
            .any(|x| x.code == "STORED_INTEGRITY_FAILED" && x.severity == "error"));
        let ours: serde_json::Value = serde_json::from_str(&r.json).unwrap();
        assert_eq!(
            without_times(&ours),
            core_json(&bad, VerificationLevel::Restoration, false)
        );
        assert_eq!(hash(&bad), before);
    }

    #[test]
    fn the_export_is_the_exact_json_and_never_replaces_a_file() {
        let f = fx();
        let id = f.choose(&f.good("good.mochi"));
        let r = f.test(id, CheckLevel::Restoration, false);
        let (suggested, json) = f.checks.report_json(r.report_id).unwrap();
        assert_eq!(suggested, "good-test-report.json");
        assert_eq!(json, r.json);
        let out = f.at("report.json");
        write_new_file(&out, json.as_bytes()).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), r.json.as_bytes());
        let e = write_new_file(&out, b"other").unwrap_err();
        assert_eq!(e.code, ErrorCode::DestinationExists);
        assert_eq!(std::fs::read(&out).unwrap(), r.json.as_bytes());
        assert_eq!(
            f.checks.report_json(9_999).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn only_the_latest_reports_are_kept_for_export() {
        let f = fx();
        let id = f.choose(&f.good("good.mochi"));
        let first = f.test(id, CheckLevel::Structural, false).report_id;
        let mut last = first;
        for _ in 0..REPORTS_KEPT {
            last = f.test(id, CheckLevel::Structural, false).report_id;
        }
        assert!(f.checks.report_json(first).is_err());
        assert!(f.checks.report_json(last).is_ok());
    }

    #[test]
    fn an_encrypted_archive_is_tested_keylessly_and_a_repair_needs_the_key() {
        let f = fx();
        let path = f.at("enc.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(3)),
            encrypted_options(&["correct horse"]),
        )
        .unwrap();
        let mut tx = Transaction::new();
        tx.put_file(apath("secret.txt"), b"x".to_vec(), Attributes::default());
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
        let id = f.choose(&path);
        let r = f.test(id, CheckLevel::Restoration, false);
        assert!(r.keyless);
        assert_eq!(status(&r, "key_availability"), StatusView::Unknown);
        assert_ne!(r.overall_status, StatusView::Pass);
        assert_ne!(r.exit_code, 0);
        assert!(r
            .dimensions
            .iter()
            .all(|d| d.dimension == "key_availability"
                || d.status != StatusView::Pass
                || d.dimension == "integrity"));
        let e = f
            .run(&JobRequest::PlanRepair {
                subject: id,
                salvage: false,
                archive_id: None,
            })
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::KeyUnavailable);
        assert!(
            e.message.contains("needs the passphrase"),
            "refused by the app first: {e:?}"
        );
    }

    #[test]
    fn health_starts_unknown_and_follows_this_session_s_tests() {
        let f = fx();
        let good = f.good("good.mochi");
        let id = f.choose(&good);
        let h = f.checks.health(id).unwrap();
        assert_eq!(h.records, 0);
        assert_eq!(h.report.kind, ReportKind::Health);
        assert_eq!(status(&h.report, "integrity"), StatusView::Unknown);
        assert_eq!(status(&h.report, "recoverability"), StatusView::Unknown);
        assert_eq!(h.report.exit_code, 2);

        f.test(id, CheckLevel::Restoration, false);
        let h = f.checks.health(id).unwrap();
        assert_eq!(h.records, 1);
        assert_eq!(status(&h.report, "integrity"), StatusView::Pass);
        assert_eq!(status(&h.report, "recoverability"), StatusView::Pass);
        assert_eq!(status(&h.report, "freshness"), StatusView::Unknown);
        assert_ne!(h.report.overall_status, StatusView::Pass);
        let (name, json) = f.checks.report_json(h.report.report_id).unwrap();
        assert_eq!(
            (name.as_str(), json),
            ("good-health-report.json", h.report.json.clone())
        );

        // A structural run assesses nothing and does not erase the PASS.
        f.test(id, CheckLevel::Structural, false);
        assert_eq!(
            status(&f.checks.health(id).unwrap().report, "integrity"),
            StatusView::Pass
        );

        // The same archive (same ID and head), damaged: the failure stands.
        let bad = f.damaged(&good, "bad.mochi");
        let bad_id = f.choose(&bad);
        f.test(bad_id, CheckLevel::Restoration, false);
        let h = f.checks.health(bad_id).unwrap();
        assert_eq!(status(&h.report, "integrity"), StatusView::Fail);
        assert_eq!(h.report.overall_status, StatusView::Fail);
        assert_eq!(h.report.exit_code, 1);
    }

    /// Annex B.2.22: with local history on, the app shares the CLI's head
    /// history and evidence log. First sight is `UNKNOWN`; a test in which
    /// nothing failed records the head, so the next one passes freshness; an
    /// anchor the archive no longer contains (a rollback or substitution)
    /// fails it; the health summary reads the shared log, not the session.
    #[test]
    fn local_history_anchors_freshness_and_is_shared_with_the_cli() {
        use mochi_core::storage::os::history::{HeadStore, SeenHead};
        let f = fx();
        let state = f.at("state");
        crate::history::use_dir_for_test(Some(&state));
        let good = f.good("good.mochi");
        let id = f.choose(&good);

        let first = f.test(id, CheckLevel::Restoration, false);
        assert_eq!(status(&first, "freshness"), StatusView::Unknown);
        assert_eq!(status(&first, "retention_compliance"), StatusView::Pass);
        assert!(state.join("heads.json").exists(), "the head was recorded");
        let second = f.test(id, CheckLevel::Restoration, false);
        assert_eq!(status(&second, "freshness"), StatusView::Pass);

        // The log is the CLI's: a fresh `Checks` (a new session) reads it.
        let later = Checks::default();
        let again = later
            .choose_subject(&f.archives, SubjectPick::Path(good.clone()))
            .unwrap()
            .id;
        let h = later.health(again).unwrap();
        assert_eq!(h.records, 2);
        assert_eq!(status(&h.report, "integrity"), StatusView::Pass);
        assert_eq!(status(&h.report, "freshness"), StatusView::Pass);
        assert_eq!(status(&h.report, "retention_compliance"), StatusView::Pass);

        // A head this archive does not contain was seen before: FAIL.
        let src = OsReadStorage::open(&good).unwrap();
        let archive = crate::history::claimed_archive_id(&src, &ReadOptions::default()).unwrap();
        let store = HeadStore::locate(Some(&state)).unwrap();
        store
            .record(
                &archive,
                SeenHead {
                    seq: 99,
                    commit_id: mochi_format::digest::CommitId::from_bytes([9; 32]),
                },
            )
            .unwrap();
        let rolled = f.test(id, CheckLevel::Structural, false);
        assert_eq!(status(&rolled, "freshness"), StatusView::Fail);
        assert_eq!(rolled.exit_code, 1);
        // A failing run never lowers or replaces the anchor.
        assert_eq!(store.get(&archive).unwrap().unwrap().seq, 99);

        crate::history::use_dir_for_test(None);
    }

    #[test]
    fn a_plan_lists_what_is_left_out_and_writes_nothing() {
        let f = fx();
        let good = f.good("good.mochi");
        let bad = f.damaged(&good, "bad.mochi");
        let before = hash(&bad);
        let files = listing(f.dir.path());
        let id = f.choose(&bad);
        let p = f.plan(id, false);
        assert_eq!((p.outcome, p.exit_code), (RepairOutcomeView::Partial, 2));
        assert_eq!(p.omitted_total, "1");
        assert_eq!(p.omitted[0].path.text, "big");
        assert_eq!(p.omitted[0].code, "STORED_INTEGRITY_FAILED");
        assert_eq!(p.omitted[0].snapshots, vec![2]);
        assert!(p.lost.is_empty());
        assert_eq!(
            p.snapshots.iter().map(|s| s.seq).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let step7 = p.ladder.iter().find(|l| l.step == 7).unwrap();
        assert_eq!(step7.status, "not_attempted");
        assert!(!p.salvage_used);
        assert!(matches!(p.retention, Some(RetentionView::Carried { .. })));
        assert_eq!(hash(&bad), before, "planning is read-only");
        assert_eq!(listing(f.dir.path()), files, "nothing is created");
    }

    #[test]
    fn apply_writes_a_new_verified_archive_without_the_lost_entry() {
        let f = fx();
        let good = f.good("good.mochi");
        let bad = f.damaged(&good, "bad.mochi");
        let before = hash(&bad);
        let id = f.choose(&bad);
        let p = f.plan(id, false);
        // No new file chosen yet.
        let e = f.apply(id, p.plan_id).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        // The source, or any existing file, is refused as the new file.
        let e = f
            .checks
            .setup(RepairSetupAction::ChooseOutput, Some(bad.clone()))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::DestinationExists);
        assert_eq!(f.output("new").output.as_deref(), Some("new.mochi"));
        let r = f.apply(id, p.plan_id).unwrap();
        assert_eq!((r.outcome, r.exit_code), (RepairOutcomeView::Partial, 2));
        assert_eq!(r.file_name, "new.mochi");
        assert_eq!(r.omitted_total, "1");
        assert!(r.source_kept);
        assert_eq!(r.reverification.exit_code, 0);
        assert_eq!(r.commits.len(), 3);
        assert_eq!(hash(&bad), before, "the source is never written");

        // Oracle: the pristine head, without the entry the plan left out.
        let mut expected = state_of(&good);
        expected.remove(b"big".as_slice());
        let new = f.at("new.mochi");
        assert_eq!(state_of(&new), expected);

        // The plan was used; the new file can be tested next.
        assert_eq!(
            f.apply(id, p.plan_id).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let next = f
            .checks
            .choose_subject(&f.archives, SubjectPick::RepairOutput)
            .unwrap();
        assert_eq!(next.file_name, "new.mochi");
        let t = f.test(next.id, CheckLevel::Restoration, true);
        assert_eq!(status(&t, "integrity"), StatusView::Pass);
    }

    #[test]
    fn a_clean_archive_plans_and_applies_complete() {
        let f = fx();
        let good = f.good("good.mochi");
        let id = f.choose(&good);
        let p = f.plan(id, false);
        assert_eq!((p.outcome, p.exit_code), (RepairOutcomeView::Complete, 0));
        assert!(!p.damage_found);
        f.output("copy.mochi");
        let r = f.apply(id, p.plan_id).unwrap();
        assert_eq!(r.outcome, RepairOutcomeView::Complete);
        assert_eq!(r.reverification.policy_result, StatusView::Pass);
        assert_eq!(state_of(&f.at("copy.mochi")), state_of(&good));
    }

    #[test]
    fn apply_refuses_a_new_file_that_appeared_after_it_was_chosen() {
        let f = fx();
        let id = f.choose(&f.good("good.mochi"));
        let p = f.plan(id, false);
        f.output("late.mochi");
        std::fs::write(f.at("late.mochi"), b"someone else's").unwrap();
        let e = f.apply(id, p.plan_id).unwrap_err();
        assert_eq!(e.code, ErrorCode::DestinationExists);
        assert_eq!(
            std::fs::read(f.at("late.mochi")).unwrap(),
            b"someone else's"
        );
    }

    #[test]
    fn the_plan_is_the_approval_and_goes_stale_with_its_inputs() {
        let f = fx();
        let good = f.good("good.mochi");
        let bad = f.damaged(&good, "bad.mochi");
        let id = f.choose(&bad);
        let p = f.plan(id, false);
        f.output("new.mochi");
        // Another plan ID is refused, also by the job itself (the plan can
        // change between validation and the run).
        assert!(f.apply(id, p.plan_id + 1).is_err());
        let stale = JobRequest::ApplyRepair {
            subject: id,
            plan: p.plan_id + 1,
            accept_retention_loss: false,
        };
        let e = f.checks.run(&stale, &Job::new().ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(!f.at("new.mochi").exists());
        // Changing the copies drops the plan.
        f.checks
            .setup(RepairSetupAction::AddCopy, Some(good.clone()))
            .unwrap();
        assert_eq!(
            f.apply(id, p.plan_id).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        // Choosing an archive again drops the plan, the copies, and the file.
        let p = f.plan(id, false);
        let id2 = f.choose(&bad);
        let view = f.checks.setup(RepairSetupAction::Show, None).unwrap();
        assert!(view.copies.is_empty() && view.output.is_none());
        assert!(f.apply(id2, p.plan_id).is_err());
        assert!(f.apply(id, p.plan_id).is_err(), "an old subject is refused");
        assert!(!f.at("new.mochi").exists());
    }

    #[test]
    fn a_copy_supplies_the_damaged_frame_and_the_archive_is_not_its_own_copy() {
        let f = fx();
        let good = f.good("good.mochi");
        let bad = f.damaged(&good, "bad.mochi");
        let id = f.choose(&bad);
        let e = f
            .checks
            .setup(RepairSetupAction::AddCopy, Some(bad.clone()))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        let view = f
            .checks
            .setup(RepairSetupAction::AddCopy, Some(good.clone()))
            .unwrap();
        assert_eq!(view.copies, vec!["good.mochi".to_owned()]);
        assert!(f
            .checks
            .setup(RepairSetupAction::AddCopy, Some(good.clone()))
            .is_err());
        let p = f.plan(id, false);
        assert_eq!(p.outcome, RepairOutcomeView::Complete);
        assert_eq!(p.copies[0].status, "used");
        assert_eq!(p.donations, "1");
        f.output("new.mochi");
        let r = f.apply(id, p.plan_id).unwrap();
        assert_eq!(r.outcome, RepairOutcomeView::Complete);
        assert_eq!(state_of(&f.at("new.mochi")), state_of(&good));
        // Removing copies.
        assert!(f
            .checks
            .setup(RepairSetupAction::RemoveCopy { index: 5 }, None)
            .is_err());
        let v = f
            .checks
            .setup(RepairSetupAction::RemoveCopy { index: 0 }, None)
            .unwrap();
        assert!(v.copies.is_empty());
    }

    #[test]
    fn salvage_options_are_checked_before_a_job_starts() {
        let f = fx();
        let id = f.choose(&f.good("good.mochi"));
        let req = |salvage, archive_id: Option<&str>| JobRequest::PlanRepair {
            subject: id,
            salvage,
            archive_id: archive_id.map(str::to_owned),
        };
        let hex64 = "ab".repeat(32);
        assert!(f.checks.validate(&req(false, Some(&hex64))).is_err());
        assert!(f.checks.validate(&req(true, Some("xyz"))).is_err());
        assert!(f
            .checks
            .validate(&req(true, Some(&"zz".repeat(32))))
            .is_err());
        assert!(f.checks.validate(&req(true, Some(&hex64))).is_ok());
        assert!(f.checks.validate(&req(true, Some("  "))).is_ok());
        let p = f.plan(id, true);
        let step7 = p.ladder.iter().find(|l| l.step == 7).unwrap();
        assert_eq!(step7.status, "not_needed");
    }

    #[test]
    fn requests_name_the_chosen_archive_and_a_file() {
        let f = fx();
        assert!(f
            .checks
            .validate(&JobRequest::TestArchive {
                subject: 1,
                level: CheckLevel::Restoration,
                deep: false
            })
            .is_err());
        let e = f
            .checks
            .choose_subject(&f.archives, SubjectPick::Path(f.dir.path().to_path_buf()))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(f
            .checks
            .choose_subject(&f.archives, SubjectPick::RepairOutput)
            .is_err());
        assert!(f
            .checks
            .choose_subject(&f.archives, SubjectPick::OpenArchive(7))
            .is_err());
        assert!(f.checks.setup(RepairSetupAction::Show, None).is_err());
        let id = f.choose(&f.good("good.mochi"));
        assert!(f
            .checks
            .validate(&JobRequest::TestArchive {
                subject: id + 1,
                level: CheckLevel::Restoration,
                deep: false
            })
            .is_err());
        assert!(f
            .checks
            .validate(&JobRequest::SelfTest {
                steps: 1,
                step_ms: 0
            })
            .is_err());
        // The browser's open archive can be chosen by handle.
        let s = f
            .archives
            .open(&f.at("good.mochi"), ReadOptions::default())
            .unwrap();
        let v = f
            .checks
            .choose_subject(&f.archives, SubjectPick::OpenArchive(s.handle))
            .unwrap();
        assert_eq!(v.file_name, "good.mochi");
        assert!(v.id > id);
    }

    #[test]
    fn cancelled_jobs_end_cancelled_and_write_nothing() {
        let f = fx();
        let good = f.good("good.mochi");
        let files = listing(f.dir.path());
        let id = f.choose(&good);
        let token = CancellationToken::new();
        token.cancel();
        let ctx = JobContext {
            progress: &NullProgress,
            cancel: &token,
        };
        let test = JobRequest::TestArchive {
            subject: id,
            level: CheckLevel::Restoration,
            deep: false,
        };
        assert_eq!(
            f.checks.run(&test, &ctx).unwrap_err().code,
            ErrorCode::Cancelled
        );
        let plan = JobRequest::PlanRepair {
            subject: id,
            salvage: false,
            archive_id: None,
        };
        assert_eq!(
            f.checks.run(&plan, &ctx).unwrap_err().code,
            ErrorCode::Cancelled
        );
        let p = f.plan(id, false);
        f.output("new.mochi");
        let apply = JobRequest::ApplyRepair {
            subject: id,
            plan: p.plan_id,
            accept_retention_loss: false,
        };
        assert_eq!(
            f.checks.run(&apply, &ctx).unwrap_err().code,
            ErrorCode::Cancelled
        );
        assert!(!f.at("new.mochi").exists());
        assert_eq!(listing(f.dir.path()), files, "no temporary file is left");
    }

    #[test]
    fn archive_text_in_plans_is_escaped() {
        let f = fx();
        let path = f.at("names.mochi");
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(SeqIds::new(5)),
            mochi_testkit::archive::test_options(),
        )
        .unwrap();
        let mut tx = Transaction::new();
        let name = "esc\u{1b}[31mred\u{202E}";
        tx.put_file(apath(name), vec![7u8; 200], Attributes::default());
        tx.put_file(apath("ok"), b"fine".to_vec(), Attributes::default());
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
        let src = OsReadStorage::open(&path).unwrap();
        let at =
            first_chunk_middle(&open_head(&src, &ReadOptions::default()).unwrap(), name).unwrap();
        drop(src);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[usize::try_from(at).unwrap()] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();
        let id = f.choose(&path);
        let p = f.plan(id, false);
        let o = &p.omitted[0];
        assert!(!o.path.verbatim);
        assert!(!o.path.text.contains('\u{1b}') && !o.path.text.contains('\u{202E}'));
        assert_eq!(o.path.text, mochi_core::text::escape(name.as_bytes()));
        let r = f.test(id, CheckLevel::Restoration, false);
        for x in &r.findings {
            for t in [&x.message, &x.expected, &x.observed].into_iter().flatten() {
                assert!(!t.text.contains('\u{1b}') && !t.text.contains('\u{202E}'));
            }
        }
    }

    struct Collect(
        std::sync::Mutex<mpsc::Sender<JobEvent>>,
        Arc<JobRegistry>,
        u64,
    );
    impl EventSink for Collect {
        fn send(&self, e: JobEvent) -> bool {
            if let JobEvent::Progress {
                job_id, completed, ..
            } = &e
            {
                if *completed >= self.2 {
                    self.1.cancel(*job_id);
                }
            }
            let _ = self.0.lock().unwrap().send(e);
            true
        }
    }

    fn over_registry(checks: Arc<Checks>, req: JobRequest, cancel_at: u64) -> Vec<JobEvent> {
        let reg = Arc::new(JobRegistry::with_services(Services {
            checks,
            ..Services::default()
        }));
        let (tx, rx) = mpsc::channel();
        reg.start(
            req,
            Arc::new(Collect(
                std::sync::Mutex::new(tx),
                Arc::clone(&reg),
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
                return events;
            }
        }
    }

    #[test]
    fn jobs_stream_progress_and_one_finished_and_cancel_over_the_registry() {
        let f = fx();
        let good = f.good("good.mochi");
        let checks = Arc::new(Checks::default());
        let id = checks
            .choose_subject(&f.archives, SubjectPick::Path(good.clone()))
            .unwrap()
            .id;
        let test = JobRequest::TestArchive {
            subject: id,
            level: CheckLevel::Restoration,
            deep: true,
        };
        let events = over_registry(Arc::clone(&checks), test.clone(), u64::MAX);
        assert!(matches!(events[0], JobEvent::Started { .. }));
        assert!(events
            .iter()
            .any(|e| matches!(e, JobEvent::Progress { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, JobEvent::Finished { .. }))
                .count(),
            1
        );
        match events.last() {
            Some(JobEvent::Finished {
                outcome: JobOutcome::Completed { check: Some(c), .. },
                ..
            }) => assert!(matches!(**c, CheckResult::Test { .. })),
            other => panic!("{other:?}"),
        }
        // Cancelled at the first progress event.
        let events = over_registry(Arc::clone(&checks), test, 0);
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Cancelled,
                ..
            })
        ));
        let plan = JobRequest::PlanRepair {
            subject: id,
            salvage: false,
            archive_id: None,
        };
        let events = over_registry(Arc::clone(&checks), plan, 0);
        assert!(matches!(
            events.last(),
            Some(JobEvent::Finished {
                outcome: JobOutcome::Cancelled,
                ..
            })
        ));
        // A stale request is refused before a job starts.
        let reg = Arc::new(JobRegistry::with_services(Services {
            checks,
            ..Services::default()
        }));
        assert!(reg
            .start(
                JobRequest::ApplyRepair {
                    subject: id,
                    plan: 1,
                    accept_retention_loss: false
                },
                Arc::new(Collect(
                    std::sync::Mutex::new(mpsc::channel().0),
                    Arc::clone(&reg),
                    0
                )),
                Box::new(|w| {
                    std::thread::spawn(w);
                }),
            )
            .is_err());
    }
}
