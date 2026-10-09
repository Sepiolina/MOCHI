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
| R5 | Cryptographic suite and vectors | C11 | **draft for review** (D20, spec Annex B.2.10): `R5-crypto-draft.md`, vectors in `R5-vectors/` (independent oracles, reproducible), schemas `key-envelope-v0.cddl`, `commit-record-v2.cddl`, `recovery-manifest-v3.cddl`. Not implemented; frozen only after gate G10 and review |
| R6 | Erasure-coding suite and vectors | C12 | not started |
| R7 | Report schema and stable error codes | C7 | not started; drafts in `mochi-core` (`report.rs` v0, `error.rs`) |
| R8 | Golden valid/corrupt/interrupted archives | all C phases | not started; see `fixtures/golden/` |
| R9 | Independent reader/writer interop results | R1–R8 | not started |
| R10 | Segmented-location rules | none | post-1.0 |
