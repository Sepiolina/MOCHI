# T16, T17, T15: implementation plan

**Status: plan, not started.** Written 2026-10-05 for the implementer. Work
order is **T16 → T17 → T15** (checklist, "What G2 still needs"). Each task
builds on the previous one: T17's "reads rebuild from the snapshot manifest"
uses T16's builder, and T15's adoption check uses T16's state model and plugs
into T17's damage assessment.

Normative sources: spec Annex B.2 **D10.7** (adoption), **D10.8** (baseline
recovery), **D10.9** (damage scope), D10.4/D10.6 (replay and segment rules,
already implemented), D11 (identity binding), D12 (descriptor), §11, §11.1,
§18.1, §20.3–§20.5. Tasks and gate: `b2-implementation-checklist.md` rows
T15–T17, gate **G2**.

**Nothing here changes the wire format.** No golden fixture should change.
If one does, stop and explain the diff before going on (AGENTS.md).

---

## 0. Ground rules for this work

Read `AGENTS.md` first. The rules that matter most here:

* Library code: no `unwrap`/`expect`/`panic!` on paths archive bytes can
  reach. Return `MochiError` with a stable code. Use checked arithmetic on
  archive-derived values.
* All I/O in `mochi-core` goes through `ReadStorage`/`Storage`.
* **Hash before parse**: every object is loaded with `load_verified`
  (stored-object hash checked against the referencing record) before
  anything decodes it. A catalog image is hash-verified and envelope-checked
  before `Catalog::open_image`.
* **No search, no fallback to an earlier state** (D10.4, D10.6). T17 adds one
  alternative path: the *same* commit's snapshot manifest. It never uses an
  earlier checkpoint.
* Test hooks go behind `#[cfg(any(test, feature = "test-controls"))]`
  (review decision 14; `ci/check-invariants.sh` rule 9).
* Every new behaviour needs a test that fails without it. Record a
  **mutation check** for each key rule, as the checklist does for T11–T13:
  break the rule, name the tests that fail, restore the code, and confirm it
  is byte-identical (`md5sum`).
* Open questions go into the checklist's "Questions found during
  implementation" list, numbered from **Q29**, each with a provisional
  choice. The proposals below are pre-numbered. The owner delegated review
  decisions (checklist, "Decision authority"). Mark choices you make
  **[delegated]** and record them as self-review, not independent review.

Run after every step, and all of these before each push:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash ci/check-invariants.sh
```

Commit per sub-step group, with messages that cite the spec items. Do not put
model names in commits.

### 0.1 Shared test helpers (do first, separate commit)

`crates/mochi-testkit/tests/t11_t12_replay.rs` holds the helpers all three
tasks need: `Model`/`Step` with per-file attributes, `history()`, `write()`,
`append()`, `flip()`, `is_cp()`, `snapshot_attrs()`, the `Tracing` storage
wrapper, and `within()`. Move them, **unchanged**, into a new public module
`mochi_testkit::replay` (`crates/mochi-testkit/src/replay.rs`, exported from
`lib.rs`), and import them in `t11_t12_replay.rs`. Then add:

* `history_long() -> Vec<Step>`: `history()` followed by **two** more steps
  (9 commits). Do not edit `history()`: other tests slice it.
  Suggested extra steps: `m.file("i", deterministic_bytes(8, 40), attrs(0o620, 71)); m.end(70);`
  then `m.rename("h", "d/h"); m.dir("j", attrs(0o755, 81)); m.end(80);`.
* `wipe(bytes: &mut [u8], r: ObjectRef)`: zero the payload of a skippable
  frame (keep the 8-byte header so the file still walks). This models a
  deleted object, as in `c5_publication.rs`
  `a_destroyed_catalog_is_recovered_through_the_footer_verified_head`.
* `damage(bytes, r: ObjectRef)`: `flip` one byte at `r.offset + r.stored_len / 2`.
* `object_range(r: ObjectRef) -> (u64, u64)`.

Done when every existing test passes unchanged.

---

## 1. T16: baseline recovery (D10.8)

> Recovery from the checkpoint at *b* starts from snapshot manifest S(*b*)
> and applies the delta manifests after *b*. It needs **commit *b*'s
> record** … It needs **neither SQLite, nor delta manifest *b*, nor any
> earlier manifest.**

**Checklist DoD:** recovery succeeds with every image deleted, delta *b*
deleted, and all earlier manifests deleted.

### 1.1 Decisions to record

* **Q29: what "needs no SQLite" means. Proposed:** baseline recovery reads
  **no SQLite image**. Its inputs are commit records, S(*b*), and deltas.
  The *output* is still a catalog: a fresh in-memory working catalog
  (`Catalog::new_working`, O25), filled from S(*b*), with the deltas
  applied by the existing `SegmentApplier`. That keeps one implementation of
  the replay semantics (D10.4: introduction, reference, `NAMESPACE_INVALID`,
  atomicity, T13 bounds). Rejected alternative: a second, pure-Rust delta
  applier. It would duplicate every D10.4 rule, and the two could drift
  apart. The state comparisons that must not use SQLite (T15's snapshot
  half) use `AuthoritativeState::from_snapshot` (1.2), which never touches
  SQLite.
* **Q30: shape of `recover_with_trusted_head`. Proposed:** keep the existing
  manifest-chain recovery and run it first, because it is the only path that
  gives historical recovery before *b*. Run baseline recovery only when the
  chain does not reach the head. Return both results in a new struct (1.6).
  Both stay honest about scope. No search: baseline recovery uses only the
  head's own segment and never tries an earlier checkpoint.
* **Q30a: descriptor.** Baseline recovery is interpretation (replay), so D12
  applies: it reads and checks the descriptor as `open_at` does, and a bad
  descriptor fails with `DESCRIPTOR_INVALID`. (Explicit partial salvage is
  C8.)
* **Writer parameters** (`META_WRITER_PARAMS`) live only in the image's
  `archive_meta`, not in S(*b*). A catalog rebuilt from S(*b*) has
  `META_ARCHIVE_ID` but no writer parameters. That is fine for reading and
  recovery. It is why T17 refuses append when the base image is damaged
  (Q34).

### 1.2 `crate::state`: the authoritative-state model (new module)

`crates/mochi-core/src/state.rs`, `pub mod state;` in `lib.rs`. T16 tests
and T15 adoption both use it.

```rust
/// What a snapshot covers (D10.3): the namespace at `seq`, every version it
/// reaches with extents, every chunk those extents reach with its location,
/// and (when known) promised attributes. Historical rows are not included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeState {
    pub seq: u64,
    pub namespace: BTreeMap<ArchivePath, FileVersionId>,
    pub versions: BTreeMap<FileVersionId, (FileVersion, Vec<Extent>)>,
    pub chunks: BTreeMap<ObjectId, (ObjectRecord, Option<u64>)>,
    /// `None` = this representation does not carry attributes (a catalog
    /// image until C6, checklist Q6).
    pub attributes: Option<BTreeMap<FileVersionId, Attributes>>,
}

impl AuthoritativeState {
    /// From a decoded snapshot manifest. Pure: no SQLite. Refuses a delta
    /// manifest (`INVALID_ARGUMENT`).
    pub fn from_snapshot(m: &Manifest) -> Result<Self>;
    /// From a catalog at `seq`: `replay(Some(seq))`, then `file_version`,
    /// `object`, `object_location` for what the namespace reaches (the same
    /// projection as `Manifest::snapshot_from_catalog`). `attributes: None`.
    pub fn from_catalog(c: &Catalog, seq: u64) -> Result<Self>;
    pub fn with_attributes(self, a: BTreeMap<FileVersionId, Attributes>) -> Self;
    pub fn without_attributes(self) -> Self;
    /// Up to `max` human-readable differences (missing or extra path,
    /// version, chunk; changed record, extent, location, attribute), in a
    /// deterministic order. Empty iff equal.
    pub fn differences(&self, other: &Self, max: usize) -> Vec<String>;
}
```

`from_snapshot` must check that every namespace entry's version is listed
and every extent's chunk is listed. The decoder should already guarantee
this. Return `RECORD_INVALID` rather than panicking if it does not.

Unit tests (in `state.rs`):
* round trip: for a catalog built by hand, `from_snapshot(snapshot_from_catalog(c, …, attrs))`
  equals `from_catalog(c, seq).with_attributes(attrs)`;
* `differences` reports each of: a changed attribute, a missing entry, a
  changed chunk location, a changed extent; and is empty for equal states;
* `from_snapshot` on a delta manifest is `INVALID_ARGUMENT`.

### 1.3 `catalog_from_snapshot` (recovery.rs)

```rust
/// A working catalog materializing exactly commit `s.commit_seq`, built
/// from a snapshot manifest that the caller has already hash-verified and
/// identity-bound (D10.8). One commit row (seq = b, parent = null), chunks
/// and versions inserted, entries applied as PUTs in path order,
/// META_ARCHIVE_ID set, then `verify()`. No SQLite image is read.
pub(crate) fn catalog_from_snapshot(s: &Manifest) -> Result<Catalog>;
```

`recover_from_manifests` already does this inline for a snapshot named as
the head (`diff_ops` from an empty `Snapshot`). Factor that into this
function and call it from both places. `Catalog::append_commit` accepts a
first commit with `seq > 0` and `parent: None`, and `SegmentApplier::new`
then sees head = *b*. Check both in a unit test.

Unit test: for a writer-produced checkpoint *c*,
`AuthoritativeState::from_catalog(&catalog_from_snapshot(S(c)), c)` equals
the same projection of the image catalog, and `head_commit() == Some(c)`.

### 1.4 Refactor: one delta loop for open and recovery

Move the loop in `publish.rs::replay_segment` (load delta *j* with
`read_bound_manifest`, `check_delta_parent_link(delta, &prev.commit)`,
`applier.apply`, attribute accumulation with the D10.4 reintroduction
check) into:

```rust
fn apply_segment_deltas(
    src: &dyn ReadStorage,
    entries: &[HistoryEntry],          // b ..= h, from walk_segment
    head_manifest: Option<&Manifest>,  // already-loaded delta h, if any
    applier: &mut SegmentApplier,
    attributes: Option<&mut BTreeMap<FileVersionId, Attributes>>,
    opts: &ReadOptions,
) -> Result<()>;
```

This is a pure refactor. Every `t11_t12_replay`, `t12_oracle`, and
`work_bounds` test must pass unchanged. Commit it on its own.

### 1.5 `recover_baseline` (publish.rs)

```rust
/// Result of baseline recovery for one head.
#[derive(Debug)]
pub struct BaselineRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    pub segment: SegmentInfo,          // base_seq = b
    /// Query-only; materializes commits b ..= h (replay(Some(s)) works for
    /// those, not before b).
    pub catalog: Catalog,
    /// Promised attributes of every version reachable at the head.
    pub attributes: BTreeMap<FileVersionId, Attributes>,
}

/// D10.8 for the commit whose footer is at `footer_offset`.
pub fn recover_baseline_at_footer(src: &dyn ReadStorage, footer_offset: u64, opts: &ReadOptions)
    -> Result<BaselineRecovery>;

fn recover_baseline(src: &dyn ReadStorage, head: HistoryEntry, opts: &ReadOptions)
    -> Result<BaselineRecovery>;
```

`recover_baseline`, in this order:

1. `read_descriptor(src, &head.commit, head.commit_offset, opts)` (D12).
2. `walk_segment(src, head, opts)`: footers and commit records *b* … *h*
   only. This gives commit *b*'s record, which is all D10.8 needs from
   *b*. It reads no manifests.
3. S(*b*): `read_bound_manifest(src, &base.commit, &checkpoint_snapshot_ref(&base.commit)?, base.commit_offset, ManifestKind::Snapshot, opts)`.
   Hash first, then decode, then D11 identity against commit *b*.
4. `catalog_from_snapshot(&s_b)`. Check `head_commit() == Some(b)`.
5. Attributes map from `s_b.file_versions`.
6. `SegmentApplier::new(catalog)`, then `apply_segment_deltas(src, &entries, None, &mut applier, Some(&mut attrs), opts)`.
   Delta *b*+1's parent link is checked against commit *b*'s key 6 hash
   from the record (Q22). Delta *b* is never read.
7. `applier.head() == h`. Then `reachable_attributes(applier.namespace(), attrs)`.
   If it fails, return `RECORD_INVALID`: §11.1 snapshot recovery includes
   promised attributes, so a gap is not a partial success (same code as
   Q24).
8. `into_catalog()`, `make_query_only()`.

Any failure returns the error. There is no partial result and no earlier
state.

`recover_baseline_at_footer` builds the `HistoryEntry` the way
`open_at_footer` does (`validate_footer`, `footer_names_commit`,
`read_commit`).

### 1.6 Extend `recover_with_trusted_head`

New return type (breaking change to a pre-1.0 API; update the call sites in
`c5_publication.rs`):

```rust
#[derive(Debug)]
pub struct TrustedRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    /// Manifest-chain recovery (C4/C5), anchored on the head commit's key-6
    /// hash. `Err` if it could not run at all (for example, the head's delta
    /// manifest is gone).
    pub chain: std::result::Result<ManifestRecovery, MochiError>,
    /// D10.8, attempted only when `chain` does not reach `head_seq`.
    /// `None` = not attempted.
    pub baseline: Option<std::result::Result<BaselineRecovery, MochiError>>,
}

impl TrustedRecovery {
    /// §11.1 scope for `seq`: from the baseline if it covers seq (Snapshot
    /// at the head, Historical for b..h), else from the chain, else
    /// PayloadSalvage.
    pub fn scope_for(&self, seq: u64) -> RecoveryScope;
    /// The catalog that reaches the head, if any (baseline first).
    pub fn head_catalog(&self) -> Option<&Catalog>;
}
```

Keep the frame scan inside `recover_with_trusted_head`, limited to the
committed prefix. A scan that stops early (damaged frame headers) is not an
error: the chain then simply finds fewer manifests.

### 1.7 Tests: `crates/mochi-testkit/tests/t16_baseline_recovery.rs`

Fixture A: `write(CheckpointPolicy::Every(4), &history_long()[..8])` gives
cp0 d1 d2 d3 **cp4** d5 d6 d7. Head 7, *b* = 4.

For "state equals", compare `AuthoritativeState::from_catalog(rec.catalog, s)`
with the same projection of `open_at_footer(original, s)`'s catalog for each
*s* in *b*..=*h*. Compare attributes by path with the model (`steps[h].attrs`).

| Test | What it shows |
|---|---|
| `t16_recovers_with_images_delta_b_and_earlier_manifests_destroyed` | **DoD.** Wipe both images (0, 4), delta 4, and every manifest of commits 0–3 (deltas and S(0)). `recover_with_trusted_head`: `chain` stops short of 7, `baseline` is `Ok`, base 4, states 4..=7 equal the originals, attributes equal the model, `scope_for(7)` Snapshot, `scope_for(5)` Historical. **Second variant:** also wipe every byte of commits 0–3's commit records and footers. Baseline still succeeds, which shows nothing before *b* is needed. |
| `t16_baseline_reads_only_b_record_s_b_and_later_deltas` | `Tracing` around `recover_baseline_at_footer(7)`. Allowed reads: the descriptor; footers and commit records 4..=7; S(4); deltas 5..=7. Forbidden: image 4, delta 4, anything belonging to commits 0–3. Model it on `t11_opening_reads_only_the_segment`. |
| `t16_checkpoint_head_recovers_from_its_snapshot_alone` | Head 4 (*b* = *h*), with image 4 and delta 4 wiped. Recovers from S(4). |
| `t16_matches_open_for_every_head_under_every_policy` | Policies `EveryCommit`, `Never`, `Every(2)`, `Every(3)`; every footer of `history()`. Undamaged. Baseline state and attributes equal the opened state and the model. |
| `t16_damaged_snapshot_refuses_without_trying_an_earlier_checkpoint` | Damage S(4), keep S(0) and everything else. `recover_baseline_at_footer(7)` is `STORED_INTEGRITY_FAILED`. `TrustedRecovery` reports `baseline: Some(Err)` and does not claim a snapshot of 7. |
| `t16_damaged_delta_in_segment_refuses` | Damage delta 6. `STORED_INTEGRITY_FAILED`. |
| `t16_first_delta_link_is_checked_against_commit_b_record` | Forge (as `q22_link_to_the_base_snapshot_is_record_invalid`): delta *b*+1 links to S(*b*)'s hash. `RECORD_INVALID`. |
| `t16_snapshot_bound_to_another_commit_is_refused` | Forge a checkpoint whose snapshot ref names another checkpoint's S by its correct hash. D11 identity failure (`ENVELOPE_INVALID`, the code `read_bound_manifest` already returns). |
| `t16_incomplete_base_snapshot_refuses` | Reuse `checkpoint_with_incomplete_snapshot` (move it to `replay.rs`). `RECORD_INVALID`. |
| `t16_bad_descriptor_refuses` | Damage the descriptor. `DESCRIPTOR_INVALID`. |
| `t16_missing_delta_before_a_checkpoint_is_bridged_by_commit_records` | The C4 case on a real archive: `Every(6)`, `history_long()` plus one step (10 commits), delta 3 wiped. Chain broken at 4; baseline covers 6..=9; `scope_for(3)` is FileRecovery. Update the doc comment of `c4_recovery.rs::missing_delta_before_a_snapshot_is_not_bridged_without_commit_records` to name this test. |
| `t16_baseline_not_attempted_when_the_chain_reaches_the_head` | Undamaged archive: `baseline` is `None`. |

Mutation checks to record:
1. Skip `check_delta_parent_link` in the baseline path. The link test fails.
2. Open image *b* instead of building from S(*b*). The trace test and the DoD test fail.
3. Take attributes from the deltas only. The attribute assertions fail.
4. On an S(*b*) failure, retry from the previous checkpoint. The refusal test fails.

### 1.8 Docs

* `recovery.rs` module docs: replace "unused until T16" and "returns with
  T16" with what now exists.
* Checklist: T16 row → **Implemented**, with evidence and mutation results
  in the same style as T13. Add Q29, Q30, Q30a. Under "What G2 still needs",
  T16 → implemented, T17 next.
* Fuzz: in `mochi_testkit::fuzz::exercise_archive_open`, when the open fails
  with `STORED_INTEGRITY_FAILED`, also call `recover_baseline_at_footer` on
  the head footer and assert it neither panics nor returns a state for a
  different sequence. The stable-Rust smoke tests then exercise this over
  the vectors.

---

## 2. T17: damage scope (D10.9)

> * A damaged delta manifest *j* breaks every replay segment containing *j*.
>   Snapshots before *j*, and snapshots whose base checkpoint is at or after
>   *j*, are unaffected.
> * A damaged image with an intact snapshot manifest: reads rebuild the
>   catalog from the snapshot manifest.
> * A damaged snapshot manifest with an intact image: reads continue, and
>   recoverability is `DEGRADED`.
> * No repair inside a segment. …
> * Verification names the affected sequence range.

**Checklist DoD:** a damage matrix over deltas, images, and snapshot
manifests across 3 segments. **G2:** the matrix matches D10.9.

Two parts: **(A)** changes to the read path so reads behave as D10.9 says,
and **(B)** a read-only damage assessment that names ranges and statuses.
The `verify` command itself is C7. T17 delivers the core function C7 will
call, plus its mapping onto report v0.

### 2.1 Decisions to record

* **Q31: a checkpoint's own delta manifest.** D10.9 says a snapshot whose
  base is at or after *j* is unaffected, so commit *j* = *b* still reads
  when only its own delta manifest is damaged. Today `open_at` always loads
  the head's delta manifest and fails. **Proposed:** for a **checkpoint
  head in read mode**, a failure loading or binding its delta manifest is
  recorded, not fatal. `OpenedHead.manifest` becomes `Option<Manifest>`,
  with a new `manifest_error: Option<MochiError>`. I/O errors still
  propagate: they are operational, not damage. A delta head still needs its
  own delta manifest. Append refuses (Q34).
* **Q32: which image failures trigger the snapshot rebuild. Proposed:**
  `STORED_INTEGRITY_FAILED`, `ENVELOPE_INVALID`, `CATALOG_INVALID`,
  `RECORD_INVALID` (image does not materialize *b*, or belongs to another
  archive), `MALFORMED_FRAME`. **Not** `IO_ERROR`, `LIMIT_EXCEEDED`,
  `UNSUPPORTED_FEATURE`, or `CANCELLED`: those are "cannot", not "damaged".
  This keeps the frozen `reject-archive-bare-image.mochi` at
  `UNSUPPORTED_FEATURE` (checklist Q12). Pin that in a test.
* **Q33: statuses. Proposed.**
  * Integrity: `FAIL` if any checked object fails; `PASS` only if every
    object was checked and none failed.
  * Recoverability per commit:
    * `FAIL` if the commit is unreadable (no repair inside a segment
      without Redundancy);
    * `DEGRADED` if readable but its base's snapshot manifest **or** image
      is damaged (one of two independent representations lost);
    * `PASS` otherwise.

    The spec says `DEGRADED` only for the damaged-snapshot case. Treating a
    damaged image the same way is provisional.
  * Rollup over all commits (worst wins). Nothing is retention-scoped until
    C9.
* **Q34: append over damage. Proposed:** `open_append` keeps today's
  behaviour and refuses on any failure, with no snapshot rebuild and no
  tolerated delta. Two reasons: writer parameters are only in the image
  (1.1), and repair is plan-then-apply (§22.2), not a side effect of
  append. The error is the underlying object's code.
* **Q35: report field. Proposed:** add an optional `affected:
  Option<SeqRange { first: u64, last: u64 }>` to report v0's `Finding`
  (`skip_serializing_if = "Option::is_none"`, so existing JSON is
  unchanged). Report v0 is a draft that R7/C7 replaces, so this belongs on
  the R7 input list.
* **Q36: scope limit.** Damage to commit records or footers is outside the
  T17 matrix. If `commit_history` fails, the assessment returns that error
  (structural; C8's ladder handles it).

### 2.2 (A) Read-path changes (publish.rs)

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogSource {
    /// The base checkpoint's catalog image (the normal path).
    Image,
    /// Rebuilt from the base checkpoint's snapshot manifest because the
    /// image failed (D10.9). Same commit, not an earlier state.
    SnapshotManifest { image_error: MochiError },
}
```

`OpenedHead` gains `catalog_source: CatalogSource` and
`manifest_error: Option<MochiError>`, and `manifest` becomes
`Option<Manifest>`. Update `fuzz.rs::exercise_archive_open` and the C5
benchmark's use of it.

Factor a single helper used by both checkpoint heads and `replay_segment`:

```rust
/// The base checkpoint's catalog, writable (for replay) or query-only.
/// Read mode: on a Q32 image failure, read S(b) and build from it
/// (`catalog_from_snapshot`). If S(b) also fails, return the image's error,
/// with the snapshot's failure added to the message. Append mode: no
/// rebuild.
fn base_catalog(src, base: &HistoryEntry, opts, mode) -> Result<(Catalog, CatalogSource)>;
```

* Checkpoint head, read mode: `base_catalog`, then `make_query_only`.
* Delta head, read mode: `base_catalog` (writable), then `SegmentApplier`,
  `apply_segment_deltas`, `make_query_only`. Unchanged otherwise.
* In the normal path, S(*b*) is **still not read** (D10.9, and
  `t11_opening_reads_only_the_segment` must keep passing unchanged).

Behaviour change to an existing test:
`c5_publication.rs::a_destroyed_catalog_is_recovered_through_the_footer_verified_head`
asserts that `open_head` fails once every image is zeroed. Under D10.9 it
now succeeds from S(2) with `CatalogSource::SnapshotManifest`. Change that
assertion. Add a case that also zeroes every snapshot manifest, where the
open fails with `STORED_INTEGRITY_FAILED`. Explain this in the commit
message: it is the spec's intended behaviour, not a regression.

### 2.3 (B) Damage assessment: `crate::damage` (new module)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole { DeltaManifest, SnapshotManifest, CatalogImage }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDamage { pub seq: u64, pub role: ObjectRole, pub offset: u64, pub error: MochiError }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readability { Image, SnapshotRebuild, Unreadable }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitScope {
    pub seq: u64,
    pub base_seq: u64,
    pub readable: Readability,
    pub recoverability: Status,          // Q33
    pub cause: Option<usize>,            // index into `objects`
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect { Unreadable, ReadsFromSnapshot, Degraded, None }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedRange { pub first: u64, pub last: u64, pub effect: Effect, pub cause: usize }

#[derive(Debug)]
pub struct DamageReport {
    pub head_seq: u64,
    pub objects: Vec<ObjectDamage>,      // every failed object, in sequence order
    pub commits: Vec<CommitScope>,       // 0 ..= head
    pub ranges: Vec<AffectedRange>,      // one per damaged object
    pub objects_checked: u64,
}

impl DamageReport {
    pub fn integrity(&self) -> Status;
    pub fn recoverability(&self) -> Status;
    /// Report-v0 findings, one per damaged object, with `affected` set (Q35).
    pub fn findings(&self) -> Vec<Finding>;
}

/// Read-only (takes `&dyn ReadStorage`). A job: reports progress per commit
/// and honours cancellation (AGENTS.md, "Long operations are jobs").
pub fn assess_damage(src: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>)
    -> Result<DamageReport>;
```

Algorithm:

1. `commit_history(src, opts)` (Q36).
2. For each commit *j*: check delta manifest *j* (`read_bound_manifest`,
   kind Delta). For each checkpoint *c*: check its snapshot manifest
   (`read_bound_manifest`, kind Snapshot) and its image (the image half of
   `open_checkpoint_catalog`: `load_verified`, `decode_image_record`,
   `Catalog::open_image` read-only, head and archive checks). Factor that
   into `check_image(...) -> Result<Catalog>` and use it from both places.
   Each object is read once.
3. Derive each commit *h* (base *b* from its record):
   * any damaged delta in (*b*, *h*] → `Unreadable` (cause: the lowest such *j*);
   * else image *b* ok → `Image`; else S(*b*) ok → `SnapshotRebuild`; else `Unreadable`;
   * damage to delta *b* itself never affects *h* (Q31);
   * recoverability per Q33.
4. Ranges: let *e*(*x*) be the last commit of the segment containing *x*
   (the commit before the next checkpoint, capped at the head).
   * Damaged delta *j*, where *j* is a delta commit: [*j*, *e*(*j*)], `Unreadable`.
   * Damaged delta *j*, where *j* is a checkpoint: [*j*, *j*], `None`. An
     object finding only; nothing reads it (Q31).
   * Damaged image *c*: [*c*, *e*(*c*)], `ReadsFromSnapshot`, or
     `Unreadable` if S(*c*) is also damaged.
   * Damaged snapshot *c*: [*c*, *e*(*c*)], `Degraded`, or `Unreadable` if
     the image is also damaged.

The assessment must agree with the read path. A test checks that
`commits[h].readable` predicts exactly what `open_at_footer(h)` does.

### 2.4 Tests: `crates/mochi-testkit/tests/t17_damage_scope.rs`

Fixture: `write(CheckpointPolicy::Every(3), &history_long())` gives
cp0 d1 d2 | cp3 d4 d5 | cp6 d7 d8. Three segments, each with deltas.

Write an **independent oracle** in the test, from the D10.9 text, without
calling `damage.rs`:
`expected_open(h, damaged) -> Result<Readability, ErrorCode>` and
`expected_recoverability(h, damaged) -> Status`, using only the fixture's
known forms (which commits are checkpoints).

| Test | What it shows |
|---|---|
| `t17_damage_matrix_matches_d10_9` | **DoD.** 18 damage cases: delta *j* for *j* in 0..=8; image *c*, snapshot *c*, and image+snapshot *c* for *c* in {0, 3, 6}. For each case and each head 0..=8: `open_at_footer` matches `expected_open` (on `Ok`: `catalog_source` matches, and `read_state` equals `steps[h].after`; on `Err`: `STORED_INTEGRITY_FAILED`). `assess_damage`'s `commits`, `ranges`, `integrity() == FAIL`, and `recoverability()` match the oracle. Print the matrix on failure. |
| `t17_undamaged_control` | No findings, no ranges, every commit `Image` / `PASS`, `integrity()` and `recoverability()` `PASS`. |
| `t17_damaged_snapshot_reads_continue_degraded` | S(3) damaged: heads 3..=5 open from the image; recoverability `DEGRADED`; one range [3, 5]. A `Report` built from `findings()` and the two dimensions passes `Report::validate`, and its overall status is not `PASS`. |
| `t17_damaged_image_reads_rebuild_from_snapshot` | Image 3 damaged: heads 3..=5 open with `SnapshotManifest { image_error.code == STORED_INTEGRITY_FAILED }`. States equal the model. |
| `t17_checkpoint_own_delta_damage_does_not_affect_reads` | Delta 3 damaged: head 3 opens with `manifest == None` and `manifest_error` set; heads 4 and 5 open; `open_append` on head 3 is refused (Q34). |
| `t17_no_fallback_to_an_earlier_checkpoint` | Image 3 and S(3) damaged: heads 3..=5 fail. A head never returns the state of 2. |
| `t17_append_refused_when_base_image_damaged` | Image 3 damaged and head 5: reads open, `open_append` fails with the image's code, and nothing is written (bytes identical). |
| `t17_fallback_reads_s_b_only_after_the_image_fails` | `Tracing`: with image 3 damaged, the reads for head 5 add S(3) to the T11 allowed set, and nothing else. |
| `t17_unsupported_image_does_not_fall_back` | `reject-archive-bare-image.mochi` is still `UNSUPPORTED_FEATURE` (Q32). |
| `t17_assessment_is_read_only` | BLAKE3 of the bytes before and after `assess_damage` is equal; cancellation returns `CANCELLED`. |

Mutation checks to record:
1. Disable the snapshot rebuild. Every image case fails.
2. Make a checkpoint's own delta required again. The Q31 cases fail.
3. Make a range end at *e* + 1. The matrix fails.
4. Report `PASS` instead of `DEGRADED`. The degraded test fails.
5. Rebuild from an earlier checkpoint's S. The no-fallback test fails.

### 2.5 Docs

* Checklist: T17 row → **Implemented**, with evidence and mutation results.
  Add Q31–Q36.
* `publish.rs` module docs: D10.9 read behaviour, and that the snapshot
  manifest is read only after an image failure.
* Golden vectors: none change. Say so in the checklist row.

---

## 3. T15: adoption (D10.7, §18.1)

> Before publishing a checkpoint commit, the writer re-reads both checkpoint
> representations from their serialized, hashed bytes and checks that each
> one's authoritative state equals the source snapshot: the snapshot
> manifest is decoded with the canonical CBOR codec, without SQLite; the
> image's state is extracted from the image. On any mismatch the commit
> fails, and there is no new head. `verify` repeats this comparison; a
> mismatch there is a `FAIL`.

**Checklist DoD:** a planted divergence blocks the head; the previous head
stays valid. **G2:** an adoption mismatch blocks the head.

### 3.1 Decisions to record

* **Q37: codes.** A semantic disagreement is `CHECKPOINT_MISMATCH`, which is
  already registered: exit 3 from the writer, a `FAIL` finding in verify. A
  re-read whose hash does not match what was written is a storage fault
  (`STORED_INTEGRITY_FAILED`, or `IO_ERROR` if the read fails). Either way
  nothing is published.
* **Q38: attributes.** The snapshot half compares attributes. The image
  half does not, because the image holds none until C6 (Q6). When C6 adds
  them, the image comparison must include them. Put a `// C6:` note at that
  spot.
* **Q39: two codes for one substance.** For published checkpoints, verify
  reports image/snapshot disagreement as `CHECKPOINT_MISMATCH`. Q24 keeps
  `RECORD_INVALID` for append-open (decided, not ratified). Record the
  inconsistency for review. Do not change Q24.
* **Q40: writer state after a mismatch.** Fail in `prepare` (before the
  commit record and footer), so the existing roll-back removes the
  unpublished bytes. Do **not** poison the writer: nothing was published and
  no sync failed (§12.2). The next commit may succeed.
* **Re-read bounds.** Re-read with `Limits::WRITER_DEFAULT` and
  `CatalogLimits::default()`, not `self.read`. The writer bounded its
  output by the reader defaults (B.2.3), and a user's lower read limits
  must not make the writer reject its own valid output.

### 3.2 Writer integration (publish.rs, `prepare`)

In the checkpoint branch:

1. **Source**, before building the snapshot manifest:
   `let reachable = reachable_attributes(&after, attributes.clone())?;`
   `let source = AuthoritativeState::from_catalog(&cat, seq)?.with_attributes(reachable.clone());`
   Use `reachable` as the writer's new attributes, instead of re-deriving
   them from the snapshot it just built (which would be circular).
2. Build and append the snapshot and the image, as today.
3. **Adopt** (new; a cancellation point before it):

```rust
/// D10.7. Re-read both representations from storage, hash-verify against
/// the refs just written, decode, and compare with `source`.
fn adopt_checkpoint(
    storage: &dyn ReadStorage,
    source: &AuthoritativeState,
    snapshot: &ObjectRef,
    image: &ObjectRef,
    identity: &RecordIdentity,
    archive_id: ArchiveId,
    writer_params: &[u8],
) -> Result<()>;
```

* Snapshot: `load_verified(storage, snapshot, snapshot.end()?, WRITER_DEFAULT.max_frame_len, …)`,
  then `Manifest::from_stored`, kind Snapshot, `identity().check(identity)`,
  `AuthoritativeState::from_snapshot`. It must equal `source` **including
  attributes**.
* Image: `load_verified`, `decode_image_record(&stored, identity, &WRITER_DEFAULT)`,
  `Catalog::open_image` (read-only, full verify), `head_commit() == Some(seq)`,
  `META_ARCHIVE_ID` equals `archive_id`, `META_WRITER_PARAMS` equals
  `writer_params`, then `from_catalog(&img, seq)` must equal
  `source.clone().without_attributes()`.
* On a difference: `CHECKPOINT_MISMATCH`, naming the representation and up
  to 8 differences from `differences()`.

Report progress under the existing `phase::CHECKPOINT`. Do not add a
second progress mechanism.

Cost: this adds a re-read, a CBOR decode, and an SQLite open plus verify to
every checkpoint, which is every commit in production until T14. Re-run
`cargo run --release -p mochi-testkit --example c5_append_bench` and add a
dated section to `docs/benchmarks/c5-append.md` with the §27 decomposition.
Note which column now includes adoption.

### 3.3 Test hooks (test-controls only)

* Writer: `pub fn set_checkpoint_tamper(&mut self, t: Option<CheckpointTamper>)`,
  in the existing `#[cfg(any(test, feature = "test-controls"))] impl` block.
  It changes only what is **serialized**, never `source`:
  * `SnapshotAttributes`: XOR the mode of the first file version's POSIX
    attributes before encoding.
  * `SnapshotOmitsEntry`: drop the last namespace entry, plus its version
    and chunks if nothing else references them. Re-canonicalize. The result
    must still pass `check_structure`, so the divergence is semantic, not a
    decode error.
  * `ImageOmitsLastOp`: serialize a catalog built like `cat` but with the
    transaction's last namespace operation left out. Build it from the
    previous head's catalog (or `new_working` plus meta for commit 0), the
    same objects and versions, and `append_commit` with `ops[..n-1]`.
    Requires at least one op; the test transactions have one.
* `SimStorage`: `Fault::CorruptAppend { append_index: usize, at: usize, xor: u8 }`.
  The append stores altered bytes and returns `Ok`, which models silent
  write corruption. Add a unit test in `sim.rs`.

### 3.4 Reader side (for C7's verify)

```rust
/// D10.7 for a published checkpoint: both representations, hash-verified
/// and decoded, compared without attributes (Q38). Mismatch is
/// `CHECKPOINT_MISMATCH`.
pub fn check_checkpoint_representations(src: &dyn ReadStorage, cp: &HistoryEntry, opts: &ReadOptions)
    -> Result<()>;
```

Call it from `assess_damage` for each checkpoint whose image and snapshot
both load. A mismatch becomes an `ObjectDamage` with role `CatalogImage`
and code `CHECKPOINT_MISMATCH`: integrity `FAIL`, recoverability
`DEGRADED` for that segment (provisional, Q33), reads unaffected.

### 3.5 Tests: `crates/mochi-testkit/tests/t15_adoption.rs`

For every blocked-head test, assert all of the following:
* the error code;
* `s.contents()` is byte-identical to before the commit;
* `open_head(&s)` still opens the previous head, to its model state;
* `w.head_seq()` is unchanged;
* after `set_checkpoint_tamper(None)`, the same transaction commits and
  opens.

| Test | What it shows |
|---|---|
| `t15_snapshot_attribute_divergence_blocks_the_head` | `SnapshotAttributes` → `CHECKPOINT_MISMATCH`. **DoD.** |
| `t15_snapshot_missing_entry_blocks_the_head` | `SnapshotOmitsEntry` → `CHECKPOINT_MISMATCH`. |
| `t15_image_divergence_blocks_the_head` | `ImageOmitsLastOp` → `CHECKPOINT_MISMATCH`. |
| `t15_first_commit_divergence_leaves_an_empty_file` | Tamper on commit 0: the file is empty afterwards (compare `a_cancelled_first_commit_leaves_no_descriptor_behind`). |
| `t15_delta_commits_are_not_adopted` | Under `Never`, a tamper set on a delta commit has no effect: no checkpoint is written, so nothing is compared. |
| `t15_silent_write_corruption_is_caught_on_reread` | `CorruptAppend` on the snapshot's append, and separately on the image's: `STORED_INTEGRITY_FAILED`, no head, bytes rolled back. |
| `t15_verify_reports_a_published_mismatch` | `checkpoint_with_incomplete_snapshot` (forged, published): `check_checkpoint_representations` is `CHECKPOINT_MISMATCH`; `assess_damage` reports it at that sequence; `open_head` is unaffected; Q24 append refusal unchanged. |
| (existing suites) | Every C5, T11/T12, oracle, and golden test still passes. These are the no-tamper controls under every policy. |

Mutation checks to record:
1. Skip the snapshot comparison. Both snapshot tests fail.
2. Skip the image comparison. The image test fails.
3. Compare the in-memory frames instead of re-reading storage. The
   corruption test fails.
4. Include attributes on the image side. Every checkpoint commit fails, so
   the controls catch it.

### 3.6 Docs

* Checklist: T15 row → **Implemented**. Add Q37–Q40. Under "What G2 still
  needs", mark T15, T16, and T17 implemented, with CI outstanding. G2 also
  lists a GC item (C9) that this work does not close; say so.
* `publish.rs` module docs: add adoption to the step table (step 4, before
  the commit record).
* Benchmark section (3.2).

---

## 4. Done means

Each task is **implemented** when its tests and mutation checks pass
locally and the checklist row records them. It is **done** only when CI is
green on Ubuntu and Windows (checklist "Status"). CI runs only on pushes to
`main` and on pull requests, so getting that evidence needs a PR. Do not
claim G2 from local runs.
