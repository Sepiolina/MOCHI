//! Default limits at value − 1, value, value + 1 (spec Annex B.2.3; plan T3,
//! gate G4), on the reader and on the writer.
//!
//! The large limits are exercised at their real default values without
//! allocating them: [`Sparse`] materialises only frame and block headers,
//! and the walker never reads payload bytes. That is a property of the
//! walker (it bounds-checks lengths, then skips), which `walker_reads_only_headers`
//! pins so these tests cannot silently stop meaning anything.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::collections::BTreeMap;

use mochi_format::error::{CapacityKind, LimitKind};
use mochi_format::footer::{encode_footer_frame, validate_footer_at_eof};
use mochi_format::frame::{encode_skippable_frame_within, walk_frame};
use mochi_format::limits::{
    zstd_compress_bound_large, DEFAULT_COMMIT_FRAME_LEN, DEFAULT_FRAME_LEN,
    DEFAULT_REQUIRED_FEATURES, DEFAULT_SKIPPABLE_PAYLOAD,
};
use mochi_format::registry::{self, FrameKind};
use mochi_format::source::{ReadAt, ReadError};
use mochi_format::{FormatError, Limits};

/// A byte source of length `len` that is zero except for `patches`, and that
/// counts how many bytes were read.
struct Sparse {
    len: u64,
    patches: BTreeMap<u64, Vec<u8>>,
    read: Cell<u64>,
}

impl Sparse {
    fn new(len: u64) -> Self {
        Sparse {
            len,
            patches: BTreeMap::new(),
            read: Cell::new(0),
        }
    }
    fn patch(&mut self, at: u64, bytes: &[u8]) {
        self.patches.insert(at, bytes.to_vec());
    }
}

impl ReadAt for Sparse {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(ReadError::OutOfRange)?;
        if end > self.len {
            return Err(ReadError::OutOfRange);
        }
        self.read.set(self.read.get() + buf.len() as u64);
        buf.fill(0);
        // Patches are short (≤ 8 bytes) and never overlap.
        for (&at, bytes) in self.patches.range(offset.saturating_sub(16)..end) {
            for (i, b) in bytes.iter().enumerate() {
                let p = at + i as u64;
                if p >= offset && p < end {
                    buf[(p - offset) as usize] = *b;
                }
            }
        }
        Ok(())
    }
}

fn skippable_header(magic: u32, payload_len: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(&magic.to_le_bytes());
    h[4..].copy_from_slice(&payload_len.to_le_bytes());
    h
}

/// A data frame of exactly `total` stored bytes: 128 MiB window (the
/// default maximum), raw blocks of at most 128 KiB, no checksum.
fn data_frame_of(total: u64) -> Sparse {
    const BLOCK: u64 = 128 * 1024;
    let mut s = Sparse::new(total);
    let mut head = registry::ZSTD_DATA_FRAME.to_le_bytes().to_vec();
    head.push(0x00); // no FCS, not single-segment, no checksum, no dictionary
    head.push(17 << 3); // window 2^(10+17) = 128 MiB
    s.patch(0, &head);
    let mut pos = head.len() as u64;
    let block = |size: u64, last: bool| {
        let word = ((size as u32) << 3) | u32::from(last); // raw block
        let b = word.to_le_bytes();
        [b[0], b[1], b[2]]
    };
    loop {
        let remaining = total - pos;
        if remaining <= 3 + BLOCK {
            s.patch(pos, &block(remaining - 3, true));
            break;
        }
        // Leave at least one header's worth for the last block.
        let size = BLOCK.min(remaining - 3 - 3);
        s.patch(pos, &block(size, false));
        pos += 3 + size;
    }
    s
}

#[test]
fn sparse_data_frame_has_the_requested_length() {
    for total in [9u64, 131_081, 131_082, 1 << 20, 10_000_007] {
        let s = data_frame_of(total);
        let span = walk_frame(&s, 0, &Limits::default()).unwrap();
        assert_eq!(span.len, total);
    }
}

#[test]
fn walker_reads_only_headers() {
    let s = data_frame_of(DEFAULT_FRAME_LEN);
    walk_frame(&s, 0, &Limits::default()).unwrap();
    let blocks = DEFAULT_FRAME_LEN / (128 * 1024) + 2;
    assert!(
        s.read.get() < 16 + 3 * blocks,
        "walker read {} bytes; it must not touch payloads",
        s.read.get()
    );
}

// ---- reader ----------------------------------------------------------------

#[test]
fn frame_limit_data_frame() {
    let l = Limits::default();
    assert_eq!(l.max_frame_len, 269_484_032);
    for (total, ok) in [
        (DEFAULT_FRAME_LEN - 1, true),
        (DEFAULT_FRAME_LEN, true),
        (DEFAULT_FRAME_LEN + 1, false),
    ] {
        let r = walk_frame(&data_frame_of(total), 0, &l);
        if ok {
            assert_eq!(r.unwrap().len, total);
        } else {
            assert!(
                matches!(
                    r,
                    Err(FormatError::LimitExceeded {
                        kind: LimitKind::FrameLength,
                        ..
                    })
                ),
                "{total}: {r:?}"
            );
        }
    }
}

/// With *S* raised past the frame limit, the frame limit still binds.
#[test]
fn frame_limit_skippable_frame() {
    let l = Limits {
        max_skippable_payload: u64::from(u32::MAX),
        ..Limits::default()
    };
    for (payload, ok) in [
        (DEFAULT_FRAME_LEN - 9, true),
        (DEFAULT_FRAME_LEN - 8, true),
        (DEFAULT_FRAME_LEN - 7, false),
    ] {
        let mut s = Sparse::new(8 + payload);
        s.patch(
            0,
            &skippable_header(registry::RECOVERY_MANIFEST, payload as u32),
        );
        let r = walk_frame(&s, 0, &l);
        assert_eq!(r.is_ok(), ok, "{payload}: {r:?}");
        if !ok {
            assert!(matches!(
                r,
                Err(FormatError::LimitExceeded {
                    kind: LimitKind::FrameLength,
                    ..
                })
            ));
        }
    }
}

#[test]
fn skippable_payload_limit() {
    let l = Limits::default();
    for (payload, ok) in [
        (DEFAULT_SKIPPABLE_PAYLOAD - 1, true),
        (DEFAULT_SKIPPABLE_PAYLOAD, true),
        (DEFAULT_SKIPPABLE_PAYLOAD + 1, false),
    ] {
        let mut s = Sparse::new(8 + payload);
        s.patch(
            0,
            &skippable_header(registry::RECOVERY_MANIFEST, payload as u32),
        );
        let r = walk_frame(&s, 0, &l);
        assert_eq!(r.is_ok(), ok, "{payload}: {r:?}");
        if !ok {
            assert!(matches!(
                r,
                Err(FormatError::LimitExceeded {
                    kind: LimitKind::SkippablePayload,
                    ..
                })
            ));
        }
    }
}

/// The footer refuses to read and hash a commit frame over 8 + 64 KiB.
#[test]
fn commit_frame_limit() {
    let l = Limits::default();
    assert_eq!(l.max_commit_frame_len, 65_544);
    for (frame_len, ok) in [
        (DEFAULT_COMMIT_FRAME_LEN - 1, true),
        (DEFAULT_COMMIT_FRAME_LEN, true),
        (DEFAULT_COMMIT_FRAME_LEN + 1, false),
    ] {
        let mut commit = skippable_header(registry::COMMIT_RECORD, (frame_len - 8) as u32).to_vec();
        commit.resize(frame_len as usize, 0x5A);
        let mut archive = commit.clone();
        archive.extend_from_slice(&encode_footer_frame(0, 0, &commit));
        let r = validate_footer_at_eof(&archive[..], &l);
        assert_eq!(r.is_ok(), ok, "{frame_len}: {r:?}");
        if !ok {
            assert!(matches!(
                r,
                Err(FormatError::LimitExceeded {
                    kind: LimitKind::CommitFrameLength,
                    ..
                })
            ));
        }
    }
}

#[test]
fn required_feature_limit() {
    use mochi_format::envelope::check_required_features;
    let known: Vec<u64> = (0..100).collect();
    let l = Limits::default();
    for (n, ok) in [
        (DEFAULT_REQUIRED_FEATURES - 1, true),
        (DEFAULT_REQUIRED_FEATURES, true),
        (DEFAULT_REQUIRED_FEATURES + 1, false),
    ] {
        let features: Vec<u64> = (0..n).collect();
        let r = check_required_features(&features, &known, &l);
        assert_eq!(r.is_ok(), ok, "{n}");
        if !ok {
            assert_eq!(
                r.unwrap_err(),
                FormatError::LimitExceeded {
                    kind: LimitKind::RequiredFeatures,
                    limit: 64,
                    actual: 65
                }
            );
        }
    }
}

// ---- writer ------------------------------------------------------------------
//
// The writer check is the same comparison on the same fields; it is tested at
// a small configured size (to avoid allocating 256 MiB) and the defaults it
// would use are the constants pinned above.

#[test]
fn writer_frame_limits_at_boundaries() {
    let l = Limits {
        max_skippable_payload: 1000,
        max_frame_len: 900,
        max_commit_frame_len: 500,
        ..Limits::default()
    };
    let w = |kind, n: usize| encode_skippable_frame_within(kind, &vec![0u8; n], &l);
    // Frame limit 900 binds before S = 1000 for a manifest.
    assert!(w(FrameKind::RecoveryManifest, 891).is_ok());
    assert!(w(FrameKind::RecoveryManifest, 892).is_ok());
    assert_eq!(
        w(FrameKind::RecoveryManifest, 893).unwrap_err(),
        FormatError::CapacityExceeded {
            kind: CapacityKind::Frame(LimitKind::FrameLength),
            limit: 900,
            actual: 901
        }
    );
    // Commit frames: 500.
    assert!(w(FrameKind::CommitRecord, 491).is_ok());
    assert!(w(FrameKind::CommitRecord, 492).is_ok());
    assert!(matches!(
        w(FrameKind::CommitRecord, 493),
        Err(FormatError::CapacityExceeded {
            kind: CapacityKind::Frame(LimitKind::CommitFrameLength),
            ..
        })
    ));
    // S binds when the frame limit is roomier.
    let l2 = Limits {
        max_skippable_payload: 100,
        ..Limits::default()
    };
    assert!(encode_skippable_frame_within(FrameKind::RecoveryManifest, &[0; 100], &l2).is_ok());
    assert!(matches!(
        encode_skippable_frame_within(FrameKind::RecoveryManifest, &[0; 101], &l2),
        Err(FormatError::CapacityExceeded {
            kind: CapacityKind::Frame(LimitKind::SkippablePayload),
            ..
        })
    ));
    // CapacityExceeded is a writer class, distinct from a reader's LimitExceeded.
    assert_eq!(
        w(FrameKind::CommitRecord, 493).unwrap_err().class(),
        mochi_format::ErrorClass::CapacityExceeded
    );
}

/// B.2.3 derives the frame limit from libzstd's documented worst case,
/// `ZSTD_COMPRESSBOUND`, claiming no output of the writer's encoder can
/// exceed it. Check the claim against this codec's actual settings (checksum
/// and content size on) on incompressible input, at sizes ≥ 128 KiB where
/// the bound's simple form holds, up to the 8 MiB maximum chunk (§13).
#[test]
fn codec_output_respects_zstd_compress_bound() {
    use mochi_format::codec::{encode_object, EncodeParams, Protection};
    use mochi_format::repr::DecodedBytes;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for len in [128 * 1024, (1 << 20) + 17, 8 << 20] {
        let data: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        for level in [1, 3, 19] {
            let stored = encode_object(
                &DecodedBytes::new(data.clone()),
                &EncodeParams {
                    level,
                    ..EncodeParams::default()
                },
                Protection::None,
                &Limits::default(),
            )
            .unwrap();
            let bound = zstd_compress_bound_large(len as u64);
            assert!(
                stored.as_bytes().len() as u64 <= bound,
                "len {len} level {level}: {} > {bound}",
                stored.as_bytes().len()
            );
        }
    }
}
