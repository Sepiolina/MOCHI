# MOCHI 1.0 Specification

**Status:** Proposed design specification; not yet a ratified interoperable wire format. Until the gates in Section 28 pass, implementations MUST describe support as experimental or draft-compatible.
**Specification revision:** 2.0 (the "MOCHI Design Document v2.0" refinement, integrated for the MOCHI 1.0 product release)
**Supersedes:** MOCHI design documentation v1.2, and the unified "v1.2 + extended capabilities" draft built on it.
**Product release targeted:** MOCHI 1.0 — `mochi-core` library, `mochi` CLI, and the MOCHI desktop application (Tauri v2).
**Primary extension:** `.mochi`
**Document filename:** `docs/spec.md`

> **How to read this document.** The base text is the v2.0 refinement. Its normative requirements are carried over unchanged. Material added while integrating it for the 1.0 release is marked **[1.0 integration]** and is one of three things: (a) a non-normative note, (b) a v1.2 mechanism carried forward and explicitly labelled *Proposed* (not ratified), or (c) an open decision cross-referenced to Annex B. Anything not so marked is refinement text. Where the refinement and v1.2 disagree, the refinement wins; Section 29.1 lists every such correction.

## 1. Purpose

MOCHI is an archival container and preservation architecture for retaining files, preserving historical snapshots, supporting searchable catalogs, and verifying that archived content remains recoverable.

MOCHI combines:
- Independently addressable compressed objects.
- Append-only commits and immutable historical content.
- SQLite-based metadata catalogs.
- Independently recoverable manifests.
- Cryptographic content and storage integrity checks.
- Optional encryption, deduplication, and erasure coding.
- Independent preservation inventories and recovery copies.
- Automated health verification and restoration testing.

The primary objective is not merely to store bytes. It is to provide evidence that retained files remain present, identifiable, readable, searchable, and recoverable.

### 1.1 Preservation limitation

No archive format can guarantee that files will never disappear.

Loss of every copy, destruction of all usable encryption keys, or damage beyond all available recovery mechanisms can make recovery impossible.

MOCHI therefore defines a measurable preservation contract:

> Within the configured failure model and retention policy, MOCHI must detect missing or damaged content, preserve complete recovery dependencies, and support verified restoration from independent recovery sources.

Checksums detect corruption. They do not replace backups.
Parity repairs bounded damage. It does not replace independent copies.
Successful writes do not prove successful restoration.
Successful searches do not prove index completeness.

### 1.2 Version identifiers **[1.0 integration]**

Section 26 requires these versions to be distinct. For MOCHI 1.0 they are:

| Identifier | Value | Governs |
|---|---|---|
| Product release | 1.0 | Semantic version of `mochi-core`, `mochi` CLI, and the desktop application |
| Specification revision | 2.0 | This document |
| Container wire generation | 2 (proposed) | Byte layout; footer magic `MOCHI2\0\0` (Section 8.4) |
| Metadata schema version | Assigned by the ratification package | SQLite DDL (Section 10) |
| Recovery-manifest encoding version | Assigned by the ratification package | Section 11 |
| Extension versions | Per extension | Annex A items, once ratified |

A product labelled "1.0" writing wire generation "2" is intentional under this scheme but is a user-facing naming risk. See Annex B, decision D1.

### 1.3 Product context (non-normative) **[1.0 integration]**

MOCHI 1.0 ships on the same model as WinRAR: a reusable format library plus an end-user application.

| Deliverable | Role under Section 4 |
|---|---|
| `mochi-core` (Rust library) | Container implementation; the only component that reads or writes `.mochi` bytes |
| `mochi` CLI | Container client and local verification tool; automation surface (Section 23) |
| MOCHI desktop application (Tauri v2) | Interactive container client and local verification tool (Section 23.3) |

None of these, by themselves, is a preservation service. They MUST NOT claim Preservation-profile conformance (Sections 4 and 7.6), and user-facing text MUST NOT describe a single `.mochi` file as a backup or as "preserved" (Section 1.1, Section 5.3).

## 2. Requirement Language and Scope

The terms **MUST**, **MUST NOT**, **SHOULD**, and **MAY** describe requirements for implementations claiming conformance to this design.

Requirements apply to the relevant component or advertised profile. For example, replication requirements apply to the preservation service, not to a standalone read-only format decoder.

This document defines:
- System responsibilities and safety invariants.
- Proposed container organization.
- Logical metadata and recovery semantics.
- Commit, retention, and recovery behavior.
- Search and health-verification contracts.
- Acceptance tests and release gates.

A separate ratification package MUST finalize exact serialization, complete SQL DDL, cryptographic profiles, and interoperability vectors before the wire format is frozen.

## 3. Goals and Non-Goals

### 3.1 Goals

| Goal | Required outcome |
|---|---|
| Durable retention | Retained snapshots remain recoverable under the configured failure model |
| Missing-data detection | Independent inventory detects missing archives, segments, and required objects |
| Integrity | Stored and reconstructed content can be checked against precisely defined hashes |
| Recoverability | Recovery does not depend solely on the latest footer, primary catalog, or original machine |
| File discovery | Retained files are discoverable by identity, path, and snapshot |
| Content search | Supported content is searchable, with visible indexing coverage and failures |
| Auditability | Verification, repair, retention, and deletion operations produce retained evidence |
| Automation | Health checks provide stable machine-readable results |
| Efficient access | Selected files can be read without decoding the entire archive |
| Historical access | Retained snapshots have deterministic namespace and content semantics |
| Safe evolution | Unsupported required features fail explicitly |

### 3.2 Non-goals

MOCHI does not promise:
- Recovery after loss of every complete recovery source.
- Recovery of encrypted content without usable keys.
- Unlimited repair from finite parity.
- Current-snapshot equivalence through ordinary TAR extraction for every archive profile.
- Full-text indexing of every possible file format.
- Constant-time restoration independent of file size.
- Protection against every privileged adversary without independently protected trust anchors and recovery copies.

## 4. System Architecture

MOCHI separates three layers.

| Layer | Responsibility |
|---|---|
| Container | Stores objects, metadata, manifests, commits, and footers |
| Preservation service | Maintains inventories, copies, retention, keys, and recovery procedures |
| Verification service | Measures integrity, completeness, freshness, search coverage, and recoverability |

A standalone archive reader MAY implement only container functionality.

A deployment MUST NOT claim preservation-profile conformance merely because it can read and write `.mochi` files.

### 4.1 Logical structure

```text
Preservation inventory
    -> archive identity
    -> expected preserved head
    -> retained snapshot roots
    -> independent recovery locations
    -> health and restoration evidence

Archive
    -> archive descriptor
    -> immutable data objects
    -> dictionaries and key envelopes
    -> metadata deltas and recovery manifests
    -> commits
    -> skippable commit footers
    -> optional indexes and parity
```

### 4.2 Terminology

| Term | Meaning |
|---|---|
| Archive ID | Stable logical identity of an archive |
| Object | Immutable stored unit with a defined representation and digest |
| File version | Immutable file content and preserved attributes |
| Snapshot | Complete logical namespace at a particular commit |
| Commit | Published transition from a parent snapshot |
| Head | Selected published commit for an archive or branch |
| Retained root | Snapshot or recovery root protected from collection |
| Dependency closure | Every object and prerequisite needed to recover a root |
| Recovery copy | A storage location containing a complete required recovery set |
| Checkpoint | Materialized catalog state that reduces metadata replay |
| Preservation inventory | Independently protected record of expected archives and retained content |

## 5. Safety Invariants

### 5.1 Published-commit invariant

A published commit MUST reference only complete objects available according to the backend's local durability contract.

A partially written commit or footer MUST NOT become a valid head.

### 5.2 Retained-root invariant

Every retained root MUST retain its complete dependency closure.

Dependencies include:
- File content objects.
- Metadata and recovery manifests.
- Compression dictionaries.
- Required key envelopes and usable key-recovery arrangements.
- Required segment-location records.
- Other objects necessary to interpret and restore the snapshot.

### 5.3 Preservation-state invariant

A locally committed snapshot MUST NOT be reported as preserved until its configured recovery-copy and retention requirements are satisfied.

### 5.4 Verification invariant

A required check that is incomplete, unsupported, skipped, or overdue MUST NOT be reported as passing.

### 5.5 Repair invariant

Repair MUST NOT destroy the last known-good recovery source.

### 5.6 Search invariant

Incomplete search coverage MUST be visible to callers.

### 5.7 Freshness invariant

Internal validity alone MUST NOT establish that an archive contains the expected latest preserved commit.

### 5.8 Deletion invariant

Physical deletion MUST be justified against current retained roots, legal holds, replication pins, and active recovery operations.

## 6. Threat and Failure Model

The preservation policy MUST state which failures it is designed to tolerate.

| Failure | Required mechanism |
|---|---|
| Interrupted append | Durable publication protocol and previous-head recovery |
| Torn or reordered writes | Backend-specific synchronization and commit validation |
| Bit corruption | Stored-object and reconstructed-content hashes |
| Missing object | Independent inventory and dependency validation |
| Primary archive deletion | Independent recovery copy |
| Site loss | Geographically separate recovery source |
| Credential compromise | Independently controlled immutable or offline recovery source |
| Catalog damage | Recovery manifests and metadata replicas |
| Dictionary loss | Dependency tracking and protected dictionary copies |
| Key-service outage | Documented recovery access appropriate to policy |
| Key loss | Independent key-recovery arrangements |
| Stale archive replacement | Expected-head comparison |
| Search-index loss | Rebuild from authoritative retained data |
| Faulty garbage collection | Reachability, fencing, quarantine, and deletion audit |
| Unsupported format feature | Explicit unsupported result |
| Damage beyond parity capacity | Accurate unrecoverable status and alternate-copy recovery |

Unkeyed hashes provide corruption detection, not proof against an attacker who can replace both content and hashes.

Protection against malicious replacement requires separately trusted inventory records, authenticated manifests, signatures, or another documented trust mechanism.

## 7. Archive Profiles

### 7.1 Core profile

The Core profile supports:
- A single logical writer at publication time.
- Independently decodable compressed objects.
- Immutable file versions.
- Deterministic namespace operations.
- SQLite metadata.
- Recovery manifests.
- Commit validation.
- Read-only verification.
- File restoration and catalog rebuilding.

### 7.2 TAR-stream compatibility profile

This restricted profile supports extraction of complete TAR members through documented Zstandard and TAR tooling.

It MUST:
- Emit decoded bytes forming the documented sequence of complete TAR streams.
- Avoid reference-only representations that generic TAR tools cannot reconstruct.
- Avoid MOCHI-specific fragmentation unless the generic extraction path produces complete files without MOCHI interpretation.
- Avoid external dictionaries unless the documented decoder invocation supplies them.
- Clearly distinguish historical stream extraction from latest-snapshot reconstruction.

Historical entries and deletions may prevent generic extraction from representing the latest logical snapshot.

The compatibility claim MUST identify supported tools and tested versions. It MUST NOT be described as universal POSIX compatibility.

**[Amendment, Annex B.2.9 D19: decided; implementation and evidence pending (B.2.6)]** The profile is realized by one pax TAR stream per commit with at least one put, whose non-content bytes are stream-only data chunks that no extent references. It changes no wire structure.

### 7.3 Encrypted profile

Adds authenticated encryption and key-envelope handling.

Generic Zstandard decoding does not constitute decryption or meaningful file extraction.

### 7.4 Redundancy profile

Adds erasure coding with a fully specified coding suite and shard-placement policy.

### 7.5 Segmented profile

Stores immutable segments independently while preserving stable object identities and verified location mappings.

### 7.6 Preservation profile

Adds independent inventory, recovery copies, retention enforcement, scheduled verification, and independent restoration exercises.

### 7.7 Profile targets for MOCHI 1.0 **[1.0 integration]**

| Profile | 1.0 status | Rationale |
|---|---|---|
| Core (7.1) | **Required** | Everything else depends on it |
| TAR-stream compatibility (7.2) | **Target**, writer-selectable | Preserves v1.2's zero-tool extraction goal, now as a restricted, tested profile. Default on/off is Annex B, D4 |
| Encrypted (7.3) | **Target** | Password-protected archives are a baseline expectation for a WinRAR-class product. Blocked on the cryptographic ratification item (Section 28) |
| Redundancy (7.4) | **Target** | Equivalent of WinRAR's "recovery record." Blocked on the erasure-coding ratification item |
| Segmented (7.5) | **Post-1.0 (1.x)** | Includes split volumes. WinRAR users commonly expect multi-volume archives; see Annex B, D5 |
| Preservation (7.6) | **Not in 1.0** | Requires an independently protected inventory and recovery copies, which a desktop archiver does not provide |
| Concurrent preparation (12.5) | **Post-1.0** | Core serializes publication; the 1.0 writer is single-writer |

A 1.0 reader encountering a required feature from a profile it does not implement MUST return an explicit `UNSUPPORTED` result (Sections 20.4, 26), never partial or empty content.

## 8. Container Organization

### 8.1 Framing

The container uses Zstandard data frames and Zstandard skippable frames.

Every committed footer MUST itself be contained in a skippable frame. Historical footers MUST remain skippable after later appends.

Bare MOCHI trailer bytes MUST NOT interrupt the Zstandard frame sequence.

A conceptual session is:

```text
[data objects]
[dictionaries or key envelopes, when required]
[recovery manifest]
[metadata delta or checkpoint]
[commit record]
[footer skippable-frame header]
[footer payload]
```

Physical ordering MAY vary where bootstrap dependencies and the backend publication protocol permit it.

### 8.2 Proposed frame registry

The following values are proposed for v2.0. They are not a declaration that the v1.2 draft values were already standardized.

| Frame magic | Proposed use |
|---|---|
| `0xFD2FB528` | Standard Zstandard data frame |
| `0x184D2A50` | Metadata delta or checkpoint |
| `0x184D2A51` | Commit record |
| `0x184D2A52` | Prepared-transaction manifest |
| `0x184D2A53` | Reserved; no Core-profile interpretation |
| `0x184D2A54` | Signature or authenticated attestation extension |
| `0x184D2A55` | Search-index delta or checkpoint |
| `0x184D2A56` | Commit footer |
| `0x184D2A57` | Archive descriptor |
| `0x184D2A58` | Recovery manifest |
| `0x184D2A59` | Encrypted object envelope |
| `0x184D2A5A`–`0x184D2A5B` | Reserved |
| `0x184D2A5C` | Key envelope |
| `0x184D2A5D` | Compression dictionary |
| `0x184D2A5E` | Optional footer-history accelerator |
| `0x184D2A5F` | Parity object |

Frame IDs alone do not establish that an arbitrary Zstandard stream is a MOCHI archive. Readers MUST validate the MOCHI descriptor and versioned record envelopes.

**[1.0 integration]** *Relationship to earlier drafts.* This registry replaces the placeholder allocations in the unified-draft Appendix B. Notable changes: `0x184D2A56` is now the commit footer (v1.2 had no framed footer); `0x184D2A57`/`58`/`59` are new (descriptor, recovery manifest, encrypted object envelope); `0x184D2A53` (previously proposed for range-reservation records) is reserved with no Core interpretation, because shared-file range reservation is no longer part of Core (Section 12.5); `0x184D2A5E` is now an optional *accelerator* rather than a recovery authority (Section 22). The registry is still proposed until the ratification package publishes it (Section 28).

### 8.3 Record envelopes

Each MOCHI control-record type MUST have a ratified envelope defining:
- Record type and schema version.
- Required and optional features.
- Archive identity.
- Payload encoding.
- Payload length.
- Integrity-protection scope.
- Relevant object or transaction identity.

Skippable-frame payloads are limited by their 32-bit length field. Larger logical records MUST be split using a specified representation or rejected.

**[Amendment, Annex B.2 D11: decided; implementation and evidence pending (B.2.6)]** An envelope MAY be realized in either of two encodings:
- **Deterministic-CBOR records** (Annex B.1 D2): the envelope is a set of required keys in the record body.
- **Other payloads:** a binary envelope header precedes the payload in the same frame.

Both encodings MUST carry every field listed above, and MUST impose identical validation obligations (Annex B.2 D11). Neither encoding may omit or weaken an obligation. The binary layout is given in Annex B.2.2. For recovery manifests, including snapshot manifests, and for catalog images, 1.0 takes the "rejected" branch of the preceding paragraph (Annex B.2 D10, item 11).

### 8.4 Proposed footer

The footer is a skippable frame with an 8-byte Zstandard skippable header and a 64-byte MOCHI payload.

The proposed payload is:

| Payload offset | Size | Field |
|---|---|---|
| 0 | 8 bytes | Magic: ASCII `MOCHI2` followed by two zero bytes |
| 8 | 8 bytes | Commit-frame offset |
| 16 | 8 bytes | Commit-frame stored length |
| 24 | 8 bytes | Commit sequence |
| 32 | 32 bytes | Footer digest |

All integer fields are unsigned little-endian.

For a monolithic archive:
- The offset is measured from byte zero of the archive.
- The offset points to the commit frame's magic.
- The stored length includes the commit frame header and payload.

The footer digest is BLAKE3-256 over the concatenation of:
1. ASCII `MOCHI2-FOOTER` followed by one zero byte.
2. Footer payload bytes 0 through 31.
3. The exact stored commit-frame bytes identified by the offset and length.

A clean committed EOF permits reading the final 64-byte payload, but readers MUST also validate its preceding skippable header.

This is a proposed incompatible footer layout. Existing archives MUST NOT be silently relabeled.

**[1.0 integration]** *Why the footer moved into a skippable frame.* The v1.2 layout ended the file with 64 bare trailer bytes, and after each append the previous trailer remained mid-file. Neither is a valid Zstandard frame, so `zstd -dc` would stop with a header error at the first trailer — the v1.2 zero-tool fallback could not have worked as written. Framing every footer fixes this. The footer digest is also now computed over *stored* commit-frame bytes, so it can be checked without decryption keys, unlike v1.2's hash over decrypted metadata.

**[1.0 integration]** *Open decision:* whether any v1.2-layout archives exist that must be recognized as legacy (Section 26). If none were ever produced outside development, the `MOCHI2` magic could be reconsidered; see Annex B, D1. Until decided, implementations use `MOCHI2` as specified above.

### 8.5 Parsing requirements

Readers MUST:
- Check bounds and integer overflow before seeking or allocating.
- Apply configurable memory, object-size, and decompression limits.
- Validate record schemas and required features.
- Treat magic-number matches as candidates, not proof.
- Validate actual Zstandard frame structure when determining frame boundaries.

`Frame_Content_Size` describes decoded content size. It MUST NOT be used as the compressed frame's physical length.

Zstandard's optional checksum is 32 bits derived from the low four bytes of XXH64 over decoded content. It is supplementary to MOCHI's cryptographic hashes.

Reference: [rfc-editor](https://www.rfc-editor.org/rfc/rfc8878.xml).

### 8.6 Determining physical frame length (implementation note) **[1.0 integration]**

Because `Frame_Content_Size` is a decoded size (Section 8.5), a reader determines a Zstandard frame's stored length structurally, per RFC 8878:

- **Skippable frame:** 4-byte magic, 4-byte little-endian `Frame_Size`, then exactly `Frame_Size` payload bytes. Total = 8 + `Frame_Size`.
- **Data frame:** 4-byte magic, then a frame header whose length follows from the `Frame_Header_Descriptor` flags, then one or more blocks, then a 4-byte checksum only if `Content_Checksum_flag` is set.
- **Each block** begins with a 3-byte header: `Last_Block` (1 bit), `Block_Type` (2 bits), `Block_Size` (21 bits). Raw and Compressed blocks occupy `Block_Size` bytes after the header. **An RLE block occupies exactly one byte** after the header; its `Block_Size` is the regenerated size. `Block_Type` 3 is reserved and invalid.
- The walk ends after the block with `Last_Block = 1` (plus the checksum, if flagged).

Every step MUST be bounds-checked against the available bytes and the limits in Section 8.5. A structurally valid walk makes a location a *candidate* frame; it does not make it a MOCHI object until the record envelope validates (Section 8.3).

## 9. Object Representations and Integrity

### 9.1 Representation pipeline

```text
logical content
    -> compression, if enabled
encoded plaintext
    -> authenticated encryption, if enabled
stored payload
    -> record framing
stored object bytes
```

These representations MUST NOT be described interchangeably.

### 9.2 Required digest scopes

| Digest | Input |
|---|---|
| File content hash | Complete logical file byte stream, excluding path and attributes |
| Chunk content hash | Exact decoded chunk bytes |
| Stored-object hash | Exact stored object bytes, including the framing specified by its object record |
| Dictionary hash | Exact dictionary bytes |
| Metadata-object hash | Exact stored metadata-object bytes |
| Recovery-manifest hash | Exact stored manifest-object bytes |
| Commit ID | Domain-separated canonical commit body, excluding the ID field itself |
| Footer digest | Exact construction in Section 8.4 |

The baseline digest algorithm is BLAKE3-256.

**[1.0 integration]** *Correction carried from review.* Earlier drafts described the chunk checksum in ways that were easy to invert, and the unified v1.2 draft's encryption note (its §9.1) stated it wrongly as a hash of compressed-but-unencrypted bytes. Under this table the rule is unambiguous: the **chunk content hash** covers exact *decoded* bytes (after decryption and decompression); the **stored-object hash** covers exact *stored* bytes (what is on disk, ciphertext if encrypted). Neither covers the intermediate compressed-plaintext representation. Implementations SHOULD make these representations distinct types (see `AGENTS.md`) so that hashing the wrong one does not compile.

Canonical commit serialization and the exact domain separators for commit IDs MUST be frozen in the ratification package.

### 9.3 Additional validation

A matching hash does not replace:
- Length validation.
- Schema validation.
- Referential-integrity checks.
- Snapshot reconstruction.
- Encryption authentication.
- Freshness checks.

Sparse-file content hashing MUST use the logical byte stream, including zero bytes represented by holes.

### 9.4 Deduplication

Deduplication MUST NOT change file identity, version history, or logical content.

Deduplication candidates MUST be validated using cryptographic content digests and lengths.

A content index is a rebuildable accelerator, not an authoritative source of reachability.

Cross-archive or cross-tenant deduplication SHOULD be disabled by default because it can couple retention, confidentiality, and recovery domains.

### 9.5 Write-path deduplication consistency (Proposed, carried from v1.2) **[1.0 integration]**

The following v1.2 rule is compatible with this specification and is proposed for the ratification package:

1. Deduplication lookups are performed only against the published head the writer validated when it acquired the publication lock (Section 12.2, step 2) — never against uncommitted state from the same transaction.
2. Two identical files within one uncommitted transaction are each stored normally, unless the writer buffers the whole transaction before publication. A writer that does so MUST document it, and MUST NOT publish a reference to an object that is not itself part of the same published commit or an earlier one (Section 5.1).
3. On encrypted archives, deduplication reveals content equality (Section 14.3). This MUST be disclosed to users when they enable it.

## 10. Metadata and Snapshot Model

### 10.1 Immutable records and mutable namespace

MOCHI separates immutable content records from ordered namespace operations.

| Logical table | Purpose |
|---|---|
| `archive_meta` | Archive identity, versions, and declared features |
| `commits` | Parent relationships and published transitions |
| `objects` | Object identities, stored hashes, sizes, and locations |
| `chunks` | Decoded-content properties and encoding dependencies |
| `file_versions` | Immutable file content and preserved attributes |
| `file_extents` | Ordered mappings from files into chunks |
| `namespace_ops` | Ordered path creation, replacement, and deletion |
| `dictionaries` | Dictionary identities and references |
| `key_envelopes` | Key access and recovery metadata |
| `parity_groups` | Coding-suite and shard membership |
| `retained_roots` | Snapshot retention and hold references |
| `search_documents` | Search extraction and coverage state |

Exact DDL is a release-gated companion artifact.

### 10.2 Namespace operations

The Core profile defines:
- `PUT(path, file_version_id)`: Create or replace the entry at a path.
- `DELETE(path)`: Remove the entry at a path.

Operations are applied in parent-chain order and then by operation sequence within each commit.

A rename is represented by a deletion and insertion within the same atomic commit.

Directory deletion is non-recursive at the logical operation level. Recursive deletion MUST explicitly enumerate the affected entries or use a separately specified deterministic operation.

Parent-directory validity and path conflicts are checked against the completed commit state.

Conflicting immutable IDs with different record content are corruption, not updates.

### 10.3 File extents

Each extent records:
- File-version ID.
- Extent ordinal.
- Logical offset.
- Length.
- Referenced chunk ID, or an explicit hole representation.
- Offset within the decoded chunk.

Extent validation MUST reject unintended gaps, overlaps, out-of-range reads, and length mismatches.

### 10.4 Path semantics

Authoritative paths MUST use a reversible representation preserving the source name components.

The logical model MUST distinguish path separators from bytes within names.

Display normalization and search normalization MUST NOT alter authoritative path identity.

Restoration to a filesystem with incompatible naming or case rules MUST report collisions and unsupported names. It MUST NOT silently overwrite distinct entries.

#### 10.4.1 Preserved attributes (Proposed, open) **[1.0 integration]**

The Core profile must define which attributes are "promised" (Sections 11, 24.3). Proposed starting point, carried from v1.2:

- POSIX mode, uid, gid, mtime (seconds plus optional nanoseconds) when authored on POSIX.
- Windows `FILE_ATTRIBUTE_*` bits when authored on Windows, plus POSIX-plausible defaults (`0o644` files, `0o755` directories) rather than zeros, so POSIX extraction yields usable permissions.
- The authoritative convention MUST be recoverable from the record.

Extended attributes, ACLs, alternate data streams, and ownership by name are **not** promised in 1.0 unless the ratification package adds them. Restoration MUST report unrestorable attributes as exceptions (Section 24.3). See Annex B, D6.

### 10.5 SQLite usage

Metadata catalogs use SQLite 3 with a proposed page size of 4096 bytes.

Published database images MUST be self-contained and MUST NOT depend on external WAL or journal files.

Private working databases MAY use transactional journaling. Publishing a standalone image is a separate step.

The earlier `journal_mode = OFF` setting MUST NOT be interpreted as a safe general-purpose authoring protocol.

Verification MUST include:
- SQLite structural integrity checks.
- Foreign-key or equivalent relationship validation.
- MOCHI-specific dependency and snapshot checks.

### 10.6 Replay and checkpoints

Readers reconstruct a snapshot from a verified checkpoint plus subsequent ordered deltas.

A checkpoint MUST declare the commit state it materializes.

Creating a checkpoint does not authorize deletion of metadata needed by retained historical snapshots.

## 11. Recovery Manifests

Recovery MUST NOT depend exclusively on SQLite metadata.

Each commit MUST provide a recovery manifest sufficient to reproduce its logical transition using its declared parent recovery state.

A retained snapshot MUST have either:
- A complete recovery-manifest chain from an available baseline; or
- A complete snapshot manifest plus all later required deltas.

Recovery manifests MUST preserve:
- File identities, paths, types, and versions.
- File lengths and content hashes.
- Ordered extents.
- Chunk identities, lengths, and stored hashes.
- Dictionary and encryption dependencies.
- Snapshot membership and namespace operations.
- Required preserved attributes.
- Segment and object-location information, or a verified way to rediscover locations.

The manifest encoding MUST be versioned and independently parseable without SQLite.

The encoding and canonicalization rules MUST be ratified before format freeze.

### 11.1 Recovery guarantees

MOCHI MUST distinguish:
- **Payload salvage:** Recovering some content bytes.
- **File recovery:** Recovering complete files with verified content.
- **Snapshot recovery:** Recovering the namespace, versions, and promised attributes.
- **Historical recovery:** Recovering specified retained snapshots.

A scanner MUST NOT claim snapshot recovery merely because it found valid chunk frames.

## 12. Commit and Publication Protocol

### 12.1 Commit contents

A commit records:
- Archive ID.
- Commit sequence and transaction UUID.
- Parent commit ID.
- Parent location hint, where useful.
- Metadata delta or checkpoint reference.
- Recovery-manifest reference.
- Required dependency references.
- Required feature set.
- Writer-fencing information when supported.
- Canonical commit ID.

Time fields are informational. Commit order MUST NOT depend solely on wall-clock timestamps.

### 12.2 Single-file publication

A local writer MUST:
1. Acquire the archive's exclusive publication lock.
2. Validate the current head and resolve any interrupted tail.
3. Write new content and dependencies.
4. Write recovery and metadata records.
5. Write the commit record.
6. Persist the referenced bytes using the filesystem's documented durability mechanism.
7. Append the footer.
8. Persist the footer.
9. Persist directory-entry changes when creating or replacing files requires it.
10. Report local commit completion.

The implementation MUST document its filesystem and storage assumptions.

A previous valid footer may lie before an incomplete tail. Readers MUST NOT assume the only recoverable commit is at physical EOF.

Truncating an incomplete tail requires exclusive access, validation that the tail is uncommitted, and an auditable recovery action.

**[Amendment, Annex B.2 D14: decided; implementation and evidence pending (B.2.6)]** No check can prove that a tail is uncommitted: a single corruption event can erase every marker of a later commit. The validation required above is therefore the eligibility rule of Annex B.2 D14. That rule establishes only that no recognisable evidence of a commit exists, not that none was ever written. Truncation MUST therefore be explicit, and by default MUST be preceded by a verified, persisted quarantine copy of the removed bytes. A waiver MUST be explicit and MUST be recorded in the report.

### 12.3 Object-store publication

Object-store backends MUST:
1. Upload immutable objects under unique keys.
2. Confirm completed uploads using documented backend semantics.
3. Publish immutable metadata and commit objects.
4. Update the head using a conditional operation against the expected predecessor.
5. Record the resulting storage version or equivalent publication identity.

Object-store ETags MUST NOT be assumed to be content hashes.

### 12.4 Preservation acknowledgement

Commit status is separated into:
- `LOCAL_COMMITTED`
- `REPLICATION_PENDING`
- `PRESERVED`
- `PRESERVATION_DEGRADED`

`PRESERVED` requires the configured complete recovery copies, retention protections, and independently recorded expected root.

If an operation returns before preservation completes, it MUST expose that state.

### 12.5 Concurrent writers

The Core profile serializes publication.

An optional concurrent-preparation profile MAY prepare data in private staging areas.

Publication MUST use locking, conditional updates, or equivalent serialization. `O_APPEND` alone is insufficient.

The default conflict policy is `fail-on-conflict`.

Fencing is effective only when the publication backend rejects stale tokens. Merely recording a token in a commit does not provide fencing.

Shared-file concurrent range reservation is not part of the Core profile.

## 13. Compression and Chunking

Chunks are independently addressable and independently decodable when their declared dictionaries are available.

Chunking parameters MUST be recorded or otherwise unambiguously associated with the producing profile.

Implementations SHOULD support workload-specific chunk sizes and MAY support content-defined chunking.

No specific chunk-size default is ratified by this document.

**[1.0 integration]** *Product defaults are not format defaults.* The 1.0 applications still need presets. They MAY ship v1.2's presets — roughly 8–16 MiB chunks for interactive archives and 64–128 MiB for cold storage — as *implementation* defaults, provided the chosen parameters are recorded per archive as required above. These values carry no format-level guarantee, and the v1.2 dictionary-ratio claim (200–500%) MUST NOT appear in user-facing text unless reproduced under Section 27 benchmark rules.

Trade-offs MUST be documented:
- Smaller chunks improve repair granularity and selected-read efficiency.
- Larger chunks may improve compression and reduce metadata overhead.
- Dictionary sharing introduces a shared recovery dependency.
- Deduplication can increase the number of files affected by one missing object.

Dictionaries MUST be immutable and retained while any retained object depends on them.

## 14. Encryption and Key Recovery

### 14.1 Encryption requirements

Encrypted objects use compress-then-encrypt processing.

The cryptographic profile MUST define:
- AEAD algorithm and exact parameters.
- Key derivation and envelope formats.
- Nonce construction and uniqueness requirements.
- Associated-data serialization.
- Key identifiers and key lifecycle.
- Ciphertext framing.
- Error behavior and test vectors.

AES-256-GCM and ChaCha20-Poly1305 remain candidate suites. They MUST NOT be treated as interchangeable implementations of an unspecified profile.

Ciphertext MUST NOT be mislabeled as an ordinary compressed-content frame. This design proposes a dedicated skippable encrypted-object envelope.

**[1.0 integration]** *Change from v1.2.* v1.2 placed ciphertext inside `0xFD2FB528` data frames and claimed generic tools could "skip cleanly." They cannot: a Zstandard decoder would attempt to decode the ciphertext and fail. Encrypted objects now use the `0x184D2A59` envelope (Section 8.2), which generic decoders do skip. Candidate primitives carried from v1.2, pending ratification: AES-256-GCM or ChaCha20-Poly1305 (one suite per profile identifier, not interchangeable), Argon2id for passphrase derivation, HKDF-SHA256 for key-file or public-key material.

### 14.2 Nonce safety

A nonce MUST NOT repeat under the same AEAD key.

Generation MUST remain safe across:
- Retries.
- Interrupted writes.
- Restored backups.
- Concurrent writers.
- Re-encryption operations.

A nonce derived only from a resettable counter is insufficient unless the key and epoch rules prove uniqueness.

### 14.3 Bootstrap and confidentiality

Key-envelope discovery MUST NOT depend on first decrypting the only metadata that locates those envelopes.

The profile MUST document which information remains visible, including archive identity, object sizes, key identifiers, and possible content-equality leakage.

Encrypted names and manifests remain unavailable without usable keys.

**[1.0 integration]** *Change from v1.2.* v1.2 recorded key-envelope locations in a `key_envelopes` table inside the encrypted metadata — the circular dependency this section forbids. Envelope discovery must use unencrypted, integrity-protected records (for example, the archive descriptor or commit record). v1.2's multi-recipient model (one data key wrapped separately per recipient) and rotation model (new sessions MAY use a new data key; older objects keep their original envelope reference) remain compatible and are proposed for the ratification package.

### 14.4 Key preservation

Preservation deployments MUST:
- Maintain independent key-recovery arrangements.
- Preserve required historical envelopes.
- Test restoration through recovery credentials.
- Prevent key retirement while retained snapshots depend on it.
- Distinguish temporary key-service failure from permanent key loss.

Rewrapping an existing data key and re-encrypting content are different operations and MUST have separate audit records.

## 15. Redundancy and Independent Copies

### 15.1 Erasure coding

The optional redundancy profile uses a ratified Reed–Solomon suite.

The suite MUST define:
- Field arithmetic and coding matrix.
- Data-shard count `k`.
- Parity-shard count `m`.
- Shard ordering, sizes, and padding.
- Shard integrity checks.
- Protected representation: stored bytes or decoded bytes.
- Reconstruction and validation procedures.

For a conventional `(k + m, k)` erasure-coded group, recovery requires at least `k` valid shards. Corrupt shards MUST be identified and treated according to the coding suite.

The design MUST NOT promise repair beyond the suite's tested capacity.

**[1.0 integration]** *Carried from v1.2 (Proposed).* A parity object SHOULD self-describe its coding suite, `k`, `m`, and the object range it protects, and readers MUST cross-check that description against the catalog's `parity_groups` record — so either can rebuild the other if one is lost. v1.2's rule that parity must not span a compaction boundary becomes simpler here: compaction always writes a new representation (Section 18.2), so parity for it is regenerated against the new layout, and the old representation's parity stays valid for the old representation during its safety period. Cauchy Reed–Solomon remains the candidate suite; v1.2's `m/k` guidance of about 10–20% is an operational suggestion, not a format guarantee.

### 15.2 Protection coverage

Parity policy MUST explicitly state coverage for:
- Content objects.
- Metadata.
- Recovery manifests.
- Dictionaries.
- Commit records.
- Key envelopes.

Parity MUST NOT be the sole protection for its own only surviving configuration metadata.

### 15.3 Suggested preservation policy

A recommended starting policy is:
- Three complete recovery copies.
- Independent storage failure domains.
- At least one geographically separate copy.
- At least one immutable or offline copy inaccessible to ordinary writer credentials.
- Independent inventory and key-recovery protection.

This is an operational recommendation, not an intrinsic property of the file format.

## 16. Preservation Inventory and Retention

The preservation service MUST maintain an independently protected inventory.

### 16.1 Inventory contents

The inventory records:
- Archive identity.
- Expected preserved head.
- Retained snapshot roots.
- Required objects or authenticated manifests enumerating them.
- Expected stored sizes and hashes.
- Replica locations and storage versions.
- Retention expiration and legal holds.
- Recovery-key references, excluding plaintext secret keys.
- Verification and restoration evidence.
- Authorized deletion history.

### 16.2 Reconciliation

Reconciliation runs in both directions:
- Inventory to storage: detect missing, stale, or inaccessible expected objects.
- Storage to inventory: detect untracked objects and incomplete uploads.

Untracked objects MUST NOT be automatically deleted solely because they are absent from the inventory.

The inventory itself MUST be backed up and restoration-tested.

### 16.3 Retention semantics

The system distinguishes:
- Logical deletion from the current namespace.
- Expiration of a retained snapshot.
- Garbage collection of unreachable objects.
- Physical deletion from recovery copies.
- Encryption-key destruction.

A normal file deletion MUST NOT imply removal from retained historical snapshots.

Legal holds override ordinary expiration and collection.

Retention reductions and destructive actions SHOULD require elevated authorization and a delay or approval appropriate to the deployment.

## 17. Segmented and Remote Storage

Segmented archives use immutable segment identities and verified location mappings.

Logical object identity MUST NOT depend on the current physical segment offset.

Segment manifests record:
- Segment identity and stored digest.
- Expected length.
- Object ranges.
- Required segment dependencies.
- Storage locations and versions.

Range reads MUST be checked against expected object bounds and integrity metadata.

Split-volume ordering MUST be established by verified manifests, not filenames alone.

Joining or relocating segments MUST preserve logical content identity or provide a verified mapping.

A segment copy is not a complete recovery copy unless the retained root's full dependency closure is present.

## 18. Checkpointing, Compaction, and Garbage Collection

### 18.1 Checkpointing

Checkpointing reduces replay work without changing snapshot semantics.

Checkpoint creation MUST be verified against the source snapshot before adoption.

### 18.2 Compaction

Compaction writes a new archive or immutable segment set.

It MUST NOT destructively rewrite the only known-good retained representation.

Before switching:
1. Validate the compacted structure.
2. Verify required retained roots and dependency closure.
3. Verify selected or complete reconstructed content according to policy.
4. Establish required recovery copies.
5. Publish the replacement atomically.
6. Retain the previous representation for the configured safety period.

Physical-layout changes MUST NOT silently change logical snapshot content.

### 18.3 Garbage collection

Collection marks from:
- Current heads.
- Retained snapshots.
- Legal holds.
- Recovery roots.
- Replication pins.
- Active restoration operations.
- Unexpired prepared transactions where required.

Marking MUST include transitive dependencies.

Collection MUST account for concurrent publication through locking, epochs, pins, or another specified protocol.

Objects selected for deletion SHOULD enter quarantine before final removal.

Every deletion batch MUST produce an auditable explanation of why its objects are no longer required.

## 19. Searchability

### 19.1 Essential file discovery

Every retained snapshot MUST support discovery by:
- Path.
- File identity.
- File version.
- Snapshot or commit.

This capability MUST be rebuildable from authoritative metadata or recovery manifests.

### 19.2 Full-text search

Full-text search is optional and MAY use SQLite FTS5.

The index is a derived cache unless extracted text is separately designated as preserved authoritative content.

Each indexed document records:
- File-version ID.
- Source-content hash.
- Extractor and tokenizer versions.
- Extraction status.
- Indexing status.
- Error or exclusion reason.

### 19.3 Coverage reporting

Search responses MUST expose:
- Requested snapshot.
- Indexed commit or coverage watermark.
- Complete or partial coverage.
- Pending, failed, unsupported, and excluded document counts.

A zero-result response with incomplete coverage MUST NOT be presented as proof that no matching file exists.

### 19.4 Rebuilding

Indexes MUST be rebuildable without the original source filesystem.

Rebuilding may use:
- Retained archived content; or
- Retained extracted text with documented provenance.

A contentless FTS index does not remove the need to preserve its rebuild source.

### 19.5 Security

Content extraction MUST run under resource limits and SHOULD be sandboxed.

Search indexes containing sensitive plaintext require protection consistent with the archive's confidentiality policy.

Authorization filtering MUST apply to search results and snippets.

## 20. Health Verification

### 20.1 Verification levels

| Level | Checks | Evidence |
|---|---|---|
| Inventory | Expected archives, objects, copies, and versions | Missing and stale objects |
| Structural | Descriptor, frames, footer, commits, and SQLite structure | Parse and structural failures |
| Referential | File extents and complete dependency closure | Missing prerequisites |
| Stored integrity | Read and hash stored objects | Verified byte coverage |
| Content integrity | Decrypt, decompress, validate lengths and chunk hashes | Decoded-object validity |
| Restoration | Reassemble files and compare complete file hashes | Restored-file evidence |
| Search | Catalog/index reconciliation and known-answer queries | Coverage and correctness |
| Disaster recovery | Restore using independent storage and recovery credentials | Recovery time and recovered roots |

### 20.2 Non-mutating default

`verify` and `fsck` MUST be read-only by default.

Repair requires a separate explicitly authorized operation.

A read-only verification tool MUST NOT silently rewrite metadata, normalize damaged records, or change the archive head.

### 20.3 Health dimensions

Health reports show separate dimensions:
- Durability.
- Integrity.
- Recoverability.
- Searchability.
- Freshness.
- Retention compliance.
- Key availability.

An overall status MUST NOT conceal a failing dimension.

### 20.4 Status values

| Status | Meaning |
|---|---|
| `PASS` | Required checks completed successfully within policy |
| `FAIL` | A required check found a violation |
| `DEGRADED` | Some capability remains available, but a required protection is reduced |
| `UNKNOWN` | Evidence is insufficient or unavailable |
| `OVERDUE` | Required evidence is older than policy permits |
| `UNSUPPORTED` | The implementation cannot perform the required interpretation or check |

A successful stored-byte scrub MAY coexist with unknown decryptability or overdue restoration evidence.

### 20.5 Report contents

Machine-readable reports MUST include:
- Report schema version.
- Archive and checked commit identities.
- Expected preserved head.
- Tool and configuration versions.
- Check start and completion times.
- Scope and verification level.
- Objects and bytes checked.
- Objects and bytes expected.
- Skipped items and reasons.
- Coverage and evidence age.
- Structured findings with stable error codes.
- Repair actions, if separately performed.
- Result per health dimension.

Reports SHOULD be stored outside the archive being checked.

### 20.6 Example report

The following is illustrative, not the final JSON schema:

```json
{
  "schema_version": 1,
  "archive_id": "example-archive",
  "checked_commit": "example-commit",
  "overall_status": "DEGRADED",
  "dimensions": {
    "durability": "DEGRADED",
    "integrity": "PASS",
    "recoverability": "OVERDUE",
    "searchability": "PASS",
    "freshness": "PASS"
  },
  "coverage": {
    "expected_objects": 10000,
    "checked_objects": 10000,
    "stored_bytes_complete": true,
    "full_restore_complete": false
  },
  "findings": [
    {
      "code": "REPLICA_BELOW_POLICY",
      "severity": "error",
      "expected": 3,
      "observed": 2
    },
    {
      "code": "RESTORE_EVIDENCE_EXPIRED",
      "severity": "warning"
    }
  ]
}
```

## 21. Scheduled Maintenance and Review

Cadence is configurable according to archive size, risk, and recovery objectives.

| Trigger or interval | Suggested action |
|---|---|
| Every commit | Validate dependencies, durable publication, and preservation state |
| Daily | Reconcile inventory, replica lag, retention, keys, and overdue jobs |
| Continuous rotation | Scrub stored bytes against a full-coverage deadline |
| Weekly | Restore risk-weighted samples across file types and snapshots |
| Monthly | Rebuild search in a clean workspace and run known-answer tests |
| Quarterly | Exercise independent disaster recovery across a rotating scope |
| After repair or migration | Verify repaired content and affected retained roots |
| Before destructive maintenance | Verify recovery sources, holds, and retention dependencies |

Sampling MUST report its scope. It MUST NOT be represented as full-archive verification.

Operators MUST be able to review:
- Current health by dimension.
- Unresolved findings.
- Oldest unverified content.
- Replica and preservation lag.
- Last independent restoration.
- Search coverage and extraction failures.
- Planned retention expiration and deletion.
- Repair history and recurring failures.

## 22. Recovery and Repair Workflow

Recovery proceeds from least invasive to most invasive sources:

1. Validate the expected head and clean footer.
2. Locate and validate previous committed footers if the tail is damaged.
3. Use verified checkpoints and metadata chains.
4. Use recovery manifests.
5. Retrieve verified alternate objects from independent copies.
6. Reconstruct missing objects using available parity.
7. Perform bounded frame scanning and salvage if structured recovery is insufficient.

Footer history is an accelerator, not a sole source of truth.

**[1.0 integration]** *Proposed default (from v1.2).* When present, the `0x184D2A5E` accelerator holds the most recent N footers (proposed default N = 16) with their original offsets, so step 2 can usually avoid a backward scan. Each entry MUST still be validated against the commit frame it names. An optional `<archive>.mochi.trailer-backup` sidecar (atomic write-then-rename after each commit) remains permitted and is never authoritative over the archive itself.

### 22.1 Scan safety

Recovery scanners MUST:
- Use bounded parsing and allocation.
- Validate candidate frame boundaries.
- Check stored hashes where available.
- Reject contradictory identities or references.
- Distinguish committed content from uncommitted remnants.
- Record uncertainty instead of inventing missing namespace information.

### 22.2 Repair safety

Repair MUST:
- Preserve the original damaged source where practical.
- Produce a repair plan before mutation.
- Identify the source of each reconstructed object.
- Write repaired output to a new object, segment, or archive where practical.
- Verify the repaired result independently.
- Update inventory and location records only after validation.
- Record unrecoverable files and affected snapshots explicitly.

A successful partial salvage MUST NOT be labeled a successful complete restoration.

## 23. CLI Design

The following command interface is proposed; it does not claim existing implementation support.

| Command | Purpose |
|---|---|
| `mochi create` | Create an archive |
| `mochi append` | Publish a new commit |
| `mochi get` | Extract selected files |
| `mochi list` | List a snapshot namespace |
| `mochi snapshot` | Inspect or retain snapshots |
| `mochi search` | Search names, metadata, or content |
| `mochi verify` | Perform read-only verification |
| `mochi fsck` | Perform deep read-only consistency analysis |
| `mochi health` | Summarize policy and evidence |
| `mochi inventory reconcile` | Compare expected and observed storage |
| `mochi restore-test` | Restore into an isolated test destination |
| `mochi repair plan` | Produce a proposed repair plan |
| `mochi repair apply` | Apply an explicitly approved repair |
| `mochi checkpoint` | Create a verified metadata checkpoint |
| `mochi compact` | Create a compacted representation |
| `mochi gc plan` | Identify collection candidates |
| `mochi gc apply` | Apply an approved collection plan |
| `mochi rekey` | Rotate or rewrap keys under explicit semantics |
| `mochi dump-index` | Inspect catalog or index records |
| `mochi split` / `mochi join` | Manage verified segmented representations |
| `mochi mount` | Expose a read-only snapshot |

### 23.1 Example commands

```bash
mochi verify archive.mochi --level stored --json
mochi verify archive.mochi --level content --snapshot retained --json
mochi health archive.mochi --policy preservation.yaml --json
mochi inventory reconcile --inventory inventory.db --json
mochi restore-test archive.mochi --source independent --destination ./restore-test
mochi search archive.mochi "invoice" --snapshot HEAD --require-complete
mochi repair plan archive.mochi --output repair-plan.json
```

### 23.2 Automation exit codes

| Code | Meaning |
|---|---|
| 0 | Requested checks passed |
| 1 | Verification or policy failure |
| 2 | Degraded, unknown, or overdue required evidence |
| 3 | Invocation, configuration, or operational error |
| 4 | Unsupported required feature or verification capability |

When multiple conditions occur, the JSON report is authoritative. Exit-code precedence MUST be documented in the CLI specification.

**[1.0 integration]** *1.0 command scope.* In 1.0: `create`, `append`, `get`, `list`, `snapshot`, `search`, `verify`, `fsck`, `health` (local evidence only), `restore-test`, `repair plan`, `repair apply`, `checkpoint`, `compact`, `gc plan`, `gc apply`, `rekey` (with the Encrypted profile), and `dump-index`. Post-1.0: `inventory reconcile` (Preservation profile), `split` / `join` (Segmented profile), and `mount`. A 1.0 build invoked with a post-1.0 command MUST exit with code 4, not 3. The v1.2 form `mochi verify --repair` is removed; verification and repair are separate operations (Sections 20.2, 22.2).

**[1.0 integration]** *Health from local evidence [delegated 2026-10-08; plan K5, W4].* `mochi health` runs no check. It summarizes the evidence this client recorded when it last ran `verify`, `fsck`, `restore-test`, and a complete `repair apply`, judged against a policy for the archive's current head, and reports the Section 20.4 values per dimension. The policy file is JSON (`mochi-health-policy-v1`: the required dimensions and a maximum age in days per kind of evidence, `verify` and `restore_test`); the `.yaml` file in the Section 23.1 example is illustrative, not normative. Per dimension and kind of evidence: no evidence is `UNKNOWN`; a recorded `FAIL` stays `FAIL` (an append-only archive does not heal damaged bytes, and no policy hides a failure); evidence about an earlier head is `UNKNOWN`; evidence older than the policy permits is `OVERDUE`; otherwise the recorded status. Freshness is judged as `verify` judges it. Durability, key availability, searchability, and retention compliance are not established by local evidence and are never `PASS` in 1.0. Evidence that cannot be read prevents any `PASS`. The rules and the log format are in `docs/c14-cli.md`, rule 13.

### 23.3 Desktop application requirements **[1.0 integration]**

These requirements apply to the MOCHI desktop application and any other interactive client claiming conformance. They restate the invariants above in interface terms; where they conflict with a usability preference, the invariant wins.

1. **One implementation.** All reading, writing, verification, and repair go through `mochi-core`. The UI layer MUST NOT parse or produce MOCHI bytes.
2. **Honest status.** Results use the Section 20.4 values and are shown per health dimension (Section 20.3). `UNKNOWN`, `OVERDUE`, `UNSUPPORTED`, and `DEGRADED` MUST NOT be rendered with success styling, and an overall indicator MUST NOT hide a failing dimension.
3. **"Test archive" is read-only.** It maps to `verify` and MUST NOT modify the archive (Section 20.2).
4. **Repair is a reviewed plan.** Repair MUST show the plan, require explicit approval, write to a new file by default, and re-verify the result (Section 22.2). Partial salvage MUST be labelled as such.
5. **Deletion is namespace deletion.** Removing a file from an archive MUST be presented as removal from the current snapshot, with a clear statement that the content remains in retained history until retention and garbage collection permit removal (Section 16.3).
6. **Local commit is not preservation.** After an add or append, the UI MAY say the commit completed; it MUST NOT say the data is "backed up," "safe," or "preserved" (Sections 5.3, 12.4).
7. **Safe extraction.** Path traversal MUST be rejected. Name collisions and unsupported names on the destination filesystem MUST be reported and MUST NOT silently overwrite (Section 10.4).
8. **Search coverage is visible.** Zero results with partial coverage MUST be shown as incomplete, not as "no matches" (Section 19.3).
9. **Archive strings are untrusted.** Paths, labels, and other archive-supplied text MUST be treated as data by the UI and never interpreted as markup or script.
10. **Secrets.** Passphrases and keys MUST NOT be logged or persisted unless the user explicitly opts into the operating system's credential store.
11. **Cancellation.** Long operations MUST be cancellable. Cancellation before publication MUST leave the previous head as the valid head (Section 5.1).
12. **Evidence export.** Any health result shown in the UI MUST be exportable as the Section 20.5 JSON report.

### 23.4 Library interface (non-normative) **[1.0 integration]**

`mochi-core` exposes the same operations as the CLI. Verification results are typed values that serialize to the Section 20.5 report schema, errors carry the same stable codes as report findings, and long operations accept a progress sink and a cancellation token. The CLI and desktop application are thin clients of this interface, so both produce identical results for identical inputs.

## 24. Automated Test Strategy

### 24.1 Test layers

Implementations MUST maintain:
- Unit tests.
- Golden serialization vectors.
- Cross-reader interoperability tests.
- Property-based metadata and extent tests.
- Parser fuzzing.
- Crash and storage-fault injection.
- Search completeness tests.
- Retention and collection tests.
- Clean-environment restoration tests.
- Migration and backward-read tests.

### 24.2 Release-blocking fault matrix

| Injected failure | Required result |
|---|---|
| Termination at each publication stage | Previous complete commit or new complete commit; never a mixed state |
| Truncation at every byte of small fixtures | No invalid commit accepted |
| Torn, reordered, or lost writes | No false durability acknowledgement |
| Corrupted latest footer | Recover validated history or report lost freshness |
| False magic inside payload | No false object or commit acceptance |
| Oversized or malicious frame metadata | Bounded resource use and explicit rejection |
| Destroyed SQLite catalogs | Recover the promised scope through manifests |
| Missing shared dictionary | Identify all affected files and exercise recovery |
| Corrupted content object | Detect and recover where policy permits |
| Damage beyond parity capacity | Explicit unrecoverable result |
| Deleted primary archive | Inventory detects absence; independent restore succeeds |
| Older valid archive substituted | Freshness failure despite valid internal hashes |
| Original machine unavailable | Restore on a clean machine |
| Primary key service unavailable | Exercise the documented recovery arrangement |
| Index deleted | Rebuild and match expected search coverage |
| Interrupted indexing | Incomplete coverage remains visible |
| Concurrent conflicting commits | One valid publication or explicit conflict |
| Stale writer resumes | Publication rejected by the enforced fencing mechanism |
| GC overlaps publication | Required objects remain reachable and retained |
| Compaction interrupted | Original retained representation remains recoverable |
| Corrupted inventory | Restore inventory and reconcile without unsafe deletion |
| Retention expires under legal hold | Protected objects are not deleted |
| Path traversal or naming collision | Safe extraction with explicit errors |

### 24.3 Restore acceptance

A restoration test passes only when:
- The intended snapshot is identified.
- Expected namespace entries are accounted for.
- File lengths and complete content hashes match.
- Promised attributes are verified or exceptions are reported.
- No required dependency is missing.
- Recovery time and tested scope are recorded.

Read-only mounting is not a substitute for restoration testing.

## 25. Observability and Service Objectives

Deployments MUST configure:
- Maximum preservation lag.
- Recovery-point objective.
- Recovery-time objective.
- Full-scrub coverage deadline.
- Maximum age of independent restoration evidence.
- Search-index lag and coverage targets.
- Required recovery-copy count and failure domains.
- Alert severity and ownership.

Useful metrics include:
- Missing expected objects.
- Corrupt bytes and objects.
- Oldest unchecked object age.
- Replica completeness.
- Preserved-head lag.
- Restore success rate and duration.
- Search coverage and extraction failures.
- Key-recovery readiness.
- Pending quarantined objects.
- Unresolved repair findings.

Targets are policy values, not universal format guarantees.

## 26. Compatibility and Migration

Design-document version, container wire version, metadata schema version, and extension versions are distinct.

Readers MUST:
- Reject unsupported required features.
- Preserve safe read-only behavior where possible.
- Avoid interpreting unknown semantics as empty or successful content.
- Identify legacy formats explicitly.

Migration from v1.2 SHOULD:
1. Preserve the original archive.
2. Verify readable source content.
3. Reconstruct deterministic snapshot semantics.
4. Create v2 recovery manifests.
5. Write the proposed v2 representation.
6. Compare restored file hashes and retained namespaces.
7. Establish independent recovery copies.
8. Record the migration mapping and evidence.
9. Retain the source until migration acceptance and retention policy permit removal.

Ambiguous legacy deletion or version semantics MUST be reported. Migration MUST NOT invent historical certainty.

## 27. Performance Claims

Performance documentation MUST separate:
- Footer lookup.
- Catalog opening.
- Metadata replay.
- Dependency lookup.
- Compressed-byte reads.
- Decryption and decompression.
- File reconstruction and output.
- Search indexing and query execution.
- Full verification and restoration.

A footer seek may be constant-time. Opening an archive with an unbounded metadata chain is not necessarily constant-time.

Restoration cost is at least proportional to the content produced.

Benchmarks MUST state dataset, hardware, cache state, chunking policy, compression settings, encryption settings, and verification level.

## 28. Ratification and Release Gates

Before declaring v2.0 interoperable, publish:
- Final frame registry and version mapping.
- Complete record-envelope layouts.
- Canonical commit and manifest serialization.
- Complete SQLite DDL and constraints.
- Cryptographic suite definitions and vectors.
- Erasure-coding suite definitions and vectors.
- Segmented-location and publication rules.
- Stable report schemas and error codes.
- Golden valid, corrupt, and interrupted archives.
- Independent reader/writer interoperability results.

Before claiming preservation readiness, demonstrate:
- Independent inventory restoration.
- Detection of whole-archive deletion.
- Detection of stale valid heads.
- Restoration without the original machine.
- Restoration after primary catalog loss.
- Recovery-key operation where encryption is enabled.
- Search-index rebuilding.
- Safe compaction and collection under fault injection.
- Required recovery-copy and retention enforcement.

Until these gates pass, implementations MUST describe support as experimental or draft-compatible.

## 29. Summary of Changes from v1.2

| Area | v2.0 refinement |
|---|---|
| Preservation | Separates format integrity from operational durability |
| Footer | Places every footer inside a skippable frame |
| Versioning | Separates document, wire, schema, and extension versions |
| Metadata | Defines immutable records and explicit namespace operations |
| Recovery | Requires independent manifests and named recovery scopes |
| Frame scanning | Rejects decoded-size-as-stored-size assumptions |
| Integrity | Defines distinct byte representations and digest scopes |
| Commit safety | Requires backend-specific durable publication |
| Replication | Separates local commit from preservation acknowledgement |
| Freshness | Checks against independently recorded expected roots |
| Encryption | Requires ratified framing, nonce rules, and key recovery |
| Redundancy | Defines bounded repair and explicit coverage |
| Retention | Protects complete transitive dependencies |
| Search | Adds coverage, provenance, and rebuild requirements |
| Verification | Makes checks read-only by default |
| Repair | Separates planning, authorization, mutation, and re-verification |
| Health | Reports evidence by dimension rather than one opaque result |
| Testing | Adds crash, corruption, deletion, rollback, and restore gates |
| Compatibility | Narrows TAR fallback to a tested restricted profile |

### 29.1 Specific corrections to v1.2 technical claims **[1.0 integration]**

These are cases where v1.2 (or the unified draft built on it) was not merely less complete but incorrect. Implementers working from the older documents should treat each as a known defect.

| # | v1.2 claim | Problem | Now |
|---|---|---|---|
| 1 | 64 bare trailer bytes at EOF; old trailers left mid-file after append | Not valid Zstandard frames; `zstd -dc` errors at the first trailer, so the headline POSIX fallback could not work | Every footer in a skippable frame (8.1, 8.4) |
| 2 | Recovery scanner derives frame boundaries from `Frame_Content_Size` | That field is decoded size, not stored length | Structural block walk (8.5, 8.6) |
| 3 | Zstandard `Content_Checksum` is XXH64 | It is 32 bits, the low four bytes of XXH64 | 8.5 |
| 4 | Ciphertext inside `0xFD2FB528` frames that tools "skip cleanly" | A decoder would try to decode ciphertext and fail | Dedicated `0x184D2A59` envelope (14.1) |
| 5 | Key-envelope locations stored in encrypted metadata | Bootstrap circularity | Unencrypted discovery path (14.3) |
| 6 | Trailer hash over *decrypted* metadata | Structure cannot be verified without keys | Footer digest over stored commit bytes (8.4) |
| 7 | `PRAGMA journal_mode = OFF` for metadata | Unsafe as an authoring protocol | Private journaled working DB, then publish a standalone image (10.5) |
| 8 | In-place compaction by truncation | Destroys the only known-good representation | Compaction writes a new representation (18.2) |
| 9 | `mochi verify --repair` | Conflates read-only verification with mutation | `verify` read-only; `repair plan` / `repair apply` (20.2, 22.2) |
| 10 | "O(1) open," "< 5 ms TTFB" | Not decomposed; false with unbounded metadata chains | Decomposed performance claims (27) |
| 11 | Integer `AUTOINCREMENT` IDs for sessions, chunks, files | Unsafe under concurrent preparation and repacking | Stable logical identities (4.2, 10.1, 17) |
| 12 | Unified draft §9.1: chunk checksum over compressed, pre-encryption bytes | Contradicted v1.2 §5 and the implementation plan | Decoded-bytes chunk hash; separate stored-object hash (9.2) |
| 13 | Unqualified "zero-tool POSIX fallback" | Overstated; history and deletions break latest-snapshot equivalence | Restricted, version-tested TAR profile (7.2) |

## 30. Final Design Principle

MOCHI should never equate “the archive opens” with “the archive is safe.”

A preservation claim requires evidence that:
- Expected archives and objects still exist.
- Stored content passes integrity checks.
- Retained snapshots have complete dependencies.
- Independent recovery sources are available.
- Required encryption keys remain recoverable.
- Files can actually be restored.
- Search coverage is known.
- Health evidence is current.
- Destructive maintenance cannot silently invalidate retention.

The system must make uncertainty visible before it becomes irreversible loss.

---

## Annex A. Extension Drafts: Status Under This Specification **[1.0 integration]**

The unified v1.2 draft contained detailed extension sections (its §11–§20). They are **not** normative under this specification. Each must be re-expressed against the object, commit, and namespace model of Sections 10–12 before ratification. Their full text remains available in the superseded draft for reference.

| Extension | Target | What survives | What must change |
|---|---|---|---|
| Multi-writer transactions | Post-1.0 (concurrent-preparation profile) | Prepare/commit split; transaction UUIDs for idempotency; `fail-on-conflict` default; path-level mutation preconditions; security/retention conflicts never auto-resolved | Fencing counts only if the backend rejects stale tokens (12.5). Shared-file range reservation leaves Core; frame `0x184D2A53` stays reserved |
| Remote HTTP/S3 range access | Post-1.0 (segmented) | Suffix-range bootstrap; `If-Match` generation pinning; ETag is a transport token, not a content hash; range coalescing; no transparent HTTP compression | Must locate the framed footer and verify with 8.4's digest; object-store publication per 12.3 |
| Read-only mount (FUSE) | Post-1.0 | Read-only first; snapshot-pinned by default; stable inode derivation; untrusted-content safety | A mount is not restoration evidence (24.3) |
| Content-defined chunking (FastCDC) | 1.x | Standardized Gear table, normalization masks, and test vectors are mandatory for interop; chunking before encryption; convergent encryption not default | Parameters recorded per archive (13); no ratified size defaults |
| Snapshots, Merkle trees, signatures | Snapshots: 1.0 (Core). Merkle/signatures: post-1.0 | Commit-hash chain; sorted-leaf object tree; odd-node promotion rule; signatures prove key control, not identity; forks detectable, rollback needs an external witness | Signatures use the `0x184D2A54` extension frame; commit IDs per 9.2 canonicalization |
| Full-text search (FTS5) | 1.0 (path/identity discovery required; FTS5 optional) | Contentless index; sandboxed, versioned extractors; encrypted-index caveats | Coverage reporting and rebuild sources are mandatory (19.3, 19.4) |
| Split / multi-volume | 1.x (segmented) | Volume manifest; per-volume identification; exact missing-volume reporting | Ordering by verified manifest, not filenames (17); a segment copy is not a recovery copy |

## Annex B. Open Decisions for MOCHI 1.0 **[1.0 integration]**

Each decision blocks the listed work until recorded in this annex. D1–D9 are recorded in B.1. D10–D15 are decided in B.2. Their implementation and evidence are pending (B.2.6), and they are not yet recorded in B.1.

| ID | Decision | Options | Blocks |
|---|---|---|---|
| D1 | Wire generation and footer magic | Keep `MOCHI2` (refinement default); or, if no v1.2-layout archives exist outside development, restart at generation 1 so product and wire versions align | Ratification item: frame registry and version mapping |
| D2 | Recovery-manifest encoding | A canonical binary encoding (for example deterministic CBOR) or a purpose-built format; must be parseable without SQLite and canonicalizable | Section 11; recovery and repair |
| D3 | Cryptographic suite for 1.0 | A single AEAD suite for the first profile identifier, or two distinct profile identifiers | Encrypted profile |
| D4 | TAR-compatibility default | On by default (with its restrictions on dedup and fragmentation) or opt-in | Writer defaults; desktop "create" dialog |
| D5 | Split volumes in 1.0 | Keep in 1.x as planned, or pull the Segmented profile into 1.0 | Scope, and the segmented ratification item |
| D6 | Promised attributes | The Section 10.4.1 proposal, or a narrower/wider set | Restore acceptance (24.3) |
| D7 | Interoperability evidence | Section 28 requires independent reader/writer results; name the second implementation (for example a minimal spec-only reader in another language) | Declaring the format interoperable |
| D8 | Freshness anchor without an inventory | Report freshness as `UNKNOWN`; accept a user-supplied expected head; and/or let clients remember the last-seen head per archive as a local anchor | Health dimension "freshness" in 1.0 |
| D9 | Other archive formats | Whether the desktop application opens ZIP, 7z, TAR, or RAR in 1.0 (a common WinRAR expectation), which is outside this specification | Desktop scope |
| D10 | Metadata delta (plan O26) | Recovery manifest as delta; SQLite row delta; dedicated CBOR row delta. Each needs a checkpoint policy | C6; R3; R4 |
| D11 | Record envelopes (plan O16) | Uniform binary header; required CBOR keys plus a binary header for opaque payloads; bare frames | R2; R3 |
| D12 | Archive descriptor (plan O27) | Contents; placement; immutability; reader behaviour when unavailable | R1; R2; C10; C11 |
| D13 | Archive creation (plan O12) | In place; or a temporary file published without replacement | C5 follow-up; D2 |
| D14 | Tail truncation eligibility and audit (plan O28) | Audit in the report or in the archive; eligibility rule | C7; C14 |
| D15 | Report status layers and timestamps (plan O13) | Single rollup; or separate evidence, policy, and exit layers | C7; C14 |
| D16 | **Decided [delegated 2026-10-08]: 1.x, not in 1.0.** `create --exceed-default-limits` is refused with `UNSUPPORTED_FEATURE` (exit 4) and a message that it is a 1.x feature; no writer writes key 6, and readers keep decoding it without acting on it. Gate G4's opt-in item moves to 1.x. The sub-questions below stay unanswered and must be answered before the flag ships. *The question:* raising writer limits at creation (`create --exceed-default-limits`, B.2.3). B.2.3 says the choice records key 6 and prints a warning, but not which limits a writer may raise, by how much (any ceiling below the wire maxima), whether key 6 must equal what the writer then enforces, which report field shows it, or whether a reader with lower limits reports `LIMIT_EXCEEDED` naming the declared value as soon as it opens the archive or only when an object exceeds its own limit (plan checklist Q10). Today a reader decodes key 6 and does not act on it. **Deferred by the owner on 2026-10-06 (plan Q10), decided out of 1.0 on 2026-10-08:** builds keep the reader defaults for every writer and refuse the flag by name; nothing writes key 6 | Values allowed per limit; ceiling; report field | T29 opt-in path; G4 opt-in item |
| D17 | **Decided by the owner on 2026-10-06 (plan Q54); not wire format.** The §12.2 step 1 publication lock is an OS lock on a separate lock file, not on the archive (on Windows a lock on the archive blocks every other handle's reads). The lock file's existence never means ownership, and normal unlock never removes it. Naming and alias handling: see B.2.7 | — | C6 Windows reads during append |
| D18 | **Decided by the owner on 2026-10-07 (plan C9); wire format: recovery-manifest schema 2.** Retention is expiry plus legal holds; collection and compaction write a new archive with a new archive ID and provenance; the source is never removed automatically. See B.2.8 | — | C9; D4 (desktop retention UI); C14 `snapshot`, `gc`, `compact` |

| D19 | **Decided [delegated 2026-10-08]; no wire change.** The TAR-stream compatibility profile (spec 7.2) is written by emitting, per commit with at least one put, one complete POSIX pax TAR stream whose non-content bytes live in ordinary data chunks that no extent references (*stream-only chunks*). See B.2.9 | — | C10; C9 (GC accounting, rewrites keep the profile); C7 (`PROFILE_VIOLATION`); desktop "create" dialog (D4) |

Status of the open questions from the original v1.2 review: chunk-hash ordering — resolved (9.2); concurrent writers — resolved for Core (12.5); `content_index` schema — superseded, DDL is a ratification artifact (10.1); version-byte mapping — reframed by Section 26 and D1; Windows attribute defaults — D6.

### B.1 Recorded decisions (2026-09-30)

Each entry below closes the decision above with the same ID. Rationale and consequences are in `docs/implementation-plan.md` §9.

| ID | Decision |
|---|---|
| D1 | **Keep `MOCHI2`.** The wire generation stays 2, independent of the product version. No collision with any v1.2-layout file is possible, whether or not such files exist outside development. |
| D2 | **Deterministic CBOR** (RFC 8949 §4.2.1 core deterministic encoding), restricted to: unsigned and negative integers, byte strings, UTF-8 text strings, arrays, and maps with unsigned-integer keys in canonical order. No floats, tags, simple values other than `false`/`true`/`null`, indefinite lengths, or duplicate keys. Schemas are written in CDDL (RFC 8610) and versioned. A reader MUST reject any input that does not re-encode to identical bytes. The **same encoding is used for the canonical commit body** (Section 9.2, Commit ID; ratification item R3), so the format has one canonicalization rule. |
| D3 | **One suite for the first Encrypted-profile identifier: XChaCha20-Poly1305 with independently random 192-bit nonces; Argon2id for passphrase key derivation, with its parameters recorded per key envelope. 1.0 unlocks by passphrase only.** Random 192-bit nonces satisfy Section 14.2 across retries, interrupted writes, restored backups, and concurrent writers without counter state. Key files and public-key recipients are post-1.0 and would use new profile or envelope identifiers. |
| D4 | **TAR compatibility is opt-in, per archive, fixed at creation.** The default profile keeps dedup, dictionaries, and fragmentation. |
| D5 | **Split volumes stay in 1.x.** The Segmented profile and R10 are not part of 1.0. |
| D6 | **Section 10.4.1, made concrete.** POSIX: permission bits, numeric uid/gid, mtime with nanoseconds; setuid/setgid restored only on explicit request; uid/gid restored only with privilege, otherwise reported as an exception. Windows: `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE` attributes and mtime. Entry types: regular file, directory, symbolic link (target stored as bytes; restored as a link, never written through; reported as an exception where the platform cannot create it). Hard links are stored as separate files. Not promised: extended attributes, ACLs, alternate data streams, owner names, atime, ctime, creation time. |
| D7 | **A read-only verifier in Python, written from this specification alone**, sharing no code with the reference implementation. Section 28's interoperability claim for 1.0 is scoped to the profiles 1.0 ships (Core, TAR-compatibility, Encrypted, Redundancy). |
| D8 | **Layered, and the source is always reported.** With no anchor, freshness is `UNKNOWN`. A user-supplied expected head gives `PASS`/`FAIL`. Otherwise a client MAY keep the last-seen head per archive ID as a local anchor, giving `FAIL` on rollback or substitution. First sight is `UNKNOWN`, never `PASS`. The report names the anchor used. |
| D9 | **The desktop application reads ZIP, 7z, RAR, and `.tar.gz`/`.tar` (browse, extract, test) and never writes them.** Outside this specification; see the plan, phase D8. |

### B.2 Wire batch (decided 2026-10-01)

**Status.** This annex separates three things, and the text below never treats one as another:
- **Semantic decisions** (B.2.1) and **wire layouts** (B.2.2) are *decided*. Their schemas are drafts under R3.
- **Implementation** is tracked task by task in `docs/b2-implementation-checklist.md`. None of it is done.
- **Evidence and release gates** (B.2.6) are *pending*.

An entry moves to B.1 only when its implementation tasks are complete and its gates pass. Nothing in this annex is implemented, tested in the reference implementation, or evidenced by being written here.

#### B.2.1 Semantic decisions

**D10 — Metadata deltas and checkpoints (plan O26).**
1. **The delta is the recovery manifest.** A commit's metadata delta is its recovery manifest (kind 0, "delta"). There is no other delta encoding.
2. **Checkpoint commits bind three objects.** Every checkpoint commit references, by stored-object hash:
   - its delta manifest;
   - a complete catalog image;
   - a complete snapshot manifest (kind 1).

   Commit 0 is always a checkpoint.
3. **Authoritative state.** Every mutation of state that a reader, verifier, or GC relies on is a manifest operation, and a snapshot manifest covers all of that state. Derived, rebuildable state (search documents, §19.4) is excluded. Roots, holds, retention changes, dictionaries, key-envelope references, and parity membership join both manifest kinds in the phases that introduce them, each as a new manifest schema version.
4. **Replay.** Snapshot *c* is built by applying the delta manifests after base *b*, in sequence order, to *b*'s checkpoint.
   - Each manifest applies atomically: on any failure the catalog is unchanged and replay stops.
   - Operations apply in array order, with validity checked against the commit's completed state.
   - **Introduction** (amended 2026-10-04, decision log Q18). A delta manifest *introduces* the object IDs it lists as chunks and the file-version IDs it lists as file versions. An introduced ID MUST NOT already exist in the replay state (introduced by the base checkpoint or by an earlier delta of the segment), and MUST NOT be introduced twice within one manifest. This holds even when the two records are identical: during replay, a second introduction is corruption (O19), and the manifest is invalid (`RECORD_INVALID`). For replay this rule governs; it is stricter than, and does not relax, §10.2's rule for conflicting immutable IDs.
   - **Reference** (amended 2026-10-04, Q18, Q26). Naming an existing ID without listing it (a `PUT` of an existing file-version ID, or an extent of an existing chunk) is reuse by reference, not introduction, and is valid. A `PUT` that names a file-version ID existing neither in the replay state nor among the manifest's own file versions is an invalid namespace operation (`NAMESPACE_INVALID`); validity is judged against the state after the manifest, so a reference to a version the same manifest introduces is valid.
   - An unknown required feature (manifest key 9) causes refusal as an unsupported feature (`UNSUPPORTED_FEATURE`; §26).
   - Under a supported manifest schema version, an operation kind that the version does not define is a schema violation, not an unsupported capability: the manifest is invalid (`RECORD_INVALID`). New operation kinds are introduced only by a new manifest schema version (item 3), and an unknown schema version is refused (D11). (Amended 2026-10-04, Q19.)
   - In both of the preceding cases the commit whose manifest it is, and every snapshot whose replay segment contains that commit, cannot be opened; no earlier state is returned (item 9).
5. **Bounded work.**
   - Each operation kind declares a constant maximum number of logical row mutations (fixed in R3), and a constant number of validation lookups.
   - No operation may imply mutations of rows it does not name, so there are no cascades. Directory delete is non-recursive (C3).
   - A manifest is fully decoded under the B.2.3 limits before any operation is applied, so operation and mutation counts are bounded by the limits, independent of catalog state.
   - Database runtime per mutation (B-tree depth, index maintenance, I/O, memory) is measured, not claimed.
6. **Base rule (deterministic).** For a delta commit, the base is the parent if the parent is a checkpoint; otherwise it is the parent's base. The commit records its base (B.2.2). A reader MUST check the recorded base against this rule and MUST NOT search for another checkpoint.

   The two footer offsets a commit records have different roles (amended 2026-10-03, decision log Q17/Q23):

   * **Parent footer offset (key 4): the traversal offset.** Walking ancestry follows it. It MUST resolve to a valid footer (§8.4), and the commit that footer covers MUST have the commit ID and sequence the child declares for its parent. A reader validates the footer first and compares IDs second. If the offset does not resolve to a valid footer, the open fails with the footer-validation error (`FOOTER_INVALID` in the reference error registry). If it resolves to a valid footer whose commit is not the declared parent, or the walk it yields violates the segment rules (this item and item 4), the open fails as an invalid record (`RECORD_INVALID`). An unusable traversal offset is **not** recoverable: a reader MUST NOT search the archive for the parent, and MUST NOT fall back to an earlier head or commit.
   * **Base footer offset (key 5, delta form): a redundant hint.** It is non-authoritative. The base is the commit the validated ancestry reaches by the rule above, identified by the base's commit ID and sequence; a base footer offset that disagrees with where that commit's footer actually is does not invalidate the commit, and a reader MAY report the mismatch as a diagnostic. It MUST NOT be used to locate a base the ancestry has not established.

   **One descriptor per segment** (amended 2026-10-04, decision log Q16). Every commit in a replay segment, from the base *b* through the head *h*, MUST reference the same descriptor: identical offset, stored length, and stored-object hash (commit key 10). A reader checks this while validating the segment, before applying any delta manifest. A difference is a mismatched descriptor (D12): the head is not interpreted, no earlier state is returned, and the open fails as an invalid descriptor (`DESCRIPTOR_INVALID`).
7. **Adoption (§18.1).** Before publishing a checkpoint commit, the writer re-reads both checkpoint representations from their serialized, hashed bytes and checks that each one's authoritative state equals the source snapshot:
   - the snapshot manifest is decoded with the canonical CBOR codec, without SQLite;
   - the image's state is extracted from the image.

   On any mismatch the commit fails, and there is no new head. `verify` repeats this comparison; a mismatch there is a `FAIL`.
8. **Baseline recovery.** Recovery from the checkpoint at *b* starts from snapshot manifest S(*b*) and applies the delta manifests after *b*.
   - It needs **commit *b*'s record**, because the first later delta's parent link is checked against the delta-manifest hash recorded in commit *b*.
   - It needs **neither SQLite, nor delta manifest *b*, nor any earlier manifest.**
9. **Damage scope.**
   - **A damaged delta manifest *j*** breaks every replay segment containing *j*. Snapshots before *j*, and snapshots whose base checkpoint is at or after *j*, are unaffected.
   - **A damaged image with an intact snapshot manifest:** reads rebuild the catalog from the snapshot manifest.
   - **A damaged snapshot manifest with an intact image:** reads continue, and recoverability is `DEGRADED`.
   - **No repair inside a segment.** Without the Redundancy profile, damage inside a segment cannot be repaired.
   - **Reporting.** Verification names the affected sequence range.
10. **GC.** GC rebuilds retention state at the head by replay. If any manifest in the head's segment is missing or unverified, `gc plan` and `gc apply` refuse. GC never falls back to an older checkpoint or a partial replay.
11. **Size.** 1.0 writes every manifest, delta or snapshot, as exactly one frame.
    - **What is rejected.** A commit whose delta manifest, snapshot manifest, or image would exceed the default reader limits (B.2.3) is rejected.
    - **What rejection means.** Rejection happens before the footer: the previous head remains the head, nothing is split, and a mandatory checkpoint is never skipped.
    - **Capacity.** The archive therefore has a bounded maximum state (B.2.4).
    - **Deferred.** Multi-frame manifests are deferred, and would need a new schema version.

**D11 — Record envelopes (plan O16).** The §8.3 envelope has two encodings, by amendment of §8.3:
- **Deterministic-CBOR records** (commit, manifest, descriptor): required keys inside the record body.
- **Opaque payloads** (catalog images now; dictionaries and parity later): the binary envelope header defined in B.2.2.

Each §8.3 field has one validation obligation, enforced identically for both encodings:

| §8.3 field | Obligation |
|---|---|
| Record type | Frame kind matches the field that references it |
| Schema version | Unknown version: refuse |
| Required features | Any unknown feature: refuse; the list must be strictly increasing |
| Archive identity | Equals the archive ID of the commit that references it |
| Payload encoding | Implied by frame kind for CBOR records; for binary payloads, a registered value, and the payload must have that encoding's signature |
| Payload length | CBOR: the whole payload is consumed. Binary: the header's payload length equals the frame payload minus the header |
| Integrity scope | The stored-object hash held by the referencing record; a record found by scanning is a candidate until bound this way |
| Identity | Archive ID, commit sequence, and transaction ID of the commit it belongs to |

No object contains the ID of the commit that references it. The commit ID covers those objects' hashes, so including it would be circular.

**D12 — Archive descriptor (plan O27).**
- **Form.** One descriptor frame at offset 0, written once and never rewritten. Every commit references it by hash.
- **Contents.** Creation-time facts only. It never locates key envelopes.
- **Changing a creation-time constraint means writing a new archive.** In particular, **there is no in-place conversion to the Encrypted profile in 1.0**; a request for one exits 4.
- **Security.** The descriptor hash is an integrity binding relative to a trusted head (D8 anchor), not authentication. Authentication requires signatures (post-1.0).
- **Missing, damaged, or mismatched descriptor.**
  - *Permitted:* head discovery, footer and commit validation, the structural walk, stored-integrity checks, and diagnostics.
  - *Reported:* `FAIL`.
  - *Refused:* interpretation (listing, extraction, replay), except through explicit salvage labelled partial; append.
  - *Repair* may write a new archive with a descriptor that is labelled as reconstructed.
- **Placement.** A descriptor frame at any offset other than 0 is invalid. Because there is exactly one descriptor, at offset 0, two commits of one archive that reference different descriptors cannot both be valid; D10.6 checks this for every commit a replay depends on.

**D13 — Creation (plan O12; §12.2 step 9).**
- **Mechanism.** The first commit is written to an exclusively created, locked temporary file in the destination directory, synced, published at the final name **without replacing an existing file**, and the directory is then flushed.
- **Overwrite** requires an explicit request and a separate code path.
- **Cleanup** removes only temporary files whose lock it can acquire.
- **Durability claims are limited to documented mechanisms.**
  - *Linux (local ext4):* confirmed when both the file fsync and the directory fsync succeed.
  - *Windows:* directory durability stays `Unconfirmed` until gate G6 passes.
  - *Other filesystems:* unconfirmed.
- **Outcomes.** A published archive whose directory flush is `Unconfirmed` reports `LOCAL_COMMITTED` with durability `DEGRADED`. A directory-flush *error* reports `COMMIT_UNCONFIRMED` and poisons the writer; the file is left in place.

**D14 — Tail truncation (plan O28).**
- **Explicit only.** Truncation never happens automatically. It happens only on explicit request, under the exclusive publication lock, for an *eligible* tail.
- **Eligibility.** A tail is eligible only if all of these hold:
  - it walks as complete frames that are not footers, optionally ending in one frame cut short by end-of-file;
  - neither footer marker (the footer's skippable header or its payload magic) occurs where a complete footer could fit;
  - it contains no descriptor frame;
  - it contains no unrecognised bytes.
- **Eligibility is a conservative screen, not proof that the tail is uncommitted.** A single corruption event can destroy both footer markers, which lie within the footer's first 16 bytes. The term is "eligible for truncation".
- **Quarantine first, by default.** The tail is first copied, exactly, into a no-clobber sidecar file (B.2.2) with identifying metadata. The sidecar is re-read, its hash compared with the tail's, and the sidecar and directory synced.
- **What blocks truncation.** Any quarantine failure blocks truncation unless the user passes `--no-quarantine`. A directory flush that is not `Confirmed` (which on Windows is always the case before G6) blocks truncation unless the user passes `--accept-unconfirmed-durability`.
- **With `--accept-unconfirmed-durability`,** the sidecar is still written, verified, and synced, so all available protection is kept. Only the confirmation requirement is waived.
- **Recording waivers.** Every waiver is recorded as a report finding.
- **Audit record.** The audit record goes to the caller and the external report (§20.5). Nothing is written into the archive.

**D15 — Reports, exit codes, and timestamps (plan O13).**
- **Three results.** Reports carry three results under distinct fields:
  - *evidence:* each dimension's status, plus a rollup ordered `FAIL` > `UNSUPPORTED` > `DEGRADED` > `OVERDUE` > `UNKNOWN` > `PASS`;
  - *policy result:* the same rollup over the required dimensions only, with the policy listed;
  - *exit code.*
- **When freshness is required.** Only when the user supplied an expected head, the local history holds this archive ID, or freshness was explicitly requested.
- **Exit precedence.** The first rule that matches wins:

  | Exit | Condition |
  |---|---|
  | 1 | Any reported dimension is `FAIL`, whether required or not |
  | 3 | An operational error occurred |
  | 4 | The policy result is `UNSUPPORTED` |
  | 2 | The policy result is `DEGRADED`, `OVERDUE`, or `UNKNOWN` |
  | 0 | Otherwise |

- **Timestamps.**
  - *Format:* RFC 3339, UTC, `Z` suffix, exactly nine fractional digits.
  - *Range:* years 0000–9999; the writer refuses times outside it.
  - *Leap seconds:* the writer never emits second 60; readers accept it.
  - *Ordering:* lexical order equals chronological order for values the writer produces.

#### B.2.2 Wire layouts

| Structure | Frame kind | Schema |
|---|---|---|
| Commit record v1 | `0x184D2A51` | `docs/schemas/commit-record-v1.cddl` |
| Recovery manifest v1 (delta / snapshot) | `0x184D2A58` | `docs/schemas/recovery-manifest-v1.cddl` |
| Archive descriptor v0 | `0x184D2A57`, offset 0 | `docs/schemas/archive-descriptor-v0.cddl` |
| Catalog image | `0x184D2A50` | Binary envelope v0 (below) + SQLite image |
| Tail-quarantine sidecar v0 | Not in the archive | `docs/schemas/tail-quarantine-v0.cddl` |

**Commit v1 keys.**

| Key | Meaning |
|---|---|
| 0 | Schema version = 1 |
| 1 | Archive ID |
| 2 | Sequence |
| 3 | Transaction ID |
| 4 | Parent `{commit ID, sequence, footer offset}`, or null for sequence 0. The footer offset is the traversal offset (D10.6): it must resolve to the parent's valid footer. |
| 5 | Metadata: checkpoint `{0:0, 1: image ref, 2: snapshot-manifest ref}`, or delta `{0:1, 1: base {commit ID, sequence, footer-offset hint}}`. The base footer offset is a non-authoritative hint (D10.6). |
| 6 | Delta-manifest ref |
| 7 | Required features |
| 8 | Optional informational time |
| 9 | Commit ID |
| 10 | Descriptor ref |

- **Commit ID.** BLAKE3 over the draft separator and the canonical encoding of the map without key 9 (unchanged from v0).
- **Hash acyclicity.** Every reference points to bytes earlier in the file. No referenced object contains the commit's ID; images and manifests carry only the archive ID, sequence, and transaction ID.
- **Sequence 0.** The CDDL itself requires sequence 0 to be a checkpoint with a null parent.

**Manifest v1.**
- **Changes from v0.** v0 plus key 9 (required features) and key 10 (transaction ID).
- **Delta (kind 0).**
  - *Parent:* the parent is null only at sequence 0.
  - *Parent link:* `parent.1` is the stored-object hash of the **parent commit's delta manifest** (that commit's key 6), never of a snapshot manifest.
  - *Contents:* key 8 is empty.
- **Snapshot (kind 1).**
  - *Parent:* the parent is always null.
  - *Contents:* key 7 is empty, and keys 5, 6, and 8 are complete.
  - *Identity:* keys 2 and 10 equal the checkpoint commit's sequence and transaction ID.
- **CDDL enforcement.** The schema expresses the delta and snapshot shapes as separate rules, so CDDL rejects a snapshot with a parent or operations, and a delta with entries.

**Descriptor v0.**

| Key | Meaning |
|---|---|
| 0 | Schema version = 0 |
| 1 | Archive ID |
| 2 | Wire generation = 2 (`MOCHI2`) |
| 3 | Draft identifier: 1 for this batch; null only in a ratified archive |
| 4 | Required features |
| 5 | Constraints `{0: TAR-compatible}` |
| 6 (optional) | Declared limits; present only on opt-in (B.2.3) |

Archives from before this batch have no descriptor and are rejected as legacy drafts (§26).

**Binary envelope v0** (at the start of the skippable payload; integers unsigned little-endian; every byte defined):

| Offset | Size | Field | Rule |
|---|---|---|---|
| 0 | 4 | Header length | = 80 + 8·n |
| 4 | 2 | Envelope version | = 0 |
| 6 | 2 | Record schema version | For a catalog image: equals the image's SQLite `user_version` |
| 8 | 4 | Payload encoding | 0 = SQLite 3 database image (payload starts `SQLite format 3\0`); other values unregistered |
| 12 | 4 | n = required-feature count | ≤ 64 |
| 16 | 8 | Payload length | = frame payload length − header length |
| 24 | 32 | Archive ID | |
| 56 | 8 | Commit sequence | |
| 64 | 16 | Transaction ID | |
| 80 | 8·n | Required features | u64 values, strictly increasing |

This envelope replaces the 68-byte draft in `mochi-format/src/envelope.rs`. That draft's 16-byte archive identity does not match the 32-byte archive ID used everywhere else.

**Tail-quarantine sidecar v0.**
- **File name:** `<archive name>.tail-<decimal offset>-<16 hex digits of the tail's BLAKE3>.mochiq`, in the archive's directory, created exclusively.
- **Layout:** the magic `MOCHITQ\0`, then a u32 little-endian metadata length L (1 ≤ L ≤ 65,536), then the metadata as deterministic CBOR, then the exact tail bytes. The file length is 12 + L + the tail length.
- **Metadata contents:** archive ID, head commit ID and sequence, tail offset and length, the tail's plain BLAKE3, the frame magics found in the tail, whether the final frame is incomplete, the tool version, and the time.

**CLI surface.**
- **Truncation:** `mochi append --truncate-tail`, which may be combined with `--no-quarantine` or `--accept-unconfirmed-durability`.
- **Limits:** `mochi create --exceed-default-limits` opts into larger limits (B.2.3); **not in 1.0** (D16: exits 4 `UNSUPPORTED_FEATURE`). On readers, `--limit <name>=<value>` raises a limit explicitly.
- **Desktop:** the desktop app offers the same choices, behind explicit confirmation.

**New error codes** (drafts, O15/R7):

| Code | Exit | Raised when |
|---|---|---|
| `DESTINATION_EXISTS` | 3 | Creation finds an existing file at the final name |
| `CAPACITY_EXCEEDED` | 3 | A commit would exceed a default limit; no head is published |
| `CHECKPOINT_MISMATCH` | 3 (writer) / 1 (`verify` finding) | Checkpoint representations disagree with the source snapshot |
| `DESCRIPTOR_INVALID` | 1 | The descriptor is missing, damaged, or mismatched |
| `RETENTION_UNRESOLVED` | 1 | GC cannot rebuild retention state |
| `QUARANTINE_FAILED` | 3 | A quarantine step fails |
| `DURABILITY_UNCONFIRMED` | 3 | Directory durability is unconfirmed and truncation is blocked |
| `PROFILE_CHANGE_UNSUPPORTED` | 4 | An in-place profile change is requested |

#### B.2.3 Limits and writer defaults

**Default reader limits.** Each default below is chosen so that it can actually be reached through the frame that carries it.

| Kind | Limit | Default | Derivation |
|---|---|---|---|
| Stored | Skippable payload *S* | 256 MiB | Unchanged; the wire maximum is 4 GiB − 1 |
| Stored | Any frame | 269,484,032 B | max(8 + *S*, `ZSTD_COMPRESSBOUND`(*D*)), where `ZSTD_COMPRESSBOUND`(*D*) = *D* + (*D* >> 8) for *D* ≥ 128 KiB. This is libzstd's documented worst case for one frame, so no output from the writer's encoder can exceed it. Was 4 GiB. |
| Stored | Commit frame | 8 + 64 KiB | A v1 commit is a few hundred bytes plus at most 64 features. Was 256 MiB. |
| Stored | Manifest payload (CBOR) | *S* | One frame (D10, item 11) |
| Stored | Image payload | *S* − 592 = 268,434,864 B | Header at most 80 + 8·64. Replaces the 1 GiB `max_image_len`, which was unreachable. |
| Decoded | Data object *D* | 256 MiB | Unchanged; ≥ the 8 MiB maximum chunk |
| Decoded | Image / CBOR record | = stored | Stored uncompressed; revisit under C11 |
| Resource | CBOR items / depth | 16 Mi / 64 | Unchanged |
| Resource | Required features per record | 64 | New |
| Resource | Catalog memory | ≈ 5.2–5.7 × image size | O25; measured (G4, T32): peak heap while opening, which holds the stored image, SQLite's in-memory copy, and verification's working set. About 1.5 GiB at the full image budget |
| Resource | Decoded-CBOR memory | ≈ 50 B per item (≈ 7.6 × stored size) | Unbounded by the format; measured (G4, T32). About 800 MiB for one record at the 16 Mi item limit |

**Writer default rule.**
- A default writer enforces every *reader-default* limit on everything it emits, whatever limits it was configured to read with.
- Exceeding a default requires `create --exceed-default-limits`. That choice is made at creation, prints a warning, records descriptor key 6, and is shown in every report.
- Readers never raise their limits from a descriptor. They report `LIMIT_EXCEEDED`, naming the declared value, until the user raises the limit explicitly.

**Checkpoint trigger (writer policy, not wire format).**
- **Δ** is the stored bytes of delta manifests, commit records, and footers since the base. It excludes generated checkpoint bytes.
- **B** is the base's combined image and snapshot-manifest size.
- **Rule.** A checkpoint is mandatory when Δ ≥ α·max(*B*, *F*).
- **Defaults** are α = 1 and *F* = 1 MiB, both configurable. **Confirmed by G3** (T32, 2026-10-05; `docs/benchmarks/t32-scaling.md`): over four workloads to 10,000 commits, every replay stayed within its limit, *k* stayed below 1, and neither α = 1/2 nor α = 2 improved one cost without a larger loss in the other. There is no delta-count cap.
- **Forced checkpoints** (the `checkpoint` command) reset the base.

**Storage bound (conditional).**
- **Assumption.** Suppose *B*ᵢ ≤ *B*ᵢ₋₁ + *k*·Δᵢ holds uniformly across intervals and workloads. Here *B* counts both representations, and *k* is measured per interval.
- **Bound.** Then total stored metadata ≤ (1 + 1/α + *k*)·ΣΔ + *B*₀ + Σ*B*_forced. Here ΣΔ is the delta-manifest, commit-record, and footer bytes of every commit after commit 0, checkpoint commits' own included; *B*₀ is all of commit 0's metadata (descriptor, delta manifest, commit record, footer, image, and snapshot manifest); and *B*_forced is each forced checkpoint's image and snapshot manifest. (Clarified 2026-10-05 from the T32 measurements: with *B*₀ read as image and snapshot only, the bound fails whenever commit 0 introduces much state, because commit 0's own delta manifest is then as large as its snapshot.)
- **When it is linear.** The bound is linear in ΣΔ only if that assumption holds and forced checkpoints are bounded.
- **Replay.** Replay reads less than α·max(*B*, *F*) bytes plus one commit's metadata.
- **Evidence, not proof.** Benchmarks can estimate *k* and can refute the assumption. They cannot prove it.

#### B.2.4 Capacity

Every mandatory checkpoint must fit within all of the limits above, so archive state is bounded. Once the bound is reached, mandatory checkpoints fail with `CAPACITY_EXCEEDED`. Reads continue, and only commits that reduce state below the bound succeed.

**Measured (gate G5, T32, 2026-10-05).** From the MOCHI writer and codec (`docs/benchmarks/t32-capacity-memory.md`). These replace the earlier estimates, which came from a general-purpose CBOR library; each estimate was within 5%:
- **Per-file cost.** A single-chunk file with POSIX attributes and a 22-byte path costs **322 bytes and 49 items** in a snapshot manifest (estimated: 318 bytes, 49 items), and 426.5 bytes in the catalog image.
- **Binding limit.** The item limit binds first, at **342,391** such files. The snapshot's stored-size limit would bind at 833,761.
- **Multi-chunk files** cost **24** more items per chunk (estimated: about 25), so a file of *c* chunks costs 49 + 24(*c* − 1) items.
- **Image limit.** The image budget binds at **629,231** such files (estimated: roughly 600,000).

#### B.2.5 Specification text amended by this batch

- **§8.3.** CBOR-native envelopes, with identical validation obligations (D11).
- **§12.2.** Replaces "validation that the tail is uncommitted", which no check can provide (D14), with the eligibility rule and default quarantine. This resolved a contradiction between that MUST and D14.
- **D10.4** (2026-10-04). Introduction versus reference during replay, and its relation to §10.2 (Q18, Q26); unknown required feature versus unknown operation kind (Q19).
- **D10.6** (2026-10-03 and 2026-10-04). Parent traversal offset versus base footer hint (Q17, Q23); one descriptor per segment (Q16).
- **D12** (2026-10-04). Placement note tying the single descriptor to D10.6.
- **B.2.3, B.2.4** (2026-10-05, from the T32 measurements). Storage-bound terms defined (ΣΔ, *B*₀, *B*_forced); α and *F* defaults confirmed; measured catalog and decoded-CBOR memory; B.2.4's estimates replaced by measurements.

#### B.2.6 Evidence and release gates

These are pending. None is satisfied by this text. Every gate's tests must pass in CI on Ubuntu, and on Windows where the gate says so.

- **G1 Conformance.**
  - v1 golden vectors are regenerated with explained diffs: valid and reject vectors for every CDDL rule and every D11 obligation, in both encodings.
  - Fuzz targets cover commit, manifest, descriptor, and envelope.
- **G2 Replay and recovery.**
  - The replay property test passes against the C5 full-checkpoint oracle.
  - The atomicity test passes.
  - The damage matrix over deltas, images, and snapshot manifests matches D10, item 9.
  - Baseline recovery succeeds with all SQLite images and all earlier manifests deleted.
  - An adoption mismatch blocks the head.
  - GC: a hold added after the last checkpoint survives, and GC refuses when retention state is unresolved.
- **G3 Scaling (§27).** Four workloads, each from 10 to 10,000 commits: a growing catalog, repeated updates, tiny commits, and a large import. Plus a forced-checkpoint run. Report Δ, *B*, *k*, replay operations, and open time. The α/*F* defaults are confirmed or revised from these results.
- **G4 Limits.**
  - Every limit is tested just under and just over its value.
  - A default writer never emits what a default reader rejects (property test).
  - The opt-in path produces its warning and descriptor declaration.
  - Memory per decoded item and catalog memory are measured.
- **G5 Capacity.** The B.2.4 estimates are replaced by measurements from the MOCHI writer.
- **G6 Windows durability.** Documented evidence matching the guarantee to NTFS on the supported versions, plus release-checklist results. Until G6 passes, Windows reports `Unconfirmed`, and truncation there requires `--accept-unconfirmed-durability`.
- **G7 Creation.**
  - Process-crash tests cover every creation step.
  - Two concurrent creators: exactly one wins, and neither deletes the other's temporary file.
  - The `link` fallback is exercised.
  - A locked temporary file can be renamed on Windows.
  - Power-loss tests run separately from process-crash tests (Linux, via device-mapper, where runners allow).
- **G8 Truncation.**
  - A footer whose header was flipped to `0x57` is not eligible.
  - A single-event destruction of both markers is documented, with quarantine shown to exist before truncation.
  - A failure injected at each quarantine step blocks truncation.
  - Each waiver is recorded.
  - Truncation on Windows without the override is refused.
- **G9 Reports.**
  - First-sight `UNKNOWN` freshness gives exit 0.
  - The mixed outcomes each give their expected exit code: `FAIL` with an I/O error, 1; a `FAIL` in a dimension that is not required, 1; a required `UNSUPPORTED` with an I/O error, 3; a required `UNSUPPORTED` with a required `UNKNOWN`, 4; an `UNSUPPORTED` that is not required with a required `DEGRADED`, 2.
  - The timestamp property test passes.


#### B.2.7 Writer lock (D17; decided by the owner 2026-10-06, plan Q54)

Not wire format: nothing is written into an archive. It is recorded here because two implementations that write the same archive exclude each other only if they take the same lock.

- **Mechanism.** The §12.2 step 1 lock is an OS file lock (POSIX `flock`, Windows `LockFileEx`, through a safe API; no new `unsafe` in `mochi-core`) held on a separate **lock file**, never on the archive alone. On Windows a lock on the archive makes every other handle's reads fail, so nothing could verify or read an archive during an append.
- **Name.** `<archive file name>.mochi-lock`, in the archive's directory, both taken from the canonical path (symbolic links, `.` and `..`, and Windows short names resolved).
- **Ownership.** Only the OS lock counts. The lock file is created if missing, never written or truncated, and never removed, by unlock or otherwise; a leftover lock file nobody holds does not block. The OS releases the lock when its holder exits.
- **Aliases.** Every path that canonicalizes to the same directory and name reaches the same lock file; names a filesystem treats as equal (for example case on a case-insensitive filesystem) reach the same lock file as they reach the same archive. Hard links have no canonical name: on Unix a writer also takes `flock` on the archive itself, which excludes them and does not affect readers; **on Windows, writers through two hard-link names of one archive are not excluded** (no stable, safe API gives the file's identity). This is a known limitation, not a guarantee.
- **Creation (D13).** The final name's lock file is taken before the temporary file is published, so a new archive is never visible unlocked. A lock held by another process there is `LOCK_CONFLICT`, and nothing is created.
- **Readers** take no lock. Concurrent reads are correct because a reader interprets only what a valid footer commits (§12.2), not because of the lock.
- **A directory where the lock file cannot be created** (read-only) cannot be appended to; the writer fails before writing.

#### B.2.8 Retention, collection, and compaction (D18; decided by the owner 2026-10-07, plan C9)

The owner chose the model (the first three bullets); the representation is the implementer's, recorded under the owner's delegation and marked as such in the plan.

- **Retention model.** Every commit is a retained snapshot by default. A snapshot can be **expired**; a **legal hold** (a label of 1–255 bytes on one snapshot) keeps a snapshot a root whether or not it is expired, until the hold is released (§16.3: holds override expiration and collection). The roots at head *h* are *h*, every commit not expired, and every held commit (§18.3; 1.0 has no replication pins, restoration roots, or prepared transactions). Only an earlier commit can be expired; the head never is.
- **Collection and compaction write a new archive.** A single-file archive is append-only, so nothing is deleted in place (§18.2, §29.1 #8). `gc apply` and `compact` write the snapshots they keep, one commit each, into a **new archive with a new archive ID**: commit IDs bind footer offsets and cannot survive a rewrite, so freshness anchors start over and first sight of the new archive is `UNKNOWN` (D8). Object and file-version IDs, stored chunk bytes, and promised attributes are preserved (O19); holds and the expiry of kept snapshots carry over.
- **The source is never removed automatically.** It is the retained previous representation (§18.2 step 6) and the quarantine copy (§18.3). Removing it is a separate, explicit user action.
- **[delegated] Retention is manifest state.** A delta manifest lists its commit's retention operations (expire, hold, release; applied after the namespace operations, in order, atomically, each validated against the state the previous left); a snapshot manifest carries the complete state. Like promised attributes, it is not in the catalog image: readers rebuild it from S(*b*) plus the segment's deltas, which is exactly D10 item 10, and D10.7 compares it through the snapshot manifest.
- **[delegated] Recovery-manifest schema 2** adds key 11 (delta: retention operations; snapshot: retention state) and key 12 (delta(0) of a compacted archive only: provenance, i.e. the source archive ID, the source sequence and commit ID behind each new commit, and the snapshots collected). A writer uses schema 2 exactly when a manifest carries either, so every other manifest keeps its schema-1 bytes and one manifest has one encoding; a reader refuses schema 2 without them (`RECORD_INVALID`). Schema: `docs/schemas/recovery-manifest-v2.cddl`.
- **[delegated] Before publication** a rewrite checks §18.2 steps 1–3 against what it wrote: one commit per kept snapshot, each namespace equal to its source snapshot's, attributes, retention, and provenance as written, and (by default) every file version read back and verified. It is published by the D13 mechanism; any failure or crash before that leaves nothing at the destination name.
- **[delegated] Concurrent publication.** A rewrite holds the source's publication lock (D17) throughout, and `gc apply` refuses a plan made at any head other than the current one.
- **Decided [delegated 2026-10-08]: reference scope (plan checklist Q64).** D10.4 lets a delta reference an existing chunk or version, but an image-based open and baseline recovery from S(*b*) see different sets, so a reference to something unreachable at *b* (neither reachable in S(*b*) nor introduced by the deltas after *b*) opens normally and fails baseline recovery. This build's writers never produce one (deduplication and compaction both stay within what a baseline replay sees, forcing a checkpoint where needed). **Readers do not refuse such a commit: opening and reading it work, because the bytes are intact.** `fsck` (deep verification) recovers every replay segment from its baseline and reports a commit that opens from its image but cannot be recovered that way as `REFERENCE_INVALID`, Recoverability `FAIL`; Integrity is unaffected. A commit that does not open at all is reported as the damage it is, not as a reference defect. No wire change.

#### B.2.9 TAR-stream compatibility profile (D19; decided by delegation 2026-10-08, plan C10)

The profile is writer-side only. It adds no frame kind, no schema, no record field, and no required feature: every Core reader reads a TAR-compatible archive as it reads any other, because the extra bytes are ordinary data chunks that nothing references. What changes is what a conforming **writer** emits and what `verify` additionally checks.

1. **Opt-in, fixed at creation** (D4, D12). Descriptor constraint 0 (`tar_compatible`) records the choice; `mochi create --tar-compatible` makes it. Appending with another profile is `PROFILE_CHANGE_UNSUPPORTED` (exit 4). Compaction, collection, and repair write a new archive in **the source's profile** (rule 9).
2. **Writer restrictions.** No deduplication: the writer's default (`Dedup::Auto`) is *off* in this profile and an explicit request for in-archive deduplication is `INVALID_ARGUMENT`. No dictionaries (none exist). No holes: a file is stored as contiguous chunks, each used whole and once, in logical order.
3. **One stream per commit.** A commit with at least one **put** (a file, a directory, a rename's target, a copied entry) emits exactly one complete POSIX pax TAR stream, one member per put in the commit's operation order. A commit with only deletions and retention operations emits no data frame and no stream: generic tools see the **historical** stream, never the latest snapshot (spec 7.2).
4. **Member encoding.** Each member is: an optional pax extended header (ustar typeflag `x`, name `PaxHeader`), the ustar header (typeflag `0` for a file, `5` for a directory, `ustar\0` magic, version `00`, empty user and group names, no device fields, no prefix), the content, and zero padding to a multiple of 512. The stream ends with two zero blocks. Fields:
   - **path.** The ustar name holds the path (directories with a trailing `/`) when it is at most 100 bytes of printable ASCII. Otherwise the pax record `path` holds the exact bytes and the ustar name holds the first 100 bytes of it; when any pax value is not UTF-8 the extended header begins with the record `hdrcharset=BINARY`.
   - **mode, uid, gid.** The promised POSIX attributes (spec D6): mode permission bits (default `0644` for a file and `0755` for a directory when none were recorded), numeric uid and gid (default 0). A uid or gid above `0o7777777` is a pax `uid` or `gid` record.
   - **mtime.** The promised modification time. The ustar field holds the whole seconds when they are in `0..=0o77777777777`, else 0; a pax `mtime` record (`seconds.fraction`, at most nine fractional digits, trailing zeros removed, a leading `-` allowed) is written whenever the time has a fraction or the ustar field cannot hold the seconds. No promised time: ustar field 0 and no record.
   - **size.** The logical length. Above `0o77777777777` (8 GiB − 1) the ustar field is 0 and a pax `size` record holds it. A directory has size 0.
   - A header is limited to 1 MiB of path and pax records; a path that would exceed it is `INVALID_ARGUMENT` before anything is written.
   Windows attributes are not representable and are not written; symbolic links are skipped as in Core.
5. **Where the bytes live.** File-content chunks are exactly Core's: pure file bytes, referenced by extents. Everything else (the previous member's padding, a pax header, a ustar header, the end blocks) is written in **stream-only chunks**: ordinary data objects (one Zstandard frame each, with `Frame_Content_Size` and the frame checksum, O21) with a random object ID, inserted in `objects` and `chunks`, listed in the delta manifest's `chunks` like any introduced chunk, and referenced by no extent. The writer appends data frames in stream order, and every other frame of the archive is skippable, so the concatenation of the decoded data frames in physical order is the sequence of the commits' streams.
6. **A put of a version that is already held.** A rename's target, a re-put of an existing version, and a copied entry whose version the archive already holds cannot reference old chunks in the stream, so the writer emits the content again as stream-only chunks. The version's extents are unchanged. This is the profile's documented space cost. A copied entry whose version the archive does not hold appends its own chunk frames, **in extent order**, and those frames are the member's content; the writer refuses (`INVALID_ARGUMENT`, nothing written) a copied version whose extents are not whole, in-order, not-yet-held chunks used once.
7. **Accounting.** The GC plan reports stream-only chunks (chunks no extent of any version references) in their own total, `stream_framing`, and never as collectable: they belong to the commits' streams, not to a file version. A rewrite regenerates framing for the new archive.
8. **Verification.** For a TAR-compatible archive `verify` decodes the framing chunks at every level and parses each commit's stream with a bounded, read-only pax and ustar parser (no new dependency; limits on header size and on members per stream). It checks that each stream is complete (two zero blocks, nothing after them in the commit's data frames) and that its members equal the commit's puts: path bytes, type, size, mode, uid, gid, and mtime; for a fresh put, that the content chunks are the version's own extent chunks, in order; and at `content_integrity` and deeper, that re-emitted content hashes to the version's file-content hash. A mismatch is a `FAIL` under Integrity with the new code **`PROFILE_VIOLATION`** (a violation: exit 1). Below `content_integrity` the re-emitted content is counted but not hashed, as every content hash is.
9. **Rewrites keep the profile.** `compact`, `gc apply`, and `repair apply` take the profile from the source's descriptor. A TAR-compatible source is copied through the writer, so framing is regenerated.
10. **Documented invocation and claim** (spec 7.2). `zstd -dc ARCHIVE.mochi | tar -x --ignore-zeros -f - -C DIR`, with GNU tar and with bsdtar (libarchive, Windows `tar.exe`); a tool that needs another flag (for example bsdtar's `--options read_concatenated_archives`) gets its exact command in `docs/c10-tar-compat.md`. The claim names only the tools and versions that CI ran (`tar interop` job); it is never "POSIX compatible" and never covers an untested tool.

**Open items recorded here.** (a) Default `Dedup` was `InArchive` with no way to tell a request from the default, so the writer option gained an `Auto` default (rule 2). (b) Sparse files (holes) are not stored by the Core writer either; the rule is a guard for future writers. (c) Whether the desktop app offers the profile at creation is D4's UI question and is unchanged.

## Annex C. Document Lineage **[1.0 integration]**

| Document | Status |
|---|---|
| MOCHI design documentation v1.2 | Superseded by this specification. Retained for history |
| Multi-writer concurrency draft | Folded into 12.5 (Core) and Annex A (post-1.0) |
| Extended capabilities draft (remote, mount, FastCDC, snapshots, search, split) | Annex A |
| Unified v1.2 + extensions draft | Superseded. Its Appendix B frame placeholders are replaced by 8.2 |
| MOCHI Design Document v2.0 | Base text of this specification |
| `docs/implementation-plan.md` | Delivery plan and Definition of Done for 1.0 |
| `AGENTS.md` | Working rules for contributors and coding agents |
