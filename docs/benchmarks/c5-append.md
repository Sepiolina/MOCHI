# C5 benchmark: append cost versus prior commits

Two record sets: **schema 1** (plan T8/T9, current) and the original
**schema 0** run, kept below for comparison. Same dataset, settings, and VM.

Plan C5 exit criterion; reported per spec §27. Reproduce with
`cargo run --release -p mochi-testkit --example c5_append_bench [dir]`.

## Setup (§27 required statements)

| Item | Value |
|---|---|
| Dataset | Commit 0 creates directory `d`; each later commit adds one new 4 KiB pseudo-random (incompressible) file under it. History and catalog both grow linearly with N. |
| Hardware | Sandbox VM: 1 vCPU, Intel Xeon @ 2.80 GHz, 3 GiB RAM, ext4 on a virtio disk (`/dev/vda`), kernel 6.18. **Not representative hardware**: see the sync caveat below. |
| Cache state | Warm: the archive was just written by the same process. |
| Chunking | Fixed-size, 8 MiB maximum (product default, spec §13); each 4 KiB file is one chunk. |
| Compression | Zstandard level 3 (data is incompressible, so near-raw frames). |
| Encryption | None (Core profile). |
| Verification level | Opening = footer validation, commit decode, manifest and checkpoint stored-object hash, full SQLite + MOCHI catalog verification including namespace replay. No content-object verification. |
| Build | `--release`, Rust 1.91.1, bundled SQLite (rusqlite 0.40). |
| Measurement | Median of 7 appends per row; each append is a fresh writer session (open, commit, close), as one CLI invocation would be. |

## Results, schema 1 (plan T8/T9; milliseconds)

Every commit is still a checkpoint, and now binds three objects (D10.2):
delta manifest, **snapshot manifest**, catalog image. Columns changed:
*snapshot manifest KiB* is new; *checkpoint (snapshot + image)* now covers
building the snapshot from the catalog, encoding, hashing, and writing it, as
well as the image; *append open* is new and times `open_append`, which,
unlike `open_head`, also reads and decodes the head's snapshot manifest (the
writer needs its promised attributes). Second of two runs; the first run's
N=100 row was 2× slower in open and checkpoint and is treated as VM noise.
Run-to-run variation on this 1-vCPU VM is roughly 10–20% on the larger rows.

| prior commits | archive MiB | catalog image KiB | snapshot manifest KiB | footer lookup | open (verify + catalog) | append open (+ snapshot read) | head catalog copy + replay | content | catalog update | delta manifest | checkpoint (snapshot + image) | commit record | sync objects | footer + sync | **append total** (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.4 | 44 | 2 | 0.02 | 0.93 | 0.97 | 0.34 | 0.32 | 0.13 | 0.02 | 1.80 | 0.02 | 0.68 | 0.35 | **4.64** |
| 10 | 0.8 | 44 | 5 | 0.02 | 1.65 | 1.43 | 0.43 | 0.34 | 0.27 | 0.02 | 2.95 | 0.02 | 0.73 | 0.34 | **6.52** |
| 100 | 8.8 | 100 | 30 | 0.01 | 3.97 | 4.31 | 0.94 | 0.32 | 0.70 | 0.02 | 8.09 | 0.06 | 0.68 | 0.27 | **15.32** |
| 1000 | 403.2 | 472 | 287 | 0.01 | 44.20 | 41.13 | 6.99 | 0.55 | 7.12 | 0.05 | 86.16 | 0.26 | 1.44 | 0.49 | **145.29** |

**T10 note (not re-run).** T10 adds an 80-byte envelope header per image
and an additional image-sized copy in the current implementation (envelope
body, then frame). Performance and peak-memory impact were not remeasured;
evaluation is deferred to T14/G3. The storage increase (80 bytes per image)
is established; the runtime and memory effect of the extra copy is not, and
near the 268,434,864-byte image budget that copy is a buffer of the same size.

### What changed against schema 0

* **Append total at N=1000: 85 → 135–145 ms** (+60–70%, two runs). Almost
  all of it is the checkpoint phase (33 → 76–86 ms): a snapshot manifest is
  built from the catalog and written on every commit.
* **Archive size at N=1000: 262 → 403 MiB** (+54%). Each commit now stores
  the image (472 KiB) *and* a snapshot (287 KiB), so the quadratic growth
  noted below is steeper by the snapshot's share. This is the full-checkpoint
  writer by design until the checkpoint trigger (plan T14) makes most commits
  deltas; G3 is where that is measured.
* **Reading the head snapshot on append is not a significant term**: append
  open (41 ms) is within noise of `open_head` (44 ms), which does not read it.
* Snapshot size per file: 287 KiB / 1001 entries ≈ 294 bytes for a
  single-chunk file with *no* attributes and a 9-byte path. B.2.4 estimates
  318 bytes with POSIX attributes and a 22-byte path, which is consistent but
  **not** a G5 measurement: G5 needs the item count and attributed files.
* Unchanged: footer lookup constant; content constant per byte; open linear
  in catalog size.

## Results, schema 1 with adoption (plan T15, 2026-10-05; milliseconds)

Annex B.2 D10.7 and §18.1 require the writer to re-read both checkpoint
representations before publishing and compare each with the source state.
Adoption is mandatory and has no opt-out. It re-reads the snapshot manifest
and the image from storage, hash-verifies them, decodes the snapshot without
SQLite, and opens the image **exactly as a reader does** (SQLite integrity
check, foreign keys, every version's extents, full namespace replay). Its own
progress phase, `adopt`, sits between `checkpoint` and `commit-record`, so
the *checkpoint* column below is the same quantity as in the schema 1 table
above and *adopt* is new.

### Setup (§27)

| Item | Value |
|---|---|
| Dataset, chunking, compression, encryption, build, measurement | As the schema 1 setup above (4 KiB incompressible files, one commit each; fixed-size chunks; zstd 3; Core profile; `--release`; median of 7 fresh writer sessions per row). |
| Hardware | **Different from the earlier runs:** sandbox VM, 4 vCPU, Intel Xeon @ 2.10 GHz, 15 GiB RAM, ext4 on a virtio disk, kernel 6.18. The earlier tables ran on a 1-vCPU 2.80 GHz VM. **Absolute times are not comparable across the two machines**; the within-run *adopt / checkpoint* ratio is. |
| Cache state | Warm (the archive was just written by the same process). |
| Run | One clean run (an earlier attempt had two copies running at once and was discarded). Not repeated; run-to-run variation on these VMs is 10 to 20%. |

| prior commits | archive MiB | catalog image KiB | snapshot manifest KiB | footer lookup | open (verify + catalog) | append open (+ snapshot read) | head catalog copy + replay | content | catalog update | delta manifest | checkpoint (snapshot + image) | adopt (re-read + compare) | adopt / checkpoint | commit record | sync objects | footer + sync | **append total** (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.4 | 44 | 2 | 0.01 | 0.75 | 0.83 | 0.26 | 0.23 | 0.09 | 0.02 | 1.46 | 0.95 | 0.65 | 0.03 | 0.51 | 0.21 | **4.55** |
| 10 | 0.8 | 44 | 5 | 0.01 | 0.80 | 0.90 | 0.21 | 0.18 | 0.11 | 0.01 | 1.92 | 1.22 | 0.64 | 0.02 | 0.60 | 0.18 | **5.53** |
| 100 | 8.8 | 100 | 30 | 0.01 | 3.58 | 4.34 | 0.75 | 0.18 | 0.63 | 0.01 | 10.60 | 7.27 | 0.69 | 0.03 | 0.78 | 0.28 | **24.98** |
| 1000 | 403.3 | 472 | 287 | 0.01 | 29.62 | 38.28 | 5.88 | 0.33 | 5.44 | 0.02 | 93.54 | 63.49 | 0.68 | 0.18 | 1.51 | 0.47 | **207.10** |

### What this shows

* **Adoption costs about two thirds of the checkpoint phase at every size**
  (ratio 0.64 to 0.69, flat from N=1 to N=1000). It grows linearly with the
  catalog, like the checkpoint and the open, because it is the same work in
  the other direction: decode the snapshot, open and verify the image.
* **Share of the append:** with adoption removed, the same rows total about
  3.6, 4.3, 17.7, and 143.6 ms, so adoption adds roughly 26%, 28%, 41%, and
  44% (N = 1, 10, 100, 1000). At N=1000 the 143.6 ms agrees with the earlier
  schema 1 run (145 ms) although the hardware differs; treat that agreement as
  loose, not as a controlled comparison.
* **Not 10× the checkpoint phase at any N**, so the plan's stop-and-ask
  condition did not trigger.
* **Why it is kept as it is.** Adoption is required by §18.1 and D10.7; the
  full reader open is the point (it proves a reader can open what is about to
  be published). It is paid per checkpoint, and until the T14 trigger
  (Δ ≥ α·max(*B*, *F*)) every commit is a checkpoint. T14 makes checkpoints
  rare and so bounds this cost by design; the cost belongs to G3's inputs,
  not to a reason to weaken the check.
* Unchanged: footer lookup constant, content constant per byte, archive size
  as before (adoption writes nothing).

## Results, schema 0 (C5 original; milliseconds)

| prior commits | archive MiB | catalog image KiB | footer lookup | open (verify + catalog) | head catalog copy + replay | content | catalog update | manifest | checkpoint publish | commit record | sync objects | footer + sync | **append total** (ms) |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0.4 | 44 | 0.01 | 0.64 | 0.22 | 0.20 | 0.08 | 0.01 | 0.83 | 0.01 | 0.37 | 0.20 | **2.56** |
| 10 | 0.8 | 44 | 0.01 | 0.82 | 0.24 | 0.21 | 0.13 | 0.01 | 1.14 | 0.01 | 0.48 | 0.27 | **3.27** |
| 100 | 7.2 | 100 | 0.01 | 3.86 | 0.78 | 0.23 | 0.73 | 0.02 | 4.07 | 0.02 | 0.51 | 0.20 | **10.26** |
| 1000 | 261.9 | 472 | 0.01 | 35.81 | 6.60 | 0.40 | 6.65 | 0.03 | 32.68 | 0.08 | 1.32 | 0.39 | **84.68** |

Columns, in §27 terms: *footer lookup* is §27 "footer lookup"; *open* is
"catalog opening" plus "metadata replay" (verification replays the whole
namespace); *head catalog copy + replay* and *catalog update* are metadata
replay and dependency checks inside the commit; *checkpoint publish* is
serializing the full catalog image (VACUUM + serialize + hash + write);
*sync* columns are the §12.2 step 6 and step 8 flushes.

## What this shows

* **Footer lookup is constant** (0.01 ms at every size), as §27 allows.
* **Content cost is constant per byte** (0.2–0.4 ms for one 4 KiB file).
* **Everything metadata-shaped is linear in catalog size**: open/verify,
  the head-catalog copy and replay, the namespace check, and above all the
  full checkpoint publish. Append total grows 33× from N=1 to N=1000.
* **Archive size grows roughly quadratically.** 1000 commits holding about
  4 MiB of file data produce a **262 MiB** archive, because every commit
  stores a complete catalog image (472 KiB at the end). This is the direct
  cost of the C5 choice to write a checkpoint on every commit, because the
  spec does not define a metadata delta (plan §9, **O26**). It is not
  acceptable for a release; it is the strongest reason to settle O26 before
  C6, and a checkpoint interval (§18.1) will be needed once deltas exist.
* Opening also replays the whole namespace on every open (the C3 note:
  "C6 caches it"). That is the second linear term.

## Caveat on sync timings

0.2–1.3 ms per `fdatasync` is faster than most physical devices. Virtio
disks in this kind of VM commonly acknowledge flushes from a host cache, so
these numbers show protocol overhead, not device durability latency. Real
numbers need the CI runners named in plan O11 (Ubuntu and Windows), on local
disks, reported with the same table.
