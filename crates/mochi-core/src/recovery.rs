//! Catalog recovery from recovery manifests alone (spec §11, §11.1; plan C4).
//!
//! Input: manifest objects found anywhere (typically by scanning an archive
//! whose SQLite catalogs are destroyed), and optionally the hash of the
//! manifest to treat as the head. Output: a rebuilt, verified catalog plus an
//! honest statement of which recovery scope the metadata supports.
//!
//! Trust rules:
//!
//! * **Trust flows only from the head.** The chain is walked backward from
//!   the head by parent links, each the stored-object hash (O20) of the
//!   parent commit's **delta** manifest (manifest schema 1). A manifest not
//!   reachable that way is never used, whatever it claims. A link that lands
//!   on a snapshot manifest breaks the chain: schema 1 forbids it.
//! * **Snapshot recovery needs a baseline** in the verified chain. From
//!   manifests alone that is the root commit's delta (parent null). Without
//!   one, only file-level metadata is claimed (§11.1: finding valid pieces is
//!   not snapshot recovery).
//! * **Snapshot manifests are not on the chain** (schema 1: parent always
//!   null). A snapshot S(b) can serve as a baseline only through commit *b*'s
//!   record, which binds it by hash and supplies the delta-manifest hash the
//!   next delta links to (Annex B.2 D10.8). That is baseline recovery, plan
//!   T16, and needs commit records; this function sees manifests only, so a
//!   scanned snapshot is counted as `unused`. Exception: a snapshot the
//!   caller names as the head is used on its own, since it is self-contained.
//!   **Change from schema 0**, where a snapshot carried a parent link and
//!   could restart a chain past a missing delta: that capability returns with
//!   T16, anchored on commit records instead of a manifest-only link.
//! * **Authenticity comes only from the head.** A forger who rewrites one
//!   manifest and re-links every later one produces a new, self-consistent
//!   chain with a different head hash. Only a *trusted* head exposes that:
//!   the footer-verified commit (C5) or a user-supplied hash (spec D8).
//!   Without one, recovery reconstructs whichever consistent history it is
//!   given (pinned by `a_rewritten_history_is_only_detectable_against_a_trusted_head`).
//! * Scopes here describe what the *metadata* makes possible. Content must
//!   still verify against stored and content hashes when it is read (C6/C7).
//!
//! Without an explicit head, the latest manifest is chosen only if exactly
//! one archive and one candidate at the highest sequence exist; otherwise the
//! caller must name the head. Freshness is not established here (§5.7, D8).

use std::collections::{BTreeMap, HashMap};

use mochi_format::cbor::CborLimits;
use mochi_format::digest::StoredObjectHash;
use mochi_format::repr::StoredObject;
use mochi_format::Limits;

use crate::catalog::namespace::{FileVersionId, NamespaceOp, Snapshot};
use crate::catalog::{Catalog, Commit, META_ARCHIVE_ID};
use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{Attributes, Manifest, ManifestKind};
use crate::object::ArchiveId;

/// The four recovery scopes of spec §11.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryScope {
    /// Some content bytes, no verified file or namespace metadata.
    PayloadSalvage,
    /// Complete files with their versions and extents, but not the namespace.
    FileRecovery,
    /// The namespace, versions, and promised attributes at the head.
    SnapshotRecovery,
    /// The same for an earlier, retained commit.
    HistoricalRecovery,
}

/// Why rebuilding stopped before the head, if it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedAt {
    pub seq: u64,
    pub reason: String,
}

#[derive(Debug)]
pub struct ManifestRecovery {
    pub archive_id: ArchiveId,
    pub head_seq: u64,
    pub head_hash: StoredObjectHash,
    /// Commits whose complete snapshot was rebuilt: `(first, last)` inclusive.
    pub snapshot_range: Option<(u64, u64)>,
    /// The verified chain reaches back to this commit.
    pub chain_start_seq: u64,
    /// The commit whose parent manifest could not be found or did not match.
    pub chain_broken_at: Option<u64>,
    /// Rebuilding stopped here, with the reason (the range ends just before).
    pub stopped: Option<StoppedAt>,
    /// The rebuilt, verified catalog covering `snapshot_range`.
    pub catalog: Option<Catalog>,
    /// Promised attributes by version (stored in the catalog from C6).
    pub attributes: BTreeMap<FileVersionId, Attributes>,
    /// File versions described by verified-chain manifests (file recovery).
    pub file_versions_known: usize,
    /// Chunks described by verified-chain manifests.
    pub chunks_known: usize,
    /// Found objects that were not valid manifests.
    pub rejected: usize,
    /// Valid manifests not on the verified chain (other archives, forks,
    /// orphans before a break).
    pub unused: usize,
}

impl ManifestRecovery {
    /// Which §11.1 scope the metadata supports for commit `seq`.
    pub fn scope_for(&self, seq: u64) -> RecoveryScope {
        match self.snapshot_range {
            Some((first, last)) if (first..=last).contains(&seq) => {
                if seq == self.head_seq {
                    RecoveryScope::SnapshotRecovery
                } else {
                    RecoveryScope::HistoricalRecovery
                }
            }
            _ if self.file_versions_known > 0 => RecoveryScope::FileRecovery,
            _ => RecoveryScope::PayloadSalvage,
        }
    }
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

/// Namespace operations taking `from` to `to` (for a snapshot manifest that
/// stands in for its commit's delta). Deletions first, then puts; validity is
/// judged on the completed state, so order within each group is free.
fn diff_ops(
    from: &Snapshot,
    to: &[(crate::catalog::path::ArchivePath, FileVersionId)],
) -> Vec<NamespaceOp> {
    let target: BTreeMap<_, _> = to.iter().map(|(p, v)| (p.clone(), *v)).collect();
    let mut ops: Vec<NamespaceOp> = from
        .iter()
        .filter(|(p, _)| !target.contains_key(*p))
        .map(|(p, _)| NamespaceOp::Delete { path: p.clone() })
        .collect();
    for (p, v) in &target {
        if from.get(p).map(|e| e.version) != Some(*v) {
            ops.push(NamespaceOp::Put {
                path: p.clone(),
                version: *v,
            });
        }
    }
    ops
}

/// A working catalog that materializes exactly commit `s.commit_seq`, built
/// from a snapshot manifest the caller has already hash-verified and
/// identity-bound (Annex B.2 D10.8). One commit row (parent null), the
/// snapshot's chunks and versions, its entries as `PUT`s, and the archive ID.
/// Reads no catalog image.
///
/// **Precondition:** the manifest's stored bytes were verified against the
/// hash that referenced it, as for [`Catalog::open_image`].
///
/// Errors from filling a hash-verified snapshot mean the snapshot is an
/// invalid record, not damaged bytes, so they are reported as
/// `RECORD_INVALID` (`IDENTITY_CONFLICT` and I/O keep their codes).
pub fn catalog_from_snapshot(s: &Manifest) -> Result<Catalog> {
    if s.kind != ManifestKind::Snapshot {
        return Err(invalid("a catalog is built from a snapshot manifest"));
    }
    let as_record_invalid = |e: MochiError| match e.code {
        ErrorCode::InvalidArgument
        | ErrorCode::NamespaceInvalid
        | ErrorCode::ExtentInvalid
        | ErrorCode::CatalogInvalid => MochiError::new(
            ErrorCode::RecordInvalid,
            format!("snapshot manifest {}: {}", s.commit_seq, e.message),
        ),
        _ => e,
    };
    let build = || -> Result<Catalog> {
        let mut cat = Catalog::new_working()?;
        for c in &s.chunks {
            cat.insert_object(&c.record, c.location)?;
        }
        for v in &s.file_versions {
            cat.insert_file_version(&v.version, &v.extents)?;
        }
        let ops = diff_ops(&Snapshot::new(), &s.entries);
        cat.append_commit(&Commit {
            seq: s.commit_seq,
            parent: None,
            ops,
        })?;
        cat.set_meta(META_ARCHIVE_ID, s.archive_id.as_bytes())?;
        cat.verify()?;
        if cat.head_commit()? != Some(s.commit_seq) {
            return Err(MochiError::new(
                ErrorCode::CatalogInvalid,
                "internal: the catalog built from a snapshot does not materialize its commit",
            ));
        }
        Ok(cat)
    };
    build().map_err(as_record_invalid)
}

fn snapshot_matches(
    s: &Snapshot,
    entries: &[(crate::catalog::path::ArchivePath, FileVersionId)],
) -> bool {
    s.len() == entries.len()
        && entries
            .iter()
            .all(|(p, v)| s.get(p).map(|e| e.version) == Some(*v))
}

pub fn recover_from_manifests(
    found: &[StoredObject],
    head: Option<StoredObjectHash>,
    limits: &Limits,
) -> Result<ManifestRecovery> {
    let cbor_limits = CborLimits::default();
    let mut parsed: Vec<(Manifest, StoredObjectHash)> = Vec::new();
    let mut rejected = 0;
    for obj in found {
        match Manifest::from_stored(obj, limits, &cbor_limits) {
            Ok(m) => parsed.push(m),
            Err(_) => rejected += 1,
        }
    }
    let by_hash: HashMap<StoredObjectHash, usize> = parsed
        .iter()
        .enumerate()
        .map(|(i, (_, h))| (*h, i))
        .collect();

    // ---- choose the head ------------------------------------------------------------
    let head_idx = match head {
        Some(h) => *by_hash
            .get(&h)
            .ok_or_else(|| invalid("the requested head manifest is not among the found objects"))?,
        None => {
            let mut archives: Vec<ArchiveId> = parsed.iter().map(|(m, _)| m.archive_id).collect();
            archives.sort();
            archives.dedup();
            match archives.len() {
                0 => return Err(invalid("no valid recovery manifests were found")),
                1 => {}
                n => {
                    return Err(invalid(format!(
                        "manifests from {n} different archives were found; name the head to recover"
                    )))
                }
            }
            // The head of a chain is a delta: every commit has exactly one
            // (commit key 6), and snapshots are off-chain.
            let deltas: Vec<usize> = (0..parsed.len())
                .filter(|i| parsed[*i].0.kind == ManifestKind::Delta)
                .collect();
            if deltas.is_empty() {
                return Err(invalid(
                    "no delta manifests were found; name a snapshot manifest as the head to use it alone",
                ));
            }
            let max = deltas
                .iter()
                .map(|i| parsed[*i].0.commit_seq)
                .max()
                .unwrap_or(0);
            let tops: Vec<usize> = deltas
                .into_iter()
                .filter(|i| parsed[*i].0.commit_seq == max)
                .collect();
            if tops.len() != 1 {
                return Err(invalid(format!(
                    "{} different manifests claim the latest commit {max}; name the head to recover",
                    tops.len()
                )));
            }
            tops[0]
        }
    };

    // ---- verify the chain backward from the head --------------------------------------
    let mut chain = vec![head_idx];
    let mut chain_broken_at = None;
    loop {
        let (cur, _) = &parsed[*chain.last().unwrap_or(&head_idx)];
        let Some(link) = cur.parent else { break };
        match by_hash.get(&link.delta_manifest_hash) {
            Some(&i)
                if parsed[i].0.archive_id == cur.archive_id
                    && parsed[i].0.commit_seq == link.seq
                    && parsed[i].0.kind == ManifestKind::Delta =>
            {
                chain.push(i);
            }
            _ => {
                chain_broken_at = Some(cur.commit_seq);
                break;
            }
        }
    }
    chain.reverse();
    let (head_m, head_hash) = &parsed[head_idx];
    let mut report = ManifestRecovery {
        archive_id: head_m.archive_id,
        head_seq: head_m.commit_seq,
        head_hash: *head_hash,
        snapshot_range: None,
        chain_start_seq: parsed[chain[0]].0.commit_seq,
        chain_broken_at,
        stopped: None,
        catalog: None,
        attributes: BTreeMap::new(),
        file_versions_known: 0,
        chunks_known: 0,
        rejected,
        unused: parsed.len() - chain.len(),
    };
    for &i in &chain {
        report.file_versions_known += parsed[i].0.file_versions.len();
        report.chunks_known += parsed[i].0.chunks.len();
    }

    // ---- find the earliest baseline and rebuild forward ----------------------------------
    let Some(base) = chain.iter().position(|&i| {
        let m = &parsed[i].0;
        m.kind == ManifestKind::Snapshot || m.parent.is_none()
    }) else {
        return Ok(report);
    };
    let mut catalog = Catalog::new_working()?;
    let mut applied: Option<u64> = None;
    for &i in &chain[base..] {
        let m = &parsed[i].0;
        let step = (|| -> Result<()> {
            if m.kind == ManifestKind::Snapshot && applied.is_none() {
                // No prior state: a snapshot stands alone (D10.8).
                catalog = catalog_from_snapshot(m)?;
                return Ok(());
            }
            for c in &m.chunks {
                catalog.insert_object(&c.record, c.location)?;
            }
            for v in &m.file_versions {
                catalog.insert_file_version(&v.version, &v.extents)?;
            }
            let ops = match m.kind {
                ManifestKind::Delta => m.ops.clone(),
                ManifestKind::Snapshot => {
                    let before = if applied.is_some() {
                        catalog.replay(None)?
                    } else {
                        Snapshot::new()
                    };
                    diff_ops(&before, &m.entries)
                }
            };
            let snapshot = catalog.append_commit(&Commit {
                seq: m.commit_seq,
                parent: applied,
                ops,
            })?;
            // Internal consistency of the diff above, not a check on the
            // manifest (see the module note): a mismatch is a bug here.
            if m.kind == ManifestKind::Snapshot && !snapshot_matches(&snapshot, &m.entries) {
                return Err(MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "internal: rebuilt state differs from the snapshot manifest just applied",
                ));
            }
            Ok(())
        })();
        match step {
            Ok(()) => {
                for v in &m.file_versions {
                    report.attributes.insert(v.version.id, v.attributes);
                }
                applied = Some(m.commit_seq);
            }
            Err(e) => {
                report.stopped = Some(StoppedAt {
                    seq: m.commit_seq,
                    reason: e.to_string(),
                });
                break;
            }
        }
    }
    if let Some(last) = applied {
        catalog.verify()?;
        report.snapshot_range = Some((parsed[chain[base]].0.commit_seq, last));
        report.catalog = Some(catalog);
    }
    Ok(report)
}
