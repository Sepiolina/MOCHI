# T12 post-replay verification audit

**Status: accepted 2026-10-04 [delegated]** (see "Sign-off" at the end).
Question:
may a catalog built by replay skip a final `Catalog::verify()`?

**Rule applied** (review 2026-10-03): no unconditional `verify()` without a
demonstrated gap; map every check first; add targeted validation and
regression tests for any required invariant left uncovered. Query-only
enforcement (Q25) is about writes, not validity, and is **not** used as
evidence anywhere below.

## 1. Setting

A replayed catalog is built in two stages.

1. **Base.** Checkpoint *b*'s image is opened by `open_image_inner`
   (`catalog/mod.rs`). Before any delta is applied, that runs an exact schema
   comparison and the **full `verify()`** on the base. So every invariant
   below holds for the base state.
2. **Deltas.** `SegmentApplier::apply` (`catalog/apply.rs`) applies delta
   manifests *b*+1 … *h*, one SQLite transaction each, on a connection
   hardened by `harden` (`SQLITE_DBCONFIG_DEFENSIVE`, `foreign_keys = ON`).
   Each delta was already hash-verified, decoded under the B.2.3 limits,
   and identity-bound before `apply` sees it.

So the question is narrower than "is the catalog valid": **can a delta
take a valid catalog to an invalid one without being refused?** Each row
below answers it for one invariant.

## 2. What `verify()` checks

`Catalog::verify` (`catalog/mod.rs`) runs four checks:

| # | Check |
|---|---|
| V1 | `PRAGMA integrity_check` |
| V2 | `PRAGMA foreign_key_check` |
| V3 | `check_file_version` on every file version |
| V4 | `Catalog::replay(None)` over all commits, then `validate_all` |

Outside `verify()`, the schema enforces STRICT column types and every
`CHECK` constraint on every insert, unconditionally.

## 3. Invariant by invariant

### V1. SQLite page and b-tree structure

* **Enforced during replay by:** construction. Replay changes the database
  only through SQL statements in defensive mode; archive input never
  reaches the page layer as bytes. SQLite maintains its own structure on
  every statement.
* **Tests:** none can construct a violation through SQL (that is the
  point). Empirical: the oracle runs full `verify()`, including
  `integrity_check`, on every replayed catalog (`t12_oracle.rs`, B side).
* **Why no gap:** the only way to violate V1 during replay is a SQLite
  defect. Detecting SQLite defects is outside the format's threat model
  (§6 concerns archive bytes, storage, and operators). This is the one
  invariant where the argument rests on a dependency's correctness rather
  than on a MOCHI check; it is stated, not hidden.

### V2. Every reference resolves (8 foreign keys)

The DDL (`catalog/schema.rs`) declares eight references, none `DEFERRABLE`:

| Reference | Enforced during replay by |
|---|---|
| `commits.parent_seq → commits.seq` | FK, immediate; and `insert_commit_rows` writes parent = previous head, after `check_shape` checks sequence order |
| `object_locations.object_id → objects` | FK, immediate; `insert_object_rows` writes `objects` → `chunks` → `chunk_dependencies` → `object_locations`, so every referencing row follows the row it references |
| `chunks.object_id → objects` | FK, immediate; same writer, same order |
| `chunk_dependencies.object_id → chunks` | FK, immediate; same writer, after the chunk row (replay does write this table, through `insert_object_rows`) |
| `file_extents.file_version_id → file_versions` | FK, immediate; `insert_file_version_rows` writes the version first |
| `file_extents.chunk_id → chunks` | FK, immediate; **and** `check_file_version` resolves every chunk before any extent row (`ExtentDefect::UnknownChunk`) |
| `namespace_ops.commit_seq → commits` | FK, immediate; commit row written before its operations |
| `namespace_ops.file_version_id → file_versions` | FK, immediate; **and** the Q26 reference check in `apply`, before any row is written (`NAMESPACE_INVALID`) |

* **Tests:**
  `the_applier_connection_enforces_foreign_keys` (the pragma is on for the
  applier's connection; a dangling `namespace_ops` reference is refused
  inside the applier's transaction type);
  `a_put_of_an_unknown_version_is_refused_and_leaves_nothing`,
  `an_unknown_reference_after_valid_work_leaves_nothing`,
  `q26_put_of_an_unknown_version_is_namespace_invalid` (open path);
  `an_introduced_version_with_invalid_extents_is_refused_and_leaves_nothing`
  (unknown/out-of-range chunk).
* **Why no gap:** immediate foreign keys reject a dangling reference at
  the statement that creates it, and the transaction is then dropped, so
  the delta leaves nothing. A post-replay `foreign_key_check` would
  re-check the same eight relations and could only find a violation that
  immediate enforcement let through, which SQLite's semantics exclude.
  The two references that archive input most directly controls (chunk and
  file version) are additionally checked by MOCHI code before any row.

### V3. File-version rules (`check_file_version`)

| Sub-invariant | Enforced during replay by |
|---|---|
| Directory: content hash absent | **Decoder** (`manifest.rs`: "content hash must be null exactly for a directory"); schema `CHECK`; `check_file_version` |
| File: content hash present | Same three |
| Directory: logical length 0, no extents | **Decoder and encoder** (`Manifest::check_structure`, since Q27, 2026-10-04; schema violation, `RECORD_INVALID`); then `check_file_version`, before the version's rows |
| Extents (§10.3): ordinals contiguous, no gap, no overlap, lengths sum to the logical length, chunk range within the chunk's decoded length, chunk exists, no empty extent, no overflow | `check_file_version` → `validate_extents` (`ExtentDefect`, all eight: `OrdinalSequence`, `Gap`, `Overlap`, `LengthMismatch`, `OutOfRange`, `UnknownChunk`, `Empty`, `Overflow`) |
| Existing versions stay valid | Versions are immutable, and a delta can never replace one: reintroducing an existing ID is refused (`check_introductions`, decision 18), identical or not |

* **Tests:**
  `an_introduced_version_with_invalid_extents_is_refused_and_leaves_nothing`
  (short extents; extent past the chunk end; `EXTENT_INVALID`, nothing
  changes). Removing the `check_file_version` call from `apply` fails
  **exactly this test** (mutation run 2026-10-03), so it is the test that
  guards V3 during replay. Each defect class: `extent::tests` (C3).
  Immutability: `reintroducing_an_id_is_record_invalid_identical_or_not`.
* **Why no gap:** every version that exists after the delta either
  existed in the verified base (and cannot have been replaced), or was
  introduced by the delta and passed `check_file_version` before its rows
  were written. A post-replay V3 would re-run the same function on the
  same immutable records.
* **History:** until 2026-10-03 this row had a **test** gap: the check
  existed, but removing it failed no test. Closed then.

### V4. Namespace validity

| Sub-invariant | Enforced during replay by |
|---|---|
| Commit chain linear, sequences contiguous | `check_shape` (sequence = head + 1, parent = head) before any row; commit row parent = previous head |
| Every PUT names an existing version | Q26 reference check (before rows) |
| DELETE names a present path | `apply_commit_staged` (`DeleteMissing`) |
| Every entry's parent exists and is a directory, in the completed state | `apply_commit_staged`, incrementally: every touched path, plus the descendants of deleted or file-replaced paths (`MissingParent`, `ParentNotDirectory`) |

* **Why incremental is enough:** the base snapshot is valid (it passed
  `validate_all` inside `verify()` at open). A commit can only invalidate
  an entry it touches, or an entry below a path it deletes or turns into a
  file; those are exactly the paths `apply_commit_staged` re-validates.
  The argument is in `namespace.rs`; the C3 property test compares
  incremental against full re-validation on generated histories.
* **Tests:** `semantic_failure_at_any_position_leaves_nothing` (each
  invalid operation first, middle, and last), `temporarily_invalid_but_completed_valid_is_accepted`,
  the namespace property test (C3), the Q26 tests above; the oracle
  (equal namespaces to the full-checkpoint archive, every commit).
* **Why no gap:** a post-replay `replay(None)` + `validate_all` would
  re-derive the namespace that the applier already validated commit by
  commit against completed state, starting from a validated base.

### Outside `verify()`, but required at the end of replay

| Invariant | Enforced by |
|---|---|
| The catalog materializes the head | `replay_segment`: `applier.head() == h`; `open_at`: `catalog.head_commit() == h` |
| Archive ID metadata is the base's | The applier never writes `archive_meta`; `open_checkpoint_catalog` checks the base's |
| No partial delta survives | One transaction per delta; 18 injected fault points, each leaves nothing (`injected_failure_at_every_point_leaves_nothing`) |

## 4. Conclusion

Every check `verify()` makes is either re-established per delta before any
row is written (V3, V4, and the two archive-controlled references in V2),
enforced by SQLite at the statement that would violate it (V2), or
excluded by construction (V1, resting on SQLite's correctness). A
post-replay `verify()` would repeat these checks over the same immutable
records and could not find a violation the above lets through, short of a
SQLite defect. **No `verify()` is added after replay.**

## 5. Findings (classification only; outcomes are already correct)

* **F1 (resolved, Q26, 2026-10-04).** A PUT of an unknown version was
  refused as `CATALOG_INVALID` because the foreign key fired first. Now an
  explicit check before any row returns `NAMESPACE_INVALID`, judged against
  the post-delta set of versions.
* **F2 (resolved, Q27, 2026-10-04 [delegated]).** A directory version
  with a nonzero logical length or with extents passed the manifest decoder
  (which checked only the content hash for directories) and was refused by
  `check_file_version` with `INVALID_ARGUMENT`. Now `Manifest::check_structure`
  enforces the schema's rule on encode and decode: `RECORD_INVALID`, found
  before any catalog code runs.

## 6. Not claimed by this audit

* That referenced objects' bytes lie before their commit frame or
  hash-verify. These are read-path checks, made when an object is loaded,
  and not part of `verify()`.
* Anything about catalogs not built by replay (the writer's catalog, or
  T15 checkpoint adoption).

## 7. Sign-off

**Accepted 2026-10-04, under the owner's delegation of review decisions
(decision log, "Decision authority").** This is a **self-review by the
author, not an independent review**, and is recorded as such.

Method: every factual claim in sections 1–5 was checked against the code
before acceptance, not assumed. In particular, verified on 2026-10-04:

* both `open_image` and `open_image_writable` (the path replay uses) go
  through `open_image_inner`, which runs the exact schema comparison and
  the full `verify()` on the base;
* the 8 foreign keys and their non-`DEFERRABLE` (immediate) declarations,
  read from the DDL;
* `insert_object_rows` statement order (above);
* `check_shape` checks sequence = head + 1 and parent = head;
* the 8 `ExtentDefect` classes (an earlier draft wrongly listed two
  `ExtentSource` variants among them; corrected before acceptance);
* 18 fault points (6 + 3 + 2 + 1 + 5 + 1) in
  `injected_failure_at_every_point_leaves_nothing`;
* each cited test exists, and the V3 and Q26 guards are mutation-checked.

Residual risk, accepted: V1 rests on SQLite's own correctness (section 3).
Recommended, not required for G2: an independent reviewer re-reads
sections 3–4 before the format is declared interoperable (§28).

