# What the checks prove

Sources: spec §5.4, §5.7, §20, §21, §23.3 #2–#3; `docs/cli.md` (rule 13 for `health`);
`docs/report-schema-v1.md`; `docs/dev/encrypted-profile.md`.

MOCHI separates "the bytes in this file match what was recorded" from any claim about where else your data exists.
Only the first is something a check can show, and only for what it ran.

## The commands

| Command | What it does | Writes to the archive? |
|---|---|---|
| `mochi verify ARCHIVE [--level L]` | Checks the whole retained history at a chosen depth. Default level: `restoration` | No (spec §20.2) |
| `mochi fsck ARCHIVE` | `verify`, plus every commit opened at its own footer and its namespace cross-checked against the head's catalog, and every replay segment recovered from its baseline | No |
| `mochi restore-test ARCHIVE -C NEWDIR` | Restores the whole latest commit into a directory that must not exist yet, verifying each file's hash; the archive is read-only | No |
| `mochi health ARCHIVE [--policy FILE.json]` | **Runs no check.** Summarizes what earlier runs on this machine recorded, judged against a policy for the current head | No |

The desktop application's "Test archive" is `verify` and must not modify the archive (spec
§23.3 #3).

### The levels of `verify`

Each level includes the ones before it (`mochi verify --help` lists them all):

| Level | What it looked at |
|---|---|
| `structural` | descriptor, frames, footers, commits, and the catalog's structure |
| `referential` | file extents and the complete set of objects each snapshot needs |
| `stored` | every stored object was read and its hash compared |
| `content` | objects decrypted (if encrypted) and decompressed; lengths and chunk hashes checked |
| `restoration` (default) | each file reassembled and its whole-file hash compared |

`mochi verify` also accepts `search`, `inventory`, and `disaster-recovery`. In this build each
reports `UNSUPPORTED` and exits 4 (observed); the Preservation profile that would give them meaning
is not in 1.0 (spec §7.7). `structural` and `referential` runs exit 2 (`UNKNOWN` overall) because
the stored bytes were not read.

## The result: one status per dimension

A report shows separate dimensions, and an overall result never hides a failing dimension
(spec §20.3):

| Dimension | What it speaks to |
|---|---|
| durability | Whether the storage survives a crash or power loss. Not established by a local check |
| integrity | Stored bytes (and, at deeper levels, decoded bytes) match their recorded hashes |
| recoverability | Files can be reassembled and match their hashes |
| searchability | Search index completeness |
| freshness | The archive's head is the one you expect (see below) |
| retention_compliance | Every snapshot the archive's own retention settings keep (not expired, or on hold) is present and restorable. A local check: it says nothing about other copies |
| key_availability | The key for an encrypted archive opens it |

The status values (spec §20.4):

| Status | Meaning |
|---|---|
| `PASS` | The required checks completed successfully within policy |
| `FAIL` | A required check found a violation |
| `DEGRADED` | Something still works, but a required protection is reduced |
| `UNKNOWN` | The evidence is insufficient or unavailable |
| `OVERDUE` | The required evidence is older than the policy allows |
| `UNSUPPORTED` | This build cannot perform the required check |

**`UNKNOWN`, `OVERDUE`, `UNSUPPORTED`, and `DEGRADED` are not passes** (spec §5.4: a required
check that is incomplete, unsupported, skipped, or overdue must not be reported as passing).
A check you did not run, or could not run, tells you nothing about the thing it would have
checked. An old successful check says nothing about the bytes today. So the tool never
shows them with success wording, and a script gets a non-zero exit code.

Example from a plain archive (these lines are from a real run):

```
  durability            UNKNOWN
  integrity             PASS         required
  recoverability        PASS         required
  searchability         UNSUPPORTED
  freshness             PASS         required
  retention_compliance  PASS
  key_availability      PASS
data objects: 3 of 3 checked (53 of 53 stored bytes)
result: PASS (exit 0)
```

The result covers the dimensions the policy requires (by default integrity, recoverability,
and freshness for `verify`). The ones shown `UNKNOWN` or `UNSUPPORTED` were not
checked. Read the whole table, not only the last line.

Exit codes follow the report (spec §23.2): 0 passed, 1 a violation, 2 degraded/unknown/overdue
required evidence, 3 operational error (for example a bad passphrase or policy), 4 an
unsupported required feature or check. With `--json`, the report is a single JSON object
whose `dimensions` and `exit_code` are the same as the text.

## What a passing `verify` does not prove

* **That any other copy exists.** It reads this one file.
* **That you can restore on another machine, or from another copy.** Restoration was
  reassembled from this file, here. Independent-source restoration exercises are not built
  in 1.0 (spec §7.6, §28).
* **That the file is the latest you wrote.** Freshness (spec §5.7) is judged against an anchor:
  the last head this machine saw for this archive ID (local history, kept in the state
  directory), or a head you supply with `--expected-head COMMIT_ID`. With neither,
  freshness is `UNKNOWN`. Local history only knows what *this* client saw. A different
  archive at the same path is simply first sight.
* **That search is complete.** Searchability is `UNSUPPORTED`. `mochi search` prints its own coverage; zero
  matches with partial coverage is not "no matches".
* **That storage is durable** (durability `UNKNOWN` in `verify`). `mochi health` shows what
  this computer recorded when the current version was written: `DEGRADED` at best, because
  one copy on one disk is never more than that.
* **That other copies exist or keep your retention rules.** Retention compliance checks only
  this archive, against its own expiry and holds, and only at the `restoration` level.
* **Anything about bytes it did not read.** Sampling and level choice decide the coverage; the
  report says how many objects and bytes were checked of how many. A `stored`-level run says
  nothing about whether the content decodes.
* **That `PASS` today means `PASS` tomorrow.** It is the state when the check ran.

## `restore-test`

```bash
mochi restore-test photos.mochi -C rt        # rt must not exist; an existing one is DESTINATION_EXISTS
```

It proves that, on this machine, from this file, every file in the latest commit
could be reassembled and matched its recorded hash. Exit 1 if an entry failed its hash;
exit 2 if an entry was not restored for another reason (a name collision or an
unsupported name). It does not prove the restored files are what you meant to store, and not
anything about earlier snapshots (restore one with `mochi get --snapshot SEQ`).

## `health`: a summary, not a check

`mochi health` reads the log of evidence that `verify`, `fsck`, `restore-test`, and a
complete `repair apply` wrote on this machine, and judges it for the archive's current head.
It does not read the archive's objects. Per dimension:

* no evidence: `UNKNOWN`;
* the last recorded result was a `FAIL`: `FAIL`, at any age (damaged bytes do not heal);
* evidence about an earlier head: `UNKNOWN`;
* evidence older than the policy allows: `OVERDUE`;
* otherwise the recorded status.

The default policy requires integrity and recoverability, with `verify` evidence no
older than 30 days. A policy is JSON:

```json
{"schema":"mochi-health-policy-v1","required":["integrity","recoverability"],"max_age_days":{"verify":30,"restore_test":90}}
```

A kind of evidence named in `max_age_days` is required: with the policy above, a `health` run on
an archive that has never been through `restore-test` is `UNKNOWN`. `--no-local-history` records and reads nothing. A line of the log that cannot be read stops any
dimension from being `PASS`.

## Errors that are not verification results

A wrong or missing passphrase is `KEY_UNAVAILABLE` (exit 3), not a verification result. A tag
failure on an object whose hash matched is `CONTENT_INTEGRITY_FAILED`. See
[encrypted.md](encrypted.md).
