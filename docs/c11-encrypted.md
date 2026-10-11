# The Encrypted profile and `rekey` (plan C11; spec Annex B.2.10 D20)

An Encrypted archive is an ordinary MOCHI archive whose names, attributes, file
sizes, directory structure, and content are sealed under a random data key. The
choice is made at creation and is fixed for the archive's life (descriptor feature
1, D12). The design is spec Annex B.2.10 (D20) and `docs/ratification/R5-crypto-draft.md`;
this file says what the code does, how to use it, and where it chose.

```bash
mochi create --encrypted --passphrase-file p.txt ARCHIVE.mochi INPUT…
mochi list   --passphrase-file p.txt ARCHIVE.mochi
mochi verify ARCHIVE.mochi --level stored        # no passphrase: stored integrity only
mochi rekey  ARCHIVE.mochi --list                # no passphrase needed
```

## Passphrases

A passphrase is the NFC-normalised UTF-8 text, 1 to 4,096 bytes. It comes from, in
this order of what you name:

| Source | How |
|---|---|
| `--passphrase-file PATH` (repeatable) | The first line of the file, line ending removed, read once. On Unix a file readable by others earns a warning |
| `--passphrase-env-for-automation` | The variable `MOCHI_PASSPHRASE`. Read **only** behind this switch, whose name says what it is for: a process environment is readable by more than its owner |
| a prompt | On a terminal, without echo; asked **twice** when a passphrase is being created |

A passphrase is **never an argument value**, appears in no output, error message, or
`Debug` text (the tests search for it), and is held in `zeroize` types. Without a
source and without a terminal, a command that needs a passphrase exits **3**
(`KEY_UNAVAILABLE`) and writes nothing. A wrong passphrase is the same: it is not
evidence about the archive, so it is an operational error, never a verification
result.

`create --encrypted` wraps the data key once per `--passphrase-file` given (up to 16
envelopes); `--encrypted` conflicts with `--tar-compatible`. **There is no recovery if
every passphrase is lost.**

The Argon2id cost is fixed at the D20 defaults (m = 64 MiB, t = 3, p = 4): the shipped
CLI has no knob for it (D20 item 1). The library takes `WriterOptions::kdf` (and
`CompactOptions::kdf`, `RepairOptions::kdf`), and builds with `mochi-core`'s
`test-controls` feature have `publish::test_controls::set_default_kdf`, which the CLI's
integration tests use so that they do not spend seconds in Argon2 on every command.
Readers accept what an archive declares up to `--limit max-kdf-memory-kib`,
`max-kdf-iterations`, `max-kdf-lanes`, `max-key-envelopes` (defaults 1 GiB, 16, 16, 16),
checked before anything is allocated.

## Which commands need the passphrase

| Command | Passphrase |
|---|---|
| `list`, `get`, `search`, `restore-test`, `dump-index`, `snapshot list`, `gc plan`, `repair plan` | Required |
| `append`, `snapshot retain/expire/release`, `checkpoint`, `rekey --add/--remove-passphrase` | Required (they need the data key; `rekey` accepts any passphrase of the head's set) |
| `compact`, `gc apply`, `repair apply`, `rekey --reencrypt` | Required; the **new** archive is wrapped under the passphrases given (a passphrase not given again does not carry over) |
| `verify`, `fsck` | Optional: with one (`--passphrase-file`, the environment switch, or `--ask-passphrase`) every level runs; without one the run is keyless (below) |
| `rekey --list`, `health` | Never |

Every command that needs the key opens it **before** it creates anything: no output
file, directory, or partial restore exists after a `KEY_UNAVAILABLE`.

## Verification without a key

Stored integrity needs no key: every stored-object hash covers the whole sealed frame,
and each commit's **data region** (commit key 12) names the byte range of its data
objects and their combined stored-object hash. A keyless `verify`:

* hashes the descriptor, every key envelope, every sealed manifest and image, and every
  commit's data region, and walks each region as complete sealed frames of kind 0 with
  the archive's key ID;
* can report `integrity PASS` at `--level stored` and **only** there. Asked for
  `content` or `restoration` it reports `integrity UNKNOWN` (the content checks did not
  run);
* reports `recoverability UNKNOWN` and `key_availability UNKNOWN`, with `skipped` items
  that say "no key supplied", and a scope text that says the run was "WITHOUT a key".
  The overall result is therefore never `PASS` (exit 2 when the policy's required
  dimensions are not all `PASS`);
* finds a flipped byte in any data region, manifest, image, or envelope: `integrity
  FAIL`, exit 1.

With the passphrase every level runs as for Core, `key_availability` is `PASS`, and
`verify` additionally checks, **for every commit**, that the envelopes the commit lists
equal the key state its segment's manifests replay to (`RECORD_INVALID` otherwise).
Opening for reading does not replay S(*b*) (D10.9), so a reader does not make that
check; `verify`, appending, baseline recovery, and `fsck` do.

## `rekey`

* `rekey ARCHIVE --list` shows the head's envelope IDs, the commit that wrote each, and
  its KDF cost.
* `rekey ARCHIVE --add-passphrase [--new-passphrase-file P]` and
  `--remove-passphrase ENVELOPE_ID` are **rewraps**: one ordinary commit with no namespace
  operations, a new envelope frame for an addition, the new complete set in commit key 11,
  and the **key operations** (manifest key 13) as the audit record. The data key does not
  change and no data is rewritten. The set never becomes empty.
* **Removal is not revocation.** The removed envelope's frame stays in the file's
  history, and any earlier copy of the archive still opens with it. The head does not
  open with a removed passphrase, even through a session that already holds the data key.
  Output and documentation never say "revoked" or "secure".
* `rekey ARCHIVE --reencrypt -o NEW --new-passphrase-file P` writes a **new archive**
  through the compaction path: new archive ID, new data key, every snapshot kept. Its
  audit record is delta(0)'s provenance with reason *re-encryption*; a collection or
  compaction carries the other reason. It does not recall copies already made.

## Rewrites re-seal

The associated data of a sealed object binds the archive ID, so `compact`, `gc apply`,
and `repair apply` open every sealed object and seal it again under the new archive's
fresh data key, with fresh nonces. No sealed frame of the source reappears in the
result (a test asserts that), and the source is untouched.

## What it does not do (spec B.2.10 item 13)

It protects the confidentiality of content, names, sizes of files, and structure, and
the integrity of every sealed object against someone without the key. It does **not**
authenticate the commit graph (commit records, footers, and the descriptor are plaintext
and hash-chained, not signed), so a party who can rewrite the whole file can roll it
back or withhold commits; freshness (D8) still applies. It leaks, and documents:

* the archive ID, the commit count, parent links, the number, kinds, and sizes of
  objects, and whether a commit added data;
* **equal content stored once**: deduplication works under encryption, so a party who
  compares the file before and after an append learns whether the appended content was
  already stored. A test asserts the leak exists so nobody "fixes" it by breaking dedup;
* compressed sizes (compress-then-encrypt).

Nothing in the CLI or the desktop app calls the result "safe", "secure", or "backed up".

## Decisions made in the implementation (for review)

1. **Self-unlocking low-level reads.** `read_bound_manifest` and `check_image` unlock
   from the commit's own envelopes when the session has not opened the archive yet, so
   `read_provenance` and friends work with a fresh session. The *head* is opened strictly
   (`open_head`): the passphrase must open one of the head's own envelopes, which is what
   makes a removed passphrase stop opening the head.
2. **Key-state replay scope** (see "Verification"): every commit in `verify`/`fsck`; the
   opened commit in appends, baseline recovery, and `segment_state`; not in a plain read
   open.
3. **Keyless integrity above `stored`** is `UNKNOWN`, not `PASS`.
4. **The referential check** accepts one sealed frame (`ENCRYPTED_OBJECT`) of the recorded
   length for an `AEAD` object, one Zstandard data frame otherwise.
5. **`CompactOptions`** gained `read` (the passphrases), `new_keys` (for re-encryption),
   `kdf`, and `reason`; `RepairOptions` gained `kdf`; `CompactReport` gained `resealed`.
   `read_bound_manifest` became `pub`.
6. **Required-feature identifier 1** was previously used by tests as "an unknown feature";
   those tests now use 2 (features) and 3/4 (schema versions), and eight golden vectors
   changed for that reason only.

## Evidence

Tests: `crates/mochi-testkit/tests/c11_encrypted.rs` (layout, no plaintext in the file,
wrong key, append, schema 2, keyless and keyed verify, tampering, the dedup leak, fail
closed), `c11_rewrites.rs` (compact, `gc apply`, `repair apply` re-seal), `c11_rekey.rs`
(list, rewrap, removal is not revocation, re-encryption, **nonces never reused** over
random histories), `c11_forged.rs` (forged commits: key-state disagreement, bad key
operations, a manifest sealed for another commit), `c11_golden.rs` (vectors); the R5
vectors in `mochi-format`; the CLI end to end in `crates/mochi-cli/tests/c11_encrypted_cli.rs`.
Fuzz targets `key_envelope` and `sealed_object`; golden vectors `fixtures/golden/c11/`.
Mutation checks run for this phase are listed in the PR.
