# Ratification artifacts

The spec (§28) requires these before the format is called interoperable. Each is
written *from* working code and golden vectors, then frozen. Where one exists it is
more precise than the spec and wins for its topic (AGENTS.md, "Sources of truth").

| ID | Artifact | Informed by | Status |
|---|---|---|---|
| R1 | Final frame registry and version mapping | C1 | not started; draft constants in `mochi-format/src/registry.rs` |
| R2 | Record-envelope layouts | C1–C4 | not started |
| R3 | Canonical commit and manifest serialization; domain separators | C2, C4, C5 | in progress: encoding decided (spec D2, deterministic CBOR; codec in `mochi-format/src/cbor.rs`); manifest schema drafted (`docs/schemas/recovery-manifest-v0.cddl`, vectors in `fixtures/golden/c4`); commit body schema arrives in C5; separator strings still draft (O20) |
| R4 | Complete SQLite DDL and constraints | C3 | not started; working DDL in `mochi-core/src/catalog/schema.rs` (five §10.1 tables deferred to their phases; plan C3 status) |
| R5 | Cryptographic suite and vectors | C11 | **frozen 2026-10-11** (D20, spec Annex B.2.10; gate G10 met; review in §12): `R5-crypto-draft.md` (name kept for links), vectors in `R5-vectors/` (independent oracles, reproducible), schemas `key-envelope-v0.cddl` (frozen), and the R5 keys of `commit-record-v2.cddl` (11, 12) and `recovery-manifest-v3.cddl` (13, provenance `reason`); the rest of those two schemas freezes with R3 |
| R6 | Erasure-coding suite and vectors | C12 | not started |
| R7 | Report schema and stable error codes | C7 | not started; drafts in `mochi-core` (`report.rs` v0, `error.rs`) |
| R8 | Golden valid/corrupt/interrupted archives | all C phases | not started; see `fixtures/golden/` |
| R9 | Independent reader/writer interop results | R1–R8 | not started |
| R10 | Segmented-location rules | none | post-1.0 |
