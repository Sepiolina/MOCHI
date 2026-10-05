//! Deterministic CBOR, restricted subset (spec Annex B.1, D2; plan O2).
//!
//! Every canonical structure in MOCHI (recovery manifests, commit bodies,
//! envelope payloads) goes through this one codec. The subset:
//!
//! * unsigned and negative integers (major types 0 and 1), shortest form;
//! * byte strings and UTF-8 text strings (2, 3), definite length, shortest form;
//! * arrays (4), definite length;
//! * maps (5) whose keys are unsigned integers in strictly increasing order;
//!   this equals RFC 8949 §4.2.1 bytewise key ordering for shortest-form
//!   integer keys, and it makes duplicate keys impossible;
//! * the simple values `false`, `true`, `null` (major type 7, values 20–22).
//!
//! Excluded, and each rejected with its own [`CborFault`]: tags (6), floats,
//! every other simple value (including `undefined`), indefinite lengths,
//! the reserved additional-information values 28–30, and any non-shortest
//! integer or length. As a final guard the decoder re-encodes what it read and
//! requires identical bytes, the rule Annex B.1 states, so a gap in the
//! individual checks still cannot admit a non-canonical input.
//!
//! Untrusted input is bounded before allocation: every declared length is
//! checked against the bytes actually remaining, nesting depth and the total
//! item count are limited, and trailing bytes are an error.

use crate::error::{FormatError, Result};

/// A value in the MOCHI CBOR subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Uint(u64),
    /// The negative integer `-1 - n`, as CBOR encodes it (so the full range
    /// down to -2^64 is representable).
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Keys strictly increasing; the encoder refuses anything else.
    Map(Vec<(u64, Value)>),
    Bool(bool),
    Null,
}

/// Why an input is not in the subset, or not canonical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborFault {
    Truncated,
    /// An integer or length not in its shortest form.
    NotShortest,
    /// Additional information 28, 29, or 30.
    ReservedAdditionalInfo,
    /// Indefinite length (additional information 31), or a stray break.
    Indefinite,
    Tag,
    Float,
    /// A simple value other than false, true, or null.
    SimpleValue,
    /// A map key that is not an unsigned integer.
    NonIntegerKey,
    /// Map keys not strictly increasing (unsorted or duplicate).
    KeyOrder,
    InvalidUtf8,
    /// A declared length larger than the remaining input.
    LengthExceedsInput,
    TooDeep,
    TooManyItems,
    TrailingBytes,
    /// Decoded, but did not re-encode to the same bytes.
    NotCanonical,
}

/// Bounds for decoding untrusted input. Placeholder defaults (plan O18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CborLimits {
    pub max_depth: usize,
    pub max_items: usize,
}

impl Default for CborLimits {
    fn default() -> Self {
        CborLimits {
            max_depth: 64,
            max_items: 1 << 24,
        }
    }
}

fn fault(f: CborFault) -> FormatError {
    FormatError::Cbor(f)
}

// ---- encoding -------------------------------------------------------------------

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    // Truncating casts are intended: each branch has checked the range.
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xFF {
        out.push(m | 24);
        out.push(n as u8);
    } else if n <= 0xFFFF {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xFFFF_FFFF {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_into(v: &Value, out: &mut Vec<u8>) -> Result<()> {
    match v {
        Value::Uint(n) => head(out, 0, *n),
        Value::Nint(n) => head(out, 1, *n),
        Value::Bytes(b) => {
            head(out, 2, b.len() as u64);
            out.extend_from_slice(b);
        }
        Value::Text(t) => {
            head(out, 3, t.len() as u64);
            out.extend_from_slice(t.as_bytes());
        }
        Value::Array(items) => {
            head(out, 4, items.len() as u64);
            for item in items {
                encode_into(item, out)?;
            }
        }
        Value::Map(entries) => {
            if entries.windows(2).any(|w| w[0].0 >= w[1].0) {
                return Err(FormatError::InvalidArgument(
                    "CBOR map keys must be strictly increasing",
                ));
            }
            head(out, 5, entries.len() as u64);
            for (k, val) in entries {
                head(out, 0, *k);
                encode_into(val, out)?;
            }
        }
        Value::Bool(false) => out.push(0xF4),
        Value::Bool(true) => out.push(0xF5),
        Value::Null => out.push(0xF6),
    }
    Ok(())
}

/// Canonical encoding. Fails only for a map with unsorted or duplicate keys,
/// which is a caller bug, never silently re-sorted.
pub fn encode(v: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    encode_into(v, &mut out)?;
    Ok(out)
}

/// What the decoder would count for `v`: total data items (every value and
/// every map key, B.2.4) and the deepest value's depth (root = 0, each array
/// element or map value one deeper; keys add no depth). Must mirror
/// [`Decoder::value`] exactly; `writer_measure_equals_reader_verdict` tests it.
fn measure(v: &Value, depth: usize, items: &mut u64, deepest: &mut usize) {
    *deepest = (*deepest).max(depth);
    *items = items.saturating_add(1);
    match v {
        Value::Array(xs) => {
            for x in xs {
                measure(x, depth + 1, items, deepest);
            }
        }
        Value::Map(entries) => {
            for (_, x) in entries {
                *items = items.saturating_add(1); // the key
                measure(x, depth + 1, items, deepest);
            }
        }
        _ => {}
    }
}

/// [`encode`], refusing a value that [`decode`] with `limits` would reject
/// for its item count or depth (spec Annex B.2.3 writer default rule). The
/// refusal is [`FormatError::CapacityExceeded`]. Writers pass
/// `CborLimits::default()`, the reader defaults, whatever they read with.
pub fn encode_within(v: &Value, limits: &CborLimits) -> Result<Vec<u8>> {
    use crate::error::CapacityKind;
    let (mut items, mut deepest) = (0u64, 0usize);
    measure(v, 0, &mut items, &mut deepest);
    if deepest > limits.max_depth {
        return Err(FormatError::CapacityExceeded {
            kind: CapacityKind::CborDepth,
            limit: limits.max_depth as u64,
            actual: deepest as u64,
        });
    }
    if items > limits.max_items as u64 {
        return Err(FormatError::CapacityExceeded {
            kind: CapacityKind::CborItems,
            limit: limits.max_items as u64,
            actual: items,
        });
    }
    encode(v)
}

// ---- decoding -------------------------------------------------------------------

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    items: usize,
    limits: CborLimits,
}

impl Decoder<'_> {
    fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.remaining() < n {
            return Err(fault(CborFault::Truncated));
        }
        let s = &self.input[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Returns (major type, argument). Enforces shortest form.
    fn head(&mut self) -> Result<(u8, u64)> {
        let b = self.take(1)?[0];
        let major = b >> 5;
        let ai = b & 0x1F;
        let n = match ai {
            0..=23 => u64::from(ai),
            24 => {
                let v = u64::from(self.take(1)?[0]);
                // For major type 7, ai 24 is a 1-byte simple value; always
                // outside the subset, reported as such below.
                if major != 7 && v < 24 {
                    return Err(fault(CborFault::NotShortest));
                }
                v
            }
            25 => {
                let s = self.take(2)?;
                let v = u64::from(u16::from_be_bytes([s[0], s[1]]));
                if major != 7 && v <= 0xFF {
                    return Err(fault(CborFault::NotShortest));
                }
                v
            }
            26 => {
                let s = self.take(4)?;
                let v = u64::from(u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
                if major != 7 && v <= 0xFFFF {
                    return Err(fault(CborFault::NotShortest));
                }
                v
            }
            27 => {
                let s = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(s);
                let v = u64::from_be_bytes(a);
                if major != 7 && v <= 0xFFFF_FFFF {
                    return Err(fault(CborFault::NotShortest));
                }
                v
            }
            28..=30 => return Err(fault(CborFault::ReservedAdditionalInfo)),
            _ => return Err(fault(CborFault::Indefinite)),
        };
        if major == 7 {
            return match (ai, n) {
                (20, _) | (21, _) | (22, _) => Ok((7, u64::from(ai))),
                (25..=27, _) => Err(fault(CborFault::Float)),
                _ => Err(fault(CborFault::SimpleValue)),
            };
        }
        Ok((major, n))
    }

    fn length(&self, n: u64, min_bytes_each: usize) -> Result<usize> {
        let n = usize::try_from(n).map_err(|_| fault(CborFault::LengthExceedsInput))?;
        if n.saturating_mul(min_bytes_each) > self.remaining() {
            return Err(fault(CborFault::LengthExceedsInput));
        }
        Ok(n)
    }

    fn count_item(&mut self) -> Result<()> {
        self.items += 1;
        if self.items > self.limits.max_items {
            return Err(fault(CborFault::TooManyItems));
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > self.limits.max_depth {
            return Err(fault(CborFault::TooDeep));
        }
        self.count_item()?;
        let (major, n) = self.head()?;
        Ok(match major {
            0 => Value::Uint(n),
            1 => Value::Nint(n),
            2 => {
                let len = self.length(n, 1)?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            3 => {
                let len = self.length(n, 1)?;
                let s = self.take(len)?;
                Value::Text(
                    std::str::from_utf8(s)
                        .map_err(|_| fault(CborFault::InvalidUtf8))?
                        .to_owned(),
                )
            }
            4 => {
                let len = self.length(n, 1)?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(self.value(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let len = self.length(n, 2)?;
                let mut entries: Vec<(u64, Value)> = Vec::with_capacity(len);
                for _ in 0..len {
                    self.count_item()?;
                    let (kmajor, key) = self.head()?;
                    if kmajor != 0 {
                        return Err(fault(CborFault::NonIntegerKey));
                    }
                    if let Some((prev, _)) = entries.last() {
                        if key <= *prev {
                            return Err(fault(CborFault::KeyOrder));
                        }
                    }
                    let v = self.value(depth + 1)?;
                    entries.push((key, v));
                }
                Value::Map(entries)
            }
            6 => return Err(fault(CborFault::Tag)),
            7 => match n {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                _ => Value::Null,
            },
            _ => unreachable!("major type is three bits"),
        })
    }
}

/// Decode exactly one canonical value from `bytes`.
pub fn decode(bytes: &[u8], limits: &CborLimits) -> Result<Value> {
    let mut d = Decoder {
        input: bytes,
        pos: 0,
        items: 0,
        limits: *limits,
    };
    let v = d.value(0)?;
    if d.pos != bytes.len() {
        return Err(fault(CborFault::TrailingBytes));
    }
    if encode(&v)? != bytes {
        return Err(fault(CborFault::NotCanonical));
    }
    Ok(v)
}

// ---- schema helpers ---------------------------------------------------------------
//
// Typed readers for the structures built on this codec. Every schema is
// closed: a missing required key and an unknown key are both errors, because a
// new field means a new schema version, never a silently ignored one.

fn schema(msg: impl Into<String>) -> FormatError {
    FormatError::Schema(msg.into())
}

impl Value {
    pub fn uint(&self, what: &str) -> Result<u64> {
        match self {
            Value::Uint(n) => Ok(*n),
            _ => Err(schema(format!("{what}: expected an unsigned integer"))),
        }
    }

    pub fn u32(&self, what: &str) -> Result<u32> {
        u32::try_from(self.uint(what)?).map_err(|_| schema(format!("{what}: out of range")))
    }

    pub fn bytes(&self, what: &str) -> Result<&[u8]> {
        match self {
            Value::Bytes(b) => Ok(b),
            _ => Err(schema(format!("{what}: expected a byte string"))),
        }
    }

    pub fn bytes32(&self, what: &str) -> Result<[u8; 32]> {
        <[u8; 32]>::try_from(self.bytes(what)?)
            .map_err(|_| schema(format!("{what}: expected exactly 32 bytes")))
    }

    pub fn text(&self, what: &str) -> Result<&str> {
        match self {
            Value::Text(t) => Ok(t),
            _ => Err(schema(format!("{what}: expected a text string"))),
        }
    }

    pub fn array(&self, what: &str) -> Result<&[Value]> {
        match self {
            Value::Array(a) => Ok(a),
            _ => Err(schema(format!("{what}: expected an array"))),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// A closed map being read: each key may be taken once; [`Fields::finish`]
/// rejects any key that was not taken.
pub struct Fields<'a> {
    what: &'a str,
    entries: &'a [(u64, Value)],
    used: Vec<bool>,
}

impl<'a> Fields<'a> {
    pub fn of(v: &'a Value, what: &'a str) -> Result<Self> {
        match v {
            Value::Map(entries) => Ok(Fields {
                what,
                entries,
                used: vec![false; entries.len()],
            }),
            _ => Err(schema(format!("{what}: expected a map"))),
        }
    }

    pub fn opt(&mut self, key: u64) -> Option<&'a Value> {
        let i = self.entries.binary_search_by_key(&key, |(k, _)| *k).ok()?;
        self.used[i] = true;
        Some(&self.entries[i].1)
    }

    pub fn req(&mut self, key: u64) -> Result<&'a Value> {
        self.opt(key)
            .ok_or_else(|| schema(format!("{}: missing required key {key}", self.what)))
    }

    pub fn finish(self) -> Result<()> {
        match self.used.iter().position(|u| !u) {
            None => Ok(()),
            Some(i) => Err(schema(format!(
                "{}: unknown key {} (a new field requires a new schema version)",
                self.what, self.entries[i].0
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn lim() -> CborLimits {
        CborLimits::default()
    }

    /// RFC 8949 Appendix A examples that fall inside the subset: each must
    /// encode to exactly the RFC's bytes and decode back.
    #[test]
    fn rfc8949_appendix_a_vectors_in_subset() {
        use Value::*;
        let t = |s: &str| Text(s.to_owned());
        let mut twenty_five: Vec<Value> = (1..=25).map(Uint).collect();
        let cases: Vec<(Value, &str)> = vec![
            (Uint(0), "00"),
            (Uint(1), "01"),
            (Uint(10), "0a"),
            (Uint(23), "17"),
            (Uint(24), "1818"),
            (Uint(25), "1819"),
            (Uint(100), "1864"),
            (Uint(1000), "1903e8"),
            (Uint(1_000_000), "1a000f4240"),
            (Uint(1_000_000_000_000), "1b000000e8d4a51000"),
            (Uint(u64::MAX), "1bffffffffffffffff"),
            (Nint(0), "20"),                        // -1
            (Nint(9), "29"),                        // -10
            (Nint(99), "3863"),                     // -100
            (Nint(999), "3903e7"),                  // -1000
            (Nint(u64::MAX), "3bffffffffffffffff"), // -18446744073709551616
            (Bool(false), "f4"),
            (Bool(true), "f5"),
            (Null, "f6"),
            (Bytes(vec![]), "40"),
            (Bytes(vec![1, 2, 3, 4]), "4401020304"),
            (t(""), "60"),
            (t("a"), "6161"),
            (t("IETF"), "6449455446"),
            (t("\"\\"), "62225c"),
            (t("\u{00fc}"), "62c3bc"),
            (t("\u{6c34}"), "63e6b0b4"),
            (t("\u{10151}"), "64f0908591"),
            (Array(vec![]), "80"),
            (Array(vec![Uint(1), Uint(2), Uint(3)]), "83010203"),
            (
                Array(vec![
                    Uint(1),
                    Array(vec![Uint(2), Uint(3)]),
                    Array(vec![Uint(4), Uint(5)]),
                ]),
                "8301820203820405",
            ),
            (
                Array(std::mem::take(&mut twenty_five)),
                "98190102030405060708090a0b0c0d0e0f101112131415161718181819",
            ),
            (Map(vec![]), "a0"),
            (Map(vec![(1, Uint(2)), (3, Uint(4))]), "a201020304"),
        ];
        for (v, h) in cases {
            let want = hex(h);
            assert_eq!(encode(&v).unwrap(), want, "{v:?}");
            assert_eq!(decode(&want, &lim()).unwrap(), v, "{h}");
        }
    }

    /// Every excluded feature and every non-canonical form, with its fault.
    #[test]
    fn rejections_name_their_fault() {
        use CborFault::*;
        let cases: &[(&str, CborFault)] = &[
            // Floats (RFC 8949 Appendix A examples).
            ("f93c00", Float),
            ("fa47c35000", Float),
            ("fb3ff199999999999a", Float),
            // Other simple values.
            ("f7", SimpleValue),   // undefined
            ("f0", SimpleValue),   // simple(16)
            ("f8ff", SimpleValue), // simple(255)
            // Tags.
            ("c074323031332d30332d32315432303a30343a30305a", Tag),
            ("c11a514b67b0", Tag),
            // Indefinite lengths and a stray break.
            ("5f42010243030405ff", Indefinite),
            ("7f657374726561646d696e67ff", Indefinite),
            ("9fff", Indefinite),
            ("bf6346756ef563416d7421ff", Indefinite),
            ("ff", Indefinite),
            // Reserved additional information.
            ("1c", ReservedAdditionalInfo),
            ("5d", ReservedAdditionalInfo),
            // Non-shortest integers and lengths.
            ("1817", NotShortest),
            ("190017", NotShortest),
            ("1900ff", NotShortest),
            ("1a0000ffff", NotShortest),
            ("1b00000000ffffffff", NotShortest),
            ("3817", NotShortest),
            ("580100", NotShortest),
            ("9800", NotShortest),
            ("b800", NotShortest),
            // Map keys.
            ("a1616101", NonIntegerKey), // {"a": 1}
            ("a12001", NonIntegerKey),   // {-1: 1}
            ("a203040102", KeyOrder),    // {3: 4, 1: 2}
            ("a201020103", KeyOrder),    // {1: 2, 1: 3}
            // Text.
            ("62c328", InvalidUtf8),
            ("63eda080", InvalidUtf8), // an encoded surrogate is not UTF-8
            // Truncation and lengths.
            ("", Truncated),
            ("19", Truncated),
            ("1901", Truncated),
            ("5aff", Truncated),
            ("5affffffff", LengthExceedsInput),
            ("9bffffffffffffffff", LengthExceedsInput),
            ("440102", LengthExceedsInput),
            ("83", LengthExceedsInput),
            ("8301", LengthExceedsInput),
            // Trailing bytes.
            ("0000", TrailingBytes),
            ("f6f6", TrailingBytes),
        ];
        for (h, want) in cases {
            assert_eq!(
                decode(&hex(h), &lim()),
                Err(FormatError::Cbor(*want)),
                "{h}"
            );
        }
    }

    #[test]
    fn depth_and_item_limits() {
        let deep: Vec<u8> = std::iter::repeat_n(0x81, 100).chain([0x00]).collect();
        assert_eq!(
            decode(&deep, &lim()),
            Err(FormatError::Cbor(CborFault::TooDeep))
        );
        let tight = CborLimits {
            max_depth: 64,
            max_items: 3,
        };
        assert_eq!(
            decode(&hex("83010203"), &tight),
            Err(FormatError::Cbor(CborFault::TooManyItems))
        );
        // Map keys count as items too.
        assert_eq!(
            decode(
                &hex("a201020304"),
                &CborLimits {
                    max_depth: 64,
                    max_items: 4
                }
            ),
            Err(FormatError::Cbor(CborFault::TooManyItems))
        );
    }

    /// T4 (B.2.4): `max_items` counts every data item, map keys included.
    /// The structure below is shaped like a manifest file entry and counted
    /// by hand; the codec must agree exactly, at the limit and one below.
    #[test]
    fn item_count_matches_a_hand_counted_structure() {
        use Value::*;
        let chunk = |a, b| Map(vec![(0, Uint(a)), (1, Uint(b))]);
        let v = Map(vec![
            (0, Uint(7)),                                 // key + value: 2
            (1, Bytes(vec![0xAB; 32])),                   // 2
            (2, Array(vec![chunk(0, 10), chunk(10, 5)])), // key 1 + array 1 + 2 × (map 1 + 2 keys + 2 values) = 12
            (3, Null),                                    // 2
            (4, Text("a/b".into())),                      // 2
        ]); //                                               outer map: 1
        let hand_counted = 2 + 2 + 12 + 2 + 2 + 1;
        assert_eq!(hand_counted, 21);
        let bytes = encode(&v).unwrap();
        let at = |n| CborLimits {
            max_depth: 64,
            max_items: n,
        };
        assert_eq!(decode(&bytes, &at(hand_counted)).unwrap(), v);
        assert_eq!(
            decode(&bytes, &at(hand_counted - 1)),
            Err(FormatError::Cbor(CborFault::TooManyItems))
        );
        // The writer counts the same way.
        assert!(encode_within(&v, &at(hand_counted)).is_ok());
        assert!(matches!(
            encode_within(&v, &at(hand_counted - 1)),
            Err(FormatError::CapacityExceeded { actual: 21, .. })
        ));
    }

    /// Depth: root 0; the chunk maps' values are at depth 3.
    #[test]
    fn depth_is_measured_as_the_decoder_measures_it() {
        use Value::*;
        let v = Map(vec![(0, Array(vec![Map(vec![(0, Uint(1))])]))]);
        let bytes = encode(&v).unwrap();
        let at = |d| CborLimits {
            max_depth: d,
            max_items: 1 << 20,
        };
        assert!(decode(&bytes, &at(3)).is_ok());
        assert_eq!(
            decode(&bytes, &at(2)),
            Err(FormatError::Cbor(CborFault::TooDeep))
        );
        assert!(encode_within(&v, &at(3)).is_ok());
        assert!(matches!(
            encode_within(&v, &at(2)),
            Err(FormatError::CapacityExceeded { actual: 3, .. })
        ));
    }

    fn arb_value() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            any::<u64>().prop_map(Value::Uint),
            any::<u64>().prop_map(Value::Nint),
            proptest::collection::vec(any::<u8>(), 0..8).prop_map(Value::Bytes),
            "[a-z]{0,4}".prop_map(Value::Text),
            any::<bool>().prop_map(Value::Bool),
            Just(Value::Null),
        ];
        leaf.prop_recursive(6, 64, 5, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
                proptest::collection::btree_map(0u64..1000, inner, 0..5)
                    .prop_map(|m| Value::Map(m.into_iter().collect())),
            ]
        })
    }

    proptest::proptest! {
        /// T5 / gate G4: the writer refuses exactly what the reader rejects.
        /// Stronger than "no writer output is rejected": also no needless
        /// refusal. Small random limits make both outcomes common.
        #[test]
        fn writer_measure_equals_reader_verdict(
            v in arb_value(),
            max_depth in 0usize..8,
            max_items in 0usize..48,
        ) {
            let limits = CborLimits { max_depth, max_items };
            let bytes = encode(&v).unwrap();
            let reader_ok = decode(&bytes, &limits).is_ok();
            let writer = encode_within(&v, &limits);
            proptest::prop_assert_eq!(writer.is_ok(), reader_ok);
            if let Ok(w) = writer {
                proptest::prop_assert_eq!(w, bytes);
            }
        }

        /// With default limits every writer output decodes to the same value.
        #[test]
        fn default_writer_output_is_accepted_by_default_reader(v in arb_value()) {
            let lim = CborLimits::default();
            if let Ok(bytes) = encode_within(&v, &lim) {
                proptest::prop_assert_eq!(decode(&bytes, &lim).unwrap(), v);
            }
        }
    }

    #[test]
    fn encoder_refuses_unsorted_or_duplicate_keys() {
        for m in [
            Value::Map(vec![(2, Value::Null), (1, Value::Null)]),
            Value::Map(vec![(1, Value::Null), (1, Value::Null)]),
        ] {
            assert!(encode(&m).is_err());
        }
    }

    #[test]
    fn closed_schema_rejects_unknown_and_missing_keys() {
        let v = Value::Map(vec![(1, Value::Uint(5)), (7, Value::Null)]);
        let mut f = Fields::of(&v, "thing").unwrap();
        assert_eq!(f.req(1).unwrap().uint("a").unwrap(), 5);
        assert!(f.finish().is_err(), "key 7 was never read");
        let mut f = Fields::of(&v, "thing").unwrap();
        assert!(f.req(2).is_err());
    }
}
