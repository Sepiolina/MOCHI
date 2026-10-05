//! A minimal writer that lays out an archive-like byte stream: encoded chunk
//! objects followed by each commit's recovery manifest, all as back-to-back
//! frames. It keeps a working catalog alongside, as the real writer (C5) will,
//! so tests can destroy the catalog and check what manifests alone recover.
//!
//! This is test scaffolding, not the C5 writer: there is no footer, commit
//! record, or durability protocol here.
//!
//! Manifest schema 1: every commit writes its delta manifest, linked to the
//! previous *delta*. A "snapshot commit" writes a snapshot manifest as well
//! (parent null, same sequence and transaction ID), as a checkpoint commit
//! would; it is never on the delta chain.

use std::collections::BTreeMap;

use mochi_core::catalog::extent::{Extent, ExtentSource};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use mochi_core::catalog::{Catalog, Commit, FileVersion};
use mochi_core::manifest::{
    Attributes, ChunkEntry, FileVersionEntry, Manifest, ManifestKind, ParentLink,
};
use mochi_core::object::{build_object, ArchiveId, IdSource};
use mochi_core::Result;
use mochi_format::codec::{EncodeParams, Protection};
use mochi_format::digest::{file_content_hash, StoredObjectHash};
use mochi_format::repr::{DecodedBytes, DecodedSlice};
use mochi_format::Limits;

use crate::SeqIds;

/// One committed manifest: where it sits and its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrittenManifest {
    pub seq: u64,
    pub offset: u64,
    pub len: u64,
    pub hash: StoredObjectHash,
    pub kind: ManifestKind,
}

pub struct HistoryWriter {
    pub catalog: Catalog,
    pub archive_id: ArchiveId,
    pub bytes: Vec<u8>,
    pub manifests: Vec<WrittenManifest>,
    pub attributes: BTreeMap<FileVersionId, Attributes>,
    ids: SeqIds,
    pending_chunks: Vec<ChunkEntry>,
    pending_versions: Vec<FileVersionEntry>,
    parent: Option<ParentLink>,
    next_seq: u64,
}

impl HistoryWriter {
    /// The delta manifest of commit `seq`.
    pub fn delta(&self, seq: u64) -> &WrittenManifest {
        self.manifest(seq, ManifestKind::Delta)
    }

    /// The manifest of `kind` written for commit `seq`.
    pub fn manifest(&self, seq: u64, kind: ManifestKind) -> &WrittenManifest {
        self.manifests
            .iter()
            .find(|m| m.seq == seq && m.kind == kind)
            .expect("no such manifest in this history")
    }
}

impl HistoryWriter {
    pub fn new(seed: u64) -> Result<Self> {
        let mut ids = SeqIds::new(seed);
        let archive_id = ArchiveId::generate(&mut ids)?;
        Ok(HistoryWriter {
            catalog: Catalog::new_working()?,
            archive_id,
            bytes: Vec::new(),
            manifests: Vec::new(),
            attributes: BTreeMap::new(),
            ids,
            pending_chunks: Vec::new(),
            pending_versions: Vec::new(),
            parent: None,
            next_seq: 0,
        })
    }

    fn new_version_id(&mut self) -> Result<FileVersionId> {
        Ok(FileVersionId::from_bytes(self.ids.next_id()?))
    }

    /// A new directory version.
    pub fn dir(&mut self, attributes: Attributes) -> Result<FileVersionId> {
        let id = self.new_version_id()?;
        let version = FileVersion {
            id,
            kind: EntryKind::Directory,
            logical_len: 0,
            content_hash: None,
        };
        self.catalog.insert_file_version(&version, &[])?;
        self.attributes.insert(id, attributes);
        self.pending_versions.push(FileVersionEntry {
            version,
            extents: vec![],
            attributes,
        });
        Ok(id)
    }

    /// A new file version: `content` in one freshly encoded chunk, followed by
    /// a hole of `hole` zero bytes (sparse), if nonzero.
    pub fn file(
        &mut self,
        content: &[u8],
        hole: u64,
        attributes: Attributes,
    ) -> Result<FileVersionId> {
        let mut extents = Vec::new();
        if !content.is_empty() {
            let obj = build_object(
                &DecodedBytes::new(content.to_vec()),
                &EncodeParams::default(),
                Protection::None,
                &mut self.ids,
                &Limits::default(),
            )?;
            let offset = self.bytes.len() as u64;
            self.bytes.extend_from_slice(obj.stored.as_bytes());
            self.catalog.insert_object(&obj.record, Some(offset))?;
            extents.push(Extent {
                ordinal: 0,
                logical_offset: 0,
                length: content.len() as u64,
                source: ExtentSource::Chunk {
                    chunk: obj.record.id,
                    chunk_offset: 0,
                },
            });
            self.pending_chunks.push(ChunkEntry {
                record: obj.record,
                location: Some(offset),
            });
        }
        if hole > 0 {
            extents.push(Extent {
                ordinal: extents.len() as u32,
                logical_offset: content.len() as u64,
                length: hole,
                source: ExtentSource::Hole,
            });
        }
        let mut logical = content.to_vec();
        logical.resize(content.len() + hole as usize, 0);
        let id = self.new_version_id()?;
        let version = FileVersion {
            id,
            kind: EntryKind::File,
            logical_len: logical.len() as u64,
            content_hash: Some(file_content_hash(DecodedSlice::from_logical(&logical))),
        };
        self.catalog.insert_file_version(&version, &extents)?;
        self.attributes.insert(id, attributes);
        self.pending_versions.push(FileVersionEntry {
            version,
            extents,
            attributes,
        });
        Ok(id)
    }

    /// Commit `ops`, then write this commit's delta manifest, and also a
    /// snapshot manifest of the result if `with_snapshot`.
    pub fn commit(&mut self, ops: Vec<NamespaceOp>, with_snapshot: bool) -> Result<()> {
        let seq = self.next_seq;
        let parent_seq = self.parent.map(|p| p.seq);
        self.catalog.append_commit(&Commit {
            seq,
            parent: parent_seq,
            ops: ops.clone(),
        })?;
        let mut txid = [0u8; 16];
        txid.copy_from_slice(&self.ids.next_id()?[..16]);
        let transaction_id = mochi_core::commit::uuid_v4(txid);
        let mut delta = Manifest {
            archive_id: self.archive_id,
            commit_seq: seq,
            transaction_id,
            parent: self.parent,
            kind: ManifestKind::Delta,
            chunks: std::mem::take(&mut self.pending_chunks),
            file_versions: std::mem::take(&mut self.pending_versions),
            ops,
            entries: vec![],
            required_features: vec![],
        };
        delta.canonicalize();
        let delta_hash = self.write(&delta)?;
        if with_snapshot {
            let snapshot = Manifest::snapshot_from_catalog(
                &self.catalog,
                self.archive_id,
                seq,
                transaction_id,
                &self.attributes,
            )?;
            self.write(&snapshot)?;
        }
        self.parent = Some(ParentLink {
            seq,
            delta_manifest_hash: delta_hash,
        });
        self.next_seq += 1;
        Ok(())
    }

    fn write(&mut self, manifest: &Manifest) -> Result<StoredObjectHash> {
        let stored = manifest.to_stored()?;
        let hash = mochi_format::digest::stored_object_hash(stored.view());
        let offset = self.bytes.len() as u64;
        self.bytes.extend_from_slice(stored.as_bytes());
        self.manifests.push(WrittenManifest {
            seq: manifest.commit_seq,
            offset,
            len: stored.len(),
            hash,
            kind: manifest.kind,
        });
        Ok(hash)
    }
}
