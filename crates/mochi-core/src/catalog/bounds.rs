//! Declared work bounds for applying one delta manifest (spec Annex B.2
//! D10.5, "Bounded work"; checklist T13).
//!
//! D10.5 requires each operation kind to declare a constant maximum number
//! of logical row mutations and a constant number of validation lookups, so
//! that the work of a replay is bounded by the manifest (and through it by
//! the B.2.3 limits), never by catalog state. The constants below are that
//! declaration for [`SegmentApplier::apply`](super::SegmentApplier::apply).
//! **Draft values**: the spec says R3 fixes them; until R3 exists they are
//! this implementation's declaration, recorded in the decision log (T13).
//!
//! # What is counted
//!
//! * **Catalog lookups**: SQL statements that read the catalog. Each is a
//!   point query or a range on a primary-key prefix; none scans a table
//!   (the tests check SQLite's own full-scan counter is zero).
//! * **Catalog row mutations**: rows inserted, updated, or deleted, as
//!   SQLite counts them (`total_changes`). Replay only inserts.
//! * **Namespace probes**: lookups in the in-memory namespace snapshot (a
//!   point lookup, or a probe of a directory's descendant range).
//! * **Namespace entry mutations**: entries inserted, replaced, or removed.
//!
//! Not counted: lookups in maps built from the manifest itself (bounded by
//! the manifest, not the catalog), transaction control (`BEGIN`, `COMMIT`),
//! and restoring the namespace from its undo log after a failure (at most
//! one entry per operation). Database cost *per* mutation (B-tree depth,
//! index maintenance) is measured under G3, not claimed here (D10.5).
//!
//! # Derivation
//!
//! `PUT(path, v)`: one `namespace_ops` row; one entry set at `path`; at most
//! one catalog lookup, for the kind of a `v` that this manifest does not
//! introduce. Validation of the completed state probes `path`, its parent,
//! and (for a file or an absent path) its descendant range; a failure adds
//! the two probes that name the first orphan. So at most 5 probes.
//!
//! `DELETE(path)`: one `namespace_ops` row; one entry removed; no catalog
//! lookup. Validation of an absent path probes it and its descendant
//! range, plus the two orphan probes on failure: at most 4.
//!
//! Validation runs once per *touched path*, not per operation. A path's
//! final state is present only if its last operation is a `PUT`, so
//! charging each path to its last operation keeps every path within that
//! operation's maximum, and the manifest within the sum over operations.
//!
//! Neither operation mutates a row it does not name: directory delete is
//! non-recursive (C3), and there are no cascading foreign-key actions or
//! triggers in the catalog (`schema::DDL`).
//!
//! Introductions (the manifest's chunk and file-version arrays) are not
//! operations, but their work is declared the same way, per listed item, so
//! the whole manifest's work is a function of the manifest alone
//! ([`manifest_work_bound`]).

use crate::manifest::Manifest;

use super::namespace::NamespaceOp;

/// Work counted for one operation, item, or manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Work {
    pub catalog_lookups: u64,
    pub catalog_row_mutations: u64,
    pub namespace_probes: u64,
    pub namespace_entry_mutations: u64,
}

impl Work {
    /// Field-wise sum, saturating (a bound that saturates is still a bound).
    #[must_use]
    pub const fn plus(self, o: Work) -> Work {
        Work {
            catalog_lookups: self.catalog_lookups.saturating_add(o.catalog_lookups),
            catalog_row_mutations: self
                .catalog_row_mutations
                .saturating_add(o.catalog_row_mutations),
            namespace_probes: self.namespace_probes.saturating_add(o.namespace_probes),
            namespace_entry_mutations: self
                .namespace_entry_mutations
                .saturating_add(o.namespace_entry_mutations),
        }
    }

    /// `self` scaled by `n`, saturating.
    #[must_use]
    pub const fn times(self, n: u64) -> Work {
        Work {
            catalog_lookups: self.catalog_lookups.saturating_mul(n),
            catalog_row_mutations: self.catalog_row_mutations.saturating_mul(n),
            namespace_probes: self.namespace_probes.saturating_mul(n),
            namespace_entry_mutations: self.namespace_entry_mutations.saturating_mul(n),
        }
    }

    /// Every field of `self` is at most the same field of `bound`.
    pub const fn within(self, bound: Work) -> bool {
        self.catalog_lookups <= bound.catalog_lookups
            && self.catalog_row_mutations <= bound.catalog_row_mutations
            && self.namespace_probes <= bound.namespace_probes
            && self.namespace_entry_mutations <= bound.namespace_entry_mutations
    }
}

/// Maximum work of one `PUT`.
pub const PUT_MAX: Work = Work {
    catalog_lookups: 1,
    catalog_row_mutations: 1,
    namespace_probes: 5,
    namespace_entry_mutations: 1,
};

/// Maximum work of one `DELETE`.
pub const DELETE_MAX: Work = Work {
    catalog_lookups: 0,
    catalog_row_mutations: 1,
    namespace_probes: 4,
    namespace_entry_mutations: 1,
};

/// Fixed work per manifest, whatever it lists: the `commits` row.
pub const MANIFEST_FIXED_MAX: Work = Work {
    catalog_lookups: 0,
    catalog_row_mutations: 1,
    namespace_probes: 0,
    namespace_entry_mutations: 0,
};

/// Per introduced chunk: the existence check (one statement when absent,
/// two when present, which then fails), and its `objects` and `chunks` rows.
pub const CHUNK_MAX: Work = Work {
    catalog_lookups: 2,
    catalog_row_mutations: 2,
    namespace_probes: 0,
    namespace_entry_mutations: 0,
};

/// Per dependency of an introduced chunk: its `chunk_dependencies` row.
pub const CHUNK_DEPENDENCY_MAX: Work = Work {
    catalog_lookups: 0,
    catalog_row_mutations: 1,
    namespace_probes: 0,
    namespace_entry_mutations: 0,
};

/// Per introduced chunk with a recorded location: its `object_locations` row.
pub const CHUNK_LOCATION_MAX: Work = CHUNK_DEPENDENCY_MAX;

/// Per introduced file version: the existence check (as for a chunk) and its
/// `file_versions` row.
pub const FILE_VERSION_MAX: Work = CHUNK_DEPENDENCY_MAX.plus(Work {
    catalog_lookups: 2,
    catalog_row_mutations: 0,
    namespace_probes: 0,
    namespace_entry_mutations: 0,
});

/// Per extent of an introduced file version: its `file_extents` row, and at
/// most one lookup of the length of the chunk it names.
pub const EXTENT_MAX: Work = Work {
    catalog_lookups: 1,
    catalog_row_mutations: 1,
    namespace_probes: 0,
    namespace_entry_mutations: 0,
};

/// The declared maximum for one operation.
pub const fn op_max(op: &NamespaceOp) -> Work {
    match op {
        NamespaceOp::Put { .. } => PUT_MAX,
        NamespaceOp::Delete { .. } => DELETE_MAX,
    }
}

/// The declared maximum work of applying `delta`: a function of the
/// manifest only, never of the catalog it is applied to.
pub fn manifest_work_bound(delta: &Manifest) -> Work {
    let mut w = MANIFEST_FIXED_MAX;
    for c in &delta.chunks {
        w = w
            .plus(CHUNK_MAX)
            .plus(CHUNK_DEPENDENCY_MAX.times(c.record.dependencies.len() as u64));
        if c.location.is_some() {
            w = w.plus(CHUNK_LOCATION_MAX);
        }
    }
    for v in &delta.file_versions {
        w = w
            .plus(FILE_VERSION_MAX)
            .plus(EXTENT_MAX.times(v.extents.len() as u64));
    }
    for op in &delta.ops {
        w = w.plus(op_max(op));
    }
    w
}
