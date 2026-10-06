//! [`OsRestoreDir`] on Linux (plan C6): race-resistant restoration through
//! directory descriptors (`rustix`, no `unsafe`). The Windows counterpart is
//! `restore_windows.rs`; both implement [`RestoreDir`] and are chosen by
//! target in `os.rs`. See `docs/c6-restore-platforms.md`.

use std::ffi::OsStr;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;

use super::*;
use crate::storage::CaseBehavior;

fn os(name: &[u8]) -> &OsStr {
    OsStr::from_bytes(name)
}

fn io(e: Errno) -> StorageError {
    StorageError::Io(e.into())
}

fn exists_or_io(name: &[u8], e: Errno) -> StorageError {
    if e == Errno::EXIST {
        StorageError::Exists {
            name: String::from_utf8_lossy(name).into_owned(),
        }
    } else {
        io(e)
    }
}

/// A restore destination directory on the local filesystem (plan C6;
/// [`RestoreDir`]).
///
/// **Race resistance (Linux).** Every operation is relative to a directory
/// descriptor this restoration holds, never to a path, so replacing a
/// directory on the path with a symbolic link redirects nothing:
/// * the root is opened once (`O_DIRECTORY`); later changes to the path
///   that led to it do not matter;
/// * directories are made with `mkdirat` mode `0700` and opened with
///   `openat(O_DIRECTORY | O_NOFOLLOW)`; files are created with
///   `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` mode `0600`, so an existing
///   entry or a planted symbolic link is a collision, never followed or
///   written through;
/// * publication is `renameat2(RENAME_NOREPLACE)` (or `linkat` then
///   `unlinkat`, as [`OsDir`]) inside the held directory; attributes are
///   applied through a descriptor opened with `O_NOFOLLOW` (`futimens`,
///   `fchown`, `fchmod`), the promised modes only at the end.
/// * **Nobody else can rename or remove entries in between.** Everything
///   below the root is private to the restoring user until attributes are
///   applied, and the root must be one that other users cannot modify: owned
///   by the restoring user or by root, and not writable by group or others
///   unless sticky (as `/tmp`). Any other root is refused before anything is
///   written (`UNSUPPORTED_FEATURE`); restore into a private directory
///   inside it instead. The check is made on the held descriptor, whose
///   owner and mode only its owner or root can change.
///
/// Names are the archive's bytes, unaltered (plan O24).
#[derive(Debug)]
pub struct OsRestoreDir {
    path: PathBuf,
    fd: OwnedFd,
}

/// Why restoring into a directory other users can modify is refused.
const UNSAFE_ROOT: &str = "restoring into a directory that other users can modify (not owned by \
     you or root, or writable by group or others without the sticky bit) is not supported: \
     entries could be swapped while they are restored. Nothing was written; restore into a \
     private directory instead";

const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const NEW_FILE: OFlags = OFlags::RDWR
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const OPEN_DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

impl OsRestoreDir {
    /// The existing directory at `path`, as a restore root. A root other
    /// users can modify is refused before anything is written.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_root(path.as_ref())
    }

    /// Where this directory is (for messages; operations never use it).
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_root(path: &Path) -> Result<Self, StorageError> {
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
        let st = rustix::fs::fstat(&fd).map_err(io)?;
        let owner_ok = st.st_uid == euid() || st.st_uid == 0;
        let shared = st.st_mode & 0o022 != 0;
        let sticky = st.st_mode & 0o1000 != 0;
        if !owner_ok || (shared && !sticky) {
            return Err(StorageError::Unsupported(UNSAFE_ROOT));
        }
        Ok(Self {
            path: path.to_path_buf(),
            fd,
        })
    }

    fn open_entry(&self, name: &[u8], kind: EntryKind) -> Result<File, StorageError> {
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if kind == EntryKind::Directory {
            flags |= OFlags::DIRECTORY;
        }
        let fd: OwnedFd =
            rustix::fs::openat(&self.fd, os(name), flags, Mode::empty()).map_err(io)?;
        Ok(File::from(fd))
    }
}

impl RestoreDir for OsRestoreDir {
    type File = OsStorage;

    fn name_issue(&self, _name: &[u8]) -> Option<NameIssue> {
        // Archive components already exclude NUL and `/`; any other
        // byte is a valid Linux name byte.
        None
    }

    fn create_dir(&mut self, name: &[u8]) -> Result<Self, StorageError> {
        rustix::fs::mkdirat(&self.fd, os(name), Mode::RWXU).map_err(|e| exists_or_io(name, e))?;
        let fd = rustix::fs::openat(&self.fd, os(name), OPEN_DIR, Mode::empty()).map_err(io)?;
        // Nobody else can rename entries here (see the type's docs);
        // this only confirms it, on the descriptor now held.
        if rustix::fs::fstat(&fd).map_err(io)?.st_uid != euid() {
            return Err(StorageError::Io(io::Error::other(
                "a directory this restoration created was replaced",
            )));
        }
        Ok(Self {
            path: self.path.join(os(name)),
            fd,
        })
    }

    fn create_file(&mut self, name: &[u8]) -> Result<OsStorage, StorageError> {
        let fd = rustix::fs::openat(&self.fd, os(name), NEW_FILE, PRIVATE_FILE)
            .map_err(|e| exists_or_io(name, e))?;
        Ok(OsStorage::itself(File::from(fd), self.path.join(os(name))))
    }

    fn publish_no_replace(&mut self, from: &[u8], to: &[u8]) -> Result<(), StorageError> {
        let renamed =
            rustix::fs::renameat_with(&self.fd, os(from), &self.fd, os(to), RenameFlags::NOREPLACE);
        let Err(e) = renamed else {
            return Ok(());
        };
        match classify_rename_errno(e.raw_os_error()) {
            RenameOutcome::Exists => Err(exists_or_io(to, Errno::EXIST)),
            RenameOutcome::Failed => Err(io(e)),
            RenameOutcome::Unsupported => {
                rustix::fs::linkat(&self.fd, os(from), &self.fd, os(to), AtFlags::empty())
                    .map_err(|e| exists_or_io(to, e))?;
                rustix::fs::unlinkat(&self.fd, os(from), AtFlags::empty()).map_err(io)
            }
        }
    }

    fn discard(&mut self, file: OsStorage, name: &[u8]) -> Result<(), StorageError> {
        drop(file);
        rustix::fs::unlinkat(&self.fd, os(name), AtFlags::empty()).map_err(io)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        rustix::fs::fsync(&self.fd).map_err(io)?;
        Ok(DirectoryDurability::Confirmed)
    }

    /// Creates a probe file of its own (exclusively, `0600`), looks it
    /// up under its upper-case name without following links, and
    /// removes it. The same file under both names means insensitive.
    fn case_behavior(&mut self) -> Result<CaseBehavior, StorageError> {
        for n in 0..16u32 {
            let lower = format!(".mochi-case-probe.{}.{n}", std::process::id());
            let upper = lower.to_ascii_uppercase();
            let fd = match rustix::fs::openat(&self.fd, lower.as_str(), NEW_FILE, PRIVATE_FILE) {
                Ok(fd) => fd,
                Err(Errno::EXIST) => continue,
                Err(e) => return Err(io(e)),
            };
            let mine = rustix::fs::fstat(&fd).map_err(io);
            let seen = rustix::fs::statat(&self.fd, upper.as_str(), AtFlags::SYMLINK_NOFOLLOW);
            drop(fd);
            rustix::fs::unlinkat(&self.fd, lower.as_str(), AtFlags::empty()).map_err(io)?;
            let mine = mine?;
            match seen {
                Ok(st) if (st.st_dev, st.st_ino) == (mine.st_dev, mine.st_ino) => {
                    return Ok(CaseBehavior::Insensitive)
                }
                // Another entry has the upper-case name: inconclusive.
                Ok(_) => continue,
                Err(Errno::NOENT) => return Ok(CaseBehavior::Sensitive),
                Err(e) => return Err(io(e)),
            }
        }
        Err(StorageError::Io(io::Error::other(
            "could not determine whether the destination is case-sensitive",
        )))
    }

    fn entry_exists(&mut self, name: &[u8]) -> Result<bool, StorageError> {
        match rustix::fs::statat(&self.fd, os(name), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(io(e)),
        }
    }

    fn apply_attributes(
        &mut self,
        name: &[u8],
        kind: EntryKind,
        attributes: &Attributes,
    ) -> Vec<AttributeIssue> {
        let mut issues = Vec::new();
        match self.open_entry(name, kind) {
            Ok(file) => apply_fd_attributes(&file, kind, attributes, &mut issues),
            Err(e) => issue(&mut issues, AttributeKind::Mtime, e),
        }
        issues
    }
}

/// POSIX (plan O6), through the entry's own descriptor: the time first,
/// then the owner (which clears setuid and setgid), then the mode. An
/// entry without POSIX attributes gets spec §10.4.1's defaults, `0644`
/// for files and `0755` for directories (it was created private). The
/// Windows read-only bit clears the write bits; hidden and system have no
/// POSIX equivalent and are reported. The Windows archive bit is a backup
/// marker with no meaning here and is ignored.
fn apply_fd_attributes(
    file: &File,
    kind: EntryKind,
    a: &Attributes,
    issues: &mut Vec<AttributeIssue>,
) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(m) = a.mtime {
        let set = system_time(m)
            .ok_or_else(|| "the time is out of this platform's range".to_string())
            .and_then(|t| file.set_modified(t).map_err(|e| e.to_string()));
        if let Err(e) = set {
            issue(issues, AttributeKind::Mtime, e);
        }
    }
    let readonly = a.windows.is_some_and(|w| w & WINDOWS_READONLY != 0);
    if let Some(w) = a.windows {
        if w & (WINDOWS_HIDDEN | WINDOWS_SYSTEM) != 0 {
            issue(issues, AttributeKind::HiddenOrSystem, "no POSIX equivalent");
        }
    }
    let mut mode = match a.posix {
        Some(p) => {
            // -1 means "leave unchanged" to chown: never pass it on.
            if p.uid == u32::MAX || p.gid == u32::MAX {
                issue(
                    issues,
                    AttributeKind::Ownership,
                    "an owner or group of -1 cannot be set",
                );
            } else if let Err(e) = std::os::unix::fs::fchown(file, Some(p.uid), Some(p.gid)) {
                issue(issues, AttributeKind::Ownership, e);
            }
            p.mode & 0o7777
        }
        None if kind == EntryKind::Directory => 0o755,
        None => 0o644,
    };
    if readonly {
        mode &= !0o222;
    }
    if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(mode)) {
        issue(issues, AttributeKind::Mode, e);
    }
}
