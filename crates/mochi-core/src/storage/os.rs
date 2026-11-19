//! OS-backed storage.
//!
//! **The only module in `mochi-core` permitted to use `std::fs`** (this file
//! and its per-platform restore submodules in `storage/os/`). Everything else
//! reaches the disk through [`ReadStorage`] / [`Storage`].
//!
//! # Filesystem and durability assumptions (spec §12.2, plan C5, O12)
//!
//! Spec §12.2 requires these to be documented. MOCHI's single-file publish
//! only appends; it never renames or rewrites in place. What each call relies
//! on:
//!
//! **Linux / POSIX (Ubuntu 22.04, 24.04 — plan O11).**
//! * `sync_data` is `fdatasync(2)` (`File::sync_data`). POSIX requires it to
//!   flush the data *and the metadata needed to read it back*, which includes
//!   the file size, so an append needs no directory flush.
//! * `sync_directory` opens the parent directory and `fsync(2)`s it. Needed
//!   once, when a file is created (`create_new`), so its directory entry
//!   survives power loss. A failure is an error, not a degradation.
//! * Assumed: the device honours flush commands (no volatile write cache that
//!   lies), and the filesystem does not reorder a later `fdatasync`'d write
//!   ahead of an earlier one that was already acknowledged. If a device lies,
//!   MOCHI's acknowledgement is only as good as the device's (the testkit's
//!   `LieSync` fault shows what then survives: still never a mixed commit).
//! * After a failed `fsync` the state of unsynced pages is unknown (Linux may
//!   drop them and report success on the next call). The writer therefore
//!   stops (`WRITER_POISONED`) instead of retrying.
//!
//! **Windows (10 22H2+, 11 — plan O11, O12).**
//! * `sync_data` is `FlushFileBuffers` (what `File::sync_data` calls), which
//!   also flushes the file's metadata, including its size. Appending needs
//!   no directory flush.
//! * Creating (D13, T22): the first commit goes to a locked temporary file,
//!   which is published by `MoveFileExW(src, dst, MOVEFILE_WRITE_THROUGH)`
//!   **without** `MOVEFILE_REPLACE_EXISTING`, so an existing name is refused
//!   (`ERROR_ALREADY_EXISTS` → [`StorageError::Exists`]) and nothing is
//!   replaced; see [`OsDir`]. The call is made by the `mochi-sys` crate, the
//!   only place outside `mochi-foreign` that holds `unsafe`, so `mochi-core`
//!   keeps `forbid(unsafe_code)`. Owner decision of 2026-10-15, superseding
//!   T21 and checklist Q63 (hard link then unlink): neither of those steps is
//!   documented as write-through, which left no documented evidence for gate
//!   G6 on creation.
//!   Microsoft's `MoveFileExW` page says of `MOVEFILE_WRITE_THROUGH`: "The
//!   function does not return until the file is actually moved on the disk."
//!   Its next sentence ("Setting this value guarantees that a move performed
//!   as a copy and delete operation is flushed to disk before the function
//!   returns.") concerns the copy-and-delete case, which a rename within one
//!   directory is not.
//!
//!   **What the evidence shows (G6b, `docs/dev/windows-crash-replay.md`, #60).**
//!   On NTFS (Windows 11 build 26300.9457, Windows 10 22H2 build 19045.2006,
//!   emulated disk that honours FLUSH/FUA), the first sentence alone is *not*
//!   enough for a same-directory rename: a `MOVEFILE_WRITE_THROUGH` rename on
//!   its own lost the new name in 3 of 3 controls and left no flush in the
//!   block log. What makes a created archive's name survive a crash is the
//!   directory flush that [`sync_directory`](crate::storage::Storage::sync_directory)
//!   performs (`FlushFileBuffers` on the directory handle): with only that
//!   step suppressed, the real publication path lost the name at the crash
//!   point after each creation acknowledgement. **The shipped durability on
//!   Windows therefore rests on the directory flush**, which Microsoft does not
//!   document. This module does not rely on write-through for creation
//!   durability.
//!
//!   **What is reported, and why (G6 policy B, [owner, 2026-10-15]; spec
//!   Annex B.2.6 G6 and B.3).** `sync_directory` reports
//!   [`DirectoryDurability::Unconfirmed`] on Windows **even when the flush
//!   succeeds**, so the first commit of a new archive is reported as degraded
//!   ("directory durability unconfirmed"), and truncation there needs
//!   `--accept-unconfirmed-durability`. This is a policy disposition under
//!   plan O12 ("claim only what the Windows APIs document"), not a claim of
//!   documented durability and not a pass on physical machines. Changing it
//!   needs a documented primitive or a new owner decision; the weekly
//!   `g6-windows` workflow guards the observed behaviour as a regression test.
//!   [`OsDir`] only records whether the last publication was a
//!   `MoveWriteThrough` on an NTFS volume. Appends to an existing archive are
//!   unaffected.
//! * Windows CI (`windows-latest`) runs the storage conformance suite and the
//!   `windows_*` tests below: a locked temporary file is published (with the
//!   byte-range lock held on it and another handle open on it), an
//!   existing destination is refused with both files untouched, and two
//!   concurrent creators produce exactly one winner.
//!
//! **Locking (spec §12.2 step 1; owner decisions Q54 and 2026-10-20, spec
//! Annex B D17 and B.2.7 as amended by B.2.17, D27).** An archive's
//! publication lock is an OS lock **on the archive file itself**. There is no
//! separate lock file and nothing is created beside the archive.
//! * **Unix:** an exclusive `flock` (`File::try_lock`). It is advisory and
//!   does not affect readers. It belongs to the open file, not to the name,
//!   so hard links of the archive share it.
//! * **Windows:** `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK |
//!   LOCKFILE_FAIL_IMMEDIATELY)` on **one byte at offset 2^62**
//!   ([`WINDOWS_LOCK_OFFSET`]), released with `UnlockFileEx`, both through
//!   `mochi-sys`. Windows byte-range locks are mandatory, but only for the
//!   bytes they cover: a lock far beyond any practical file size conflicts with
//!   no read and no write of the real contents, so readers keep working while
//!   a writer holds it. (`File::try_lock` locks the whole range, which is why
//!   B.2.7 had used a separate file; that was error 33 on every other
//!   handle's reads.) Byte-range locks belong to the file, so hard links of
//!   one archive share the lock on Windows too.
//! * **Ownership.** Only the OS lock counts, and the OS drops it when the
//!   holder exits, so a crashed writer leaves nothing to clean up.
//! * **Creation (D13).** The temporary file is locked on itself
//!   ([`StorageDir::create_exclusive`]) before it is published. The lock is on
//!   the open handle, so it carries across the no-replace rename and the new
//!   archive is never visible unlocked; nothing else is taken.
//! * **The locker checks the name.** After locking an existing archive, the
//!   path (canonicalized when the handle was opened, so a later change of the
//!   working directory cannot redirect the check) must still name the file
//!   that was locked: on Unix `dev`/`ino` of the handle against
//!   `metadata(path)`; on Windows the volume serial number and file index
//!   (`GetFileInformationByHandle`, through `mochi-sys`, because `std`'s
//!   `volume_serial_number`/`file_index` are unstable) of the held handle
//!   against a second handle opened on the path for attribute access only,
//!   sharing everything. If they differ (the archive was replaced by hand),
//!   the handle is dropped and the path opened again, a bounded number of
//!   times (`LOCK_ATTEMPTS`, 8). A Windows filesystem that reports file
//!   index 0 has no identity to compare, so the check answers yes there. MOCHI
//!   never replaces or renames an archive itself.
//! * **Aliases.** Relative paths, `.`/`..`, symbolic links, short names,
//!   case on a case-insensitive filesystem and hard links all reach the same
//!   file, and the lock belongs to the file, so every alias is excluded.
//!   (B.2.7's Windows hard-link gap is closed by this.)
//! * **Legacy lock files.** Builds up to 0.9.2 kept the lock in a separate
//!   `<archive file name>.mochi-lock` file ([`LEGACY_LOCK_FILE_SUFFIX`]).
//!   While a writer holds an archive's lock (an existing archive just locked,
//!   or a created archive just published), [`sweep_legacy_lock_files`] looks
//!   at that archive's directory (bounded, no recursion, never follows a
//!   link, every error ignored) and removes each regular, empty
//!   `*.mochi-lock` file it can lock the way those builds did (whole-file
//!   `File::try_lock`), orphan or not, on Unix and on Windows. One held by
//!   anyone else is kept. Nothing sweeps on read-only commands. No build from
//!   0.9.3 on opens such a file, so the creation race of spec B.2.16 no
//!   longer arises between new builds; on Unix old builds also `flock` the
//!   archive, so old and new still exclude each other; **on Windows a build
//!   up to 0.9.2 and a build from 0.9.3 writing the same archive at the same
//!   time are not excluded** (spec B.2.17 item 6).
//! * Temporary files (D13) and sidecars (D14) are locked on themselves with
//!   the same primitive (`LockRole::Itself`): nobody reads them while they
//!   are held, and cleanup tests their own lock.
//! * **Readers take no lock.** Concurrent reads are correct because a reader
//!   interprets only what a valid footer commits (the size is taken once,
//!   §12.2: "Readers MUST NOT assume the only recoverable commit is at
//!   physical EOF"), not because of the lock.
//! * Advisory on Linux: a process that ignores the lock is not stopped,
//!   which is why the writer also re-checks the file size before each commit
//!   (spec §12.5: `O_APPEND` alone is insufficient).
//! * A read-only archive cannot be appended to, lock or no lock (the file is
//!   opened for writing); the directory's permissions do not matter any more.
//!
//! **Network filesystems** (NFS, SMB) are not supported for writing: their
//! locking and flush semantics vary, and none of the above is assumed there.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::{
    check_file_name, AttributeIssue, AttributeKind, DirectoryDurability, Existing, NameIssue,
    ReadStorage, RemoveOutcome, RestoreDir, Storage, StorageDir, StorageError,
};
use crate::catalog::namespace::EntryKind;
use crate::manifest::{Attributes, Mtime, WINDOWS_HIDDEN, WINDOWS_READONLY, WINDOWS_SYSTEM};

#[cfg(not(any(unix, windows)))]
compile_error!("mochi-core storage supports only Unix and Windows targets");

/// Largest offset the OS read calls accept (`off_t` is signed). A file cannot
/// be that large, so any larger, archive-derived offset is simply past the end
/// and must read as EOF rather than surface a confusing `EINVAL`.
const MAX_OS_OFFSET: u64 = i64::MAX as u64;

fn read_at_impl(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    if offset > MAX_OS_OFFSET {
        return Ok(0);
    }
    os_read_at(file, offset, buf)
}

#[cfg(unix)]
fn os_read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn os_read_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(file, buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = buf.get(n..).unwrap_or_default();
        offset = offset.saturating_add(n as u64);
    }
    Ok(())
}

/// `path` canonicalized (symbolic links, `.`/`..` and, on Windows, short names
/// resolved), else as given. A handle keeps this so a later change of the
/// working directory cannot make its path name a different file than the one
/// it opened (the check after locking, spec B.2.17 item 4).
fn absolute(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// What a successful directory flush counts as on Windows (spec Annex B.2.20,
/// D30; plan O12). Microsoft documents no call that makes a new name in a
/// directory durable, so by default ([`Self::Unconfirmed`], the G6 policy B of
/// 2026-10-15) the flush runs and the result is reported as unconfirmed. A
/// user may choose [`Self::TrustLocalNtfs`]: then a flush that succeeded on a
/// local NTFS volume counts as confirmed, which is what the G6 crash replays
/// observed (a flushed new name survived 3 of 3, an unflushed one 0 of 3) but
/// not a documented guarantee. Network locations and every other filesystem
/// stay unconfirmed either way. Unix ignores the setting: `fsync` on a
/// directory is the documented call there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowsDirectoryFlush {
    /// Policy B: never confirmed on Windows (the default).
    #[default]
    Unconfirmed,
    /// A successful flush on a local NTFS volume counts as confirmed.
    TrustLocalNtfs,
}

/// The verdict on a Windows directory flush (D30), kept apart from the Win32
/// calls so every platform tests it: `filesystem` is the volume's filesystem
/// name, `network` whether the directory is on a network location.
pub fn windows_flush_verdict(
    flushed: &io::Result<()>,
    policy: WindowsDirectoryFlush,
    filesystem: Option<&str>,
    network: bool,
) -> DirectoryDurability {
    match flushed {
        Err(e) => {
            DirectoryDurability::Unconfirmed(format!("best-effort directory flush failed: {e}"))
        }
        Ok(()) => match policy {
            WindowsDirectoryFlush::Unconfirmed => DirectoryDurability::Unconfirmed(
                "directory flush ran, but Windows does not document it as durable".into(),
            ),
            WindowsDirectoryFlush::TrustLocalNtfs
                if !network && filesystem.is_some_and(|f| f.eq_ignore_ascii_case("NTFS")) =>
            {
                DirectoryDurability::Confirmed
            }
            WindowsDirectoryFlush::TrustLocalNtfs => DirectoryDurability::Unconfirmed(format!(
                "directory flush ran; it is trusted only on a local NTFS volume, and this is {}",
                if network {
                    "a network location"
                } else {
                    filesystem.unwrap_or("an unknown filesystem")
                }
            )),
        },
    }
}

/// Whether `dir` is on a network location: a UNC path, or (after
/// canonicalization, which resolves a mapped drive letter to its share) a
/// `\\?\UNC\` path.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_network_path(dir: &Path) -> bool {
    let resolved = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    is_network_text(&resolved.to_string_lossy())
}

/// [`is_network_path`] on the path's text.
fn is_network_text(path: &str) -> bool {
    let text = path.replace('/', "\\");
    text.starts_with("\\\\?\\UNC\\")
        || (text.starts_with("\\\\")
            && !text.starts_with("\\\\?\\")
            && !text.starts_with("\\\\.\\"))
}

#[cfg(unix)]
fn sync_dir(dir: &Path, _: WindowsDirectoryFlush) -> Result<DirectoryDurability, StorageError> {
    File::open(dir)?.sync_all()?;
    Ok(DirectoryDurability::Confirmed)
}

#[cfg(windows)]
fn sync_dir(
    dir: &Path,
    policy: WindowsDirectoryFlush,
) -> Result<DirectoryDurability, StorageError> {
    // Best effort; confirmed only as `policy` allows (D30, plan O12).
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let flushed = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .and_then(|d| d.sync_all());
    let (filesystem, network) = match policy {
        WindowsDirectoryFlush::Unconfirmed => (None, false),
        WindowsDirectoryFlush::TrustLocalNtfs => {
            (volume_filesystem_name(dir), is_network_path(dir))
        }
    };
    Ok(windows_flush_verdict(
        &flushed,
        policy,
        filesystem.as_deref(),
        network,
    ))
}

/// Read-only handle. There is no write API on this type, and the file is opened
/// without write access, so verification cannot modify the archive.
#[derive(Debug)]
pub struct OsReadStorage {
    file: File,
}

impl OsReadStorage {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let file = OpenOptions::new().read(true).open(path.as_ref())?;
        Ok(Self { file })
    }
}

impl ReadStorage for OsReadStorage {
    fn size(&self) -> Result<u64, StorageError> {
        Ok(self.file.metadata()?.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        Ok(read_at_impl(&self.file, offset, buf)?)
    }
}

/// Name suffix of the separate writer lock file that builds up to 0.9.2 kept
/// beside every archive (Q54; spec Annex B.2.7, superseded by B.2.17, D27).
/// **No build from 0.9.3 on opens or creates such a file**; the name is kept
/// only so [`sweep_legacy_lock_files`] and the tests can recognise one.
pub const LEGACY_LOCK_FILE_SUFFIX: &str = ".mochi-lock";

/// The byte the writer lock covers on Windows: offset 2^62 (spec B.2.17 item
/// 2). It is far beyond any practical file size, so the lock conflicts with no
/// read or write of the archive's real bytes. Every implementation that
/// writes the same archives must use this byte.
pub const WINDOWS_LOCK_OFFSET: u64 = 0x4000_0000_0000_0000;

/// How often an archive locker starts over when the path no longer names the
/// file it locked (the archive was replaced by hand in between). Reaching the
/// end is not a real case.
const LOCK_ATTEMPTS: usize = 8;

/// Most directory entries one legacy sweep reads.
const SWEEP_MAX_ENTRIES: usize = 4096;

/// Take `File::try_lock` (the whole file), mapping contention to
/// [`StorageError::LockHeld`]. This is what builds up to 0.9.2 took on their
/// lock files, and so what the legacy sweep takes; on Unix it is also the
/// writer lock.
fn try_lock_whole_file(file: &File) -> Result<(), StorageError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(std::fs::TryLockError::WouldBlock) => Err(StorageError::LockHeld),
        Err(std::fs::TryLockError::Error(e)) => Err(StorageError::Io(e)),
    }
}

/// Take the writer lock on `file` itself, without waiting (spec B.2.17 item
/// 2): Unix `flock`. `LockHeld` if another handle holds it.
#[cfg(unix)]
fn lock_file_handle(file: &File) -> Result<(), StorageError> {
    try_lock_whole_file(file)
}

/// Take the writer lock on `file` itself, without waiting (spec B.2.17 item
/// 2): one byte at [`WINDOWS_LOCK_OFFSET`]. `LockHeld` if another handle
/// holds it.
#[cfg(windows)]
fn lock_file_handle(file: &File) -> Result<(), StorageError> {
    match mochi_sys::try_lock_byte(file, WINDOWS_LOCK_OFFSET) {
        Ok(true) => Ok(()),
        Ok(false) => Err(StorageError::LockHeld),
        // Anything but a lock violation is a problem with the volume, not a
        // second writer: say which lock and why it may fail.
        Err(e) => Err(StorageError::Io(io::Error::new(
            e.kind(),
            format!(
                "cannot take the writer lock (a one-byte LockFileEx at offset 2^62): {e}. \
                 The filesystem of this volume may not support byte-range locks that far \
                 beyond the end of a file; archives are not supported there"
            ),
        ))),
    }
}

#[cfg(unix)]
fn unlock_file_handle(file: &File) -> io::Result<()> {
    file.unlock()
}

#[cfg(windows)]
fn unlock_file_handle(file: &File) -> io::Result<()> {
    mochi_sys::unlock_byte(file, WINDOWS_LOCK_OFFSET)
}

/// Whether `path` (followed through links, as the open was) still names the
/// file `file` is open on. Unix compares `dev`/`ino`: `file` keeps its inode
/// alive, so the number cannot have been reused.
#[cfg(unix)]
fn names_same_file(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata()?;
    match std::fs::metadata(path) {
        Ok(named) => Ok((named.dev(), named.ino()) == (held.dev(), held.ino())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Windows: the volume serial number and file index of the held handle
/// against those of a second handle opened on the path (`mochi-sys`,
/// `GetFileInformationByHandle`). The second handle asks for attribute access
/// only and shares everything, so it never blocks a reader, a writer, a
/// delete or a rename. A filesystem that reports file index 0 has no identity
/// to compare; the check then answers yes.
#[cfg(windows)]
fn names_same_file(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    /// `FILE_READ_ATTRIBUTES`, and `FILE_SHARE_READ | WRITE | DELETE`.
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    let held = mochi_sys::file_identity(file)?;
    let named = match OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .open(path)
    {
        Ok(named) => named,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let named = mochi_sys::file_identity(&named)?;
    Ok(held.1 == 0 || held == named)
}

/// The directory a legacy sweep looks at for the archive at `archive`: the
/// directory of its canonical path, which is where builds up to 0.9.2 put the
/// lock file (they took both parts from the canonical path), else its
/// literal parent.
fn sweep_dir(archive: &Path) -> PathBuf {
    std::fs::canonicalize(archive)
        .ok()
        .and_then(|canonical| canonical.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| parent_dir(archive).to_path_buf())
}

/// Whether `name` is `<something>.mochi-lock` with a non-empty `<something>`.
/// The suffix is ASCII, so a lossy conversion cannot create or hide a match.
fn is_legacy_lock_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.len() > LEGACY_LOCK_FILE_SUFFIX.len() && name.ends_with(LEGACY_LOCK_FILE_SUFFIX)
}

/// Remove the legacy `*.mochi-lock` files in `dir` (spec B.2.17 item 5):
/// regular, empty files that are not held by anyone else, whether or not
/// their archive exists. Best effort: it looks at this one directory only,
/// never recurses, never follows a link, reads at most `SWEEP_MAX_ENTRIES` (4,096)
/// entries, and every error is ignored, so it cannot fail the operation that
/// triggered it. Returns how many files it removed.
///
/// Writers call it while they hold an archive's lock; read-only commands
/// never do.
pub fn sweep_legacy_lock_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.take(SWEEP_MAX_ENTRIES).flatten() {
        if !is_legacy_lock_name(&entry.file_name()) {
            continue;
        }
        // `DirEntry::file_type` does not follow links: a link, a directory
        // or anything else that is not a regular file is kept.
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if remove_legacy_lock_file(&entry.path()) {
            removed += 1;
        }
    }
    removed
}

/// `O_NOFOLLOW | O_NONBLOCK` for the sweep's open (Linux: from `rustix`, as
/// the values differ between architectures).
#[cfg(target_os = "linux")]
const SWEEP_OPEN_FLAGS: i32 =
    (rustix::fs::OFlags::NOFOLLOW.bits() | rustix::fs::OFlags::NONBLOCK.bits()) as i32;

/// The same on the other Unixes (macOS and the BSDs agree on these values;
/// neither is a supported platform, plan O11).
#[cfg(all(unix, not(target_os = "linux")))]
const SWEEP_OPEN_FLAGS: i32 = 0x100 | 0x4;

/// `FILE_FLAG_OPEN_REPARSE_POINT` (Win32 `winbase.h`): open a link itself,
/// not its target.
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// `FILE_ATTRIBUTE_REPARSE_POINT` (Win32 `winnt.h`).
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// One legacy lock file: lock it the way old builds did (whole file), check
/// that it is what the name says (the very file the path names, regular, not
/// a link, empty), then unlink it **while still holding the lock**. Anything
/// unexpected leaves the file. No `create`: a sweep never makes the file it
/// is looking for.
fn remove_legacy_lock_file(path: &Path) -> bool {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, FILE_FLAG_OPEN_REPARSE_POINT);
    // Unix: never follow a link (`ELOOP`), and never block opening a FIFO
    // that was swapped in after the directory was read.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, SWEEP_OPEN_FLAGS);
    let Ok(file) = options.open(path) else {
        return false;
    };
    // Held by anyone else (or an error): not ours to remove.
    if try_lock_whole_file(&file).is_err() {
        return false;
    }
    let Ok(held) = file.metadata() else {
        return false;
    };
    // A lock file is never written, so anything with bytes is not one of
    // ours.
    if !held.is_file() || held.len() != 0 || !names_exactly(&file, path) {
        return false;
    }
    let removed = std::fs::remove_file(path).is_ok();
    let _ = file.unlock();
    removed
}

/// Whether `path`, **without** following a link, is the very file `file` is
/// open on. Unix compares `dev`/`ino`. On Windows the handle was opened with
/// `FILE_FLAG_OPEN_REPARSE_POINT`, so a link is what we hold and its
/// reparse-point attribute is refused; there is no stable safe file
/// identity beyond that.
#[cfg(unix)]
fn names_exactly(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::symlink_metadata(path)) {
        (Ok(held), Ok(named)) => {
            named.file_type().is_file() && (held.dev(), held.ino()) == (named.dev(), named.ino())
        }
        _ => false,
    }
}

#[cfg(windows)]
fn names_exactly(file: &File, _path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    file.metadata().is_ok_and(|held| {
        held.is_file() && held.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    })
}

/// What [`Storage::try_lock_exclusive`] does for an [`OsStorage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockRole {
    /// An archive: lock it, then check the path still names it and sweep the
    /// legacy lock files of its directory.
    Archive,
    /// A temporary file or sidecar: lock it on itself, nothing more.
    Itself,
}

/// Read-write handle for a single writer.
#[derive(Debug)]
pub struct OsStorage {
    file: File,
    path: PathBuf,
    role: LockRole,
    /// Whether this handle holds the writer lock on `file`. Taking it again
    /// is a no-op: `flock` (Linux) allows that, but `LockFileEx` (Windows)
    /// refuses an overlapping lock even from the same handle, so the state is
    /// kept here rather than asked of the OS (found by T22 on
    /// `windows-latest`).
    locked: bool,
    /// What a directory flush counts as on Windows (D30).
    flush: WindowsDirectoryFlush,
}

impl Drop for OsStorage {
    /// Release the lock at once. Closing the handle would too, but on Windows
    /// only after a delay the OS does not bound.
    fn drop(&mut self) {
        let _ = self.unlock_handle();
    }
}

impl OsStorage {
    /// Create a new file; fails if it already exists (never silently replaces).
    pub fn create_new(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self::new(file, absolute(path), LockRole::Archive))
    }

    /// Open an existing file for reading and writing.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok(Self::new(file, absolute(path), LockRole::Archive))
    }

    fn new(file: File, path: PathBuf, role: LockRole) -> Self {
        Self {
            file,
            path,
            role,
            locked: false,
            flush: WindowsDirectoryFlush::default(),
        }
    }

    /// What a directory flush of this file's directory counts as on Windows
    /// (D30). The default is policy B, never confirmed.
    pub fn with_windows_flush(mut self, flush: WindowsDirectoryFlush) -> Self {
        self.flush = flush;
        self
    }

    /// A temporary or sidecar file, locked on itself and nothing more.
    fn itself(file: File, path: PathBuf) -> Self {
        Self::new(file, path, LockRole::Itself)
    }

    /// Take the writer lock on the open file, if this handle does not hold it.
    fn lock_handle(&mut self) -> Result<(), StorageError> {
        if !self.locked {
            lock_file_handle(&self.file)?;
            self.locked = true;
        }
        Ok(())
    }

    fn unlock_handle(&mut self) -> Result<(), StorageError> {
        if self.locked {
            unlock_file_handle(&self.file)?;
            self.locked = false;
        }
        Ok(())
    }

    /// Lock an archive (spec B.2.17 items 2, 4 and 5): the lock on the file;
    /// on Unix then a check that the path still names it (else release, open
    /// the path again and start over, [`LOCK_ATTEMPTS`] times); then the
    /// legacy sweep of its directory, best effort.
    fn lock_archive(&mut self) -> Result<(), StorageError> {
        for _ in 0..LOCK_ATTEMPTS {
            self.lock_handle()?;
            match names_same_file(&self.file, &self.path) {
                Ok(true) => {
                    sweep_legacy_lock_files(&sweep_dir(&self.path));
                    return Ok(());
                }
                Ok(false) => {
                    // The archive was replaced after we opened it: what we
                    // hold is a file no path names. Follow the path.
                    self.unlock_handle()?;
                    self.file = OpenOptions::new().read(true).write(true).open(&self.path)?;
                }
                Err(e) => {
                    let _ = self.unlock_handle();
                    return Err(e.into());
                }
            }
        }
        Err(StorageError::Io(io::Error::other(format!(
            "{} was replaced again and again while locking it",
            self.path.display()
        ))))
    }
}

impl ReadStorage for OsStorage {
    fn size(&self) -> Result<u64, StorageError> {
        Ok(self.file.metadata()?.len())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        Ok(read_at_impl(&self.file, offset, buf)?)
    }
}

impl Storage for OsStorage {
    fn append(&mut self, data: &[u8]) -> Result<u64, StorageError> {
        let offset = self.size()?;
        let len = data.len() as u64;
        if offset.checked_add(len).is_none() {
            return Err(StorageError::OutOfBounds {
                offset,
                len,
                size: offset,
            });
        }
        write_all_at(&self.file, data, offset)?;
        Ok(offset)
    }

    fn sync_data(&mut self) -> Result<(), StorageError> {
        Ok(self.file.sync_data()?)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        sync_dir(parent_dir(&self.path), self.flush)
    }

    fn try_lock_exclusive(&mut self) -> Result<(), StorageError> {
        if self.locked {
            return Ok(());
        }
        match self.role {
            LockRole::Itself => self.lock_handle(),
            LockRole::Archive => self.lock_archive(),
        }
    }

    fn unlock(&mut self) -> Result<(), StorageError> {
        self.unlock_handle()
    }

    fn truncate(&mut self, new_len: u64) -> Result<(), StorageError> {
        let size = self.size()?;
        if new_len > size {
            return Err(StorageError::OutOfBounds {
                offset: new_len,
                len: 0,
                size,
            });
        }
        Ok(self.file.set_len(new_len)?)
    }
}

/// The directory that holds an archive (plan T19; Annex B.2 D13, D14).
///
/// **Publication without replacing** (plan T20):
/// * **Linux:** one atomic `renameat2(RENAME_NOREPLACE)`. `EEXIST` is
///   `Exists`. If the filesystem (`EINVAL`) or kernel (`ENOSYS`) rejects the
///   flag, it falls back to the portable mechanism below.
/// * **Portable** (other Unix, and Linux's fallback): a hard link to the new
///   name, then removal of the old one. `link(2)` fails if the new name
///   exists, so nothing is ever replaced. A crash between the two steps leaves both names on the same
///   bytes; cleanup removes the temporary name once its lock is free.
///
/// * **Windows:** `MoveFileExW(…, MOVEFILE_WRITE_THROUGH)` without
///   `MOVEFILE_REPLACE_EXISTING`, via `mochi-sys` (owner decision 2026-10-15,
///   superseding T21 / Q63). It fails if the new name exists, so nothing is
///   replaced, and it never falls back to link then unlink.
///
/// A filesystem with neither refuses publication with an I/O error; nothing
/// ever falls back to a replacing rename.
#[derive(Debug)]
pub struct OsDir {
    path: PathBuf,
    last_publish: Option<PublishMechanism>,
    /// Whether the last publication was a [`PublishMechanism::MoveWriteThrough`]
    /// on a volume whose filesystem is NTFS (see [`is_ntfs_write_through`]).
    last_publish_ntfs_write_through: bool,
    /// What a directory flush counts as on Windows (D30), for this directory
    /// and every file it opens or creates.
    flush: WindowsDirectoryFlush,
}

/// How the last [`StorageDir::publish_no_replace`] was carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMechanism {
    /// One atomic `renameat2(RENAME_NOREPLACE)` (Linux).
    RenameNoReplace,
    /// Hard link to the new name, then unlink of the old: the portable
    /// mechanism, and Linux's fallback where the filesystem or kernel
    /// rejects the flag.
    LinkThenUnlink,
    /// `MoveFileExW(…, MOVEFILE_WRITE_THROUGH)` without
    /// `MOVEFILE_REPLACE_EXISTING` (Windows, through `mochi-sys`).
    MoveWriteThrough,
}

impl OsDir {
    /// The mechanism the last successful publication used (diagnostics and
    /// tests).
    pub fn last_publish(&self) -> Option<PublishMechanism> {
        self.last_publish
    }

    /// Whether the last publication was a `MoveWriteThrough` on NTFS. Nothing
    /// reports it: Windows stays `Unconfirmed` in 1.0 by the G6 policy
    /// ([owner, 2026-10-15]; module docs).
    #[cfg_attr(not(test), allow(dead_code))]
    fn last_publish_was_ntfs_write_through(&self) -> bool {
        self.last_publish_ntfs_write_through
    }

    fn record_publish(&mut self, mechanism: PublishMechanism) {
        self.last_publish = Some(mechanism);
        self.last_publish_ntfs_write_through = mechanism == PublishMechanism::MoveWriteThrough
            && is_ntfs_write_through(mechanism, volume_filesystem_name(&self.path).as_deref());
    }
}

/// Whether a publication by `mechanism` on a volume named `filesystem` is the
/// documented-write-through-on-NTFS case G6 asks about.
fn is_ntfs_write_through(mechanism: PublishMechanism, filesystem: Option<&str>) -> bool {
    mechanism == PublishMechanism::MoveWriteThrough
        && filesystem.is_some_and(|name| name.eq_ignore_ascii_case("NTFS"))
}

#[cfg(windows)]
fn volume_filesystem_name(dir: &Path) -> Option<String> {
    mochi_sys::volume_filesystem_name(dir).ok()
}

#[cfg(not(windows))]
fn volume_filesystem_name(_: &Path) -> Option<String> {
    None
}

/// Also the restore engine's publication on Windows (`restore_windows.rs`):
/// restored files are not archives, so G6 does not concern them.
fn link_then_unlink(src: &Path, dst: &Path, to: &str) -> Result<PublishMechanism, StorageError> {
    std::fs::hard_link(src, dst).map_err(exists_as(to))?;
    std::fs::remove_file(src)?;
    Ok(PublishMechanism::LinkThenUnlink)
}

/// What a failed `renameat2(RENAME_NOREPLACE)` means (plan T20).
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameOutcome {
    /// The destination exists: never replaced.
    Exists,
    /// The filesystem (`EINVAL`) or kernel (`ENOSYS`) does not support the
    /// flag: fall back to link then unlink, which is also no-replace.
    Unsupported,
    /// Any other failure is reported as is.
    Failed,
}

#[cfg(any(target_os = "linux", test))]
fn classify_rename_errno(raw: i32) -> RenameOutcome {
    const EEXIST: i32 = 17;
    const EINVAL: i32 = 22;
    const ENOSYS: i32 = 38;
    match raw {
        EEXIST => RenameOutcome::Exists,
        EINVAL | ENOSYS => RenameOutcome::Unsupported,
        _ => RenameOutcome::Failed,
    }
}

#[cfg(target_os = "linux")]
fn publish_no_replace_at(
    src: &Path,
    dst: &Path,
    to: &str,
) -> Result<PublishMechanism, StorageError> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    publish_with(src, dst, to, |s, d| {
        renameat_with(CWD, s, CWD, d, RenameFlags::NOREPLACE).map_err(|e| e.raw_os_error())
    })
}

/// The Linux decision, with the no-replace rename passed in (it returns the
/// raw errno on failure), so the fallback can be tested on filesystems that
/// support the flag.
#[cfg(any(target_os = "linux", test))]
fn publish_with(
    src: &Path,
    dst: &Path,
    to: &str,
    rename_noreplace: impl FnOnce(&Path, &Path) -> Result<(), i32>,
) -> Result<PublishMechanism, StorageError> {
    match rename_noreplace(src, dst) {
        Ok(()) => Ok(PublishMechanism::RenameNoReplace),
        Err(errno) => match classify_rename_errno(errno) {
            RenameOutcome::Exists => Err(StorageError::Exists {
                name: to.to_string(),
            }),
            RenameOutcome::Unsupported => link_then_unlink(src, dst, to),
            RenameOutcome::Failed => Err(StorageError::Io(io::Error::from_raw_os_error(errno))),
        },
    }
}

/// Windows: one `MoveFileExW` with write-through and without replacing. The
/// source is untouched when the destination exists.
#[cfg(windows)]
fn publish_no_replace_at(
    src: &Path,
    dst: &Path,
    to: &str,
) -> Result<PublishMechanism, StorageError> {
    mochi_sys::move_no_replace_write_through(src, dst).map_err(exists_as(to))?;
    Ok(PublishMechanism::MoveWriteThrough)
}

/// Rename the folder (or file) `src` to `dst` on the same volume, never
/// replacing anything: an existing `dst` (an empty folder included) is
/// [`io::ErrorKind::AlreadyExists`] and `src` is left as it was. A move to
/// another volume fails ([`io::ErrorKind::CrossesDevices`]); nothing is
/// copied. Linux: `renameat2(RENAME_NOREPLACE)`, refused where the
/// filesystem lacks the flag (a folder cannot be hard-linked, so there is no
/// safe fallback). Windows: `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`.
/// Elsewhere: refused. The desktop app renames and moves the folder its quick
/// "Extract to new folder" made with it.
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    rename_no_replace_impl(src, dst)
}

#[cfg(target_os = "linux")]
fn rename_no_replace_impl(src: &Path, dst: &Path) -> io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};
    const EXDEV: i32 = 18;
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(e) => {
            let raw = e.raw_os_error();
            match classify_rename_errno(raw) {
                RenameOutcome::Exists => Err(io::Error::from(io::ErrorKind::AlreadyExists)),
                RenameOutcome::Unsupported => Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "this file system cannot rename without the risk of replacing a name",
                )),
                RenameOutcome::Failed if raw == EXDEV => {
                    Err(io::Error::from(io::ErrorKind::CrossesDevices))
                }
                RenameOutcome::Failed => Err(io::Error::from_raw_os_error(raw)),
            }
        }
    }
}

#[cfg(windows)]
fn rename_no_replace_impl(src: &Path, dst: &Path) -> io::Result<()> {
    // ERROR_NOT_SAME_DEVICE: without MOVEFILE_COPY_ALLOWED nothing is copied.
    const ERROR_NOT_SAME_DEVICE: i32 = 17;
    match mochi_sys::move_no_replace_write_through(src, dst) {
        Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) => {
            Err(io::Error::from(io::ErrorKind::CrossesDevices))
        }
        other => other,
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn rename_no_replace_impl(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn publish_no_replace_at(
    src: &Path,
    dst: &Path,
    to: &str,
) -> Result<PublishMechanism, StorageError> {
    link_then_unlink(src, dst, to)
}

impl OsDir {
    /// The directory at `path`, which must exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if !std::fs::metadata(&path)?.is_dir() {
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", path.display()),
            )));
        }
        Ok(Self {
            path,
            last_publish: None,
            last_publish_ntfs_write_through: false,
            flush: WindowsDirectoryFlush::default(),
        })
    }

    /// What a directory flush counts as on Windows for this directory and the
    /// files it opens or creates (D30). The default is policy B.
    pub fn with_windows_flush(mut self, flush: WindowsDirectoryFlush) -> Self {
        self.flush = flush;
        self
    }

    /// The directory that contains `file`.
    pub fn containing(file: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open(parent_dir(file.as_ref()))
    }

    fn entry(&self, name: &str) -> Result<PathBuf, StorageError> {
        check_file_name(name)?;
        Ok(self.path.join(name))
    }
}

fn exists_as(name: &str) -> impl Fn(io::Error) -> StorageError + '_ {
    move |e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            StorageError::Exists {
                name: name.to_string(),
            }
        } else {
            StorageError::Io(e)
        }
    }
}

impl StorageDir for OsDir {
    type File = OsStorage;

    fn open(&mut self, name: &str) -> Result<OsStorage, StorageError> {
        Ok(OsStorage::open_existing(self.entry(name)?)?.with_windows_flush(self.flush))
    }

    fn create_exclusive(&mut self, name: &str) -> Result<OsStorage, StorageError> {
        let path = self.entry(name)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(exists_as(name))?;
        let mut s = OsStorage::itself(file, path).with_windows_flush(self.flush);
        s.try_lock_exclusive()?;
        Ok(s)
    }

    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError> {
        let (src, dst) = (self.entry(from)?, self.entry(to)?);
        let mechanism = publish_no_replace_at(&src, &dst, to)?;
        self.record_publish(mechanism);
        Ok(())
    }

    /// `file` holds the temporary file's lock, taken on the file itself by
    /// [`create_exclusive`](StorageDir::create_exclusive) (it is taken here if
    /// it somehow is not), so the archive is never visible unlocked: the lock
    /// is on the open handle and carries across the no-replace rename. Then
    /// publishes, and sweeps the legacy lock files of the archive's
    /// directory (spec B.2.17 item 5, best effort). On a failed publication
    /// the temporary file stays locked by `file` for its creator to discard.
    fn publish_archive(
        &mut self,
        file: &mut OsStorage,
        from: &str,
        to: &str,
    ) -> Result<(), StorageError> {
        let (src, dst) = (self.entry(from)?, self.entry(to)?);
        file.lock_handle()?;
        let mechanism = publish_no_replace_at(&src, &dst, to)?;
        self.record_publish(mechanism);
        file.path = absolute(dst);
        file.role = LockRole::Archive;
        // Trigger b of the legacy sweep: a creator published and still holds
        // the new archive's lock.
        sweep_legacy_lock_files(&sweep_dir(&file.path));
        Ok(())
    }

    fn discard(&mut self, file: OsStorage, name: &str) -> Result<(), StorageError> {
        let path = self.entry(name)?;
        // Removed while still held: no other process can take the name's
        // lock in between and see a half-removed file.
        let removed = std::fs::remove_file(&path);
        drop(file);
        Ok(removed?)
    }

    fn remove_if_unlocked(&mut self, name: &str) -> Result<RemoveOutcome, StorageError> {
        let path = self.entry(name)?;
        let file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(RemoveOutcome::Missing),
            Err(e) => return Err(e.into()),
        };
        match lock_file_handle(&file) {
            Ok(()) => {}
            Err(StorageError::LockHeld) => return Ok(RemoveOutcome::Locked),
            Err(e) => return Err(e),
        }
        let removed = std::fs::remove_file(&path);
        // Released on every path, not left to the close (on Windows that
        // happens after a delay the OS does not bound).
        let _ = unlock_file_handle(&file);
        drop(file);
        removed?;
        Ok(RemoveOutcome::Removed)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        sync_dir(&self.path, self.flush)
    }
}

// ---- Restore destinations (plan C6) ----------------------------------------
//
// One implementation per supported platform, each in its own module so it
// can be worked on separately (docs/dev/restore-platforms.md). All three
// export `OsRestoreDir`, which implements `RestoreDir`.

/// How an [`OsRestoreDir`] moves an existing file out of the way so a
/// restored file can take its name (`OnExisting::Replace`, owner decision
/// 2026-11-11): given the file's full path, move it to where the user can get
/// it back (the app passes the Recycle Bin / Trash). Core has no such place of
/// its own, so a restore directory opened without one refuses to replace.
pub type SetAside = dyn Fn(&Path) -> io::Result<()> + Send + Sync;

#[cfg(target_os = "linux")]
mod restore_linux;
#[cfg(target_os = "linux")]
pub use restore_linux::OsRestoreDir;

#[cfg(windows)]
mod restore_windows;
#[cfg(windows)]
pub use restore_windows::OsRestoreDir;

#[cfg(not(any(target_os = "linux", windows)))]
mod restore_unsupported;
#[cfg(not(any(target_os = "linux", windows)))]
pub use restore_unsupported::OsRestoreDir;

// ---- Import sources (plan C14, D2) ------------------------------------------

mod source;
pub use source::{OsFile, OsSourceTree};

// ---- Find's folder tree (spec Annex B.2.18, D28) -----------------------------

pub mod find;
pub use find::OsFindTree;

fn issue(issues: &mut Vec<AttributeIssue>, attribute: AttributeKind, reason: impl ToString) {
    issues.push(AttributeIssue {
        attribute,
        reason: reason.to_string(),
    });
}

/// An archive time as a `SystemTime`, or `None` if it is out of range.
fn system_time(m: Mtime) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};
    if m.nanos >= 1_000_000_000 {
        return None;
    }
    let nanos = Duration::from_nanos(u64::from(m.nanos));
    if m.secs >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::from_secs(m.secs.unsigned_abs()))?
            .checked_add(nanos)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(m.secs.unsigned_abs()))?
            .checked_add(nanos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mochi");
        (dir, path)
    }

    #[test]
    fn append_returns_offsets_and_reads_back() {
        let (_d, path) = tmp();
        let mut s = OsStorage::create_new(&path).unwrap();
        assert_eq!(s.size().unwrap(), 0);
        assert_eq!(s.append(b"hello").unwrap(), 0);
        assert_eq!(s.append(b" world").unwrap(), 5);
        s.sync_data().unwrap();
        let dir = s.sync_directory().unwrap();
        #[cfg(unix)]
        assert_eq!(dir, DirectoryDurability::Confirmed);
        #[cfg(windows)]
        assert!(matches!(dir, DirectoryDurability::Unconfirmed(_)));
        let mut buf = [0u8; 11];
        s.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
    }

    #[test]
    fn a_windows_flush_counts_only_as_the_user_chose_and_only_on_local_ntfs() {
        use WindowsDirectoryFlush::{TrustLocalNtfs, Unconfirmed};
        let ok: io::Result<()> = Ok(());
        let failed: io::Result<()> = Err(io::Error::other("no"));
        let confirmed = |d: DirectoryDurability| matches!(d, DirectoryDurability::Confirmed);
        // Policy B, the default: never confirmed, whatever the volume.
        assert_eq!(WindowsDirectoryFlush::default(), Unconfirmed);
        assert!(!confirmed(windows_flush_verdict(
            &ok,
            Unconfirmed,
            Some("NTFS"),
            false
        )));
        // Trusted: confirmed on local NTFS (any letter case) only.
        assert!(confirmed(windows_flush_verdict(
            &ok,
            TrustLocalNtfs,
            Some("NTFS"),
            false
        )));
        assert!(confirmed(windows_flush_verdict(
            &ok,
            TrustLocalNtfs,
            Some("ntfs"),
            false
        )));
        for (fs, network) in [
            (Some("NTFS"), true),
            (Some("FAT32"), false),
            (Some("exFAT"), false),
            (Some("ReFS"), false),
            (None, false),
        ] {
            let d = windows_flush_verdict(&ok, TrustLocalNtfs, fs, network);
            assert!(!confirmed(d.clone()), "{fs:?} network={network}");
            let DirectoryDurability::Unconfirmed(why) = d else {
                unreachable!()
            };
            assert!(why.contains("local NTFS"), "{why}");
        }
        // A flush that failed is never confirmed.
        assert!(!confirmed(windows_flush_verdict(
            &failed,
            TrustLocalNtfs,
            Some("NTFS"),
            false
        )));
    }

    #[test]
    fn network_locations_are_recognized_by_their_path() {
        for p in [
            r"\\server\share\dir",
            r"\\?\UNC\server\share\dir",
            "//server/share",
        ] {
            assert!(is_network_text(p), "{p}");
        }
        for p in [r"C:\Users\me", r"\\?\C:\Users\me", r"\\.\C:", "/home/me"] {
            assert!(!is_network_text(p), "{p}");
        }
    }

    #[test]
    fn create_new_never_replaces_an_existing_file() {
        let (_d, path) = tmp();
        OsStorage::create_new(&path).unwrap();
        assert!(OsStorage::create_new(&path).is_err());
    }

    #[test]
    fn reads_are_bounds_checked() {
        let (_d, path) = tmp();
        let mut s = OsStorage::create_new(&path).unwrap();
        s.append(b"abc").unwrap();
        let mut buf = [0u8; 4];
        assert!(matches!(
            s.read_exact_at(0, &mut buf),
            Err(StorageError::OutOfBounds { .. })
        ));
        let mut buf = [0u8; 1];
        assert!(matches!(
            s.read_exact_at(u64::MAX, &mut buf),
            Err(StorageError::OutOfBounds { .. })
        ));
        // A short read at the boundary reports how much was available.
        let mut buf = [0u8; 8];
        assert_eq!(s.read_at(1, &mut buf).unwrap(), 2);
        assert_eq!(s.read_at(3, &mut buf).unwrap(), 0);
    }

    #[test]
    fn truncate_shrinks_but_never_extends() {
        let (_d, path) = tmp();
        let mut s = OsStorage::create_new(&path).unwrap();
        s.append(b"0123456789").unwrap();
        s.truncate(4).unwrap();
        assert_eq!(s.size().unwrap(), 4);
        assert!(matches!(
            s.truncate(5),
            Err(StorageError::OutOfBounds { .. })
        ));
    }

    // ---- The writer lock is on the archive itself (spec B.2.17, D27) --------

    /// The names in `dir`, sorted.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// An empty legacy lock file named for `name` in `dir`.
    fn legacy_lock(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(format!("{name}{LEGACY_LOCK_FILE_SUFFIX}"));
        File::create(&p).unwrap();
        p
    }

    /// Locking, committing and unlocking an archive, and creating one through
    /// `OsDir`, leave exactly one file in the directory: no lock file, on any
    /// platform.
    #[test]
    fn no_lock_file_is_ever_created() {
        let (d, path) = tmp();
        let mut s = OsStorage::create_new(&path).unwrap();
        s.try_lock_exclusive().unwrap();
        s.append(b"x").unwrap();
        assert_eq!(names_in(d.path()), ["a.mochi"]);
        s.unlock().unwrap();
        let mut again = OsStorage::open_existing(&path).unwrap();
        again.try_lock_exclusive().unwrap();
        drop(again);
        assert_eq!(names_in(d.path()), ["a.mochi"]);

        let mut dir = OsDir::open(d.path()).unwrap();
        let mut f = dir.create_exclusive("b.tmp").unwrap();
        f.append(b"y").unwrap();
        dir.publish_archive(&mut f, "b.tmp", "b.mochi").unwrap();
        f.unlock().unwrap();
        assert_eq!(names_in(d.path()), ["a.mochi", "b.mochi"]);
    }

    /// A reader (no lock, a separate handle) reads, and a writer appends and
    /// is read back, while the lock is held. On Windows this is the reason
    /// the lock is one byte beyond the data: a whole-file lock failed these
    /// reads with error 33 (Q54).
    #[test]
    fn readers_read_while_the_lock_is_held() {
        let (_d, path) = tmp();
        let mut w = OsStorage::create_new(&path).unwrap();
        w.append(b"hello").unwrap();
        w.try_lock_exclusive().unwrap();
        let r = OsReadStorage::open(&path).unwrap();
        let mut buf = [0u8; 5];
        r.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        w.append(b" world").unwrap();
        w.sync_data().unwrap();
        assert_eq!(r.size().unwrap(), 11);
        let mut buf = [0u8; 11];
        r.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"hello world");
        // `std::fs::read` opens another handle too.
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        w.truncate(5).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    }

    /// Every alias of one file is excluded, hard links included (on Windows
    /// too: the byte-range lock belongs to the file, not to the name).
    #[test]
    fn a_hard_link_shares_the_lock() {
        let (d, path) = tmp();
        OsStorage::create_new(&path).unwrap();
        let link = d.path().join("link.mochi");
        std::fs::hard_link(&path, &link).unwrap();
        let mut a = OsStorage::open_existing(&path).unwrap();
        let mut b = OsStorage::open_existing(&link).unwrap();
        a.try_lock_exclusive().unwrap();
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        a.unlock().unwrap();
        b.try_lock_exclusive().unwrap();
        assert!(matches!(
            a.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
    }

    /// Dropping a handle releases its lock at once, without `unlock`.
    #[test]
    fn dropping_a_handle_releases_the_lock() {
        let (_d, path) = tmp();
        let mut a = OsStorage::create_new(&path).unwrap();
        a.try_lock_exclusive().unwrap();
        drop(a);
        OsStorage::open_existing(&path)
            .unwrap()
            .try_lock_exclusive()
            .unwrap();
    }

    /// After locking, the path must still name the locked file (Unix; spec
    /// B.2.17 item 4): an archive replaced by hand between opening and
    /// locking is followed, and the lock ends up on the file now at the
    /// path, which a second writer is then refused.
    #[cfg(unix)]
    #[test]
    fn a_replaced_archive_is_followed() {
        let (d, path) = tmp();
        std::fs::write(&path, b"old").unwrap();
        let mut a = OsStorage::open_existing(&path).unwrap();
        // Replaced by hand after `a` opened it.
        let replacement = d.path().join("new");
        std::fs::write(&replacement, b"newer contents").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        a.try_lock_exclusive().unwrap();
        assert_eq!(a.size().unwrap(), 14, "a follows the file at the path");
        let mut b = OsStorage::open_existing(&path).unwrap();
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
    }

    /// **Windows (spec B.2.17 item 4).** The archive is replaced by a rename
    /// between opening and locking: the writer ends up locking the new file
    /// (the identity check found the held handle is not what the path names),
    /// and a second writer on the path is refused.
    #[cfg(windows)]
    #[test]
    fn windows_a_replaced_archive_is_followed() {
        let (d, path) = tmp();
        std::fs::write(&path, b"old").unwrap();
        let mut a = OsStorage::open_existing(&path).unwrap();
        let replacement = d.path().join("new");
        std::fs::write(&replacement, b"newer contents").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        a.try_lock_exclusive().unwrap();
        assert_eq!(a.size().unwrap(), 14, "a follows the file at the path");
        let mut b = OsStorage::open_existing(&path).unwrap();
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        assert!(names_same_file(&a.file, &path).unwrap(), "and now names it");
    }

    /// **Windows.** Two hard-link names are the same file for the identity
    /// check; two files are not.
    #[cfg(windows)]
    #[test]
    fn windows_identity_follows_the_file_not_the_name() {
        let (d, path) = tmp();
        std::fs::write(&path, b"x").unwrap();
        let link = d.path().join("link.mochi");
        std::fs::hard_link(&path, &link).unwrap();
        let other = d.path().join("other.mochi");
        std::fs::write(&other, b"x").unwrap();
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(names_same_file(&held, &link).unwrap());
        assert!(!names_same_file(&held, &other).unwrap());
        assert!(!names_same_file(&held, &d.path().join("missing")).unwrap());
    }

    /// The legacy sweep's open never follows a link and never blocks on a
    /// FIFO that was swapped in after the directory was read (spec B.2.17
    /// item 5). The call is made directly, because the sweep's own
    /// `file_type` filter would hide the race.
    #[cfg(unix)]
    #[test]
    fn the_sweep_open_never_blocks_on_a_fifo_or_follows_a_link() {
        let td = tempfile::tempdir().unwrap();
        let fifo = td.path().join("x.mochi-lock");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.is_ok_and(|s| s.success()), "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(remove_legacy_lock_file(&p));
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(20)),
            Ok(false),
            "the open of a FIFO blocked or removed it"
        );
        assert!(fifo.symlink_metadata().is_ok());

        let target = td.path().join("target");
        File::create(&target).unwrap();
        let link = td.path().join("y.mochi-lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!remove_legacy_lock_file(&link));
        assert!(link.symlink_metadata().is_ok() && target.exists());
    }

    /// An archive deleted after it was opened cannot be locked: an error,
    /// not a lock on a file nobody can reach.
    #[cfg(unix)]
    #[test]
    fn a_deleted_archive_is_not_locked() {
        let (_d, path) = tmp();
        let mut a = OsStorage::create_new(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(a.try_lock_exclusive(), Err(StorageError::Io(_))));
    }

    /// The sweep removes empty legacy lock files, orphan or not, and keeps
    /// everything else (spec B.2.17 item 5).
    #[test]
    fn the_sweep_removes_empty_legacy_lock_files_and_nothing_else() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let orphan = legacy_lock(dir, "gone.mochi");
        std::fs::write(dir.join("here.mochi"), b"archive").unwrap();
        let present = legacy_lock(dir, "here.mochi");

        // Held by someone else the way old builds held it.
        let held = legacy_lock(dir, "held.mochi");
        let holder = File::open(&held).unwrap();
        holder.try_lock().unwrap();
        // Bytes, a directory, other names, a sub-directory.
        let bytes = dir.join("bytes.mochi.mochi-lock");
        std::fs::write(&bytes, b"user data").unwrap();
        let as_dir = dir.join("dir.mochi.mochi-lock");
        std::fs::create_dir(&as_dir).unwrap();
        let bare = dir.join(LEGACY_LOCK_FILE_SUFFIX);
        File::create(&bare).unwrap();
        let near = dir.join("notes.mochi-lock.txt");
        File::create(&near).unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        let nested = legacy_lock(&dir.join("sub"), "deep.mochi");

        assert_eq!(sweep_legacy_lock_files(dir), 2);
        assert!(!orphan.exists() && !present.exists());
        for kept in [&held, &bytes, &as_dir, &bare, &near, &nested] {
            assert!(kept.symlink_metadata().is_ok(), "{}", kept.display());
        }
        assert!(dir.join("here.mochi").exists());

        // Released, the held one goes too.
        drop(holder);
        assert_eq!(sweep_legacy_lock_files(dir), 1);
        assert!(!held.exists());
    }

    /// A link named like a lock file is kept, and so is its target.
    #[cfg(unix)]
    #[test]
    fn the_sweep_never_follows_a_link() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let target = dir.join("target.txt");
        File::create(&target).unwrap();
        let link = dir.join("link.mochi.mochi-lock");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dangling = dir.join("dangling.mochi.mochi-lock");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
        assert_eq!(sweep_legacy_lock_files(dir), 0);
        assert!(link.symlink_metadata().is_ok() && target.exists());
        assert!(dangling.symlink_metadata().is_ok());
    }

    /// The sweep reads at most `SWEEP_MAX_ENTRIES` entries and a missing
    /// directory is nothing.
    #[test]
    fn the_sweep_is_bounded_and_forgiving() {
        assert_eq!(sweep_legacy_lock_files(Path::new("/nonexistent/mochi")), 0);
        let td = tempfile::tempdir().unwrap();
        for i in 0..SWEEP_MAX_ENTRIES + 20 {
            File::create(td.path().join(format!("{i}.mochi-lock"))).unwrap();
        }
        assert_eq!(sweep_legacy_lock_files(td.path()), SWEEP_MAX_ENTRIES);
        assert_eq!(names_in(td.path()).len(), 20);
    }

    /// Both triggers: taking an existing archive's lock, and publishing a
    /// created archive.
    #[test]
    fn locking_and_publishing_an_archive_sweep_its_directory() {
        let (d, path) = tmp();
        let gone = legacy_lock(d.path(), "gone.mochi");
        let mut s = OsStorage::create_new(&path).unwrap();
        // `create_new` alone swept nothing.
        assert!(gone.exists());
        s.try_lock_exclusive().unwrap();
        assert!(!gone.exists(), "taking an archive's lock sweeps");

        let own = legacy_lock(d.path(), "a.mochi");
        let mut dir = OsDir::open(d.path()).unwrap();
        let mut f = dir.create_exclusive("c.tmp").unwrap();
        assert!(own.exists(), "creating a temporary file sweeps nothing");
        dir.publish_archive(&mut f, "c.tmp", "c.mochi").unwrap();
        assert!(!own.exists(), "publishing a created archive sweeps");
    }

    /// A temporary file or sidecar locked on itself sweeps nothing, and a
    /// read-only open never does.
    #[test]
    fn temporary_files_and_readers_sweep_nothing() {
        let (d, path) = tmp();
        OsStorage::create_new(&path).unwrap();
        let lock = legacy_lock(d.path(), "x.mochi");
        let mut dir = OsDir::open(d.path()).unwrap();
        dir.create_exclusive("t.tmp").unwrap();
        let _r = OsReadStorage::open(&path).unwrap();
        assert!(lock.exists());
    }

    /// A whole-file lock on a legacy lock file, taken by an old build that
    /// is still running, keeps the file and does not fail the writer.
    #[test]
    fn a_legacy_lock_file_held_by_an_old_build_is_kept() {
        let (d, path) = tmp();
        let old = legacy_lock(d.path(), "a.mochi");
        let holder = File::open(&old).unwrap();
        holder.try_lock().unwrap();
        let mut s = OsStorage::create_new(&path).unwrap();
        s.try_lock_exclusive().unwrap();
        assert!(old.exists());
    }

    /// **Windows (spec B.2.17 item 2).** The lock is one byte at offset 2^62:
    /// another handle cannot take that byte, and cannot take the whole file
    /// either (the range overlaps), yet it reads the real bytes. A foreign
    /// holder of the same byte refuses the writer.
    #[cfg(windows)]
    #[test]
    fn windows_lock_is_one_byte_far_beyond_the_data() {
        assert_eq!(WINDOWS_LOCK_OFFSET, 1 << 62);
        let (_d, path) = tmp();
        let mut w = OsStorage::create_new(&path).unwrap();
        w.append(b"data").unwrap();
        w.try_lock_exclusive().unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(!mochi_sys::try_lock_byte(&other, WINDOWS_LOCK_OFFSET).unwrap());
        assert!(matches!(
            other.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        let mut buf = [0u8; 4];
        OsReadStorage::open(&path)
            .unwrap()
            .read_exact_at(0, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"data");
        w.unlock().unwrap();

        assert!(mochi_sys::try_lock_byte(&other, WINDOWS_LOCK_OFFSET).unwrap());
        let mut w2 = OsStorage::open_existing(&path).unwrap();
        assert!(matches!(
            w2.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        mochi_sys::unlock_byte(&other, WINDOWS_LOCK_OFFSET).unwrap();
        w2.try_lock_exclusive().unwrap();
    }

    /// **Windows.** A hidden legacy lock file (what 0.9.2 created) is
    /// removed too.
    #[cfg(windows)]
    #[test]
    fn windows_sweep_removes_a_hidden_legacy_lock_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let td = tempfile::tempdir().unwrap();
        let hidden = td.path().join("a.mochi.mochi-lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(0x2)
            .open(&hidden)
            .unwrap();
        let visible = legacy_lock(td.path(), "b.mochi");
        assert_eq!(sweep_legacy_lock_files(td.path()), 2);
        assert!(!hidden.exists() && !visible.exists());
    }

    #[test]
    fn second_writer_is_refused_the_lock() {
        let (_d, path) = tmp();
        let mut a = OsStorage::create_new(&path).unwrap();
        let mut b = OsStorage::open_existing(&path).unwrap();
        a.try_lock_exclusive().unwrap();
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        a.unlock().unwrap();
        b.try_lock_exclusive().unwrap();
    }

    /// The holder may take the lock again (`Storage` contract): a no-op, on
    /// Windows too, where `LockFileEx` itself refuses an overlapping lock.
    #[test]
    fn the_holder_may_lock_again() {
        let (_d, path) = tmp();
        let mut a = OsStorage::create_new(&path).unwrap();
        a.try_lock_exclusive().unwrap();
        a.try_lock_exclusive().unwrap();
        let mut b = OsStorage::open_existing(&path).unwrap();
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        a.unlock().unwrap();
        a.unlock().unwrap();
        b.try_lock_exclusive().unwrap();
    }

    #[test]
    fn read_only_handle_reads_and_leaves_bytes_unchanged() {
        let (_d, path) = tmp();
        {
            let mut w = OsStorage::create_new(&path).unwrap();
            w.append(b"payload").unwrap();
            w.sync_data().unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let r = OsReadStorage::open(&path).unwrap();
        let mut buf = [0u8; 7];
        r.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"payload");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn t20_rename_errnos_are_classified() {
        assert_eq!(classify_rename_errno(17), RenameOutcome::Exists);
        assert_eq!(classify_rename_errno(22), RenameOutcome::Unsupported);
        assert_eq!(classify_rename_errno(38), RenameOutcome::Unsupported);
        for other in [1, 2, 5, 13, 18, 28, 30, 95] {
            assert_eq!(
                classify_rename_errno(other),
                RenameOutcome::Failed,
                "{other}"
            );
        }
    }

    /// A folder is renamed, and never onto a name that exists: not a
    /// file, not a folder, and not an empty folder (which a plain POSIX
    /// rename would replace).
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn rename_no_replace_moves_a_folder_and_never_replaces_a_name() {
        let (td, _) = tmp();
        let dir = td.path();
        let src = dir.join("docs");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::create_dir(dir.join("empty")).unwrap();
        std::fs::write(dir.join("file"), b"mine").unwrap();
        for taken in ["empty", "file"] {
            let e = rename_no_replace(&src, &dir.join(taken)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{taken}");
            assert_eq!(std::fs::read(src.join("a.txt")).unwrap(), b"alpha");
        }
        assert!(dir.join("empty").is_dir());
        assert_eq!(std::fs::read(dir.join("file")).unwrap(), b"mine");
        let sub = dir.join("empty").join("docs");
        rename_no_replace(&src, &sub).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(sub.join("a.txt")).unwrap(), b"alpha");
    }

    /// **T20 DoD (fallback).** A rename that the filesystem rejects with
    /// `EINVAL` (or the kernel with `ENOSYS`) falls back to link then
    /// unlink, which still never replaces. The rename is injected because
    /// every filesystem in CI supports the flag.
    #[test]
    fn t20_rejected_flag_falls_back_to_link_then_unlink() {
        for errno in [22, 38] {
            let (td, _) = tmp();
            let dir = td.path().to_path_buf();
            let (src, dst) = (dir.join("a.tmp"), dir.join("a"));
            std::fs::write(&src, b"payload").unwrap();
            let used = publish_with(&src, &dst, "a", |_, _| Err(errno)).unwrap();
            assert_eq!(used, PublishMechanism::LinkThenUnlink);
            assert!(!src.exists());
            assert_eq!(std::fs::read(&dst).unwrap(), b"payload");

            // The fallback never replaces either.
            std::fs::write(&src, b"other").unwrap();
            let e = publish_with(&src, &dst, "a", |_, _| Err(errno)).unwrap_err();
            assert!(
                matches!(e, StorageError::Exists { ref name } if name == "a"),
                "{e}"
            );
            assert_eq!(std::fs::read(&dst).unwrap(), b"payload");
            assert_eq!(std::fs::read(&src).unwrap(), b"other");
        }
    }

    #[test]
    fn t20_other_rename_failures_do_not_fall_back() {
        let (td, _) = tmp();
        let dir = td.path().to_path_buf();
        let (src, dst) = (dir.join("a.tmp"), dir.join("a"));
        std::fs::write(&src, b"payload").unwrap();
        let e = publish_with(&src, &dst, "a", |_, _| Err(13)).unwrap_err();
        assert!(matches!(e, StorageError::Io(_)), "{e}");
        assert!(src.exists() && !dst.exists());
    }

    /// On Linux filesystems that support the flag (ext4 and tmpfs in CI),
    /// publication is the one atomic rename, and `EEXIST` comes from it.
    #[cfg(target_os = "linux")]
    #[test]
    fn t20_linux_uses_rename_noreplace() {
        let (td, _) = tmp();
        let dir = td.path().to_path_buf();
        let mut d = OsDir::open(&dir).unwrap();
        let mut f = d.create_exclusive("a.tmp").unwrap();
        f.append(b"payload").unwrap();
        d.publish_no_replace("a.tmp", "a").unwrap();
        assert_eq!(d.last_publish(), Some(PublishMechanism::RenameNoReplace));
        let mut g = d.create_exclusive("b.tmp").unwrap();
        g.append(b"other").unwrap();
        let e = d.publish_no_replace("b.tmp", "a").unwrap_err();
        assert!(matches!(e, StorageError::Exists { .. }), "{e}");
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"payload");
        assert_eq!(std::fs::read(dir.join("b.tmp")).unwrap(), b"other");
    }

    #[test]
    fn ntfs_write_through_needs_both_the_mechanism_and_the_filesystem() {
        use PublishMechanism::*;
        assert!(is_ntfs_write_through(MoveWriteThrough, Some("NTFS")));
        assert!(is_ntfs_write_through(MoveWriteThrough, Some("ntfs")));
        assert!(!is_ntfs_write_through(MoveWriteThrough, Some("FAT32")));
        assert!(!is_ntfs_write_through(MoveWriteThrough, Some("ReFS")));
        assert!(!is_ntfs_write_through(MoveWriteThrough, Some("")));
        assert!(!is_ntfs_write_through(MoveWriteThrough, None));
        assert!(!is_ntfs_write_through(LinkThenUnlink, Some("NTFS")));
        assert!(!is_ntfs_write_through(RenameNoReplace, Some("NTFS")));
        let (_d, path) = tmp();
        let dir = OsDir::open(path.parent().unwrap()).unwrap();
        assert!(!dir.last_publish_was_ntfs_write_through());
    }

    /// Several creators race to publish distinct temporary files at one name:
    /// exactly one wins, every loser is told `Exists` and keeps its own
    /// temporary file, and the winner's bytes are what is at the name.
    #[test]
    fn concurrent_creators_have_exactly_one_winner() {
        const N: usize = 8;
        let td = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(N);
        let results: Vec<Result<(), StorageError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..N)
                .map(|i| {
                    let (root, barrier) = (td.path(), &barrier);
                    scope.spawn(move || {
                        let mut dir = OsDir::open(root).unwrap();
                        let temp = format!("c{i}.tmp");
                        let mut f = dir.create_exclusive(&temp).unwrap();
                        f.append(format!("creator {i}").as_bytes()).unwrap();
                        f.sync_data().unwrap();
                        barrier.wait();
                        let r = dir.publish_no_replace(&temp, "archive");
                        f.unlock().unwrap();
                        r
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners: Vec<usize> = (0..N).filter(|&i| results[i].is_ok()).collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        for (i, r) in results.iter().enumerate() {
            let temp = td.path().join(format!("c{i}.tmp"));
            if let Err(e) = r {
                assert!(
                    matches!(e, StorageError::Exists { name } if name == "archive"),
                    "{e}"
                );
                assert_eq!(
                    std::fs::read(&temp).unwrap(),
                    format!("creator {i}").as_bytes()
                );
            } else {
                assert!(!temp.exists());
            }
        }
        assert_eq!(
            std::fs::read(td.path().join("archive")).unwrap(),
            format!("creator {}", winners[0]).as_bytes()
        );
    }

    /// **Windows (G6a).** A locked temporary file is published by
    /// `MoveFileExW(…, MOVEFILE_WRITE_THROUGH)` while MOCHI holds the file's
    /// own byte-range lock and another open handle: neither blocks the move,
    /// and the lock is on the open handle, so it carries across the rename
    /// (the new archive is never visible unlocked). Nothing but the lock
    /// holder can then take the archive's lock, readers read it meanwhile, and
    /// the NTFS write-through record is set.
    #[cfg(windows)]
    #[test]
    fn windows_publishes_a_locked_temporary_file_by_move_write_through() {
        let td = tempfile::tempdir().unwrap();
        let mut dir = OsDir::open(td.path()).unwrap();
        let mut f = dir.create_exclusive("a.tmp").unwrap();
        f.append(b"first commit").unwrap();
        f.sync_data().unwrap();
        let other_handle = OsReadStorage::open(td.path().join("a.tmp")).unwrap();
        let mut before = OsStorage::open_existing(td.path().join("a.tmp")).unwrap();
        assert!(matches!(
            before.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));

        dir.publish_archive(&mut f, "a.tmp", "a.mochi").unwrap();

        assert_eq!(dir.last_publish(), Some(PublishMechanism::MoveWriteThrough));
        assert!(dir.last_publish_was_ntfs_write_through());
        assert!(!td.path().join("a.tmp").exists());
        assert_eq!(names_in(td.path()), ["a.mochi"], "no lock file");
        let mut second = OsStorage::open_existing(td.path().join("a.mochi")).unwrap();
        assert!(matches!(
            second.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        assert_eq!(other_handle.size().unwrap(), 12);
        let mut buf = [0u8; 12];
        f.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"first commit");
        f.append(b"!").unwrap();
        f.unlock().unwrap();
        assert_eq!(
            std::fs::read(td.path().join("a.mochi")).unwrap(),
            b"first commit!"
        );
    }

    /// **Windows (G6a).** The plain no-replace publish of a locked temporary
    /// file reports the mechanism too.
    #[cfg(windows)]
    #[test]
    fn windows_publish_no_replace_reports_move_write_through() {
        let td = tempfile::tempdir().unwrap();
        let mut dir = OsDir::open(td.path()).unwrap();
        let mut f = dir.create_exclusive("a.tmp").unwrap();
        f.append(b"payload").unwrap();
        dir.publish_no_replace("a.tmp", "a").unwrap();
        assert_eq!(dir.last_publish(), Some(PublishMechanism::MoveWriteThrough));
        assert!(dir.last_publish_was_ntfs_write_through());
        f.unlock().unwrap();
        assert_eq!(std::fs::read(td.path().join("a")).unwrap(), b"payload");
    }

    /// **Windows (G6a).** An existing destination is refused as `Exists`
    /// (for both `publish_no_replace` and `publish_archive`), the source and
    /// the destination stay byte-identical, and the mechanism record is not
    /// advanced.
    #[cfg(windows)]
    #[test]
    fn windows_refuses_an_existing_destination_leaving_both_files_untouched() {
        let td = tempfile::tempdir().unwrap();
        let mut dir = OsDir::open(td.path()).unwrap();
        std::fs::write(td.path().join("a"), b"existing archive").unwrap();
        let mut f = dir.create_exclusive("b.tmp").unwrap();
        f.append(b"new commit").unwrap();
        f.sync_data().unwrap();

        let e = dir.publish_no_replace("b.tmp", "a").unwrap_err();
        assert!(
            matches!(e, StorageError::Exists { ref name } if name == "a"),
            "{e}"
        );
        let e = dir.publish_archive(&mut f, "b.tmp", "a").unwrap_err();
        assert!(
            matches!(e, StorageError::Exists { ref name } if name == "a"),
            "{e}"
        );

        assert_eq!(dir.last_publish(), None);
        assert!(!dir.last_publish_was_ntfs_write_through());
        f.unlock().unwrap();
        assert_eq!(
            std::fs::read(td.path().join("a")).unwrap(),
            b"existing archive"
        );
        assert_eq!(
            std::fs::read(td.path().join("b.tmp")).unwrap(),
            b"new commit"
        );
    }

    #[test]
    fn opening_a_missing_file_is_an_io_error() {
        let (_d, path) = tmp();
        assert!(matches!(
            OsReadStorage::open(&path),
            Err(StorageError::Io(_))
        ));
        assert!(matches!(
            OsStorage::open_existing(&path),
            Err(StorageError::Io(_))
        ));
    }
}
