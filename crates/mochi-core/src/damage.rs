//! Damage scope (spec Annex B.2 D10.9): which stored objects of the published
//! history fail, which commits can still be read and how, each commit's
//! recoverability, and the affected sequence ranges.
//!
//! Read-only: it takes a [`ReadStorage`], writes nothing, and is a job
//! (progress per commit, cancellation). It checks *objects*: the archive
//! descriptor each commit references (once per distinct reference), each
//! commit's delta manifest, and each checkpoint's snapshot manifest and
//! catalog image, through the same code the read path uses
//! ([`read_descriptor`], [`read_bound_manifest`], [`check_image`],
//! [`catalog_from_snapshot`]), so the report predicts what
//! `open_at_footer` does. It does not replay deltas: a delta that is
//! hash-valid but fails to apply is found by opening, not here. Damage to
//! commit records or footers is out of scope (review decision Q36):
//! `commit_history` fails first, and the recovery ladder (C8) handles it.
//!
//! # The rules (D10.9, D12, review decisions Q31 to Q33)
//!
//! * A descriptor that is missing, damaged, or mismatched (D12,
//!   `DESCRIPTOR_INVALID`) refuses interpretation of every commit that
//!   references it, and, through the one-descriptor-per-segment rule
//!   (D10.6), of every later commit of a segment that contains such a
//!   commit. Those commits are unreadable (`FAIL`); there is no fallback.
//!   One object is reported per run of consecutive commits it makes
//!   unreadable, so an archive whose only descriptor is damaged has one
//!   finding covering 0 … head. A descriptor this build refuses
//!   (`UNSUPPORTED_FEATURE`) is not damage: the assessment itself is
//!   refused, never reported as `PASS`.
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
//! * Image and snapshot both load but disagree (D10.7, `CHECKPOINT_MISMATCH`):
//!   reads use the image, and the redundancy D10.2 relies on is gone, so
//!   `DEGRADED` (review decision Q33).
//! * Recoverability per commit: `FAIL` when unreadable; `DEGRADED` when it is
//!   readable but its base checkpoint lost one of its two representations;
//!   `PASS` otherwise. The archive's status is the worst over all commits.
//!
//! A segment's base is the last checkpoint at or before the commit (the
//! D10.6 rule); a commit record that names another base is an invalid record
//! that opening refuses, which this report does not model.

use std::collections::btree_map::{BTreeMap, Entry};

use serde::Serialize;

use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::manifest::{Manifest, ManifestKind};
use crate::object::ArchiveId;
use crate::publish::{
    check_image, checkpoint_snapshot_ref, commit_history, compare_representations,
    is_stored_damage, read_bound_manifest, read_descriptor, HistoryEntry, ReadOptions,
};
use crate::recovery::catalog_from_snapshot;
use crate::report::{Finding, SeqRange, Severity};
use crate::status::Status;
use crate::storage::ReadStorage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole {
    /// The archive descriptor (D12). `seq` is the first commit of the run
    /// of commits it makes unreadable.
    Descriptor,
    DeltaManifest,
    SnapshotManifest,
    CatalogImage,
    /// Both representations load, and they disagree (D10.7,
    /// `CHECKPOINT_MISMATCH`).
    CheckpointPair,
}

impl ObjectRole {
    fn label(self) -> &'static str {
        match self {
            ObjectRole::Descriptor => "archive descriptor",
            ObjectRole::DeltaManifest => "delta manifest",
            ObjectRole::SnapshotManifest => "snapshot manifest",
            ObjectRole::CatalogImage => "catalog image",
            ObjectRole::CheckpointPair => "checkpoint representations",
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

/// One descriptor check: the reference and the archive ID the commit
/// claims, which [`read_descriptor`] checks against the descriptor. The
/// commit-frame bound it also applies is left out: the one valid descriptor
/// ends before commit 0's frame, so the bound never decides validity.
type DescriptorKey = (u64, u64, [u8; 32], ArchiveId);

fn descriptor_key(e: &HistoryEntry) -> DescriptorKey {
    let r = e.commit.descriptor;
    (
        r.offset,
        r.stored_len,
        *r.stored_hash.as_bytes(),
        e.commit.archive_id,
    )
}

/// Why commit *h* cannot interpret its descriptor, if it cannot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DescriptorCause {
    /// Its own descriptor check failed.
    Own(DescriptorKey),
    /// An earlier commit of its segment references a different descriptor
    /// (D10.6), whose check failed.
    Segment(DescriptorKey),
}

impl DescriptorCause {
    fn key(self) -> DescriptorKey {
        match self {
            DescriptorCause::Own(k) | DescriptorCause::Segment(k) => k,
        }
    }
}

/// What checking one checkpoint's two representations found.
struct CheckpointChecks {
    image: Option<MochiError>,
    snapshot: Option<MochiError>,
    pair: Option<MochiError>,
}

/// Assess the whole published history. Read-only; a job.
pub fn assess_damage(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<DamageReport> {
    let history = commit_history(src, opts)?;
    let n = history.len();
    // Every index below is a commit sequence: `commit_history` returns
    // 0 ..= head in order. Checked, not assumed (no panic on archive input).
    if n == 0
        || history
            .iter()
            .enumerate()
            .any(|(i, e)| e.commit.seq != i as u64)
    {
        return Err(internal("the history is not commits 0 ..= head in order"));
    }
    ctx.report("damage", 0, Some(n as u64));

    let is_cp: Vec<bool> = history
        .iter()
        .map(|e| e.commit.metadata.is_checkpoint())
        .collect();
    let base_of = |h: usize| (0..=h).rev().find(|&s| is_cp[s]).unwrap_or(0);
    // The last commit of the segment that starts at or contains `x`.
    let end_of = |x: usize| (x + 1..n).find(|&s| is_cp[s]).map_or(n - 1, |s| s - 1);
    let mut checked = 0u64;

    // ---- D12: descriptors, each distinct reference once -----------------------
    let mut descriptors: BTreeMap<DescriptorKey, Option<MochiError>> = BTreeMap::new();
    let mut own_failed: Vec<bool> = vec![false; n];
    for (i, e) in history.iter().enumerate() {
        ctx.check_cancelled()?;
        let result = match descriptors.entry(descriptor_key(e)) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(v) => {
                checked += 1;
                v.insert(
                    match read_descriptor(src, &e.commit, e.commit_offset, opts) {
                        Ok(_) => None,
                        Err(err) if err.code == ErrorCode::DescriptorInvalid => Some(err),
                        // A refusal (this build will not interpret the
                        // archive), a reader limit, or I/O: not evidence of
                        // damage, and nothing after it could be assessed
                        // honestly.
                        Err(err) => return Err(err),
                    },
                )
            }
        };
        own_failed[i] = result.is_some();
    }
    let mut desc_cause: Vec<Option<DescriptorCause>> = Vec::with_capacity(n);
    for h in 0..n {
        if own_failed[h] {
            desc_cause.push(Some(DescriptorCause::Own(descriptor_key(&history[h]))));
            continue;
        }
        // D10.6 compares references across the segment. Commit h's own
        // reference is valid, and only one reference can be (one frame at
        // offset 0), so any differing one in the segment failed its check.
        let r_h = history[h].commit.descriptor;
        match (base_of(h)..h).find(|&j| history[j].commit.descriptor != r_h) {
            None => desc_cause.push(None),
            Some(j) if own_failed[j] => {
                desc_cause.push(Some(DescriptorCause::Segment(descriptor_key(&history[j]))))
            }
            Some(j) => {
                return Err(internal(format!(
                    "commits {j} and {h} reference different descriptors and both are valid"
                )))
            }
        }
    }

    let mut objects: Vec<ObjectDamage> = Vec::new();
    // Index into `objects` per commit and role.
    let mut desc_obj: Vec<Option<usize>> = vec![None; n];
    let mut delta_obj: Vec<Option<usize>> = vec![None; n];
    let mut snap_obj: Vec<Option<usize>> = vec![None; n];
    let mut image_obj: Vec<Option<usize>> = vec![None; n];
    let mut pair_obj: Vec<Option<usize>> = vec![None; n];

    for (i, e) in history.iter().enumerate() {
        ctx.check_cancelled()?;
        let seq = e.commit.seq;
        // One descriptor object per run of consecutive commits with the same
        // failing descriptor, opened at the run's first commit.
        if let Some(cause) = desc_cause[i] {
            let key = cause.key();
            let continues = i > 0 && desc_cause[i - 1].map(DescriptorCause::key) == Some(key);
            desc_obj[i] = if continues {
                desc_obj[i - 1]
            } else {
                let error = descriptors
                    .get(&key)
                    .and_then(|r| r.clone())
                    .ok_or_else(|| internal("a failed descriptor without its error"))?;
                objects.push(ObjectDamage {
                    seq,
                    role: ObjectRole::Descriptor,
                    offset: key.0,
                    error,
                });
                Some(objects.len() - 1)
            };
        }
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
            if let Some(error) = checks.pair {
                pair_obj[i] = Some(objects.len());
                objects.push(ObjectDamage {
                    seq,
                    role: ObjectRole::CheckpointPair,
                    offset: image_ref.offset,
                    error,
                });
            }
        }
        ctx.report("damage", (i + 1) as u64, Some(n as u64));
    }

    let damaged = |idx: Option<usize>| idx.is_some_and(|k| is_stored_damage(&objects[k].error));

    // ---- per commit -------------------------------------------------------------
    let mut commits = Vec::with_capacity(n);
    for h in 0..n {
        let b = base_of(h);
        let mut causes = Vec::new();
        // In the order opening meets them: the commit's own descriptor; its
        // own delta manifest (for a checkpoint, only a failure that is not
        // stored damage, Q31); the segment's descriptors (D10.6); then any
        // other delta in (b, h).
        let own_desc =
            desc_obj[h].filter(|_| matches!(desc_cause[h], Some(DescriptorCause::Own(_))));
        let seg_desc =
            desc_obj[h].filter(|_| matches!(desc_cause[h], Some(DescriptorCause::Segment(_))));
        let own_delta = delta_obj[h].filter(|&k| !is_cp[h] || !is_stored_damage(&objects[k].error));
        let blocker = own_desc
            .or(own_delta)
            .or(seg_desc)
            .or_else(|| (b + 1..=h).find_map(|j| delta_obj[j]));
        let (readable, status) = if let Some(k) = blocker {
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
            match snap_obj[b].or(pair_obj[b]) {
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
            // The run of commits this descriptor object was opened for.
            ObjectRole::Descriptor => {
                let last = (c..n)
                    .take_while(|&h| desc_obj[h] == Some(k))
                    .last()
                    .ok_or_else(|| internal("a descriptor object with no commit"))?;
                (c, last, Effect::Unreadable)
            }
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
            // Both representations load, so reads work; the pair is
            // inconsistent.
            ObjectRole::CheckpointPair => (c, seg_end, Effect::Degraded),
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
        pair: None,
    };
    match (&snapshot, &image) {
        (Ok(s), Err(ie)) if is_stored_damage(ie) => {
            if let Err(se) = catalog_from_snapshot(s) {
                out.snapshot = Some(se);
            }
        }
        _ => {}
    }
    if let (Ok(s), Ok(i)) = (&snapshot, &image) {
        if let Err(pe) = compare_representations(s, i, e.commit.seq) {
            out.pair = Some(pe);
        }
    }
    if let Err(se) = snapshot {
        out.snapshot = Some(se);
    }
    if let Err(ie) = image {
        out.image = Some(ie);
    }
    // D10.7: when both loaded, they must agree (verify repeats adoption).
    Ok(out)
}
