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
    stored_object_hash, ChunkContentHash, FileContentHash, StoredObjectHash,
};
use mochi_format::envelope::{check_required_features, RecordIdentity};
use mochi_format::frame::{encode_skippable_frame_within, walk_frame, FrameDetail};
use mochi_format::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::{FormatError, Limits};

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, FileVersion};
use crate::error::{ErrorCode, MochiError, Result};
use crate::object::{ArchiveId, Dependency, ObjectId, ObjectRecord};

/// Schema version this build writes and reads (R3 draft).
pub const SCHEMA_VERSION: u64 = 1;

/// The pre-batch draft schema. Refused, and named as legacy (§26).
pub const LEGACY_SCHEMA_VERSION: u64 = 0;

/// Required features this build understands (key 9). None; fail closed.
pub const KNOWN_REQUIRED_FEATURES: &[u64] = &[];

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

    fn to_value_unchecked(&self) -> Value {
        let parent = match &self.parent {
            None => Value::Null,
            Some(p) => Value::Map(vec![
                (0, Value::Uint(p.seq)),
                (1, b32(p.delta_manifest_hash.as_bytes())),
            ]),
        };
        Value::Map(vec![
            (0, Value::Uint(SCHEMA_VERSION)),
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
        ])
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
        match f.req(0)?.uint("schema version")? {
            SCHEMA_VERSION => {}
            LEGACY_SCHEMA_VERSION => {
                return Err(MochiError::new(
                    ErrorCode::UnsupportedFeature,
                    "recovery-manifest schema 0 is the pre-batch draft (legacy, spec §26); \
                     this build reads schema 1 only",
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
        };
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
        };
        m.canonicalize();
        m.check_structure(&Limits::WRITER_DEFAULT)?;
        Ok(m)
    }
}
