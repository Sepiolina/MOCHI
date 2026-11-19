//! R8 recipes needing forged records: a chunk sealed under another key, and a
//! checkpoint whose catalog image is damaged but hashes correctly.

use mochi_core::catalog::extent::{Extent, ExtentSource};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use mochi_core::catalog::FileVersion;
use mochi_core::commit::{Metadata, ObjectRef};
use mochi_core::image::{decode_image_record, encode_image_record};
use mochi_core::manifest::{ChunkEntry, FileVersionEntry};
use mochi_core::object::build_object_sealed;
use mochi_core::publish::{open_head, ArchiveWriter, CheckpointPolicy, TailPolicy, Transaction};
use mochi_format::codec::EncodeParams;
use mochi_format::digest::{file_content_hash, stored_object_hash};
use mochi_format::envelope::RecordIdentity;
use mochi_format::repr::{DecodedBytes, DecodedSlice, StoredObject};
use mochi_format::seal::{seal_frame, SealContext, SealTarget};
use mochi_format::secret::{DataKey, OsRandom};
use mochi_format::Limits;
use mochi_testkit::archive::{keyed_read, path, Job};
use mochi_testkit::forge::{delta_record, empty_delta, rule_base, txid, Forge};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

use crate::recipes::{append_options, KDF, PASS};

/// A head chunk sealed under a key the archive does not hold: its stored
/// hash, the data region, the manifest and the commit all agree, and only
/// the AEAD tag fails (`CONTENT_INTEGRITY_FAILED`).
pub fn aead_tag(base: Vec<u8>) -> Vec<u8> {
    let _ = KDF;
    let mut f = Forge::new(base);
    let prev = f.history().pop().unwrap();
    let head = open_head(&f.storage(), &keyed_read(&[PASS])).unwrap();
    let real = head.seal_context().expect("an Encrypted head is unlocked");
    let other = DataKey::from_bytes([0x5A; 32]);
    let wrong = SealContext {
        key: &other,
        key_id: real.key_id,
        archive_id: real.archive_id,
    };

    let content = deterministic_bytes(95, 150);
    let obj = build_object_sealed(
        &DecodedBytes::new(content.clone()),
        &EncodeParams::default(),
        &wrong,
        &mut SeqIds::new(0x7000),
        &mut OsRandom,
        &Limits::WRITER_DEFAULT,
    )
    .unwrap();
    let chunk_ref = f.append_object(&obj.stored);

    let id = txid(0x51);
    let vid = FileVersionId::from_bytes([0xD8; 32]);
    let mut m = empty_delta(&prev, id);
    m.required_features = vec![1];
    m.chunks.push(ChunkEntry {
        record: obj.record.clone(),
        location: Some(chunk_ref.offset),
    });
    m.file_versions.push(FileVersionEntry {
        version: FileVersion {
            id: vid,
            kind: EntryKind::File,
            logical_len: content.len() as u64,
            content_hash: Some(file_content_hash(DecodedSlice::from_logical(&content))),
        },
        extents: vec![Extent {
            ordinal: 0,
            logical_offset: 0,
            length: content.len() as u64,
            source: ExtentSource::Chunk {
                chunk: obj.record.id,
                chunk_offset: 0,
            },
        }],
        attributes: attrs(0o644, 9),
    });
    m.ops.push(NamespaceOp::Put {
        path: path("tag.bin"),
        version: vid,
    });
    m.canonicalize();
    let sealed = seal_frame(
        &real,
        &SealTarget::DeltaManifest {
            sequence: prev.commit.seq + 1,
            transaction_id: id,
        },
        &m.encode().unwrap(),
        &mut OsRandom,
        &Limits::WRITER_DEFAULT,
    )
    .unwrap();
    let manifest_ref = f.append_object(&sealed);
    let mut record = delta_record(&prev, rule_base(&prev), manifest_ref, id);
    record.required_features = vec![1];
    record.key_envelopes = prev.commit.key_envelopes.clone();
    record.data_region = Some(ObjectRef {
        offset: chunk_ref.offset,
        stored_len: chunk_ref.stored_len,
        stored_hash: stored_object_hash(obj.stored.view()),
    });
    f.append_commit(&record);
    f.bytes
}

/// A checkpoint whose catalog image has a zeroed page but is wrapped in a
/// correct envelope and hashed correctly: the commit references what is
/// there, and SQLite refuses it (`CATALOG_INVALID`).
pub fn damaged_image_checkpoint(base: Vec<u8>) -> Vec<u8> {
    let s = SimStorage::from_bytes(base);
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(0x71)),
        append_options(false),
        TailPolicy::Refuse,
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    let mut t = Transaction::new();
    t.put_file(
        path("cp.bin"),
        deterministic_bytes(96, 120),
        attrs(0o644, 10),
    );
    w.commit(t, &Job::new().ctx()).unwrap();
    w.close().unwrap();

    let mut f = Forge::new(s.contents());
    let real = f.history().pop().unwrap();
    let Metadata::Checkpoint { image, snapshot } = real.commit.metadata else {
        panic!("EveryCommit writes a checkpoint")
    };
    let identity = RecordIdentity {
        archive_id: *real.commit.archive_id.as_bytes(),
        commit_sequence: real.commit.seq,
        transaction_id: real.commit.transaction_id,
    };
    let frame = StoredObject::from_loaded(
        f.bytes[image.offset as usize..(image.offset + image.stored_len) as usize].to_vec(),
    );
    let mut sqlite = decode_image_record(&frame, &identity, &Limits::default())
        .unwrap()
        .to_vec();
    let page = u16::from_be_bytes([sqlite[16], sqlite[17]]) as usize;
    assert!(sqlite.len() >= 3 * page, "the image has a third page");
    sqlite[2 * page..3 * page].fill(0);
    let bad = encode_image_record(&sqlite, identity, false).unwrap();
    f.bytes.truncate(real.commit_offset as usize);
    let image_ref = f.append_object(&bad);
    let mut rec = real.commit.clone();
    rec.metadata = Metadata::Checkpoint {
        image: image_ref,
        snapshot,
    };
    f.append_commit(&rec);
    f.bytes
}

/// The plaintext of commit 0's sealed delta manifest in the Encrypted archive
/// `bytes`, opened with `pass`: the only place schema 3 exists on disk.
pub fn delta0_plaintext(bytes: &[u8], pass: &str) -> Vec<u8> {
    use mochi_core::publish::{commit_history, ReadOptions};
    use mochi_format::seal::open_payload;
    let s = SimStorage::from_bytes(bytes.to_vec());
    let read = keyed_read(&[pass]);
    let head = open_head(&s, &read).unwrap();
    let ctx = head.seal_context().expect("unlocked");
    let first = commit_history(&s, &ReadOptions::default())
        .unwrap()
        .remove(0);
    let r = first.commit.delta_manifest;
    let frame = &bytes[r.offset as usize..(r.offset + r.stored_len) as usize];
    let sealed = &frame[8..];
    open_payload(
        &ctx,
        &SealTarget::DeltaManifest {
            sequence: first.commit.seq,
            transaction_id: first.commit.transaction_id,
        },
        sealed,
    )
    .unwrap()
}

#[derive(Debug, Clone, Copy)]
pub enum Enc11 {
    /// The commit lists its two key envelopes in decreasing envelope-ID order.
    Unordered,
    /// The commit lists the same key envelope twice.
    Duplicate,
    /// The commit's data region is a sealed frame that is not a data object.
    RegionNotData,
}

/// Forge one delta commit on an Encrypted archive with two key envelopes
/// (`enc-valid-rekeyed`), well formed except for the one thing `what` names
/// (G1 audit: commit schema 2, keys 11 and 12).
pub fn forged_enc_commit(base: Vec<u8>, what: Enc11) -> Vec<u8> {
    let mut f = Forge::new(base);
    let prev = f.history().pop().unwrap();
    assert_eq!(prev.commit.key_envelopes.len(), 2);
    let head = open_head(&f.storage(), &keyed_read(&[PASS])).unwrap();
    let ctx = head.seal_context().expect("unlocked");
    let id = txid(0x52);
    let mut m = empty_delta(&prev, id);
    m.required_features = vec![1];
    let sealed = seal_frame(
        &ctx,
        &SealTarget::DeltaManifest {
            sequence: prev.commit.seq + 1,
            transaction_id: id,
        },
        &m.encode().unwrap(),
        &mut OsRandom,
        &Limits::WRITER_DEFAULT,
    )
    .unwrap();
    let region = matches!(what, Enc11::RegionNotData).then(|| prev.commit.delta_manifest);
    let manifest_ref = f.append_object(&sealed);
    let mut record = delta_record(&prev, rule_base(&prev), manifest_ref, id);
    record.required_features = vec![1];
    let e = &prev.commit.key_envelopes;
    record.key_envelopes = match what {
        Enc11::Unordered => vec![e[1], e[0]],
        Enc11::Duplicate => vec![e[0], e[0]],
        Enc11::RegionNotData => e.clone(),
    };
    record.data_region = region;
    f.append_commit(&record);
    f.bytes
}
