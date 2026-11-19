//! Positional byte source. `mochi-format` does no I/O of its own; `mochi-core`
//! adapts its `Storage` trait to [`ReadAt`], and tests use in-memory slices.

use crate::error::FormatError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The requested range is not entirely inside the source.
    OutOfRange,
    /// The underlying source failed. The message must not contain archive content.
    Failed(String),
}

/// Random-access reads. Implementations must never return partial data: either
/// `buf` is filled completely or an error is returned.
pub trait ReadAt {
    /// Total length in bytes.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError>;
}

impl ReadAt for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        let start = usize::try_from(offset).map_err(|_| ReadError::OutOfRange)?;
        let end = start.checked_add(buf.len()).ok_or(ReadError::OutOfRange)?;
        let src = self.get(start..end).ok_or(ReadError::OutOfRange)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), ReadError> {
        self.as_slice().read_at(offset, buf)
    }
}

/// Read exactly `N` bytes at `offset`, mapping failures to [`FormatError`].
pub(crate) fn read_array<const N: usize>(
    src: &(impl ReadAt + ?Sized),
    offset: u64,
) -> Result<[u8; N], FormatError> {
    let mut buf = [0u8; N];
    read_exact(src, offset, &mut buf)?;
    Ok(buf)
}

pub(crate) fn read_exact(
    src: &(impl ReadAt + ?Sized),
    offset: u64,
    buf: &mut [u8],
) -> Result<(), FormatError> {
    match src.read_at(offset, buf) {
        Ok(()) => Ok(()),
        Err(ReadError::OutOfRange) => Err(FormatError::Truncated { offset }),
        Err(ReadError::Failed(msg)) => Err(FormatError::Source(msg)),
    }
}
