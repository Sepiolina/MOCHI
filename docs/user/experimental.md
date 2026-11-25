# Format status and archives from before 1.0

Sources: spec §26, §28, Annex B.2.14; `docs/cli.md`; `mochi --version`.

The archive format is **ratified at 1.0** (spec §28): a 1.0 build writes a ratified
archive, and the layout it reads is the one the ratification artifacts describe. That
is a statement about the format, not about your files. A `.mochi` file is still not a
backup, and 1.0 does not claim preservation readiness: the Preservation profile is not in
1.0, and no 1.0 build reports `PRESERVED`.

## Archives written before 1.0

Builds before 1.0 wrote a **draft** layout. A 1.0 build refuses a draft archive in every
command but one, with exit code 4 and a message that names the draft. For the last draft
(draft 2) the message points to `mochi migrate`:

```bash
mochi migrate old.mochi --output new.mochi
mochi migrate old-encrypted.mochi --output new.mochi --passphrase-file pass.txt
```

What `migrate` does (spec §26, B.2.14):

* It reads **only draft 2**. Older drafts are refused and cannot be converted.
* It opens the source read-only and never changes or removes it. It refuses an output
  that already exists.
* It first verifies the source at the restoration level. A source that does not verify
  is refused whole, with nothing written: repair it first (`mochi repair plan`).
* It writes a new archive with the same snapshots, file versions, attributes, retention
  and profile (Core, TAR-compatible, Encrypted, with or without a recovery record).
  An Encrypted archive is sealed again under a new data key, so it needs the passphrase
  (a file or the prompt, never an argument). A wrong passphrase is `KEY_UNAVAILABLE`.
* Before publishing, it compares the new archive with the source (every snapshot's
  files, file hashes, attributes, retention) and verifies the new archive.
* The new archive has a new archive ID, so its first verification has no freshness
  anchor. `--json` gives a report with the commit mapping.

What it does not do: it makes no independent copies, and it does not remove the
source. Keep the old archive until you have checked the new one and no longer need it.

## Still true

* **A reader that does not understand a required feature says `UNSUPPORTED`,** never
  returns partial or empty content as if it were complete (spec §7.7, §26).
* **Not in 1.0** (spec Annex A; these commands exit 4 `UNSUPPORTED_FEATURE`):
  `inventory`, `split`, `join`, `mount`. Passphrase-only encryption (no key files or public
  keys), split volumes, remote storage, signatures, and writing any non-MOCHI archive
  format are also outside 1.0.
* **Version numbers are different things** (spec §1.2, §26): the product release, the
  specification revision, the container wire generation, the metadata schema version.
  A product labelled 1.0 writing wire generation 2 is intentional.
