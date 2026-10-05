# T16, T17, T15: implementation plan

**Status: T16 implemented (see the checklist); T17 and T15 not started.** Written 2026-10-05; revised the same day with
the open decisions taken (section 1). Work order is **T16 → T17 → T15**
(checklist, "What G2 still needs"). Each task builds on the previous one:

* T16 adds the state model (`crate::state`), the snapshot-to-catalog
  builder, and the shared delta loop.
* T17's read-path fallback uses T16's builder. Its damage assessment is the
  module that T15's reader-side check plugs into.
* T15's writer check uses T16's state model.

Normative sources: spec Annex B.2 **D10.7** (adoption), **D10.8** (baseline
recovery), **D10.9** (damage scope), D10.2–D10.6 (checkpoint binding,
replay, segment rules; already implemented), D11 (identity binding), D12
(descriptor), §11, §11.1, §12.2, §18.1, §20.3–§20.5. Tasks and gate:
`b2-implementation-checklist.md` rows T15–T17, gate **G2**.

**Nothing here changes the wire format.** No golden fixture changes. If one
does, stop and explain the diff before going on (AGENTS.md).

Contents:
1. Decisions taken
2. Ground rules and shared preparation
3. T16: baseline recovery
4. T17: damage scope
5. T15: adoption
6. Done means
7. Stop and ask

---

## 1. Decisions taken (2026-10-05 [delegated])

The owner delegated review decisions to the implementer (checklist,
"Decision authority"). The decisions below are taken. Record each in the
checklist's question list as **Decided [delegated]**, with its rationale,
when the task that implements it lands. They are a self-review, not an
independent review.

### D-Q29. Meaning of "needs no SQLite" (D10.8)

**Decided:** baseline recovery reads **no SQLite image**, and its result is
still a catalog.

* Inputs: commit records *b* … *h*, the descriptor, S(*b*), and deltas
  *b*+1 … *h*.
* Output: a fresh in-memory working catalog (`Catalog::new_working`, O25),
  filled from S(*b*). The deltas are applied by the existing
  `SegmentApplier`.

Rationale:
* D10.8 lists what recovery must not *need*: the inputs that may be lost.
  It does not forbid the in-memory catalog that every reader materializes
  (O25: "the catalog is in-memory SQLite").
* Reusing `SegmentApplier` keeps one implementation of the D10.4 rules
  (introduction, reference, `NAMESPACE_INVALID`, atomicity, T13 bounds).
* Rejected: a second, pure-Rust delta applier. It would duplicate every
  D10.4 rule, the copies could drift, and T13's bounds would then cover
  only one of them.

Where the spec does require SQLite-free code (D10.7's snapshot half),
`AuthoritativeState::from_snapshot` provides it, and a test proves it never
touches SQLite (3.2).

Evidence: G2 says "Baseline recovery succeeds with all SQLite images and
all earlier manifests deleted". The completion test deletes every image,
and a read-trace test proves no image byte is read.

### D-Q30. Shape of `recover_with_trusted_head`

**Decided:**
1. Run the existing manifest-chain recovery first. It is the only path that
   gives historical recovery before *b*.
2. Run baseline recovery only if the chain does not reach the head.
3. Return both results in a new struct, `TrustedRecovery`.

Baseline recovery uses only the head's own segment and never tries an
earlier checkpoint (D10.4, D10.6).

### D-Q30a. Descriptor during recovery

**Decided:** baseline recovery is interpretation (replay), so D12 applies.
It reads and checks the descriptor exactly as `open_at` does. A missing,
damaged, or mismatched descriptor fails with `DESCRIPTOR_INVALID`.
Explicit salvage labelled partial is C8's.

### D-Q31. A checkpoint's own delta manifest; D-Q32. When reads use the snapshot instead of the image

**Decided: only `STORED_INTEGRITY_FAILED` is "damage".** Both rules use this
same test.

* **Q31:** a checkpoint head opened for reading tolerates a
  `STORED_INTEGRITY_FAILED` on its own delta manifest. It records the error
  and opens from the image (or the snapshot). D10.9 requires this:
  "snapshots whose base checkpoint is at or after *j* are unaffected", and
  the checkpoint at *j* has base *j*.
* **Q32:** a read uses S(*b*) in place of image *b* only when the image
  fails with `STORED_INTEGRITY_FAILED`.

Any other failure of a hash-verified object is refused as before, with no
alternative path. That includes a decode error, `RECORD_INVALID`,
`ENVELOPE_INVALID`, `CATALOG_INVALID`, `UNSUPPORTED_FEATURE`,
`LIMIT_EXCEEDED`, `OUT_OF_BOUNDS`, and `IO_ERROR`.

Rationale:
* Every object is hash-checked before it is parsed (C5). So any change to
  stored bytes, whether a flipped bit, zeroed bytes, or a deleted payload,
  shows up as `STORED_INTEGRITY_FAILED`, before any decoder runs.
* An object that passes its hash but then fails to decode or validate
  carries exactly the bytes its commit was published with. It is an
  invalid record, not damage.
* D10.4 requires refusal in those cases: "the commit whose manifest it is,
  and every snapshot whose replay segment contains that commit, cannot be
  opened". Falling back would hide an invalid record behind its twin.
* `IO_ERROR` and `OUT_OF_BOUNDS` are operational. `LIMIT_EXCEEDED` and
  `UNSUPPORTED_FEATURE` are "cannot", not "damaged". The frozen
  `reject-archive-bare-image.mochi` therefore stays `UNSUPPORTED_FEATURE`
  (checklist Q12).

Consequences:
* `OpenedHead.manifest` becomes `Option<Manifest>`, plus
  `manifest_error: Option<MochiError>`.
* A **delta** head still needs its own delta manifest, because it is in
  its replay segment.
* **Append is unchanged** (D-Q34): any failure refuses.

Define the test once in `publish.rs`:

```rust
/// D-Q31/Q32: the only failure that counts as damage to stored bytes.
fn is_stored_damage(e: &MochiError) -> bool {
    e.code == ErrorCode::StoredIntegrityFailed
}
```

### D-Q33. Status values

**Decided.**

| Situation for commit *h* (base *b*) | Readable via | Recoverability |
|---|---|---|
| Image *b*, S(*b*), and deltas (*b*, *h*] all intact | image | `PASS` |
| S(*b*) damaged, image intact | image | `DEGRADED` (D10.9, literal) |
| Image *b* damaged, S(*b*) intact | snapshot rebuild | `DEGRADED` |
| Image and S(*b*) intact but disagreeing (T15, `CHECKPOINT_MISMATCH`) | image | `DEGRADED` |
| A delta in (*b*, *h*] damaged, or image and S(*b*) both damaged | none | `FAIL` |
| Only delta *b* (the checkpoint's own delta) damaged | image | `PASS` |

**Integrity** is `FAIL` if any object finding exists. It is `PASS` only if
every object was checked and none failed.

**Rollups**, worst wins (`FAIL` > `DEGRADED` > `PASS`):
* the archive's Recoverability is the worst over **all** commits 0 … head;
* the head's Recoverability is reported separately (`head_recoverability`).

Rationale:
* `DEGRADED` (§20.4) means "some capability remains available, but a
  required protection is reduced".
* D10.2 requires two independent representations of every checkpoint.
  Losing either one, or having them disagree, leaves reads working but
  removes the redundancy D10 relies on. The spec names the snapshot case
  explicitly; the image case is the same situation mirrored, and calling
  it `PASS` would claim a protection that is gone.
* Unreadable snapshots cannot be repaired inside a segment without
  Redundancy (D10.9), so they are `FAIL`.
* Until GC (C9), every commit is retained, so a lost historical snapshot
  is a real loss. Rolling it up keeps it visible, and the separate head
  status keeps a healthy head visible too.
* Delta *b* is not needed by any read and not needed by baseline recovery
  (D10.8). It is an integrity finding only.

### D-Q34. Append over damage

**Decided:** `open_append` keeps today's behaviour. Any failure refuses
append. There is no snapshot rebuild and no tolerated delta.

Rationale:
* Writer parameters (`META_WRITER_PARAMS`) exist only in the image.
* Repair is plan-then-apply (§22.2), not a side effect of append.
* The next checkpoint would bind new representations over damaged history
  without any record of it.

The error is the underlying object's code, with the existing
`append_needs_snapshot` wording where it applies.

### D-Q35. Affected range in reports

**Decided:** report v0's `Finding` gains
`affected: Option<SeqRange>`, where `SeqRange { first: u64, last: u64 }`.
Use `#[serde(default, skip_serializing_if = "Option::is_none")]`, so every
existing report serializes identically. Report v0 is a draft that R7/C7
replaces; add "affected range" to the R7 input list in the checklist.

### D-Q36. Scope of T17

**Decided:** damage to commit records or footers is outside the T17
matrix. If `commit_history` fails, `assess_damage` returns that error
(structural). The C8 ladder handles it.

### Existing C5 test: decision

**Decided:** change
`c5_publication.rs::a_destroyed_catalog_is_recovered_through_the_footer_verified_head`
to the D10.9 behaviour, and add a companion test for the case the old
assertion was really protecting.

* **Today** the test zeroes every image payload and asserts that
  `open_head` fails with `STORED_INTEGRITY_FAILED`.
* **Under D10.9** the snapshot manifests are intact, so `open_head` now
  succeeds from S(2), with `CatalogSource::SnapshotManifest`.
* **New assertions in that test:**
  * `open_head` is `Ok`;
  * `catalog_source` is `SnapshotManifest`, and its `image_error.code` is
    `STORED_INTEGRITY_FAILED`;
  * the opened catalog's `AuthoritativeState` at 2 equals the original's;
  * the existing `recover_with_trusted_head` assertions are unchanged
    (the chain still reaches 2, `baseline` is `None`).
* **New test `a_destroyed_catalog_and_snapshot_fail_to_open`:** zero every
  image **and** every snapshot manifest. `open_head` fails with
  `STORED_INTEGRITY_FAILED` and returns no earlier state.
  `recover_with_trusted_head` still recovers 0..=2 through the delta
  chain.

The commit message must say this is the spec's intended behaviour change
(D10.9), not a regression, and name both tests.

### T15 cost: decision

**Decided:**
* **No opt-out.** Adoption runs on every checkpoint commit. §18.1 says
  "MUST be verified … before adoption", and D10.7 has no exception.
* **The image half uses the full `Catalog::open_image`** (integrity check,
  foreign keys, every version's extents, full namespace replay). That is
  exactly what a reader will do, so it proves a reader can open what is
  about to be published. Weaker extraction would leave the check short of
  that.
* **Its own progress phase**, `phase::ADOPT = "adopt"`, placed between
  `CHECKPOINT` and `COMMIT_RECORD`. This adds a phase value, not a second
  progress mechanism. It lets the benchmark time adoption separately.
* **Accept the cost.** Production writes `EveryCommit` until T14, so for
  now this is paid on every commit. T14's trigger (Δ ≥ α·max(*B*, *F*))
  makes checkpoints rare, which bounds the cost by design.
* **Measure it.** Re-run the C5 append benchmark and record a new section
  (5.7). Do not invent a numeric budget. If adoption exceeds the
  checkpoint phase itself at the largest N, record that as a finding for
  G3/T14, not as a reason to weaken the check.
* **Avoid needless work:**
  * build `source` once per commit;
  * re-read each object into one buffer;
  * compare the snapshot before opening the image, since that is cheaper
    and fails earlier;
  * do not re-derive attributes from the snapshot that was just written.

---

## 2. Ground rules and shared preparation

### 2.1 Rules (AGENTS.md; the ones that matter most here)

* Library code: no `unwrap`, `expect`, `panic!`, or slice indexing on
  paths archive bytes can reach. Return `MochiError` with a stable code.
  Use checked arithmetic on archive-derived values (`ObjectRef::end()`
  already does).
* All I/O in `mochi-core` goes through `ReadStorage`/`Storage`. Nothing in
  this plan needs `std::fs`.
* **Hash before parse:** every object is loaded with `load_verified`
  before anything decodes it. An image is hash-verified, then
  envelope-checked (`decode_image_record`), then opened in SQLite.
* **No search, no earlier state** (D10.4, D10.6). T17 adds exactly one
  alternative path: the *same* commit's snapshot manifest, which the same
  commit record binds by hash.
* Test hooks go behind `#[cfg(any(test, feature = "test-controls"))]`
  (review decision 14; `ci/check-invariants.sh` rule 9).
* Every new rule needs a test that fails without it. For each key rule,
  record a **mutation check**, as the checklist does for T11–T13:
  1. break the rule;
  2. run the tests and name the ones that fail;
  3. restore the code;
  4. confirm it is byte-identical with `md5sum` before and after.

  Put the results in the checklist row.
* New questions found during implementation continue from **Q41**, each
  with a provisional choice. Do not resolve them silently.
* Do not put model names in commits, comments, or docs.

Run these after every step, and all of them before each push:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash ci/check-invariants.sh
```

Before the last push of each task, also run
`cargo +nightly test --doc -p mochi-format` if nightly is available. If it
is not, say so in the checklist row.

### 2.2 Commit sequence

One commit per numbered step below. Each commit compiles and passes the
full suite on its own, and its message cites the spec items it
implements.

| Task | Commits |
|---|---|
| Prep | P1 helpers |
| T16 | 16.1 state model · 16.2 builder · 16.3 delta-loop refactor · 16.4 `recover_baseline` · 16.5 `TrustedRecovery` · 16.6 tests · 16.7 fuzz + docs + checklist |
| T17 | 17.1 `is_stored_damage` + `OpenedHead` fields · 17.2 `base_catalog` + read fallback + C5 test change · 17.3 `damage` module · 17.4 report field · 17.5 tests · 17.6 fuzz + docs + checklist |
| T15 | 15.1 test hooks (writer + `SimStorage`) · 15.2 `adopt_checkpoint` + writer wiring + `phase::ADOPT` · 15.3 reader-side check + `assess_damage` integration · 15.4 tests · 15.5 benchmark · 15.6 docs + checklist |

### 2.3 P1: shared test helpers

`crates/mochi-testkit/tests/t11_t12_replay.rs` holds helpers all three
tasks need. Move them **unchanged** into a new public module
`mochi_testkit::replay` (`crates/mochi-testkit/src/replay.rs`; add
`pub mod replay;` to `lib.rs`). Make them `pub` and import them back into
`t11_t12_replay.rs`.

Move:
* `attrs`, `Step`, `Model` (with `file`, `dir`, `delete`, `rename`, `end`);
* `history`, `write`, `append`, `is_cp`, `snapshot_attrs`, `flip`;
* `Tracing`, `within`;
* `checkpoint_with_incomplete_snapshot` and `open_append_err`.

Then add:

```rust
/// history() followed by two more steps: 9 commits (0..=8).
pub fn history_long() -> Vec<Step>;
/// history_long() plus one more step: 10 commits (0..=9).
pub fn history_10() -> Vec<Step>;
/// Zero the payload of the skippable frame `r` (keep its 8-byte header, so
/// the file still walks): a deleted object.
pub fn wipe(bytes: &mut [u8], r: ObjectRef);
/// Flip one byte in the middle of `r`: a damaged object.
pub fn damage(bytes: &mut [u8], r: ObjectRef);
/// Half-open byte range of `r`.
pub fn range(r: ObjectRef) -> (u64, u64);
/// image and snapshot refs of a checkpoint entry (panics on a delta; tests only).
pub fn checkpoint_refs(e: &HistoryEntry) -> (ObjectRef, ObjectRef);
/// Path → attributes reachable at head `h` of a recovery or open, by path.
pub fn attrs_by_path(ns: &Snapshot, a: &BTreeMap<FileVersionId, Attributes>)
    -> BTreeMap<Vec<u8>, Attributes>;
```

The extra steps in `history_long` (do not edit `history()`, because other
tests slice it):

```rust
m.file("i", deterministic_bytes(8, 40), attrs(0o620, 71));
m.end(70);
m.rename("h", "d/h");
m.dir("j", attrs(0o755, 81));
m.end(80);
```

`history_10` adds `m.file("j/k", deterministic_bytes(9, 33), attrs(0o601, 91)); m.end(90);`.

Done when every existing test passes unchanged and
`git diff --stat` shows only moves plus the new functions.

---

## 3. T16: baseline recovery (D10.8)

> Recovery from the checkpoint at *b* starts from snapshot manifest S(*b*)
> and applies the delta manifests after *b*. It needs **commit *b*'s
> record** … It needs **neither SQLite, nor delta manifest *b*, nor any
> earlier manifest.**

**Checklist DoD:** recovery succeeds with every image deleted, delta *b*
deleted, and all earlier manifests deleted.
**G2:** "Baseline recovery succeeds with all SQLite images and all earlier
manifests deleted."

Decisions used: D-Q29, D-Q30, D-Q30a.

### 3.1 Files touched

| File | Change |
|---|---|
| `crates/mochi-core/src/state.rs` | **new**: `AuthoritativeState` |
| `crates/mochi-core/src/lib.rs` | `pub mod state;`; phase status line in the crate docs |
| `crates/mochi-core/src/recovery.rs` | `catalog_from_snapshot`; the inline snapshot path uses it; module docs |
| `crates/mochi-core/src/publish.rs` | `apply_segment_deltas` (refactor); `BaselineRecovery`, `recover_baseline`, `recover_baseline_at_footer`; `TrustedRecovery`; `recover_with_trusted_head` |
| `crates/mochi-testkit/tests/t16_baseline_recovery.rs` | **new** |
| `crates/mochi-testkit/tests/c5_publication.rs` | adapt to `TrustedRecovery` (field access only) |
| `crates/mochi-testkit/tests/c4_recovery.rs` | doc comment on `missing_delta_before_a_snapshot_…` |
| `crates/mochi-testkit/src/fuzz.rs` | baseline step in `exercise_archive_open` |
| `docs/b2-implementation-checklist.md` | T16 row, Q29/Q30/Q30a, G2 list |

### 3.2 Step 16.1: `crate::state`

```rust
//! The authoritative state a checkpoint represents (Annex B.2 D10.3), in a
//! form that two representations can be compared in (D10.7) and recovery
//! can be checked against (D10.8). `from_snapshot` never touches SQLite.

use std::collections::BTreeMap;

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::FileVersionId;
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, FileVersion};
use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::{Attributes, Manifest, ManifestKind};
use crate::object::{ObjectId, ObjectRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeState {
    pub seq: u64,
    pub namespace: BTreeMap<ArchivePath, FileVersionId>,
    pub versions: BTreeMap<FileVersionId, (FileVersion, Vec<Extent>)>,
    pub chunks: BTreeMap<ObjectId, (ObjectRecord, Option<u64>)>,
    /// `None`: this representation carries no attributes (an image until C6,
    /// checklist Q6). Comparing `Some` with `None` is a difference.
    pub attributes: Option<BTreeMap<FileVersionId, Attributes>>,
}
```

`from_snapshot(m: &Manifest) -> Result<Self>`:
* `m.kind != ManifestKind::Snapshot` → `INVALID_ARGUMENT`.
* Fill `namespace` from `m.entries`, `versions` from `m.file_versions`
  (with extents), `chunks` from `m.chunks` (record and location), and
  `attributes` from `m.file_versions[*].attributes`.
* Check closure, failing with `RECORD_INVALID` on any gap:
  * every namespace version is in `versions`;
  * every `ExtentSource::Chunk { chunk, .. }` is in `chunks`.

  The decoder already enforces both, so this is defence in depth. It must
  return an error, never panic.
* **No `Catalog`, no `rusqlite`** anywhere in this function's call graph.
  Pin it with a unit test that runs `from_snapshot` on a decoded manifest
  inside a closure, with `catalog::replay_count()` (and, if feasible, a
  `#[cfg(test)]` counter on `Catalog::new_working`) unchanged.

`from_catalog(c: &Catalog, seq: u64) -> Result<Self>`:
* The same projection as `Manifest::snapshot_from_catalog`:
  `c.replay(Some(seq))`, then for each reached version
  `c.file_version(id)`, then for each chunk an extent reaches
  `c.object(id)` and `c.object_location(id)`.
* A version or chunk that is missing → `CATALOG_INVALID`.
* `attributes: None`.
* Do **not** make `snapshot_from_catalog` call this, and do not make this
  call it. Two independent paths are the point of the comparison. Share
  only the tiny `chunk ids of extents` helper if you like.

`with_attributes(self, a) -> Self`; `without_attributes(self) -> Self`.

`differences(&self, other: &Self, max: usize) -> Vec<String>`:
* Comparison order: `seq`; namespace keys and values; versions (record,
  then extents); chunks (record, then location); attributes (presence,
  then per version).
* The output is deterministic (BTreeMap order) and stops after `max`
  entries.
* Example lines:
  * `path "d/a": version 0x… vs 0x…`
  * `chunk 0x…: location Some(812) vs Some(9000)`
  * `version 0x…: attributes differ`
* Hex-print IDs with their existing `Debug`/`to_hex`. No secrets are
  involved.

Unit tests (in `state.rs`, building catalogs with `Catalog::new_working`
and the public insert API):

| Test | Asserts |
|---|---|
| `snapshot_and_catalog_projections_agree` | `from_snapshot(snapshot_from_catalog(c, …, attrs))` == `from_catalog(c, seq).with_attributes(attrs)` |
| `history_rows_are_not_state` | a catalog with an older, unreachable version and chunk projects the same as one without them |
| `differences_name_each_kind` | one test per kind: changed attribute, missing path, extra path, changed extent, changed location, attributes `Some` vs `None`; `differences` is empty for equal states |
| `from_snapshot_refuses_a_delta` | `INVALID_ARGUMENT` |
| `from_snapshot_does_not_use_sqlite` | as above |

### 3.3 Step 16.2: `catalog_from_snapshot` (recovery.rs)

```rust
/// A working catalog that materializes exactly commit `s.commit_seq`, built
/// from a snapshot manifest that the caller has already hash-verified and
/// identity-bound (D10.8). Reads no image.
pub(crate) fn catalog_from_snapshot(s: &Manifest) -> Result<Catalog> {
    // 1. kind must be Snapshot (INVALID_ARGUMENT otherwise).
    // 2. Catalog::new_working()
    // 3. for c in &s.chunks         { cat.insert_object(&c.record, c.location)? }
    // 4. for v in &s.file_versions  { cat.insert_file_version(&v.version, &v.extents)? }
    // 5. cat.append_commit(&Commit { seq: s.commit_seq, parent: None,
    //        ops: entries as NamespaceOp::Put in path order })?
    // 6. cat.set_meta(META_ARCHIVE_ID, s.archive_id.as_bytes())?
    // 7. cat.verify()?;  head_commit() == Some(s.commit_seq) else CATALOG_INVALID
}
```

* `META_ARCHIVE_ID` lives in `publish.rs`. Either move the constant to
  `catalog` (and re-export it from `publish`), or pass the key in. Prefer
  moving it: `recovery` must not depend on `publish`.
* Replace the snapshot branch of `recover_from_manifests` with a call to
  this function **only where there is no prior state** (`applied.is_none()`).
  The mid-chain snapshot diff (`diff_ops` against the current state) stays
  as it is: it handles a different case.
* Add an error-mapping note. Errors from steps 3–7 on a hash-verified
  snapshot mean the snapshot is invalid, not damaged. Map
  `INVALID_ARGUMENT`, `NAMESPACE_INVALID`, `EXTENT_INVALID`, and
  `CATALOG_INVALID` to `RECORD_INVALID` with the message
  "snapshot manifest *b*: …". Keep `IDENTITY_CONFLICT` as is, and keep
  `IO_ERROR` (there is none here, but keep the rule).

Unit tests (`recovery.rs` tests module, or `tests/t16_…`):
* `catalog_from_snapshot_matches_the_image`: for each checkpoint of a
  writer-produced `Every(3)` archive,
  `from_catalog(catalog_from_snapshot(S(c)), c)` equals `from_catalog(image(c), c)`,
  and `head_commit() == Some(c)`.
* `catalog_from_snapshot_starts_a_chain_at_b`: `SegmentApplier::new`
  accepts it and reports `head() == b`.
* `catalog_from_snapshot_refuses_a_delta`.

### 3.4 Step 16.3: one delta loop (pure refactor)

Move the body of the `for pair in entries.windows(2)` loop in
`replay_segment` into:

```rust
/// Apply deltas b+1 ..= h (from `entries`, which walk_segment produced,
/// b first) onto `applier`, which holds b. For each j: load delta j
/// (hash, decode, D11 identity, parent seq) unless j == h and
/// `head_manifest` is given; check its parent link against commit j-1's
/// key 6 (Q22); apply atomically; with `attributes`, add the versions it
/// introduces (reintroduction → RECORD_INVALID, D10.4). Stops at the first
/// failure.
fn apply_segment_deltas(
    src: &dyn ReadStorage,
    entries: &[HistoryEntry],
    head_manifest: Option<&Manifest>,
    applier: &mut SegmentApplier,
    mut attributes: Option<&mut BTreeMap<FileVersionId, Attributes>>,
    opts: &ReadOptions,
) -> Result<()>;
```

`replay_segment` calls it with `Some(head_manifest)`. Nothing else changes
in this commit.

Proof that it is pure: every test in `t11_t12_replay.rs`, `t12_oracle.rs`,
`catalog/apply/tests/work_bounds.rs`, and `c5_publication.rs` passes
unchanged, and so do the golden archive tests.

### 3.5 Step 16.4: `recover_baseline` (publish.rs)

```rust
/// Result of baseline recovery (D10.8) for one head.
#[derive(Debug)]
pub struct BaselineRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    pub segment: SegmentInfo,
    /// Query-only. Materializes commits b ..= h: `replay(Some(s))` works
    /// for those, not before b. No `META_WRITER_PARAMS` (D-Q29).
    pub catalog: Catalog,
    /// Promised attributes of every version reachable at the head.
    pub attributes: BTreeMap<FileVersionId, Attributes>,
}

pub fn recover_baseline_at_footer(
    src: &dyn ReadStorage,
    footer_offset: u64,
    opts: &ReadOptions,
) -> Result<BaselineRecovery>;

fn recover_baseline(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    opts: &ReadOptions,
) -> Result<BaselineRecovery>;
```

`recover_baseline`, in this order (each step's failure is returned as is;
there is no partial result and no earlier state):

| # | Action | Reads | Failure |
|---|---|---|---|
| 1 | `read_descriptor(src, &head.commit, head.commit_offset, opts)` | descriptor | `DESCRIPTOR_INVALID`, … (D-Q30a) |
| 2 | `walk_segment(src, head, opts)` → `(entries, info)` | footers and commit records *b* … *h* | as for open (T11) |
| 3 | `base = &entries[0]`; S(*b*) = `read_bound_manifest(src, &base.commit, &checkpoint_snapshot_ref(&base.commit)?, base.commit_offset, ManifestKind::Snapshot, opts)` | S(*b*) | `STORED_INTEGRITY_FAILED` / decode code / `ENVELOPE_INVALID` (identity) |
| 4 | `catalog_from_snapshot(&s_b)` | none | `RECORD_INVALID` (3.3 mapping) |
| 5 | `attrs` = `s_b.file_versions` → map | none | none |
| 6 | `SegmentApplier::new(catalog)`; `apply_segment_deltas(src, &entries, None, &mut applier, Some(&mut attrs), opts)` | deltas *b*+1 … *h* | as for open (T12, Q22) |
| 7 | `applier.head() == h`, else `RECORD_INVALID` "replay did not reach the head commit" | none | |
| 8 | `reachable_attributes(applier.namespace(), attrs)`; failure → `RECORD_INVALID` via `attributes_incomplete` wording, adapted to "cannot recover" | none | |
| 9 | `into_catalog()`, `make_query_only()` | none | |

Commit *b*'s key 6 hash, which delta *b*+1's parent link is checked
against in step 6, comes from the **record** read in step 2. Delta *b* is
never loaded. This is the D10.8 point; the trace test pins it.

`recover_baseline_at_footer` builds the `HistoryEntry` the way
`open_at_footer` does: `validate_footer`, then `footer_names_commit` (else
`FOOTER_INVALID`), then `read_commit`. It does not classify the tail.

### 3.6 Step 16.5: `TrustedRecovery`

```rust
#[derive(Debug)]
pub struct TrustedRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    /// Manifest-chain recovery (C4/C5), anchored on the head commit's key-6
    /// hash. `Err` when it cannot run at all (for example the head's delta
    /// manifest is gone, or no manifest frame could be scanned).
    pub chain: std::result::Result<ManifestRecovery, MochiError>,
    /// D10.8, attempted only when `chain` does not reach `head_seq`
    /// (D-Q30). `None` = not attempted.
    pub baseline: Option<std::result::Result<BaselineRecovery, MochiError>>,
}

impl TrustedRecovery {
    /// §11.1 scope for `seq`:
    /// * baseline Ok and b <= seq <= h: Snapshot at h, Historical below;
    /// * else chain Ok: chain.scope_for(seq);
    /// * else PayloadSalvage.
    pub fn scope_for(&self, seq: u64) -> RecoveryScope;
    /// The catalog that reaches the head: the baseline's, else the chain's if
    /// its snapshot range ends at head_seq.
    pub fn head_catalog(&self) -> Option<&Catalog>;
    /// Attributes at the head, from the same source as `head_catalog`.
    pub fn head_attributes(&self) -> Option<&BTreeMap<FileVersionId, Attributes>>;
}
```

`recover_with_trusted_head`:
1. `locate_head` and `read_commit` (as today), then build the head
   `HistoryEntry`.
2. Scan the committed prefix for `RecoveryManifest` frames (as today). A
   scan that stops early is not an error.
3. `chain = recover_from_manifests(&found, Some(head.delta_manifest.stored_hash), &opts.limits)`.
   Keep the `Err`; do not `?` it.
4. `reached = matches!(&chain, Ok(r) if r.snapshot_range.map(|(_, l)| l) == Some(head_seq))`.
5. If `!reached`: `baseline = Some(recover_baseline(src, head_entry, opts))`.
6. Return. **The function itself fails only on head location and on the
   head commit record** (`NO_VALID_HEAD`, `FOOTER_INVALID`,
   `RECORD_INVALID`, `IO_ERROR`).

Update `c5_publication.rs` call sites: `rec.scope_for(..)` is unchanged;
`rec.catalog` becomes `rec.head_catalog()`; for chain-specific fields use
`rec.chain.as_ref().unwrap()`.

### 3.7 Step 16.6: tests, `crates/mochi-testkit/tests/t16_baseline_recovery.rs`

Fixtures:
* **A** = `write(CheckpointPolicy::Every(4), &history_long()[..8])` gives
  cp0 d1 d2 d3 **cp4** d5 d6 d7. Head 7, *b* = 4.
* **B** = `write(CheckpointPolicy::Every(6), &history_10())` gives
  cp0 d1 … d5 **cp6** d7 d8 d9.

"State equals" always means: `AuthoritativeState::from_catalog(&rec.catalog, s)`
== `AuthoritativeState::from_catalog(&open_at_footer(original, s).catalog, s)`
for each *s* in *b*..=*h*, and `attrs_by_path(head ns, rec.attributes)` ==
`steps[h].attrs`. Use the **undamaged** original for the right-hand side.

| Test | Setup | Assertions |
|---|---|---|
| `t16_recovers_with_images_delta_b_and_earlier_manifests_destroyed` (**DoD**) | A. `wipe` image 0, image 4, delta 4, deltas 0–3, S(0). | `recover_with_trusted_head`: `chain` is `Err` or does not reach 7; `baseline` is `Some(Ok)` with `segment.base_seq == 4`; states 4..=7 equal; attributes equal the model; `scope_for(7)` Snapshot; `scope_for(5)` Historical; `scope_for(2)` is not Snapshot or Historical. |
| `t16_recovers_with_everything_before_b_destroyed` | A. As above, and also `fill(0)` every byte of commits 0–3's commit frames and footers, and of their data objects. Keep the descriptor and everything from commit 4's first object on. | `recover_baseline_at_footer(7)` is `Ok`, states equal. This shows nothing before *b* is needed. |
| `t16_baseline_reads_only_b_record_s_b_and_later_deltas` | A, undamaged. `Tracing` around `recover_baseline_at_footer(7)`. | Every read is within {descriptor; `[commit_offset, footer_offset + 72)` of 4..=7; S(4); deltas 5..=7}. No read overlaps image 4, delta 4, or any byte belonging to commits 0–3. Model on `t11_opening_reads_only_the_segment`. |
| `t16_checkpoint_head_recovers_from_its_snapshot_alone` | A. `wipe` image 4 and delta 4. | `recover_baseline_at_footer(4)` is `Ok`; state 4 equals; attributes equal `steps[4]`. |
| `t16_matches_open_for_every_head_under_every_policy` | `history()` under `EveryCommit`, `Never`, `Every(2)`, `Every(3)`; undamaged. | For every footer: baseline state and attributes equal the opened state and the model. |
| `t16_damaged_snapshot_refuses_without_trying_an_earlier_checkpoint` | A. `damage` S(4). S(0) and everything else intact. | `recover_baseline_at_footer(7)` is `STORED_INTEGRITY_FAILED`. Through `recover_with_trusted_head`: the chain reaches 7 (deltas intact), so `baseline` is `None`. **Second case:** also `wipe` delta 2. Now the chain does not reach 7, `baseline` is `Some(Err(STORED_INTEGRITY_FAILED))`, and `scope_for(7)` is not Snapshot. |
| `t16_damaged_delta_in_segment_refuses` | A. `damage` delta 6. | `STORED_INTEGRITY_FAILED`. |
| `t16_first_delta_link_is_checked_against_commit_b_record` | Forge: on A's head, append a commit 8 delta on base 4 whose delta manifest links to S(4)'s hash (as in `q22_link_to_the_base_snapshot_is_record_invalid`). | `recover_baseline_at_footer(8)` is `RECORD_INVALID`. |
| `t16_snapshot_bound_to_another_commit_is_refused` | Forge a checkpoint commit 8 whose snapshot ref names S(4) by its correct hash, offset, and length. | `ENVELOPE_INVALID` (D11 identity). |
| `t16_incomplete_base_snapshot_refuses` | `checkpoint_with_incomplete_snapshot()` | `RECORD_INVALID` |
| `t16_bad_descriptor_refuses` | A. `damage` the descriptor. | `DESCRIPTOR_INVALID` |
| `t16_missing_delta_before_a_checkpoint_is_bridged_by_commit_records` | B. `wipe` delta 3. | `chain`: `chain_broken_at == Some(4)` and no snapshot range; `baseline` `Some(Ok)` with base 6; `scope_for(9)` Snapshot; `scope_for(7)` Historical; `scope_for(3)` FileRecovery. |
| `t16_baseline_not_attempted_when_the_chain_reaches_the_head` | A, undamaged. | `baseline.is_none()`; `head_catalog()` is the chain's. |
| `t16_recovered_catalog_is_query_only` | A, undamaged; `recover_baseline_at_footer(7)`. | Each of the four public mutators is refused with `CATALOG_INVALID` (as `q25_replayed_reader_catalog_refuses_every_public_mutator`). |

Mutation checks to record:
1. Skip `check_delta_parent_link` in `apply_segment_deltas`, only in the
   baseline call (add a temporary flag). `t16_first_delta_link_…` fails.
2. In step 4, open image *b* instead of building from S(*b*). The trace
   test and both destruction tests fail.
3. Take attributes from the deltas only (start from an empty map). The
   attribute assertions and `t16_matches_open_…` fail.
4. On an S(*b*) failure, retry from the previous checkpoint. The
   snapshot-refusal test fails.
5. Drop `make_query_only`. The query-only test fails.

### 3.8 Step 16.7: fuzz and docs

* `mochi_testkit::fuzz::exercise_archive_open`: after an open that fails
  with `STORED_INTEGRITY_FAILED`, call `recover_baseline_at_footer` on the
  head footer. Assert it does not panic and, on `Ok`, that
  `head_seq` equals the footer's sequence and `catalog.head_commit()`
  equals it. The stable-Rust smoke tests then exercise it over every
  archive vector.
* `recovery.rs` module docs: replace "unused until T16" and "returns with
  T16" with a pointer to `publish::recover_baseline`.
* `lib.rs` crate docs: add `state`.
* `c4_recovery.rs`: the doc comment on
  `missing_delta_before_a_snapshot_is_not_bridged_without_commit_records`
  names `t16_missing_delta_before_a_checkpoint_is_bridged_by_commit_records`.
* Checklist:
  * T16 row → **Implemented**, with evidence (tests, mutation results,
    "no fixture changed");
  * Q29, Q30, Q30a as **Decided [delegated]**, with the section-1 text;
  * "What G2 still needs": T16 implemented, T17 next.

---

## 4. T17: damage scope (D10.9)

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

Decisions used: D-Q31, D-Q32, D-Q33, D-Q34, D-Q35, D-Q36, and the C5 test
decision.

### 4.1 Files touched

| File | Change |
|---|---|
| `crates/mochi-core/src/publish.rs` | `is_stored_damage`; `CatalogSource`; `OpenedHead` fields; `base_catalog`; `check_image` (factored from `open_checkpoint_catalog`); `open_at`/`replay_segment` use them; module docs |
| `crates/mochi-core/src/damage.rs` | **new**: `assess_damage` and its types |
| `crates/mochi-core/src/report.rs` | `SeqRange`; `Finding::affected` |
| `crates/mochi-core/src/lib.rs` | `pub mod damage;` |
| `crates/mochi-testkit/src/fuzz.rs` | `archive_open` assertions for the new fields |
| `crates/mochi-testkit/examples/c5_append_bench.rs` | compile fix if it reads `OpenedHead.manifest` |
| `crates/mochi-testkit/tests/c5_publication.rs` | the C5 test decision (section 1) |
| `crates/mochi-testkit/tests/t17_damage_scope.rs` | **new** |
| any test that reads `opened.manifest` | `.as_ref().unwrap()` (grep `\.manifest\b`) |
| `docs/b2-implementation-checklist.md` | T17 row, Q31–Q36, R7 note |

### 4.2 Step 17.1: types

```rust
/// Where an opened commit's catalog came from (D10.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogSource {
    /// The base checkpoint's catalog image: the normal path.
    Image,
    /// Rebuilt from the base checkpoint's snapshot manifest, because the
    /// image's stored bytes are damaged (D-Q32). Same commit, never an
    /// earlier state.
    SnapshotManifest { image_error: MochiError },
}

pub struct OpenedHead {
    // …existing fields…
    /// This commit's delta manifest. `None` only for a checkpoint opened for
    /// reading whose delta manifest's stored bytes are damaged (D-Q31).
    pub manifest: Option<Manifest>,
    /// Why `manifest` is `None`.
    pub manifest_error: Option<MochiError>,
    pub catalog_source: CatalogSource,
}
```

### 4.3 Step 17.2: read path

`check_image` (factored out of `open_checkpoint_catalog`; same checks,
same order):

```rust
/// Image of checkpoint `cp`: stored hash, envelope bound to cp (D11),
/// SQLite open (read-only or writable), head == cp.seq, archive ID.
fn check_image(src, cp: &HistoryEntry, image_ref: &ObjectRef, opts, writable: bool) -> Result<Catalog>;
```

`base_catalog`:

```rust
/// Base checkpoint b's catalog, writable (the caller replays onto it and,
/// for readers, makes it query-only afterwards).
/// * Ok(image)                                  → (catalog, Image)
/// * Err(e) if mode == Read && is_stored_damage(&e):
///     S(b) via read_bound_manifest; catalog_from_snapshot(&s_b)
///     → Ok((catalog, SnapshotManifest { image_error: e }))
///     if S(b) fails too: Err(MochiError::new(e.code,
///         format!("{}; the snapshot manifest could not be used either: {}", e.message, s_err.message)))
/// * Err(e) otherwise                            → Err(e)   (append; non-damage codes)
fn base_catalog(src, base: &HistoryEntry, opts, mode: OpenMode) -> Result<(Catalog, CatalogSource)>;
```

`open_at` changes:

```text
manifest = read_bound_manifest(… Delta …)
match (manifest, commit.metadata, mode):
  Ok(m)                                   → (Some(m), None)
  Err(e) if checkpoint && Read && is_stored_damage(&e) → (None, Some(e))
  Err(e)                                  → return Err(e)
checkpoint head:
  (catalog, source) = base_catalog(src, &head_entry, opts, mode)?
  Read: catalog.make_query_only()?
  Append: unchanged (snapshot attributes, Q24)
delta head:
  replay_segment(…) now takes base_catalog's result and returns the source too
  (the head's delta manifest is Some here: a delta head never takes the None branch)
```

* `replay_segment`'s signature gains `CatalogSource` in its result tuple
  (extend the `Replayed` alias).
* The "catalog materializes the commit" check at the end of `open_at`
  stays, for both sources.

**The normal path must not read S(*b*).**
`t11_opening_reads_only_the_segment` must pass **unchanged**. If it needs
edits, the implementation is wrong.

**C5 test change:** apply the section-1 decision in this commit, with the
commit-message wording given there.

### 4.4 Step 17.3: `crate::damage`

```rust
//! Damage scope (Annex B.2 D10.9): which stored objects of the published
//! history are damaged, which commits can still be read and how, each
//! commit's recoverability (D-Q33), and the affected sequence ranges.
//! Read-only: takes `&dyn ReadStorage`. A job (progress + cancellation).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole { DeltaManifest, SnapshotManifest, CatalogImage, CheckpointPair }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectDamage {
    pub seq: u64,
    pub role: ObjectRole,
    /// Offset of the object (for CheckpointPair: the image's).
    pub offset: u64,
    pub error: MochiError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readability { Image, SnapshotRebuild, Unreadable { code: ErrorCode } }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitScope {
    pub seq: u64,
    pub base_seq: u64,
    pub readable: Readability,
    pub recoverability: Status,
    /// Indices into `DamageReport::objects` that determine this commit's status.
    pub causes: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect { Unreadable, ReadsFromSnapshot, Degraded, NoReadAffected }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedRange { pub first: u64, pub last: u64, pub effect: Effect, pub cause: usize }

#[derive(Debug)]
pub struct DamageReport {
    pub head_seq: u64,
    pub objects: Vec<ObjectDamage>,   // ascending (seq, role)
    pub commits: Vec<CommitScope>,    // index = seq, 0 ..= head
    pub ranges: Vec<AffectedRange>,   // one per entry of `objects`, same order
    pub objects_checked: u64,
}

impl DamageReport {
    pub fn integrity(&self) -> Status;              // D-Q33
    pub fn recoverability(&self) -> Status;         // worst over commits
    pub fn head_recoverability(&self) -> Status;
    /// One per damaged object: code = error.code, severity Error,
    /// message = role + seq + error message, affected = its range (D-Q35).
    pub fn findings(&self) -> Vec<Finding>;
}

pub fn assess_damage(src: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>)
    -> Result<DamageReport>;
```

`assess_damage` algorithm:

1. `history = commit_history(src, opts)?` (D-Q36). Then
   `ctx.report("damage", 0, Some(n))`.
2. For each entry *j* (check cancellation between objects):
   * Delta: `read_bound_manifest(src, &e.commit, &e.commit.delta_manifest, e.commit_offset, Delta, opts)`.
     On `Err` → `ObjectDamage { role: DeltaManifest, … }`.
   * If checkpoint:
     * snapshot via `read_bound_manifest(… Snapshot …)`;
     * image via `check_image(…, writable: false)`.
     * Each `Err` → its own `ObjectDamage`.
     * Keep both `Ok` values for T15's pair check (step 15.3 adds that
       here; leave a `// T15:` marker).
   * `objects_checked += 1` per object attempted.
   * **Classification of an `Err`** (D-Q31/32): record **every** error as
     an `ObjectDamage`, whatever its code. For the readability derivation,
     only `is_stored_damage` errors are "damaged". Any other error means
     the object is an invalid record, and commits that need it are
     `Unreadable { code }` with **no** fallback, exactly as the read path
     behaves.
3. Segment ends: walk the forms once. `end(x)` = the last *s* ≥ *x* such
   that no checkpoint lies in (*x*, *s*], capped at head.
4. For each *h* (base *b* from its record, `b = h` for a checkpoint):
   * a delta *j* in (*b*, *h*] failed (any code) → `Unreadable { code of lowest such j }`;
   * else image *b* ok → `Image`;
   * else image error `is_stored_damage` and S(*b*) ok → `SnapshotRebuild`;
   * else → `Unreadable { code: image error code }`;
   * then recoverability from the D-Q33 table. `causes` lists the object
     indices used.
5. Ranges, one per `ObjectDamage`:

   | Damaged object | Range | Effect |
   |---|---|---|
   | delta *j*, *j* a delta commit | [*j*, end(*j*)] | `Unreadable` |
   | delta *j*, *j* a checkpoint, `is_stored_damage` | [*j*, *j*] | `NoReadAffected` |
   | delta *j*, *j* a checkpoint, other code | [*j*, end(*j*)] | `Unreadable` (D10.4: the record is invalid) |
   | image *c* (stored damage), S(*c*) ok | [*c*, end(*c*)] | `ReadsFromSnapshot` |
   | image *c*, other code, or S(*c*) also failed | [*c*, end(*c*)] | `Unreadable` |
   | S(*c*), image ok | [*c*, end(*c*)] | `Degraded` |
   | S(*c*), image also failed | [*c*, end(*c*)] | `Unreadable` |

6. **Self-check before returning** (internal consistency, `CATALOG_INVALID`
   "internal: …" on violation): every `Unreadable` commit has a range with
   effect `Unreadable` that covers it, and every range's first and last
   are ≤ head.

**Agreement with the read path is a test, not a hope.** In the matrix
test, `commits[h].readable` must predict exactly what `open_at_footer(h)`
returns.

### 4.5 Step 17.4: report field

* In `report.rs`:
  * `#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] pub struct SeqRange { pub first: u64, pub last: u64 }`;
  * on `Finding`, `#[serde(default, skip_serializing_if = "Option::is_none")] pub affected: Option<SeqRange>`.
* Update every `Finding { … }` literal (grep) with `affected: None`.
* Test: an existing report's JSON is byte-identical before and after.
  Serialize a fixture `Report` built in the test both ways.

### 4.6 Step 17.5: tests, `crates/mochi-testkit/tests/t17_damage_scope.rs`

Fixture **M** = `write(CheckpointPolicy::Every(3), &history_long())` gives
cp0 d1 d2 | cp3 d4 d5 | cp6 d7 d8. That is three segments, each with
deltas, so every D10.9 clause occurs.

**Independent oracle**, in the test file. It must not call `damage.rs` or
`publish.rs` logic, only the fixture's known forms:

```rust
const CP: [bool; 9] = [true, false, false, true, false, false, true, false, false];
#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
enum Obj { Delta(u64), Image(u64), Snap(u64) }
fn base(h: u64) -> u64 { (0..=h).rev().find(|&s| CP[s as usize]).unwrap() }
fn end(x: u64) -> u64 { /* last s >= x before the next checkpoint, capped at 8 */ }
fn expected_open(h: u64, d: &BTreeSet<Obj>) -> Result<Expect, ErrorCode>;  // Image | Snapshot
fn expected_recoverability(h: u64, d: &BTreeSet<Obj>) -> Status;          // D-Q33 table
fn expected_ranges(d: &BTreeSet<Obj>) -> Vec<(u64, u64, Effect)>;           // 4.4 step 5
```

Damage is applied with `damage()` (a flipped byte), so every damaged
object fails with `STORED_INTEGRITY_FAILED`.

| Test | Setup | Assertions |
|---|---|---|
| `t17_damage_matrix_matches_d10_9` (**DoD**) | 18 cases: {Delta(*j*) : *j* ∈ 0..=8}, {Image(*c*)}, {Snap(*c*)}, {Image(*c*), Snap(*c*)} for *c* ∈ {0, 3, 6} | For each case and each head 0..=8: (a) `open_at_footer` matches `expected_open`; on `Ok`, `catalog_source` matches and `read_state` equals `steps[h].after`; on `Err`, the code equals the oracle's. (b) `assess_damage`: `commits[h].readable` and `.recoverability` match; ranges as a set of `(first, last, effect)` equal `expected_ranges`; `integrity() == FAIL`; `recoverability()` equals the worst of the oracle; `head_recoverability()` equals `expected_recoverability(8, d)`. On failure, print the whole 18×9 matrix (expected vs actual) before panicking. |
| `t17_two_damages_in_different_segments` | {Delta(1), Snap(6)} and {Image(0), Delta(4)} | as the matrix (the oracle handles sets) |
| `t17_undamaged_control` | M | no objects, no ranges; every commit `Image`/`PASS`; `integrity()`, `recoverability()`, `head_recoverability()` all `PASS`; `objects_checked == 9 + 2·3` |
| `t17_damaged_snapshot_reads_continue_degraded` | Snap(3) | heads 3..=5 open with `CatalogSource::Image`; recoverability `DEGRADED`; one range (3, 5, `Degraded`); a `Report` built from `findings()` plus Integrity `FAIL` and Recoverability `DEGRADED` passes `Report::validate`; its overall status is `FAIL`, not `PASS`; the finding's `affected` is `{3, 5}` |
| `t17_damaged_image_reads_rebuild_from_snapshot` | Image(3) | heads 3..=5: `SnapshotManifest { image_error.code == STORED_INTEGRITY_FAILED }`; states equal the model; head 2 still reads from image 0 |
| `t17_checkpoint_own_delta_damage_does_not_affect_reads` | Delta(3) | head 3: `manifest == None`, `manifest_error.code == STORED_INTEGRITY_FAILED`, state equals the model; heads 4 and 5 open; range (3, 3, `NoReadAffected`); recoverability of 3..=5 `PASS`; `open_append` with head 3 (truncate the copy at 3's footer end) fails `STORED_INTEGRITY_FAILED` |
| `t17_invalid_record_is_not_treated_as_damage` | `checkpoint_with_incomplete_snapshot()` gives a hash-valid but invalid S. Also a forged checkpoint whose delta manifest is hash-valid but has an unknown required feature (reuse the `t12_unknown_required_feature…` forge). | The first: reads open from the image (S not needed). The second: open is `UNSUPPORTED_FEATURE`, **no** tolerance (D-Q31); `assess_damage` marks it `Unreadable { UNSUPPORTED_FEATURE }`. |
| `t17_no_fallback_to_an_earlier_checkpoint` | {Image(3), Snap(3)} | heads 3..=5 are `Err(STORED_INTEGRITY_FAILED)`; the error message names both failures |
| `t17_append_refused_when_base_image_damaged` | Image(6), head 8 | reads open via the snapshot; `open_append` fails `STORED_INTEGRITY_FAILED`; `s.contents()` unchanged |
| `t17_fallback_reads_s_b_only_after_the_image_fails` | Image(3); `Tracing` on head 5 | reads ⊆ T11 allowed set ∪ {S(3)}; the first read of S(3) comes after a read of image 3 |
| `t17_unsupported_image_does_not_fall_back` | `fixtures/golden/c5/reject-archive-bare-image.mochi` | still `UNSUPPORTED_FEATURE` |
| `t17_assessment_is_read_only_and_cancellable` | M with Snap(3) | BLAKE3 of the bytes is equal before and after; with the token cancelled up front → `CANCELLED` |
| C5 change | section 1 | as specified there |

Mutation checks to record:
1. Disable the snapshot rebuild in `base_catalog`. Every Image case of the
   matrix fails, as does `t17_damaged_image_…`.
2. Treat a checkpoint's own delta as required again. Delta(0/3/6) rows
   fail, as does `t17_checkpoint_own_delta_…`.
3. Widen `is_stored_damage` to all codes. `t17_invalid_record_…` and
   `t17_unsupported_image_…` fail.
4. Make a range end at `end + 1`. The matrix fails.
5. Report `PASS` instead of `DEGRADED`. The degraded test and the matrix
   fail.
6. Rebuild from the previous checkpoint's S. The no-fallback test fails.

### 4.7 Step 17.6: fuzz and docs

* `exercise_archive_open`:
  * replace `head.manifest.identity()` with `if let Some(m) = &head.manifest`;
  * assert that `manifest.is_none()` implies a checkpoint and
    `manifest_error` is stored damage;
  * assert that `CatalogSource::SnapshotManifest { image_error }` implies
    `is_stored_damage(image_error)`;
  * also run `assess_damage` on any input whose `commit_history` succeeds,
    and assert its self-check holds.

  This needs `is_stored_damage` to be `pub` (or a public mirror in
  `damage`).
* `publish.rs` module docs:
  * D10.9 read behaviour;
  * S(*b*) is read only after an image's stored bytes fail;
  * a checkpoint's own delta manifest is optional for reads;
  * append is unchanged.
* Checklist:
  * T17 row → **Implemented** (evidence, mutation results, "no fixture
    changed; one C5 assertion changed by design");
  * Q31–Q36 as **Decided [delegated]**;
  * add "Finding.affected" to the R7 inputs.

---

## 5. T15: adoption (D10.7, §18.1)

> Before publishing a checkpoint commit, the writer re-reads both checkpoint
> representations from their serialized, hashed bytes and checks that each
> one's authoritative state equals the source snapshot: the snapshot
> manifest is decoded with the canonical CBOR codec, without SQLite; the
> image's state is extracted from the image. On any mismatch the commit
> fails, and there is no new head. `verify` repeats this comparison; a
> mismatch there is a `FAIL`.

**Checklist DoD:** a planted divergence blocks the head, and the previous
head stays valid. **G2:** an adoption mismatch blocks the head.

Decisions used: the T15 cost decision (section 1), D-Q33 (mismatch →
`DEGRADED`), and the four below. Record them as Q37–Q40, **Decided
[delegated]**.

* **Q37: codes.**
  * Semantic disagreement → `CHECKPOINT_MISMATCH` (registered; exit 3
    from the writer, `FAIL` in verify).
  * A re-read whose hash differs from what was written → storage fault,
    `STORED_INTEGRITY_FAILED`.
  * A failed read → `IO_ERROR`/`OUT_OF_BOUNDS` as mapped.
  * In every case: no head, rolled back.
* **Q38: attributes.** The snapshot half compares attributes. The image
  half does not: the image holds none until C6 (Q6). Put a
  `// C6: include attributes once the image stores them (Q6, Q38).` at the
  comparison.
* **Q39: two codes for one substance.** For a published checkpoint, verify
  reports image/snapshot disagreement as `CHECKPOINT_MISMATCH`. Q24 keeps
  `RECORD_INVALID` for append-open (decided, not ratified). Record the
  inconsistency for review; do not change Q24.
* **Q40: writer state.** Adoption fails inside `prepare` (before the
  commit record and footer), so the existing `roll_back` removes the
  unpublished bytes. The writer is **not** poisoned: nothing was published
  and no sync failed (§12.2). The next commit may succeed.

### 5.1 Files touched

| File | Change |
|---|---|
| `crates/mochi-core/src/publish.rs` | `phase::ADOPT`; `adopt_checkpoint`; `prepare` wiring; `CheckpointTamper` + `set_checkpoint_tamper` (test-controls); `check_checkpoint_representations`; step table in module docs |
| `crates/mochi-core/src/damage.rs` | pair check → `CheckpointPair` finding |
| `crates/mochi-testkit/src/sim.rs` | `Fault::CorruptAppend` + unit test |
| `crates/mochi-testkit/tests/t15_adoption.rs` | **new** |
| `crates/mochi-testkit/examples/c5_append_bench.rs` | report the `adopt` phase as a column |
| `docs/benchmarks/c5-append.md` | new dated section |
| `docs/b2-implementation-checklist.md` | T15 row, Q37–Q40, G2 list |

### 5.2 Step 15.1: test hooks (test-controls only)

Writer:

```rust
#[cfg(any(test, feature = "test-controls"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTamper {
    /// XOR the POSIX mode of the first file version (by ID) that has POSIX
    /// attributes, in the serialized snapshot only.
    SnapshotAttributes,
    /// Drop the last namespace entry (by path) from the serialized snapshot,
    /// and its version and chunks if no remaining entry or version uses them.
    SnapshotOmitsEntry,
    /// Serialize an image whose head commit omits the transaction's last
    /// namespace operation.
    ImageOmitsLastOp,
}
// in the existing test-controls impl block:
pub fn set_checkpoint_tamper(&mut self, t: Option<CheckpointTamper>);
```

* Store it in a field that exists only under the same cfg. In
  production, `prepare` must compile to exactly today's code plus
  adoption.
* Apply it **only to what is serialized**. `source` (5.3) is never
  touched.
* After tampering, the snapshot must still pass `check_structure`
  (`to_stored` runs it). The divergence is semantic, not a decode error.
  If `SnapshotOmitsEntry` would drop the only entry, use the second-last;
  test transactions always have at least two entries after commit 0.
* `ImageOmitsLastOp`: build the tampered catalog as follows:
  * start from `self.catalog.duplicate()`, or for commit 0 from
    `Catalog::new_working()` plus the two meta keys;
  * insert the same chunks and versions as `cat`;
  * `append_commit` with `ops[..ops.len() - 1]`;
  * `publish()` it.

  Requires at least one op; `set_checkpoint_tamper` documents that.

`SimStorage`:

```rust
/// The `append_index`-th append stores its bytes with `data[at] ^= xor`,
/// and returns Ok as if they were written as given: silent write corruption.
CorruptAppend { append_index: usize, at: usize, xor: u8 },
```

Unit test in `sim.rs`: the stored bytes differ at exactly one position,
and the returned offset is unchanged.

### 5.3 Step 15.2: `adopt_checkpoint` and writer wiring

Add `pub const ADOPT: &str = "adopt";` to `phase`, between `CHECKPOINT`
and `COMMIT_RECORD`. Its doc comment: "Re-reading and checking both
checkpoint representations (D10.7)."

Check whether any test pins the phase list (grep `phase::` and
`"checkpoint"`). If one does, extend it.

New order in `prepare`'s checkpoint branch:

```text
ctx.report(CHECKPOINT)
reachable = reachable_attributes(&after, attributes.clone())?          // writer's own map, not the snapshot
source    = AuthoritativeState::from_catalog(&cat, seq)?.with_attributes(reachable.clone())
snapshot  = Manifest::snapshot_from_catalog(&cat, …, &attributes)?      // as today
[test-controls: tamper the snapshot copy]
snapshot_ref = append_object(snapshot.to_stored()?)
check_cancelled
image = cat.publish()?   [test-controls: or the tampered catalog's]
image_ref = append_object(encode_image_record(image, identity))
check_cancelled
ctx.report(ADOPT)
adopt_checkpoint(&self.storage, &source, &snapshot_ref, &image_ref, &identity,
                 self.archive_id, &self.params.encode()?)?
check_cancelled
attributes = reachable                                                  // replaces the snapshot-derived map
```

```rust
/// D10.7, §18.1. Re-read the two checkpoint representations from storage,
/// hash-verify them against the refs just written, decode them, and compare
/// each with `source`. Bounds: the writer default limits (B.2.3), not the
/// writer's own read limits, because the writer bounded its output by those.
fn adopt_checkpoint(
    storage: &dyn ReadStorage,
    source: &AuthoritativeState,
    snapshot_ref: &ObjectRef,
    image_ref: &ObjectRef,
    identity: &RecordIdentity,
    archive_id: ArchiveId,
    writer_params: &[u8],
) -> Result<()>
```

Its body:
1. **Snapshot:**
   * `load_verified(storage, snapshot_ref, snapshot_ref.end()?, Limits::WRITER_DEFAULT.max_frame_len, "snapshot manifest")?`;
   * `Manifest::from_stored(&stored, &Limits::WRITER_DEFAULT, &CborLimits::default())?`;
   * kind must be Snapshot; `identity()` must equal `identity`;
   * `got = AuthoritativeState::from_snapshot(&m)?`;
   * if `got != *source` → `CHECKPOINT_MISMATCH`, with message
     "snapshot manifest of commit {seq} disagrees with the source state: {differences(…, 8).join("; ")}".
2. **Image:**
   * `load_verified(storage, image_ref, image_ref.end()?, …, "catalog image")?`;
   * `decode_image_record(&stored, identity, &Limits::WRITER_DEFAULT)?`;
   * `Catalog::open_image(image, &CatalogLimits::default())?`;
   * `head_commit() == Some(seq)`; `meta(META_ARCHIVE_ID) == archive_id`;
     `meta(META_WRITER_PARAMS) == writer_params`. Any failure →
     `CHECKPOINT_MISMATCH`, naming the field;
   * `got = AuthoritativeState::from_catalog(&img, seq)?`;
   * `// C6:` note (Q38);
   * `got != source.clone().without_attributes()` → `CHECKPOINT_MISMATCH`.
3. **Errors from the decoders** on hash-valid bytes (steps 1–2) mean the
   writer serialized something its own reader rejects. Map them to
   `CHECKPOINT_MISMATCH`, keeping the original code and message in the
   text. Keep `STORED_INTEGRITY_FAILED`, `IO_ERROR`, and `OUT_OF_BOUNDS`
   as they are (Q37).

`prepare` already returns errors into `commit`, which calls `roll_back`.
No change is needed there. **Do not** call `poison` (Q40).

Ordering constraints, which must hold and which a reviewer will check:
* adoption runs **after** both objects are appended;
* adoption runs **before** the commit record;
* adoption happens before step 6's `sync_data`. The re-read comes from the
  storage handle's current contents (the page cache for `OsStorage`),
  which is what "the serialized, hashed bytes" means. Durability is the
  sync's job.

`sync_order_follows_spec_12_2` must pass unchanged: reads are not traced
`Op`s.

### 5.4 Step 15.3: reader-side check (for C7's verify)

```rust
/// D10.7 for a published checkpoint: both representations hash-verified
/// and decoded, then compared without attributes (Q38). A disagreement is
/// CHECKPOINT_MISMATCH. Either representation failing to load returns that
/// failure unchanged (T17 reports it as object damage).
pub fn check_checkpoint_representations(
    src: &dyn ReadStorage,
    cp: &HistoryEntry,
    opts: &ReadOptions,
) -> Result<()>;
```

In `assess_damage`, at the `// T15:` marker: if both the image and S(*c*)
loaded, compare
`from_snapshot(&s).without_attributes()` with `from_catalog(&img, c)`.
If they differ:
* push `ObjectDamage { role: CheckpointPair, error: CHECKPOINT_MISMATCH }`;
* readability is unchanged (reads use the image);
* recoverability of [*c*, end(*c*)] is `DEGRADED` (D-Q33);
* the range is (c, end(c), `Degraded`).

Extend the T17 oracle's `expected_ranges`, and the self-check, for this
role.

### 5.5 Step 15.4: tests, `crates/mochi-testkit/tests/t15_adoption.rs`

Shared assertion helper:

```rust
/// Commit `tx` with `tamper` set, expect `code`, then show nothing changed
/// and the writer still works.
fn assert_blocked(w: &mut ArchiveWriter<SimStorage>, s: &SimStorage, tx: Transaction,
                  tamper: CheckpointTamper, code: ErrorCode, prev_model: &State);
```

It checks:
* the commit error code;
* `s.contents()` is byte-identical to before the commit;
* `w.head_seq()` is unchanged;
* `open_head(s)` opens the previous head, and its `read_state` equals
  `prev_model`;
* the audit log's last event is `RolledBack`;
* after `set_checkpoint_tamper(None)`, the same `tx` commits, `open_head`
  is the new head, and its state equals the model.

| Test | Shows |
|---|---|
| `t15_snapshot_attribute_divergence_blocks_the_head` | `SnapshotAttributes` on commit 2 of `history()` → `CHECKPOINT_MISMATCH`. **DoD.** The message names "snapshot manifest" and "attributes". |
| `t15_snapshot_missing_entry_blocks_the_head` | `SnapshotOmitsEntry` → `CHECKPOINT_MISMATCH` |
| `t15_image_divergence_blocks_the_head` | `ImageOmitsLastOp` → `CHECKPOINT_MISMATCH`; the message names "catalog image" |
| `t15_first_commit_divergence_leaves_an_empty_file` | tamper on commit 0 → `CHECKPOINT_MISMATCH`; `s.contents().is_empty()` (compare `a_cancelled_first_commit_leaves_no_descriptor_behind`) |
| `t15_delta_commits_are_not_adopted` | policy `Never`, tamper set on commit 1 (a delta) → commit succeeds and opens: there is no checkpoint, so nothing is compared. Commit 0 was written before the tamper was set. |
| `t15_checkpoints_under_every_policy_are_adopted` | policies `Every(2)` and `Every(3)`, tamper `SnapshotAttributes` on a checkpoint commit (seq 2 or 3) → blocked; on a delta commit → succeeds |
| `t15_silent_write_corruption_is_caught_on_reread` | two cases: `Fault::CorruptAppend` on the snapshot's append index, and on the image's (find the indices from a clean dry run's `trace()`) → `STORED_INTEGRITY_FAILED`; file rolled back; previous head opens |
| `t15_user_read_limits_do_not_block_adoption` | writer with `ReadOptions.limits.max_frame_len` lowered below a typical snapshot frame but above the commit frame → commit succeeds (re-read uses `WRITER_DEFAULT`). If the existing writer refuses such options earlier, record that and drop this row. |
| `t15_verify_reports_a_published_mismatch` | `checkpoint_with_incomplete_snapshot()` (forged, published) → `check_checkpoint_representations` is `CHECKPOINT_MISMATCH`; `assess_damage` has one `CheckpointPair` finding at that seq with range (seq, seq, `Degraded`); `open_head` is unaffected; Q24 append refusal still `RECORD_INVALID` |
| (existing suites) | every C5, T11/T12, oracle, golden, and T16/T17 test still passes: the no-tamper controls under every policy |

Mutation checks to record:
1. Skip the snapshot comparison. Both snapshot tests fail.
2. Skip the image comparison. The image test fails.
3. Compare the in-memory `StoredObject`s instead of re-reading storage.
   The corruption test fails.
4. Include attributes on the image side. Every checkpoint commit fails, so
   the controls catch it.
5. Build `source` from the snapshot manifest instead of from `cat`.
   `SnapshotOmitsEntry` passes when it should fail, so that test fails.
6. Poison the writer on a mismatch. The "next commit succeeds" part of
   `assert_blocked` fails.

### 5.6 Step 15.5: benchmark

`c5_append_bench.rs` already times phases from progress events. Add an
`adopt` column, and keep `checkpoint` as the time up to the `adopt` event.
Run `cargo run --release -p mochi-testkit --example c5_append_bench` and
add a dated section, "Schema 1 with adoption (T15)", to
`docs/benchmarks/c5-append.md` with:
* the same §27 setup table (copy it, and change only what changed);
* the results table with the new column;
* the **ratio** of adoption to checkpoint at each N;
* one paragraph:
  * adoption is mandatory (§18.1, D10.7) and uses the full reader open;
  * it is paid per checkpoint, which is every commit until T14;
  * T14 bounds checkpoint frequency;
  * any adoption-dominated cost is a G3/T14 input, not a reason to weaken
    the check.

Do not change the earlier sections.

### 5.7 Step 15.6: docs

* `publish.rs` module docs: in the §12.2 step table, add "4b adopt:
  re-read and compare both checkpoint representations (D10.7); on mismatch
  roll back, no head" after the checkpoint row.
* Checklist:
  * T15 row → **Implemented**;
  * Q37–Q40 as **Decided [delegated]**;
  * under "What G2 still needs", T15, T16, and T17 implemented with CI
    outstanding;
  * state that G2's GC item (C9) remains open and is not closed by this
    work.

---

## 6. Done means

* **Implemented** (per task): its tests and mutation checks pass locally,
  the full command set in 2.1 is clean, and the checklist row records the
  evidence.
* **Done** (per task): the same tests pass in CI on Ubuntu and Windows
  (checklist "Status"). CI runs on pull requests and on pushes to `main`,
  so the PR carrying the work provides it. Record the run ID in the row.
* **G2 is not claimed** from these tasks alone. It also needs the GC
  retention test (C9) and green CI for T11–T13.

## 7. Stop and ask

Stop, record the question, and ask before going on if:
* any golden fixture's bytes would change;
* a test outside those named here needs an assertion changed. The C5 test
  in section 1 is the only planned one;
* `t11_opening_reads_only_the_segment` or `sync_order_follows_spec_12_2`
  would need edits;
* a spec rule appears to conflict with a decision in section 1;
* the benchmark shows adoption more than 10× the checkpoint phase at any
  N. That is not a reason to weaken the check, but the owner should know
  before T14 is planned.
