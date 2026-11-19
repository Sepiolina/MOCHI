//! Namespace operations and snapshots (spec §10.2, §10.6).
//!
//! A snapshot is the complete path → file-version mapping at a commit. It is
//! produced by replaying `PUT`/`DELETE` operations in parent-chain order, then
//! by operation sequence within each commit.
//!
//! Rules (spec §10.2, with the choices this implementation makes explicit):
//!
//! * `PUT(path, v)` creates or replaces the entry at `path`.
//! * `DELETE(path)` removes it. Deleting a path that does not exist is an
//!   error, not a no-op: a writer never emits one, so seeing one means the
//!   metadata is wrong.
//! * Directories are explicit entries (kind `directory`). Every entry's parent
//!   must exist *as a directory*; top-level entries have the implicit root.
//! * Directory deletion is non-recursive: a commit that leaves any entry under
//!   a deleted (or replaced-by-a-file) directory is invalid.
//! * Validity is checked against the **completed** commit state, so a rename
//!   (delete + put), or creating a child before its parent within one commit,
//!   is fine.
//! * An invalid commit leaves the snapshot exactly as it was.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound;

use crate::catalog::path::{ArchivePath, SEPARATOR};
use crate::error::{ErrorCode, MochiError};

/// Stable identity of an immutable file version (O19: random, never derived).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileVersionId([u8; 32]);

impl FileVersionId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        FileVersionId(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for FileVersionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileVersionId(")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceOp {
    Put {
        path: ArchivePath,
        version: FileVersionId,
    },
    Delete {
        path: ArchivePath,
    },
}

impl NamespaceOp {
    pub fn path(&self) -> &ArchivePath {
        match self {
            NamespaceOp::Put { path, .. } | NamespaceOp::Delete { path } => path,
        }
    }
}

/// Why a commit's operations are invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceFault {
    /// `PUT` names a file version the catalog does not have.
    UnknownFileVersion { op_seq: usize },
    /// `DELETE` of a path absent at that point in the commit.
    DeleteMissing { op_seq: usize },
    /// An entry's parent is absent in the completed state.
    MissingParent { path: ArchivePath },
    /// An entry's parent is a file in the completed state.
    ParentNotDirectory { path: ArchivePath },
}

impl fmt::Display for NamespaceFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<NamespaceFault> for MochiError {
    fn from(fault: NamespaceFault) -> Self {
        let code = match fault {
            NamespaceFault::UnknownFileVersion { .. } => ErrorCode::CatalogInvalid,
            _ => ErrorCode::NamespaceInvalid,
        };
        MochiError::new(code, format!("invalid namespace operations: {fault}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub version: FileVersionId,
    pub kind: EntryKind,
}

/// The undo log of one staged commit ([`Snapshot::apply_commit_staged`]).
#[derive(Debug)]
#[must_use = "dropping the undo log makes a staged commit permanent"]
pub(crate) struct NamespaceUndo(Vec<(ArchivePath, Option<Entry>)>);

impl NamespaceUndo {
    /// Entries the staged commit set or removed: one per operation.
    pub(crate) fn mutations(&self) -> usize {
        self.0.len()
    }
}

/// The complete namespace at one commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    entries: BTreeMap<ArchivePath, Entry>,
}

/// Exclusive upper bound of `dir`'s descendants in byte order: every
/// descendant starts with `dir` + `/`, and `0` is the byte after `/`.
fn descendant_range(dir: &ArchivePath) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
    let mut lo = dir.as_stored().to_vec();
    lo.push(SEPARATOR);
    let mut hi = dir.as_stored().to_vec();
    hi.push(SEPARATOR + 1);
    (Bound::Excluded(lo), Bound::Excluded(hi))
}

impl Snapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, path: &ArchivePath) -> Option<&Entry> {
        self.entries.get(path)
    }

    /// Entries in byte order of their stored paths (directories before their
    /// descendants).
    pub fn iter(&self) -> impl Iterator<Item = (&ArchivePath, &Entry)> {
        self.entries.iter()
    }

    fn has_descendants(&self, dir: &ArchivePath, probes: &Cell<u64>) -> bool {
        bump(probes);
        // Paths are keyed by ArchivePath, which orders by stored bytes; probe
        // with byte bounds via a stored-form scan of the smallest candidate.
        let (lo, hi) = descendant_range(dir);
        let (Bound::Excluded(lo), Bound::Excluded(hi)) = (lo, hi) else {
            return false;
        };
        self.entries
            .range::<[u8], _>((Bound::Excluded(&lo[..]), Bound::Excluded(&hi[..])))
            .next()
            .is_some()
    }

    fn check_parent(&self, path: &ArchivePath, probes: &Cell<u64>) -> Result<(), NamespaceFault> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        bump(probes);
        match self.entries.get(&parent) {
            None => Err(NamespaceFault::MissingParent { path: path.clone() }),
            Some(e) if e.kind != EntryKind::Directory => {
                Err(NamespaceFault::ParentNotDirectory { path: path.clone() })
            }
            Some(_) => Ok(()),
        }
    }

    /// Apply one commit's operations, in order, then validate the completed
    /// state. On error the snapshot is unchanged.
    ///
    /// Only paths the commit touched are checked. That is sound because the
    /// snapshot was valid before the commit: an entry can become invalid only
    /// if it is itself new or replaced (its parent is checked), or if its
    /// parent was deleted or replaced by a file (then it is a descendant of a
    /// touched path whose final state is absent or a file, which is checked).
    /// A property test compares this against a full re-validation.
    pub fn apply_commit(
        &mut self,
        ops: &[NamespaceOp],
        kind_of: impl Fn(&FileVersionId) -> Option<EntryKind>,
    ) -> Result<(), NamespaceFault> {
        self.apply_commit_staged(ops, kind_of, &Cell::new(0))
            .map(drop)
    }

    /// As [`Snapshot::apply_commit`], but on success also returns the undo
    /// log, so a caller whose own step fails *after* the namespace accepted
    /// the commit (for example a SQLite statement or `COMMIT`) can put the
    /// snapshot back exactly with [`Snapshot::undo`]. The log is
    /// proportional to the commit's operations, never to the namespace, so
    /// staging needs no copy of the snapshot (T12 atomicity; review
    /// 2026-10-02 amendment 1).
    ///
    /// `probes` is incremented once per lookup in the snapshot (a point
    /// lookup or a descendant-range probe): the namespace half of the
    /// declared per-operation work (`super::bounds`, D10.5).
    pub(crate) fn apply_commit_staged(
        &mut self,
        ops: &[NamespaceOp],
        kind_of: impl Fn(&FileVersionId) -> Option<EntryKind>,
        probes: &Cell<u64>,
    ) -> Result<NamespaceUndo, NamespaceFault> {
        let mut undo: Vec<(ArchivePath, Option<Entry>)> = Vec::with_capacity(ops.len());
        let result = self
            .apply_ops(ops, &kind_of, &mut undo)
            .and_then(|touched| {
                for path in &touched {
                    bump(probes);
                    match self.entries.get(path) {
                        Some(entry) => {
                            self.check_parent(path, probes)?;
                            if entry.kind == EntryKind::File && self.has_descendants(path, probes) {
                                return Err(self.first_orphan(path, probes));
                            }
                        }
                        None => {
                            if self.has_descendants(path, probes) {
                                return Err(self.first_orphan(path, probes));
                            }
                        }
                    }
                }
                Ok(())
            });
        match result {
            Ok(()) => Ok(NamespaceUndo(undo)),
            Err(e) => {
                self.undo(NamespaceUndo(undo));
                Err(e)
            }
        }
    }

    /// Reverse a staged commit. Must be the most recent change applied.
    pub(crate) fn undo(&mut self, undo: NamespaceUndo) {
        for (path, prev) in undo.0.into_iter().rev() {
            match prev {
                Some(e) => self.entries.insert(path, e),
                None => self.entries.remove(&path),
            };
        }
    }

    fn first_orphan(&self, dir: &ArchivePath, probes: &Cell<u64>) -> NamespaceFault {
        probes.set(probes.get() + 2); // the range probe and `contains_key`
        let (lo, hi) = descendant_range(dir);
        let (Bound::Excluded(lo), Bound::Excluded(hi)) = (lo, hi) else {
            return NamespaceFault::MissingParent { path: dir.clone() };
        };
        let child = self
            .entries
            .range::<[u8], _>((Bound::Excluded(&lo[..]), Bound::Excluded(&hi[..])))
            .next()
            .map(|(p, _)| p.clone())
            .unwrap_or_else(|| dir.clone());
        if self.entries.contains_key(dir) {
            NamespaceFault::ParentNotDirectory { path: child }
        } else {
            NamespaceFault::MissingParent { path: child }
        }
    }

    fn apply_ops(
        &mut self,
        ops: &[NamespaceOp],
        kind_of: &impl Fn(&FileVersionId) -> Option<EntryKind>,
        undo: &mut Vec<(ArchivePath, Option<Entry>)>,
    ) -> Result<BTreeSet<ArchivePath>, NamespaceFault> {
        let mut touched = BTreeSet::new();
        for (op_seq, op) in ops.iter().enumerate() {
            match op {
                NamespaceOp::Put { path, version } => {
                    let kind =
                        kind_of(version).ok_or(NamespaceFault::UnknownFileVersion { op_seq })?;
                    let prev = self.entries.insert(
                        path.clone(),
                        Entry {
                            version: *version,
                            kind,
                        },
                    );
                    undo.push((path.clone(), prev));
                }
                NamespaceOp::Delete { path } => {
                    let prev = self
                        .entries
                        .remove(path)
                        .ok_or(NamespaceFault::DeleteMissing { op_seq })?;
                    undo.push((path.clone(), Some(prev)));
                }
            }
            touched.insert(op.path().clone());
        }
        Ok(touched)
    }

    /// Full re-validation of every entry. O(n log n); used by verification
    /// and as the reference for the incremental check.
    pub fn validate_all(&self) -> Result<(), NamespaceFault> {
        let probes = Cell::new(0);
        for path in self.entries.keys() {
            self.check_parent(path, &probes)?;
        }
        Ok(())
    }
}

fn bump(probes: &Cell<u64>) {
    probes.set(probes.get().saturating_add(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: FileVersionId = FileVersionId([1; 32]);
    const D: FileVersionId = FileVersionId([2; 32]);

    fn kind(v: &FileVersionId) -> Option<EntryKind> {
        match v.0[0] {
            1 => Some(EntryKind::File),
            2 => Some(EntryKind::Directory),
            _ => None,
        }
    }
    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }
    fn put(s: &str, v: FileVersionId) -> NamespaceOp {
        NamespaceOp::Put {
            path: p(s),
            version: v,
        }
    }
    fn del(s: &str) -> NamespaceOp {
        NamespaceOp::Delete { path: p(s) }
    }

    #[test]
    fn child_before_parent_within_one_commit_is_fine() {
        let mut s = Snapshot::new();
        s.apply_commit(&[put("a/b", F), put("a", D)], kind).unwrap();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn rename_is_delete_plus_put() {
        let mut s = Snapshot::new();
        s.apply_commit(&[put("old", F)], kind).unwrap();
        s.apply_commit(&[del("old"), put("new", F)], kind).unwrap();
        assert!(s.get(&p("old")).is_none());
        assert_eq!(s.get(&p("new")).map(|e| e.version), Some(F));
    }

    #[test]
    fn orphans_are_rejected_and_state_is_rolled_back() {
        let mut s = Snapshot::new();
        s.apply_commit(&[put("d", D), put("d/x", F), put("keep", F)], kind)
            .unwrap();
        let before = s.clone();

        // Non-recursive directory delete.
        let e = s.apply_commit(&[del("d")], kind).unwrap_err();
        assert_eq!(e, NamespaceFault::MissingParent { path: p("d/x") });
        assert_eq!(s, before);

        // Directory replaced by a file while it has children.
        let e = s.apply_commit(&[put("d", F)], kind).unwrap_err();
        assert_eq!(e, NamespaceFault::ParentNotDirectory { path: p("d/x") });
        assert_eq!(s, before);

        // Child under a file.
        let e = s.apply_commit(&[put("keep/x", F)], kind).unwrap_err();
        assert_eq!(e, NamespaceFault::ParentNotDirectory { path: p("keep/x") });
        assert_eq!(s, before);

        // Missing parent; delete of an absent path; unknown version.
        assert!(s.apply_commit(&[put("nope/x", F)], kind).is_err());
        assert_eq!(
            s.apply_commit(&[del("absent")], kind),
            Err(NamespaceFault::DeleteMissing { op_seq: 0 })
        );
        assert_eq!(
            s.apply_commit(&[put("z", FileVersionId([9; 32]))], kind),
            Err(NamespaceFault::UnknownFileVersion { op_seq: 0 })
        );
        assert_eq!(s, before);
    }

    #[test]
    fn recursive_delete_must_enumerate_children_first_or_together() {
        let mut s = Snapshot::new();
        s.apply_commit(
            &[put("d", D), put("d/x", F), put("d/y", D), put("d/y/z", F)],
            kind,
        )
        .unwrap();
        // Any order within the commit, as long as the completed state is valid.
        s.apply_commit(&[del("d"), del("d/y/z"), del("d/x"), del("d/y")], kind)
            .unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn sibling_with_common_prefix_is_not_a_descendant() {
        let mut s = Snapshot::new();
        s.apply_commit(&[put("d", D), put("d.txt", F), put("d0", F)], kind)
            .unwrap();
        // Deleting "d" must not be blocked by "d.txt" or "d0".
        s.apply_commit(&[del("d")], kind).unwrap();
        assert_eq!(s.len(), 2);
    }
}
