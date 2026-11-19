//! The OS folder tree for [`crate::find`] (spec Annex B.2.18, D28): folder
//! listings that never follow a link, file stamps, read-only archive handles,
//! and the scope list both `mochi find` and the desktop app walk.
//!
//! A symbolic link is reported as a link, never followed; on Windows a
//! junction (a name-surrogate reparse point) is reported the same way by the
//! standard library's `FileType::is_symlink`.

use std::fs;
use std::path::{Path, PathBuf};

use super::OsReadStorage;
use crate::find::{FindTree, Stamp, TreeEntry, TreeKind};
use crate::storage::StorageError;

/// The local filesystem as Find's tree.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsFindTree;

/// The stamp (size, modified time) of file metadata.
pub fn stamp_of(meta: &fs::Metadata) -> Stamp {
    Stamp {
        size: meta.len(),
        mtime: meta.modified().ok(),
    }
}

/// The stamp of the file at `path`, following links (a hit's archive is
/// checked against the stamp the scan saw before it is opened again).
pub fn file_stamp(path: &Path) -> std::io::Result<Option<Stamp>> {
    let meta = fs::metadata(path)?;
    Ok(meta.is_file().then(|| stamp_of(&meta)))
}

impl FindTree for OsFindTree {
    type Archive = OsReadStorage;

    fn list(&self, dir: &Path) -> Option<Vec<TreeEntry>> {
        let read = fs::read_dir(dir).ok()?;
        let mut out = Vec::new();
        for entry in read {
            let Ok(entry) = entry else {
                out.push(TreeEntry {
                    path: dir.to_path_buf(),
                    kind: TreeKind::Unreadable,
                });
                continue;
            };
            let path = entry.path();
            let kind = match entry.file_type() {
                Err(_) => TreeKind::Unreadable,
                Ok(ft) if ft.is_symlink() => TreeKind::Link {
                    // A stat of the target only, to say what was not followed.
                    to_folder: fs::metadata(&path).is_ok_and(|m| m.is_dir()),
                },
                Ok(ft) if ft.is_dir() => TreeKind::Folder,
                Ok(ft) if ft.is_file() => TreeKind::File,
                Ok(_) => TreeKind::Other,
            };
            out.push(TreeEntry { path, kind });
        }
        Some(out)
    }

    fn stamp(&self, path: &Path) -> Option<Stamp> {
        fs::symlink_metadata(path).ok().map(|m| stamp_of(&m))
    }

    fn open(&self, path: &Path) -> Result<OsReadStorage, StorageError> {
        OsReadStorage::open(path)
    }
}

/// The scopes as a scan walks them: absolute, existing folders, without a
/// folder that lies inside another one in the list, and without duplicates,
/// in the order given (`existing` first, then `paths`). Paths that are not
/// existing absolute folders are left out.
pub fn normalize_scopes(paths: &[PathBuf], existing: &[PathBuf]) -> Vec<PathBuf> {
    let key = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut all: Vec<(PathBuf, PathBuf)> = existing
        .iter()
        .chain(paths.iter())
        .filter(|p| p.is_absolute() && p.is_dir())
        .map(|p| (p.clone(), key(p)))
        .collect();
    let mut out: Vec<(PathBuf, PathBuf)> = Vec::new();
    // Shortest first, so a folder is kept before the ones inside it.
    all.sort_by_key(|(_, k)| k.components().count());
    for (p, k) in all {
        if !out.iter().any(|(_, ok)| k.starts_with(ok)) {
            out.push((p, k));
        }
    }
    // Back to the order the user gave them.
    let order = |p: &PathBuf| {
        existing
            .iter()
            .chain(paths.iter())
            .position(|x| x == p)
            .unwrap_or(usize::MAX)
    };
    out.sort_by_key(|(p, _)| order(p));
    out.into_iter().map(|(p, _)| p).collect()
}
