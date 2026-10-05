//! Cross-check of the MOCHI CBOR subset codec against `ciborium`, an
//! independent, widely used implementation (test-only dependency).
//!
//! * Every value we encode is valid CBOR by ciborium's reading, with the same
//!   meaning.
//! * ciborium's own encoding of the same value is byte-identical to ours, so
//!   "canonical" here is not just our own opinion.
//! * Anything our strict decoder accepts, including mutated inputs, ciborium
//!   accepts with the same meaning: we never admit malformed CBOR.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ciborium::value::{Integer, Value as CValue};
use mochi_format::cbor::{decode, encode, CborLimits, Value};
use proptest::prelude::*;

fn to_ciborium(v: &Value) -> CValue {
    match v {
        Value::Uint(n) => CValue::Integer(Integer::from(*n)),
        Value::Nint(n) => CValue::Integer(Integer::try_from(-1i128 - i128::from(*n)).unwrap()),
        Value::Bytes(b) => CValue::Bytes(b.clone()),
        Value::Text(t) => CValue::Text(t.clone()),
        Value::Array(a) => CValue::Array(a.iter().map(to_ciborium).collect()),
        Value::Map(m) => CValue::Map(
            m.iter()
                .map(|(k, v)| (CValue::Integer(Integer::from(*k)), to_ciborium(v)))
                .collect(),
        ),
        Value::Bool(b) => CValue::Bool(*b),
        Value::Null => CValue::Null,
    }
}

fn value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        prop_oneof![
            any::<u64>(),
            0u64..30,
            Just(23u64),
            Just(24u64),
            Just(255u64),
            Just(256u64),
            Just(65_535u64),
            Just(65_536u64),
            Just(u64::from(u32::MAX)),
            Just(u64::from(u32::MAX) + 1),
        ]
        .prop_map(Value::Uint),
        any::<u64>().prop_map(Value::Nint),
        proptest::collection::vec(any::<u8>(), 0..300).prop_map(Value::Bytes),
        ".{0,40}".prop_map(Value::Text),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ];
    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
            proptest::collection::btree_map(any::<u64>(), inner, 0..8)
                .prop_map(|m| Value::Map(m.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn our_encoding_is_valid_cbor_and_matches_ciborium_byte_for_byte(v in value()) {
        let ours = encode(&v).unwrap();
        let theirs_read: CValue = ciborium::de::from_reader(&ours[..]).unwrap();
        prop_assert_eq!(&theirs_read, &to_ciborium(&v));
        let mut theirs = Vec::new();
        ciborium::ser::into_writer(&to_ciborium(&v), &mut theirs).unwrap();
        prop_assert_eq!(&theirs, &ours);
        prop_assert_eq!(decode(&ours, &CborLimits::default()).unwrap(), v);
    }

    #[test]
    fn anything_we_accept_ciborium_reads_identically(
        v in value(),
        edits in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..4),
    ) {
        let mut bytes = encode(&v).unwrap();
        if bytes.is_empty() { return Ok(()); }
        for (at, b) in &edits {
            let i = at.index(bytes.len());
            bytes[i] = *b;
        }
        if let Ok(ours) = decode(&bytes, &CborLimits::default()) {
            let theirs: CValue = ciborium::de::from_reader(&bytes[..])
                .expect("we accepted input ciborium rejects");
            prop_assert_eq!(theirs, to_ciborium(&ours));
            prop_assert_eq!(encode(&ours).unwrap(), bytes);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic_and_acceptance_implies_canonical(
        bytes in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        if let Ok(v) = decode(&bytes, &CborLimits::default()) {
            prop_assert_eq!(encode(&v).unwrap(), bytes);
        }
    }
}
