//! File extents (spec §10.3).
//!
//! A file version's content is an ordered list of extents covering exactly
//! `[0, logical_len)`. Each extent maps a logical range either to a range of a
//! chunk's *decoded* bytes or to an explicit hole (sparse files, §9.3).
//!
//! Validation rejects the four defect classes §10.3 names — gaps, overlaps,
//! out-of-range reads, and length mismatches — plus the structural ones that
//! make the list ambiguous (ordinals, empty extents) and the referential one
//! (a chunk the catalog does not know). Every rule is checked with overflow-
//! checked arithmetic; extents come from untrusted catalogs.

use std::fmt;

use crate::error::{ErrorCode, MochiError};
use crate::object::ObjectId;

/// Where an extent's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtentSource {
    /// `length` bytes starting at `chunk_offset` in the chunk's decoded bytes.
    Chunk { chunk: ObjectId, chunk_offset: u64 },
    /// `length` zero bytes, stored nowhere.
    Hole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Extent {
    pub ordinal: u32,
    pub logical_offset: u64,
    pub length: u64,
    pub source: ExtentSource,
}

/// Why an extent list is invalid. The first four are §10.3's classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentDefect {
    /// Logical bytes between two extents (or before the first) that no extent
    /// covers. Holes must be explicit, so every gap is unintended.
    Gap { ordinal: u32 },
    /// An extent starts before the previous one ends.
    Overlap { ordinal: u32 },
    /// The extent reads past the end of its chunk's decoded bytes.
    OutOfRange { ordinal: u32 },
    /// The extents do not cover exactly `[0, logical_len)`.
    LengthMismatch { covered: u64, logical_len: u64 },
    /// Ordinals are not exactly 0, 1, 2, … in order.
    OrdinalSequence { ordinal: u32 },
    /// A zero-length extent: meaningless, and ambiguous with its neighbours.
    Empty { ordinal: u32 },
    /// The referenced chunk is not in the catalog (referential, §20.1).
    UnknownChunk { ordinal: u32 },
    /// Archive-derived arithmetic overflowed.
    Overflow { ordinal: u32 },
}

impl fmt::Display for ExtentDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<ExtentDefect> for MochiError {
    fn from(d: ExtentDefect) -> Self {
        MochiError::new(
            ErrorCode::ExtentInvalid,
            format!("invalid file extents: {d}"),
        )
    }
}

/// Validate `extents` (in the order given) for a file of `logical_len` bytes.
/// `decoded_len_of` returns a chunk's decoded length, or `None` if the chunk
/// is unknown.
pub fn validate_extents(
    logical_len: u64,
    extents: &[Extent],
    decoded_len_of: impl Fn(&ObjectId) -> Option<u64>,
) -> Result<(), ExtentDefect> {
    let mut end = 0u64;
    for (i, e) in extents.iter().enumerate() {
        let ordinal = e.ordinal;
        if u32::try_from(i).ok() != Some(ordinal) {
            return Err(ExtentDefect::OrdinalSequence { ordinal });
        }
        if e.length == 0 {
            return Err(ExtentDefect::Empty { ordinal });
        }
        if e.logical_offset > end {
            return Err(ExtentDefect::Gap { ordinal });
        }
        if e.logical_offset < end {
            return Err(ExtentDefect::Overlap { ordinal });
        }
        end = e
            .logical_offset
            .checked_add(e.length)
            .ok_or(ExtentDefect::Overflow { ordinal })?;
        if let ExtentSource::Chunk {
            chunk,
            chunk_offset,
        } = &e.source
        {
            let chunk_len = decoded_len_of(chunk).ok_or(ExtentDefect::UnknownChunk { ordinal })?;
            let read_end = chunk_offset
                .checked_add(e.length)
                .ok_or(ExtentDefect::Overflow { ordinal })?;
            if read_end > chunk_len {
                return Err(ExtentDefect::OutOfRange { ordinal });
            }
        }
    }
    if end != logical_len {
        return Err(ExtentDefect::LengthMismatch {
            covered: end,
            logical_len,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: ObjectId = ObjectId::from_bytes([7; 32]);

    fn chunk(ordinal: u32, off: u64, len: u64, chunk_offset: u64) -> Extent {
        Extent {
            ordinal,
            logical_offset: off,
            length: len,
            source: ExtentSource::Chunk {
                chunk: C,
                chunk_offset,
            },
        }
    }

    fn hole(ordinal: u32, off: u64, len: u64) -> Extent {
        Extent {
            ordinal,
            logical_offset: off,
            length: len,
            source: ExtentSource::Hole,
        }
    }

    fn lens(id: &ObjectId) -> Option<u64> {
        (*id == C).then_some(100)
    }

    #[test]
    fn valid_lists_including_holes_and_empty_files() {
        assert_eq!(validate_extents(0, &[], lens), Ok(()));
        let v = [
            chunk(0, 0, 10, 0),
            hole(1, 10, 1000),
            chunk(2, 1010, 90, 10),
        ];
        assert_eq!(validate_extents(1100, &v, lens), Ok(()));
        // A file made only of a hole.
        assert_eq!(validate_extents(5, &[hole(0, 0, 5)], lens), Ok(()));
    }

    #[test]
    fn each_defect_class_is_named_precisely() {
        use ExtentDefect::*;
        let cases: Vec<(u64, Vec<Extent>, ExtentDefect)> = vec![
            (
                20,
                vec![chunk(0, 0, 10, 0), chunk(1, 11, 9, 0)],
                Gap { ordinal: 1 },
            ),
            (20, vec![chunk(0, 5, 15, 0)], Gap { ordinal: 0 }),
            (
                20,
                vec![chunk(0, 0, 10, 0), chunk(1, 9, 11, 0)],
                Overlap { ordinal: 1 },
            ),
            (20, vec![chunk(0, 0, 20, 90)], OutOfRange { ordinal: 0 }),
            (
                20,
                vec![chunk(0, 0, 10, 0)],
                LengthMismatch {
                    covered: 10,
                    logical_len: 20,
                },
            ),
            (
                5,
                vec![chunk(0, 0, 10, 0)],
                LengthMismatch {
                    covered: 10,
                    logical_len: 5,
                },
            ),
            (0, vec![chunk(0, 0, 0, 0)], Empty { ordinal: 0 }),
            (20, vec![chunk(1, 0, 20, 0)], OrdinalSequence { ordinal: 1 }),
            (
                20,
                vec![Extent {
                    source: ExtentSource::Chunk {
                        chunk: ObjectId::from_bytes([9; 32]),
                        chunk_offset: 0,
                    },
                    ..chunk(0, 0, 20, 0)
                }],
                UnknownChunk { ordinal: 0 },
            ),
            (
                u64::MAX,
                vec![chunk(0, 0, 10, 0), hole(1, 10, u64::MAX)],
                Overflow { ordinal: 1 },
            ),
            (20, vec![chunk(0, 0, 20, u64::MAX)], Overflow { ordinal: 0 }),
        ];
        for (len, v, want) in cases {
            assert_eq!(validate_extents(len, &v, lens), Err(want), "{v:?}");
        }
    }
}
