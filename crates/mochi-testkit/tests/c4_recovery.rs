//! C4 exit criterion (plan C4; spec §24.2 fault matrix):
//! **destroyed SQLite catalogs → recover the promised scope through
//! manifests, on multi-commit fixtures.**
//!
//! Each fixture is a byte stream of real encoded chunk objects and recovery
//! manifests. The working catalog is thrown away; manifests are found by
//! scanning frames, as a salvage tool would; the rebuilt catalog must match
//! the original at every commit, with every chunk, version, and attribute.
//! Then the damage cases: missing, corrupted, foreign, and forked manifests.
//!
//! Manifest schema 1: deltas chain to deltas; snapshot manifests have no
//! parent and are not on the chain. Manifest-only recovery therefore cannot
//! use a snapshot to bridge a missing delta: D10.8 anchors that on commit
//! *b*'s record (baseline recovery, plan T16). The tests that showed a
//! schema-0 snapshot restarting the chain now pin the schema-1 behaviour.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use mochi_core::catalog::namespace::{EntryKind, NamespaceOp};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::catalog::Catalog;
use mochi_core::manifest::{Attributes, Manifest, Mtime, PosixAttributes, WINDOWS_HIDDEN};
use mochi_core::recovery::{recover_from_manifests, ManifestRecovery, RecoveryScope};
use mochi_format::frame::Frames;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;
use mochi_format::Limits;
use mochi_testkit::deterministic_bytes;
use mochi_testkit::history::HistoryWriter;
use proptest::prelude::*;

/// What a salvage tool does: walk every frame, keep recovery manifests.
fn scan_manifests(bytes: &[u8]) -> Vec<StoredObject> {
    let mut out = Vec::new();
    for span in Frames::new(bytes, 0, Limits::default()) {
        let span = span.unwrap();
        if span.kind == FrameKind::RecoveryManifest {
            out.push(StoredObject::from_loaded(
                bytes[span.offset as usize..span.end() as usize].to_vec(),
            ));
        }
    }
    out
}

fn attrs(n: u64) -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode: (0o600 + n as u32) & 0o7777,
            uid: 1000,
            gid: 1000 + (n % 3) as u32,
        }),
        windows: n.is_multiple_of(2).then_some(WINDOWS_HIDDEN),
        mtime: Some(Mtime {
            secs: 1_700_000_000 - n as i64 * 86_400_000,
            nanos: (n * 7919 % 1_000_000_000) as u32,
        }),
    }
}

/// The recovered catalog equals the original, commit by commit, record by record.
fn assert_matches(original: &HistoryWriter, rec: &ManifestRecovery, from: u64, to: u64) {
    let cat = rec.catalog.as_ref().expect("a rebuilt catalog");
    for seq in from..=to {
        assert_eq!(
            cat.replay(Some(seq)).unwrap(),
            original.catalog.replay(Some(seq)).unwrap(),
            "commit {seq}"
        );
    }
    let head = cat.replay(Some(to)).unwrap();
    for (_, entry) in head.iter() {
        let (v, extents) = cat.file_version(&entry.version).unwrap().unwrap();
        assert_eq!(
            Some((v.clone(), extents.clone())),
            original.catalog.file_version(&entry.version).unwrap()
        );
        assert_eq!(
            rec.attributes.get(&v.id),
            original.attributes.get(&v.id),
            "attributes of {:?}",
            v.id
        );
        for e in &extents {
            if let mochi_core::catalog::extent::ExtentSource::Chunk { chunk, .. } = e.source {
                assert_eq!(
                    cat.object(&chunk).unwrap(),
                    original.catalog.object(&chunk).unwrap()
                );
                assert_eq!(
                    cat.object_location(&chunk).unwrap(),
                    original.catalog.object_location(&chunk).unwrap()
                );
            }
        }
    }
}

// ---- random histories ------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Action {
    PutTree(Vec<u8>, bool),
    DeleteSubtree(prop::sample::Index),
    Rename(prop::sample::Index),
    Replace(prop::sample::Index),
}

fn action() -> impl Strategy<Value = Action> {
    let path = proptest::collection::vec(0u8..3, 1..=3);
    prop_oneof![
        4 => (path, any::<bool>()).prop_map(|(p, d)| Action::PutTree(p, d)),
        1 => any::<prop::sample::Index>().prop_map(Action::DeleteSubtree),
        1 => any::<prop::sample::Index>().prop_map(Action::Rename),
        2 => any::<prop::sample::Index>().prop_map(Action::Replace),
    ]
}

fn p(s: &str) -> ArchivePath {
    ArchivePath::from_stored(s.as_bytes()).unwrap()
}

/// Build one always-valid commit from `actions` against the current state.
fn build_commit(w: &mut HistoryWriter, actions: &[Action], n: &mut u64) -> Vec<NamespaceOp> {
    let snap = w.catalog.replay(None).ok().unwrap_or_default();
    let mut state: BTreeMap<String, EntryKind> = snap
        .iter()
        .map(|(p, e)| (String::from_utf8(p.as_stored().to_vec()).unwrap(), e.kind))
        .collect();
    let versions: BTreeMap<String, _> = snap
        .iter()
        .map(|(p, e)| {
            (
                String::from_utf8(p.as_stored().to_vec()).unwrap(),
                e.version,
            )
        })
        .collect();
    let mut ops = Vec::new();
    for a in actions {
        *n += 1;
        match a {
            Action::PutTree(parts, is_dir) => {
                let names: Vec<&str> = parts.iter().map(|i| ["a", "b", "c"][*i as usize]).collect();
                for depth in 1..=names.len() {
                    let path = names[..depth].join("/");
                    let leaf = depth == names.len();
                    match state.get(&path) {
                        Some(EntryKind::Directory) if !leaf || *is_dir => continue,
                        Some(EntryKind::File) if !leaf => break, // cannot nest under a file
                        Some(_) => {
                            // Leaf replaces an existing entry; only safe if no children.
                            let prefix = format!("{path}/");
                            if state.keys().any(|k| k.starts_with(&prefix)) {
                                break;
                            }
                        }
                        None => {}
                    }
                    let kind = if leaf && !*is_dir {
                        EntryKind::File
                    } else {
                        EntryKind::Directory
                    };
                    let v = match kind {
                        EntryKind::Directory => w.dir(attrs(*n)).unwrap(),
                        EntryKind::File => w
                            .file(
                                &deterministic_bytes(*n, (*n % 5000) as usize),
                                *n % 3 * 1000,
                                attrs(*n),
                            )
                            .unwrap(),
                    };
                    state.insert(path.clone(), kind);
                    ops.push(NamespaceOp::Put {
                        path: p(&path),
                        version: v,
                    });
                }
            }
            Action::DeleteSubtree(i) | Action::Rename(i) | Action::Replace(i) => {
                let keys: Vec<String> = state.keys().cloned().collect();
                if keys.is_empty() {
                    continue;
                }
                let target = keys[i.index(keys.len())].clone();
                let prefix = format!("{target}/");
                let is_file = state.get(&target) == Some(&EntryKind::File);
                match a {
                    Action::DeleteSubtree(_) => {
                        for k in keys
                            .iter()
                            .filter(|k| **k == target || k.starts_with(&prefix))
                        {
                            state.remove(k);
                            ops.push(NamespaceOp::Delete { path: p(k) });
                        }
                    }
                    Action::Rename(_) if is_file => {
                        if let Some(v) = versions.get(&target) {
                            let dest = format!("r{n}");
                            state.remove(&target);
                            state.insert(dest.clone(), EntryKind::File);
                            ops.push(NamespaceOp::Delete { path: p(&target) });
                            ops.push(NamespaceOp::Put {
                                path: p(&dest),
                                version: *v,
                            });
                        }
                    }
                    Action::Replace(_) if is_file => {
                        let v = w
                            .file(&deterministic_bytes(*n ^ 0xABC, 300), 0, attrs(*n))
                            .unwrap();
                        ops.push(NamespaceOp::Put {
                            path: p(&target),
                            version: v,
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    ops
}

fn history(commits: &[Vec<Action>], snapshot_every: usize, seed: u64) -> HistoryWriter {
    let mut w = HistoryWriter::new(seed).unwrap();
    let mut n = 0u64;
    for (i, actions) in commits.iter().enumerate() {
        let ops = build_commit(&mut w, actions, &mut n);
        let snap = snapshot_every > 0 && i > 0 && i % snapshot_every == 0;
        w.commit(ops, snap).unwrap();
    }
    w
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn destroyed_catalog_is_rebuilt_from_manifests_at_every_commit(
        commits in proptest::collection::vec(proptest::collection::vec(action(), 0..4), 1..16),
        snapshot_every in 0usize..5,
        seed in any::<u64>(),
    ) {
        let w = history(&commits, snapshot_every, seed);
        let found = scan_manifests(&w.bytes);
        let snapshots = (1..commits.len())
            .filter(|i| snapshot_every > 0 && i % snapshot_every == 0)
            .count();
        prop_assert_eq!(found.len(), commits.len() + snapshots);

        let rec = recover_from_manifests(&found, None, &Limits::default()).unwrap();
        let head = (commits.len() - 1) as u64;
        prop_assert_eq!(rec.head_seq, head);
        prop_assert_eq!(rec.snapshot_range, Some((0, head)));
        prop_assert!(rec.stopped.is_none(), "{:?}", rec.stopped);
        prop_assert_eq!(rec.unused, snapshots, "snapshots are off the delta chain");
        prop_assert_eq!(rec.scope_for(head), RecoveryScope::SnapshotRecovery);
        if head > 0 {
            prop_assert_eq!(rec.scope_for(0), RecoveryScope::HistoricalRecovery);
        }
        assert_matches(&w, &rec, 0, head);

        // The same, but with manifests handed over in reverse order.
        let mut shuffled = found.clone();
        shuffled.reverse();
        let rec2 = recover_from_manifests(&shuffled, None, &Limits::default()).unwrap();
        prop_assert_eq!(rec2.snapshot_range, Some((0, head)));
    }
}

// ---- damage cases -----------------------------------------------------------------------

/// A fixed ten-commit history with a snapshot manifest at commit 6.
fn fixed() -> HistoryWriter {
    let mut w = HistoryWriter::new(42).unwrap();
    let d = w.dir(attrs(0)).unwrap();
    w.commit(
        vec![NamespaceOp::Put {
            path: p("docs"),
            version: d,
        }],
        false,
    )
    .unwrap();
    for i in 1..10u64 {
        let f = w
            .file(
                &deterministic_bytes(i, 2000 + i as usize),
                i % 2 * 4096,
                attrs(i),
            )
            .unwrap();
        let mut ops = vec![NamespaceOp::Put {
            path: p(&format!("docs/f{i}")),
            version: f,
        }];
        if i % 3 == 0 {
            ops.push(NamespaceOp::Delete {
                path: p(&format!("docs/f{}", i - 2)),
            });
        }
        w.commit(ops, i == 6).unwrap();
    }
    w
}

/// `found` without the *delta* manifests of `seqs`.
fn without(found: &[StoredObject], w: &HistoryWriter, seqs: &[u64]) -> Vec<StoredObject> {
    let drop: Vec<_> = seqs.iter().map(|s| w.delta(*s).hash).collect();
    found
        .iter()
        .filter(|o| !drop.contains(&mochi_format::digest::stored_object_hash(o.view())))
        .cloned()
        .collect()
}

/// **Changed from schema 0.** There, the snapshot at 6 carried a parent
/// link and restarted the chain past the missing delta(3), recovering 6..=9.
/// In schema 1 a snapshot has no parent, and binding S(6) to delta(7)'s
/// parent link needs commit 6's record (D10.8), which manifest-only recovery
/// does not have. So: file-level metadata only, and S(6) is unused. Plan T16
/// restores 6..=9 through `recover_with_trusted_head`.
#[test]
fn missing_delta_before_a_snapshot_is_not_bridged_without_commit_records() {
    let w = fixed();
    let found = without(&scan_manifests(&w.bytes), &w, &[3]);
    let rec = recover_from_manifests(&found, None, &Limits::default()).unwrap();
    assert_eq!(rec.chain_broken_at, Some(4));
    assert_eq!(rec.snapshot_range, None);
    assert!(rec.catalog.is_none());
    assert_eq!(rec.scope_for(9), RecoveryScope::FileRecovery);
    assert_eq!(rec.scope_for(1), RecoveryScope::FileRecovery);
    // Off the chain: deltas 0..=2 and S(6).
    assert_eq!(rec.unused, 4);
}

/// A snapshot named as the head stands alone: it is self-contained.
#[test]
fn a_snapshot_named_as_head_recovers_its_commit_alone() {
    use mochi_core::manifest::ManifestKind;
    let w = fixed();
    let found = without(&scan_manifests(&w.bytes), &w, &[3]);
    let s6 = w.manifest(6, ManifestKind::Snapshot).hash;
    let rec = recover_from_manifests(&found, Some(s6), &Limits::default()).unwrap();
    assert_eq!(rec.head_seq, 6);
    assert_eq!(rec.snapshot_range, Some((6, 6)));
    assert_matches(&w, &rec, 6, 6);
}

/// Schema 1: a delta's parent link names the parent's *delta* manifest. A
/// link that resolves to a snapshot manifest (hash, archive, and sequence all
/// matching) breaks the chain instead of being followed.
#[test]
fn a_parent_link_to_a_snapshot_breaks_the_chain() {
    use mochi_core::manifest::{ManifestKind, ParentLink};
    let w = fixed();
    let mut found = scan_manifests(&w.bytes);
    let d7 = w.delta(7);
    let (mut m, _) = Manifest::from_stored(
        &StoredObject::from_loaded(
            w.bytes[d7.offset as usize..(d7.offset + d7.len) as usize].to_vec(),
        ),
        &Limits::default(),
        &Default::default(),
    )
    .unwrap();
    m.parent = Some(ParentLink {
        seq: 6,
        delta_manifest_hash: w.manifest(6, ManifestKind::Snapshot).hash,
    });
    let forged = m.to_stored().unwrap();
    let forged_hash = mochi_format::digest::stored_object_hash(forged.view());
    found.push(forged);
    let rec = recover_from_manifests(&found, Some(forged_hash), &Limits::default()).unwrap();
    assert_eq!(rec.chain_broken_at, Some(7));
    assert_eq!(rec.snapshot_range, None);
}

#[test]
fn missing_manifest_after_the_last_snapshot_claims_no_snapshot() {
    let w = fixed();
    let found = without(&scan_manifests(&w.bytes), &w, &[8]);
    let rec = recover_from_manifests(&found, None, &Limits::default()).unwrap();
    assert_eq!(rec.head_seq, 9);
    assert_eq!(rec.chain_broken_at, Some(9));
    assert_eq!(rec.snapshot_range, None);
    assert!(rec.catalog.is_none());
    assert_eq!(rec.scope_for(9), RecoveryScope::FileRecovery);
    assert!(rec.file_versions_known > 0);
    assert!(
        rec.unused >= 8,
        "manifests before the break are not trusted"
    );
}

#[test]
fn a_corrupted_manifest_is_rejected_or_breaks_the_chain_never_trusted() {
    let w = fixed();
    let target = w.delta(4);
    for delta in [9u64, 40, target.len / 2, target.len - 1] {
        let mut bytes = w.bytes.clone();
        bytes[(target.offset + delta) as usize] ^= 0x01;
        let found = scan_manifests(&bytes);
        let rec =
            recover_from_manifests(&found, Some(w.delta(9).hash), &Limits::default()).unwrap();
        // Either way, delta(4) is not on the verified chain: the walk from 9
        // stops at 5, and (schema 1) S(6) cannot bridge it from manifests
        // alone. Nothing past the break is claimed.
        assert_eq!(rec.chain_broken_at, Some(5), "flip at +{delta}");
        assert_eq!(rec.snapshot_range, None, "flip at +{delta}");
        assert_eq!(rec.chain_start_seq, 5);
    }
}

#[test]
fn foreign_archives_require_a_named_head_and_are_never_mixed_in() {
    let w = fixed();
    let other = history(&vec![vec![Action::PutTree(vec![0], false)]; 3], 0, 7);
    let mut found = scan_manifests(&w.bytes);
    found.extend(scan_manifests(&other.bytes));
    let e = recover_from_manifests(&found, None, &Limits::default()).unwrap_err();
    assert!(e.message.contains("different archives"), "{}", e.message);
    let rec = recover_from_manifests(&found, Some(w.delta(9).hash), &Limits::default()).unwrap();
    assert_eq!(rec.snapshot_range, Some((0, 9)));
    // The other archive's three deltas, and this archive's S(6).
    assert_eq!(rec.unused, 4);
    assert_matches(&w, &rec, 0, 9);
}

#[test]
fn a_forged_competing_head_is_ambiguous_until_the_head_is_named() {
    let w = fixed();
    let mut found = scan_manifests(&w.bytes);
    // Same archive, same sequence as the head, different content.
    let (mut forged, _) = Manifest::from_stored(
        found.last().unwrap(),
        &Limits::default(),
        &Default::default(),
    )
    .unwrap();
    forged.ops.clear();
    found.push(forged.to_stored().unwrap());
    let e = recover_from_manifests(&found, None, &Limits::default()).unwrap_err();
    assert!(
        e.message.contains("claim the latest commit"),
        "{}",
        e.message
    );
    let rec = recover_from_manifests(&found, Some(w.delta(9).hash), &Limits::default()).unwrap();
    assert_eq!(rec.snapshot_range, Some((0, 9)));
    assert_matches(&w, &rec, 0, 9);
}

/// **Limitation, pinned on purpose.** A forger rewrites delta(6) (dropping
/// its delete) and re-links 7..9 so the chain verifies. That is a new,
/// self-consistent history; nothing inside it is detectably wrong. Only a
/// trusted head exposes it: the original head hash is simply not among the
/// forged objects. Without a trusted head, the forgery is reconstructed. This
/// is spec §5.7 (internal validity does not establish freshness), and why C5
/// takes the head from the verified footer and D8 lets users supply one.
#[test]
fn a_rewritten_history_is_only_detectable_against_a_trusted_head() {
    let w = fixed();
    let decode = |o: &StoredObject| {
        Manifest::from_stored(o, &Limits::default(), &Default::default())
            .unwrap()
            .0
    };
    let mut chain: Vec<Manifest> = scan_manifests(&w.bytes)
        .iter()
        .map(decode)
        .filter(|m| m.kind == mochi_core::manifest::ManifestKind::Delta)
        .collect();
    chain.sort_by_key(|m| m.commit_seq);
    assert!(matches!(
        chain[6].ops.pop(),
        Some(NamespaceOp::Delete { .. })
    ));
    let mut prev_hash = None;
    let mut forged = Vec::new();
    for m in &mut chain {
        if let (Some(h), Some(link)) = (prev_hash, m.parent.as_mut()) {
            link.delta_manifest_hash = h;
        }
        let stored = m.to_stored().unwrap();
        prev_hash = Some(mochi_format::digest::stored_object_hash(stored.view()));
        forged.push(stored);
    }

    // With the trusted head: refused.
    let trusted = w.delta(9).hash;
    let e = recover_from_manifests(&forged, Some(trusted), &Limits::default()).unwrap_err();
    assert!(e.message.contains("requested head"), "{}", e.message);

    // Without it: the forgery is accepted and differs from the real history.
    let rec = recover_from_manifests(&forged, None, &Limits::default()).unwrap();
    assert_eq!(rec.snapshot_range, Some((0, 9)));
    assert_ne!(rec.head_hash, trusted);
    let cat = rec.catalog.unwrap();
    assert_ne!(
        cat.replay(Some(6)).unwrap(),
        w.catalog.replay(Some(6)).unwrap()
    );
}

/// A manifest whose parent link names a manifest of *another* archive (hash
/// correct, archive wrong) breaks the chain instead of splicing histories.
#[test]
fn a_parent_link_into_another_archive_breaks_the_chain() {
    let w = fixed();
    let other = history(&vec![vec![Action::PutTree(vec![0], false)]; 3], 0, 7);
    let other_head = other.manifests.last().unwrap();
    let mut found = scan_manifests(&w.bytes);
    found.extend(scan_manifests(&other.bytes));
    let d9 = w.delta(9);
    let (mut head, _) = Manifest::from_stored(
        &StoredObject::from_loaded(
            w.bytes[d9.offset as usize..(d9.offset + d9.len) as usize].to_vec(),
        ),
        &Limits::default(),
        &Default::default(),
    )
    .unwrap();
    // Schema 1 requires the link to say 8 (this sequence − 1); the hash it
    // carries names the other archive's head delta (sequence 2).
    assert_eq!(other_head.seq, 2);
    head.parent = Some(mochi_core::manifest::ParentLink {
        seq: 8,
        delta_manifest_hash: other_head.hash,
    });
    let forged = head.to_stored().unwrap();
    let forged_hash = mochi_format::digest::stored_object_hash(forged.view());
    found.push(forged);
    let rec = recover_from_manifests(&found, Some(forged_hash), &Limits::default()).unwrap();
    assert_eq!(rec.chain_broken_at, Some(9));
    assert_eq!(rec.snapshot_range, None);
    assert_eq!(rec.scope_for(9), RecoveryScope::FileRecovery);
}

#[test]
fn recovered_catalog_publishes_and_reopens() {
    let w = fixed();
    let rec = recover_from_manifests(&scan_manifests(&w.bytes), None, &Limits::default()).unwrap();
    let image = rec.catalog.unwrap().publish().unwrap();
    let reopened = Catalog::open_image(image.as_bytes(), &Default::default()).unwrap();
    assert_eq!(
        reopened.replay(None).unwrap(),
        w.catalog.replay(None).unwrap()
    );
}
