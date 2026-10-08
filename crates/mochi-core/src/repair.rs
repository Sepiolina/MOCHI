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
//! 3. **Ladder steps 1–4 only** in this slice: the expected head and its
//!    footer, earlier footers (the forward scan of [`locate_head`]; the
//!    footer-history accelerator is not written by this build), checkpoints
//!    and metadata chains (catalog images), and recovery manifests (a
//!    damaged image's snapshot manifest). Independent copies (step 5) are
//!    not taken, parity (step 6) waits for the Redundancy profile (C12), and
//!    the bounded salvage scan (step 7) is a later slice. Each is reported
//!    as such in [`RepairPlan::ladder`], never as passed.
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
use crate::error::{ErrorCode, MochiError, Result};
use crate::exit;
use crate::gc::resolve_retention;
use crate::job::JobContext;
use crate::manifest::{Attributes, ManifestKind, Mtime};
use crate::object::IdSource;
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
use crate::status::Status;
use crate::storage::{ReadStorage, Storage, StorageDir};
use crate::verify::{classify, verify, ErrorClass, VerifyOptions};

/// Progress phases, in order (then the writer's own phases for each
/// commit, `publish`, and `directory`).
pub mod phase {
    pub const SURVEY: &str = "repair-survey";
    pub const CHECK: &str = "repair-check";
    pub const COPY: &str = "repair-copy";
    pub const VERIFY: &str = "repair-verify";
}

/// The `format` of every plan this build writes.
pub const PLAN_FORMAT: &str = "mochi-repair-plan-v1";

fn hex(bytes: &[u8]) -> String {
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
    /// This build does not take this step (yet); not a pass.
    NotAttempted,
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

/// Steps 5–7: never taken by this build.
fn later_steps() -> [LadderStep; 3] {
    [
        step(
            5,
            "independent copies",
            StepStatus::NotAttempted,
            "this build takes no alternate copy",
        ),
        step(
            6,
            "parity",
            StepStatus::NotAttempted,
            "the Redundancy profile is not built (plan C12)",
        ),
        step(
            7,
            "bounded salvage scan",
            StepStatus::NotAttempted,
            "not built yet (plan C8, later slice)",
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
            writer_parameters: None,
            damage_found: true,
            outcome,
            exit_code: outcome.exit_code(),
        },
        opened: Vec::new(),
        assigned: BTreeMap::new(),
        times: BTreeMap::new(),
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

fn survey(src: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>) -> Result<Survey> {
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
    let by_seq: BTreeMap<u64, &HistoryEntry> = chain.iter().map(|e| (e.commit.seq, e)).collect();

    // Step 3 and 4: open the trusted commits from the head down; each
    // catalog supplies every snapshot it holds that no later one did.
    let mut opened: Vec<OpenedHead> = Vec::new();
    let mut assigned: BTreeMap<u64, usize> = BTreeMap::new();
    let mut open_errors: BTreeMap<u64, Problem> = BTreeMap::new();
    for e in chain.iter().rev() {
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
        ctx.report(phase::SURVEY, assigned.len() as u64, Some(head_seq + 1));
    }
    let mut lost_snapshots = Vec::new();
    for seq in 0..=head_seq {
        if assigned.contains_key(&seq) {
            continue;
        }
        let commit_id = by_seq.get(&seq).map(|e| hex(e.commit_id.as_bytes()));
        let reason = match (open_errors.get(&seq), &chain_break) {
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
    for e in &chain {
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
    let head_open = opened.first().filter(|o| o.seq() == head_seq);
    let retention_source = match head_open {
        Some(o) => match resolve_retention(src, o, opts) {
            Ok(state) => Ok(state),
            Err(e) => Err(evidence(e)?),
        },
        None => Err(Problem {
            code: open_errors
                .get(&head_seq)
                .map_or(ErrorCode::RetentionUnresolved, |p| p.code),
            message: format!(
                "the head commit cannot be opened, so its retention state cannot be rebuilt{}",
                open_errors
                    .get(&head_seq)
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
    let mut ladder = vec![step1, step2, step3, step4];
    ladder.extend(later_steps());

    let outcome = if snapshots.is_empty() {
        Outcome::NothingRecoverable
    } else if lost_snapshots.is_empty()
        && omitted.is_empty()
        && matches!(&retention, RetentionPlan::Carried { holds, .. }
            if holds.iter().all(|h| h.new_seq.is_some()))
        && tail.state == "clean"
    {
        Outcome::Complete
    } else {
        Outcome::Partial
    };
    let damage_found = loc.source != HeadSource::Eof
        || tail.state != "clean"
        || chain_break.is_some()
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
    let attributes = attrs.found;
    Ok(Survey {
        plan: RepairPlan {
            format: PLAN_FORMAT.to_owned(),
            archive_id: Some(archive_id),
            source_len,
            head: Some(head),
            tail: Some(tail),
            chain_break,
            ladder,
            snapshots,
            lost_snapshots,
            omitted,
            manifest_failures,
            retention: (outcome != Outcome::NothingRecoverable).then_some(retention),
            writer_parameters,
            damage_found,
            outcome,
            exit_code: outcome.exit_code(),
        },
        opened,
        assigned,
        times,
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
    Ok(survey(src, opts, ctx)?.plan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepairOptions {
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
    let s = survey(src, read, ctx)?;
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
            "nothing is recoverable through §22 steps 1–4 (the salvage scan is not built); \
             nothing was written",
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

    let wopts = WriterOptions {
        chunk_size: plan.writer_parameters.map(|p| p.chunk_size),
        zstd_level: plan.writer_parameters.map(|p| p.zstd_level),
        record_time: false,
        dedup: Dedup::Off,
        checkpoint_trigger: options.checkpoint_trigger,
        ..WriterOptions::default()
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
        // Catalogs were opened from the head down, so their snapshots are
        // in ascending order when taken in reverse.
        for (idx, o) in s.opened.iter().enumerate().rev() {
            o.catalog.replay_each(|seq, snapshot| {
                if s.assigned.get(&seq) != Some(&idx) {
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
                read: *read,
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
            })
            .collect(),
        lost_snapshots: plan.lost_snapshots.clone(),
        omitted: plan.omitted.clone(),
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
