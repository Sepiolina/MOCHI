//! Verification (spec §20; plan C7): read-only checks at a chosen level,
//! reported per health dimension in report schema v1.
//!
//! # Read-only
//!
//! [`verify`] takes a [`ReadStorage`] and has no way to write: the CLI and
//! the desktop app open the archive with [`crate::storage::OsReadStorage`]
//! (spec §20.2). It never truncates, repairs, or changes the head, and a
//! test hashes the archive before and after. Remembering the last-seen head
//! for freshness (D8) is the client's business and happens outside the
//! archive.
//!
//! # Levels (spec §20.1)
//!
//! Each level includes the ones above it.
//!
//! | Level | What runs |
//! |---|---|
//! | `structural` | head location and tail screen; every footer and commit record from the head back to commit 0; every control object of the history (descriptor, delta and snapshot manifests, catalog images) through [`assess_damage`]; the head opened as a reader opens it (replay included); the head catalog's SQLite and MOCHI rules ([`Catalog::verify`]) |
//! | `referential` | every catalog object has a location; its recorded range is exactly one Zstandard data frame of the recorded stored length, inside the committed prefix and before the head's commit frame (§5.1); every dependency resolves (§5.2) |
//! | `stored_integrity` | every data object's stored bytes: length and stored-object hash |
//! | `content_integrity` | every data object decoded: length and chunk content hash |
//! | `restoration` | every file version of the retained history reassembled, compared with its file-content hash |
//!
//! With `deep` (what `fsck` asks for), every commit of the history is also
//! opened at its own footer, its catalog checked, and its namespace compared
//! with the one the head catalog replays for that commit: two derivations of
//! each snapshot that must agree.
//! Each replay segment is then recovered from its baseline (D10.8). A commit
//! that opens from its image but cannot be rebuilt from S(*b*) and the deltas
//! after it references something outside the baseline view (Annex B D18,
//! "reference scope"): `REFERENCE_INVALID`, recoverability `FAIL`, integrity
//! unaffected, and nothing about reading is refused.
//!
//! `inventory`, `search`, and `disaster_recovery` are not available in 1.0
//! (Preservation profile; search is C13): nothing runs, and the report says
//! `UNSUPPORTED`, exit 4. Nothing is ever reported as checked that was not.
//!
//! # Dimensions (spec §20.3; delegated decisions, recorded in plan C7)
//!
//! * **integrity:** `FAIL` on any violation found at any level, including an
//!   unresolved tail (bytes after the head that may hold a damaged commit).
//!   `PASS` only when the level reads stored bytes (`stored_integrity` or
//!   deeper) and every check ran and passed. Otherwise `UNKNOWN`.
//! * **recoverability:** the worst of [`DamageReport::recoverability`] and,
//!   when data objects were read, `FAIL` for any damaged data object (1.0 has
//!   no parity to rebuild one). `FAIL` too when `deep` finds a commit that
//!   baseline recovery cannot rebuild. `UNKNOWN` if data objects were not read or the
//!   history could not be assessed.
//! * **freshness:** from the anchor (D8): none → `UNKNOWN`; otherwise `PASS`
//!   if the verified history contains the anchor commit (for a local-history
//!   anchor, at its recorded sequence), else `FAIL` (`FRESHNESS_FAILED`).
//!   A head that advanced past the anchor passes: it still contains it.
//! * **durability:** `UNKNOWN`. Bytes cannot show whether they reached stable
//!   media; an interrupted write is reported as a finding.
//! * **key availability:** `PASS` when the descriptor declares no encryption
//!   (no key is needed); `UNKNOWN` if the descriptor was not read.
//! * **searchability, retention compliance:** `UNSUPPORTED` (C13, C9).
//!
//! The policy requires integrity and recoverability, plus freshness when
//! D15 says so. Exit codes follow D15 through [`Report::conclude`].
//!
//! # Errors become evidence
//!
//! [`verify`] does not return `Err`. A failure is classified
//! ([`classify`]): evidence that the archive is wrong is a finding and a
//! `FAIL`; a refusal of this build is `UNSUPPORTED`; anything else (I/O,
//! limits, cancellation) marks the run as an operational error (exit 3) and
//! lists what was skipped.

use std::collections::BTreeSet;

use mochi_format::digest::CommitId;
use mochi_format::frame::{walk_frame, FrameDetail};
use mochi_format::registry::FrameKind;

use crate::catalog::Catalog;
use crate::commit::Metadata;
use crate::damage::{assess_damage, DamageReport};
use crate::error::{ErrorCode, MochiError};
use crate::job::JobContext;
use crate::object::{decode_verified, load_stored, verify_stored, Dependency, ObjectRecord};
use crate::publish::{
    commit_history, locate_head, open_at_footer, open_head, recover_baseline_at_footer, HeadSource,
    HistoryEntry, OpenedHead, ReadOptions, TailState,
};
use crate::read::read_version;
use crate::report::{
    Finding, FreshnessAnchorKind, FreshnessBasis, Policy, Report, Severity, SkippedItem,
};
use crate::status::{Dimension, Status, VerificationLevel};
use crate::storage::{ReadStorage, StorageReader};
use crate::timestamp::Timestamp;

/// Progress phases of [`verify`], in order.
pub mod phase {
    /// Head, history, control objects, catalog. `completed` counts steps.
    pub const STRUCTURE: &str = "verify-structure";
    /// Every commit opened at its own footer (`deep` only).
    pub const HISTORY: &str = "verify-history";
    /// Baseline recovery of every replay segment (`deep` only).
    pub const BASELINE: &str = "verify-baseline";
    /// Data objects: `completed` of `total` objects.
    pub const OBJECTS: &str = "verify-objects";
    /// File versions reassembled: `completed` of `total` versions.
    pub const FILES: &str = "verify-files";
}

/// What freshness is judged against (spec Annex B.1 D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreshnessAnchor {
    /// No anchor: freshness is `UNKNOWN`, and not required unless asked.
    #[default]
    None,
    /// The user named the head they expect.
    User { commit_id: CommitId },
    /// This client last saw `commit_id` at sequence `seq` for the archive.
    LocalHistory { seq: u64, commit_id: CommitId },
}

impl FreshnessAnchor {
    fn kind(&self) -> FreshnessAnchorKind {
        match self {
            FreshnessAnchor::None => FreshnessAnchorKind::None,
            FreshnessAnchor::User { .. } => FreshnessAnchorKind::User,
            FreshnessAnchor::LocalHistory { .. } => FreshnessAnchorKind::LocalHistory,
        }
    }

    fn commit_id(&self) -> Option<CommitId> {
        match self {
            FreshnessAnchor::None => None,
            FreshnessAnchor::User { commit_id }
            | FreshnessAnchor::LocalHistory { commit_id, .. } => Some(*commit_id),
        }
    }
}

/// What to verify and how.
#[derive(Debug, Clone, Copy)]
pub struct VerifyOptions {
    pub level: VerificationLevel,
    pub read: ReadOptions,
    pub anchor: FreshnessAnchor,
    /// Freshness was explicitly requested (D15's third condition): without
    /// an anchor it is then required and `UNKNOWN`, so the run exits 2.
    pub require_freshness: bool,
    /// Also open every commit at its own footer and cross-check its
    /// namespace (`fsck`).
    pub deep: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        VerifyOptions {
            level: VerificationLevel::Restoration,
            read: ReadOptions::default(),
            anchor: FreshnessAnchor::None,
            require_freshness: false,
            deep: false,
        }
    }
}

/// The verified head, for a client that keeps a local-history anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedHead {
    pub archive_id: crate::object::ArchiveId,
    pub seq: u64,
    pub commit_id: CommitId,
}

/// The result of [`verify`]: the report, and the head it checked when one
/// was found and opened.
#[derive(Debug, Clone)]
pub struct Verification {
    pub report: Report,
    pub head: Option<VerifiedHead>,
}

impl Verification {
    /// Whether a client may record [`Verification::head`] as its new
    /// local-history anchor: no dimension failed and the run was not
    /// compromised. A rolled-back archive must never become the anchor.
    pub fn head_is_anchorable(&self) -> bool {
        self.head.is_some()
            && !self.report.operational_error
            && !self.report.dimensions.values().any(|s| *s == Status::Fail)
    }
}

/// How a failure counts as evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Evidence that the archive is wrong: `FAIL`.
    Violation,
    /// This build cannot interpret the archive: `UNSUPPORTED`.
    Unsupported,
    /// The run itself was compromised (I/O, limits, cancellation, a bad
    /// argument): exit 3, and nothing is concluded from it.
    Operational,
}

/// Classify an error found while verifying. Exhaustive on purpose: a new
/// code must be placed explicitly. `OUT_OF_BOUNDS` and `LIMIT_EXCEEDED`
/// are operational, as in the damage rules (review decisions Q31, Q32):
/// a reader limit is configuration, and every archive-derived range is
/// checked against the file before it is read (the referential level
/// reports a range outside the committed prefix as `REFERENCE_INVALID`).
pub fn classify(code: ErrorCode) -> ErrorClass {
    match code {
        ErrorCode::StoredIntegrityFailed
        | ErrorCode::ContentIntegrityFailed
        | ErrorCode::DescriptorInvalid
        | ErrorCode::MalformedFrame
        | ErrorCode::Truncated
        | ErrorCode::FooterInvalid
        | ErrorCode::EnvelopeInvalid
        | ErrorCode::PathInvalid
        | ErrorCode::ExtentInvalid
        | ErrorCode::NamespaceInvalid
        | ErrorCode::CatalogInvalid
        | ErrorCode::IdentityConflict
        | ErrorCode::RecordInvalid
        | ErrorCode::NoValidHead
        | ErrorCode::TailUnresolved
        | ErrorCode::CheckpointMismatch
        | ErrorCode::RetentionUnresolved
        | ErrorCode::ReferenceInvalid
        | ErrorCode::FreshnessFailed
        | ErrorCode::ProfileViolation => ErrorClass::Violation,
        ErrorCode::UnsupportedFeature | ErrorCode::ProfileChangeUnsupported => {
            ErrorClass::Unsupported
        }
        ErrorCode::IoError
        | ErrorCode::OutOfBounds
        | ErrorCode::LimitExceeded
        | ErrorCode::LockConflict
        | ErrorCode::InvalidArgument
        | ErrorCode::Cancelled
        | ErrorCode::NotImplemented
        | ErrorCode::ReportInconsistent
        | ErrorCode::UncommittedTail
        | ErrorCode::CommitUnconfirmed
        | ErrorCode::WriterPoisoned
        | ErrorCode::DestinationExists
        | ErrorCode::CapacityExceeded
        | ErrorCode::QuarantineFailed
        | ErrorCode::DurabilityUnconfirmed
        | ErrorCode::NameCollision
        | ErrorCode::NameUnsupported
        | ErrorCode::AttributeNotRestored => ErrorClass::Operational,
    }
}

fn depth(level: VerificationLevel) -> Option<u8> {
    match level {
        VerificationLevel::Structural => Some(1),
        VerificationLevel::Referential => Some(2),
        VerificationLevel::StoredIntegrity => Some(3),
        VerificationLevel::ContentIntegrity => Some(4),
        VerificationLevel::Restoration => Some(5),
        VerificationLevel::Inventory
        | VerificationLevel::Search
        | VerificationLevel::DisasterRecovery => None,
    }
}

/// Accumulated evidence of one run.
struct Run {
    report: Report,
    violation: bool,
    unsupported: bool,
    operational: bool,
    /// Data objects were read with every planned check completed.
    data_complete: bool,
    data_failed: bool,
    /// Baseline recovery (D10.8) of a segment failed where an image-based
    /// open of the same commit succeeded (`deep` only; Annex B D18, Q64).
    /// A recoverability failure, not an integrity one: the bytes are intact.
    recovery_failed: bool,
}

impl Run {
    fn finding(&mut self, code: ErrorCode, severity: Severity, message: impl Into<String>) {
        self.report.findings.push(Finding {
            code,
            severity,
            message: Some(message.into()),
            expected: None,
            observed: None,
            affected: None,
        });
    }

    /// Record a failure under its class. Returns the class.
    fn error(&mut self, what: &str, e: &MochiError) -> ErrorClass {
        let class = classify(e.code);
        let severity = match class {
            ErrorClass::Violation | ErrorClass::Operational => Severity::Error,
            ErrorClass::Unsupported => Severity::Warning,
        };
        match class {
            ErrorClass::Violation => self.violation = true,
            ErrorClass::Unsupported => self.unsupported = true,
            ErrorClass::Operational => self.operational = true,
        }
        self.finding(e.code, severity, format!("{what}: {}", e.message));
        class
    }

    fn skip(&mut self, item: &str, reason: impl Into<String>) {
        self.report.skipped.push(SkippedItem {
            item: item.to_owned(),
            reason: reason.into(),
        });
    }
}

fn now() -> Option<String> {
    Timestamp::now().ok().map(|t| t.to_string())
}

/// Verify the archive in `src` (see the module documentation). Read-only;
/// a job; never `Err`: every outcome is in the report.
pub fn verify(src: &dyn ReadStorage, opts: &VerifyOptions, ctx: &JobContext<'_>) -> Verification {
    let mut report = Report::new(opts.level);
    report.started_at = now();
    report.freshness_anchor = opts.anchor.kind();
    report.expected_head = opts.anchor.commit_id().map(|c| c.to_hex());
    report.policy = Policy::new(
        [Dimension::Integrity, Dimension::Recoverability],
        FreshnessBasis {
            expected_head_supplied: matches!(opts.anchor, FreshnessAnchor::User { .. }),
            archive_in_local_history: matches!(opts.anchor, FreshnessAnchor::LocalHistory { .. }),
            requested: opts.require_freshness,
        },
    );
    let mut run = Run {
        report,
        violation: false,
        unsupported: false,
        operational: false,
        data_complete: false,
        data_failed: false,
        recovery_failed: false,
    };
    let mut head = None;
    let mut damage = None;
    let mut freshness = Status::Unknown;
    let mut key_availability = Status::Unknown;

    match depth(opts.level) {
        None => {
            run.unsupported = true;
            run.finding(
                ErrorCode::UnsupportedFeature,
                Severity::Warning,
                format!(
                    "verification level {:?} is not available in MOCHI 1.0 (spec §20.1, \
                     §7.7); nothing was checked",
                    opts.level
                ),
            );
            run.report.scope = Some("nothing: the requested level is unsupported".into());
        }
        Some(d) => {
            let checked = check(src, opts, d, ctx, &mut run);
            if let Some(c) = checked {
                freshness = c.freshness;
                key_availability = c.key_availability;
                head = c.head;
                damage = c.damage;
            }
            run.report.scope = Some(scope_text(opts, d));
        }
    }

    let integrity = if run.violation {
        Status::Fail
    } else if run.unsupported {
        Status::Unsupported
    } else if !run.operational && run.data_complete {
        Status::Pass
    } else {
        Status::Unknown
    };
    let recoverability = if run.recovery_failed {
        // Known, whatever else could not be assessed: a segment that image
        // opens read but that a baseline replay cannot rebuild (D10.8).
        Status::Fail
    } else if run.unsupported && !run.violation {
        Status::Unsupported
    } else {
        match &damage {
            // The history could not be assessed: whether anything can be
            // recovered is for the repair ladder (C8).
            None => Status::Unknown,
            Some(d) => {
                let mut s = d.recoverability();
                if run.data_failed {
                    s = Status::Fail;
                } else if !run.data_complete || run.operational {
                    s = Status::rollup([s, Status::Unknown]);
                }
                s
            }
        }
    };
    let dims = &mut run.report.dimensions;
    dims.insert(Dimension::Integrity, integrity);
    dims.insert(Dimension::Recoverability, recoverability);
    dims.insert(Dimension::Freshness, freshness);
    dims.insert(Dimension::Durability, Status::Unknown);
    dims.insert(Dimension::KeyAvailability, key_availability);
    dims.insert(Dimension::Searchability, Status::Unsupported);
    dims.insert(Dimension::RetentionCompliance, Status::Unsupported);
    if opts.level == VerificationLevel::Search {
        run.report.policy.required.insert(Dimension::Searchability);
    }
    run.report.completed_at = now();
    let operational = run.operational;
    run.report.conclude(operational);
    Verification {
        report: run.report,
        head,
    }
}

fn scope_text(opts: &VerifyOptions, d: u8) -> String {
    let data = match d {
        1 => "control objects only; data objects not read",
        2 => "control objects; data-object references and frame boundaries",
        3 => "control objects; every data object's stored bytes",
        4 => "control objects; every data object stored and decoded",
        _ => "control objects; every data object; every retained file version reassembled",
    };
    format!(
        "full retained history: head, every commit record and footer, {data}{}. \
         Durability is not observable from the archive's bytes.",
        if opts.deep {
            "; every commit opened at its own footer"
        } else {
            ""
        }
    )
}

struct Checked {
    head: Option<VerifiedHead>,
    damage: Option<DamageReport>,
    freshness: Status,
    key_availability: Status,
}

fn check(
    src: &dyn ReadStorage,
    opts: &VerifyOptions,
    depth: u8,
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> Option<Checked> {
    let ro = &opts.read;
    let later = |run: &mut Run, reason: &str| {
        run.skip("history and control objects", reason);
        run.skip("head catalog", reason);
        if depth >= 2 {
            run.skip("data objects", reason);
        }
        if depth >= 5 {
            run.skip("file versions", reason);
        }
    };

    // ---- head and tail ------------------------------------------------------
    ctx.report(phase::STRUCTURE, 0, Some(4));
    let location = match locate_head(src, &ro.limits) {
        Ok(l) => l,
        Err(e) => {
            run.error("locating the head", &e);
            later(run, "no head could be located");
            return None;
        }
    };
    match &location.tail {
        TailState::Clean => {}
        TailState::Uncommitted { len, .. } => run.finding(
            ErrorCode::UncommittedTail,
            Severity::Warning,
            format!(
                "{len} bytes after the head look like an interrupted write (Annex B.2 D14 \
                 screen, not proof); appending is refused until they are explicitly truncated"
            ),
        ),
        TailState::Unresolved { len, reason } => {
            run.violation = true;
            run.finding(
                ErrorCode::TailUnresolved,
                Severity::Error,
                format!(
                    "{len} bytes after the head may hold a damaged later commit ({reason}); \
                     the head checked here may not be the latest"
                ),
            );
        }
    }
    if location.source == HeadSource::Scan {
        run.finding(
            ErrorCode::FooterInvalid,
            Severity::Warning,
            "the end of the file is not a valid footer; the head was found by scanning",
        );
    }

    // ---- history and control objects ---------------------------------------
    if ctx.check_cancelled().is_err() {
        return cancelled(run);
    }
    ctx.report(phase::STRUCTURE, 1, Some(4));
    let history = match commit_history(src, ro) {
        Ok(h) => h,
        Err(e) => {
            run.error("walking the commit history", &e);
            later(run, "the commit history could not be read");
            return None;
        }
    };
    let damage = match assess_damage(src, ro, ctx) {
        Ok(d) => {
            if !d.objects.is_empty() {
                run.violation = true;
            }
            run.report.findings.extend(d.findings());
            Some(d)
        }
        Err(e) => {
            if e.code == ErrorCode::Cancelled {
                return cancelled(run);
            }
            run.error("assessing the history's control objects", &e);
            None
        }
    };
    let freshness = judge_freshness(&opts.anchor, &history, run);

    // ---- the head, as a reader opens it ----------------------------------------
    ctx.report(phase::STRUCTURE, 2, Some(4));
    let opened = match open_head(src, ro) {
        Ok(h) => h,
        Err(e) => {
            run.error("opening the head", &e);
            run.skip("head catalog", "the head could not be opened");
            if depth >= 2 {
                run.skip("data objects", "the head could not be opened");
            }
            if depth >= 5 {
                run.skip("file versions", "the head could not be opened");
            }
            return Some(Checked {
                head: None,
                damage,
                freshness,
                key_availability: Status::Unknown,
            });
        }
    };
    if let crate::publish::CatalogSource::SnapshotManifest { image_error } = &opened.catalog_source
    {
        run.finding(
            ErrorCode::StoredIntegrityFailed,
            Severity::Warning,
            format!(
                "the head's catalog was rebuilt from its snapshot manifest because the image \
                 is damaged: {}",
                image_error.message
            ),
        );
    }
    let key_availability = if opened.descriptor.profile().encrypted {
        Status::Unknown
    } else {
        Status::Pass
    };
    let verified = VerifiedHead {
        archive_id: opened.commit.archive_id,
        seq: opened.seq(),
        commit_id: opened.commit_id,
    };
    run.report.archive_id = Some(verified.archive_id.to_hex());
    run.report.checked_commit = Some(verified.commit_id.to_hex());

    ctx.report(phase::STRUCTURE, 3, Some(4));
    if let Err(e) = opened.catalog.verify() {
        run.error("checking the head catalog", &e);
    }
    ctx.report(phase::STRUCTURE, 4, Some(4));

    if opts.deep
        && !(deep_history(src, ro, &opened, &history, ctx, run)
            && baseline_scope(src, ro, &history, ctx, run))
    {
        return cancelled_with(run, Some(verified), damage, freshness, key_availability);
    }

    let bounds = Bounds {
        commit_offset: opened.location.footer.fields.commit_offset,
        committed_len: opened.location.committed_len,
    };
    if depth >= 2 && !data_objects(src, ro, &opened.catalog, bounds, depth, ctx, run) {
        return cancelled_with(run, Some(verified), damage, freshness, key_availability);
    }
    if depth >= 5 && !file_versions(src, ro, &opened.catalog, ctx, run) {
        return cancelled_with(run, Some(verified), damage, freshness, key_availability);
    }
    run.data_complete = depth >= 3 && !run.data_failed;

    Some(Checked {
        head: Some(verified),
        damage,
        freshness,
        key_availability,
    })
}

fn cancelled(run: &mut Run) -> Option<Checked> {
    cancelled_with(run, None, None, Status::Unknown, Status::Unknown)
}

/// The run was cancelled: an operational error, with what remained skipped.
fn cancelled_with(
    run: &mut Run,
    head: Option<VerifiedHead>,
    damage: Option<DamageReport>,
    freshness: Status,
    key_availability: Status,
) -> Option<Checked> {
    run.operational = true;
    run.finding(
        ErrorCode::Cancelled,
        Severity::Error,
        "verification was cancelled; later checks did not run",
    );
    run.skip("remaining checks", "cancelled");
    Some(Checked {
        head,
        damage,
        freshness,
        key_availability,
    })
}

/// The D8 verdict on an anchor: a status and the findings behind it.
/// Shared by [`verify`] and `mochi health`, which must judge freshness the
/// same way (plan K5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreshnessJudgement {
    pub status: Status,
    pub findings: Vec<Finding>,
}

fn freshness_finding(severity: Severity, message: String) -> Finding {
    Finding {
        code: ErrorCode::FreshnessFailed,
        severity,
        message: Some(message),
        expected: None,
        observed: None,
        affected: None,
    }
}

/// D8: does the history contain the anchor? No archive access: the caller
/// walked the history (head last). An empty history, or no anchor, is
/// `UNKNOWN`.
pub fn judge_freshness_of(
    anchor: &FreshnessAnchor,
    history: &[HistoryEntry],
) -> FreshnessJudgement {
    let Some(head) = history.last() else {
        return FreshnessJudgement {
            status: Status::Unknown,
            findings: Vec::new(),
        };
    };
    let fail = |msg: String| FreshnessJudgement {
        status: Status::Fail,
        findings: vec![freshness_finding(Severity::Error, msg)],
    };
    let pass = FreshnessJudgement {
        status: Status::Pass,
        findings: Vec::new(),
    };
    match anchor {
        FreshnessAnchor::None => FreshnessJudgement {
            status: Status::Unknown,
            findings: Vec::new(),
        },
        FreshnessAnchor::User { commit_id } => {
            match history.iter().find(|e| e.commit_id == *commit_id) {
                Some(e) if e.commit.seq == head.commit.seq => pass,
                Some(e) => FreshnessJudgement {
                    status: Status::Pass,
                    findings: vec![freshness_finding(
                        Severity::Info,
                        format!(
                            "the expected head is commit {}; the archive has advanced to \
                             commit {} and still contains it",
                            e.commit.seq, head.commit.seq
                        ),
                    )],
                },
                None => fail(format!(
                    "the expected head {} is not in this archive's history (head is commit \
                     {}): rolled back or substituted",
                    commit_id.to_hex(),
                    head.commit.seq
                )),
            }
        }
        FreshnessAnchor::LocalHistory { seq, commit_id } => {
            match history.iter().find(|e| e.commit.seq == *seq) {
                None => fail(format!(
                    "this client last saw commit {seq} of this archive, but its head is \
                     commit {}: rolled back",
                    head.commit.seq
                )),
                Some(e) if e.commit_id != *commit_id => fail(format!(
                    "commit {seq} is not the commit this client last saw ({} expected, {} \
                     found): substituted",
                    commit_id.to_hex(),
                    e.commit_id.to_hex()
                )),
                Some(_) => pass,
            }
        }
    }
}

/// D8: does the verified history contain the anchor?
fn judge_freshness(anchor: &FreshnessAnchor, history: &[HistoryEntry], run: &mut Run) -> Status {
    let j = judge_freshness_of(anchor, history);
    if j.status == Status::Fail {
        run.violation = true;
    }
    run.report.findings.extend(j.findings);
    j.status
}

/// `deep`: open every commit at its footer and compare its namespace with
/// the head catalog's replay of that commit. `false` if cancelled.
fn deep_history(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    head: &OpenedHead,
    history: &[HistoryEntry],
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    let total = history.len() as u64;
    for (i, e) in history.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            return false;
        }
        ctx.report(phase::HISTORY, i as u64, Some(total));
        let what = format!("commit {}", e.commit.seq);
        let at = match open_at_footer(src, e.footer_offset, ro) {
            Ok(at) => at,
            Err(err) => {
                run.error(&format!("opening {what} at its footer"), &err);
                continue;
            }
        };
        if let Err(err) = at.catalog.verify() {
            run.error(&format!("checking the catalog of {what}"), &err);
            continue;
        }
        let theirs = at.catalog.replay(None);
        let ours = head.catalog.replay(Some(e.commit.seq));
        match (theirs, ours) {
            (Ok(a), Ok(b)) if a == b => {}
            (Ok(_), Ok(_)) => {
                run.violation = true;
                run.finding(
                    ErrorCode::NamespaceInvalid,
                    Severity::Error,
                    format!("{what}: its namespace differs from the head catalog's replay of it"),
                );
            }
            (Err(err), _) | (_, Err(err)) => {
                run.error(&format!("replaying {what}"), &err);
            }
        }
    }
    ctx.report(phase::HISTORY, total, Some(total));
    true
}

/// `deep`: what a baseline replay of each segment can see (Annex B D18,
/// "reference scope", checklist Q64 [delegated 2026-10-08]).
///
/// D10.4 lets a delta reference an existing version or chunk, but an
/// image-based open sees the whole catalog while baseline recovery (D10.8)
/// sees S(*b*) plus the deltas after *b*. A delta that references something
/// in neither opens normally and cannot be recovered from its segment's
/// snapshot. Writers never produce one; this finds one that something else
/// did. It is reported as `REFERENCE_INVALID` under recoverability only:
/// reading is not refused, and integrity is unaffected (the bytes are
/// intact).
///
/// Deltas apply in order, so recovering a segment's last commit succeeds
/// exactly when every commit of the segment recovers: one recovery per
/// segment in the common case. Only on failure is each commit recovered in
/// turn, to name the first one that fails. A commit that does not open at
/// its footer is not blamed here: [`deep_history`] has already reported it.
/// `false` if cancelled.
fn baseline_scope(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    history: &[HistoryEntry],
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    let base_of = |e: &HistoryEntry| match e.commit.metadata {
        Metadata::Checkpoint { .. } => e.commit.seq,
        Metadata::Delta { base } => base.seq,
    };
    let total = history.len() as u64;
    ctx.report(phase::BASELINE, 0, Some(total));
    let mut done = 0u64;
    let mut at = 0;
    while at < history.len() {
        let base = base_of(&history[at]);
        let end = history[at..]
            .iter()
            .position(|e| base_of(e) != base)
            .map_or(history.len(), |n| at + n);
        let segment = &history[at..end];
        at = end;
        if ctx.check_cancelled().is_err() {
            return false;
        }
        let Some(last) = segment.last() else { continue };
        match recover_baseline_at_footer(src, last.footer_offset, ro) {
            Ok(_) => {}
            Err(e) if classify(e.code) == ErrorClass::Operational => {
                run.error(
                    &format!("recovering the segment from commit {base} at its baseline"),
                    &e,
                );
            }
            Err(_) => {
                for entry in segment {
                    if ctx.check_cancelled().is_err() {
                        return false;
                    }
                    if open_at_footer(src, entry.footer_offset, ro).is_err() {
                        continue;
                    }
                    match recover_baseline_at_footer(src, entry.footer_offset, ro) {
                        Ok(_) => {}
                        Err(e) if classify(e.code) == ErrorClass::Operational => {
                            run.error(
                                &format!("recovering commit {} at its baseline", entry.commit.seq),
                                &e,
                            );
                            break;
                        }
                        Err(e) => {
                            run.recovery_failed = true;
                            run.finding(
                                ErrorCode::ReferenceInvalid,
                                Severity::Error,
                                format!(
                                    "commit {} opens from its catalog image but cannot be \
                                     recovered from commit {base}'s snapshot and the deltas \
                                     after it ({}: {}); it references something a baseline \
                                     replay does not see",
                                    entry.commit.seq,
                                    e.code.as_str(),
                                    e.message
                                ),
                            );
                            break;
                        }
                    }
                }
            }
        }
        done += segment.len() as u64;
        ctx.report(phase::BASELINE, done, Some(total));
    }
    true
}

/// Referential, stored, and content checks of every data object. `false` if
/// cancelled.
fn data_objects(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    cat: &Catalog,
    bounds: Bounds,
    depth: u8,
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    let ids = match cat.object_ids() {
        Ok(ids) => ids,
        Err(e) => {
            run.error("listing the catalog's objects", &e);
            run.skip("data objects", "the catalog's objects could not be listed");
            return true;
        }
    };
    let known: BTreeSet<_> = ids.iter().copied().collect();
    let commit_offset = bounds.commit_offset;
    let prefix = match StorageReader::prefix(src, bounds.committed_len) {
        Ok(r) => r,
        Err(e) => {
            run.error("reading the committed prefix", &MochiError::from(e));
            run.skip("data objects", "the committed prefix could not be read");
            return true;
        }
    };
    let total = ids.len() as u64;
    let mut expected_bytes = 0u64;
    let mut checked_objects = 0u64;
    let mut checked_bytes = 0u64;
    ctx.report(phase::OBJECTS, 0, Some(total));
    for (i, id) in ids.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            record_coverage(run, total, expected_bytes, checked_objects, checked_bytes);
            return false;
        }
        let record = match cat.object(id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                run.violation = true;
                run.data_failed = true;
                run.finding(
                    ErrorCode::ReferenceInvalid,
                    Severity::Error,
                    format!("object {}: listed but has no record", id.to_hex()),
                );
                continue;
            }
            Err(e) => {
                run.data_failed |=
                    run.error(&format!("object {}", id.to_hex()), &e) == ErrorClass::Violation;
                continue;
            }
        };
        expected_bytes = expected_bytes.saturating_add(record.stored_len);
        let at = match referential(cat, &record, &known, commit_offset, &prefix, ro) {
            Ok(at) => at,
            Err(e) => {
                run.data_failed |=
                    run.error(&format!("object {}", id.to_hex()), &e) == ErrorClass::Violation;
                continue;
            }
        };
        if depth >= 3 {
            let result = load_stored(src, at, &record, &ro.limits).and_then(|stored| {
                if depth >= 4 {
                    decode_verified(&record, &stored, &ro.limits).map(|_| ())
                } else {
                    verify_stored(&record, &stored)
                }
            });
            match result {
                Ok(()) => {
                    checked_objects += 1;
                    checked_bytes = checked_bytes.saturating_add(record.stored_len);
                }
                Err(e) => {
                    run.data_failed |= run
                        .error(&format!("object {} at offset {at}", id.to_hex()), &e)
                        == ErrorClass::Violation;
                }
            }
        }
        ctx.report(phase::OBJECTS, i as u64 + 1, Some(total));
    }
    record_coverage(run, total, expected_bytes, checked_objects, checked_bytes);
    true
}

fn record_coverage(run: &mut Run, total: u64, expected: u64, objects: u64, bytes: u64) {
    let c = &mut run.report.coverage;
    c.expected_objects = Some(total);
    c.expected_bytes = Some(expected);
    c.checked_objects = Some(objects);
    c.checked_bytes = Some(bytes);
}

/// §5.1, §5.2: the object's range is exactly one data frame of its stored
/// length, inside the committed prefix and before the head's commit frame,
/// and its dependencies resolve. Returns the location.
fn referential(
    cat: &Catalog,
    record: &ObjectRecord,
    known: &BTreeSet<crate::object::ObjectId>,
    commit_offset: u64,
    prefix: &StorageReader<'_>,
    ro: &ReadOptions,
) -> Result<u64, MochiError> {
    let bad = |msg: String| MochiError::new(ErrorCode::ReferenceInvalid, msg);
    let at = cat
        .object_location(&record.id)?
        .ok_or_else(|| bad("the catalog records no location".into()))?;
    let end = at
        .checked_add(record.stored_len)
        .ok_or_else(|| bad(format!("range at {at} overflows")))?;
    if end > commit_offset {
        return Err(bad(format!(
            "range {at}..{end} does not end before the head's commit frame at {commit_offset}"
        )));
    }
    let span = walk_frame(prefix, at, &ro.limits).map_err(|e| {
        let e = MochiError::from(e);
        bad(format!("no valid frame at {at}: {}", e.message))
    })?;
    if span.kind != FrameKind::ZstdData
        || !matches!(span.detail, FrameDetail::Data(_))
        || span.len != record.stored_len
    {
        return Err(bad(format!(
            "the frame at {at} is a {:?} frame of {} bytes, not a data frame of the recorded \
             {} bytes",
            span.kind, span.len, record.stored_len
        )));
    }
    for d in &record.dependencies {
        let (Dependency::Dictionary(dep) | Dependency::KeyEnvelope(dep)) = d;
        if !known.contains(dep) {
            return Err(bad(format!(
                "dependency {} is not in the catalog (§5.2)",
                dep.to_hex()
            )));
        }
    }
    Ok(at)
}

/// Restoration: reassemble every retained file version. `false` if
/// cancelled.
fn file_versions(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    cat: &Catalog,
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    let ids = match cat.file_version_ids() {
        Ok(ids) => ids,
        Err(e) => {
            run.error("listing the catalog's file versions", &e);
            run.skip(
                "file versions",
                "the catalog's file versions could not be listed",
            );
            return true;
        }
    };
    let total = ids.len() as u64;
    ctx.report(phase::FILES, 0, Some(total));
    for (i, id) in ids.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            return false;
        }
        let is_dir = matches!(
            cat.file_version(id),
            Ok(Some((v, _))) if v.kind == crate::catalog::namespace::EntryKind::Directory
        );
        if !is_dir {
            // Progress inside one file is not forwarded: the per-version
            // count is what this phase reports.
            let quiet = crate::job::NullProgress;
            let inner = JobContext {
                progress: &quiet,
                cancel: ctx.cancel,
            };
            match read_version(src, cat, id, &mut std::io::sink(), ro, &inner) {
                Ok(_) => {}
                Err(e) if e.code == ErrorCode::Cancelled => return false,
                Err(e) => {
                    run.data_failed |= run.error("restoration check", &e) == ErrorClass::Violation;
                }
            }
        }
        ctx.report(phase::FILES, i as u64 + 1, Some(total));
    }
    true
}

/// Where a catalog's objects must lie: before `commit_offset` (the commit
/// frame that references them, §5.1), inside the first `committed_len`
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub commit_offset: u64,
    pub committed_len: u64,
}

/// What [`check_catalog_contents`] found.
#[derive(Debug, Clone)]
pub struct CatalogCheck {
    pub findings: Vec<Finding>,
    pub skipped: Vec<SkippedItem>,
    pub coverage: crate::report::Coverage,
    /// A violation was found (an integrity `FAIL`).
    pub violation: bool,
    /// Every check of the level ran: nothing was cancelled, unsupported, or
    /// operationally interrupted.
    pub complete: bool,
}

/// The data-object and file-version checks of `level` (referential and
/// deeper) for any catalog whose objects lie in `src` within `bounds`:
/// what [`verify`] runs on the head catalog, for a catalog obtained some
/// other way (recovery, repair re-verification, tests). Structural checks
/// of the archive are not part of it. Read-only; a job.
pub fn check_catalog_contents(
    src: &dyn ReadStorage,
    cat: &Catalog,
    bounds: Bounds,
    level: VerificationLevel,
    read: &ReadOptions,
    ctx: &JobContext<'_>,
) -> CatalogCheck {
    let mut run = Run {
        report: Report::new(level),
        violation: false,
        unsupported: false,
        operational: false,
        data_complete: false,
        data_failed: false,
        recovery_failed: false,
    };
    let d = depth(level).unwrap_or(0);
    let mut finished = true;
    if d == 0 {
        run.unsupported = true;
        run.skip(
            "catalog contents",
            "the level is not available in MOCHI 1.0",
        );
    }
    if d >= 2 {
        finished = data_objects(src, read, cat, bounds, d, ctx, &mut run);
    }
    if finished && d >= 5 {
        finished = file_versions(src, read, cat, ctx, &mut run);
    }
    if !finished {
        run.operational = true;
        run.skip("remaining catalog checks", "cancelled");
    }
    CatalogCheck {
        complete: finished && !run.unsupported && !run.operational,
        violation: run.violation,
        findings: run.report.findings,
        skipped: run.report.skipped,
        coverage: run.report.coverage,
    }
}

/// Parse a commit ID given as 64 hexadecimal digits (an expected head).
pub fn parse_commit_id(s: &str) -> Result<CommitId, MochiError> {
    let bad = || {
        MochiError::new(
            ErrorCode::InvalidArgument,
            format!("{s:?} is not a commit ID (64 hexadecimal digits)"),
        )
    };
    let b = s.as_bytes();
    if b.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks_exact(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16).ok_or_else(bad)?;
        let lo = (pair[1] as char).to_digit(16).ok_or_else(bad)?;
        // `i` < 32 because `b` has 64 bytes; `get_mut` keeps it panic-free.
        if let Some(slot) = out.get_mut(i) {
            *slot = u8::try_from(hi * 16 + lo).map_err(|_| bad())?;
        }
    }
    Ok(CommitId::from_bytes(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_ids_parse_from_hex_only() {
        let hex = "00ff".repeat(16);
        let id = parse_commit_id(&hex).unwrap();
        assert_eq!(id.to_hex(), hex);
        assert_eq!(parse_commit_id(&hex.to_uppercase()).unwrap(), id);
        for bad in ["", "00", &"0g".repeat(32), &"0".repeat(65), &"é".repeat(32)] {
            assert_eq!(
                parse_commit_id(bad).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad:?}"
            );
        }
    }

    /// Integrity failures are violations, refusals unsupported, and I/O
    /// operational: the three D15 inputs.
    #[test]
    fn errors_are_classified_for_d15() {
        assert_eq!(
            classify(ErrorCode::StoredIntegrityFailed),
            ErrorClass::Violation
        );
        assert_eq!(classify(ErrorCode::FreshnessFailed), ErrorClass::Violation);
        assert_eq!(classify(ErrorCode::ProfileViolation), ErrorClass::Violation);
        assert_eq!(
            classify(ErrorCode::UnsupportedFeature),
            ErrorClass::Unsupported
        );
        assert_eq!(classify(ErrorCode::IoError), ErrorClass::Operational);
        assert_eq!(classify(ErrorCode::Cancelled), ErrorClass::Operational);
    }
}
