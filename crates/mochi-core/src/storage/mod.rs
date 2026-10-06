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
    /// `Err(StorageError::LockHeld)` if another writer holds it. Taking it
    /// again through the handle that holds it is a no-op, on every platform.
    fn try_lock_exclusive(&mut self) -> Result<(), StorageError>;

    /// Release the publication lock. A no-op if this handle does not hold it.
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

    /// Open the existing file `name` for reading and appending, unlocked
    /// (the archive itself, when appending through its directory).
    fn open(&mut self, name: &str) -> Result<Self::File, StorageError>;

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

    /// Publish `from`, held by `file`, as the archive `to`
    /// ([`publish_no_replace`](Self::publish_no_replace)), with `file` then
    /// holding `to`'s publication lock: taken **before** `to` becomes
    /// visible, so no other writer can lock the new archive first
    /// (`LockHeld` if one already holds it; nothing is published). The
    /// default suits backends whose lock belongs to the file, which keeps it
    /// under its new name; the OS backend locks a separate lock file (Q54).
    fn publish_archive(
        &mut self,
        file: &mut Self::File,
        from: &str,
        to: &str,
    ) -> Result<(), StorageError> {
        let _ = file;
        self.publish_no_replace(from, to)
    }

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

/// Why an archive name cannot be created on a restore target (spec §10.4,
/// §23.3 #7). Restoration reports it and skips the entry; it never renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameIssue {
    /// The bytes cannot be expressed as a name on this platform (for example
    /// a non-UTF-8 POSIX name on Windows).
    NotRepresentable,
    /// A reserved device name (`CON`, `NUL`, `COM1`, …), with or without an
    /// extension.
    Reserved,
    /// A byte the platform forbids in names.
    IllegalCharacter(u8),
    /// The name ends with a space or a period, which Windows strips.
    TrailingDotOrSpace,
}

impl fmt::Display for NameIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRepresentable => f.write_str("not representable on this platform"),
            Self::Reserved => f.write_str("a reserved device name"),
            Self::IllegalCharacter(b) => write!(f, "contains the forbidden byte 0x{b:02x}"),
            Self::TrailingDotOrSpace => f.write_str("ends with a space or a period"),
        }
    }
}

/// Windows naming rules for one component (Microsoft, "Naming Files, Paths,
/// and Namespaces"): no `< > : " / \ | ? *` or control characters; no
/// trailing space or period; none of the reserved device names `CON`, `PRN`,
/// `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` (also with the superscript
/// digits ¹ ² ³), in any case, even with an extension; and valid WTF-8
/// (plan O24). A pure function, so the rules are tested on every platform.
pub fn windows_name_issue(name: &[u8]) -> Option<NameIssue> {
    if crate::catalog::path::utf16_from_wtf8(name).is_none() {
        return Some(NameIssue::NotRepresentable);
    }
    if let Some(b) = name
        .iter()
        .find(|b| **b < 0x20 || b"<>:\"/\\|?*".contains(b))
    {
        return Some(NameIssue::IllegalCharacter(*b));
    }
    if matches!(name.last(), Some(b' ' | b'.')) {
        return Some(NameIssue::TrailingDotOrSpace);
    }
    // The device name is the part before the first period, trailing spaces
    // ignored ("NUL .txt" is NUL too).
    let stem = name.split(|b| *b == b'.').next().unwrap_or(name);
    let stem = stem.trim_ascii_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_slice(), b"CON" | b"PRN" | b"AUX" | b"NUL")
        || ((stem.starts_with(b"COM") || stem.starts_with(b"LPT"))
            && matches!(
                &stem[3..],
                [b'1'..=b'9'] | [0xC2, 0xB9] | [0xC2, 0xB2] | [0xC2, 0xB3]
            ));
    reserved.then_some(NameIssue::Reserved)
}

/// How a restore destination compares names (plan C6: "case-insensitive
/// filesystem detection"). Detected once, on the restore root, before
/// anything is restored; directories created below it inherit it on the
/// filesystems MOCHI supports (ext4 casefold is inherited; NTFS and vfat are
/// insensitive throughout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseBehavior {
    /// Names differing only in case are different entries.
    Sensitive,
    /// Names differing only in case are the same entry.
    Insensitive,
}

/// The key under which the restore preflight treats two sibling names as
/// one on a [`CaseBehavior::Insensitive`] destination: valid UTF-8 runs
/// lower-cased with Unicode's full lowercase mapping, other bytes kept.
///
/// An approximation of the filesystem's own rule, which differs between
/// filesystems (ext4 casefold also normalizes; NTFS upcases per UTF-16
/// unit). It decides only what is *reported* before writing. Safety does not
/// rest on it: every entry is still created exclusively and published
/// without replacing, so a collision the key misses (Unicode normalization)
/// is caught when the entry is created, and nothing is overwritten.
pub fn case_fold_key(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    for chunk in name.utf8_chunks() {
        out.extend_from_slice(chunk.valid().to_lowercase().as_bytes());
        out.extend_from_slice(chunk.invalid());
    }
    out
}

/// A directory that restoration writes into (plan C6; spec §10.4). Names
/// are archive path components, raw bytes, and are never altered: a name
/// the platform cannot hold is reported ([`RestoreDir::name_issue`]), not
/// rewritten. No operation ever replaces or merges into an existing entry:
/// [`StorageError::Exists`] is how collisions surface, including the ones a
/// case-insensitive or normalizing filesystem creates.
pub trait RestoreDir: Sized {
    /// The file type created here.
    type File: Storage;

    /// Why `name` cannot be created here, if it cannot.
    fn name_issue(&self, name: &[u8]) -> Option<NameIssue>;

    /// Create the subdirectory `name`, which must not exist, and return it.
    fn create_dir(&mut self, name: &[u8]) -> Result<Self, StorageError>;

    /// Create the file `name`, which must not exist (a temporary name while
    /// its content is written and verified).
    fn create_file(&mut self, name: &[u8]) -> Result<Self::File, StorageError>;

    /// Make `from` appear at `to`, never replacing an existing `to`
    /// (`Exists`, and `from` is left as it was).
    fn publish_no_replace(&mut self, from: &[u8], to: &[u8]) -> Result<(), StorageError>;

    /// Remove a file this restoration created and still holds.
    fn discard(&mut self, file: Self::File, name: &[u8]) -> Result<(), StorageError>;

    /// Persist this directory's entries.
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError>;

    /// How this directory compares names. Asked of the restore root before
    /// anything is restored. A backend may create and remove a probe entry
    /// of its own to find out.
    fn case_behavior(&mut self) -> Result<CaseBehavior, StorageError>;

    /// Whether `name` (or a name the destination treats as the same) exists
    /// here, without following a symbolic link.
    fn entry_exists(&mut self, name: &[u8]) -> Result<bool, StorageError>;

    /// Apply promised attributes (plan O6) to the entry `name`, which this
    /// restoration created. Returns what could not be applied; never fails
    /// the restoration. The engine has already removed setuid and setgid
    /// unless they were requested.
    fn apply_attributes(
        &mut self,
        name: &[u8],
        kind: crate::catalog::namespace::EntryKind,
        attributes: &crate::manifest::Attributes,
    ) -> Vec<AttributeIssue>;
}

/// An attribute restoration could not apply (spec §10.4.1: "Restoration
/// MUST report unrestorable attributes as exceptions").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AttributeKind {
    /// The modification time.
    Mtime,
    /// POSIX permission bits.
    Mode,
    /// Setuid or setgid, not restored because it was not requested (O6).
    SetId,
    /// Numeric owner and group (needs privilege, O6).
    Ownership,
    /// The Windows read-only bit.
    ReadOnly,
    /// The Windows hidden or system bit.
    HiddenOrSystem,
}

/// One attribute that was not applied, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeIssue {
    pub attribute: AttributeKind,
    pub reason: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn case_fold_key_lowercases_unicode_and_keeps_other_bytes() {
        assert_eq!(case_fold_key(b"ReadMe.TXT"), b"readme.txt");
        assert_eq!(
            case_fold_key("\u{c9}T\u{c9}".as_bytes()),
            "\u{e9}t\u{e9}".as_bytes()
        );
        assert_eq!(case_fold_key(b"A\xffB"), b"a\xffb");
        assert_ne!(case_fold_key(b"a"), case_fold_key(b"b"));
    }

    #[test]
    fn windows_name_rules() {
        use NameIssue::*;
        let cases: &[(&[u8], Option<NameIssue>)] = &[
            (b"normal.txt", None),
            (b"CONSOLE", None),
            (b"COM0", None),
            (b"LPT10", None),
            (b"con.txt.bak", Some(Reserved)),
            (b"CON", Some(Reserved)),
            (b"con", Some(Reserved)),
            (b"Nul.txt", Some(Reserved)),
            (b"NUL .txt", Some(Reserved)),
            (b"aux.tar.gz", Some(Reserved)),
            (b"PRN", Some(Reserved)),
            (b"com1", Some(Reserved)),
            (b"LPT9.log", Some(Reserved)),
            ("COM\u{b9}".as_bytes(), Some(Reserved)),
            ("lpt\u{b3}".as_bytes(), Some(Reserved)),
            (b"a.", Some(TrailingDotOrSpace)),
            (b"a ", Some(TrailingDotOrSpace)),
            (b"a<b", Some(IllegalCharacter(b'<'))),
            (b"a:b", Some(IllegalCharacter(b':'))),
            (b"a\"b", Some(IllegalCharacter(b'"'))),
            (b"a\\b", Some(IllegalCharacter(b'\\'))),
            (b"a|b", Some(IllegalCharacter(b'|'))),
            (b"a*", Some(IllegalCharacter(b'*'))),
            (b"tab\there", Some(IllegalCharacter(b'\t'))),
            (b"\xff\xfe", Some(NotRepresentable)),
            ("caf\u{e9}".as_bytes(), None),
            // An unpaired surrogate in WTF-8 is a valid Windows name (O24).
            (b"\xed\xa0\x80", None),
        ];
        for (name, want) in cases {
            assert_eq!(
                windows_name_issue(name),
                *want,
                "{:?}",
                String::from_utf8_lossy(name)
            );
        }
    }
}
