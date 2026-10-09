# MOCHI 1.0 — Implementation Plan & Definition of Done

**Source of truth:** `docs/spec.md` (Specification revision 2.0). Section references like "§12.2" point there.
**Supersedes:** the v1.2 implementation plan (Phases 0–10) and its addendum (Phases 11–18).
**Product:** MOCHI 1.0 = `mochi-core` library + `mochi` CLI + MOCHI desktop application (Tauri v2), shipped the way WinRAR ships its engine and its GUI.

---

## 1. What Changed and Why the Plan Was Rebuilt

The v2.0 refinement is not an incremental edit. It changes the wire layout (framed footers, new frame registry, encrypted-object envelope), the metadata model (immutable records plus namespace operations instead of mutable `files` rows), the recovery model (SQLite-independent recovery manifests), and the operational contract (read-only verification, planned repair, per-dimension health, `LOCAL_COMMITTED` vs `PRESERVED`). Several v1.2 mechanisms the old plan was built around were incorrect; spec §29.1 lists them. The old Phase 0–2 work would have produced archives the new spec rejects, so the phases are re-cut rather than patched.

Three consequences drive this plan:

1. **The format is not frozen yet.** §28 says implementations MUST call themselves experimental until the ratification package exists. So the plan has an explicit **ratification track** that gates the 1.0 release, and every pre-1.0 build is labelled draft-compatible.
2. **Tauri v2 settles the core language: Rust.** The format library is a Rust crate that the CLI and the Tauri backend both link directly. The frontend framework is still open (§9, O1).
3. **A desktop archiver is not a preservation service.** 1.0 targets the Core, TAR-compatibility, Encrypted, and Redundancy profiles (§7.7). The Preservation profile — independent inventory, recovery copies — is out of 1.0 scope, and the product must not imply otherwise (§1.3, §23.3).

---

## 2. Product Architecture

### 2.1 Repository layout (proposed)

```text
mochi/
├── Cargo.toml                 # workspace
├── crates/
│   ├── mochi-format/          # framing, record envelopes, digests, frame walker. No filesystem policy.
│   ├── mochi-core/            # archive operations: create/append/read/verify/repair/compact/gc,
│   │                          # catalog, recovery manifests, jobs (progress + cancellation), reports
│   ├── mochi-cli/             # `mochi` binary: argument parsing, JSON output, exit codes (§23.2)
│   └── mochi-testkit/         # fault-injecting storage, fixture builders, golden-vector helpers
├── apps/
│   └── mochi-desktop/
│       ├── src-tauri/         # Tauri v2 Rust backend: thin commands over mochi-core
│       │   └── capabilities/  # least-privilege capability files
│       └── ui/                # frontend (framework: open decision O1)
├── fuzz/                      # cargo-fuzz targets
├── fixtures/golden/           # valid, corrupt, interrupted archives + expected reports
├── docs/
│   ├── spec.md
│   ├── implementation-plan.md
│   └── ratification/          # ratification package artifacts (§6)
└── AGENTS.md
```

**Dependency direction is one-way:** `mochi-format` ← `mochi-core` ← {`mochi-cli`, `mochi-desktop/src-tauri`}. Nothing in `mochi-core` knows about Tauri, and nothing in the UI knows about bytes.

### 2.2 Load-bearing design decisions

- **Storage abstraction from day one.** All file I/O in `mochi-core` goes through a `Storage` trait (read-at, append, sync-data, sync-directory, lock, truncate). Production uses the OS; `mochi-testkit` provides implementations that tear, reorder, drop, or halt writes. The §24.2 fault matrix is not testable without this, and retrofitting it is expensive.
- **Representation types.** Decoded bytes, encoded plaintext, stored payload, and stored-object bytes (§9.1) are distinct Rust types, and each digest function (§9.2) accepts only its own input type. This turns the chunk-hash-ordering mistake that has appeared in earlier drafts into a compile error.
- **One jobs layer.** Long operations run as `mochi-core` jobs that take a progress sink and a cancellation token and return a typed report. The CLI renders reports as text or JSON; the desktop app streams progress over a Tauri `Channel` and renders the same report. Identical semantics are guaranteed structurally, not by discipline (§23.4).
- **Stable error codes** are defined once in `mochi-core` and reused by reports, CLI output, and UI messages.

### 2.3 Candidate dependencies (verify license, maintenance, and fit before adopting)

| Need | Candidates | Note |
|---|---|---|
| Zstandard | `zstd` (libzstd bindings) | Frame *boundary* walking is our own code (spec §8.6); the library is used for encode/decode |
| BLAKE3 | `blake3` | |
| SQLite | `rusqlite` with bundled SQLite | Pin the SQLite version; published images must be self-contained (§10.5) — `VACUUM INTO` is a candidate for producing them |
| AEAD / KDF | RustCrypto `aes-gcm` or `chacha20poly1305`, `argon2`, `hkdf`; `zeroize` | Blocked on the crypto ratification item |
| Reed–Solomon | To be chosen against the ratified suite | The suite definition comes first; the crate must match it exactly, with vectors |
| TAR | `tar` | TAR-compatibility profile only |
| Testing | `proptest`, `cargo-fuzz`, `insta` (snapshot reports) | |
| Tauri IPC typing | `ts-rs` + thin typed `invoke` wrappers (decision O1) | Generated types so the UI and backend cannot drift; `tauri-specta` revisited when its Tauri v2 line is stable |
| Foreign archives (D8) | `zip` 8.x (MIT), `sevenz-rust2` (Apache-2.0, compressor feature off), `tar` + `flate2` (MIT/Apache), `unrar` (bindings MIT/Apache; **vendored UnRAR source is RARLAB freeware**, see O23) | All in the isolated helper process only. `libarchive` rejected: one large C attack surface, and it does not decrypt encrypted RAR or 7z |

---

## 3. WinRAR Feature Mapping

This table is how user expectations translate into MOCHI semantics. Where the semantics differ, the UI has to teach the difference rather than hide it.

| WinRAR concept | MOCHI 1.0 equivalent | Semantic difference the UX must carry |
|---|---|---|
| Add files to archive | `create` / `append` (new commit) | Every add is a commit in history, not an in-place edit |
| Delete from archive | Namespace `DELETE` in a new commit (§10.2) | Bytes stay in retained history until retention + GC allow removal (§16.3, §23.3 #5) |
| Extract / Extract to | `get` / restore | Collisions and unsupported names reported, never silently overwritten (§10.4) |
| Test archive | `verify` (read-only) | Result is per-dimension, and "unknown" is not "OK" (§20.3–20.4) |
| Repair archive | `repair plan` → approve → `repair apply` → re-verify | Output to a new file by default; partial salvage labelled (§22.2) |
| Recovery record | Redundancy profile (§7.4) | Bounded: repairs up to the suite's tested capacity, then says so |
| Password / encrypt file names | Encrypted profile (§7.3) | Lost passphrase = lost data; dedup under encryption leaks equality (§9.5) |
| Multi-volume archives | Segmented profile — **1.x**, not 1.0 | Open decision O5 |
| Archive history | Snapshots (Core) | New to WinRAR users; a differentiator — browse and restore any retained snapshot |
| Lock archive | Retained roots / holds | Protects history from GC; not a write lock |
| SFX (self-extracting) archives | **Out of scope** | Executable archives have distinct security and signing concerns; revisit post-1.0 |
| Opening ZIP/7z/RAR | **Read-only in 1.0** (browse, extract, test): ZIP, 7z, RAR, `.tar.gz` — phase D8, decision O9. Outside the MOCHI spec | Foreign archives get CRC-level checks only; "Test" on a ZIP must not look like "Test" on a `.mochi` (D8 rule 6). MOCHI never writes RAR (license) |

---

## 4. Release Strategy

| Stage | Label | Contents | Gate to exit |
|---|---|---|---|
| Alpha (0.1–0.x) | "Experimental — format will change; do not use as your only copy" | Core track through C7, desktop D0–D3 | Internal use only |
| Beta (0.9.x) | "Draft-compatible" | All 1.0 profiles, full CLI, desktop D0–D8 | Ratification track complete except independent-interop evidence |
| **1.0** | Stable, format frozen for the supported profiles | Everything in §8 DoD | §8.2 release gates all pass |
| 1.x | | Segmented/split volumes, FastCDC, signatures | Per-extension ratification (spec Annex A) |
| Later | | Preservation profile (inventory service), concurrent preparation, remote access, mount | |

A pre-1.0 archive is not guaranteed readable by 1.0. The writer embeds its draft identifier so 1.0 can reject it explicitly (§26) rather than misread it.

---

## 5. Core Library Track (C)

Each phase ends with a working, tested artifact. "Fault-matrix rows" refers to the §24.2 table; a phase is not done until its rows pass.

### C0 — Workspace, CI, and harness
- Cargo workspace per §2.1; CI on Windows and Ubuntu (decision O11; originally all three OSes); `fmt`, `clippy -D warnings`, tests, fuzz smoke runs.
- `mochi-testkit` with the `Storage` trait and first fault-injecting implementation.
- Report schema v0 and error-code registry skeleton.
- **Exit:** CI green on every supported platform (O11); a test can inject a halted write and observe it.

### C1 — Framing primitives
- Skippable-frame read/write; data-frame boundary walker per spec §8.6, including the RLE one-byte rule and reserved-block rejection.
- Frame registry constants (§8.2) in one module; draft record envelope (§8.3) with a version field.
- Framed footer (§8.4): 64-byte payload, domain-separated BLAKE3 digest over payload bytes 0–31 plus stored commit-frame bytes; validation of the preceding skippable header.
- Bounds, overflow, and allocation limits configurable (§8.5).
- **Exit:** golden vectors for every frame type; fuzz target for the walker running in CI. Fault-matrix rows: *false magic inside payload*, *oversized or malicious frame metadata*.

### C2 — Object model and digests
- Representation pipeline as distinct types (§2.2); digest functions per §9.2 scope, each with its own domain separator (final strings are a ratification item).
- Object records: identity, stored hash, stored length, decoded length, dependencies.
- **Exit:** one unit test per digest scope proving it hashes exactly its declared input; property test: encode → store → load → decode round-trips and every digest matches.

### C3 — Catalog and namespace
- Logical tables from §10.1 as working DDL (the ratified DDL replaces it later).
- Private journaled working database; separate "publish standalone image" step (§10.5).
- `PUT` / `DELETE` namespace operations with deterministic replay (§10.2); extent validation — gaps, overlaps, out-of-range, length mismatch (§10.3); holes for sparse files.
- Reversible path representation separating separators from name bytes; display and search normalization kept separate from identity (§10.4). Decide the Windows representation for unpaired UTF-16 surrogates.
- Verification helpers: SQLite integrity check, foreign-key check, MOCHI dependency checks.
- **Exit:** property tests for namespace replay (random op sequences replay to the same snapshot) and extent validation; published images open with no WAL/journal present.
- **Status (C3 done).** Working DDL: `archive_meta`, `commits` (ordering only; commit IDs arrive in C5), `objects`, `object_locations` (identity kept separate from location, §17), `chunks`, `chunk_dependencies`, `file_versions` (file/directory; symlinks and preserved attributes wait for D6/O6), `file_extents`, `namespace_ops`. **Deferred to their phases, because their columns depend on decisions those phases own:** `dictionaries` (O21, C8/C9), `key_envelopes` (C11/R5), `parity_groups` (C12/R6), `retained_roots` (C9), `search_documents` (C13). Also deferred: **delta images and checkpoint + delta replay** (§10.6). C3 publishes full images, which are checkpoints; per-commit delta images and their merge are C5/C6, where commits get framed. Namespace choices made explicit in `catalog/namespace.rs`: directories are explicit entries; deleting an absent path is an error, not a no-op; the chain is linear (Core has a single writer, §12.5).
- **Found in C3:** SQLite detects structural damage, but not a bit flip inside a stored value (no page checksums). Carried into C5 as a requirement: hash-verify an image before opening it.

### C4 — Recovery manifests
- Encoding: deterministic CBOR (decision O2 / spec D2). First deliverable: the strict subset codec in `mochi-format`, with RFC 8949 Appendix A vectors that fall inside the subset, a rejection vector for every excluded feature and every non-canonical form, and a fuzz target. Then the manifest CDDL schema. Parser does not link SQLite; parent-chain semantics (§11).
- Envelope payload encoding `1` = deterministic CBOR (resolves part of O16).
- Recovery scopes as a type: payload salvage, file recovery, snapshot recovery, historical recovery (§11.1).
- **Exit:** fault-matrix row *destroyed SQLite catalogs → recover the promised scope through manifests*, on multi-commit fixtures.
- **Status (C4 done).** `mochi_format::cbor`: the strict subset codec, byte-identical to `ciborium` on random values, with every RFC 8949 Appendix A vector inside the subset and a named rejection for every excluded feature. `docs/schemas/recovery-manifest-v0.cddl`: draft schema (R3). `mochi_core::manifest` (delta and snapshot manifests, closed schema, extent ordinals implicit) and `mochi_core::recovery` (rebuild from manifests found by scanning; trust flows only from the head; snapshot recovery only from a baseline in the verified chain; the four §11.1 scopes). New error code `RECORD_INVALID`. **Deferred:** symlink entries (kind 2, reserved) and storing attributes in the catalog: C6 (attributes are recovered from manifests but kept beside the catalog until then). Manifests are bare skippable frames; whether they also carry the record envelope is O16.
- **Found in C4:** a snapshot manifest defines its commit's state, so its agreement with history cannot be checked from inside the chain, and a forger who rewrites one manifest and re-links the rest produces a new, self-consistent history. Only a trusted head detects that. Carried into C5 below.

### C5 — Commit and single-file publication
- The ten-step protocol in §12.2: exclusive lock, head validation and interrupted-tail detection, content → recovery/metadata → commit → sync → footer → sync → directory sync where needed.
- Document per-OS durability assumptions (POSIX `fsync` on file and parent directory; Windows per decision O12: `FlushFileBuffers` on the file, publication of a new file by hard link then unlink (T21 decision, 2026-10-05; not `MOVEFILE_WRITE_THROUGH`), best-effort directory flush reported as degraded if it fails), per §12.2.
- Locate previous valid footers when the tail is incomplete; auditable, explicit tail truncation only under exclusive access.
- Commit status reporting: `LOCAL_COMMITTED` (Preservation states are unreachable in 1.0 and must say so).
- Cancellation before step 7 leaves the previous head valid.
- **Recovery takes its head from the footer-verified commit** (found in C4). The commit record references its recovery manifest by stored-object hash, so a verified footer → commit → manifest path is the trusted head that `recover_from_manifests` needs; without it, a consistently rewritten manifest chain is indistinguishable from the real one (pinned by `a_rewritten_history_is_only_detectable_against_a_trusted_head`).
- **Catalog images are hash-verified before they are opened.** SQLite pages have no content checksums: a flipped bit inside a stored value yields a different, fully valid catalog that no SQLite or MOCHI rule can detect (found in C3; pinned by `sqlite_checks_alone_cannot_detect_value_corruption`). The reader MUST check the metadata object's stored-object hash (O20), reached from the footer-verified commit, before calling `Catalog::open_image`, and the C5 exit tests MUST include a value-level bit flip in a published image being refused.
- **Exit:** fault-matrix rows *termination at each publication stage*, *truncation at every byte of small fixtures*, *torn/reordered/lost writes* (via testkit). Benchmark: append cost versus prior commit count across three orders of magnitude, reported with §27 decomposition.

- **Status (C5 done, with O26 open).** `mochi_core::commit`: commit records per a draft CDDL schema (`docs/schemas/commit-record-v0.cddl`), deterministic CBOR, commit ID = domain-separated BLAKE3 of the body without its ID key, recomputed on decode; unknown required features fail closed. `mochi_core::publish`: the ten §12.2 steps in order (trace-tested); head location from the EOF footer or, failing that, the last valid footer by forward scan, always reported (`HeadSource`, `TailState`); footer → commit → manifest → checkpoint with **every referenced object hash-verified before it is parsed** (the catalog image before `Catalog::open_image`); rollback of the writer's own unpublished bytes on error or cancellation before the footer (audited); `COMMIT_UNCONFIRMED` for any failure from the footer on; writer poisoned after any sync failure; explicit, audited truncation of a provably uncommitted tail, refusal of anything else; `recover_with_trusted_head` takes the manifest head from the footer-verified commit. Storage: `sync_directory` reports `Confirmed`/`Unconfirmed` (O12); per-OS durability assumptions documented in `storage/os.rs`. Five error codes added (`NO_VALID_HEAD`, `UNCOMMITTED_TAIL`, `TAIL_UNRESOLVED`, `COMMIT_UNCONFIRMED`, `WRITER_POISONED`).
- **Exit evidence.** `crates/mochi-testkit/tests/c5_publication.rs`: halt before every mutation of a commit × 9 crash models (keep-all, synced-only, four torn tails, three lost-write patterns), torn writes at every append, lying and failing syncs, truncation at every byte of a 3-commit fixture, a value-level bit flip in a published image refused (and shown to open in SQLite alone), destroyed catalogs recovered through the footer head, a substituted manifest chain refused, cancellation at every progress point, second writer and unlocked writes refused. Every acceptance is checked by rebuilding every file of the head and comparing with an independent model. Two rules were mutation-checked (disabling each makes its tests fail): the hidden-footer tail rule and the hash-before-open rule. Golden: `fixtures/golden/c5/` (14 commit-record vectors, one archive that must stay readable). Fuzz: `commit_decode`, `archive_open`. Benchmark: `docs/benchmarks/c5-append.md`.
- **Found in C5:** full checkpoints per commit make append time linear and **archive size roughly quadratic** in commit count (262 MiB for 1000 commits of 4 KiB each). Blocks real use until O26 is settled; carried into C6. Also carried: on Windows, creation is not yet by write-through rename (O12 note), so the first commit of a new archive reports directory durability as unconfirmed; and the Windows directory-flush code has not yet been compiled (no Windows toolchain where C5 was written), so Windows CI is its first check.

### C6 — Read path and extraction
- Open: footer → commit → checkpoint + deltas replay (§10.6); snapshot selection by commit.
- `list`, `get`, restore to directory with traversal rejection, collision and unsupported-name reporting, case-insensitive-filesystem detection, sparse-file handling, attribute restoration per O6 with exceptions reported.
- **Exit:** fault-matrix row *path traversal or naming collision*. Benchmarks per §27: footer lookup, catalog open, replay, selected-file read — each reported separately, with dataset, hardware, and cache state.

- **Status (C6 started 2026-10-05).** Opening with replay and snapshot selection by commit already exist (C5, B.2: `open_head`, `open_at_footer` with offsets from `commit_history`). New `mochi_core::read`:
  - `list(head, under)`: path, kind, logical length, and content hash, from the catalog alone.
  - `read_file` / `read_file_in`: streams one file to a writer as a job (progress, cancellation). Every chunk is hash-verified as stored and decoded bytes; holes are written as zeros and hashed. The whole logical stream must match the file-content hash, and extents are re-checked where their offsets are used.
  - **Output reaches the writer before the final comparison** (streaming), so it is unverified until `Ok`. The restore engine must write to a temporary file and publish only on `Ok`.
  - Tests: `c6_read.rs` (8), against the scripted history's independent model and the test kit's separate reassembly. They cover every commit's snapshot, an absent path or a directory refused with nothing written, a damaged chunk, cancellation, sparse files, and a file whose intact chunks do not match its content hash.
  - Mutations (9): 6 killed. A shorter zero block is an equivalent mutant. Dropping the extent re-check or the length check survives because the catalog already refuses invalid extents at insertion and at open (defense in depth, unreachable through the public API).
  - **Decision [delegated]:** a path absent from the snapshot is `INVALID_ARGUMENT` (exit 3), not a new code; a dedicated not-found code is for R7 if automation needs one.
- **Status (C6 restore engine, 2026-10-05).** `mochi_core::restore::restore` writes an opened commit, or one subtree with the directories above it, into a `RestoreDir`. New pieces:
  - `storage::RestoreDir`: byte names, nested directories, exclusive creation, no-replace publication.
  - `OsRestoreDir`: Unix names are the bytes as stored; Windows names are WTF-8 decoded to UTF-16 (O24).
  - `windows_name_issue`: reserved device names (with extensions, any case, superscript digits), forbidden characters, trailing dot or space, and names that are not representable.
  - Test kit `SimTree`: an in-memory destination that can be case-insensitive or apply Windows rules on any host.
  - **Rules** (delegated decisions; spec §10.4 and §23.3 #7 require reporting but leave policy open):
    - Nothing is ever overwritten or merged. An existing or case-folded name is `NAME_COLLISION`, and the earlier entry keeps its bytes.
    - Names are never altered. An unsupported one is `NAME_UNSUPPORTED`, and the entry is skipped.
    - A skipped directory's subtree is reported with its cause.
    - Every file is streamed to a temporary name, verified (chunks and file-content hash), synced, and only then published. A damaged file is absent, never partial.
    - Exceptions do not stop the job; only cancellation or a failing destination do.
  - Two error codes were added: `NAME_COLLISION` and `NAME_UNSUPPORTED` (draft, R7).
  - **Traversal** cannot be encoded: archive paths reject `.`, `..`, empty, and separator-bearing components. A catalog that holds `d/../f` is refused at open (`catalog` test `relationship_and_mochi_rule_violations_are_refused`).
  - **Tests** (`c6_restore.rs`, 11, plus Windows-rule unit tests, 27 names) cover:
    - the head;
    - case-insensitive collisions (no overwrite, no merge);
    - existing destination content;
    - unsupported names under Windows rules;
    - an entry named like a temporary file;
    - a damaged file left absent;
    - subtrees (top-level and nested);
    - cancellation, including before a directory;
    - the real filesystem, restored twice: the second run collides everywhere and keeps a locally edited file. On Windows the hostile fixture name is reported as unsupported.
  - **Mutations:** 15, all killed (two after adding the nested-subtree and directory-cancellation tests).
  - **Open (2026-10-05):** path-based `OsRestoreDir`; up-front case detection; the §27 read benchmarks. The first two are addressed by "C6 safety" below (Linux; Windows refused).
- **Status (C6 attributes, 2026-10-05).** `publish::promised_attributes` rebuilds an opened commit's promised attributes as appending does: the segment base's snapshot manifest, then each delta in order, each manifest hash-verified against its commit. A reintroduced version or a reachable version without attributes is `RECORD_INVALID`.
  - **How restore applies them:** files right after they are published; directories at the end, deepest first, so creating children neither disturbs a directory's time nor needs write permission it no longer has. Every attribute not applied is an `ATTRIBUTE_NOT_RESTORED` exception (new code), summarised as one finding per kind with a count and the first path.
  - **If attributes cannot be reconstructed** (for example a damaged snapshot manifest), content is still restored and verified, nothing is guessed, and the report says the attributes were unavailable.
  - **Rules (O6; the platform mappings are delegated decisions):**
    - Setuid and setgid are removed unless `RestoreOptions::restore_setid` is set, and the removal is reported. The sticky bit is kept.
    - **Unix:** the time (nanoseconds), then the owner (attempted; a refusal is reported, which is the unprivileged case), then the mode. The Windows read-only bit clears the write bits. Hidden and system are reported. The archive bit is ignored.
    - **Windows:** the time and the read-only bit (from the Windows bits, or from POSIX write bits). Hidden and system are reported, because they need `SetFileAttributesW` and `mochi-core` has no `unsafe` (Q63). A POSIX owner and the POSIX bits Windows cannot hold are reported.
  - **Tests:** `c6_restore_attributes.rs` (8) covers exact attributes for every entry, directory order (nested), setuid and setgid with and without the request, a refused owner summarised once, attributes unavailable after a damaged snapshot manifest, attributes from delta manifests, and on the real filesystem the time to the nanosecond, the mode, ownership both privileged and unprivileged, and Windows-authored bits on Unix. All restore tests also pass as an unprivileged user here.
  - **Mutations:** 11 run; 10 killed (one only when unprivileged, as CI runs). One is equivalent: an empty map instead of none applies nothing either way.
- **Status (C6 safety, 2026-10-06; owner decisions: race resistance, case detection, and collision preflight before writing; unsupported cases fail clearly before writing).**
  - **Race-resistant restore (Linux).** `OsRestoreDir` holds directory descriptors and never uses a path after opening the root: `mkdirat` (mode `0700`) then `openat(O_DIRECTORY | O_NOFOLLOW)`; files `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` mode `0600`; `renameat2(RENAME_NOREPLACE)` or `linkat`/`unlinkat` inside the held directory; attributes through an `O_NOFOLLOW` descriptor (`futimens`, `fchown`, `fchmod`), the promised modes only at the end. Nobody but the restoring user (and root) can rename entries meanwhile, because everything created is private and the root must not be modifiable by other users: a root not owned by the user or root, or group/other-writable without the sticky bit, is refused before anything is written (`UNSUPPORTED_FEATURE`, exit 4). A `-1` owner or group from the archive is reported, never passed to `chown`. Uses `rustix` (already a dependency; `process` feature added for `geteuid`); no `unsafe`.
  - **Windows (superseded 2026-10-06, see below):** at first refused with `UNSUPPORTED_FEATURE`; now restored through pinned directory handles.
  - **Case detection and preflight.** `RestoreDir::case_behavior` probes the root once (an exclusive `0600` probe file, looked up under its upper-case name without following links, then removed). `restore::preflight` then finds, before writing: sibling names the destination treats as one (first in path order wins; key `storage::case_fold_key`, Unicode lowercase), names it cannot hold, and top-level names already present (`entry_exists`, the filesystem's own lookup). `RestoreOptions::refuse_on_preflight_exceptions` ends the job with `NAME_COLLISION`/`NAME_UNSUPPORTED` and nothing written. The report records `case_behavior`. Exclusive creation stays the authority: normalization-only collisions (ext4 casefold also normalizes) are caught at creation, not in the preflight.
  - **Behaviour changes, by design:** an entry without POSIX attributes gets spec §10.4.1's `0644`/`0755` instead of the umask; if attributes are unavailable, entries stay private (`0600`/`0700`) and the report says so; a later entry that collides with an earlier one is skipped even if the earlier one then fails verification (the decision no longer depends on content).
  - **Tests:** `c6_restore_races.rs` (6 on Linux, plus 1 needing a casefold directory): planted symbolic links never followed (creation and attributes); **a directory swapped for a symbolic link mid-restore redirects nothing**; entries private until attributes apply; unsafe roots refused (including, as root, a user-owned root); case probe leaves nothing. `c6_restore.rs` gains two preflight tests (`SimTree`, every platform) and a refusal test on non-Linux. All restore and lock tests pass as root and as `nobody` here. Mutations: 7, all killed (path-based file creation, attributes following links, weakened root check, public directories, preflight ignored, no existing-entry check, no case folding); restored byte-identical (md5).
  - **Evidence still owed:** insensitive detection on a real case-folding filesystem. This container's kernel has no ext4 casefold; CI job `casefold` (new) mounts one and runs `c6_casefold_destination`, which fails without the directory. Windows CI must show the refusal and that everything else still builds there.
- **§27 read benchmarks (2026-10-06): recorded** in `docs/benchmarks/c6-read.md` (`examples/c6_read_bench.rs`), warm and guest-cold: footer lookup 3 µs warm / 0.23 ms cold; catalog open ~27 ms (456 KiB image); replay of 200 deltas ~15 ms warm / ~24 ms cold (derived: the API has no replay-only entry point); selected-file read 2.7 ms (4 KiB) and ~90 ms (64 MiB, ~700 MiB/s) warm. One VM, one dataset; the host cache is not controlled.
- **CI evidence (run 37408967555, Sepiolina/MOCHI#5, head `72524e2`): all 18 jobs green.** `windows-latest`: fmt, clippy, every test, invariants; `c6_os_restore_is_refused_where_unsupported` passed (Windows refuses before writing) and all four `q54_writer_lock` tests passed, including readers reading during an append. `casefold` on Ubuntu 22.04 and 24.04: ext4 casefold mounted (`lsattr` shows `F`), `c6_casefold_destination` ran (1 passed) — insensitive detection and the preflight hold on a real case-folding filesystem. Both power-loss jobs (G7) green with the lock file.
- **Status (C6 Windows restore, 2026-10-06; owner delegated the call).** `OsRestoreDir` on Windows pins the root and every directory it creates (read access, no `FILE_SHARE_DELETE`): none can be renamed or deleted, so none can be swapped for a junction while names resolve through it. Every open uses `FILE_FLAG_OPEN_REPARSE_POINT`, and a pinned directory that turns out to be a reparse point fails. Files are `CREATE_NEW`, published by hard link then unlink; attributes go through the entry's own handle. `std` only, no `unsafe`. Restore implementations are now one module per platform (`storage/os/restore_{linux,windows,unsupported}.rs`); `ci/check-invariants.sh` rule 1 allows `std::fs` there. **Concessions W1–W6** (ancestors not pinned, no ACL check, temporary files not pinned, hidden/system not set, durability unconfirmed, probe race affects only the case report) and the separate-crate remedy: `docs/c6-restore-platforms.md`. Tests: `c6_restore_windows.rs` (3) and the cross-platform OS restore and attribute tests, which run on Windows again.
- **C6 is complete [CI evidence, 2026-10-08]: run 37735586637, immerh8/MOCHI#12, head `e2301f7`, all 18 jobs green on `ubuntu-22.04`, `ubuntu-24.04`, and `windows-latest`** (fmt, clippy, every test, invariants; `c6_os_restore_is_refused_where_unsupported` in `c6_restore.rs`, and `c6_casefold_destination` in the two `casefold` jobs, exist at that commit). The items C6 carried for C14 (`mochi get`/`list`) are built (C14 status below); T29's truncation and limit flags are built, and its opt-in limit path (`create --exceed-default-limits`) is **not in 1.0**: decided 2026-10-08 (D16, Q10), the flag exits 4 `UNSUPPORTED_FEATURE`.
- **C6 addition (2026-10-06):** `restore::restore_selected` restores several subtrees in one job (each ancestor once), for `mochi get a b c`; `restore` is the one-subtree case of it.
- **C6 CI evidence (2026-10-05):** all jobs green on Ubuntu 22.04/24.04 (non-root runners, so the unprivileged ownership path) and `windows-latest`: the read API (run 37340859066, `f96b546`), the restore engine (run 37342327439, `f18b82c`), and attributes (run 37343548193, `0e172d9`). On Windows this covers WTF-8 names, `NAME_UNSUPPORTED` for the hostile fixture name, times on files and directories through backup-semantics handles, and the read-only bit.

### C7 — Verification, health, and reports
- Levels: structural, referential, stored integrity, content integrity, restoration (§20.1). Inventory, search, and disaster-recovery levels return `UNSUPPORTED` in 1.0 unless implemented.
- Report schema v1 per §20.5; health dimensions (§20.3); status values (§20.4); exit codes 0–4 with documented precedence (§23.2).
- Freshness per decision O8.
- Read-only guarantee: verification opens storage read-only, and a test hashes the archive before and after to prove it unchanged.
- **Exit:** fault-matrix rows *corrupted content object* (detection half), *older valid archive substituted* (to the extent O8 allows — otherwise the report must show freshness `UNKNOWN`, never `PASS`).
- **Status (C7 implemented 2026-10-06; CI evidence: run 37735586637, immerh8/MOCHI#12, head `e2301f7`, green on Ubuntu 22.04/24.04 and `windows-latest`).** `mochi_core::verify::verify` runs structural, referential, stored, content, and restoration levels over the whole retained history and never returns `Err`: every outcome is in the report. Inventory, search, and disaster recovery run nothing and report `UNSUPPORTED` (exit 4). `deep` (`fsck`) also opens every commit at its own footer and compares its namespace with the head catalog's replay of it. `check_catalog_contents` exposes the data-object and file-version checks for any catalog (for C8's re-verification). Report schema v1 (`docs/report-schema-v1.md`) adds `freshness_anchor` (D8). Two codes added: `REFERENCE_INVALID`, `FRESHNESS_FAILED` (exit 1).
  - **Decisions [delegated]:** (1) errors are classified once (`verify::classify`, exhaustive): evidence of damage is `FAIL`, a refusal `UNSUPPORTED`, I/O, limits, and cancellation operational (exit 3); `OUT_OF_BOUNDS` and `LIMIT_EXCEEDED` are operational as in Q31/Q32, and the referential level reports ranges outside the committed prefix as `REFERENCE_INVALID` before anything is read. (2) Integrity is `PASS` only at `stored_integrity` or deeper with every check run; structural and referential runs report `UNKNOWN` (exit 2), never `PASS`. (3) Recoverability is the D10.9 damage assessment, `FAIL` for any damaged data object (no parity in 1.0), `UNKNOWN` if data objects were not read. (4) Freshness: no anchor `UNKNOWN`; an anchor passes if the verified history contains it (for local history, at its recorded sequence), so a head that advanced still passes and a rollback or substitution fails. (5) Durability is `UNKNOWN` (not observable from bytes); an interrupted write is a warning, a tail that might hold a damaged commit (`TAIL_UNRESOLVED`) an integrity `FAIL`. (6) Key availability `PASS` for an unencrypted descriptor. (7) Searchability and retention `UNSUPPORTED` until C13 and C9, so the evidence rollup is never `PASS` in this build; the policy (integrity, recoverability, and freshness under D15) decides the exit code. (8) The restoration level reassembles every retained file version, not only the head's.
  - **Tests:** `c7_verify.rs` (16): every `fixtures/golden/c5` archive verifies as labelled (22 rejects exit 1, the pre-batch draft exits 4 per §26, 3 valid archives exit 0); every level on checkpoint-only and delta archives; read-only by storage trace (no write, sync, truncate, or lock) and by file hash through `OsReadStorage`; a flipped bit in a data object (FAIL at stored and deeper, `UNKNOWN` above) and in a catalog image (structural FAIL, recoverability `DEGRADED`); user and local-history anchors; the archive cut back to an earlier commit (`UNKNOWN` without an anchor, `FAIL` with one; a different archive `FAIL`); requested freshness without an anchor (exit 2); unsupported levels (exit 4); cancellation (exit 3); no valid head; eligible and unresolved tails; deep checks; referential defects (outside the prefix, at a non-data frame, wrong length, no location, missing dependency) and a retained version that fails restoration, on forged catalogs. **Mutations:** 11, all killed (two after adding the forged-catalog tests).
  - **W2: reference scope (Q64) [delegated 2026-10-08, K2].** `fsck` (`deep`) now recovers each replay segment from its baseline (`recover_baseline_at_footer`, D10.8) after the per-commit opens. Deltas apply in order, so recovering a segment's last commit succeeds exactly when every commit of the segment does: one recovery per segment; only on a failure is each commit recovered in turn, to name the first. A commit that opens from its image but fails baseline recovery is `REFERENCE_INVALID` (the baseline error's own code is in the message) with Recoverability `FAIL` and Integrity unchanged, so the exit is 1 through the policy; a commit that does not open at all is left to the damage findings that already cover it. Reading and opening are not refused; `verify` without `deep` is unchanged; no wire change. Spec Annex B D18 "reference scope" and checklist Q64 are decided. **Tests:** `q64_reference_scope.rs` (6): a forged delta that puts a version reachable only before the base; a forged new version whose extent names a chunk only reachable before the base (both open, read, and fail the real baseline recovery, and `fsck` reports each once with the commit named); the unforged fixture is clean; a deduplicating archive (reuse inside a segment, re-store across a checkpoint) and its compaction are clean; a damaged middle commit is damage, not a reference finding; cancellation is an operational stop. CLI: `crates/mochi-cli/tests/q64_fsck_command.rs` (1): `fsck --json` exits 1 with the finding, Recoverability `FAIL`, Integrity `PASS`; `verify` and `list` on the same file are unchanged; the file is byte-identical afterwards. Fuzz: `exercise_archive_open` now runs deep structural verification on every input (no panic, valid report, any `REFERENCE_INVALID` implies Recoverability `FAIL`). **Mutations:** 4 (skip the check, no Recoverability `FAIL`, also an Integrity violation, blame commits that do not open), all killed.
  - **Open for C7:** none. `health` (stored evidence, evidence age, `OVERDUE`) is built in C14 (W4, below).

### C8 — Recovery and repair
- The §22 ladder: expected head → previous footers → footer-history accelerator (N=16 proposed) → checkpoints and metadata chains → recovery manifests → parity → bounded salvage scan.
- Scanner safety rules (§22.1); uncertainty recorded, never invented.
- `repair plan` produces a JSON plan naming the source of every reconstructed object; `repair apply` requires the plan, writes to a new archive by default, re-verifies, and records unrecoverable files explicitly (§22.2).
- **Exit:** fault-matrix rows *corrupted latest footer*, *damage beyond parity capacity* (with C12), *missing shared dictionary*. A partial salvage is reported as partial in both report and exit code.
- **Status (C8 first slice, 2026-10-08; CI evidence: run 37735586637, immerh8/MOCHI#12, head `e2301f7`).** `mochi_core::repair`: `plan` (read-only, a job) and `apply`; `mochi repair plan|apply` (C14 below). **Owner decisions (2026-10-08):** (1) `repair apply` writes a **new archive** by the C9 rewrite path (new archive ID, D13 publication, IDs, stored chunks, and promised attributes preserved, the source never written); (2) an entry whose content or metadata cannot be recovered is **left out** of every repaired snapshot that held it and listed (path, version ID, snapshots, reason) in the plan and the report only: nothing in the new archive records the repair, so there is no wire-format change; (3) **ladder steps 1–4 only**: head, earlier footers (forward scan; the footer-history accelerator is not written by this build), catalogs, and snapshot manifests. Steps 5 (copies), 6 (parity, C12), and 7 (bounded salvage scan) are reported `not_attempted`, never as passed.
  - **Rules [delegated]:** trust flows from the head through validated parent links, and a break ends the chain (snapshots before it are still recovered when a trusted catalog holds their history, without commit IDs or times); commits are opened from the head down with every `open_at_footer` check, and each catalog supplies the snapshots no later one did; a version is kept only if it reads back verified and a verified manifest of a trusted commit records its promised attributes (manifests that disagree reject it; nothing is invented), and everything under a left-out directory is left out; retention is the head's (D10.10), mapped to the new sequences, and a hold on a lost snapshot is listed as lost. Unresolved retention (including a lost head) refuses `apply` unless the caller passes `accept_retention_loss`, recorded in the report. Outcome `complete` (exit 0) needs every snapshot and entry, carried retention, and a **clean tail** (an eligible tail, D14, is a screen, not proof); otherwise `partial` (exit 2); `nothing_recoverable` (exit 1) writes nothing. The plan is deterministic and is the approval: `apply` surveys again and refuses any difference. Before publication the new archive must reproduce the planned namespaces, attributes, and retention, and `verify` at restoration level with `fsck` depth must exit 0.
  - **Refactor:** compaction's per-snapshot copy (with the Q64 checkpoint forcing) is now `compact::Copier`, shared by `compact` and `repair::apply`; `publish::walk_back_tolerant` and `publish::recorded_writer_parameters` added.
  - **Tests:** `c8_repair.rs` (14) against a pristine copy as oracle: a clean archive (complete, ladder statuses, JSON round trip, every snapshot reproduced, retention identical); **corrupted latest footer** (DoD row: head by scan, partial for the tail); damaged chunks (each version left out under every path and snapshot, a rename included); a damaged head image (step 4, complete); a damaged delta covered by a later catalog (complete); a lost head (retention waiver required, holds and expiry dropped with it); retention carried and remapped across lost snapshots (a lost hold listed); attributes in no verified manifest; a chain break; forged manifests disagreeing about a version; a stale or edited plan; nothing recoverable (no head; damaged descriptor, D12); never replacing and cancellation. Unit: `filter` (subtree omission, byte-order neighbours) and attribute disagreement. CLI: `crates/mochi-cli/tests/c8_repair_command.rs` (4). Fuzz: `exercise_archive_open` now also checks every plan's consistency (every sequence recovered or lost, never both; omissions only in recovered snapshots; the outcome never claims more than listed; planning fails only for operational reasons). **Mutations:** 9 (tail rule, plan re-check, missing attributes, subtree omission, hold and expiry remapping, retention waiver, attribute conflicts, content check), all killed.
  - **Not done:** steps 5–7 (alternate copies, parity with C12, bounded salvage scan); *damage beyond parity capacity* waits for C12. *Missing shared dictionary* has nothing to exercise yet: this build writes no dictionaries (O21). A chunk with dependencies fails the plan's read check (`decode_verified`, `UNSUPPORTED_FEATURE`). **Decided [delegated 2026-10-08, K4]: refuse, do not omit.** `repair plan` (and so `apply`) fails with `UNSUPPORTED_FEATURE` (exit 4) when any recoverable version's read check fails that way: the content is intact but undecodable by this build, and `UNSUPPORTED_FEATURE` promises results are never partial or empty (§7.7, §26). Damage is still omitted as before. Test: `c8_repair.rs` `c8_undecodable_content_refuses_the_plan` (a forged commit introduces a chunk whose record declares a dictionary dependency; the control without the dependency plans cleanly; 1 mutation, the refusal removed, killed). A source descriptor that cannot be read makes every commit unreadable (D12), so this slice recovers nothing from it; writing a descriptor "labelled as reconstructed" (D12) needs the salvage slice and a wire decision.

### C9 — Checkpoint, compaction, dedup, GC
- Verified checkpoints (§18.1); compaction to a new representation, verified, then atomically swapped with the previous kept for a safety period (§18.2).
- In-archive dedup with the §9.5 consistency rule; cross-archive dedup off.
- Retained roots and holds; GC as `plan` then `apply`, marking transitively from all roots in §18.3, quarantine before deletion, auditable deletion batches.
- **Exit:** fault-matrix rows *compaction interrupted*, *GC overlaps publication*, *retention expires under legal hold*. Compacted output restores identically (file hashes and namespaces) to its source.

- **Status (C9 dedup, 2026-10-06; owner chose C9 for this session while C7/C14 proceed in Sepiolina/MOCHI#7).** Checkpointing (§18.1) already exists from Annex B.2: the B.2.3 trigger, forced checkpoints (`ArchiveWriter::request_checkpoint`), and D10.7 adoption. What it lacks is the `checkpoint` command (C14). New here: **write-path deduplication** (`publish::Dedup`, default `InArchive`; `CommitOutcome::dedup` reports chunks and bytes reused and candidates rejected).
  - **Rules:**
    - Lookups use only the head the writer validated under its lock (§9.5 rule 1). Identical chunks within one commit are each stored (rule 2): chunks are streamed before publication, so the buffered-transaction exception is not taken.
    - Candidates match on decoded length and chunk content hash. They are then read back, verified as stored and decoded, and compared byte for byte (§9.4). A failing candidate is never referenced: the chunk is stored anew and the candidate leaves the index.
    - Only unprotected chunks without dependencies are candidates; C11 must add the §9.5 rule 3 disclosure before encrypted archives deduplicate. Nothing is deduplicated across archives.
  - **Reference scope (found here, checklist Q64):** the index holds only chunks reachable at the head when it is built, plus chunks introduced since, and is rebuilt after each checkpoint. Referencing any chunk in the catalog image would make baseline recovery (D10.8) fail, because S(*b*) holds only what *b* reaches. A test shows this; the spec gap is recorded in Q64 with a recommendation.
  - **No wire change:** reuse by reference is already D10.4. Readers cannot tell a deduplicated archive from one written without dedup, except by the shared chunk IDs.
  - **Tests:** `c9_dedup.rs` (6) covers:
    - reuse in the same session and after reopening;
    - no reuse within one commit;
    - per-chunk partial overlap;
    - `Off`;
    - a damaged candidate never referenced, with the good copy reused next;
    - reuse limited to what baseline recovery sees, with baseline recovery and recovery from manifests reading every file exactly.

    The whole workspace passes with dedup on by default.
  - **Mutations:** 9. Seven were killed: no reset at a checkpoint, no index extension after a delta, no read-back, `Off` ignored, a rejected candidate kept, every error propagated, and an index over every catalog chunk (it fails baseline recovery with `EXTENT_INVALID`). Two survive as defense in depth: the byte comparison (needs a hash collision) and the committed-range check (the head catalog is verified at open). The file was restored byte-identical (md5).
  - **Retention, GC, and compaction** were open here on 2026-10-06; see the next entry.

- **Status (C9 retention, GC, compaction, 2026-10-07).** The owner made three decisions: retention is expiry plus legal holds; collection and compaction write a new archive with a new archive ID and provenance; the source is never removed automatically. They are recorded in spec Annex B **D18** (B.2.8). The representation is **[delegated]**, recorded there and below.
  - **Retention** (`mochi_core::retention`; `Transaction::expire`, `hold`, `release`):
    - The roots are the head, every commit not expired, and every held commit. Operations apply atomically and in order.
    - An invalid operation is `INVALID_ARGUMENT` before anything is written.
    - Retention is manifest state, like promised attributes. Recovery-manifest **schema 2** adds key 11: a delta's operations and a snapshot's complete state.
    - Schema 2 is written exactly when a manifest carries retention or provenance, so every existing manifest, vector, and archive keeps its schema-1 bytes.
    - Readers rebuild retention from S(*b*) plus the segment's deltas (`publish::segment_state`; D10.10). It flows through open, replay, baseline recovery, and the writer as one `SegmentState` with the attributes.
    - D10.7 adoption compares it, and `CheckpointTamper::SnapshotRetention` tests that comparison.
  - **GC plan** (`gc::plan`, read-only):
    - Retention is rebuilt from the manifests. Any failure except I/O or cancellation is `RETENTION_UNRESOLVED`, with no fallback.
    - Marking is transitive, from the roots, in one replay (`Catalog::replay_each`). Chunks with dependencies are refused.
    - The plan records its head, every collectable snapshot with its reason, the collectable chunk and version IDs, and totals.
  - **Compaction and `gc apply`** (`compact::compact`, `Keep::Every` or `Keep::Roots(plan head)`):
    - The source is passed as an open writer, so its lock is held throughout, and a stale plan is refused.
    - The output has one commit per kept snapshot, with object and file-version IDs preserved (O19). Stored chunks are copied byte for byte after a hash check. Attributes come from every delta manifest. Commit times are copied, and holds and kept expiries carry over.
    - Provenance goes in the new delta(0): schema 2, key 12 (`compact::read_provenance`).
    - Before publication (§18.2 steps 1–3): namespaces, attributes, retention, and provenance are compared, and every version is read back and verified (`verify_content`, on by default).
    - Publication uses D13 via the new `ArchiveWriter::build_in`; `create_in` is now a one-commit case of it.
    - Q64: a reference that a baseline replay of the current segment would not see forces a checkpoint.
  - **Fault-matrix rows covered (library level):**
    - *Retention expires under legal hold:* `c9_a_legal_hold_protects_an_expired_snapshot`, and `gc apply` carrying the hold.
    - *GC overlaps publication:* `c9_gc_never_overlaps_publication`.
    - *Compaction interrupted:* `c9_interrupted_compaction_leaves_the_source_and_no_archive` halts before every file mutation and every directory operation. The source stays byte-identical, and nothing is at the name, also after a power loss.
    - Compacted output restores identically to its source: the test kit's own reassembly, IDs, attributes, and times are compared at every kept snapshot.
  - **G2's GC item:** `c9_a_hold_added_after_the_last_checkpoint_survives` and `c9_gc_refuses_when_retention_is_unresolved` (CI evidence: run 37735586637, immerh8/MOCHI#12, head `e2301f7`; **G2 passes**).
  - **Tests:** `c9_retention.rs` (5), `c9_gc_plan.rs` (4), `c9_compact.rs` (6), and retention unit tests (2). Golden c4 gained 19 vectors (3 valid, 16 reject). `reject-manifest-schema-version` now uses schema 3. Existing valid vectors are byte-identical.
  - **Mutations:** 16 over retention, GC, and compaction; 15 killed. The namespace re-check before publication survives as defense in depth: only a writer bug reaches it. The files were restored byte-identical (md5).
  - **Not in this phase:**
    - The `checkpoint`, `snapshot` (retain, expire, hold, release), `gc plan`, `gc apply`, and `compact` commands and their reports (C14; built 2026-10-08, see C14).
    - Explicit removal of a kept source (a user action, C14).
    - §16.3's "elevated authorization and a delay" for retention reductions (deployment policy; CLI confirmation in C14).
    - Collecting chunks with dependencies (none written before C8, C9 dictionaries, or C11).
    - §27 benchmarks: the spec makes no cost claim for compaction or GC.

### C10 — TAR-compatibility profile
- Writer mode meeting every §7.2 constraint (no reference-only representations, no MOCHI-specific fragmentation, no external dictionaries unless the documented invocation supplies them).
- Interop tests against pinned tool versions: GNU tar, bsdtar/libarchive (also what Windows ships as `tar.exe`), and the `zstd` CLI.
- **Exit:** the documented tool/version list, with a CI job per tool; documentation distinguishes historical stream extraction from latest-snapshot restoration.

### C11 — Encrypted profile *(blocked on R5)*
- `0x184D2A59` encrypted-object envelope; compress-then-encrypt; key envelopes discoverable without decrypting metadata (§14.3).
- Nonce construction proven unique across retries, interrupted writes, restored backups, and re-encryption (§14.2).
- Suite (spec D3): XChaCha20-Poly1305, random 192-bit nonces; Argon2id passphrase derivation with parameters recorded per envelope. **Passphrase only in 1.0**: several passphrases may wrap the same DEK; no key files, no public-key recipients. `rekey` distinguishes rewrap (new passphrase, same DEK) from re-encrypt, with separate audit records.
- Secrets zeroized; never logged.
- **Exit:** wrong key fails closed with no partial output; nonce-uniqueness tests; stored-integrity verification works without keys while content-level checks report `UNKNOWN` for keyless runs; dedup-equality leak covered by a test that asserts it *exists* (so nobody "fixes" it in a way that breaks dedup).

### C12 — Redundancy profile *(blocked on R6)*
- Ratified Reed–Solomon suite; parity coverage policy explicitly covering content, metadata, manifests, dictionaries, commits, and key envelopes (§15.2); header ↔ `parity_groups` cross-check.
- **Exit:** ≤ m lost shards reconstruct bit-identically; > m returns an explicit unrecoverable result, never wrong bytes; parity survives loss of the catalog.

### C13 — Search
- Required: discovery by path, file identity, file version, and snapshot, rebuildable from metadata or manifests (§19.1).
- Optional: FTS5 index with per-document extraction and indexing status; coverage reporting (§19.3). 1.0 extractors limited to plain text unless others are sandboxed and resource-limited (§19.5).
- **Exit:** fault-matrix rows *index deleted*, *interrupted indexing*; zero results under partial coverage reported as partial.
- **Status (required part, 2026-10-08; CI evidence: run 37735586637, immerh8/MOCHI#12, head `e2301f7`).** `mochi_core::search::search` finds entries by name bytes, exact path or subtree, file version, file-content hash, and entry kind (criteria AND), in the head, one commit, every retained snapshot (Annex B D18 roots), or every commit. It reads the opened head's catalog only, in one replay, and no file content. Every result carries the §19.3 coverage: requested snapshots, the watermark (the head's seq and commit ID), the catalog's source, the snapshots searched, those opened separately, those unavailable with the reason, entries examined, and the four document counts (zero by construction for metadata). Rebuildable from manifests (§19.1): with a damaged image the head's catalog comes from its snapshot manifest (`CatalogSource::SnapshotManifest`), and snapshots before that segment are opened at their own footers; any that still cannot be read make coverage partial. `mochi search` (C14 below).
  - **Decisions [delegated]:** (1) names match as stored bytes (O24); the only search normalization is optional ASCII case folding, never Unicode normalization, and results carry exact bytes (§10.4). (2) Hits are per snapshot, so "which snapshots hold this version" is answered directly. (3) Partial coverage exits 2 (degraded evidence), 1 with `--require-complete` (the spec's example flag, §23.1); complete coverage exits 0 whatever the number of hits. (4) The retained scope needs the retention state; if it cannot be rebuilt the search refuses with `RETENTION_UNRESOLVED` (D10.10) rather than guess a scope; other scopes still answer. (5) Content search (`--content`) is refused with exit 4 (`UNSUPPORTED_FEATURE`), never answered from names.
  - **Tests:** `c13_search.rs` (10): name at the head and across snapshots (zero hits complete when the name was deleted); ASCII folding without altering identity; exact, subtree, and kind criteria; a path's history across snapshots; version and content hash against plain BLAKE3 of what was written (one content under two names); the retained scope after expiry and a hold; **index deleted** as a damaged image (the same hits as the intact archive, earlier snapshots opened separately); **zero hits under partial coverage reported partial** (every image and the first snapshot manifest damaged); unresolved retention refused; a commit after the head; cancellation; hostile and non-UTF-8 names. CLI: `crates/mochi-cli/tests/c13_search_command.rs` (4). **Mutations:** 6; 5 killed, 1 equivalent (a redundant clause in `Coverage::complete`, then removed).
  - **Open for C13:** "file identity" (§19.1) is the path in Core (O29, decided 2026-10-08), so there is nothing to index beyond it; FTS5 (optional) and the fault row *interrupted indexing* wait for an index; `verify --level search` (known-answer queries, §20 table) still reports `UNSUPPORTED`.

### C14 — CLI completion
- Every 1.0 command in spec §23 scope; post-1.0 commands exit 4.
- **Exit:** an end-to-end test per command, including JSON output and exit code; exit-code precedence documented.
- **Status (first slice 2026-10-06; CI evidence: run 37735586637, immerh8/MOCHI#12, head `e2301f7`, which includes the C9 commands, `search`, and `repair`; `c14_append_truncates_an_eligible_tail_only_on_request` ran in the Ubuntu and Windows jobs, so G8 passes).** Built: `create`, `append` (with `--delete` and T29's `--truncate-tail`, `--no-quarantine`, `--accept-unconfirmed-durability`), `list`, `get` (to a directory or `--stdout`), `snapshot list`, `verify`, `fsck`, `restore-test`, and the global `--json`, `--limit NAME=VALUE` (T29), `--state-dir`, `--no-local-history`. Reference, rules, and concessions K1–K8: `docs/c14-cli.md`. The filesystem import is in `mochi-core` (`import`, `storage::os::OsSourceTree`) so the desktop app shares it; the CLI keeps the D8 local head history (`heads.json`, outside every archive). Tests: `crates/mochi-cli/tests/c14_commands.rs` (12, real filesystem), plus `exit_codes.rs` updated for the built commands. Still `NOT_IMPLEMENTED`: `snapshot retain`, `search`, `health`, `repair`, `checkpoint`, `compact`, `gc`, `rekey`, `dump-index`.
- **Status (C9 commands, 2026-10-08).** Built: `snapshot retain` (alias `hold`), `snapshot expire --confirm`, `snapshot release --confirm`, `checkpoint`, `gc plan [--output]`, `gc apply --plan --output`, `compact --output`; `snapshot list` shows retention. CLI rules (confirmation for retention reductions, the saved plan re-checked in full under the source's lock, no replacement of an existing name, the source kept): `docs/c14-cli.md` rules 6–8. `gc::PlanHead` is now deserializable. Also fixed: `create` stopped compiling after C9 added `WriterOptions::dedup`. Tests: `crates/mochi-cli/tests/c14_c9_commands.rs` (5). Mutations: 2 (plan re-check, `--confirm`), both killed. Still `NOT_IMPLEMENTED`: `search`, `health`, `repair`, `rekey`, `dump-index`.
- **Status (`search`, 2026-10-08).** Built: `mochi search ARCHIVE [PATTERN] [--snapshot HEAD|retained|all|SEQ] [-i] [--path P | --under P] [--version ID] [--content-hash HASH] [--kind file|dir] [--require-complete]` over C13's `search`, with §19.3 coverage in text and JSON; `--content` exits 4. `docs/c14-cli.md` rule 10. Still `NOT_IMPLEMENTED`: `health`, `repair`, `rekey`, `dump-index`.
- **Status (`repair`, 2026-10-08).** Built: `mochi repair plan ARCHIVE [--output PLAN]` and `mochi repair apply ARCHIVE --plan PLAN --output NEW [--accept-retention-loss]` over C8's `repair` (status above). `docs/c14-cli.md` rule 11. Still `NOT_IMPLEMENTED`: `rekey` (`dump-index`: W3 below; `health`: W4 below).
- **Status (`dump-index`, W3, 2026-10-08 [delegated, K6]).** `mochi dump-index ARCHIVE [--snapshot SEQ] [--table NAME]...` over a new `Catalog::dump` / `table_counts` / `table_names` (`mochi_core::catalog::dump`, always compiled; `logical_dump` stays for the tests). The catalog is the opened commit's (head or `--snapshot SEQ`), reached through `open_head` / `open_at_footer` like every read, so it is hash-verified first and a damaged image is replaced by the snapshot manifest's catalog with `catalog_source` saying `snapshot-manifest` (the same string `search` uses). Table names come only from `sqlite_schema` and are quoted; a name that is not there is `INVALID_ARGUMENT`, so no SQL, no `sqlite_schema`, and no case-folding can be reached. Rows are ordered by every column in turn (the primary-key order); values keep their SQLite type; JSON as in K6 (`{"tables": {NAME: {"columns", "rows"}}}` plus `commit`), with TEXT that is not UTF-8 as `text_hex` and BLOBs as `blob_hex`; text mode prints a count per table and, with `--table`, the rows through `render::text`. `docs/c14-cli.md` rule 12. **Tests:** `c14_dump_index.rs` (6: every DDL table appears; `commits` equals `snapshot list`; `file_versions` equals `search --snapshot all`; `objects` equals the catalog API's IDs; `--snapshot` and `--table` selection and order; unknown tables, SQL, and quoting tricks refused with the archive untouched; a hostile name exact as hex in JSON and never raw in text; the damaged-image fallback says `snapshot-manifest`; a missing archive is an error), core unit tests (4: DDL oracle, typed values and order, refusals, subset order, no change), a CLI unit test for hostile TEXT cells, and `exit_codes.rs` updated. **Mutations:** 7 (row order, name check, BLOB shown as text, raw text on a terminal, source label, `--snapshot` ignored, missing counts), all killed. **Not done:** no `--where` or paging; a large catalog is dumped whole.
- **Status (`health`, W4, 2026-10-08 [delegated, K5]).** `mochi health ARCHIVE [--policy FILE.json]` over a new `mochi_core::health` (no I/O, shared with the desktop app): `EvidenceRecord`, `HealthPolicy`, and `assess(&Inputs, &HealthPolicy) -> Report` (report schema v1, D15 through `Report::conclude`; the plan's `assess(evidence, policy, current, now)` takes one `Inputs` struct because freshness, the unreadable-line count, and the "local history off" reason also come from the caller). The CLI keeps the log (`state.rs`: `evidence/<archive id>.jsonl`, append-only, torn lines cannot swallow the next record) and the head history; `verify`, `fsck`, `restore-test`, and a complete `repair apply` append to it; `--no-local-history` neither records nor reads. Freshness is judged by the same `verify::judge_freshness_of` that `verify` uses (extracted, behaviour unchanged). Rules and format: `docs/c14-cli.md` rule 13 and concessions K9, K10; spec §23 note. **Decisions made under the delegation, beyond K5's text:** (1) a dimension a run reported `UNKNOWN` was not assessed and is ignored for it, so a later structural `verify` cannot erase an earlier stored-integrity `PASS`; (2) a recorded `FAIL` stands whatever its age and head, and even when the policy does not name that kind of evidence (K5's rule 2, "head differs → `UNKNOWN`", applies to non-`FAIL` records only: otherwise appending a commit would forget a damaged object); (3) a kind named in `max_age_days` is *required*, so naming `restore_test` requires a restore; (4) `restore-test` is recoverability evidence only, and only when complete (`PASS`) or when an entry failed its integrity check (`FAIL`); (5) `repair apply` is evidence for the new archive only when complete; (6) while any evidence line is unreadable, no dimension can be `PASS`; (7) evidence dated after the clock is `UNKNOWN`; (8) the evidence record gains `finding_codes`, so a `FAIL` carries its stable codes; (9) key availability is `UNKNOWN`, not `verify`'s unencrypted `PASS`, because `health` does not read the descriptor; (10) the report's `level` is `structural` (the only look at the archive is its head), with the `scope` saying no check was run. **Tests:** `mochi_core::health` (20 unit tests with an injectable clock: every rule above, the strict age boundary, the policy file's strictness, record round-trips), `state.rs` (2: per-archive append and order; a damaged and a torn line), `commands.rs` (1: what a restore proves), `c14_health.rs` (12, real filesystem: no evidence; verify then health; append makes it unknown; a recorded failure stands; overdue; bad policy files; a damaged line; `--no-local-history`; restore-test; repair apply complete and partial; freshness and rollback like `verify`; a failed restore-test; wording), `exit_codes.rs` updated. **Mutations:** 17 (a failure only at the same head, a non-strict age limit, `UNKNOWN` counted as assessed, freshness without an anchor, unreadable lines not blocking a pass, only the first required kind, future evidence trusted, restore-test feeding integrity, `verify` recording nothing, a partial repair recorded, an incomplete restore recorded as `PASS`, a torn line swallowing the next record, damaged lines not counted, the local anchor ignored, an unnamed kind's failure ignored, `--no-local-history` not announced): 16 killed and 1 equivalent (restore-test assessing integrity: a second guard, `EvidenceKind::feeds`, already excludes it, and that one is killed). A mutation run found the unnamed-kind rule (2 above) was missing: a `restore-test` failure was invisible under the default policy. **Not done:** `health` does not report an unresolved tail or read the descriptor (K10); the evidence log is never pruned (K9); no `--require-complete`-style flag; the report schema has no `health` marker (R7 may add one).

---

## 6. Ratification Track (R) — gates 1.0

These are the §28 artifacts. They live in `docs/ratification/` and are versioned. Each is written *from* working code and golden vectors, then frozen.

| ID | Artifact | Informed by | Needed before |
|---|---|---|---|
| R1 | Final frame registry and version mapping (incl. decision O1/D1) | C1 | Beta |
| R2 | Record-envelope layouts | C1–C4 | Beta |
| R3 | Canonical commit and manifest serialization; domain separators | C2, C4, C5 | Beta |
| R4 | Complete SQLite DDL and constraints | C3 | Beta |
| R5 | Cryptographic suite definition and test vectors | C11 prototype | C11 completion |
| R6 | Erasure-coding suite definition and test vectors | C12 prototype | C12 completion |
| R7 | Report schema and stable error codes | C7 | Beta |
| R8 | Golden valid, corrupt, and interrupted archives | All C phases | 1.0 |
| R9 | Independent reader/writer interoperability results | R1–R8 | 1.0 |
| R10 | Segmented-location and publication rules | — | 1.x (Segmented is post-1.0; 1.0's interop claim is scoped to its profiles — decision O7) |

**R9 is the most commonly underestimated item.** It requires a second implementation written from the spec alone — realistically a minimal read-only verifier in another language. Budget for it explicitly.

---

## 7. Desktop Application Track (D) — Tauri v2

### D0 — Shell and plumbing *(after C0)*
- Tauri v2 app scaffold: React + TypeScript + Vite, pnpm (decision O1); strict CSP.
- Capabilities: least privilege. The webview does not get broad filesystem access; file access happens in Rust after the user picks paths through the dialog plugin.
- Typed IPC contract (generated TS types preferred). A job runner bridging `mochi-core` jobs to Tauri: progress over `Channel`, cancellation commands, CPU-bound work off the async runtime.
- **Exit:** a sample job streams progress and cancels cleanly on Windows and Ubuntu (O11), verified by `tauri-driver` E2E.

### D1 — Archive browser *(after C6)*
- Open `.mochi`; tree and list views of a snapshot; file details (size, hashes, attributes); snapshot history timeline.
- All archive-supplied strings rendered as text only (§23.3 #9). A test archive containing names like `<img src=x onerror=…>` is part of the fixture set.
- **Exit:** 100k-entry archive browses without loading the whole namespace into the webview (virtualized lists, paged IPC).

### D2 — Create and add *(after C5)*
- Drag-and-drop onto window; add dialog with profile options (compression preset, TAR-compat per O4, and later encryption and recovery record); progress and cancel.
- Post-commit messaging says "committed," never "backed up" (§23.3 #6).
- Deletion flow explains namespace-vs-history (§23.3 #5).

### D3 — Extract *(after C6)*
- Extract all / selected / to…; collision dialog; unsupported-name report; attribute exceptions shown.

### D4 — Test, health, and repair *(after C7, C8)*
- Per-dimension health view; no success styling on `UNKNOWN`/`OVERDUE`/`UNSUPPORTED`/`DEGRADED`; export report JSON.
- Repair wizard: show plan → explicit approval → progress → re-verification result; output to a new file by default; partial salvage labelled.

### D5 — Encryption, recovery record, search *(after C11–C13)*
- Passphrase entry (not persisted; optional OS keychain opt-in), clear "lost passphrase = lost data" warning, dedup-leak disclosure when both are enabled, rekey flow.
- Recovery-record options with honest capacity wording.
- Search with visible coverage.

### D6 — Operating-system integration
- `.mochi` file association (Tauri bundle configuration), plus `.zip`/`.7z`/`.rar`/`.tar.gz` associations as an opt-in in the installer, never taken over silently (D8); single-instance handling so opening a second archive routes to the running app.
- **Shell context menus are a separate per-OS workstream, not a Tauri feature:** Windows classic verbs via installer registry entries (the Windows 11 top-level menu additionally needs an `IExplorerCommand` handler with package identity); Ubuntu's default file manager (Nautilus) via a scripts or extension mechanism. Scope decision O10.

### D7 — Distribution and quality
- Installers (O11): Windows NSIS (per-user install, WebView2 bootstrapper for machines without it), signed; Ubuntu `.deb` built on 22.04, declaring its webkit2gtk-4.1 dependency. MSI for managed enterprise deployment is post-1.0 unless requested.
- Auto-update via the Tauri updater with signed update artifacts; signing keys stored outside the repo.
- Accessibility (keyboard navigation, screen-reader labels, contrast), i18n scaffolding, and crash-report policy (opt-in only).
- E2E testing: WebDriver via `tauri-driver` + WebdriverIO on Windows and Ubuntu, i.e. every supported platform (O11). Windows 10/11 client-only behaviour not reproducible on Windows Server runners goes on a documented release checklist.

### D8 — Foreign archive formats, read-only *(after C6, D3; RAR blocked on O23)*

Decision O9. The desktop app opens ZIP, 7z, RAR, and `.tar.gz`/`.tgz` (plus plain `.tar`) to **browse, extract, and test**. These formats are outside the MOCHI specification; nothing here changes the `.mochi` wire format. Rules, each with its reason:

1. **Read-only, permanently for RAR.** Writing RAR is excluded by the UnRAR license, which forbids using it to build a RAR-compatible archiver. Writing ZIP/7z/tar would be a second archive product with its own correctness surface; MOCHI's write path stays `.mochi` only.
2. **Formats and libraries.** ZIP incl. ZIP64 and AES/ZipCrypto decryption (`zip` 8.x); 7z incl. AES-256 (`sevenz-rust2`, compressor feature disabled); RAR 4 and RAR 5 incl. encrypted headers (`unrar`, which vendors RARLAB's UnRAR C++); `.tar.gz`/`.tgz`/`.tar` (`tar` + `flate2`). Other tar compressions, ISO, CAB, and so on are out unless requested.
3. **Parsing runs in an isolated helper process**, never in the app or `mochi-core`, with memory, time, and output limits. UnRAR is C++ parsing hostile input; the Rust crates are not held to AGENTS.md's no-panic rule and may panic or over-allocate on malformed input. A crash, panic, or limit breach in the helper becomes a reported error; it cannot take down the UI or touch an in-progress `.mochi` commit. This is the same principle spec §19.5 applies to search extractors. Isolation also keeps UnRAR out of every other binary (licensing, O23).
4. **One extraction engine.** Entries from foreign archives are restored through the C6 engine, so every §10.4 protection applies unchanged: traversal and absolute-path rejection, symlinks never followed and rejected if they point outside the destination, hard links and device nodes skipped and reported, Windows reserved names (`CON`, `NUL`, …) and alternate-data-stream syntax rejected, case collisions reported, nothing overwritten silently. **UnRAR's own extract-to-disk functions are never used**: they bypass these rules, and that code path has had path-traversal vulnerabilities. The helper streams entry bytes; only the engine writes files.
5. **Decompression-bomb limits** on total output, per-entry size, entry count, and compression ratio, configurable, each breach reported by entry.
6. **Honest integrity reporting (§20.4, §23.3).** ZIP, 7z, and RAR 4 carry CRC32 per entry; RAR 5 carries CRC32 or BLAKE2sp; `.tar.gz` has gzip CRC32 over the whole stream and nothing per entry. "Test" reports exactly which check ran per entry ("CRC32 matched"); it never uses MOCHI's verification wording, success styling, or health dimensions. Recoverability, freshness, and retention are `UNSUPPORTED` for foreign formats. ZipCrypto-encrypted entries are labelled weak encryption.
7. **Encryption.** Passphrases follow the same rules as MOCHI's (cross IPC once, never persisted without opt-in). A wrong password fails closed with no partial output. Archives with encrypted names (7z, RAR 5) ask for the password before listing.
8. **Volumes and variants.** Multi-volume RAR (`.partN.rar`, `.rNN`) is supported, since UnRAR handles it and it is the common RAR case. 7z split volumes (`.7z.001`…) are plain byte splits and are read as one concatenated stream. Split/spanned ZIP and self-extracting `.exe` archives return `UNSUPPORTED` with a clear message (SFX is out of scope, plan §3).
9. **Surface.** Desktop only in 1.0; the engine lives in its own crate (`mochi-foreign`) and helper binary so a CLI surface can be added later without new semantics (§23.4). `mochi-core` and `mochi-format` never depend on it (checked by `ci/check-invariants.sh`).

- **Exit:** the D8 Definition-of-Done row in §8.1; a pinned corpus of real archives from current 7-Zip, WinRAR, Info-ZIP, and GNU tar opens and extracts bit-identically; fuzz targets for each format's listing path run in CI.
- **Not decided here:** "convert to `.mochi`" (open a foreign archive and commit its contents as a new `.mochi`). It reuses both halves and would be a natural follow-on; it needs its own decision.

---

## 8. Definition of Done

### 8.1 Per area

An area is done when every criterion is met by an automated, repeatable test in CI — not a demo.

| Area | Done when… | Spec | Fault-matrix rows |
|---|---|---|---|
| Framing | Every registry frame type round-trips; walker rejects reserved blocks and bounds violations; standard `zstd` skips all MOCHI skippable frames | 8.1–8.6 | False magic; oversized metadata |
| Digests | Each §9.2 scope has a test proving its exact input; representation types prevent cross-use at compile time | 9 | — |
| Catalog & namespace | Replay is deterministic under property testing; extent validation rejects all four defect classes; published images need no WAL/journal | 10 | — |
| Recovery manifests | Promised scope recovered with every SQLite catalog destroyed | 11 | Destroyed SQLite catalogs |
| Publication | Every kill point yields old-complete or new-complete, never mixed; no false durability acknowledgement | 5.1, 12.2 | Termination at each stage; truncation at every byte; torn/reordered/lost writes |
| Read & extract | Traversal rejected; collisions reported; decomposed benchmarks published | 10.4, 27 | Path traversal / naming collision |
| Verification | Read-only proven by before/after hash; per-dimension results; exit codes and precedence documented | 20, 23.2 | Corrupted content object (detect) |
| Freshness | Substituted older archive never reports freshness `PASS` | 5.7 | Older valid archive substituted |
| Recovery & repair | Ladder order honoured; repair via plan/apply; partial salvage labelled; output re-verified | 22 | Corrupted latest footer; missing shared dictionary |
| Compaction & GC | Interrupted compaction leaves the original recoverable; GC never deletes a reachable or held object; deletion batches auditable | 18 | Compaction interrupted; GC overlaps publication; legal hold |
| TAR profile | Pinned tool/version matrix passes in CI | 7.2 | — |
| Encryption | Wrong key fails closed; nonce uniqueness tested across retry/restore; keyless stored-integrity verification works; dedup leak documented and tested | 14 | — |
| Redundancy | ≤ m reconstructs bit-identically; > m explicit failure | 15 | Damage beyond parity capacity |
| Search | Rebuild matches expected coverage; interrupted indexing visible | 19 | Index deleted; interrupted indexing |
| Concurrency (Core) | Second writer blocked or rejected; never a mixed publication | 12.5 | Concurrent conflicting commits |
| Desktop app | Every §23.3 requirement has an automated test on Windows and Ubuntu; report shown in UI equals CLI JSON for the same archive | 23.3 | — |
| Foreign archives (D8) | Hostile-archive corpus (traversal, absolute paths, symlink escapes, bombs, reserved Windows names, duplicate/case-colliding names) extracted with zero writes outside the destination; helper crash or limit breach reported, app unaffected; wrong password fails closed; CRC-only results never shown as verified | — (product) | Path traversal / naming collision (reused) |

Rows not listed for 1.0 — deleted primary archive, original machine unavailable, key-service unavailable, corrupted inventory, stale writer resumes — belong to the Preservation and concurrent-preparation profiles. In 1.0 their checks must return `UNSUPPORTED`, and that behaviour is itself tested.

### 8.2 1.0 release gates

1. Every §8.1 row passes in CI on Windows and Ubuntu (O11).
2. Ratification artifacts R1–R9 published; the format is frozen for Core, TAR-compat, Encrypted, and Redundancy.
3. R9 interop results show the independent reader verifying the R8 golden set.
4. The draft-to-1.0 boundary is enforced: pre-1.0 archives are rejected explicitly or migrated per §26, never misread.
5. User documentation covers: a `.mochi` file is not a backup (§1.1); the encrypted-archive limits on tool fallback and self-healing without keys; the dedup-under-encryption leak; what "Test archive" does and does not prove.
6. Signed installers and signed updates for Windows and Ubuntu.
7. All Annex B decisions that block 1.0 scope are recorded.
8. A project license is chosen that permits distributing UnRAR (O23). **Met:** MIT OR Apache-2.0, with UnRAR's notice in `THIRD-PARTY-NOTICES.md`.

These release gates are numbered 1–8. The Annex B.2 evidence gates G1–G9 (spec Annex B.2.6, `docs/b2-implementation-checklist.md`) are a separate series.

---

## 9. Open Decisions

Spec Annex B holds the format-level decisions (D1–D9). Product-level decisions tracked here:

| ID | Decision | Blocks |
|---|---|---|
| O1 | **Decided:** React + TypeScript (strict) on Vite, the standard Tauri v2 template; pnpm with a frozen lockfile; Node 24 LTS pinned. IPC types generated from Rust with `ts-rs` (stable) plus thin hand-written typed `invoke` wrappers, with CI failing if generated types are stale. `tauri-specta` rejected for now: its Tauri v2 line is still a release candidate (2.0.0-rc.25, May 2026), and a 1.0 IPC contract should not rest on one. React-specific rules: `dangerouslySetInnerHTML` is banned by lint (`react/no-danger` as an error) so archive-supplied names are always rendered as text (§23.3 #9); large listings use a virtualized list (`@tanstack/react-virtual`) with paged IPC (D1); unit tests with Vitest + React Testing Library; E2E with `tauri-driver` + WebdriverIO | D0 |
| O2 | **Decided (= spec D2, Annex B.1): deterministic CBOR**, a restricted RFC 8949 §4.2.1 subset (integer map keys, no floats, tags, or indefinite lengths), with CDDL schemas; readers reject anything that does not re-encode to identical bytes. **The same encoding serves the canonical commit body (R3)**, so the format has one canonicalization rule. Rejected: canonical JSON (RFC 8785 numbers are IEEE doubles, so lengths above 2^53 are not exact); Protobuf (deterministic mode is not canonical across implementations); a purpose-built format (every implementer writes a parser from scratch, and there is more spec to get exactly right). Implementation: our own strict subset codec in `mochi-format` (pure, fuzzed), cross-checked against a CBOR library in tests, rather than trusting any one library's notion of "deterministic" | C4, C5 |
| O4 | **Decided (= spec D4): TAR compatibility is opt-in, per archive, fixed at creation.** The default keeps dedup, dictionaries, and fragmentation. With TAR compatibility on, generic tools show the historical stream, not the latest snapshot, which is a confusing default for a product whose point is snapshots. The desktop create dialog offers it as a named option ("Readable with standard zstd + tar") with its trade-offs stated | C10, D2 |
| O5 | **Decided (= spec D5): split volumes stay in 1.x.** The Segmented profile and R10 are out of 1.0 | Scope |
| O6 | **Decided (= spec D6, Annex B.1).** POSIX: permission bits, numeric uid/gid, mtime with nanoseconds; setuid/setgid only on explicit request (restoring them from an untrusted archive is a privilege-escalation risk); uid/gid only with privilege, otherwise reported. Windows: `READONLY`, `HIDDEN`, `SYSTEM`, `ARCHIVE` and mtime. Symbolic links are an entry type (target stored as bytes; never written through; an exception where the platform cannot create one, e.g. Windows without developer mode). Hard links become separate files (dedup recovers the space). Not promised: xattrs, ACLs, ADS, owner names, atime, ctime, creation time. C3's `file_versions` gains a `symlink` kind and an attributes table in C6 | C6 |
| O7 | **Decided (= spec D7): yes, scoped per profile.** 1.0's interoperability claim covers Core, TAR-compatibility, Encrypted, and Redundancy; R10 waits with Segmented. The second implementation is a **read-only verifier in Python written from the spec alone**, in its own directory, sharing no code (Python's standard `sqlite3`, plus BLAKE3, zstd, and CBOR from PyPI). Ideally written by someone, or a fresh session, that has read only the spec | 1.0 gate |
| O8 | **Decided (= spec D8): layered, source always reported.** No anchor → `UNKNOWN`. User-supplied expected head → `PASS`/`FAIL`. Otherwise the client keeps the last-seen head per archive ID locally (opt-out available) → `FAIL` on rollback or substitution. First sight is `UNKNOWN`, never `PASS`. The JSON report names the anchor used (`none`, `user`, `local-history`) | C7 |
| O9 | **Decided (= spec D9):** the desktop app **opens ZIP, 7z, RAR, and `.tar.gz`/`.tgz` (and plain `.tar`) read-only**: browse, extract, test. It never creates or modifies them. Phase D8 has the scope and rules; the reasoning is recorded there. Foreign formats are outside the MOCHI specification: none of this changes the `.mochi` wire format | D8 |
| O10 | **Decided.** Windows: classic context-menu verbs (Open with MOCHI, Extract here, Extract to folder, Add to archive) via installer registry entries; on Windows 11 they sit under "Show more options". The Windows 11 top-level menu needs an `IExplorerCommand` handler with package identity: 1.x. Ubuntu: file associations and "Open with" via the `.desktop` file only; Nautilus extensions need an extra package and change with GNOME versions: 1.x | D6 |
| O11 | **Decided: supported platforms are Windows and Ubuntu; macOS is not supported in 1.0.** Minimums, x86-64 only: **Windows 10 22H2 and Windows 11**; **Ubuntu 22.04 LTS and 24.04 LTS**. Reasons: Tauri v2 needs webkit2gtk 4.1, first packaged in Ubuntu 22.04, which is therefore the floor, and the `.deb` is built on 22.04 so its glibc requirement matches; Windows 10 costs nothing extra because Microsoft services WebView2 on Windows 10 22H2 until at least October 2028 even though the OS itself left support in October 2025. Revisit both floors on those vendors' dates (Windows 10: October 2028 WebView2 horizon; Ubuntu 22.04: end of standard support, April 2027). ARM64 on either OS is post-1.0. Consequences: CI runs Windows and Ubuntu 22.04/24.04 only; installers are NSIS (Windows) and `.deb` (Ubuntu); `tauri-driver` covers both platforms, so every §23.3 requirement gets an automated E2E test and the macOS manual-checklist carve-out is gone. Known gap: GitHub-hosted Windows runners are Windows Server, so Windows 10/11 client behaviour (shell integration, WebView2 bootstrap) needs a documented release checklist or a self-hosted runner | D7 |
| O12 | **Decided: claim only what the Windows APIs document.** **Creation decided as spec D13 (Annex B.2): exclusive temporary file, published without replacing an existing file. Windows stays `Unconfirmed` until gate G6. Implementation: `docs/b2-implementation-checklist.md`; evidence: gates G6–G7.** Appending to an existing archive (the common path) needs no directory flush: `FlushFileBuffers` on the file also flushes its metadata, including size. Creating or replacing a file uses `MoveFileExW(… MOVEFILE_WRITE_THROUGH)`, documented not to return until the move is on disk, then a best-effort `FlushFileBuffers` on a directory handle. **Superseded for creation by the T21 decision (owner, 2026-10-05):** Windows publishes the temporary file by hard link then unlink, the same no-replace mechanism as Linux's fallback, so `mochi-core` keeps `forbid(unsafe_code)`; directory durability stays `Unconfirmed` (checklist Q63). That last step is reported to work on NTFS but is not documented as a guarantee, so if it fails the publish is reported as **degraded ("directory durability unconfirmed")**, never as durable. `sync_directory` stops being a silent no-op on Windows in C5. **C5 note:** C5 creates archives in place (`create_new`), not by rename, so the premise for accepting a successful best-effort flush does not hold yet; until creation-by-rename lands, Windows reports the creating commit's directory durability as unconfirmed even when the flush succeeds (stricter than this decision, never looser) | C5, D2 |
| O13 | *(raised in C0; **decided** in C2)* **Exit-code precedence.** When several conditions occur, the first that applies wins: **1** failure > **3** error > **4** unsupported > **2** degraded > **0** ok. Failure outranks everything because definite evidence of damage must never be masked (§5.4, §20.3), even by an I/O error later in the same run; error outranks unsupported and degraded because a compromised run makes any remaining pass untrustworthy; unsupported outranks degraded because a *required* check could not run at all, whereas degraded evidence exists but is weak. Implemented as `mochi_cli::exit::combine`, with tests. The JSON report stays authoritative. **Remainder decided as spec D15 (Annex B.2): evidence rollup, policy result, and exit code are separate; any `FAIL` exits 1; exit precedence 1 > 3 > 4 > 2 > 0; RFC 3339 UTC timestamps with nine fractional digits. Implementation: `docs/b2-implementation-checklist.md` (C7); evidence: gate G9.** | C7, C14 |
| O14 | **Decided: MSRV 1.89** (`File::try_lock`); toolchain pinned to 1.91 | — |
| O15 | *(raised in C0)* Error-code names in `mochi-core/src/error.rs` are drafts written to give reports and the CLI something stable to carry; the spec only shows two illustrative codes (§20.6). Final registry is ratification item R7 | C7 |
| O16 | *(raised in C1; **decided as spec D11, Annex B.2, with a §8.3 amendment: CBOR-native envelopes, plus binary envelope v0 (80 + 8n bytes) for opaque payloads. Replaces the 68-byte draft below. Implementation: `docs/b2-implementation-checklist.md`; evidence: gate G1**)* The §8.3 envelope layout is unspecified. `envelope.rs` implements a **draft** (68-byte header, schema version 0) carrying the listed fields, but only payload encoding `0` and integrity scope `0` ("unspecified") are defined; other values are rejected. A real integrity scope needs R3 (canonical serialization, domain separators). Which frame kinds carry an envelope is also a guess: every skippable kind except the footer and reserved kinds **C4 update:** payload encoding `1` is deterministic CBOR (spec D2). Recovery manifests are currently bare `RECOVERY_MANIFEST` skippable frames with no envelope; deciding whether they get one is still part of this item, and adding one would change their stored-object hashes and the C4 vectors. **C5 update:** commit records and catalog checkpoints are also bare skippable frames (`COMMIT_RECORD`, `METADATA_DELTA`); the same envelope decision covers them. | R2, R3 |
| O17 | **Decided: a footer must follow its commit frame.** The format is append-only; nothing needs a footer before its commit, and allowing it would weaken tail validation. Current walker behaviour stands | — |
| O18 | *(raised in C1; **defaults aligned with frame payload budgets in spec Annex B.2.3**: image 268,434,864 B (was an unreachable 1 GiB); any frame 269,484,032 B (was 4 GiB); commit frame 8 + 64 KiB; at most 64 required features. Default writers stay within default reader limits; larger limits need opt-in at creation. These defaults now bound archive capacity (B.2.4, unmeasured until gate G5). Walker comparison against other decoders: still open for C1 sign-off)* Walker strictness and defaults. The walker enforces RFC 8878 `Block_Maximum_Size` (min of window and 128 KiB) and rejects the reserved header bit, matching libzstd on the frames tested, but has not been compared against other decoders. Default limits (256 MiB skippable payload, 4 GiB frame, 128 MiB window, 4M blocks, since C2 256 MiB decoded object length, since C3 1 GiB catalog image, and since C4 CBOR depth 64 and 16M items) are placeholders for §8.5's "configurable" | C1 sign-off |
| O19 | *(raised and **decided** in C2)* **Object identity generation: a uniformly random 256-bit ID per object from the OS CSPRNG** (`mochi_core::object::OsIds`). Rejected: IDs derived from content (on encrypted archives the ID sits in the unencrypted envelope and would leak content equality even with dedup off, which §9.5 makes opt-in; and two encodings of the same content would share one ID with different records, which §10.2 defines as corruption); IDs equal to the stored-object hash (change on recompression or re-encryption, so not stable under repacking, §29.1 #11). Consequences: identity is stable across compaction and relocation; an independent reader needs no derivation rule; fixtures inject a deterministic source; a retried write gets fresh IDs, and the abandoned ones are unreferenced and collected by GC. An RNG failure is an error, never a fallback. The catalog (C3) must still enforce uniqueness and report a duplicate as corruption. **Apply the same rule to file-version IDs in C3** | C3 |
| O20 | *(raised and **decided** in C2)* **Digest separation.** (1) **The file-content hash is plain BLAKE3-256, unseparated**, so it equals `b3sum` of the restored file: users can check restorations with a stock tool, the R9 reader needs nothing MOCHI-specific for it, and the value survives migration to a later wire generation (§26). Accepted cost: a file beginning with a MOCHI separator has a file-content hash equal to that scope's hash of the remainder; harmless because digests are never compared across scopes (enforced by `Digest<S>` types; C3 DDL and reports must keep scopes in distinct labelled fields). (2) **The §9.2 metadata-object and recovery-manifest hashes are the stored-object hash of those objects**, not separate scopes: same input, and stored bytes already begin with the frame magic that identifies the object kind, so separate separators would add nothing except two digests per control object. All other scopes keep their draft separators (final strings: R3) | R3 |
| O21 | *(raised and **decided** in C2)* **Data-object profile.** Every data object is exactly one Zstandard frame that (a) **must carry `Frame_Content_Size`**, so structural checks and catalog-less salvage (§22) learn decoded length without decompressing; (b) **must carry the frame checksum**, the only content check a salvage scan has for an unencrypted chunk once the catalog is gone (still supplementary to the chunk hash, §8.5); (c) **has `Dictionary_ID` equal to the dictionary's own embedded ID, or 0 with none**: the record's dependency is authoritative, the 32-bit frame field is a salvage hint and a consistency check only, and disagreement is a content-integrity failure. "No compression" is not a separate encoding but the same frame made of raw blocks: an encoding names the decoder a reader needs, not the writer's effort. Stock `zstd` accepts all of this, so the TAR profile (§7.2) is unaffected. Readers reject frames missing (a) or (b) as malformed | C8, C9 |
| O22 | *(raised and **decided** in C2)* **Integrity failures exit 1**, whichever command found them (`STORED_INTEGRITY_FAILED`, `CONTENT_INTEGRITY_FAILED`): `get` refusing a corrupt object is a verification failure just as `verify` finding it is. No error code maps to 0; a test enforces both | — |
| O23 | **Decided: MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`; `license` set in every crate). Permissive, so shipping RARLAB's UnRAR in the D8 helper is possible (UnRAR is freeware, not open source, and generally treated as GPL-incompatible). UnRAR's required notice is in `THIRD-PARTY-NOTICES.md`. The copyright line says "The MOCHI Project contributors"; replace it with a legal entity if you have one. New dependencies must be MIT/Apache-compatible, and the release build must generate a full third-party license list (D7). Not legal advice | D7, D8 |
| O24 | *(raised and **decided** in C3; the plan's C3 item "decide the Windows representation for unpaired UTF-16 surrogates")* **Path identity is the bytes of each name component; Windows names are stored as WTF-8.** A path is a list of components (none empty, `.`, `..`, or containing `/` or NUL), stored joined by `/`, which is reversible because `/` can never be a name byte. POSIX names are kept byte-for-byte, including non-UTF-8. Windows UTF-16 names are encoded as WTF-8: identical to UTF-8 for valid Unicode, and a reversible encoding of unpaired surrogates. So the same valid-Unicode path has one identity whichever OS added it; each OS round-trips its own names exactly (tested for all 65,536 single UTF-16 units); and a name one OS cannot represent is reported as unsupported at restore (C6), never altered. Rejected: storing raw UTF-16LE with an OS tag (the same path from two OSes would be two different entries); lossy conversion to UTF-8 (irreversible). Display and search normalization never change identity | — |
| O25 | *(raised and **decided** in C3)* **The catalog is an in-memory SQLite database; images move as bytes.** Published images come from `sqlite3_serialize`, are opened with `sqlite3_deserialize`, and are stored and read through `Storage` like any object. This keeps fault injection complete (AGENTS.md), makes "no WAL or journal beside a published image" true by construction (§10.5), and lets `open_image` check the header before SQLite parses anything. `ci/check-invariants.sh` rule 8 forbids file-backed SQLite in `mochi-core`. **Trade-off:** the whole catalog is held in memory; for very large catalogs a SQLite VFS over `Storage` is the escape hatch, an implementation change, not a format change. Revisit with the C6 benchmarks | C6 |
| O26 | *(raised in C5; **decided as spec D10, Annex B.2**: the recovery manifest is the delta; a checkpoint commit binds its delta, its catalog image, and a complete snapshot manifest, verified equivalent before adoption; baseline recovery needs the baseline commit record but not SQLite or any earlier manifest; single-frame manifests, with oversized ones rejected and no new head; provisional writer defaults α = 1, F = 1 MiB; no delta-count cap. Schemas: commit-record-v1, recovery-manifest-v1. Implementation: `docs/b2-implementation-checklist.md`; evidence: gates G1–G5)* **What a metadata delta is.** Spec §8.2 allocates `0x184D2A50` to "metadata delta or checkpoint" and §10.6 replays "checkpoint plus subsequent ordered deltas", but no section defines a delta's contents. C5 writes a full checkpoint on every commit (valid under §10.6; commit-record metadata kind 1 is reserved for deltas and read as unsupported). Measured cost: append time linear and archive size roughly quadratic in history. Options: (a) a delta is a small SQLite image holding only the commit's new rows, merged on open; (b) the recovery manifest *is* the delta (one canonical structure instead of two; replay applies manifests to the last checkpoint); (c) a dedicated CBOR row-delta format. Each also needs a checkpoint interval policy (§18.1). Recommendation to evaluate first: (b), since the manifest already carries everything replay needs and is already hashed and chained | C6, R3, R4 |
| O27 | *(raised in C5; **decided as spec D12, Annex B.2**: immutable, at offset 0, bound by hash from every commit (commit key 10); no in-place conversion to Encrypted in 1.0. Schema: archive-descriptor-v0. Implementation: `docs/b2-implementation-checklist.md`; evidence: gate G1)* **Archive descriptor contents.** §8.2 says readers MUST validate the descriptor (`0x184D2A57`), and no section defines it. C5 writes none; identification currently rests on the footer magic, the domain-separated footer digest, and a closed-schema commit record carrying the archive ID and draft schema version. Decide its contents (profile, TAR-compat flag fixed at creation per D4, draft/wire identifiers) and where it sits (offset 0 is natural, but a reader must not need it to find the head) | R1, R2, C10 |
| O28 | *(raised in C5; **decided as spec D14, Annex B.2, with a §12.2 amendment**: external report; eligibility is a screen, not proof; quarantine before truncation by default (sidecar `.mochiq`, schema tail-quarantine-v0); `--no-quarantine` and `--accept-unconfirmed-durability` are separate, recorded waivers. Implementation: `docs/b2-implementation-checklist.md`; evidence: gate G8)* **Where the tail-truncation audit record lives.** §12.2 requires an "auditable recovery action" and does not say where. C5 returns a typed `TailTruncation` to the caller (and keeps it in the writer's audit log); the CLI/desktop must persist it in their report (C7/C14). Writing it into the archive would need a frame kind; none fits | C7, C14 |
| O29 | *(raised in C13; **Decided (a) [delegated 2026-10-08, K1]**)* **What "file identity" is (§19.1).** §19.1 requires discovery by path, file identity, file version, and snapshot, and §11 lists "file identities" among what manifests preserve, but nothing defines an identity distinct from a path or a version: the Core catalog and manifests hold paths, file-version IDs, and content hashes, and a rename is a delete plus a put (§10.2). Options: (a) **identity is the path** in Core; discovery by identity is a path's history across snapshots (what C13 builds now, `search --path P --snapshot all`); no rename tracking. (b) **A persistent file ID** carried by every version (a new catalog column and manifest key, so a wire change with an Annex B entry and golden-fixture updates), kept across replacement and, if a rename operation is added, across renames. (c) Content hash as identity: rejected, since two distinct files with the same bytes would be one "file". **Decided (a):** in Core, file identity is the path; discovery "by file identity" is a path's history across snapshots, as `mochi search --path` already does. No new identifier and no wire change; a separate identity that survives renames is post-1.0, if ever. Nothing claims identity discovery beyond (a) | C13; R3; R4 |

---

## 10. Mapping From the Previous Plan

| Old phase | Where it went |
|---|---|
| 0 Frame I/O | C1 (rewritten: framed footer, structural walker) |
| 1 Write path | C3 + C5 |
| 2 Read path | C6 |
| 3 Appendability | C5 |
| 4 Corruption Layers 0–2 | C7 + C8 |
| 5 FEC | C12 |
| 6 Compaction | C9 (now writes a new representation) |
| 7 Dedup | C9 |
| 8 Encryption | C11 |
| 9 CLI | C14 |
| 10 Performance | Folded into each phase's benchmarks under §27 rules |
| 11 Transactions | Post-1.0 (concurrent-preparation profile) |
| 12 Logical archive model | Core now: stable logical IDs are part of §10.1 from C2 onward |
| 13–18 Remote, FUSE, FastCDC, Merkle/signatures, FTS5 depth, split | Spec Annex A; 1.x or later |
