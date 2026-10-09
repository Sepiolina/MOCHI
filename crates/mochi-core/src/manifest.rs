//! Recovery manifests, schema version 1 (spec §11, Annex B.2 D10–D11; schema
//! `docs/schemas/recovery-manifest-v1.cddl`). **DRAFT** (R3).
//!
//! A manifest describes one commit in a form that needs no SQLite. Two kinds,
//! with separate shapes:
//!
//! * **Delta** (kind 0): the logical transition of its commit — the new
//!   chunks, the new file versions with their extents and promised
//!   attributes, and the namespace operations in order. Every commit has one
//!   (commit key 6). Its parent link names the **parent commit's delta
//!   manifest** by stored-object hash (never a snapshot), and is null exactly
//!   at sequence 0. Deltas therefore form a hash chain that a scan can verify
//!   end to end from a trusted head.
//! * **Snapshot** (kind 1): the complete authoritative state after a
//!   checkpoint commit (commit key 5, form 0). Parent always null, no
//!   operations, and self-contained: every chunk and version it names is in
//!   it. It is bound to its commit by the commit's reference (hash) and by
//!   identity (archive ID, sequence, transaction ID), never by a chain link.
//!
//! Both carry the D11 CBOR-native envelope: keys 0 (schema), 1 (archive ID),
//! 2 (sequence), 9 (required features), 10 (transaction ID). Required
//! features and identity use the same checks as the binary envelope
//! (`mochi_format::envelope`), so a fault gets the same code in both.
//!
//! Encoding is canonical CBOR (spec D2) through `mochi_format::cbor`; one
//! logical manifest has exactly one encoding, so its hash is well defined.
//! Schema 0 (`recovery-manifest-v0.cddl`) is the pre-batch draft and is
//! refused as legacy (§26).

use std::collections::BTreeSet;

use mochi_format::cbor::{self, CborLimits, Fields, Value};
use mochi_format::codec::{Encoding, Protection};
use mochi_format::digest::{
    stored_object_hash, ChunkContentHash, CommitId, FileContentHash, StoredObjectHash,
};
use mochi_format::envelope::{check_required_features, RecordIdentity};
use mochi_format::frame::{encode_skippable_frame_within, walk_frame, FrameDetail};
use mochi_format::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::seal::FEATURE_ENCRYPTED;
use mochi_format::{FormatError, Limits};

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, FileVersion};
use crate::error::{ErrorCode, MochiError, Result};
use crate::object::{ArchiveId, Dependency, ObjectId, ObjectRecord};
use crate::retention::{check_op, RetentionOp, RetentionState};

/// Schema version of a manifest without retention data (R3 draft).
pub const SCHEMA_VERSION: u64 = 1;

/// Schema version that adds key 11, retention (spec Annex B D18, plan C9).
/// A writer uses it only when the manifest carries retention data, so a
/// manifest without any keeps its schema-1 bytes, and one logical manifest
/// still has exactly one encoding: schema 2 with empty retention data is
/// refused.
pub const RETENTION_SCHEMA_VERSION: u64 = 2;

/// Schema version that adds key 13, key operations or key state (Annex B.2.10
/// D20; `docs/schemas/recovery-manifest-v3.cddl`). Used exactly for the
/// manifests of an Encrypted-profile archive (required feature 1): there it
/// is schema 2 with key 11 required even when empty and key 13 required.
pub const ENCRYPTED_SCHEMA_VERSION: u64 = 3;

/// The pre-batch draft schema. Refused, and named as legacy (§26).
pub const LEGACY_SCHEMA_VERSION: u64 = 0;

/// Required features this build understands (key 9): the Encrypted profile's
/// identifier (D20 item 4). Anything else fails closed.
pub const KNOWN_REQUIRED_FEATURES: &[u64] = &[FEATURE_ENCRYPTED];

/// Most key envelopes one commit lists (D20 item 3); also bounds key state.
pub const MAX_KEY_ENVELOPES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    Delta,
    Snapshot,
}

/// A delta's link to its parent commit (key 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentLink {
    /// Always this manifest's sequence − 1.
    pub seq: u64,
    /// Stored-object hash of the parent commit's **delta** manifest (that
    /// commit's key 6). Never a snapshot manifest's hash.
    pub delta_manifest_hash: StoredObjectHash,
}

/// POSIX attributes (spec D6): permission bits only, numeric owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PosixAttributes {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mtime {
    pub secs: i64,
    pub nanos: u32,
}

/// Windows attribute bits MOCHI promises (spec D6).
pub const WINDOWS_READONLY: u32 = 0x01;
pub const WINDOWS_HIDDEN: u32 = 0x02;
pub const WINDOWS_SYSTEM: u32 = 0x04;
pub const WINDOWS_ARCHIVE: u32 = 0x20;
pub const WINDOWS_PROMISED_MASK: u32 =
    WINDOWS_READONLY | WINDOWS_HIDDEN | WINDOWS_SYSTEM | WINDOWS_ARCHIVE;

/// Promised attributes (spec Annex B.1 D6). `None` = not recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attributes {
    pub posix: Option<PosixAttributes>,
    pub windows: Option<u32>,
    pub mtime: Option<Mtime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkEntry {
    pub record: ObjectRecord,
    /// Offset in a monolithic archive; `None` = rediscover by scanning and
    /// matching the stored hash.
    pub location: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersionEntry {
    pub version: FileVersion,
    pub extents: Vec<Extent>,
    pub attributes: Attributes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub archive_id: ArchiveId,
    pub commit_seq: u64,
    /// Key 10: the transaction ID of commit `commit_seq` (D11 identity).
    pub transaction_id: [u8; 16],
    /// Delta only; `None` exactly at sequence 0. Always `None` for a snapshot.
    pub parent: Option<ParentLink>,
    pub kind: ManifestKind,
    /// Sorted by object ID, unique.
    pub chunks: Vec<ChunkEntry>,
    /// Sorted by file-version ID, unique.
    pub file_versions: Vec<FileVersionEntry>,
    /// Delta only; in order.
    pub ops: Vec<NamespaceOp>,
    /// Snapshot only; sorted by path, unique.
    pub entries: Vec<(ArchivePath, FileVersionId)>,
    /// Key 9. Strictly increasing; empty for Core.
    pub required_features: Vec<u64>,
    /// Delta only (schema 2, key 11): the commit's retention operations, in
    /// order.
    pub retention_ops: Vec<RetentionOp>,
    /// Snapshot only (schema 2, key 11): the complete retention state.
    pub retention: RetentionState,
    /// Delta(0) of a compacted archive only (schema 2, key 12).
    pub provenance: Option<Provenance>,
    /// Encrypted profile only (schema 3, key 13): the key operations of a
    /// delta, or the key state of a snapshot (D20 item 10).
    pub keys: ManifestKeys,
}

/// A key operation on the set of valid key envelopes (D20 item 10): the
/// rewrap audit record. Applied in order, each judged against the state the
/// previous one left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOp {
    /// Add an envelope; its ID must not be in the set.
    Add([u8; 16]),
    /// Remove an envelope; its ID must be in the set, and the set must not
    /// become empty.
    Remove([u8; 16]),
}

/// Key 13 of a schema-3 manifest: operations in a delta, state in a snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestKeys {
    /// Delta only; in order.
    pub ops: Vec<KeyOp>,
    /// Snapshot only: the complete set of valid envelope IDs, strictly
    /// increasing bytewise.
    pub state: Vec<[u8; 16]>,
}

impl ManifestKeys {
    /// Apply `ops` to `set` in order, atomically: on any violation `set` is
    /// unchanged. `RECORD_INVALID`, because a manifest that does this is
    /// invalid, not unsupported.
    pub fn apply(set: &mut Vec<[u8; 16]>, ops: &[KeyOp]) -> Result<()> {
        let mut next = set.clone();
        for op in ops {
            match op {
                KeyOp::Add(id) => {
                    if next.contains(id) {
                        return Err(schema(
                            "a key operation adds an envelope already in the set",
                        ));
                    }
                    next.push(*id);
                }
                KeyOp::Remove(id) => {
                    let Some(i) = next.iter().position(|e| e == id) else {
                        return Err(schema("a key operation removes an envelope not in the set"));
                    };
                    next.remove(i);
                    if next.is_empty() {
                        return Err(schema("a key operation would leave no key envelope"));
                    }
                }
            }
            if next.len() > MAX_KEY_ENVELOPES {
                return Err(schema("a key operation exceeds the key-envelope limit"));
            }
        }
        next.sort();
        *set = next;
        Ok(())
    }
}

/// Where a compacted archive came from (spec Annex B D18; plan C9). Carried
/// by the new archive's delta(0), the only place it can be: it is fixed when
/// the archive is created, like the descriptor, and the descriptor holds
/// identity only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub source_archive_id: ArchiveId,
    /// For new commit *i*, at index *i*: the source commit whose snapshot it
    /// reproduces, as (sequence, commit ID). Strictly increasing by
    /// sequence; the last is the source head.
    pub commits: Vec<(u64, CommitId)>,
    /// Source snapshots left out because no retained root protected them,
    /// strictly increasing, each before the source head.
    pub collected: Vec<u64>,
    /// Why the archive was rewritten (schema 3 only; key 3 of the map). A
    /// rewrite of an Encrypted archive always creates a new data key; this
    /// says whether that was the point (D20 item 10, the re-encryption audit
    /// record). Absent on the wire means [`RewriteReason::Collection`].
    pub reason: RewriteReason,
}

/// The reason for a rewrite (`rekey --reencrypt` versus collection or
/// compaction): the two separate audit records of spec §14.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RewriteReason {
    /// Collection or compaction (the default; key 3 absent).
    #[default]
    Collection,
    /// `rekey --reencrypt` (key 3 = 1).
    Reencryption,
}

impl Provenance {
    fn check(&self) -> Result<()> {
        let Some((head, _)) = self.commits.last() else {
            return Err(schema("provenance names no source commit"));
        };
        if !self.commits.windows(2).all(|w| w[0].0 < w[1].0) {
            return Err(schema("provenance commits are not strictly increasing"));
        }
        if !self.collected.windows(2).all(|w| w[0] < w[1]) {
            return Err(schema("collected snapshots are not strictly increasing"));
        }
        for c in &self.collected {
            if c >= head || self.commits.iter().any(|(s, _)| s == c) {
                return Err(schema(format!(
                    "collected snapshot {c} is the source head, after it, or also kept"
                )));
            }
        }
        Ok(())
    }
}

pub(crate) fn schema(msg: impl Into<String>) -> MochiError {
    MochiError::from(FormatError::Schema(msg.into()))
}

fn b32(b: &[u8; 32]) -> Value {
    Value::Bytes(b.to_vec())
}

// ---- encode -----------------------------------------------------------------------

impl Manifest {
    /// Put collections in canonical order. Encoding requires it; callers
    /// building manifests incrementally call this first.
    pub fn canonicalize(&mut self) {
        self.chunks.sort_by(|a, b| a.record.id.cmp(&b.record.id));
        self.file_versions
            .sort_by(|a, b| a.version.id.cmp(&b.version.id));
        self.entries.sort_by(|a, b| a.0.cmp(&b.0));
        for c in &mut self.chunks {
            c.record.dependencies.sort_by_key(dep_key);
        }
    }

    /// The D11 identity this manifest carries; must equal its commit's.
    pub fn identity(&self) -> RecordIdentity {
        RecordIdentity {
            archive_id: *self.archive_id.as_bytes(),
            commit_sequence: self.commit_seq,
            transaction_id: self.transaction_id,
        }
    }

    fn to_value(&self) -> Result<Value> {
        self.check_structure(&Limits::WRITER_DEFAULT)?;
        Ok(self.to_value_unchecked())
    }

    /// Whether this manifest belongs to an Encrypted-profile archive: it
    /// lists the D20 required feature (key 9).
    pub fn encrypted(&self) -> bool {
        self.required_features.contains(&FEATURE_ENCRYPTED)
    }

    /// 3 for an Encrypted-profile manifest (D20); otherwise 2 when the
    /// manifest carries retention data or provenance, else 1.
    pub fn schema_version(&self) -> u64 {
        if self.encrypted() {
            ENCRYPTED_SCHEMA_VERSION
        } else if self.retention_ops.is_empty()
            && self.retention.is_empty()
            && self.provenance.is_none()
        {
            SCHEMA_VERSION
        } else {
            RETENTION_SCHEMA_VERSION
        }
    }

    fn to_value_unchecked(&self) -> Value {
        let parent = match &self.parent {
            None => Value::Null,
            Some(p) => Value::Map(vec![
                (0, Value::Uint(p.seq)),
                (1, b32(p.delta_manifest_hash.as_bytes())),
            ]),
        };
        let mut fields = vec![
            (0, Value::Uint(self.schema_version())),
            (1, b32(self.archive_id.as_bytes())),
            (2, Value::Uint(self.commit_seq)),
            (3, parent),
            (
                4,
                Value::Uint(match self.kind {
                    ManifestKind::Delta => 0,
                    ManifestKind::Snapshot => 1,
                }),
            ),
            (
                5,
                Value::Array(self.chunks.iter().map(chunk_value).collect()),
            ),
            (
                6,
                Value::Array(self.file_versions.iter().map(version_value).collect()),
            ),
            (7, Value::Array(self.ops.iter().map(op_value).collect())),
            (
                8,
                Value::Array(
                    self.entries
                        .iter()
                        .map(|(p, v)| {
                            Value::Array(vec![
                                Value::Bytes(p.as_stored().to_vec()),
                                b32(v.as_bytes()),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                9,
                Value::Array(
                    self.required_features
                        .iter()
                        .map(|f| Value::Uint(*f))
                        .collect(),
                ),
            ),
            (10, Value::Bytes(self.transaction_id.to_vec())),
        ];
        if self.schema_version() >= RETENTION_SCHEMA_VERSION {
            fields.push((
                11,
                match self.kind {
                    ManifestKind::Delta => {
                        Value::Array(self.retention_ops.iter().map(retention_op_value).collect())
                    }
                    ManifestKind::Snapshot => retention_state_value(&self.retention),
                },
            ));
            if let Some(p) = &self.provenance {
                fields.push((12, provenance_value(p)));
            }
        }
        if self.schema_version() == ENCRYPTED_SCHEMA_VERSION {
            fields.push((
                13,
                match self.kind {
                    ManifestKind::Delta => Value::Array(
                        self.keys
                            .ops
                            .iter()
                            .map(|op| match op {
                                KeyOp::Add(id) => {
                                    Value::Array(vec![Value::Uint(0), Value::Bytes(id.to_vec())])
                                }
                                KeyOp::Remove(id) => {
                                    Value::Array(vec![Value::Uint(1), Value::Bytes(id.to_vec())])
                                }
                            })
                            .collect(),
                    ),
                    ManifestKind::Snapshot => Value::Array(
                        self.keys
                            .state
                            .iter()
                            .map(|id| Value::Bytes(id.to_vec()))
                            .collect(),
                    ),
                },
            ));
        }
        Value::Map(fields)
    }

    /// Canonical CBOR bytes (the frame payload). Refuses, with
    /// `CAPACITY_EXCEEDED`, a manifest that a default reader would reject for
    /// its item count or depth (spec Annex B.2.3 writer default rule).
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(cbor::encode_within(
            &self.to_value()?,
            &CborLimits::default(),
        )?)
    }

    /// Stored form: one skippable frame of kind `RecoveryManifest`. Its
    /// stored-object hash is this manifest's hash. Bounded by the reader
    /// defaults, never by the limits a writer reads with (B.2.3).
    pub fn to_stored(&self) -> Result<StoredObject> {
        let frame = encode_skippable_frame_within(
            FrameKind::RecoveryManifest,
            &self.encode()?,
            &Limits::WRITER_DEFAULT,
        )?;
        Ok(StoredObject::from_loaded(frame))
    }

    /// Rules that hold for any valid manifest on its own, checked on encode
    /// (against the writer defaults) and decode (against the reader's limits).
    fn check_structure(&self, limits: &Limits) -> Result<()> {
        let sorted_unique = |keys: Vec<&[u8]>| keys.windows(2).all(|w| w[0] < w[1]);
        if !sorted_unique(
            self.chunks
                .iter()
                .map(|c| &c.record.id.as_bytes()[..])
                .collect(),
        ) {
            return Err(schema("chunks are not sorted by ID without duplicates"));
        }
        if !sorted_unique(
            self.file_versions
                .iter()
                .map(|v| &v.version.id.as_bytes()[..])
                .collect(),
        ) {
            return Err(schema(
                "file versions are not sorted by ID without duplicates",
            ));
        }
        // Checklist Q27 [delegated, 2026-10-04]: a directory version has
        // logical length 0 and no extents (recovery-manifest-v1.cddl,
        // file-version keys 2 and 4). Enforced here, on encode and decode
        // alike, so the writer cannot emit what the reader rejects, and a
        // malformed directory is a schema violation (RECORD_INVALID) found
        // before any catalog code runs. The content-hash half of the rule is
        // enforced while decoding the entry.
        for v in &self.file_versions {
            if v.version.kind == EntryKind::Directory
                && (v.version.logical_len != 0 || !v.extents.is_empty())
            {
                return Err(schema(
                    "a directory version has logical length 0 and no extents",
                ));
            }
        }
        if !sorted_unique(self.entries.iter().map(|(p, _)| p.as_stored()).collect()) {
            return Err(schema(
                "snapshot entries are not sorted by path without duplicates",
            ));
        }
        for c in &self.chunks {
            let keys: Vec<(u8, [u8; 32])> = c.record.dependencies.iter().map(dep_key).collect();
            if !keys.windows(2).all(|w| w[0] < w[1]) {
                return Err(schema(
                    "chunk dependencies are not sorted without duplicates",
                ));
            }
            if c.record.stored_len == 0 {
                return Err(schema("chunk stored length is zero"));
            }
        }
        match self.kind {
            ManifestKind::Delta => {
                if !self.entries.is_empty() {
                    return Err(schema("a delta manifest has snapshot entries"));
                }
                if (self.commit_seq == 0) != self.parent.is_none() {
                    return Err(schema("a delta has no parent exactly when it is commit 0"));
                }
                if let Some(p) = &self.parent {
                    if p.seq.checked_add(1) != Some(self.commit_seq) {
                        return Err(schema(format!(
                            "parent sequence {} is not this sequence ({}) minus one",
                            p.seq, self.commit_seq
                        )));
                    }
                }
            }
            ManifestKind::Snapshot => {
                if self.parent.is_some() {
                    return Err(schema(
                        "a snapshot manifest has a parent link; a snapshot is a \
                         self-contained baseline (D10)",
                    ));
                }
                if !self.ops.is_empty() {
                    return Err(schema("a snapshot manifest has namespace operations"));
                }
                // A baseline must stand alone: everything it references is in it.
                let versions: BTreeSet<&FileVersionId> =
                    self.file_versions.iter().map(|v| &v.version.id).collect();
                let chunks: BTreeSet<&ObjectId> =
                    self.chunks.iter().map(|c| &c.record.id).collect();
                if self.entries.iter().any(|(_, v)| !versions.contains(v)) {
                    return Err(schema(
                        "a snapshot entry names a version the snapshot does not contain",
                    ));
                }
                for v in &self.file_versions {
                    for e in &v.extents {
                        if let ExtentSource::Chunk { chunk, .. } = &e.source {
                            if !chunks.contains(chunk) {
                                return Err(schema(
                                    "a snapshot extent names a chunk the snapshot does not contain",
                                ));
                            }
                        }
                    }
                }
            }
        }
        for v in &self.file_versions {
            check_attributes(&v.attributes)?;
            for (i, e) in v.extents.iter().enumerate() {
                if u32::try_from(i).ok() != Some(e.ordinal) {
                    return Err(schema("extent ordinals must equal their positions"));
                }
            }
        }
        if let Some(p) = &self.provenance {
            if self.kind != ManifestKind::Delta || self.commit_seq != 0 {
                return Err(schema("only delta(0) carries provenance"));
            }
            p.check()?;
        }
        // Retention (schema 2): ops belong to deltas, state to snapshots.
        match self.kind {
            ManifestKind::Delta => {
                if !self.retention.is_empty() {
                    return Err(schema("a delta manifest carries a retention state"));
                }
                for op in &self.retention_ops {
                    check_op(op, self.commit_seq)?;
                }
            }
            ManifestKind::Snapshot => {
                if !self.retention_ops.is_empty() {
                    return Err(schema("a snapshot manifest carries retention operations"));
                }
                self.retention.check(self.commit_seq)?;
            }
        }
        // Encrypted profile (schema 3, D20): the data key is archive-wide, so
        // no chunk depends on a key envelope, and every chunk is sealed;
        // outside the profile none is. Key operations belong to deltas, key
        // state to snapshots.
        let encrypted = self.encrypted();
        for c in &self.chunks {
            if encrypted != (c.record.protection == Protection::Aead) {
                return Err(schema(if encrypted {
                    "an Encrypted archive's manifest lists a chunk that is not sealed"
                } else {
                    "a manifest outside the Encrypted profile lists a sealed chunk"
                }));
            }
            if encrypted
                && c.record
                    .dependencies
                    .iter()
                    .any(|d| matches!(d, Dependency::KeyEnvelope(_)))
            {
                return Err(schema(
                    "a sealed chunk depends on no key envelope: the data key is archive-wide (D20)",
                ));
            }
        }
        if encrypted {
            match self.kind {
                ManifestKind::Delta => {
                    if !self.keys.state.is_empty() {
                        return Err(schema("a delta manifest carries a key state"));
                    }
                    if self.commit_seq == 0 && self.keys.ops.is_empty() {
                        return Err(schema(
                            "delta(0) of an Encrypted archive adds no key envelope",
                        ));
                    }
                    if self.commit_seq == 0
                        && self.keys.ops.iter().any(|o| matches!(o, KeyOp::Remove(_)))
                    {
                        return Err(schema("delta(0) removes a key envelope"));
                    }
                    if self.keys.ops.len() > 2 * MAX_KEY_ENVELOPES {
                        return Err(schema("too many key operations in one commit"));
                    }
                    // Replayed from nothing, the operations must stay valid
                    // (an add of a duplicate, a remove of a stranger, an
                    // emptied set): judged here for the delta alone, with
                    // removals allowed to name envelopes of earlier commits.
                    let mut seen = BTreeSet::new();
                    for op in &self.keys.ops {
                        if let KeyOp::Add(id) = op {
                            if !seen.insert(*id) {
                                return Err(schema("a delta adds the same key envelope twice"));
                            }
                        }
                    }
                }
                ManifestKind::Snapshot => {
                    if !self.keys.ops.is_empty() {
                        return Err(schema("a snapshot manifest carries key operations"));
                    }
                    if self.keys.state.is_empty() || self.keys.state.len() > MAX_KEY_ENVELOPES {
                        return Err(schema(
                            "a snapshot's key state lists between 1 and 16 envelopes",
                        ));
                    }
                    if !self.keys.state.windows(2).all(|w| w[0] < w[1]) {
                        return Err(schema(
                            "key state is not strictly increasing by envelope ID",
                        ));
                    }
                }
            }
        } else {
            if !self.keys.ops.is_empty() || !self.keys.state.is_empty() {
                return Err(schema(
                    "key operations and key state belong to the Encrypted profile (schema 3)",
                ));
            }
            if self
                .provenance
                .as_ref()
                .is_some_and(|p| p.reason != RewriteReason::Collection)
            {
                return Err(schema(
                    "a re-encryption reason belongs to the Encrypted profile (schema 3)",
                ));
            }
        }
        // D11, shared with the binary envelope and the commit record.
        check_required_features(&self.required_features, KNOWN_REQUIRED_FEATURES, limits)?;
        Ok(())
    }
}

fn dep_key(d: &Dependency) -> (u8, [u8; 32]) {
    match d {
        Dependency::Dictionary(id) => (0, *id.as_bytes()),
        Dependency::KeyEnvelope(id) => (1, *id.as_bytes()),
    }
}

fn check_attributes(a: &Attributes) -> Result<()> {
    if let Some(p) = a.posix {
        if p.mode > 0o7777 {
            return Err(schema("POSIX mode has bits beyond permissions"));
        }
    }
    if let Some(w) = a.windows {
        if w & !WINDOWS_PROMISED_MASK != 0 {
            return Err(schema("Windows attributes outside the promised set"));
        }
    }
    if let Some(m) = a.mtime {
        if m.nanos >= 1_000_000_000 {
            return Err(schema("mtime nanoseconds out of range"));
        }
    }
    Ok(())
}

fn chunk_value(c: &ChunkEntry) -> Value {
    let r = &c.record;
    Value::Map(vec![
        (0, b32(r.id.as_bytes())),
        (
            1,
            Value::Uint(match r.encoding {
                Encoding::ZstdFrame => 0,
            }),
        ),
        (
            2,
            Value::Uint(match r.protection {
                Protection::None => 0,
                Protection::Aead => 1,
            }),
        ),
        (3, Value::Uint(r.stored_len)),
        (4, b32(r.stored_hash.as_bytes())),
        (5, Value::Uint(r.decoded_len)),
        (6, b32(r.content_hash.as_bytes())),
        (
            7,
            Value::Array(
                r.dependencies
                    .iter()
                    .map(|d| {
                        let (k, id) = dep_key(d);
                        Value::Array(vec![Value::Uint(u64::from(k)), Value::Bytes(id.to_vec())])
                    })
                    .collect(),
            ),
        ),
        (8, c.location.map_or(Value::Null, Value::Uint)),
    ])
}

fn retention_op_value(op: &RetentionOp) -> Value {
    match op {
        RetentionOp::Expire { seq } => Value::Array(vec![Value::Uint(0), Value::Uint(*seq)]),
        RetentionOp::Hold { label, seq } => Value::Array(vec![
            Value::Uint(1),
            Value::Bytes(label.clone()),
            Value::Uint(*seq),
        ]),
        RetentionOp::Release { label } => {
            Value::Array(vec![Value::Uint(2), Value::Bytes(label.clone())])
        }
    }
}

fn retention_state_value(r: &RetentionState) -> Value {
    Value::Map(vec![
        (
            0,
            Value::Array(r.expired.iter().map(|s| Value::Uint(*s)).collect()),
        ),
        (
            1,
            Value::Array(
                r.holds
                    .iter()
                    .map(|(l, s)| Value::Array(vec![Value::Bytes(l.clone()), Value::Uint(*s)]))
                    .collect(),
            ),
        ),
    ])
}

fn provenance_value(p: &Provenance) -> Value {
    let mut fields = vec![
        (0, b32(p.source_archive_id.as_bytes())),
        (
            1,
            Value::Array(
                p.commits
                    .iter()
                    .map(|(s, id)| Value::Array(vec![Value::Uint(*s), b32(id.as_bytes())]))
                    .collect(),
            ),
        ),
        (
            2,
            Value::Array(p.collected.iter().map(|s| Value::Uint(*s)).collect()),
        ),
    ];
    // Key 3 only when it says something: absent means collection, so one
    // provenance has one encoding.
    if p.reason == RewriteReason::Reencryption {
        fields.push((3, Value::Uint(1)));
    }
    Value::Map(fields)
}

fn decode_provenance(v: &Value, version: u64) -> Result<Provenance> {
    let mut f = Fields::of(v, "provenance")?;
    let source_archive_id = ArchiveId::from_bytes(f.req(0)?.bytes32("source archive id")?);
    let commits = f
        .req(1)?
        .array("source commits")?
        .iter()
        .map(|c| {
            let t = tuple(c, 2, "source commit")?;
            Ok((
                t[0].uint("source sequence")?,
                CommitId::from_bytes(t[1].bytes32("source commit id")?),
            ))
        })
        .collect::<Result<_>>()?;
    let collected = f
        .req(2)?
        .array("collected snapshots")?
        .iter()
        .map(|c| c.uint("collected sequence").map_err(MochiError::from))
        .collect::<Result<_>>()?;
    // Key 3 exists from schema 3 on, and only to say "re-encryption" (1);
    // 0 is the default and is never written, so it is not accepted either.
    let reason = match (version >= ENCRYPTED_SCHEMA_VERSION, f.opt(3)) {
        (_, None) => RewriteReason::Collection,
        (true, Some(v)) => match v.uint("rewrite reason")? {
            1 => RewriteReason::Reencryption,
            0 => return Err(schema("rewrite reason 0 is the default and must be absent")),
            n => return Err(schema(format!("unknown rewrite reason {n}"))),
        },
        (false, Some(_)) => return Err(schema("provenance key 3 needs manifest schema 3")),
    };
    f.finish()?;
    Ok(Provenance {
        source_archive_id,
        commits,
        collected,
        reason,
    })
}

fn decode_retention_op(v: &Value) -> Result<RetentionOp> {
    let a = v.array("retention operation")?;
    match a
        .first()
        .map(|k| k.uint("retention operation kind"))
        .transpose()?
    {
        Some(0) => {
            let t = tuple(v, 2, "expire")?;
            Ok(RetentionOp::Expire {
                seq: t[1].uint("expired sequence")?,
            })
        }
        Some(1) => {
            let t = tuple(v, 3, "hold")?;
            Ok(RetentionOp::Hold {
                label: t[1].bytes("hold label")?.to_vec(),
                seq: t[2].uint("held sequence")?,
            })
        }
        Some(2) => {
            let t = tuple(v, 2, "release")?;
            Ok(RetentionOp::Release {
                label: t[1].bytes("hold label")?.to_vec(),
            })
        }
        _ => Err(schema("unknown or missing retention operation kind")),
    }
}

fn decode_retention_state(v: &Value) -> Result<RetentionState> {
    let mut f = Fields::of(v, "retention state")?;
    let mut state = RetentionState::default();
    let mut last: Option<u64> = None;
    for e in f.req(0)?.array("expired")? {
        let s = e.uint("expired sequence")?;
        if last.is_some_and(|l| l >= s) {
            return Err(schema("expired sequences are not strictly increasing"));
        }
        last = Some(s);
        state.expired.insert(s);
    }
    let mut last: Option<Vec<u8>> = None;
    for h in f.req(1)?.array("holds")? {
        let t = tuple(h, 2, "hold")?;
        let label = t[0].bytes("hold label")?.to_vec();
        if last.as_ref().is_some_and(|l| *l >= label) {
            return Err(schema("holds are not sorted by label without duplicates"));
        }
        last = Some(label.clone());
        state.holds.insert(label, t[1].uint("held sequence")?);
    }
    f.finish()?;
    Ok(state)
}

pub(crate) fn int_value(v: i64) -> Value {
    if v >= 0 {
        Value::Uint(v as u64)
    } else {
        // -1 - n = v  ⇒  n = -1 - v, which is non-negative and fits u64.
        Value::Nint((-1 - i128::from(v)) as u64)
    }
}

fn version_value(v: &FileVersionEntry) -> Value {
    let kind = match v.version.kind {
        EntryKind::File => 0,
        EntryKind::Directory => 1,
    };
    let mut attrs = Vec::new();
    if let Some(p) = v.attributes.posix {
        attrs.push((
            0,
            Value::Array(vec![
                Value::Uint(u64::from(p.mode)),
                Value::Uint(u64::from(p.uid)),
                Value::Uint(u64::from(p.gid)),
            ]),
        ));
    }
    if let Some(w) = v.attributes.windows {
        attrs.push((1, Value::Uint(u64::from(w))));
    }
    if let Some(m) = v.attributes.mtime {
        attrs.push((
            2,
            Value::Array(vec![int_value(m.secs), Value::Uint(u64::from(m.nanos))]),
        ));
    }
    Value::Map(vec![
        (0, b32(v.version.id.as_bytes())),
        (1, Value::Uint(kind)),
        (2, Value::Uint(v.version.logical_len)),
        (
            3,
            v.version
                .content_hash
                .as_ref()
                .map_or(Value::Null, |h| b32(h.as_bytes())),
        ),
        (
            4,
            Value::Array(
                v.extents
                    .iter()
                    .map(|e| {
                        let (chunk, off) = match &e.source {
                            ExtentSource::Chunk {
                                chunk,
                                chunk_offset,
                            } => (b32(chunk.as_bytes()), *chunk_offset),
                            ExtentSource::Hole => (Value::Null, 0),
                        };
                        Value::Array(vec![
                            Value::Uint(e.logical_offset),
                            Value::Uint(e.length),
                            chunk,
                            Value::Uint(off),
                        ])
                    })
                    .collect(),
            ),
        ),
        (5, Value::Map(attrs)),
    ])
}

fn op_value(op: &NamespaceOp) -> Value {
    match op {
        NamespaceOp::Put { path, version } => Value::Array(vec![
            Value::Uint(0),
            Value::Bytes(path.as_stored().to_vec()),
            b32(version.as_bytes()),
        ]),
        NamespaceOp::Delete { path } => Value::Array(vec![
            Value::Uint(1),
            Value::Bytes(path.as_stored().to_vec()),
        ]),
    }
}

// ---- decode -----------------------------------------------------------------------

/// Exactly `n` elements, as a fixed-size tuple-array.
fn tuple<'a>(v: &'a Value, n: usize, what: &str) -> Result<&'a [Value]> {
    let a = v.array(what)?;
    if a.len() != n {
        return Err(schema(format!(
            "{what}: expected {n} elements, found {}",
            a.len()
        )));
    }
    Ok(a)
}

fn opt_b32(v: &Value, what: &str) -> Result<Option<[u8; 32]>> {
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(v.bytes32(what)?))
    }
}

fn decode_chunk(v: &Value) -> Result<ChunkEntry> {
    let mut f = Fields::of(v, "chunk")?;
    let id = ObjectId::from_bytes(f.req(0)?.bytes32("chunk id")?);
    let encoding = match f.req(1)?.uint("encoding")? {
        0 => Encoding::ZstdFrame,
        n => return Err(schema(format!("unknown chunk encoding {n}"))),
    };
    let protection = match f.req(2)?.uint("protection")? {
        0 => Protection::None,
        1 => Protection::Aead,
        n => return Err(schema(format!("unknown protection {n}"))),
    };
    let stored_len = f.req(3)?.uint("stored length")?;
    let stored_hash = StoredObjectHash::from_bytes(f.req(4)?.bytes32("stored hash")?);
    let decoded_len = f.req(5)?.uint("decoded length")?;
    let content_hash = ChunkContentHash::from_bytes(f.req(6)?.bytes32("content hash")?);
    let mut dependencies = Vec::new();
    for d in f.req(7)?.array("dependencies")? {
        let t = tuple(d, 2, "dependency")?;
        let id = ObjectId::from_bytes(t[1].bytes32("dependency id")?);
        dependencies.push(match t[0].uint("dependency kind")? {
            0 => Dependency::Dictionary(id),
            1 => Dependency::KeyEnvelope(id),
            n => return Err(schema(format!("unknown dependency kind {n}"))),
        });
    }
    let loc = f.req(8)?;
    let location = if loc.is_null() {
        None
    } else {
        Some(loc.uint("location")?)
    };
    f.finish()?;
    Ok(ChunkEntry {
        record: ObjectRecord {
            id,
            encoding,
            protection,
            stored_len,
            stored_hash,
            decoded_len,
            content_hash,
            dependencies,
        },
        location,
    })
}

pub(crate) fn decode_int(v: &Value, what: &str) -> Result<i64> {
    match v {
        Value::Uint(n) => i64::try_from(*n).map_err(|_| schema(format!("{what}: out of range"))),
        Value::Nint(n) => {
            let x = -1 - i128::from(*n);
            i64::try_from(x).map_err(|_| schema(format!("{what}: out of range")))
        }
        _ => Err(schema(format!("{what}: expected an integer"))),
    }
}

fn decode_version(v: &Value) -> Result<FileVersionEntry> {
    let mut f = Fields::of(v, "file version")?;
    let id = FileVersionId::from_bytes(f.req(0)?.bytes32("file version id")?);
    let kind = match f.req(1)?.uint("kind")? {
        0 => EntryKind::File,
        1 => EntryKind::Directory,
        2 => {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "symbolic-link entries arrive with C6 (spec D6)",
            ))
        }
        n => return Err(schema(format!("unknown entry kind {n}"))),
    };
    let logical_len = f.req(2)?.uint("logical length")?;
    let content_hash = opt_b32(f.req(3)?, "content hash")?.map(FileContentHash::from_bytes);
    if (kind == EntryKind::Directory) != content_hash.is_none() {
        return Err(schema("content hash must be null exactly for a directory"));
    }
    let mut extents = Vec::new();
    for (i, e) in f.req(4)?.array("extents")?.iter().enumerate() {
        let t = tuple(e, 4, "extent")?;
        let chunk = opt_b32(&t[2], "extent chunk")?;
        let chunk_offset = t[3].uint("chunk offset")?;
        let source = match chunk {
            Some(c) => ExtentSource::Chunk {
                chunk: ObjectId::from_bytes(c),
                chunk_offset,
            },
            None if chunk_offset == 0 => ExtentSource::Hole,
            None => return Err(schema("a hole has a nonzero chunk offset")),
        };
        extents.push(Extent {
            ordinal: u32::try_from(i).map_err(|_| schema("too many extents"))?,
            logical_offset: t[0].uint("logical offset")?,
            length: t[1].uint("extent length")?,
            source,
        });
    }
    let mut a = Fields::of(f.req(5)?, "attributes")?;
    let posix = match a.opt(0) {
        None => None,
        Some(p) => {
            let t = tuple(p, 3, "posix attributes")?;
            Some(PosixAttributes {
                mode: t[0].u32("mode")?,
                uid: t[1].u32("uid")?,
                gid: t[2].u32("gid")?,
            })
        }
    };
    let windows = a.opt(1).map(|w| w.u32("windows attributes")).transpose()?;
    let mtime = match a.opt(2) {
        None => None,
        Some(m) => {
            let t = tuple(m, 2, "mtime")?;
            Some(Mtime {
                secs: decode_int(&t[0], "mtime seconds")?,
                nanos: t[1].u32("mtime nanoseconds")?,
            })
        }
    };
    a.finish()?;
    f.finish()?;
    Ok(FileVersionEntry {
        version: FileVersion {
            id,
            kind,
            logical_len,
            content_hash,
        },
        extents,
        attributes: Attributes {
            posix,
            windows,
            mtime,
        },
    })
}

fn decode_op(v: &Value) -> Result<NamespaceOp> {
    let a = v.array("operation")?;
    match a.first().map(|k| k.uint("operation kind")).transpose()? {
        Some(0) => {
            let t = tuple(v, 3, "put")?;
            Ok(NamespaceOp::Put {
                path: ArchivePath::from_stored(t[1].bytes("path")?)?,
                version: FileVersionId::from_bytes(t[2].bytes32("file version id")?),
            })
        }
        Some(1) => {
            let t = tuple(v, 2, "delete")?;
            Ok(NamespaceOp::Delete {
                path: ArchivePath::from_stored(t[1].bytes("path")?)?,
            })
        }
        _ => Err(schema("unknown or missing operation kind")),
    }
}

impl Manifest {
    /// Decode canonical CBOR bytes, enforcing the closed schema and every
    /// structural rule.
    pub fn decode(bytes: &[u8], limits: &Limits, cbor_limits: &CborLimits) -> Result<Manifest> {
        let root = cbor::decode(bytes, cbor_limits)?;
        let mut f = Fields::of(&root, "manifest")?;
        let version = f.req(0)?.uint("schema version")?;
        match version {
            SCHEMA_VERSION | RETENTION_SCHEMA_VERSION | ENCRYPTED_SCHEMA_VERSION => {}
            LEGACY_SCHEMA_VERSION => {
                return Err(MochiError::new(
                    ErrorCode::UnsupportedFeature,
                    "recovery-manifest schema 0 is the pre-batch draft (legacy, spec §26); \
                     this build reads schemas 1, 2, and 3",
                ))
            }
            version => {
                return Err(MochiError::new(
                    ErrorCode::UnsupportedFeature,
                    format!(
                        "recovery-manifest schema version {version} is not supported by this build"
                    ),
                ))
            }
        }
        let archive_id = ArchiveId::from_bytes(f.req(1)?.bytes32("archive id")?);
        let commit_seq = f.req(2)?.uint("commit sequence")?;
        let parent = {
            let p = f.req(3)?;
            if p.is_null() {
                None
            } else {
                let mut pf = Fields::of(p, "parent")?;
                let seq = pf.req(0)?.uint("parent sequence")?;
                let delta_manifest_hash =
                    StoredObjectHash::from_bytes(pf.req(1)?.bytes32("parent delta-manifest hash")?);
                pf.finish()?;
                Some(ParentLink {
                    seq,
                    delta_manifest_hash,
                })
            }
        };
        let kind = match f.req(4)?.uint("kind")? {
            0 => ManifestKind::Delta,
            1 => ManifestKind::Snapshot,
            n => return Err(schema(format!("unknown manifest kind {n}"))),
        };
        let chunks = f
            .req(5)?
            .array("chunks")?
            .iter()
            .map(decode_chunk)
            .collect::<Result<_>>()?;
        let file_versions = f
            .req(6)?
            .array("file versions")?
            .iter()
            .map(decode_version)
            .collect::<Result<_>>()?;
        let ops = f
            .req(7)?
            .array("operations")?
            .iter()
            .map(decode_op)
            .collect::<Result<_>>()?;
        let mut entries = Vec::new();
        for e in f.req(8)?.array("entries")? {
            let t = tuple(e, 2, "entry")?;
            entries.push((
                ArchivePath::from_stored(t[0].bytes("path")?)?,
                FileVersionId::from_bytes(t[1].bytes32("file version id")?),
            ));
        }
        let required_features = f
            .req(9)?
            .array("required features")?
            .iter()
            .map(|v| v.uint("required feature"))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        let transaction_id = <[u8; 16]>::try_from(f.req(10)?.bytes("transaction id")?)
            .map_err(|_| schema("transaction id: expected exactly 16 bytes"))?;
        let mut retention_ops = Vec::new();
        let mut retention = RetentionState::default();
        let mut provenance = None;
        let mut keys = ManifestKeys::default();
        if version >= RETENTION_SCHEMA_VERSION {
            let v = f.req(11)?;
            match kind {
                ManifestKind::Delta => {
                    retention_ops = v
                        .array("retention operations")?
                        .iter()
                        .map(decode_retention_op)
                        .collect::<Result<_>>()?;
                }
                ManifestKind::Snapshot => retention = decode_retention_state(v)?,
            }
            provenance = f
                .opt(12)
                .map(|v| decode_provenance(v, version))
                .transpose()?;
        }
        if version == ENCRYPTED_SCHEMA_VERSION {
            let v = f.req(13)?;
            match kind {
                ManifestKind::Delta => {
                    for op in v.array("key operations")? {
                        let t = tuple(op, 2, "key operation")?;
                        let id = <[u8; 16]>::try_from(t[1].bytes("envelope id")?)
                            .map_err(|_| schema("envelope id: expected exactly 16 bytes"))?;
                        keys.ops.push(match t[0].uint("key operation kind")? {
                            0 => KeyOp::Add(id),
                            1 => KeyOp::Remove(id),
                            n => return Err(schema(format!("unknown key operation kind {n}"))),
                        });
                    }
                }
                ManifestKind::Snapshot => {
                    for id in v.array("key state")? {
                        keys.state.push(
                            <[u8; 16]>::try_from(id.bytes("envelope id")?)
                                .map_err(|_| schema("envelope id: expected exactly 16 bytes"))?,
                        );
                    }
                }
            }
        }
        f.finish()?;
        let m = Manifest {
            archive_id,
            commit_seq,
            transaction_id,
            parent,
            kind,
            chunks,
            file_versions,
            ops,
            entries,
            required_features,
            retention_ops,
            retention,
            provenance,
            keys,
        };
        if m.schema_version() != version {
            return Err(schema(
                "the manifest's schema version does not match its content: schema 3 exactly \
                 when it lists the Encrypted required feature, schema 2 exactly when it \
                 carries retention data or provenance, schema 1 otherwise",
            ));
        }
        m.check_structure(limits)?;
        Ok(m)
    }

    /// Parse a stored manifest object: exactly one skippable frame of kind
    /// `RecoveryManifest`, whose payload decodes. Returns the manifest and its
    /// hash (the stored-object hash of the frame).
    pub fn from_stored(
        stored: &StoredObject,
        limits: &Limits,
        cbor_limits: &CborLimits,
    ) -> Result<(Manifest, StoredObjectHash)> {
        let bytes = stored.as_bytes();
        let span = walk_frame(bytes, 0, limits)?;
        if span.kind != FrameKind::RecoveryManifest || span.len != bytes.len() as u64 {
            return Err(MochiError::new(
                ErrorCode::MalformedFrame,
                "not exactly one recovery-manifest frame",
            ));
        }
        let FrameDetail::Skippable { payload_len } = span.detail else {
            return Err(MochiError::new(
                ErrorCode::MalformedFrame,
                "recovery manifest is not a skippable frame",
            ));
        };
        let start = SKIPPABLE_HEADER_LEN;
        let len = usize::try_from(payload_len).map_err(|_| schema("length"))?;
        let payload = bytes
            .get(start..start + len)
            .ok_or_else(|| schema("payload out of range"))?;
        let manifest = Manifest::decode(payload, limits, cbor_limits)?;
        Ok((manifest, stored_object_hash(stored.view())))
    }

    /// A self-contained snapshot manifest of `catalog` at commit `seq`, for
    /// the checkpoint commit with `transaction_id`.
    ///
    /// The catalog does not store promised attributes until C6, so the
    /// caller supplies them for every version the snapshot reaches. A missing
    /// entry is an error, not "not recorded": a snapshot covers all
    /// authoritative state (D10.3), and silently dropping attributes would
    /// make baseline recovery disagree with delta replay.
    pub fn snapshot_from_catalog(
        catalog: &Catalog,
        archive_id: ArchiveId,
        seq: u64,
        transaction_id: [u8; 16],
        attributes: &std::collections::BTreeMap<FileVersionId, Attributes>,
    ) -> Result<Manifest> {
        let snapshot = catalog.replay(Some(seq))?;
        let mut versions = std::collections::BTreeMap::new();
        let mut chunk_ids = BTreeSet::new();
        let mut entries = Vec::new();
        for (path, entry) in snapshot.iter() {
            entries.push((path.clone(), entry.version));
            if versions.contains_key(&entry.version) {
                continue;
            }
            let (version, extents) = catalog.file_version(&entry.version)?.ok_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "snapshot names an unknown version",
                )
            })?;
            for e in &extents {
                if let ExtentSource::Chunk { chunk, .. } = e.source {
                    chunk_ids.insert(chunk);
                }
            }
            let attributes = *attributes.get(&entry.version).ok_or_else(|| {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "no promised attributes are known for a version the snapshot reaches",
                )
            })?;
            versions.insert(
                entry.version,
                FileVersionEntry {
                    version,
                    extents,
                    attributes,
                },
            );
        }
        let mut chunks = Vec::new();
        for id in chunk_ids {
            let record = catalog.object(&id)?.ok_or_else(|| {
                MochiError::new(ErrorCode::CatalogInvalid, "extent names an unknown chunk")
            })?;
            chunks.push(ChunkEntry {
                location: catalog.object_location(&id)?,
                record,
            });
        }
        let mut m = Manifest {
            archive_id,
            commit_seq: seq,
            transaction_id,
            parent: None,
            kind: ManifestKind::Snapshot,
            chunks,
            file_versions: versions.into_values().collect(),
            ops: Vec::new(),
            entries,
            required_features: Vec::new(),
            retention_ops: Vec::new(),
            retention: Default::default(),
            provenance: None,
            keys: Default::default(),
        };
        m.canonicalize();
        m.check_structure(&Limits::WRITER_DEFAULT)?;
        Ok(m)
    }
}
