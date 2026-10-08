# Next work: decisions and workstreams (2026-10-08)

Status after C8 merged (immerh8/MOCHI#12, head `e2301f7`, CI run 37735586637 green on Ubuntu 22.04/24.04 and `windows-latest`). This file hands the next phases to implementation sessions. It does two things:

1. **Makes the open calls** (section 1), so that no workstream has to decide anything that changes the wire format or a product rule. Each call is the owner's delegated decision of 2026-10-08, marked **[delegated 2026-10-08]** where it is recorded.
2. **Splits the remaining work into workstreams** (section 3) small enough for one session and one PR each, with the files they touch, the tests that prove them, and a kickoff prompt.

Sources of truth stay as AGENTS.md says: `docs/spec.md`, then `docs/ratification/`, then `docs/implementation-plan.md`. This file does not override them. Every call below must be written into the spec or plan by the workstream that implements it, and then it is the record. This file is only the hand-off.

---

## 1. Calls made now

Each call names who records it and where. "Wire" means a stored byte or field changes.

| # | Question | Call | Why | Recorded by / where |
|---|---|---|---|---|
| K1 | **O29: what "file identity" is (§19.1)** | **Option (a): in Core, identity is the path.** Discovery "by file identity" is a path's history across snapshots, as `mochi search --path` already does. No new identifier, no wire change. A separate identity that survives renames is post-1.0, if ever. | Core renames are delete plus put (§10.2), and nothing stores an identity distinct from a path or version. Inventing one now would be a wire change with no 1.0 user. | W1: plan §9 O29 → Decided |
| K2 | **Q64: a delta referencing a chunk or version that a baseline replay of its segment cannot see** | **`fsck` (deep verify) reports it as `REFERENCE_INVALID` under Recoverability (`FAIL`). Opening and reading do not refuse it.** Writers keep never producing one (already true). | The bytes are intact and image-based opens work; only baseline recovery (D10.8) breaks. That is a recoverability failure, which is `fsck`'s job, and refusing to read intact data would be worse for users. | W2: spec Annex B D18 "Open" bullet → decided; code in `verify` |
| K3 | **D16 / Q10: `create --exceed-default-limits`** | **Not in 1.0.** The flag exits **4** `UNSUPPORTED_FEATURE`, the rule for features outside 1.0 (today it is `NOT_IMPLEMENTED`, exit 3). Gate G4's opt-in item moves to 1.x, and G4 is judged on its other items. | No 1.0 user needs archives larger than the default capacity (B.2.4), and the feature has five unanswered sub-questions. Shipping less is the safe call. | W1: Annex B D16 → "Decided: 1.x"; checklist G4; `docs/c14-cli.md` K8 |
| K4 | **Repair over content this build cannot decode (dependencies: dictionaries, keys)** | **Refuse, do not omit.** `repair plan` fails with `UNSUPPORTED_FEATURE` (exit 4) when any recoverable version's check fails as `UNSUPPORTED_FEATURE`. Damage is still omitted as now. | `ErrorCode::UnsupportedFeature` promises "results are never partial or empty" (§7.7, §26). Leaving out data that is intact but undecodable would be silent loss dressed as damage. | W1: `repair.rs` survey; plan C8 "Open" → decided |
| K5 | **`health` evidence and policy (C14, §20, §21; "local evidence only")** | See W4: evidence is an append-only JSON-lines log per archive ID in the CLI state directory, written by `verify`, `fsck`, `restore-test`, and `repair apply`. The policy file is **JSON** (`--policy FILE.json`). The logic lives in `mochi-core` (`health::assess`) so the desktop app shares it. | Local evidence only is what §23.2 scopes for 1.0. JSON adds no dependency (`serde_yaml` is unmaintained; the spec's `.yaml` is a non-normative example). | W4: `docs/c14-cli.md`, plan C14 |
| K6 | **`dump-index` output** | The catalog of the opened commit (head or `--snapshot SEQ`), hash-verified as every open is, dumped table by table: deterministic row order, BLOBs as hex, JSON as `{"tables": {name: {"columns": [...], "rows": [[...]]}}}`. Text mode prints counts per table and, with `--table`, the rows escaped through `render::text`. Never accepts SQL, never writes. | It is an inspection tool for humans and bug reports. Table names come only from `sqlite_schema`, never from input. | W3 |
| K7 | **C10: how TAR framing lives in the archive** | See W5, section "Design D19". In short: **no new frame kinds and no schema change.** In a TAR-compatible archive, each commit with at least one put emits one complete POSIX pax TAR stream. File-content chunks stay pure file bytes, referenced by extents exactly as in Core. Headers, padding, and end markers live in ordinary data chunks that no extent references (**stream-only chunks**), listed in the delta manifest like any introduced chunk. | Keeps every Core reader, the catalog DDL, and the manifest schemas unchanged; only the writer, `verify`, and GC accounting learn about the profile. | W5: spec Annex B **D19** (new) |
| K8 | **C11: the Encrypted profile's wire layout** | See W7, section "Design D20": XChaCha20-Poly1305 and Argon2id (D3 already decided), a random per-archive data key wrapped per passphrase, key envelopes discoverable from the head commit record, encrypted data, image, and manifest objects in `0x184D2A59` envelopes, plaintext commit records, footers, descriptor, and key envelopes. | Satisfies §14.1–§14.3 and D12 (the descriptor never locates envelopes). | W7: spec Annex B **D20** (new) + CDDL + R5 draft |
| K9 | **Platform tooling** | Node **24 LTS** pinned in CI (`actions/setup-node`), `packageManager` set in `package.json`; the dev container's Node 22 is acceptable locally. Tauri **2.x latest stable**, React + TypeScript strict + Vite, pnpm frozen lockfile (O1 already). | O1/O11 already decided; this only fixes the versions a session should pin. | W6 |

Nothing else in 1.0 scope is waiting on a decision. The remaining open items are **owner-only**: gate **G6** (Windows durability, needs physical Windows hardware, T33) and **who writes R9** (O7 says a fresh session that has read only the spec; see W9).

---

## 2. How every workstream session works

These are the conventions the C5–C9 and C13 PRs followed. Reviewers expect them.

**Before coding**
- Start from the latest `main`. Read `AGENTS.md`, the spec sections the workstream cites, and its plan section.
- If something the workstream needs is not answered by the spec, this file, or the plan: **stop and write the gap into the PR** (options and trade-offs) instead of guessing. Do not invent wire-format answers. A guess baked into the format is worse than an open question (AGENTS.md).

**While coding**
- `mochi-core` does all I/O through `Storage`. No `unwrap`/`expect`/`panic!` on paths reachable from archive bytes. Checked arithmetic on archive-derived values. Long operations are jobs (progress plus cancellation).
- New error codes go in `error.rs` only, with a placement in `verify::classify` and `mochi_cli::exit_code_for` (both are exhaustive on purpose).
- Archive strings are untrusted: text output goes through `render::text`, and JSON carries exact bytes as hex where they are not UTF-8.

**Tests (the Definition of Done, not optional)**
- The DoD row of the phase (plan §8.1) plus the fault rows it names.
- An **independent oracle**: compare against a pristine copy, the test kit's `read_state`, or a model written from the spec. Never use the code under test's own checks.
- **Mutation check**: break each important rule by hand (a scratch script that edits the file, runs the test, and restores it), confirm a test fails, and list the count in the plan status ("N mutations, all killed"). `c8_repair.rs` and the C8 status show the pattern.
- Parser or untrusted-input changes: extend `mochi_testkit::fuzz` (an exerciser reached by `exercise_archive_open` or a new target) plus a golden vector.
- CLI behaviour: an in-process test under `crates/mochi-cli/tests/` against a temporary directory (see `c8_repair_command.rs`).

**Before pushing:** all of these must pass.
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
ci/check-invariants.sh
```
The full test run takes a few minutes; run it in the background.

**Docs in the same PR**
- Plan: a `- **Status (...)**` bullet under the phase, with the decisions (marked **[delegated 2026-10-08]** when they come from section 1), the tests by file and count, the mutations, and **Not done**.
- `docs/c14-cli.md` for every command change (table row plus rule).
- Spec Annex B for every wire or rule decision.
- `AGENTS.md` "Live today" if a phase completes.

**PR**
- One workstream per PR, titled with the phase ("C10: …"). The description cites spec sections and follows the AGENTS.md checklist.
- CI must be green on `windows-latest` as well as Ubuntu before merge.
- After merge, record the CI run ID in the plan status. That record is what closes "CI evidence pending".

**Parallel sessions:** workstreams marked ∥ touch disjoint files and can run at the same time. The others share `commands.rs`, `cli.rs`, the plan, or `verify.rs`, so run them one after another, each starting from a `main` that has the previous one merged.

---

## 3. Workstreams, in order

Sizes: S ≈ under a day, M ≈ 1–2 days, L ≈ several days of agent time.

| ID | Workstream | Size | Depends on | Parallel |
|---|---|---|---|---|
| W1 | Close CI evidence; record K1, K3, K4 | S | — | — |
| W2 | Q64 check in `fsck` (K2) | S | W1 | — |
| W3 | `mochi dump-index` (K6) | S | W1 | — |
| W4 | `mochi health` with local evidence (K5) | M | W3 | — |
| W5 | C10 TAR-compatibility profile (K7) | L | W1 | ∥ W6, W8 |
| W6 | D0 desktop shell | M–L | — | ∥ anything |
| W7 | C11 Encrypted profile and `rekey` (K8) | L | W5 merged | ∥ W6, W8 |
| W8 | Ratification drafts R1, R2, R4, R7 | M | — | ∥ anything (docs only) |
| W9 | Later: C12 + R6, C8 step 7, R8, R9, D1–D8 | — | see W9 | — |

### W1 — Close CI evidence and record the simple calls (S)

**Do:**
- **CI evidence.** Run 37735586637 (immerh8/MOCHI#12, head `e2301f7`) ran the whole workspace green on Ubuntu 22.04, Ubuntu 24.04, and `windows-latest`. Record it as the CI evidence for:
  - C6: the Windows restore. Remove "C6 is not complete", unless something else in that bullet remains.
  - C7.
  - C8 (first slice).
  - C9, including G2's GC item, so **G2 passes**.
  - C13.
  - C14 (first slice, the C9 commands, `search`, `repair`).
  - **G8**: the `c14_append_truncates_an_eligible_tail_only_on_request` test ran in that job, so G8 passes.

  Update `docs/b2-implementation-checklist.md` (G2, G8 lines and the "CI evidence" list) and every "CI evidence pending" in `docs/implementation-plan.md`. Before claiming a test ran there, check that it exists at `e2301f7` (`git grep -n <test name> e2301f7`).
- **K1:** mark plan §9 O29 Decided (a).
- **K3:**
  - Annex B D16: Decided, 1.x.
  - Change `create --exceed-default-limits` from `NOT_IMPLEMENTED` (exit 3; `crates/mochi-cli/src/commands.rs`, the check before `create` builds its options) to `UNSUPPORTED_FEATURE` (exit 4), with a message saying it is a 1.x feature. Update `exit_codes.rs` and the C14 tests.
  - G4: "passed except the opt-in item, moved to 1.x" if its other items have evidence; otherwise list what is missing.
  - `docs/c14-cli.md` K8.
- **K4:** in `repair::survey`, if `read_version` fails with `UNSUPPORTED_FEATURE`, return that error from `plan`/`apply` (exit 4) instead of recording an omission. Write the C8 "Open" item in the plan as decided.
  - Test: build an archive whose chunk record declares a dependency. `mochi_core::object` has a unit test, `declared_dependencies_are_unsupported_not_ignored`, that shows how such a record looks. The cleanest place may be a `forge`-built delta that introduces such a chunk. Check that `plan` errors with `UNSUPPORTED_FEATURE`.

**Done when:** docs updated, the two code changes tested, CI green.

### W2 — Q64: references outside the baseline view (S)

**Do:** in `verify` (deep, i.e. `fsck`), for each replay segment *b…h*:
1. Compute what a baseline replay sees: the versions and chunks reachable at *b* (from S(*b*), or from the image replayed at *b*), plus everything introduced by deltas *b+1…k−1*.
2. Check that every put in delta *k* of an existing version, and every extent of a version introduced in *k* that names an existing chunk, is in that set.

On a violation, add a finding `REFERENCE_INVALID` and set Recoverability to `FAIL` (Integrity is unaffected).

Update Annex B D18's "Open: reference scope" bullet to "Decided [delegated 2026-10-08]: …".

**Tests:**
- A forged delta (`mochi_testkit::forge`) that puts a version reachable only before *b*: `fsck` reports Recoverability `FAIL` with `REFERENCE_INVALID`, `verify` (not deep) is unchanged, and `open_head` still opens.
- A writer-produced archive with dedup and compaction across checkpoints has no finding (reuse the `c9_dedup.rs` and `c9_compact.rs` fixtures).
- Mutation: drop the check, and the forged case passes.

### W3 — `mochi dump-index` (S)

**Do:**
- Core: `Catalog::dump(&self, tables: Option<&[String]>) -> Result<CatalogDump>`, built like the test-only `logical_dump` in `catalog/mod.rs` but always compiled:
  - Table names are read from `sqlite_schema`; a requested name that is not in that list is `INVALID_ARGUMENT`.
  - Identifiers are quoted.
  - Rows are ordered by `rowid` (or the primary key for `WITHOUT ROWID` tables).
  - Values are typed (integer, text, hex BLOB, null).
  - Keep `logical_dump` for the tests, or reimplement it on `dump`.
- CLI: `mochi dump-index ARCHIVE [--snapshot SEQ] [--table NAME]...`.
  - The catalog is opened through `open_head` / `open_at_footer` (already hash-verified) on `OsReadStorage`.
  - JSON as in K6, plus `"commit": {seq, commit_id, catalog_source}`.
  - Text: per-table row counts; rows only with `--table`, every string through `render::text`.
  - Exit 0; errors as usual.
  - Remove it from the `NOT_IMPLEMENTED` lists (`cli.rs` `scope()`, `exit_codes.rs`).

**Tests:**
- Every table appears.
- Row counts match `list` and `snapshot list`.
- `--table objects` equals the IDs from `search`/`list` JSON.
- An unknown table is refused.
- A hostile name (control characters, non-UTF-8) is escaped in text and exact (hex) in JSON.
- Opening a damaged image falls back to the snapshot manifest, and the output says `catalog_source: snapshot_manifest`.
- The archive bytes are unchanged afterwards.

### W4 — `mochi health` with local evidence (M)

**Design (K5):**
- **Evidence log:** `<state dir>/evidence/<archive-id-hex>.jsonl`, append-only. One line per completed `verify`, `fsck`, `restore-test`, or `repair apply` (the new archive's ID):
  ```
  {"schema":1,"command":…,"level":…,"completed_at":RFC3339,
   "head":{"seq":…,"commit_id":…},"dimensions":{…},"exit_code":…,"scope":…}
  ```
  - Written by the CLI after the report validates.
  - Never written by `--no-local-history`.
  - A write failure is a warning, as with `heads.json`.
  - A damaged log line is skipped with a warning and counted in the report as `unreadable_evidence`. It never becomes a `PASS`.
- **Policy:** `--policy FILE.json`:
  ```
  {"schema":"mochi-health-policy-v1",
   "required":["integrity","recoverability",…],
   "max_age_days":{"verify":30,"restore_test":90}}
  ```
  - The default policy without `--policy`: required integrity and recoverability; `verify` (any level ≥ stored integrity) at most 30 days old.
  - Unknown keys are refused (`deny_unknown_fields`).
- **Core:** `mochi_core::health::assess(evidence: &[EvidenceRecord], policy: &HealthPolicy, current: CurrentHead, now: Timestamp) -> Report` (report schema v1, D15 through `Report::conclude`). Per dimension, take the latest relevant evidence:
  1. None → `UNKNOWN`.
  2. Its head ≠ the current head (commits added since) → `UNKNOWN`, with a finding naming both heads.
  3. Older than `max_age_days` → `OVERDUE`.
  4. Otherwise its recorded status (a `FAIL` stays `FAIL` until newer evidence).

  More rules:
  - Freshness is computed as `verify` does, from the local head store.
  - Dimensions no 1.0 evidence covers (durability, searchability, retention compliance, key availability on non-encrypted archives) follow `verify`'s existing convention for unassessed dimensions. Read `verify.rs` and use the same status; never `PASS`.
  - Sampling or partial scopes are reported as such (§21: never presented as full verification).
- **CLI:** `mochi health ARCHIVE [--policy FILE]`. It opens the archive read-only only to locate the current head (no verification) and prints dimensions with the §20.4 values. The exit code is the report's. Remove it from `NOT_IMPLEMENTED`.

**Tests:**
- Unit tests for `assess` over every rule above, including a `FAIL` older than a newer `PASS` and an expired `PASS` becoming `OVERDUE`.
- Report validation passes for every case.
- CLI: no evidence → exit 2 with all dimensions `UNKNOWN`; `verify` then `health` → exit 0; `append` then `health` → `UNKNOWN` (new commits); clock-dependent cases use an injectable `now` in core, and the CLI test uses a policy with `max_age_days: 0` to force `OVERDUE`; a corrupted log line; `--no-local-history`.
- Wording: never "safe", "backed up", or "preserved" (§23.3 #6).

### W5 — C10: TAR-compatibility profile (L) ∥

**Design D19 [delegated 2026-10-08]** (write it into spec Annex B as D19 *first*, in the same PR):
1. **Opt-in at creation, fixed** (D4, D12). The descriptor's constraint 0 (`tar_compatible`) and the CLI flag `mochi create --tar-compatible` already exist. Today `publish::check_create_profile` refuses the profile (exit 4); C10 lifts that refusal once the writer below exists. Appending with a different profile is already refused.
2. **Writer restrictions in the profile:**
   - Deduplication is off (a request for it is `INVALID_ARGUMENT`).
   - No dictionaries (none exist yet).
   - No holes: a file is stored as contiguous chunks, each used whole and once, in logical order.
3. **One stream per commit.** Each commit with at least one put emits exactly one complete POSIX **pax** TAR stream (POSIX.1-2001 / `ustar` headers plus pax extended headers). For each put, in operation order:
   - a pax extended header when needed: `path` (as bytes, with `hdrcharset=BINARY` when not UTF-8), `mtime` (with nanoseconds), `uid`, `gid`, `size` above 8 GiB − 1;
   - the ustar header (directory `5` or regular file `0`, mode, uid, gid, mtime);
   - the content;
   - zero padding to 512.
   
   The stream ends with two zero blocks. Deletions and retention operations emit nothing: generic tools see the **historical** stream (§7.2 last bullets, to be said in user docs).
4. **Where the bytes live.** File-content chunks are exactly Core's: pure file bytes, referenced by extents. Every other byte of the stream (headers, padding, end blocks) is written in **stream-only chunks**: ordinary data objects (one zstd frame each, O21 rules: `Frame_Content_Size` and checksum) that no extent references. They are inserted in the catalog's `objects`/`object_locations` and listed in the commit's delta manifest `chunks` like any chunk. The writer appends data frames in stream order, so concatenating the decoded bytes of all data frames in physical order (skippable frames are skipped by `zstd`) is the sequence of streams.
5. **A put of an existing version** (a rename, or a rewrite's copy) cannot reference old chunks in the stream, so the writer emits its content again as stream-only chunks. The version's extents are unchanged. This is the documented space cost of the profile.
6. **Accounting:**
   - GC plan reports stream-only chunks in their own total, `stream_framing`, never as `collectable`.
   - Rewrites (`compact`, `gc apply`, `repair apply`) **preserve the source's profile** and regenerate framing for the new archive. Today they write Core regardless: fix by taking the profile from the source descriptor (`compact.rs` and `repair.rs` build `WriterOptions`).
   - Copying a TAR-compatible source into the same profile goes through the writer, so framing is regenerated.
7. **Verification.** A new check for TAR-compatible archives, at structural level and above. It decodes the data frames in physical order with a bounded, read-only pax/ustar parser in `mochi-core` (no new dependency; limits on header sizes and counts) and checks that each commit's stream is complete and that its members equal the commit's puts: path bytes, type, size, file-content hash, mode, and mtime. A mismatch is `FAIL` under Integrity with a new code **`PROFILE_VIOLATION`** (register it; classify as a violation; exit 1).
8. **Documented invocation and claim** (§7.2: name tools and tested versions; never "POSIX compatible"):
   ```
   zstd -dc ARCHIVE.mochi | tar -x --ignore-zeros -f - -C DIR
   ```
   with GNU tar and bsdtar/libarchive (Windows `tar.exe`). Record the tested versions from CI in a new `docs/c10-tar-compat.md`. If a tool needs a different flag (for example bsdtar's `--options read_concatenated_archives`), document the exact per-tool command. Never claim untested tools.

**CI:** a new job `tar interop (${{ matrix.os }})` on `ubuntu-22.04`, `ubuntu-24.04` (apt: `zstd`, `libarchive-tools` for `bsdtar`; GNU tar is preinstalled) and `windows-latest` (`tar.exe` built in; `zstd` from the pinned official release zip, with its SHA-256 checked in the workflow). It runs a test binary gated by an environment variable (for example `MOCHI_TAR_INTEROP=1`, skipped otherwise) that:
- builds archives with the CLI: nested directories, an empty file, a file larger than the chunk size, a name longer than 100 bytes, non-ASCII and (on Linux) non-UTF-8 names, mode 0755, mtime with nanoseconds;
- appends a modify, a rename, and a delete;
- extracts with each tool;
- compares the result with the expected historical state (last write wins, deletions not applied);
- prints each tool's `--version` for the docs.

**Tests (local, no external tools):**
- The writer's stream parses with the core parser and reproduces each commit's puts.
- A Core reader still reads TAR-compatible archives identically (`read_state` equals the Core archive built from the same steps).
- `verify` passes on them and fails with `PROFILE_VIOLATION` when a stream-only chunk's bytes are forged (re-hashed so stored integrity passes) or a member is dropped.
- GC plan reports `stream_framing` separately.
- Compaction and repair outputs keep the profile and pass the stream check.
- Dedup requests are refused.
- Fuzz: run the stream parser from `exercise_archive_open` (or a `tar_stream` target) over the golden set and hostile headers.
- Golden vector: one small TAR-compatible archive in `fixtures/golden/c10/`, with the expected decoded stream bytes, generated deliberately and explained in the PR.

**Not in this workstream:** symlinks (O6 decided, not built; they are skipped as today) and Windows attributes in TAR (not representable; documented).

### W6 — D0: desktop shell (M–L) ∥

The container has no `webkit2gtk-4.1`, so the app is built and E2E-tested **in CI**. Locally run `cargo check -p mochi-desktop` (with `tauri`'s Linux deps missing, use `cargo check --no-default-features` if needed, or rely on CI) plus the frontend unit tests and lint.

**Do (plan §7 D0, O1, K9):**
- `apps/mochi-desktop/src-tauri`: a Tauri 2 crate, added to the workspace, MIT OR Apache-2.0.
  - **Capabilities:** `core:default` minus anything not needed, `dialog:allow-open`, and `dialog:allow-save` only. No `fs`, `shell`, or `http` plugins. Each permission is justified in the PR.
  - **CSP:** `default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src ipc: http://ipc.localhost` (adjust only as Tauri 2 IPC requires, and say why).
  - No remote content.
- **Job runner:**
  - Commands `start_job(kind, args, channel: Channel<JobEvent>) -> JobId` and `cancel_job(JobId)`.
  - Work runs in `tauri::async_runtime::spawn_blocking`, bridging `mochi_core::job::{JobContext, Progress}` to the `Channel` (no second progress mechanism, AGENTS.md).
  - Cancellation maps to `CancellationToken`.
  - A sample job, `self_test`, runs a synthetic multi-step job with progress and honours cancellation (no archive I/O yet).
- **IPC types** from `ts-rs`, generated into `ui/src/ipc/generated/`, plus thin typed `invoke` wrappers. A CI step regenerates the types and fails on a diff.
- `apps/mochi-desktop/ui`: React + TypeScript strict + Vite, pnpm with `packageManager` pinned and a frozen lockfile. ESLint with `react/no-danger` as an error. A single screen that starts and cancels the sample job and shows progress. Status text follows §23.3: no success styling for anything but `PASS`.
- **E2E:** `tauri-driver` plus WebdriverIO on `ubuntu-22.04` (deps: `libwebkit2gtk-4.1-dev`, `webkit2gtk-driver`, `xvfb`) and `windows-latest` (`msedgedriver` matching the runner's WebView2). The test starts the sample job, sees progress events, cancels, and sees a clean cancelled state. Add these as new CI jobs. Keep the existing Rust jobs untouched except for workspace membership.
- `ci/check-invariants.sh` already greps for `dangerouslySetInnerHTML`; make sure it scans `ui/src`.

**Done when:** the D0 exit (plan §7) passes in CI on both OSes.

### W7 — C11: Encrypted profile and `rekey` (L) ∥ W6, W8; after W5 merged (shared writer code)

**Design D20 [delegated 2026-10-08]** (spec Annex B D20, CDDL schemas, and `docs/ratification/R5-crypto-draft.md` with test vectors, **in the PR before or with the code**):
1. **Suite (D3):**
   - XChaCha20-Poly1305 (RustCrypto `chacha20poly1305`) with a **fresh random 24-byte nonce per encryption** from the OS CSPRNG; no counters anywhere (§14.2).
   - Argon2id (RustCrypto `argon2`), version 0x13. Writer defaults: m = 65,536 KiB, t = 3, p = 4 (RFC 9106 §4, second recommended option).
   - Salt: 16 random bytes; output: 32 bytes.
   - Reader limits refuse m > 1 GiB, t > 16, p > 16 (`LIMIT_EXCEEDED`), so a hostile archive cannot exhaust memory or time.
   - New dependencies `chacha20poly1305`, `argon2`, `zeroize`, `unicode-normalization` are all MIT/Apache.
2. **Passphrase:** the UTF-8 bytes after Unicode **NFC** normalization (the same passphrase typed on Windows and Ubuntu input methods must match). Held in `zeroize` types, never logged, never in errors.
3. **Keys:**
   - One random 256-bit **data key (DEK)** per archive, made at creation.
   - Each passphrase derives a KEK with Argon2id, and the KEK wraps the DEK in a **key envelope**: frame `0x184D2A5C`, deterministic CBOR (`mochi_format::cbor`), D11 identity keys. Fields: envelope ID (16 random bytes), KDF parameters and salt, wrap nonce, and wrapped DEK plus tag.
   - Wrap AAD: a new domain separator in `mochi_format::digest` (defined there only), archive ID, envelope ID, and the canonical KDF-parameter bytes.
   - Several envelopes may wrap the same DEK (several passphrases).
4. **Discovery (§14.3, D12):**
   - Every commit record of an encrypted archive carries a new key listing the ObjectRefs of the currently valid key envelopes. Commit record schema v2: the next free key, CDDL updated.
   - A reader finds them from the footer-verified head without decrypting anything.
   - The descriptor records only the creation-time fact that the archive is encrypted, the way the existing design already reserves for it: **assign the Encrypted profile's required-feature identifier** (the first unassigned value; today `KNOWN_REQUIRED_FEATURES` is empty and `Descriptor::profile` says the identifier awaits this item) in descriptor key 4. It names the suite (one identifier per suite, D3). A reader that does not know it refuses the archive (`UNSUPPORTED_FEATURE`, exit 4). There is no descriptor schema change. The descriptor never locates envelopes.
   - Commit records and manifests of an encrypted archive carry the same required feature (their key 9), so that a reader of any one record refuses rather than misreads it.
5. **Objects:**
   - Data chunks, catalog images, and recovery manifests (delta and snapshot) are compress-then-encrypt. The plaintext (the zstd frame, or the image or manifest bytes) is sealed and stored in a `0x184D2A59` skippable frame.
   - Envelope payload: version, suite ID, key ID (the DEK's ID), 24-byte nonce, ciphertext plus tag.
   - Object AAD: domain separator, archive ID, object ID (or the commit binding for images and manifests), object kind, and the header bytes.
   - The stored-object hash covers the whole stored frame, so **stored integrity verifies without keys**. Content integrity needs the key.
6. **Plaintext and visible:**
   - The descriptor, commit records, footers, and key envelopes are plaintext.
   - **Visible without a key:** archive ID, commit count and sequence, object sizes and counts, envelope IDs, KDF parameters.
   - Writers of the Encrypted profile **do not record commit times** (`record_time` forced off).
   - Document all of this in the profile docs (§14.3).
7. **Dedup under encryption** keeps working (same DEK, equality by plaintext chunk hash inside the encrypted catalog). The equality leak is documented, and a test asserts that it **exists** (plan C11 exit).
8. **Errors:**
   - A wrong passphrase fails the envelope AEAD, giving a new code `KEY_UNAVAILABLE`, **exit 3** (operational: a wrong passphrase does not show the archive is damaged). Place it in both exhaustive matches.
   - A tag failure on an object whose stored hash verified is `CONTENT_INTEGRITY_FAILED`.
   - No partial output in either case.
9. **Profile fixed at creation** (D12, no in-place conversion: exit 4).
10. **`rekey`:**
    - `rekey --add-passphrase` and `--remove-passphrase ENVELOPE_ID` are **rewrap**: a commit with a new envelope set and an audit record in the manifest's retention-like operations list, or a new manifest key, per the D20 text.
    - `rekey --reencrypt` writes a new archive through the rewrite path (new DEK), never in place.
    - The two have separate audit records (§14.4).
11. **Passphrase entry (CLI):** an interactive prompt without echo (the `rpassword` crate, MIT/Apache) or `--passphrase-file PATH`. Never as an argument value. `MOCHI_PASSPHRASE` is allowed only behind a flag that says it is for automation.

**Tests:** wrong key fails closed with no partial output; nonce uniqueness (a property test over many writes, retries, crash/reopen, and re-encryption: all nonces distinct); stored-integrity verify without a key passes while content-level checks are `UNKNOWN` and Key availability is `UNKNOWN`; with the key, everything passes; vectors for XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha) and Argon2id (RFC 9106) pass; the dedup-equality leak exists; secrets never appear in `Debug` or errors (grep the test output); hostile KDF parameters are refused before allocation; fuzz the key-envelope and encrypted-envelope decoders; golden vectors.

**Review gate:** crypto layout mistakes are permanent once archives exist. Open the D20 spec text, CDDL, and R5 draft as their own PR first and get the owner's review (or a dedicated review pass) before the implementation PR merges.

### W8 — Ratification drafts R1, R2, R4, R7 (M) ∥

Docs only, in `docs/ratification/`, each **written from the working code and golden vectors** (plan §6), marked **draft, not frozen** until Beta:
- **R1:** the frame registry (`registry.rs`) and version mapping (D1).
- **R2:** record-envelope layouts (D11: the binary envelope v0 and the CBOR-native keys).
- **R4:** the complete SQLite DDL (`catalog/schema.rs`), with every constraint and what reader validation enforces.
- **R7:** report schema v1 (`docs/report-schema-v1.md`) and the full error-code registry, with exit codes and classes from the two exhaustive matches.

Each draft cites its source file and commit, lists any place the spec and code disagree (as open items, not fixes), and updates `docs/ratification/README.md`'s status table.

**Test:** a small test that the R1 table and the R7 code list equal the code (parse the markdown table), so the drafts cannot drift silently.

### W9 — Later (after the above)

- **C12 Redundancy + R6:** a Reed–Solomon suite (candidate: Cauchy RS over GF(2^8); pick a maintained MIT/Apache crate, or implement against published vectors), parity objects `0x184D2A5F` with a `parity_groups` cross-check (§15). Needs its own Annex B entry (D21) like D19/D20. Then C8 steps 5–6 and the DoD row "damage beyond parity capacity".
- **C8 step 7 (bounded salvage scan):** needs rules for labelling uncertain finds and a decision on writing a descriptor "labelled as reconstructed" (D12). Write those as an Annex B proposal for the owner before code.
- **R8 golden set** (valid, corrupt, interrupted archives for every profile), then **R9**: per O7, a read-only verifier in Python written by a **fresh session given only `docs/spec.md` and the ratification artifacts**, never the Rust code. That is a good fit for a separate model session started cold.
- **Desktop D1–D8** after D0, in plan order.
- **Owner-only:** G6 needs runs on physical Windows 10/11 hardware (T33).

---

## 4. Kickoff prompts

Paste one into a new session on this repository. Each assumes the previous workstreams in its dependency column are merged.

**W1**
> Read AGENTS.md and docs/next-work-plan.md. Do workstream W1 exactly as written there (CI evidence from run 37735586637, calls K1, K3, K4). Follow section 2 of that file for tests, docs, and checks. Open one PR titled "W1: CI evidence, O29, D16, repair refuses undecodable content".

**W2**
> Read AGENTS.md and docs/next-work-plan.md. Do workstream W2 (call K2: the Q64 reference-scope check in fsck), following section 2. One PR: "W2: fsck reports references outside the baseline view (Q64)".

**W3**
> Read AGENTS.md and docs/next-work-plan.md. Do workstream W3 (`mochi dump-index`, call K6), following section 2. One PR: "C14: dump-index".

**W4**
> Read AGENTS.md and docs/next-work-plan.md. Do workstream W4 (`mochi health` with local evidence, call K5), following section 2. One PR: "C14: health (local evidence)".

**W5**
> Read AGENTS.md, docs/spec.md §7.2, and docs/next-work-plan.md. Do workstream W5 (C10, design D19). Write D19 into spec Annex B first, then implement, then add the tar interop CI job. Follow section 2. If anything in D19 conflicts with the code or spec, stop and describe the conflict in the PR rather than changing the design. One PR: "C10: TAR-compatibility profile".

**W6**
> Read AGENTS.md, docs/implementation-plan.md §7 (D0) and decision O1, and docs/next-work-plan.md. Do workstream W6 (D0 desktop shell). The container lacks webkit2gtk, so prove the app and E2E in CI on ubuntu-22.04 and windows-latest. Justify every Tauri permission in the PR. One PR: "D0: desktop shell, job runner, E2E".

**W7 (design PR first)**
> Read AGENTS.md, docs/spec.md §7.3 and §14, and docs/next-work-plan.md. For workstream W7, first write only the design: spec Annex B D20 from the plan's "Design D20", the CDDL schemas, and docs/ratification/R5-crypto-draft.md with test vectors. Do not implement yet. One PR: "D20: Encrypted profile design (for review)".

**W7 (implementation, after the design PR is reviewed and merged)**
> Read AGENTS.md, Annex B D20, and docs/next-work-plan.md. Implement workstream W7 (C11 and `rekey`) to D20, following section 2. One PR: "C11: Encrypted profile and rekey".

**W8**
> Read AGENTS.md, docs/implementation-plan.md §6, and docs/next-work-plan.md. Do workstream W8: draft R1, R2, R4, R7 in docs/ratification from the code, with the drift test. One PR: "Ratification drafts R1, R2, R4, R7".
