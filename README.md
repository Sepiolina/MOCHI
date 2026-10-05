# MOCHI

An archive format and archiver, shipped the way WinRAR is: a format library plus an
end-user application.

> **Experimental. The format will change. Do not use this as your only copy.**
> A `.mochi` file is not a backup (spec §1.1). Every pre-1.0 build is
> "experimental / draft-compatible" (spec §28).

## Where to read first

1. [`AGENTS.md`](AGENTS.md): working rules and invariants. Read before changing anything.
2. [`docs/spec.md`](docs/spec.md): the specification (revision 2.0). Normative.
3. [`docs/implementation-plan.md`](docs/implementation-plan.md): phases, scope, Definition of Done.

## Layout

| Path | Role | Status |
|---|---|---|
| `crates/mochi-format` | Framing, record envelopes, digests. Pure, no I/O. | C1: frame walker, skippable writer, framed footer, draft envelope. C2: representation types, per-scope typed digests, unencrypted object codec |
| `crates/mochi-core` | Archive operations, jobs, reports, `Storage` trait | C0 harness; C2: object records, integrity checks; C3: catalog (paths, extents, namespace replay, published images); C4: recovery manifests; C5: commit records, §12.2 publication, head location, tail handling; Annex B.2 batch: archive descriptor, commit v1, delta manifests with checkpoints, D13 creation, D14 tail quarantine, D15 report results and timestamps |
| `crates/mochi-cli` | The `mochi` binary | C0: full command surface, every command reports honestly that it is not built |
| `crates/mochi-testkit` | Fault-injecting storage, fixtures | C0: `SimStorage`; C1/C2: golden vectors, fuzz exercisers, C2 property tests |
| `apps/mochi-desktop` | Tauri v2 app: React + TypeScript + Vite (O1); reads `.mochi` plus ZIP, 7z, RAR, `.tar.gz` (D8, read-only) | **Not started; unblocked** (D0 next on the desktop track). Windows 10 22H2+/11 and Ubuntu 22.04/24.04 (O11) |
| `fuzz/` | cargo-fuzz targets | `frame_walker`, `footer`, `envelope` (C1), `object_decode` (C2), `catalog_image` (C3), `cbor_decode`, `manifest_decode` (C4), `commit_decode`, `archive_open` (C5); see `fuzz/README.md` |
| `fixtures/golden/` | Valid / corrupt / interrupted archives | `c1/`: 16 framing vectors; `c2/`: 13 object vectors + 15 digest known answers; `c3/`: one catalog image that must stay readable; `c4/`: recovery-manifest vectors; `c5/`: commit-record vectors and archives that must stay readable; `b2/`: Annex B.2 vectors (binary envelope v0, archive descriptor). See `fixtures/golden/README.md` |
| `docs/ratification/` | Frozen format artifacts R1–R10 | Empty; written from working code |

Dependency direction is one-way: `mochi-format` ← `mochi-core` ← {`mochi-cli`, desktop backend}.

## Build and test

Requires Rust 1.89 or newer (`rust-toolchain.toml` pins 1.91).

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
ci/check-invariants.sh          # AGENTS.md rules clippy cannot express
cargo run -p mochi-cli -- --help
```

## What works today

Phases C1 (framing), C2 (object model and digests), C3 (catalog and
namespace), C4 (recovery manifests), and C5 (commit and single-file
publication) are implemented. The library can create, append to, and open a
real `.mochi` file; the CLI does not expose that yet (C14).

- Structural frame walker (spec §8.6): skippable and data frames, the RLE
  one-byte rule, reserved block types and header bit rejected, `Block_Maximum_Size`
  enforced, every step bounds-checked against configurable limits (§8.5).
- Skippable-frame writer; it refuses data frames and reserved magics.
- Framed footer (§8.4): 72-byte skippable frame, domain-separated BLAKE3 digest,
  keyless validation including the preceding skippable header.
- Record envelopes (§8.3 as amended by Annex B.2 D11): the binary envelope v0
  for opaque payloads, with one fault per validation obligation, and shared
  feature and identity checks for the CBOR-native records. **Draft** (R2).
- Golden vectors in `fixtures/golden/c1/` and three fuzz targets.
- **C2.** The four spec §9.1 representations are distinct types; encoded
  plaintext has no public byte accessor, so it cannot be hashed. Every §9.2
  digest scope has its own draft domain separator and its own output type.
  Unencrypted objects encode to exactly one Zstandard frame and decode only to
  their exact recorded length. Object records support stored-integrity and
  content-integrity checks (§20.1) that are tested to be independent.
  Encryption and dictionaries return `UNSUPPORTED`, never a silent fallback.

Decisions settled during C2 (plan §9): object IDs are random 256-bit values
from the OS (O19); the file-content hash is plain BLAKE3, equal to `b3sum`
(O20); data objects must carry a content size and frame checksum (O21);
integrity failures exit 1, with a documented precedence (O22, O13).

Product decisions (plan §9): the desktop app is Tauri v2 with React and TypeScript
(O1); it supports Windows 10 22H2 / 11 and Ubuntu 22.04 / 24.04 on x86-64, not macOS
(O11); and it opens ZIP, 7z, RAR, and `.tar.gz` read-only (O9, phase D8). The
project is licensed MIT OR Apache-2.0 (O23), which permits distributing UnRAR
in the D8 helper; its notice is in `THIRD-PARTY-NOTICES.md`.

- **C3.** Reversible archive paths (byte components; Windows names as WTF-8, so
  unpaired surrogates round-trip). Extent validation rejecting gaps, overlaps,
  out-of-range reads, and length mismatches. `PUT`/`DELETE` replay checked
  against the completed commit state, verified against an independent model
  that catches four planted bugs. An in-memory SQLite catalog whose published
  images are byte strings through `Storage`, opened only after header, exact
  schema, integrity, foreign-key, and MOCHI checks. SQLite cannot detect a bit
  flip inside a stored value, so C5 must hash-verify images before opening them.

The nine format decisions D1–D9 are recorded in spec Annex B.1, and D10–D15
are decided in Annex B.2, with their evidence tracked by gates G1–G9. Every
product decision in plan §9 is settled except two drafts that close with
ratification: error-code names (O15) and walker defaults (O18). Notably: manifests and commit bodies use
deterministic CBOR (D2); encryption is XChaCha20-Poly1305, passphrase-only in
1.0 (D3); TAR compatibility is opt-in (D4).

- **C4.** A strict deterministic-CBOR codec (byte-identical to `ciborium`; every
  excluded feature rejected by name). Recovery manifests per a draft CDDL schema
  (`docs/schemas/`). Rebuilding a destroyed catalog from manifests found by
  scanning, trusting only what chains back from the head, and claiming snapshot
  recovery only from a verified baseline. A forged-but-consistent history is
  detectable only against a trusted head, which C5's footer supplies.

- **C5.** Commit records (draft CDDL in `docs/schemas/`) and the §12.2
  publication protocol: lock, head validation, content → manifest → catalog
  checkpoint → commit → sync → footer → sync → directory sync, then
  `LOCAL_COMMITTED`. Readers find the head at EOF or, after an interrupted
  write, by scanning, and say which. Every referenced object is hash-verified
  before it is parsed. Uncommitted tails are removed only on request, with an
  audit record; anything that might be a damaged commit is refused. Tested
  against halts at every write, torn and lost writes, lying and failing
  syncs, and truncation at every byte.
  C5 stored a full catalog image with every commit, so archive size grew
  roughly quadratically with history (`docs/benchmarks/c5-append.md`). The
  Annex B.2 batch replaced that: each commit stores a delta manifest, and a
  checkpoint (image plus snapshot manifest) is written only when the deltas
  since the last one reach α·max(*B*, *F*). Measured to 10,000 commits in
  `docs/benchmarks/t32-scaling.md` (gate G3).

**Annex B.2 batch in progress** (`docs/b2-implementation-checklist.md`,
which tracks every task, decision, CI run, and gate). Archives written today
carry the archive descriptor at offset 0, v1 commit records, and delta
manifests with checkpoints; pre-batch drafts (no descriptor, v0 commits or
manifests) are refused as legacy (§26). Gates G1 (conformance), G3
(scaling), G5 (capacity), and G7 (creation, including power loss on ext4
over dm-flakey) have passed, and G9's (reports) criteria are met at
library level. Still open: the CLI flags for
truncation and limits (T29, with C14), native
Windows durability evidence (T33, gate G6), and the GC hold test (C9).

**C6 (read path) has started:** `mochi_core::read` lists any commit's
snapshot and streams a file out of it, verifying every chunk and the whole
file's content hash. Restore to a directory is next. The CLI does not
expose reading yet (C14).

The phase after the batch is **C6: read path and extraction**. D0 (desktop shell) and D2 (create and add) can start. Until later phases land,
`mochi verify` and friends exit `3` with `NOT_IMPLEMENTED`, and post-1.0 commands
(`inventory`, `split`, `join`, `mount`) exit `4` with `UNSUPPORTED_FEATURE`, as
spec §23.2 requires.

## License

Licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option. Unless you state otherwise, any
contribution you submit for inclusion is dual-licensed as above, without
additional terms. Third-party components with other conditions are listed in
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
