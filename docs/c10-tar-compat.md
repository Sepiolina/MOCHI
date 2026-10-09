# The TAR-compatible profile (plan C10; spec §7.2, Annex B.2.9 D19)

A TAR-compatible archive is an ordinary MOCHI archive in which every commit that
adds or replaces something also writes **one complete pax TAR stream**, so that the
Zstandard decoder alone turns the file into those streams:

```bash
zstd -dc ARCHIVE.mochi | tar -x --ignore-zeros -f - -C DIR          # GNU tar
zstd -dc ARCHIVE.mochi | bsdtar -x --options read_concatenated_archives -f - -C DIR
```

`mochi create ARCHIVE INPUT… --tar-compatible` makes one. The choice is fixed at
creation (descriptor constraint 0, D12): `append`, `compact`, `gc apply`, and `repair
apply` keep it, and nothing converts an archive in place. The default profile is
**not** TAR-compatible (D4).

## What extraction gives you: the history, not the latest snapshot

Generic tools extract the **historical stream**, never the latest snapshot.

* Commits are extracted in order, so for every path the **last put wins**.
* A **deleted file is still extracted**: a deletion writes nothing to a stream, and
  the earlier commit that put the file did.
* A **renamed file** appears at its new path *and* at the old one (the old one from
  the earlier commit). The rename writes the content again at the new path.
* A commit that only deletes, expires, or holds adds no bytes and no member.

To get the latest snapshot, use `mochi get` (or `restore-test`), which reads the
catalog. `ci/tar-interop.sh` asserts both: that a path deleted in commit 2 is still
extracted by every tool, and that `mochi get` does not restore it.

## What the profile costs

* **No deduplication** (D19 rule 2). Asking for it is `INVALID_ARGUMENT`. Two equal
  files are stored twice, because the stream needs each file's bytes where its
  member is.
* **Re-put content is written again.** A rename's target, or a file put again
  unchanged, re-emits its bytes as "stream-only" chunks. The version's extents are
  unchanged and still point at the original chunks. This is the documented space
  cost; 3 MB renamed is 3 MB more.
* **512-byte framing per member**, plus two end blocks per commit.
* Holes and Windows attributes are not representable; symbolic links are skipped as
  in the default profile. A path longer than 1 MiB of header is refused before
  anything is written.

## How it is stored (no wire change)

D19 adds no frame kind, schema, or record field. File-content chunks are exactly
the default profile's. Everything else in a stream (padding, headers, end blocks, and
the re-emitted content above) is written as **stream-only chunks**: ordinary
Zstandard data objects that no extent references, listed in the commit's delta
manifest like any introduced chunk. Every other frame in the file is skippable, so
decoding the whole file yields the streams back to back. A Core reader reads a
TAR-compatible archive exactly as it reads any other.

## Checking and accounting

* **`verify`** parses every commit's stream at every level (bounded, read-only; no
  new dependency) and requires: a complete stream (two end blocks, nothing after) in
  every commit with a put and no data frame in any commit without one; members equal
  to the commit's puts in order (path bytes, type, size, mode, uid, gid, mtime, and
  the exact header encoding); a fresh put's content to be the version's own chunks in
  order; and, from `content_integrity` up, re-emitted content to hash to the
  version's file-content hash. A mismatch is a `FAIL` under integrity with
  **`PROFILE_VIOLATION`** (exit 1). The report's scope says the streams were parsed.
* **`gc plan`** reports `stream_framing` (chunks and stored bytes) apart from
  `collectable`. Framing is never collectable: it belongs to the commits' streams,
  not to a file version.
* **Rewrites** (`compact`, `gc apply`, `repair apply`) write the new archive through
  the writer in the source's profile, so framing is regenerated, never copied.

## The compatibility claim

The claim is: *the commands above extract the same files as the streams contain, with
the tools and versions below, on the platforms below*. It is not "POSIX compatible"
and it does not cover any tool or version not listed. The list is what the CI job
`tar interop` ran (each run prints the versions in its job summary):

| Tool | Command | Ubuntu 22.04 | Ubuntu 24.04 | Windows |
|---|---|---|---|---|
| zstd CLI | `zstd -dc`, `zstd -t` | 1.5.7 | 1.5.7 | 1.5.7 |
| GNU tar | `tar -x --ignore-zeros -f -` | 1.34 | 1.35 | 1.35 (Git for Windows `/usr/bin/tar`) |
| bsdtar (libarchive) | `bsdtar -x --options read_concatenated_archives -f -` | 3.6.0 | 3.7.2 | 3.8.4 (system `tar.exe`) |

Versions are those printed by the `tar interop` jobs of the PR that introduced the
profile (run 37886040933, all six cells passed); the runner images move, so a later run
may print newer ones, and only a run that prints a version supports a claim for it.
On Windows `mochi create` exits 2 (directory entry not confirmed durable, until gate G6),
which `ci/tar-interop.sh` accepts; everything else in the script is checked.

Notes on tools:

* **bsdtar and Windows `tar.exe`** stop at the first end-of-archive marker unless
  told to read concatenated archives; without `--options read_concatenated_archives`
  they extract only the first commit's stream. This is why the option is part of the
  documented command.
* **GNU tar** needs `--ignore-zeros` (`-i`) for the same reason.
* A tool that reads the whole file as one TAR stream, or skips the end blocks by
  itself, may behave differently; none is claimed.

## Tests

| What | Where |
|---|---|
| Encoder and parser; header layout, pax cases, rejection of every deviation, split-independence | `crates/mochi-core/src/tar.rs` tests |
| Encoder against a real GNU tar (local evidence) | `crates/mochi-testkit/tests/c10_tar_encoder_gnu.rs` |
| `zstd` decodes the file to the streams of the puts; Core readers see the same snapshots; dedup rules; profile persistence; pax and odd sizes | `c10_tar_profile.rs` |
| `verify`: healthy archives at every level; each stream deviation is `PROFILE_VIOLATION`; re-emitted content hashed from `content_integrity` | `c10_tar_verify.rs` (writer test controls: `TarTamper`) |
| GC `stream_framing`; `compact`, `gc apply`, `repair apply` keep the profile | `c10_tar_rewrites.rs` |
| Golden streams (byte-exact), rejects, a whole archive that must keep verifying | `c10_golden.rs`, `fixtures/golden/c10/` |
| Fuzz target `tar_stream` (canonical-encoding and split-independence properties) | `fuzz/fuzz_targets/tar_stream.rs`, CI `fuzz smoke` |
| CLI end to end | `crates/mochi-cli/tests/c14_commands.rs` |
| Real tools on Ubuntu and Windows | CI job `tar interop`, `ci/tar-interop.sh` |
