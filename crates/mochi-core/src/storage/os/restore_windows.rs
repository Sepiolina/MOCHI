//! [`OsRestoreDir`] on Windows (plan C6): restoration through **pinned
//! directory handles**, with `std` only and no `unsafe` (`mochi-core`
//! forbids it; T21's decision). The Linux counterpart is
//! `restore_linux.rs`. What this guarantees, what it concedes, and how a
//! stronger implementation would replace it: `docs/c6-restore-platforms.md`.
//!
//! # Mechanism
//!
//! * **Pinning.** The root and every directory this restoration creates are
//!   held open for the whole restoration with read access and a share mode
//!   without `FILE_SHARE_DELETE`. While such a handle exists, Windows
//!   refuses every open that asks for `DELETE` access, which renaming or
//!   deleting the directory needs. So no directory in the restored tree can
//!   be moved away and replaced (for example by a junction) while names are
//!   resolved through it. Read access matters: an open that asks only for
//!   attribute access takes no part in share checks and would pin nothing.
//! * **No reparse point is ever followed.** Every open passes
//!   `FILE_FLAG_OPEN_REPARSE_POINT`. A directory that, once opened and
//!   pinned, turns out to be a reparse point (swapped between creation and
//!   pinning) fails the entry; it is checked on the held handle.
//! * **Exclusive creation, no-replace publication.** Files are created with
//!   `CREATE_NEW` (an existing name, including a planted link, is a
//!   collision) and published by hard link then unlink, which never
//!   replaces ([`OsDir`]'s mechanism).
//! * **Attributes** are applied through a handle opened with attribute
//!   access only and `FILE_FLAG_OPEN_REPARSE_POINT`, after checking on that
//!   handle that it is not a reparse point.
//!
//! # Concessions (see the docs file for the reasoning and the remedies)
//!
//! * Operations still pass paths, resolved through pinned directories. The
//!   root's own ancestors are not pinned: someone able to rename a
//!   directory *above* the chosen destination could redirect later writes.
//! * The destination's ACL is not checked (no ACL API in `std`). Pinning
//!   protects directories, not files: a user with write access to a
//!   restored directory could swap a temporary file before it is
//!   published. Restore into a directory only you can write (the default
//!   for folders in your profile).
//! * Directory durability is `Unconfirmed` (plan O12, until G6).
//! * Hidden and system attributes are reported, not set (no `std` API).

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

use super::*;
use crate::storage::{windows_name_issue, CaseBehavior};

const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0000_0100;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// A restore destination directory on Windows ([`RestoreDir`]); see the
/// module documentation for the mechanism and its concessions.
///
/// Names are the archive's WTF-8 bytes decoded to UTF-16 without loss
/// (plan O24); a name Windows cannot hold is reported by
/// [`windows_name_issue`], never altered.
#[derive(Debug)]
pub struct OsRestoreDir {
    path: PathBuf,
    /// Held without `FILE_SHARE_DELETE`: while it is open, this directory
    /// cannot be renamed or deleted.
    _pin: File,
}

fn os_name(name: &[u8]) -> Result<OsString, StorageError> {
    crate::catalog::path::utf16_from_wtf8(name)
        .map(|w| OsString::from_wide(&w))
        .ok_or_else(|| StorageError::InvalidName {
            name: String::from_utf8_lossy(name).into_owned(),
        })
}

/// Open and pin the directory at `path`, refusing a reparse point.
fn pin(path: &Path) -> Result<File, StorageError> {
    let dir = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let attributes = dir.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(StorageError::Io(io::Error::other(format!(
            "{} is not a plain directory (a reparse point or a file); nothing was written \
             through it",
            path.display()
        ))));
    }
    Ok(dir)
}

impl OsRestoreDir {
    /// The existing directory at `path`, as a restore root, pinned for as
    /// long as this value lives. A root that is a reparse point is refused.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let pinned = pin(&path)?;
        Ok(Self { path, _pin: pinned })
    }

    /// Where this directory is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn entry(&self, name: &[u8]) -> Result<PathBuf, StorageError> {
        if let Some(issue) = windows_name_issue(name) {
            return Err(StorageError::InvalidName {
                name: format!("{} ({issue})", String::from_utf8_lossy(name)),
            });
        }
        Ok(self.path.join(os_name(name)?))
    }
}

/// Create `path` exclusively without following a reparse point at it.
fn create_new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

impl RestoreDir for OsRestoreDir {
    type File = OsStorage;

    fn name_issue(&self, name: &[u8]) -> Option<NameIssue> {
        windows_name_issue(name)
    }

    fn create_dir(&mut self, name: &[u8]) -> Result<Self, StorageError> {
        let path = self.entry(name)?;
        let lossy = String::from_utf8_lossy(name);
        std::fs::create_dir(&path).map_err(exists_as(&lossy))?;
        let pinned = pin(&path)?;
        Ok(Self { path, _pin: pinned })
    }

    fn create_file(&mut self, name: &[u8]) -> Result<OsStorage, StorageError> {
        let path = self.entry(name)?;
        let lossy = String::from_utf8_lossy(name);
        let file = create_new_file(&path).map_err(exists_as(&lossy))?;
        Ok(OsStorage::itself(file, path))
    }

    fn publish_no_replace(&mut self, from: &[u8], to: &[u8]) -> Result<(), StorageError> {
        let (src, dst) = (self.entry(from)?, self.entry(to)?);
        link_then_unlink(&src, &dst, &String::from_utf8_lossy(to))?;
        Ok(())
    }

    fn discard(&mut self, file: OsStorage, name: &[u8]) -> Result<(), StorageError> {
        let path = self.entry(name)?;
        drop(file);
        Ok(std::fs::remove_file(path)?)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        sync_dir(&self.path)
    }

    /// Creates a probe file of its own, looks it up under its upper-case
    /// name without following links, and removes it. A probe whose
    /// upper-case name already exists is skipped (it would prove nothing).
    fn case_behavior(&mut self) -> Result<CaseBehavior, StorageError> {
        for n in 0..16u32 {
            let lower = format!(".mochi-case-probe.{}.{n}", std::process::id());
            let upper = self.path.join(lower.to_ascii_uppercase());
            let lower = self.path.join(lower);
            if std::fs::symlink_metadata(&upper).is_ok() {
                continue;
            }
            let probe = match create_new_file(&lower) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            };
            drop(probe);
            let seen = std::fs::symlink_metadata(&upper);
            std::fs::remove_file(&lower)?;
            return match seen {
                Ok(_) => Ok(CaseBehavior::Insensitive),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(CaseBehavior::Sensitive),
                Err(e) => Err(e.into()),
            };
        }
        Err(StorageError::Io(io::Error::other(
            "could not determine whether the destination is case-sensitive",
        )))
    }

    fn entry_exists(&mut self, name: &[u8]) -> Result<bool, StorageError> {
        match std::fs::symlink_metadata(self.entry(name)?) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn apply_attributes(
        &mut self,
        name: &[u8],
        kind: EntryKind,
        attributes: &Attributes,
    ) -> Vec<AttributeIssue> {
        let mut issues = Vec::new();
        let opened = self.entry(name).and_then(|path| {
            let f = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?;
            if f.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(StorageError::Io(io::Error::other(
                    "the entry was replaced by a reparse point; nothing was applied",
                )));
            }
            Ok(f)
        });
        match opened {
            Ok(f) => apply_handle_attributes(&f, kind, attributes, &mut issues),
            Err(e) => issue(&mut issues, AttributeKind::Mtime, e),
        }
        issues
    }
}

/// Windows (plan O6), through the entry's own handle: the time, then the
/// read-only bit (from the Windows bits, or from POSIX write bits when the
/// entry was authored on POSIX). Hidden and system have no `std` API, so
/// they are reported; so are a POSIX owner and the POSIX bits Windows
/// cannot hold.
fn apply_handle_attributes(
    f: &File,
    kind: EntryKind,
    a: &Attributes,
    issues: &mut Vec<AttributeIssue>,
) {
    if let Some(m) = a.mtime {
        let set = system_time(m)
            .ok_or_else(|| "the time is out of this platform's range".to_string())
            .and_then(|t| f.set_modified(t).map_err(|e| e.to_string()));
        if let Err(e) = set {
            issue(issues, AttributeKind::Mtime, e);
        }
    }
    let readonly = match (a.windows, a.posix) {
        (Some(w), _) => w & WINDOWS_READONLY != 0,
        (None, Some(p)) => p.mode & 0o222 == 0,
        (None, None) => false,
    };
    if let Some(w) = a.windows {
        if w & (WINDOWS_HIDDEN | WINDOWS_SYSTEM) != 0 {
            issue(
                issues,
                AttributeKind::HiddenOrSystem,
                "not restored: setting them needs a Windows API this build does not call",
            );
        }
    }
    if a.posix.is_some() {
        issue(
            issues,
            AttributeKind::Ownership,
            "Windows has no numeric owner or group",
        );
        issue(
            issues,
            AttributeKind::Mode,
            "only the write bits map to Windows (the read-only attribute)",
        );
    }
    if readonly && kind == EntryKind::File {
        let set = f.metadata().and_then(|m| {
            let mut p = m.permissions();
            p.set_readonly(true);
            f.set_permissions(p)
        });
        if let Err(e) = set {
            issue(issues, AttributeKind::ReadOnly, e);
        }
    }
}
