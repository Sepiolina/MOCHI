# MOCHI user guide

MOCHI is an archive format and archiver. This guide is for people who use the `mochi`
command line tool. Every statement here comes from the specification (`docs/spec.md`),
a frozen ratification artifact (`docs/ratification/`), the CLI reference
(`docs/cli.md`), or the behaviour of the code, and each page says which.

> **A `.mochi` file is not a backup.** The format is ratified at 1.0 (spec §28). Archives
> written by a pre-1.0 build must be converted with `mochi migrate`
> (see [format status](experimental.md)).

Read these first, in this order:

| Page | What it tells you |
|---|---|
| [A `.mochi` file is not a backup](not-a-backup.md) | What an archive is and is not, and what to do about copies |
| [Getting started](getting-started.md) | The commands that exist today, each with an example that was run |
| [Encrypted archives](encrypted.md) | What is visible without the key, what stops working, lost passphrases, removing a passphrase, the equal-content leak |
| [What the checks prove](verification.md) | `verify`, `fsck`, `restore-test`, `health`, and the status values |
| [TAR-compatible archives](tar-compatible.md) | The tested commands and tools, and why generic tools show history |
| [Recovery record](recovery-record.md) | Parity as a repair aid, with its stated capacity |
| [Format status and old archives](experimental.md) | What "1.0" does and does not promise, and `mochi migrate` for archives from before 1.0 |

The desktop application is a client of the same library (spec §23.3). Its "Test archive"
command is `verify`; this guide documents the command line.

On Windows, right-clicking a file or folder offers a **MOCHI** submenu (under "Show more
options" on Windows 11). On a `.mochi` file it has "Open with MOCHI", "Extract here" and
"Extract to a new folder"; on any file or folder, "Add to archive" and "Add to
archive...". "Extract here", "Extract to a new folder" and "Add to archive" work at once in
a small window (each job in a window of its own, so several can run side by side; closing
one stops only its job) and never overwrite: if a name in the way exists, nothing is written and the
extract plan opens in the main window; a new archive is named for the item (or the folder)
and gets a number if the name is taken. When an extraction is done, the folder it went into
opens in a new file-manager window; untick **Open the folder when done** in that small window
to stop this (the choice is remembered). After "Extract to a new folder", type a new name and
press **Rename**, or press **Move to…** to put the folder somewhere else on the same drive; a
name that already exists is refused, never replaced. In the main window, **Extract to…** starts
on the Desktop (in **Settings → Extraction** you can make it start in the folder used last, or in
a folder you choose); a folder you type that does not exist yet is created, and a line under the field
says what will happen before you press OK (for example that a folder named after the archive
already exists there, so a new one is made with a number). The same **Open the folder when done**
choice applies there. If the folder you extract into already has some of the same files, MOCHI
asks first and lists them: **Add only new files** (the default) adds what is missing, also into
folders that are already there, and keeps your files as they are; **Replace** puts the archive's
files in their place and moves yours to the Recycle Bin (Windows) or the Trash (Ubuntu), so you
can get them back; **Extract to a new folder** leaves the folder alone. A folder is never
replaced by a file or the other way round, and the command line never replaces anything. The
command-line equivalents are `mochi get`
and `mochi create` and `mochi append`. A new archive is a local file, not a backup.

Platforms: the desktop application targets Windows 10 22H2 / 11 and Ubuntu 22.04 / 24.04
on x86-64 (plan O11).

Command reference, exit codes, and known concessions: [`docs/cli.md`](../cli.md).
