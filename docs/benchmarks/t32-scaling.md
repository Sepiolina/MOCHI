# T32: checkpoint-trigger scaling (G3)

Spec Annex B.2.3 (checkpoint trigger, storage bound) and B.2.6 gate G3:
"Four workloads, each from 10 to 10,000 commits: a growing catalog,
repeated updates, tiny commits, and a large import. Plus a forced-checkpoint
run. Report Δ, *B*, *k*, replay operations, and open time. The α/*F*
defaults are confirmed or revised from these results." Reproduce with
`cargo run --release -p mochi-testkit --example t32_scaling [max_n] [α as n/d] [F]`
(`MOCHI_T32_ONLY=<workload>` runs one workload).

## Setup (§27 required statements)

| Item | Value |
|---|---|
| Dataset | Per workload, below. Files are 256 bytes of pseudo-random data with POSIX attributes. |
| Trigger | Production policy (T14), default α = 1, *F* = 1 MiB unless stated. |
| Hardware | Cloud container: 4 vCPU Intel Xeon @ 2.10 GHz, 15 GiB RAM, kernel 6.18. Runs are single-threaded; the α = 1/2 and α = 2 runs and the forced run shared the machine (3 processes on 4 vCPUs). |
| Storage, cache | In memory (`SimStorage`): no syncs, warm. Write times are CPU and SQLite cost only (the C5 benchmark covers syncs). |
| Chunking | Fixed-size, 8 MiB maximum (product default); every file is one chunk. |
| Compression | Zstandard level 3. |
| Encryption | None (Core profile). |
| Verification level | Opening = footer, commit, descriptor, and every referenced object's stored-object hash; envelope binding; SQLite integrity and MOCHI catalog verification of the base image; each replayed delta decoded, identity-bound, linked (Q22), and applied under D10.4. No content-object reads. |
| Build | `--release`, Rust 1.91.1, bundled SQLite (libsqlite3-sys 0.38.2), zstd 1.5.7. |
| Open time | Median of 5. "Head" opens the last commit; "longest replay" opens the commit with the largest Δ at its decision. |

Every figure is computed from the archive bytes (history entries and object
references), not from the writer's counters. Columns:

* **ΣΔ**: delta-manifest, commit-record, and footer bytes of the delta
  commits, as the trigger counts them. **final B**: the last base's image +
  snapshot. **metadata**: every byte that is not a data object.
* **bound as worded**: (1 + 1/α + *k*)·ΣΔ + *B*₀ + Σ*B*_forced with *B*₀ =
  commit 0's image + snapshot. **bound, full B₀**: the same with ΣΔ = every
  commit after 0 (checkpoint commits' own records included) and *B*₀ = all of
  commit 0's metadata, as B.2.3 now defines them. **✗** marks a violated bound.
* **k**: (*B*ᵢ − *B*ᵢ₋₁)/Δᵢ per triggered interval; "–" when the trigger
  never fired after commit 0.
* **replay limit**: α·max(*B*, *F*) at the base plus that commit's own
  metadata, B.2.3's replay claim.

## Results (α = 1, *F* = 1 MiB)

Workloads: **growing catalog** (one new file per commit); **repeated
updates** (100 files at commit 0, then one rewritten per commit);
**tiny commits** (empty transactions); **large import** (10,000 files and
their directories at commit 0, then one new file per commit); **forced**
(growing catalog with `request_checkpoint` every N/10 commits).

### growing catalog (one new file per commit)

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 | 6.8 | 44.5 | 0.051 | 0.057 | 0.058 | 0.00 | – | – | 9 | 6.8 | 1024.8 | 0.9 | 1.0 | 0.0 |
| 100 | 1 | 76.0 | 44.5 | 0.118 | 0.192 | 0.193 | 0.03 | – | – | 99 | 76.0 | 1024.8 | 6.2 | 6.2 | 0.1 |
| 1000 | 1 | 772.4 | 44.5 | 0.798 | 1.552 | 1.553 | 0.26 | – | – | 999 | 772.4 | 1024.8 | 61.2 | 62.4 | 5.6 |
| 10000 | 5 | 7772.5 | 7234.2 | 21.244 | 22.327 | 22.338 | 2.57 | 0.94 | 0.93 | 4823 | 3753.7 | 3754.0 | 325.6 | 507.3 | 556.7 |

### repeated updates (100 files, one rewritten per commit)

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 | 6.9 | 118.0 | 0.152 | 0.129 **✗** | 0.158 | 0.03 | – | – | 9 | 6.9 | 1024.8 | 3.1 | 3.0 | 0.0 |
| 100 | 1 | 76.1 | 118.0 | 0.219 | 0.264 | 0.294 | 0.05 | – | – | 99 | 76.1 | 1024.8 | 8.4 | 8.1 | 0.1 |
| 1000 | 1 | 771.5 | 118.0 | 0.898 | 1.622 | 1.652 | 0.28 | – | – | 999 | 771.5 | 1024.8 | 68.7 | 67.7 | 5.4 |
| 10000 | 5 | 7746.0 | 2882.2 | 14.194 | 19.336 | 19.374 | 2.60 | 0.54 | 0.54 | 3368 | 2611.4 | 2883.0 | 448.4 | 445.8 | 495.3 |

### tiny commits (empty transactions)

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 | 4.1 | 44.2 | 0.048 | 0.051 | 0.052 | 0.00 | – | – | 9 | 4.1 | 1024.5 | 0.4 | 0.4 | 0.0 |
| 100 | 1 | 45.8 | 44.2 | 0.088 | 0.133 | 0.133 | 0.00 | – | – | 99 | 45.8 | 1024.5 | 1.3 | 1.3 | 0.1 |
| 1000 | 1 | 467.0 | 44.2 | 0.500 | 0.955 | 0.956 | 0.00 | – | – | 999 | 467.0 | 1024.5 | 10.2 | 10.1 | 3.5 |
| 10000 | 5 | 4714.4 | 132.2 | 5.037 | 9.359 | 9.363 | 0.00 | 0.02 | 0.02 | 2188 | 1024.4 | 1024.5 | 90.2 | 22.6 | 330.0 |

### large import (10,000 files at commit 0, then one file per commit)

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1 | 6.9 | 7389.1 | 10.304 | 7.229 **✗** | 10.311 | 2.58 | – | – | 9 | 6.9 | 7389.9 | 221.9 | 218.9 | 2.7 |
| 100 | 1 | 76.7 | 7389.1 | 10.372 | 7.366 **✗** | 10.447 | 2.60 | – | – | 99 | 76.7 | 7389.9 | 235.7 | 240.4 | 6.3 |
| 1000 | 1 | 778.4 | 7389.1 | 11.057 | 8.736 **✗** | 11.818 | 2.83 | – | – | 999 | 778.4 | 7389.9 | 298.6 | 287.4 | 47.4 |
| 10000 | 2 | 7800.7 | 14282.8 | 31.864 | 29.559 **✗** | 32.642 | 5.15 | 0.93 | 0.93 | 9472 | 7389.3 | 7389.9 | 585.7 | 965.4 | 1016.3 |

### forced checkpoints (growing catalog, `checkpoint` every N/10 commits)

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 10 | 10 | 0.0 | 47.2 | 0.455 | 0.447 **✗** | 0.463 | 0.00 | – | – | 0 | 0.0 | 0.0 | 0.6 | 0.3 | 0.0 |
| 100 | 10 | 69.4 | 111.7 | 0.791 | 0.851 | 0.866 | 0.03 | – | – | 9 | 7.0 | 1024.8 | 4.7 | 2.5 | 0.2 |
| 1000 | 10 | 768.7 | 701.7 | 4.445 | 5.188 | 5.203 | 0.26 | – | – | 99 | 77.1 | 1024.8 | 34.3 | 15.4 | 6.3 |
| 10000 | 10 | 7769.9 | 6537.5 | 39.769 | 47.349 | 47.364 | 2.57 | – | – | 999 | 777.5 | 1024.8 | 363.2 | 97.9 | 562.1 |

## α sensitivity (growing catalog)

### α = 1/2

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
| 10 | 1 | 6.8 | 44.5 | 0.051 | 0.063 | 0.064 | 0.00 | – | – | 9 | 6.8 | 512.8 | 0.9 | 1.0 | 0.0 |
| 100 | 1 | 76.0 | 44.5 | 0.118 | 0.266 | 0.267 | 0.03 | – | – | 99 | 76.0 | 512.8 | 6.6 | 6.3 | 0.1 |
| 1000 | 2 | 772.9 | 530.1 | 1.317 | 3.024 | 3.027 | 0.26 | 0.95 | 0.95 | 663 | 512.2 | 512.8 | 42.8 | 40.5 | 5.7 |
| 10000 | 8 | 7772.8 | 6793.3 | 26.946 | 30.083 | 30.106 | 2.57 | 0.96 | 0.93 | 2983 | 2321.6 | 2322.4 | 351.0 | 428.0 | 556.5 |

### α = 2

| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |
| 10 | 1 | 6.8 | 44.5 | 0.051 | 0.053 | 0.054 | 0.00 | – | – | 9 | 6.8 | 2048.8 | 0.9 | 0.9 | 0.0 |
| 100 | 1 | 76.0 | 44.5 | 0.118 | 0.155 | 0.156 | 0.03 | – | – | 99 | 76.0 | 2048.8 | 6.0 | 6.0 | 0.1 |
| 1000 | 1 | 772.4 | 44.5 | 0.798 | 1.175 | 1.176 | 0.26 | – | – | 999 | 772.4 | 2048.8 | 64.7 | 63.6 | 5.6 |
| 10000 | 3 | 7768.9 | 5565.6 | 14.971 | 18.475 | 18.480 | 2.57 | 0.93 | 0.93 | 5008 | 3897.6 | 3897.9 | 422.7 | 452.4 | 556.9 |

## Findings

1. **The trigger behaves as B.2.3 says.** In every row the longest replay is
   below its limit, α·max(*B*, *F*) plus one commit; at 10,000 commits it
   lands within a few hundred bytes of it (growing: 3,753.7 of 3,754.0 KiB;
   tiny: 1,024.4 of 1,024.5 KiB; import: 7,389.3 of 7,389.9 KiB), which is
   the trigger firing on the first commit that reaches the threshold. No
   commit is skipped and none fires early.
2. **The storage bound holds once its terms are defined (B.2.3 amended).**
   With *B*₀ = all of commit 0's metadata and ΣΔ over every commit after 0,
   no row violates it. As worded before, it failed for every size of the
   large import and for small runs of the repeated-updates and forced
   workloads, because commit 0's own delta manifest (3.6 MiB for the
   10,000-file import) is outside both terms.
3. ***k* is below 1** on every workload: 0.93 to 0.96 (growing catalog,
   large import), 0.54 (repeated updates), 0.02 (tiny commits). A checkpoint
   costs at most about one interval's Δ more than its base, so the bound's
   coefficient 1 + 1/α + *k* is about 2.9 at α = 1. Measured metadata at
   10,000 commits is 2.8× ΣΔ for the growing catalog and 1.1× for tiny
   commits. These are estimates over these workloads, not a proof (B.2.3
   "Evidence, not proof").
4. **Open time = base open + replay.** Replay costs about 10 µs (empty
   deltas) to 60 µs (one-file deltas) per delta in memory (1,000 deltas:
   61 ms growing, 10 ms tiny). Base open grows
   with the catalog: 220 ms for the 7 MiB image of the 10,000-file import,
   rising to 586 ms at its head with a 14 MiB base. The worst open measured
   is 965 ms (import, 9,472 deltas on a 7 MiB base). Opening a delta head is
   never worse than base open plus the bounded replay.
5. **Forced checkpoints cost Σ*B*_forced, as the bound says.** Ten forced
   checkpoints in 10,000 commits of a growing catalog: 39.8 MiB of metadata
   against 21.2 MiB without them, and the longest replay drops from 4,823 to
   999 deltas.
6. **Writer cost per commit grows with the catalog** (found here, not a
   format property): 10,000 commits took 557 s for the growing catalog
   (5.6 s for the first 1,000), 1,016 s for the large import. Every commit,
   delta or checkpoint, copies the writer's in-memory catalog before applying
   the transaction (the C5 "head catalog copy" phase). Recorded for C6 as a
   writer optimization; it does not affect the archive or the bound.

## Decision: α and *F*

| α | checkpoints | metadata | longest replay | head open | worst open |
|---:|---:|---:|---:|---:|---:|
| 1/2 | 8 | 26.9 MiB | 2,983 deltas, 2.3 MiB | 351 ms | 428 ms |
| **1** | **5** | **21.2 MiB** | **4,823 deltas, 3.8 MiB** | **326 ms** | **507 ms** |
| 2 | 3 | 15.0 MiB | 5,008 deltas, 3.9 MiB | 423 ms | 452 ms |

(Growing catalog, 10,000 commits. Open times are medians of 5 on a shared
machine; differences under about 100 ms are within noise here.)

**α = 1 and *F* = 1 MiB are confirmed, not revised.** Halving α costs 27%
more metadata for about 80 ms on the worst open; doubling it saves 30% of
metadata, and the measured worst open does not move beyond noise, but the
replay *guarantee* doubles to 2·max(*B*, *F*), which is what a user with a
large base would feel (estimated from twice the replay: the import's worst open would rise
toward 1.4 s).
α = 1 keeps checkpoint storage at most equal to delta storage (1/α = 1) and
replay at most one base's worth. *F* = 1 MiB only decides while *B* < *F*
(small catalogs): there the worst replay is about 2,200 empty deltas and
under 100 ms (tiny commits: 90 ms head, 22.6 ms longest replay), so a
smaller floor would add checkpoints without a visible gain. Both stay
configurable (`WriterOptions::checkpoint_trigger`); α = 2 is a reasonable
choice for storage-constrained archives.
