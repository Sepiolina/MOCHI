//! Object model (plan C2; spec §4.2, §9, §10.1).
//!
//! An [`ObjectRecord`] is everything the catalog will say about one stored
//! data object: its identity, its stored hash and length, its decoded length
//! and chunk content hash, how it was encoded, and what it depends on. The
//! record's two halves line up with two verification levels (§20.1):
//!
//! * **stored integrity** — [`verify_stored`]: the loaded bytes have the
//!   recorded length and stored-object hash. Needs no keys and no decoder.
//! * **content integrity** — [`decode_verified`]: after stored integrity,
//!   the object decodes to exactly the recorded length and chunk content hash.
//!
//! Neither check substitutes for the other (§9.3). A writer that recorded a
//! correct stored hash over a bad frame passes stored integrity and fails
//! content integrity; the tests in `mochi-testkit/tests/c2_objects.rs` build
//! exactly that case.
//!
//! **Object identity** (plan §9, O19, decided): a uniformly random 256-bit
//! value drawn from the operating system's CSPRNG when the object is created,
//! never derived from content, stored bytes, or position. That makes it stable
//! under re-encoding, compaction, and relocation (§10.1, §17, §29.1 #11),
//! reveals nothing about content on encrypted archives, and never assigns one
//! ID to two different records (§10.2). [`OsIds`] is the only production
//! source; tests inject a deterministic one. An RNG failure is an error, never
//! a fallback to a weaker source.

use std::fmt;

use mochi_format::codec::{self, EncodeParams, Encoding, Protection};
use mochi_format::digest::{
    chunk_content_hash, stored_object_hash, ChunkContentHash, StoredObjectHash,
};
use mochi_format::repr::{DecodedBytes, StoredObject};
use mochi_format::Limits;

use crate::error::{ErrorCode, MochiError, Result};
use crate::storage::ReadStorage;

/// Stable logical identity of an object. Opaque; 32 bytes so it fits the
/// draft envelope's subject-identity field (spec §8.3; `envelope.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId([u8; 32]);

impl ObjectId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        ObjectId(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

/// Identity of one archive (spec §12.1 "Archive ID"): 256 random bits,
/// generated once at creation by an [`IdSource`] (O19 rule). Every commit and
/// recovery manifest carries it, so salvage can tell archives apart.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchiveId([u8; 32]);

impl ArchiveId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        ArchiveId(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
    pub fn generate(ids: &mut dyn IdSource) -> Result<Self> {
        Ok(ArchiveId(ids.next_id()?))
    }
    /// Lowercase hex of all 32 bytes, as reports carry it.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Debug for ArchiveId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ArchiveId(")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

/// Supplies 256-bit identities for new immutable records: objects and file
/// versions (O19). Fallible so that an RNG failure stops the write instead of
/// producing a predictable ID.
pub trait IdSource {
    fn next_id(&mut self) -> Result<[u8; 32]>;
}

/// Production source: 256 bits from the OS CSPRNG per record (O19).
///
/// Collision probability is negligible (birthday bound 2^128), but the
/// catalog (C3) still enforces uniqueness, and a duplicate is reported as
/// corruption per §10.2 rather than treated as an update.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsIds;

impl IdSource for OsIds {
    fn next_id(&mut self) -> Result<[u8; 32]> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| {
            MochiError::new(
                ErrorCode::IoError,
                format!("operating-system random source failed: {e}"),
            )
        })?;
        Ok(bytes)
    }
}

/// Something an object needs before it can be decoded (spec §5.2, §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dependency {
    /// A shared compression dictionary object.
    Dictionary(ObjectId),
    /// A key envelope (C11).
    KeyEnvelope(ObjectId),
}

/// Catalog record for one stored data object (§10.1 `objects` + `chunks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    pub id: ObjectId,
    pub encoding: Encoding,
    pub protection: Protection,
    /// Exact stored length, including framing.
    pub stored_len: u64,
    /// Over exact stored bytes (ciphertext, if protected).
    pub stored_hash: StoredObjectHash,
    /// Exact decoded length.
    pub decoded_len: u64,
    /// Over exact decoded bytes.
    pub content_hash: ChunkContentHash,
    pub dependencies: Vec<Dependency>,
}

/// A freshly encoded object: its record and the bytes to write.
#[derive(Debug, Clone)]
pub struct EncodedObject {
    pub record: ObjectRecord,
    pub stored: StoredObject,
}

/// Encode `content` and describe it. Both digests are computed from the
/// representations they are defined over, never from each other.
pub fn build_object(
    content: &DecodedBytes,
    params: &EncodeParams,
    protection: Protection,
    ids: &mut dyn IdSource,
    limits: &Limits,
) -> Result<EncodedObject> {
    let stored = codec::encode_object(content, params, protection, limits)?;
    let record = ObjectRecord {
        id: ObjectId(ids.next_id()?),
        encoding: params.encoding,
        protection,
        stored_len: stored.len(),
        stored_hash: stored_object_hash(stored.view()),
        decoded_len: content.len(),
        content_hash: chunk_content_hash(content),
        dependencies: Vec::new(),
    };
    Ok(EncodedObject { record, stored })
}

/// Read an object's stored bytes at `offset`. The recorded length is
/// archive-derived, so it is limit-checked before allocation (spec §8.5).
pub fn load_stored(
    storage: &dyn ReadStorage,
    offset: u64,
    record: &ObjectRecord,
    limits: &Limits,
) -> Result<StoredObject> {
    if record.stored_len > limits.max_frame_len {
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            format!(
                "object {} records stored length {} above limit {}",
                record.id.to_hex(),
                record.stored_len,
                limits.max_frame_len
            ),
        ));
    }
    offset.checked_add(record.stored_len).ok_or_else(|| {
        MochiError::new(
            ErrorCode::OutOfBounds,
            format!("object {} range overflows", record.id.to_hex()),
        )
    })?;
    let len = usize::try_from(record.stored_len).map_err(|_| {
        MochiError::new(
            ErrorCode::LimitExceeded,
            "stored length does not fit in memory on this platform",
        )
    })?;
    let mut buf = vec![0u8; len];
    storage.read_exact_at(offset, &mut buf)?;
    Ok(StoredObject::from_loaded(buf))
}

/// Stored integrity (spec §20.1): exact length, then stored-object hash.
/// Keyless and decoder-free by construction.
pub fn verify_stored(record: &ObjectRecord, stored: &StoredObject) -> Result<()> {
    if stored.len() != record.stored_len {
        return Err(MochiError::new(
            ErrorCode::StoredIntegrityFailed,
            format!(
                "object {}: stored length {} but record says {}",
                record.id.to_hex(),
                stored.len(),
                record.stored_len
            ),
        ));
    }
    if stored_object_hash(stored.view()) != record.stored_hash {
        return Err(MochiError::new(
            ErrorCode::StoredIntegrityFailed,
            format!("object {}: stored-object hash mismatch", record.id.to_hex()),
        ));
    }
    Ok(())
}

/// Stored integrity, then content integrity (spec §20.1). Returns decoded
/// bytes only if every check passes; there is no partial result.
pub fn decode_verified(
    record: &ObjectRecord,
    stored: &StoredObject,
    limits: &Limits,
) -> Result<DecodedBytes> {
    verify_stored(record, stored)?;
    if let Some(dep) = record.dependencies.first() {
        // Dependency resolution arrives with dictionaries (O21) and keys (C11).
        return Err(MochiError::new(
            ErrorCode::UnsupportedFeature,
            format!(
                "object {} depends on {dep:?}; dependency resolution is not implemented",
                record.id.to_hex()
            ),
        ));
    }
    let decoded = codec::decode_object(stored, record.protection, record.decoded_len, limits)
        .map_err(|e| {
            let err = MochiError::from(e);
            MochiError::new(
                err.code,
                format!("object {}: {}", record.id.to_hex(), err.message),
            )
        })?;
    if chunk_content_hash(&decoded) != record.content_hash {
        return Err(MochiError::new(
            ErrorCode::ContentIntegrityFailed,
            format!("object {}: chunk content hash mismatch", record.id.to_hex()),
        ));
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(u8);
    impl IdSource for Counter {
        fn next_id(&mut self) -> Result<[u8; 32]> {
            self.0 += 1;
            Ok([self.0; 32])
        }
    }

    #[test]
    fn os_ids_are_distinct_and_not_degenerate() {
        let mut src = OsIds;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = src.next_id().unwrap();
            assert_ne!(id, [0u8; 32]);
            assert!(seen.insert(id), "duplicate ID from the OS source");
        }
    }

    #[test]
    fn identical_content_gets_distinct_ids() {
        // O19: identity names an object, not its content; §10.2 would call
        // one ID with two different records corruption.
        let content = DecodedBytes::new(b"same".to_vec());
        let mut ids = OsIds;
        let a = build_object(
            &content,
            &EncodeParams::default(),
            Protection::None,
            &mut ids,
            &Limits::default(),
        )
        .unwrap();
        let b = build_object(
            &content,
            &EncodeParams {
                level: 19,
                ..EncodeParams::default()
            },
            Protection::None,
            &mut ids,
            &Limits::default(),
        )
        .unwrap();
        assert_ne!(a.record.id, b.record.id);
        assert_eq!(a.record.content_hash, b.record.content_hash);
    }

    fn built(content: &[u8]) -> EncodedObject {
        build_object(
            &DecodedBytes::new(content.to_vec()),
            &EncodeParams::default(),
            Protection::None,
            &mut Counter(0),
            &Limits::default(),
        )
        .unwrap()
    }

    #[test]
    fn record_digests_cover_their_own_representations() {
        let o = built(b"some content");
        assert_eq!(o.record.stored_hash, stored_object_hash(o.stored.view()));
        assert_eq!(
            o.record.content_hash,
            chunk_content_hash(&DecodedBytes::new(b"some content".to_vec()))
        );
        assert_eq!(o.record.stored_len, o.stored.len());
        assert_eq!(o.record.decoded_len, 12);
        // Different representations of the same content, different digests.
        assert_ne!(
            o.record.stored_hash.as_bytes(),
            o.record.content_hash.as_bytes()
        );
    }

    #[test]
    fn stored_length_mismatch_is_stored_integrity() {
        let o = built(b"abc");
        let mut short = o.stored.as_bytes().to_vec();
        short.pop();
        let e = verify_stored(&o.record, &StoredObject::from_loaded(short)).unwrap_err();
        assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    }

    #[test]
    fn declared_dependencies_are_unsupported_not_ignored() {
        let mut o = built(b"abc");
        o.record
            .dependencies
            .push(Dependency::Dictionary(ObjectId::from_bytes([9; 32])));
        let e = decode_verified(&o.record, &o.stored, &Limits::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature);
    }

    #[test]
    fn oversized_record_is_refused_before_allocation() {
        struct Never;
        impl ReadStorage for Never {
            fn size(&self) -> std::result::Result<u64, crate::storage::StorageError> {
                Ok(0)
            }
            fn read_at(
                &self,
                _: u64,
                _: &mut [u8],
            ) -> std::result::Result<usize, crate::storage::StorageError> {
                panic!("must not read");
            }
        }
        let mut o = built(b"abc");
        o.record.stored_len = u64::MAX;
        let e = load_stored(&Never, 0, &o.record, &Limits::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::LimitExceeded);
        o.record.stored_len = 10;
        let e = load_stored(&Never, u64::MAX - 3, &o.record, &Limits::default()).unwrap_err();
        assert_eq!(e.code, ErrorCode::OutOfBounds);
    }
}
