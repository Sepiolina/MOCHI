# AGENTS.md — Working on MOCHI

Instructions for coding agents and human contributors. Read this before changing anything.

## What this project is

MOCHI is an archive format and archiver, shipped like WinRAR: a format library plus an end-user application.

- `crates/mochi-format` — framing, record envelopes, digests. Pure; no filesystem policy.
- `crates/mochi-core` — archive operations, catalog, recovery manifests, verification, repair, jobs, reports.
- `crates/mochi-cli` — the `mochi` binary.
- `crates/mochi-testkit` — fault-injecting storage and fixture builders.
- `apps/mochi-desktop` — Tauri v2 application (`src-tauri/` Rust backend, `ui/` frontend).

The product is **pre-1.0 and the wire format is not frozen**. Every build before 1.0 is "experimental / draft-compatible" (spec §28).

## Sources of truth, in order

1. `docs/spec.md` — the specification. Normative text wins over everything below.
2. `docs/ratification/` — frozen artifacts (frame registry, DDL, serialization, vectors) once they exist. Where one exists, it is more precise than the spec and wins for its topic.
3. `docs/implementation-plan.md` — phases, scope, and Definition of Done.
4. This file.

Older documents (v1.2 spec, the unified v1.2 draft, the v1.2 plan) are **superseded**. Do not implement from them. Spec §29.1 lists v1.2 claims that are known to be wrong.

If the spec does not answer a question, **do not invent an answer.** Stop, state the gap, and propose options with trade-offs, or add it to spec Annex B. A plausible guess baked into the wire format is worse than an open question.

## Commands

Live today (plan phases C1–C5 complete):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
ci/check-invariants.sh                       # mechanical checks for rules clippy can't express
cargo run -p mochi-cli -- --help
cargo run --release -p mochi-testkit --example c5_append_bench   # §27 append benchmark (C5)
```

Pinned `compile_fail` error codes are only checked on nightly (CI job `doc-nightly`):

```bash
cargo +nightly test --doc -p mochi-format
```

Fuzzing (nightly plus `cargo install cargo-fuzz`; targets `frame_walker`, `footer`, `envelope`, `object_decode`, `catalog_image`, `cbor_decode`, `manifest_decode`, `commit_decode`, `archive_open`):

```bash
cd fuzz && cargo +nightly fuzz run frame_walker corpus/frame_walker ../fixtures/golden/c1 ../fixtures/golden/c2 -- -max_total_time=60
```

Not available yet (arrive with the phase noted):

```bash
cargo run -p mochi-cli -- verify fixtures/golden/<file>.mochi --json   # verify: C7; whole-archive fixtures: C5+
```

Desktop (React + TypeScript + Vite, pnpm, Node 24 LTS — plan decision O1; scaffold arrives with D0):

```bash
# cd apps/mochi-desktop/ui && pnpm install --frozen-lockfile && pnpm tauri dev
```

## Invariants you must not break

These come from the spec. Code review rejects changes that violate them, regardless of test results.

**Wire format**
- Byte layouts, magic numbers, and field semantics follow spec §8 and the ratification artifacts exactly. They are not style choices. If one looks wrong, raise it; do not "fix" it in code.
- Frame registry constants live in one module in `mochi-format`. Never write a magic literal anywhere else.
- Every footer is a skippable frame (§8.4). Never emit bare bytes between frames.
- Physical frame length comes from the structural walk (§8.6). **Never** use `Frame_Content_Size` as a stored length. RLE blocks occupy one stored byte.
- The Zstandard frame checksum is 32 bits and supplementary. BLAKE3-256 is the integrity anchor.

**Representations and digests (§9)**
- Decoded bytes, encoded plaintext, stored payload, and stored-object bytes are distinct types. Do not add conversions that erase the distinction.
- Chunk content hash = decoded bytes. Stored-object hash = stored bytes. Do not hash the compressed-plaintext representation for either.
- Digest domain separators are defined once (`mochi-format/src/digest.rs`); do not inline them.
- Do not add a public constructor or byte accessor to `EncodedPlaintext` or `StoredPayload`. Their unreachability is what makes hashing the compressed-plaintext representation impossible; the `compile_fail` doctests in `repr.rs` guard it.
- Object and file-version IDs come from `OsIds` (random, O19) in production. Never derive an ID from content, stored bytes, or position.
- The file-content hash is plain BLAKE3 (O20); every other digest is domain-separated. Keep scopes in distinct, labelled fields everywhere (DDL, reports, JSON): an unseparated scope is only safe because digests are never compared across scopes.
- Data objects must carry `Frame_Content_Size` and the frame checksum (O21).
- Every canonical structure (recovery manifests, commit bodies, the archive descriptor) is **deterministic CBOR** in the restricted subset of spec Annex B.1 D2, through the one codec in `mochi-format`. Never serialize a canonical structure with a general-purpose CBOR library, and never accept input that does not re-encode to identical bytes. Opaque payloads (catalog images) carry the fixed binary envelope of spec Annex B.2.2 instead; both encodings enforce the same obligations (spec §8.3 amendment, D11).
- The writer's default profile is **not** TAR-compatible (spec D4); TAR compatibility is an explicit, per-archive choice at creation.
- New dependencies must be MIT/Apache-2.0-compatible (the project is MIT OR Apache-2.0, plan O23). The one exception is UnRAR, confined to `mochi-foreign`, with its notice in `THIRD-PARTY-NOTICES.md`.
- The catalog is in-memory SQLite; images move as bytes through `Storage` (O25). Never open SQLite on a file, `ATTACH`, or `VACUUM INTO` in `mochi-core` (`ci/check-invariants.sh` rule 8).
- **Hash-verify a catalog image before `Catalog::open_image`.** SQLite has no page checksums; a flipped bit in a stored value yields a different, fully valid catalog.
- Paths are byte components joined by `/`; Windows names are WTF-8 (O24). Never normalize, case-fold, or lossily convert a name that is used as identity.

**Publication and safety (§5, §12)**
- All I/O in `mochi-core` goes through the `Storage` trait. No direct `std::fs` calls in core logic — they make fault injection impossible.
- A partially written commit must never become the head. Follow the §12.2 step order, including syncs (`mochi_core::publish`; `sync_order_follows_spec_12_2` pins it).
- Every object a commit references is hash-verified **before** it is parsed. A commit references only bytes before its own commit frame.
- Never truncate a tail that is not *eligible* (`TailState::Uncommitted`; spec Annex B.2 D14). Eligibility is a conservative screen, not proof: a single corruption event can erase a later commit's footer markers. Truncation is explicit only, and is preceded by a verified quarantine copy unless the user waives it, which is recorded. A wrong verdict destroys a commit; refusing is always safe.
- After a failed sync the writer stops (`WRITER_POISONED`); never retry a sync and report success.
- Report `LOCAL_COMMITTED` after a local commit. Nothing in 1.0 may report `PRESERVED`.
- Verification (`verify`, `fsck`, desktop "Test archive") is read-only. Open storage read-only for it.
- Repair is `plan` then `apply`, writes to a new representation by default, and re-verifies (§22.2).
- Compaction never rewrites the only known-good representation in place (§18.2).
- GC deletes only after transitive marking from every root in §18.3, via quarantine, with an audit record.

**Reporting (§20)**
- Use only the §20.4 status values. A check that was skipped, unsupported, incomplete, or overdue is never `PASS`.
- An overall status never hides a failing dimension.
- Error codes are stable and registered in one place. Adding a code is fine; changing the meaning of an existing one is a breaking change.

**Parsing untrusted input (§8.5, §22.1)**
- Archives are untrusted. Bounds-check and overflow-check before every seek or allocation; respect configured limits.
- A magic-number match is a candidate, never proof.
- Record uncertainty; never invent namespace information during salvage.

## Rust conventions

- Library crates: no `unwrap`/`expect`/`panic!` on paths reachable from archive contents. Return typed errors carrying a stable code.
- `#![forbid(unsafe_code)]` in `mochi-format` and `mochi-core`. Any exception needs a justification comment and review.
- Secrets use `zeroize`; never `Debug`-print, log, or put them in error messages.
- Integer arithmetic on archive-derived values uses checked operations.
- Long operations are jobs: accept a progress sink and a cancellation token, return a typed report. Do not add a second progress mechanism.

## Tauri v2 rules (desktop app)

- **No format logic in the UI or in Tauri commands.** Commands are thin adapters: validate input, call `mochi-core`, return typed results.
- **Least-privilege capabilities.** Do not grant the webview broad filesystem, shell, or HTTP permissions. The user picks paths via the dialog plugin; Rust performs the I/O. Every new permission needs a reason in the PR.
- Keep the CSP strict. No remote content; no `eval`.
- **Archive-supplied strings are untrusted.** Render file names, labels, and comments as text only. Never inject them as HTML. The fixture set includes hostile names; keep it passing. In React this means: never `dangerouslySetInnerHTML` (lint error `react/no-danger`, and `ci/check-invariants.sh` greps for it), never build HTML strings, never pass archive strings to `href`/`src` attributes.
- **IPC types are generated** from Rust with `ts-rs` (O1). Do not hand-edit generated files; CI fails if they are stale.
- **Supported platforms are Windows 10 22H2+/11 and Ubuntu 22.04/24.04, x86-64 (O11).** macOS is not supported in 1.0; do not add macOS-specific code paths or claim macOS support in user-facing text.
- Progress streams over a Tauri `Channel`; CPU-heavy work runs off the async runtime.
- UI wording follows spec §23.3: no success styling for `UNKNOWN`/`OVERDUE`/`UNSUPPORTED`/`DEGRADED`; never say "backed up," "safe," or "preserved" after a commit; deleting a file removes it from the current snapshot, not from history; partial salvage is labelled partial.
- Passphrases cross IPC once, are not stored in frontend state longer than needed, and are never persisted unless the user opts into the OS credential store.

## Tests required with every change

- **Feature code comes with its Definition of Done test**, not just a happy path. See the plan's §8.1 table and spec §24.2.
- Corruption, recovery, dedup, or encryption code → the matching fault-injection or corruption test.
- Anything the spec makes a cost or timing claim about → a benchmark reported with the §27 decomposition (dataset, hardware, cache state, settings).
- Parser changes → extend a fuzz target and add a golden vector.
- Format-affecting changes → update golden fixtures deliberately, never by blanket regeneration, and explain the diff.
- Desktop behaviour → an E2E test via `tauri-driver` on Windows and Ubuntu. Client-Windows-only behaviour that Windows Server CI runners cannot reproduce → an entry on the release checklist.
- Foreign-format code (D8) → a hostile-archive fixture for each new rule, and a fuzz target for each format's listing path.

## Scope discipline

Active scope is MOCHI 1.0: Core, TAR-compatibility, Encrypted, and Redundancy profiles; the CLI commands listed in spec §23; the desktop app phases D0–D7.

**Not in 1.0** unless the plan is changed first: key files and public-key recipients for encryption (1.0 is passphrase-only, spec D3), split volumes / segmented storage, the Preservation profile and inventory service, concurrent preparation / multi-writer, remote HTTP/S3 access, mount, FastCDC, signatures and Merkle proofs, SFX archives, and *writing* any non-MOCHI archive format. If a task touches one of these, say so and point to spec Annex A before writing code. A 1.0 build must answer requests for these with `UNSUPPORTED` / exit code 4.


**Foreign archive formats (plan D8, O9).** The desktop app *reads* ZIP, 7z, RAR, and `.tar.gz`/`.tar`: browse, extract, test. Rules that code review enforces:

- Parsing happens only in the isolated helper process (`mochi-foreign`), never in the app process, `mochi-core`, or `mochi-format`. `unrar`/`unrar_sys` may appear only in that crate; `ci/check-invariants.sh` checks this.
- **Never write RAR**, and never use UnRAR code for anything but reading: its license forbids building a RAR-compatible archiver (plan O23). The `unrar` crates are labelled MIT/Apache, but the UnRAR source they vendor is RARLAB freeware; license tooling that reads crate metadata will miss this.
- Never call UnRAR's (or any library's) own extract-to-disk functions. Entries are streamed to the C6 restore engine, which alone writes files.
- Foreign-format results never use MOCHI verification wording or health dimensions: report the check that actually ran ("CRC32 matched").

## Pull request checklist

- [ ] Spec sections affected are cited in the description.
- [ ] No wire-format change, or a ratification artifact / Annex B entry accompanies it.
- [ ] Required tests from the section above are included and pass on Windows and Ubuntu.
- [ ] No new `unwrap`, `unsafe`, direct `std::fs` in core, or magic literals.
- [ ] Any new Tauri permission is justified.
- [ ] User-facing wording checked against spec §23.3.
- [ ] Open questions discovered are recorded (spec Annex B or plan §9), not silently resolved.
