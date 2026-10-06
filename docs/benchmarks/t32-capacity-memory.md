# T32: capacity (G5) and memory (G4)

Spec Annex B.2.4 (capacity) and B.2.3 (the resource rows "Catalog memory"
and "Decoded-CBOR memory"); gates G4 and G5 (B.2.6). Measured from the MOCHI
writer and codec, replacing B.2.4's estimates, which came from a
general-purpose CBOR library. Reproduce with
`cargo run --release -p mochi-testkit --example t32_capacity`.

## Setup (§27 required statements)

| Item | Value |
|---|---|
| Dataset | One commit (commit 0) importing N files into one directory: each file single-chunk, 64 bytes of pseudo-random data, POSIX attributes (mode, uid, gid, mtime with nanoseconds), path exactly 22 bytes (`dir00/file-NNNNNNNNNNN`), the shape B.2.4's estimate assumed. N = 1,000, 4,000, 16,000. Multi-chunk: 1,000 files of 1, 2, and 5 chunks. |
| Hardware | Cloud container: 4 vCPU Intel Xeon @ 2.10 GHz, 15 GiB RAM, kernel 6.18. |
| Storage, cache | In memory (`SimStorage`); warm. |
| Chunking | Fixed-size, 64-byte chunks (so a file's chunk count is set exactly). |
| Compression | Zstandard level 3. |
| Encryption | None (Core profile). |
| Verification level | Opening the head = footer, commit, descriptor, delta manifest, and image stored-object hashes, envelope binding, SQLite integrity and MOCHI catalog verification. Snapshot decode = stored-object hash, canonical-CBOR decode, schema and identity checks. |
| Build | `--release`, Rust 1.91.1, bundled SQLite (libsqlite3-sys 0.38.2), zstd 1.5.7. |
| Item counting | Every CBOR data item, map keys included, as `CborLimits::max_items` counts them (T4). |
| Memory | A counting global allocator in the example: peak heap growth above the level at the start of the measured call. |

## Capacity (G5)

| files | snapshot bytes | snapshot items | image bytes |
|---:|---:|---:|---:|
| 1,000 | 320,446 | 49,048 | 475,224 |
| 4,000 | 1,286,323 | 196,048 | 1,769,560 |
| 16,000 | 5,149,831 | 784,048 | 6,873,176 |

Per file (slope from 1,000 to 16,000 files): **322.0 snapshot bytes, 49.00
items, 426.5 image bytes.** The item count is exact (49 per file plus 48
fixed); bytes vary with varint widths.

Files at each reader-default limit, from those slopes:

| limit | value | files | B.2.4 estimate |
|---|---:|---:|---:|
| CBOR items per record | 16 Mi (16,777,216) | **342,391** | about 340,000 |
| Skippable payload (snapshot) | 268,435,456 bytes | 833,761 | about 840,000 |
| Image budget | 268,434,864 bytes | 629,231 | roughly 600,000 |

**The item limit binds first**, at about 342,000 files of this shape, as
B.2.4 said. Every estimate is within 5% of the measurement.

### Multi-chunk files (1,000 files)

| chunks per file | snapshot bytes | snapshot items | image bytes |
|---:|---:|---:|---:|
| 1 | 320,446 | 49,048 | 475,224 |
| 2 | 484,446 | 73,048 | 749,656 |
| 5 | 978,446 | 145,048 | 1,605,720 |

Per extra chunk: **24.00 items, 164.5 snapshot bytes, 282.6 image bytes**
(B.2.4 estimated about 25 items). A file of *c* chunks costs 49 + 24(*c* − 1)
items, so with large files the item limit binds sooner: at 8 MiB chunks, a
store of 1 GiB files (128 chunks each) reaches 16 Mi items at about 5,400
files.

## Memory (G4)

| files | snapshot decode peak | per decoded item | per stored byte | head open peak | per image byte |
|---:|---:|---:|---:|---:|---:|
| 1,000 | 2,443,123 B | 49.8 B | 7.62 | 2,463,500 B | 5.18 |
| 4,000 | 9,773,096 B | 49.9 B | 7.60 | 9,854,873 B | 5.57 |
| 16,000 | 39,090,988 B | 49.9 B | 7.59 | 39,420,365 B | 5.74 |

* **Decoded-CBOR memory: about 50 bytes per item, 7.6× the stored size.**
  The format does not bound it (B.2.3); the item limit does. At the default
  16 Mi items, one snapshot decode peaks near **800 MiB**.
* **Catalog memory: about 5.2 to 5.7× the image size, rising slowly with
  size**, not "≈ image size" as the B.2.3 table says. The open holds the
  stored image, SQLite's in-memory copy, and verification's working set at
  once. At the full image budget (256 MiB) that extrapolates to about
  **1.5 GiB**.

These are peaks for one operation in a process that holds nothing else
large. They are the inputs B.2.3's "measured (G4)" rows asked for, not
limits.

## What changes in the spec (recorded in the checklist, Q49–Q51)

* B.2.4's figures are replaced by these measurements, and its "unmeasured"
  label is removed.
* B.2.3's "Catalog memory ≈ image size" is replaced by the measured ratio.
