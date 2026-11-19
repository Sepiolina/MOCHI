# pyverify: an independent, read-only verifier for MOCHI archives

`mochi_verify` is the second implementation behind gate G14 (spec Annex B.1 D7 and its
2026-10-12 amendment, Annex B.2.6): a read-only verifier in Python, written from the
specification and the ratification artifacts only, sharing no code with the reference
implementation. Its value is its independence. A disagreement with the Rust implementation is
evidence for W18, in either direction.

It never writes to an archive, never opens one for writing, and applies parity repairs
only to an in-memory copy.

## Provenance

This section says exactly what the verifier was written from. Read it before trusting the
independence claim.

**Session.** Written in a separate Claude Code session of its own, on its own branch
(`claude/mochi-archive-verifier-biuybp`), started with only the files listed below.

**Read, as the task allows:**

* `docs/spec.md`, in full.
* `docs/ratification/R1` to `R7` (`*-draft.md`), in full.
* `docs/schemas/`: the rules of `archive-descriptor-v0`, `commit-record-v1`, `commit-record-v2`,
  `key-envelope-v0`, `recovery-manifest-v1`, `-v2` and `-v3` (v3 as a diff against v2's rules). Their header
  comments in full for the commit, key-envelope and manifest-v1 schemas, in part for the others. Not opened: the
  three v0 schemas and `tail-quarantine-v0`.
* `docs/ratification/R5-vectors/vectors.json` and `R6-vectors/vectors.json`, as test data.
* `fixtures/golden/`: `README.md`, every `vectors.txt` and `descriptor-vectors.txt`, and the
  fixture files themselves, as test data.
* The task description (section W17 of the ratification plan, a document since retired; it set the
  scope below and the summary format). Also, one `grep -n` of that plan for the headings of W16 to W20
  printed those five heading lines (titles only).

**Not read:** any file under `crates/` or `apps/`; any `gen_vectors.py`; any other `docs/*.md`;
`docs/ratification/README.md`; `ci/` scripts; the Rust sources of the fixtures' writers.

**Things that touched the rules, disclosed in full:**

1. **`AGENTS.md` was in my context.** The Claude Code harness injects the repository's `AGENTS.md`
   into every session as project instructions, before the first tool call. I did not open the
   file, but its text was visible to me. It contains the repository's layout, its command list
   and a list of invariants, several of which restate rules the spec also states. I did not
   consult it while deciding any reading and I do not believe any behaviour here comes from it, but I cannot
   prove that, and it breaks the strict reading of "must not read AGENTS.md". The reading log lists
   every interpretation so that W18 can see which ones could have been influenced.
2. **This was not a "cold session" in W17's sense.** W17 asks for a session created with a sparse
   checkout of `docs`, `fixtures/golden`, `interop` only. This checkout is the whole repository, so the
   Rust sources were reachable on disk; the independence rests on my not opening them.
3. **Directory listings.** I ran `ls` on `docs/`, `docs/ratification`, `docs/schemas`, `fixtures/golden` and
   `.github`, which show the names of other files (for example other `docs/*.md`) without their
   contents, and `git log --oneline` (three commit subjects).
4. **The CI file.** I read parts of `.github/workflows/ci.yml` (its head, the list of job names, the
   `tar-interop` and `doc-nightly` jobs, and one grep for the R6 regeneration step) in order to add the `r9-interop` job. That
   file mentions build steps of the Rust workspace and nothing about the format.
5. **Fixtures taught me three things the text does not say**, recorded in `READING-LOG.md` (C1, E1,
   G2): a snapshot manifest holds the live namespace's closure, not every version ever introduced; the passphrase
   of the Encrypted golden archive is `golden passphrase` (found by trying 12 strings, since no readable
   document states it); and one TAR reject fixture has an off-by-one pax record length.
6. **The reading log** (`READING-LOG.md`) was written alongside the code and finished at the end. It lists
   every place where I interpreted or guessed, with the reading chosen and its section.
7. **No expected summary was opened** by the session that wrote the verifier: `fixtures/golden/r8/` did not
   exist yet. The W18 session that ran the comparison (see `READING-LOG.md`, section W18) did read them, by
   design, and changed the verifier only where the table in `docs/ratification/R9-interop-results.md` says so.

## Layout

```
mochi_verify/
  cbor.py        strict deterministic CBOR decoder + re-encode identity check (own implementation)
  frames.py      structural Zstandard frame walk (RFC 8878), skippable frames
  footer.py      commit footer (8.4, R1 section 3)
  descriptor.py  archive descriptor v0
  envelope.py    binary envelope v0, sealed-object header
  commit.py      commit records v1/v2, commit ID
  manifest.py    recovery manifests v1/v2/v3 (decoding)
  state.py       manifest replay without SQLite
  catalog.py     catalog image against R4 (sqlite3, through a temporary file)
  objects.py     data objects (one Zstandard frame each)
  crypto.py      R5: key envelope, Argon2id, XChaCha20-Poly1305 (via PyNaCl), sealed objects
  parity.py      R6: GF(2^8) Cauchy Reed-Solomon, layout rule, frames, scan and rebuild (in memory)
  tar.py         strict pax/ustar reader (stdlib tarfile as second reader)
  archive.py     head discovery, chain, open, verification pass
  summary.py     mochi-interop-summary-v1
  cli.py         command line
tests/           pytest suite
tools/compare_r8.py   the R8 comparison used by CI
READING-LOG.md   every interpretation, with the reading chosen
requirements.txt hash-pinned dependencies
```

## Running it

```
python3 -m venv .venv && . .venv/bin/activate
pip install --require-hashes -r interop/pyverify/requirements.txt
pip install --no-deps interop/pyverify

python3 -I -m mochi_verify ARCHIVE [--passphrase-file F] [--expected-head HEX] --summary [--case NAME]
```

* `--summary` prints the `mochi-interop-summary-v1` JSON document (sorted keys, bytes as lowercase
  hex). `--case` sets the `case` field; it defaults to the file name without its extension.
* `--passphrase-file F`: the first line, line ending removed (a passphrase is never a command-line
  argument). Without it an Encrypted archive gets the keyless view: structure and stored bytes
  only, `snapshots` is `null`. A passphrase that opens nothing is `KEY_UNAVAILABLE` (see Exit status).
* `--expected-head HEX`: a commit ID; absent from the verified chain gives a `FRESHNESS_FAILED` finding.
* Exit status: 0 after printing a summary (the document, not the exit status, carries the verdict);
  3 for an operational error; without `--summary` a short text report, exit 1 if the head is refused or
  there are findings. A passphrase that opens nothing is `KEY_UNAVAILABLE`: with `--summary` the document says
  `head` refused with that code (the reference does the same, `fixtures/golden/README.md`), without it exit 3.

`python3 -I` is used by the tests too, so the package must be installed (not just on `PYTHONPATH`).

### Tests

```
cd interop/pyverify && python -m pytest -q
```

* the R3 codec table, the R5 vectors (AEAD, HChaCha20 cross-check, NFC, Argon2id, key envelopes, sealed
  objects, the commit layout) and the R6 vectors (field, matrix, layout rule, codes with every erasure
  pattern, shard hashes, byte-exact frames, rejects);
* every per-phase golden vector (`c1`, `c2`, `c3`, `c4`, `c5`, `b2`, `c10`, `c11`, `c12`) gives the verdict its
  `vectors.txt` or README lists, including `open_head` verdicts for all 23 `c5` archive vectors and a
  bit-identical parity rebuild of `c12/damaged-within-capacity`;
* truncation at **every byte** of three small valid archives (a core archive, a TAR-compatible one and
  an Encrypted one, keyless) never yields a commit outside the original's committed prefix, and the same
  at footer and commit boundaries plus a stride for three larger ones;
* the summary document, the tail rules, the command line.

CI runs the suite as job `r9-interop` on ubuntu-22.04 (Python 3.10) and ubuntu-24.04 (Python 3.12),
and `tools/compare_r8.py` over `fixtures/golden/r8/`: `cases.json` lists the cases, each case's archive is
`<case>.mochi`, its `passphrase` and `expected_head` come from `cases.json`, and its summary is compared exactly
with `<case>.summary.json` (a case without a summary, or a summary without a case, fails). Any mismatch fails the
job and the list is printed. The results, with every disagreement found, are in
`docs/ratification/R9-interop-results.md`.

## Dependencies

All are pinned with hashes in `requirements.txt` (every published file of each release, so any
matching wheel or sdist installs under `--require-hashes`). Licences, as published on PyPI:

| Package | Version | Licence | Used for |
|---|---|---|---|
| blake3 | 1.0.8 | CC0-1.0 OR Apache-2.0 | BLAKE3-256 |
| zstandard | 0.25.0 | BSD-3-Clause | decoding data chunks |
| PyNaCl | 1.6.2 | Apache-2.0 (bundles libsodium, ISC) | XChaCha20-Poly1305 |
| argon2-cffi | 25.1.0 | MIT | Argon2id (low-level API) |
| argon2-cffi-bindings | 26.1.0 | MIT (bundles the Argon2 reference code, CC0-1.0 OR Apache-2.0) | transitive |
| cffi | 2.1.1 | MIT-0 | transitive |
| pycparser | 3.11 | BSD-3-Clause | transitive |
| pytest | 9.1.1 | MIT | tests |
| iniconfig, pluggy, tomli | 2.3.1, 1.6.0, 2.5.0 | MIT | pytest |
| packaging | 26.3 | Apache-2.0 OR BSD-2-Clause | pytest |
| pygments | 2.21.0 | BSD-2-Clause | pytest |
| exceptiongroup | 1.3.1 | MIT | pytest (Python < 3.11) |
| typing-extensions | 4.16.0 | PSF-2.0 | exceptiongroup (Python < 3.13) |

No general-purpose CBOR library is used: the codec is `cbor.py`. SQLite is the standard library's
`sqlite3`; the images are opened only from a temporary file, read-only. The verifier needs SQLite 3.37
or later (`STRICT` tables). `tarfile` is the standard library's.

The verifier itself is offered under the repository's licence (MIT).

## What it does not do

It does not repair, salvage or copy (ladder steps 5 and 7), enforce `--limit`, or write a report in the
Rust schema. `READING-LOG.md` section J lists the rest.
