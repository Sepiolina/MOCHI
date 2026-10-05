# Golden fixtures

Valid, corrupt, and interrupted archives with their expected reports (ratification
artifact R8). The first set, `c1/`, covers the framing layer: 17 vectors (valid frames,
a valid session with a footer, and rejects for reserved blocks, oversized frames,
truncation, a bad footer digest, a bare v1.2-style trailer, a forged footer
inside a data payload, and a descriptor frame away from offset 0). They are frame-level vectors, not whole `.mochi` archives.

Rules (AGENTS.md):

- Golden files change deliberately, never by blanket regeneration. A format-affecting
  change updates the specific fixtures it affects and the PR explains each diff.
- The set includes archives with **hostile names** (for example `<img src=x onerror=…>`)
  so the desktop UI's "text only" rendering stays covered (plan D1, spec §23.3 #9).
- Fixtures are built by `mochi-testkit`, never from ambient randomness or wall-clock time.

## B.2 vectors

`b2/`: the Annex B.2 wire batch (drafts, R1/R2).

* 15 binary envelope v0 frames (`*-envelope-*`): one valid catalog-image
  envelope and one reject per D11 obligation, each with its own fault.
  Reader expectations are in `vectors.txt`. Payloads are minimal SQLite
  headers (signature and `user_version` only).
* 19 archive descriptor v0 frames (`*-descriptor-*`): two valid (minimal; with
  declared limits) and a reject for every CDDL rule, with the D12
  classification (`DESCRIPTOR_INVALID` for damage, `UNSUPPORTED_FEATURE` for
  refusals) in `descriptor-vectors.txt`.

c1 changed in this batch: `valid-skippable-all-registered` now starts with the
descriptor (its only valid offset), and `reject-descriptor-not-at-offset-0`
was added. Regenerate with `cargo test -p mochi-testkit --test b2_golden --
--ignored write_b2_golden_files`.

## C5 vectors

Commit record **schema 1** (`docs/schemas/commit-record-v1.cddl`; plan T8).

`c5/*.bin`: 26 commit records, each one complete stored frame. Valid: a root
checkpoint, a child checkpoint linked by commit ID, and a delta on base (the
child, by the base rule). 23 rejects with exactly one defect each: ID
mismatch; unknown key; missing delta-manifest key; missing descriptor key;
future schema; **legacy schema 0**; unknown metadata form; checkpoint without
a snapshot reference; delta carrying a snapshot reference; commit 0 as a
delta; base not before the commit; root with a parent; orphan; parent
sequence gap; descriptor off offset 0; unknown required feature; features not
increasing (`ENVELOPE_INVALID`, the shared D11 rule); short transaction ID;
non-canonical CBOR; wrong frame kind; trailing frame; payload not fully
consumed. Body edits recompute the
stored ID so each reject exercises its own rule. `vectors.txt` lists each
valid vector's commit ID and each reject's error code.

`reject-commit-legacy-v0.bin` is byte-identical to the schema-0
`valid-commit-root.bin` it replaces: the builder reconstructs it from the v0
CDDL, and that identity was checked when the set was regenerated.

`c5/valid-archive-3-commits.mochi`: the scripted 3-commit history from
`mochi_testkit::archive`, written by the schema-1 writer (descriptor at
offset 0; every commit a checkpoint binding delta manifest, snapshot
manifest, and image). Like the C3 image it must **keep opening** to the
expected state at every commit, and is not compared with a fresh build byte
for byte (it contains zstd and SQLite output). Regenerate deliberately with
`cargo test -p mochi-testkit --test c5_golden -- --ignored write_c5_golden_files`.

T10 regenerated `valid-archive-3-commits.mochi` (152,028 → 152,268 bytes):
each catalog image is now binary envelope v0 + SQLite image (+80 bytes each,
n = 0). Data frames are byte-identical; every frame after the first image
moves by 80 per earlier image, so the stored offsets recorded in later
catalogs and manifests change, and with them the delta-manifest parent
hashes, commit IDs, and footers. The `.bin` commit vectors are hand-built and
did not change.

`c5/reject-archive-bare-image.mochi`: the schema-1 archive as checked in at
T8/T9, before T10, **frozen**. Its images are bare SQLite. Head location and
the commit chain must still work; opening any commit must be refused at the
image envelope (`UNSUPPORTED_FEATURE`: envelope version 0x6574), never handed
to SQLite.

`c5/reject-archive-legacy-v0.mochi`: the schema-0 archive checked in at C5,
**frozen** (the write function never touches it). Head location must still
succeed on it, and opening must refuse it as legacy (`UNSUPPORTED_FEATURE`,
spec §26).

**T11/T12 archive vectors** (`docs/t11-t12-acceptance.md`, "Fixtures"),
built by `mochi_testkit::golden::c5_archive_vectors` and listed after the
commit vectors in `c5/vectors.txt` with their expected outcome at
`open_head`: `valid-archive-delta-segment.mochi` (checkpoint, three deltas,
checkpoint, one delta; every commit opens to `c5_segment_history`),
`valid-archive-wrong-base-hint.mochi` (decision 17: opens, mismatch
reported), and 14 `reject-archive-*.mochi`: one per rejecting T11 row and
per T12 parent-link (Q22), duplicate-ID, and unknown-feature case, plus
`reject-archive-parent-offset-no-footer.mochi` (D10.6 as amended, Q23: a
parent traversal offset that resolves to no valid footer is
`FOOTER_INVALID`; added 2026-10-04, distinct from the base-footer hint). Each
reject changes one rule relative to a valid writer-produced archive, by
appending forged commits (`mochi_testkit::forge`). **Frozen** when first
written: `write_c5_archive_vectors` creates only missing files and never
overwrites an existing one,
and `write_c5_golden_files` does not touch them. Checked by behaviour, on
the frozen file and on a fresh build. Deliberately not included: the
unknown-operation case (not in the acceptance list).

**G1 additions (2026-10-05, T30).** Seven more `reject-archive-*.mochi`,
appended to `vectors.txt` so earlier lines are unchanged. They give the D11
obligations that bind a CBOR record to its commit an archive-level vector:
`manifest-archive-id`, `manifest-transaction-id`, `manifest-sequence`
(`ENVELOPE_INVALID`), `manifest-hash` (`STORED_INTEGRITY_FAILED`),
`descriptor-hash` and `descriptor-archive-id` (`DESCRIPTOR_INVALID`). The
seventh is `segment-descriptor-differs-head-valid` (D10.6). The frozen
`reject-archive-segment-descriptor-differs.mochi` never reached that rule:
the forge copied the intermediate commit's wrong descriptor reference into
the head, so the head fails on its own reference. It stays frozen and is
still a valid reject (D12, own reference); the new vector has a head that
references the real descriptor. `binding_vectors_fail_on_their_own_rule`
pins each refusal's message. Also added: one `*-payload-not-consumed`
reject per CBOR record type (`c5/reject-commit-…`, `c4/reject-manifest-…`,
`b2/reject-descriptor-…`): a byte after the CBOR item inside the frame,
D11 "Payload length".

## C4 vectors

Recovery manifest **schema 1** (`docs/schemas/recovery-manifest-v1.cddl`;
plan T9).

`c4/*.bin`: 26 recovery manifests, each one complete stored frame. Valid:
delta(0); delta(1) linked to delta(0) by hash; and the snapshot S(1), with no
parent and the same sequence and transaction ID as delta(1). 23 rejects:
unknown key; missing entries key; missing or short transaction ID; future
schema; **legacy schema 0**; unknown required feature; features not
increasing (`ENVELOPE_INVALID`); symlink kind; Windows bits; mode bits;
unsorted versions; traversal path; orphan delta; root delta with a parent;
parent sequence gap; delta with entries; snapshot with a parent; snapshot with
operations; snapshot not self-contained; non-canonical CBOR; wrong frame
kind; trailing frame; payload not fully consumed. `vectors.txt` lists each
valid vector's stored-object
hash and each reject's error code. Records are hand-made, so nothing depends
on libzstd. `reject-manifest-legacy-v0.bin` is byte-identical to the schema-0
`valid-manifest-root-delta.bin` it replaces.

## C3 vector

`c3/valid-catalog-v0.sqlite`: a published catalog image with three commits (a
directory and a sparse file with a hole; a Windows name with an unpaired
surrogate; a rename). `crates/mochi-testkit/tests/c3_golden.rs` requires that
it keeps opening, verifying, and replaying to the expected snapshot at every
commit. It is deliberately **not** compared byte-for-byte with a fresh build:
SQLite may lay out pages differently in a later version, and that is not a
format change. Losing the ability to read this file would be one.

## C2 vectors

Built by `mochi_testkit::golden::{c2_object_vectors, c2_digest_vectors,
render_c2_manifest}`; checked by `crates/mochi-testkit/tests/c2_golden.rs`.

- `*.bin`: 13 hand-built single-object frames. Valid: raw, RLE, two-block, empty,
  each with content size and checksum (plan O21). Rejects: missing content size,
  missing checksum, block larger than a single-segment window, declared-size
  mismatch, overlong output, bad checksum, frame dictionary ID with no
  dictionary dependency, trailing frame, skippable frame. Never libzstd
  compressor output, so they do not change with the zstd library version.
  Checksums were computed independently (python `xxhash`) and are confirmed by
  libzstd accepting every valid vector.
- `vectors.txt`: 15 digest known answers (5 scopes × empty, `abc`, 1025 bytes)
  and each object vector's record length and expected outcome. `file-content`
  is plain BLAKE3 and matches `b3sum` (O20); the other separators are draft
  (R3).

The test recomputes every known answer from separator strings written out
literally, so a changed constant fails twice: in the file diff and in the
recomputation. Regenerate deliberately with
`cargo test -p mochi-testkit --test c2_golden -- --ignored write_c2_golden_files`.

## C1 vectors

Built by `mochi_testkit::golden::c1_vectors()`; `crates/mochi-testkit/tests/c1_golden.rs`
checks each file against its builder and its expected outcome. Regenerate one
deliberate run at a time with
`cargo test -p mochi-testkit --test c1_golden -- --ignored write_c1_golden_files`,
then review `git diff --stat fixtures/golden/c1` and explain each changed file.
Note the builders and files are generated together, so the independent check on
them is the libzstd differential test in `crates/mochi-format/tests/framing.rs`.
