//! In-memory storage with a durability model and scripted faults.

use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use mochi_core::storage::{DirectoryDurability, ReadStorage, Storage, StorageError};

/// Marker error carried inside `StorageError::Io` when the simulated process
/// has halted (a crash). Use [`is_halted`] to recognise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Halted;

impl fmt::Display for Halted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("simulated halt: the process died here")
    }
}

impl std::error::Error for Halted {}

fn halted_error() -> StorageError {
    StorageError::Io(io::Error::other(Halted))
}

fn injected_error(what: &str) -> StorageError {
    StorageError::Io(io::Error::other(format!("injected failure: {what}")))
}

/// True if `err` is the simulated-halt marker.
pub fn is_halted(err: &StorageError) -> bool {
    match err {
        StorageError::Io(e) => e.get_ref().is_some_and(|inner| inner.is::<Halted>()),
        _ => false,
    }
}

/// A scripted fault. Indices are zero-based and count operations of that kind
/// on the shared storage, across all handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Halt *before* the `mutation_index`-th mutating operation
    /// (append, sync_data, sync_directory, truncate) takes effect. That
    /// operation and everything after it fails with [`Halted`].
    HaltBeforeMutation { mutation_index: usize },
    /// The `append_index`-th append writes only its first `keep` bytes, then the
    /// process halts. Models a torn write.
    TearAppend { append_index: usize, keep: usize },
    /// The `sync_index`-th `sync_data` returns an I/O error and persists nothing.
    FailSync { sync_index: usize },
    /// The `sync_index`-th `sync_data` returns `Ok` **without persisting**.
    /// Lets tests prove code does not treat an ack as proof of durability.
    LieSync { sync_index: usize },
    /// The `index`-th `sync_directory` returns an I/O error.
    FailSyncDirectory { index: usize },
    /// The `index`-th `sync_directory` reports a best-effort flush that did
    /// not confirm durability (the Windows case, plan O12).
    UnconfirmedSyncDirectory { index: usize },
}

/// Operations recorded in the trace, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Append { offset: u64, len: u64 },
    SyncData,
    SyncDirectory,
    Truncate { new_len: u64 },
    Lock,
    Unlock,
}

/// What survives a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashMode {
    /// Process killed; the OS keeps everything written (page cache survives).
    KeepAll,
    /// Power loss: only bytes covered by a successful sync survive.
    SyncedOnly,
    /// Power loss with a torn tail: synced bytes plus the first `keep_unsynced`
    /// unsynced bytes survive.
    TornTail { keep_unsynced: usize },
    /// Power loss with reordered or lost writes: the file keeps its full
    /// written length, synced bytes survive, and each `sector`-byte sector of
    /// the unsynced region independently survives or reads back as zeros,
    /// chosen deterministically from `seed`. Models a device or filesystem
    /// that persisted later writes but not earlier ones within an unsynced
    /// window (e.g. ext4 `data=writeback` exposing zero-filled extents).
    LostWrites { seed: u64, sector: usize },
}

#[derive(Debug, Default)]
struct Inner {
    data: Vec<u8>,
    /// Bytes `0..synced_len` are durable.
    synced_len: usize,
    faults: Vec<Fault>,
    mutations: usize,
    appends: usize,
    syncs: usize,
    dir_syncs: usize,
    halted: bool,
    lock_owner: Option<u64>,
    next_handle: u64,
    trace: Vec<Op>,
}

impl Inner {
    fn has(&self, pred: impl Fn(&Fault) -> bool) -> bool {
        self.faults.iter().any(pred)
    }

    fn halt(&mut self) {
        self.halted = true;
        // A dead process releases its locks.
        self.lock_owner = None;
    }

    /// Common prologue for mutating operations: refuse if halted, then apply a
    /// scheduled halt-before-mutation. Increments the mutation counter.
    fn begin_mutation(&mut self) -> Result<(), StorageError> {
        if self.halted {
            return Err(halted_error());
        }
        let idx = self.mutations;
        if self.has(
            |f| matches!(f, Fault::HaltBeforeMutation { mutation_index } if *mutation_index == idx),
        ) {
            self.halt();
            return Err(halted_error());
        }
        self.mutations += 1;
        Ok(())
    }
}

/// In-memory [`Storage`] with fault injection. Cloning via [`SimStorage::handle`]
/// gives a second writer on the *same* bytes, which is how tests exercise lock
/// contention. The original handle keeps working after a simulated halt only to
/// report the halt; inspect state afterwards with the accessor methods or
/// [`SimStorage::crash_image`].
#[derive(Debug, Clone)]
pub struct SimStorage {
    shared: Arc<Mutex<Inner>>,
    id: u64,
}

impl Default for SimStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SimStorage {
    /// Empty storage, no faults.
    pub fn new() -> Self {
        Self::from_bytes(Vec::new())
    }

    /// Storage pre-loaded with `bytes`, all of which count as durable.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let inner = Inner {
            synced_len: bytes.len(),
            data: bytes,
            next_handle: 1,
            ..Inner::default()
        };
        Self {
            shared: Arc::new(Mutex::new(inner)),
            id: 0,
        }
    }

    /// Empty storage with the given faults scheduled.
    pub fn with_faults(faults: impl IntoIterator<Item = Fault>) -> Self {
        let s = Self::new();
        for f in faults {
            s.add_fault(f);
        }
        s
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.shared.lock().expect("SimStorage mutex poisoned")
    }

    pub fn add_fault(&self, fault: Fault) {
        self.lock_inner().faults.push(fault);
    }

    /// A second handle onto the same bytes (its own lock identity).
    pub fn handle(&self) -> SimStorage {
        let mut inner = self.lock_inner();
        let id = inner.next_handle;
        inner.next_handle += 1;
        drop(inner);
        SimStorage {
            shared: Arc::clone(&self.shared),
            id,
        }
    }

    /// Every byte currently written, durable or not.
    pub fn contents(&self) -> Vec<u8> {
        self.lock_inner().data.clone()
    }

    /// Number of leading bytes made durable by a successful sync.
    pub fn synced_len(&self) -> usize {
        self.lock_inner().synced_len
    }

    /// Whether a scheduled fault has halted the simulated process.
    pub fn is_halted(&self) -> bool {
        self.lock_inner().halted
    }

    /// The operations performed so far, in order.
    pub fn trace(&self) -> Vec<Op> {
        self.lock_inner().trace.clone()
    }

    /// What a fresh process would find on disk after a crash: a new, healthy,
    /// fault-free storage whose bytes are all durable.
    pub fn crash_image(&self, mode: CrashMode) -> SimStorage {
        let inner = self.lock_inner();
        let keep = match mode {
            CrashMode::KeepAll | CrashMode::LostWrites { .. } => inner.data.len(),
            CrashMode::SyncedOnly => inner.synced_len,
            CrashMode::TornTail { keep_unsynced } => inner
                .synced_len
                .saturating_add(keep_unsynced)
                .min(inner.data.len()),
        };
        let mut bytes = inner.data.get(..keep).unwrap_or_default().to_vec();
        if let CrashMode::LostWrites { seed, sector } = mode {
            let sector = sector.max(1);
            let mut state = seed;
            let mut at = inner.synced_len;
            while at < bytes.len() {
                state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = state;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                let end = at.saturating_add(sector).min(bytes.len());
                if (z ^ (z >> 31)) & 1 == 1 {
                    bytes[at..end].fill(0);
                }
                at = end;
            }
        }
        SimStorage::from_bytes(bytes)
    }
}

impl ReadStorage for SimStorage {
    fn size(&self) -> Result<u64, StorageError> {
        let inner = self.lock_inner();
        if inner.halted {
            return Err(halted_error());
        }
        Ok(inner.data.len() as u64)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let inner = self.lock_inner();
        if inner.halted {
            return Err(halted_error());
        }
        let Ok(start) = usize::try_from(offset) else {
            return Ok(0);
        };
        let Some(src) = inner.data.get(start..) else {
            return Ok(0);
        };
        let n = src.len().min(buf.len());
        if let (Some(dst), Some(from)) = (buf.get_mut(..n), src.get(..n)) {
            dst.copy_from_slice(from);
        }
        Ok(n)
    }
}

impl Storage for SimStorage {
    fn append(&mut self, data: &[u8]) -> Result<u64, StorageError> {
        let mut inner = self.lock_inner();
        inner.begin_mutation()?;
        let append_idx = inner.appends;
        inner.appends += 1;
        let offset = inner.data.len() as u64;

        let tear = inner.faults.iter().find_map(|f| match f {
            Fault::TearAppend { append_index, keep } if *append_index == append_idx => Some(*keep),
            _ => None,
        });
        if let Some(keep) = tear {
            let keep = keep.min(data.len());
            inner
                .data
                .extend_from_slice(data.get(..keep).unwrap_or_default());
            inner.trace.push(Op::Append {
                offset,
                len: keep as u64,
            });
            inner.halt();
            return Err(halted_error());
        }

        inner.data.extend_from_slice(data);
        inner.trace.push(Op::Append {
            offset,
            len: data.len() as u64,
        });
        Ok(offset)
    }

    fn sync_data(&mut self) -> Result<(), StorageError> {
        let mut inner = self.lock_inner();
        inner.begin_mutation()?;
        let sync_idx = inner.syncs;
        inner.syncs += 1;
        inner.trace.push(Op::SyncData);
        if inner.has(|f| matches!(f, Fault::FailSync { sync_index } if *sync_index == sync_idx)) {
            return Err(injected_error("sync_data"));
        }
        if inner.has(|f| matches!(f, Fault::LieSync { sync_index } if *sync_index == sync_idx)) {
            return Ok(());
        }
        inner.synced_len = inner.data.len();
        Ok(())
    }

    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        let mut inner = self.lock_inner();
        inner.begin_mutation()?;
        let idx = inner.dir_syncs;
        inner.dir_syncs += 1;
        inner.trace.push(Op::SyncDirectory);
        if inner.has(|f| matches!(f, Fault::FailSyncDirectory { index } if *index == idx)) {
            return Err(injected_error("sync_directory"));
        }
        if inner.has(|f| matches!(f, Fault::UnconfirmedSyncDirectory { index } if *index == idx)) {
            return Ok(DirectoryDurability::Unconfirmed(
                "simulated best-effort directory flush".into(),
            ));
        }
        Ok(DirectoryDurability::Confirmed)
    }

    fn try_lock_exclusive(&mut self) -> Result<(), StorageError> {
        let mut inner = self.lock_inner();
        if inner.halted {
            return Err(halted_error());
        }
        match inner.lock_owner {
            Some(owner) if owner != self.id => Err(StorageError::LockHeld),
            _ => {
                inner.lock_owner = Some(self.id);
                inner.trace.push(Op::Lock);
                Ok(())
            }
        }
    }

    fn unlock(&mut self) -> Result<(), StorageError> {
        let mut inner = self.lock_inner();
        if inner.halted {
            return Err(halted_error());
        }
        if inner.lock_owner == Some(self.id) {
            inner.lock_owner = None;
            inner.trace.push(Op::Unlock);
        }
        Ok(())
    }

    fn truncate(&mut self, new_len: u64) -> Result<(), StorageError> {
        let mut inner = self.lock_inner();
        inner.begin_mutation()?;
        let size = inner.data.len() as u64;
        if new_len > size {
            return Err(StorageError::OutOfBounds {
                offset: new_len,
                len: 0,
                size,
            });
        }
        let new_len_usize = usize::try_from(new_len).unwrap_or(usize::MAX);
        inner.data.truncate(new_len_usize);
        inner.synced_len = inner.synced_len.min(new_len_usize);
        inner.trace.push(Op::Truncate { new_len });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan C0 exit criterion: a test can inject a halted write and observe it.
    #[test]
    fn injected_halt_is_observable() {
        let mut s = SimStorage::with_faults([Fault::HaltBeforeMutation { mutation_index: 1 }]);
        s.append(b"abc").unwrap(); // mutation 0 succeeds
        let err = s.sync_data().unwrap_err(); // mutation 1 halts
        assert!(is_halted(&err));
        assert!(s.is_halted());
        // The write happened but was never made durable.
        assert_eq!(s.contents(), b"abc");
        assert_eq!(s.synced_len(), 0);
        // After the halt every operation fails, reads included.
        assert!(is_halted(&s.append(b"x").unwrap_err()));
        assert!(is_halted(&s.size().unwrap_err()));
        // What a fresh process finds depends on the crash model.
        assert_eq!(s.crash_image(CrashMode::KeepAll).contents(), b"abc");
        assert_eq!(s.crash_image(CrashMode::SyncedOnly).contents(), b"");
    }

    #[test]
    fn halt_before_first_mutation_writes_nothing() {
        let mut s = SimStorage::with_faults([Fault::HaltBeforeMutation { mutation_index: 0 }]);
        assert!(is_halted(&s.append(b"abc").unwrap_err()));
        assert!(s.contents().is_empty());
    }

    #[test]
    fn torn_append_leaves_only_a_prefix_then_halts() {
        let mut s = SimStorage::with_faults([Fault::TearAppend {
            append_index: 1,
            keep: 3,
        }]);
        s.append(b"0123").unwrap();
        s.sync_data().unwrap();
        let err = s.append(b"ABCDEFGH").unwrap_err();
        assert!(is_halted(&err));
        assert_eq!(s.contents(), b"0123ABC");
        assert_eq!(s.synced_len(), 4);
        assert_eq!(s.crash_image(CrashMode::SyncedOnly).contents(), b"0123");
        assert_eq!(
            s.crash_image(CrashMode::TornTail { keep_unsynced: 2 })
                .contents(),
            b"0123AB"
        );
    }

    #[test]
    fn lost_writes_keep_length_and_synced_bytes_but_zero_some_sectors() {
        let mut s = SimStorage::new();
        s.append(&[7u8; 8]).unwrap();
        s.sync_data().unwrap();
        s.append(&[9u8; 64]).unwrap();
        let mut zeroed_any = false;
        let mut kept_any = false;
        for seed in 0..16 {
            let img = s
                .crash_image(CrashMode::LostWrites { seed, sector: 8 })
                .contents();
            assert_eq!(img.len(), 72);
            assert_eq!(&img[..8], &[7u8; 8]);
            for chunk in img[8..].chunks(8) {
                assert!(chunk == [0u8; 8] || chunk == [9u8; 8]);
                zeroed_any |= chunk == [0u8; 8];
                kept_any |= chunk == [9u8; 8];
            }
        }
        assert!(zeroed_any && kept_any);
    }

    #[test]
    fn directory_sync_faults() {
        let mut s = SimStorage::with_faults([
            Fault::FailSyncDirectory { index: 0 },
            Fault::UnconfirmedSyncDirectory { index: 1 },
        ]);
        assert!(s.sync_directory().is_err());
        assert!(matches!(
            s.sync_directory().unwrap(),
            DirectoryDurability::Unconfirmed(_)
        ));
        assert_eq!(s.sync_directory().unwrap(), DirectoryDurability::Confirmed);
    }

    #[test]
    fn crash_image_is_healthy_and_fault_free() {
        let mut s = SimStorage::with_faults([Fault::HaltBeforeMutation { mutation_index: 0 }]);
        let _ = s.append(b"x");
        let mut fresh = s.crash_image(CrashMode::KeepAll);
        assert!(!fresh.is_halted());
        assert_eq!(fresh.append(b"ok").unwrap(), 0);
        assert_eq!(fresh.synced_len(), 0);
    }

    #[test]
    fn sync_makes_bytes_durable_and_is_traced() {
        let mut s = SimStorage::new();
        s.append(b"abcd").unwrap();
        assert_eq!(s.synced_len(), 0);
        s.sync_data().unwrap();
        assert_eq!(s.synced_len(), 4);
        assert_eq!(s.sync_directory().unwrap(), DirectoryDurability::Confirmed);
        assert_eq!(
            s.trace(),
            vec![
                Op::Append { offset: 0, len: 4 },
                Op::SyncData,
                Op::SyncDirectory
            ]
        );
    }

    #[test]
    fn failed_sync_reports_error_and_persists_nothing() {
        let mut s = SimStorage::with_faults([Fault::FailSync { sync_index: 0 }]);
        s.append(b"abcd").unwrap();
        let err = s.sync_data().unwrap_err();
        assert!(!is_halted(&err));
        assert_eq!(s.synced_len(), 0);
        s.sync_data().unwrap(); // the next sync works
        assert_eq!(s.synced_len(), 4);
    }

    #[test]
    fn lying_sync_acks_without_persisting() {
        let mut s = SimStorage::with_faults([Fault::LieSync { sync_index: 0 }]);
        s.append(b"abcd").unwrap();
        s.sync_data().unwrap(); // acknowledged...
        assert_eq!(s.synced_len(), 0); // ...but nothing is durable
        assert_eq!(s.crash_image(CrashMode::SyncedOnly).contents(), b"");
    }

    #[test]
    fn second_handle_is_refused_the_lock_until_release() {
        let mut a = SimStorage::new();
        let mut b = a.handle();
        a.try_lock_exclusive().unwrap();
        a.try_lock_exclusive().unwrap(); // re-entrant for the same handle
        assert!(matches!(
            b.try_lock_exclusive(),
            Err(StorageError::LockHeld)
        ));
        a.unlock().unwrap();
        b.try_lock_exclusive().unwrap();
    }

    #[test]
    fn halt_releases_the_lock() {
        let mut a = SimStorage::with_faults([Fault::HaltBeforeMutation { mutation_index: 0 }]);
        let mut b = a.handle();
        a.try_lock_exclusive().unwrap();
        let _ = a.append(b"x");
        b.try_lock_exclusive().unwrap_err(); // b is also halted: same process model
        assert!(a.is_halted());
    }

    #[test]
    fn handles_share_bytes() {
        let mut a = SimStorage::new();
        let b = a.handle();
        a.append(b"shared").unwrap();
        let mut buf = [0u8; 6];
        b.read_exact_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"shared");
    }

    #[test]
    fn reads_are_bounds_checked() {
        let mut s = SimStorage::from_bytes(b"abc".to_vec());
        let mut buf = [0u8; 4];
        assert!(matches!(
            s.read_exact_at(0, &mut buf),
            Err(StorageError::OutOfBounds { .. })
        ));
        assert_eq!(s.read_at(u64::MAX, &mut buf).unwrap(), 0);
        s.truncate(1).unwrap();
        assert_eq!(s.size().unwrap(), 1);
        assert!(s.truncate(2).is_err());
    }

    #[test]
    fn truncate_clamps_durable_length() {
        let mut s = SimStorage::new();
        s.append(b"abcdef").unwrap();
        s.sync_data().unwrap();
        s.truncate(2).unwrap();
        assert_eq!(s.synced_len(), 2);
    }

    #[test]
    fn preloaded_bytes_count_as_durable() {
        let s = SimStorage::from_bytes(b"abc".to_vec());
        assert_eq!(s.synced_len(), 3);
        assert_eq!(s.crash_image(CrashMode::SyncedOnly).contents(), b"abc");
    }
}
