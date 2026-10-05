//! Binary envelope v0 (spec Annex B.2.2) and the D11 obligations (plan T2).
//!
//! One test per obligation, each asserting its own fault, then the writer
//! refusing the same inputs (plan T5: a writer never emits what a reader
//! rejects). The integrity-scope obligation is checked by `mochi-core`
//! (stored-object hash before parsing; `c5_publication.rs`), not here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mochi_format::envelope::{
    check_required_features, decode_binary_record, encode_binary_record, encode_cbor_record,
    image_payload_budget, BinaryEnvelope, EnvelopeRules, PayloadEncoding, RecordIdentity,
    BINARY_ENVELOPE_FIXED_LEN,
};
use mochi_format::error::{CapacityKind, EnvelopeFault, LimitKind};
use mochi_format::frame::walk_frame;
use mochi_format::limits::DEFAULT_IMAGE_PAYLOAD;
use mochi_format::registry::{self, FrameKind};
use mochi_format::{ErrorClass, FormatError, Limits};

const KNOWN: &[u64] = &[3, 5, 9];

fn id() -> RecordIdentity {
    RecordIdentity {
        archive_id: [0xA1; 32],
        commit_sequence: 7,
        transaction_id: [0xB2; 16],
    }
}

/// The 100-byte SQLite header with only what the envelope checks set: the
/// signature and `user_version` (big-endian i32 at 60). Opening the image is
/// `mochi-core`'s job and is not exercised here.
fn sqlite_image(user_version: i32, extra: usize) -> Vec<u8> {
    let mut h = vec![0u8; 100 + extra];
    h[..16].copy_from_slice(&registry::SQLITE_IMAGE_SIGNATURE);
    h[60..64].copy_from_slice(&user_version.to_be_bytes());
    h
}

fn env(schema: u16, features: Vec<u64>) -> BinaryEnvelope {
    BinaryEnvelope {
        record_schema_version: schema,
        encoding: PayloadEncoding::SqliteImage,
        identity: id(),
        required_features: features,
    }
}

fn rules() -> EnvelopeRules<'static> {
    EnvelopeRules {
        kind: FrameKind::MetadataDelta,
        schema_versions: &[0, 1],
        known_features: KNOWN,
    }
}

/// A valid frame and its payload (after the 8-byte skippable header).
fn valid(features: Vec<u64>) -> Vec<u8> {
    encode_binary_record(
        FrameKind::MetadataDelta,
        &env(0, features),
        &sqlite_image(0, 20),
        KNOWN,
        &Limits::default(),
    )
    .unwrap()
}

fn decode(payload: &[u8]) -> Result<(BinaryEnvelope, Vec<u8>), FormatError> {
    decode_binary_record(
        registry::METADATA_DELTA,
        payload,
        &rules(),
        &Limits::default(),
    )
    .and_then(|c| c.bind(&id()))
    .map(|(e, p)| (e, p.to_vec()))
}

fn fault(f: EnvelopeFault) -> FormatError {
    FormatError::Envelope(f)
}

fn set_u32(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn set_u64(p: &mut [u8], at: usize, v: u64) {
    p[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

// ---- layout ------------------------------------------------------------------

#[test]
fn round_trip_and_exact_layout() {
    let frame = valid(vec![3, 9]);
    let span = walk_frame(&frame[..], 0, &Limits::default()).unwrap();
    assert_eq!(span.len, frame.len() as u64);
    let p = &frame[8..];
    // Every header byte, at its B.2.2 offset.
    assert_eq!(&p[0..4], &(80u32 + 16).to_le_bytes(), "header length");
    assert_eq!(&p[4..6], &0u16.to_le_bytes(), "envelope version");
    assert_eq!(&p[6..8], &0u16.to_le_bytes(), "record schema version");
    assert_eq!(&p[8..12], &0u32.to_le_bytes(), "payload encoding");
    assert_eq!(&p[12..16], &2u32.to_le_bytes(), "n");
    assert_eq!(&p[16..24], &120u64.to_le_bytes(), "payload length");
    assert_eq!(&p[24..56], &[0xA1; 32], "archive id");
    assert_eq!(&p[56..64], &7u64.to_le_bytes(), "sequence");
    assert_eq!(&p[64..80], &[0xB2; 16], "transaction id");
    assert_eq!(&p[80..88], &3u64.to_le_bytes());
    assert_eq!(&p[88..96], &9u64.to_le_bytes());
    assert_eq!(&p[96..], &sqlite_image(0, 20)[..]);

    let (e, body) = decode(p).unwrap();
    assert_eq!(e, env(0, vec![3, 9]));
    assert_eq!(body, sqlite_image(0, 20));
}

#[test]
fn zero_features_header_is_80_bytes() {
    let frame = valid(vec![]);
    assert_eq!(frame.len(), 8 + BINARY_ENVELOPE_FIXED_LEN + 120);
    assert!(decode(&frame[8..]).is_ok());
}

// ---- one test per obligation -------------------------------------------------

/// Record type: the frame kind matches the kind the referencing field names.
#[test]
fn obligation_record_type() {
    let frame = valid(vec![]);
    let p = &frame[8..];
    // A kind that never carries a binary envelope.
    for magic in [
        registry::RECOVERY_MANIFEST,
        registry::COMMIT_RECORD,
        registry::COMMIT_FOOTER,
        registry::ZSTD_DATA_FRAME,
        0,
    ] {
        assert_eq!(
            decode_binary_record(magic, p, &rules(), &Limits::default()).unwrap_err(),
            fault(EnvelopeFault::KindHasNoEnvelope),
            "{magic:#x}"
        );
    }
    // A binary-envelope frame where the referencing field expects another kind.
    let other = EnvelopeRules {
        kind: FrameKind::CompressionDictionary,
        ..rules()
    };
    assert_eq!(
        decode_binary_record(registry::METADATA_DELTA, p, &other, &Limits::default()).unwrap_err(),
        fault(EnvelopeFault::RecordTypeMismatch)
    );
}

/// Schema version: unknown envelope version or record schema version refuses
/// (Unsupported, exit 4); for SQLite the version must equal `user_version`.
#[test]
fn obligation_schema_version() {
    let frame = valid(vec![]);
    let mut p = frame[8..].to_vec();
    p[4] = 1;
    assert_eq!(
        decode(&p).unwrap_err(),
        FormatError::UnsupportedEnvelopeVersion { version: 1 }
    );
    assert_eq!(decode(&p).unwrap_err().class(), ErrorClass::Unsupported);

    let mut p = frame[8..].to_vec();
    p[6] = 2; // not in rules().schema_versions
    assert_eq!(
        decode(&p).unwrap_err(),
        FormatError::UnsupportedSchema { version: 2 }
    );

    // Known version, but the image's user_version disagrees.
    let mut p = frame[8..].to_vec();
    p[6] = 1;
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::SchemaVersionMismatch)
    );
    // ... and a negative user_version never matches.
    let mut p = frame[8..].to_vec();
    let img = 80;
    p[img + 60..img + 64].copy_from_slice(&(-1i32).to_be_bytes());
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::SchemaVersionMismatch)
    );
}

/// Required features: unknown refuses; the list is strictly increasing; at
/// most 64 in v0; at most the reader limit.
#[test]
fn obligation_required_features() {
    let frame = valid(vec![3, 9]);
    let p = frame[8..].to_vec();

    let mut q = p.clone();
    set_u64(&mut q, 80, 4); // [4, 9]: 4 unknown
    assert_eq!(
        decode(&q).unwrap_err(),
        FormatError::UnsupportedRequiredFeature { feature: 4 }
    );
    assert_eq!(decode(&q).unwrap_err().class(), ErrorClass::Unsupported);

    let mut q = p.clone();
    set_u64(&mut q, 88, 3); // [3, 3]
    assert_eq!(
        decode(&q).unwrap_err(),
        fault(EnvelopeFault::FeaturesNotIncreasing)
    );
    let mut q = p.clone();
    set_u64(&mut q, 80, 9);
    set_u64(&mut q, 88, 3); // [9, 3]
    assert_eq!(
        decode(&q).unwrap_err(),
        fault(EnvelopeFault::FeaturesNotIncreasing)
    );

    // n = 65 breaks the v0 wire rule, before n is trusted for the header length.
    let mut q = p.clone();
    set_u32(&mut q, 12, 65);
    assert_eq!(
        decode(&q).unwrap_err(),
        fault(EnvelopeFault::FeatureCountOutOfRange)
    );
    let mut q = p.clone();
    set_u32(&mut q, 12, u32::MAX);
    assert_eq!(
        decode(&q).unwrap_err(),
        fault(EnvelopeFault::FeatureCountOutOfRange)
    );

    // A reader limit below 64 applies too.
    let tight = Limits {
        max_required_features: 1,
        ..Limits::default()
    };
    let e = decode_binary_record(registry::METADATA_DELTA, &p, &rules(), &tight).unwrap_err();
    assert_eq!(
        e,
        FormatError::LimitExceeded {
            kind: LimitKind::RequiredFeatures,
            limit: 1,
            actual: 2
        }
    );
}

/// Header length must be 80 + 8·n.
#[test]
fn obligation_header_length() {
    let frame = valid(vec![3]);
    for bad in [0u32, 80, 87, 89, 96, u32::MAX] {
        let mut p = frame[8..].to_vec();
        set_u32(&mut p, 0, bad);
        assert_eq!(
            decode(&p).unwrap_err(),
            fault(EnvelopeFault::HeaderLengthMismatch),
            "{bad}"
        );
    }
}

/// Payload encoding: registered value, and the payload carries its signature.
#[test]
fn obligation_payload_encoding() {
    let frame = valid(vec![]);
    let mut p = frame[8..].to_vec();
    set_u32(&mut p, 8, 1);
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::UnknownPayloadEncoding)
    );

    let mut p = frame[8..].to_vec();
    p[80] ^= 0x20; // "sQLite format 3\0"
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::EncodingSignatureMismatch)
    );
}

/// Payload length: the header's value equals frame payload − header length.
#[test]
fn obligation_payload_length() {
    let frame = valid(vec![]);
    let p = frame[8..].to_vec();
    for delta in [-1i64, 1, i64::MAX] {
        let mut q = p.clone();
        let v = 120u64.wrapping_add_signed(delta);
        set_u64(&mut q, 16, v);
        assert_eq!(
            decode(&q).unwrap_err(),
            fault(EnvelopeFault::PayloadLengthMismatch),
            "{v}"
        );
    }
    // The frame payload itself one byte short: header says 120, 119 remain.
    assert_eq!(
        decode(&p[..p.len() - 1]).unwrap_err(),
        fault(EnvelopeFault::PayloadLengthMismatch)
    );
    // Shorter than the fixed header, or than the features it announces.
    assert_eq!(
        decode(&p[..79]).unwrap_err(),
        fault(EnvelopeFault::TooShort)
    );
    let with_f = valid(vec![3]);
    assert_eq!(
        decode(&with_f[8..8 + 85]).unwrap_err(),
        fault(EnvelopeFault::TooShort)
    );
    // A SQLite payload too short to hold user_version.
    let mut q = p[..80 + 16].to_vec();
    set_u64(&mut q, 16, 16);
    assert_eq!(decode(&q).unwrap_err(), fault(EnvelopeFault::TooShort));
}

/// Archive identity and Identity: each field must equal the referencing
/// commit's, each with its own fault. Unbound candidates expose fields only
/// for diagnostics.
#[test]
fn obligation_identity() {
    let frame = valid(vec![]);
    let cand = || {
        decode_binary_record(
            registry::METADATA_DELTA,
            &frame[8..],
            &rules(),
            &Limits::default(),
        )
        .unwrap()
    };
    assert_eq!(cand().unbound_envelope().identity, id());

    let mut other = id();
    other.archive_id[31] ^= 1;
    assert_eq!(
        cand().bind(&other).unwrap_err(),
        fault(EnvelopeFault::ArchiveIdMismatch)
    );
    let mut other = id();
    other.commit_sequence = 8;
    assert_eq!(
        cand().bind(&other).unwrap_err(),
        fault(EnvelopeFault::SequenceMismatch)
    );
    let mut other = id();
    other.transaction_id[0] ^= 1;
    assert_eq!(
        cand().bind(&other).unwrap_err(),
        fault(EnvelopeFault::TransactionIdMismatch)
    );
    assert!(cand().bind(&id()).is_ok());
}

// ---- boundaries --------------------------------------------------------------

#[test]
fn sixty_four_features_pass_sixty_five_do_not() {
    let known: Vec<u64> = (1..=65).collect();
    let r = EnvelopeRules {
        known_features: &known,
        ..rules()
    };
    let enc = |n: u64| {
        encode_binary_record(
            FrameKind::MetadataDelta,
            &env(0, (1..=n).collect()),
            &sqlite_image(0, 0),
            &known,
            &Limits::default(),
        )
    };
    let f = enc(64).unwrap();
    assert_eq!(&f[8..12], &(80u32 + 512).to_le_bytes());
    assert!(
        decode_binary_record(registry::METADATA_DELTA, &f[8..], &r, &Limits::default()).is_ok()
    );
    assert_eq!(
        enc(65).unwrap_err(),
        fault(EnvelopeFault::FeatureCountOutOfRange)
    );
    // Raising the reader limit does not widen the v0 wire rule.
    let raised = Limits {
        max_required_features: 1000,
        ..Limits::default()
    };
    let mut p = f[8..].to_vec();
    set_u32(&mut p, 12, 65);
    assert_eq!(
        decode_binary_record(registry::METADATA_DELTA, &p, &r, &raised).unwrap_err(),
        fault(EnvelopeFault::FeatureCountOutOfRange)
    );
}

/// The image budget is S − 592 (B.2.3), the same on both sides. Tested at a
/// small S; the default is pinned against B.2.3's figure.
#[test]
fn image_budget_is_s_minus_592_on_both_sides() {
    assert_eq!(
        image_payload_budget(&Limits::default()),
        DEFAULT_IMAGE_PAYLOAD
    );
    assert_eq!(DEFAULT_IMAGE_PAYLOAD, 268_434_864);

    let small = Limits {
        max_skippable_payload: 4096,
        ..Limits::default()
    };
    let budget = image_payload_budget(&small);
    assert_eq!(budget, 4096 - 592);
    let enc = |len: u64| {
        encode_binary_record(
            FrameKind::MetadataDelta,
            &env(0, vec![]),
            &sqlite_image(0, len as usize - 100),
            KNOWN,
            &small,
        )
    };
    let ok = enc(budget).unwrap();
    assert!(decode_binary_record(registry::METADATA_DELTA, &ok[8..], &rules(), &small).is_ok());
    assert_eq!(
        enc(budget + 1).unwrap_err(),
        FormatError::CapacityExceeded {
            kind: CapacityKind::EnvelopePayload,
            limit: budget,
            actual: budget + 1
        }
    );
    // A reader rejects an over-budget image even when the frame fits S
    // (possible because a header with n < 64 is shorter than 592 bytes).
    let roomy = Limits {
        max_skippable_payload: 8192,
        ..Limits::default()
    };
    let over = encode_binary_record(
        FrameKind::MetadataDelta,
        &env(0, vec![]),
        &sqlite_image(0, budget as usize + 1 - 100),
        KNOWN,
        &roomy,
    )
    .unwrap();
    assert!(over.len() - 8 <= 4096, "the frame payload itself fits S");
    assert!(matches!(
        decode_binary_record(registry::METADATA_DELTA, &over[8..], &rules(), &small),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::ImagePayload,
            ..
        })
    ));
}

// ---- writer refuses what the reader rejects (T5) -----------------------------

#[test]
fn writer_refuses_every_rejectable_input() {
    let w = |e: &BinaryEnvelope, payload: &[u8], kind: FrameKind| {
        encode_binary_record(kind, e, payload, KNOWN, &Limits::default()).unwrap_err()
    };
    let img = sqlite_image(0, 0);
    let k = FrameKind::MetadataDelta;
    assert_eq!(
        w(&env(0, vec![]), &img, FrameKind::RecoveryManifest),
        fault(EnvelopeFault::KindHasNoEnvelope)
    );
    assert_eq!(
        w(&env(0, vec![4]), &img, k),
        FormatError::UnsupportedRequiredFeature { feature: 4 }
    );
    assert_eq!(
        w(&env(0, vec![9, 3]), &img, k),
        fault(EnvelopeFault::FeaturesNotIncreasing)
    );
    assert_eq!(
        w(&env(1, vec![]), &img, k),
        fault(EnvelopeFault::SchemaVersionMismatch)
    );
    assert_eq!(
        w(&env(0, vec![]), b"not sqlite", k),
        fault(EnvelopeFault::EncodingSignatureMismatch)
    );
    assert_eq!(
        w(&env(0, vec![]), &img[..32], k),
        fault(EnvelopeFault::TooShort)
    );
}

#[test]
fn shared_feature_check_orders_limit_then_order_then_known() {
    let l1 = Limits {
        max_required_features: 1,
        ..Limits::default()
    };
    // Over the limit wins even when the list is also unsorted and unknown.
    assert!(matches!(
        check_required_features(&[99, 1], &[], &l1),
        Err(FormatError::LimitExceeded { .. })
    ));
    assert_eq!(
        check_required_features(&[99, 1], &[], &Limits::default()).unwrap_err(),
        fault(EnvelopeFault::FeaturesNotIncreasing)
    );
    assert_eq!(
        check_required_features(&[1, 99], &[1], &Limits::default()).unwrap_err(),
        FormatError::UnsupportedRequiredFeature { feature: 99 }
    );
    assert!(check_required_features(&[], &[], &Limits::default()).is_ok());
}

#[test]
fn cbor_record_writer_refuses_binary_and_structural_kinds() {
    use mochi_format::cbor::{CborLimits, Value};
    let v = Value::Map(vec![(0, Value::Uint(1))]);
    for kind in [
        FrameKind::MetadataDelta,
        FrameKind::CommitFooter,
        FrameKind::ZstdData,
    ] {
        assert!(matches!(
            encode_cbor_record(kind, &v, &CborLimits::default(), &Limits::default()),
            Err(FormatError::CannotWrite(_))
        ));
    }
    let f = encode_cbor_record(
        FrameKind::ArchiveDescriptor,
        &v,
        &CborLimits::default(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(&f[8..], &[0xA1, 0x00, 0x01]);
}

// ---- robustness --------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn decoder_never_panics_on_mutations() {
    let frame = valid(vec![3, 5, 9]);
    let base = &frame[8..];
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let mut p = base.to_vec();
        for _ in 0..(1 + rng.next() % 3) {
            let i = (rng.next() as usize) % p.len();
            p[i] = rng.next() as u8;
        }
        let cut = (rng.next() as usize) % (p.len() + 1);
        if let Ok(c) = decode_binary_record(
            registry::METADATA_DELTA,
            &p[..cut],
            &rules(),
            &Limits::default(),
        ) {
            let _ = c.bind(&id());
        }
        let _ = decode_binary_record(rng.next() as u32, &p, &rules(), &Limits::default());
    }
}
