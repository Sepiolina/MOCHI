//! Shared helpers for the replay, recovery, damage, and adoption tests
//! (T11 to T17): a scripted history with per-file promised attributes and an
//! independent model of the state after each commit, writers under each test
//! checkpoint policy, and byte-level damage helpers.
//!
//! The model is plain maps built from the transactions, not from any
//! reconstruction path under test (T12 amendment 4). Test infrastructure:
//! unwraps freely.

use std::cell::RefCell;
use std::collections::BTreeMap;

use mochi_core::catalog::namespace::{FileVersionId, Snapshot};
use mochi_core::commit::{Metadata, ObjectRef};
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{
    commit_history, open_at_footer, read_snapshot, ArchiveWriter, CheckpointPolicy, HistoryEntry,
    OpenedHead, ReadOptions, TailPolicy, Transaction,
};
use mochi_core::storage::{ReadStorage, StorageError};

use crate::archive::{path, test_options, Content, Job, State};
use crate::forge::Forge;
use crate::{deterministic_bytes, SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

// ---- a scripted history with per-file attributes ---------------------------------

/// Attributes that differ per file and per step, so a version credited with
/// another version's attributes is caught.
pub fn attrs(mode: u32, t: i64) -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode,
            uid: 1000 + mode,
            gid: 2000,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 1_700_000_000 + t,
            nanos: (t as u32) * 7,
        }),
    }
}

/// One commit and the model after it: contents, and promised attributes by
/// path. The model is plain maps built from the transactions, not from any
/// reconstruction path under test (amendment 4).
#[derive(Clone)]
pub struct Step {
    pub tx: Transaction,
    pub after: State,
    pub attrs: BTreeMap<Vec<u8>, Attributes>,
}

#[derive(Default)]
pub struct Model {
    pub state: State,
    pub attrs: BTreeMap<Vec<u8>, Attributes>,
    pub steps: Vec<Step>,
    pub tx: Transaction,
}

impl Model {
    pub fn file(&mut self, p: &str, bytes: Vec<u8>, a: Attributes) {
        self.tx.put_file(path(p), bytes.clone(), a);
        self.state.insert(p.into(), Content::File(bytes));
        self.attrs.insert(p.into(), a);
    }
    pub fn dir(&mut self, p: &str, a: Attributes) {
        self.tx.put_dir(path(p), a);
        self.state.insert(p.into(), Content::Dir);
        self.attrs.insert(p.into(), a);
    }
    pub fn delete(&mut self, p: &str) {
        self.tx.delete(path(p));
        self.state.remove(p.as_bytes());
        self.attrs.remove(p.as_bytes());
    }
    /// A rename keeps the version, so it keeps its attributes.
    pub fn rename(&mut self, from: &str, to: &str) {
        self.tx.rename(path(from), path(to));
        let c = self.state.remove(from.as_bytes()).unwrap();
        let a = self.attrs.remove(from.as_bytes()).unwrap();
        self.state.insert(to.into(), c);
        self.attrs.insert(to.into(), a);
    }
    pub fn end(&mut self, t: i64) {
        let mut tx = std::mem::take(&mut self.tx);
        tx.at(Mtime {
            secs: 1_700_000_000 + t,
            nanos: 0,
        });
        self.steps.push(Step {
            tx,
            after: self.state.clone(),
            attrs: self.attrs.clone(),
        });
    }
}

/// Seven commits: multi-chunk files, empty file, replacement, renames
/// (attributes follow the version), deletes, nested directories.
pub fn history() -> Vec<Step> {
    let mut m = Model::default();
    m.dir("d", attrs(0o750, 0));
    m.file("d/a", deterministic_bytes(1, 200), attrs(0o640, 1));
    m.file("b", deterministic_bytes(2, 70), attrs(0o600, 2));
    m.file("c", Vec::new(), attrs(0o444, 3));
    m.end(0);
    m.file("d/a", deterministic_bytes(3, 130), attrs(0o641, 11));
    m.file("e", deterministic_bytes(4, 10), attrs(0o700, 12));
    m.end(10);
    m.rename("b", "d/b");
    m.delete("c");
    m.end(20);
    m.dir("f", attrs(0o711, 31));
    m.file("f/g", deterministic_bytes(5, 300), attrs(0o604, 32));
    m.end(30);
    m.delete("e");
    m.file("f/g", deterministic_bytes(6, 90), attrs(0o606, 41));
    m.end(40);
    m.rename("d/a", "f/a");
    m.end(50);
    m.file("h", deterministic_bytes(7, 65), attrs(0o655, 61));
    m.delete("d/b");
    m.end(60);
    m.steps
}

/// Write `steps` into fresh storage under `policy`, in one session.
pub fn write(policy: CheckpointPolicy, steps: &[Step]) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), test_options()).unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    let job = Job::new();
    for st in steps {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

/// Append `steps` in a new session (fresh ID seed, so IDs never repeat).
pub fn append(s: &SimStorage, seed: u64, policy: CheckpointPolicy, steps: &[Step]) {
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    let job = Job::new();
    for st in steps {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
}

pub fn is_cp(e: &HistoryEntry) -> bool {
    e.commit.metadata.is_checkpoint()
}

/// Path → promised attributes, from the snapshot manifest of checkpoint `h`.
pub fn snapshot_attrs(src: &dyn ReadStorage, h: &OpenedHead) -> BTreeMap<Vec<u8>, Attributes> {
    let snap = read_snapshot(src, h, &opts()).unwrap();
    let by_version: BTreeMap<FileVersionId, Attributes> = snap
        .file_versions
        .iter()
        .map(|v| (v.version.id, v.attributes))
        .collect();
    snap.entries
        .iter()
        .map(|(p, v)| (p.as_stored().to_vec(), by_version[v]))
        .collect()
}

pub fn flip(bytes: &mut [u8], at: u64) {
    bytes[at as usize] ^= 0x40;
}

/// Records every read.
pub struct Tracing<'a> {
    pub inner: &'a SimStorage,
    pub reads: RefCell<Vec<(u64, u64)>>,
}

impl ReadStorage for Tracing<'_> {
    fn size(&self) -> Result<u64, StorageError> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let n = self.inner.read_at(offset, buf)?;
        self.reads.borrow_mut().push((offset, n as u64));
        Ok(n)
    }
}

pub fn within(r: (u64, u64), allowed: &[(u64, u64)]) -> bool {
    allowed.iter().any(|&(lo, hi)| r.0 >= lo && r.0 + r.1 <= hi)
}

/// cp0 d1 (Never), then a checkpoint cp2 whose snapshot manifest omits one
/// reachable file version (its entry, version, and the chunks only it
/// uses). The edited snapshot is structurally valid and bound to commit 2;
/// it just disagrees with the image, which the real writer never produces.
pub fn checkpoint_with_incomplete_snapshot() -> (Forge, HistoryEntry, Vec<u8>) {
    use mochi_core::catalog::extent::ExtentSource;
    let s = write(CheckpointPolicy::Never, &history()[..2]);
    append(
        &s,
        4001,
        CheckpointPolicy::EveryCommit,
        &[Step {
            tx: Transaction::new(),
            after: history()[1].after.clone(),
            attrs: history()[1].attrs.clone(),
        }],
    );
    let real = commit_history(&s, &opts()).unwrap().pop().unwrap();
    assert!(is_cp(&real));
    let opened = open_at_footer(&s, real.footer_offset, &opts()).unwrap();
    let mut snap = read_snapshot(&s, &opened, &opts()).unwrap();

    // Drop the file at "d/a" from the snapshot only.
    let pos = snap
        .entries
        .iter()
        .position(|(p, _)| p.as_stored() == b"d/a")
        .unwrap();
    let (_, vid) = snap.entries.remove(pos);
    let vpos = snap
        .file_versions
        .iter()
        .position(|v| v.version.id == vid)
        .unwrap();
    let dropped = snap.file_versions.remove(vpos);
    let still_used = |id: &_| {
        snap.file_versions.iter().any(|v| {
            v.extents
                .iter()
                .any(|e| matches!(e.source, ExtentSource::Chunk { chunk, .. } if chunk == *id))
        })
    };
    let gone: Vec<_> = dropped
        .extents
        .iter()
        .filter_map(|e| match e.source {
            ExtentSource::Chunk { chunk, .. } if !still_used(&chunk) => Some(chunk),
            _ => None,
        })
        .collect();
    snap.chunks.retain(|c| !gone.contains(&c.record.id));

    let mut f = Forge::new(s.contents());
    f.bytes.truncate(real.commit_offset as usize);
    let snap_ref = f.append_manifest(&snap);
    let mut rec = real.commit.clone();
    let Metadata::Checkpoint { image, .. } = rec.metadata else {
        unreachable!()
    };
    rec.metadata = Metadata::Checkpoint {
        image,
        snapshot: snap_ref,
    };
    let cp2 = f.append_commit(&rec);
    (f, cp2, b"d/a".to_vec())
}

pub fn open_append_err(s: &SimStorage) -> mochi_core::MochiError {
    ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(4100)),
        test_options(),
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .expect_err("append must be refused")
}

/// `history()` followed by two more steps: nine commits, 0..=8. `history()`
/// itself is unchanged because other tests slice it.
pub fn history_long() -> Vec<Step> {
    let mut m = model_of(history());
    m.file("i", deterministic_bytes(8, 40), attrs(0o620, 71));
    m.end(70);
    m.rename("h", "d/h");
    m.dir("j", attrs(0o755, 81));
    m.end(80);
    m.steps
}

/// `history_long()` plus one more step: ten commits, 0..=9.
pub fn history_10() -> Vec<Step> {
    let mut m = model_of(history_long());
    m.file("j/k", deterministic_bytes(9, 33), attrs(0o601, 91));
    m.end(90);
    m.steps
}

/// A model that continues after `steps` (its state and attributes are those
/// after the last step).
fn model_of(steps: Vec<Step>) -> Model {
    let last = steps.last().cloned();
    Model {
        state: last.as_ref().map(|s| s.after.clone()).unwrap_or_default(),
        attrs: last.as_ref().map(|s| s.attrs.clone()).unwrap_or_default(),
        steps,
        tx: Transaction::new(),
    }
}

/// Zero the payload of the skippable frame `r`, keeping its 8-byte header so
/// the file still walks: a deleted object.
pub fn wipe(bytes: &mut [u8], r: ObjectRef) {
    let start = r.offset as usize + mochi_format::registry::SKIPPABLE_HEADER_LEN;
    let end = (r.offset + r.stored_len) as usize;
    bytes[start..end].fill(0);
}

/// Flip one byte in the middle of `r`: a damaged object.
pub fn damage(bytes: &mut [u8], r: ObjectRef) {
    flip(bytes, r.offset + r.stored_len / 2);
}

/// Half-open byte range of `r`.
pub fn range(r: ObjectRef) -> (u64, u64) {
    (r.offset, r.offset + r.stored_len)
}

/// Image and snapshot references of a checkpoint entry. Panics on a delta.
pub fn checkpoint_refs(e: &HistoryEntry) -> (ObjectRef, ObjectRef) {
    match e.commit.metadata {
        Metadata::Checkpoint { image, snapshot } => (image, snapshot),
        Metadata::Delta { .. } => panic!("commit {} is not a checkpoint", e.commit.seq),
    }
}

/// Path to promised attributes at a head, given its namespace and the
/// version-keyed attribute map.
pub fn attrs_by_path(
    ns: &Snapshot,
    a: &BTreeMap<FileVersionId, Attributes>,
) -> BTreeMap<Vec<u8>, Attributes> {
    ns.iter()
        .map(|(p, e)| (p.as_stored().to_vec(), a[&e.version]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histories_have_the_documented_lengths_and_extend_each_other() {
        let (h, hl, h10) = (history(), history_long(), history_10());
        assert_eq!((h.len(), hl.len(), h10.len()), (7, 9, 10));
        for (a, b) in h.iter().zip(&hl) {
            assert_eq!(a.after, b.after);
        }
        for (a, b) in hl.iter().zip(&h10) {
            assert_eq!(a.after, b.after);
        }
    }

    #[test]
    fn long_histories_open_to_their_model_under_every_commit_policy() {
        let steps = history_10();
        let s = write(CheckpointPolicy::Every(3), &steps);
        let hist = commit_history(&s, &opts()).unwrap();
        assert_eq!(hist.len(), 10);
        for (e, st) in hist.iter().zip(&steps) {
            let o = open_at_footer(&s, e.footer_offset, &opts()).unwrap();
            assert_eq!(crate::archive::read_state(&s, &o).unwrap(), st.after);
        }
    }
}
