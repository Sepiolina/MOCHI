# Report schema v1 (draft)

Spec §20.5 lists what a machine-readable report must contain; this file is the field reference for what MOCHI emits today. It is a **draft**: the final schema and error-code registry are ratification item R7 (plan §6). Rust type: `mochi_core::report::Report`. `mochi verify --json`, `mochi fsck --json`, and the desktop "Test archive" export (D4) emit exactly this object.

## Changes from v0

- `schema_version` is `1`.
- New field `freshness_anchor` (spec Annex B.1 D8: "the report names the anchor used").
- `started_at` / `completed_at` are filled, in the D15 form `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`.
- New error codes `REFERENCE_INVALID` and `FRESHNESS_FAILED` (both exit 1).

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
| recoverability | Worst of the history damage assessment (D10.9) and, when data objects were read, `FAIL` for a damaged one (1.0 has no parity); `UNKNOWN` if data objects were not read |
| freshness | `UNKNOWN` without an anchor; `PASS` if the verified history contains the anchor (at its sequence, for `local-history`); `FAIL` otherwise |
| durability | `UNKNOWN`: bytes cannot show whether they reached stable media |
| key_availability | `PASS` for an unencrypted archive (no key needed); `UNKNOWN` if the descriptor was not read |
| searchability | `UNSUPPORTED` (plan C13) |
| retention_compliance | `UNSUPPORTED` (plan C9) |

The policy requires integrity and recoverability, plus freshness under D15's conditions (and searchability when the `search` level is asked for). Because searchability and retention are unsupported, `overall_status` is never `PASS` in this build; `policy_result` and `exit_code` are what automation should read.

## Health reports (`mochi health`)

`mochi health` emits this same schema from `mochi_core::health`, with these conventions (plan K5): `level` is `structural` (its only look at the archive is locating the head) and `scope` says that no check was run; `checked_commit` is the current head; the dimensions come from locally recorded evidence (rules in `docs/c14-cli.md`, rule 13); reasons a dimension is `UNKNOWN` or `OVERDUE` are `skipped` items named after the dimension, plus one named `unreadable_evidence` when evidence log lines could not be read; a recorded failure appears as a finding with the stable codes of the run that found it; `coverage.evidence_age_seconds` is the age of the oldest evidence behind integrity or recoverability. Durability and key availability are `UNKNOWN`, searchability and retention compliance `UNSUPPORTED`, so the overall status is never `PASS` in 1.0.

## Open for R7

- Final field names and the JSON Schema document itself.
- Whether control objects get their own coverage counts.
- Whether `UNCOMMITTED_TAIL` (an interrupted write) should lower durability rather than stay a warning.
- Whether health reports need their own marker (today only `level` and `scope` distinguish them).
- A dedicated not-found code for a path absent from a snapshot (today `INVALID_ARGUMENT`, plan C6).
