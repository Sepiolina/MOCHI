//! T12 oracle property test (review amendment 4, `docs/t11-t12-acceptance.md`).
//!
//! The same seeded `SeqIds` stream and the same generated transactions are
//! written under `EveryCommit` (archive A) and under `Never` / `Every(n)`,
//! n ∈ {2, 3, 7} (archive B). B is reopened, continuing the same ID
//! stream, only when the split falls strictly inside the history; the
//! proptest includes single-commit and end-split histories, which are one
//! session. Two-session coverage for all four policies is guaranteed by
//! `fixed_history_under_each_amendment_4_policy` (reopen at sequence 4: a
//! delta head under `Never`, `Every(3)`, `Every(7)`; a checkpoint head
//! under `Every(2)`). For every sequence *c*:
//!
//! * **Logical state equal.** Both catalogs' logical dumps, with exactly one
//!   field normalized: `object_locations.stored_offset`, the only
//!   layout-dependent column in the catalog DDL. That covers the namespace
//!   operations, file versions and extents, object identities and semantic
//!   metadata (encoding, protection, lengths, content and stored hashes),
//!   dependencies, and archive metadata. Promised attributes are compared
//!   separately (below), because the catalog holds none until C6.
//! * **Physical locations validated in each archive independently.** The
//!   location lies after the descriptor and before commit *c*'s frame, and
//!   (checked with `mochi-format`'s frame walker and digest, not core's
//!   loading path) exactly one Zstandard data frame of the recorded stored
//!   length is there and its stored-object hash matches the record. It
//!   must also decode to the recorded content hash; that check uses core's
//!   `decode_verified`, so decoding is shared code, not independent.
//! * **IDs.** Object, file-version, and transaction IDs (and the archive ID)
//!   match across A and B; commit IDs are not compared (they cover
//!   layout-dependent references).
//! * **Expected values from the transaction model.** Namespace, file
//!   contents, and promised attributes at *c* are checked against plain
//!   maps built from the generated transactions, for A and B alike.
//!
//! # Independence
//!
//! Archive A avoids **delta-segment replay**: it is opened only through
//! checkpoint images, so no segment walk, no `SegmentApplier`, and no
//! attribute reconstruction is involved on A's side. A does **not** avoid
//! `Catalog::replay` (namespace from stored operations): both sides use it,
//! including in `read_state`. Also shared: the SQLite row writers
//! (`insert_*_rows`, used by the writer and the applier) and
//! `decode_verified`. A defect in shared code could hide from the A-vs-B
//! comparison; the model check (plain maps from the transactions, no
//! catalog code) runs on both sides for namespace, contents, and
//! attributes, but reaches the namespace through `Catalog::replay`, so it
//! does not independently cover that function. The row writers and
//! `Catalog::replay` keep their own C3 tests.
//!
//! Attributes for B at a *delta* commit have no snapshot; they are read by
//! reopening B truncated to commit *c* and committing an empty transaction
//! as a checkpoint, whose snapshot must equal the model at *c*. That
//! exercises decision 21's reconstruction and checks it against the model,
//! never against another output of the code under test.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use mochi_core::catalog::Catalog;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::object::{decode_verified, IdSource};
use mochi_core::publish::{
    commit_history, open_at_footer, read_snapshot, ArchiveWriter, CheckpointPolicy, HistoryEntry,
    OpenedHead, ReadOptions, TailPolicy, Transaction,
};
use mochi_core::storage::ReadStorage;
use mochi_format::digest::stored_object_hash;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_format::frame::walk_frame;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;
use mochi_testkit::archive::{path, read_state, test_options, Content, Job, State};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};
use proptest::prelude::*;

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// One ID stream shared by every session of one archive, so a reopen
/// continues it exactly where the previous session stopped.
#[derive(Clone)]
struct SharedIds(Rc<RefCell<SeqIds>>);

impl SharedIds {
    fn new(seed: u64) -> Self {
        SharedIds(Rc::new(RefCell::new(SeqIds::new(seed))))
    }
}

impl IdSource for SharedIds {
    fn next_id(&mut self) -> mochi_core::Result<[u8; 32]> {
        self.0.borrow_mut().next_id()
    }
}

// ---- generated transactions and their model -------------------------------------

/// Files may live at the top level or in one of two directories; `d` and
/// `e` are only ever directories.
const FILES: [&str; 6] = ["a", "b", "c", "d/x", "d/y", "e/z"];
const DIRS: [&str; 2] = ["d", "e"];

/// A raw operation; interpreted against the model, invalid ones skipped.
type RawOp = (u8, u8, u8, u8, usize, u8);

/// Operations `interpret` accepted into a transaction (skipped raw choices
/// are not counted).
#[derive(Debug, Default, Clone, Copy)]
struct Accepted {
    new_file: u32,
    replace_file: u32,
    put_dir: u32,
    delete_file: u32,
    delete_dir: u32,
    rename: u32,
}

#[derive(Clone)]
struct Step {
    tx: Transaction,
    after: State,
    attrs: BTreeMap<Vec<u8>, Attributes>,
}

fn parent(p: &str) -> Option<&str> {
    p.rsplit_once('/').map(|(d, _)| d)
}

fn attrs(mode: u32, t: i64) -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode,
            uid: 1000,
            gid: 1000 + mode,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 1_700_000_000 + t,
            nanos: (t as u32 % 1000) * 3,
        }),
    }
}

/// Turn raw choices into transactions that are valid in sequence: each
/// path is touched at most once per transaction; a file's parent directory
/// exists; a directory is deleted only when empty; a rename moves a file
/// that exists at the start of the transaction to a free path. Sequential
/// validity implies validity in the completed state (§10.2).
fn interpret(raw: &[Vec<RawOp>]) -> (Vec<Step>, Accepted) {
    let mut n = Accepted::default();
    let mut state = State::new();
    let mut amap: BTreeMap<Vec<u8>, Attributes> = BTreeMap::new();
    let mut steps = Vec::new();
    for (c, ops) in raw.iter().enumerate() {
        let start = state.clone();
        let mut touched: Vec<String> = Vec::new();
        let mut tx = Transaction::new();
        tx.at(Mtime {
            secs: 1_700_000_000 + c as i64 * 100,
            nanos: 0,
        });
        let has = |s: &State, p: &str| s.contains_key(p.as_bytes());
        let is_dir = |s: &State, p: &str| matches!(s.get(p.as_bytes()), Some(Content::Dir));
        for (i, &(kind, p1, p2, seed, len, mode)) in ops.iter().enumerate() {
            let t = (c * 10 + i) as i64;
            let a = attrs(0o600 + u32::from(mode % 64), t);
            match kind % 4 {
                0 => {
                    let p = FILES[p1 as usize % FILES.len()];
                    let parent_ok = parent(p).is_none_or(|d| is_dir(&state, d));
                    if touched.iter().any(|x| x == p) || !parent_ok {
                        continue;
                    }
                    if has(&state, p) {
                        n.replace_file += 1;
                    } else {
                        n.new_file += 1;
                    }
                    let bytes = deterministic_bytes(u64::from(seed) + 1000 * c as u64, len);
                    tx.put_file(path(p), bytes.clone(), a);
                    state.insert(p.into(), Content::File(bytes));
                    amap.insert(p.into(), a);
                    touched.push(p.into());
                }
                1 => {
                    let p = DIRS[p1 as usize % DIRS.len()];
                    if touched.iter().any(|x| x == p) || has(&state, p) {
                        continue;
                    }
                    n.put_dir += 1;
                    tx.put_dir(path(p), a);
                    state.insert(p.into(), Content::Dir);
                    amap.insert(p.into(), a);
                    touched.push(p.into());
                }
                2 => {
                    // Pick among paths that can be deleted now.
                    let cands: Vec<&str> = FILES
                        .iter()
                        .chain(DIRS.iter())
                        .copied()
                        .filter(|p| {
                            has(&state, p)
                                && !touched.iter().any(|x| x == p)
                                && !state
                                    .keys()
                                    .any(|k| k.starts_with(format!("{p}/").as_bytes()))
                        })
                        .collect();
                    let Some(&p) = cands.get(p1 as usize % cands.len().max(1)) else {
                        continue;
                    };
                    if DIRS.contains(&p) {
                        n.delete_dir += 1;
                    } else {
                        n.delete_file += 1;
                    }
                    tx.delete(path(p));
                    state.remove(p.as_bytes());
                    amap.remove(p.as_bytes());
                    touched.push(p.into());
                }
                _ => {
                    // A file present at the start of the transaction and not
                    // touched since, to a free file path whose parent exists.
                    let froms: Vec<&str> = FILES
                        .iter()
                        .copied()
                        .filter(|p| {
                            start.contains_key(p.as_bytes())
                                && has(&state, p)
                                && !touched.iter().any(|x| x == p)
                        })
                        .collect();
                    let tos: Vec<&str> = FILES
                        .iter()
                        .copied()
                        .filter(|p| {
                            !has(&state, p)
                                && !touched.iter().any(|x| x == p)
                                && parent(p).is_none_or(|d| is_dir(&state, d))
                        })
                        .collect();
                    let (Some(&from), Some(&to)) = (
                        froms.get(p1 as usize % froms.len().max(1)),
                        tos.get(p2 as usize % tos.len().max(1)),
                    ) else {
                        continue;
                    };
                    n.rename += 1;
                    tx.rename(path(from), path(to));
                    let content = state.remove(from.as_bytes()).unwrap();
                    let at = amap.remove(from.as_bytes()).unwrap();
                    state.insert(to.into(), content);
                    amap.insert(to.into(), at);
                    touched.push(from.into());
                    touched.push(to.into());
                }
            }
        }
        steps.push(Step {
            tx,
            after: state.clone(),
            attrs: amap.clone(),
        });
    }
    (steps, n)
}

// ---- writing ---------------------------------------------------------------------

/// Write `steps` with one ID stream: session 1 is `steps[..split]` under
/// `policy`, session 2 (a reopen) the rest under the same policy.
fn write(policy: CheckpointPolicy, seed: u64, steps: &[Step], split: usize) -> SimStorage {
    let s = SimStorage::new();
    let ids = SharedIds::new(seed);
    let job = Job::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(ids.clone()), test_options()).unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    for st in &steps[..split] {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    if split < steps.len() {
        let (mut w, _) = ArchiveWriter::open_append(
            s.clone(),
            Box::new(ids),
            test_options(),
            TailPolicy::Refuse,
        )
        .unwrap();
        w.set_checkpoint_policy(policy).unwrap();
        for st in &steps[split..] {
            w.commit(st.tx.clone(), &job.ctx()).unwrap();
        }
        w.close().unwrap();
    }
    s
}

// ---- comparison ------------------------------------------------------------------

/// The catalog's logical dump with exactly one normalization: the stored
/// offset of each object location is dropped (the row, and so the object
/// ID it locates, is kept). Every other column is compared as stored.
fn normalized_dump(cat: &Catalog) -> Vec<String> {
    cat.logical_dump()
        .unwrap()
        .into_iter()
        .map(|line| match line.strip_prefix("object_locations:") {
            Some(rest) => {
                let cut = rest.rfind(" Integer(").expect("stored_offset column");
                format!("object_locations:{} <offset>", &rest[..cut])
            }
            None => line,
        })
        .collect()
}

/// Amendment 4 physical validation, for one archive at commit `e`.
fn validate_locations(src: &SimStorage, head: &OpenedHead, e: &HistoryEntry, who: &str) {
    let limits = opts().limits;
    let d = head.commit.descriptor;
    let lo = d.offset + d.stored_len;
    let hi = e.commit_offset;
    let cat = &head.catalog;
    for id in cat.object_ids().unwrap() {
        let record = cat.object(&id).unwrap().unwrap();
        let at = cat
            .object_location(&id)
            .unwrap()
            .unwrap_or_else(|| panic!("{who}: object {} has no location", id.to_hex()));
        let end = at.checked_add(record.stored_len).unwrap();
        assert!(
            at >= lo && end <= hi,
            "{who}: object {} at {at}..{end} is outside {lo}..{hi}",
            id.to_hex()
        );
        let mut buf = vec![0u8; record.stored_len as usize];
        src.read_exact_at(at, &mut buf).unwrap();
        let span = walk_frame(&buf, 0, &limits).unwrap();
        assert_eq!(span.kind, FrameKind::ZstdData, "{who}: frame kind at {at}");
        assert_eq!(span.len, record.stored_len, "{who}: frame length at {at}");
        let stored = StoredObject::from_loaded(buf);
        assert_eq!(
            stored_object_hash(stored.view()),
            record.stored_hash,
            "{who}: stored hash at {at}"
        );
        decode_verified(&record, &stored, &limits)
            .unwrap_or_else(|err| panic!("{who}: object at {at} does not decode: {err}"));
    }
}

/// Path → promised attributes from checkpoint `h`'s snapshot.
fn snapshot_attrs(src: &dyn ReadStorage, h: &OpenedHead) -> BTreeMap<Vec<u8>, Attributes> {
    let snap = read_snapshot(src, h, &opts()).unwrap();
    let by_version: BTreeMap<_, _> = snap
        .file_versions
        .iter()
        .map(|v| (v.version.id, v.attributes))
        .collect();
    snap.entries
        .iter()
        .map(|(p, v)| (p.as_stored().to_vec(), by_version[v]))
        .collect()
}

/// Promised attributes of `src` at commit `e`: from its snapshot if it is a
/// checkpoint; else by reopening the archive truncated to `e` and
/// committing an empty transaction as a checkpoint (decision 21 path).
fn attrs_at(
    src: &SimStorage,
    e: &HistoryEntry,
    opened: &OpenedHead,
) -> BTreeMap<Vec<u8>, Attributes> {
    if e.commit.metadata.is_checkpoint() {
        return snapshot_attrs(src, opened);
    }
    let end = (e.footer_offset + FOOTER_FRAME_LEN) as usize;
    let cut = SimStorage::from_bytes(src.contents()[..end].to_vec());
    let (mut w, _) = ArchiveWriter::open_append(
        cut.clone(),
        Box::new(SeqIds::new(0xA77)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    assert_eq!(w.checkpoint_policy(), CheckpointPolicy::EveryCommit);
    w.commit(Transaction::new(), &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let probe = open_at_footer(
        &cut,
        commit_history(&cut, &opts())
            .unwrap()
            .last()
            .unwrap()
            .footer_offset,
        &opts(),
    )
    .unwrap();
    assert!(probe.commit.metadata.is_checkpoint());
    snapshot_attrs(&cut, &probe)
}

fn check(steps: &[Step], policy: CheckpointPolicy, split: usize) -> Vec<HistoryEntry> {
    const SEED: u64 = 42;
    let a = write(CheckpointPolicy::EveryCommit, SEED, steps, steps.len());
    let b = write(policy, SEED, steps, split);
    let ha = commit_history(&a, &opts()).unwrap();
    let hb = commit_history(&b, &opts()).unwrap();
    assert_eq!(ha.len(), steps.len());
    assert_eq!(hb.len(), steps.len());
    assert!(ha.iter().all(|e| e.commit.metadata.is_checkpoint()));

    for (c, ((ea, eb), step)) in ha.iter().zip(&hb).zip(steps).enumerate() {
        let ctx = format!("{policy:?}, split {split}, commit {c}");
        let oa = open_at_footer(&a, ea.footer_offset, &opts()).unwrap();
        let ob = open_at_footer(&b, eb.footer_offset, &opts()).unwrap();

        // IDs that are meant to be policy-independent.
        assert_eq!(ea.commit.archive_id, eb.commit.archive_id, "{ctx}");
        assert_eq!(ea.commit.transaction_id, eb.commit.transaction_id, "{ctx}");
        assert_eq!(
            oa.catalog.object_ids().unwrap(),
            ob.catalog.object_ids().unwrap(),
            "{ctx}"
        );

        // Logical state, one normalization.
        assert_eq!(
            normalized_dump(&oa.catalog),
            normalized_dump(&ob.catalog),
            "{ctx}"
        );

        // Against the transaction model, each side on its own.
        assert_eq!(
            read_state(&a, &oa).unwrap(),
            step.after,
            "{ctx}: A vs model"
        );
        assert_eq!(
            read_state(&b, &ob).unwrap(),
            step.after,
            "{ctx}: B vs model"
        );
        assert_eq!(
            snapshot_attrs(&a, &oa),
            step.attrs,
            "{ctx}: A attributes vs model"
        );
        assert_eq!(
            attrs_at(&b, eb, &ob),
            step.attrs,
            "{ctx}: B attributes vs model"
        );

        // Verification audit (docs/t12-verify-audit.md), empirical side:
        // the full `Catalog::verify` passes on every replayed catalog. Test
        // evidence only; production does not run it after replay.
        ob.catalog
            .verify()
            .unwrap_or_else(|e| panic!("{ctx}: B fails verify(): {e}"));

        // Physical locations, each archive independently.
        validate_locations(&a, &oa, ea, &format!("{ctx}: A"));
        validate_locations(&b, &ob, eb, &format!("{ctx}: B"));
    }
    hb
}

fn raw_op() -> impl Strategy<Value = RawOp> {
    (
        0u8..4,
        any::<u8>(),
        any::<u8>(),
        any::<u8>(),
        0usize..160,
        any::<u8>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Amendment 4: B under `Never` and `Every(n)`, n ∈ {2, 3, 7}, equals A
    /// under `EveryCommit` at every sequence, and both equal the model. B
    /// is reopened only when `split` is interior (after `min` with the
    /// history length); otherwise it is written in one session.
    #[test]
    fn replayed_state_equals_the_full_checkpoint_oracle(
        raw in prop::collection::vec(prop::collection::vec(raw_op(), 0..6), 1..=9),
        which in 0usize..4,
        split in 1usize..=9,
    ) {
        let (steps, _) = interpret(&raw);
        let policy = [
            CheckpointPolicy::Never,
            CheckpointPolicy::Every(2),
            CheckpointPolicy::Every(3),
            CheckpointPolicy::Every(7),
        ][which];
        check(&steps, policy, split.min(steps.len()));
    }
}

/// The four policies of amendment 4 on one fixed, busy history, always
/// run, with B written in two sessions: the first ends at sequence 4, so
/// the reopen happens on a delta head under `Never`, `Every(3)`, and
/// `Every(7)`, and on a checkpoint head under `Every(2)`. This, not the
/// proptest, is what guarantees two-session coverage for every policy.
#[test]
fn fixed_history_under_each_amendment_4_policy() {
    let raw: Vec<Vec<RawOp>> = (0u8..9)
        .map(|c| {
            (0u8..5)
                .map(|i| {
                    (
                        c.wrapping_mul(7).wrapping_add(i * 3),
                        c ^ i,
                        i.wrapping_mul(5) ^ c,
                        c,
                        (c as usize * 37 + i as usize * 11) % 160,
                        c + i,
                    )
                })
                .collect()
        })
        .collect();
    let (steps, n) = interpret(&raw);
    // Coverage of the operations actually accepted into transactions.
    eprintln!("fixed history accepted operations: {n:?}");
    assert!(n.put_dir > 0, "no directory creation: {n:?}");
    assert!(n.rename > 0, "no rename: {n:?}");
    assert!(n.delete_file > 0, "no file delete: {n:?}");
    assert!(n.delete_dir > 0, "no directory delete: {n:?}");
    const SPLIT: usize = 5;
    assert!(steps.len() > SPLIT, "the reopen must be interior");
    for (policy, boundary_is_checkpoint) in [
        (CheckpointPolicy::Never, false),
        (CheckpointPolicy::Every(2), true),
        (CheckpointPolicy::Every(3), false),
        (CheckpointPolicy::Every(7), false),
    ] {
        let hb = check(&steps, policy, SPLIT);
        let boundary = &hb[SPLIT - 1];
        assert_eq!(boundary.commit.seq, 4, "{policy:?}: the reopened head");
        assert_eq!(
            boundary.commit.metadata.is_checkpoint(),
            boundary_is_checkpoint,
            "{policy:?}: kind of the reopened head"
        );
    }
}
