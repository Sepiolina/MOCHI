# TAR-compatible archives

Sources: spec §7.2; `docs/dev/tar-compatible-profile.md` (Annex B.2.9 D19); `ci/tar-interop.sh`.

```bash
mochi create --tar-compatible t.mochi src
```

A TAR-compatible archive is an ordinary MOCHI archive in which every commit that adds
or replaces something also writes one complete pax TAR stream. It is chosen when the
archive is created and cannot be added later or removed. The default is **not** TAR-compatible.

## The tested commands

Only these were run (by the CI job `tar interop`, `ci/tar-interop.sh`):

```bash
zstd -dc ARCHIVE.mochi | tar -x --ignore-zeros -f - -C DIR                                    # GNU tar
zstd -dc ARCHIVE.mochi | bsdtar -x --options read_concatenated_archives -f - -C DIR          # bsdtar
```

| Tool | Command | Tested versions (CI run 37886040933) |
|---|---|---|
| zstd CLI | `zstd -dc`, `zstd -t` | 1.5.7 on Ubuntu 22.04, Ubuntu 24.04, Windows |
| GNU tar | `tar -x --ignore-zeros -f -` | 1.34 (Ubuntu 22.04), 1.35 (Ubuntu 24.04), 1.35 (Git for Windows) |
| bsdtar (libarchive) | `bsdtar -x --options read_concatenated_archives -f -` | 3.6.0, 3.7.2, 3.8.4 (Windows system `tar.exe`) |

The claim is that these commands extract the same files as the streams contain with these
tools and versions. It is **not** "POSIX compatible" and it names no other tool or version.
Neither flag is optional: without `--ignore-zeros` (GNU tar) or
`--options read_concatenated_archives` (bsdtar, Windows `tar.exe`), the tool stops at the first
end-of-archive marker and extracts only the first commit's stream. A tool that reads the
whole file as one TAR stream is not claimed to work.

## Generic tools show the history, not the latest snapshot

Extracting with `zstd | tar` gives you every commit's stream in order:

* for every path, the **last write wins**;
* a **deleted file is still extracted**, because the earlier commit that wrote it is in the
  stream and a deletion writes nothing;
* a **renamed file** appears at both the new path and the old one.

Example, run with GNU tar and bsdtar: commit 0 puts `src/f1` and `src/dir/f3`; commit 1
changes `f1` and adds `f4`; commit 2 deletes `f4`.

```
$ zstd -dc t.mochi | tar -x --ignore-zeros -f - -C out
out/src/dir/f3  out/src/f1  out/src/f4        # f4 is back
$ mochi get t.mochi -C snap
snap/src/dir/f3  snap/src/f1                  # the latest snapshot: no f4
```

To get the latest snapshot, use `mochi get` or `mochi restore-test`. They read the catalog.

## What it costs

* **No deduplication.** Equal files are stored twice.
* **Re-written content is stored again**, for example a renamed file or a file appended
  unchanged.
* 512 bytes of framing per file and two end blocks per commit.
* Sparse-file holes and Windows attributes are not representable; symbolic links are
  skipped as in the default profile.

`mochi verify` parses every commit's stream and checks it against the commit, and reports
`PROFILE_VIOLATION` (exit 1) if they differ. `compact`, `gc apply`, and `repair apply` keep the profile and
regenerate the framing.

An encrypted archive cannot be TAR-compatible ([encrypted.md](encrypted.md)).
