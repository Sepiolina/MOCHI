//! Forging commits on top of a real archive, for T11/T12 negative tests and
//! (later) golden reject vectors.
//!
//! Everything here appends: the bytes the real writer produced stay
//! untouched, and each forged commit is a well-formed frame sequence
//! (objects, commit record, footer) so that `locate_head` accepts it as the
//! head. What makes a forged archive *invalid* is only the field a test
//! changes, which keeps each reject case aimed at one rule.
//!
//! Test infrastructure: unwraps freely.

use mochi_core::commit::{CommitLink, CommitRecord, Metadata, ObjectRef};
use mochi_core::manifest::{Manifest, ManifestKind, ParentLink};
use mochi_core::publish::{commit_history, HistoryEntry, ReadOptions};
use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::digest::stored_object_hash;
use mochi_format::footer::{encode_footer_frame, FOOTER_FRAME_LEN};
use mochi_format::frame::encode_skippable_frame;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;

use crate::SimStorage;

/// An archive's bytes plus forged appends.
#[derive(Debug, Clone)]
pub struct Forge {
    pub bytes: Vec<u8>,
}

impl Forge {
    pub fn new(bytes: Vec<u8>) -> Self {
        Forge { bytes }
    }

    pub fn storage(&self) -> SimStorage {
        SimStorage::from_bytes(self.bytes.clone())
    }

    /// The published history, head last, as the reader's walk sees it.
    pub fn history(&self) -> Vec<HistoryEntry> {
        commit_history(&self.storage(), &ReadOptions::default()).unwrap()
    }

    /// Append one stored object and return a reference to it.
    pub fn append_object(&mut self, stored: &StoredObject) -> ObjectRef {
        let offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(stored.as_bytes());
        ObjectRef {
            offset,
            stored_len: stored.len(),
            stored_hash: stored_object_hash(stored.view()),
        }
    }

    /// Append a manifest through the real encoder (which refuses invalid
    /// structure, so use [`Forge::append_manifest_edited`] for those).
    pub fn append_manifest(&mut self, m: &Manifest) -> ObjectRef {
        self.append_object(&m.to_stored().unwrap())
    }

    /// Append a manifest whose canonical CBOR was edited after encoding:
    /// for faults the encoder refuses to write (unknown operation kind,
    /// unknown required feature). The result is still canonical CBOR in one
    /// recovery-manifest frame; only the edited field is wrong.
    pub fn append_manifest_edited(
        &mut self,
        m: &Manifest,
        edit: impl FnOnce(&mut Value),
    ) -> ObjectRef {
        let mut v = cbor::decode(&m.encode().unwrap(), &CborLimits::default()).unwrap();
        edit(&mut v);
        let payload = cbor::encode(&v).unwrap();
        let frame = encode_skippable_frame(FrameKind::RecoveryManifest, &payload).unwrap();
        self.append_object(&StoredObject::from_loaded(frame))
    }

    /// Append a commit record and its footer; return it as a history entry.
    pub fn append_commit(&mut self, record: &CommitRecord) -> HistoryEntry {
        let (frame, commit_id) = record.to_stored().unwrap();
        let commit_offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(frame.as_bytes());
        let footer_offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&encode_footer_frame(
            commit_offset,
            record.seq,
            frame.as_bytes(),
        ));
        assert_eq!(self.bytes.len() as u64, footer_offset + FOOTER_FRAME_LEN);
        HistoryEntry {
            footer_offset,
            commit_offset,
            commit: record.clone(),
            commit_id,
        }
    }

    /// Forge delta commit `prev.seq + 1` on `base` with manifest `m` (whose
    /// reference this appends first), parent = `prev`.
    pub fn append_delta(
        &mut self,
        prev: &HistoryEntry,
        base: CommitLink,
        m: ObjectRef,
        txid: [u8; 16],
    ) -> HistoryEntry {
        let record = delta_record(prev, base, m, txid);
        self.append_commit(&record)
    }
}

/// The link a child uses to name `e` (parent or base).
pub fn link(e: &HistoryEntry) -> CommitLink {
    CommitLink {
        commit_id: e.commit_id,
        seq: e.commit.seq,
        footer_offset: e.footer_offset,
    }
}

/// The base a correct delta after `prev` names (D10.6 base rule).
pub fn rule_base(prev: &HistoryEntry) -> CommitLink {
    match prev.commit.metadata {
        Metadata::Checkpoint { .. } => link(prev),
        Metadata::Delta { base } => base,
    }
}

/// A forged transaction ID, distinct per `n`.
pub fn txid(n: u8) -> [u8; 16] {
    mochi_core::commit::uuid_v4([n; 16])
}

/// A delta commit record following `prev`: same archive and descriptor.
pub fn delta_record(
    prev: &HistoryEntry,
    base: CommitLink,
    m: ObjectRef,
    txid: [u8; 16],
) -> CommitRecord {
    CommitRecord {
        seq: prev.commit.seq + 1,
        transaction_id: txid,
        parent: Some(link(prev)),
        metadata: Metadata::Delta { base },
        delta_manifest: m,
        required_features: Vec::new(),
        time: None,
        ..prev.commit.clone()
    }
}

/// An empty, correct delta manifest for commit `prev.seq + 1`: parent link
/// to `prev`'s key 6 (checklist Q22). Tests change one field.
pub fn empty_delta(prev: &HistoryEntry, txid: [u8; 16]) -> Manifest {
    Manifest {
        archive_id: prev.commit.archive_id,
        commit_seq: prev.commit.seq + 1,
        transaction_id: txid,
        parent: Some(ParentLink {
            seq: prev.commit.seq,
            delta_manifest_hash: prev.commit.delta_manifest.stored_hash,
        }),
        kind: ManifestKind::Delta,
        chunks: Vec::new(),
        file_versions: Vec::new(),
        ops: Vec::new(),
        entries: Vec::new(),
        required_features: Vec::new(),
        retention_ops: Vec::new(),
        retention: Default::default(),
        provenance: None,
    }
}

/// Mutable access to field `key` of a CBOR map.
pub fn field(v: &mut Value, key: u64) -> &mut Value {
    let Value::Map(entries) = v else {
        panic!("not a map")
    };
    &mut entries.iter_mut().find(|(k, _)| *k == key).unwrap().1
}
