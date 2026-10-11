//! Commit records, schema version 1 (spec §12.1, Annex B.2 D10–D12; schema
//! `docs/schemas/commit-record-v1.cddl`). **DRAFT** (R3).
//!
//! A commit names everything its state depends on by offset, length, and
//! stored-object hash: the archive descriptor (key 10), its delta manifest
//! (key 6), and either a checkpoint (catalog image + snapshot manifest) or a
//! delta-on-base reference to the checkpoint it replays from (key 5). The
//! footer (§8.4) names the commit frame and digests its stored bytes, so a
//! footer that validates authenticates the chain footer → commit → each
//! referenced object *as published in this file*. It does not authenticate
//! the file itself: every digest here is unkeyed, so a whole-file rewrite is
//! detectable only against an external anchor (freshness, spec §5.7, D8).
//!
//! The **commit ID** (§9.2) is the domain-separated BLAKE3 of the canonical
//! body: the record's map without key 9, which carries the ID. Encoding is the
//! deterministic CBOR subset (spec D2), so the body has exactly one encoding.
//! Decoding recomputes the ID and refuses a record whose stored ID differs.
//!
//! # What the codec checks, and what it cannot
//!
//! Everything the record states about itself: the closed schema, sequence 0
//! ⇔ no parent ⇔ (necessarily) a checkpoint, parent sequence = this − 1, a
//! base strictly before this commit, the descriptor at offset 0, reference
//! lengths of at least a frame header, and the D11 required-features rule
//! (shared with the binary envelope, so the same fault gives the same code).
//!
//! It **cannot** check the base rule (D10.6: base = parent if the parent is a
//! checkpoint, else the parent's base), because that needs the parent's
//! record; that is T11. Nor can it check that references end before this
//! frame (needs the frame offset; `publish::load_verified` does).
//!
//! # Legacy
//!
//! Schema version 0 is the pre-batch draft (`commit-record-v0.cddl`). It is
//! refused as `UNSUPPORTED_FEATURE` with a message naming it as legacy (§26:
//! identify legacy formats explicitly), the same classification the
//! descriptor uses for other drafts (checklist question 3).

use mochi_format::cbor::{self, CborLimits, Fields, Value};
use mochi_format::digest::{commit_id, CommitId, StoredObjectHash};
use mochi_format::envelope::{check_required_features, RecordIdentity};
use mochi_format::error::LimitKind;
use mochi_format::frame::{encode_skippable_frame_within, walk_frame, FrameDetail};
use mochi_format::registry::{FrameKind, DESCRIPTOR_OFFSET, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::{CanonicalCommitBody, StoredObject};
use mochi_format::seal::{FEATURE_ENCRYPTED, MIN_SEALED_FRAME_LEN};
use mochi_format::{FormatError, Limits};

use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{decode_int, int_value, schema, Mtime};
use crate::object::ArchiveId;

/// Schema version this build writes and reads (R3 draft).
pub const SCHEMA_VERSION: u64 = 1;

/// Schema version of an Encrypted-profile archive's commits (Annex B.2.10
/// D20; `docs/schemas/commit-record-v2.cddl`): schema 1 without key 8, plus
/// key 11 (the key envelopes valid at this commit) and key 12 (the data
/// region). Used exactly when the record lists required feature 1.
pub const ENCRYPTED_SCHEMA_VERSION: u64 = 2;

/// The pre-batch draft schema. Refused, and named as legacy (§26).
pub const LEGACY_SCHEMA_VERSION: u64 = 0;

/// Map key holding the commit ID; excluded from the body (§9.2).
const ID_KEY: u64 = 9;

/// Required-feature identifiers this build understands: the Encrypted
/// profile's (D20 item 4). Any other listed feature makes the commit
/// unsupported (fail closed).
pub const KNOWN_REQUIRED_FEATURES: &[u64] = &[FEATURE_ENCRYPTED];

/// Where a stored object sits in a monolithic archive, and its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    pub offset: u64,
    pub stored_len: u64,
    pub stored_hash: StoredObjectHash,
}

impl ObjectRef {
    /// End offset, overflow-checked (archive-derived values).
    pub fn end(&self) -> Result<u64> {
        self.offset.checked_add(self.stored_len).ok_or_else(|| {
            MochiError::new(ErrorCode::OutOfBounds, "object reference range overflows")
        })
    }
}

/// A link to another commit: its ID, its sequence, and where its footer is.
/// Used for the parent (key 4) and the base (key 5.1), with different roles
/// for the offset (Annex B.2 D10.6, amended 2026-10-03):
///
/// * parent: the **traversal offset**. It must resolve to a valid footer
///   (else `FOOTER_INVALID`) whose commit has the declared ID and sequence
///   (else `RECORD_INVALID`); footer first, ID second; never searched for.
/// * base: a **non-authoritative hint**. The validated ancestry establishes
///   the base; a mismatching offset is reported, not an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitLink {
    pub commit_id: CommitId,
    pub seq: u64,
    pub footer_offset: u64,
}

/// The parent link (key 4).
pub type CommitParent = CommitLink;
/// The base-checkpoint link of a delta commit (key 5, form 1).
pub type BaseRef = CommitLink;

/// Key 5: exactly one of the two forms (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metadata {
    /// Form 0. The commit binds a complete catalog image (METADATA_DELTA
    /// frame) and a complete snapshot manifest (kind 1), D10.2.
    Checkpoint {
        image: ObjectRef,
        snapshot: ObjectRef,
    },
    /// Form 1. State = the base checkpoint plus the delta manifests after it
    /// (D10.4). The base must follow the base rule (D10.6), checked by T11.
    Delta { base: BaseRef },
}

impl Metadata {
    pub fn is_checkpoint(&self) -> bool {
        matches!(self, Metadata::Checkpoint { .. })
    }
}

/// A commit record. The commit ID is computed, never a stored field of this
/// type, so the two cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub archive_id: ArchiveId,
    pub seq: u64,
    pub transaction_id: [u8; 16],
    pub parent: Option<CommitParent>,
    pub metadata: Metadata,
    /// Key 6: this commit's delta manifest (kind 0). Always present (§11).
    pub delta_manifest: ObjectRef,
    /// Strictly increasing. Empty for Core.
    pub required_features: Vec<u64>,
    /// Informational only (§12.1): never used for ordering. Always `None`
    /// in the Encrypted profile (schema 2 has no key 8).
    pub time: Option<Mtime>,
    /// Key 10: the archive descriptor (D12). Offset is always 0.
    pub descriptor: ObjectRef,
    /// Schema 2, key 11: the key envelopes valid at this commit, a complete
    /// list in increasing order of the envelope ID each frame carries (the
    /// frames' own IDs are checked when they are read, `keys.rs`). Empty
    /// outside the Encrypted profile.
    pub key_envelopes: Vec<ObjectRef>,
    /// Schema 2, key 12: the byte range holding this commit's new data
    /// objects, back to back, and its stored-object-scope hash (D20 item 7);
    /// `None` when the commit adds no data object. Always `None` outside the
    /// Encrypted profile.
    pub data_region: Option<ObjectRef>,
}

/// Format 16 random bytes as an RFC 9562 version-4 UUID.
pub fn uuid_v4(mut bytes: [u8; 16]) -> [u8; 16] {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
}

fn b32(b: &[u8; 32]) -> Value {
    Value::Bytes(b.to_vec())
}

fn ref_value(r: &ObjectRef) -> Value {
    Value::Map(vec![
        (0, Value::Uint(r.offset)),
        (1, Value::Uint(r.stored_len)),
        (2, b32(r.stored_hash.as_bytes())),
    ])
}

fn link_value(l: &CommitLink) -> Value {
    Value::Map(vec![
        (0, b32(l.commit_id.as_bytes())),
        (1, Value::Uint(l.seq)),
        (2, Value::Uint(l.footer_offset)),
    ])
}

fn decode_ref(v: &Value, what: &'static str) -> Result<ObjectRef> {
    let mut f = Fields::of(v, what)?;
    let r = ObjectRef {
        offset: f.req(0)?.uint("offset")?,
        stored_len: f.req(1)?.uint("stored length")?,
        stored_hash: StoredObjectHash::from_bytes(f.req(2)?.bytes32("stored hash")?),
    };
    f.finish()?;
    Ok(r)
}

fn decode_link(v: &Value, what: &'static str) -> Result<CommitLink> {
    let mut f = Fields::of(v, what)?;
    let l = CommitLink {
        commit_id: CommitId::from_bytes(f.req(0)?.bytes32("commit id")?),
        seq: f.req(1)?.uint("sequence")?,
        footer_offset: f.req(2)?.uint("footer offset")?,
    };
    f.finish()?;
    Ok(l)
}

impl CommitRecord {
    /// 2 for an Encrypted-profile commit (it lists the D20 required feature),
    /// else 1.
    pub fn schema_version(&self) -> u64 {
        if self.encrypted() {
            ENCRYPTED_SCHEMA_VERSION
        } else {
            SCHEMA_VERSION
        }
    }

    /// Whether this commit belongs to an Encrypted-profile archive.
    pub fn encrypted(&self) -> bool {
        self.required_features.contains(&FEATURE_ENCRYPTED)
    }

    /// The D11 identity every object this commit references must carry.
    pub fn identity(&self) -> RecordIdentity {
        RecordIdentity {
            archive_id: *self.archive_id.as_bytes(),
            commit_sequence: self.seq,
            transaction_id: self.transaction_id,
        }
    }

    /// Every object reference, with a name for messages.
    pub fn object_refs(&self) -> Vec<(&'static str, ObjectRef)> {
        let mut v = vec![
            ("archive descriptor", self.descriptor),
            ("delta manifest", self.delta_manifest),
        ];
        if let Metadata::Checkpoint { image, snapshot } = self.metadata {
            v.push(("catalog image", image));
            v.push(("snapshot manifest", snapshot));
        }
        for e in &self.key_envelopes {
            v.push(("key envelope", *e));
        }
        if let Some(r) = self.data_region {
            v.push(("data region", r));
        }
        v
    }

    /// Rules every valid record satisfies on its own, checked on encode
    /// (against the writer defaults) and decode (against the reader's limits).
    fn check_structure(&self, limits: &Limits) -> Result<()> {
        match &self.parent {
            None if self.seq != 0 => return Err(schema("only commit 0 has no parent")),
            Some(_) if self.seq == 0 => return Err(schema("commit 0 has a parent")),
            Some(p) if p.seq.checked_add(1) != Some(self.seq) => {
                return Err(schema(format!(
                    "parent sequence {} is not this sequence ({}) minus one",
                    p.seq, self.seq
                )));
            }
            _ => {}
        }
        match &self.metadata {
            Metadata::Delta { .. } if self.seq == 0 => {
                return Err(schema("commit 0 must be a checkpoint (D10.2)"));
            }
            Metadata::Delta { base } if base.seq >= self.seq => {
                return Err(schema("the base checkpoint is not before this commit"));
            }
            _ => {}
        }
        if self.descriptor.offset != DESCRIPTOR_OFFSET {
            return Err(schema(format!(
                "the archive descriptor is referenced at offset {}; only offset 0 is valid (D12)",
                self.descriptor.offset
            )));
        }
        for (what, r) in self.object_refs() {
            if r.stored_len < SKIPPABLE_HEADER_LEN as u64 {
                return Err(schema(format!(
                    "the {what} reference is shorter than a frame header"
                )));
            }
            r.end()?;
        }
        if let Some(t) = self.time {
            if t.nanos >= 1_000_000_000 {
                return Err(schema("time nanoseconds out of range"));
            }
        }
        // D11, shared with the binary envelope: count limit, strictly
        // increasing (ENVELOPE_INVALID), then every entry known.
        check_required_features(&self.required_features, KNOWN_REQUIRED_FEATURES, limits)?;
        if self.encrypted() {
            // Schema 2 (D20): exactly the one feature, no time, 1 to 16
            // envelopes, and a data region that is sealed frames before the
            // delta manifest.
            if self.required_features != [FEATURE_ENCRYPTED] {
                return Err(schema(
                    "an Encrypted commit lists exactly the Encrypted required feature",
                ));
            }
            if self.time.is_some() {
                return Err(schema(
                    "an Encrypted commit records no time (Annex B.2.10 D20 item 6)",
                ));
            }
            let n = self.key_envelopes.len() as u64;
            if n > limits.max_key_envelopes {
                return Err(MochiError::from(FormatError::LimitExceeded {
                    kind: LimitKind::KeyEnvelopes,
                    limit: limits.max_key_envelopes,
                    actual: n,
                }));
            }
            if n == 0 {
                return Err(schema("an Encrypted commit lists no key envelope"));
            }
            if let Some(r) = &self.data_region {
                if r.stored_len < MIN_SEALED_FRAME_LEN {
                    return Err(schema("the data region is shorter than one sealed frame"));
                }
                if r.end()? > self.delta_manifest.offset {
                    return Err(schema(
                        "the data region extends past the delta manifest it precedes",
                    ));
                }
            }
        } else if !self.key_envelopes.is_empty() || self.data_region.is_some() {
            return Err(schema(
                "key envelopes and a data region belong to the Encrypted profile (schema 2)",
            ));
        }
        Ok(())
    }

    fn body_entries(&self) -> Vec<(u64, Value)> {
        let parent = self.parent.as_ref().map_or(Value::Null, link_value);
        let metadata = match &self.metadata {
            Metadata::Checkpoint { image, snapshot } => Value::Map(vec![
                (0, Value::Uint(0)),
                (1, ref_value(image)),
                (2, ref_value(snapshot)),
            ]),
            Metadata::Delta { base } => {
                Value::Map(vec![(0, Value::Uint(1)), (1, link_value(base))])
            }
        };
        let mut entries = vec![
            (0, Value::Uint(self.schema_version())),
            (1, b32(self.archive_id.as_bytes())),
            (2, Value::Uint(self.seq)),
            (3, Value::Bytes(self.transaction_id.to_vec())),
            (4, parent),
            (5, metadata),
            (6, ref_value(&self.delta_manifest)),
            (
                7,
                Value::Array(
                    self.required_features
                        .iter()
                        .map(|f| Value::Uint(*f))
                        .collect(),
                ),
            ),
        ];
        if let Some(t) = self.time {
            entries.push((
                8,
                Value::Array(vec![int_value(t.secs), Value::Uint(u64::from(t.nanos))]),
            ));
        }
        // Key 9 (the ID) sits between 8 and 10 in the full record; the body
        // simply omits it.
        entries.push((10, ref_value(&self.descriptor)));
        if self.encrypted() {
            entries.push((
                11,
                Value::Array(self.key_envelopes.iter().map(ref_value).collect()),
            ));
            entries.push((12, self.data_region.as_ref().map_or(Value::Null, ref_value)));
        }
        entries
    }

    fn compute_id(&self) -> Result<CommitId> {
        let body = cbor::encode(&Value::Map(self.body_entries()))?;
        Ok(commit_id(CanonicalCommitBody::assume_canonical(&body)))
    }

    /// The commit ID: domain-separated BLAKE3 of the canonical body (§9.2).
    /// Refuses a record the default writer would not emit.
    pub fn commit_id(&self) -> Result<CommitId> {
        self.check_structure(&Limits::WRITER_DEFAULT)?;
        self.compute_id()
    }

    /// The full map (body plus ID, keys in canonical order), unchecked.
    fn full_value(&self, id: &CommitId) -> Value {
        let mut entries = self.body_entries();
        let at = entries
            .iter()
            .position(|(k, _)| *k > ID_KEY)
            .unwrap_or(entries.len());
        entries.insert(at, (ID_KEY, b32(id.as_bytes())));
        Value::Map(entries)
    }

    /// Canonical CBOR of the full record (body plus ID), and the ID.
    pub fn encode(&self) -> Result<(Vec<u8>, CommitId)> {
        let id = self.commit_id()?;
        // The stored payload is bounded by the reader defaults (B.2.3); the
        // body hashed for the ID above is a subset of it.
        Ok((
            cbor::encode_within(&self.full_value(&id), &CborLimits::default())?,
            id,
        ))
    }

    /// The stored form: one skippable frame of kind `CommitRecord`, within
    /// the default commit-frame limit (8 + 64 KiB, B.2.3) whatever limits the
    /// writer reads with.
    pub fn to_stored(&self) -> Result<(StoredObject, CommitId)> {
        let (payload, id) = self.encode()?;
        let frame = encode_skippable_frame_within(
            FrameKind::CommitRecord,
            &payload,
            &Limits::WRITER_DEFAULT,
        )?;
        if frame.len() as u64 > Limits::WRITER_DEFAULT.max_commit_frame_len {
            return Err(MochiError::new(
                ErrorCode::CapacityExceeded,
                "the commit record exceeds the default commit-frame limit (B.2.3)",
            ));
        }
        Ok((StoredObject::from_loaded(frame), id))
    }

    /// Decode canonical CBOR: closed schema, structural rules, supported
    /// features, and the stored ID equal to the recomputed one.
    pub fn decode(
        bytes: &[u8],
        limits: &Limits,
        cbor_limits: &CborLimits,
    ) -> Result<(CommitRecord, CommitId)> {
        let root = cbor::decode(bytes, cbor_limits)?;
        let mut f = Fields::of(&root, "commit")?;
        let version = match f.req(0)?.uint("schema version")? {
            v @ (SCHEMA_VERSION | ENCRYPTED_SCHEMA_VERSION) => v,
            LEGACY_SCHEMA_VERSION => {
                return Err(MochiError::new(
                    ErrorCode::UnsupportedFeature,
                    "commit-record schema 0 is the pre-batch draft (legacy, spec §26); \
                     this build reads schemas 1 and 2",
                ))
            }
            v => {
                return Err(MochiError::new(
                    ErrorCode::UnsupportedFeature,
                    format!("commit-record schema version {v} is not supported by this build"),
                ))
            }
        };
        let archive_id = ArchiveId::from_bytes(f.req(1)?.bytes32("archive id")?);
        let seq = f.req(2)?.uint("commit sequence")?;
        let transaction_id = <[u8; 16]>::try_from(f.req(3)?.bytes("transaction id")?)
            .map_err(|_| schema("transaction id: expected exactly 16 bytes"))?;
        let parent = {
            let p = f.req(4)?;
            if p.is_null() {
                None
            } else {
                Some(decode_link(p, "parent")?)
            }
        };
        let metadata = {
            let mut mf = Fields::of(f.req(5)?, "metadata")?;
            let m = match mf.req(0)?.uint("metadata form")? {
                0 => Metadata::Checkpoint {
                    image: decode_ref(mf.req(1)?, "catalog image reference")?,
                    snapshot: decode_ref(mf.req(2)?, "snapshot manifest reference")?,
                },
                1 => Metadata::Delta {
                    base: decode_link(mf.req(1)?, "base")?,
                },
                n => return Err(schema(format!("unknown metadata form {n}"))),
            };
            // A delta with checkpoint fields (or the reverse) is an unknown key.
            mf.finish()?;
            m
        };
        let delta_manifest = decode_ref(f.req(6)?, "delta manifest reference")?;
        let required_features = f
            .req(7)?
            .array("required features")?
            .iter()
            .map(|v| v.uint("required feature"))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        let time = match f.opt(8) {
            None => None,
            Some(t) => {
                let a = t.array("time")?;
                if a.len() != 2 {
                    return Err(schema("time: expected 2 elements"));
                }
                Some(Mtime {
                    secs: decode_int(&a[0], "time seconds")?,
                    nanos: a[1].u32("time nanoseconds")?,
                })
            }
        };
        let stored_id = CommitId::from_bytes(f.req(ID_KEY)?.bytes32("commit id")?);
        let descriptor = decode_ref(f.req(10)?, "archive descriptor reference")?;
        let (mut key_envelopes, mut data_region) = (Vec::new(), None);
        if version == ENCRYPTED_SCHEMA_VERSION {
            // Bound the work before decoding each reference: a hostile list
            // is cut at the reader's limit (the structural check repeats it
            // for records built in memory).
            let list = f.req(11)?.array("key envelopes")?;
            if list.len() as u64 > limits.max_key_envelopes {
                return Err(MochiError::from(FormatError::LimitExceeded {
                    kind: LimitKind::KeyEnvelopes,
                    limit: limits.max_key_envelopes,
                    actual: list.len() as u64,
                }));
            }
            for r in list {
                key_envelopes.push(decode_ref(r, "key envelope reference")?);
            }
            let region = f.req(12)?;
            if !region.is_null() {
                data_region = Some(decode_ref(region, "data region")?);
            }
        }
        f.finish()?;
        let record = CommitRecord {
            archive_id,
            seq,
            transaction_id,
            parent,
            metadata,
            delta_manifest,
            required_features,
            time,
            descriptor,
            key_envelopes,
            data_region,
        };
        // D11 first (count, increasing order, known), so a fault in the feature
        // list itself keeps its own code before the profile comparison below.
        check_required_features(&record.required_features, KNOWN_REQUIRED_FEATURES, limits)?;
        if record.schema_version() != version {
            return Err(schema(
                "the commit's schema version does not match its required features: \
                 schema 2 exactly when it lists the Encrypted required feature",
            ));
        }
        record.check_structure(limits)?;
        let id = record.compute_id()?;
        if id != stored_id {
            return Err(MochiError::new(
                ErrorCode::RecordInvalid,
                "commit ID does not match the canonical commit body (spec §9.2)",
            ));
        }
        Ok((record, id))
    }

    /// Parse a stored commit object: exactly one skippable frame of kind
    /// `CommitRecord` whose payload decodes.
    pub fn from_stored(
        bytes: &[u8],
        limits: &Limits,
        cbor_limits: &CborLimits,
    ) -> Result<(CommitRecord, CommitId)> {
        // Bound the work before walking: the footer path checks this too,
        // but a commit can also be reached by scanning (spec Annex B.2.3).
        let len = bytes.len() as u64;
        if len > limits.max_commit_frame_len {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::CommitFrameLength,
                limit: limits.max_commit_frame_len,
                actual: len,
            }
            .into());
        }
        let span = walk_frame(bytes, 0, limits)?;
        if span.kind != FrameKind::CommitRecord || span.len != bytes.len() as u64 {
            return Err(MochiError::new(
                ErrorCode::MalformedFrame,
                "not exactly one commit-record frame",
            ));
        }
        let FrameDetail::Skippable { .. } = span.detail else {
            return Err(MochiError::new(
                ErrorCode::MalformedFrame,
                "commit record is not a skippable frame",
            ));
        };
        let payload = bytes
            .get(SKIPPABLE_HEADER_LEN..)
            .ok_or_else(|| schema("commit payload out of range"))?;
        CommitRecord::decode(payload, limits, cbor_limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> StoredObjectHash {
        StoredObjectHash::from_bytes([n; 32])
    }

    fn r(offset: u64, stored_len: u64, n: u8) -> ObjectRef {
        ObjectRef {
            offset,
            stored_len,
            stored_hash: h(n),
        }
    }

    fn root() -> CommitRecord {
        CommitRecord {
            archive_id: ArchiveId::from_bytes([1; 32]),
            seq: 0,
            transaction_id: uuid_v4([2; 16]),
            parent: None,
            metadata: Metadata::Checkpoint {
                image: r(100, 4104, 3),
                snapshot: r(4204, 90, 5),
            },
            delta_manifest: r(56, 44, 4),
            required_features: vec![],
            time: Some(Mtime { secs: -5, nanos: 7 }),
            descriptor: r(0, 56, 6),
            key_envelopes: Vec::new(),
            data_region: None,
        }
    }

    fn child_delta() -> CommitRecord {
        let base = CommitLink {
            commit_id: root().commit_id().unwrap(),
            seq: 0,
            footer_offset: 4400,
        };
        CommitRecord {
            seq: 1,
            transaction_id: uuid_v4([8; 16]),
            parent: Some(base),
            metadata: Metadata::Delta { base },
            delta_manifest: r(4472, 60, 7),
            time: None,
            ..root()
        }
    }

    fn dec(bytes: &[u8]) -> Result<(CommitRecord, CommitId)> {
        CommitRecord::decode(bytes, &Limits::default(), &CborLimits::default())
    }

    #[test]
    fn round_trips_and_id_is_stable() {
        for rec in [root(), child_delta()] {
            let (bytes, id) = rec.encode().unwrap();
            let (back, id2) = dec(&bytes).unwrap();
            assert_eq!(back, rec);
            assert_eq!(id, id2);
            assert_eq!(id, rec.commit_id().unwrap());
        }
    }

    /// T8 acceptance: the ID is recomputed from the body **without key 9**,
    /// and key 10 (which follows key 9 in the record) is inside the body.
    #[test]
    fn id_is_blake3_of_the_map_without_key_9() {
        let rec = root();
        let (bytes, id) = rec.encode().unwrap();
        let Value::Map(mut m) = cbor::decode(&bytes, &CborLimits::default()).unwrap() else {
            panic!("a commit is a map")
        };
        let keys: Vec<u64> = m.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        m.retain(|(k, _)| *k != 9);
        let body = cbor::encode(&Value::Map(m)).unwrap();
        assert_eq!(commit_id(CanonicalCommitBody::assume_canonical(&body)), id);
    }

    #[test]
    fn id_covers_every_body_field_including_the_descriptor() {
        let base = root().commit_id().unwrap();
        type Edit = Box<dyn Fn(&mut CommitRecord)>;
        let edits: Vec<Edit> = vec![
            Box::new(|r| r.archive_id = ArchiveId::from_bytes([9; 32])),
            Box::new(|r| r.transaction_id[0] ^= 1),
            Box::new(|r| {
                if let Metadata::Checkpoint { image, .. } = &mut r.metadata {
                    image.offset += 1
                }
            }),
            Box::new(|r| {
                if let Metadata::Checkpoint { snapshot, .. } = &mut r.metadata {
                    snapshot.stored_hash = h(9)
                }
            }),
            Box::new(|r| r.delta_manifest.offset += 1),
            Box::new(|r| r.delta_manifest.stored_hash = h(9)),
            Box::new(|r| r.descriptor.stored_len += 1),
            Box::new(|r| r.descriptor.stored_hash = h(9)),
            Box::new(|r| r.time = None),
        ];
        for edit in edits {
            let mut r = root();
            edit(&mut r);
            assert_ne!(r.commit_id().unwrap(), base);
        }
    }

    #[test]
    fn a_wrong_stored_id_is_refused() {
        let (bytes, id) = root().encode().unwrap();
        // Key 9's value is the 32 bytes before key 10 (`0x0A`, map(3) …).
        let at = bytes.windows(32).position(|w| w == id.as_bytes()).unwrap();
        let mut bad = bytes.clone();
        bad[at + 31] ^= 1;
        assert_eq!(dec(&bad).unwrap_err().code, ErrorCode::RecordInvalid);
    }

    #[test]
    fn structure_rules() {
        type Case = (Box<dyn Fn(&mut CommitRecord)>, ErrorCode);
        let cases: Vec<Case> = vec![
            // An orphan.
            (Box::new(|r| r.seq = 1), ErrorCode::RecordInvalid),
            // Commit 0 with a parent.
            (
                Box::new(|r| {
                    r.parent = Some(CommitLink {
                        commit_id: CommitId::from_bytes([0; 32]),
                        seq: 0,
                        footer_offset: 0,
                    })
                }),
                ErrorCode::RecordInvalid,
            ),
            (
                Box::new(|r| r.delta_manifest.stored_len = 7),
                ErrorCode::RecordInvalid,
            ),
            (
                Box::new(|r| r.descriptor.stored_len = 7),
                ErrorCode::RecordInvalid,
            ),
            (
                Box::new(|r| r.delta_manifest.offset = u64::MAX),
                ErrorCode::OutOfBounds,
            ),
            (
                Box::new(|r| r.descriptor.offset = 8),
                ErrorCode::RecordInvalid,
            ),
            (
                Box::new(|r| r.required_features = vec![2, 1]),
                ErrorCode::EnvelopeInvalid,
            ),
            // Feature 1 is the Encrypted profile (D20); 2 is not assigned.
            (
                Box::new(|r| r.required_features = vec![2]),
                ErrorCode::UnsupportedFeature,
            ),
        ];
        for (i, (edit, code)) in cases.into_iter().enumerate() {
            let mut r = root();
            edit(&mut r);
            assert_eq!(r.commit_id().unwrap_err().code, code, "case {i}");
        }
    }

    #[test]
    fn delta_rules() {
        // Commit 0 must be a checkpoint.
        let mut r = root();
        r.metadata = Metadata::Delta {
            base: CommitLink {
                commit_id: CommitId::from_bytes([0; 32]),
                seq: 0,
                footer_offset: 0,
            },
        };
        assert_eq!(r.commit_id().unwrap_err().code, ErrorCode::RecordInvalid);
        // Base not before this commit.
        let mut r = child_delta();
        if let Metadata::Delta { base } = &mut r.metadata {
            base.seq = 1;
        }
        assert_eq!(r.commit_id().unwrap_err().code, ErrorCode::RecordInvalid);
        // Parent sequence must be exactly this − 1 (v0 allowed any earlier).
        let mut r = child_delta();
        r.seq = 2;
        if let Metadata::Delta { base } = &mut r.metadata {
            base.seq = 0;
        }
        assert_eq!(r.commit_id().unwrap_err().code, ErrorCode::RecordInvalid);
    }

    /// B.2.3: a commit frame over 8 + 64 KiB is refused by the decoder itself,
    /// not only on the footer path. Tested at value − 1, value, value + 1.
    #[test]
    fn commit_frame_limit_is_enforced_on_decode() {
        let (frame, _) = root().to_stored().unwrap();
        let frame = frame.as_bytes().to_vec();
        let n = frame.len() as u64;
        for (limit, ok) in [(n + 1, true), (n, true), (n - 1, false)] {
            let limits = Limits {
                max_commit_frame_len: limit,
                ..Limits::default()
            };
            let r = CommitRecord::from_stored(&frame, &limits, &CborLimits::default());
            assert_eq!(r.is_ok(), ok, "limit {limit}");
            if !ok {
                assert_eq!(r.unwrap_err().code, ErrorCode::LimitExceeded);
            }
        }
    }

    /// B.2.3 claims "a v1 commit is a few hundred bytes plus at most 64
    /// features". Check it at the worst case the schema allows within the
    /// default limits: every integer at its widest, both metadata forms, the
    /// time present, and 64 nine-byte features. The writer default frame
    /// limit is 8 + 64 KiB; the worst case must be far below it, which is
    /// why the writer's CAPACITY_EXCEEDED for commits is unreachable today.
    #[test]
    fn worst_case_v1_commit_is_far_below_the_commit_frame_limit() {
        let wide = |n| ObjectRef {
            offset: u64::MAX / 4,
            stored_len: u64::MAX / 4,
            stored_hash: h(n),
        };
        let link = CommitLink {
            commit_id: CommitId::from_bytes([0xEE; 32]),
            seq: u64::MAX - 2,
            footer_offset: u64::MAX,
        };
        let mut worst = 0;
        for metadata in [
            Metadata::Checkpoint {
                image: wide(1),
                snapshot: wide(2),
            },
            Metadata::Delta { base: link },
        ] {
            let rec = CommitRecord {
                seq: u64::MAX - 1,
                parent: Some(link),
                metadata,
                delta_manifest: wide(3),
                required_features: (0..64).map(|i| (1u64 << 63) + i).collect(),
                time: Some(Mtime {
                    secs: i64::MIN,
                    nanos: 999_999_999,
                }),
                descriptor: ObjectRef {
                    offset: 0,
                    stored_len: u64::MAX / 4,
                    stored_hash: h(4),
                },
                ..root()
            };
            // Features are unknown to this build, so measure the unchecked
            // encoding (the size is what matters here).
            let id = rec.compute_id().unwrap();
            let len = cbor::encode(&rec.full_value(&id)).unwrap().len() + 8;
            worst = worst.max(len);
        }
        assert!(worst < 1200, "worst-case v1 commit is {worst} bytes");
        assert!((worst as u64) < Limits::WRITER_DEFAULT.max_commit_frame_len / 50);
    }

    #[test]
    fn unknown_required_features_fail_closed() {
        let rec = root();
        let id = rec.compute_id().unwrap();
        let Value::Map(mut m) = rec.full_value(&id) else {
            unreachable!()
        };
        m[7].1 = Value::Array(vec![Value::Uint(2)]);
        let bytes = cbor::encode(&Value::Map(m)).unwrap();
        assert_eq!(dec(&bytes).unwrap_err().code, ErrorCode::UnsupportedFeature);
    }

    #[test]
    fn legacy_and_future_schema_versions_are_refused() {
        let (bytes, _) = root().encode().unwrap();
        for (v, legacy) in [(0u8, true), (3, false)] {
            let mut b = bytes.clone();
            // map(11), key 0, value 1 → value v.
            assert_eq!(&b[..3], &[0xAB, 0x00, 0x01]);
            b[2] = v;
            let e = dec(&b).unwrap_err();
            assert_eq!(e.code, ErrorCode::UnsupportedFeature);
            assert_eq!(e.message.contains("legacy"), legacy, "{}", e.message);
        }
    }

    #[test]
    fn uuid_bits() {
        let u = uuid_v4([0xff; 16]);
        assert_eq!(u[6] >> 4, 4);
        assert_eq!(u[8] >> 6, 0b10);
    }
}
