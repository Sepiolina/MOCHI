//! C3 exit criteria (plan §5 C3, §8.1 "Catalog & namespace"):
//!
//! * namespace replay is deterministic: random operation sequences give the
//!   same accept/reject decisions and the same snapshots as an independent
//!   reference model, incrementally, after full replay, at every earlier
//!   commit, and after publishing and reopening the image;
//! * extent validation accepts every valid list and rejects each of the four
//!   §10.3 defect classes;
//! * published images are self-contained: they open from bytes stored through
//!   `Storage`, and from a real file with no WAL or journal beside it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use mochi_core::catalog::extent::{validate_extents, Extent, ExtentDefect, ExtentSource};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp, Snapshot};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::catalog::{Catalog, CatalogLimits, Commit, FileVersion};
use mochi_core::object::ObjectId;
use mochi_core::storage::{ReadStorage, Storage};
use mochi_core::ErrorCode;
use mochi_format::digest::FileContentHash;
use mochi_testkit::SimStorage;
use proptest::prelude::*;

// ---- reference model ----------------------------------------------------------
//
// Written independently of `Snapshot`: string paths, full re-validation of
// every entry after every commit, clone-and-discard on failure.

type Model = BTreeMap<String, bool>; // path -> is_directory

fn model_apply(m: &Model, ops: &[(bool, String, bool)]) -> Option<Model> {
    // op: (is_put, path, is_directory)
    let mut next = m.clone();
    for (is_put, path, is_dir) in ops {
        if *is_put {
            next.insert(path.clone(), *is_dir);
        } else if next.remove(path).is_none() {
            return None;
        }
    }
    for path in next.keys() {
        if let Some((parent, _)) = path.rsplit_once('/') {
            if next.get(parent) != Some(&true) {
                return None;
            }
        }
    }
    Some(next)
}

/// Name of the defect class, for assertions and failure messages.
fn class(r: Result<(), ExtentDefect>) -> &'static str {
    match r {
        Ok(()) => "ok",
        Err(ExtentDefect::Gap { .. }) => "gap",
        Err(ExtentDefect::Overlap { .. }) => "overlap",
        Err(ExtentDefect::OutOfRange { .. }) => "out-of-range",
        Err(ExtentDefect::LengthMismatch { .. }) => "length-mismatch",
        Err(_) => "other",
    }
}

const FILE: FileVersionId = FileVersionId::from_bytes([0xF1; 32]);
const DIR: FileVersionId = FileVersionId::from_bytes([0xD1; 32]);

fn to_ops(ops: &[(bool, String, bool)]) -> Vec<NamespaceOp> {
    ops.iter()
        .map(|(is_put, path, is_dir)| {
            let path = ArchivePath::from_stored(path.as_bytes()).unwrap();
            if *is_put {
                NamespaceOp::Put {
                    path,
                    version: if *is_dir { DIR } else { FILE },
                }
            } else {
                NamespaceOp::Delete { path }
            }
        })
        .collect()
}

fn snapshot_as_model(s: &Snapshot) -> Model {
    s.iter()
        .map(|(p, e)| {
            (
                String::from_utf8(p.as_stored().to_vec()).unwrap(),
                e.kind == EntryKind::Directory,
            )
        })
        .collect()
}

fn kind_of(v: &FileVersionId) -> Option<EntryKind> {
    if *v == FILE {
        Some(EntryKind::File)
    } else if *v == DIR {
        Some(EntryKind::Directory)
    } else {
        None
    }
}

/// Small path universe (depth ≤ 3, names a/b/c, plus names that share a
/// prefix with a sibling) so nesting, conflicts, and orphans are common.
fn path() -> impl Strategy<Value = String> {
    let name = prop_oneof![Just("a"), Just("b"), Just("c"), Just("a.x")];
    proptest::collection::vec(name, 1..=3).prop_map(|v| v.join("/"))
}

/// What a commit does, interpreted against the model's state when the commit
/// is built. Purely random operations are mostly rejected for trivial
/// reasons (deleting absent paths), so they alone never reach the cases that
/// matter; these actions reach them on purpose.
#[derive(Debug, Clone)]
enum Action {
    /// Any single operation, valid or not.
    Raw(bool, String, bool),
    /// Create `path`, with every missing ancestor as a directory.
    PutTree(String, bool),
    /// Delete one existing entry. A directory with children makes the commit
    /// invalid (non-recursive delete, §10.2).
    DeleteExisting(prop::sample::Index),
    /// Delete an existing entry and everything under it, parent first: valid
    /// because validity is judged on the completed state.
    DeleteSubtree(prop::sample::Index),
    /// Replace an existing entry with a file: invalid if it had children.
    ReplaceWithFile(prop::sample::Index),
}

fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        2 => (any::<bool>(), path(), any::<bool>()).prop_map(|(a, p, d)| Action::Raw(a, p, d)),
        4 => (path(), any::<bool>()).prop_map(|(p, d)| Action::PutTree(p, d)),
        2 => any::<prop::sample::Index>().prop_map(Action::DeleteExisting),
        2 => any::<prop::sample::Index>().prop_map(Action::DeleteSubtree),
        1 => any::<prop::sample::Index>().prop_map(Action::ReplaceWithFile),
    ]
}

fn expand(model: &Model, actions: &[Action]) -> Vec<(bool, String, bool)> {
    let existing: Vec<&String> = model.keys().collect();
    let pick = |i: &prop::sample::Index| {
        (!existing.is_empty()).then(|| existing[i.index(existing.len())].clone())
    };
    let mut ops = Vec::new();
    for a in actions {
        match a {
            Action::Raw(put, p, dir) => ops.push((*put, p.clone(), *dir)),
            Action::PutTree(p, leaf_dir) => {
                let parts: Vec<&str> = p.split('/').collect();
                for depth in 1..parts.len() {
                    let prefix = parts[..depth].join("/");
                    if model.get(&prefix) != Some(&true) {
                        ops.push((true, prefix, true));
                    }
                }
                ops.push((true, p.clone(), *leaf_dir));
            }
            Action::DeleteExisting(i) => {
                if let Some(p) = pick(i) {
                    ops.push((false, p, false));
                }
            }
            Action::DeleteSubtree(i) => {
                if let Some(p) = pick(i) {
                    let prefix = format!("{p}/");
                    ops.push((false, p.clone(), false));
                    for q in model.keys().filter(|q| q.starts_with(&prefix)) {
                        ops.push((false, q.clone(), false));
                    }
                }
            }
            Action::ReplaceWithFile(i) => {
                if let Some(p) = pick(i) {
                    ops.push((true, p, false));
                }
            }
        }
    }
    ops
}

fn commits() -> impl Strategy<Value = Vec<Vec<Action>>> {
    proptest::collection::vec(proptest::collection::vec(action(), 0..4), 1..30)
}

fn working_catalog() -> Catalog {
    let mut c = Catalog::new_working().unwrap();
    c.insert_file_version(
        &FileVersion {
            id: FILE,
            kind: EntryKind::File,
            logical_len: 0,
            content_hash: Some(FileContentHash::from_bytes([0; 32])),
        },
        &[],
    )
    .unwrap();
    c.insert_file_version(
        &FileVersion {
            id: DIR,
            kind: EntryKind::Directory,
            logical_len: 0,
            content_hash: None,
        },
        &[],
    )
    .unwrap();
    c
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn replay_matches_the_reference_model_everywhere(commits in commits()) {
        let mut model: Model = Model::new();
        let mut snapshot = Snapshot::new();
        let mut catalog = working_catalog();
        let mut accepted_states: Vec<(u64, Model)> = Vec::new();
        let mut seq = 0u64;

        for actions in &commits {
            let ops = &expand(&model, actions);
            let want = model_apply(&model, ops);
            let got = snapshot.apply_commit(&to_ops(ops), kind_of);

            // Same decision as the model; on rejection, state unchanged.
            prop_assert_eq!(got.is_ok(), want.is_some(), "ops {:?}", ops);
            if let Some(next) = want {
                model = next;
                // The catalog makes the same decision and records the commit.
                let parent = accepted_states.last().map(|(s, _)| *s);
                let c = Commit { seq, parent, ops: to_ops(ops) };
                let from_catalog = catalog.append_commit(&c).unwrap();
                prop_assert_eq!(snapshot_as_model(&from_catalog), model.clone());
                accepted_states.push((seq, model.clone()));
                seq += 1;
            } else {
                let parent = accepted_states.last().map(|(s, _)| *s);
                let c = Commit { seq, parent, ops: to_ops(ops) };
                prop_assert_eq!(catalog.append_commit(&c).unwrap_err().code, ErrorCode::NamespaceInvalid);
            }
            prop_assert_eq!(snapshot_as_model(&snapshot), model.clone());
            // The incremental check never admits what a full check rejects.
            prop_assert!(snapshot.validate_all().is_ok());
        }

        // Full replay, every earlier commit, and after publish + reopen.
        prop_assert_eq!(snapshot_as_model(&catalog.replay(None).unwrap()), model.clone());
        let image = catalog.publish().unwrap();
        let reopened = Catalog::open_image(image.as_bytes(), &CatalogLimits::default()).unwrap();
        for (s, state) in &accepted_states {
            prop_assert_eq!(&snapshot_as_model(&catalog.replay(Some(*s)).unwrap()), state);
            prop_assert_eq!(&snapshot_as_model(&reopened.replay(Some(*s)).unwrap()), state);
        }
    }

    #[test]
    fn valid_extent_lists_are_accepted_and_each_defect_class_is_rejected(
        parts in proptest::collection::vec((1u64..5000, any::<bool>(), 0u64..1000), 1..12),
        which in 0usize..12,
        delta in 1u64..50,
    ) {
        const CHUNK_LEN: u64 = 10_000;
        let chunk = ObjectId::from_bytes([7; 32]);
        let lens = |c: &ObjectId| (*c == chunk).then_some(CHUNK_LEN);

        // A valid partition of [0, total): chunk ranges that fit, and holes.
        let mut extents = Vec::new();
        let mut off = 0u64;
        for (i, (len, is_hole, slack)) in parts.iter().enumerate() {
            let source = if *is_hole {
                ExtentSource::Hole
            } else {
                ExtentSource::Chunk { chunk, chunk_offset: (*slack).min(CHUNK_LEN - len) }
            };
            extents.push(Extent { ordinal: i as u32, logical_offset: off, length: *len, source });
            off += len;
        }
        let total = off;
        prop_assert_eq!(validate_extents(total, &extents, lens), Ok(()));

        let i = which % extents.len();

        // Gap: shift an extent (and those after it) forward.
        let mut gap = extents.clone();
        for e in gap.iter_mut().skip(i) { e.logical_offset += delta; }
        prop_assert_eq!(class(validate_extents(total + delta, &gap, lens)), "gap");

        // Overlap: pull a non-first extent back.
        if i > 0 {
            let mut overlap = extents.clone();
            let d = delta.min(overlap[i - 1].length);
            for e in overlap.iter_mut().skip(i) { e.logical_offset -= d; }
            prop_assert_eq!(class(validate_extents(total - d, &overlap, lens)), "overlap");
        }

        // Out-of-range: push a chunk-backed read past the chunk's end.
        let mut oor = extents.clone();
        let len_i = oor[i].length;
        if let ExtentSource::Chunk { chunk_offset, .. } = &mut oor[i].source {
            *chunk_offset = CHUNK_LEN - len_i + delta;
            prop_assert_eq!(class(validate_extents(total, &oor, lens)), "out-of-range");
        }

        // Length mismatch: the file claims more or less than is covered.
        prop_assert_eq!(class(validate_extents(total + delta, &extents, lens)), "length-mismatch");
        if total > delta {
            prop_assert_eq!(class(validate_extents(total - delta, &extents, lens)), "length-mismatch");
        }
    }
}

// ---- published images are self-contained ------------------------------------------

fn sample_catalog() -> Catalog {
    let mut c = working_catalog();
    let p = |s: &str| ArchivePath::from_stored(s.as_bytes()).unwrap();
    c.append_commit(&Commit {
        seq: 0,
        parent: None,
        ops: vec![
            NamespaceOp::Put {
                path: p("docs"),
                version: DIR,
            },
            NamespaceOp::Put {
                path: p("docs/readme"),
                version: FILE,
            },
        ],
    })
    .unwrap();
    c
}

#[test]
fn published_image_round_trips_through_storage() {
    let catalog = sample_catalog();
    let image = catalog.publish().unwrap();
    let mut storage = SimStorage::new();
    storage.append(b"unrelated leading bytes").unwrap();
    let off = storage.append(image.as_bytes()).unwrap();
    let mut back = vec![0u8; image.as_bytes().len()];
    storage.read_exact_at(off, &mut back).unwrap();
    let opened = Catalog::open_image(&back, &CatalogLimits::default()).unwrap();
    assert_eq!(opened.replay(None).unwrap(), catalog.replay(None).unwrap());
}

#[test]
fn published_image_needs_no_wal_or_journal_file() {
    let image = sample_catalog().publish().unwrap();
    let bytes = image.as_bytes();
    // Header: legacy (rollback-journal) format, not WAL; 4096-byte pages.
    assert_eq!((bytes[18], bytes[19]), (1, 1));
    assert_eq!(u16::from_be_bytes([bytes[16], bytes[17]]), 4096);

    // As a real file, alone in its directory, it is complete.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    std::fs::write(&path, bytes).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec!["catalog.sqlite".to_string()],
        "no -wal, -shm, or -journal"
    );
    let from_disk = std::fs::read(&path).unwrap();
    Catalog::open_image(&from_disk, &CatalogLimits::default()).unwrap();
    // Opening from bytes never creates files either.
    let after: usize = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(after, 1);
}
