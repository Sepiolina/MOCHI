//! The `Storage` abstraction (plan §2.2).
//!
//! **All I/O in `mochi-core` goes through these traits.** Direct `std::fs` use
//! in core logic makes fault injection impossible and is rejected by
//! `ci/check-invariants.sh`; the one OS-backed implementation lives in
//! [`os`] and is the only file allowed to use it.
//!
//! The interface is split in two on purpose:
//!
//! * [`ReadStorage`]: size and positional reads. Verification (`verify`,
//!   `fsck`, "Test archive") takes `&dyn ReadStorage`, so read-only behaviour
//!   (spec §20.2) is enforced by the type system, not by discipline.
//! * [`Storage`]: adds append, the two sync operations, locking, and truncate,
//!   i.e. exactly what the §12.2 publication protocol needs.
//! * [`StorageDir`]: the directory that holds an archive. Creates a sibling
//!   file exclusively, publishes it under another name **without replacing**
//!   anything, and removes temporary files only under their lock: what
//!   creation (Annex B.2 D13) and tail quarantine (D14) need (plan T19).
//!
//! Durability contract: bytes written by [`Storage::append`] are *not* durable
//! until [`Storage::sync_data`] returns `Ok`; a new or replaced file's directory
//! entry is not durable until [`Storage::sync_directory`] returns
//! `Ok(DirectoryDurability::Confirmed)`. Implementations must never claim a
//! sync they did not perform (spec §24.2: "no false durability
//! acknowledgement"). Where the platform offers only a best-effort directory
//! flush (Windows, plan O12), the result is
//! [`DirectoryDurability::Unconfirmed`], never `Confirmed`.
//!
//! The per-OS assumptions behind these calls are documented in [`os`].

pub mod os;

use std::fmt;
use std::io;

use mochi_format::source::{ReadAt, ReadError};

/// Failure of a storage operation.
#[derive(Debug)]
pub enum StorageError {
    /// The backend reported an I/O failure (or a simulated one, in the testkit).
    Io(io::Error),
    /// A read or truncate fell outside the object; checked before any use of
    /// archive-derived offsets (spec §8.5).
    OutOfBounds { offset: u64, len: u64, size: u64 },
    /// Another writer holds the exclusive publication lock (spec §12.2 step 1).
    LockHeld,
    /// The backend cannot provide this operation.
    Unsupported(&'static str),
    /// A file of this name already exists in the directory, and the operation
    /// never replaces one (exclusive creation, no-replace publication).
    Exists { name: String },
    /// Not a single file-name component (empty, `.`, `..`, or containing a
    /// path separator or NUL). Directory operations take names, not paths.
    InvalidName { name: String },
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "storage I/O error: {e}"),
            Self::OutOfBounds { offset, len, size } => write!(
                f,
                "range {offset}+{len} is out of bounds for object of size {size}"
            ),
            Self::LockHeld => f.write_str("exclusive lock is held by another writer"),
            Self::Unsupported(what) => write!(f, "unsupported storage operation: {what}"),
            Self::Exists { name } => write!(f, "{name:?} already exists and is never replaced"),
            Self::InvalidName { name } => write!(f, "{name:?} is not a single file name"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Outcome of [`Storage::sync_directory`] (spec §12.2 step 9, plan O12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryDurability {
    /// The platform's documented mechanism persisted the directory entry.
    Confirmed,
    /// A best-effort flush was attempted and did not confirm durability. The
    /// publish must be reported as degraded ("directory durability
    /// unconfirmed"), never as durable. The string says why, without archive
    /// content.
    Unconfirmed(String),
}

/// Read-only view of one archive location.
pub trait ReadStorage {
    /// Current size in bytes.
    fn size(&self) -> Result<u64, StorageError>;

    /// Read up to `buf.len()` bytes at `offset`. Returns the number of bytes
    /// read; `0` means `offset` is at or past the end. Never reads past the end.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError>;

    /// Fill `buf` completely from `offset`, or fail with
    /// [`StorageError::OutOfBounds`]. All offset arithmetic is checked.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        let want = buf.len() as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let out_of_bounds = || StorageError::OutOfBounds {
                offset,
                len: want,
                size: self.size().unwrap_or(0),
            };
            let at = offset.checked_add(done as u64).ok_or_else(out_of_bounds)?;
            let rest = buf.get_mut(done..).ok_or_else(out_of_bounds)?;
            let rest_len = rest.len();
            let n = self.read_at(at, rest)?;
            if n == 0 || n > rest_len {
                return Err(out_of_bounds());
            }
            done += n;
        }
        Ok(())
    }
}

/// Read-write storage for one archive location, single writer under lock.
pub trait Storage: ReadStorage {
    /// Append `data` at the current end; returns the offset it was written at.
    /// Not durable until [`Storage::sync_data`].
    fn append(&mut self, data: &[u8]) -> Result<u64, StorageError>;

    /// Persist previously written bytes using the platform's documented
    /// mechanism (POSIX `fsync`/`fdatasync`; Windows `FlushFileBuffers`).
    fn sync_data(&mut self) -> Result<(), StorageError>;

    /// Persist directory-entry changes for the archive's parent directory where
    /// creating or replacing the file requires it (spec §12.2 step 9).
    /// `Err` = the documented mechanism failed; `Ok(Unconfirmed)` = only a
    /// best-effort mechanism exists and it did not confirm (O12).
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError>;

    /// Try to take the exclusive publication lock without blocking.
    /// `Err(StorageError::LockHeld)` if another writer holds it.
    fn try_lock_exclusive(&mut self) -> Result<(), StorageError>;

    /// Release the publication lock.
    fn unlock(&mut self) -> Result<(), StorageError>;

    /// Shrink the object to `new_len`. Only for explicit, auditable tail
    /// recovery under exclusive access (spec §12.2); `new_len` may not exceed
    /// the current size.
    fn truncate(&mut self, new_len: u64) -> Result<(), StorageError>;
}

/// What [`StorageDir::remove_if_unlocked`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The file existed, its lock was acquired, and it was removed.
    Removed,
    /// Another live holder has the file's lock; it was left alone.
    Locked,
    /// No file of that name exists.
    Missing,
}

/// Check that `name` is one file-name component. Archive-adjacent names are
/// built by MOCHI (temporary and sidecar names), never taken from archive
/// contents, but they are still validated here rather than trusted.
pub fn check_file_name(name: &str) -> Result<(), StorageError> {
    let bad = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || (cfg!(windows) && name.contains(':'));
    if bad {
        return Err(StorageError::InvalidName {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// The directory that holds an archive (plan T19; Annex B.2 D13, D14).
///
/// Every operation takes a single file name in this directory. None of them
/// ever replaces an existing file: that is the property creation (D13: "published at the final name
/// without replacing an existing file") and quarantine (D14: a "no-clobber
/// sidecar") rest on.
///
/// Durability: a new or renamed entry is not durable until
/// [`StorageDir::sync_directory`] returns `Ok(DirectoryDurability::Confirmed)`,
/// with the same contract as [`Storage::sync_directory`].
pub trait StorageDir {
    /// The file type this directory opens.
    type File: Storage;

    /// Create `name`, which must not exist (`StorageError::Exists`
    /// otherwise), open it for reading and appending, and take its exclusive
    /// lock. A temporary file created this way stays locked for as long as
    /// the handle lives, so cleanup by another process cannot remove it.
    fn create_exclusive(&mut self, name: &str) -> Result<Self::File, StorageError>;

    /// Make the file at `from` appear at `to`, never replacing an existing
    /// `to` (`StorageError::Exists`, and `from` is left as it was). On
    /// success `from` no longer names it. An implementation may link and then
    /// unlink; a crash between the two leaves both names on the same bytes,
    /// which cleanup resolves by removing `from` once its lock is free.
    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError>;

    /// Remove a temporary file this process created and still holds (for
    /// example after a failed creation). Consumes the handle.
    fn discard(&mut self, file: Self::File, name: &str) -> Result<(), StorageError>;

    /// Remove `name` only if its exclusive lock can be acquired: cleanup of a
    /// temporary file left by a process that has died (D13 "Cleanup removes
    /// only temporary files whose lock it can acquire"). A file another live
    /// process holds is left alone.
    fn remove_if_unlocked(&mut self, name: &str) -> Result<RemoveOutcome, StorageError>;

    /// Persist this directory's entries (creations, publications, removals).
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError>;
}

/// Adapts a [`ReadStorage`] to `mochi_format`'s [`ReadAt`] so the pure frame
/// walker and footer validator can run over any storage. The size is taken
/// once at construction: archive-derived offsets are checked against that
/// snapshot, and a concurrent change cannot widen what is read.
pub struct StorageReader<'a> {
    inner: &'a dyn ReadStorage,
    len: u64,
}

impl<'a> StorageReader<'a> {
    pub fn new(inner: &'a dyn ReadStorage) -> Result<Self, StorageError> {
        Ok(StorageReader {
            len: inner.size()?,
            inner,
        })
    }

    /// A view of only the first `len` bytes (for example, the committed
    /// prefix). `len` may not exceed the underlying size snapshot.
    pub fn prefix(inner: &'a dyn ReadStorage, len: u64) -> Result<Self, StorageError> {
        let size = inner.size()?;
        if len > size {
            return Err(StorageError::OutOfBounds {
                offset: len,
                len: 0,
                size,
            });
        }
        Ok(StorageReader { inner, len })
    }
}

impl ReadAt for StorageReader<'_> {
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
        self.inner.read_exact_at(offset, buf).map_err(|e| match e {
            StorageError::OutOfBounds { .. } => ReadError::OutOfRange,
            other => ReadError::Failed(other.to_string()),
        })
    }
}
