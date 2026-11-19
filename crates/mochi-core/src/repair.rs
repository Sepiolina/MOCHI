//! Recovery and repair (spec §22, §22.1, §22.2; plan C8).
//!
//! [`plan`] is read-only. It walks the §22 ladder as far as this build
//! goes, decides what a repaired archive can hold, and returns a
//! [`RepairPlan`] naming the source of every snapshot it recovers and every
//! entry it cannot. [`apply`] takes an approved plan, surveys the source
//! again, and refuses unless the result is identical (the plan is what the
//! user approved, §22.2); it then writes the recovered snapshots into a
//! **new archive**, verifies that archive independently before publishing
//! it, and never writes to the source.
//!
//! # Owner decisions (2026-10-08, plan C8 status)
//!
//! 1. **Output is a rewrite**, as for compaction (Annex B D18): every
//!    recovered snapshot becomes one commit of a new archive with a new
//!    archive ID, published by the D13 mechanism; the source is kept.
//!    Object and file-version IDs, stored chunk bytes, and promised
//!    attributes are preserved (O19). Commit IDs bind footer offsets and
//!    cannot survive, so freshness anchors start over.
//! 2. **An entry whose content or metadata cannot be recovered is left out**
//!    of every repaired snapshot that held it, and listed by path, version
//!    ID, snapshots, and reason in the plan and the report only. Nothing in
//!    the new archive records the repair (no wire-format change): the plan
//!    and report are the record (§22.2 "record unrecoverable files and
//!    affected snapshots explicitly").
//! 3. **Ladder steps 1–6** in this build: the expected head and its
//!    footer, earlier footers (the forward scan of [`locate_head`]; the
//!    footer-history accelerator is not written by this build), checkpoints
//!    and metadata chains (catalog images), recovery manifests (a damaged
//!    image's snapshot manifest), and parity (C12, Annex B.2.11 D21).
//!    Independent copies (step 5, Annex B.2.12 D22) are taken when the caller
//!    names them; the bounded salvage scan (step 7, Annex B.2.13 D23) only
//!    when the caller asks for it. Each is reported as such in
//!    [`RepairPlan::ladder`], never as passed.
//!
//! **Parity runs first** although it is step 6: a Redundancy-profile
//! archive's damaged shards are rebuilt into an overlay
//! ([`crate::parity::HealedSource`]) and steps 1–4 then run over the healed
//! bytes, so a commit record, footer, catalog, or chunk that parity rebuilt
//! is used as if it had never been damaged (every hash is still checked).
//! **Copies come next** ([`crate::copies`]): byte copies of the archive, named
//! by the caller, donate whole frames over the parity-healed view where the
//! source's frame fails its check and the copy's passes, inside the commits
//! the copy agrees on with the source. A copy supplies bytes; the source
//! supplies the trust, so the survey below still verifies everything it
//! uses against the commit chain. Every donated frame is listed in
//! [`RepairPlan::donations`], and a snapshot whose commit only a copy can
//! show is labelled [`Trust::CopyConfirmed`]. A repair made complete by
//! copies is complete: its result is fully verified (D22 item 7).
//!
//! **Salvage comes last** ([`crate::salvage`], only with `--salvage`, and only
//! if the survey above left something unrecovered): the scan finds commits by
//! frame, decides which were published (their own footer, or a kept commit's
//! parent link), regenerates their footers (and, when it hashes to what the
//! commits record, the descriptor) in the in-memory view, and the survey runs
//! again over the trusted head's chain and the top of each salvaged segment,
//! in that order, so the trusted chain's snapshots are never replaced. A
//! snapshot that needed the scan is labelled [`Trust::Salvage`], an
//! unconfirmed commit is listed and never recovered, and any plan that used
//! the scan or stopped at its limit is **partial**, whatever it recovered
//! (D23 item 9).
//!
//! A group whose damage exceeds its parity is listed in
//! [`RepairPlan::parity`] (`PARITY_CAPACITY_EXCEEDED`), and what the damage
//! touches is left out as before: never rebuilt wrong.
//!
//! # Rules [delegated] (recorded in the plan, C8 status)
//!
//! * **Trust flows from the head** (§22.1, as in recovery): the commits
//!   used are the head and every commit reachable from it by validated
//!   parent links. A break in the chain ends it; nothing before the break
//!   is taken from a scan. Snapshots before a break are still recovered
//!   when a catalog bound to a trusted commit holds their history (a
//!   checkpoint image holds every earlier snapshot, §10.6), but their
//!   commit records, and so their commit IDs and times, are not.
//! * **Where each snapshot comes from.** The trusted commits are opened from
//!   the head down with every check of
//!   [`crate::publish::open_at_footer`] (an image, or its snapshot
//!   manifest when the image's stored bytes are damaged, D10.9); each
//!   catalog that opens supplies every snapshot it holds that no later one
//!   supplied. A snapshot no catalog supplies is **lost**, with the reason
//!   its own open gave.
//! * **What an entry needs.** A file version is kept only if every chunk
//!   verifies (stored and content integrity) and its logical stream matches
//!   its file-content hash ([`crate::read::read_version`]); any version,
//!   file or directory, also needs its promised attributes from a verified
//!   manifest bound to a trusted commit (delta or snapshot), and manifests
//!   that disagree about a version reject it. Nothing is invented (§22.1).
//!   Below an omitted directory, every entry is omitted too, because a
//!   namespace never holds an entry without its parent (§10.2).
//! * **Retention** is the head's, rebuilt from its segment's manifests
//!   (D10.10). Holds and expiry carry over to the snapshots that survive,
//!   at their new sequences; a hold on a lost snapshot is lost and listed.
//!   If the head's retention cannot be rebuilt (or the head itself is
//!   lost), the plan says so and [`apply`] refuses unless the caller waives
//!   it ([`RepairOptions::accept_retention_loss`]), which the report
//!   records: holds are never guessed.
//! * **Outcome and exit code.** `complete` (exit 0): every snapshot and
//!   every entry recovered, retention carried, and nothing after the head's
//!   footer. `partial` (exit 2): anything less, including any tail at all:
//!   even an *eligible* tail (D14) is a screen, not proof that no commit
//!   was there, and this slice has no salvage scan to look.
//!   `nothing_recoverable` (exit 1): no snapshot could be recovered, and
//!   [`apply`] writes nothing. A partial repair is never labelled a
//!   complete one (§22.2).
//! * **Re-verification** (§22.2): before publication, the new archive must
//!   reproduce the planned namespaces, attributes, and retention, and
//!   [`crate::verify::verify`] (restoration level, `fsck` depth) must
//!   report every dimension of its policy `PASS`. Otherwise nothing is
//!   published.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::namespace::{EntryKind, FileVersionId, Snapshot};
use crate::catalog::path::ArchivePath;
use crate::compact::{all_attributes, Copier};
use crate::copies::{self, CopyReport, CopySource, CopyStatus, Donation, Trust};
use crate::error::{ErrorCode, MochiError, Result};
use crate::exit;
use crate::gc::resolve_retention;
use crate::job::JobContext;
use crate::manifest::{Attributes, ManifestKind, Mtime};
use crate::object::IdSource;
use crate::parity::HealedSource;
#[cfg(any(test, feature = "test-controls"))]
use crate::publish::CheckpointPolicy;
use crate::publish::{
    checkpoint_snapshot_ref, locate_head, open_at_footer, open_head, read_bound_manifest,
    read_commit, recorded_writer_parameters, segment_state, walk_back_tolerant, ArchiveWriter,
    CatalogSource, CheckpointTrigger, Dedup, HeadSource, HistoryEntry, OpenedHead,
    PublishDurability, ReadOptions, TailState, WriterOptions,
};
use crate::read::read_version;
use crate::retention::{RetentionOp, RetentionState};
use crate::salvage::{self, SalvageOptions, SalvageReport};
use crate::status::Status;
use crate::storage::{ReadStorage, Storage, StorageDir};
use crate::verify::{classify, verify, ErrorClass, VerifyOptions};

/// Progress phases, in order (then the writer's own phases for each
/// commit, `publish`, and `directory`).
pub mod phase {
    pub const SURVEY: &str = "repair-survey";
    pub const CHECK: &str = "repair-check";
    pub const SALVAGE: &str = "repair-salvage";
    pub const COPY: &str = "repair-copy";
    pub const VERIFY: &str = "repair-verify";
}

/// The `format` of every plan this build writes.
///
/// v2 (Annex B.2.12 D22) adds `copies`, `donations`, and each recovered
/// snapshot's `trust`. A v1 plan is refused by [`apply`]: plan again.
pub const PLAN_FORMAT: &str = "mochi-repair-plan-v2";

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why something failed: a stable code and the message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Problem {
    pub code: ErrorCode,
    pub message: String,
}

impl From<&MochiError> for Problem {
    fn from(e: &MochiError) -> Self {
        Problem {
            code: e.code,
            message: e.message.clone(),
        }
    }
}

/// A failure that is evidence about the archive is recorded; anything that
/// compromises the run itself (I/O, limits, cancellation) stops it.
fn evidence(e: MochiError) -> Result<Problem> {
    match classify(e.code) {
        ErrorClass::Operational => Err(e),
        ErrorClass::Violation | ErrorClass::Unsupported => Ok(Problem::from(&e)),
    }
}

/// What happened at one rung of the §22 ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// The step supplied what the plan uses.
    Used,
    /// Earlier steps sufficed.
    NotNeeded,
    /// The step ran and found nothing usable.
    NotAvailable,
    /// The step was not asked for (step 7 needs `--salvage`); not a pass.
    NotAttempted,
    /// The step ran and stopped at a limit before it finished (the salvage
    /// scan at `max-salvage-candidates`); never a pass.
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LadderStep {
    /// The §22 step number, 1–7.
    pub step: u8,
    pub name: String,
    pub status: StepStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHead {
    pub seq: u64,
    pub commit_id: String,
    pub footer_offset: u64,
    pub committed_len: u64,
    /// `eof`: the footer at end of file; `scan`: the latest valid footer
    /// found by a forward scan because the end of the file is not one.
    pub found_by: String,
}

/// What follows the head's footer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tail {
    /// `clean`, `uncommitted` (eligible for truncation, D14: what an
    /// interrupted write leaves), or `unresolved` (could hold a damaged
    /// later commit).
    pub state: String,
    pub len: u64,
    pub detail: Option<String>,
}

/// The first trusted commit, whose parent could not be validated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainBreak {
    pub first_trusted_seq: u64,
    pub reason: Problem,
}

/// Which representation a catalog came from (D10.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    Image,
    /// The image's stored bytes are damaged; rebuilt from the same
    /// checkpoint's snapshot manifest.
    SnapshotManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveredSnapshot {
    pub seq: u64,
    /// `None` before a chain break: the commit record is not trusted.
    pub commit_id: Option<String>,
    /// The catalog that supplies it: opened at commit `opened_at`.
    pub opened_at: u64,
    pub catalog: CatalogKind,
    /// Entries kept, and entries left out (listed in
    /// [`RepairPlan::omitted`]).
    pub entries: u64,
    pub omitted: u64,
    /// How its commit is established (D22 item 6).
    #[serde(default)]
    pub trust: Trust,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LostSnapshot {
    pub seq: u64,
    pub commit_id: Option<String>,
    pub reason: Problem,
}

/// One (path, version) left out of every recovered snapshot that held it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmittedEntry {
    /// The stored path, lossily decoded for display; `path_hex` is exact.
    pub path: String,
    pub path_hex: String,
    pub version_id: String,
    /// `file` or `directory`.
    pub kind: String,
    pub snapshots: Vec<u64>,
    pub reason: Problem,
}

/// A manifest bound to a trusted commit that failed its checks. The plan
/// does not need it, or works around it; it is listed as damage found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestFailure {
    pub seq: u64,
    /// `delta` or `snapshot`.
    pub kind: String,
    pub reason: Problem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedHold {
    /// Lossily decoded for display; `label_hex` is exact.
    pub label: String,
    pub label_hex: String,
    /// The source snapshot held.
    pub seq: u64,
    /// The new commit that reproduces it; `None`: the snapshot is lost and
    /// so is the hold.
    pub new_seq: Option<u64>,
}

/// The source head's retention state and what carries over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RetentionPlan {
    Carried {
        holds: Vec<PlannedHold>,
        /// Source snapshots that stay expired in the new archive. (The new
        /// head is never among them: retention is resolved only when the
        /// source head opens, and then it is the new head, which the source
        /// cannot have expired.)
        expired: Vec<u64>,
    },
    /// Not rebuildable: [`apply`] needs
    /// [`RepairOptions::accept_retention_loss`], and the new archive then
    /// has no holds and nothing expired.
    Unresolved { reason: Problem },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Complete,
    Partial,
    NothingRecoverable,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Outcome::Complete => exit::OK,
            Outcome::Partial => exit::DEGRADED,
            Outcome::NothingRecoverable => exit::FAILED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterParameters {
    pub chunk_size: u64,
    pub zstd_level: i32,
}

/// What a repair would do, decided from the source alone. Deterministic:
/// the same bytes give the same plan, which is how [`apply`] checks that
/// the plan it is given is still the one that applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairPlan {
    /// [`PLAN_FORMAT`].
    pub format: String,
    /// From the head commit; `None` when no head was found.
    pub archive_id: Option<String>,
    pub source_len: u64,
    pub head: Option<SourceHead>,
    pub tail: Option<Tail>,
    pub chain_break: Option<ChainBreak>,
    pub ladder: Vec<LadderStep>,
    /// Ascending by sequence; each becomes one commit of the new archive,
    /// in this order.
    pub snapshots: Vec<RecoveredSnapshot>,
    pub lost_snapshots: Vec<LostSnapshot>,
    pub omitted: Vec<OmittedEntry>,
    pub manifest_failures: Vec<ManifestFailure>,
    /// `None` when nothing is recoverable.
    pub retention: Option<RetentionPlan>,
    /// The Redundancy profile's scan (step 6), when the archive has a
    /// recovery record: what was rebuilt, what was beyond capacity.
    #[serde(default)]
    pub parity: Option<crate::parity::ParityScan>,
    /// The copies given (`--copy`), in argument order, and what each did
    /// (step 5, Annex B.2.12 D22).
    #[serde(default)]
    pub copies: Vec<CopyReport>,
    /// Every frame a copy supplies, sorted by offset: the source of each
    /// reconstructed object (§22.2).
    #[serde(default)]
    pub donations: Vec<Donation>,
    /// What the bounded salvage scan (step 7, Annex B.2.13 D23) found, when
    /// it ran.
    #[serde(default)]
    pub salvage: Option<SalvageReport>,
    /// Recorded by a catalog image and carried over; `None`: no image could
    /// be read, and the new archive records this build's defaults.
    pub writer_parameters: Option<WriterParameters>,
    /// The survey ran into something wrong. It reads only what the repair
    /// needs (for example, not an older checkpoint whose history a later
    /// catalog supplies), so `false` is not a verification result: that is
    /// `verify` / `fsck`.
    pub damage_found: bool,
    pub outcome: Outcome,
    pub exit_code: u8,
}

/// Everything [`apply`] needs beyond the plan itself.
struct Survey {
    plan: RepairPlan,
    /// Opened catalogs, from the head down.
    opened: Vec<OpenedHead>,
    /// Recovered sequence → index into `opened`.
    assigned: BTreeMap<u64, usize>,
    times: BTreeMap<u64, Mtime>,
    /// The archive comment at each recovered version (D29); a version that
    /// had none is absent, and the rewrite clears the comment there.
    comments: BTreeMap<u64, String>,
    bad: BTreeMap<FileVersionId, Problem>,
    attributes: BTreeMap<FileVersionId, Attributes>,
    /// The retention state the new archive must end with, and the
    /// operations that produce it (on its last commit).
    new_retention: RetentionState,
    retention_ops: Vec<RetentionOp>,
}

type Namespace = BTreeMap<ArchivePath, FileVersionId>;

/// One entry left out of one snapshot.
struct Left {
    path: ArchivePath,
    version: FileVersionId,
    kind: EntryKind,
    reason: Problem,
}

/// The part of `snapshot` a repaired archive can hold: every entry whose
/// version is not `bad` and whose parent directories are all kept.
fn filter(snapshot: &Snapshot, bad: &BTreeMap<FileVersionId, Problem>) -> (Namespace, Vec<Left>) {
    let mut kept = Namespace::new();
    let mut left = Vec::new();
    let mut dropped_dirs: BTreeSet<ArchivePath> = BTreeSet::new();
    for (path, entry) in snapshot.iter() {
        let mut ancestor = path.parent();
        let mut under = None;
        while let Some(a) = ancestor {
            if dropped_dirs.contains(&a) {
                under = Some(a);
                break;
            }
            ancestor = a.parent();
        }
        let reason = if let Some(p) = bad.get(&entry.version) {
            Some(p.clone())
        } else {
            under.map(|dir| Problem {
                code: ErrorCode::NamespaceInvalid,
                message: format!(
                    "its directory {:?} is left out, and an entry is never kept without its \
                     parent (§10.2)",
                    String::from_utf8_lossy(dir.as_stored())
                ),
            })
        };
        match reason {
            Some(reason) => {
                if entry.kind == EntryKind::Directory {
                    dropped_dirs.insert(path.clone());
                }
                left.push(Left {
                    path: path.clone(),
                    version: entry.version,
                    kind: entry.kind,
                    reason,
                });
            }
            None => {
                kept.insert(path.clone(), entry.version);
            }
        }
    }
    (kept, left)
}

fn kind_name(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Directory => "directory",
        _ => "file",
    }
}

fn step(n: u8, name: &str, status: StepStatus, detail: impl Into<String>) -> LadderStep {
    LadderStep {
        step: n,
        name: name.to_owned(),
        status,
        detail: detail.into(),
    }
}

/// Steps 5–7 before steps 5 and 6 are known: 7 is never taken by this build;
/// [`survey_healed`] replaces steps 5 and 6.
fn later_steps() -> [LadderStep; 3] {
    [
        step(
            5,
            "independent copies",
            StepStatus::NotAvailable,
            "no --copy given",
        ),
        step(
            6,
            "parity",
            StepStatus::NotAvailable,
            "the archive has no recovery record (Redundancy profile, Annex B.2.11 D21)",
        ),
        step(
            7,
            "bounded salvage scan",
            StepStatus::NotAttempted,
            "not requested (--salvage asks for the bounded scan, Annex B.2.13)",
        ),
    ]
}

/// A plan that recovers nothing: no head, or no readable head commit.
fn nothing(source_len: u64, ladder: Vec<LadderStep>, head: Option<SourceHead>) -> Survey {
    let outcome = Outcome::NothingRecoverable;
    Survey {
        plan: RepairPlan {
            format: PLAN_FORMAT.to_owned(),
            archive_id: None,
            source_len,
            head,
            tail: None,
            chain_break: None,
            ladder,
            snapshots: Vec::new(),
            lost_snapshots: Vec::new(),
            omitted: Vec::new(),
            manifest_failures: Vec::new(),
            retention: None,
            parity: None,
            copies: Vec::new(),
            donations: Vec::new(),
            salvage: None,
            writer_parameters: None,
            damage_found: true,
            outcome,
            exit_code: outcome.exit_code(),
        },
        opened: Vec::new(),
        assigned: BTreeMap::new(),
        times: BTreeMap::new(),
        comments: BTreeMap::new(),
        bad: BTreeMap::new(),
        attributes: BTreeMap::new(),
        new_retention: RetentionState::default(),
        retention_ops: Vec::new(),
    }
}

/// Promised attributes from every verified manifest bound to a trusted
/// commit; a version two manifests disagree about is rejected.
struct AttributeSources {
    found: BTreeMap<FileVersionId, Attributes>,
    conflicts: BTreeSet<FileVersionId>,
}

impl AttributeSources {
    fn add(&mut self, id: FileVersionId, a: Attributes) {
        match self.found.get(&id) {
            Some(have) if *have != a => {
                self.conflicts.insert(id);
            }
            Some(_) => {}
            None => {
                self.found.insert(id, a);
            }
        }
    }
}

fn survey(
    src: &dyn ReadStorage,
    confirmed: &BTreeSet<u64>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Survey> {
    ctx.report(phase::SURVEY, 0, None);
    let source_len = src.size()?;

    // Steps 1 and 2: the head footer, else the latest valid one.
    let loc = match locate_head(src, &opts.limits) {
        Ok(l) => l,
        Err(e) => {
            let p = evidence(e)?;
            let mut ladder = vec![
                step(
                    1,
                    "expected head",
                    StepStatus::NotAvailable,
                    "no valid footer at the end of the file",
                ),
                step(
                    2,
                    "previous footers",
                    StepStatus::NotAvailable,
                    format!(
                        "the forward scan found no valid committed footer ({}); the \
                         footer-history accelerator is not written by this build",
                        p.message
                    ),
                ),
                step(
                    3,
                    "checkpoints and metadata chains",
                    StepStatus::NotAvailable,
                    "no head",
                ),
                step(4, "recovery manifests", StepStatus::NotAvailable, "no head"),
            ];
            ladder.extend(later_steps());
            return Ok(nothing(source_len, ladder, None));
        }
    };
    let found_by = match loc.source {
        HeadSource::Eof => "eof",
        HeadSource::Scan | HeadSource::Explicit => "scan",
    };
    let (step1, step2) = match loc.source {
        HeadSource::Eof => (
            step(
                1,
                "expected head",
                StepStatus::Used,
                "the footer at the end of the file",
            ),
            step(2, "previous footers", StepStatus::NotNeeded, ""),
        ),
        _ => (
            step(
                1,
                "expected head",
                StepStatus::NotAvailable,
                "the end of the file is not a valid footer",
            ),
            step(
                2,
                "previous footers",
                StepStatus::Used,
                "the latest valid footer, by forward scan (the footer-history accelerator is \
                 not written by this build)",
            ),
        ),
    };
    let tail = match &loc.tail {
        TailState::Clean => Tail {
            state: "clean".into(),
            len: 0,
            detail: None,
        },
        TailState::Uncommitted { len, .. } => Tail {
            state: "uncommitted".into(),
            len: *len,
            detail: Some(
                "eligible for truncation (D14): what an interrupted write leaves; not copied"
                    .into(),
            ),
        },
        TailState::Unresolved { len, reason } => Tail {
            state: "unresolved".into(),
            len: *len,
            detail: Some(format!(
                "{reason}; it could hold a damaged later commit, which only a salvage scan \
                 could look for"
            )),
        },
    };
    let (commit, commit_id) = match read_commit(src, &loc.footer, opts) {
        Ok(c) => c,
        Err(e) => {
            let p = evidence(e)?;
            let mut ladder = vec![
                step1,
                step2,
                step(
                    3,
                    "checkpoints and metadata chains",
                    StepStatus::NotAvailable,
                    format!("the head commit record cannot be read: {}", p.message),
                ),
                step(
                    4,
                    "recovery manifests",
                    StepStatus::NotAvailable,
                    "no head commit",
                ),
            ];
            ladder.extend(later_steps());
            let mut s = nothing(source_len, ladder, None);
            s.plan.tail = Some(tail);
            return Ok(s);
        }
    };
    let head_seq = commit.seq;
    let head = SourceHead {
        seq: head_seq,
        commit_id: hex(commit_id.as_bytes()),
        footer_offset: loc.footer.footer_offset,
        committed_len: loc.committed_len,
        found_by: found_by.into(),
    };
    let archive_id = hex(commit.archive_id.as_bytes());
    let head_entry = HistoryEntry {
        footer_offset: loc.footer.footer_offset,
        commit_offset: loc.footer.fields.commit_offset,
        commit,
        commit_id,
    };
    let (chain, broken) = walk_back_tolerant(src, head_entry, opts)?;
    let chain_start = chain.first().map_or(head_seq, |e| e.commit.seq);
    let chain_break = match broken {
        Some(e) => Some(ChainBreak {
            first_trusted_seq: chain_start,
            reason: evidence(e)?,
        }),
        None => None,
    };
    let front = Front {
        source_len,
        archive_id: Some(archive_id),
        head: Some(head),
        tail: Some(tail),
        chain_break,
        step1,
        step2,
        top_seq: head_seq,
        primary_seq: Some(head_seq),
        damaged: loc.source != HeadSource::Eof,
    };
    survey_chains(src, front, vec![chain], confirmed, None, opts, ctx)
}

/// What the front of a survey decided: where the source's head and tail are,
/// and the first two rungs of the ladder.
struct Front {
    source_len: u64,
    archive_id: Option<String>,
    head: Option<SourceHead>,
    tail: Option<Tail>,
    chain_break: Option<ChainBreak>,
    step1: LadderStep,
    step2: LadderStep,
    /// The highest sequence a snapshot can have here: sequences up to it
    /// that no catalog holds are lost.
    top_seq: u64,
    /// The trusted head's sequence: its retention state is the one carried.
    /// `None`: there is no trusted head, and retention is unresolved.
    primary_seq: Option<u64>,
    /// Damage was found before any catalog was opened.
    damaged: bool,
}

/// Steps 3 and 4 onward, over `chains` of validated commits (each ascending;
/// the first is the trusted head's chain, the rest come from the salvage
/// scan, Annex B.2.13 D23 item 6). Catalogs are opened chain by chain from
/// the top down, so a sequence the first chain supplies is never replaced by
/// a later one. With `trusted` (the sequences the trusted chain recovered
/// without salvage), any other recovered snapshot is labelled
/// [`Trust::Salvage`].
fn survey_chains(
    src: &dyn ReadStorage,
    front: Front,
    chains: Vec<Vec<HistoryEntry>>,
    confirmed: &BTreeSet<u64>,
    trusted: Option<&BTreeSet<u64>>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Survey> {
    let source_len = front.source_len;
    let mut order: Vec<&HistoryEntry> = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for chain in &chains {
        for e in chain.iter().rev() {
            if seen_ids.insert(*e.commit_id.as_bytes()) {
                order.push(e);
            }
        }
    }
    let mut by_seq: BTreeMap<u64, &HistoryEntry> = BTreeMap::new();
    for e in &order {
        by_seq.entry(e.commit.seq).or_insert(e);
    }
    let top_seq = front.top_seq;

    // Step 3 and 4: open the commits from the top down; each catalog
    // supplies every snapshot it holds that no earlier one did.
    let mut opened: Vec<OpenedHead> = Vec::new();
    let mut assigned: BTreeMap<u64, usize> = BTreeMap::new();
    let mut open_errors: BTreeMap<u64, Problem> = BTreeMap::new();
    for e in order.iter().copied() {
        let seq = e.commit.seq;
        if assigned.contains_key(&seq) {
            continue;
        }
        ctx.check_cancelled()?;
        let o = match open_at_footer(src, e.footer_offset, opts) {
            Ok(o) => o,
            Err(err) => {
                open_errors.insert(seq, evidence(err)?);
                continue;
            }
        };
        let idx = opened.len();
        let mut held = Vec::new();
        if let Err(err) = o.catalog.replay_each(|s, _| {
            held.push(s);
            Ok(())
        }) {
            open_errors.insert(seq, evidence(err)?);
            continue;
        }
        for s in held {
            assigned.entry(s).or_insert(idx);
        }
        opened.push(o);
        ctx.report(phase::SURVEY, assigned.len() as u64, Some(top_seq + 1));
    }
    let mut lost_snapshots = Vec::new();
    for seq in 0..=top_seq {
        if assigned.contains_key(&seq) {
            continue;
        }
        let commit_id = by_seq.get(&seq).map(|e| hex(e.commit_id.as_bytes()));
        let reason = match (open_errors.get(&seq), &front.chain_break) {
            (Some(p), _) => p.clone(),
            (None, Some(b)) if seq < b.first_trusted_seq => Problem {
                code: b.reason.code,
                message: format!(
                    "not linked to the head (the chain breaks below commit {}: {}), and no \
                     catalog that opened holds it",
                    b.first_trusted_seq, b.reason.message
                ),
            },
            (None, _) => Problem {
                code: ErrorCode::RecordInvalid,
                message: "no catalog that opened holds it".into(),
            },
        };
        lost_snapshots.push(LostSnapshot {
            seq,
            commit_id,
            reason,
        });
    }

    // Promised attributes, from every verified manifest of a trusted
    // commit.
    let mut attrs = AttributeSources {
        found: BTreeMap::new(),
        conflicts: BTreeSet::new(),
    };
    let mut manifest_failures = Vec::new();
    for e in order.iter().copied() {
        ctx.check_cancelled()?;
        let mut refs = vec![(ManifestKind::Delta, Ok(e.commit.delta_manifest))];
        if e.commit.metadata.is_checkpoint() {
            refs.push((ManifestKind::Snapshot, checkpoint_snapshot_ref(&e.commit)));
        }
        for (kind, r) in refs {
            let read = r
                .and_then(|r| read_bound_manifest(src, &e.commit, &r, e.commit_offset, kind, opts));
            match read {
                Ok(m) => {
                    for v in m.file_versions {
                        attrs.add(v.version.id, v.attributes);
                    }
                }
                Err(err) => manifest_failures.push(ManifestFailure {
                    seq: e.commit.seq,
                    kind: match kind {
                        ManifestKind::Delta => "delta",
                        ManifestKind::Snapshot => "snapshot",
                    }
                    .into(),
                    reason: evidence(err)?,
                }),
            }
        }
    }

    // Every version a recovered snapshot holds: the catalog that describes
    // it, and whether it can be kept.
    let mut versions: BTreeMap<FileVersionId, (usize, EntryKind)> = BTreeMap::new();
    for (idx, o) in opened.iter().enumerate() {
        o.catalog.replay_each(|seq, snapshot| {
            if assigned.get(&seq) != Some(&idx) {
                return Ok(());
            }
            for (_, entry) in snapshot.iter() {
                versions.entry(entry.version).or_insert((idx, entry.kind));
            }
            Ok(())
        })?;
    }
    let mut bad: BTreeMap<FileVersionId, Problem> = BTreeMap::new();
    let total = Some(versions.len() as u64);
    ctx.report(phase::CHECK, 0, total);
    for (n, (id, (idx, kind))) in versions.iter().enumerate() {
        ctx.check_cancelled()?;
        if attrs.conflicts.contains(id) {
            bad.insert(
                *id,
                Problem {
                    code: ErrorCode::RecordInvalid,
                    message: "verified manifests disagree about its promised attributes".into(),
                },
            );
        } else if !attrs.found.contains_key(id) {
            bad.insert(
                *id,
                Problem {
                    code: ErrorCode::RecordInvalid,
                    message: "no verified manifest of a trusted commit records its promised \
                              attributes"
                        .into(),
                },
            );
        } else if *kind != EntryKind::Directory {
            let cat = &opened[*idx].catalog;
            if let Err(e) = read_version(src, cat, id, &mut std::io::sink(), opts, ctx) {
                // Intact content this build cannot decode (a dependency such as a
                // dictionary or a key) is not damage: omitting it would be silent loss
                // dressed as damage, and `UNSUPPORTED_FEATURE` promises results are never
                // partial or empty (§7.7, §26). Refuse the run.
                if e.code == ErrorCode::UnsupportedFeature {
                    return Err(e);
                }
                bad.insert(*id, evidence(e)?);
            }
        }
        ctx.report(phase::CHECK, n as u64 + 1, total);
    }

    // What each recovered snapshot keeps.
    let mut snapshots = Vec::new();
    let mut omitted: BTreeMap<(Vec<u8>, FileVersionId), OmittedEntry> = BTreeMap::new();
    for (idx, o) in opened.iter().enumerate().rev() {
        o.catalog.replay_each(|seq, snapshot| {
            if assigned.get(&seq) != Some(&idx) {
                return Ok(());
            }
            let (kept, left) = filter(snapshot, &bad);
            snapshots.push(RecoveredSnapshot {
                seq,
                commit_id: by_seq.get(&seq).map(|e| hex(e.commit_id.as_bytes())),
                opened_at: o.seq(),
                catalog: match o.catalog_source {
                    CatalogSource::Image => CatalogKind::Image,
                    CatalogSource::SnapshotManifest { .. } => CatalogKind::SnapshotManifest,
                },
                entries: kept.len() as u64,
                omitted: left.len() as u64,
                trust: if trusted.is_some_and(|t| !t.contains(&seq)) {
                    Trust::Salvage
                } else if confirmed.contains(&seq) {
                    Trust::CopyConfirmed
                } else {
                    Trust::Chain
                },
            });
            for l in left {
                let key = (l.path.as_stored().to_vec(), l.version);
                omitted
                    .entry(key)
                    .or_insert_with(|| OmittedEntry {
                        path: String::from_utf8_lossy(l.path.as_stored()).into_owned(),
                        path_hex: hex(l.path.as_stored()),
                        version_id: hex(l.version.as_bytes()),
                        kind: kind_name(l.kind).into(),
                        snapshots: Vec::new(),
                        reason: l.reason,
                    })
                    .snapshots
                    .push(seq);
            }
            Ok(())
        })?;
    }
    snapshots.sort_by_key(|s| s.seq);
    let omitted: Vec<OmittedEntry> = omitted.into_values().collect();
    let new_seq: BTreeMap<u64, u64> = snapshots
        .iter()
        .enumerate()
        .map(|(i, s)| (s.seq, i as u64))
        .collect();

    // Retention: the head's, rebuilt from its segment (D10.10).
    let head_open = front
        .primary_seq
        .and_then(|ps| opened.first().filter(|o| o.seq() == ps));
    let retention_source = match (head_open, front.primary_seq) {
        (Some(o), _) => match resolve_retention(src, o, opts) {
            Ok(state) => Ok(state),
            Err(e) => Err(evidence(e)?),
        },
        (None, None) => Err(Problem {
            code: ErrorCode::RetentionUnresolved,
            message: "there is no trusted head, so the source's retention state cannot be \
                      rebuilt"
                .into(),
        }),
        (None, Some(ps)) => Err(Problem {
            code: open_errors
                .get(&ps)
                .map_or(ErrorCode::RetentionUnresolved, |p| p.code),
            message: format!(
                "the head commit cannot be opened, so its retention state cannot be rebuilt{}",
                open_errors
                    .get(&ps)
                    .map(|p| format!(": {}", p.message))
                    .unwrap_or_default()
            ),
        }),
    };
    let mut new_retention = RetentionState::default();
    let retention = match retention_source {
        Err(reason) => RetentionPlan::Unresolved { reason },
        Ok(state) => {
            let mut holds = Vec::new();
            for (label, s) in &state.holds {
                let n = new_seq.get(s).copied();
                if let Some(n) = n {
                    new_retention.holds.insert(label.clone(), n);
                }
                holds.push(PlannedHold {
                    label: String::from_utf8_lossy(label).into_owned(),
                    label_hex: hex(label),
                    seq: *s,
                    new_seq: n,
                });
            }
            let mut expired = Vec::new();
            for s in &state.expired {
                if let Some(n) = new_seq.get(s) {
                    new_retention.expired.insert(*n);
                    expired.push(*s);
                }
            }
            RetentionPlan::Carried { holds, expired }
        }
    };
    let mut retention_ops: Vec<RetentionOp> = new_retention
        .expired
        .iter()
        .map(|s| RetentionOp::Expire { seq: *s })
        .collect();
    retention_ops.extend(new_retention.holds.iter().map(|(l, s)| RetentionOp::Hold {
        label: l.clone(),
        seq: *s,
    }));

    let writer_parameters = opened
        .iter()
        .find_map(|o| recorded_writer_parameters(&o.catalog).transpose())
        .transpose()
        .or_else(|e| evidence(e).map(|_| None))?
        .map(|(chunk_size, zstd_level)| WriterParameters {
            chunk_size,
            zstd_level,
        });

    let from_manifest = opened
        .iter()
        .any(|o| matches!(o.catalog_source, CatalogSource::SnapshotManifest { .. }));
    let step3 = if opened.is_empty() {
        step(
            3,
            "checkpoints and metadata chains",
            StepStatus::NotAvailable,
            "no trusted commit could be opened",
        )
    } else {
        let at: Vec<String> = opened.iter().map(|o| o.seq().to_string()).collect();
        step(
            3,
            "checkpoints and metadata chains",
            StepStatus::Used,
            format!("catalogs opened at commit {}", at.join(", ")),
        )
    };
    let step4 = if from_manifest {
        step(
            4,
            "recovery manifests",
            StepStatus::Used,
            "a damaged catalog image was rebuilt from its checkpoint's snapshot manifest",
        )
    } else {
        step(4, "recovery manifests", StepStatus::NotNeeded, "")
    };
    let mut ladder = vec![front.step1.clone(), front.step2.clone(), step3, step4];
    ladder.extend(later_steps());

    let outcome = if snapshots.is_empty() {
        Outcome::NothingRecoverable
    } else if lost_snapshots.is_empty()
        && omitted.is_empty()
        && matches!(&retention, RetentionPlan::Carried { holds, .. }
            if holds.iter().all(|h| h.new_seq.is_some()))
        && front.tail.as_ref().is_some_and(|t| t.state == "clean")
    {
        Outcome::Complete
    } else {
        Outcome::Partial
    };
    let damage_found = front.damaged
        || front.tail.as_ref().is_none_or(|t| t.state != "clean")
        || front.chain_break.is_some()
        || !lost_snapshots.is_empty()
        || !omitted.is_empty()
        || !manifest_failures.is_empty()
        || from_manifest
        || matches!(retention, RetentionPlan::Unresolved { .. });

    let times = snapshots
        .iter()
        .filter_map(|s| {
            by_seq
                .get(&s.seq)
                .and_then(|e| e.commit.time)
                .map(|t| (s.seq, t))
        })
        .collect();
    let comments = snapshots
        .iter()
        .filter_map(|s| {
            by_seq
                .get(&s.seq)
                .and_then(|e| e.commit.comment.clone())
                .map(|c| (s.seq, c))
        })
        .collect();
    let attributes = attrs.found;
    Ok(Survey {
        plan: RepairPlan {
            format: PLAN_FORMAT.to_owned(),
            archive_id: front.archive_id,
            source_len,
            head: front.head,
            tail: front.tail,
            chain_break: front.chain_break,
            ladder,
            snapshots,
            lost_snapshots,
            omitted,
            manifest_failures,
            retention: (outcome != Outcome::NothingRecoverable).then_some(retention),
            parity: None,
            copies: Vec::new(),
            donations: Vec::new(),
            salvage: None,
            writer_parameters,
            damage_found,
            outcome,
            exit_code: outcome.exit_code(),
        },
        opened,
        assigned,
        times,
        comments,
        bad,
        attributes,
        new_retention,
        retention_ops,
    })
}

/// Plan a repair of the archive in `src`. Read-only; a job (progress per
/// snapshot surveyed and per version checked; cancellable). Every version
/// a recovered snapshot holds is read and verified, so this costs a full
/// read of the recoverable content.
///
/// Errors only for what compromises the run itself (I/O, limits,
/// cancellation): damage is the plan's content.
pub fn plan(src: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>) -> Result<RepairPlan> {
    plan_with_copies(src, &[], opts, ctx)
}

/// [`plan`] with independent copies (step 5, Annex B.2.12 D22): byte copies
/// of the source, each opened read-only, in the order the caller gave them.
pub fn plan_with_copies(
    src: &dyn ReadStorage,
    copies: &[CopySource<'_>],
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<RepairPlan> {
    plan_with_salvage(src, copies, None, opts, ctx)
}

/// [`plan_with_copies`] with, when `salvage` is given, the bounded salvage
/// scan (step 7, Annex B.2.13 D23) after steps 6, 5, and 1–4. The scan runs
/// only if they left something unrecovered; any plan that uses it, or whose
/// scan stopped at its limit, is partial.
///
/// Errors, besides the run itself failing: `ARCHIVE_ID_AMBIGUOUS` when the
/// scan finds commits of several archives and nothing picks one;
/// `INVALID_ARGUMENT` when `--archive-id` disagrees with the source
/// descriptor; `RECORD_INVALID` when the confirmed commits disagree about the
/// Encrypted profile.
pub fn plan_with_salvage(
    src: &dyn ReadStorage,
    copies: &[CopySource<'_>],
    salvage: Option<&SalvageOptions>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<RepairPlan> {
    Ok(survey_healed(src, copies, salvage, opts, ctx)?.0.plan)
}

fn set_step7(plan: &mut RepairPlan, status: StepStatus, detail: impl Into<String>) {
    if let Some(slot) = plan.ladder.iter_mut().find(|l| l.step == 7) {
        *slot = step(7, "bounded salvage scan", status, detail);
    }
}

/// The footer-bearing chain of validated commits whose head footer lies at
/// `footer_offset` in `src`, or `None` if no valid footer names a readable
/// commit there.
fn chain_at(
    src: &dyn ReadStorage,
    footer_offset: u64,
    opts: &ReadOptions,
) -> Result<Option<Vec<HistoryEntry>>> {
    let r = crate::storage::StorageReader::new(src)?;
    let footer = match mochi_format::footer::validate_footer(&r, footer_offset, &opts.limits) {
        Ok(f) if f.commit_kind == mochi_format::registry::FrameKind::CommitRecord => f,
        Err(mochi_format::FormatError::Source(m)) => {
            return Err(MochiError::new(ErrorCode::IoError, m))
        }
        _ => return Ok(None),
    };
    let (commit, commit_id) = match read_commit(src, &footer, opts) {
        Ok(x) => x,
        Err(e) => {
            evidence(e)?;
            return Ok(None);
        }
    };
    let entry = HistoryEntry {
        footer_offset,
        commit_offset: footer.fields.commit_offset,
        commit,
        commit_id,
    };
    Ok(Some(walk_back_tolerant(src, entry, opts)?.0))
}

/// Ladder step 7 (Annex B.2.13 D23): runs after the survey `a` over steps
/// 6, 5, and 1–4 has left something unrecovered.
///
/// [`salvage::prepare`] scans the view, decides which commits were published,
/// and lays regenerated footers (and, when its hash matches, the descriptor)
/// over it. The survey then runs again over the trusted head's chain and the
/// top of each salvaged segment, in that order, so a sequence the trusted
/// chain recovered is never replaced and every other recovered snapshot is
/// labelled [`Trust::Salvage`].
fn salvage_pass(
    view: &mut HealedSource<'_>,
    a: Survey,
    sv: &SalvageOptions,
    confirmed: &BTreeSet<u64>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Survey> {
    let needed = a.plan.head.is_none()
        || !a.plan.lost_snapshots.is_empty()
        || a.plan.chain_break.is_some()
        || a.plan.tail.as_ref().is_some_and(|t| t.state != "clean");
    if !needed {
        let mut a = a;
        set_step7(
            &mut a.plan,
            StepStatus::NotNeeded,
            "earlier steps left nothing unrecovered",
        );
        return Ok(a);
    }
    ctx.report(phase::SALVAGE, 0, None);
    let prep = salvage::prepare(view, sv, opts, ctx)?;
    let head_footer = a.plan.head.as_ref().map(|h| h.footer_offset);
    let mut roots: Vec<u64> = head_footer.into_iter().collect();
    let new_roots = prep.roots.iter().any(|r| Some(*r) != head_footer);
    for r in &prep.roots {
        if !roots.contains(r) {
            roots.push(*r);
        }
    }
    let mut chains = Vec::new();
    if new_roots || prep.footers_regenerated > 0 || prep.descriptor_restored {
        for r in &roots {
            let Some(mut c) = chain_at(&*view, *r, opts)? else {
                continue;
            };
            // A chain opened by salvage holds only commits the scan confirmed
            // (D23 items 5, 11(c)). The walk follows parent links by footer
            // alone, so it can reach a commit whose link the scan refused
            // (its footer lies inside a frame that walked validly). Below
            // the first such commit, the chain's catalogs still supply the
            // older snapshots, but no commit record is trusted. The trusted
            // head's chain is the same below its break: the ordinary walk
            // reached only the commits from the break up, and regenerated
            // footers carried it further.
            let trusted_from = if Some(*r) == head_footer {
                a.plan.chain_break.as_ref().map(|b| b.first_trusted_seq)
            } else {
                Some(u64::MAX)
            };
            if let Some(from) = trusted_from {
                let keep = c
                    .iter()
                    .rev()
                    .take_while(|e| {
                        Some(*r) == head_footer && e.commit.seq >= from
                            || prep
                                .confirmed
                                .contains(&(e.commit_offset, *e.commit_id.as_bytes()))
                    })
                    .count();
                c.drain(..c.len() - keep);
            }
            if !c.is_empty() {
                chains.push(c);
            }
        }
    }
    let trusted: BTreeSet<u64> = a.plan.snapshots.iter().map(|s| s.seq).collect();
    let mut report = prep.report;
    let mut s = if chains.is_empty() {
        a
    } else {
        let find = |n: u8| {
            a.plan
                .ladder
                .iter()
                .find(|l| l.step == n)
                .cloned()
                .unwrap_or_else(|| step(n, "", StepStatus::NotAvailable, ""))
        };
        let top_seq = chains
            .iter()
            .filter_map(|c| c.last().map(|e| e.commit.seq))
            .max()
            .unwrap_or(0)
            .max(a.plan.head.as_ref().map_or(0, |h| h.seq));
        let front = Front {
            source_len: a.plan.source_len,
            archive_id: a
                .plan
                .archive_id
                .clone()
                .or_else(|| report.archive_id.clone()),
            head: a.plan.head.clone(),
            tail: a.plan.tail.clone(),
            chain_break: a.plan.chain_break.clone(),
            step1: find(1),
            step2: find(2),
            top_seq,
            primary_seq: a.plan.head.as_ref().map(|h| h.seq),
            damaged: true,
        };
        survey_chains(&*view, front, chains, confirmed, Some(&trusted), opts, ctx)?
    };

    // Orphans: data objects no catalog that opened references (D23 item 7).
    let mut referenced = BTreeSet::new();
    for o in &s.opened {
        for id in o.catalog.object_ids()? {
            if let Some(at) = o.catalog.object_location(&id)? {
                referenced.insert(at);
            }
        }
    }
    (report.orphan_data_objects, report.orphan_bytes) =
        salvage::orphans(&prep.data_frames, &referenced);
    if let Some(inf) = report.profile_inference.as_mut() {
        inf.writer_parameters = match s.plan.writer_parameters {
            Some(p) => format!(
                "recorded in a catalog image (chunk size {}, zstd level {})",
                p.chunk_size, p.zstd_level
            ),
            None => format!(
                "no catalog image could be read: this build's defaults (chunk size {}, zstd \
                 level {})",
                crate::publish::DEFAULT_CHUNK_SIZE,
                mochi_format::codec::EncodeParams::default().level
            ),
        };
    }

    let salvaged: Vec<u64> = s
        .plan
        .snapshots
        .iter()
        .filter(|x| x.trust == Trust::Salvage)
        .map(|x| x.seq)
        .collect();
    let (status, detail) = if report.limit_reached {
        (
            StepStatus::Incomplete,
            format!(
                "stopped at the candidate limit max-salvage-candidates={} after {} \
                 candidates; what lies beyond was not examined{}",
                report.candidate_limit,
                report.candidates,
                if salvaged.is_empty() {
                    String::new()
                } else {
                    format!("; snapshots recovered so far: {salvaged:?}")
                }
            ),
        )
    } else if !salvaged.is_empty() {
        (
            StepStatus::Used,
            format!(
                "{} snapshot{} recovered from commits the scan confirmed ({salvaged:?}); {} \
                 footer{} regenerated{}; {} commit{} unconfirmed and left out (§5.1). \
                 Without a trusted head completeness cannot be shown, so the repair is partial",
                salvaged.len(),
                if salvaged.len() == 1 { "" } else { "s" },
                report.footers_regenerated.len(),
                if report.footers_regenerated.len() == 1 {
                    ""
                } else {
                    "s"
                },
                if prep.descriptor_restored {
                    "; the descriptor was re-encoded (profile inferred)"
                } else {
                    ""
                },
                report.unconfirmed_count,
                if report.unconfirmed_count == 1 {
                    " is"
                } else {
                    "s are"
                },
            ),
        )
    } else {
        (
            StepStatus::NotAvailable,
            format!(
                "the scan ({} candidates) confirmed no commit that adds to what steps 1–6 \
                 recovered; {} unconfirmed commit{} listed",
                report.candidates,
                report.unconfirmed_count,
                if report.unconfirmed_count == 1 {
                    ""
                } else {
                    "s"
                },
            ),
        )
    };
    s.plan.salvage = Some(report);
    set_step7(&mut s.plan, status, detail);
    if matches!(status, StepStatus::Used | StepStatus::Incomplete) {
        s.plan.damage_found = true;
        if s.plan.outcome == Outcome::Complete {
            s.plan.outcome = Outcome::Partial;
            s.plan.exit_code = s.plan.outcome.exit_code();
        }
    }
    Ok(s)
}

/// The ladder's step 5 from what the copies did.
fn copies_step(copies: &[CopyReport], donations: &[Donation], damage_found: bool) -> LadderStep {
    let name = "independent copies";
    if copies.is_empty() {
        return step(5, name, StepStatus::NotAvailable, "no --copy given");
    }
    if !donations.is_empty() {
        let bytes: u64 = donations.iter().map(|d| d.len).sum();
        let used: Vec<String> = copies
            .iter()
            .enumerate()
            .filter(|(_, c)| c.status == CopyStatus::Used)
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        return step(
            5,
            name,
            StepStatus::Used,
            format!(
                "{} frame{} ({bytes} bytes) from copy {}; the survey verifies them against the commit chain like any other byte",
                donations.len(),
                if donations.len() == 1 { "" } else { "s" },
                used.join(", ")
            ),
        );
    }
    let usable = copies.iter().any(|c| c.status == CopyStatus::NotNeeded);
    if usable && !damage_found {
        return step(
            5,
            name,
            StepStatus::NotNeeded,
            "earlier steps sufficed; the copies were consulted",
        );
    }
    let detail = if usable {
        "the copies were consulted and supplied nothing for the damage that remains".to_owned()
    } else {
        format!(
            "{} cop{} given; none could supply anything (see each copy's status)",
            copies.len(),
            if copies.len() == 1 { "y" } else { "ies" }
        )
    };
    step(5, name, StepStatus::NotAvailable, detail)
}

/// Step 6 first, then step 5 (module docs): rebuild what parity can, lay
/// over it what the copies supply, then survey the result, and record steps
/// 5 and 6 in the ladder.
fn survey_healed<'a>(
    src: &'a dyn ReadStorage,
    copies: &[CopySource<'_>],
    salvage: Option<&SalvageOptions>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<(Survey, HealedSource<'a>)> {
    let maybe =
        crate::parity::read_descriptor_raw(src, opts).is_none_or(|d| d.redundancy.is_some());
    let (scan, mut view) = if maybe {
        let (scan, view) = crate::parity::scan(src, opts, ctx)?;
        if scan.declared_percent.is_some() {
            (Some(scan), view)
        } else {
            (None, HealedSource::new(src))
        }
    } else {
        (None, HealedSource::new(src))
    };
    let donated = copies::donate(&mut view, copies, opts, ctx)?;
    let mut s = survey(&view, &donated.confirmed, opts, ctx)?;
    match salvage {
        Some(sv) => s = salvage_pass(&mut view, s, sv, &donated.confirmed, opts, ctx)?,
        None => {
            let unrecovered = s.plan.head.is_none()
                || !s.plan.lost_snapshots.is_empty()
                || s.plan.chain_break.is_some()
                || s.plan.tail.as_ref().is_some_and(|t| t.state != "clean");
            set_step7(
                &mut s.plan,
                StepStatus::NotAttempted,
                if unrecovered {
                    "not requested; `--salvage` may recover more (a bounded scan whose result is \
                     always partial, Annex B.2.13)"
                } else {
                    "not requested"
                },
            );
        }
    }
    let step6 = match &scan {
        None => step(
            6,
            "parity",
            StepStatus::NotAvailable,
            "the archive has no recovery record (Redundancy profile, Annex B.2.11 D21)",
        ),
        Some(sc) => {
            let beyond = sc.beyond_capacity().count();
            let rebuilt = sc.shards_rebuilt();
            let mut detail = format!(
                "commits checked: {}; damaged shards: {}, rebuilt: {rebuilt}; damaged parity \
                 frames: {}",
                sc.commits.len(),
                sc.shards_damaged(),
                sc.frames_damaged()
            );
            if beyond > 0 {
                detail.push_str(&format!(
                    "; parity groups beyond capacity: {beyond} (PARITY_CAPACITY_EXCEEDED): what \
                     their damage touches is left out"
                ));
            }
            if let Some(why) = &sc.stopped {
                detail.push_str(&format!("; {why}"));
            }
            let status = if rebuilt > 0 {
                StepStatus::Used
            } else if beyond > 0 || sc.violations().next().is_some() {
                StepStatus::NotAvailable
            } else {
                StepStatus::NotNeeded
            };
            step(6, "parity", status, detail)
        }
    };
    if let Some(slot) = s.plan.ladder.iter_mut().find(|l| l.step == 6) {
        *slot = step6;
    }
    let step5 = copies_step(&donated.copies, &donated.donations, s.plan.damage_found);
    if let Some(slot) = s.plan.ladder.iter_mut().find(|l| l.step == 5) {
        *slot = step5;
    }
    if !donated.donations.is_empty() {
        s.plan.damage_found = true;
    }
    s.plan.copies = donated.copies;
    s.plan.donations = donated.donations;
    if let Some(sc) = &scan {
        if sc.shards_damaged() > 0
            || sc.frames_damaged() > 0
            || sc.shards_unchecked() > 0
            || sc.violations().next().is_some()
        {
            s.plan.damage_found = true;
        }
    }
    s.plan.parity = scan;
    Ok((s, view))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairOptions {
    /// Argon2id parameters of the new archive's key envelopes; `None`: the
    /// defaults. Encrypted archives only.
    pub kdf: Option<mochi_format::kdf::KdfParams>,
    /// Write the repair although the source head's retention state cannot
    /// be rebuilt: the new archive then has no holds and nothing expired.
    /// Recorded in the report.
    pub accept_retention_loss: bool,
    /// The new archive's checkpoint trigger (B.2.3); `None`: the default.
    pub checkpoint_trigger: Option<CheckpointTrigger>,
    /// Test control: place the new archive's checkpoints exactly.
    #[cfg(any(test, feature = "test-controls"))]
    pub checkpoint_policy: Option<CheckpointPolicy>,
}

/// One new commit and the source snapshot it holds what was recovered of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairedCommit {
    pub source_seq: u64,
    pub source_commit_id: Option<String>,
    pub new_seq: u64,
    pub new_commit_id: String,
    pub entries: u64,
    pub omitted: u64,
    pub trust: Trust,
}

/// The independent verification of the new archive (§22.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reverification {
    pub level: String,
    pub overall_status: Status,
    pub policy_result: Status,
    pub exit_code: u8,
    pub findings: u64,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairReport {
    /// `complete` or `partial`; never anything else from a written repair.
    pub outcome: Outcome,
    pub exit_code: u8,
    pub source_archive_id: Option<String>,
    pub source_len: u64,
    pub new_archive_id: String,
    pub new_len: u64,
    pub commits: Vec<RepairedCommit>,
    pub lost_snapshots: Vec<LostSnapshot>,
    pub omitted: Vec<OmittedEntry>,
    /// The copies consulted and every frame they supplied (step 5).
    pub copies: Vec<CopyReport>,
    pub donations: Vec<Donation>,
    /// What the salvage scan found (step 7), when it ran.
    pub salvage: Option<SalvageReport>,
    /// Constraints the repaired archive does not carry (D23 item 8).
    pub profile_constraints_lost: Vec<String>,
    pub retention: Option<RetentionPlan>,
    /// The caller accepted that retention could not be carried.
    pub retention_loss_accepted: bool,
    pub chunks_copied: u64,
    pub stored_bytes_copied: u64,
    pub reverification: Reverification,
    /// `None`: the directory flush was confirmed.
    pub durability_unconfirmed: Option<String>,
    /// Always true: the source is never written or removed.
    pub source_kept: bool,
}

/// Apply `approved` to the archive in `src`, writing the new archive `name`
/// in `dir`. See the module documentation.
///
/// Refuses, writing nothing: a plan that no longer matches the source
/// (`INVALID_ARGUMENT`); a plan that recovers nothing (`NO_VALID_HEAD` when
/// there is no head, else `RECORD_INVALID`); unresolved retention without
/// the waiver (`RETENTION_UNRESOLVED`); a new archive that does not
/// reproduce the plan or fails re-verification (`CHECKPOINT_MISMATCH`).
#[allow(clippy::too_many_arguments)]
pub fn apply<D>(
    src: &dyn ReadStorage,
    approved: &RepairPlan,
    dir: &mut D,
    name: &str,
    ids: Box<dyn IdSource>,
    read: &ReadOptions,
    options: &RepairOptions,
    ctx: &JobContext<'_>,
) -> Result<RepairReport>
where
    D: StorageDir,
    D::File: Storage,
{
    apply_with_copies(src, &[], approved, dir, name, ids, read, options, ctx)
}

/// [`apply`] with independent copies. They must be the copies the plan was
/// made with, in the same order: the re-survey refuses any difference, as it
/// does for the source.
#[allow(clippy::too_many_arguments)]
pub fn apply_with_copies<D>(
    src: &dyn ReadStorage,
    copies: &[CopySource<'_>],
    approved: &RepairPlan,
    dir: &mut D,
    name: &str,
    ids: Box<dyn IdSource>,
    read: &ReadOptions,
    options: &RepairOptions,
    ctx: &JobContext<'_>,
) -> Result<RepairReport>
where
    D: StorageDir,
    D::File: Storage,
{
    apply_with_salvage(
        src, copies, None, approved, dir, name, ids, read, options, ctx,
    )
}

/// [`apply_with_copies`] with the salvage options the plan was made with
/// (step 7, Annex B.2.13 D23): the re-survey refuses any difference, as it
/// does for the source and the copies. The repaired archive is a new archive
/// with its own ordinary descriptor; when the source descriptor was
/// re-encoded, TAR compatibility is not carried over (D12 as amended).
#[allow(clippy::too_many_arguments)]
pub fn apply_with_salvage<D>(
    src: &dyn ReadStorage,
    copies: &[CopySource<'_>],
    salvage: Option<&SalvageOptions>,
    approved: &RepairPlan,
    dir: &mut D,
    name: &str,
    ids: Box<dyn IdSource>,
    read: &ReadOptions,
    options: &RepairOptions,
    ctx: &JobContext<'_>,
) -> Result<RepairReport>
where
    D: StorageDir,
    D::File: Storage,
{
    if approved.format != PLAN_FORMAT {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "the repair plan has format {:?}, but this build applies {PLAN_FORMAT:?} (the plan was made by another build, or before independent copies were taken into account): plan again. Nothing was written",
                approved.format
            ),
        ));
    }
    let (s, view) = survey_healed(src, copies, salvage, read, ctx)?;
    // Everything below reads the healed bytes (step 6).
    let src: &dyn ReadStorage = &view;
    if s.plan != *approved {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "the repair plan no longer matches the archive (it changed, or the plan was made \
             by another build or edited): plan again. Nothing was written",
        ));
    }
    let plan = s.plan;
    if plan.outcome == Outcome::NothingRecoverable {
        return Err(MochiError::new(
            if plan.head.is_none() {
                ErrorCode::NoValidHead
            } else {
                ErrorCode::RecordInvalid
            },
            "nothing is recoverable through §22 steps 1–6, or through step 7 if it was asked \
             for (--salvage); nothing was written",
        ));
    }
    if let Some(RetentionPlan::Unresolved { reason }) = &plan.retention {
        if !options.accept_retention_loss {
            return Err(MochiError::new(
                ErrorCode::RetentionUnresolved,
                format!(
                    "the source's retention state cannot be rebuilt ({}), so its legal holds \
                     cannot be carried; accept that explicitly to write a repair without any. \
                     Nothing was written",
                    reason.message
                ),
            ));
        }
    }

    let reconstructed = plan
        .salvage
        .as_ref()
        .and_then(|v| v.profile_inference.as_ref())
        .is_some_and(|i| i.descriptor_reconstructed);
    let wopts = WriterOptions {
        threads: None,
        chunk_size: plan.writer_parameters.map(|p| p.chunk_size),
        zstd_level: plan.writer_parameters.map(|p| p.zstd_level),
        record_time: false,
        dedup: Dedup::Off,
        // A rewrite keeps the source's profile (Annex B.2.9 D19 rule 9); the
        // newest opened head's descriptor is the one that was verified.
        profile: s.opened.first().map(|o| {
            let mut p = o.descriptor.profile();
            // D23 item 8: a descriptor that salvage re-encoded does not carry
            // TAR compatibility into the new archive; it is listed as lost.
            if reconstructed {
                p.tar_compatible = false;
            }
            p
        }),
        // Encrypted: the passphrases given open the new archive, which has its
        // own data key (D20 item 10).
        read: read.clone(),
        kdf: options.kdf,
        checkpoint_trigger: options.checkpoint_trigger,
    };
    let mut copier = Copier::default();
    let mut new_ids: Vec<String> = Vec::new();
    let mut reverification = None;
    let mut new_archive_id = None;
    let last = plan.snapshots.len().saturating_sub(1);
    let total = Some(plan.snapshots.len() as u64);

    let (writer, (), durability) = ArchiveWriter::build_in(dir, name, ids, wopts, ctx, |w| {
        #[cfg(any(test, feature = "test-controls"))]
        if let Some(p) = options.checkpoint_policy {
            w.set_checkpoint_policy(p)?;
        }
        // Ascending by source sequence, whichever catalog supplies each
        // snapshot (with salvage, segments interleave, D23 item 6): runs of
        // consecutive snapshots from one catalog are written in one pass.
        let mut runs: Vec<(usize, BTreeSet<u64>)> = Vec::new();
        for r in &plan.snapshots {
            let idx = *s.assigned.get(&r.seq).ok_or_else(|| {
                MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: a planned snapshot has no catalog",
                )
            })?;
            match runs.last_mut() {
                Some((i, set)) if *i == idx => {
                    set.insert(r.seq);
                }
                _ => runs.push((idx, BTreeSet::from([r.seq]))),
            }
        }
        for (idx, wanted) in &runs {
            let o = &s.opened[*idx];
            o.catalog.replay_each(|seq, snapshot| {
                if !wanted.contains(&seq) {
                    return Ok(());
                }
                ctx.check_cancelled()?;
                let i = copier.commits();
                let (kept, _) = filter(snapshot, &s.bad);
                let outcome =
                    copier.commit(w, src, &o.catalog, kept, &s.attributes, read, ctx, |tx| {
                        if i as usize == last {
                            for op in &s.retention_ops {
                                tx.push_retention(op.clone());
                            }
                        }
                        if let Some(t) = s.times.get(&seq) {
                            tx.at(*t);
                        }
                        tx.change_comment(s.comments.get(&seq).cloned().map_or(
                            crate::publish::CommentChange::Clear,
                            crate::publish::CommentChange::Set,
                        ));
                    })?;
                if outcome.seq != i {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        "internal: the new archive's sequence ran ahead of the plan",
                    ));
                }
                new_ids.push(hex(outcome.commit_id.as_bytes()));
                ctx.report(phase::COPY, copier.commits(), total);
                Ok(())
            })?;
        }

        // §22.2: verify the result independently, before publication.
        ctx.report(phase::VERIFY, 0, None);
        let out: &dyn ReadStorage = w.storage();
        let fail = |what: String| {
            MochiError::new(
                ErrorCode::CheckpointMismatch,
                format!(
                    "the repaired archive does not match its plan: {what}; nothing was published"
                ),
            )
        };
        if copier.commits() != plan.snapshots.len() as u64 {
            return Err(fail(format!(
                "{} commits for {} recovered snapshots",
                copier.commits(),
                plan.snapshots.len()
            )));
        }
        let new_head = open_head(out, read)?;
        copier.check_namespaces(&new_head, &fail)?;
        let written = all_attributes(out, read)?;
        let mut want = BTreeMap::new();
        new_head.catalog.replay_each(|_, snapshot| {
            for (_, e) in snapshot.iter() {
                let a = s
                    .attributes
                    .get(&e.version)
                    .ok_or_else(|| fail("attributes".into()))?;
                want.insert(e.version, *a);
            }
            Ok(())
        })?;
        if written != want {
            return Err(fail("promised attributes differ".into()));
        }
        if segment_state(out, &new_head, read)?.retention != s.new_retention {
            return Err(fail("the retention state differs".into()));
        }
        let v = verify(
            out,
            &VerifyOptions {
                read: read.clone(),
                deep: true,
                ..VerifyOptions::default()
            },
            ctx,
        );
        if v.report.operational_error {
            let what = v
                .report
                .findings
                .first()
                .and_then(|f| f.message.clone())
                .unwrap_or_default();
            return Err(MochiError::new(
                ErrorCode::IoError,
                format!("re-verifying the repaired archive did not complete: {what}"),
            ));
        }
        if v.report.exit_code != exit::OK {
            return Err(fail(format!(
                "re-verification reports {:?} (exit {})",
                v.report.overall_status, v.report.exit_code
            )));
        }
        reverification = Some(Reverification {
            level: format!("{:?}", v.report.level).to_lowercase(),
            overall_status: v.report.overall_status,
            policy_result: v.report.policy_result,
            exit_code: v.report.exit_code,
            findings: v.report.findings.len() as u64,
        });
        new_archive_id = Some(hex(new_head.commit.archive_id.as_bytes()));
        Ok(())
    })?;

    let new_len = writer.storage().size()?;
    drop(writer);
    let reverification = reverification.ok_or_else(|| {
        MochiError::new(ErrorCode::InvalidArgument, "internal: no re-verification")
    })?;
    let durability_unconfirmed = match durability {
        Some(PublishDurability::DirectoryUnconfirmed(why)) => Some(why),
        _ => None,
    };
    let exit_code = match plan.outcome {
        Outcome::Complete if durability_unconfirmed.is_some() => exit::DEGRADED,
        o => o.exit_code(),
    };
    Ok(RepairReport {
        outcome: plan.outcome,
        exit_code,
        source_archive_id: plan.archive_id.clone(),
        source_len: plan.source_len,
        new_archive_id: new_archive_id.unwrap_or_default(),
        new_len,
        commits: plan
            .snapshots
            .iter()
            .zip(new_ids)
            .enumerate()
            .map(|(i, (r, id))| RepairedCommit {
                source_seq: r.seq,
                source_commit_id: r.commit_id.clone(),
                new_seq: i as u64,
                new_commit_id: id,
                entries: r.entries,
                omitted: r.omitted,
                trust: r.trust,
            })
            .collect(),
        lost_snapshots: plan.lost_snapshots.clone(),
        omitted: plan.omitted.clone(),
        copies: plan.copies.clone(),
        donations: plan.donations.clone(),
        profile_constraints_lost: plan
            .salvage
            .as_ref()
            .and_then(|v| v.profile_inference.as_ref())
            .map(|i| i.profile_constraints_lost.clone())
            .unwrap_or_default(),
        salvage: plan.salvage.clone(),
        retention_loss_accepted: matches!(plan.retention, Some(RetentionPlan::Unresolved { .. })),
        retention: plan.retention,
        chunks_copied: copier.chunks_copied,
        stored_bytes_copied: copier.bytes_copied,
        reverification,
        durability_unconfirmed,
        source_kept: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::namespace::NamespaceOp;

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    fn id(n: u8) -> FileVersionId {
        FileVersionId::from_bytes([n; 32])
    }

    #[test]
    fn filter_drops_bad_versions_and_everything_under_a_dropped_directory() {
        let mut snap = Snapshot::new();
        let put = |path: &str, n: u8| NamespaceOp::Put {
            path: p(path),
            version: id(n),
        };
        let ops = vec![put("d", 1), put("d/x", 2), put("d-y", 3), put("e", 4)];
        snap.apply_commit(&ops, |v| {
            Some(if *v == id(1) {
                EntryKind::Directory
            } else {
                EntryKind::File
            })
        })
        .unwrap();
        let mut bad = BTreeMap::new();
        let why = Problem {
            code: ErrorCode::RecordInvalid,
            message: "x".into(),
        };
        bad.insert(id(1), why.clone());
        bad.insert(id(4), why);
        let (kept, left) = filter(&snap, &bad);
        // `d-y` sorts between `d` and `d/x` and is not under `d`.
        assert_eq!(kept.keys().cloned().collect::<Vec<_>>(), vec![p("d-y")]);
        let names: Vec<&[u8]> = left.iter().map(|l| l.path.as_stored()).collect();
        assert_eq!(names, vec![&b"d"[..], b"d/x", b"e"]);
        assert_eq!(left[0].reason.code, ErrorCode::RecordInvalid);
        assert_eq!(left[1].reason.code, ErrorCode::NamespaceInvalid);
        assert_eq!(left[2].reason.code, ErrorCode::RecordInvalid);
    }

    /// Two verified manifests that disagree about a version reject it;
    /// agreeing ones do not. (A writer never produces a disagreement; only
    /// a forged archive could, so this is checked here.)
    #[test]
    fn disagreeing_attribute_sources_reject_the_version() {
        let a = Attributes {
            posix: None,
            windows: None,
            mtime: None,
        };
        let b = Attributes {
            mtime: Some(Mtime { secs: 1, nanos: 0 }),
            ..a
        };
        let mut s = AttributeSources {
            found: BTreeMap::new(),
            conflicts: BTreeSet::new(),
        };
        s.add(id(1), a);
        s.add(id(1), a);
        s.add(id(2), a);
        s.add(id(2), b);
        assert_eq!(s.conflicts, BTreeSet::from([id(2)]));
        assert_eq!(s.found.get(&id(1)), Some(&a));
    }
}
