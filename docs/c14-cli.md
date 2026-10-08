# The `mochi` CLI (plan C7, C14): commands, rules, and concessions

Status 2026-10-08. Built: `create`, `append`, `list`, `get`, `snapshot list|retain|expire|release`, `verify`, `fsck`, `restore-test`, (C9 commands, 2026-10-08) `checkpoint`, `compact`, `gc plan|apply`, and (C13, 2026-10-08) `search`. In 1.0 scope, not built (exit 3, `NOT_IMPLEMENTED`): `health`, `repair plan|apply`, `rekey`, `dump-index`. Post-1.0 (exit 4): `inventory`, `split`, `join`, `mount`.

The CLI is a thin client (spec §23.3 #1, §23.4). Format logic, verification, restore, and the filesystem import live in `mochi-core` (`verify`, `restore`, `import`, `storage::os::OsSourceTree`), so the desktop app (D2–D4) calls the same functions and gets the same results. The CLI adds only argument parsing, rendering, exit codes, and the local head history.

## Commands

| Command | What it does | Exit codes |
|---|---|---|
| `mochi create ARCHIVE INPUT…` | Creates `ARCHIVE` by the D13 mechanism (temporary file, published without replacing anything) with one commit holding the inputs. Each input becomes a top-level entry named after its last component | 0; 2 if anything was skipped or the directory entry is not confirmed durable (always on Windows before G6); 3 `DESTINATION_EXISTS` if `ARCHIVE` exists; 4 for `--tar-compatible` (C10 not built) |
| `mochi append ARCHIVE [INPUT…] [--delete PATH]…` | One new commit: deletions first (each with everything under it), then inputs added or replaced | as `create`; 3 `UNCOMMITTED_TAIL` if an interrupted write left a tail |
| `mochi append … --truncate-tail [--no-quarantine] [--accept-unconfirmed-durability]` | Quarantines an eligible tail (D14) to a verified sidecar `<name>.tail-<offset>-<16 hex>.mochiq`, truncates it, then commits (or only truncates, if nothing else was asked). Waivers are recorded in the output | 0; 3 `QUARANTINE_FAILED` / `DURABILITY_UNCONFIRMED` / `TAIL_UNRESOLVED` |
| `mochi list ARCHIVE [PATH] [--snapshot SEQ]` | The namespace of the head or of commit `SEQ`, optionally under `PATH` | 0 |
| `mochi snapshot list ARCHIVE` | Every commit: sequence, commit ID, recorded time, checkpoint or delta, and its retention at the head (expired, holds, retained). If retention cannot be rebuilt the commits are still listed, with retention unknown and a warning | 0 |
| `mochi snapshot retain ARCHIVE SEQ --label L` (alias `hold`) | One commit placing a legal hold `L` on snapshot `SEQ` (Annex B D18) | 0; 2 directory unconfirmed; 3 `INVALID_ARGUMENT` for an invalid hold, nothing committed |
| `mochi snapshot expire ARCHIVE SEQ… --confirm` | One commit expiring earlier snapshots. Irreversible (there is no un-expire operation); a held snapshot stays retained | as `retain`; 3 without `--confirm` |
| `mochi snapshot release ARCHIVE --label L --confirm` | One commit releasing hold `L` | as `expire` |
| `mochi checkpoint ARCHIVE` | One commit with an unchanged namespace, written as a checkpoint (`request_checkpoint`; verified before adoption, D10.7) | 0; 2 directory unconfirmed |
| `mochi gc plan ARCHIVE [--output PLAN]` | Read-only. The GC plan (`mochi_core::gc::GcPlan`): head, expiries, holds, roots, collectable snapshots with reasons, totals, IDs. `--output` saves it (never replacing a file) | 0; 1 `RETENTION_UNRESOLVED`; 3 `DESTINATION_EXISTS` |
| `mochi gc apply ARCHIVE --plan PLAN --output NEW [--no-verify-content]` | Under the source's lock, plans again and refuses any difference from the saved plan (a moved head, changed retention, an edited file, another archive); then writes `NEW` with the retained roots only (`compact`, `Keep::Roots`). The source is unchanged and kept | 0; 2 if `NEW`'s directory entry is unconfirmed; 3 `INVALID_ARGUMENT` (stale or foreign plan), `DESTINATION_EXISTS` |
| `mochi compact ARCHIVE --output NEW [--no-verify-content]` | Writes every snapshot into `NEW` (`Keep::Every`), one commit each. The source is unchanged and kept | as `gc apply` |
| `mochi get ARCHIVE [PATH…] [-C DIR] [--snapshot SEQ]` | Restores everything or the given subtrees into `DIR` (default `.`; created if missing) through the C6 engine | 0; 1 if any file failed verification (it is absent, never partial); 2 if any entry was not restored for another reason (collision, unsupported name); 3 with `--refuse-on-conflict` when the preflight finds one |
| `mochi get ARCHIVE PATH --stdout` | One file's bytes to standard output | 0; 1 on an integrity failure, and the bytes already written are unverified |
| `mochi restore-test ARCHIVE -C NEWDIR` | Restores the whole commit into a directory that must not exist yet | as `get`; 3 `DESTINATION_EXISTS` |
| `mochi search ARCHIVE [PATTERN] [--snapshot HEAD\|retained\|all\|SEQ] [-i] [--path P \| --under P] [--version ID] [--content-hash HASH] [--kind file\|dir]` | Read-only discovery (spec §19.1; `mochi_core::search`): entries whose stored path contains `PATTERN` (bytes; `-i` folds ASCII only) and that meet every other criterion, one hit per snapshot holding the entry. `--content-hash` is plain BLAKE3 of the file, as `b3sum` prints it; `--version` is a `file_version_id` from earlier JSON output. Prints the §19.3 coverage: requested and searched snapshots, the catalog's commit and source, unavailable snapshots with reasons; file content is never searched | 0 with complete coverage, whatever the number of hits; 2 with partial coverage, 1 with `--require-complete`; 4 for `--content` (full-text search, §19.2, is not built); 1 `RETENTION_UNRESOLVED` for `--snapshot retained` when retention cannot be rebuilt |
| `mochi verify ARCHIVE [--level L] [--expected-head ID] [--require-freshness]` | Read-only verification (spec §20; `docs/report-schema-v1.md`). Default level `restoration` | the report's D15 exit code |
| `mochi fsck ARCHIVE …` | `verify` plus every commit opened at its own footer and its namespace cross-checked against the head catalog | the report's D15 exit code |

Global options: `--json`, `--state-dir DIR`, `--no-local-history`, `--limit NAME=VALUE` (reader limits, spec §8.5: `max-skippable-payload`, `max-frame-len`, `max-blocks-per-frame`, `max-window-size`, `max-commit-frame-len`, `max-decoded-object-len`, `max-required-features`).

Errors in `--json` mode are one line on standard output: `{"error": {"code": "…", "message": "…"}}`. The exit code of an error comes from `mochi_cli::exit_code_for`, the single mapping (integrity and freshness failures 1, refusals 4, everything else 3).

## Rules the CLI follows (delegated decisions)

1. **Wording (spec §23.3 #6).** After a commit the CLI says "committed … (LOCAL_COMMITTED)" and reminds the user to keep an independent copy. It never says "backed up", "safe", or "preserved". Deletion says the entries were removed "from this snapshot (still in history)" (§23.3 #5).
2. **Archive strings are untrusted (§23.3 #9).** Text output escapes control characters, bidirectional controls, backslashes, and non-UTF-8 bytes (`render::text`), so a name cannot move the cursor, recolour the terminal, or impersonate another name. JSON output carries names as JSON strings, plus `path_hex` with the exact bytes when they are not UTF-8.
3. **Freshness anchor (Annex B.1 D8).** The CLI keeps the last-seen head per archive ID in `heads.json` (`--state-dir`, else `$MOCHI_STATE_DIR`, else `$XDG_STATE_HOME/mochi`, `~/.local/state/mochi`, or `%LOCALAPPDATA%\mochi`). It records a head after its own `create`/`append`, and after a `verify`/`fsck` in which nothing failed. The recorded sequence never decreases, so a rolled-back copy cannot move the anchor back. `--no-local-history` opts out. A damaged history file is a warning on standard error, and freshness then has no anchor.
4. **`get` exit codes.** Attributes that could not be applied (for example ownership as an unprivileged user) are warnings only; they do not change the exit code, or every unprivileged extraction would exit 2.
5. **Unsupported source entries.** Symbolic links (O6 decided, not built), devices, FIFOs, and sockets are skipped and listed; the command exits 2. Links are never followed. An entry that cannot be read fails the whole command before anything is committed.
6. **Retention reductions are confirmed (spec §16.3).** `snapshot expire` and `snapshot release` refuse without `--confirm` and commit nothing. Placing a hold needs no confirmation. Elevated authorization and a delay are deployment policy, not built (§16.3 says SHOULD).
7. **A GC plan is applied exactly as approved.** `gc apply` recomputes the plan under the source's publication lock and compares the whole JSON value with the saved file, so it never collects more or less than what the user reviewed.
8. **Rewrites never replace anything and never touch the source.** `compact` and `gc apply` refuse an existing output name before taking any lock (`DESTINATION_EXISTS`; publication refuses it again, D13), say that the source is unchanged and kept, and record the new archive's head as its freshness anchor (a new archive ID, so the old anchor does not carry over). Removing the source is left to the user (D18).
9. **Verification output.** `verify` refuses to print a report that fails `Report::validate` (`REPORT_INCONSISTENT`, exit 3). Its exit code is the report's `exit_code`, computed by `Report::conclude`, so the CLI obeys D15 through one path (gate G9).
10. **Search coverage is visible (spec §19.3, §23.3 #8).** Every `search` result states its coverage. Partial coverage prints `coverage PARTIAL` and that zero matches are not proof, and exits 2 (1 with `--require-complete`), so a script never reads "no matches" from an incomplete search. A content query is refused (exit 4) rather than answered from names.

## Concessions (to refine later; each is separable)

| # | Concession | Why it is acceptable now | Remedy |
|---|---|---|---|
| K1 | **Content is held in memory** for one commit (`Transaction::put_file` takes `Vec<u8>`), bounded by `--max-memory` (default 2 GiB) and refused above it (`LIMIT_EXCEEDED`), never truncated | Correct for typical desktop inputs; the bound makes the failure explicit | A streaming `put_file` that chunks from a reader inside `ArchiveWriter::commit` (core change only; the importer already reads per file) |
| K2 | **No progress output and no Ctrl-C cancellation** in the CLI; it passes a `NullProgress` sink and a token it never cancels | Interrupting the process is safe: before the footer nothing is published (§12.2), and a later `append` sees the tail (exit 3) | Wire a progress renderer to `ProgressSink` and a signal handler to the `CancellationToken` (CLI only) |
| K3 | **Inputs become top-level entries.** There is no `--prefix`/`--as` to place an input under an archive directory, and no way to update one nested file without re-adding its top-level directory | Matches `tar`/`zip` defaults | An `--into ARCHIVE_DIR` option in `import` (the parent must exist in the head) |
| K4 | **Local head history is keyed by archive ID only.** A different archive substituted at the same path is first sight (`UNKNOWN`), not `FAIL` | That is D8 as decided; it never reports `PASS` | Optionally also key by canonical path and report a different archive ID at a known path |
| K5 | **Concurrent writers of `heads.json`** may lose one update (last rename wins) | The anchor is then older, never wrong | A lock file beside `heads.json` |
| K6 | **Verification cost.** `restoration` re-reads every chunk once per file version after the `content` pass decoded it | Simple and obviously complete; documented level semantics | Fold the content pass into the restoration pass when every object is referenced |
| K7 | **Symbolic links are skipped**, not stored | The entry kind (manifest kind 2) is reserved but not built (plan C4, C6) | Build symlink entries per O6 (store target bytes, never follow, restore as an exception where the platform cannot create one) |
| K8 | **`create --exceed-default-limits`** stays refused by name | Owner deferral Q10, spec Annex B D16 | Decide D16, then T29's opt-in path and G4 |

## What is not in this slice

`health` (needs stored evidence and policy files), `repair` (C8), `rekey` (C11), `dump-index`. They keep exiting 3 `NOT_IMPLEMENTED`, never success. Full-text search (C13, optional) is refused with exit 4.
