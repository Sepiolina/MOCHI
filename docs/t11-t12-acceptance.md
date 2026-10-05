# T11 + T12 acceptance criteria

**Status: accepted for implementation on approach** (review 2026-10-02,
with decisions 14–21 and amendments 1–5 below; amendment 2 replaced
2026-10-03, checklist Q22). Approval of approach is not
verification; each task is done only when its tests pass in CI (Ubuntu, and
Windows where the gate says so). Decisions are in spec Annex B.2
D10.4–D10.6; tasks and gate in `b2-implementation-checklist.md` (T11, T12 →
G2). Nothing here changes the wire format.

## Scope

In: the reader's base rule and chain walk (T11), the writer's base
derivation (T11), and replay of a segment onto its base checkpoint (T12),
for `open_head`, `open_at_footer`, and `open_append`.

Out, owned elsewhere: per-operation mutation and lookup maxima (T13); the
α/*F* checkpoint trigger (T14); adoption (T15); baseline recovery from S(*b*)
without SQLite (T16); damage-scope reporting and `DEGRADED` (T17); GC
retention (C9); `verify` (C7).

## Implementation order (review)

1. Segment validator — **component done** (`mochi_core::segment`).
2. Atomic incremental applier — **component done**
   (`mochi_core::catalog::SegmentApplier`).
3. Test checkpoint policy in the writer (decision 14) — **done locally**
   (`CheckpointPolicy`, `ArchiveWriter::set_checkpoint_policy`).
4. Wire into `open_head`, `open_at_footer`, `open_append`; integration
   tests — **done locally** (`tests/t11_t12_replay.rs`); oracle test —
   **done locally** (`tests/t12_oracle.rs`). Golden vectors and fuzz
   exerciser assertions — **done locally** (see "Fixtures, fuzzing,
   benchmark").

## Decisions (review 2026-10-02)

| # | Decision |
|---|---|
| 14 | Delta writer before T14: non-default `test-controls` feature, enabled only by `mochi-testkit`. Production stays `EveryCommit` until T14. `Never` = checkpoint at commit 0, none after. `Every(0)` is rejected. Cargo features are additive: enabling the feature enables the controls for the whole build; it is not an isolation boundary (documented in `mochi-core/Cargo.toml`). |
| 15 | T12 does the architectural fix: namespace state built once from the base, then validated and applied incrementally; no whole-history replay per delta. T13 owns lookup and mutation accounting. |
| 16 | The base rule is checked for **every delta in the segment**, plus the reached checkpoint's identity. Descriptor-hash consistency across base and segment is implemented as an explicit clarification **to be recorded in the spec**, not as an already-established reader requirement. |
| 17 | A mismatching base hint is **ignored** once the authenticated parent walk has established the base; identity, ancestry, sequence, and checkpoint status stay mandatory. Parent hints keep the existing rule (`RECORD_INVALID`). The asymmetry is recorded for spec clarification. |
| 18 | Duplicate introduction during replay, identical or conflicting, is `RECORD_INVALID`, via a strict replay insertion path. The general-purpose idempotent inserts keep their behaviour. Reusing an existing ID by reference is not reintroduction. |
| 19 | An unknown operation kind under supported, closed schema 1 is `RECORD_INVALID`. Unsupported schema versions and unknown required features remain `UNSUPPORTED_FEATURE`. |
| 20 | Oracle: logical-state comparison plus independent physical-location validation (amendment 4). Physical offsets and layout-dependent commit hashes are not required to match. |
| 21 | Append attributes are reconstructed from S(*b*) plus the ordered deltas, using the actual attribute semantics, not an unconditional map merge. If a required input fails validation, append refuses and publishes nothing. This is an append prerequisite, distinct from reader-open requirements. |

## T11 — base rule and chain walk

**Reader.** For a head *h*: walk parent links from *h* down to *b* (its own
sequence if a checkpoint, else its recorded base's), validating each parent
footer at its hint exactly as for the head. Then check the segment:

* the chain is contiguous, linked by commit ID, one archive;
* the commit reached at *b* has the recorded base's ID and is a checkpoint;
* every commit *b*+1 … *h* is a delta whose base is (*id_b*, *b*);
* every commit in the segment references the same descriptor object
  (decision 16; provisional code `DESCRIPTOR_INVALID`, D12 "mismatched");
* the base hint is compared with where the walk found *b* and only reported
  (`SegmentInfo::base_hint_mismatch`).

**Writer.** A delta's base is the parent if the parent is a checkpoint,
else the parent's base. Commit 0 is a checkpoint.

**No search (amendment 3).** The trace assertion is:
* no scan for an alternative checkpoint;
* no speculative traversal of unrelated history;
* reads limited to required metadata, followed references (including a
  failing reference's target), and referenced replay inputs;
* failure never substitutes an earlier state.

**Tests.**

| Case | Expected | Where |
|---|---|---|
| Tampered base ID (record self-consistent) | `RECORD_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) |
| Base off the ancestry (another archive's checkpoint, same sequence) | `RECORD_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) |
| Base is a delta | `RECORD_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) |
| Base skips a later checkpoint | `RECORD_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) |
| Inconsistent segment (head copies its parent's base; an intermediate delta names another) | `RECORD_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) (intermediate case; the head-copies case is the skip-a-checkpoint row) |
| Descriptor differs within the segment | `DESCRIPTOR_INVALID` | component ✔; integration ✔ (`t11_t12_replay`) |
| **Wrong base hint, otherwise valid** (decision 17) | **opens** through its authenticated ancestry; mismatch reported in `OpenedHead::segment` | component ✔; integration ✔ (`t11_t12_replay`) |
| Parent traversal offset broken mid-segment (D10.6 as amended, Q23) | Resolves to another valid footer: `RECORD_INVALID`; resolves to no valid footer: `FOOTER_INVALID`; never searched for, no earlier state returned | integration ✔ (`t11_parent_traversal_offset_to_another_footer_is_record_invalid`, `t11_parent_traversal_offset_to_no_footer_is_footer_invalid`) |
| No search | trace assertion above | integration ✔ (`t11_t12_replay`): every read lies in the segment's inputs; S(*b*) and the base's delta manifest are not read |
| Writer derivation | property: every delta's base equals the rule | ✔ proptest across policies and a mid-history reopen (`writer_base_derivation_follows_the_rule`) |

## T12 — replay

**Mechanism.** Open commit *b*'s image as today (hash first, envelope bound
to *b*, catalog head = *b*), writable. Build the namespace once. For each
*j* = *b*+1 … *h*:

1. Load commit *j*'s delta manifest: stored hash first, decode under the
   B.2.3 limits, bind identity to commit *j*. **Parent link (amendment 2,
   as replaced 2026-10-03, Q22):** the delta manifest's parent sequence
   must equal *j* − 1, and `parent.1` must equal the stored hash in the
   preceding commit's key-6 manifest reference. This applies equally when
   the preceding commit is a checkpoint. A mismatch is `RECORD_INVALID`.
   The base snapshot (key 5) is not an alternative parent-link target.
2. Apply it atomically (amendment 1): one SQLite transaction for every row
   it adds, and the namespace change staged with an undo log proportional to
   the manifest. On any failure, including at `COMMIT`, the database **and**
   the in-memory namespace are at *j* − 1. The namespace change is kept only
   after the transaction commits. No whole-namespace clone per manifest.
3. Operations apply in array order; validity is checked against the
   completed commit state (§10.2).

The public open API returns only an error on failure; rollback state is
inspected through the applier directly.

**Complexity (amendment 5).** Namespace initialization occurs once per
open; replay performs no whole-history or whole-namespace rebuild per
manifest. T13 establishes the specified lookup and mutation bounds.

**Tests.**

| Test | Detail | Where |
|---|---|---|
| Incremental equals full | After several deltas, the held namespace equals a from-scratch replay of the result, and the catalog verifies | component ✔ |
| No per-manifest rebuild | Full namespace replays counted: one at construction, none per `apply` | component ✔ |
| Completed-state semantics | Child put before its parent in one commit is accepted | component ✔ |
| Semantic failure | Ops invalid **in the completed commit state** (parent never exists; delete of a path absent throughout; directory replaced by a file orphaning a child; unknown version), each first / middle / last: error, and database dump, namespace, and head unchanged; the applier is usable afterwards | component ✔ |
| Injected failure | Every fault point of a multi-row manifest (each SQL row mutation, and the point immediately before `COMMIT`): state unchanged, then the same manifest applies cleanly | component ✔ (18 points) |
| Duplicate IDs | Identical and conflicting reintroduction from the base; across two deltas; within one manifest: `RECORD_INVALID`, state unchanged | component ✔; integration ✔ (identical reintroduction from the base, through `open_head`) |
| Reuse by reference | `PUT` of an existing version, extent over an existing chunk: accepted | component ✔ |
| Shape | Wrong sequence, wrong parent sequence, snapshot kind: `RECORD_INVALID`; required feature: `UNSUPPORTED_FEATURE` | component ✔ |
| Parent link (Q22) | Link to the base's snapshot (at the head and mid-segment), to another delta, wrong sequence, missing: `RECORD_INVALID`; checkpoint as the preceding commit, key 6: opens | component ✔; integration ✔ (`t11_t12_replay`) |
| Oracle property test | Amendment 4, below | ✔ locally (`tests/t12_oracle.rs`): proptest (32 cases; B reopened only when the split is interior, single-session otherwise) plus a fixed history under all four B policies that always reopens at sequence 4 (asserted: a delta head under `Never`, `Every(3)`, `Every(7)`; a checkpoint head under `Every(2)`), with accepted-operation coverage asserted (directory creation, renames, file and directory deletes). One normalized field (`object_locations.stored_offset`). Frame walking and stored hashing checked independently through `mochi-format`; decoding shares core's `decode_verified`. A avoids delta-segment replay, not `Catalog::replay`. Mutations: 6 replay/attribute defects caught, one (complete-but-wrong attributes) only by the model comparison. |
| Unknown feature / unknown op mid-segment | `UNSUPPORTED_FEATURE` / `RECORD_INVALID` (decision 19); nothing returned | integration ✔ (`t11_t12_replay`); control: the unedited manifest opens |
| Damage in the segment | Flipped byte in delta *j*: `STORED_INTEGRITY_FAILED` at every head *j* … *h*; earlier heads and heads after a later checkpoint still open | integration ✔ (`t11_t12_replay`) |
| No fallback | With delta *j* damaged, `open_head` returns no earlier state | integration ✔ (`t11_t12_replay`) |
| Append on a delta head | Decision 21; result passes the oracle | integration ✔ (`t11_t12_replay`) against the transaction model (states and snapshot attributes); oracle comparison pending |

**Oracle (amendment 4).** Same seeded `SeqIds` and transactions, written
under `EveryCommit` (A) and `Never` / `Every(n)`, n ∈ {2, 3, 7} (B). For
every sequence *c*:

* **Logical state equal:** namespace; file versions and extents; object
  identities and semantic metadata (encoding, protection, lengths, content
  and stored hashes); dependencies; promised attributes. Only fields
  explicitly identified as layout-dependent are normalized.
* **Physical locations validated in each archive independently:** the
  referenced frame exists, has the applicable kind and identity, its sizes
  and bounds hold, and its stored hash matches. Not hash alone.
* **`SeqIds`:** assert that identities intended to be policy-independent
  (object, file-version, transaction IDs) match; not commit IDs, which cover
  layout-dependent content.
* **Attributes:** expected values come from the generated transaction model,
  not only from the reconstruction path under test.

## Fixtures, fuzzing, benchmark

* **c5 golden:** `valid-archive-delta-segment.mochi` (checkpoint, three
  deltas, checkpoint, one delta) and `valid-archive-wrong-base-hint.mochi`
  (decision 17), plus one reject archive per rejecting T11 row and per T12
  parent-link, duplicate-ID, and unknown-feature case, each in `vectors.txt`
  with its expected code. Existing vectors are not regenerated.
* **Fuzz:** `archive_open` asserts, for anything it opens, the segment rules
  and that the catalog head equals the footer's sequence.

  *Status (local, 2026-10-03):* c5 golden — the two valid archives and 13
  rejects are checked in, listed in `c5/vectors.txt`, frozen, and checked by
  behaviour (frozen file and fresh build); existing vectors untouched (only
  `vectors.txt` gained lines; its prior content is a byte-exact prefix).
  Excluded: the Q23 no-footer case (provisional code, not frozen) and the
  unknown-operation case (not in this list). Fuzz — `exercise_archive_open`
  re-walks the opened head's segment independently of `mochi_core::segment`
  and asserts the base rule, contiguity, one descriptor, the reported base,
  and catalog head = footer sequence; the stale "head is a checkpoint"
  assertion is removed. The stable-Rust smoke test runs it on every archive
  vector and on mutations of the delta-segment vector. Mutations: core
  skipping the segment rules, and core skipping replay and its head checks,
  are each caught by the exerciser. libFuzzer itself not run.
* **Benchmark:** no claim. Any replay column added to `c5_append_bench` is
  labelled T12 smoke data; G3 (T32) is the evidence.

## Validation results (2026-10-03)

Reported as run; nothing here is a CI result. T11/T12 remain **Partial
(integrated locally)**; G2 open.

| Gate | Result |
|---|---|
| Linux (Ubuntu 24.04 sandbox, Rust 1.91.1): the CI `test` job's commands | **PASS**: `cargo fmt --all --check`; `cargo clippy --workspace --all-targets --locked -- -D warnings` (also with `mochi-core/test-controls`); `cargo test --workspace --locked`, 361 passed, 0 failed; `ci/check-invariants.sh` |
| GitHub CI (ubuntu-22.04, ubuntu-24.04, windows-latest; fuzz-smoke; doc-nightly) | **BLOCKED**: no runner access |
| Windows | **BLOCKED** on real Windows. Partial evidence only: the workspace cross-builds for `x86_64-pc-windows-gnu` (CI uses MSVC). Under Wine 9.0: 359 passed, 2 failed (`storage::os::tests::second_writer_is_refused_the_lock`, `os_storage_end_to_end`), both pre-existing storage locking. An isolated probe shows Wine's `unlock()` returns OS error 33 while actually releasing the lock; likely a Wine/`std` mismatch, unconfirmed. All T11/T12 suites passed under Wine. |
| libFuzzer (nightly 1.101.0, cargo-fuzz 0.13.2) | **PASS, local**: no crash, no artifacts. `archive_open` 601 s, 1,011,410 runs, seeded with all c5 vectors (incl. 17 archives), `-max_len=400000`. Replaying its 239-input corpus: 45 open (24 checkpoint heads, **21 delta heads**, 1 with a base-hint mismatch), so the segment assertions were reached on delta heads; refusals: `NO_VALID_HEAD` 111, `STORED_INTEGRITY_FAILED` 34, `FOOTER_INVALID` 21, `RECORD_INVALID` 16, `UNSUPPORTED_FEATURE` 11, `DESCRIPTOR_INVALID` 1. CI's seven fuzz-smoke targets and `commit_decode`, `descriptor_decode`: 60 s each, seeded as CI seeds (c1–c4), no crash. |
| Golden bytes | Unchanged this round. |

**CI fuzz matrix (approved 2026-10-04; changed, not yet run in CI).** The
fuzz-smoke matrix adds `archive_open` (seeded with `fixtures/golden/c5`,
`-max_len=400000`), `commit_decode` (c5) and `descriptor_decode` (b2). Each
target first executes every seed once (`-runs=0`), then fuzzes 60 s. That
the T11/T12 segment assertions are *reached* is a CI failure condition in
the stable test job: `fuzz_smoke_over_mutated_inputs` asserts both valid
archive vectors come back `OpenedDelta` (mutation-checked).

**Pre-existing CI bug found and fixed (2026-10-04).** libFuzzer requires
every corpus directory on its command line to exist; `fuzz/corpus/` is
gitignored. On a clean checkout the original fuzz-smoke command fails at
once (`The required directory "corpus/frame_walker" does not exist`, exit
1), reproduced locally with the literal command. A `mkdir -p` step now
precedes the runs for all ten targets. Consequence: the fuzz-smoke job
cannot have passed as written before this change.
