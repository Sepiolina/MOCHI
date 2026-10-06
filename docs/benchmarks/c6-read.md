# C6: read path, decomposed (spec §27)

Plan C6 exit: "Benchmarks per §27: footer lookup, catalog open, replay,
selected-file read — each reported separately, with dataset, hardware, and
cache state." Measured 2026-10-06. Reproduce with

```text
cargo run --release -p mochi-testkit --example c6_read_bench [dir]          # warm
sudo cargo run --release -p mochi-testkit --example c6_read_bench [dir] --cold
```

## Setup (§27 required statements)

| Item | Value |
|---|---|
| Dataset | One archive, 73,299,098 bytes, one replay segment. Commit 0 (checkpoint): directory `small/` with 1,000 files of 4 KiB, plus `large.bin`, 64 MiB; all pseudo-random (incompressible), POSIX attributes. Then 200 delta commits, one new 4 KiB file each. Base image 467,032 bytes; 1,201 files at the head. |
| Writer | Production defaults: checkpoint trigger α = 1, *F* = 1 MiB (the 200 deltas stay in commit 0's segment, so opening the head replays all 200). |
| Hardware | Cloud VM container: 4 vCPU Intel Xeon @ 2.10 GHz, 15 GiB RAM, kernel 6.18.44, ext4 on a virtio disk (`/dev/vda`). Single-threaded; nothing else running. |
| Storage | A real file read through `OsReadStorage` (positional reads, no `mmap`). |
| Cache state | **Warm:** one discarded warm-up, then 7 samples. **Cold:** `sync` and `drop_caches` (guest page cache) before each of 7 samples, and again between opening and reading for file reads. The host's cache below the VM is not controlled, so "cold" is guest-cold only. |
| Chunking | Fixed-size, 8 MiB maximum (product default): `large.bin` is 8 chunks; each small file is 1. |
| Compression | Zstandard level 3 (product default), checksum on. |
| Encryption | None (Core profile). |
| Verification level | Opening: footer; commit record, descriptor, delta manifest, and catalog image each hash-verified (stored-object hash) before parsing; envelope binding; SQLite integrity and MOCHI catalog checks on the base image; every replayed delta decoded, identity-bound, linked, and applied under D10.4. File read: every chunk checked as stored and decoded bytes, plus the whole-file content hash. |
| Build | `--release`, Rust 1.91.1, bundled SQLite (libsqlite3-sys 0.38.2), zstd 1.5.7. |
| Statistic | Median of 7, with minimum and maximum. |

What each row covers (all public API; see the example's header):

* **footer lookup** — `publish::locate_head`: the footer at end of file is
  validated. The tail is clean, so there is no scan.
* **catalog open** — `publish::open_at_footer` at the base checkpoint:
  commit record, descriptor, and delta manifest read and verified; the
  catalog image read, verified, deserialized into SQLite, and checked. No
  replay.
* **metadata replay** — **derived**, not timed alone: `open_head` minus
  catalog open minus footer lookup (medians). The public API has no
  replay-only entry point.
* **selected-file read** — `read::read_file` of one file into a sink, the
  head already open (not timed).

## Results

### Warm

| measurement | median ms | min ms | max ms | covers |
|---|---:|---:|---:|---|
| footer lookup | 0.003 | 0.003 | 0.003 | `locate_head`: footer at EOF validated |
| catalog open | 27.334 | 25.815 | 27.748 | `open_at_footer` at the base checkpoint, no replay |
| (open head) | 41.901 | 40.681 | 43.340 | `open_head`: lookup + base open + replay |
| metadata replay | 14.564 | – | – | derived: open head − catalog open − footer lookup (medians), 200 deltas |
| selected-file read, 4 KiB | 2.682 | 2.618 | 2.918 | `read_file` to a sink, verified |
| selected-file read, 64 MiB | 89.565 | 84.379 | 96.680 | `read_file` to a sink, verified; 715 MiB/s at the median |

### Cold (guest page cache dropped)

| measurement | median ms | min ms | max ms | covers |
|---|---:|---:|---:|---|
| footer lookup | 0.233 | 0.174 | 18.362 | `locate_head`: footer at EOF validated |
| catalog open | 26.904 | 25.881 | 28.372 | `open_at_footer` at the base checkpoint, no replay |
| (open head) | 51.228 | 49.547 | 53.790 | `open_head`: lookup + base open + replay |
| metadata replay | 24.092 | – | – | derived: open head − catalog open − footer lookup (medians), 200 deltas |
| selected-file read, 4 KiB | 3.085 | 2.885 | 4.455 | `read_file` to a sink, verified |
| selected-file read, 64 MiB | 96.995 | 94.812 | 224.221 | `read_file` to a sink, verified; 660 MiB/s at the median |

## Reading the numbers

* **Footer lookup is constant-time and negligible** (3 µs warm; 0.2 ms
  cold median, one 72-byte read; one cold sample took 18 ms). §27: "A
  footer seek may be constant-time."
* **Opening is dominated by the catalog, not the footer.** Catalog open
  (~27 ms for a 456 KiB image) costs the same warm and cold here, which
  suggests it is CPU-bound (verification, SQLite deserialization and
  checks) rather than I/O-bound; not profiled.
* **Replay of 200 deltas** adds ~15 ms warm, ~24 ms cold (each delta is a
  separate small read). It grows with the segment, which the checkpoint
  trigger bounds (T32: `t32-scaling.md`). §27: "Opening an archive with an
  unbounded metadata chain is not necessarily constant-time"; this one is
  bounded by B.2.3.
* **Selected-file read** costs ~2.7 ms for 4 KiB (dominated by catalog
  queries and per-file setup) and ~90 ms for 64 MiB (~700 MiB/s, warm),
  which is proportional to the content produced, as §27 says it must be at
  least. Cold reads are ~10% slower; one cold 64 MiB sample took 224 ms
  (host-side cache miss, presumably; not controlled).
* **Not measured here**, because C6 does not claim them: dependency lookup
  and decryption (C9, C11), search (C13), full verification and restoration
  (C7). Restore to a directory adds directory and file creation and syncs
  on top of selected-file read.
