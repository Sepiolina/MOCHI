//! T13 (spec Annex B.2 D10.5): every operation stays within its declared
//! maximum of row mutations and validation lookups, and the work of a
//! manifest depends on the manifest only, never on the catalog.
//!
//! Catalog work is observed with SQLite's own instruments, not with
//! counters in the code under test: the statement profiler (every statement
//! run, and its full-scan step count) and `total_changes`, plus a per-table
//! row count as a cross-check. Namespace work is counted by the applier
//! (`last_namespace_probes`, `last_namespace_mutations`) and cross-checked
//! by diffing the namespace.

use std::cell::RefCell;
use std::collections::BTreeSet;

use rusqlite::trace::{TraceEvent, TraceEventCodes};
use rusqlite::StatementStatus;

use super::*;
use crate::catalog::bounds::{self, manifest_work_bound, Work};
use crate::catalog::CatalogLimits;

thread_local! {
    /// (SQL, full-scan steps) of every statement profiled on this thread.
    static PROFILED: RefCell<Vec<(String, i32)>> = const { RefCell::new(Vec::new()) };
}

fn on_trace(ev: TraceEvent<'_>) {
    if let TraceEvent::Profile(stmt, _) = ev {
        let scan = stmt.get_status(StatementStatus::FullscanStep);
        PROFILED.with(|p| p.borrow_mut().push((stmt.sql().into_owned(), scan)));
    }
}

const TABLES: [&str; 8] = [
    "commits",
    "objects",
    "object_locations",
    "chunks",
    "chunk_dependencies",
    "file_versions",
    "file_extents",
    "namespace_ops",
];

fn row_counts(c: &Catalog) -> Vec<i64> {
    TABLES
        .iter()
        .map(|t| {
            c.conn
                .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        })
        .collect()
}

struct Observed {
    result: Result<()>,
    work: Work,
    /// Statements other than transaction control.
    statements: Vec<String>,
    /// Rows added per table (`TABLES` order).
    rows_added: Vec<i64>,
    /// Paths whose namespace entry differs before and after.
    changed_paths: BTreeSet<ArchivePath>,
}

/// Apply `m`, observing everything. Asserts the invariants that hold for
/// every apply: no statement scans a table, replay only inserts, and every
/// insert adds exactly one row.
fn observe(a: &mut SegmentApplier, m: &Manifest) -> Observed {
    let ns_before = a.namespace.clone();
    let rows_before = row_counts(&a.catalog);
    let changes_before = a.catalog.conn.total_changes();

    PROFILED.with(|p| p.borrow_mut().clear());
    a.catalog
        .conn
        .trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(on_trace));
    let result = a.apply(m);
    a.catalog.conn.trace_v2(TraceEventCodes::empty(), None);
    let profiled = PROFILED.with(|p| std::mem::take(&mut *p.borrow_mut()));

    let mutations = a.catalog.conn.total_changes() - changes_before;
    let rows_after = row_counts(&a.catalog);

    let mut statements = Vec::new();
    let (mut selects, mut inserts) = (0u64, 0u64);
    for (sql, scan) in profiled {
        assert_eq!(scan, 0, "statement scanned a table: {sql}");
        let verb = sql
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match verb.as_str() {
            "BEGIN" | "COMMIT" | "ROLLBACK" => continue,
            "SELECT" => selects += 1,
            "INSERT" => inserts += 1,
            other => panic!("replay ran a {other} statement: {sql}"),
        }
        statements.push(sql);
    }
    assert_eq!(
        inserts, mutations,
        "every insert adds exactly one row, and nothing else changes a row"
    );
    let rows_added: Vec<i64> = rows_after
        .iter()
        .zip(&rows_before)
        .map(|(a, b)| a - b)
        .collect();
    if result.is_ok() {
        assert_eq!(rows_added.iter().sum::<i64>() as u64, mutations);
    } else {
        assert!(
            rows_added.iter().all(|&n| n == 0),
            "a failed apply left rows"
        );
    }

    let mut changed_paths: BTreeSet<ArchivePath> = BTreeSet::new();
    for (path, e) in a.namespace.iter() {
        if ns_before.get(path) != Some(e) {
            changed_paths.insert(path.clone());
        }
    }
    for (path, _) in ns_before.iter() {
        if a.namespace.get(path).is_none() {
            changed_paths.insert(path.clone());
        }
    }

    Observed {
        result,
        work: Work {
            catalog_lookups: selects,
            catalog_row_mutations: mutations,
            namespace_probes: a.last_namespace_probes(),
            namespace_entry_mutations: a.last_namespace_mutations(),
        },
        statements,
        rows_added,
        changed_paths,
    }
}

/// Every path an operation of `m` names.
fn named_paths(m: &Manifest) -> BTreeSet<ArchivePath> {
    m.ops.iter().map(|op| op.path().clone()).collect()
}

/// `base()` plus `d/s` (directory, version 3) and `d/s/x` (file, version 4)
/// at commit 1, so that there is a nested non-empty directory.
fn nested() -> SegmentApplier {
    let mut a = SegmentApplier::new(base()).unwrap();
    a.apply(&delta(
        1,
        vec![],
        vec![dir(3), file(4, 1)],
        vec![put("d/s", 3), put("d/s/x", 4)],
    ))
    .unwrap();
    a
}

#[test]
fn t13_each_operation_stays_within_its_declared_maximum_and_reaches_it() {
    // (name, manifest at commit 2 over `nested()`, expected to apply)
    let cases: Vec<(&str, Manifest, bool)> = vec![
        // PUT of an existing version (one kind lookup) as a new file under
        // a nested directory: path, parent, descendant range.
        (
            "put existing version",
            delta(2, vec![], vec![], vec![put("d/s/y", 2)]),
            true,
        ),
        // PUT replacing a file in place.
        (
            "put replace",
            delta(2, vec![], vec![], vec![put("d/f", 4)]),
            true,
        ),
        // PUT of a version this manifest introduces: no catalog lookup.
        (
            "put introduced",
            delta(2, vec![], vec![dir(9)], vec![put("t", 9)]),
            true,
        ),
        // PUT of a file over a non-empty directory: the failing worst case
        // (path, parent, descendants, and the two orphan probes).
        (
            "put over non-empty dir",
            delta(2, vec![], vec![], vec![put("d/s", 2)]),
            false,
        ),
        // PUT under a file: parent is not a directory.
        (
            "put under a file",
            delta(2, vec![], vec![], vec![put("d/f/z", 2)]),
            false,
        ),
        (
            "delete file",
            delta(2, vec![], vec![], vec![del("d/s/x")]),
            true,
        ),
        // DELETE of a non-empty directory: non-recursive, so it fails with
        // the two orphan probes; nothing below it is touched.
        (
            "delete non-empty dir",
            delta(2, vec![], vec![], vec![del("d/s")]),
            false,
        ),
        (
            "delete absent",
            delta(2, vec![], vec![], vec![del("nope")]),
            false,
        ),
    ];
    let mut reached_put = Work::default();
    let mut reached_delete = Work::default();
    for (name, m, ok) in cases {
        let mut a = nested();
        let o = observe(&mut a, &m);
        assert_eq!(o.result.is_ok(), ok, "{name}: {:?}", o.result);
        let bound = manifest_work_bound(&m);
        assert!(
            o.work.within(bound),
            "{name}: {:?} exceeds {bound:?}",
            o.work
        );
        assert!(
            o.work.within(
                bounds::MANIFEST_FIXED_MAX
                    .plus(bounds::op_max(&m.ops[0]))
                    .plus(manifest_work_bound(&delta(
                        2,
                        m.chunks.clone(),
                        m.file_versions.clone(),
                        vec![]
                    )))
            ),
            "{name}: over the operation's own maximum"
        );
        assert!(o.changed_paths.is_subset(&named_paths(&m)), "{name}");

        // The operation's own share: remove the manifest's `commits` row
        // (every case reaches the inserts; namespace faults are found
        // after them) and the one introduction ("put introduced": a
        // directory, one existence lookup and one row).
        let mut own = o.work;
        own.catalog_row_mutations -= bounds::MANIFEST_FIXED_MAX.catalog_row_mutations;
        if name == "put introduced" {
            own.catalog_lookups -= 1;
            own.catalog_row_mutations -= 1;
        }
        let reached = match m.ops[0] {
            NamespaceOp::Put { .. } => &mut reached_put,
            NamespaceOp::Delete { .. } => &mut reached_delete,
        };
        *reached = Work {
            catalog_lookups: reached.catalog_lookups.max(own.catalog_lookups),
            catalog_row_mutations: reached.catalog_row_mutations.max(own.catalog_row_mutations),
            namespace_probes: reached.namespace_probes.max(own.namespace_probes),
            namespace_entry_mutations: reached
                .namespace_entry_mutations
                .max(own.namespace_entry_mutations),
        };
    }
    // The declared maxima are attained, so they are not padded guesses.
    assert_eq!(reached_put, bounds::PUT_MAX);
    assert_eq!(reached_delete, bounds::DELETE_MAX);
}

/// A base checkpoint like `base()`, plus directory `big` holding `n` files
/// (all version 2 by reference) and `n` extra unreferenced chunks.
fn base_with(n: u32) -> Catalog {
    let mut c = Catalog::new_working().unwrap();
    let ch = chunk(1);
    c.insert_object(&ch.record, ch.location).unwrap();
    for v in [dir(1), file(2, 1)] {
        c.insert_file_version(&v.version, &v.extents).unwrap();
    }
    let mut big = dir(5);
    big.version.id = vid(5);
    c.insert_file_version(&big.version, &big.extents).unwrap();
    let mut ops = vec![put("d", 1), put("d/f", 2), put("big", 5)];
    for i in 0..n {
        let mut extra = chunk(200);
        let mut id = [0xA0; 32];
        id[..4].copy_from_slice(&i.to_be_bytes());
        extra.record.id = ObjectId::from_bytes(id);
        c.insert_object(&extra.record, extra.location).unwrap();
        ops.push(NamespaceOp::Put {
            path: p(&format!("big/f{i:05}")),
            version: vid(2),
        });
    }
    c.append_commit(&Commit {
        seq: 0,
        parent: None,
        ops,
    })
    .unwrap();
    let img = c.publish().unwrap();
    Catalog::open_image_writable(img.as_bytes(), &CatalogLimits::default()).unwrap()
}

/// Every row kind and both operations, including a rename, a replace, and
/// work inside the large directory.
fn mixed_delta() -> Manifest {
    delta(
        1,
        vec![chunk(2), chunk(3)],
        vec![dir(10), file(11, 2), file(12, 3)],
        vec![
            put("e", 10),
            put("e/g", 11),
            del("d/f"),
            put("d/h", 12),
            put("e/f2", 2),
            put("big/new", 2),
            put("big/f00000", 12),
            del("big/f00000"),
        ],
    )
}

#[test]
fn t13_work_is_independent_of_catalog_size() {
    let mut results = Vec::new();
    for n in [1u32, 3000] {
        let mut a = SegmentApplier::new(base_with(n)).unwrap();
        assert!(a.namespace().len() > n as usize);
        let m = mixed_delta();
        let o = observe(&mut a, &m);
        o.result.unwrap();
        assert!(o.work.within(manifest_work_bound(&m)), "{:?}", o.work);
        assert!(o.changed_paths.is_subset(&named_paths(&m)));
        results.push((o.work, o.statements, o.rows_added));
    }
    assert_eq!(
        results[0], results[1],
        "the same manifest did different work on a larger catalog"
    );
}

#[test]
fn t13_no_operation_mutates_a_row_it_does_not_name() {
    // Deleting an empty directory touches its own entry and adds exactly
    // one operation row and one commit row; nothing else.
    let mut a = nested();
    let m = delta(2, vec![], vec![], vec![del("d/s/x"), del("d/s")]);
    let o = observe(&mut a, &m);
    o.result.unwrap();
    assert_eq!(o.changed_paths, named_paths(&m));
    let expect: Vec<i64> = TABLES
        .iter()
        .map(|t| match *t {
            "commits" => 1,
            "namespace_ops" => 2,
            _ => 0,
        })
        .collect();
    assert_eq!(o.rows_added, expect);
    assert_eq!(o.work.namespace_entry_mutations, 2);

    // Deleting a non-empty directory alone is refused, not cascaded.
    let mut a = nested();
    let before = state(&a);
    let o = observe(&mut a, &delta(2, vec![], vec![], vec![del("d/s")]));
    assert_eq!(o.result.unwrap_err().code, ErrorCode::NamespaceInvalid);
    assert!(o.changed_paths.is_empty());
    assert_eq!(state(&a), before);
}

/// A small deterministic generator (no new dependency): xorshift64.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn t13_generated_manifests_stay_within_the_bound_whether_or_not_they_apply() {
    let paths = [
        "d", "d/f", "d/s", "d/s/x", "d/s/y", "e", "e/g", "t", "d/f/z",
    ];
    let mut rng = Rng(0x7131_3000_5EED_0001);
    let (mut applied, mut refused) = (0, 0);
    for round in 0..300u64 {
        let mut a = nested();
        let mut seq = 2;
        for _ in 0..3 {
            // Introductions: fresh IDs per round and commit.
            let tag = (round * 7 + seq) as u8;
            let new_chunk = rng.below(2) == 0;
            let mut chunks = vec![];
            let mut versions = vec![];
            // Byte 0 marks the kind and byte 1 the commit, so no fresh ID
            // equals a base ID (all bytes equal) or an earlier commit's.
            let fresh = |kind: u8| {
                let mut id = [0u8; 32];
                id[0] = kind;
                id[1] = tag;
                id
            };
            if new_chunk {
                let mut c = chunk(9);
                c.record.id = ObjectId::from_bytes(fresh(0xC0));
                chunks.push(c);
            }
            let mut dv = dir(9);
            dv.version.id = FileVersionId::from_bytes(fresh(0xD0));
            let mut fv = file(9, 1);
            fv.version.id = FileVersionId::from_bytes(fresh(0xF0));
            let (dvid, fvid) = (dv.version.id, fv.version.id);
            versions.push(dv);
            versions.push(fv);
            let mut ops = vec![];
            for _ in 0..rng.below(6) + 1 {
                let path = p(paths[rng.below(paths.len() as u64) as usize]);
                ops.push(match rng.below(5) {
                    0 => NamespaceOp::Delete { path },
                    1 => NamespaceOp::Put {
                        path,
                        version: dvid,
                    },
                    2 => NamespaceOp::Put {
                        path,
                        version: fvid,
                    },
                    3 => NamespaceOp::Put {
                        path,
                        version: vid(2),
                    },
                    _ => NamespaceOp::Put {
                        path,
                        version: vid(1),
                    },
                });
            }
            let mut m = delta(seq, chunks, versions, ops);
            m.canonicalize();
            let o = observe(&mut a, &m);
            let bound = manifest_work_bound(&m);
            assert!(
                o.work.within(bound),
                "round {round}, commit {seq}: {:?} exceeds {bound:?} for {:?}",
                o.work,
                m.ops
            );
            assert!(o.changed_paths.is_subset(&named_paths(&m)));
            if o.result.is_ok() {
                applied += 1;
                seq += 1;
            } else {
                refused += 1;
                assert!(o.changed_paths.is_empty());
            }
        }
    }
    // Both outcomes are exercised in quantity.
    assert!(
        applied > 100 && refused > 100,
        "applied {applied}, refused {refused}"
    );
}

#[test]
fn t13_control_the_profiler_sees_a_full_scan() {
    // The zero-full-scan assertion in `observe` can fail: the same probe on
    // a deliberate scan reports a nonzero count.
    let a = nested();
    PROFILED.with(|p| p.borrow_mut().clear());
    a.catalog
        .conn
        .trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(on_trace));
    let n: i64 = a
        .catalog
        .conn
        .query_row(
            "SELECT count(*) FROM file_versions WHERE logical_len >= 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    a.catalog.conn.trace_v2(TraceEventCodes::empty(), None);
    assert!(n > 0);
    let profiled = PROFILED.with(|p| std::mem::take(&mut *p.borrow_mut()));
    assert_eq!(profiled.len(), 1);
    assert!(profiled[0].1 > 0, "full scan not reported: {profiled:?}");
}
