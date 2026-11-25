# Report schema v1 (frozen with R7)

Spec §20.5 lists what a machine-readable report must contain; this file is the field reference for what MOCHI emits today. R7 froze it on 2026-10-14 (gate G15): the registry and field tables are `docs/ratification/R7-reports-and-error-codes-draft.md`, and the machine-checked definition is `docs/schemas/json/report-v1.schema.json`; where this file and they differ, they win. Rust type: `mochi_core::report::Report`. `mochi verify --json`, `mochi fsck --json`, and the desktop "Test archive" export (D4) emit exactly this object.

## Changes from v0

- `schema_version` is `1`.
- New field `freshness_anchor` (spec Annex B.1 D8: "the report names the anchor used").
- `started_at` / `completed_at` are filled, in the D15 form `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`.
- New error codes `REFERENCE_INVALID` and `FRESHNESS_FAILED` (both exit 1). `PROFILE_VIOLATION` (exit 1) is added by the TAR-compatibility profile (Annex B.2.9 D19).
- The Encrypted profile (Annex B.2.10 D20) adds `KEY_UNAVAILABLE` (an operational error, exit 3, never a verification result: only `verify` and `fsck` print it, as a report in which no dimension is assessed, `operational_error` is true, and the one `error` finding carries this code; every other command prints the error line; R7 O1, owner decision 2026-10-14) and `KDF_COST_BELOW_DEFAULT`, which only ever appears as a finding with severity `info`: a key envelope in the file (any commit's, removed ones included, since they still yield the data key) declares an Argon2id cost below the writer default (m < 65,536 KiB or t < 3); the message says when the head no longer lists it. `expected` and `observed` give the two costs; it changes no dimension and no exit code (D20 item 14).
- `LIMIT_NOT_DEFAULT` (R7 decision of 2026-10-11, spec §20.5 "configuration"): one finding with severity `info` per reader limit that differs from its default (`--limit`); `expected` is the default, `observed` the value in force, and the message names the limit. It changes no dimension and no exit code.
- The Redundancy profile (Annex B.2.11 D21) adds `PARITY_DAMAGED` (a finding with severity `warning`: a parity frame is damaged or missing, or a group has no intact frame; integrity is unaffected and recoverability is at most `DEGRADED`) and `PARITY_CAPACITY_EXCEEDED` (severity `error`: a group's damage exceeds its intact parity; recoverability `FAIL`). Frames that verify but disagree with their commit are `PROFILE_VIOLATION`. When parity rebuilds all the damage found and the rebuilt bytes verify, recoverability is `DEGRADED` rather than `FAIL`, and the `scope` says how many shards were rebuilt; integrity stays `FAIL`, because the archive's bytes are damaged.

## Fields

| Field | Type | Meaning |
|---|---|---|
| `schema_version` | integer | `1` |
| `archive_id` | hex string or null | The archive ID of the checked head (64 hex digits); null if no head was opened |
| `checked_commit` | hex string or null | Commit ID of the checked head |
| `expected_head` | hex string or null | The freshness anchor's commit ID. Null exactly when `freshness_anchor` is `none` |
| `freshness_anchor` | `none` / `user` / `local-history` | Where `expected_head` came from (D8) |
| `tool` | object | `name`, `version`, `spec_revision`, `wire_generation`, `format_status` (always the draft label before 1.0) |
| `started_at`, `completed_at` | D15 timestamp or null | Wall-clock times of the run |
| `level` | string | `structural`, `referential`, `stored_integrity`, `content_integrity`, `restoration`; `inventory`, `search`, `disaster_recovery` are accepted and reported `UNSUPPORTED` |
| `scope` | string | What was checked, in words. Sampling, if ever added, must say so here (§21) |
| `coverage` | object | Data objects (chunks) only: `expected_objects`, `checked_objects`, `expected_bytes`, `checked_bytes` (stored bytes), `evidence_age_seconds`. Null means "not measured", which is not zero. Control objects (descriptor, manifests, catalog images) are covered by findings, not counts |
| `skipped` | array of `{item, reason}` | Checks in scope that did not run (a failed prerequisite, cancellation) |
| `findings` | array | `{code, severity, message?, expected?, observed?, affected?}`; `severity` is `info`/`warning`/`error`; `affected` is an inclusive commit-sequence range `{first, last}` for history damage |
| `repair_actions` | array | Empty: verification never repairs (§20.2) |
| `dimensions` | object | One §20.4 status per §20.3 dimension |
| `overall_status` | status | D15 evidence rollup over every dimension (`FAIL` > `UNSUPPORTED` > `DEGRADED` > `OVERDUE` > `UNKNOWN` > `PASS`) |
| `policy` | object | `required` (dimension names) and `freshness` (the D15 basis: `expected_head_supplied`, `archive_in_local_history`, `requested`) |
| `policy_result` | status | Rollup over the required dimensions only |
| `operational_error` | bool | The run itself was compromised (I/O, limits, cancellation) |
| `exit_code` | integer | D15 exit code, the same value the CLI exits with |

## How `verify` fills the dimensions

From `mochi_core::verify` (plan C7 records these as delegated decisions):

| Dimension | Value |
|---|---|
| integrity | `FAIL` on any violation at any level, including a tail that may hold a damaged commit; `PASS` only at `stored_integrity` or deeper with every check run; otherwise `UNKNOWN` |
| recoverability | Worst of the history damage assessment (D10.9) and, when data objects were read, `FAIL` for a damaged one, except that in an archive with a recovery record (D21) damage that parity rebuilds and that verifies is `DEGRADED`, and damage beyond a group's parity is `FAIL` (`PARITY_CAPACITY_EXCEEDED`); `UNKNOWN` if data objects were not read |
| freshness | `UNKNOWN` without an anchor; `PASS` if the verified history contains the anchor (at its sequence, for `local-history`); `FAIL` otherwise |
| durability | `UNKNOWN`: bytes cannot show whether they reached stable media |
| key_availability | `PASS` for an unencrypted archive (no key needed); `UNKNOWN` if the descriptor was not read |
| searchability | `UNSUPPORTED` (plan C13) |
| retention_compliance | A **local** check (spec Annex B.2.22): the retention state rebuilt at the head (D10.10). `PASS` at the `restoration` level when integrity and recoverability pass (every snapshot the archive's own retention state keeps is present and restorable); `FAIL` when recoverability fails and nothing is expired, or, under `deep` (`fsck`), when the state cannot be rebuilt (`RETENTION_UNRESOLVED`); `DEGRADED` when parity can rebuild the damage; otherwise `UNKNOWN`, with a `skipped` item named `retention compliance` saying why (below `restoration`; no key; snapshots expired and damage found; plain `verify` that cannot rebuild the state names `RETENTION_UNRESOLVED` and `fsck`). No inventory is consulted |

The policy requires integrity and recoverability, plus freshness under D15's conditions (and searchability when the `search` level is asked for). Because searchability is unsupported and durability unknown, `overall_status` is never `PASS` in this build; `policy_result` and `exit_code` are what automation should read.

## Health reports (`mochi health`)

`mochi health` emits this same schema from `mochi_core::health`, with these conventions (plan K5): `level` is `structural` (its only look at the archive is locating the head) and `scope` says that no check was run; `checked_commit` is the current head; the dimensions come from locally recorded evidence (rules in `docs/cli.md`, rule 13); reasons a dimension is `UNKNOWN` or `OVERDUE` are `skipped` items named after the dimension, plus one named `unreadable_evidence` when evidence log lines could not be read; a recorded failure appears as a finding with the stable codes of the run that found it; `coverage.evidence_age_seconds` is the age of the oldest evidence behind integrity, recoverability, or retention compliance. Retention compliance follows recorded `verify`, `fsck`, and `repair apply` evidence like integrity. **Durability** comes from the commit records clients write when they publish (evidence schema 2, Annex B.2.22): `DEGRADED` for the current head with a `skipped` item named `durability` saying whether every sync was confirmed, `UNKNOWN` without one; never `PASS`. Key availability is `UNKNOWN` and searchability `UNSUPPORTED`, so the overall status is never `PASS` in 1.0.

## Decided for R7 (2026-10-11, delegated)

The items this section listed as open are decided in
`docs/ratification/R7-reports-and-error-codes-draft.md` §8: the JSON Schema document is
written at the R7 freeze; control objects get no coverage counts (findings cover them);
`UNCOMMITTED_TAIL` stays a warning and does not lower durability; `health` reports need
no marker (`level` and `scope` distinguish them); a path absent from a snapshot stays
`INVALID_ARGUMENT`. The schema version stays 1.
