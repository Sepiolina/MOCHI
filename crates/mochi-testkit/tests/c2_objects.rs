//! C2 exit criteria (plan §5 C2, §8.1 "Digests"):
//!
//! * property: encode → store → load → decode round-trips, and every digest
//!   matches an independent recomputation over its own representation;
//! * stored integrity catches every single-bit flip and every truncation;
//! * content integrity is independent of stored integrity: even when the
//!   recorded stored hash is re-sealed over damaged bytes (a writer bug, or a
//!   hash that "verifies" the wrong thing), decoding never returns bytes that
//!   differ from the original content.
//!
//! The last one is the detection half of fault-matrix row *corrupted content
//! object* (spec §24.2) at object granularity; the report-level half is C7.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_core::object::{
    build_object, decode_verified, load_stored, verify_stored, EncodedObject,
};
use mochi_core::storage::Storage;
use mochi_core::ErrorCode;
use mochi_format::codec::{EncodeParams, Protection};
use mochi_format::digest::{
    chunk_content_hash, file_content_hash, stored_object_hash, FileContentHasher,
};
use mochi_format::repr::{DecodedBytes, DecodedSlice, StoredObject};
use mochi_format::Limits;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};
use proptest::prelude::*;

/// Content shapes that exercise raw, RLE, and compressed Zstandard blocks,
/// including multi-block frames (block size is at most 128 KiB).
fn content() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => proptest::collection::vec(any::<u8>(), 0..2048),
        2 => (any::<u64>(), 0usize..200_000).prop_map(|(seed, n)| deterministic_bytes(seed, n)),
        2 => (any::<u8>(), 0usize..300_000).prop_map(|(b, n)| vec![b; n]),
        2 => (proptest::collection::vec(any::<u8>(), 1..64), 1usize..4000)
            .prop_map(|(unit, reps)| unit.repeat(reps)),
    ]
}

fn level() -> impl Strategy<Value = i32> {
    prop_oneof![Just(-5), Just(1), Just(3), Just(9)]
}

fn build(content: &[u8], level: i32, seed: u64) -> EncodedObject {
    build_object(
        &DecodedBytes::new(content.to_vec()),
        &EncodeParams {
            level,
            ..EncodeParams::default()
        },
        Protection::None,
        &mut SeqIds::new(seed),
        &Limits::default(),
    )
    .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn encode_store_load_decode_round_trips_and_every_digest_matches(
        content in content(),
        level in level(),
        junk_len in 0usize..300,
        seed in any::<u64>(),
    ) {
        let limits = Limits::default();
        let obj = build(&content, level, seed);

        // Store after unrelated bytes so the object does not sit at offset 0.
        let mut storage = SimStorage::new();
        storage.append(&deterministic_bytes(seed ^ 0xA5, junk_len)).unwrap();
        let offset = storage.append(obj.stored.as_bytes()).unwrap();
        prop_assert_eq!(offset, junk_len as u64);

        // Load and decode through the verifier.
        let loaded = load_stored(&storage, offset, &obj.record, &limits).unwrap();
        let decoded = decode_verified(&obj.record, &loaded, &limits).unwrap();
        prop_assert_eq!(decoded.as_bytes(), &content[..]);

        // Independent recomputation of every digest over its own representation.
        prop_assert_eq!(stored_object_hash(loaded.view()), obj.record.stored_hash);
        prop_assert_eq!(chunk_content_hash(&decoded), obj.record.content_hash);
        prop_assert_eq!(obj.record.stored_len, loaded.len());
        prop_assert_eq!(obj.record.decoded_len, content.len() as u64);
        // The stored hash is over stored bytes, not decoded ones: the two
        // scopes never coincide, even for content that happens to be stored raw.
        prop_assert_ne!(obj.record.stored_hash.as_bytes(), obj.record.content_hash.as_bytes());
    }

    #[test]
    fn file_hash_is_extent_independent_and_holes_are_zeros(
        content in content(),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..6),
        hole_at in any::<prop::sample::Index>(),
        hole_len in 0u64..70_000,
    ) {
        let decoded = DecodedBytes::new(content.clone());
        let n = decoded.len();

        // Arbitrary extent boundaries give the same file hash (§10.3, §9.2).
        let mut bounds: Vec<u64> = cuts.iter().map(|i| i.index(content.len() + 1) as u64).collect();
        bounds.push(0);
        bounds.push(n);
        bounds.sort_unstable();
        let mut h = FileContentHasher::new();
        for w in bounds.windows(2) {
            h.update(decoded.slice(w[0]..w[1]).unwrap()).unwrap();
        }
        prop_assert_eq!(h.len(), n);
        prop_assert_eq!(h.finalize(), file_content_hash(decoded.as_slice()));

        // A hole hashes as its logical zero bytes (§9.3).
        let p = hole_at.index(content.len() + 1) as u64;
        let mut sparse = FileContentHasher::new();
        sparse.update(decoded.slice(0..p).unwrap()).unwrap();
        sparse.hole(hole_len).unwrap();
        sparse.update(decoded.slice(p..n).unwrap()).unwrap();
        let mut materialized = content[..p as usize].to_vec();
        materialized.extend(std::iter::repeat_n(0u8, hole_len as usize));
        materialized.extend_from_slice(&content[p as usize..]);
        prop_assert_eq!(
            sparse.finalize(),
            file_content_hash(DecodedSlice::from_logical(&materialized))
        );
    }

    #[test]
    fn every_single_bit_flip_fails_stored_integrity(
        content in content(),
        level in level(),
        at in any::<prop::sample::Index>(),
        bit in 0u8..8,
    ) {
        let obj = build(&content, level, 1);
        let mut bytes = obj.stored.as_bytes().to_vec();
        let i = at.index(bytes.len());
        bytes[i] ^= 1 << bit;
        let e = decode_verified(&obj.record, &StoredObject::from_loaded(bytes), &Limits::default())
            .unwrap_err();
        prop_assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    }

    #[test]
    fn content_integrity_never_returns_wrong_bytes_even_if_stored_hash_is_resealed(
        content in content(),
        level in level(),
        flips in proptest::collection::vec((any::<prop::sample::Index>(), 0u8..8), 1..4),
    ) {
        let limits = Limits::default();
        let obj = build(&content, level, 2);
        let mut bytes = obj.stored.as_bytes().to_vec();
        for (at, bit) in &flips {
            let i = at.index(bytes.len());
            bytes[i] ^= 1 << bit;
        }
        let damaged = StoredObject::from_loaded(bytes);
        // Re-seal: the record now vouches for the damaged bytes, so stored
        // integrity passes and only content integrity stands in the way.
        let mut record = obj.record.clone();
        record.stored_hash = stored_object_hash(damaged.view());
        prop_assert!(verify_stored(&record, &damaged).is_ok());

        match decode_verified(&record, &damaged, &limits) {
            Ok(decoded) => prop_assert_eq!(decoded.as_bytes(), &content[..]),
            Err(e) => prop_assert!(
                matches!(
                    e.code,
                    ErrorCode::ContentIntegrityFailed
                        | ErrorCode::MalformedFrame
                        | ErrorCode::Truncated
                        | ErrorCode::LimitExceeded
                        | ErrorCode::UnsupportedFeature
                ),
                "unexpected code {:?}: {}", e.code, e.message
            ),
        }
    }
}

/// Every strict prefix of a stored object is rejected: by stored integrity
/// as recorded, and by the codec even when the record is re-sealed.
#[test]
fn truncation_at_every_byte_is_rejected() {
    let limits = Limits::default();
    let content = deterministic_bytes(7, 3000);
    for level in [-5, 3] {
        let obj = build(&content, level, 3);
        let full = obj.stored.as_bytes();
        for cut in 0..full.len() {
            let prefix = StoredObject::from_loaded(full[..cut].to_vec());
            let e = decode_verified(&obj.record, &prefix, &limits).unwrap_err();
            assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "cut {cut}");

            let mut resealed = obj.record.clone();
            resealed.stored_len = prefix.len();
            resealed.stored_hash = stored_object_hash(prefix.view());
            let e = decode_verified(&resealed, &prefix, &limits).unwrap_err();
            assert_ne!(e.code, ErrorCode::StoredIntegrityFailed, "cut {cut}");
        }
    }
}

/// A record whose decoded length or content hash is wrong is caught at the
/// content level even though the stored bytes are pristine.
#[test]
fn wrong_decoded_claims_fail_content_integrity() {
    let limits = Limits::default();
    let obj = build(b"exact content", 3, 4);

    let mut r = obj.record.clone();
    r.decoded_len += 1;
    let e = decode_verified(&r, &obj.stored, &limits).unwrap_err();
    assert_eq!(e.code, ErrorCode::ContentIntegrityFailed, "{}", e.message);

    let mut r = obj.record.clone();
    r.content_hash = chunk_content_hash(&DecodedBytes::new(b"other content".to_vec()));
    let e = decode_verified(&r, &obj.stored, &limits).unwrap_err();
    assert_eq!(e.code, ErrorCode::ContentIntegrityFailed, "{}", e.message);
}

/// A stored object read through a torn storage view is a stored-integrity
/// failure, not a decode attempt on partial bytes.
#[test]
fn load_past_end_of_storage_is_out_of_bounds() {
    let obj = build(b"abc", 3, 5);
    let mut storage = SimStorage::new();
    let off = storage.append(&obj.stored.as_bytes()[..5]).unwrap();
    let e = load_stored(&storage, off, &obj.record, &Limits::default()).unwrap_err();
    assert_eq!(e.code, ErrorCode::OutOfBounds);
}
