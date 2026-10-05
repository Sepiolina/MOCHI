//! Damage scope (spec Annex B.2 D10.9): which stored objects of the published
//! history fail, which commits can still be read and how, each commit's
//! recoverability, and the affected sequence ranges.
//!
//! Read-only: it takes a [`ReadStorage`], writes nothing, and is a job
//! (progress per commit, cancellation). It checks *objects*: each commit's
//! delta manifest, each checkpoint's snapshot manifest and catalog image,
//! through the same code the read path uses ([`read_bound_manifest`],
//! [`check_image`], [`catalog_from_snapshot`]), so the report predicts what
//! `open_at_footer` does. It does not replay deltas: a delta that is
//! hash-valid but fails to apply is found by opening, not here. Damage to
//! commit records or footers is out of scope (review decision Q36):
//! `commit_history` fails first, and the recovery ladder (C8) handles it.
//!
//! # The rules (D10.9, review decisions Q31 to Q33)
//!
//! * A failed delta manifest *j* breaks every replay segment that contains it:
//!   the commits *j* … `end(j)` of *j*'s segment, where *j* is a delta commit.
//!   A checkpoint's own delta manifest is in no replay segment, so stored
//!   damage to it affects no read (range `NoReadAffected`); any other failure
//!   of it is an invalid record, which refuses that one commit.
//! * Image *c* with stored damage and an intact snapshot manifest: the
//!   segment reads from the snapshot (`ReadsFromSnapshot`). With any other
//!   failure, or with the snapshot unusable too: unreadable. Never an earlier
//!   checkpoint.
//! * Snapshot *c* failing with an intact image: reads continue, `DEGRADED`.
//! * Recoverability per commit: `FAIL` when unreadable; `DEGRADED` when it is
//!   readable but its base checkpoint lost one of its two representations;
//!   `PASS` otherwise. The archive's status is the worst over all commits.
//!
//! A segment's base is the last checkpoint at or before the commit (the
//! D10.6 rule); a commit record that names another base is an invalid record
//! that opening refuses, which this report does not model.

use serde::Serialize;

use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::manifest::{Manifest, ManifestKind};
use crate::publish::{
    check_image, checkpoint_snapshot_ref, commit_history, is_stored_damage, read_bound_manifest,
    HistoryEntry, ReadOptions,
};
use crate::recovery::catalog_from_snapshot;
use crate::report::{Finding, SeqRange, Severity};
use crate::status::Status;
use crate::storage::ReadStorage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole {
    DeltaManifest,
    SnapshotManifest,
    CatalogImage,
}

impl ObjectRole {
    fn label(self) -> &'static str {
        match self {
            ObjectRole::DeltaManifest => "delta manifest",
            ObjectRole::SnapshotManifest => "snapshot manifest",
            ObjectRole::CatalogImage => "catalog image",
        }
    }
}

/// One stored object that failed its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDamage {
    pub seq: u64,
    pub role: ObjectRole,
    pub offset: u64,
    pub error: MochiError,
}

/// How commit *h* can be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readability {
    /// From its base checkpoint's catalog image (the normal path).
    Image,
    /// From its base checkpoint's snapshot manifest, because the image's
    /// stored bytes are damaged.
    SnapshotRebuild,
    /// Not at all; `code` is what opening it returns.
    Unreadable { code: ErrorCode },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitScope {
    pub seq: u64,
    /// The base checkpoint of its replay segment.
    pub base_seq: u64,
    pub readable: Readability,
    pub recoverability: Status,
    /// Indices into [`DamageReport::objects`] that determine this commit's
    /// readability and status.
    pub causes: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Unreadable,
    ReadsFromSnapshot,
    Degraded,
    /// The object is damaged but no read needs it (a checkpoint's own delta
    /// manifest, stored damage).
    NoReadAffected,
}

/// The commits one damaged object affects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedRange {
    pub first: u64,
    pub last: u64,
    pub effect: Effect,
    /// Index into [`DamageReport::objects`].
    pub cause: usize,
}

#[derive(Debug)]
pub struct DamageReport {
    pub head_seq: u64,
    /// Every failed object, ascending by (sequence, role).
    pub objects: Vec<ObjectDamage>,
    /// One per commit, index = sequence, 0 ..= head.
    pub commits: Vec<CommitScope>,
    /// One per entry of `objects`, in the same order.
    pub ranges: Vec<AffectedRange>,
    pub objects_checked: u64,
}

fn rank(s: Status) -> u8 {
    match s {
        Status::Fail => 2,
        Status::Degraded => 1,
        _ => 0,
    }
}

fn worst(a: Status, b: Status) -> Status {
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

impl DamageReport {
    /// `FAIL` if any object failed; otherwise `PASS` (every object was
    /// checked: a cancelled assessment returns an error, not a report).
    pub fn integrity(&self) -> Status {
        if self.objects.is_empty() {
            Status::Pass
        } else {
            Status::Fail
        }
    }

    /// The worst recoverability over all commits (everything is retained
    /// until GC, C9, so a lost historical object is a real loss).
    pub fn recoverability(&self) -> Status {
        self.commits
            .iter()
            .fold(Status::Pass, |a, c| worst(a, c.recoverability))
    }

    /// The head commit's own recoverability, kept separate so a healthy head
    /// stays visible next to a damaged history.
    pub fn head_recoverability(&self) -> Status {
        self.commits
            .last()
            .map_or(Status::Pass, |c| c.recoverability)
    }

    /// One finding per damaged object, with its affected range (Q35).
    pub fn findings(&self) -> Vec<Finding> {
        self.objects
            .iter()
            .zip(&self.ranges)
            .map(|(o, r)| Finding {
                code: o.error.code,
                severity: Severity::Error,
                message: Some(format!(
                    "{} of commit {} (offset {}): {}",
                    o.role.label(),
                    o.seq,
                    o.offset,
                    o.error.message
                )),
                expected: None,
                observed: None,
                affected: Some(SeqRange {
                    first: r.first,
                    last: r.last,
                }),
            })
            .collect()
    }
}

fn internal(msg: impl Into<String>) -> MochiError {
    MochiError::new(
        ErrorCode::CatalogInvalid,
        format!("internal: damage assessment: {}", msg.into()),
    )
}

/// What checking one checkpoint's two representations found.
struct CheckpointChecks {
    image: Option<MochiError>,
    snapshot: Option<MochiError>,
}

/// Assess the whole published history. Read-only; a job.
pub fn assess_damage(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<DamageReport> {
    let history = commit_history(src, opts)?;
    let n = history.len();
    ctx.report("damage", 0, Some(n as u64));

    let mut objects: Vec<ObjectDamage> = Vec::new();
    // Index into `objects` per commit and role.
    let mut delta_obj: Vec<Option<usize>> = vec![None; n];
    let mut snap_obj: Vec<Option<usize>> = vec![None; n];
    let mut image_obj: Vec<Option<usize>> = vec![None; n];
    let mut checked = 0u64;

    for (i, e) in history.iter().enumerate() {
        ctx.check_cancelled()?;
        let seq = e.commit.seq;
        checked += 1;
        if let Err(error) = read_bound_manifest(
            src,
            &e.commit,
            &e.commit.delta_manifest,
            e.commit_offset,
            ManifestKind::Delta,
            opts,
        ) {
            delta_obj[i] = Some(objects.len());
            objects.push(ObjectDamage {
                seq,
                role: ObjectRole::DeltaManifest,
                offset: e.commit.delta_manifest.offset,
                error,
            });
        }
        if e.commit.metadata.is_checkpoint() {
            let checks = check_checkpoint(src, e, opts, ctx)?;
            checked += 2;
            let (image_ref, snap_ref) = refs(e)?;
            if let Some(error) = checks.snapshot {
                snap_obj[i] = Some(objects.len());
                objects.push(ObjectDamage {
                    seq,
                    role: ObjectRole::SnapshotManifest,
                    offset: snap_ref.offset,
                    error,
                });
            }
            if let Some(error) = checks.image {
                image_obj[i] = Some(objects.len());
                objects.push(ObjectDamage {
                    seq,
                    role: ObjectRole::CatalogImage,
                    offset: image_ref.offset,
                    error,
                });
            }
        }
        ctx.report("damage", (i + 1) as u64, Some(n as u64));
    }

    let is_cp: Vec<bool> = history
        .iter()
        .map(|e| e.commit.metadata.is_checkpoint())
        .collect();
    let base_of = |h: usize| (0..=h).rev().find(|&s| is_cp[s]).unwrap_or(0);
    // The last commit of the segment that starts at or contains `x`.
    let end_of = |x: usize| (x + 1..n).find(|&s| is_cp[s]).map_or(n - 1, |s| s - 1);
    let damaged = |idx: Option<usize>| idx.is_some_and(|k| is_stored_damage(&objects[k].error));

    // ---- per commit -------------------------------------------------------------
    let mut commits = Vec::with_capacity(n);
    for h in 0..n {
        let b = base_of(h);
        let mut causes = Vec::new();
        // A delta in (b, h] that failed, or the commit's own delta when it is
        // a checkpoint and the failure is not stored damage (Q31).
        let delta_blocker = (b + 1..=h)
            .find_map(|j| delta_obj[j])
            .or_else(|| delta_obj[h].filter(|&k| is_cp[h] && !is_stored_damage(&objects[k].error)));
        let (readable, status) = if let Some(k) = delta_blocker {
            causes.push(k);
            (
                Readability::Unreadable {
                    code: objects[k].error.code,
                },
                Status::Fail,
            )
        } else if image_obj[b].is_none() {
            // Image intact: reads use it. A lost snapshot manifest removes
            // the second representation (DEGRADED, D10.9).
            match snap_obj[b] {
                Some(k) => {
                    causes.push(k);
                    (Readability::Image, Status::Degraded)
                }
                None => (Readability::Image, Status::Pass),
            }
        } else if damaged(image_obj[b]) && snap_obj[b].is_none() {
            causes.extend(image_obj[b]);
            (Readability::SnapshotRebuild, Status::Degraded)
        } else {
            // Image failed and nothing can stand in for it.
            let k = image_obj[b].ok_or_else(|| internal("image failure without an object"))?;
            causes.push(k);
            causes.extend(snap_obj[b]);
            (
                Readability::Unreadable {
                    code: objects[k].error.code,
                },
                Status::Fail,
            )
        };
        commits.push(CommitScope {
            seq: h as u64,
            base_seq: b as u64,
            readable,
            recoverability: status,
            causes,
        });
    }

    // ---- ranges, one per damaged object -----------------------------------------
    let mut ranges = Vec::with_capacity(objects.len());
    for (k, o) in objects.iter().enumerate() {
        let c = o.seq as usize;
        let seg_end = end_of(c);
        let (first, last, effect) = match o.role {
            ObjectRole::DeltaManifest if !is_cp[c] => (c, seg_end, Effect::Unreadable),
            ObjectRole::DeltaManifest if is_stored_damage(&o.error) => {
                (c, c, Effect::NoReadAffected)
            }
            // A checkpoint's own delta, invalid: refuses that one commit.
            ObjectRole::DeltaManifest => (c, c, Effect::Unreadable),
            ObjectRole::CatalogImage if damaged(Some(k)) && snap_obj[c].is_none() => {
                (c, seg_end, Effect::ReadsFromSnapshot)
            }
            ObjectRole::CatalogImage => (c, seg_end, Effect::Unreadable),
            ObjectRole::SnapshotManifest if image_obj[c].is_none() => {
                (c, seg_end, Effect::Degraded)
            }
            ObjectRole::SnapshotManifest => (c, seg_end, Effect::Unreadable),
        };
        ranges.push(AffectedRange {
            first: first as u64,
            last: last as u64,
            effect,
            cause: k,
        });
    }

    // ---- self-check ------------------------------------------------------------
    for c in &commits {
        if matches!(c.readable, Readability::Unreadable { .. })
            && !ranges
                .iter()
                .any(|r| r.effect == Effect::Unreadable && (r.first..=r.last).contains(&c.seq))
        {
            return Err(internal(format!(
                "commit {} is unreadable but no unreadable range covers it",
                c.seq
            )));
        }
    }
    if ranges
        .iter()
        .any(|r| r.first > r.last || r.last as usize >= n)
    {
        return Err(internal("a range lies outside the history"));
    }

    Ok(DamageReport {
        head_seq: (n - 1) as u64,
        objects,
        commits,
        ranges,
        objects_checked: checked,
    })
}

fn refs(e: &HistoryEntry) -> Result<(crate::commit::ObjectRef, crate::commit::ObjectRef)> {
    match e.commit.metadata {
        crate::commit::Metadata::Checkpoint { image, snapshot } => Ok((image, snapshot)),
        crate::commit::Metadata::Delta { .. } => {
            Err(internal("a delta commit has no checkpoint objects"))
        }
    }
}

/// Check checkpoint `e`'s snapshot manifest and image. The snapshot is also
/// built into a catalog when the image failed with stored damage, because that
/// is exactly what the read path then does and an invalid snapshot would not
/// stand in.
fn check_checkpoint(
    src: &dyn ReadStorage,
    e: &HistoryEntry,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<CheckpointChecks> {
    let (image_ref, _) = refs(e)?;
    let snapshot: std::result::Result<Manifest, MochiError> = read_bound_manifest(
        src,
        &e.commit,
        &checkpoint_snapshot_ref(&e.commit)?,
        e.commit_offset,
        ManifestKind::Snapshot,
        opts,
    );
    ctx.check_cancelled()?;
    let image: std::result::Result<Catalog, MochiError> =
        check_image(src, e, &image_ref, opts, false);
    let mut out = CheckpointChecks {
        image: None,
        snapshot: None,
    };
    match (&snapshot, &image) {
        (Ok(s), Err(ie)) if is_stored_damage(ie) => {
            if let Err(se) = catalog_from_snapshot(s) {
                out.snapshot = Some(se);
            }
        }
        _ => {}
    }
    if let Err(se) = snapshot {
        out.snapshot = Some(se);
    }
    if let Err(ie) = image {
        out.image = Some(ie);
    }
    // T15: when both loaded, compare their authoritative state here.
    Ok(out)
}
