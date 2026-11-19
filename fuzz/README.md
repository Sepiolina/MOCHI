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
| `parity_frame` | C12 | parity frames (spec Annex B.2.11 D21, R6): digest before fields, layout rule, re-encode identity; then input-derived group shapes and erasure patterns: ≤ m always rebuilds exactly, more is `BeyondCapacity` | written; seed with `../fixtures/golden/c12` |
| `foreign_zip` | D8 | ZIP list/test/read path of `mochi-foreign` (plan D8): every message survives the wire round trip, names and entry counts stay within the limits, data frames stay within the chunk size | written; seed with `../fixtures/foreign` |
| `foreign_tar` | D8 | plain tar list/test/read path, same assertions | written; seed with `../fixtures/foreign` |
| `foreign_targz` | D8 | tar.gz path (gzip decoding, ratio and total-output guards), same assertions | written; seed with `../fixtures/foreign` |
| `foreign_tarzst` | D8 | tar.zst path (pure-Rust ruzstd decoding, frame checksums, ratio and total-output guards), same assertions | written; seed with `../fixtures/foreign` (`basic.tar.zst`) |
| `foreign_sevenz` | D8 | 7z list/test/read path (sevenz-rust2): same assertions | written; seed with `../fixtures/foreign/7z` |
| `foreign_rar` | D8 | RAR 4/5 path (UnRAR, driven through a temporary file the harness writes) | written; seed with `../fixtures/foreign/rar` |
| `foreign_protocol` | D8 | the helper protocol's frame and message decoders (the parent treats the helper as untrusted): no panic, decode then encode then decode is stable | written; seed with `../fixtures/foreign` |
| checkpoint + delta replay | C6 | spec §10.6 (blocked on O26) | not yet |

Run (needs nightly and `cargo install cargo-fuzz`). Put a scratch corpus directory
first so new inputs do not land in `fixtures/golden/`; the golden vectors are
read as seeds:

```bash
cd fuzz
cargo +nightly fuzz run frame_walker corpus/frame_walker ../fixtures/golden/c1 -- -max_total_time=60
```

Rule (AGENTS.md): a parser change extends a fuzz target and adds a golden vector.
