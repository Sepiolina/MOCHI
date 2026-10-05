//! OS-backed storage.
//!
//! **The only file in `mochi-core` permitted to use `std::fs`.** Everything else
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
//!   which is published by hard link (`CreateHardLinkW`, which fails if the
//!   name exists) and then removal of the temporary name; see [`OsDir`].
//!   The owner chose this over `MoveFileExW(…, MOVEFILE_WRITE_THROUGH)`
//!   (T21, checklist Q63), so `mochi-core` keeps `forbid(unsafe_code)` and
//!   needs no Windows API crate. Neither step is documented as
//!   write-through, so the new directory entry rests on the undocumented
//!   directory flush, and `sync_directory` reports
//!   [`DirectoryDurability::Unconfirmed`] on Windows **even when the flush
//!   succeeds**: the first commit of a new archive is reported as degraded
//!   ("directory durability unconfirmed"), as D13 requires until G6.
//!   Appends to an existing archive are unaffected.
//! * Windows CI (`windows-latest`) runs the storage conformance suite,
//!   including publication of a locked temporary file and refusal of an
//!   existing destination.
//!
//! **Locking.** `try_lock_exclusive` is `File::try_lock` (Linux `flock`,
//! Windows `LockFileEx`). Advisory on Linux: a process that ignores it is not
//! stopped, which is why the writer also re-checks the file size before each
//! commit (spec §12.5: `O_APPEND` alone is insufficient).
//!
//! **Network filesystems** (NFS, SMB) are not supported for writing: their
//! locking and flush semantics vary, and none of the above is assumed there.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use super::{
    check_file_name, DirectoryDurability, ReadStorage, RemoveOutcome, Storage, StorageDir,
    StorageError,
};

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

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<DirectoryDurability, StorageError> {
    File::open(dir)?.sync_all()?;
    Ok(DirectoryDurability::Confirmed)
}

#[cfg(windows)]
fn sync_dir(dir: &Path) -> Result<DirectoryDurability, StorageError> {
    // Best effort only (module docs, plan O12): never Confirmed.
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let flushed = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .and_then(|d| d.sync_all());
    Ok(DirectoryDurability::Unconfirmed(match flushed {
        Ok(()) => "directory flush ran, but Windows does not document it as durable".into(),
        Err(e) => format!("best-effort directory flush failed: {e}"),
    }))
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

/// Read-write handle for a single writer.
#[derive(Debug)]
pub struct OsStorage {
    file: File,
    path: PathBuf,
    /// Whether this handle holds the publication lock. Taking it again is a
    /// no-op: `flock` (Linux) allows that, but `LockFileEx` (Windows) refuses
    /// an overlapping lock even from the same handle, so the state is kept
    /// here rather than asked of the OS (found by T22 on `windows-latest`).
    locked: bool,
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
        Ok(Self {
            file,
            path,
            locked: false,
        })
    }

    /// Open an existing file for reading and writing.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok(Self {
            file,
            path,
            locked: false,
        })
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
        sync_dir(parent_dir(&self.path))
    }

    fn try_lock_exclusive(&mut self) -> Result<(), StorageError> {
        if self.locked {
            return Ok(());
        }
        match self.file.try_lock() {
            Ok(()) => {
                self.locked = true;
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => Err(StorageError::LockHeld),
            Err(std::fs::TryLockError::Error(e)) => Err(StorageError::Io(e)),
        }
    }

    fn unlock(&mut self) -> Result<(), StorageError> {
        if !self.locked {
            return Ok(());
        }
        self.file.unlock()?;
        self.locked = false;
        Ok(())
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
/// * **Portable** (Windows, other Unix, and Linux's fallback): a
///   hard link to the new name, then removal of the old one. `link(2)` /
///   `CreateHardLinkW` fail if the new name exists, so nothing is ever
///   replaced. A crash between the two steps leaves both names on the same
///   bytes; cleanup removes the temporary name once its lock is free.
///
/// A filesystem with neither refuses publication with an I/O error; nothing
/// ever falls back to a replacing rename. Windows keeps the portable
/// mechanism (T21 decided, checklist Q63): no `MoveFileExW`.
#[derive(Debug)]
pub struct OsDir {
    path: PathBuf,
    last_publish: Option<PublishMechanism>,
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
}

impl OsDir {
    /// The mechanism the last successful publication used (diagnostics and
    /// tests).
    pub fn last_publish(&self) -> Option<PublishMechanism> {
        self.last_publish
    }
}

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

#[cfg(not(target_os = "linux"))]
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
        })
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
        OsStorage::open_existing(self.entry(name)?)
    }

    fn create_exclusive(&mut self, name: &str) -> Result<OsStorage, StorageError> {
        let path = self.entry(name)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(exists_as(name))?;
        let mut s = OsStorage {
            file,
            path,
            locked: false,
        };
        s.try_lock_exclusive()?;
        Ok(s)
    }

    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError> {
        let (src, dst) = (self.entry(from)?, self.entry(to)?);
        self.last_publish = Some(publish_no_replace_at(&src, &dst, to)?);
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
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(RemoveOutcome::Locked),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        std::fs::remove_file(&path)?;
        drop(file);
        Ok(RemoveOutcome::Removed)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        sync_dir(&self.path)
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
