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
    set_u32(&mut p, 8, 3);
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::UnknownPayloadEncoding)
    );
    // 2 is registered (a compressed image), but this payload is not a
    // Zstandard frame.
    set_u32(&mut p, 8, 2);
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::EncodingSignatureMismatch)
    );
    // 1 is registered (a parity shard, D21), but not for a catalog image.
    set_u32(&mut p, 8, 1);
    assert_eq!(
        decode(&p).unwrap_err(),
        fault(EnvelopeFault::EncodingKindMismatch)
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

// ---- payload encoding 2: a Zstandard-compressed SQLite image (B.2.2) -------------------

mod compressed_image {
    use super::*;
    use mochi_format::codec::compress_frame;
    use mochi_format::envelope::{compress_image, decode_image};

    fn env2(schema: u16) -> BinaryEnvelope {
        BinaryEnvelope {
            encoding: PayloadEncoding::ZstdSqliteImage,
            ..env(schema, vec![])
        }
    }

    /// A frame carrying `payload` under encoding 2, and its decode result.
    fn decode2(payload: &[u8], limits: &Limits) -> Result<Vec<u8>, FormatError> {
        let frame = encode_binary_record(
            FrameKind::MetadataDelta,
            &env2(0),
            payload,
            KNOWN,
            &Limits::default(),
        )?;
        let (e, p) = decode_binary_record(registry::METADATA_DELTA, &frame[8..], &rules(), limits)
            .and_then(|c| c.bind(&id()))?;
        decode_image(&e, p, limits).map(|c| c.into_owned())
    }

    #[test]
    fn round_trip_and_the_frame_meets_the_o21_profile() {
        let image = sqlite_image(0, 5000);
        let payload = compress_image(&env2(0), &image, &Limits::default()).unwrap();
        assert!(payload.starts_with(&registry::ZSTD_IMAGE_SIGNATURE));
        assert!(payload.len() < image.len() / 10);
        assert_eq!(decode2(&payload, &Limits::default()).unwrap(), image);
        // Deterministic: the same image compresses to the same bytes.
        assert_eq!(
            payload,
            compress_image(&env2(0), &image, &Limits::default()).unwrap()
        );
    }

    #[test]
    fn decoded_bytes_that_are_not_sqlite_are_refused() {
        let payload = compress_frame(&vec![7u8; 4096], 3).unwrap();
        assert_eq!(
            decode2(&payload, &Limits::default()).unwrap_err(),
            fault(EnvelopeFault::DecodedSignatureMismatch)
        );
        // The writer refuses to compress it in the first place.
        assert_eq!(
            compress_image(&env2(0), &vec![7u8; 4096], &Limits::default()).unwrap_err(),
            fault(EnvelopeFault::DecodedSignatureMismatch)
        );
    }

    #[test]
    fn the_decoded_user_version_must_equal_the_record_schema_version() {
        let payload = compress_frame(&sqlite_image(1, 100), 3).unwrap();
        assert_eq!(
            decode2(&payload, &Limits::default()).unwrap_err(),
            fault(EnvelopeFault::SchemaVersionMismatch)
        );
    }

    /// The frame must declare its size and carry the checksum (O21).
    #[test]
    fn a_frame_without_content_size_or_checksum_is_refused() {
        use std::io::Write;
        let image = sqlite_image(0, 1000);
        // Streaming encoder without `pledged_src_size`: no Frame_Content_Size.
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        enc.include_checksum(true).unwrap();
        enc.write_all(&image).unwrap();
        let no_size = enc.finish().unwrap();
        let e = decode2(&no_size, &Limits::default()).unwrap_err();
        assert!(matches!(
            e,
            FormatError::Codec(mochi_format::error::CodecFault::MissingContentSize)
        ));
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        enc.set_pledged_src_size(Some(image.len() as u64)).unwrap();
        enc.include_checksum(false).unwrap();
        enc.write_all(&image).unwrap();
        let no_sum = enc.finish().unwrap();
        let e = decode2(&no_sum, &Limits::default()).unwrap_err();
        assert!(matches!(
            e,
            FormatError::Codec(mochi_format::error::CodecFault::MissingChecksum)
        ));
    }

    /// Codes pinned for a payload that is not a frame (spec B.2.15 item 1): no
    /// magic, even an empty payload or a 2-byte prefix of it, is an envelope
    /// signature fault; cut short after the magic is truncation.
    #[test]
    fn a_payload_without_or_cut_after_the_magic_has_its_own_fault() {
        let good = compress_image(&env2(0), &sqlite_image(0, 1000), &Limits::default()).unwrap();
        for short in [&good[..0], &good[..2], &good[..3]] {
            assert_eq!(
                decode2(short, &Limits::default()).unwrap_err(),
                fault(EnvelopeFault::EncodingSignatureMismatch),
                "{} bytes",
                short.len()
            );
        }
        for cut in [4, 5, 10, good.len() - 1] {
            let e = decode2(&good[..cut], &Limits::default()).unwrap_err();
            assert!(matches!(e, FormatError::Truncated { .. }), "{cut}: {e:?}");
            assert_eq!(e.class(), ErrorClass::Truncated);
        }
    }

    /// Trailing bytes, a second frame, and a damaged payload are all refused.
    #[test]
    fn extra_frames_and_damage_are_refused() {
        let image = sqlite_image(0, 1000);
        let good = compress_image(&env2(0), &image, &Limits::default()).unwrap();
        let mut two = good.clone();
        two.extend_from_slice(&good);
        assert!(decode2(&two, &Limits::default()).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode2(&trailing, &Limits::default()).is_err());
        let mut damaged = good.clone();
        let mid = damaged.len() / 2;
        damaged[mid] ^= 0x40;
        assert!(decode2(&damaged, &Limits::default()).is_err());
        let mut sum = good;
        let last = sum.len() - 1;
        sum[last] ^= 1; // the content checksum
        assert!(decode2(&sum, &Limits::default()).is_err());
    }

    /// The decoded-size bound is the image payload budget, applied to the
    /// *declared* size before any allocation: a frame header declaring
    /// 2^40 bytes (and nothing else) is refused without trying to read it.
    #[test]
    fn a_declared_size_over_the_bound_is_refused_before_allocating() {
        // Not single-segment (so the window is small and the window limit
        // does not fire first): descriptor with Frame_Content_Size_flag = 3
        // (8 bytes) and the content checksum, a 1 KiB window descriptor, the
        // declared size, one empty last raw block, the checksum.
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0b1100_0100, 0x00];
        frame.extend_from_slice(&(1u64 << 40).to_le_bytes());
        frame.extend_from_slice(&[0x01, 0x00, 0x00]);
        frame.extend_from_slice(&[0; 4]);
        let e = decode2(&frame, &Limits::default()).unwrap_err();
        assert_eq!(
            e,
            FormatError::LimitExceeded {
                kind: LimitKind::ImageDecoded,
                limit: image_payload_budget(&Limits::default()),
                actual: 1 << 40,
            }
        );
        assert_eq!(e.class(), ErrorClass::LimitExceeded);

        // One byte over the bound, exactly.
        let over = image_payload_budget(&Limits::default()) + 1;
        let mut frame = vec![0x28, 0xB5, 0x2F, 0xFD, 0b1100_0100, 0x00];
        frame.extend_from_slice(&over.to_le_bytes());
        frame.extend_from_slice(&[0x01, 0x00, 0x00]);
        frame.extend_from_slice(&[0; 4]);
        assert!(matches!(
            decode2(&frame, &Limits::default()).unwrap_err(),
            FormatError::LimitExceeded {
                kind: LimitKind::ImageDecoded,
                ..
            }
        ));
    }

    /// A smaller reader limit moves the bound with it.
    #[test]
    fn the_bound_follows_the_reader_limit() {
        let image = sqlite_image(0, 20_000);
        let payload = compress_image(&env2(0), &image, &Limits::default()).unwrap();
        let small = Limits {
            max_skippable_payload: 592 + 10_000,
            ..Limits::default()
        };
        let e = decode2(&payload, &small).unwrap_err();
        assert!(
            matches!(
                e,
                FormatError::LimitExceeded {
                    kind: LimitKind::ImageDecoded,
                    ..
                }
            ),
            "{e:?}"
        );
        // A writer under those limits refuses the image too.
        assert!(matches!(
            compress_image(&env2(0), &image, &small).unwrap_err(),
            FormatError::CapacityExceeded { .. }
        ));
    }

    #[test]
    fn encoding_2_is_for_catalog_images_only() {
        let payload = compress_frame(&sqlite_image(0, 100), 3).unwrap();
        let e = encode_binary_record(
            FrameKind::ParityObject,
            &env2(0),
            &payload,
            KNOWN,
            &Limits::default(),
        )
        .unwrap_err();
        assert_eq!(e, fault(EnvelopeFault::EncodingKindMismatch));
    }
}
