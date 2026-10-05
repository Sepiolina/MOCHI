//! [`SimTree`]: an in-memory restore destination (`RestoreDir`, plan C6)
//! that can behave like a case-insensitive filesystem or apply Windows
//! naming rules on any host, so collision and unsupported-name handling is
//! tested everywhere, not only where such a filesystem happens to exist.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mochi_core::storage::{
    windows_name_issue, DirectoryDurability, NameIssue, RestoreDir, StorageError,
};

use crate::SimStorage;

#[derive(Debug, Clone)]
enum Node {
    Dir,
    File(SimStorage),
}

#[derive(Debug, Default)]
struct Tree {
    /// Full path (components) → node. The root is the empty path and is
    /// implicit.
    nodes: BTreeMap<Vec<Vec<u8>>, Node>,
    case_insensitive: bool,
    windows_rules: bool,
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
}
