# A `.mochi` file is not a backup

Sources: spec §1.1, §1.3, §5.3, §12.4, §15.3, §23.3 #6.

A `.mochi` file is one file. It keeps files, keeps earlier versions of them, and lets you
check that what it holds still matches what was written. It does not protect itself
from the loss of the file.

The specification says it directly (§1.1):

* No archive format can guarantee that files will never disappear.
* Loss of every copy, destruction of all usable encryption keys, or damage beyond all
  available recovery mechanisms can make recovery impossible.
* Checksums detect corruption. They do not replace backups.
* Parity repairs bounded damage. It does not replace independent copies.
* Successful writes do not prove successful restoration.

## What this means in practice

* **After `create` or `append`, the tool says the commit is local.** The message
  contains `LOCAL_COMMITTED`: the commit is complete in that one file, on that one
  storage location. It does not say the data exists anywhere else, and MOCHI 1.0
  has no state that says it does (spec §5.3, §12.4: a local commit is reported as local).
* **Deleting a file removes it from the current snapshot only.** The earlier
  snapshots still hold it (`mochi append --delete` prints "still in history"). Nothing
  here removes content from the file unless you run `compact` or `gc apply` into a new
  archive and then remove the old one yourself.
* **Verification checks one file.** A passing `verify` says the bytes in that file
  match what was recorded. It does not say another copy exists
  (see [What the checks prove](verification.md)).
* **A recovery record is not a second copy** ([Recovery record](recovery-record.md)).
* **Encryption makes a lost passphrase fatal** ([Encrypted archives](encrypted.md)).

## What to do

Keep independent copies of anything you cannot lose. The specification's suggested
starting policy (§15.3) is an operational recommendation, not something the format
provides:

* three complete copies;
* on independent storage that does not fail together;
* at least one in a different place;
* at least one offline or otherwise out of reach of the account that writes the archive;
* a separate record of your encryption passphrases.

MOCHI 1.0 does not create or track copies. `mochi repair` can use copies you give it
(`--copy PATH`, see `docs/cli.md` rule 11a), but the copies are yours to make and keep.
