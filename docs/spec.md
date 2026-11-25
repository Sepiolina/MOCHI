# MOCHI 1.0 Specification

**Status:** Design specification, **ratified at 1.0** (the 1.0 flip, plan W20, 2026-10-15; Annex B.2.14). The wire layouts R1–R4 and the suites R5 and R6 were frozen as draft 2 (format freeze, gate G12, 2026-10-12) and are the 1.0 format: a 1.0 archive differs from a draft-2 archive only in descriptor key 3, which is null, and in the payload encoding of its catalog images, which is the Zstandard-compressed form (encoding 2; owner decision of 2026-10-16, Annex B.2.15). The interoperability gates of Section 28 have passed. Implementations MUST NOT claim preservation readiness until the Preservation gates of Section 28 pass (the Preservation profile is not in 1.0).
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
| Container wire generation | 2 | Byte layout; footer magic `MOCHI2\0\0` (Section 8.4) |
| Metadata schema version | 1 (SQLite `user_version`; R4, frozen with draft 2) | SQLite DDL (Section 10) |
| Recovery-manifest encoding version | Schemas 1, 2, 3, selected by the record's content (R3 §6, frozen with draft 2) | Section 11 |
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

**[Amendment, Annex B.2.10 D20: decided; implemented (plan C11); evidence gate G10 met and R5 frozen 2026-10-11]** The profile seals data chunks, catalog images, and manifests with XChaCha20-Poly1305 under one data key per archive, wrapped per passphrase with Argon2id in key envelopes that every commit record lists. Required-feature identifier 1.

Generic Zstandard decoding does not constitute decryption or meaningful file extraction.

### 7.4 Redundancy profile

Adds erasure coding with a fully specified coding suite and shard-placement policy.

**[Amendment, Annex B.2.11 D21: decided; implemented (plan C12); evidence gate G11 met and R6 frozen 2026-10-12]** The profile is a per-archive *recovery record*: each commit writes Cauchy Reed–Solomon parity (GF(2^8)) over the stored bytes it adds, between its commit frame and its footer, at a percentage chosen at creation (descriptor constraint 1). It is not a required feature: readers that do not check parity read the archive unchanged.

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
| Redundancy (7.4) | **Target** | Equivalent of WinRAR's "recovery record." Designed and implemented (Annex B.2.11 D21, plan C12); the erasure-coding ratification item R6 is frozen (2026-10-12, gate G11) with independent vectors and a measured cost (`docs/dev/performance.md#parity`) |
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

### 8.2 Frame registry

The following values are the frozen registry of draft 2 (ratification artifact R1, which is more precise than this table and wins for its topic). They are not a declaration that the v1.2 draft values were already standardized.

| Frame magic | Use |
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
| `0x184D2A59` | Encrypted object envelope (the *sealed object* of Annex B.2.10, D20) |
| `0x184D2A5A`–`0x184D2A5B` | Reserved |
| `0x184D2A5C` | Key envelope |
| `0x184D2A5D` | Compression dictionary |
| `0x184D2A5E` | Optional footer-history accelerator |
| `0x184D2A5F` | Parity object |

Frame IDs alone do not establish that an arbitrary Zstandard stream is a MOCHI archive. Readers MUST validate the MOCHI descriptor and versioned record envelopes before interpreting any record. **[Amendment, ratification R1 decision of 2026-10-11 (delegated)]** "Before interpreting" is the scope of this rule: head discovery, footer and commit validation, the structural walk, and stored-integrity checks remain permitted with a missing or damaged descriptor, as Annex B.2 D12 says. A frame that no verified record references is walked and never interpreted; 1.0 readers do not report unreferenced frames of kinds 1.0 does not write (they are outside every committed hash, so a report about them would be a claim about bytes no commit vouches for).

**[1.0 integration]** *Relationship to earlier drafts.* This registry replaces the placeholder allocations in the unified-draft Appendix B. Notable changes: `0x184D2A56` is now the commit footer (v1.2 had no framed footer); `0x184D2A57`/`58`/`59` are new (descriptor, recovery manifest, encrypted object envelope); `0x184D2A53` (previously proposed for range-reservation records) is reserved with no Core interpretation, because shared-file range reservation is no longer part of Core (Section 12.5); `0x184D2A5E` is now an optional *accelerator* rather than a recovery authority (Section 22). The registry was frozen with R1 at the format freeze (draft 2, gate G12); the format is ratified at 1.0 (Section 28, B.2.14).

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

**[Amendment, Annex B.2 D11: decided; implemented; evidence: the G1 audit is complete in both encodings (R8, B.2.6 G13)]** An envelope MAY be realized in either of two encodings:
- **Deterministic-CBOR records** (Annex B.1 D2): the envelope is a set of required keys in the record body.
- **Other payloads:** a binary envelope header precedes the payload in the same frame.

Both encodings MUST carry every field listed above, and MUST impose identical validation obligations (Annex B.2 D11). Neither encoding may omit or weaken an obligation. The binary layout is given in Annex B.2.2. For recovery manifests, including snapshot manifests, and for catalog images, 1.0 takes the "rejected" branch of the preceding paragraph (Annex B.2 D10, item 11).

### 8.4 Commit footer

The footer is a skippable frame with an 8-byte Zstandard skippable header and a 64-byte MOCHI payload.

The payload is:

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

This footer layout is incompatible with v1.2's. Existing archives MUST NOT be silently relabeled.

**[1.0 integration]** *Why the footer moved into a skippable frame.* The v1.2 layout ended the file with 64 bare trailer bytes, and after each append the previous trailer remained mid-file. Neither is a valid Zstandard frame, so `zstd -dc` would stop with a header error at the first trailer — the v1.2 zero-tool fallback could not have worked as written. Framing every footer fixes this. The footer digest is also now computed over *stored* commit-frame bytes, so it can be checked without decryption keys, unlike v1.2's hash over decrypted metadata.

**[1.0 integration]** *Decided (Annex B.1 D1):* the magic stays `MOCHI2` and the wire generation stays 2, independent of the product version.

**[Amendment, plan O17, recorded in the spec by the R1 decision of 2026-10-11]** A footer MUST follow the commit frame it names: the commit frame's last byte precedes the footer's first byte. A footer whose commit-frame range reaches the footer or beyond is invalid. The format is append-only, so no writer needs a footer before its commit, and tail validation relies on this order.

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

**[Amendment, ratification R4 decision of 2026-10-11 (delegated)]** The table above lists *logical* records. A record MAY live outside the SQLite catalog when another structure is its authority. In 1.0, `dictionaries` and `search_documents` do not exist yet; `parity_groups` is not a record at all, because group membership follows from the commit chain and the descriptor (B.2.11, D21 item 7); key envelopes are their own frames listed by every commit, with key operations and state in the recovery manifests (B.2.10); retention operations and state are in the recovery manifests (D18); and the preserved attributes of `file_versions` (Section 10.4.1, D6) are in the recovery manifests only, so restoring attributes needs the segment's manifests, not only the head's image. The exact catalog DDL, and where each logical record lives, are in ratification artifact R4.

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

Metadata catalogs use SQLite 3 with a page size of 4096 bytes (R4).

Published database images MUST be self-contained and MUST NOT depend on external WAL or journal files.

Private working databases MAY use transactional journaling. Publishing a standalone image is a separate step.

The earlier `journal_mode = OFF` setting MUST NOT be interpreted as a safe general-purpose authoring protocol.

Verification MUST include:
- SQLite structural integrity checks.
- Foreign-key or equivalent relationship validation.
- MOCHI-specific dependency and snapshot checks.

**[Amendment, ratification R4 decision of 2026-10-11 (delegated)]** No canonical page layout or row order is defined, so two writers MAY produce different images of the same catalog. Images are compared by content (for example the snapshot-manifest equivalence check, Annex B.2 D10 item 7), never by bytes, and an independent reader MUST NOT assume byte-identical images.

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

**[Amendment, Annex B.2.10 D20]** The cryptographic profile is defined there: suite, key derivation, envelope formats, nonces, associated data, key identifiers, ciphertext framing, errors, and `docs/ratification/R5-crypto-draft.md` with vectors. Of §14.1's two candidate suites, 1.0 uses neither: it uses XChaCha20-Poly1305 (D3).

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

**[Amendment, Annex B.2.11 D21]** Suite 1, the layout rule, the frame layout, shard integrity, and the reconstruction procedure are defined there and in R6. The cross-check below against a catalog `parity_groups` record is **replaced**: every parity frame self-describes its suite, `k`, `m`, and range, every frame of a commit must agree with the others and with the commit chain and descriptor, and no catalog record exists (D21 item 7), so parity survives the loss of every catalog.

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

**[Amendment, Annex B.2.11 D21]** The 1.0 coverage policy is the table in D21 item 3: content objects, metadata, recovery manifests, commit records, key envelopes, and the descriptor are covered by the parity of the commit that wrote them; each footer by the next commit's; the head's footer and parity frames themselves by none; no dictionaries are written. The configuration is in the descriptor and in every parity frame, outside every protected range.

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
- Expected head (the freshness anchor, Annex B.1 D8).
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

The following is a real report of schema version 1, abridged only in its two wall-clock times and the tool version, which are replaced by fixed values: `mochi verify --json` of the golden archive `fixtures/golden/r8/core-corrupt-chunk-older.mochi` (a data object of an older commit is damaged), in the field order of the report type. `crates/mochi-cli/tests/r7_json_schemas.rs` regenerates it and compares, so it cannot drift from the code. Its JSON Schema is `docs/schemas/json/report-v1.schema.json`, the field meanings are in `docs/report-schema-v1.md`, and the report schema and the error codes are frozen as ratification artifact R7 (Annex B.2.6, gate G15).

```json
{
  "schema_version": 1,
  "archive_id": "031d11be423d1e3fcbbf47098791541cbdc98abe3dc6a77726d38b31d43ff9e1",
  "checked_commit": "ba1041d18a7bd794d805b6222e1b8a7642b06e23e1ebfc10231007b5051a18d3",
  "expected_head": null,
  "freshness_anchor": "none",
  "tool": {
    "name": "mochi-core",
    "version": "1.0.0",
    "spec_revision": "2.0",
    "wire_generation": 2,
    "format_status": "1.0 (ratified format)"
  },
  "started_at": "2026-10-14T09:00:00.000000000Z",
  "completed_at": "2026-10-14T09:00:00.250000000Z",
  "level": "restoration",
  "scope": "full retained history: head, every commit record and footer, control objects; every data object; every retained file version reassembled. Durability is not observable from the archive's bytes.",
  "coverage": {
    "expected_objects": 21,
    "checked_objects": 20,
    "expected_bytes": 1278,
    "checked_bytes": 1201,
    "evidence_age_seconds": null
  },
  "skipped": [],
  "findings": [
    {
      "code": "STORED_INTEGRITY_FAILED",
      "severity": "error",
      "message": "object 576027a96846deb31a49a3b73e8260e099a5382e0d58b224ffc446cf20b951de at offset 56: object 576027a96846deb31a49a3b73e8260e099a5382e0d58b224ffc446cf20b951de: stored-object hash mismatch"
    },
    {
      "code": "STORED_INTEGRITY_FAILED",
      "severity": "error",
      "message": "restoration check: object 576027a96846deb31a49a3b73e8260e099a5382e0d58b224ffc446cf20b951de: stored-object hash mismatch"
    }
  ],
  "repair_actions": [],
  "dimensions": {
    "durability": "UNKNOWN",
    "integrity": "FAIL",
    "recoverability": "FAIL",
    "searchability": "UNSUPPORTED",
    "freshness": "UNKNOWN",
    "retention_compliance": "FAIL",
    "key_availability": "PASS"
  },
  "overall_status": "FAIL",
  "policy": {
    "required": [
      "integrity",
      "recoverability"
    ],
    "freshness": {
      "expected_head_supplied": false,
      "archive_in_local_history": false,
      "requested": false
    }
  },
  "policy_result": "FAIL",
  "operational_error": false,
  "exit_code": 1
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
5. Retrieve verified alternate objects from independent copies. **[Amendment, Annex B.2.12 D22]** In this build, copies are byte copies of the same archive given to repair explicitly. They are consulted after parity (step 6) and before steps 1–4 read the result. A copy supplies bytes only, and the source supplies the trust.
6. Reconstruct missing objects using available parity. **[Amendment, Annex B.2.11 D21]** In this build repair takes this step first, rebuilding into an in-memory overlay that steps 1–4 then read; damage beyond a group's parity is reported (`PARITY_CAPACITY_EXCEEDED`) and never rebuilt.
7. Perform bounded frame scanning and salvage if structured recovery is insufficient. **[Amendment, Annex B.2.13 D23]** Only on explicit request. A commit found by scanning is used only if its publication is confirmed (its own footer, or a confirmed later commit's parent link). Any repair that uses this step is partial.

Footer history is an accelerator, not a sole source of truth.

**[1.0 integration]** *Proposed default (from v1.2).* When present, the `0x184D2A5E` accelerator holds the most recent N footers (proposed default N = 16) with their original offsets, so step 2 can usually avoid a backward scan. Each entry MUST still be validated against the commit frame it names. An optional `<archive>.mochi.trailer-backup` sidecar (atomic write-then-rename after each commit) remains permitted and is never authoritative over the archive itself.

**[Amendment, decided by delegation 2026-10-12, plan K16]** MOCHI 1.0 writes neither the accelerator nor the sidecar. The `0x184D2A5E` magic stays registered (R1); a 1.0 reader walks such a frame and never interprets it. Step 2 is the forward and backward walk. Both mechanisms are 1.x options.

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

The following command interface is the 1.0 command surface; the scope of each command is stated under 23.2.

| Command | Purpose |
|---|---|
| `mochi create` | Create an archive |
| `mochi append` | Publish a new commit |
| `mochi get` | Extract selected files |
| `mochi list` | List a snapshot namespace |
| `mochi snapshot` | Inspect or retain snapshots |
| `mochi search` | Search names, metadata, or content |
| `mochi find` | Find entry names across the archives under some folders (read-only; B.2.18) |
| `mochi comment` | Show the archive comment of a version (read-only; B.2.19) |
| `mochi verify` | Perform read-only verification |
| `mochi fsck` | Perform deep read-only consistency analysis |
| `mochi health` | Summarize policy and evidence |
| `mochi inventory reconcile` | Compare expected and observed storage |
| `mochi restore-test` | Restore into an isolated test destination |
| `mochi repair plan` | Produce a proposed repair plan |
| `mochi repair apply` | Apply an explicitly approved repair |
| `mochi checkpoint` | Create a verified metadata checkpoint |
| `mochi compact` | Create a compacted representation |
| `mochi migrate` | Convert a draft-2 archive into a new 1.0 archive (Section 26, B.2.14) |
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
mochi find "*.pdf" ~/Archives --json
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

**[1.0 integration]** *1.0 command scope.* In 1.0: `create`, `append`, `get`, `list`, `snapshot`, `search`, `find` (B.2.18, added 2026-10-24), `comment` (B.2.19, added 2026-11-14), `verify`, `fsck`, `health` (local evidence only), `restore-test`, `repair plan`, `repair apply`, `checkpoint`, `compact`, `migrate`, `gc plan`, `gc apply`, `rekey` (with the Encrypted profile), and `dump-index`. Post-1.0: `inventory reconcile` (Preservation profile), `split` / `join` (Segmented profile), and `mount`. A 1.0 build invoked with a post-1.0 command MUST exit with code 4, not 3. The v1.2 form `mochi verify --repair` is removed; verification and repair are separate operations (Sections 20.2, 22.2).

**[1.0 integration]** *Health from local evidence [delegated 2026-10-08; plan K5, W4].* `mochi health` runs no check. It summarizes the evidence this client recorded when it last ran `verify`, `fsck`, `restore-test`, and a complete `repair apply`, judged against a policy for the archive's current head, and reports the Section 20.4 values per dimension. The policy file is JSON (`mochi-health-policy-v1`: the required dimensions and a maximum age in days per kind of evidence, `verify` and `restore_test`); the `.yaml` file in the Section 23.1 example is illustrative, not normative. Per dimension and kind of evidence: no evidence is `UNKNOWN`; a recorded `FAIL` stays `FAIL` (an append-only archive does not heal damaged bytes, and no policy hides a failure); evidence about an earlier head is `UNKNOWN`; evidence older than the policy permits is `OVERDUE`; otherwise the recorded status. Freshness is judged as `verify` judges it. Durability, key availability, searchability, and retention compliance are not established by local evidence and are never `PASS` in 1.0. **[Amendment, B.2.22 D32, owner 2026-11-23]** Retention compliance is judged from recorded `verify`, `fsck`, and `repair apply` evidence like integrity, and can be `PASS` (a local check, no inventory); durability comes from the commit records clients write and is `DEGRADED` at best, never `PASS`; key availability and searchability are unchanged. Evidence that cannot be read prevents any `PASS`. The rules and the log format are in `docs/cli.md`, rule 13.

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
| Missing shared dictionary | Identify all affected files and exercise recovery. **[Amendment, delegated 2026-10-12, plan K17]** 1.0 writes no dictionaries (plan O21). A dictionary-dependent chunk is reported as `UNSUPPORTED` (`UNSUPPORTED_FEATURE`, exit 4) with every affected file version listed, never as partial content, and repair refuses rather than omits it (plan K4). **[Clarified, delegated 2026-10-12, W10]** This applies to a dependency that resolves to an intact catalog object; a dependency naming an object the catalog does not hold is a dangling reference (§5.2), reported `REFERENCE_INVALID` (`FAIL`) |
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

**[1.0 integration]** *The draft-to-1.0 boundary (Annex B.2.14, D24).* The archives of the pre-1.0 drafts are the only legacy this release meets. A 1.0 build writes a ratified archive (descriptor key 3 null), refuses a draft archive in every command but `mochi migrate`, and migrates only the last draft (draft 2) to a new 1.0 archive through steps 1–6 and 8 above; steps 7 and 9 stay with the user, and the source is always kept. Older drafts are refused without a conversion path.

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

Until the interoperability gates pass, implementations MUST describe support as experimental or draft-compatible. Until the preservation gates pass, they MUST NOT claim preservation readiness.

**Status 2026-10-15 (the 1.0 flip).** R5 and R6 are frozen (G10, G11). R1, R2, R3, and R4 were frozen together as draft 2 (G12). The golden set R8 (G13), the independent results R9 (G14), and R7 (G15) are met. The 1.0 flip (W20, D24) is made: a 1.0 writer writes null in descriptor key 3, every command but `migrate` refuses a draft archive, and R8 is regenerated in the 1.0 format. A 1.0 build therefore describes its format as ratified (`FORMAT_STATUS`: `1.0 (ratified format)`). It still does not claim preservation readiness: the Preservation profile is not in 1.0, and no 1.0 build reports `PRESERVED`.

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

Each decision blocks the listed work until recorded in this annex. D1–D9 and D17–D23 are recorded in B.1 (D17–D23 moved there at the format freeze, 2026-10-12, because each is implemented and its gates in B.2.6 pass or it has none). D10, D11, D12, D14, and D15 moved to B.1 on the owner's review [owner, 2026-10-15] (B.3 item 8); their text stays in B.2.1. D13, D16, and D24 stay in B.2: D13's Windows durability is settled by the G6 policy (B.3 item 1), D16 is out of 1.0, and D24 is implemented at the 1.0 flip (B.2.14).

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
| D17 | **Decided by the owner on 2026-10-06 (plan Q54); not wire format.** The §12.2 step 1 publication lock is an OS lock on a separate lock file, not on the archive (on Windows a lock on the archive blocks every other handle's reads). The lock file's existence never means ownership, and normal unlock never removes it. Naming and alias handling: see B.2.7. **[Amendment, B.2.17 D27, owner 2026-10-20]** The lock file is gone: the lock is an OS lock on the archive itself (Unix `flock`; Windows one byte at offset 2^62), see B.2.17 | — | C6 Windows reads during append |
| D18 | **Decided by the owner on 2026-10-07 (plan C9); wire format: recovery-manifest schema 2.** Retention is expiry plus legal holds; collection and compaction write a new archive with a new archive ID and provenance; the source is never removed automatically. See B.2.8 | — | C9; D4 (desktop retention UI); C14 `snapshot`, `gc`, `compact` |

| D19 | **Decided [delegated 2026-10-08]; no wire change.** The TAR-stream compatibility profile (spec 7.2) is written by emitting, per commit with at least one put, one complete POSIX pax TAR stream whose non-content bytes live in ordinary data chunks that no extent references (*stream-only chunks*). See B.2.9 | — | C10; C9 (GC accounting, rewrites keep the profile); C7 (`PROFILE_VIOLATION`); desktop "create" dialog (D4) |
| D21 | **Decided [delegated 2026-10-12]; wire format: parity frame v0 (binary-envelope payload encoding 1), descriptor constraint 1; no required feature.** The Redundancy profile (spec 7.4, 15) writes, per commit, Cauchy Reed–Solomon parity over GF(2^8) of the stored bytes from the parent's footer to the end of the commit frame, between the commit frame and its footer, at a percentage fixed at creation. See B.2.11 | — | C12; R6; C8 step 6; desktop "create" dialog (D5) |
| D20 | **Decided [delegated 2026-10-09]; wire format: key envelope v0, sealed object v0, commit record schema 2, recovery-manifest schema 3, required feature 1.** The Encrypted profile (spec 7.3, 14) seals data chunks, catalog images, and manifests with XChaCha20-Poly1305 under one random data key per archive, wrapped per passphrase (Argon2id) in key envelopes that every commit record lists. See B.2.10 | — | C11 and `rekey`; R5; desktop passphrase flows |
| D22 | **Decided [delegated 2026-10-12]; no wire change.** Ladder step 5 (§22): repair takes verified frames from **byte copies of the same archive** given explicitly (`--copy`). Trust comes from the source and bytes may come from a copy: a frame is donated only inside the commits the copy agrees on with the source, only where the source's frame fails its check and the copy's passes, and the survey verifies it again. See B.2.12 | — | C8 step 5; repair plan v2 |
| D23 | **Decided [delegated 2026-10-12], implemented (W12); no wire change; new error code `ARCHIVE_ID_AMBIGUOUS`, new limit `max-salvage-candidates`.** Ladder step 7 (§22): an explicit, bounded salvage scan. Scanned commits are used only when publication is confirmed; contradictory or self-inconsistent candidates are refused; any repair using it is partial. See B.2.13 | — | C8 step 7; D12 "explicit salvage" |
| D24 | **Decided [delegated 2026-10-12]; implemented at the 1.0 flip (W20, 2026-10-15).** The draft-to-1.0 boundary (§26; plan release gate 4): the draft identifier becomes 2 at the format freeze (gate G12). 1.0 writes `null`, refuses drafts with exit 4, and migrates only the last draft through `mochi migrate`. See B.2.14 | — | Format freeze; 1.0 release |
| D25 | **Decided [owner, 2026-10-16]; wire format: binary-envelope payload encoding 2 (Zstandard-compressed catalog image); amends frozen artifacts R1 and R2 before any 1.0 release.** A 1.0 writer stores every catalog image as one Zstandard frame (level 9, `Frame_Content_Size`, checksum) instead of the bare SQLite image; readers accept encoding 0 (draft 2) and 2. See B.2.15 | — | Catalog image writers and readers; `mochi migrate` |
| D26 | **Decided [owner, 2026-10-19]; not wire format.** Amends the writer lock (D17, B.2.7): a newly created lock file is hidden on Windows, and on Unix a bounded sweep removes orphaned lock files under a lock-then-verify rule that every locker keeps. Windows has no sweep (no stable safe file identity). See B.2.16. **[Amendment, B.2.17 D27, owner 2026-10-20]** Superseded by D27 in its hidden-file and sweep rules (B.2.16 items 1, 3, 4, 5, 7) | — | `storage/os.rs`; Q54 tests |
| D28 | **Decided [owner, 2026-10-24]; not wire format.** `mochi find PATTERN FOLDER…` searches entry names across every `.mochi` archive under some folders, read-only, through the engine the desktop app's Find uses (`mochi_core::find`). Links are not followed; Encrypted archives are reported as not searched (no passphrase); exit 0 when everything found was searched, 2 when anything was left out. See B.2.18 | — | `mochi_core::find`; C14 `find`; desktop Find |
| D30 | **Decided [owner, 2026-11-14]; not wire format.** Windows directory durability on creation becomes the user's choice: policy B (unconfirmed, exit 2) stays the default; a user may choose to count a successful folder flush on a local NTFS volume as confirmed (`--trust-ntfs-directory-flush`, desktop Settings). Network locations and other filesystems stay unconfirmed. Amends B.3 item 1 and plan O12. See B.2.20 | — | `storage/os.rs`; C14 global switch; desktop Settings |
| D31 | **Decided [owner request, 2026-11-17]; not wire format.** The desktop app's Find searches for a list of names at once (typed or pasted, split by line breaks, commas, semicolons or tabs), with *Match case*, *Whole name only*, *Files only / Folders only* and *Search subfolders*, and retrieves the matches the user ticks from every archive they are in, through the restore engine. `mochi find PATTERN FOLDER...` and its v1 JSON are unchanged; `mochi find` also takes `--name`, `--names-from` and the options (JSON `mochi-cli-find-v2`). No cap on matches listed or retrieved. See B.2.21 | — | `mochi_core::find` (`TermSet`, `MatchOptions`, `Limits::subfolders`); desktop Find, `retrieve.rs` |
| D32 | **Decided [owner, 2026-11-23]; not wire format.** Freshness, durability, and retention compliance from local evidence. The local head history and evidence log move to `mochi-core` and are shared by the CLI and the desktop app (which now keeps the history, reversing B.3 item 5); updates of `heads.json` are serialized by a lock file (plan K5). A client records each commit it publishes as health evidence (evidence schema 2), from which `health` reports durability `DEGRADED` at best, never `PASS`. `verify` reports retention compliance from a local check, `PASS` only at the restoration level. The desktop app does not show searchability while it is `UNSUPPORTED` and not required. See B.2.22 | — | `storage/os/history.rs`; `health`; `verify`; desktop Test, Health, Settings |
| D33 | **Decided [owner, delegated to the assistant, 2026-11-24]; not wire format.** The archive-creation options a WinRAR-class archiver offers, decided one by one: two built now (wait for another writer, opt-in; keep the source's access time on Linux, always); delete files after archiving built in the desktop app and the CLI (to the Bin, only after read-back verification); background archiving after a Windows priority call is reviewed; modification time stays always stored and is already high precision; lock archive, creation and access times, ACLs, alternate data streams, and links stay out of 1.0 (format changes, each a 1.x candidate, symbolic links first); self-extracting `.exe` archives are refused. See B.2.23 | — | `publish::WriterOptions::lock_wait`; `storage/os/source.rs`; CLI `--wait-for-lock`; desktop Settings |
| D29 | **Decided [owner, 2026-11-14]; wire format: commit record schema 4 (key 13), required feature 3.** The archive comment (plan O32 option b): a commit that carries a comment uses schema 4 and lists feature 3; the comment is the archive's comment as of that commit, repeated until changed; an Encrypted archive has none; older builds refuse such a commit with exit 4. Adds `mochi comment`. Schema 3 and feature 2 stay unassigned (frozen vectors use them as unknown values). See B.2.19 | — | `commit.rs`; writer and rewrites; C14 `comment`, `--comment`; desktop Info |
| D27 | **Decided [owner, 2026-10-20]; not wire format.** The writer lock is an OS lock on the archive file itself, not on a separate lock file: Unix `flock`, Windows `LockFileEx` on one byte at offset 2^62 (readers keep working). No `.mochi-lock` file is created any more; a writer removes the empty ones earlier builds left behind, on Unix and on Windows, under a lock taken the way those builds took it. Amends B.2.7 and supersedes B.2.16's hidden-file and sweep rules. Mixed old and new builds writing one archive at the same time are not excluded on Windows. See B.2.17 | — | `storage/os.rs`; `mochi-sys`; Q54 tests |

Status of the open questions from the original v1.2 review: chunk-hash ordering — resolved (9.2); concurrent writers — resolved for Core (12.5); `content_index` schema — superseded, DDL is a ratification artifact (10.1); version-byte mapping — reframed by Section 26 and D1; Windows attribute defaults — D6.

### B.1 Recorded decisions (2026-09-30)

Each entry below closes the decision above with the same ID. Rationale and consequences are in `docs/dev/decisions.md` §9.

| ID | Decision |
|---|---|
| D1 | **Keep `MOCHI2`.** The wire generation stays 2, independent of the product version. No collision with any v1.2-layout file is possible, whether or not such files exist outside development. |
| D2 | **Deterministic CBOR** (RFC 8949 §4.2.1 core deterministic encoding), restricted to: unsigned and negative integers, byte strings, UTF-8 text strings, arrays, and maps with unsigned-integer keys in canonical order. No floats, tags, simple values other than `false`/`true`/`null`, indefinite lengths, or duplicate keys. Schemas are written in CDDL (RFC 8610) and versioned. A reader MUST reject any input that does not re-encode to identical bytes. The **same encoding is used for the canonical commit body** (Section 9.2, Commit ID; ratification item R3), so the format has one canonicalization rule. |
| D3 | **One suite for the first Encrypted-profile identifier: XChaCha20-Poly1305 with independently random 192-bit nonces; Argon2id for passphrase key derivation, with its parameters recorded per key envelope. 1.0 unlocks by passphrase only.** Random 192-bit nonces satisfy Section 14.2 across retries, interrupted writes, restored backups, and concurrent writers without counter state. Key files and public-key recipients are post-1.0 and would use new profile or envelope identifiers. |
| D4 | **TAR compatibility is opt-in, per archive, fixed at creation.** The default profile keeps dedup, dictionaries, and fragmentation. |
| D5 | **Split volumes stay in 1.x.** The Segmented profile and R10 are not part of 1.0. |
| D6 | **Section 10.4.1, made concrete.** POSIX: permission bits, numeric uid/gid, mtime with nanoseconds; setuid/setgid restored only on explicit request; uid/gid restored only with privilege, otherwise reported as an exception. Windows: `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE` attributes and mtime. Entry types: regular file, directory. **Symbolic links are deferred to 1.x [owner, 2026-10-15]** (B.3 item 2): the frozen draft-2 format reserves manifest kind 2 and refuses it, the 1.0 writer skips links and reports each skip (exit 2), and no format byte changes. The earlier design for links (target stored as bytes; restored as a link, never written through; reported as an exception where the platform cannot create it) stays the design for 1.x. Hard links are stored as separate files. Not promised: extended attributes, ACLs, alternate data streams, owner names, atime, ctime, creation time. |
| D7 | **A read-only verifier in Python, written from this specification alone**, sharing no code with the reference implementation. Section 28's interoperability claim for 1.0 is scoped to the profiles 1.0 ships (Core, TAR-compatibility, Encrypted, Redundancy). **[Amendment, delegated 2026-10-12, plan K22, K23]** It is written by a session started cold on a checkout that does not contain the Rust code (`docs/`, `fixtures/golden/`, `interop/` only), reading only this specification, the ratification artifacts, the schemas, and vectors and fixtures as data. It lives in `interop/pyverify/`. Agreement with the reference implementation is exact equality of an interop summary over the R8 golden set (gate G14). Its independence is procedural (a separate session, from the specification alone), not organizational; it is not described as a third-party implementation [owner, 2026-10-15]. |
| D8 | **Layered, and the source is always reported.** With no anchor, freshness is `UNKNOWN`. A user-supplied expected head gives `PASS`/`FAIL`. Otherwise a client MAY keep the last-seen head per archive ID as a local anchor, giving `FAIL` on rollback or substitution. First sight is `UNKNOWN`, never `PASS`. The report names the anchor used. **In the 1.0 desktop application the client keeps no head history, no evidence log, and offers no restore-test, so its freshness is always `UNKNOWN`, never `PASS` [owner, 2026-10-15]** (B.3 item 5); the CLI keeps the local history. **[Amendment, B.2.22 D32, owner 2026-11-23]** The desktop app now keeps the same local history as the CLI, shared with it (on by default, a Settings opt-out), so its freshness is judged as the CLI's. |
| D9 | **The desktop application reads ZIP, 7z, RAR, and `.tar.gz`/`.tar` (browse, extract, test) and never writes them.** Outside this specification; see the plan, phase D8. The 1.0 RAR claim is narrowed **[owner, 2026-10-15]** (B.3 item 6): RAR is read-only, has no hostile-input fixtures yet, and multi-volume RAR is read but not tested as a complete set. |
| D10 | **Metadata deltas and checkpoints:** the delta is the recovery manifest; checkpoints bind delta, image, and snapshot manifest; bounded, deterministic replay; one-frame manifests, hence the B.2.4 capacity ceiling. Details in B.2.1. Moved here on the owner's review [owner, 2026-10-15]. |
| D11 | **Record envelopes:** two encodings (deterministic CBOR, binary envelope) with one validation obligation per §8.3 field. Details in B.2.1. Moved here on the owner's review [owner, 2026-10-15]. |
| D12 | **Archive descriptor:** one frame at offset 0, written once, creation-time facts only; a repaired archive gets its own accurate descriptor (K20 amendment). Details in B.2.1. Moved here on the owner's review [owner, 2026-10-15]. |
| D14 | **Tail truncation:** explicit only, eligible tails only, quarantine first by default, every waiver recorded. Details in B.2.1. Moved here on the owner's review [owner, 2026-10-15]. |
| D15 | **Reports, exit codes, and timestamps:** three results (evidence, policy, exit code), the exit precedence 1, 3, 4, 2, 0, RFC 3339 UTC timestamps. Details in B.2.1. Moved here on the owner's review [owner, 2026-10-15]. |
| D17 | **Writer lock** (not wire format): an OS lock on a separate lock file; details in B.2.7. Moved here at the format freeze. **[Amendment, B.2.17 D27, owner 2026-10-20]** Superseded in mechanism: the lock is on the archive itself, see B.2.17 (D27). |
| D18 | **Retention, collection, and compaction** (recovery-manifest schema 2): details in B.2.8. Moved here at the format freeze. |
| D19 | **TAR-stream compatibility profile** (no wire change): stream-only chunks; details in B.2.9. Moved here at the format freeze. |
| D20 | **Encrypted profile** (key envelope v0, sealed object v0, commit schema 2, manifest schema 3, feature 1): details in B.2.10; gate G10 met 2026-10-11, R5 frozen. Moved here at the format freeze. |
| D21 | **Redundancy profile** (parity frame v0, descriptor constraint 1): details in B.2.11; gate G11 met 2026-10-12, R6 frozen. Moved here at the format freeze. |
| D22 | **Independent copies, ladder step 5** (no wire change): details in B.2.12. Moved here at the format freeze. |
| D23 | **Bounded salvage, ladder step 7** (no wire change): details in B.2.13. Moved here at the format freeze. |

### B.2 Wire batch (decided 2026-10-01)

**Status.** This annex separates three things, and the text below never treats one as another:
- **Semantic decisions** (B.2.1) and **wire layouts** (B.2.2) are *decided*. Their schemas are frozen with R3 as part of draft 2 (G12, 2026-10-12).
- **Implementation.** The wire batch (D10 to D15) is implemented; D13's Windows durability report is fixed at `Unconfirmed` by the G6 policy (B.3 item 1), and D16's opt-in path is out of 1.0.
- **Evidence and release gates** (B.2.6): G10, G11, and G12 are met; the others are pending.

An entry moves to B.1 only when its implementation tasks are complete and its gates pass. Writing a decision here is not evidence that it is implemented or tested.

#### B.2.1 Semantic decisions

**D10 — Metadata deltas and checkpoints (plan O26).**
1. **The delta is the recovery manifest.** A commit's metadata delta is its recovery manifest (kind 0, "delta"). There is no other delta encoding.
2. **Checkpoint commits bind three objects.** Every checkpoint commit references, by stored-object hash:
   - its delta manifest;
   - a complete catalog image;
   - a complete snapshot manifest (kind 1).

   Commit 0 is always a checkpoint.
3. **Authoritative state.** Every mutation of state that a reader, verifier, or GC relies on is a manifest operation, and a snapshot manifest covers all of that state. Derived, rebuildable state (search documents, §19.4) is excluded. Roots, holds, retention changes, dictionaries, key-envelope references, and parity membership join both manifest kinds in the phases that introduce them, each as a new manifest schema version. **[Amendment, D21]** Parity membership turned out not to be state: it is a function of the commit chain and the descriptor (B.2.11 item 7), so no manifest carries it.
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
   - **A damaged image with an intact snapshot manifest:** reads rebuild the catalog from the snapshot manifest. **[Clarified, W18 (R9), 2026-10-13; no byte changes]** "Damaged" means the image fails its stored-object hash (or its AEAD tag, in the Encrypted profile). An image whose bytes are authentic but which then fails the envelope, schema, or relationship checks of R4 §3 is not damage but a writer's fault: when it is the head's base image the head is refused (`CATALOG_INVALID`, or the code of the failed check) and no fallback applies.
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
- **Opaque payloads** (catalog images and parity frames; dictionaries later): the binary envelope header defined in B.2.2. A parity frame is referenced by no record, so its integrity scope is its own digest (B.2.11 item 6).

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
  - *Repair* may write a new archive with a descriptor that is labelled as reconstructed. **[Amendment, delegated 2026-10-12, plan K20]** No descriptor is labelled or written as reconstructed. A repaired archive is a new archive with its own, accurate descriptor. The reconstruction is recorded in the repair plan and report, which is where every repair fact lives (plan C8). Without the source descriptor, the output profile follows B.2.13 item 8.
- **Placement.** A descriptor frame at any offset other than 0 is invalid. Because there is exactly one descriptor, at offset 0, two commits of one archive that reference different descriptors cannot both be valid; D10.6 checks this for every commit a replay depends on.

**D13 — Creation (plan O12; §12.2 step 9).**
- **Mechanism.** The first commit is written to an exclusively created, locked temporary file in the destination directory, synced, published at the final name **without replacing an existing file**, and the directory is then flushed.
- **Overwrite** requires an explicit request and a separate code path.
- **Cleanup** removes only temporary files whose lock it can acquire.
- **Durability claims are limited to documented mechanisms.**
  - *Linux (local ext4):* confirmed when both the file fsync and the directory fsync succeed.
  - *Windows:* the creating commit's directory durability is reported `Unconfirmed` in 1.0, by policy **[owner, 2026-10-15]**. The shipped mechanism is documented only in part, and what makes the created name survive a crash on NTFS (the flush of the directory handle) is not documented by Microsoft. Gate G6 (B.2.6) records the policy and the evidence; B.3 item 1.
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
- **What blocks truncation.** Any quarantine failure blocks truncation unless the user passes `--no-quarantine`. A directory flush that is not `Confirmed` (which on Windows is always the case in 1.0, by the G6 policy **[owner, 2026-10-15]**) blocks truncation unless the user passes `--accept-unconfirmed-durability`.
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
| Commit record v2, manifest v3, key envelope v0 (Encrypted profile only; D20) | `0x184D2A51`, `0x184D2A58`, `0x184D2A5C` | `commit-record-v2.cddl`, `recovery-manifest-v3.cddl`, `key-envelope-v0.cddl` |
| Sealed object v0 (Encrypted profile only; D20) | `0x184D2A59` | Binary layout in B.2.10 item 5 |
| Parity frame v0 (Redundancy profile only; D21) | `0x184D2A5F` | Binary envelope v0, payload encoding 1, + layout in B.2.11 item 5 (R6) |
| Catalog image | `0x184D2A50` | Binary envelope v0 (below), payload encoding 2: one Zstandard frame whose decoded bytes are a SQLite image (B.2.15). Draft 2 wrote encoding 0, the bare SQLite image |
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
| 3 | Draft identifier: **null** in a ratified 1.0 archive, which is all a 1.0 writer writes. A number is a pre-1.0 draft: 2 is the layout frozen at G12 (D24), read only by `mochi migrate`; every other command refuses it, and any other number, with `UNSUPPORTED_FEATURE` (exit 4), naming the draft. Draft 1, the pre-freeze batch, is refused without the migrate hint |
| 4 | Required features |
| 5 | Constraints `{0: TAR-compatible, ? 1: {suite, percentage}}`; key 1 is the Redundancy profile (D21, B.2.11) and is absent otherwise |
| 6 (optional) | Declared limits; present only on opt-in (B.2.3) |

Archives from before this batch have no descriptor and are rejected as legacy drafts (§26).

**Binary envelope v0** (at the start of the skippable payload; integers unsigned little-endian; every byte defined):

| Offset | Size | Field | Rule |
|---|---|---|---|
| 0 | 4 | Header length | = 80 + 8·n |
| 4 | 2 | Envelope version | = 0 |
| 6 | 2 | Record schema version | For a catalog image: equals the image's SQLite `user_version` |
| 8 | 4 | Payload encoding | 0 = SQLite 3 database image (payload starts `SQLite format 3\0`; written by draft 2, read by 1.0); 1 = parity shard (payload starts `MOCHI2-PARITY\0\0\0`; D21); 2 = Zstandard-compressed SQLite 3 database image (payload is exactly one Zstandard frame, starting `28 B5 2F FD`; B.2.15); other values unregistered. A catalog-image frame `0x184D2A50` carries 2 (a 1.0 writer) or 0 (draft 2, and a reader accepts it); a parity frame `0x184D2A5F` carries 1 |
| 12 | 4 | n = required-feature count | ≤ 64 |
| 16 | 8 | Payload length | = frame payload length − header length |
| 24 | 32 | Archive ID | |
| 56 | 8 | Commit sequence | |
| 64 | 16 | Transaction ID | |
| 80 | 8·n | Required features | u64 values, strictly increasing |

**[Amendment, ratification R2 decision of 2026-10-11 (delegated)]** n ≤ 64 is a wire rule of envelope version 0, not a reader limit: raising a reader's required-feature limit does not admit a longer v0 header, and a record that needs more than 64 features needs envelope version 1.

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
| Decoded | Image (payload encoding 2) | = *S* − 592 = 268,434,864 B | The image payload limit above, applied to the Zstandard frame's *declared* `Frame_Content_Size` before any output is allocated, and to what the decoder produces (B.2.15). Encoding 0 images are their stored size |
| Decoded | CBOR record | = stored | Stored uncompressed |
| Resource | CBOR items / depth | 16 Mi / 64 | Unchanged |
| Resource | Required features per record | 64 | New |
| Resource | Salvage candidates per scan (`max-salvage-candidates`) | 1,048,576 (2²⁰) | Ladder step 7 only (B.2.13 item 2). A reader limit like the others, raised or lowered by `--limit`; reported as `LIMIT_NOT_DEFAULT` when changed. At the limit the scan stops and the step is `incomplete` |
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
- **Defaults** are α = 1 and *F* = 1 MiB, both configurable. **Confirmed by G3** (T32, 2026-10-05; `docs/dev/performance.md#scaling`): over four workloads to 10,000 commits, every replay stayed within its limit, *k* stayed below 1, and neither α = 1/2 nor α = 2 improved one cost without a larger loss in the other. There is no delta-count cap.
- **Forced checkpoints** (the `checkpoint` command) reset the base.

**Storage bound (conditional).**
- **Assumption.** Suppose *B*ᵢ ≤ *B*ᵢ₋₁ + *k*·Δᵢ holds uniformly across intervals and workloads. Here *B* counts both representations, and *k* is measured per interval.
- **Bound.** Then total stored metadata ≤ (1 + 1/α + *k*)·ΣΔ + *B*₀ + Σ*B*_forced. Here ΣΔ is the delta-manifest, commit-record, and footer bytes of every commit after commit 0, checkpoint commits' own included; *B*₀ is all of commit 0's metadata (descriptor, delta manifest, commit record, footer, image, and snapshot manifest); and *B*_forced is each forced checkpoint's image and snapshot manifest. (Clarified 2026-10-05 from the T32 measurements: with *B*₀ read as image and snapshot only, the bound fails whenever commit 0 introduces much state, because commit 0's own delta manifest is then as large as its snapshot.)
- **When it is linear.** The bound is linear in ΣΔ only if that assumption holds and forced checkpoints are bounded.
- **Replay.** Replay reads less than α·max(*B*, *F*) bytes plus one commit's metadata.
- **Evidence, not proof.** Benchmarks can estimate *k* and can refute the assumption. They cannot prove it.

#### B.2.4 Capacity

Every mandatory checkpoint must fit within all of the limits above, so archive state is bounded. Once the bound is reached, mandatory checkpoints fail with `CAPACITY_EXCEEDED`. Reads continue, and only commits that reduce state below the bound succeed.

**Measured (gate G5, T32, 2026-10-05).** From the MOCHI writer and codec (`docs/dev/performance.md#capacity-memory`). These replace the earlier estimates, which came from a general-purpose CBOR library; each estimate was within 5%:
- **Per-file cost.** A single-chunk file with POSIX attributes and a 22-byte path costs **322 bytes and 49 items** in a snapshot manifest (estimated: 318 bytes, 49 items), and 426.5 bytes in the catalog image.
- **Binding limit.** The item limit binds first, at **342,391** such files. The snapshot's stored-size limit would bind at 833,761.
- **Multi-chunk files** cost **24** more items per chunk (estimated: about 25), so a file of *c* chunks costs 49 + 24(*c* − 1) items.
- **Image limit.** The image budget binds at **629,231** such files (estimated: roughly 600,000). Since B.2.15 the budget bounds the *decoded* image, so this figure and the item limit above (342,391 files) are unchanged by compressing images; only the stored size of the image falls (measured in B.2.15).

#### B.2.5 Specification text amended by this batch

- **§8.3.** CBOR-native envelopes, with identical validation obligations (D11).
- **§12.2.** Replaces "validation that the tail is uncommitted", which no check can provide (D14), with the eligibility rule and default quarantine. This resolved a contradiction between that MUST and D14.
- **D10.4** (2026-10-04). Introduction versus reference during replay, and its relation to §10.2 (Q18, Q26); unknown required feature versus unknown operation kind (Q19).
- **D10.6** (2026-10-03 and 2026-10-04). Parent traversal offset versus base footer hint (Q17, Q23); one descriptor per segment (Q16).
- **D12** (2026-10-04). Placement note tying the single descriptor to D10.6.
- **§7.3, §14.1** (2026-10-09). Pointers to D20 (B.2.10), the cryptographic profile this section required. B.2.2's wire-layout table lists the Encrypted-profile structures.
- **§7.4, §10.1, §15.1, §15.2, §22 step 6, D10 item 3, D11** (2026-10-12). Pointers to D21 (B.2.11); §15.1's catalog `parity_groups` cross-check is replaced by the frames' self-description checked against the commit chain and descriptor. B.2.2 registers payload encoding 1 and lists the parity frame.
- **B.2.2, B.2.3, B.2.4, B.2.10, header** (2026-10-16, owner). Payload encoding 2 (compressed catalog images), the decoded bound, and capacity notes: B.2.15 (D25).
- **B.2.7** (2026-10-19, owner). Hidden lock file on Windows, orphan lock-file sweep on Unix, and the locker-side verification: B.2.16 (D26).
- **B.2.7, B.2.16** (2026-10-20, owner). The writer lock moves onto the archive itself (Unix `flock`, Windows one byte at offset 2^62); no lock file is created; legacy `.mochi-lock` files are removed by writers on both platforms: B.2.17 (D27). B.2.16 items 1, 3, 4, 5 and 7 are superseded.
- **§23, §23.1, §23.2** (2026-10-24, owner). `mochi find` joins the 1.0 command surface: B.2.18 (D28).
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
- **G6 Windows durability.** Closed as a **policy disposition [owner, 2026-10-15]**, not as documented evidence and not as a pass on physical machines.
  - **The 1.0 rule.** Windows reports the creating commit's directory durability as `Unconfirmed`, and truncation there requires `--accept-unconfirmed-durability`. This replaces "until G6 passes".
  - **Why.** Plan O12 says to claim only what the Windows APIs document. The crash-replay evidence (`docs/dev/windows-crash-replay.md`, merged as #60) shows that on NTFS (Windows 11 build 26300.9457 and Windows 10 build 19045.2006, emulated disk that honours FLUSH/FUA) a new archive's name is made durable by the directory flush (`FlushFileBuffers` on the directory handle), which Microsoft does not document; `MOVEFILE_WRITE_THROUGH` alone lost the name in 3 of 3 controls. Observed behaviour of an undocumented step is not a documented guarantee.
  - **Regression guard.** The weekly `g6-windows` workflow (`.github/workflows/g6-windows.yml`) re-runs that evidence. It guards the observed behaviour; it does not change what Windows reports.
  - **Changing it** needs a documented primitive for the guarantee, or a new owner decision.
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
- **G10 Encrypted profile (D20, B.2.10 item 15).** **Met 2026-10-11**: the review recorded in `docs/ratification/R5-crypto-draft.md` §12 checked each item below against the tests, and R5 is frozen.
  - The R5 vectors pass in the implementation: XChaCha20-Poly1305 (CFRG draft), HChaCha20, Argon2id (RFC 9106), the NFC rule, and the key-envelope, sealed-object, and commit layouts.
  - A wrong passphrase fails closed with no partial output (`KEY_UNAVAILABLE`, exit 3); a tag failure on a hash-verified object is `CONTENT_INTEGRITY_FAILED`.
  - A property test over many writes, retries, crash and reopen, rewraps, and re-encryption finds all nonces distinct.
  - Without a key, stored-integrity verification passes while content levels, Recoverability, and Key availability are `UNKNOWN`; with the key, everything passes.
  - The dedup-equality leak exists and is asserted. Secrets never appear in `Debug` output or errors.
  - Hostile KDF parameters are refused before any allocation. Fuzz targets cover the key-envelope and sealed-object decoders.
- **G11 Redundancy profile (D21, B.2.11 item 15).** **Met 2026-10-12**: the review recorded in `docs/ratification/R6-erasure-coding-draft.md` §10 checked each item below against the tests (and the benchmark `docs/dev/performance.md#parity`), found no byte to change, and R6 is frozen.
  - The R6 vectors, from an independent implementation, reproduce in CI and match the implementation byte for byte.
  - ≤ *m* damaged shards per group rebuild bit-identically, in every commit at once; *m* + 1 is `PARITY_CAPACITY_EXCEEDED` and nothing is rewritten.
  - Parity survives the loss of the whole catalog image; the descriptor and earlier footers are rebuilt; a contiguous burst spreads across groups.
  - Damaged parity is `PARITY_DAMAGED` with integrity unaffected; forged frames are `PROFILE_VIOLATION`. A fuzz target covers the frame decoder and reconstruction.
- **G12 Format freeze (decided 2026-10-12, plan K12, K26).** **Met 2026-10-12** [delegated; owner review pending at the desktop checkpoint, plan K11]: each of R1 to R4 ends with its freeze review, the drift test and `r3_cddl.rs` pass, `user_version` is 1, the draft identifier is 2, and `b2/reject-archive-draft-1.mochi` is the draft-1 reject fixture.
  - R1, R2, R3, and R4 are frozen in one review, recorded in each artifact. R6 is frozen first (G11).
  - The drift test and R3's mechanical CDDL check pass.
  - R4 `user_version` is 1, and the draft identifier is 2 (D24). Whole-archive fixtures are regenerated deliberately, with the diff explained, and one draft-1 archive is kept as a reject fixture.
  - A cold reading of each artifact, by a reader given only this specification and that artifact, is recorded with every ambiguity fixed or decided.
- **G13 Golden set (R8).** **Met 2026-10-13** [delegated; owner review pending at the desktop checkpoint]: `fixtures/golden/r8/` holds 76 cases and 48 G1 audit vectors (`r8/g1/`), checked by `crates/mochi-testkit/tests/r8_golden.rs`.
  - `fixtures/golden/r8/` covers every 1.0 profile and allowed combination with valid, corrupt, and interrupted archives, plus copy and salvage cases.
  - Coverage is checked by a test: every §24.2 row about archive bytes, and every violation-class error code reachable from archive bytes, has a case. Exemptions are named in the test.
  - The B.2.6 G1 audit is complete in both encodings.
- **G14 Interoperability (R9).** **Met 2026-10-13** [delegated; owner review pending at the desktop checkpoint]: CI run 38364569151, job `r9-interop`, 76 of 76 R8 cases equal; results and every disagreement in `docs/ratification/R9-interop-results.md`. No byte changed.
  - The independent verifier (B.1 D7) produces, for every R8 case, an interop summary equal to the reference implementation's, in CI.
  - Every disagreement is resolved at its source and recorded in `docs/ratification/R9-interop-results.md`.
- **G15 Report freeze (R7).** **Met 2026-10-14** [delegated; owner review pending at the desktop checkpoint, plan K11]: R7 ends with its freeze review (§9 there; two owner questions recorded as options and decided by the owner on 2026-10-14: O1 keeps the `verify`/`fsck` report for `KEY_UNAVAILABLE` and amends B.2.10 item 12; O2 adds an in-band `schema` tag to the eleven command outputs, which freeze at the 1.0 flip; neither changes a frozen schema, a code, or an exit code); `docs/schemas/json/` holds a JSON Schema (2020-12) for every JSON the CLI writes, eight of them frozen; `crates/mochi-cli/tests/r7_json_schemas.rs` validates real outputs over all 76 R8 cases and temporary archives, kills 4,260 of 4,346 mutated outputs (the 86 survivors listed with reasons), and regenerates Section 20.6's example. No error code was renamed or changed meaning.
  - R7 is frozen, with a JSON Schema for every machine-readable output, and real outputs are validated against them in tests.

#### B.2.7 Writer lock (D17; decided by the owner 2026-10-06, plan Q54)

Not wire format: nothing is written into an archive. It is recorded here because two implementations that write the same archive exclude each other only if they take the same lock.

- **[Amendment, B.2.17 D27, owner 2026-10-20]** **Superseded in mechanism: the lock is no longer taken on a separate lock file.** The bullets Mechanism, Name, Ownership, Aliases, Creation and "A directory where the lock file cannot be created" below are kept as history; B.2.17 is the rule. No build from 0.9.3 on creates or opens a `.mochi-lock` file.
- **Mechanism.** The §12.2 step 1 lock is an OS file lock (POSIX `flock`, Windows `LockFileEx`, through a safe API; no new `unsafe` in `mochi-core`) held on a separate **lock file**, never on the archive alone. On Windows a lock on the archive makes every other handle's reads fail, so nothing could verify or read an archive during an append. **[Amendment, B.2.17 D27, owner 2026-10-20]** On Windows that holds only for a lock over the byte range the data occupies: `std`'s `File::try_lock` locks the whole range, but a one-byte lock at offset 2^62 does not touch any readable byte, so the lock can be on the archive itself (B.2.17 item 2).
- **Name.** `<archive file name>.mochi-lock`, in the archive's directory, both taken from the canonical path (symbolic links, `.` and `..`, and Windows short names resolved). **[Amendment, B.2.17 D27, owner 2026-10-20]** There is no lock file name any more; the name survives only as the legacy name that B.2.17 item 5 recognises.
- **Ownership.** Only the OS lock counts. The lock file is created if missing, never written or truncated, and never removed by unlock; a leftover lock file nobody holds does not block. **[Amendment, B.2.16 D26, owner 2026-10-19]** The only removal is the orphan sweep of B.2.16 (Unix only), and the file is created hidden on Windows. The OS releases the lock when its holder exits. **[Amendment, B.2.17 D27, owner 2026-10-20]** The lock file does not exist: the lock is on the archive (B.2.17); the hidden attribute and the orphan sweep of B.2.16 are superseded.
- **Aliases.** Every path that canonicalizes to the same directory and name reaches the same lock file; names a filesystem treats as equal (for example case on a case-insensitive filesystem) reach the same lock file as they reach the same archive. Hard links have no canonical name: on Unix a writer also takes `flock` on the archive itself, which excludes them and does not affect readers; **on Windows, writers through two hard-link names of one archive are not excluded** (no stable, safe API gives the file's identity). This is a known limitation, not a guarantee. **[Amendment, B.2.17 D27, owner 2026-10-20]** Closed: the lock is on the file, so hard links share it on Windows too (B.2.17 item 2).
- **Creation (D13).** The final name's lock file is taken before the temporary file is published, so a new archive is never visible unlocked. A lock held by another process there is `LOCK_CONFLICT`, and nothing is created. **[Amendment, B.2.17 D27, owner 2026-10-20]** The temporary file is locked on itself before it is published and the lock carries across the rename; a taken name is `DESTINATION_EXISTS` (B.2.17 item 3).
- **Readers** take no lock. Concurrent reads are correct because a reader interprets only what a valid footer commits (§12.2), not because of the lock.
- **A directory where the lock file cannot be created** (read-only) cannot be appended to; the writer fails before writing. **[Amendment, B.2.17 D27, owner 2026-10-20]** No lock file is created, so the directory's permissions do not matter; an archive that cannot be opened for writing still cannot be appended to.

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
10. **Documented invocation and claim** (spec 7.2). `zstd -dc ARCHIVE.mochi | tar -x --ignore-zeros -f - -C DIR`, with GNU tar and with bsdtar (libarchive, Windows `tar.exe`); a tool that needs another flag (for example bsdtar's `--options read_concatenated_archives`) gets its exact command in `docs/dev/tar-compatible-profile.md`. The claim names only the tools and versions that CI ran (`tar interop` job); it is never "POSIX compatible" and never covers an untested tool.

**Open items recorded here.** (a) Default `Dedup` was `InArchive` with no way to tell a request from the default, so the writer option gained an `Auto` default (rule 2). (b) Sparse files (holes) are not stored by the Core writer either; the rule is a guard for future writers. (c) Whether the desktop app offers the profile at creation is D4's UI question and is unchanged.

#### B.2.10 Encrypted profile (D20; decided by delegation 2026-10-09, plan C11)

**Status: implemented (plan C11) and frozen with R5 (2026-10-11, owner-approved).** The layouts below are permanent once archives exist; `docs/schemas/key-envelope-v0.cddl`, `docs/schemas/commit-record-v2.cddl`, `docs/schemas/recovery-manifest-v3.cddl`, and `docs/ratification/R5-crypto-draft.md` (with `R5-vectors/`) were reviewed before the freeze, and R5 §12 records that review. A change to the layouts is a new identifier (suite, envelope schema, sealed-object version, or required feature), never an edit. It fills the gaps §14.1 lists (suite, key derivation, envelope formats, nonces, associated data, key identifiers, ciphertext framing, errors, vectors) and keeps D3 and D12.

1. **Suite and parameters (D3).**
   - **AEAD.** XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha) with a 32-byte key, a **fresh random 24-byte nonce for every encryption** from the operating system's CSPRNG, and a 16-byte tag. There is no counter, no nonce derived from content or position, and no deterministic encryption anywhere (§14.2). Suite identifier **1**.
   - **Key derivation.** Argon2id (RFC 9106), version 0x13, 32-byte output, a 16-byte random salt per key envelope. Writer defaults: **m = 65,536 KiB, t = 3, p = 4** (RFC 9106 §4, second recommended option). Writers expose no other parameters in 1.0.
   - **Reader limits**, checked before any Argon2 memory is allocated: m > 1 GiB (1,048,576 KiB), t > 16, or p > 16 is `LIMIT_EXCEEDED`, naming the declared value; a reader raises these only by an explicit `--limit`. m < 8·p, t < 1, p < 1, or a salt that is not 16 bytes is `RECORD_INVALID`. An unknown KDF identifier or Argon2 version is `UNSUPPORTED_FEATURE`. Readers enforce no *minimum* cost (item 14, open item c).
2. **Passphrase.** The UTF-8 bytes of the passphrase after Unicode **NFC** normalization, so the same passphrase typed through a Windows or an Ubuntu input method derives the same key. An empty passphrase, or one longer than 4,096 bytes after normalization, is `INVALID_ARGUMENT`. Passphrases and every key live in `zeroize` types, are never `Debug`-printed, logged, or put in an error message, and cross the desktop IPC once.
3. **Keys and key envelopes.**
   - **Data key (DEK).** One random 256-bit key per archive, drawn at creation, with a random 128-bit **key ID** that names it. Every data chunk, catalog image, and manifest is sealed under it (item 5).
   - **Key envelope.** Each passphrase yields a KEK by item 1, and the KEK wraps the DEK in a **key envelope**: one skippable frame, kind `0x184D2A5C`, deterministic CBOR (D2), schema `key-envelope-v0.cddl`. Fields: the D11 identity (archive ID, the writing commit's sequence and transaction ID, required features `[1]`), a random 128-bit **envelope ID**, the key ID, the suite, the KDF identifier and parameters and salt, a random 24-byte wrap nonce, and the wrapped DEK with its tag (48 bytes).
   - **Wrap associated data**, fixed-width fields in this order, the last of variable length: the 16 bytes `MOCHI2-KEY-WRAP` 0x00 (defined once, in `mochi-format/src/digest.rs`), the 32-byte archive ID, the 16-byte envelope ID, the 16-byte key ID, the suite as a u16 little-endian, and the exact bytes of the envelope's KDF-parameter map (its canonical CBOR). A parameter altered in the file therefore fails the unwrap instead of weakening the key.
   - **Several envelopes may wrap the same DEK** (several passphrases); every envelope of one archive carries the same key ID (else `RECORD_INVALID`). A reader tries a commit's envelopes in listed order and stops at the first that opens. At most **16** envelopes per commit.
   - **Envelope identity (D11).** An envelope belongs to the commit that wrote it; later commits reference it without rewriting it. A reader checks the archive ID against the referencing commit and that the sequence is not after the referencing commit.
4. **Discovery and the required feature (§14.3, D12).**
   - **Commit record schema 2** (`commit-record-v2.cddl`) adds key 11, the **complete** list of the key envelopes valid at that commit (references with stored-object hashes, listed in increasing envelope-ID order), so a reader finds them from the footer-verified head **without decrypting anything**. The descriptor never locates envelopes (D12). A commit's envelope set is bound by the commit ID and the footer chain like any other reference.
   - **The Encrypted profile's required-feature identifier is 1** (the first unassigned value; suite 1, XChaCha20-Poly1305 with Argon2id). Descriptor key 4 lists it, as do every commit record's key 7, every manifest's key 9, and every catalog image's envelope; `KNOWN_REQUIRED_FEATURES` of this version of the format is `[1]` in all four. A reader that does not know it refuses the archive: `UNSUPPORTED_FEATURE`, exit 4. **The descriptor schema does not change**, and a feature identifier of 1 that earlier test code used as an "unknown feature" must be replaced by an unassigned value (for example 2).
   - **One profile per archive, agreed by every record.** Schema 2 commits and schema 3 manifests occur exactly in archives whose descriptor lists feature 1; a disagreement between a commit and a manifest is `RECORD_INVALID`, and between a commit and the descriptor `DESCRIPTOR_INVALID` (D12: a mismatched descriptor; R3 §10 item 9). An Encrypted archive cannot be TAR-compatible (descriptor constraint 0 must be false: `DESCRIPTOR_INVALID`; `create` refuses the combination with `INVALID_ARGUMENT`).
5. **Sealed objects.** Data chunks, catalog images, and recovery manifests (delta and snapshot) are **compress-then-encrypt**: the plaintext is exactly the skippable payload (images, manifests) or the whole Zstandard frame (chunks) that the same object has in a Core archive, sealed and stored in one `0x184D2A59` frame. The frame replaces `0x184D2A50` (image), `0x184D2A58` (manifest), and the bare data frame; D11's rule that the frame kind match the referencing field is read accordingly (commit keys 5.1, 5.2, and 6 reference `0x184D2A59`; key 10 stays `0x184D2A57`). Layout (integers little-endian; every byte defined):

   | Offset | Size | Field | Rule |
   |---|---|---|---|
   | 0 | 2 | Sealed-object version | = 0; other values `UNSUPPORTED_FEATURE` |
   | 2 | 2 | Suite | = 1; other values `UNSUPPORTED_FEATURE` |
   | 4 | 4 | Object kind | 0 data chunk, 1 catalog image, 2 delta manifest, 3 snapshot manifest; other values `UNSUPPORTED_FEATURE` |
   | 8 | 16 | Key ID | equals the key ID of the commit's envelopes (else `RECORD_INVALID`) |
   | 24 | 24 | Nonce | fresh per encryption |
   | 48 | n | Ciphertext | n = frame payload length − 64 ≥ 1 |
   | 48 + n | 16 | Tag | |

   - **Object associated data**, fixed width: the 19 bytes `MOCHI2-OBJECT-SEAL` 0x00 (defined once, in `digest.rs`), the 32-byte archive ID, header bytes 0 to 23 (version, suite, kind, key ID; the nonce is the AEAD's own input), then the **binding**: for a data chunk, the 32-byte object ID; for an image or manifest, the commit sequence (u64) and the 16-byte transaction ID (the D11 identity). A sealed frame therefore opens only in its own archive, at its own object, or its own commit.
   - **Stored-object hash.** It covers the whole stored frame (header, ciphertext, tag), so **stored integrity needs no key**. Content integrity needs the key. The hash is checked before the header is parsed, as everywhere.
   - **Sizes.** Sealing adds 64 bytes to a payload (header 48, tag 16), and 72 to a data chunk, which has no skippable header in Core. A writer enforces the B.2.3 limits on the sealed frame, so in an Encrypted archive a manifest's plaintext is at most *S* − 64 and the image payload budget is *S* − 656 = 268,434,800 bytes (the binary envelope header, at most 592, and the 64 bytes of sealing both come out of *S*). Since B.2.15 that budget applies to the stored (compressed) image payload that is sealed; the decoded image is bounded by *S* − 592 as in Core, so no Encrypted image that fit before is refused now. The sealed plaintext is still exactly the skippable payload (envelope header and Zstandard frame): compress, then seal.
   - **Chunk entries in manifests.** Key 2 (protection) is 1 for every chunk, key 3 and 4 describe the whole sealed frame, key 7 (dependencies) is empty (the DEK is archive-wide), and dependency kind 1 stays reserved. `recovery-manifest-v3.cddl`.
6. **What is plaintext, and what is visible.**
   - **Plaintext:** the descriptor, commit records, footers, key envelopes (except the wrapped DEK, which is ciphertext), and every frame header and sealed-object header.
   - **Visible without a key:** the archive ID; the wire generation and draft identifier; the commit count and sequence; each commit's parent link and footer offsets; the number, kinds, and sizes of objects (so the number of chunks and their compressed sizes, and roughly how many files a commit changed); envelope IDs, the key ID, KDF parameters, and salts; whether a commit added data.
   - **Not visible without the key:** file names and paths, attributes, directory structure, file sizes and content hashes, the dedup relation, and content.
   - **No commit times.** Writers of the Encrypted profile do not record commit times: the commit schema has no key 8. Anything that knows only the file learns no wall-clock time from it.
   - **Compression lengths are visible.** Compress-then-encrypt leaks each chunk's compressed size, and a party who can inject chosen content into a chunk and observe sizes could learn about the rest of it (the CRIME and BREACH class). 1.0 does not compress across files, which limits but does not remove this. It is documented, not mitigated.
7. **Data region (new commit key 12).** A commit's new data objects are written back to back, and key 12 names that byte range and its stored-object-scope hash (`null` when the commit adds no data). This is what lets a verifier that has no key hash every data object's stored bytes: the per-object hashes are in the sealed manifest. The range must walk as complete `0x184D2A59` frames of kind 0, end at or before the delta manifest's offset, and hash to the recorded value; otherwise `RECORD_INVALID` (shape) or `STORED_INTEGRITY_FAILED` (hash). With the key, `verify` additionally checks each object's own stored hash from the manifest and localizes damage to the object.
8. **Deduplication under encryption** keeps working: the writer deduplicates by plaintext chunk hash, which lives in the sealed catalog, and a duplicate reuses the existing sealed chunk. **The equality leak exists and is documented** (plan C11 exit): a party who can compare the file before and after an append learns whether the appended content was already stored (no new data region); a party who can make the owner add chosen content, and sees the file grow or not, confirms whether that content is already in the archive. A test asserts the leak exists. TAR-compatible archives, which do not deduplicate, cannot be Encrypted (item 4).
9. **Errors.**
   - A key envelope whose stored hash verified but whose AEAD open fails, or no envelope opened with the supplied passphrase, or no passphrase was supplied for a command that needs one: new code **`KEY_UNAVAILABLE`**, **exit 3**: a wrong passphrase is not evidence that the archive is damaged. It is added to both exhaustive matches (`exit_code_for`, `verify::classify`, where it is operational). No command that needs the key creates any output before the key is unwrapped.
   - A tag failure on a sealed object whose stored hash verified is `CONTENT_INTEGRITY_FAILED` (exit 1): the bytes are as written, and the object is not what it should be. A stored-hash mismatch is `STORED_INTEGRITY_FAILED`.
   - Hostile KDF parameters: `LIMIT_EXCEEDED` (item 1). A malformed envelope or sealed header: `RECORD_INVALID`. An unknown suite, KDF, version, kind, or feature: `UNSUPPORTED_FEATURE`.
   - **No partial output** in any of these: `get`, `restore`, and `restore-test` open the key before they create anything, and an object that fails mid-restore follows the existing restore rule (no half-written file is left).
10. **Fixed at creation; rewrites; `rekey`.**
    - **Profile fixed at creation** (D12): no in-place conversion to or from Encrypted; a request exits 4 (`PROFILE_CHANGE_UNSUPPORTED`). A rewrite does not convert either: `compact`, `gc apply`, and `repair apply` write a new archive **in the source's profile**.
    - **Rewrites re-seal.** The associated data binds the archive ID, and a rewrite creates a new archive ID, so every sealed object is opened and sealed again. Each rewrite of an Encrypted archive therefore **needs a passphrase**, creates a **new DEK and key ID**, and writes new envelopes for the passphrases given (a passphrase not given again does not carry over). Without a passphrase, `compact`, `gc plan`, `gc apply`, `repair plan`, and `repair apply` fail with `KEY_UNAVAILABLE`: retention state and chunk references are sealed, so even a plan needs the key. What `verify` finds without a key is the keyless view (item 12). The new archive's delta(0) carries provenance (manifest key 12).
    - **`rekey --add-passphrase` and `--remove-passphrase ENVELOPE_ID` are rewraps.** A rewrap writes one ordinary commit: no namespace operations, a new envelope frame for an addition, the new complete envelope set in commit key 11, and the **key operations** (manifest key 13: `[0, envelope ID]` add, `[1, envelope ID]` remove) as the rewrap's **audit record**. The DEK and key ID do not change and no data is rewritten. The set must never become empty; a commit's key 11 must equal the key state replayed from the manifests (checked wherever manifests are replayed with a key). Authentication: any passphrase of the head's set.
    - **Removal is not revocation.** The removed envelope's frame stays in the file's history, and a rewrap keeps the DEK, so the removed passphrase still yields the DEK from this very file (and from any copy), and the DEK opens content appended **after** the removal too. A reader may refuse a removed passphrase at the head, as `mochi` does, but that is policy, not a cryptographic barrier. Only a rewrite (`--reencrypt`, `compact`, `gc apply`, `repair apply`) puts content under a DEK that a passphrase not given again cannot reach. Output and documentation say so, and never say "revoked" or "secure".
    - **`rekey --reencrypt` writes a new archive** through the rewrite path (never in place): new archive ID, new DEK, new key ID, one envelope per new passphrase, every snapshot kept. Its audit record is the provenance key 12 with `reason` 1 (manifest schema 3, key 3 of the provenance map); a collection or compaction has reason 0, written by the **absence** of key 3 (an explicit 0 is refused, R3 §6 and §10 item 2). **Rewrap and re-encryption have separate audit records** (§14.4). Re-encryption does not recall copies already made.
    - **`rekey --list`** shows the head's envelope IDs, creation sequence, and KDF parameters. It needs no key.
11. **Passphrase entry (CLI).** An interactive prompt without echo (the `rpassword` crate), asked twice when a passphrase is created, or `--passphrase-file PATH` (the first line, line ending removed; read once; a warning if it is readable by others on Unix). **Never as an argument value.** The environment variable `MOCHI_PASSPHRASE` is read only behind `--passphrase-env-for-automation`, whose name and help say so. The desktop app follows AGENTS.md (a passphrase crosses IPC once and is not persisted unless the user opts into the OS credential store).
12. **Reports and verification (§20).**
    - **Without a passphrase**, `verify` runs the structural, referential, and `stored_integrity` levels. At `stored_integrity` it hashes the descriptor, every key envelope, every manifest and image, and every commit's data region: every stored byte a commit references. Integrity can be `PASS` at that level, and the report's scope says the content was not checked. Content-level checks (`content_integrity`, `restoration`) and Recoverability are `UNKNOWN`, and **Key availability is `UNKNOWN`**, each with a `skipped` item naming the reason (no key supplied). The overall status is therefore never `PASS`.
    - **With the passphrase**, every level runs as for Core, and Key availability is `PASS` once an envelope unwrapped the DEK and each sealed object that level opens verified; a passphrase that opens nothing is `KEY_UNAVAILABLE` (operational, exit 3) and **not a verification result**: it says nothing about whether the archive is damaged. `verify` and `fsck` may print a report for it, with no dimension assessed, `operational_error` true, one `error` finding with this code, and exit 3; every other command prints the error line. **[Clarified, T18, owner decision 2026-10-14 (R7 O1); text only, no code, byte, exit code, or error-code meaning changes]** The earlier wording, "not a report", disagreed with the frozen behaviour of those two commands.
    - `mochi health` keeps reporting Key availability `UNKNOWN` from local evidence (never `PASS` in 1.0).
13. **What the profile does not claim.** It protects the confidentiality of content, names, sizes of files, and structure beneath the visible set (item 6), and the integrity of every sealed object against modification **by a party without the key**. It does **not** authenticate the commit graph: commit records, footers, and the descriptor are plaintext and hash-chained, not signed (D12; signatures are post-1.0), so a party who can rewrite the whole file can roll it back, truncate it, or withhold commits (freshness, D8, still applies). It does not protect against a compromised endpoint, a weak passphrase (item 14), or traffic and size analysis. No wording in the CLI or the desktop app calls the result "safe", "secure", or "backed up" (§23.3).
14. **Limits (B.2.3 additions).** The reader limits `max-kdf-memory-kib` (1,048,576), `max-kdf-iterations` (16), `max-kdf-lanes` (16), and `max-key-envelopes` (16) are reader limits like the others, raised only by `--limit`; the sealed budgets of item 5 apply to writers. No limit bounds the *total* time of a hostile archive's envelopes (up to 16 at the limits); the derivation is a cancellable job with progress, and the CLI prints the declared cost before it starts a derivation above the writer defaults. Readers enforce no minimum cost: an archive written with cheaper parameters is the writer's choice and is reported as a finding, not refused. The finding (decided at the R5 freeze, open item c): for each distinct key envelope that any commit lists, the head's and removed ones alike (a removed envelope stays in the file and still yields the DEK, item 10), whose m < 65,536 KiB or t < 3, `verify` (keyed or keyless, at every level) reports one finding, saying when the head no longer lists the envelope, with the code `KDF_COST_BELOW_DEFAULT` and severity `info`, naming the declared and the writer-default values; it changes no dimension's status and no exit code, and its wording says "below the MOCHI writer default", never "weak" or "insecure".
15. **Evidence (gate G10).** The vectors in `docs/ratification/R5-vectors/vectors.json` pass (XChaCha20-Poly1305 from the CFRG draft; HChaCha20; Argon2id from RFC 9106; the NFC rule; the envelope and sealed-object layouts; the commit layout). Wrong key fails closed with no partial output. A property test over many writes, retries, crash and reopen, rewraps, and re-encryption finds every nonce distinct. Stored-integrity verify without a key passes while content levels and Key availability are `UNKNOWN`; with the key, everything passes. The dedup-equality leak is asserted to exist. Secrets never appear in `Debug` output or errors (the test output is searched). Hostile KDF parameters are refused before allocation. Fuzz targets cover the key-envelope and sealed-object decoders. Golden vectors are added deliberately, with the diff explained.

**Open items recorded here.** All decided at the R5 freeze (2026-10-11): each is kept as written, with (c) defined in item 14 and (d) recorded as Unicode 17.0.0 (`unicode-normalization` 0.1.25). The reasons are in `docs/ratification/R5-crypto-draft.md` §12.
(a) Item 7 (the data region) is a design choice beyond the plan's list: without it, a verifier with no key cannot check data objects at all, and the plan's claim that stored integrity verifies without keys would hold only for metadata. The alternative is to accept the weaker claim and report integrity `UNKNOWN` without a key.
(b) The associated data binds the archive ID (item 5), so every rewrite re-seals and needs the passphrase (item 10). Binding the key ID alone would let `compact` copy sealed chunks, at the cost of letting a sealed frame move between archives that share a DEK, which D20 never creates.
(c) No minimum Argon2id cost is enforced on readers. A reader-side floor would refuse archives written elsewhere; a warning is a finding.
(d) NFC depends on a Unicode version; unassigned code points pass through unchanged. The pinned crate version is part of the compatibility claim and is recorded in R5.
(e) Removing a passphrase does not revoke anything (item 10). Retiring a key while retained snapshots depend on it (§14.4) is therefore not a 1.0 operation.
(f) If every envelope is damaged the archive is unreadable; 1.0 has no key recovery beyond the passphrase, and the Redundancy profile (C12) is the only repair for envelope damage.
(g) `recovery-manifest-v2.cddl` does not yet draw key 12 (provenance), which the code defines and B.2.8 describes; `recovery-manifest-v3.cddl` restates it. The v2 file should be corrected when R3 is frozen.
(h) Converting an archive to or from the Encrypted profile by rewrite is not offered (D12).

**Implementation notes (C11, 2026-10-09; accepted at the R5 freeze).** The implementation follows items 1 to 15. Where the text left a choice, the code chose as listed in `docs/dev/encrypted-profile.md` ("Decisions made in the implementation"): low-level reads unlock from the commit's own envelopes while the head is opened strictly (item 10); "checked wherever manifests are replayed with a key" (item 10) is met for every commit by `verify` and `fsck`, and for the opened commit by appends, baseline recovery, and `segment_state`, but not by a plain read open, which never reads S(*b*) (D10.9); a keyless run's Integrity is `UNKNOWN` above `stored_integrity` (item 12); and the shipped CLI has no KDF-cost option (item 1: writers expose no other parameters). The commit schema's data region has a minimum length of 73 bytes (`8 + 48 + 1 + 16`: skippable header, sealed header, one ciphertext byte, tag), not 72 as the CDDL first said.

#### B.2.11 Redundancy profile (D21; decided by delegation 2026-10-12, plan C12)

**Status: implemented (plan C12); gate G11 met and ratification artifact R6 frozen 2026-10-12.** `docs/ratification/R6-erasure-coding-draft.md` states the suite and layouts below as checked tables, and `R6-vectors/` holds vectors from an independent implementation that CI regenerates. R6 froze after a review, as R5 did (`docs/ratification/R6-erasure-coding-draft.md` §10): a change to the suite, the layout rule, or the frame layout is a new identifier with its own vectors, not an edit. Archives with a recovery record remain draft-compatible only until 1.0 (§28). This entry fills the gaps §15.1 lists (field and matrix, `k`, `m`, ordering, sizes and padding, shard integrity, protected representation, reconstruction and validation) and states the §15.2 coverage.

1. **What it is.** A *recovery record*, in WinRAR's terms: each commit writes Reed–Solomon parity over the stored bytes it adds, in the same file, so that damage to those bytes can be rebuilt up to a stated capacity. It is chosen at creation as a **parity percentage** *r*, 1 to 100, recorded as descriptor constraint 1, `{0: suite, 1: r}` (`archive-descriptor-v0.cddl`; absent = no recovery record), and fixed for the archive's life (D12). It is **not a required feature**: no record references a parity frame, so a reader that does not check parity reads the archive exactly as before. A reader that does not know the suite named in the descriptor refuses the descriptor (`UNSUPPORTED_FEATURE`); a percentage outside 1–100 is `DESCRIPTOR_INVALID`.
2. **Suite 1: Cauchy Reed–Solomon over GF(2^8).** The field is GF(2^8) with the polynomial x^8 + x^4 + x^3 + x^2 + 1 (`0x11D`); addition is XOR. A group has *k* data shards and *m* parity shards, 1 ≤ m ≤ k ≤ 128. Parity shard *p* is Σⱼ C[p][j] · Dⱼ, byte by byte, with C[p][j] = 1 / (p ⊕ (128 + j)). Every square submatrix of a Cauchy matrix is invertible, so the code is MDS: **any k of a group's k + m shards rebuild it**. Because p < 128 ≤ 128 + j, no coefficient is 0 or 1, so no parity shard is a byte copy of a data shard (a copied footer could otherwise make an interrupted tail look like a damaged commit, D14 (b)).
3. **Protected range and coverage (§15.2).** Commit *n* protects the stored bytes [*a*, *b*): *a* is its parent's footer offset (0 for commit 0), *b* the end of its commit frame. The representation protected is **stored bytes** (§15.1), so parity works without keys.

   | Structure | Covered by |
   |---|---|
   | Content objects (data chunks, stream-only chunks, sealed chunks) | The parity of the commit that wrote them |
   | Metadata (catalog images), recovery manifests, key envelopes | The parity of the commit that wrote them |
   | Commit records | Their own commit's parity |
   | The archive descriptor | Commit 0's parity (it is at offset 0) |
   | Commit footers | The **next** commit's parity. The head's footer is covered by none (item 8) |
   | Dictionaries | None are written in 1.0; one written in a commit's range would be covered like any object |
   | Parity frames | None: each carries its own digest (item 6), and a damaged frame only reduces capacity |

   §15.2's rule that parity must not be the sole protection of its own configuration holds: the configuration is in the descriptor and repeated in **every** parity frame, and no parity frame is inside any protected range.
4. **Layout rule** (normative; R6 has vectors). With *L* = *b* − *a*: shard size *s* = min(1 MiB, max(⌈*L* / 1024⌉↑64, min(16 KiB, *L*↑64))), where *x*↑64 rounds up to a multiple of 64; real data shards *N* = ⌈*L* / *s*⌉, shard *i* covering [*a* + *i*·*s*, …) zero-padded to *s* (padding is hashed, never stored); groups *G* = ⌈*N* / 128⌉; **shard *i* belongs to group *i* mod *G*** at index *i* div *G*, so a contiguous burst spreads across groups; *k* = ⌈*N* / *G*⌉, and an index past *N* is a virtual all-zero shard; *m* = ⌈*k* · *r* / 100⌉. *G* must fit 32 bits.
5. **Placement and frame layout.** Parity frames are skippable frames of kind `0x184D2A5F`, written **between the commit frame and its footer** (§8.4's amendment requires only that the commit frame precede its footer), *G*·*m* of them, all of one length, group by group (frame *j* is parity shard *j* mod *m* of group *j* div *m*). A frame's payload is a binary envelope v0 (B.2.2) with **payload encoding 1** and record schema version 0, carrying the identity (archive ID, sequence, transaction ID) and the required features of the commit whose footer follows, then (integers little-endian): signature `MOCHI2-PARITY` and three zero bytes (16); suite (u16); *r* (u16); *s* (u32); *a* (u64); *L* (u64); *G* (u32); *k* (u16); *m* (u16); group index (u32); parity index (u32); the group's *k* data-shard hashes (32 bytes each, index order); the parity shard (*s* bytes); the frame digest (32). Each frame repeats its group's hash table, so any one intact frame locates damage in the group.
6. **Integrity.** A data-shard hash is BLAKE3 over `MOCHI2-PARITY-SHARD` 0x00 and the padded shard; the frame digest is BLAKE3 over `MOCHI2-PARITY-FRAME` 0x00 and every frame byte before it, skippable header included (both separators are defined once, in `digest.rs`). The digest is checked before any field is read. **Parity is a repair aid, never an integrity anchor**: no hash of the commit chain covers it, a rebuilt byte is used only if it hashes to the table, and everything that reads rebuilt bytes still checks the chain's own hashes (§9.2), which no parity frame can satisfy for bytes the writer did not write.
7. **Cross-checks (replaces §15.1's `parity_groups` cross-check).** Every frame of a commit must agree with every other (layout, identity, features, the group's hash table) and with the commit: its identity and required features, *a* = the parent's footer offset (0 at commit 0), *a* + *L* = the end of the commit frame, the layout = item 4's rule for the descriptor's *r*, the frames filling exactly [end of commit frame, footer), and each frame in its slot. A frame that verifies but disagrees is **`PROFILE_VIOLATION`** (integrity `FAIL`), and nothing from that commit's frames is used. A commit of a Redundancy-profile archive with no parity frames is the same violation. There is **no `parity_groups` catalog record and no manifest state**: which shards a group holds is a function of the commit chain and the descriptor, so it is neither mutable state (D10 item 3) nor a §10.1 catalog table in 1.0, and parity survives the loss of every catalog.
8. **Reconstruction.** From the head back to commit 0: parse (not validate) the footer, because its digest covers a commit frame that may itself need rebuilding; find a frame of the commit that decodes; check every real data shard against the table; for each group with damaged shards and at least *k* intact shards in all, solve for the damaged data shards and keep each only if it hashes to its table entry; lay the rebuilt shards over the file in memory; then validate the footer and read the commit record through the rebuilt bytes, apply item 7, and continue at the parent's footer, which this commit's range covered. **The head's footer is in no range**: when it is damaged, the head is found by the forward footer scan as without parity, and the head commit is never republished (D14: a missing footer looks exactly like an interrupted write).
9. **Capacity, and nothing beyond it** (§15.1). Per group: any *m* damaged shards, data or parity. A contiguous burst over up to about *m* · *G* shards (≈ *r* % of the range) lands at most *m* shards in each group. A group with more damaged data shards than intact parity shards is **`PARITY_CAPACITY_EXCEEDED`**: recoverability `FAIL`, nothing in the group is rewritten, and repair leaves out what the damage touches (C8 rule 2). Never wrong bytes.
10. **Writer.** After appending the commit frame, the writer reads the range back one group at a time (memory (*m* + 1)·*s*, at most 129 MiB at 100 %), writes the frames, and only then syncs and writes the footer (§12.2 steps 6–8 unchanged). A crash before the footer leaves complete frames and no footer: an eligible tail (D14). Cost: about *r* % of each commit's stored bytes in parity shards plus 176 + 8·*n* + 32·*k* bytes per frame (*n* required features), and at least one whole shard per parity shard, so a very small commit pays proportionally more (a 100 KiB commit at 1 % to 10 % writes 11.4 % of its size). Time is measured in `docs/dev/performance.md#parity` (§27): on its one VM the parity phase ran at about 1.4 to 2.2 GiB/s of multiply-add per core, e.g. 3.2 s for a 10 % record on a 512 MiB commit. No speed is promised beyond that table.
11. **Profiles together.** Encrypted (D20): parity is plaintext over sealed stored bytes, a linear function of ciphertext that reveals nothing the ciphertext does not, and keyless `verify` checks it. TAR-compatible (D19): parity frames are skippable, so the commits' streams are unchanged. Both combinations are allowed. **Fixed at creation**: asking an append for another percentage, or to add or drop the record, is `PROFILE_CHANGE_UNSUPPORTED` (exit 4); rewrites (`compact`, `gc apply`, `repair apply`, `rekey --reencrypt`) keep the source's profile, percentage included, and write fresh parity for the new archive.
12. **Verification and repair.** `verify` checks parity at `stored_integrity` and deeper (it reads every byte) and stays read-only: rebuilding happens in memory. **[Clarified, W18 (R9), 2026-10-13; no byte changes]** The first pass reads the bytes as stored, and every finding it makes is reported, including damage that parity then rebuilds (the stored-integrity finding stays; recoverability is what parity improves). The rebuilt view is read after that pass, for the commit chain, the snapshots, and the damage parity could not heal. A damaged or missing frame, or a group with no intact frame, is **`PARITY_DAMAGED`** (warning; integrity unaffected; recoverability at most `DEGRADED`). When damage was found and parity rebuilt all of it, the rebuilt view is verified again without parity; if it verifies, recoverability is `DEGRADED` ("run `mochi repair`"), not `FAIL`. Repair takes ladder step 6 **first**: it rebuilds what it can into the overlay, runs steps 1–4 over the rebuilt bytes, records the scan in the plan (`parity`), and writes a new archive (C8 rule 1) with fresh parity.
13. **CLI.** `mochi create --recovery-record PERCENT`. The desktop option is plan D5.
14. **What it does not claim.** It is not a copy or a backup (§15.3): a lost, overwritten, or deleted file loses its recovery record with it. A file cut short loses its newest parity first, and the head's footer is unprotected (item 8). It is not authentication: whoever can rewrite the file can rewrite its parity. No wording calls an archive "safe" or "backed up" because it has a recovery record (§23.3).
15. **Evidence (gate G11).** The R6 vectors, from an independent implementation (bit-by-bit field arithmetic), reproduce in CI and match the Rust code byte for byte; every erasure pattern of up to *m* rebuilds for all *k* ≤ 6; ≤ *m* damaged shards per group rebuild bit-identically in every commit at once, including the descriptor, a commit record, a catalog image, and an earlier footer; *m* + 1 is reported and left unchanged; the whole catalog image destroyed is rebuilt; a contiguous burst spreads across groups; damaged and forged frames are reported as above; Encrypted and TAR-compatible archives carry parity; a fuzz target covers the frame decoder and reconstruction; golden fixtures `c12/` must keep verifying and healing.

**Open items recorded here.** (a) The head commit's footer is unprotected (item 8); writing a second copy of the footer, or the §22 footer-history accelerator, would cover it at the cost of a wire change. (b) Damage at the end of the file reaches the newest parity first; a trailer copy or sidecar is a possible 1.x addition. (c) Each frame repeats its group's 32-byte shard hashes (32·*k* bytes), which dominates the overhead of small commits (a 100 KB commit at 10 % carries one 16 KiB shard plus 400 bytes of table); a separate index frame would shrink it at the cost of a single point of failure. (d) GF(2^8) bounds a group at 128 + 128 shards; larger commits use more groups rather than a larger field. (e) *Resolved at the freeze (2026-10-12):* the benchmark `docs/dev/performance.md#parity` was written; items (a) to (d) were kept as drafted (R6 §10), with (c) quantified there.

#### B.2.12 Independent copies, ladder step 5 (D22; decided by delegation 2026-10-12, plan K18)

**Status: implemented** (plan workstream W11; `mochi_core::copies`; plan C8 status). No wire change.

1. **What a copy is.** A file given to `repair plan` or `repair apply` with `--copy PATH`, repeatable, and opened read-only. It is a copy of the source when one of these holds:
   - (a) its frame at offset 0 is byte-identical to the source's descriptor frame;
   - (b) the source's descriptor is damaged, and the copy's descriptor's stored-object hash equals the descriptor hash (commit key 10) that a self-verifying source commit records.

   Anything else is reported as not a copy and is never used. Donation by stored hash from files that are not byte copies (rewrites, other archives) is not in 1.0.
2. **Order.** Parity (step 6) first, then copies, then steps 1–4, which read the stacked in-memory overlay. Nothing is written to the source or to a copy.
3. **Agreement.** The copy is opened as-is (its own parity is not applied in 1.0). A copy commit *agrees* with the source when its footer lies entirely within the source's length and one of these holds:
   - (i) the source holds a self-verifying commit with the same sequence and commit ID at the same footer offset;
   - (ii) its sequence is above the source's last self-verifying commit and it chains by parent commit ID to an agreed commit. Such a commit is labelled as confirmed by the copy.

   Where the source holds a *different* self-verifying commit at that offset, the copy has **diverged**, and agreement stops below that point. Copy commits beyond the source's length are reported, never used.
4. **Donation.** Within the agreed range, a frame is patched from a copy only when the copy's bytes verify against the expected hash taken from the agreed structure (footer digest, commit ID, stored-object hash, parity-frame digest, descriptor hash) and the source's bytes do not. Patches are whole frames. When several copies could donate, the first in argument order wins.
5. **Re-verification.** The survey then verifies everything it uses against the commit chain, exactly as without copies. Donation proposes bytes; it never vouches for them.
6. **Reporting (§22.2).** The plan names every copy (status, agreed range, divergence, later commits, what it donated) and lists every donated frame with its offset and copy. Each recovered snapshot is labelled by its trust: the chain, or confirmed by a copy. An I/O error on a copy disables that copy and is reported; it is not fatal. The plan format becomes `mochi-repair-plan-v2`.
7. **Outcome.** The C8 rules are unchanged. A repair completed with donated frames is complete, because its result is fully verified.

#### B.2.13 Bounded salvage, ladder step 7 (D23; decided by delegation 2026-10-12, plan K19, K20)

**Status: implemented** (plan workstream W12; `mochi_core::salvage`; plan C8 status). No wire change. New error code `ARCHIVE_ID_AMBIGUOUS` (operational, exit 3); new reader limit `max-salvage-candidates`. Item 11 records where the implementation read this text more precisely.

1. **Explicit.** Salvage runs only when requested (`--salvage`), after steps 6, 5, and 1–4, and only when they leave one of these: no trusted head; snapshots that no trusted catalog covers after a chain break; an unrecoverable descriptor. This is D12's "explicit salvage labelled partial".
2. **Bounded scan (§22.1).** The scanner walks frames forward. Where the walk fails, it resynchronizes by testing each later byte offset as a candidate frame start. It never resynchronizes inside a frame that walked validly. Each candidate counts against the reader limit `max-salvage-candidates` (default 1,048,576). At the limit the scan stops, and the step reports *incomplete*, never a pass. Every offset uses checked arithmetic.
3. **Candidates kept.**
   - A footer whose digest validates over the commit frame it names, and whose recorded commit offset, length, and sequence equal that frame's actual position and sequence.
   - A commit frame whose commit ID recomputes and whose envelope is valid.
   - Other frames are kept by position and interpreted only through a reference from a kept commit.

   A magic-number match alone is never a candidate kept.
4. **Identity.** The expected archive ID comes from the source descriptor; else from `--archive-id`; else from the kept commits. More than one archive ID among the kept commits, with neither of the first two sources available, is `ARCHIVE_ID_AMBIGUOUS`. Two kept commits with one sequence and different commit IDs are contradictory: neither is used, and both are reported.
5. **Confirmation (§5.1).** A kept commit is *confirmed* when its own footer is kept, or when a confirmed commit's parent link names it. **An unconfirmed commit is never recovered.** It may be an interrupted publication, so it is reported with its sequence, commit ID, and offset.
6. **Recovery.** Confirmed commits form chains linked by parent ID. Each chain's top commit is opened as a head, and its snapshots are recovered by the C8 rules (verified catalogs and manifests; verified versions only; nothing invented). A sequence the trusted chain recovered is never replaced.
7. **Uncertainty is recorded, never filled in (§22.1).** Data objects that no recovered commit references are counted with their total size, and are never placed in a namespace. A raw export of them is not in 1.0.
8. **Profile without a descriptor (D12 amendment).** When neither parity nor a copy restored the descriptor:
   - Encrypted follows required feature 1 in the confirmed commits, which must agree.
   - The recovery-record percentage follows intact parity frames, which must agree.
   - TAR compatibility is not carried and is reported as a lost constraint.

   The new archive has its own ordinary descriptor.
9. **Outcome.** Any repair in which salvage was used or stopped incomplete is **partial** (exit 2). Without a trusted head, completeness cannot be established. Retention follows C8: with no trusted head, it is unresolved, and apply needs the recorded waiver.
10. **Bounds.** A fixed-size index entry per candidate kept (about 200 bytes, so about 200 MiB at the default limit, not the 64 bytes first estimated) and 24 bytes per frame walked; time linear in the file plus the validation of each candidate; every offset checked; cancellation checked once per MiB scanned.
11. **Readings made at implementation (W12; no design change except where marked).**
    - (a) **What counts as a candidate.** The limit `max-salvage-candidates` (default 1,048,576) counts each magic number a resynchronization tests and each commit record or footer the scan evaluates. Data frames and other control frames walked in sequence are not counted. At the limit the scan stops; what it had confirmed is still recovered, and the step is `incomplete`.
    - (b) **Link confirmation, and the one change to item 5 [clarification, needed by the W12 test "every footer has one flipped byte"].** A kept commit is confirmed when its own footer is kept, or when **any kept commit's** parent link names it, and the link is position-consistent. Item 5 as first written said "a confirmed commit's"; with every footer damaged no commit is confirmed by footer, so that wording confirms nothing and the stated outcome (commits 0..n−1 recovered, the head listed as unconfirmed) cannot occur. The weaker rule is sound for §5.1: a writer starts a commit only after its parent's footer is durable (§12.2), so a commit that names a parent proves the parent was published, whether or not the child was. The child itself stays unconfirmed unless it has a footer of its own or a kept child. An interrupted publication (no footer, no child) is therefore still never recovered, and in the truncation-at-every-byte test nothing but footer-confirmed commits ever is.
    - (c) **Position consistency.** A parent link records the offset *H* of the parent's footer. The link names the parent only if the parent's frame ends at or before *H*, *H* plus a footer's length is at or before the linking commit, no frame that walked validly overlaps those 72 bytes (except a footer frame exactly there), and a valid footer at *H* names that parent. A byte copy of an earlier state stored inside the archive keeps the offsets of the original, so its commits fail this test; they are listed as *rejected*, not recovered. The same limit holds for the trusted head's chain below its break: where salvage regenerated a footer and the walk went on to a commit the scan rejected, that commit's record is not used (its snapshot still comes from the catalog of the commit above it, without a commit record). The same test keeps a forged link from regenerating a footer over valid bytes.
    - (d) **Regenerated footers.** For each confirmed commit with no valid footer at the offset its child records, the footer is re-encoded there from the commit frame and its position (it is a pure function of both) and laid over the in-memory healed view. The ordinary survey then runs over that view: the segments' tops are opened like heads, and every hash is checked as for any commit. Nothing is written to the source.
    - (e) **Descriptor [replaces the frame-count wording of item 8 for the percentage].** The descriptor is a pure function of the archive ID and the creation-time choices, so candidates are re-encoded from the confirmed commits' archive ID, the Encrypted profile (required feature 1; the commits must agree, else `RECORD_INVALID`), TAR compatibility, and a percentage 1–100 or none; the one whose stored-object hash equals the hash every confirmed commit records (commit key 10) is laid at offset 0. The hash is the oracle, which is stricter than reading the percentage from parity frames and cannot disagree with the commits. A descriptor that declared limits (key 6) cannot be re-encoded this way; the commits then stay unreadable and the step reports nothing usable. TAR compatibility is known when the hash matches, and is still not carried to the new archive: it is listed in `profile_constraints_lost` when the source had it.
    - (f) **When it runs.** Besides the three conditions of item 1, whenever the tail after the trusted head is not clean: that is where an unconfirmed commit lies, and it is listed.
    - (g) **Identity.** If the source descriptor is readable and `--archive-id` names another archive, the request is `INVALID_ARGUMENT`. A trusted chain's commit is never replaced by salvage: a contradictory twin of a commit the chain reached is listed and not used, and the chain's commit stays.
    - (h) **Label.** `salvage` on a recovered snapshot means "recovered only with the help of step 7": it was established, bridged, or opened (descriptor) by the scan. A snapshot the trusted chain recovers alone keeps `chain`.

#### B.2.14 Draft-to-1.0 boundary (D24; decided by delegation 2026-10-12, plan K13, K25)

**Status: item 1 implemented at the format freeze (W15, 2026-10-12); items 2 to 4 implemented at the 1.0 flip (W20, 2026-10-15; the owner approved D24 and plan K25 on 2026-10-15).**

1. **Draft identifier.** Descriptor key 3 becomes **2** in the change that freezes R1–R4 (gate G12), together with R4 `user_version` 1. Draft 2 names the frozen layout. Any later wire change before 1.0 increments it again, with an Annex B entry. A build refuses every draft other than its own (`UNSUPPORTED_FEATURE`, exit 4), naming the draft.
2. **1.0.** A 1.0 writer writes `null` (a ratified archive), and a reader accepts `null`. Every 1.0 command except `migrate` refuses a draft archive (`UNSUPPORTED_FEATURE`, exit 4), naming the draft. Only for the last draft (draft 2) does the message suggest `mochi migrate`; an older draft, such as draft 1, is refused without the suggestion. `repair plan` is a refusal in its existing form (nothing recoverable, each snapshot lost as `UNSUPPORTED_FEATURE` naming the draft, exit 1) and `health`, which reads no stored byte, does not read the descriptor; neither converts anything.
3. **Migration (§26).** `mochi migrate SRC --output DST [--passphrase-file F]` reads **only the last draft** and writes a new 1.0 archive through the rewrite path (`compact::Copier`: one commit per source snapshot, identifiers, attributes, retention and stored chunks preserved; the profile is kept; an Encrypted archive is sealed again under a new archive ID and data key, so it needs the passphrase, never as an argument). The source is opened read-only and never modified; an existing output is refused (`DESTINATION_EXISTS`). Steps of §26 performed: 1 (preserve), 2 (the source must verify at the restoration level, deep; one that does not is refused whole with the code of its first finding and nothing written, never converted partially), 3–5 (rewrite), 6 (restored file hashes, per-snapshot namespaces, file-version records and extents, promised attributes and retention are compared with the source before publication, and the new archive must itself verify at the restoration level), 8 (the report `mochi-cli-migrate-v1` records the source-to-new commit mapping and the evidence). Steps 7 and 9 are the user's and the report says so (`not_done`). No provenance record is written (a new rewrite reason would be a wire change). A wrong passphrase is `KEY_UNAVAILABLE` (exit 3). A ratified source is refused (`INVALID_ARGUMENT`: nothing to migrate). Older drafts are refused (`UNSUPPORTED_FEATURE`, exit 4). It is a job: it reports progress by phase (`migrate-verify-source`, `migrate-copy`, `migrate-verify`) and honours cancellation, which leaves nothing at the destination.
4. **Golden set.** R8 is regenerated in the 1.0 format at the flip, R9 is run on it again, and draft-2 archives are kept as migration cases: `fixtures/golden/r8/legacy/` holds three (Core, Encrypted, and Core with a recovery record). The regeneration changes no byte but descriptor key 3 (0x02 to 0xF6) and what hashes it; sizes are unchanged except `core-valid-self-containing`, which embeds an archive as file content (one byte longer after compression).

#### B.2.15 Compressed catalog images (D25; decided by the owner 2026-10-16)

**Status: decided [owner, 2026-10-16]; implemented the same day. An amendment to the frozen 1.0 layout, approved by the owner before any 1.0 release (only a 0.9.0 test release exists).** The reason is the owner's Windows 11 test: a one-file archive was about 46 KB, of which the catalog image was 45,136 bytes (SQLite 4 KiB pages, stored raw), and ten small files made an archive larger than the originals. Zstandard at level 19 reduces that image to 1,524 bytes.

1. **Encoding 2.** Binary envelope v0, payload encoding **2** = Zstandard-compressed SQLite 3 image. After the envelope header the payload is exactly one Zstandard frame (no bytes before or after it) with `Frame_Content_Size` present, the content checksum flag set, and no dictionary ID, as for data objects (O21). Its decoded bytes are an encoding-0 payload: they start `SQLite format 3\0`, and their `user_version` equals the envelope's record schema version. The envelope header is otherwise unchanged (payload length is the Zstandard frame's length). The signature check at envelope level is the Zstandard magic `28 B5 2F FD`; the SQLite signature and `user_version` are checked on the decoded bytes. Registry: R1 (`ZSTD_IMAGE_SIGNATURE`), R2. **Error codes for a payload that is not that frame** (decided in the reference implementation, in the order the checks run): a payload that does not start with the four magic bytes `28 B5 2F FD` (including an empty payload and one shorter than four bytes) is refused first, by the envelope's signature check, as `ENVELOPE_INVALID`; a payload that has the magic and is cut short inside the frame is `TRUNCATED`; one whose frame header is not a Zstandard data frame, or has bytes after the frame (including a second frame), or lacks `Frame_Content_Size`, or lacks the checksum flag is `MALFORMED_FRAME`, the same as a data object; a non-zero dictionary ID, a Zstandard decoding error (a bad block, a checksum mismatch), or a declared size the decoder does not produce is `CONTENT_INTEGRITY_FAILED`. These are checked in this order: structure and window (below), `Frame_Content_Size` present, the declared size against the bound (item 2), then checksum flag, dictionary ID, and decoding. So a frame that lacks the checksum flag and also declares more than the bound is `LIMIT_EXCEEDED`.
2. **Decoded-size bound.** The same number as the image payload limit (B.2.3): *S* − 592 = 268,434,864 bytes at the defaults. A reader refuses a frame whose *declared* content size exceeds it **before allocating any output** (`LIMIT_EXCEEDED`, limit kind `ImageDecoded`), reads at most one byte more than the declared size, and treats a different actual size as `CONTENT_INTEGRITY_FAILED`. **Window.** The reader limit `max-window-size` (default 134,217,728 bytes, 128 MiB; raised only by `--limit`) applies exactly as for data objects: it is checked from the frame header during the structural walk, before the decoder is created (a frame declaring a larger window is `LIMIT_EXCEEDED`, limit kind window size), and the decoder is configured with the same maximum. A single-segment frame has a window equal to its content size, so one declaring more than 128 MiB is refused on the window before the image bound is compared; a writer's frames for large images use a window below their content size, so this never affects a conforming 1.0 image. No decompression is unbounded.
3. **Digests are unchanged and cover stored bytes only.** The stored-object hash and the commit's reference to the image cover the stored frame as written; a reader hash-verifies it **before** decompressing or parsing, then runs the envelope's intrinsic checks in R2's decode order (which include the required-features list: count, strictly increasing, each known; `LIMIT_EXCEEDED`, `ENVELOPE_INVALID`, `UNSUPPORTED_FEATURE`), then binds the envelope identity (D11), then checks that the features equal the archive's profile (`[1]` exactly in an Encrypted archive, else none; `RECORD_INVALID`), then decompresses, then checks the decoded bytes. Every envelope and profile check therefore precedes any decompression. Nothing in the format is a digest of the decoded image. The Zstandard checksum is supplementary and never an integrity anchor (§9).
4. **Writer.** A 1.0 writer always writes encoding 2 for `0x184D2A50`, at a fixed Zstandard level of **9**. The level is a writer choice that readers cannot see (any frame meeting item 1 decodes), fixed so the same image gives the same bytes under one libzstd. Measured on three real catalog images (45,056 to 90,112 bytes), levels 3, 9, 15, 19 gave 22,439, 22,155, 21,720, and 21,690 bytes for the largest, a 3% spread; level 19 costs about ten times level 9 in time, which grows with the image up to the 256 MiB bound. A writer refuses a decoded image over the bound (`CAPACITY_EXCEEDED`), and a compressed payload over the payload budget, as before.
5. **Encrypted profile.** Compress, then seal. The sealed plaintext is the whole skippable payload (envelope header and Zstandard frame); the associated data and the *S* − 656 sentence of B.2.10 item 5 are unchanged, and now bound the compressed payload. The decoded bound of item 2 applies after opening.
6. **Readers.** Every reader of a catalog image (open, verify, fsck, repair, salvage, compaction, `dump-index`, `migrate`, the desktop app) goes through one decoder that accepts encodings 0 and 2. A draft-2 archive's images are encoding 0; `mochi migrate` reads them and writes a new 1.0 archive whose images are encoding 2. A 1.0 archive written before this decision (none was released) with encoding 0 images is read like a draft-2 image.
7. **Capacity (B.2.4).** The item limit (342,391 single-chunk files) and the image budget (629,231 files) are properties of the decoded catalog and do not change; only the stored size of images falls.
8. **Measured (2026-10-16, `mochi create` on Linux x86-64, defaults, release build, random file contents).** Archive sizes in bytes, before and after:

| Archive | Before | After |
|---|---|---|
| 1 file of 1,000 bytes | 47,570 | 4,083 |
| 10 files of 3,000 bytes (30,000 total) | 82,289 | 40,749 |
| 100 files of 2,000 bytes (200,000 total) | 354,757 | 286,285 |

   Random contents do not compress, so what remains above the input is the per-file cost of manifests (delta and snapshot, which are deterministic CBOR and stay uncompressed) and the catalog; compressing those is not part of this decision.
9. **Fixtures and ratification text.** R1 and R2 (`docs/ratification/`) are amended in place with this entry as their authority (the one exception to "a change is a new identifier", by the owner). B.2 envelope vectors gain `valid-envelope-image-zstd` and `reject-envelope-decoded-signature`; `reject-envelope-payload-encoding` now uses the unregistered value 3. R8 is regenerated (`legacy/` is kept: draft-2 encoding-0 archives) because every archive's images changed; earlier golden sets under `c5`, `c11`, and `c12` keep their encoding-0 images and are read by the same decoder. The independent Python reader (`interop/pyverify`) is outside this change and must learn encoding 2 in a cold session (plan K22).

#### B.2.16 Hidden and orphaned lock files (D26; decided by the owner 2026-10-19)

**[Amendment, B.2.17 D27, owner 2026-10-20]** **Items 1, 3, 4, 5 and 7 are superseded by B.2.17 (D27)**: no lock file is created or opened by a build from 0.9.3 on, so there is nothing to hide, no locker rule to keep between new builds, and the sweep's rules (item 4) and triggers (item 5) are replaced by B.2.17 item 5, which also runs on Windows (item 7). Items 2, 6, 8 and 9 describe the rules of the builds that wrote lock files (0.9.2 and earlier) and are kept as history; where B.2.17 item 5 uses "orphan", it does not need the notion. Nothing below is deleted.

**Status: decided [owner, 2026-10-19]; implemented. Not wire format:** nothing is written into an archive and no byte of `docs/ratification/` changes. It amends B.2.7 because implementations that write the same archive exclude each other only if they take the same lock file, and because B.2.7 said the file is never removed. The reason is the owner's: users see a stray `photos.mochi.mochi-lock` next to every archive, and leftovers after renaming or deleting archives.

1. *[Superseded by B.2.17, D27.]* **Hidden on Windows.** A lock file the writer *creates* carries `FILE_ATTRIBUTE_HIDDEN` (`0x2`). It is opened open-or-create without truncation (Win32 `OPEN_ALWAYS`: Rust `create(true)` and `truncate(false)`; `CREATE_ALWAYS` or `TRUNCATE_EXISTING` would fail on an existing hidden file), so an existing file, hidden or not, is opened as it is and its attributes are never changed. Lock files made before this decision stay visible. The name is unchanged on every platform (no dotfile on Unix): other implementations must take the same file.
2. **Orphan.** A file `<name>.mochi-lock` in a directory, with a non-empty `<name>`, such that `<name>` has no directory entry there (a dangling symbolic link is an entry), and that is a regular file (not a link, directory, or other) of length 0. Anything else is left alone, whatever its name.
3. *[Superseded by B.2.17, D27.]* **Locker rule (every locker).** After taking the OS lock, the locker checks that the lock file's path still names the file it locked (Unix: `dev` and `ino` of the open handle against `stat` of the path, following links as the open did). If not, it releases that file and starts again (open-or-create, lock, check), a bounded number of times. Without the rule, the hazard below is real.
4. *[Superseded by B.2.17, D27.]* **Sweep.** For each entry of one directory that ends in `.mochi-lock`: if the archive is absent (item 2), open the lock file *without creating it*, try the OS lock (held by anyone else, or any error: leave it), check that the path still names the handle's file and that it is a regular, empty file, check *again* that the archive is absent, and only then unlink it, **while still holding the lock**. A sweep never recurses, never follows a symbolic link, reads at most 4,096 entries, and ignores every error: it cannot fail the operation that triggered it.
5. *[Superseded by B.2.17, D27.]* **Triggers.** (a) A writer takes an existing archive's lock (an append, and any other operation that takes the archive's writer lock, such as compaction of its source); (b) a created archive has just been published (D13) and its creator holds the new archive's lock. In both the sweep covers the archive's own directory only. Nothing sweeps on read-only commands (`verify`, `fsck`, `list`, restore), nothing runs in the background, and MOCHI does not rename or delete archives itself (the CLI, the desktop app and `mochi-core` have no such operation: compaction, repair, and migration write new archives and keep the source), so there is no trigger of that kind.
6. **Safety argument.** The hazard: process B opens the orphan lock file, cleaner A locks and unlinks it, B then locks the unlinked inode, and C creates and locks a fresh file of the same name, so B and C both believe they hold the lock. (i) A removes only while holding the lock, so it never removes a file a locker holds, and a locker that has the lock keeps its file linked. (ii) B's lock on the dead inode is caught by item 3: the path no longer names B's file, so B starts over and either locks C's file (and is refused while C holds it) or creates its own. A handle on an inode keeps its number from being reused, so an equal `dev` and `ino` means the same file. (iii) A is not fooled by a creator: creators publish with a no-replace rename (D13), so two cannot both publish, and a creator takes the final name's lock *before* publishing and keeps it while it appends. A sweeper that finds the archive absent and the lock free has no creator or appender holding it; one that finds it held leaves it. (iv) An appender whose archive was deleted by hand between opening it and locking is the same case as B and ends on a fresh lock file; the deleted archive is a file it still has open, which no other writer can reach by name. (v) A creator that meets a sweeper holding the lock gets `LOCK_CONFLICT` once, for microseconds; the operation can be retried. The rule needs every locker to follow item 3: **a build or implementation that does not (any before this decision) can still lock an unlinked inode** while a newer build sweeps in the same directory. This is a known limitation of mixed versions, not a guarantee, and a reason not to sweep directories shared with implementations that do not follow B.2.16.
7. *[Superseded by B.2.17, D27.]* **Windows: no sweep (decided with the owner's brief: "if it cannot be made sound on a platform, do not ship a race there").** Item 3 needs a file identity, and stable Rust has none on Windows (`MetadataExt::volume_serial_number` and `file_index` are unstable, feature `windows_by_handle`, rechecked on 1.91; B.2.7 records the same gap for hard links). Rust opens files on Windows with `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, so a delete of an open lock file succeeds and its name is reusable at once: the B/C race above exists there, and nothing in this build can detect it. On Windows an orphan lock file is therefore left; the user may delete it by hand when no program writes that archive. **Open question, not decided:** a design that opens lock files without `FILE_SHARE_DELETE` and removes with delete-on-close could be sound, but it changes the sharing mode of every locker, interacts with lock files already open in older builds, and cannot be tested without Windows hardware; it needs a Windows test run before anyone relies on it.
8. **Hard links.** The Unix archive `flock` of B.2.7 is unchanged. An archive reached only through another hard link of the same file still has its own name in the directory, so its lock file is not an orphan; a lock file named for a name that is gone is, whatever other names the bytes have.
9. **Tests.** `crates/mochi-core/src/storage/os.rs` (the B/C interleaving forced between open and lock, the locker-side check, lockers racing a sweeper); `crates/mochi-testkit/tests/q54_writer_lock.rs` (both triggers; a lock file whose archive exists, one held by another handle, one with bytes, a link, a directory, non-lock names and sub-directories are kept; a sweep that fails never fails the writer; creators racing a sweeper end with one winner; Windows: hidden when created, reopened, visible files still work, no sweep).

#### B.2.17 Writer lock on the archive itself (D27; decided by the owner 2026-10-20)

**Status: decided [owner, 2026-10-20]; implemented. Not wire format:** nothing is written into an archive and no byte of `docs/ratification/` changes. It amends B.2.7 (D17) and supersedes B.2.16's items 1, 3, 4, 5 and 7 (D26). The reason is the owner's: users still see `<archive>.mochi-lock` next to every archive (0.9.2 hid only newly created ones, many people show hidden files, and orphans were never cleaned on Windows) and call it junk they clean up by hand. Best practice (SQLite, LMDB) is to take the lock on the file itself and leave nothing beside it. B.2.7 used a separate file for one reason only: on Windows a lock over the whole file fails every other handle's reads, and item 2 removes that reason.

1. **Rule.** The §12.2 step 1 publication lock is an exclusive, non-blocking OS lock **on the archive file itself**. No lock file is created, opened, or required by a build that follows this section. A lock held by another process is `LOCK_CONFLICT`. Only the OS lock counts, and the OS releases it when its holder exits.
2. **Mechanism per platform.**
   - **Unix:** exclusive `flock` on the open archive (Rust `File::try_lock`). It is advisory and does not affect readers. It belongs to the open file, so every hard link of the archive shares it.
   - **Windows:** `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)` on **one byte at offset 2^62 (`0x4000_0000_0000_0000`)** of the open archive, released with `UnlockFileEx` (or, at the latest, when the handle closes). A failure with `ERROR_LOCK_VIOLATION` or `ERROR_IO_PENDING` means another handle holds an overlapping range (`LOCK_CONFLICT`); any other failure is `IO_ERROR` with a message that names the lock and says the volume's filesystem may not support byte-range locks that far beyond the end of a file, not a bare I/O error. Win32 allows a byte-range lock beyond the end of the file, and it is mandatory only for the bytes it covers: no read or write of the archive's real bytes (below 2^62, far beyond any practical file size) conflicts with it, so readers, `verify`, and a second reader keep working while a writer holds the lock. Every implementation that writes the same archives must lock this byte. A byte-range lock belongs to the file, not to the name, so **hard links of one archive share it on Windows too**, which closes the gap B.2.7 recorded. The two calls need `unsafe`; they live in `mochi-sys` (AGENTS.md), and `mochi-core` keeps `forbid(unsafe_code)`. The lock is released explicitly on `unlock` and when the writer is dropped, because Microsoft documents the release at handle close as happening after a delay that depends on system resources.
   - Taking the lock again through the handle that holds it is a no-op on both platforms. Aliases (relative paths, `.` and `..`, symbolic links, short names, case on a case-insensitive filesystem, hard links) all reach the same file and therefore the same lock. Readers take no lock (B.2.7).
3. **Creation (D13).** The temporary file is created exclusively and **locked on itself** (the same lock as item 2) before it is published; the lock is on the open handle, so it carries across the no-replace rename (Windows allows renaming an open file that holds a byte-range lock; `windows_publishes_a_locked_temporary_file_by_move_write_through` keeps checking this), and the new archive is never visible unlocked. No lock on the final name is taken: there is nothing to lock there until the rename, and two creators cannot both rename onto one name. A taken name is `DESTINATION_EXISTS` whether or not its archive is locked; it replaces B.2.7's `LOCK_CONFLICT` for creation. Sidecars (D14) and temporary files are locked on themselves with the same primitive, and cleanup removes one only after taking that lock.
4. **Locker check.** After locking an existing archive, a locker checks that the path still names the file it locked, and if not releases that handle, opens the path again, and starts over, at most 8 times (an archive that no longer exists is an I/O error). The path is canonicalized when the handle is opened, so a later change of the working directory cannot redirect the check. The comparison is by file identity: on **Unix** `dev` and `ino` of the open handle against `stat` of the path (following links as the open did); on **Windows** the volume serial number and file index of the held handle against those of a second handle opened on the path for attribute access only (`GetFileInformationByHandle`, through `mochi-sys`; `std`'s `volume_serial_number` and `file_index` are unstable, `windows_by_handle`). The second handle shares read, write, and delete, so it never blocks another writer, reader, rename, or delete. A filesystem that reports file index 0 has no identity to compare, and the check answers yes there. MOCHI itself never replaces or renames an archive (compaction, repair, and migration write new archives), so only a person or another program can replace one.
5. **Legacy cleanup.** Builds up to 0.9.2 left `<archive file name>.mochi-lock` files (B.2.7). While a writer holds an archive's lock, it removes those files in that archive's directory:
   - *Triggers.* (a) A writer takes the lock of an existing archive (an append, and any other operation that takes the archive's writer lock, such as compaction of its source); (b) a created archive has just been published (D13) and its creator holds the new archive's lock. Nothing sweeps on read-only commands (`verify`, `fsck`, `list`, restore), nothing runs in the background, and MOCHI does not rename or delete archives itself.
   - *Scope.* The directory of the archive's canonical path, only; no recursion; it reads at most 4,096 entries; it never follows a link; every error is ignored, so it never fails the operation that triggered it.
   - *Candidates.* A directory entry whose name ends in `.mochi-lock` after a non-empty part, that is a regular file (not a link, directory, or other) of length 0 — **whether or not the archive it was named for exists**. A link, a directory, or a file with bytes is kept, as is anything else. **Any such file is removed by name, whatever created it** (owner's choice, 2026-10-20): an empty regular file called `<something>.mochi-lock` that a person made for another purpose would be removed too, if no other process holds a whole-file lock on it.
   - *Removal.* The sweep opens the candidate **without creating it**, takes the whole-file OS lock on it the way builds up to 0.9.2 took it (`File::try_lock`, which on Windows is `LockFileEx` over the whole file), and if that fails (held by another process) leaves the file. Under the lock it checks again that it is regular and empty (on Unix, that the path still names the file it holds; on Windows the handle is opened without following reparse points and a reparse point is refused), and unlinks it **while still holding the lock**. On Windows a hidden or visible file is removed alike. On Unix the sweep opens with `O_NOFOLLOW | O_NONBLOCK`, so a link swapped in after the directory was read is not followed and a FIFO swapped in cannot block the open. **Known limitation (Windows):** the removal is by name (`DeleteFile` of the path), and Windows offers no safe way to delete by handle identity from here, so a local attacker who can swap the directory entry between the checks and the delete can make the sweep remove another empty file of that name; the sweep never recurses and only touches names ending in `.mochi-lock`, so what can be removed is limited to empty files of that name in the archive's own directory.
   - *Why this is sound.* No build from 0.9.3 on opens a lock file, so the creation race that B.2.16 item 6 describes (an opener, a cleaner and a creator of the same lock-file name) no longer exists between builds that follow this section, and the orphan notion and the locker rule of B.2.16 are not needed. An old build that is running holds its lock file's whole-file lock, so the sweep leaves that file. Removing a lock file that nobody holds changes nothing for any build, because only the OS lock on it ever counted (B.2.7).
6. **Mixed versions (a limitation, not a guarantee).** A build up to 0.9.2 locks only the separate lock file (and, on Unix, also the archive with `flock`); a build from 0.9.3 locks only the archive. **On Unix** the two exclude each other through the archive's `flock`, as before. **On Windows a build up to 0.9.2 and a build from 0.9.3 that write the same archive at the same time are not excluded**: the old build's lock is on a file the new build never takes, and the new build's byte lock is on the archive, which the old build never locks. Both would pass the lock and then rely on the size re-check before each commit (§12.5), which detects a concurrent append but is not a lock. This is a state of mixed pre-release versions only, and the user guide says not to do it. A second, narrower case in the same family: an old build that has opened a legacy lock file but not yet locked it can lock a file a new build is removing; on Unix the archive `flock` still excludes it from the archive, on Windows it is the same non-exclusion as above.
7. **Tests.** *Filesystems checked by CI:* the lock is the same call on every volume, but whether a filesystem accepts a byte-range lock at offset 2^62 is the filesystem's property and users keep archives on USB sticks, so CI (`windows-latest`, job `test`, step "Writer lock on other filesystems") runs `q54_windows_writer_lock_on_the_volume_named_by_the_environment` on **NTFS** (the runner's own volume), **FAT32** and **exFAT** (64 MiB VHDs made with `diskpart`; a failure to create or format them fails the job), and **ReFS** (a 2 GiB expandable VHD; if the runner refuses to format ReFS, the step logs a warning, records "not run" in the job summary, and goes on, so ReFS is checked only where the runner can format it). On each: create an archive, append, read while a writer holds the lock, and a second writer is refused with `LOCK_CONFLICT`. Network filesystems remain unsupported for writing. *Test files:* `crates/mochi-core/src/storage/os.rs` (no lock file is created by lock, publish or create; readers read and a writer appends while the lock is held; hard links share the lock; drop releases it; a replaced archive is followed and a deleted one is an error (Unix; on Windows the replaced-archive test and an identity test over hard links and distinct files); the sweep's open never blocks on a FIFO or follows a link (Unix); the sweep removes empty regular legacy files, orphan or not, and keeps a held one, one with bytes, a link, a directory, other names and sub-directories; it is bounded and ignores a missing directory; both triggers; Windows: the lock is the byte at 2^62, a whole-file lock by another handle is refused while data reads work, a hidden legacy file is removed, a locked temporary file is published by `MoveFileExW` with the byte lock carried across the rename); `crates/mochi-sys/src/lib.rs` (Windows: the byte lock excludes a second handle and nothing else, hard links share it, the offset is split correctly, file identity is the file and not the name); `crates/mochi-testkit/tests/q54_writer_lock.rs` (every alias is excluded, hard links on both platforms; no lock file is ever created; readers see the committed snapshot at each phase of an append while a second writer is refused; a created archive is locked and readable, and a taken name is `DESTINATION_EXISTS` with nothing left behind; both sweep triggers; read-only use sweeps nothing; the keep-list; a failing sweep never fails the writer; creators racing end with one winner; a FIFO named like a lock file does not block a writer (Unix); Windows: the per-volume test above, two hard-link names exclude each other, a hidden legacy file is removed and the lock does not block reads).


#### B.2.18 Finding names across archives, `mochi find` (D28; decided by the owner 2026-10-24)

**Status: decided [owner, 2026-10-24]; implemented. Not wire format:** nothing is written into an archive, no byte of `docs/ratification/` changes, and no error code is added. It adds a command to §23 and its scope to §23.2. The reason is the owner's: the desktop app's MOCHI Find (released in 0.9.8) answers "which of my archives holds this file?", and the command line had no way to ask it. Both now run one engine, `mochi_core::find` (the folder walk with `storage::os::OsFindTree`), so the answers and the coverage are the same.

1. **Synopsis.** `mochi find PATTERN FOLDER… [--json]`. At least one folder is required. Global options apply as for every command (`--limit NAME=VALUE` sets the reader limits used to open each archive), except that a passphrase source (`--passphrase-file`, `--passphrase-env-for-automation`) is refused with `INVALID_ARGUMENT` (exit 3): find takes no passphrase, and accepting one silently would let a user believe an Encrypted archive was searched with it. `--state-dir` and `--no-local-history` have no effect: find records nothing.
2. **Scope.** Each `FOLDER` is made absolute (relative to the working directory, without resolving links) and must be an existing folder (a link to a folder is accepted as a scope; nothing below it is followed); otherwise `INVALID_ARGUMENT`. Duplicates and folders inside another given folder are dropped (compared by canonical path), so no folder is walked twice; at most 64 scopes. The walk is depth first: in each folder, its `.mochi` files (extension `mochi`, any letter case) in name order, then its subfolders in name order.
   - **Links are never followed.** A symbolic link, and on Windows a junction (any name-surrogate reparse point, as the standard library's `FileType::is_symlink` reports it), is not entered or opened. A link whose target is a folder, or whose name ends in `.mochi`, is counted in `links_not_followed`. No loop is possible.
   - **Bounds** (the desktop's constants, `mochi_core::find`): depth 48 below a scope, 200,000 folders, 20,000 archives per scan; 1,000,000 names per archive and 3,000,000 names held in memory across the scan (an archive over either is `too_large`); at most 8 workers open archives at once. A bound that stops the walk is reported as `limit` (`depth`, `folders`, or `archives`). None is configurable from the command line in 1.0.
   - A folder that cannot be listed, and an entry whose type or size cannot be read, is counted in `folders_unreadable`.
3. **What is opened and how.** Each archive is opened **read-only** (`OsReadStorage`; no writer lock is taken, nothing is written, swept or recorded). A file that does not start with the archive-descriptor magic is `not_mochi` without being parsed further. Otherwise the head is located and opened with `open_head` (`mochi_core::publish`), which hash-verifies the catalog image before SQLite opens it, and falls back to the snapshot manifest's catalog when the image fails, exactly as every open does; the head snapshot's entries are then listed through `mochi_core::search` with an empty query. **Only the head snapshot** is searched (the newest version); earlier snapshots are not. **No file content is read or hashed**, so find is not verification and never says that anything was verified: the hash check of the catalog image is part of opening, as for `list`. Keys are never given to the reader, and the draft policy is the default, so a draft archive is refused as for every command but `migrate`.
4. **Why an archive is not searched** (`reason`, with `code` and `message` where the reader gave them; the message can quote archive content and is escaped in text output): `not_mochi` (no MOCHI magic, including an empty or short file); `encrypted` (an Encrypted-profile archive: its names are sealed and find has no passphrase; `mochi search ARCHIVE` with a passphrase searches it); `unsupported` (`UNSUPPORTED_FEATURE`: a draft archive, an unknown required feature, a newer format); `damaged` (any other code the open or the listing returned, such as `TRUNCATED`, `STORED_INTEGRITY_FAILED`, `NO_VALID_HEAD`, `CATALOG_INVALID`; `mochi verify` is the check to run); `too_large` (bound above); `unreadable` (`IO_ERROR`, or the file could not be opened). Only cancellation stops a scan; every other failure is the archive's reason, never the command's error.
5. **Query.** Identical to the desktop app for one name with the default options (**[Amendment, B.2.21 D31, 2026-11-17]** the desktop app and `mochi find --name/--names-from` also search for a list of names, with match-case, whole-name, entry-kind and subfolder options; the plain form below is unchanged). `PATTERN` is trimmed; empty text is refused (`INVALID_ARGUMENT`; `*` matches every name), as is text over 1,024 bytes. Names are compared as stored bytes (O24, never normalized); case is folded for the comparison only: Unicode lower-casing per character (`char::to_lowercase`, not full case folding) for UTF-8 names, ASCII folding for bytes that are not UTF-8.
   - Without `*` or `?`: a **substring** of the entry's **last path component** (`invoice` finds `docs/Invoice-3.PDF`).
   - With `*` (any run) or `?` (one character): a **glob** that covers the whole last component (`*.pdf`; `rep*` finds `report.txt`, `ep*` does not). There is no escape for a literal `*` or `?`, and no character class.
   - With a `/`: the same rules against the **whole path** inside the archive (`docs/inv`, `docs/*.txt`).
   - Folders are entries too and can match.
6. **Coverage, and never "no matches" when incomplete** (§19.3, §23.3 #8). The search is **complete** only when every archive found was searched, every folder was read, no link was left unfollowed, and no limit was hit. Only then may zero hits be reported as "no matches". Otherwise the text output says `coverage INCOMPLETE`, lists every archive not searched with its reason and every count, and says that zero matches (or any number) is not proof that no matching name exists. File content is never searched, and the output says so.
7. **Output.** Text: one line per hit, `ARCHIVE: PATH` (a folder ends in `/`), archives in path order and entries in path order, then one coverage line and, when incomplete, one indented line per archive not searched and per non-zero count. Every archive-supplied string is escaped by `render::text` (§23.3 #9: control and bidirectional characters, backslash, non-UTF-8 bytes), and so are the archive file names on disk. `--json`: one line, schema `mochi-cli-find-v1` (`docs/schemas/json/cli-find.schema.json`, frozen when written): `schema`; `query` `{text, match: "substring"|"glob", whole_path, ignore_case: true}`; `scopes` (the folders walked); `coverage` `{complete, folders_visited, folders_unreadable, links_not_followed, limit, archives_found, archives_searched, archives_skipped, skipped: [{archive, reason, code, message}], names_searched, content_searched: false}`; `total_hits`; `hits: [{archive, path, path_hex?, kind, size}]` (`path_hex` only when the stored bytes are not UTF-8). Every hit is listed; the CLI does not cap them (the desktop shows the first 1,000).
8. **Exit codes** (§23.2, D15 precedence):
   - **0**: the search is complete, **whether or not anything matched**. A match is not a check that passed and no match is not a failure: find answers a question, it does not verify.
   - **2**: the search is incomplete (an archive not searched for any reason, a folder or entry not read, a link not followed, a limit hit). This is the meaning §23.2 gives 2, partial evidence, and the one `mochi search` uses for partial coverage. **A damaged archive is 2, not 1**: find runs no verification, the archive's names are simply unknown, and its reason names the code; `mochi verify` is the command whose exit 1 is evidence of damage. Find never exits 1.
   - **3**: invocation or operational error: no folder, a path that is not a folder, an empty or overlong pattern, a passphrase source, too many scopes; argument-parsing errors as for every command.
   - **4**: never in 1.0 (an archive in an unsupported format is a reason, rule 4, not the command's refusal).
   - Cancellation: the CLI has none in 1.0 (concession K2 of `docs/cli.md`); interrupting the process is safe because find writes nothing. The engine is a job (§23.4: a progress sink and a cancellation token; §23.3 #11): the desktop cancels it, and a cancelled scan is incomplete.
9. **What it never does.** It never asks for or uses a passphrase; never writes to an archive, beside one, or to the state directory; never keeps an index or cache on disk (the desktop app keeps a per-session cache in memory only, keyed by path, size and modified time); never follows a link; never searches content; and never calls an archive verified, safe, backed up, or preserved.
10. **Open questions (not decided; recorded rather than invented).** (a) *Earlier snapshots.* Find searches the head only; whether a `--snapshot retained` like `search` is wanted is open (it multiplies the cost by the number of commits). (b) *Non-Unicode file names on disk.* `archive` and `scopes` in JSON are text, lossy for a name that is not Unicode; an exact `archive_hex` like `path_hex` would need a v2 schema. (c) *Duplicates.* The same archive reached twice (hard links, bind mounts, two scopes that are aliases the canonical comparison cannot see) is searched and reported twice. (d) *Windows reparse points that are not name surrogates* (cloud-file placeholders, for example OneDrive) are files to the walk; opening one may make the provider download it. Not tested on Windows hardware. (e) *A `--require-complete` (exit 1) and configurable bounds*, as `search` has, are not built; scripts read `coverage.complete`.
11. **Tests.** `crates/mochi-core/src/find.rs` (the matcher's three forms and case folding, the walk over an in-memory tree: links counted and not followed, unreadable folders and entries, files that are not archives; each bound; cancellation; the stable reason words); `crates/mochi-cli/tests/c16_find_command.rs` (hits across nested folders, substring, glob and path queries, case folding, folders as hits, nested scopes walked once; zero matches complete exits 0 and an empty folder is complete; Encrypted, truncated, not-MOCHI, empty and draft-2 archives each reported with its reason, exit 2, never "no matches"; a passphrase source refused; a damaged catalog image rebuilt from the manifest and searched, image and manifest damaged reported as `damaged` with its code; hostile names and a hostile archive file name escaped in text and exact in JSON; links and a loop not followed, a linked scope accepted; invocation errors exit 3; nothing written anywhere); `crates/mochi-cli/tests/r7_json_schemas.rs` (real outputs validate against `cli-find.schema.json`, including one over all of R8; every field removed, retyped or added fails, plus targeted mutations); `apps/mochi-desktop/src-tauri/src/find.rs` (the desktop's Find tests, unchanged, over the shared engine).

#### B.2.19 The archive comment (D29; decided by the owner 2026-11-14)

**Status: decided [owner, 2026-11-14, plan O32 option (b)]; implemented. Wire format: commit record schema 4, required feature 3.** The owner asked for WinRAR's archive comment. 1.0 had none, and the ratified layout cannot gain a field by an edit (a change to `docs/ratification/` is a new identifier with an entry here), so the comment is a new commit-record schema that only a commit carrying a comment uses. An archive without a comment is written exactly as before, byte for byte; nothing in `docs/ratification/` changes; the golden sets R8, `c1`–`c12` and `b2` are unchanged.

1. **Where it lives.** Commit record **schema 4** (`docs/schemas/commit-record-v4.cddl`, standalone): schema 1 plus key **13**, the comment, a CBOR text string. A schema-4 commit lists **exactly** required feature **3** (the comment), registered here; `mochi_format::registry::FEATURE_COMMENT`. Key 13 is covered by the commit ID, so the comment is bound by the footer chain like every other field.
2. **Numbers.** Schema **3** and feature **2** stay **unassigned**: the frozen vectors `c5/reject-commit-schema-version.bin`, `c5/reject-commit-required-feature.bin` and `c4/reject-manifest-required-feature.bin` use them as the values no build knows, and they keep that meaning. R1's sentence "no other identifier is assigned" (required features) is amended by this entry: identifier 3 is the comment, in commit records only. Descriptors, manifests, catalog images, key envelopes and parity frames never list it; their known-feature lists stay `[1]`.
3. **The text.** 1 to **16,384** bytes of UTF-8 (`MAX_COMMENT_BYTES`), with **no control character** other than tab (U+0009) and line feed (U+000A), and no C1 control (U+0080–U+009F): no carriage return (a line ends with LF; writers convert CR LF), no escape sequences. Otherwise it is kept exactly as written: never normalized, case-folded or trimmed. A record that breaks a rule is `RECORD_INVALID`; a writer refuses the text before anything is written (`INVALID_ARGUMENT`). The worst case (16,384 bytes in a delta record) stays well inside the 64 KiB commit-frame limit (B.2.3).
4. **A value, not a change.** Key 13 is the archive's comment **as of this commit**, the rule key 11 follows for key envelopes: a writer repeats the head's comment in every later commit until a transaction sets another or clears it; a commit without a comment (schema 1) means the archive has no comment at that version. So any version's comment is read from that version's commit alone, without replaying, and earlier versions keep the comment they had (history, like everything else).
5. **Encrypted profile: no comment.** Commit records are readable without the passphrase (B.2.10), so a comment there would leak. A schema-4 record that also lists feature 1 is `RECORD_INVALID`; a writer of an Encrypted archive refuses a comment as `UNSUPPORTED_FEATURE` (exit 4) before anything is written. A sealed comment would need another schema and is not part of this decision.
6. **Readers.** A build that knows D29 reads schemas 1, 2 and 4. Every earlier build refuses a schema-4 commit as `UNSUPPORTED_FEATURE` (exit 4) naming the schema version, never as damage: the fail-closed rule of D11, as designed. An archive becomes unreadable to older builds from its first commented commit on; the user is not warned by older builds beyond that refusal. Verification treats a commented archive exactly as one without comments (same dimensions and statuses).
7. **Rewrites.** Compaction, repair and migration copy each version's comment with it (as they copy the informational time); `mochi migrate` reads draft 2, which has none.
8. **Command line (§23).** `create` and `append` take `--comment TEXT` or `--comment-file PATH` (UTF-8; CR LF becomes LF); `append --clear-comment` removes it; a comment alone makes a commit that changes nothing else. A new read-only command, **`mochi comment ARCHIVE [--snapshot SEQ] [--json]`**, prints the comment of the head or of that version (nothing when there is none; invisible formatting characters such as bidirectional overrides are escaped in text, exact in JSON; output schema `mochi-cli-comment-v1`, `docs/schemas/json/cli-comment.schema.json`, frozen as written). It reads commit records only and needs no passphrase. `snapshot list` text marks where the comment is set or cleared; its JSON (frozen) is unchanged.
9. **Desktop.** The Info window shows the comment of the version on screen as text (archive text, never markup). On the latest version of an archive that is not encrypted, *Edit comment…* saves a new version through a job (`set_archive_comment`); an empty text removes the comment.
10. **Tests and vectors.** `fixtures/golden/d29/` (one valid schema-4 commit, a carriage return and an empty comment refused) is checked against its builder, validated against the CDDL by the R3 test (`r3_cddl`), and seeds the `commit_decode` fuzz target, whose shape check covers schema 4; `mochi-core` `commit.rs` unit tests (round trip, golden bytes, every rule on write and on read); `mochi-testkit` `d29_comment.rs` (carry-forward, clear, reopened writer, Encrypted refusal, verification, compaction and repair keep every version's comment); `mochi-cli` `d29_comment_command.rs` and the R7 schema test; the desktop's `write.rs` job test and UI tests.
11. **Interoperability.** R9's independent implementation predates D29 and reads no schema-4 commit; the 1.0 interoperability claim (§28) covers archives without comments only, until R9 is extended.

#### B.2.21 Find many names at once, options, and retrieving the matches (D31; owner request 2026-11-17)

**Status: decided [owner request, 2026-11-17]; implemented in the desktop app and in `mochi find` (rule 8). Not wire format:** nothing is written into an archive, no byte of `docs/ratification/` changes, and no error code is added. `mochi find PATTERN FOLDER...` and its frozen schema `mochi-cli-find-v1` are unchanged; the new forms write the new frozen schema `mochi-cli-find-v2`. B.2.18 rule 5 ("identical to the desktop app") holds for one name with the default options, which the app still sends as the single query, and the options and lists match the same way in both.

1. **A list of names.** Find takes either one name or a list. The list is typed or pasted into a text area and split by the user's choice: *line breaks, commas, semicolons and tabs* (the default; never spaces, which file names contain), or one of line breaks, commas, semicolons, tabs, or spaces alone. Each piece is trimmed; a piece wrapped in matching double or single quotes loses them (Explorer's "Copy as path"); empty pieces are dropped; an optional *Names only* keeps each piece's last path component (by `/` or `\`). Repeats are dropped (compared case-insensitively unless *Match case* is on). At most **1,000 names** (`mochi_core::find::MAX_TERMS`), each at most 1,024 bytes, as for one name; the window says how many were left out and why, and Rust refuses anything beyond the bounds (`INVALID_ARGUMENT`). Pasting text that holds more than one name into the one-name box opens the list with it.
2. **Matching** (`mochi_core::find::TermSet`). Each name follows B.2.18 rule 5 exactly (substring of the last component; glob with `*`/`?` over the whole component; with `/`, the whole path), changed only by the options. An entry matches the list when it matches any name; it is **one** hit however many names it matches, and the hit says which (`terms`). Every name's count of matching entries and of archives holding one is reported, including zero.
3. **Options** (`MatchOptions`, `EntryFilter`). *Match case* (off by default) compares bytes as stored, with no folding. *Whole name only* (off) makes a name without `*` or `?` equal the whole last component (or, with `/`, the whole path) instead of being part of it; case folding when it is off is B.2.18's (Unicode lower-casing per character for UTF-8, ASCII for other bytes). Whole-name terms are looked up by name instead of compared one by one; a test checks the lookup gives the same answer as the comparison. *Show* is files and folders (default), files only, or folders only. *Search subfolders* (on by default) is a scan option (`Limits::subfolders`): off walks only the chosen folders themselves. It is the user's choice of scope, not a bound: no `limit` is reported for it and it alone does not make the search incomplete, but the window says that subfolders were not searched. Changing it scans again (unchanged archives come from the session cache). The choices are remembered in the window's own storage only.
4. **Coverage wording (§23.3 #8).** A name with no match is "not found" only when the search is complete; otherwise it is "not found in what was searched", next to the "Search incomplete" panel. Finding a name is not a check that passed: no success styling.
5. **What is shown: every match** (amended 2026-11-17, owner: "remove result limit"). There is no cap on the matches a search can list or retrieve, beyond the scan's own bound on names held (3,000,000, B.2.18 rule 2). Rust keeps the last query's full match list for the scan in its current state and serves it a page at a time (`find_query_many` with `offset` and `limit`; 1,000 per page by default, at most 10,000 per call). The window draws only the rows in view and fetches the page(s) under them as it scrolls, so a list of a million matches never crosses IPC or enters the page whole. Matches are in archive path order, then path order in the archive. The window can list the matches of one name alone (`term`, answered by Rust), and copy the names not found. The status line says how many matched in all.
6. **Retrieving the matches** (start_job kind `retrieve_found`, `src-tauri/src/retrieve.rs`). The user ticks matches (a box per row, Space on a row, a header box for all shown, Ctrl+A) and chooses *Retrieve…*: a folder (the native dialog, in Rust; it becomes the window's extract folder), the layout, and what to do when a name is already there.
   - The webview names the scan number, each archive's position in it and each entry's key: **no path**. Rust checks each key against the names the scan read, and that the archive file has the size and modified time it had when scanned; an archive that changed or went away is reported as *changed* and nothing is taken from it.
   - **Every match.** Ticking every loaded row offers *Tick all N matches*; the retrieve then names no entry and sends the search itself (`all`: the names, options and optional `term`). Rust finds the matches again in the same scan and takes all of them, whatever their number. The ticked-rows form is bounded only by the scan's bound on names. The restore engine's selection is a lookup by path (each entry against its ancestors), so its cost grows with the entries, not with entries times the paths selected.
   - Each archive is opened read-only (`OsReadStorage`, `open_head`, the catalog image hash-verified first) and only the ticked entries, with the folders above them, are written by the C6 restore engine (`restore_selected`), which refuses unsupported names, follows no link, and publishes a file only after its chunks and content hash verified. Find has no passphrase, so an Encrypted archive never has hits to retrieve.
   - **Layout.** *A new folder for each archive* (default): `name`, `name (2)`, … made with a no-replace create, so same-named files from different archives never meet. *All in the chosen folder*: entries keep their paths inside their archives.
   - **A name already there.** Only the two choices that make sense across archives: *keep it and add only new files* (default; `ConflictChoice::AddNew`, the entry is listed as not retrieved) and *replace it* (`Replace`: the existing file goes to the Recycle Bin or Trash once the new one has been checked). `Stop` and `SkipConflicts` are refused for this job (`INVALID_ARGUMENT`): with several archives, the second archive's folders already exist and would be skipped whole.
   - **Report.** Per archive: extracted (files, folders, bytes, the entries not retrieved with their reasons), changed, or failed (with the reader's message, escaped). Totals. It says what was extracted, never that anything was verified, is safe, or is backed up. Cancellation stops at the engine's next safe point; files already retrieved stay, each complete. "Open the folder when done" applies as for Extract.
7. **Tests.** `crates/mochi-core/src/find.rs` (match options, the term set's per-term results, filters and cap, the lookup equal to per-term matching, refused term sets, the subfolders walk); `apps/mochi-desktop/src-tauri/src/find.rs` (a list of names with per-term counts and an entry matching two names, match case, whole name, files and folders only, subfolders off still complete, one name equal to the single query, the fair share); `src-tauri/src/retrieve.rs` (one folder per archive keeps same-named files apart, all together keeps the first and reports the second, a changed archive is left alone, refused requests write nothing, an entry the scan did not read is not extracted, cancellation); `crates/mochi-core/src/restore.rs` (the path-lookup selection equals the pairwise rule); `crates/mochi-cli/tests/c16_find_command.rs` (several names, every option, names from a file, honest "not found" wording, errors) and `r7_json_schemas.rs` (v2 outputs validate, the tag rule for `-vN` files); paging over 2,500 matches and retrieving every match (desktop `find.rs`, `retrieve.rs`); `ui/src/find/terms.test.ts` and `FinderMany.test.tsx` (splitting, quotes, repeats, bounds; the list, the options, honest "not found" wording, paste into the one-name box, subfolders rescan, ticking, the retrieve request naming no path, the report as text).
8. **`mochi find` takes several names and the options too** (decided 2026-11-17; the owner left the choice to the implementer). `mochi find PATTERN FOLDER...` is unchanged and still writes `mochi-cli-find-v1`. `--name PATTERN` (repeatable) and `--names-from FILE` (one name per line, `-` for standard input; blank lines skipped) make every positional value a folder. `--match-case`, `--whole-name`, `--files-only` / `--folders-only` and `--no-subfolders` are the options of rule 3. With any of these, the text output adds one line per name (`name X: N matches in M archives`, or `not found`, or `not found in what was searched` when coverage is incomplete) and `--json` writes the new frozen schema **`mochi-cli-find-v2`** (`docs/schemas/json/cli-find-v2.schema.json`): `query` `{terms: [{text, match: substring|whole_name|glob, whole_path, hits, archives}], ignore_case, whole_name, entries, subfolders}`, the v1 coverage, `total_hits`, and `hits` with `terms` (the names each matched). Repeats are dropped; at most 1,000 names; exit codes as B.2.18 rule 8. The reason: the engine is shared, and a list of names is most useful in scripts (logs, audits), where the CLI is the tool.
9. **Open (recorded, not decided).** (a) Retrieving into a flat folder (no archive paths) would need engine support for renaming collisions; not offered. (b) The CLI has no retrieve: `mochi get` extracts from one archive.

#### B.2.20 Windows directory durability, the user's choice (D30; decided by the owner 2026-11-14)

**Status: decided [owner, 2026-11-14]; implemented. Not wire format:** no byte of an archive or of `docs/ratification/` changes. It amends B.3 item 1 (G6 policy B) and plan O12 in one respect: **policy B stays the default**, and a user may choose otherwise.

1. **The choice.** When an archive (or a tail-quarantine sidecar, or a rewrite's new archive) is created on Windows, MOCHI flushes the folder after publishing the new name (`FlushFileBuffers` on the directory handle, plan O12). Microsoft does not document that this makes the new name durable, so policy B reports the creation as `Unconfirmed` (`directory_unconfirmed`, `DEGRADED`, exit 2) whatever the flush returned. The user may instead choose **trust local NTFS**: a flush that **succeeded** on a **local NTFS** volume then counts as `Confirmed` (`durable`, exit 0).
2. **Default.** Policy B, in every build and on every first run. The CLI takes the global switch `--trust-ntfs-directory-flush` per invocation; the desktop app keeps the choice in its settings (Settings → Performance, shown on Windows only, off unless turned on; a settings file saved before it existed means off).
3. **Where trust never applies.** A flush that failed; a volume whose filesystem is not NTFS (FAT32, exFAT, ReFS, unknown); a network location (a UNC path, or a mapped drive, which canonicalization resolves to `\\?\UNC\…`). These stay `Unconfirmed` with a message that says why. Unix ignores the choice (`fsync` on a directory is the documented call). Restored (extracted) files are not archives and keep their own rules.
4. **Basis, said plainly.** The evidence is the G6 crash replays (`docs/dev/windows-crash-replay.md`): on NTFS a flushed new name survived 3 of 3, an unflushed one 0 of 3, and `MOVEFILE_WRITE_THROUGH` alone 0 of 3. It is observed behaviour on the tested Windows versions, not a documented guarantee; the weekly `g6-windows` workflow stays enabled as its regression guard. The option's help text and the desktop note say so, and the default keeps reporting only what Windows documents.
5. **Reports.** The frozen JSON schemas are unchanged: with trust on, a confirmed creation is `durable` exactly as on Unix. The choice is the user's own, made explicitly per invocation or in the app's settings.
6. **Implementation and tests.** `mochi_core::storage::os::WindowsDirectoryFlush` on `OsDir` and the files it creates (never a process-wide setting); the pure `windows_flush_verdict` and the path check are unit-tested on every platform; `mochi-cli` `d30_ntfs_flush.rs` runs on the Windows CI runner (exit 2 by default, 0 with the switch on its NTFS drive) and on Linux; the desktop's saved setting and its UI row are tested.

#### B.2.22 Freshness, durability, and retention compliance from local evidence (D32; decided by the owner 2026-11-23)

**Status: decided [owner, 2026-11-23]; implemented. Not wire format:** no byte of an archive or of `docs/ratification/` changes. The report schema (v1) is unchanged; what changes is which values three of its dimensions can take, and the evidence log gains a second record schema. It supersedes B.3 item 5 and amends B.1 D8, the §23.2 integration note on health, and plan K5.

1. **One local history, shared.** The last-seen head per archive (`heads.json`, D8) and the evidence log (`evidence/<archive id>.jsonl`, plan K5) are `mochi_core::storage::os::history`, used by the CLI and the desktop app at the same default location (`$MOCHI_STATE_DIR`, else `$XDG_STATE_HOME/mochi`, `~/.local/state/mochi`, or `%LOCALAPPDATA%\mochi`). A head either client records anchors the other's freshness check, and each reads the other's evidence. Nothing is written into an archive; verification stays read-only.
2. **Updates are serialized.** Read, change, and replace of `heads.json` happen under an exclusive lock on `heads.json.lock` beside it, waited for at most 5 s; a client that cannot get it records nothing and says so (the anchor is then older, never wrong). This closes plan K5's concession (two clients recording at once could lose one update).
3. **The desktop app keeps the history (supersedes B.3 item 5).** With Settings, *Remember archives on this computer*, on (the default, as the CLI's local history is on by default), *Test archive* is anchored at the head last seen on this computer: `PASS` if the verified history still contains it, `FAIL` (`FRESHNESS_FAILED`) on a rollback or substitution, `UNKNOWN` at first sight, exactly as `mochi verify` (D8). A test in which nothing failed records the head. Each completed test and each commit the app publishes is appended to the shared evidence log, and the health summary reads it. Off, the app behaves as before: no anchor (freshness `UNKNOWN`, never `PASS`) and this session's evidence in memory only. The app still offers no restore-test.
4. **Durability from commit records.** A client that publishes a commit (`create`, `append`, the other commands that commit, the new archive's head after `compact`, `gc apply`, `repair apply`, or `migrate`, and the desktop app's New archive, Add, Delete, Comment, and repair) appends a **commit record**: evidence schema 2 (`docs/schemas/json/health-evidence-v2.schema.json`), command `commit`, level `null`, durability `DEGRADED`, and a scope saying whether every sync was confirmed or why the directory entry is unconfirmed. `health` takes the latest commit record about the current head: durability `DEGRADED`, with that scope in its note; with none, `UNKNOWN` (bytes cannot show durability). **It is never `PASS`:** one copy on one device is a reduced protection even when every sync was confirmed, and nothing in 1.0 establishes a recovery copy (§16, §23.3: never "safe" or "backed up"). `verify` still reports durability `UNKNOWN`. Every other record stays schema 1, so a log mixes both; a reader that knows only schema 1 counts commit lines as unreadable, which blocks a `PASS` and never hides a failure.
5. **Retention compliance, local.** `verify` rebuilds the retention state at the head as GC does (D10.10; B.2.8) and reports:
   - `UNSUPPORTED` for an unsupported level;
   - `UNKNOWN` when the head was not opened (or an Encrypted archive was checked without its key), or below the `restoration` level (retained snapshots were not reassembled);
   - `PASS` at the `restoration` level when integrity and recoverability pass: every snapshot the archive's own retention state keeps (the head, every unexpired commit, every held one) is present and restorable;
   - `FAIL` when recoverability fails and no snapshot is expired (every commit is retained, so the damage is in a retained snapshot); `UNKNOWN` with a note when snapshots are expired, because whether the damage touches a retained one is not determined; `DEGRADED` when parity can rebuild the damage (D21);
   - when the state cannot be rebuilt (`RETENTION_UNRESOLVED`): `FAIL` with the finding under `deep` (`fsck`), and `UNKNOWN` with a note naming the code and `fsck` under plain `verify`. Rebuilding the state is a baseline replay of the head's segment, and Q64 (B.1 D18, "reference scope") keeps a failed baseline replay a failure of `deep` only.

   It is a **local** check: no inventory is consulted, so it says nothing about recovery copies (§16, Preservation profile). `health` judges it from `verify`, `fsck`, and `repair apply` records with the integrity rules (plan K5); a `restore-test` does not speak for it.
6. **Searchability is not shown in the desktop app while it means nothing.** In 1.0 no search index exists to assess, so searchability is always `UNSUPPORTED` (B.3 item 4). The desktop report does not list it while it is `UNSUPPORTED` and not required; any other status, or a required one, is listed, so nothing failing and no reason for a policy result is hidden. The exported JSON, the CLI, and the report schema keep every dimension.
7. **Tests.** `storage::os::history` (a lock held too long records nothing; eight concurrent recorders lose no update), `health` (durability from commit records, capped at `DEGRADED`; retention from `verify` records), `c7_verify` (retention per level), `q64_fsck_command` (`fsck` `FAIL`, `verify` `UNKNOWN` with a note), `c14_health` (commit records written by `create` and `append`, read by `health`), `r7_json_schemas` (schema 2 validates and mutations of it fail; spec §20.6 regenerated: its damaged older commit makes retention compliance `FAIL`), the desktop `check` tests (shared anchor: first sight `UNKNOWN`, then `PASS`, an unseen later head `FAIL`; a new session's health reads the shared log) and UI tests (the setting; searchability hidden), and the E2E `check` spec (a run-local `MOCHI_STATE_DIR`; the CLI oracle runs against a copy of the history taken before each app test).

#### B.2.23 Archive-creation options (D33; decided 2026-11-24 by the assistant, on the owner's delegation)

**Status: decided [owner, delegated 2026-11-24]; items 1 and 2 implemented, item 3 implemented in the desktop app and the command line. Not wire format:** no byte of an archive or of `docs/ratification/` changes. The owner asked which WinRAR-style creation options MOCHI can offer and delegated the choice ("choose the best decision based on best practices"). The rule applied: build what changes no format byte and cannot lose data silently; defer what changes the ratified format to 1.x, each as its own decision with a new identifier; refuse what Annex A or a security posture rules out.

1. **Wait if other MOCHI programs are active: built, opt-in.** `WriterOptions::lock_wait` (at most one hour): an append that finds the publication lock held retries every 100 ms for that long, then fails with `LOCK_CONFLICT` as before. The lock is still taken before the head is read (§12.2 step 2), so publication stays serialized and the commit is built on the head the other writer left; nothing about §12.5 changes except that the conflict is waited out first. Off by default everywhere, because §12.5's default policy is fail-on-conflict and a script should learn of a busy archive at once. CLI: global `--wait-for-lock SECONDS` (1 to 3600). Desktop: Settings → Performance, *Wait if another MOCHI program is writing* (two minutes; `compression-settings.json`, `wait_for_writer`, off when absent). Creating an archive never waits: it writes a new temporary file. Test: `q54_writer_lock::b2_23_a_waiting_writer_gets_the_lock_when_the_holder_lets_go`.
2. **Preserve the source's last access time: built, always on, Linux.** Archiving a file should not change it. Source files are opened with `O_NOATIME`, which Linux grants for the user's own files (or with `CAP_FOWNER`); otherwise the file is opened normally and the system's policy (usually `relatime`) applies. There is no switch: keeping the time has no cost and no downside. Directories are listed with the standard calls and may get a new access time. Windows is not covered in 1.0: it needs a call outside the reviewed `mochi-sys` list (`SetFileTime` with `0xFFFFFFFF` on the read handle), and NTFS often does not update access times at all. Test: `storage::os::source::tests::reading_a_source_file_keeps_its_access_time`.
3. **Delete files after archiving: built in the desktop app and the command line.** Because it destroys the source, these rules hold: a file is removed only after the commit is published and that file's version in the new head **reads back and hashes to its recorded file-content hash**, and only if the source file still hashes to the same value (so a file changed during the import is kept); removed files go to the Recycle Bin or Trash (`trashbin.rs`), never deleted outright; a folder goes there only once everything in it went; anything kept is listed with the reason. The wording is "moved to the Bin", never "safe" (§23.3). The command line has the same rules: `create` and `append` take `--move-sources-to-bin` and move through the `trash` crate (the Recycle Bin on Windows, the freedesktop.org Trash on Ubuntu), the crate the desktop app uses; where the system has no Bin for a location, the source is kept and listed (exit 2), never deleted. Its `--json` output is then `mochi-cli-commit-v2`, because v1 is frozen. **Implementation:** `mochi_core::import::import_recording` records which source became which entry; `mochi_core::sources::check_removable` decides (rules 1 to 3 above; a folder only if nothing under it is kept); the desktop's New archive and Add take *Delete files after archiving* (off by default, per job; `delete_sources` on the request) and, after the commit, move what may go through `trashbin.rs` (files first, then each folder that is empty, deepest first), checking each once more without following a link. The commit report gives `sources_moved` and `sources_kept` with reasons; a failure here never fails the job, because the commit stands. The moving itself is `mochi_core::storage::os::move_sources::move_removable`, shared by both clients. The quick verbs do not offer it. Tests: `b2_23_sources` (unchanged sources go; a file changed after archiving, or whose archived copy is damaged, is kept with its folders), the desktop `write` tests (everything goes to the Bin and the archive holds it; a folder still holding a link is kept), UI tests, and the E2E `create-add` spec (the box is off by default; ticked, the folder ends in the Bin and the result line says how many were moved), and `mochi-cli/tests/b2_23_move_sources` (off by default with v1 output; everything moved with exit 0; a Bin that refuses keeps every source, exit 2, and the commit stands).
4. **Background archiving: later.** The useful part, a lower processor priority for the job's threads, needs `SetThreadPriority` on Windows, another call outside the reviewed `mochi-sys` list. Until it is reviewed, Settings → Performance, one core, keeps the computer responsive, and the shell's quick verbs already run in a small window.
5. **Store modification time: always, as now; no switch.** It is a promised attribute (D6) and part of the TAR profile; a switch would add archives whose restore silently loses times. **High-precision time** is already the case: nanoseconds on POSIX, 100 ns on Windows.
6. **Out of 1.0, each a 1.x candidate with its own identifier:** symbolic links as links (designed, B.3 item 2, first in line); hard links as links (content is already stored once by deduplication, so the space is already saved); creation time and last access time; file security (ACLs) and alternate data streams (D6: not promised; restoring them needs privileges and reports exceptions). Each adds manifest keys or kinds, which is a new format identifier with an Annex B entry, never an edit of draft 2.
7. **Lock archive: not offered.** A lock flag would be advisory (any other writer could ignore it), would be a new descriptor constraint, and adds little to an append-only format, where history is never rewritten in place. A user who wants a file left alone can use the file system's read-only attribute.
8. **Self-extracting `.exe` archives: refused** (Annex A, "Not in 1.0": SFX archives; `UNSUPPORTED`, exit 4). An unsigned executable produced by an archiver is a known malware vector, and the installers themselves are unsigned by owner decision (B.3 item 3).

### B.3 Owner decisions of 2026-10-15

Each item answers a finding of the gate-1 audit (an internal review of 2026-10-15, findings F1–F10) or the G6 evidence (`docs/dev/windows-crash-replay.md`). None changes a format byte, draft 2, or an artifact in `docs/ratification/`.

1. **G6 Windows durability: policy B [owner, 2026-10-15].** Windows keeps reporting the creating commit's directory durability as `Unconfirmed` in 1.0, by policy. G6 is closed as an explicit **policy disposition**: it is not a claim of documented durability and not a pass on physical machines. The text is in B.2.6 (G6) and D13. The `g6-windows` crash-replay workflow, including its Windows 10 22H2 leg, stays enabled weekly as a regression guard. Changing the rule needs a documented primitive or a new owner decision. **[Amendment, B.2.20 D30, owner 2026-11-14]** Policy B stays the default; a user may choose to count a successful folder flush on a local NTFS volume as confirmed.
2. **Symbolic links: deferred to 1.x [owner, 2026-10-15]** (audit F2, option a). D6 no longer lists links as a 1.0 entry type. Kind 2 stays reserved and refused; the writer keeps skipping links and reporting each skip.
3. **Release gate 6 [owner, 2026-10-15]** (audit F3). Updates are signed with minisign (`requireSignedVersion`). Installers are unsigned by owner decision: no Authenticode, no `.deb` or APT signing. Plan §8.2 gate 6 is amended to match.
4. **Search and interrupted indexing [owner, 2026-10-15]** (audit F4, option a). In 1.0 the §8.1 Search row's "interrupted indexing visible" and the §24.2 row *Interrupted indexing* do not apply, because search scans the catalog directly and builds no index. The row is met as the K17-style option: searchability is `UNSUPPORTED` in `verify` and content search is refused (tested: `c7_unsupported_levels_exit_4`, `c14_search_refuses_content_search_and_bad_arguments`). The requirement returns when an index exists.
5. **Desktop freshness and evidence [owner, 2026-10-15]** (audit F5, option a). The 1.0 desktop application keeps no head history, no evidence log, and offers no restore-test. Its freshness stays `UNKNOWN`, never `PASS`. To be revisited in 1.x. **[Superseded, B.2.22 D32, owner 2026-11-23]** The desktop app now shares the CLI's head history and evidence log (on by default, a Settings opt-out); it still offers no restore-test.
6. **RAR claims narrowed [owner, 2026-10-15]** (audit F7, option a). RAR stays read-only; MOCHI never writes RAR. "WinRAR" is dropped as a corpus claim. Multi-volume RAR is read but not tested as a complete set. Two gaps are recorded: no hostile-input RAR fixtures (traversal, bomb, ratio, BLAKE2sp; 8 mutation survivors), and no complete multi-volume set test.
7. **User guide in the installers [owner, 2026-10-15]** (audit F8). `docs/user/` is bundled in both installers as static HTML, and the desktop app's Help, User guide opens that copy offline. The guide is written for the `mochi` command line and says so in its menu label and in a banner on every page, with pointers to the desktop equivalents.

8. **Review of delegated decisions (plan K11) [owner, 2026-10-15]** (audit F9). The owner approved every decision made by delegation: the freeze self-reviews of G12 (R1 to R4, draft 2), G13 (R8), G14 (R9), and G15 (R7); Annex B D10, D11, D12 (with the K20 amendment), D14, D15, and D24; and plan calls K10 to K26. D10, D11, D12, D14, and D15 move to B.1. D24 items 2 to 4 stay decided and unimplemented until W20. Two notes from the review: the R9 verifier's independence is procedural, not organizational (D7), and the D10 capacity ceiling (B.2.4) is stated in the user guide.

9. **Shell context menu: a cascading submenu with quick verbs [owner, 2026-10-16]** (owner test on Windows 11; plan D6, O10). The four flat Explorer verbs are replaced by one cascading submenu "MOCHI" (7-Zip style) in the classic menu (still under "Show more options" on Windows 11; the top-level menu stays 1.x). On `.mochi` files: Open with MOCHI, Extract here, Extract to a new folder; on every file and folder: Add to archive and Add to archive... . The two extract entries and Add to archive are **quick verbs**: they run at once in a small compact window (progress, then a one-line result with Show in folder and Close) and do not open the main window. This amends plan D6 "Nothing runs silently": a quick verb writes without a button press, under these limits. It never overwrites; if any name in the way exists, nothing is written and the extract plan for the same archive opens in the main window instead. Extract to a new folder never reuses a folder (`<name>`, `<name> (2)`, ...). Add to archive creates `<stem>.mochi` next to the item (one item) or `<parent folder>.mochi` (several; Explorer's one launch per item are collected within 500 ms of the last into one archive), never replacing a file (`<name> (2).mochi`, ...), with the Core profile and the Balanced preset and nothing else. An Encrypted archive asks for its passphrase in the compact window (once over IPC, never on a command line); there is still no passphrase option. The result wording follows §23.3: a new archive is reported as a local commit (`LOCAL_COMMITTED`), an extraction is not a verification, and neither is called backed up, safe, or preserved. Foreign archives keep the existing rule (the shell's extract entries are for `.mochi`; a foreign file opens in the read-only browser). No format byte, draft 2, or artifact in `docs/ratification/` changes.
10. **Extracting into a folder that already has some of the names: add or replace [owner, 2026-11-11]** (owner test: the conflict list "feels like a paragraph thrown at my face", and the only choice was to skip). This supersedes the plan's D3 decision of 2026-10-14 ("skip-only is accepted for 1.0"). §10.4 forbids only *silent* overwriting; these are explicit, per-extraction choices. The restore engine (C6, and the foreign-archive engine, D8) takes `OnExisting`: **Keep** (the default, unchanged: an existing name is `NAME_COLLISION` and a colliding folder's subtree is skipped; the CLI always uses it), **Merge** ("Add only new files": a folder that already exists is entered and what it lacks is restored; an existing file keeps its name and its bytes) and **Replace** (as Merge, and an existing *regular file* is moved to the Recycle Bin or the Trash, never deleted, so the user can get it back, and the restored file takes its name). Rules: a folder is entered only if it is a plain directory reached without following a link (Linux: opened `O_DIRECTORY | O_NOFOLLOW` relative to the held parent, and only if, like the root, other users cannot modify it; Windows: pinned, a reparse point refused); an entered folder is not counted as restored and gets none of the archive's attributes; a file is moved aside only after the restored file has been verified, just before it is published, and publication is still no-replace; a file where the archive has a folder or the reverse, a link, a device, and a folder that may not be entered stay `NAME_COLLISION` in every mode; if the Bin is unavailable the existing file is kept and the entry is reported. The preflight reports, without writing, which folders would be entered, which files exist, and why each remaining entry cannot be extracted; the desktop shows these as a question with **Add only new files** (the default), **Replace**, **Extract to a new folder** and Cancel. No format byte, draft 2, or artifact in `docs/ratification/` changes.

## Annex C. Document Lineage **[1.0 integration]**

| Document | Status |
|---|---|
| MOCHI design documentation v1.2 | Superseded by this specification. Retained for history |
| Multi-writer concurrency draft | Folded into 12.5 (Core) and Annex A (post-1.0) |
| Extended capabilities draft (remote, mount, FastCDC, snapshots, search, split) | Annex A |
| Unified v1.2 + extensions draft | Superseded. Its Appendix B frame placeholders are replaced by 8.2 |
| MOCHI Design Document v2.0 | Base text of this specification |
| `docs/dev/decisions.md` | Delivery plan and Definition of Done for 1.0 |
| `AGENTS.md` | Working rules for contributors and coding agents |
