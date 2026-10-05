# Fuzz targets

cargo-fuzz targets live in `fuzz/fuzz_targets/`. Each one calls an exerciser in
`mochi-testkit::fuzz`, which asserts the framing layer's invariants (spans are
contiguous and in bounds, accepted footers point before themselves, and so on)
and is also run deterministically on stable Rust by
`crates/mochi-testkit/tests/c1_golden.rs::fuzz_smoke_over_mutated_vectors` (all
targets, via `exercise_all`) `c2_golden.rs::fuzz_smoke_over_mutated_objects`, and `c5_golden.rs::fuzz_smoke_over_mutated_inputs`.

| Target | Phase | Exercises | Status |
|---|---|---|---|
| `frame_walker` | C1 | spec §8.6 structural walk: RLE one-byte rule, reserved blocks, bounds, limits | written |
| `envelope` | C1, B.2 | binary envelope v0 (spec Annex B.2.2, D11): every obligation; accepted input must re-encode byte-identically | written; seed with `../fixtures/golden/b2` |
| `footer` | C1 | footer + preceding skippable header, spec §8.4 | written |
| `object_decode` | C2 | object codec, spec §9.1/§9.3: single-frame rule, declared-size and decoded-length checks, bounded output | written; seed with `../fixtures/golden/c2` too |
| `catalog_image` | C3 | catalog image open (spec §10.5, §22.1): header checks, schema equality, SQLite and MOCHI verification | written; seed with `../fixtures/golden/c3` |
| `cbor_decode` | C4 | canonical-CBOR subset (spec D2): every excluded feature, non-shortest forms, bounds before allocation, re-encode identity | written; seed with `../fixtures/golden/c4` |
| `manifest_decode` | C4 | recovery manifests (spec §11): closed schema, structural rules, frame | written; seed with `../fixtures/golden/c4` |
| `commit_decode` | C5 | commit records (spec §12.1, §9.2): closed schema, ID recomputation, fail-closed features, re-encode identity | written; seed with `../fixtures/golden/c5` |
| `archive_open` | C5 | whole-archive open (spec §8.4, §12.2): head location, tail classification, footer → commit → manifest → checkpoint with hash-before-parse | written; seed with `../fixtures/golden/c5/valid-archive-3-commits.mochi` |
| `descriptor_decode` | B.2 | archive descriptor v0 (spec Annex B.2 D12): closed schema, generation and draft refusals, offset-0 rule, re-encode identity | written; seed with `../fixtures/golden/b2` |
| checkpoint + delta replay | C6 | spec §10.6 (blocked on O26) | not yet |

Run (needs nightly and `cargo install cargo-fuzz`). Put a scratch corpus directory
first so new inputs do not land in `fixtures/golden/`; the golden vectors are
read as seeds:

```bash
cd fuzz
cargo +nightly fuzz run frame_walker corpus/frame_walker ../fixtures/golden/c1 -- -max_total_time=60
```

Rule (AGENTS.md): a parser change extends a fuzz target and adds a golden vector.
