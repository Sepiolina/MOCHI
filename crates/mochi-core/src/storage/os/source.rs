//! The OS source tree for [`crate::import`] (plan C14, D2): metadata without
//! following links, directory listings as archive name bytes, file reads,
//! and the promised attributes (O6).
//!
//! * **Unix:** names are the bytes as stored by the filesystem. Attributes:
//!   permission bits (`mode & 0o7777`), numeric uid and gid, mtime with
//!   nanoseconds. No Windows bits.
//! * **Windows:** names are UTF-16, stored as WTF-8 (O24), so unpaired
//!   surrogates survive. Attributes: `READONLY`, `HIDDEN`, `SYSTEM`,
//!   `ARCHIVE` and mtime. No POSIX attributes.
//!
//! Reads are not taken from a snapshot of the source: a file that changes
//! while it is read is stored as read (as `tar` and `zip` do). Its length is
//! what was read, never the earlier `stat`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::import::{SourceKind, SourceMeta, SourceTree};
use crate::manifest::{Attributes, Mtime};

/// The local filesystem as an import source.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsSourceTree;

fn io(path: &Path, e: std::io::Error) -> MochiError {
    MochiError::new(
        ErrorCode::IoError,
        format!("reading {}: {e}", path.display()),
    )
}

/// The archive name bytes of one OS path component.
fn name_bytes(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = name.encode_wide().collect();
        crate::catalog::path::wtf8_from_utf16(&units)
    }
}

// Unix takes the time from `MetadataExt`, which is exact before the epoch.
#[cfg_attr(unix, allow(dead_code))]
fn mtime_of(t: SystemTime) -> Option<Mtime> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(Mtime {
            secs: i64::try_from(d.as_secs()).ok()?,
            nanos: d.subsec_nanos(),
        }),
        Err(before) => {
            // `before` is how far before the epoch: negative seconds with a
            // non-negative nanosecond part.
            let d = before.duration();
            let mut secs = -i64::try_from(d.as_secs()).ok()?;
            let mut nanos = d.subsec_nanos();
            if nanos > 0 {
                secs = secs.checked_sub(1)?;
                nanos = 1_000_000_000 - nanos;
            }
            Some(Mtime { secs, nanos })
        }
    }
}

fn attributes(meta: &fs::Metadata) -> Attributes {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mtime = u32::try_from(meta.mtime_nsec())
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .map(|nanos| Mtime {
                secs: meta.mtime(),
                nanos,
            });
        Attributes {
            posix: Some(crate::manifest::PosixAttributes {
                mode: meta.mode() & 0o7777,
                uid: meta.uid(),
                gid: meta.gid(),
            }),
            windows: None,
            mtime,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Attributes {
            posix: None,
            windows: Some(meta.file_attributes() & crate::manifest::WINDOWS_PROMISED_MASK),
            mtime: meta.modified().ok().and_then(mtime_of),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Attributes {
            posix: None,
            windows: None,
            mtime: meta.modified().ok().and_then(mtime_of),
        }
    }
}

impl OsSourceTree {
    /// The archive path an input becomes: its last component. A path with
    /// none (`.`, `..`, a trailing `..`) is named after its canonical form;
    /// a filesystem root has no name and is refused.
    pub fn input(&self, path: impl AsRef<Path>) -> Result<(ArchivePath, PathBuf)> {
        let path = path.as_ref();
        let name = match path.file_name() {
            Some(n) if !path.ends_with("..") => n.to_os_string(),
            _ => {
                let canonical = fs::canonicalize(path).map_err(|e| io(path, e))?;
                canonical
                    .file_name()
                    .map(|n| n.to_os_string())
                    .ok_or_else(|| {
                        MochiError::new(
                            ErrorCode::InvalidArgument,
                            format!(
                                "{} has no name to store it under; add its contents instead",
                                path.display()
                            ),
                        )
                    })?
            }
        };
        let archive = ArchivePath::from_components([name_bytes(&name)]).map_err(|e| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                format!("{}: {}", path.display(), e.message),
            )
        })?;
        Ok((archive, path.to_path_buf()))
    }
}

impl SourceTree for OsSourceTree {
    type Node = PathBuf;

    fn meta(&self, node: &PathBuf) -> Result<SourceMeta> {
        let meta = fs::symlink_metadata(node).map_err(|e| io(node, e))?;
        let t = meta.file_type();
        let kind = if t.is_symlink() {
            SourceKind::Symlink
        } else if t.is_file() {
            SourceKind::File
        } else if t.is_dir() {
            SourceKind::Directory
        } else {
            SourceKind::Other
        };
        Ok(SourceMeta {
            kind,
            len: meta.len(),
            attributes: attributes(&meta),
        })
    }

    fn children(&self, node: &PathBuf) -> Result<Vec<(Vec<u8>, PathBuf)>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(node).map_err(|e| io(node, e))? {
            let entry = entry.map_err(|e| io(node, e))?;
            out.push((name_bytes(&entry.file_name()), entry.path()));
        }
        Ok(out)
    }

    fn read(&self, node: &PathBuf, max: u64) -> Result<Vec<u8>> {
        let file = fs::File::open(node).map_err(|e| io(node, e))?;
        let mut buf = Vec::new();
        file.take(max.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| io(node, e))?;
        if buf.len() as u64 > max {
            return Err(MochiError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "{} grew past the import limit while it was read",
                    node.display()
                ),
            ));
        }
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_before_the_epoch_keep_nanoseconds_non_negative() {
        let t = UNIX_EPOCH - std::time::Duration::new(1, 250_000_000);
        assert_eq!(
            mtime_of(t),
            Some(Mtime {
                secs: -2,
                nanos: 750_000_000
            })
        );
        // A multiple of 100 ns: Windows `SystemTime` counts 100 ns ticks.
        let t = UNIX_EPOCH + std::time::Duration::new(5, 700);
        assert_eq!(
            mtime_of(t),
            Some(Mtime {
                secs: 5,
                nanos: 700
            })
        );
    }

    #[test]
    fn inputs_are_named_by_their_last_component() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("photos");
        fs::create_dir(&sub).unwrap();
        let (a, _) = OsSourceTree.input(&sub).unwrap();
        assert_eq!(a.as_stored(), b"photos");
        let (a, _) = OsSourceTree.input(sub.join("..").join("photos")).unwrap();
        assert_eq!(a.as_stored(), b"photos");
        let (a, _) = OsSourceTree.input(sub.join("..")).unwrap();
        assert_eq!(
            a.as_stored(),
            name_bytes(fs::canonicalize(dir.path()).unwrap().file_name().unwrap())
        );
    }
}
