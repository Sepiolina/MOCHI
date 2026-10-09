//! Compaction and collection by rewriting (spec §18.2, §18.3; Annex B D18;
//! plan C9).
//!
//! [`compact`] writes the snapshots it keeps into a **new archive** (owner
//! decisions, 2026-10-07):
//!
//! - **New archive, with provenance.** A new archive ID; one commit per
//!   kept snapshot, in order, each reproducing that snapshot's namespace
//!   exactly. Delta(0) records where it came from: the source archive ID,
//!   the source (sequence, commit ID) behind every new commit, and the
//!   snapshots left out (recovery-manifest schema 2, key 12). Commit IDs
//!   bind footer offsets, so they cannot survive a rewrite; freshness
//!   anchors start over, and first sight of the new archive is `UNKNOWN`.
//! - **Identity preserved.** Object and file-version IDs are kept (O19:
//!   identity is stable across compaction), stored chunks are copied byte
//!   for byte (so stored hashes are too), and every version keeps its
//!   promised attributes. Retention carries over: every hold, on the new
//!   commit of the snapshot it holds, and every kept snapshot's expiry.
//! - **The source is never written or removed.** It stays the retained
//!   previous representation (§18.2 step 6) and, for collection, the
//!   quarantine (§18.3). Removing it is a separate, explicit user action.
//!
//! Which snapshots are kept ([`Keep`]): all of them (`compact`), or the
//! retained roots of a [`crate::gc::GcPlan`] made at the current head (`gc
//! apply`). The caller passes the source as an open [`ArchiveWriter`], so
//! the source's publication lock is held throughout and no commit can land
//! while it is read (§18.3: concurrent publication); a plan made at another
//! head is refused.
//!
//! The §18.2 steps, in order, all before publication (the new archive is
//! built in a temporary file by [`ArchiveWriter::build_in`]; any failure or
//! crash before publication leaves nothing at the destination name):
//! 1. the new archive opens, and has one commit per kept snapshot;
//! 2. each commit's namespace equals its source snapshot's; every version's
//!    promised attributes, the retention state, and the provenance read back
//!    as written; every checkpoint was adopted (D10.7) as it was written;
//! 3. with [`CompactOptions::verify_content`] (the default), every file
//!    version is read back and verified (stored and content integrity, file
//!    hash);
//! 4. recovery copies: none are required in 1.0 (no Redundancy or
//!    Preservation profile in a compaction source; reported);
//! 5. publication without replacing anything (D13);
//! 6. the source is kept.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::catalog::extent::ExtentSource;
use crate::catalog::namespace::{FileVersionId, Snapshot};
use crate::catalog::path::ArchivePath;
use crate::catalog::Catalog;
use crate::commit::Metadata;
use crate::error::{ErrorCode, MochiError, Result};
use crate::gc::{mark, resolve_retention, PlanHead};
use crate::job::JobContext;
use crate::manifest::{Attributes, FileVersionEntry, ManifestKind, Provenance};
use crate::object::{load_stored, unseal_object, verify_stored, IdSource, ObjectId, ObjectRecord};
#[cfg(any(test, feature = "test-controls"))]
use crate::publish::CheckpointPolicy;
use crate::publish::{
    commit_history, open_head, read_bound_manifest, segment_state, ArchiveWriter,
    CheckpointTrigger, CommitOutcome, Dedup, OpenedHead, PublishDurability, ReadOptions,
    Transaction, WriterOptions,
};
use crate::read::read_version;
use crate::retention::{RetentionOp, RetentionState};
use crate::segment::check_delta_parent_link;
use crate::storage::{ReadStorage, Storage, StorageDir};
use mochi_format::codec::Protection;

/// Progress phases reported by [`compact`], in order (then the writer's own
/// phases for each commit, `publish`, and `directory`).
pub mod phase {
    pub const PREPARE: &str = "compact-prepare";
    pub const COPY: &str = "compact-copy";
    pub const VERIFY: &str = "compact-verify";
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Which source snapshots the new archive keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    /// Every snapshot (`compact`).
    Every,
    /// The retained roots at the head a GC plan was made at (`gc apply`).
    /// Any other head is refused: the plan is stale.
    Roots(PlanHead),
}

#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// How the source is read. For an Encrypted archive this carries the
    /// passphrases: they open the source, and the new archive (which has its
    /// own data key, D20 item 10) gets one key envelope for each of them.
    /// Without one the rewrite fails with `KEY_UNAVAILABLE`, plan included.
    pub read: ReadOptions,
    /// The passphrases the **new** archive is wrapped under, when they differ
    /// from the source's (`rekey --reencrypt`); `None`: those of `read`.
    pub new_keys: Option<std::sync::Arc<crate::keys::KeySession>>,
    /// Argon2id parameters of the new archive's envelopes; `None`: the
    /// defaults. Encrypted archives only.
    pub kdf: Option<mochi_format::kdf::KdfParams>,
    /// Why the new archive exists, recorded in its delta(0) provenance:
    /// collection (the default) or, for `rekey --reencrypt`, re-encryption
    /// (Encrypted profile only).
    pub reason: crate::manifest::RewriteReason,
    /// §18.2 step 3: read back and verify every file version before
    /// publishing. On by default.
    pub verify_content: bool,
    /// The new archive's checkpoint trigger (B.2.3); `None`: the default.
    pub checkpoint_trigger: Option<CheckpointTrigger>,
    /// Test control: place the new archive's checkpoints exactly.
    #[cfg(any(test, feature = "test-controls"))]
    pub checkpoint_policy: Option<CheckpointPolicy>,
}

impl Default for CompactOptions {
    fn default() -> Self {
        CompactOptions {
            read: ReadOptions::default(),
            new_keys: None,
            kdf: None,
            reason: crate::manifest::RewriteReason::Collection,
            verify_content: true,
            checkpoint_trigger: None,
            #[cfg(any(test, feature = "test-controls"))]
            checkpoint_policy: None,
        }
    }
}

/// One new commit and the source snapshot it reproduces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitMapping {
    pub source_seq: u64,
    pub source_commit_id: String,
    pub new_seq: u64,
    pub new_commit_id: String,
}

/// What [`compact`] did. The source is always kept (`source_kept`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactReport {
    pub source_archive_id: String,
    pub source_head: PlanHead,
    pub source_len: u64,
    pub new_archive_id: String,
    pub new_len: u64,
    pub commits: Vec<CommitMapping>,
    /// Source snapshots left out (no retained root protected them).
    pub collected: Vec<u64>,
    pub chunks_copied: u64,
    pub stored_bytes_copied: u64,
    pub file_versions: u64,
    /// File versions read back and verified (§18.2 step 3), when asked.
    pub versions_verified: Option<u64>,
    /// §18.2 step 4 in 1.0.
    pub recovery_copies: &'static str,
    /// `None`: the directory flush was confirmed.
    pub durability_unconfirmed: Option<String>,
    /// Always true: the source is never written or removed.
    pub source_kept: bool,
}

type Namespace = BTreeMap<ArchivePath, FileVersionId>;

pub(crate) fn namespace(s: &Snapshot) -> Namespace {
    s.iter().map(|(p, e)| (p.clone(), e.version)).collect()
}

/// Writes snapshots into a new archive, one commit each, preserving
/// version identity and copying stored chunks byte for byte (shared by
/// compaction and repair, C8). It remembers what the new archive holds so
/// each chunk and version is copied once, and what a baseline replay of the
/// new archive's current segment sees (checklist Q64): a reference outside
/// that forces a checkpoint.
#[derive(Default)]
pub(crate) struct Copier {
    prev: Namespace,
    /// Each commit's namespace as the diff from the one before, for
    /// [`Copier::check_namespaces`].
    diffs: Vec<(Vec<ArchivePath>, Namespace)>,
    copied_versions: BTreeSet<FileVersionId>,
    copied_chunks: BTreeSet<ObjectId>,
    visible_versions: BTreeSet<FileVersionId>,
    visible_chunks: BTreeSet<ObjectId>,
    pub(crate) chunks_copied: u64,
    pub(crate) bytes_copied: u64,
}

impl Copier {
    /// Commits written so far.
    pub(crate) fn commits(&self) -> u64 {
        self.diffs.len() as u64
    }

    /// Commit `next` (every version in it described by `cat`, its chunks
    /// stored in `src`) as the new archive's next snapshot. `extra` adds
    /// anything else the commit carries (time, retention, provenance).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn commit<S: Storage>(
        &mut self,
        w: &mut ArchiveWriter<S>,
        src: &dyn ReadStorage,
        cat: &Catalog,
        next: Namespace,
        attributes: &BTreeMap<FileVersionId, Attributes>,
        opts: &ReadOptions,
        ctx: &JobContext<'_>,
        extra: impl FnOnce(&mut Transaction),
    ) -> Result<CommitOutcome> {
        let deleted: Vec<ArchivePath> = self
            .prev
            .keys()
            .filter(|p| !next.contains_key(*p))
            .cloned()
            .collect();
        let put: Namespace = next
            .iter()
            .filter(|(p, v)| self.prev.get(*p) != Some(*v))
            .map(|(p, v)| (p.clone(), *v))
            .collect();

        let mut tx = Transaction::new();
        let mut force_checkpoint = false;
        for p in &deleted {
            tx.delete(p.clone());
        }
        for (p, id) in &put {
            let (version, extents) = cat.file_version(id)?.ok_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "a snapshot names a version the catalog does not hold",
                )
            })?;
            let attrs = *attributes.get(id).ok_or_else(|| {
                MochiError::new(
                    ErrorCode::RecordInvalid,
                    format!("no delta manifest introduces version {id:?}"),
                )
            })?;
            let mut stored = Vec::new();
            if self.copied_versions.contains(id) {
                force_checkpoint |= !self.visible_versions.contains(id);
            } else {
                for e in &extents {
                    let ExtentSource::Chunk { chunk, .. } = e.source else {
                        continue;
                    };
                    if self.copied_chunks.contains(&chunk) {
                        force_checkpoint |= !self.visible_chunks.contains(&chunk);
                        continue;
                    }
                    if stored
                        .iter()
                        .any(|(r, _): &(ObjectRecord, _)| r.id == chunk)
                    {
                        continue;
                    }
                    let record = cat.object(&chunk)?.ok_or_else(|| {
                        MochiError::new(ErrorCode::CatalogInvalid, "unknown chunk")
                    })?;
                    let at = cat.object_location(&chunk)?.ok_or_else(|| {
                        MochiError::new(
                            ErrorCode::UnsupportedFeature,
                            "a chunk without a location cannot be copied",
                        )
                    })?;
                    let bytes = load_stored(src, at, &record, &opts.limits)?;
                    // A sealed chunk is opened to its Zstandard frame; the
                    // writer of the new archive seals it again (D20 item 10).
                    let (record, bytes) = if record.protection == Protection::Aead {
                        let key = crate::keys::catalog_key(opts, cat).ok_or_else(|| {
                            MochiError::new(
                                ErrorCode::KeyUnavailable,
                                "this archive is encrypted: a passphrase is required to copy it",
                            )
                        })?;
                        unseal_object(&record, &bytes, &key.context(), &opts.limits)?
                    } else {
                        verify_stored(&record, &bytes)?;
                        (record, bytes)
                    };
                    stored.push((record, bytes));
                }
            }
            tx.put_copied(
                p.clone(),
                FileVersionEntry {
                    version,
                    extents,
                    attributes: attrs,
                },
                stored,
            );
        }
        extra(&mut tx);
        if force_checkpoint {
            w.request_checkpoint();
        }
        let outcome = w.commit(tx, ctx)?;

        // Bookkeeping for what the next commits may reference.
        for id in put.values() {
            if self.copied_versions.insert(*id) {
                let (_, extents) = cat.file_version(id)?.ok_or_else(|| {
                    MochiError::new(ErrorCode::CatalogInvalid, "version vanished")
                })?;
                for e in extents {
                    if let ExtentSource::Chunk { chunk, .. } = e.source {
                        if self.copied_chunks.insert(chunk) {
                            let r = cat.object(&chunk)?.ok_or_else(|| {
                                MochiError::new(ErrorCode::CatalogInvalid, "unknown chunk")
                            })?;
                            self.chunks_copied += 1;
                            self.bytes_copied += r.stored_len;
                        }
                        self.visible_chunks.insert(chunk);
                    }
                }
            }
            self.visible_versions.insert(*id);
        }
        if outcome.checkpoint {
            self.visible_versions = next.values().copied().collect();
            self.visible_chunks.clear();
            for id in &self.visible_versions {
                if let Some((_, extents)) = cat.file_version(id)? {
                    for e in extents {
                        if let ExtentSource::Chunk { chunk, .. } = e.source {
                            self.visible_chunks.insert(chunk);
                        }
                    }
                }
            }
        }
        self.prev = next;
        self.diffs.push((deleted, put));
        Ok(outcome)
    }

    /// Before publication: the new archive (opened at its head) has exactly
    /// one commit per commit written, each with the namespace written.
    pub(crate) fn check_namespaces(
        &self,
        new_head: &OpenedHead,
        fail: &dyn Fn(String) -> MochiError,
    ) -> Result<()> {
        if new_head.commit.seq + 1 != self.commits() {
            return Err(fail(format!(
                "{} commits for {} written",
                new_head.commit.seq + 1,
                self.commits()
            )));
        }
        let mut expected = Namespace::new();
        let mut diffs = self.diffs.iter();
        new_head.catalog.replay_each(|seq, snapshot| {
            let (deleted, put) = diffs.next().ok_or_else(|| fail("extra commits".into()))?;
            for p in deleted {
                expected.remove(p);
            }
            expected.extend(put.iter().map(|(p, v)| (p.clone(), *v)));
            if namespace(snapshot) != expected {
                return Err(fail(format!("commit {seq}'s namespace differs")));
            }
            Ok(())
        })?;
        Ok(())
    }
}

/// Every version's promised attributes, from every delta manifest of the
/// archive (each hash-verified against its commit, the chain checked): a
/// version is introduced exactly once, by the delta of the commit that
/// wrote it, whether or not any later snapshot still reaches it.
pub(crate) fn all_attributes(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
) -> Result<BTreeMap<FileVersionId, Attributes>> {
    let history = commit_history(src, opts)?;
    let mut map = BTreeMap::new();
    for (i, e) in history.iter().enumerate() {
        let delta = read_bound_manifest(
            src,
            &e.commit,
            &e.commit.delta_manifest,
            e.commit_offset,
            ManifestKind::Delta,
            opts,
        )?;
        if let Some(prev) = i.checked_sub(1).and_then(|p| history.get(p)) {
            check_delta_parent_link(&delta, &prev.commit)?;
        }
        for v in &delta.file_versions {
            if map.insert(v.version.id, v.attributes).is_some() {
                return Err(MochiError::new(
                    ErrorCode::RecordInvalid,
                    format!(
                        "delta manifest {} introduces version {:?} a second time (D10.4)",
                        delta.commit_seq, v.version.id
                    ),
                ));
            }
        }
    }
    Ok(map)
}

/// Rewrite the archive `source` holds into a new archive `name` in `dir`.
/// See the module documentation.
#[allow(clippy::too_many_arguments)]
pub fn compact<S, D>(
    source: &ArchiveWriter<S>,
    dir: &mut D,
    name: &str,
    ids: Box<dyn IdSource>,
    keep: &Keep,
    options: &CompactOptions,
    ctx: &JobContext<'_>,
) -> Result<CompactReport>
where
    S: Storage,
    D: StorageDir,
    D::File: Storage,
{
    let src: &dyn ReadStorage = source.storage();
    let opts = options.read.clone();
    // How the new archive is read back: with its own passphrases.
    let out_opts = match &options.new_keys {
        Some(k) => ReadOptions {
            keys: Some(k.clone()),
            ..opts.clone()
        },
        None => opts.clone(),
    };
    ctx.report(phase::PREPARE, 0, None);
    let head = open_head(src, &opts)?;
    if Some(head.commit_id) != source.head_commit_id() {
        return Err(MochiError::new(
            ErrorCode::LockConflict,
            "the source's head is not the one its writer holds",
        ));
    }
    let head_seq = head.commit.seq;
    let source_head = PlanHead {
        seq: head_seq,
        commit_id: hex(head.commit_id.as_bytes()),
        footer_offset: head.location.footer.footer_offset,
        committed_len: head.location.committed_len,
    };
    let retention = resolve_retention(src, &head, &opts)?;
    let roots: Vec<u64> = match keep {
        Keep::Every => (0..=head_seq).collect(),
        Keep::Roots(planned) => {
            if *planned != source_head {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    format!(
                        "the plan was made at commit {} ({}), but the archive's head is now \
                         commit {} ({}); plan again",
                        planned.seq, planned.commit_id, source_head.seq, source_head.commit_id
                    ),
                ));
            }
            retention.roots(head_seq).into_iter().collect()
        }
    };
    let root_set: BTreeSet<u64> = roots.iter().copied().collect();
    let marked = mark(&head.catalog, &root_set, ctx)?;
    let attributes = all_attributes(src, &opts)?;
    let history = commit_history(src, &opts)?;
    let by_seq: BTreeMap<u64, _> = history.iter().map(|h| (h.commit.seq, h)).collect();
    let source_commit = |s: u64| {
        by_seq.get(&s).copied().ok_or_else(|| {
            MochiError::new(
                ErrorCode::RecordInvalid,
                format!("commit {s} is missing from the history"),
            )
        })
    };

    // What the new archive's metadata will say.
    let new_seq: BTreeMap<u64, u64> = roots
        .iter()
        .enumerate()
        .map(|(i, r)| (*r, i as u64))
        .collect();
    let provenance = Provenance {
        source_archive_id: head.commit.archive_id,
        commits: roots
            .iter()
            .map(|r| Ok((*r, source_commit(*r)?.commit_id)))
            .collect::<Result<_>>()?,
        collected: (0..head_seq).filter(|s| !root_set.contains(s)).collect(),
        reason: options.reason,
    };
    let mut new_retention = RetentionState::default();
    for (label, s) in &retention.holds {
        // A held snapshot is a root, so it is kept.
        let n = new_seq.get(s).copied().ok_or_else(|| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                "internal: a held snapshot is not kept",
            )
        })?;
        new_retention.holds.insert(label.clone(), n);
    }
    for s in &retention.expired {
        if let Some(n) = new_seq.get(s) {
            new_retention.expired.insert(*n);
        }
    }
    let mut retention_ops: Vec<RetentionOp> = new_retention
        .expired
        .iter()
        .map(|s| RetentionOp::Expire { seq: *s })
        .collect();
    retention_ops.extend(new_retention.holds.iter().map(|(l, s)| RetentionOp::Hold {
        label: l.clone(),
        seq: *s,
    }));

    let (chunk_size, zstd_level) = source.recorded_parameters();
    let wopts = WriterOptions {
        chunk_size: Some(chunk_size),
        zstd_level: Some(zstd_level),
        record_time: false,
        dedup: Dedup::Off,
        // A rewrite keeps the source's profile; a TAR-compatible source gets
        // its framing regenerated by the writer (Annex B.2.9 D19 rule 9).
        profile: Some(head.descriptor.profile()),
        // The passphrases are the new archive's, and its key is new (D20
        // item 10): nothing sealed in the source is carried over.
        read: out_opts.clone(),
        kdf: options.kdf,
        checkpoint_trigger: options.checkpoint_trigger,
    };
    let mut versions_verified = None;
    let mut new_head_id = None;
    let mut new_ids: Vec<String> = Vec::new();
    let mut copier = Copier::default();

    let (writer, (), durability) = ArchiveWriter::build_in(dir, name, ids, wopts, ctx, |w| {
        #[cfg(any(test, feature = "test-controls"))]
        if let Some(p) = options.checkpoint_policy {
            w.set_checkpoint_policy(p)?;
        }
        let last = roots.len().saturating_sub(1);
        head.catalog.replay_each(|seq, snapshot| {
            let Some(&i) = new_seq.get(&seq) else {
                return Ok(());
            };
            ctx.check_cancelled()?;
            let time = source_commit(seq)?.commit.time;
            let outcome = copier.commit(
                w,
                src,
                &head.catalog,
                namespace(snapshot),
                &attributes,
                &opts,
                ctx,
                |tx| {
                    if i == 0 {
                        tx.set_provenance(provenance.clone());
                    }
                    if i as usize == last {
                        for op in &retention_ops {
                            tx.push_retention(op.clone());
                        }
                    }
                    if let Some(t) = time {
                        tx.at(t);
                    }
                },
            )?;
            if outcome.seq != i {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: the new archive's sequence ran ahead of the mapping",
                ));
            }
            new_ids.push(hex(outcome.commit_id.as_bytes()));
            ctx.report(phase::COPY, copier.commits(), Some(roots.len() as u64));
            Ok(())
        })?;

        // §18.2 steps 1–3, against what was written, before publication.
        ctx.report(phase::VERIFY, 0, None);
        let out: &dyn ReadStorage = w.storage();
        let new_head = open_head(out, &out_opts)?;
        let fail = |what: String| {
            MochiError::new(
                ErrorCode::CheckpointMismatch,
                format!("the compacted archive does not reproduce its source: {what}"),
            )
        };
        if new_head.commit.seq + 1 != roots.len() as u64 {
            return Err(fail(format!(
                "{} commits for {} kept snapshots",
                new_head.commit.seq + 1,
                roots.len()
            )));
        }
        copier.check_namespaces(&new_head, &fail)?;
        let written = all_attributes(out, &out_opts)?;
        let want: BTreeMap<FileVersionId, Attributes> = marked
            .versions
            .iter()
            .map(|id| {
                Ok((
                    *id,
                    *attributes
                        .get(id)
                        .ok_or_else(|| fail("attributes".into()))?,
                ))
            })
            .collect::<Result<_>>()?;
        if written != want {
            return Err(fail("promised attributes differ".into()));
        }
        if segment_state(out, &new_head, &out_opts)?.retention != new_retention {
            return Err(fail("the retention state differs".into()));
        }
        if read_provenance(out, &out_opts)?.as_ref() != Some(&provenance) {
            return Err(fail("the provenance differs".into()));
        }
        if options.verify_content {
            let mut n = 0u64;
            for id in new_head.catalog.file_version_ids()? {
                let (v, _) = new_head
                    .catalog
                    .file_version(&id)?
                    .ok_or_else(|| fail("a listed version is missing".into()))?;
                if v.kind == crate::catalog::namespace::EntryKind::Directory {
                    continue;
                }
                ctx.check_cancelled()?;
                read_version(
                    out,
                    &new_head.catalog,
                    &id,
                    &mut std::io::sink(),
                    &out_opts,
                    ctx,
                )?;
                n += 1;
            }
            versions_verified = Some(n);
        }
        new_head_id = Some(new_head.commit.archive_id);
        Ok(())
    })?;

    let new_len = writer.storage().size()?;
    let new_archive_id = new_head_id.map(|a| hex(a.as_bytes())).unwrap_or_default();
    drop(writer);
    Ok(CompactReport {
        source_archive_id: hex(head.commit.archive_id.as_bytes()),
        source_len: src.size()?,
        source_head,
        new_archive_id,
        new_len,
        commits: roots
            .iter()
            .zip(new_ids)
            .enumerate()
            .map(|(i, (r, id))| {
                Ok(CommitMapping {
                    source_seq: *r,
                    source_commit_id: hex(source_commit(*r)?.commit_id.as_bytes()),
                    new_seq: i as u64,
                    new_commit_id: id,
                })
            })
            .collect::<Result<_>>()?,
        collected: provenance.collected.clone(),
        chunks_copied: copier.chunks_copied,
        stored_bytes_copied: copier.bytes_copied,
        file_versions: marked.versions.len() as u64,
        versions_verified,
        recovery_copies: "none required: the Core profile has no recovery-copy requirement",
        durability_unconfirmed: match durability {
            Some(PublishDurability::DirectoryUnconfirmed(why)) => Some(why),
            _ => None,
        },
        source_kept: true,
    })
}

/// The provenance a compacted archive's delta(0) records; `None` for an
/// archive that was not made by compaction.
pub fn read_provenance(src: &dyn ReadStorage, opts: &ReadOptions) -> Result<Option<Provenance>> {
    let history = commit_history(src, opts)?;
    let Some(first) = history.first() else {
        return Ok(None);
    };
    if !matches!(first.commit.metadata, Metadata::Checkpoint { .. }) {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "commit 0 is not a checkpoint",
        ));
    }
    let delta = read_bound_manifest(
        src,
        &first.commit,
        &first.commit.delta_manifest,
        first.commit_offset,
        ManifestKind::Delta,
        opts,
    )?;
    Ok(delta.provenance)
}
