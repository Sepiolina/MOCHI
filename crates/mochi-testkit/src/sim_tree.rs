//! [`SimTree`]: an in-memory restore destination (`RestoreDir`, plan C6)
//! that can behave like a case-insensitive filesystem or apply Windows
//! naming rules on any host, so collision and unsupported-name handling is
//! tested everywhere, not only where such a filesystem happens to exist.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::manifest::Attributes;
use mochi_core::storage::{
    windows_name_issue, AttributeIssue, AttributeKind, CaseBehavior, DirectoryDurability, Existing,
    NameIssue, RestoreDir, StorageError,
};

use crate::SimStorage;

#[derive(Debug, Clone)]
enum Node {
    Dir,
    File(SimStorage),
    /// Neither: a symbolic link, a device (`Existing::Other`).
    Other,
}

#[derive(Debug, Default)]
struct Tree {
    /// Full path (components) → node. The root is the empty path and is
    /// implicit.
    nodes: BTreeMap<Vec<Vec<u8>>, Node>,
    /// Attributes applied to each path, in the order applied.
    attributes: BTreeMap<Vec<Vec<u8>>, Attributes>,
    applied: Vec<Vec<Vec<u8>>>,
    case_insensitive: bool,
    windows_rules: bool,
    unprivileged: bool,
    /// Files moved aside (`RestoreDir::set_aside`): their path and bytes.
    set_aside: Vec<(String, Vec<u8>)>,
    /// Moving aside fails (the Recycle Bin is unavailable).
    set_aside_fails: bool,
    /// Existing directories that may not be entered (as Linux refuses one
    /// other users can modify).
    closed_dirs: Vec<Vec<Vec<u8>>>,
}

impl Tree {
    fn fold(&self, name: &[u8]) -> Vec<u8> {
        if self.case_insensitive {
            name.to_ascii_lowercase()
        } else {
            name.to_vec()
        }
    }

    /// The existing child of `dir` that `name` lands on, if any.
    fn existing(&self, dir: &[Vec<u8>], name: &[u8]) -> Option<Vec<Vec<u8>>> {
        let want = self.fold(name);
        self.nodes
            .keys()
            .find(|k| {
                k.len() == dir.len() + 1 && k.starts_with(dir) && self.fold(&k[dir.len()]) == want
            })
            .cloned()
    }
}

/// A directory in an in-memory tree. Clones share the tree.
#[derive(Debug, Clone, Default)]
pub struct SimTree {
    tree: Arc<Mutex<Tree>>,
    at: Vec<Vec<u8>>,
}

fn exists(name: &[u8]) -> StorageError {
    StorageError::Exists {
        name: String::from_utf8_lossy(name).into_owned(),
    }
}

fn split(path: &str) -> Vec<Vec<u8>> {
    path.split('/').map(|c| c.as_bytes().to_vec()).collect()
}

impl SimTree {
    /// An empty root directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names that differ only in ASCII case land on the same entry.
    pub fn case_insensitive(self) -> Self {
        self.tree.lock().unwrap().case_insensitive = true;
        self
    }

    /// Refuse ownership changes, as an unprivileged process is refused.
    pub fn unprivileged(self) -> Self {
        self.tree.lock().unwrap().unprivileged = true;
        self
    }

    /// The attributes applied to `path` (components joined by `/`).
    pub fn attributes(&self, path: &[u8]) -> Option<Attributes> {
        let p: Vec<Vec<u8>> = path.split(|b| *b == b'/').map(<[u8]>::to_vec).collect();
        self.tree.lock().unwrap().attributes.get(&p).copied()
    }

    /// The paths attributes were applied to, in order.
    pub fn applied_order(&self) -> Vec<String> {
        self.tree
            .lock()
            .unwrap()
            .applied
            .iter()
            .map(|k| {
                k.iter()
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    /// Apply Windows naming rules (`windows_name_issue`).
    pub fn windows_rules(self) -> Self {
        self.tree.lock().unwrap().windows_rules = true;
        self
    }

    /// Pre-existing destination content: a file at `path` (its parent
    /// directories are created too).
    pub fn insert_file(&self, path: &str, bytes: &[u8]) {
        let p = split(path);
        let mut t = self.tree.lock().unwrap();
        for i in 1..p.len() {
            t.nodes.entry(p[..i].to_vec()).or_insert(Node::Dir);
        }
        t.nodes
            .insert(p, Node::File(SimStorage::from_bytes(bytes.to_vec())));
    }

    /// Pre-existing destination content: an empty directory at `path` (its
    /// parents too).
    pub fn insert_dir(&self, path: &str) {
        let p = split(path);
        let mut t = self.tree.lock().unwrap();
        for i in 1..=p.len() {
            t.nodes.entry(p[..i].to_vec()).or_insert(Node::Dir);
        }
    }

    /// Pre-existing destination content that is neither a file nor a
    /// directory (a symbolic link, a device) at `path`.
    pub fn insert_other(&self, path: &str) {
        let p = split(path);
        let mut t = self.tree.lock().unwrap();
        for i in 1..p.len() {
            t.nodes.entry(p[..i].to_vec()).or_insert(Node::Dir);
        }
        t.nodes.insert(p, Node::Other);
    }

    /// The existing directory at `path` may not be entered
    /// (`open_existing_dir` answers `None`).
    pub fn close_dir(&self, path: &str) {
        self.tree.lock().unwrap().closed_dirs.push(split(path));
    }

    /// Moving an existing file aside fails from now on.
    pub fn fail_set_aside(&self) {
        self.tree.lock().unwrap().set_aside_fails = true;
    }

    /// The files moved aside so far, as `(path, bytes)`, in order.
    pub fn set_aside_files(&self) -> Vec<(String, Vec<u8>)> {
        self.tree.lock().unwrap().set_aside.clone()
    }

    /// Every entry, as `/`-joined paths (lossy), files and directories.
    pub fn paths(&self) -> Vec<String> {
        self.tree
            .lock()
            .unwrap()
            .nodes
            .keys()
            .map(|k| {
                k.iter()
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    /// The bytes of the file at `path` (components joined by `/`).
    pub fn file(&self, path: &[u8]) -> Option<Vec<u8>> {
        let p: Vec<Vec<u8>> = path.split(|b| *b == b'/').map(<[u8]>::to_vec).collect();
        match self.tree.lock().unwrap().nodes.get(&p) {
            Some(Node::File(s)) => Some(s.contents()),
            _ => None,
        }
    }

    /// How many of the file's bytes were synced (its `sync_data` calls),
    /// for what a power failure would keep.
    pub fn file_synced_len(&self, path: &[u8]) -> Option<usize> {
        let p: Vec<Vec<u8>> = path.split(|b| *b == b'/').map(<[u8]>::to_vec).collect();
        match self.tree.lock().unwrap().nodes.get(&p) {
            Some(Node::File(s)) => Some(s.synced_len()),
            _ => None,
        }
    }

    /// Whether `path` is a directory.
    pub fn is_dir(&self, path: &[u8]) -> bool {
        let p: Vec<Vec<u8>> = path.split(|b| *b == b'/').map(<[u8]>::to_vec).collect();
        matches!(self.tree.lock().unwrap().nodes.get(&p), Some(Node::Dir))
    }

    fn child(&self, name: &[u8]) -> Vec<Vec<u8>> {
        let mut p = self.at.clone();
        p.push(name.to_vec());
        p
    }
}

impl RestoreDir for SimTree {
    type File = SimStorage;

    fn name_issue(&self, name: &[u8]) -> Option<NameIssue> {
        if self.tree.lock().unwrap().windows_rules {
            windows_name_issue(name)
        } else {
            None
        }
    }

    fn create_dir(&mut self, name: &[u8]) -> Result<Self, StorageError> {
        let mut t = self.tree.lock().unwrap();
        if t.existing(&self.at, name).is_some() {
            return Err(exists(name));
        }
        let p = self.child(name);
        t.nodes.insert(p.clone(), Node::Dir);
        Ok(SimTree {
            tree: self.tree.clone(),
            at: p,
        })
    }

    fn create_file(&mut self, name: &[u8]) -> Result<SimStorage, StorageError> {
        let mut t = self.tree.lock().unwrap();
        if t.existing(&self.at, name).is_some() {
            return Err(exists(name));
        }
        let s = SimStorage::new();
        t.nodes.insert(self.child(name), Node::File(s.clone()));
        Ok(s)
    }

    fn publish_no_replace(&mut self, from: &[u8], to: &[u8]) -> Result<(), StorageError> {
        let mut t = self.tree.lock().unwrap();
        if t.existing(&self.at, to).is_some() {
            return Err(exists(to));
        }
        let node = t
            .nodes
            .remove(&self.child(from))
            .ok_or(StorageError::Unsupported("publishing a missing file"))?;
        t.nodes.insert(self.child(to), node);
        Ok(())
    }

    fn discard(&mut self, _file: SimStorage, name: &[u8]) -> Result<(), StorageError> {
        self.tree.lock().unwrap().nodes.remove(&self.child(name));
        Ok(())
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        Ok(DirectoryDurability::Confirmed)
    }

    fn case_behavior(&mut self) -> Result<CaseBehavior, StorageError> {
        Ok(if self.tree.lock().unwrap().case_insensitive {
            CaseBehavior::Insensitive
        } else {
            CaseBehavior::Sensitive
        })
    }

    fn entry_exists(&mut self, name: &[u8]) -> Result<bool, StorageError> {
        Ok(self.tree.lock().unwrap().existing(&self.at, name).is_some())
    }

    fn existing(&mut self, name: &[u8]) -> Result<Existing, StorageError> {
        let t = self.tree.lock().unwrap();
        Ok(
            match t.existing(&self.at, name).and_then(|p| t.nodes.get(&p)) {
                None => Existing::None,
                Some(Node::Dir) => Existing::Directory,
                Some(Node::File(_)) => Existing::File,
                Some(Node::Other) => Existing::Other,
            },
        )
    }

    fn open_existing_dir(&mut self, name: &[u8]) -> Result<Option<Self>, StorageError> {
        let t = self.tree.lock().unwrap();
        let Some(p) = t.existing(&self.at, name) else {
            return Ok(None);
        };
        if !matches!(t.nodes.get(&p), Some(Node::Dir)) || t.closed_dirs.contains(&p) {
            return Ok(None);
        }
        Ok(Some(SimTree {
            tree: self.tree.clone(),
            at: p,
        }))
    }

    fn set_aside(&mut self, name: &[u8]) -> Result<(), StorageError> {
        let mut t = self.tree.lock().unwrap();
        if t.set_aside_fails {
            return Err(StorageError::Io(std::io::Error::other(
                "the Recycle Bin is not available",
            )));
        }
        let Some(p) = t.existing(&self.at, name) else {
            return Err(StorageError::Unsupported("nothing to move aside"));
        };
        let Some(Node::File(bytes)) = t.nodes.get(&p).cloned() else {
            return Err(StorageError::Unsupported(
                "only a regular file is moved aside, never a folder or a link",
            ));
        };
        t.nodes.remove(&p);
        let shown = p
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("/");
        t.set_aside.push((shown, bytes.contents()));
        Ok(())
    }

    fn apply_attributes(
        &mut self,
        name: &[u8],
        _kind: EntryKind,
        attributes: &Attributes,
    ) -> Vec<AttributeIssue> {
        let mut t = self.tree.lock().unwrap();
        let p = self.child(name);
        let mut applied = *attributes;
        let mut issues = Vec::new();
        if t.unprivileged && attributes.posix.is_some() {
            issues.push(AttributeIssue {
                attribute: AttributeKind::Ownership,
                reason: "operation not permitted".into(),
            });
            if let Some(posix) = applied.posix.as_mut() {
                posix.uid = u32::MAX;
                posix.gid = u32::MAX;
            }
        }
        t.attributes.insert(p.clone(), applied);
        t.applied.push(p);
        issues
    }
}
