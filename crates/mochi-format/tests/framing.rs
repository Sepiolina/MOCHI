//! Framing tests: walker (spec §8.6), limits (§8.5), footer (§8.4), and
//! descriptor placement (Annex B.2 D12). Envelopes: `tests/envelope.rs`.
//! Limit boundaries (B.2.3): `tests/limits.rs`.
//!
//! Fault-matrix rows covered here (spec §24.2): false magic inside payload,
//! oversized or malicious frame metadata, truncation at every byte of a small
//! fixture. Differential tests run the walker against libzstd.

// Test code opts out of the library-crate panic rules, as unit tests do.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;

use mochi_format::error::{FooterFault, LimitKind};
use mochi_format::footer::{
    encode_footer_frame, parse_footer_frame, validate_footer, validate_footer_at_eof,
    FOOTER_FRAME_LEN,
};
use mochi_format::frame::{encode_skippable_frame, walk_frame, FrameDetail, Frames};
use mochi_format::registry::{self, FrameKind};
use mochi_format::{ErrorClass, FormatError, Limits};

// ---- helpers -------------------------------------------------------------

fn raw_skippable(magic: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = magic.to_le_bytes().to_vec();
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn data_magic() -> [u8; 4] {
    registry::ZSTD_DATA_FRAME.to_le_bytes()
}

/// Hand-built data frame: descriptor, optional window byte, then `rest`.
fn data_frame(descriptor: u8, window: Option<u8>, rest: &[u8]) -> Vec<u8> {
    let mut v = data_magic().to_vec();
    v.push(descriptor);
    if let Some(w) = window {
        v.push(w);
    }
    v.extend_from_slice(rest);
    v
}

/// Empty content, single segment, 1-byte FCS = 0, one raw last block of size 0.
fn empty_frame() -> Vec<u8> {
    data_frame(0x20, None, &[0x00, 0x01, 0x00, 0x00])
}

/// `"hello"`: single segment, FCS 5, raw last block of 5 bytes.
fn hello_frame() -> Vec<u8> {
    data_frame(
        0x20,
        None,
        &[5, 0x29, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o'],
    )
}

/// Ten `A`s as an RLE block: 1 stored byte, Block_Size = 10.
fn rle_frame() -> Vec<u8> {
    data_frame(0x20, None, &[10, 0x53, 0x00, 0x00, b'A'])
}

/// Two blocks: raw `abc` (not last) then RLE `z` x4 (last). Window 1 KiB.
fn two_block_frame() -> Vec<u8> {
    data_frame(
        0x00,
        Some(0x00),
        &[0x18, 0x00, 0x00, b'a', b'b', b'c', 0x23, 0x00, 0x00, b'z'],
    )
}

/// Empty content with a content checksum (XXH64("") low 32 bits, little-endian).
fn checksum_frame() -> Vec<u8> {
    let mut f = data_frame(0x24, None, &[0x00, 0x01, 0x00, 0x00]);
    f.extend_from_slice(&0x51D8_E999u32.to_le_bytes());
    f
}

struct Session {
    bytes: Vec<u8>,
    commit_offset: u64,
    footer_offset: u64,
}

/// data frame, metadata delta, commit record, footer.
fn session() -> Session {
    let mut bytes = hello_frame();
    bytes.extend(raw_skippable(registry::METADATA_DELTA, b"delta"));
    let commit_offset = bytes.len() as u64;
    let commit = raw_skippable(registry::COMMIT_RECORD, b"commit-body");
    bytes.extend_from_slice(&commit);
    let footer_offset = bytes.len() as u64;
    bytes.extend_from_slice(&encode_footer_frame(commit_offset, 7, &commit));
    Session {
        bytes,
        commit_offset,
        footer_offset,
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

// ---- walker: hand-built frames ---------------------------------------------

#[test]
fn hand_built_frames_have_exact_stored_lengths() {
    let limits = Limits::default();
    for (name, frame, blocks) in [
        ("empty", empty_frame(), 1),
        ("hello", hello_frame(), 1),
        ("rle", rle_frame(), 1),
        ("two-block", two_block_frame(), 2),
        ("checksum", checksum_frame(), 1),
    ] {
        let span = walk_frame(&frame[..], 0, &limits).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(span.len, frame.len() as u64, "{name}");
        assert_eq!(span.kind, FrameKind::ZstdData, "{name}");
        match span.detail {
            FrameDetail::Data(info) => assert_eq!(info.blocks, blocks, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn rle_block_occupies_one_stored_byte_despite_large_block_size() {
    // Non-single-segment, window 128 KiB-capable, RLE block regenerating 100_000 bytes.
    let size: u32 = 100_000;
    let word = (size << 3) | (1 << 1) | 1;
    let w = word.to_le_bytes();
    // Window descriptor exponent 7 => window = 1 << 17 = 128 KiB.
    let frame = data_frame(0x00, Some(7 << 3), &[w[0], w[1], w[2], b'x']);
    let span = walk_frame(&frame[..], 0, &Limits::default()).unwrap();
    assert_eq!(span.len, frame.len() as u64);
    assert_eq!(span.len, 4 + 1 + 1 + 3 + 1);
}

#[test]
fn content_size_is_never_the_physical_length() {
    // FCS says 1 GiB; the frame is 10 bytes of hello-ish content.
    let mut rest = vec![];
    rest.extend_from_slice(&(1u64 << 30).to_le_bytes());
    let mut f = data_magic().to_vec();
    f.push(0b1110_0000); // FCS flag 3 (8 bytes), single segment
    f.extend_from_slice(&rest);
    f.extend_from_slice(&[0x01, 0x00, 0x00]); // raw, last, size 0
                                              // Window == FCS == 1 GiB exceeds the default 128 MiB window limit: rejected,
                                              // not treated as a length.
    assert!(matches!(
        walk_frame(&f[..], 0, &Limits::default()),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::WindowSize,
            ..
        })
    ));
    let big = Limits {
        max_window_size: 2 << 30,
        ..Limits::default()
    };
    let span = walk_frame(&f[..], 0, &big).unwrap();
    assert_eq!(span.len, f.len() as u64);
}

#[test]
fn two_byte_fcs_adds_256() {
    let mut f = data_magic().to_vec();
    f.push(0b0110_0000); // FCS flag 1 (2 bytes), single segment
    f.extend_from_slice(&[0x00, 0x00]); // stored 0 => declared 256
    f.extend_from_slice(&[0x01, 0x00, 0x00]);
    let span = walk_frame(&f[..], 0, &Limits::default()).unwrap();
    match span.detail {
        FrameDetail::Data(i) => {
            assert_eq!(i.declared_content_size, Some(256));
            assert_eq!(i.window_size, 256);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn dictionary_id_field_sizes() {
    for (flag, len) in [(1u8, 1usize), (2, 2), (3, 4)] {
        let mut f = data_magic().to_vec();
        f.push(0x20 | flag);
        f.extend(std::iter::repeat_n(0x05, len)); // Dictionary_ID
        f.push(0x00); // FCS
        f.extend_from_slice(&[0x01, 0x00, 0x00]);
        let span = walk_frame(&f[..], 0, &Limits::default()).unwrap();
        assert_eq!(span.len, f.len() as u64, "flag {flag}");
        match span.detail {
            FrameDetail::Data(i) => assert_ne!(i.dictionary_id, 0),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn reserved_block_type_is_rejected() {
    // Block_Type 3, last block.
    let f = data_frame(0x20, None, &[0x00, 0b0000_0111, 0x00, 0x00]);
    assert!(matches!(
        walk_frame(&f[..], 0, &Limits::default()),
        Err(FormatError::ReservedBlockType { .. })
    ));
}

#[test]
fn reserved_header_bit_is_rejected() {
    let f = data_frame(0x20 | 0x08, None, &[0x00, 0x01, 0x00, 0x00]);
    assert!(matches!(
        walk_frame(&f[..], 0, &Limits::default()),
        Err(FormatError::ReservedHeaderBit { .. })
    ));
}

#[test]
fn block_larger_than_block_maximum_is_rejected() {
    // Window 1 KiB, raw block declares 2000 bytes.
    let word = ((2000u32) << 3) | 1;
    let w = word.to_le_bytes();
    let mut rest = vec![w[0], w[1], w[2]];
    rest.extend(std::iter::repeat_n(0, 2000));
    let f = data_frame(0x00, Some(0x00), &rest);
    assert!(matches!(
        walk_frame(&f[..], 0, &Limits::default()),
        Err(FormatError::BlockTooLarge { max: 1024, .. })
    ));
}

#[test]
fn unknown_magic_is_not_a_frame() {
    let f = [0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0];
    let e = walk_frame(&f[..], 0, &Limits::default()).unwrap_err();
    assert!(matches!(e, FormatError::NotAFrame { .. }));
    assert_eq!(e.class(), ErrorClass::Malformed);
}

// ---- walker: differential against libzstd -----------------------------------

fn compress(data: &[u8], checksum: bool) -> Vec<u8> {
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    enc.include_checksum(checksum).unwrap();
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[test]
fn walker_matches_libzstd_on_real_frames() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut inputs: Vec<Vec<u8>> = vec![
        vec![],
        b"x".to_vec(),
        vec![0u8; 300_000],                   // RLE blocks
        rng.bytes(200_000),                   // raw blocks
        b"mochi mochi mochi ".repeat(40_000), // compressed blocks, several of them
    ];
    let mut mixed = vec![0u8; 70_000];
    mixed.extend(rng.bytes(70_000));
    mixed.extend(b"abc".repeat(50_000));
    inputs.push(mixed);

    let limits = Limits::default();
    for (i, input) in inputs.iter().enumerate() {
        for checksum in [false, true] {
            let frame = compress(input, checksum);
            let span = walk_frame(&frame[..], 0, &limits)
                .unwrap_or_else(|e| panic!("input {i} checksum {checksum}: {e}"));
            assert_eq!(
                span.len,
                frame.len() as u64,
                "input {i} checksum {checksum}"
            );
            match span.detail {
                FrameDetail::Data(info) => assert_eq!(info.has_checksum, checksum),
                other => panic!("{other:?}"),
            }
            assert_eq!(zstd::stream::decode_all(&frame[..]).unwrap(), *input);
        }
    }
}

#[test]
fn hand_built_frames_are_accepted_by_libzstd_too() {
    for (frame, want) in [
        (empty_frame(), Vec::new()),
        (hello_frame(), b"hello".to_vec()),
        (rle_frame(), b"AAAAAAAAAA".to_vec()),
        (two_block_frame(), b"abczzzz".to_vec()),
        (checksum_frame(), Vec::new()),
    ] {
        assert_eq!(zstd::stream::decode_all(&frame[..]).unwrap(), want);
    }
}

#[test]
fn standard_zstd_skips_every_mochi_skippable_frame() {
    // The descriptor at offset 0 (its only valid place, D12), a data frame,
    // every other skippable magic in the range, then another data frame.
    let mut stream = raw_skippable(registry::ARCHIVE_DESCRIPTOR, b"desc");
    stream.extend(compress(b"first", false));
    for magic in registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX {
        if magic != registry::ARCHIVE_DESCRIPTOR {
            stream.extend(raw_skippable(magic, &magic.to_le_bytes()));
        }
    }
    stream.extend(compress(b"second", true));
    // Footer frames stay skippable mid-file after later appends (spec §8.1).
    let s = session();
    stream.extend_from_slice(&s.bytes[s.footer_offset as usize..]);
    stream.extend(compress(b"third", false));

    assert_eq!(
        zstd::stream::decode_all(&stream[..]).unwrap(),
        b"firstsecondthird"
    );

    let spans: Vec<_> = Frames::new(&stream[..], 0, Limits::default())
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(spans.len(), 1 + 16 + 1 + 1 + 1);
    assert_eq!(spans.last().unwrap().end(), stream.len() as u64);
}

// ---- walker: mutation robustness (bounded, never panics) --------------------

#[test]
fn mutated_inputs_never_panic_and_spans_stay_in_bounds() {
    let base = {
        let mut b = session().bytes;
        b.extend(compress(&vec![7u8; 5000], true));
        b
    };
    let limits = Limits {
        max_skippable_payload: 1 << 16,
        max_frame_len: 1 << 20,
        max_blocks_per_frame: 4096,
        max_window_size: 1 << 24,
        max_commit_frame_len: 1 << 16,
        max_decoded_object_len: 1 << 20,
        max_required_features: 64,
        ..Limits::default()
    };
    let mut rng = Rng(0xC0FF_EE00_DEAD_BEEF);
    for _ in 0..3000 {
        let mut b = base.clone();
        for _ in 0..(1 + rng.next() % 4) {
            let i = (rng.next() as usize) % b.len();
            b[i] ^= 1 << (rng.next() % 8);
        }
        if rng.next().is_multiple_of(3) {
            b.truncate((rng.next() as usize) % b.len());
        }
        let mut prev_end = 0u64;
        for item in Frames::new(&b[..], 0, limits) {
            match item {
                Ok(span) => {
                    assert_eq!(span.offset, prev_end);
                    assert!(span.len >= 8 || span.kind == FrameKind::ZstdData);
                    assert!(span.end() <= b.len() as u64);
                    prev_end = span.end();
                }
                Err(_) => break,
            }
        }
        let _ = validate_footer_at_eof(&b[..], &limits);
    }
}

// ---- limits: oversized or malicious frame metadata ---------------------------

#[test]
fn oversized_skippable_size_is_rejected_before_any_allocation() {
    let f = raw_skippable(registry::METADATA_DELTA, &[]);
    let mut huge = f[..4].to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    let e = walk_frame(&huge[..], 0, &Limits::default()).unwrap_err();
    assert!(matches!(
        e,
        FormatError::LimitExceeded {
            kind: LimitKind::SkippablePayload,
            ..
        }
    ));
    assert_eq!(e.class(), ErrorClass::LimitExceeded);

    // Within limits but past end of input: truncation, not a panic or over-read.
    let unbounded = Limits {
        max_skippable_payload: u64::MAX,
        max_frame_len: u64::MAX,
        ..Limits::default()
    };
    assert!(matches!(
        walk_frame(&huge[..], 0, &unbounded),
        Err(FormatError::Truncated { .. })
    ));
}

#[test]
fn frame_length_limit_applies_to_data_frames() {
    let tight = Limits {
        max_frame_len: 5,
        ..Limits::default()
    };
    assert!(matches!(
        walk_frame(&hello_frame()[..], 0, &tight),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::FrameLength,
            ..
        })
    ));
}

#[test]
fn block_count_limit_stops_a_frame_of_empty_blocks() {
    let mut rest = vec![0x00]; // single segment, FCS 0
    for _ in 0..1000 {
        rest.extend_from_slice(&[0x00, 0x00, 0x00]); // raw, not last, size 0
    }
    rest.extend_from_slice(&[0x01, 0x00, 0x00]);
    let f = data_frame(0x20, None, &rest);
    let tight = Limits {
        max_blocks_per_frame: 100,
        ..Limits::default()
    };
    assert!(matches!(
        walk_frame(&f[..], 0, &tight),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::BlocksPerFrame,
            ..
        })
    ));
    assert!(walk_frame(&f[..], 0, &Limits::default()).is_ok());
}

#[test]
fn window_limit_is_enforced() {
    // Exponent 20 => window 1 GiB.
    let f = data_frame(0x00, Some(20 << 3), &[0x01, 0x00, 0x00]);
    assert!(matches!(
        walk_frame(&f[..], 0, &Limits::default()),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::WindowSize,
            ..
        })
    ));
}

// ---- truncation at every byte -----------------------------------------------

#[test]
fn truncation_at_every_byte_never_accepts_a_partial_frame_or_footer() {
    let s = session();
    let limits = Limits::default();
    for cut in 0..s.bytes.len() {
        let prefix = &s.bytes[..cut];
        let mut end = 0u64;
        for item in Frames::new(prefix, 0, limits) {
            match item {
                Ok(span) => {
                    assert!(span.end() <= cut as u64, "cut {cut}");
                    end = span.end();
                }
                Err(e) => {
                    assert!(
                        matches!(e, FormatError::Truncated { .. }),
                        "cut {cut}: expected truncation, got {e}"
                    );
                }
            }
        }
        assert!(end <= cut as u64);
        assert!(
            validate_footer_at_eof(prefix, &limits).is_err(),
            "cut {cut}: accepted a footer from a truncated archive"
        );
    }
    let ok = validate_footer_at_eof(&s.bytes[..], &limits).unwrap();
    assert_eq!(ok.footer_offset, s.footer_offset);
    assert_eq!(ok.fields.commit_offset, s.commit_offset);
    assert_eq!(ok.fields.commit_sequence, 7);
    assert_eq!(ok.commit_kind, FrameKind::CommitRecord);
}

// ---- footer ------------------------------------------------------------------

#[test]
fn footer_is_a_72_byte_skippable_frame() {
    let s = session();
    assert_eq!(FOOTER_FRAME_LEN, 72);
    let span = walk_frame(&s.bytes[..], s.footer_offset, &Limits::default()).unwrap();
    assert_eq!(span.kind, FrameKind::CommitFooter);
    assert_eq!(span.len, 72);
    assert_eq!(span.end(), s.bytes.len() as u64);
}

#[test]
fn footer_fields_round_trip() {
    let s = session();
    let f = parse_footer_frame(&s.bytes[..], s.footer_offset).unwrap();
    assert_eq!(f.commit_offset, s.commit_offset);
    assert_eq!(f.commit_len, 8 + b"commit-body".len() as u64);
    assert_eq!(f.commit_sequence, 7);
}

#[test]
fn every_single_bit_flip_in_footer_or_commit_frame_is_rejected() {
    let s = session();
    let limits = Limits::default();
    let start = s.commit_offset as usize;
    for byte in start..s.bytes.len() {
        for bit in 0..8 {
            let mut b = s.bytes.clone();
            b[byte] ^= 1 << bit;
            assert!(
                validate_footer_at_eof(&b[..], &limits).is_err(),
                "flip byte {byte} bit {bit} was accepted"
            );
        }
    }
}

#[test]
fn bare_64_byte_trailer_without_skippable_header_is_rejected() {
    // v1.2-style trailer: payload bytes at EOF with no preceding skippable header.
    let s = session();
    let payload = &s.bytes[s.footer_offset as usize + 8..];
    let mut b = s.bytes[..s.footer_offset as usize].to_vec();
    b.extend_from_slice(payload);
    // EOF - 72 now lands inside earlier bytes, which are not a footer header.
    let e = validate_footer_at_eof(&b[..], &Limits::default()).unwrap_err();
    assert_eq!(e.class(), ErrorClass::Footer);
}

#[test]
fn wrong_header_size_or_magic_is_a_header_mismatch() {
    let s = session();
    for (idx, val) in [(0usize, 0x00u8), (4, 63), (4, 65)] {
        let mut b = s.bytes.clone();
        b[s.footer_offset as usize + idx] = val;
        assert_eq!(
            validate_footer(&b[..], s.footer_offset, &Limits::default()).unwrap_err(),
            FormatError::Footer(FooterFault::HeaderMismatch),
            "idx {idx}"
        );
    }
}

#[test]
fn footer_naming_a_range_past_itself_or_overflowing_is_rejected() {
    let commit = raw_skippable(registry::COMMIT_RECORD, b"c");
    for (offset, len) in [
        (u64::MAX - 2, commit.len() as u64), // overflow
        (0, u64::MAX),                       // overflow / past footer
        (0, 0),                              // empty
        (0, 7),                              // shorter than a frame header
    ] {
        let mut b = commit.clone();
        let mut footer = encode_footer_frame(0, 1, &commit);
        footer[8 + 8..8 + 16].copy_from_slice(&offset.to_le_bytes());
        footer[8 + 16..8 + 24].copy_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&footer);
        assert!(
            validate_footer_at_eof(&b[..], &Limits::default()).is_err(),
            "offset {offset} len {len}"
        );
    }
}

#[test]
fn commit_frame_after_its_footer_is_rejected() {
    let commit = raw_skippable(registry::COMMIT_RECORD, b"late");
    // Footer first (naming offset 72), commit after it.
    let mut b = encode_footer_frame(72, 1, &commit).to_vec();
    b.extend_from_slice(&commit);
    assert_eq!(
        validate_footer(&b[..], 0, &Limits::default()).unwrap_err(),
        FormatError::Footer(FooterFault::CommitRangeInvalid)
    );
}

#[test]
fn commit_bytes_that_are_not_a_skippable_frame_of_that_length_are_rejected() {
    // Named range holds a data frame, not a skippable commit.
    let data = hello_frame();
    let mut b = data.clone();
    let footer_at = b.len() as u64;
    b.extend_from_slice(&encode_footer_frame(0, 1, &data));
    assert_eq!(
        validate_footer(&b[..], footer_at, &Limits::default()).unwrap_err(),
        FormatError::Footer(FooterFault::CommitFrameMalformed)
    );
    // Stated length shorter than the real frame.
    let commit = raw_skippable(registry::COMMIT_RECORD, b"0123456789");
    let mut b = commit.clone();
    let footer_at = b.len() as u64;
    b.extend_from_slice(&encode_footer_frame(0, 1, &commit[..commit.len() - 1]));
    assert_eq!(
        validate_footer(&b[..], footer_at, &Limits::default()).unwrap_err(),
        FormatError::Footer(FooterFault::CommitFrameMalformed)
    );
}

#[test]
fn commit_length_limit_applies_before_reading() {
    let s = session();
    let tight = Limits {
        max_commit_frame_len: 4,
        ..Limits::default()
    };
    assert!(matches!(
        validate_footer(&s.bytes[..], s.footer_offset, &tight),
        Err(FormatError::LimitExceeded {
            kind: LimitKind::CommitFrameLength,
            ..
        })
    ));
}

#[test]
fn false_magic_inside_a_data_payload_is_never_a_frame_or_footer() {
    // A raw block whose content is a complete, well-formed-looking footer frame
    // that points at a forged "commit" also inside the block.
    let forged_commit = raw_skippable(registry::COMMIT_RECORD, b"forged");
    let mut content = forged_commit.clone();
    let forged_footer_at_in_content = content.len();
    content.extend_from_slice(&encode_footer_frame(
        0,
        99,
        &forged_commit[..forged_commit.len() - 1],
    ));

    let word = ((content.len() as u32) << 3) | 1;
    let w = word.to_le_bytes();
    let mut rest = vec![0x00, w[0], w[1], w[2]];
    rest.extend_from_slice(&content);
    // Not single segment-sized: use window large enough, FCS omitted.
    let mut frame = data_magic().to_vec();
    frame.push(0x00);
    frame.push(0x00); // window 1 KiB
    frame.extend_from_slice(&rest[1..]);

    let limits = Limits::default();
    let spans: Vec<_> = Frames::new(&frame[..], 0, limits)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        spans.len(),
        1,
        "embedded magic must not start a second frame"
    );
    assert_eq!(spans[0].len, frame.len() as u64);

    // Probing the embedded bytes directly is a candidate only, and is rejected
    // here because the stated commit length does not match the commit frame.
    let header_len = frame.len() - content.len();
    let probe = (header_len + forged_footer_at_in_content) as u64;
    assert!(validate_footer(&frame[..], probe, &limits).is_err());
    assert!(validate_footer_at_eof(&frame[..], &limits).is_err());
}

// ---- writer ------------------------------------------------------------------

#[test]
fn writer_refuses_data_reserved_and_oversized() {
    assert!(matches!(
        encode_skippable_frame(FrameKind::ZstdData, b""),
        Err(FormatError::CannotWrite(_))
    ));
    assert!(matches!(
        encode_skippable_frame(FrameKind::Reserved, b""),
        Err(FormatError::CannotWrite(_))
    ));
    let frame = encode_skippable_frame(FrameKind::MetadataDelta, b"hi").unwrap();
    assert_eq!(frame, raw_skippable(registry::METADATA_DELTA, b"hi"));
    let span = walk_frame(&frame[..], 0, &Limits::default()).unwrap();
    assert_eq!(span.len, 10);
}

// ---- descriptor placement (Annex B.2 D12, plan T1) ---------------------------

fn descriptor_frame() -> Vec<u8> {
    raw_skippable(registry::ARCHIVE_DESCRIPTOR, b"descriptor")
}

#[test]
fn descriptor_at_offset_zero_walks() {
    let mut b = descriptor_frame();
    b.extend(raw_skippable(registry::COMMIT_RECORD, b"c"));
    let kinds: Vec<FrameKind> = Frames::new(&b[..], 0, Limits::default())
        .map(|s| s.unwrap().kind)
        .collect();
    assert_eq!(
        kinds,
        [FrameKind::ArchiveDescriptor, FrameKind::CommitRecord]
    );
}

#[test]
fn descriptor_anywhere_else_is_misplaced() {
    let mut b = raw_skippable(registry::COMMIT_RECORD, b"c");
    let at = b.len() as u64;
    b.extend(descriptor_frame());
    let mut walk = Frames::new(&b[..], 0, Limits::default());
    assert_eq!(walk.next().unwrap().unwrap().kind, FrameKind::CommitRecord);
    let e = walk.next().unwrap().unwrap_err();
    assert_eq!(
        e,
        FormatError::MisplacedFrame {
            offset: at,
            magic: registry::ARCHIVE_DESCRIPTOR
        }
    );
    assert_eq!(e.class(), ErrorClass::Malformed);
    assert!(walk.next().is_none(), "the walk is fused after an error");
}

/// The check runs on the magic: a descriptor cut short by EOF away from
/// offset 0 is misplaced, not merely truncated. A tail ending in one must
/// never look like an interrupted write (D14 eligibility).
#[test]
fn truncated_descriptor_off_zero_is_misplaced_not_truncated() {
    let mut b = raw_skippable(registry::COMMIT_RECORD, b"c");
    let at = b.len();
    b.extend(descriptor_frame());
    for cut in at + 4..b.len() {
        assert!(
            matches!(
                walk_frame(&b[..cut], at as u64, &Limits::default()),
                Err(FormatError::MisplacedFrame { .. })
            ),
            "cut at {cut}"
        );
    }
    // Fewer than 4 bytes: the magic itself is not there, so this is truncation.
    assert!(matches!(
        walk_frame(&b[..at + 3], at as u64, &Limits::default()),
        Err(FormatError::Truncated { .. })
    ));
}

/// A footer header flipped to the descriptor magic (G8's single-event case)
/// is misplaced wherever a footer can sit, which is never offset 0.
#[test]
fn footer_header_flipped_to_descriptor_is_misplaced() {
    let s = session();
    let mut b = s.bytes.clone();
    let footer_at = b.len() - FOOTER_FRAME_LEN as usize;
    b[footer_at..footer_at + 4].copy_from_slice(&registry::ARCHIVE_DESCRIPTOR.to_le_bytes());
    let last = Frames::new(&b[..], 0, Limits::default()).last().unwrap();
    assert!(
        matches!(last, Err(FormatError::MisplacedFrame { offset, .. }) if offset == footer_at as u64)
    );
}
