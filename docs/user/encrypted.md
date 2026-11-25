# Encrypted archives

Sources: spec §7.3, §14; `docs/dev/encrypted-profile.md` (Annex B.2.10 D20,
`docs/ratification/R5-crypto-draft.md`); `docs/cli.md`. Experimental
([experimental.md](experimental.md)).

```bash
printf 'a long passphrase\n' > pass.txt && chmod 600 pass.txt
mochi create --encrypted --passphrase-file pass.txt vault.mochi photos
mochi list --passphrase-file pass.txt vault.mochi
```

Without `--passphrase-file`, on a terminal the tool asks for the passphrase (twice when
creating). A passphrase is never a command-line argument. `--passphrase-env-for-automation`
reads `MOCHI_PASSPHRASE` and is for automation only, because a process environment is readable by
more than its owner. 1.0 is passphrase-only; key files and public-key recipients are not in 1.0.

## Lost passphrase is lost data

There is no recovery if every passphrase is lost. Nothing in the file, the tool, or a
repair can supply it. Keep the passphrase somewhere independent of the archive and its copies,
and tell whoever may need it. A wrong passphrase is `KEY_UNAVAILABLE` (exit 3), an
operational error, not a statement about the archive.

## What is visible without the key

An encrypted archive hides names, attributes, file sizes, directory structure, and content
(`docs/dev/encrypted-profile.md`). Searching the raw bytes of an example archive for its file names and file contents
finds neither. The following **stays visible** to anyone holding the file:

* the archive ID, the number of commits, and their parent links;
* the number, kind, and sizes of stored objects, and whether a commit added data;
* the key envelopes: their IDs and their Argon2id cost (`mochi rekey vault.mochi --list`
  needs no passphrase);
* compressed sizes (content is compressed, then encrypted).

The commit graph (commit records, footers, descriptor) is plaintext and hash-chained, **not
signed**. Someone who can rewrite the whole file can roll it back or withhold commits;
only the freshness check against a head you trust can catch that (R5 / B.2.10 item 13).

## What stops working without the key

* **Generic tools.** Encrypted objects are written in skippable frames, so a plain Zstandard
  decoder skips them. `zstd -dc` does not decrypt, and gives you no files
  (spec §7.3: generic Zstandard decoding is not decryption or meaningful extraction).
  The zstd + tar fallback of a [TAR-compatible archive](tar-compatible.md) is not
  available: `--encrypted` cannot be combined with `--tar-compatible`.
* **`list`, `get`, `search`, `restore-test`, `dump-index`, `snapshot list`, `gc plan`,
  `repair plan`** all need the passphrase, as do `append`, `checkpoint`, `snapshot
  retain/expire/release`, and the rewrites `compact`, `gc apply`, `repair apply`.
  Each opens the key before it creates anything, so a missing key leaves no partial output.
* **Self-healing and repair.** A repair writes a new archive and has to read the sealed contents,
  so `repair plan` and `repair apply` need the passphrase. Without it the command exits
  3 and does nothing. Parity over the sealed bytes (see
  [recovery-record.md](recovery-record.md)) can be checked without a key, and a keyless
  `verify` of a damaged example archive showed `recoverability DEGRADED`; rebuilding
  into a usable archive is still a keyed `repair`.
* **Restoring files** needs the key. There is no other path.

What does work without the key: `mochi verify` (see below), and `mochi rekey ARCHIVE --list`.
`mochi find` never uses a passphrase: it lists an Encrypted archive as not searched, says its
search is incomplete, and exits 2.

## Verifying without the key

```
$ mochi verify vault.mochi --level stored        # no passphrase
  integrity             PASS         required
  recoverability        UNKNOWN      required
  key_availability      UNKNOWN
result: UNKNOWN (exit 2)
```

A keyless run checks stored bytes only. It can report integrity `PASS` at
`--level stored` and not above; recoverability and key availability are `UNKNOWN`, and the
result is never `PASS` overall. Give a passphrase (`--passphrase-file`, or `--ask-passphrase`)
and every level runs, with key availability `PASS`.

## Adding and removing passphrases: removal is not revocation

```bash
mochi rekey vault.mochi --list
mochi rekey vault.mochi --add-passphrase --passphrase-file pass.txt --new-passphrase-file new.txt
mochi rekey vault.mochi --remove-passphrase ENVELOPE_ID --passphrase-file pass.txt
```

These rewrap the same data key: one new commit, no data rewritten.

**Removing a passphrase does not take its access away.** The removed key envelope stays in the
file's history, and a rewrap keeps the data key, so the removed passphrase still opens the
data key from this file and from any copy of it, and that key also decrypts content
appended later. `mochi` refuses a removed passphrase at the head; that is the tool's rule,
not a cryptographic barrier. To keep a passphrase away from new content,
`mochi rekey ARCHIVE --reencrypt -o NEW --new-passphrase-file P` writes a new archive under a new data key;
copies of the old file stay as they were and still open with the old passphrases.

`verify` also reports `KDF_COST_BELOW_DEFAULT` (severity `info`) for key envelopes whose
Argon2id cost is below the current writer default, including removed ones.

## Equal content is stored once, and that is visible

Deduplication still works inside an encrypted archive, so an identical file is stored once.
**Someone who can compare your archive before and after an append can tell whether the
content you appended was already in it**: if the file grew by about the file's size, it was
new; if it did not, an identical file was already stored. They learn this without the key
and without learning what the file is. This is a documented property (`docs/dev/encrypted-profile.md`;
a test asserts it), not a fault; it exists because dedup is part of the format. If it
matters to you, do not store content in the archive whose repetition would give something away.

## Rewrites need the passphrase

`compact`, `gc apply`, `repair apply`, and `rekey --reencrypt` seal everything again under a new
key for the new archive (a sealed object's associated data is bound to its archive ID). Give
every passphrase the new archive should accept; a passphrase you do not give again does not carry over.
