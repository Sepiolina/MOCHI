//! Browsing one snapshot (plan D1; spec §10.3, §23.3): the namespace of an
//! opened commit as a directory tree that a client reads a page at a time.
//!
//! [`SnapshotIndex::build`] reads the catalog once (names, kinds, sizes,
//! file-content hashes; no file content) and keeps, for every directory, its
//! children in one fixed order: **directories first, then files, each in
//! name byte order**. A client asks for a slice of that order
//! ([`SnapshotIndex::children`]), so a directory with 100,000 entries never
//! crosses an interface whole (plan D1 exit). The order is a presentation
//! choice, not identity: names are compared as stored bytes and never
//! normalized or case-folded (O24).
//!
//! Directories are explicit entries (spec §10.2, the namespace invariant), so
//! every entry's parent is in the index. A catalog that breaks this is
//! `CATALOG_INVALID`, never a guessed directory (§22.1: never invent
//! namespace information).

use std::collections::{HashMap, HashSet};

use mochi_format::digest::FileContentHash;

use crate::catalog::namespace::{EntryKind, FileVersionId};
use crate::catalog::path::ArchivePath;
use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::object::ObjectId;

/// Most entries one [`SnapshotIndex::children`] call returns.
pub const MAX_PAGE: usize = 1000;

/// A snapshot's totals ([`SnapshotIndex::totals`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotTotals {
    pub folders: u64,
    pub files: u64,
    /// Sum of the files' logical lengths.
    pub logical_bytes: u64,
    /// Sum of the stored lengths of the distinct chunks the files name.
    pub stored_bytes: u64,
    /// How many distinct chunks that is.
    pub chunks: u64,
}

/// One entry of a browsed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseEntry {
    pub path: ArchivePath,
    pub kind: EntryKind,
    /// The file version this path names in the snapshot.
    pub version: FileVersionId,
    /// Logical length in bytes; 0 for a directory.
    pub logical_len: u64,
    /// Plain BLAKE3 of the logical stream (O20). `None` exactly for
    /// directories.
    pub content_hash: Option<FileContentHash>,
    /// Direct subdirectories (0 for a file).
    pub child_dirs: u64,
    /// Direct files (0 for a file).
    pub child_files: u64,
}

impl BrowseEntry {
    /// The last component of the path, as stored.
    pub fn name(&self) -> &[u8] {
        self.path.components().last().unwrap_or_default()
    }
}

/// Which children [`SnapshotIndex::children`] pages over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Children {
    /// Every child: directories first, then files.
    All,
    /// Subdirectories only (a tree view).
    Directories,
}

/// One page of a directory's children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<'a> {
    /// How many children the request ranges over (all of them, or the
    /// subdirectories), whatever the page size.
    pub total: u64,
    /// Entries `offset .. offset + limit` of that range, possibly fewer.
    pub entries: Vec<&'a BrowseEntry>,
}

/// The namespace of one opened commit, indexed for paged browsing.
#[derive(Debug, Clone, Default)]
pub struct SnapshotIndex {
    /// Every entry, in path byte order (binary-searchable by path).
    entries: Vec<BrowseEntry>,
    /// Children of the root, as indices into `entries`, in browse order.
    root: Vec<u32>,
    /// Children of each directory entry, by its index.
    children: HashMap<u32, Vec<u32>>,
    root_dirs: u64,
}

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn too_large() -> MochiError {
    MochiError::new(
        ErrorCode::LimitExceeded,
        "a snapshot total does not fit in 64 bits",
    )
}

fn catalog_invalid(what: &str) -> MochiError {
    MochiError::new(ErrorCode::CatalogInvalid, what.to_string())
}

impl SnapshotIndex {
    /// Index the namespace at the catalog's latest commit: the snapshot of
    /// an [`crate::publish::OpenedHead`]'s catalog. Reads the catalog only.
    pub fn build(cat: &Catalog) -> Result<Self> {
        let snapshot = cat.replay(None)?;
        let versions = cat.file_versions()?;
        let mut entries = Vec::new();
        for (path, entry) in snapshot.iter() {
            let version = versions
                .get(&entry.version)
                .ok_or_else(|| catalog_invalid("the snapshot names a missing file version"))?;
            entries.push(BrowseEntry {
                path: path.clone(),
                kind: entry.kind,
                version: entry.version,
                logical_len: version.logical_len,
                content_hash: version.content_hash,
                child_dirs: 0,
                child_files: 0,
            });
        }
        let n = u32::try_from(entries.len())
            .map_err(|_| MochiError::new(ErrorCode::LimitExceeded, "too many entries to browse"))?;
        let mut root = Vec::new();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for i in 0..n {
            let at = i as usize;
            let list = match entries[at].path.parent() {
                None => &mut root,
                Some(parent) => {
                    let p = entries
                        .binary_search_by(|e| e.path.cmp(&parent))
                        .map_err(|_| catalog_invalid("an entry's parent directory is missing"))?;
                    if entries[p].kind != EntryKind::Directory {
                        return Err(catalog_invalid("an entry's parent is a file"));
                    }
                    match entries[at].kind {
                        EntryKind::Directory => entries[p].child_dirs += 1,
                        EntryKind::File => entries[p].child_files += 1,
                    }
                    // `p < at`: a parent sorts before its descendants.
                    children.entry(p as u32).or_default()
                }
            };
            list.push(i);
        }
        // Siblings arrive in name byte order (they share their parent's
        // prefix); a stable sort by kind puts directories first and keeps it.
        let dirs_first = |list: &mut Vec<u32>| {
            list.sort_by_key(|&i| entries[i as usize].kind != EntryKind::Directory);
        };
        dirs_first(&mut root);
        children.values_mut().for_each(dirs_first);
        let root_dirs = root
            .iter()
            .filter(|&&i| entries[i as usize].kind == EntryKind::Directory)
            .count() as u64;
        Ok(SnapshotIndex {
            entries,
            root,
            children,
            root_dirs,
        })
    }

    /// Entries in the snapshot, directories included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry at `path`, if the snapshot has one.
    pub fn entry(&self, path: &ArchivePath) -> Option<&BrowseEntry> {
        self.entries
            .binary_search_by(|e| e.path.cmp(path))
            .ok()
            .map(|i| &self.entries[i])
    }

    /// The snapshot's totals (the archive Info window): its folders and files,
    /// their logical size, and what their chunks occupy in the archive.
    /// `cat` must be the catalog the index was built from.
    ///
    /// The stored size counts each chunk the snapshot's files name **once**,
    /// however many files or extents share it (deduplication), with its exact
    /// stored length (framing and, if encrypted, the AEAD tag included). Chunks
    /// no file of this snapshot names (other versions' data, stream-only
    /// chunks of the TAR-compatible profile) and the archive's metadata are not
    /// counted: it is this snapshot's file data, not the archive's file size.
    ///
    /// Errors: `LIMIT_EXCEEDED` if a total does not fit in 64 bits.
    pub fn totals(&self, cat: &Catalog) -> Result<SnapshotTotals> {
        let mut t = SnapshotTotals::default();
        let mut versions = HashSet::new();
        for e in &self.entries {
            match e.kind {
                EntryKind::Directory => t.folders += 1,
                EntryKind::File => {
                    t.files += 1;
                    t.logical_bytes = t
                        .logical_bytes
                        .checked_add(e.logical_len)
                        .ok_or_else(too_large)?;
                    versions.insert(e.version);
                }
            }
        }
        let mut seen: HashSet<ObjectId> = HashSet::new();
        for (version, chunk, stored_len) in cat.chunk_refs()? {
            if versions.contains(&version) && seen.insert(chunk) {
                t.chunks += 1;
                t.stored_bytes = t
                    .stored_bytes
                    .checked_add(stored_len)
                    .ok_or_else(too_large)?;
            }
        }
        Ok(t)
    }

    /// Direct (subdirectories, files) of `dir`, or of the root for `None`.
    pub fn counts(&self, dir: Option<&ArchivePath>) -> Result<(u64, u64)> {
        Ok(match dir {
            None => (self.root_dirs, self.root.len() as u64 - self.root_dirs),
            Some(d) => {
                let e = self.directory(d)?.1;
                (e.child_dirs, e.child_files)
            }
        })
    }

    fn directory(&self, dir: &ArchivePath) -> Result<(u32, &BrowseEntry)> {
        let i = self
            .entries
            .binary_search_by(|e| e.path.cmp(dir))
            .map_err(|_| invalid("no such directory in this snapshot"))?;
        let e = &self.entries[i];
        if e.kind != EntryKind::Directory {
            return Err(invalid("not a directory"));
        }
        Ok((i as u32, e))
    }

    /// Children `offset .. offset + limit` of `dir` (the root for `None`), in
    /// browse order. `limit` is 1 to [`MAX_PAGE`]. An offset at or past the
    /// end returns no entries, with the total.
    ///
    /// Errors: `INVALID_ARGUMENT` if `dir` is absent or a file, or `limit` is
    /// out of range.
    pub fn children(
        &self,
        dir: Option<&ArchivePath>,
        which: Children,
        offset: u64,
        limit: usize,
    ) -> Result<Page<'_>> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(invalid(format!("a page holds 1 to {MAX_PAGE} entries")));
        }
        let (list, dirs) = match dir {
            None => (self.root.as_slice(), self.root_dirs),
            Some(d) => {
                let (i, e) = self.directory(d)?;
                let list = self.children.get(&i).map(Vec::as_slice).unwrap_or(&[]);
                (list, e.child_dirs)
            }
        };
        let total = match which {
            Children::All => list.len() as u64,
            Children::Directories => dirs,
        };
        let start = usize::try_from(offset.min(total)).unwrap_or(usize::MAX);
        let end = usize::try_from(total)
            .unwrap_or(usize::MAX)
            .min(start.saturating_add(limit));
        let entries = list
            .get(start..end)
            .unwrap_or(&[])
            .iter()
            .map(|&i| &self.entries[i as usize])
            .collect();
        Ok(Page { total, entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::namespace::NamespaceOp;
    use crate::catalog::{Catalog, Commit, FileVersion};

    fn p(s: &[u8]) -> ArchivePath {
        ArchivePath::from_stored(s).unwrap()
    }

    fn vid(n: u32) -> FileVersionId {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        FileVersionId::from_bytes(b)
    }

    /// A one-commit catalog holding `names` (a trailing `/` marks a directory).
    fn catalog(names: &[&[u8]]) -> Catalog {
        let mut cat = Catalog::new_working().unwrap();
        let mut ops = Vec::new();
        for (n, name) in names.iter().enumerate() {
            let dir = name.ends_with(b"/");
            let stored = if dir { &name[..name.len() - 1] } else { name };
            let id = vid(n as u32);
            let kind = if dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            cat.insert_file_version(
                &FileVersion {
                    id,
                    kind,
                    logical_len: 0,
                    content_hash: (!dir).then(|| FileContentHash::from_bytes([n as u8; 32])),
                },
                &[],
            )
            .unwrap();
            ops.push(NamespaceOp::Put {
                path: p(stored),
                version: id,
            });
        }
        cat.append_commit(&Commit {
            seq: 0,
            parent: None,
            ops,
        })
        .unwrap();
        cat
    }

    fn names(page: &Page<'_>) -> Vec<Vec<u8>> {
        page.entries.iter().map(|e| e.name().to_vec()).collect()
    }

    fn chunk(n: u8, stored_len: u64) -> crate::object::ObjectRecord {
        use mochi_format::codec::{Encoding, Protection};
        use mochi_format::digest::{chunk_content_hash, StoredObjectHash};
        use mochi_format::repr::DecodedBytes;
        crate::object::ObjectRecord {
            id: ObjectId::from_bytes([n; 32]),
            encoding: Encoding::ZstdFrame,
            protection: Protection::None,
            stored_len,
            stored_hash: StoredObjectHash::from_bytes([n; 32]),
            decoded_len: 100,
            content_hash: chunk_content_hash(&DecodedBytes::new(vec![n; 100])),
            dependencies: vec![],
        }
    }

    fn file(cat: &mut Catalog, id: u32, extents: &[(u64, Option<u8>)]) {
        use crate::catalog::extent::{Extent, ExtentSource};
        let mut at = 0;
        let mut list = Vec::new();
        for (i, (len, c)) in extents.iter().enumerate() {
            list.push(Extent {
                ordinal: i as u32,
                logical_offset: at,
                length: *len,
                source: match c {
                    Some(n) => ExtentSource::Chunk {
                        chunk: ObjectId::from_bytes([*n; 32]),
                        chunk_offset: 0,
                    },
                    None => ExtentSource::Hole,
                },
            });
            at += len;
        }
        cat.insert_file_version(
            &FileVersion {
                id: vid(id),
                kind: EntryKind::File,
                logical_len: at,
                content_hash: Some(FileContentHash::from_bytes([id as u8; 32])),
            },
            &list,
        )
        .unwrap();
    }

    /// Two files share chunk 1 (counted once); one adds chunk 2 and a hole
    /// (no stored bytes); a version replaced in the second commit names chunk
    /// 3, which the head does not count. Folders and files are counted apart.
    #[test]
    fn totals_count_each_chunk_of_the_snapshot_once() {
        let mut cat = Catalog::new_working().unwrap();
        for (n, len) in [(1, 40), (2, 25), (3, 1000)] {
            cat.insert_object(&chunk(n, len), Some(u64::from(n) * 2000))
                .unwrap();
        }
        cat.insert_file_version(
            &FileVersion {
                id: vid(10),
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            &[],
        )
        .unwrap();
        file(&mut cat, 11, &[(100, Some(1))]);
        file(&mut cat, 12, &[(100, Some(1)), (50, None), (100, Some(2))]);
        file(&mut cat, 13, &[(100, Some(3))]);
        file(&mut cat, 14, &[(100, Some(1))]);
        let put = |path: &[u8], v: u32| NamespaceOp::Put {
            path: p(path),
            version: vid(v),
        };
        cat.append_commit(&Commit {
            seq: 0,
            parent: None,
            ops: vec![
                put(b"d", 10),
                put(b"d/a", 11),
                put(b"d/b", 12),
                put(b"c", 13),
            ],
        })
        .unwrap();
        cat.append_commit(&Commit {
            seq: 1,
            parent: Some(0),
            ops: vec![put(b"c", 14)],
        })
        .unwrap();
        let index = SnapshotIndex::build(&cat).unwrap();
        assert_eq!(
            index.totals(&cat).unwrap(),
            SnapshotTotals {
                folders: 1,
                files: 3,
                logical_bytes: 100 + 250 + 100,
                stored_bytes: 40 + 25,
                chunks: 2,
            }
        );
    }

    #[test]
    fn totals_of_an_empty_snapshot_are_zero() {
        let cat = catalog(&[]);
        let index = SnapshotIndex::build(&cat).unwrap();
        assert_eq!(index.totals(&cat).unwrap(), SnapshotTotals::default());
    }

    #[test]
    fn children_are_directories_first_then_files_in_byte_order() {
        let cat = catalog(&[
            b"z.txt",
            b"b/",
            b"a.txt",
            b"B/",
            b"b/x",
            b"b/c/",
            b"b/c/deep",
            b"b/a",
            b"b!",
        ]);
        let ix = SnapshotIndex::build(&cat).unwrap();
        assert_eq!(ix.len(), 9);
        let root = ix.children(None, Children::All, 0, 100).unwrap();
        assert_eq!(root.total, 5);
        assert_eq!(
            names(&root),
            [
                b"B".to_vec(),
                b"b".to_vec(),
                b"a.txt".to_vec(),
                b"b!".to_vec(),
                b"z.txt".to_vec()
            ]
        );
        assert_eq!(ix.counts(None).unwrap(), (2, 3));
        let b = ix.children(Some(&p(b"b")), Children::All, 0, 100).unwrap();
        assert_eq!(names(&b), [b"c".to_vec(), b"a".to_vec(), b"x".to_vec()]);
        assert_eq!(ix.counts(Some(&p(b"b"))).unwrap(), (1, 2));
        let dirs = ix.children(None, Children::Directories, 0, 100).unwrap();
        assert_eq!(dirs.total, 2);
        assert_eq!(names(&dirs), [b"B".to_vec(), b"b".to_vec()]);
        let e = ix.entry(&p(b"b/c/deep")).unwrap();
        assert_eq!(e.kind, EntryKind::File);
        assert_eq!(e.version, vid(6));
        assert!(ix.entry(&p(b"b/c/nope")).is_none());
    }

    #[test]
    fn pages_cover_a_directory_exactly_once() {
        let mut v: Vec<Vec<u8>> = vec![b"d/".to_vec()];
        for i in 0..2_345u32 {
            v.push(format!("d/f{i:05}").into_bytes());
        }
        for i in 0..17u32 {
            v.push(format!("d/sub{i}/").into_bytes());
        }
        let refs: Vec<&[u8]> = v.iter().map(Vec::as_slice).collect();
        let ix = SnapshotIndex::build(&catalog(&refs)).unwrap();
        let d = p(b"d");
        let mut seen = Vec::new();
        let mut offset = 0;
        loop {
            let page = ix.children(Some(&d), Children::All, offset, 1000).unwrap();
            assert_eq!(page.total, 2_362);
            if page.entries.is_empty() {
                break;
            }
            offset += page.entries.len() as u64;
            seen.extend(names(&page));
        }
        assert_eq!(seen.len(), 2_362);
        assert!(seen[..17].iter().all(|n| n.starts_with(b"sub")));
        let mut sorted = seen[17..].to_vec();
        sorted.sort();
        assert_eq!(sorted, seen[17..], "files in byte order");
        // Past the end: nothing, with the total.
        let past = ix.children(Some(&d), Children::All, 10_000, 5).unwrap();
        assert_eq!((past.total, past.entries.len()), (2_362, 0));
        let dirs = ix
            .children(Some(&d), Children::Directories, 15, 1000)
            .unwrap();
        assert_eq!((dirs.total, dirs.entries.len()), (17, 2));
    }

    #[test]
    fn bad_requests_are_invalid_arguments() {
        let ix = SnapshotIndex::build(&catalog(&[b"d/", b"f"])).unwrap();
        for (dir, limit) in [
            (Some(p(b"f")), 10),
            (Some(p(b"missing")), 10),
            (None, 0),
            (None, MAX_PAGE + 1),
        ] {
            let e = ix
                .children(dir.as_ref(), Children::All, 0, limit)
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{dir:?} {limit}");
        }
        assert_eq!(
            ix.counts(Some(&p(b"f"))).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn names_are_kept_as_stored_bytes() {
        let ix = SnapshotIndex::build(&catalog(&[
            b"<img src=x onerror=alert(1)>",
            b"bad\xff",
            b"Case",
            b"case",
        ]))
        .unwrap();
        let root = ix.children(None, Children::All, 0, 10).unwrap();
        assert_eq!(
            names(&root),
            [
                b"<img src=x onerror=alert(1)>".to_vec(),
                b"Case".to_vec(),
                b"bad\xff".to_vec(),
                b"case".to_vec()
            ],
            "no case folding, no lossy conversion"
        );
    }

    #[test]
    fn an_empty_snapshot_browses_as_an_empty_root() {
        let ix = SnapshotIndex::build(&catalog(&[])).unwrap();
        assert!(ix.is_empty());
        let root = ix.children(None, Children::All, 0, 10).unwrap();
        assert_eq!((root.total, root.entries.len()), (0, 0));
    }
}
