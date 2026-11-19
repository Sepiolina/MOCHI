# mochi-foreign

Read-only access to **foreign archive formats** for the MOCHI desktop app (plan
phase D8, decision O9): browse, test, and stream entries out of **ZIP**,
**7z** (including AES-256, encrypted names, and split `.7z.001…` volumes),
**RAR 4 and RAR 5** (read-only, always; including encrypted headers; multi-volume
sets are read but, in 1.0, not tested as a complete set, and RAR has no
hostile-input fixtures: see "Not done" below), **tar**, and
**tar.gz / .tgz**, and **tar.zst / .tzst**.

These formats are outside the MOCHI specification. Nothing here touches the
`.mochi` wire format.

## Shape

```
app / CLI                         mochi-foreign-helper (child process)
  mochi_foreign::ForeignClient  ── stdin/stdout, protocol v1 ──▶  engine: detect, zip, tar, gzip, zstd
  (no parser linked)                                              (all parsing happens here)
                                                                  zip, sevenz-rust2 (7z), UnRAR via unrar_sys (RAR)
```

* **All parsing runs in the helper process**, under memory, time, and output
  limits. `mochi-core` and `mochi-format` never depend on this crate, and the
  app/CLI name it with `default-features = false`, which leaves out the
  `engine` feature (the `zip`, `tar`, `flate2`, and `ruzstd` crates). Both are checked by
  `ci/check-invariants.sh`.
* **tar.zst** is read by `ruzstd` (pure Rust, MIT; no C decoder in the helper).
  Frames are read one after another (concatenated and skippable frames are
  handled), the window is capped at 128 MiB, and a dictionary-compressed
  stream is refused. The decoder holds a window of output back until a frame
  ends, so a stream cut inside its first window reports "damaged or cut short
  before its first tar header", not a partial listing. A `.mochi` archive made
  with the TAR-compatible profile is also a valid tar.zst stream; the desktop
  routes MOCHI archives to the MOCHI reader first (magic wins over extension),
  so the foreign reader never sees them in the app.
* **The helper never writes a file** and no library's extract-to-disk function
  is used. It streams entry bytes; the C6 restore engine is the only writer, so
  every §10.4 protection (traversal, reserved names, symlink escapes, ...)
  applies unchanged. The helper does not judge names: it carries the exact
  bytes the archive stored, never normalized, case-folded, or decoded.
* **Nothing here uses MOCHI's verification wording or health dimensions.**
  Results name the check that ran ("CRC32 matched"). `recoverability`,
  `freshness`, and `retention` are fixed at `UNSUPPORTED`.

## Operations

| Op | What it does |
|---|---|
| `list` | Entry metadata only. For `.tar.gz` and `.tar.zst` this still decompresses the whole stream (tar has no index), under the same limits, which also reaches the gzip trailer check (or the zstd frame checksums). |
| `test` | Streams every entry through its decompressor, runs the format's own check, discards the bytes. |
| `read` | Streams one entry's bytes to the caller's sink. |
| `read_many` | (protocol 2) Streams the named entries **in one pass**, in the order the archive stores them, each announced, streamed, and closed with its outcome. A solid 7z or RAR block is decoded once, not once per entry. This is what the desktop's extraction uses; the C6 restore engine (`mochi_core::restore_foreign`) is the only writer. |

## What each format checks (and nothing more)

| Format | Per entry | Whole stream |
|---|---|---|
| ZIP | CRC32 ("CRC32 matched"); AES entries: the AES authentication code (AE-2 has no CRC32) | none |
| tar | none ("read without error; this format has no per-entry check") | none |
| tar.gz | none | gzip trailer CRC-32 + length; if it fails, **no** entry read from the stream is vouched for |
| tar.zst | none | each Zstandard frame's content checksum (XXH64, low 32 bits), **when the writer asked for one**: all frames checksummed and matching is "zstd frame checksum matched"; a frame without a checksum leaves the stream "no whole-stream check"; a mismatch fails the stream and no entry read from it is vouched for |
| 7z | CRC32 when the archive recorded one ("CRC32 matched"); an entry with no stream (directory, empty file) is "no data stream" | none (the start header and header have CRCs, which the library checks while opening: a statement about the header, not any entry) |
| RAR 4 | CRC32 | none |
| RAR 5 | CRC32 **or** BLAKE2sp, as the entry's header says; the outcome names which ("BLAKE2sp matched") | none |

* **ZipCrypto** entries are labelled `weak encryption (ZipCrypto)`. Its password
  test is one byte, so a wrong password can pass it and then fail the CRC:
  the outcome says "damaged data, or a wrong password that passed the weak
  password test".
* **Wrong password fails closed.** Encrypted entries are held in the helper
  until their authentication passes; a wrong password delivers no bytes.
* Split/spanned ZIP (`.z01`, a non-zero disk number) and self-extracting
  executables return `UNSUPPORTED_SPLIT_ZIP` / `UNSUPPORTED_SFX`.
* ZIP entries using bzip2, LZMA, zstd, or PPMd are listed and reported
  `unsupported` per entry (those decoders are not built in).
* Entries that share a name: the `zip` library keeps one per name. The helper
  compares its count with the end record's and reports the listing as
  **incomplete** with the reason, rather than presenting a smaller archive as
  the whole.

## Limits

`Limits` (what the helper may produce; every breach is reported by entry and
ends the operation cleanly, none panics):

| Limit | Default | Breach |
|---|---|---|
| `max_total_output` | 8 GiB | `LIMIT_TOTAL_OUTPUT` (stops the operation) |
| `max_entry_size` | 4 GiB | `LIMIT_ENTRY_SIZE` (skips that entry) |
| `max_entries` | 1,000,000 | `LIMIT_ENTRY_COUNT` (stops; the listing is partial) |
| `max_ratio` / `ratio_min_output` | 200 : 1, after 1 MiB | `LIMIT_COMPRESSION_RATIO` |
| `max_name_len` | 65,535 | `LIMIT_NAME_LENGTH` (the name is not carried or truncated) |

The ratio is evaluated per ZIP entry against the *declared* compressed size (a
claim) and for the whole operation against the *real* archive bytes (file
length; gzip bytes consumed). `RunLimits` bound the helper *process*: wall time
(default 10 min, then killed), address space (default 2 GiB, `RLIMIT_AS`, Unix
only), and listing bytes accepted (default 256 MiB). A panic, abort, memory
breach, hang, flood, or protocol violation becomes a typed `ForeignError`
(`HELPER_PANIC`, `HELPER_CRASHED`, `HELPER_TIMEOUT`, `HELPER_OUTPUT_LIMIT`,
`HELPER_PROTOCOL`); the caller is unaffected and the next call spawns a fresh
helper.

## Protocol, version 1

One helper process serves **one request**. The parent starts it with no
arguments (a password is never an argument), the helper writes `HELLO`, the
parent writes `REQUEST`, the helper streams messages until `DONE` or `ERROR`,
then exits. The parent treats the helper as untrusted: lengths, counts,
ordering, and totals are validated; anything off is `HELPER_PROTOCOL`.

Frame: `len: u32 LE` (counts `kind` + body; 1 ..= 1 MiB; validated before any
allocation), `kind: u8`, body. Integers are little-endian; *bytes* = `u32` length
+ data; *opt* = `u8` 0/1 flag then the value. Decoding is strict: unknown kinds
or codes, short bodies, and trailing bytes are errors.

| Kind | Direction | Body |
|---|---|---|
| `0x01 REQUEST` | parent → helper | `u16` version (1), `u8` op (1 list, 2 test, 3 read), `u64` read index, limits (`u64` total, entry, entries, ratio, ratio-min-output; `u32` name length), opt `u64` address-space limit, opt `u64` CPU seconds, *bytes* archive path (raw bytes on Unix, UTF-16LE on Windows), opt *bytes* password |
| `0x80 HELLO` | helper → parent | `u16` version, text id |
| `0x81 FORMAT` | h → p | `u8` 1 ZIP, 2 tar, 3 tar.gz, 4 7z, 5 RAR, 6 tar.zst |
| `0x82 ENTRY` | h → p | `u64` index, *bytes* name, `u8` kind, opt `u64` size, opt `u64` compressed size, encryption (`u8` + `u16` bits), text method, opt *bytes* link target, opt `u64` mode, opt `i64` mtime, `u8` UTF-8-flag state, opt text error |
| `0x83 RESULT` | h → p | `u64` index, outcome (tag `u8` + payload) |
| `0x84 DATA` | h → p | raw entry bytes (≤ 64 KiB per frame), `read` only |
| `0x85 STREAM_CHECK` | h → p | whole-stream check: not applicable / matched / failed / not reached |
| `0x86 BREACH` | h → p | `u8` kind, opt `u64` entry, `u64` limit, `u64` observed |
| `0x87 DONE` | h → p | `u8` complete, `u64` entries, `u64` bytes out, opt text stop reason |
| `0x8F ERROR` | h → p | `u8` code (helper-originated codes 1–9 only), text |

Order: `HELLO`, then (after the request) `FORMAT`, then per entry `ENTRY`
(+ `RESULT` for `test`) in index order with `BREACH` messages interleaved,
`STREAM_CHECK`, `DONE`. A version change is a new `PROTOCOL_VERSION` and a new
section here, never an edit.

## Tests

Hostile-archive fixtures live in `fixtures/foreign/` (generator and
explanation in its README). `tests/`: `zip_fixtures`, `tar_fixtures`,
`detect`, `limits`, `supervision` (crash/limit breach with the parent
surviving; helper fault hooks behind the `test-faults` feature, enabled only by
this crate's dev-dependency on itself), `protocol`, `sevenz_fixtures`, `rar_fixtures`, `corpus_interop`, `volumes`, `fuzz_smoke` (stable-Rust
exerciser over the corpus, truncations, and seeded flips; the cargo-fuzz targets
`foreign_zip`, `foreign_tar`, `foreign_targz`, `foreign_tarzst`, `foreign_protocol` call the same
functions in `mochi_foreign::fuzz`).

## 7z (`sevenz-rust2`, decoders only)

* The library's `compress` feature is off, and none of its extract-to-disk
  helpers is used (`ci/check-invariants.sh`). Methods read: COPY, LZMA, LZMA2,
  PPMd, BZip2, Deflate, BCJ/Delta filters, AES-256. The version is pinned to
  0.20.x because later releases need a newer compiler than the repository's
  pinned toolchain (rust 1.91).
* A solid block is decoded front to back, so entries come out **by block, not
  in listing order**: `test` collects outcomes and reports them in index
  order; `read_many` delivers in block order. A block with no wanted entry is
  not decoded at all.
* A wrong password has no verifier in 7z: it surfaces as a decoder error or a
  failed CRC32, and is worded "damaged data, or a wrong password". Encrypted
  entries are held in the helper until their check passes, so a wrong password
  delivers nothing. The largest entry held is 512 MiB; a bigger encrypted
  entry is reported `unsupported` rather than delivered unchecked.
* **Encrypted names** (`-mhe=on`): `list`/`test`/`read` without a password fail
  with `PASSWORD_REQUIRED` before anything is listed; a wrong one with
  `WRONG_PASSWORD`.
* **Split volumes**: a `name.7z.001` is read together with `.002, .003, ...`
  beside it, as one concatenated stream. A set that ends early is
  `MISSING_VOLUME` (the start header says where the next header ends); opening
  `.002` says to open the first volume.
* A header can claim a dictionary of up to 4 GiB. The memory a block's coders
  ask for is read from their properties (from the library's `Debug` form: the
  version in use keeps them private; `tests/sevenz_fixtures.rs` pins it) and a
  block over 1 GiB is refused as `unsupported` before anything is allocated.
* Names are the UTF-8 of what the archive stored; 7-Zip separates with `/`.

## RAR (UnRAR through `unrar_sys`)

* `unrar_sys` is the FFI crate of the `unrar` project. The safe `unrar`
  wrapper is not used: it buffers a whole entry in memory, hides the hash type,
  and unwraps on unknown flags. The module drives the C API itself, and
  `unrar_sys` 0.5.8 declares `RARHeaderDataEx` with the wrong layout (UnRAR
  compiles `dll.hpp` under `#pragma pack(push, 4)`), so `rarfmt.rs` declares
  its own structs; `layout` unit tests pin the offsets against a C++ compile
  of the vendored header.
* **Nothing is extracted by UnRAR.** Entries are processed with `RAR_TEST`
  (decode and verify, write nothing) and their bytes arrive through the data
  callback; `RAR_EXTRACT` and any destination argument are forbidden by
  `ci/check-invariants.sh`. Names are the exact characters UnRAR returns; on
  Windows its backslashes are separators and become `/`.
* **Never writes RAR.** The license of UnRAR forbids building a RAR-compatible
  archiver; the module only reads, and the fixtures are real files written by
  RARLAB's `rar` (see `fixtures/foreign/rar/PROVENANCE.txt`).
* Encrypted headers (RAR 5 and RAR 4 `-hp`): the password is supplied through
  UnRAR's callback while opening, so `PASSWORD_REQUIRED` / `WRONG_PASSWORD`
  come before listing. A RAR 4 wrong password has no verifier and shows as a
  failed CRC32 or a decoder error ("damaged data, or a wrong password"); RAR 5
  has a password check. Encrypted entries are held like 7z's.
* **Multi-volume** (`.partN.rar`, `.rNN`): UnRAR finds the next volume by name
  and asks, through the callback, when it is missing; that is reported
  (`MISSING_VOLUME` for an unusable start, otherwise the listing is incomplete
  and the affected entry is `read error: volume … is missing`). Opening a
  volume that is not the first says so. **Tested only with a missing volume
  and a later volume, never with a complete set** (a 1.0 limit [owner,
  2026-10-15]).
* A name that fills UnRAR's header buffer (1023 characters) is not carried
  (it would be a cut, wrong name); a dictionary over 1 GiB is refused.
* After a limit breach in the middle of an entry the operation stops
  (UnRAR cannot resume that entry); `complete` is false and the rest are not run.

## Protocol, version 2

Version 2 changes version 1 as follows (a version change is a new section, never
an edit of the one above, so the v1 text stays as the base):

* `HELLO` and `REQUEST` carry version `2`; a v1 peer is refused.
* `REQUEST` gains `u32` count + `u64` indices after the read index (`read_many`;
  at most 1,000,000, validated against the body length before allocating,
  sorted and de-duplicated on decode) and op `4 = read_many`.
* `FORMAT` gains `4 = 7z` and `5 = RAR`.
* `ERROR` gains helper codes `16 PASSWORD_REQUIRED`, `17 WRONG_PASSWORD`,
  `18 MISSING_VOLUME`; the parent-only code `19 CANCELLED` is never accepted
  from the wire.
* Check sets gain bit `8 = BLAKE2sp`.
* For `read_many`: per delivered entry `ENTRY`, any number of `DATA`, `RESULT`;
  entries may arrive in any order, each requested index exactly once unless
  `DONE` says the operation stopped early. `DATA` is bounded per entry by
  `max_entry_size` and overall by `max_total_output`.

Additive vocabulary since (still version 2: the app and the helper are built
from one tree and shipped together, and a peer that predates it rejects the new
code as a typed protocol error): `FORMAT` `6 = tar.zst`; check sets gain bit
`16 = zstd frame checksum`.

## Cancelling, progress, and the Windows memory limit

`ForeignClient::with_cancel(flag)` makes the parent kill the helper soon after
the flag is set and fail with `CANCELLED`; `test_with_progress` reports entries
finished. On Windows the **parent** puts the helper in a Job Object
(`jobobject.rs`: `JOB_OBJECT_LIMIT_PROCESS_MEMORY` plus kill-on-close) before it
is sent its request; if the limit cannot be applied the call fails rather than
running the helper unconfined. `tests/supervision.rs` has the test that a
breach is a reported crash and the parent survives. It runs on Windows in CI;
there is no Windows machine in the development environment.

## Shipping the helper

The app finds `mochi-foreign-helper(.exe)` next to its own executable
(`default_helper_path`). Development and the E2E job build it into the same
`target/<profile>` directory (`cargo build -p mochi-foreign`).

The installers ship it (T16). `packaging/build-installer.sh` builds the helper
(`--release`, `engine` feature), stages it as
`apps/mochi-desktop/src-tauri/binaries/mochi-foreign-helper-<target triple>[.exe]`
(git-ignored) and adds `bundle.externalBin` to the overlay config, so Tauri
installs it, without the suffix, beside the app: the NSIS install directory, or
`/usr/bin` in the `.deb`. `packaging/licenses.py` runs `cargo about` over this
crate's manifest as well as the desktop's and fails if the RARLAB UnRAR notice
(`THIRD-PARTY-NOTICES.md`) is missing. The smoke scripts run the installed helper
through `packaging/helper_probe.py` (a protocol-v2 `list` of
`fixtures/foreign/basic_infozip.zip`) and check uninstall removes it.

## Not done

* Windows: the Job Object and `unrar_sys` on MSVC are built and tested by CI
  only; cross-compiling the library for `x86_64-pc-windows-gnu` type-checks.
* No RAR writer is available, so no RAR fixture has a traversal name, a bomb,
  a BLAKE2sp hash, or a RAR 5 symlink; RAR BLAKE2sp reporting is covered by the
  wire and wording tests only. The RAR part of the pinned corpus is real
  archives written by RARLAB's `rar` (versions not recorded in the files; no
  WinRAR claim is made [owner, 2026-10-15]),
  compared with the expectations their authors wrote down, not with a second
  RAR decoder (Debian's 7-Zip omits the non-free one).
* Encrypted entries larger than 512 MiB (7z, RAR) are refused, not streamed;
  two passes (verify, then deliver) would lift it.
* ZIP: mtimes (DOS local time, not guessed); bzip2/LZMA/zstd/PPMd entries
  (reported `unsupported`); empty tar.gz (an all-zero first block).
* 7z: modes beyond the Unix extension in the attributes; symlink targets are
  not read at listing time; no SFX (`UNSUPPORTED_SFX`).
* Extraction applies the modification time only; archive permission bits
  (including the executable bit) are not applied, because the restore engine's
  attribute call also sets the owner, and a foreign owner must not be chowned.
* The cargo-fuzz targets `foreign_sevenz` and `foreign_rar` are in the CI
  matrix; see the plan status for what was run locally.
