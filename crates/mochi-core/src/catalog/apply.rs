//! Strict, atomic, incremental application of delta manifests onto a
//! checkpoint catalog: the replay half of plan T12 (spec Annex B.2 D10.4),
//! used by `open_head`, `open_at_footer`, and `open_append` in
//! `crate::publish`. Segment validation (T11) is `crate::segment`.
//!
//! # Contract
//!
//! * **Namespace built once.** [`SegmentApplier::new`] replays the base
//!   catalog's namespace one time. [`SegmentApplier::apply`] never rebuilds
//!   the whole history or the whole namespace per manifest; it validates
//!   against the held [`Snapshot`] and does lookups only for IDs the
//!   manifest names. (Review decision 15: the architectural fix is T12; the
//!   lookup and mutation bounds themselves are T13.)
//! * **Atomic, including the in-memory namespace** (review amendment 1).
//!   Every row a manifest adds is written in one SQLite transaction; the
//!   namespace change is staged with an undo log proportional to the
//!   manifest's operations. On any failure, including at `COMMIT`, both the
//!   database and the namespace are exactly at commit *j* − 1. The namespace
//!   change is kept only after the transaction commits.
//! * **Strict introduction** (D10.4, O19; review decision 18). An object ID
//!   or file-version ID that this manifest lists and that already exists, in
//!   the base or an earlier delta, is `RECORD_INVALID`, whether or not the
//!   records are identical. Reusing an existing ID *by reference* (a `PUT`
//!   of an existing version, an extent of an existing chunk) is not
//!   introduction and is allowed. The general-purpose idempotent
//!   `Catalog::insert_*` methods keep their §10.2 behaviour.
//! * **Completed-state validation** (§10.2, D10.4). Operations apply in
//!   array order; parent validity is checked against the commit's completed
//!   state, so a manifest whose namespace is only *temporarily* invalid
//!   between operations is accepted.
//!
//! What this does **not** check, because the caller holds the inputs: the
//! stored-object hash, D11 identity binding, and the parent link of the
//! manifest (`crate::segment::check_delta_parent_link`). It does check what
//! it can see: kind, sequence, parent sequence, and required features.

use std::cell::Cell;
use std::collections::HashMap;

use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{Manifest, ManifestKind};

use super::namespace::{EntryKind, FileVersionId, NamespaceOp, Snapshot};
use super::{
    insert_commit_rows, insert_file_version_rows, insert_object_rows, sql, Catalog, Commit,
};

fn record_invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

/// Fault-injection points, numbered from 0 in the order `apply` reaches
/// them: one before every row mutation, then one immediately before
/// `COMMIT`. Compiled in for unit tests and for the non-default
/// `test-controls` feature only.
#[derive(Debug, Default)]
struct Faults {
    #[cfg(any(test, feature = "test-controls"))]
    fail_at: Option<usize>,
    reached: Cell<usize>,
}

impl Faults {
    fn point(&self) -> Result<()> {
        let n = self.reached.get();
        self.reached.set(n + 1);
        #[cfg(any(test, feature = "test-controls"))]
        if self.fail_at == Some(n) {
            return Err(MochiError::new(
                ErrorCode::IoError,
                format!("injected failure at apply fault point {n}"),
            ));
        }
        Ok(())
    }
}

/// Applies the delta manifests of one segment, in order, onto its base
/// checkpoint's catalog.
#[derive(Debug)]
pub struct SegmentApplier {
    catalog: Catalog,
    namespace: Snapshot,
    head: u64,
    faults: Faults,
    /// Namespace work of the last `apply` (D10.5; `super::bounds`).
    namespace_probes: Cell<u64>,
    namespace_mutations: u64,
}

impl SegmentApplier {
    /// Start from a writable, already-verified checkpoint catalog
    /// (`Catalog::open_image_writable` after the hash and envelope checks).
    /// This is the one namespace build per open.
    pub fn new(catalog: Catalog) -> Result<Self> {
        let head = catalog.head_seq()?.ok_or_else(|| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                "a segment applier needs a checkpoint catalog with at least one commit",
            )
        })?;
        let namespace = catalog.replay(None)?;
        Ok(SegmentApplier {
            catalog,
            namespace,
            head,
            faults: Faults::default(),
            namespace_probes: Cell::new(0),
            namespace_mutations: 0,
        })
    }

    /// The commit sequence the catalog and namespace currently materialize.
    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn namespace(&self) -> &Snapshot {
        &self.namespace
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The catalog at [`SegmentApplier::head`].
    pub fn into_catalog(self) -> Catalog {
        self.catalog
    }

    /// Apply delta manifest `head + 1`. On error, nothing changed.
    pub fn apply(&mut self, delta: &Manifest) -> Result<()> {
        self.faults.reached.set(0);
        self.namespace_probes.set(0);
        self.namespace_mutations = 0;
        self.check_shape(delta)?;
        self.check_introductions(delta)?;

        // Kinds for every version a PUT names: new ones from the manifest,
        // existing ones by lookup (bounded by the manifest, not the catalog).
        let mut kinds: HashMap<FileVersionId, EntryKind> = delta
            .file_versions
            .iter()
            .map(|v| (v.version.id, v.version.kind))
            .collect();
        let existing = self
            .catalog
            .kinds_for(delta.ops.iter().filter_map(|op| match op {
                NamespaceOp::Put { version, .. } if !kinds.contains_key(version) => Some(version),
                _ => None,
            }))?;
        kinds.extend(existing);

        // Reference validity (checklist Q26), before any row is written:
        // every PUT names a version that exists once this delta is applied,
        // i.e. one the catalog already has or one this delta introduces
        // (`kinds` is exactly that set). Checked here, explicitly, so that
        // neither SQL statement order nor the foreign key decides the outcome,
        // and a valid reference to a same-delta version is accepted. This is
        // archive input, so the code is NAMESPACE_INVALID; the shared
        // namespace mapping keeps CATALOG_INVALID for the catalog's own rows
        // (`Catalog::replay`), where an unknown version means the catalog is
        // internally inconsistent.
        for (op_seq, op) in delta.ops.iter().enumerate() {
            if let NamespaceOp::Put { path, version } = op {
                if !kinds.contains_key(version) {
                    return Err(MochiError::new(
                        ErrorCode::NamespaceInvalid,
                        format!(
                            "replaying commit {}: operation {op_seq} puts {path:?} as file version \
                             {version:?}, which neither the catalog nor this delta contains",
                            delta.commit_seq
                        ),
                    ));
                }
            }
        }

        // Rows. `unchecked_transaction` lets the §10.3 extent check below see
        // this manifest's own (uncommitted) chunks on the same connection.
        // Dropping `tx` without committing rolls everything back.
        let tx = self.catalog.conn.unchecked_transaction().map_err(sql)?;
        let mut hook = || self.faults.point();
        for c in &delta.chunks {
            insert_object_rows(&tx, &c.record, c.location, &mut hook)?;
        }
        for v in &delta.file_versions {
            self.catalog.check_file_version(&v.version, &v.extents)?;
            insert_file_version_rows(&tx, &v.version, &v.extents, &mut hook)?;
        }
        insert_commit_rows(
            &tx,
            &Commit {
                seq: delta.commit_seq,
                parent: Some(self.head),
                ops: delta.ops.clone(),
            },
            &mut hook,
        )?;

        // Namespace, staged: validated against the completed commit state.
        let undo = self
            .namespace
            .apply_commit_staged(
                &delta.ops,
                |v| kinds.get(v).copied(),
                &self.namespace_probes,
            )
            .map_err(|f| {
                let e = MochiError::from(f);
                MochiError::new(
                    e.code,
                    format!("replaying commit {}: {}", delta.commit_seq, e.message),
                )
            })?;
        self.namespace_mutations = undo.mutations() as u64;

        let finished = self.faults.point().and_then(|()| tx.commit().map_err(sql));
        match finished {
            Ok(()) => {
                drop(undo);
                self.head = delta.commit_seq;
                Ok(())
            }
            Err(e) => {
                self.namespace.undo(undo);
                Err(e)
            }
        }
    }

    fn check_shape(&self, delta: &Manifest) -> Result<()> {
        if delta.kind != ManifestKind::Delta {
            return Err(record_invalid(
                "replay applies delta manifests only; a snapshot manifest is not a delta (D10.4)",
            ));
        }
        let expected = self
            .head
            .checked_add(1)
            .ok_or_else(|| record_invalid("commit sequence overflow"))?;
        if delta.commit_seq != expected {
            return Err(record_invalid(format!(
                "delta manifest for commit {} cannot follow commit {} (D10.4: sequence order)",
                delta.commit_seq, self.head
            )));
        }
        if delta.parent.map(|p| p.seq) != Some(self.head) {
            return Err(record_invalid(format!(
                "delta manifest {} does not name commit {} as its parent",
                delta.commit_seq, self.head
            )));
        }
        // The decoder already refuses unknown features; refuse again rather
        // than apply something this build cannot interpret (D10.4).
        if delta
            .required_features
            .iter()
            .any(|f| !crate::manifest::KNOWN_REQUIRED_FEATURES.contains(f))
        {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                format!(
                    "delta manifest {} requires features {:?}, which this build does not support",
                    delta.commit_seq, delta.required_features
                ),
            ));
        }
        Ok(())
    }

    /// D10.4: an ID introduced twice is corruption, identical or not.
    fn check_introductions(&self, delta: &Manifest) -> Result<()> {
        for pair in delta.chunks.windows(2) {
            if pair[0].record.id == pair[1].record.id {
                return Err(dup("object", &pair[0].record.id.to_hex(), delta.commit_seq));
            }
        }
        for pair in delta.file_versions.windows(2) {
            if pair[0].version.id == pair[1].version.id {
                return Err(dup(
                    "file version",
                    &format!("{:?}", pair[0].version.id),
                    delta.commit_seq,
                ));
            }
        }
        for c in &delta.chunks {
            if self.catalog.object(&c.record.id)?.is_some() {
                return Err(dup("object", &c.record.id.to_hex(), delta.commit_seq));
            }
        }
        for v in &delta.file_versions {
            if self.catalog.file_version(&v.version.id)?.is_some() {
                return Err(dup(
                    "file version",
                    &format!("{:?}", v.version.id),
                    delta.commit_seq,
                ));
            }
        }
        Ok(())
    }
}

fn dup(what: &str, id: &str, seq: u64) -> MochiError {
    record_invalid(format!(
        "delta manifest {seq} introduces {what} {id}, which already exists: an ID introduced \
         twice is corruption (D10.4, O19)"
    ))
}

/// Test controls (unit tests, and the non-default `test-controls` feature).
/// Enabling the feature enables these; it is not an isolation boundary.
#[cfg(any(test, feature = "test-controls"))]
impl SegmentApplier {
    /// Fail the next `apply` at fault point `n` (see [`Faults`]).
    pub fn inject_failure_at(&mut self, n: Option<usize>) {
        self.faults.fail_at = n;
    }

    /// Fault points the last `apply` reached.
    pub fn fault_points_reached(&self) -> usize {
        self.faults.reached.get()
    }

    /// Namespace probes made by the last `apply` (see `super::bounds`).
    pub fn last_namespace_probes(&self) -> u64 {
        self.namespace_probes.get()
    }

    /// Namespace entries set or removed by the last `apply`, counted from
    /// its undo log; 0 if the namespace refused the commit.
    pub fn last_namespace_mutations(&self) -> u64 {
        self.namespace_mutations
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use mochi_format::codec::{Encoding, Protection};
    use mochi_format::digest::{chunk_content_hash, FileContentHash, StoredObjectHash};
    use mochi_format::repr::DecodedBytes;

    use super::*;
    use crate::catalog::extent::{Extent, ExtentSource};
    use crate::catalog::path::ArchivePath;
    use crate::catalog::{replay_count, FileVersion};
    use crate::manifest::{Attributes, ChunkEntry, FileVersionEntry, ParentLink};
    use crate::object::{ArchiveId, ObjectId, ObjectRecord};

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }
    fn oid(n: u8) -> ObjectId {
        ObjectId::from_bytes([n; 32])
    }
    fn vid(n: u8) -> FileVersionId {
        FileVersionId::from_bytes([n; 32])
    }

    fn chunk(n: u8) -> ChunkEntry {
        ChunkEntry {
            record: ObjectRecord {
                id: oid(n),
                encoding: Encoding::ZstdFrame,
                protection: Protection::None,
                stored_len: 40,
                stored_hash: StoredObjectHash::from_bytes([n; 32]),
                decoded_len: 100,
                content_hash: chunk_content_hash(&DecodedBytes::new(vec![n; 100])),
                dependencies: vec![],
            },
            location: Some(1000 + u64::from(n)),
        }
    }

    fn dir(n: u8) -> FileVersionEntry {
        FileVersionEntry {
            version: FileVersion {
                id: vid(n),
                kind: EntryKind::Directory,
                logical_len: 0,
                content_hash: None,
            },
            extents: vec![],
            attributes: Attributes::default(),
        }
    }

    /// A 60-byte file over chunk `c`.
    fn file(n: u8, c: u8) -> FileVersionEntry {
        FileVersionEntry {
            version: FileVersion {
                id: vid(n),
                kind: EntryKind::File,
                logical_len: 60,
                content_hash: Some(FileContentHash::from_bytes([n; 32])),
            },
            extents: vec![Extent {
                ordinal: 0,
                logical_offset: 0,
                length: 60,
                source: ExtentSource::Chunk {
                    chunk: oid(c),
                    chunk_offset: 0,
                },
            }],
            attributes: Attributes::default(),
        }
    }

    fn put(path: &str, v: u8) -> NamespaceOp {
        NamespaceOp::Put {
            path: p(path),
            version: vid(v),
        }
    }
    fn del(path: &str) -> NamespaceOp {
        NamespaceOp::Delete { path: p(path) }
    }

    fn delta(
        seq: u64,
        chunks: Vec<ChunkEntry>,
        versions: Vec<FileVersionEntry>,
        ops: Vec<NamespaceOp>,
    ) -> Manifest {
        let mut m = Manifest {
            archive_id: ArchiveId::from_bytes([7; 32]),
            commit_seq: seq,
            transaction_id: [seq as u8; 16],
            parent: Some(ParentLink {
                seq: seq - 1,
                delta_manifest_hash: StoredObjectHash::from_bytes([0xEE; 32]),
            }),
            kind: ManifestKind::Delta,
            chunks,
            file_versions: versions,
            ops,
            entries: vec![],
            required_features: vec![],
            retention_ops: Vec::new(),
            retention: Default::default(),
            provenance: None,
            keys: Default::default(),
        };
        m.canonicalize();
        m
    }

    /// Base checkpoint at commit 0: dir `d` (version 1) and file `d/f`
    /// (version 2, chunk 1), published and reopened writable as a reader
    /// would.
    fn base() -> Catalog {
        let mut c = Catalog::new_working().unwrap();
        let ch = chunk(1);
        c.insert_object(&ch.record, ch.location).unwrap();
        for v in [dir(1), file(2, 1)] {
            c.insert_file_version(&v.version, &v.extents).unwrap();
        }
        c.append_commit(&Commit {
            seq: 0,
            parent: None,
            ops: vec![put("d", 1), put("d/f", 2)],
        })
        .unwrap();
        let img = c.publish().unwrap();
        Catalog::open_image_writable(img.as_bytes(), &super::super::CatalogLimits::default())
            .unwrap()
    }

    /// Everything observable: a logical dump of every table, the namespace,
    /// and the head.
    fn state(a: &SegmentApplier) -> (Vec<String>, Snapshot, u64) {
        (
            a.catalog.logical_dump().unwrap(),
            a.namespace.clone(),
            a.head,
        )
    }

    /// A delta that touches every row kind: two chunks, a directory, two
    /// files, a delete, and a rename (delete + put).
    fn busy_delta() -> Manifest {
        delta(
            1,
            vec![chunk(2), chunk(3)],
            vec![dir(10), file(11, 2), file(12, 3)],
            vec![
                put("e", 10),
                put("e/g", 11),
                del("d/f"),
                put("d/h", 12),
                put("e/f2", 2), // reuse of version 2 by reference: allowed
            ],
        )
    }

    /// T13: declared work bounds (`super::super::bounds`).
    mod work_bounds;

    #[test]
    fn applies_in_order_and_matches_a_from_scratch_replay() {
        let mut a = SegmentApplier::new(base()).unwrap();
        a.apply(&busy_delta()).unwrap();
        a.apply(&delta(2, vec![], vec![], vec![del("e/g")]))
            .unwrap();
        assert_eq!(a.head(), 2);
        assert_eq!(a.catalog.head_seq().unwrap(), Some(2));
        // The incrementally maintained namespace equals a full replay of the
        // resulting catalog, and the catalog verifies.
        assert_eq!(a.namespace(), &a.catalog.replay(None).unwrap());
        a.catalog.verify().unwrap();
        assert!(a.namespace().get(&p("d/f")).is_none());
        assert_eq!(a.namespace().get(&p("e/f2")).unwrap().version, vid(2));
    }

    #[test]
    fn no_whole_history_replay_per_manifest() {
        let mut a = SegmentApplier::new(base()).unwrap();
        let before = replay_count();
        a.apply(&busy_delta()).unwrap();
        for seq in 2..=6 {
            let v = 20 + seq as u8;
            a.apply(&delta(
                seq,
                vec![],
                vec![dir(v)],
                vec![put(&format!("x{seq}"), v)],
            ))
            .unwrap();
        }
        assert_eq!(
            replay_count(),
            before,
            "apply must not rebuild the namespace"
        );
    }

    #[test]
    fn temporarily_invalid_but_completed_valid_is_accepted() {
        // Child before parent within one commit: invalid mid-array, valid
        // in the completed state (§10.2).
        let mut a = SegmentApplier::new(base()).unwrap();
        a.apply(&delta(
            1,
            vec![],
            vec![dir(10), dir(11)],
            vec![put("n/m", 11), put("n", 10)],
        ))
        .unwrap();
        assert!(a.namespace().get(&p("n/m")).is_some());
    }

    /// Ops invalid in the completed commit state (not merely mid-array),
    /// each placed first, in the middle, and last among valid ops.
    #[test]
    fn semantic_failure_at_any_position_leaves_nothing() {
        type Bad = (&'static str, fn() -> NamespaceOp, ErrorCode);
        let bad: [Bad; 4] = [
            (
                "parent never exists",
                || put("nowhere/x", 10),
                ErrorCode::NamespaceInvalid,
            ),
            (
                "delete of a path absent throughout",
                || del("ghost"),
                ErrorCode::NamespaceInvalid,
            ),
            (
                "directory replaced by a file orphans d/f",
                || put("d", 11),
                ErrorCode::NamespaceInvalid,
            ),
            (
                // Was CATALOG_INVALID (the foreign key fired first); checklist
                // Q26, decided 2026-10-04: NAMESPACE_INVALID, by an explicit
                // reference check before any row is written.
                "unknown file version",
                || put("q", 99),
                ErrorCode::NamespaceInvalid,
            ),
        ];
        for (what, op, code) in bad {
            for pos in 0..3 {
                let mut ops = vec![put("a1", 10), put("a2", 10)];
                ops.insert(pos, op());
                let m = delta(1, vec![chunk(2)], vec![dir(10), file(11, 2)], ops);
                let mut a = SegmentApplier::new(base()).unwrap();
                let before = state(&a);
                let e = a.apply(&m).unwrap_err();
                assert_eq!(e.code, code, "{what} at {pos}: {e}");
                assert_eq!(state(&a), before, "{what} at {pos} left state behind");
                // Still usable: a valid manifest applies afterwards.
                a.apply(&delta(1, vec![], vec![dir(10)], vec![put("ok", 10)]))
                    .unwrap();
            }
        }
    }

    #[test]
    fn injected_failure_at_every_point_leaves_nothing() {
        let m = busy_delta();
        let points = {
            let mut a = SegmentApplier::new(base()).unwrap();
            a.apply(&m).unwrap();
            a.fault_points_reached()
        };
        // 2 objects × (objects, chunks, location) + 3 versions + 2 extents
        // + 1 commit row + 5 ops + the point before COMMIT.
        assert_eq!(points, 6 + 3 + 2 + 1 + 5 + 1);
        for n in 0..points {
            let mut a = SegmentApplier::new(base()).unwrap();
            let before = state(&a);
            a.inject_failure_at(Some(n));
            let e = a.apply(&m).unwrap_err();
            assert_eq!(e.code, ErrorCode::IoError, "point {n}: {e}");
            assert_eq!(state(&a), before, "point {n} left state behind");
            a.inject_failure_at(None);
            a.apply(&m).unwrap();
            assert_eq!(a.head(), 1);
        }
    }

    #[test]
    fn reintroducing_an_id_is_record_invalid_identical_or_not() {
        let cases: Vec<(&str, Manifest)> = vec![
            (
                "identical chunk from the base",
                delta(1, vec![chunk(1)], vec![], vec![]),
            ),
            ("conflicting chunk from the base", {
                let mut c = chunk(1);
                c.record.decoded_len = 7;
                delta(1, vec![c], vec![], vec![])
            }),
            (
                "identical version from the base",
                delta(1, vec![], vec![dir(1)], vec![]),
            ),
            (
                "conflicting version from the base",
                delta(1, vec![], vec![file(1, 1)], vec![]),
            ),
        ];
        for (what, m) in cases {
            let mut a = SegmentApplier::new(base()).unwrap();
            let before = state(&a);
            let e = a.apply(&m).unwrap_err();
            assert_eq!(e.code, ErrorCode::RecordInvalid, "{what}: {e}");
            assert_eq!(state(&a), before, "{what}");
        }
        // Across two deltas of one segment.
        let mut a = SegmentApplier::new(base()).unwrap();
        a.apply(&delta(1, vec![chunk(5)], vec![dir(10)], vec![]))
            .unwrap();
        for m in [
            delta(2, vec![chunk(5)], vec![], vec![]),
            delta(2, vec![], vec![dir(10)], vec![]),
        ] {
            assert_eq!(a.apply(&m).unwrap_err().code, ErrorCode::RecordInvalid);
        }
        // Within one manifest (a hand-built one; the decoder rejects these).
        let mut m = delta(2, vec![chunk(6)], vec![], vec![]);
        m.chunks.push(chunk(6));
        assert_eq!(a.apply(&m).unwrap_err().code, ErrorCode::RecordInvalid);
    }

    #[test]
    fn reuse_by_reference_is_not_reintroduction() {
        let mut a = SegmentApplier::new(base()).unwrap();
        // New file version over the base's chunk 1; PUT of the base's dir.
        a.apply(&delta(
            1,
            vec![],
            vec![file(30, 1)],
            vec![put("d2", 1), put("d2/x", 30)],
        ))
        .unwrap();
    }

    #[test]
    fn out_of_order_wrong_kind_and_features_are_refused() {
        let mut a = SegmentApplier::new(base()).unwrap();
        let before = state(&a);
        let e = a.apply(&delta(2, vec![], vec![], vec![])).unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);

        let mut m = delta(1, vec![], vec![], vec![]);
        m.parent = Some(ParentLink {
            seq: 5,
            delta_manifest_hash: StoredObjectHash::from_bytes([0; 32]),
        });
        assert_eq!(a.apply(&m).unwrap_err().code, ErrorCode::RecordInvalid);

        let mut m = delta(1, vec![], vec![], vec![]);
        m.kind = ManifestKind::Snapshot;
        assert_eq!(a.apply(&m).unwrap_err().code, ErrorCode::RecordInvalid);

        let mut m = delta(1, vec![], vec![], vec![]);
        m.required_features = vec![2];
        assert_eq!(a.apply(&m).unwrap_err().code, ErrorCode::UnsupportedFeature);
        assert_eq!(state(&a), before);
    }

    /// Verification audit, V2 (`docs/t12-verify-audit.md`): `verify()`'s
    /// `foreign_key_check` is not re-run after replay because the applier's
    /// connection enforces foreign keys on every insert. Shown directly: the
    /// pragma is on for the connection the applier writes through, and a
    /// dangling reference is refused inside the applier's transaction type.
    #[test]
    fn the_applier_connection_enforces_foreign_keys() {
        let a = SegmentApplier::new(base()).unwrap();
        let on: i64 = a
            .catalog
            .conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            on, 1,
            "foreign keys are enforced on the applier's connection"
        );
        let tx = a.catalog.conn.unchecked_transaction().unwrap();
        let e = tx
            .execute(
                "INSERT INTO namespace_ops (commit_seq, op_seq, op, path, file_version_id) \
                 VALUES (0, 99, 'put', X'7A', ?1)",
                [vec![0x99u8; 32]],
            )
            .unwrap_err();
        assert!(e.to_string().contains("FOREIGN KEY"), "{e}");
    }

    /// Q26 regression: a PUT of a version that exists nowhere (neither in
    /// the catalog nor in this delta) is `NAMESPACE_INVALID`, and nothing
    /// changes.
    #[test]
    fn a_put_of_an_unknown_version_is_refused_and_leaves_nothing() {
        let mut a = SegmentApplier::new(base()).unwrap();
        let before = state(&a);
        let e = a
            .apply(&delta(1, vec![], vec![], vec![put("x", 0x99)]))
            .unwrap_err();
        // Q26 (decided 2026-10-04): NAMESPACE_INVALID, from the explicit
        // reference check, before any row is written.
        assert_eq!(e.code, ErrorCode::NamespaceInvalid, "{e}");
        assert_eq!(state(&a), before);
    }

    /// Verification audit, V3 (`docs/t12-verify-audit.md`): `verify()`
    /// re-checks every file version's extents; replay instead checks each
    /// version it introduces (existing ones were verified with the base
    /// image, and versions are immutable). Shown through `apply` on
    /// adversarial input: extents that do not cover the declared logical
    /// length, and an extent past the end of its chunk, are refused with
    /// `EXTENT_INVALID`, and nothing changes.
    #[test]
    fn an_introduced_version_with_invalid_extents_is_refused_and_leaves_nothing() {
        let mut long = file(3, 2);
        long.version.logical_len += 1;
        let mut past_end = file(4, 2);
        if let Some(e) = past_end.extents.first_mut() {
            if let ExtentSource::Chunk { chunk_offset, .. } = &mut e.source {
                // The helper's chunk decodes to 100 bytes: 41 + 60 > 100.
                *chunk_offset = 41;
            }
        }
        for (what, v) in [("short extents", long), ("past the chunk end", past_end)] {
            let mut a = SegmentApplier::new(base()).unwrap();
            let before = state(&a);
            let id = v.version.id;
            let e = a
                .apply(&delta(
                    1,
                    vec![chunk(2)],
                    vec![v],
                    vec![NamespaceOp::Put {
                        path: p("x"),
                        version: id,
                    }],
                ))
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::ExtentInvalid, "{what}: {e}");
            assert_eq!(state(&a), before, "{what}: nothing applied");
        }
    }

    /// Q26 positive: a PUT naming a version this same delta introduces is
    /// valid (validity is judged against the post-delta set of versions),
    /// whatever the order of the delta's version list and operations; the
    /// same version can also be put at a second path in the same delta.
    #[test]
    fn a_put_of_a_version_introduced_by_the_same_delta_is_valid() {
        let mut a = SegmentApplier::new(base()).unwrap();
        a.apply(&delta(
            1,
            vec![chunk(2)],
            vec![file(3, 2), dir(4)],
            vec![put("d", 4), put("d/x", 3), put("y", 3)],
        ))
        .unwrap();
        let (_, ns, head) = state(&a);
        assert_eq!(head, 1);
        assert_eq!(ns.get(&p("d/x")).unwrap().version, vid(3));
        assert_eq!(ns.get(&p("y")).unwrap().version, vid(3));
        assert_eq!(ns.get(&p("d")).unwrap().version, vid(4));
    }

    /// Q26: the check runs before any row is written, so an unknown
    /// reference placed after valid operations and new rows still leaves
    /// nothing, with the same code.
    #[test]
    fn an_unknown_reference_after_valid_work_leaves_nothing() {
        let mut a = SegmentApplier::new(base()).unwrap();
        let before = state(&a);
        let e = a
            .apply(&delta(
                1,
                vec![chunk(2)],
                vec![file(3, 2)],
                vec![put("x", 3), put("z", 0x99)],
            ))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NamespaceInvalid, "{e}");
        assert!(e.message.contains("operation 1"), "{e}");
        assert_eq!(state(&a), before);
    }
}
