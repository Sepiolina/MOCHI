//! Frame reading and writing (spec §8.1, §8.5, §8.6).
//!
//! The physical length of a frame comes from a **structural walk** of the
//! bytes (RFC 8878), never from `Frame_Content_Size`. A successful walk makes a
//! location a *candidate* frame only; it is not proof of a MOCHI object
//! (spec §8.5, §8.6).

use crate::error::{CapacityKind, FormatError, LimitKind, Result};
use crate::limits::Limits;
use crate::registry::{self, FrameKind};
use crate::source::{read_array, ReadAt};

/// `Block_Maximum_Size` ceiling (RFC 8878 §3.1.1.2.3): 128 KiB.
const BLOCK_SIZE_CEILING: u64 = 128 * 1024;

/// What the structural walk learned about a data frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataFrameInfo {
    /// Frame header length after the 4-byte magic.
    pub header_len: u8,
    pub blocks: u64,
    pub has_checksum: bool,
    /// Declared `Dictionary_ID`; 0 means none.
    pub dictionary_id: u32,
    /// Declared `Frame_Content_Size`. **Decoded** size, untrusted; never a stored length.
    pub declared_content_size: Option<u64>,
    pub window_size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDetail {
    Skippable { payload_len: u32 },
    Data(DataFrameInfo),
}

/// A structurally valid frame at `offset` occupying exactly `len` stored bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSpan {
    pub offset: u64,
    pub len: u64,
    pub magic: u32,
    pub kind: FrameKind,
    pub detail: FrameDetail,
}

impl FrameSpan {
    /// Offset one past the last byte. Cannot overflow: construction checks it.
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

fn in_skippable_range(magic: u32) -> bool {
    (registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX).contains(&magic)
}

/// Walk the single frame starting at `offset`.
///
/// Every step is bounds-checked against the source length and [`Limits`]
/// before it reads.
///
/// **Placement.** A frame kind with a required offset (only the archive
/// descriptor, spec Annex B.2 D12) is rejected with
/// [`FormatError::MisplacedFrame`] anywhere else. The check runs on the magic,
/// before the structural walk, so a descriptor magic cut short by end-of-file
/// is still misplaced rather than merely truncated: a tail containing one is
/// never eligible for truncation (D14). Offsets are those of `src`; a caller
/// that copies a frame out and walks the copy at 0 has lost the original
/// position and must check it separately.
pub fn walk_frame(src: &(impl ReadAt + ?Sized), offset: u64, limits: &Limits) -> Result<FrameSpan> {
    let magic = u32::from_le_bytes(read_array::<4>(src, offset)?);
    if let Some(required) = FrameKind::from_magic(magic).and_then(FrameKind::required_offset) {
        if offset != required {
            return Err(FormatError::MisplacedFrame { offset, magic });
        }
    }
    if in_skippable_range(magic) {
        walk_skippable(src, offset, magic, limits)
    } else if magic == registry::ZSTD_DATA_FRAME {
        walk_data(src, offset, limits)
    } else {
        Err(FormatError::NotAFrame { offset, magic })
    }
}

fn walk_skippable(
    src: &(impl ReadAt + ?Sized),
    offset: u64,
    magic: u32,
    limits: &Limits,
) -> Result<FrameSpan> {
    let size_at = offset
        .checked_add(4)
        .ok_or(FormatError::Overflow { offset })?;
    let payload_len = u32::from_le_bytes(read_array::<4>(src, size_at)?);
    if u64::from(payload_len) > limits.max_skippable_payload {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::SkippablePayload,
            limit: limits.max_skippable_payload,
            actual: u64::from(payload_len),
        });
    }
    let len = registry::SKIPPABLE_HEADER_LEN as u64 + u64::from(payload_len);
    if len > limits.max_frame_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::FrameLength,
            limit: limits.max_frame_len,
            actual: len,
        });
    }
    let end = offset
        .checked_add(len)
        .ok_or(FormatError::Overflow { offset })?;
    if end > src.len() {
        return Err(FormatError::Truncated { offset: src.len() });
    }
    let kind = FrameKind::from_magic(magic).ok_or(FormatError::NotAFrame { offset, magic })?;
    Ok(FrameSpan {
        offset,
        len,
        magic,
        kind,
        detail: FrameDetail::Skippable { payload_len },
    })
}

/// Parsed `Frame_Header_Descriptor` and the fields it announces.
struct DataHeader {
    header_len: u8,
    has_checksum: bool,
    dictionary_id: u32,
    declared_content_size: Option<u64>,
    window_size: u64,
}

fn parse_data_header(src: &(impl ReadAt + ?Sized), offset: u64) -> Result<DataHeader> {
    let mut pos = offset
        .checked_add(4)
        .ok_or(FormatError::Overflow { offset })?;
    let [descriptor] = read_array::<1>(src, pos)?;
    pos += 1;

    if descriptor & 0b0000_1000 != 0 {
        return Err(FormatError::ReservedHeaderBit { offset });
    }
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0b0010_0000 != 0;
    let has_checksum = descriptor & 0b0000_0100 != 0;
    let dict_flag = descriptor & 0b11;

    let mut header_len: u8 = 1;

    // Window_Descriptor: present only when Single_Segment_flag is clear.
    let window_descriptor = if single_segment {
        None
    } else {
        let [wd] = read_array::<1>(src, pos)?;
        pos += 1;
        header_len += 1;
        Some(wd)
    };

    let dict_len: usize = match dict_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let mut dict_bytes = [0u8; 4];
    if dict_len > 0 {
        crate::source::read_exact(src, pos, &mut dict_bytes[..dict_len])?;
        pos += dict_len as u64;
        header_len += dict_len as u8;
    }
    let dictionary_id = u32::from_le_bytes(dict_bytes);

    // Frame_Content_Size. The 2-byte form stores (size - 256) (RFC 8878 §3.1.1.1.4).
    let fcs_len: usize = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let declared_content_size = if fcs_len == 0 {
        None
    } else {
        let mut buf = [0u8; 8];
        crate::source::read_exact(src, pos, &mut buf[..fcs_len])?;
        header_len += fcs_len as u8;
        let raw = u64::from_le_bytes(buf);
        Some(if fcs_flag == 1 {
            raw.checked_add(256)
                .ok_or(FormatError::Overflow { offset })?
        } else {
            raw
        })
    };

    let window_size = match (window_descriptor, declared_content_size) {
        (Some(wd), _) => {
            let exponent = u32::from(wd >> 3);
            let mantissa = u64::from(wd & 0b111);
            let base = 1u64 << (10 + exponent); // exponent <= 31, so this fits in u64
            base + (base / 8) * mantissa
        }
        // Single segment: the window is the content size (FCS is always present here).
        (None, Some(size)) => size,
        (None, None) => 0,
    };

    Ok(DataHeader {
        header_len,
        has_checksum,
        dictionary_id,
        declared_content_size,
        window_size,
    })
}

fn walk_data(src: &(impl ReadAt + ?Sized), offset: u64, limits: &Limits) -> Result<FrameSpan> {
    let header = parse_data_header(src, offset)?;
    if header.window_size > limits.max_window_size {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::WindowSize,
            limit: limits.max_window_size,
            actual: header.window_size,
        });
    }
    let block_max = header.window_size.min(BLOCK_SIZE_CEILING);

    let overflow = FormatError::Overflow { offset };
    let mut pos = offset
        .checked_add(4 + u64::from(header.header_len))
        .ok_or_else(|| overflow.clone())?;
    let mut blocks: u64 = 0;

    loop {
        if blocks >= limits.max_blocks_per_frame {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::BlocksPerFrame,
                limit: limits.max_blocks_per_frame,
                actual: blocks + 1,
            });
        }
        let raw = read_array::<3>(src, pos)?;
        let word = u32::from(raw[0]) | u32::from(raw[1]) << 8 | u32::from(raw[2]) << 16;
        let last = word & 1 == 1;
        let block_type = (word >> 1) & 0b11;
        let block_size = word >> 3;

        if block_type == 3 {
            return Err(FormatError::ReservedBlockType { offset: pos });
        }
        if u64::from(block_size) > block_max {
            return Err(FormatError::BlockTooLarge {
                offset: pos,
                size: block_size,
                max: block_max,
            });
        }
        // An RLE block stores exactly one byte; its Block_Size is the regenerated size.
        let stored = if block_type == 1 {
            1
        } else {
            u64::from(block_size)
        };
        pos = pos
            .checked_add(3 + stored)
            .ok_or_else(|| overflow.clone())?;
        blocks += 1;

        let so_far = pos - offset;
        if so_far > limits.max_frame_len {
            return Err(FormatError::LimitExceeded {
                kind: LimitKind::FrameLength,
                limit: limits.max_frame_len,
                actual: so_far,
            });
        }
        if pos > src.len() {
            return Err(FormatError::Truncated { offset: src.len() });
        }
        if last {
            break;
        }
    }

    if header.has_checksum {
        pos = pos.checked_add(4).ok_or_else(|| overflow.clone())?;
        if pos > src.len() {
            return Err(FormatError::Truncated { offset: src.len() });
        }
    }

    let len = pos - offset;
    if len > limits.max_frame_len {
        return Err(FormatError::LimitExceeded {
            kind: LimitKind::FrameLength,
            limit: limits.max_frame_len,
            actual: len,
        });
    }
    Ok(FrameSpan {
        offset,
        len,
        magic: registry::ZSTD_DATA_FRAME,
        kind: FrameKind::ZstdData,
        detail: FrameDetail::Data(DataFrameInfo {
            header_len: header.header_len,
            blocks,
            has_checksum: header.has_checksum,
            dictionary_id: header.dictionary_id,
            declared_content_size: header.declared_content_size,
            window_size: header.window_size,
        }),
    })
}

/// Sequential walk over back-to-back frames. Yields one item per frame and
/// stops at clean end of input, or after the first error (it is fused: a
/// corrupt frame leaves later offsets unknown, so nothing further is guessed).
pub struct Frames<'a, S: ReadAt + ?Sized> {
    src: &'a S,
    next: u64,
    limits: Limits,
    done: bool,
}

impl<'a, S: ReadAt + ?Sized> Frames<'a, S> {
    pub fn new(src: &'a S, start: u64, limits: Limits) -> Self {
        Frames {
            src,
            next: start,
            limits,
            done: false,
        }
    }

    /// Offset where the next frame would start.
    pub fn position(&self) -> u64 {
        self.next
    }
}

impl<S: ReadAt + ?Sized> Iterator for Frames<'_, S> {
    type Item = Result<FrameSpan>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.next >= self.src.len() {
            return None;
        }
        match walk_frame(self.src, self.next, &self.limits) {
            Ok(span) => {
                self.next = span.end();
                Some(Ok(span))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Encode a skippable frame: 4-byte magic, 4-byte little-endian size, payload.
///
/// Refuses data frames and reserved magics (no writer may emit those), and
/// refuses payloads that do not fit the 32-bit size field (spec §8.3).
pub fn encode_skippable_frame(kind: FrameKind, payload: &[u8]) -> Result<Vec<u8>> {
    let magic = match kind.magic() {
        Some(m) if kind.is_skippable() => m,
        Some(_) => return Err(FormatError::CannotWrite("not a skippable frame kind")),
        None => return Err(FormatError::CannotWrite("reserved frame kind")),
    };
    let size = u32::try_from(payload.len()).map_err(|_| FormatError::PayloadTooLarge {
        len: payload.len() as u64,
    })?;
    let mut out = Vec::with_capacity(registry::SKIPPABLE_HEADER_LEN + payload.len());
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// [`encode_skippable_frame`], refusing a frame that a reader using `limits`
/// would reject (spec Annex B.2.3, writer default rule). Writers pass
/// [`Limits::WRITER_DEFAULT`]. Every refusal is
/// [`FormatError::CapacityExceeded`], never a reader's `LimitExceeded`: the
/// archive is fine, and the previous head stays the head (D10 item 11).
///
/// Checked here: the skippable payload limit, the frame limit, and for a
/// commit record the commit-frame limit. Content limits (CBOR items and
/// depth, the image budget) are checked by the encoder that knows them.
pub fn encode_skippable_frame_within(
    kind: FrameKind,
    payload: &[u8],
    limits: &Limits,
) -> Result<Vec<u8>> {
    let payload_len = payload.len() as u64;
    let refuse = |kind: LimitKind, limit: u64, actual: u64| FormatError::CapacityExceeded {
        kind: CapacityKind::Frame(kind),
        limit,
        actual,
    };
    if payload_len > limits.max_skippable_payload {
        return Err(refuse(
            LimitKind::SkippablePayload,
            limits.max_skippable_payload,
            payload_len,
        ));
    }
    let frame_len = payload_len.saturating_add(registry::SKIPPABLE_HEADER_LEN as u64);
    if frame_len > limits.max_frame_len {
        return Err(refuse(
            LimitKind::FrameLength,
            limits.max_frame_len,
            frame_len,
        ));
    }
    if kind == FrameKind::CommitRecord && frame_len > limits.max_commit_frame_len {
        return Err(refuse(
            LimitKind::CommitFrameLength,
            limits.max_commit_frame_len,
            frame_len,
        ));
    }
    encode_skippable_frame(kind, payload)
}
