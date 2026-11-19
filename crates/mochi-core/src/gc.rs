//! Garbage collection, planning half (spec §18.3, §16.3; Annex B D18; plan
//! C9).
//!
//! [`plan`] is read-only. It rebuilds the retention state at the head from
//! S(*b*) and the head segment's deltas (D10.10; any missing or unverified
//! manifest is `RETENTION_UNRESOLVED`, with no fallback to an older
//! checkpoint or a partial replay), takes the retained roots (the head,
//! every commit not expired, every held commit), and marks transitively:
//! each root's namespace, every file version it reaches, every chunk those
//! versions' extents reach. What no root reaches is collectable, and every
//! collectable snapshot carries its reason.
//!
//! Nothing is deleted here or anywhere in place. A single-file archive is
//! append-only, so collection means writing a new archive without the
//! collectable content (`compact`, owner decision 2026-10-07), and the source
//! file is never removed automatically: it is the quarantine copy (§18.3)
//! and the retained previous representation (§18.2 step 6).
//!
//! §18.3 also lists replication pins, active restoration operations, and
//! unexpired prepared transactions as roots. 1.0 has none of them: no
//! Preservation profile, and a single writer that publishes or rolls back
//! within one commit. Concurrent publication is handled when a plan is
//! applied: the plan records the head it was made at, and applying it under
//! the archive's writer lock refuses any other head.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::catalog::extent::ExtentSource;
use crate::catalog::namespace::FileVersionId;
use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::object::ObjectId;
use crate::publish::{commit_history, open_head, segment_state, OpenedHead, ReadOptions};
use crate::retention::RetentionState;
use crate::storage::ReadStorage;

/// Progress phases reported by [`plan`].
pub mod phase {
    pub const RETENTION: &str = "retention";
    pub const MARK: &str = "mark";
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The head a plan was made at. Applying the plan refuses any other.
/// Deserializable so a client can read it back from a saved plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHead {
    pub seq: u64,
    pub commit_id: String,
    pub footer_offset: u64,
    pub committed_len: u64,
}

/// One active legal hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hold {
    /// The label as stored, lossily decoded for display.
    pub label: String,
    pub label_hex: String,
    pub seq: u64,
}

/// A snapshot no root protects, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CollectableSnapshot {
    pub seq: u64,
    pub commit_id: String,
    /// Always "expired; not the head; no active hold" in 1.0, the only way
    /// a snapshot stops being a root.
    pub reason: &'static str,
}

/// File versions, chunks, and the chunks' stored bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub file_versions: u64,
    pub chunks: u64,
    pub stored_bytes: u64,
}

/// A collection plan: what is kept, what is not, and why (§18.3: every
/// deletion batch has an auditable explanation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GcPlan {
    pub archive_id: String,
    pub head: PlanHead,
    pub expired: Vec<u64>,
    pub holds: Vec<Hold>,
    /// Retained roots, ascending.
    pub roots: Vec<u64>,
    pub collectable_snapshots: Vec<CollectableSnapshot>,
    /// What the roots' dependency closure holds.
    pub retained: Totals,
    /// What it does not: written to no new representation.
    pub collectable: Totals,
    /// TAR-compatible archives only: chunks no version's extent references,
    /// the framing and re-emitted content of the commits' streams (Annex
    /// B.2.9 D19 rule 7). They belong to the streams, not to a file version,
    /// so they are never collectable: a rewrite regenerates them.
    pub stream_framing: Totals,
    /// Collectable chunk IDs, sorted (hex).
    pub collectable_chunks: Vec<String>,
    /// Collectable file-version IDs, sorted (hex).
    pub collectable_versions: Vec<String>,
}

impl GcPlan {
    /// Whether applying the plan would leave anything out.
    pub fn collects_anything(&self) -> bool {
        !self.collectable_snapshots.is_empty()
            || self.collectable.chunks > 0
            || self.collectable.file_versions > 0
    }
}

/// The dependency closure of a set of roots.
#[derive(Debug, Default)]
pub(crate) struct Marked {
    pub versions: BTreeSet<FileVersionId>,
    pub chunks: BTreeSet<ObjectId>,
}

/// Retention state at `head`, rebuilt as D10.10 requires. Any failure but
/// an I/O error or cancellation is `RETENTION_UNRESOLVED`.
pub(crate) fn resolve_retention(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    opts: &ReadOptions,
) -> Result<RetentionState> {
    segment_state(src, head, opts)
        .map(|s| s.retention)
        .map_err(|e| match e.code {
            ErrorCode::IoError | ErrorCode::Cancelled => e,
            _ => MochiError::new(
                ErrorCode::RetentionUnresolved,
                format!(
                    "the retention state at commit {} cannot be rebuilt from its segment's \
                     manifests ({}: {}); nothing can be collected (D10.10)",
                    head.commit.seq, e.code, e.message
                ),
            ),
        })
}

/// Mark the dependency closure of `roots` in one replay of the history.
/// A chunk with dependencies (dictionaries, key envelopes) is refused: 1.0
/// writes none, and marking them is not implemented.
pub(crate) fn mark(
    catalog: &Catalog,
    roots: &BTreeSet<u64>,
    ctx: &JobContext<'_>,
) -> Result<Marked> {
    let mut marked = Marked::default();
    catalog.replay_each(|seq, snapshot| {
        if !roots.contains(&seq) {
            return Ok(());
        }
        ctx.check_cancelled()?;
        for (_, entry) in snapshot.iter() {
            if !marked.versions.insert(entry.version) {
                continue;
            }
            let (_, extents) = catalog.file_version(&entry.version)?.ok_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "a snapshot names a version the catalog does not hold",
                )
            })?;
            for e in extents {
                if let ExtentSource::Chunk { chunk, .. } = e.source {
                    marked.chunks.insert(chunk);
                }
            }
        }
        ctx.report(phase::MARK, seq, None);
        Ok(())
    })?;
    for id in &marked.chunks {
        let record = catalog.object(id)?.ok_or_else(|| {
            MochiError::new(
                ErrorCode::CatalogInvalid,
                "an extent names a chunk the catalog does not hold",
            )
        })?;
        if !record.dependencies.is_empty() {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                format!(
                    "chunk {} has dependencies; collecting archives with dictionaries or \
                     key envelopes is not implemented",
                    id.to_hex()
                ),
            ));
        }
    }
    Ok(marked)
}

/// Plan a collection of the archive in `src` at its head. Read-only.
pub fn plan(src: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>) -> Result<GcPlan> {
    let head = open_head(src, opts)?;
    ctx.report(phase::RETENTION, 0, None);
    let retention = resolve_retention(src, &head, opts)?;
    let head_seq = head.commit.seq;
    let roots = retention.roots(head_seq);
    let marked = mark(&head.catalog, &roots, ctx)?;

    let ids: BTreeMap<u64, String> = commit_history(src, opts)?
        .into_iter()
        .map(|h| (h.commit.seq, hex(h.commit_id.as_bytes())))
        .collect();
    let collectable_snapshots = (0..=head_seq)
        .filter(|s| !roots.contains(s))
        .map(|seq| {
            Ok(CollectableSnapshot {
                seq,
                commit_id: ids.get(&seq).cloned().ok_or_else(|| {
                    MochiError::new(
                        ErrorCode::RecordInvalid,
                        format!("commit {seq} is missing from the history"),
                    )
                })?,
                reason: "expired; not the head; no active hold",
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // In a TAR-compatible archive a chunk that no version at all references
    // is stream framing; every other unmarked chunk belongs to a version
    // that is not retained.
    let mut referenced = BTreeSet::new();
    if head.descriptor.tar_compatible {
        for id in head.catalog.file_version_ids()? {
            ctx.check_cancelled()?;
            if let Some((_, extents)) = head.catalog.file_version(&id)? {
                referenced.extend(extents.iter().filter_map(|e| match e.source {
                    ExtentSource::Chunk { chunk, .. } => Some(chunk),
                    _ => None,
                }));
            }
        }
    }
    let mut retained = Totals::default();
    let mut collectable = Totals::default();
    let mut stream_framing = Totals::default();
    let mut collectable_chunks = Vec::new();
    for id in head.catalog.object_ids()? {
        let Some(record) = head.catalog.object(&id)? else {
            continue; // not a chunk (no other object kind is catalogued yet)
        };
        let t = if marked.chunks.contains(&id) {
            &mut retained
        } else if head.descriptor.tar_compatible && !referenced.contains(&id) {
            &mut stream_framing
        } else {
            collectable_chunks.push(id.to_hex());
            &mut collectable
        };
        t.chunks += 1;
        t.stored_bytes = t.stored_bytes.saturating_add(record.stored_len);
    }
    let mut collectable_versions = Vec::new();
    for id in head.catalog.file_version_ids()? {
        if marked.versions.contains(&id) {
            retained.file_versions += 1;
        } else {
            collectable.file_versions += 1;
            collectable_versions.push(hex(id.as_bytes()));
        }
    }

    Ok(GcPlan {
        archive_id: hex(head.commit.archive_id.as_bytes()),
        head: PlanHead {
            seq: head_seq,
            commit_id: hex(head.commit_id.as_bytes()),
            footer_offset: head.location.footer.footer_offset,
            committed_len: head.location.committed_len,
        },
        expired: retention.expired.iter().copied().collect(),
        holds: retention
            .holds
            .iter()
            .map(|(l, s)| Hold {
                label: String::from_utf8_lossy(l).into_owned(),
                label_hex: hex(l),
                seq: *s,
            })
            .collect(),
        roots: roots.into_iter().collect(),
        collectable_snapshots,
        retained,
        collectable,
        stream_framing,
        collectable_chunks,
        collectable_versions,
    })
}
