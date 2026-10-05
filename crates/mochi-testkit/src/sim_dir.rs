//! An in-memory directory of [`SimStorage`] files with a crash model and
//! scripted faults (plan T19): the [`StorageDir`] operations that creation
//! (Annex B.2 D13) and tail quarantine (D14) rely on.
//!
//! **Durability model.** A name created, published, or removed is visible at
//! once, but survives a power loss only after a successful
//! [`StorageDir::sync_directory`]. A process kill ([`CrashMode::KeepAll`])
//! keeps every name. Each file's bytes follow its own [`SimStorage`] model,
//! and a crash releases every lock.
//!
//! **Publication** is modelled as link then unlink, the portable mechanism
//! (`OsDir`), so a crash between the two steps
//! ([`DirFault::HaltBetweenLinkAndUnlink`]) leaves both names on one file.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use mochi_core::storage::{
    check_file_name, DirectoryDurability, RemoveOutcome, Storage, StorageDir, StorageError,
};

use crate::sim::{CrashMode, Fault, Halted, SimStorage};

/// A scripted directory fault. Indices are zero-based and count operations
/// of that kind on this directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirFault {
    /// Halt before the `index`-th mutating directory operation (create,
    /// publish, discard, remove, sync) takes effect; it and every later
    /// operation fail with [`Halted`].
    HaltBeforeOp { index: usize },
    /// The `index`-th `create_exclusive` fails with an I/O error and creates
    /// nothing.
    FailCreate { index: usize },
    /// The `index`-th `publish_no_replace` fails before linking: nothing
    /// changes.
    FailPublish { index: usize },
    /// The `index`-th `publish_no_replace` links the new name, then the
    /// process halts before the old name is removed.
    HaltBetweenLinkAndUnlink { index: usize },
    /// The `index`-th removal (`discard` or `remove_if_unlocked`) fails with
    /// an I/O error and removes nothing.
    FailRemove { index: usize },
    /// The `index`-th `sync_directory` fails with an I/O error and persists
    /// nothing.
    FailSyncDirectory { index: usize },
    /// The `index`-th `sync_directory` persists nothing and reports a
    /// best-effort flush that did not confirm (the Windows case, plan O12).
    UnconfirmedSyncDirectory { index: usize },
}

/// Directory operations recorded in the trace, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirOp {
    Create { name: String },
    Link { from: String, to: String },
    Unlink { name: String },
    SyncDirectory,
}

#[derive(Debug, Default)]
struct DirInner {
    /// Names visible now.
    entries: BTreeMap<String, SimStorage>,
    /// Names that survive a power loss (as of the last successful sync).
    durable: BTreeMap<String, SimStorage>,
    faults: Vec<DirFault>,
    ops: usize,
    creates: usize,
    publishes: usize,
    removes: usize,
    syncs: usize,
    halted: bool,
    trace: Vec<DirOp>,
    /// Faults given to the next file `create_exclusive` makes.
    next_file_faults: Vec<Fault>,
}

fn halted_error() -> StorageError {
    StorageError::Io(io::Error::other(Halted))
}

fn injected(what: &str) -> StorageError {
    StorageError::Io(io::Error::other(format!("injected failure: {what}")))
}

impl DirInner {
    fn has(&self, pred: impl Fn(&DirFault) -> bool) -> bool {
        self.faults.iter().any(pred)
    }

    fn begin(&mut self) -> Result<(), StorageError> {
        if self.halted {
            return Err(halted_error());
        }
        let idx = self.ops;
        if self.has(|f| matches!(f, DirFault::HaltBeforeOp { index } if *index == idx)) {
            self.halted = true;
            return Err(halted_error());
        }
        self.ops += 1;
        Ok(())
    }

    fn take_remove(&mut self) -> Result<(), StorageError> {
        let idx = self.removes;
        self.removes += 1;
        if self.has(|f| matches!(f, DirFault::FailRemove { index } if *index == idx)) {
            return Err(injected("remove"));
        }
        Ok(())
    }
}

/// In-memory [`StorageDir`]. Clones share the same directory.
#[derive(Debug, Clone, Default)]
pub struct SimDir {
    shared: Arc<Mutex<DirInner>>,
}

impl SimDir {
    /// An empty directory, no faults.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty directory with the given faults scheduled.
    pub fn with_faults(faults: impl IntoIterator<Item = DirFault>) -> Self {
        let d = Self::new();
        for f in faults {
            d.add_fault(f);
        }
        d
    }

    fn inner(&self) -> MutexGuard<'_, DirInner> {
        self.shared.lock().expect("SimDir mutex poisoned")
    }

    pub fn add_fault(&self, fault: DirFault) {
        self.inner().faults.push(fault);
    }

    /// Schedule `fault` on the next file `create_exclusive` makes (for
    /// example a halt before its k-th mutation).
    pub fn add_next_file_fault(&self, fault: Fault) {
        self.inner().next_file_faults.push(fault);
    }

    /// Add an existing file under `name`, durable (a fixture: as if created
    /// and synced long ago). Replaces nothing: panics if `name` exists.
    pub fn insert(&self, name: &str, file: SimStorage) {
        let mut d = self.inner();
        assert!(!d.entries.contains_key(name), "{name} exists");
        d.entries.insert(name.to_string(), file.clone());
        d.durable.insert(name.to_string(), file);
    }

    /// The file currently named `name`.
    pub fn file(&self, name: &str) -> Option<SimStorage> {
        self.inner().entries.get(name).cloned()
    }

    /// Names visible now, sorted.
    pub fn names(&self) -> Vec<String> {
        self.inner().entries.keys().cloned().collect()
    }

    /// Names that would survive a power loss now, sorted.
    pub fn durable_names(&self) -> Vec<String> {
        self.inner().durable.keys().cloned().collect()
    }

    pub fn is_halted(&self) -> bool {
        self.inner().halted
    }

    pub fn trace(&self) -> Vec<DirOp> {
        self.inner().trace.clone()
    }

    /// What a fresh process finds after a crash: a healthy, fault-free
    /// directory. Under [`CrashMode::KeepAll`] every visible name survives;
    /// under the power-loss modes only names made durable by a directory
    /// sync. Each surviving file is its own `crash_image(mode)`, unlocked.
    pub fn crash_image(&self, mode: CrashMode) -> SimDir {
        let d = self.inner();
        let names = match mode {
            CrashMode::KeepAll => &d.entries,
            _ => &d.durable,
        };
        let out = SimDir::new();
        {
            let mut o = out.inner();
            for (name, f) in names {
                let img = f.crash_image(mode);
                o.entries.insert(name.clone(), img.clone());
                o.durable.insert(name.clone(), img);
            }
        }
        out
    }
}

impl StorageDir for SimDir {
    type File = SimStorage;

    fn create_exclusive(&mut self, name: &str) -> Result<SimStorage, StorageError> {
        check_file_name(name)?;
        let mut d = self.inner();
        d.begin()?;
        let idx = d.creates;
        d.creates += 1;
        if d.entries.contains_key(name) {
            return Err(StorageError::Exists {
                name: name.to_string(),
            });
        }
        if d.has(|f| matches!(f, DirFault::FailCreate { index } if *index == idx)) {
            return Err(injected("create"));
        }
        let faults = std::mem::take(&mut d.next_file_faults);
        let mut file = SimStorage::with_faults(faults);
        file.try_lock_exclusive()?;
        d.entries.insert(name.to_string(), file.clone());
        d.trace.push(DirOp::Create {
            name: name.to_string(),
        });
        Ok(file)
    }

    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError> {
        check_file_name(from)?;
        check_file_name(to)?;
        let mut d = self.inner();
        d.begin()?;
        let idx = d.publishes;
        d.publishes += 1;
        let Some(file) = d.entries.get(from).cloned() else {
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from:?} does not exist"),
            )));
        };
        if d.entries.contains_key(to) {
            return Err(StorageError::Exists {
                name: to.to_string(),
            });
        }
        if d.has(|f| matches!(f, DirFault::FailPublish { index } if *index == idx)) {
            return Err(injected("publish"));
        }
        d.entries.insert(to.to_string(), file);
        d.trace.push(DirOp::Link {
            from: from.to_string(),
            to: to.to_string(),
        });
        if d.has(|f| matches!(f, DirFault::HaltBetweenLinkAndUnlink { index } if *index == idx)) {
            d.halted = true;
            return Err(halted_error());
        }
        d.entries.remove(from);
        d.trace.push(DirOp::Unlink {
            name: from.to_string(),
        });
        Ok(())
    }

    fn discard(&mut self, file: SimStorage, name: &str) -> Result<(), StorageError> {
        check_file_name(name)?;
        let mut d = self.inner();
        d.begin()?;
        match d.entries.get(name) {
            Some(f) if f.same_file(&file) => {}
            _ => {
                return Err(StorageError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{name:?} does not name the file being discarded"),
                )))
            }
        }
        d.take_remove()?;
        d.entries.remove(name);
        d.trace.push(DirOp::Unlink {
            name: name.to_string(),
        });
        Ok(())
    }

    fn remove_if_unlocked(&mut self, name: &str) -> Result<RemoveOutcome, StorageError> {
        check_file_name(name)?;
        let mut d = self.inner();
        d.begin()?;
        let Some(file) = d.entries.get(name).cloned() else {
            return Ok(RemoveOutcome::Missing);
        };
        let mut probe = file.handle();
        match probe.try_lock_exclusive() {
            Ok(()) => {}
            Err(StorageError::LockHeld) => return Ok(RemoveOutcome::Locked),
            Err(e) => return Err(e),
        }
        let removed = d.take_remove();
        let _ = probe.unlock();
        removed?;
        d.entries.remove(name);
        d.trace.push(DirOp::Unlink {
            name: name.to_string(),
        });
        Ok(RemoveOutcome::Removed)
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        let mut d = self.inner();
        d.begin()?;
        let idx = d.syncs;
        d.syncs += 1;
        d.trace.push(DirOp::SyncDirectory);
        if d.has(|f| matches!(f, DirFault::FailSyncDirectory { index } if *index == idx)) {
            return Err(injected("sync_directory"));
        }
        if d.has(|f| matches!(f, DirFault::UnconfirmedSyncDirectory { index } if *index == idx)) {
            return Ok(DirectoryDurability::Unconfirmed(
                "simulated best-effort directory flush".into(),
            ));
        }
        d.durable = d.entries.clone();
        Ok(DirectoryDurability::Confirmed)
    }
}
