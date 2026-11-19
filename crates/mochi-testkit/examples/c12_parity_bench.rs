//! C12 / R6 benchmark (spec §27; plan W14, K15 items 3 and 5): what the
//! Redundancy profile (D21) costs to write, to verify, and to rebuild with.
//! Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example c12_parity_bench [dir] [--quick]
//! ```
//!
//! Three tables, all on real `OsStorage` with real `fdatasync` on `dir`
//! (default: the system temp dir; the filesystem is printed):
//!
//! 1. **Write**: one commit of a single incompressible file of 100 KiB,
//!    4 MiB, 64 MiB, and 512 MiB at 0 (no recovery record), 1, 5, 10, 50, and
//!    100 %: whole-commit time, the parity phase's share of it (from the
//!    writer's progress phases), the file's growth, the parity bytes the
//!    layout rule predicts against the growth measured, and the hash tables'
//!    share of the parity frames (K15 item 3).
//! 2. **Verify**: `stored_integrity` over the same archives with and without
//!    a recovery record, cold and warm.
//! 3. **Rebuild**: `parity::scan` after damaging *m* data shards in every
//!    group (the capacity), cold and warm, with every rebuilt shard compared
//!    byte for byte with the pristine file.
//!
//! `--quick` drops the 512 MiB rows. Cache: "warm" means the archive was
//! just written or just read by this process; "cold" means the page cache
//! was dropped first (`/proc/sys/vm/drop_caches`), which needs privilege and
//! is reported as unavailable when it fails. Dataset: deterministic
//! pseudo-random bytes (incompressible, so Zstandard level 3 emits near-raw
//! frames and the stored size is the decoded size plus framing).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mochi_core::descriptor::Profile;
use mochi_core::job::{CancellationToken, JobContext, NullProgress, ProgressEvent, ProgressSink};
use mochi_core::parity::scan;
use mochi_core::publish::{
    ArchiveWriter, CheckpointPolicy, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_core::storage::ReadStorage;
use mochi_core::verify::{verify, VerifyOptions};
use mochi_format::parity::Layout;
use mochi_testkit::archive::path;
use mochi_testkit::{deterministic_bytes, SeqIds};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const PERCENTS: &[u16] = &[0, 1, 5, 10, 50, 100];
const FLIP: u8 = 0xA5;
/// Required features in a Core-profile commit (the Encrypted profile has one).
const REQUIRED_FEATURES: usize = 0;

/// Records when each writer phase first appears.
#[derive(Default)]
struct Phases(Mutex<Vec<(&'static str, Instant)>>);

impl ProgressSink for Phases {
    fn report(&self, e: &ProgressEvent) {
        let mut v = self.0.lock().unwrap();
        if v.last().map(|(p, _)| *p) != Some(e.phase) {
            v.push((e.phase, Instant::now()));
        }
    }
}

fn options(percent: u16) -> WriterOptions {
    WriterOptions {
        threads: None,
        read: ReadOptions::default(),
        chunk_size: None,
        zstd_level: None,
        record_time: false,
        profile: (percent != 0).then(|| Profile {
            redundancy: Some(percent),
            ..Profile::default()
        }),
        checkpoint_trigger: None,
        dedup: Default::default(),
        kdf: None,
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e3)
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB as f64
}

fn size_label(bytes: u64) -> String {
    if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes / KIB)
    }
}

/// Samples per size: the larger the commit, the fewer repeats (medians of
/// few samples are stated as such).
fn samples_for(bytes: u64) -> usize {
    match bytes {
        b if b <= 4 * MIB => 7,
        b if b <= 64 * MIB => 5,
        _ => 1,
    }
}

/// Write one commit of `bytes` to a new archive at `file`; returns
/// (whole commit, parity phase). The parity phase runs from the writer's
/// first `parity` progress event to its first `sync-content` event.
fn write_archive(file: &Path, bytes: u64, percent: u16) -> (Duration, Duration) {
    let token = CancellationToken::new();
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(file).unwrap(),
        Box::new(SeqIds::new(1)),
        options(percent),
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(
        path("f"),
        deterministic_bytes(7, bytes as usize),
        Default::default(),
    );
    let phases = Phases::default();
    let ctx = JobContext {
        progress: &phases,
        cancel: &token,
    };
    let t = Instant::now();
    w.commit(tx, &ctx).unwrap();
    let total = t.elapsed();
    w.close().unwrap();
    let p = phases.0.lock().unwrap().clone();
    let at = |name: &str| p.iter().find(|(n, _)| *n == name).map(|(_, t)| *t);
    let parity = match (at("parity"), at("sync-content")) {
        (Some(a), Some(b)) => b - a,
        _ => Duration::ZERO,
    };
    (total, parity)
}

fn file_len(file: &Path) -> u64 {
    std::fs::metadata(file).unwrap().len()
}

/// Whether the page cache can be dropped here; flushes first.
fn drop_caches() -> bool {
    std::fs::write("/proc/sys/vm/drop_caches", "3").is_ok()
}

fn verify_stored(file: &Path) -> (Duration, Status) {
    let r = OsReadStorage::open(file).unwrap();
    let token = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &token,
    };
    let t = Instant::now();
    let v = verify(
        &r,
        &VerifyOptions {
            level: VerificationLevel::StoredIntegrity,
            ..VerifyOptions::default()
        },
        &ctx,
    );
    (t.elapsed(), v.report.dimensions[&Dimension::Integrity])
}

/// The commit's layout, from the pristine archive's own frames.
fn layout_of(file: &Path, percent: u16) -> (Layout, u64) {
    let r = OsReadStorage::open(file).unwrap();
    let token = CancellationToken::new();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &token,
    };
    let (sc, _) = scan(&r, &ReadOptions::default(), &ctx).unwrap();
    let c = &sc.commits[0];
    let l = Layout::compute(c.range_offset.unwrap(), c.range_len.unwrap(), percent).unwrap();
    // Measured: the bytes between the commit frame's end and its footer.
    let measured = c.footer_offset - (l.range_offset + l.range_len);
    (l, measured)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    let base = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(&base).unwrap();
    let mut sizes = vec![100 * KIB, 4 * MIB, 64 * MIB];
    if !quick {
        sizes.push(512 * MIB);
    }
    let cold_ok = drop_caches();

    println!("# C12 parity benchmark");
    println!("archive dir: {}", dir.path().display());
    println!("cold cache available: {cold_ok}");
    println!("sizes: {sizes:?} bytes; percents: {PERCENTS:?}");

    // ---- 1. write ----------------------------------------------------------
    println!("\n## Write\n");
    println!("| commit | r % | s KiB | groups G | k | m | frames | whole commit ms | parity phase ms | parity MiB/s of commit | file MiB | file growth over r=0 MiB | parity frames MiB (measured = predicted) | parity frames as % of the commit | table share of parity bytes |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    let mut baseline_len = 0u64;
    for &bytes in &sizes {
        // Unmeasured warm-up at this size: allocator and filesystem state.
        let warm = dir.path().join("warm.mochi");
        let _ = write_archive(&warm, bytes, 0);
        std::fs::remove_file(&warm).unwrap();
        for &r in PERCENTS {
            let file = dir.path().join(format!("w-{bytes}-{r}.mochi"));
            let n = samples_for(bytes);
            let mut totals = Vec::new();
            let mut phases = Vec::new();
            for i in 0..n {
                if i > 0 {
                    std::fs::remove_file(&file).unwrap();
                }
                let (t, p) = write_archive(&file, bytes, r);
                totals.push(t);
                phases.push(p);
            }
            let len = file_len(&file);
            if r == 0 {
                baseline_len = len;
            }
            if r == 0 {
                println!(
                    "| {} | none | | | | | | {} | | | {:.2} | 0 | 0 | | |",
                    size_label(bytes),
                    ms(median(totals)),
                    mib(len)
                );
            } else {
                let (l, measured) = layout_of(&file, r);
                let frame = l.frame_len(REQUIRED_FEATURES);
                let predicted = l.frame_count() * frame;
                let tables = l.frame_count() * 32 * l.data_shards;
                let parity_ms = median(phases);
                let thr = mib(bytes) / parity_ms.as_secs_f64();
                println!(
                    "| {} | {r} | {} | {} | {} | {} | {} | {} | {} | {thr:.0} | {:.2} | {:.2} | {:.2} | {:.1} % | {:.1} % |",
                    size_label(bytes),
                    l.shard_size / KIB,
                    l.groups,
                    l.data_shards,
                    l.parity_shards,
                    l.frame_count(),
                    ms(median(totals)),
                    ms(parity_ms),
                    mib(len),
                    mib(len.saturating_sub(baseline_len)),
                    mib(predicted),
                    100.0 * predicted as f64 / l.range_len as f64,
                    100.0 * tables as f64 / predicted as f64,
                );
                assert_eq!(
                    measured, predicted,
                    "the bytes between commit frame and footer are the layout's frames"
                );
            }
            // Keep r = 0 and r = 10 for the later tables; drop the rest.
            if r != 0 && r != 10 {
                std::fs::remove_file(&file).unwrap();
            }
        }
    }

    // ---- 2. verify ---------------------------------------------------------
    println!("\n## Verify (`stored_integrity`)\n");
    println!("| commit | recovery record | cache | verify ms | MiB/s | Integrity |");
    println!("|---|---|---|---:|---:|---|");
    for &bytes in sizes.iter().filter(|b| **b >= 4 * MIB) {
        for r in [0u16, 10] {
            let file = dir.path().join(format!("w-{bytes}-{r}.mochi"));
            let len = file_len(&file);
            let n = samples_for(bytes).max(3);
            for cache in ["cold", "warm"] {
                if cache == "cold" && !cold_ok {
                    println!(
                        "| {} | {} | cold | unavailable | | |",
                        size_label(bytes),
                        rr(r)
                    );
                    continue;
                }
                let mut t = Vec::new();
                let mut status = Status::Unknown;
                if cache == "warm" {
                    verify_stored(&file);
                }
                for _ in 0..n {
                    if cache == "cold" {
                        drop_caches();
                    }
                    let (d, s) = verify_stored(&file);
                    t.push(d);
                    status = s;
                }
                let d = median(t);
                println!(
                    "| {} | {} | {cache} | {} | {:.0} | {status:?} |",
                    size_label(bytes),
                    rr(r),
                    ms(d),
                    mib(len) / d.as_secs_f64()
                );
            }
        }
    }

    // ---- 3. rebuild --------------------------------------------------------
    println!("\n## Rebuild (`parity::scan`, m damaged data shards in every group)\n");
    println!("| commit | r % | s KiB | G | k | m | shards damaged | shards rebuilt | rebuilt MiB | cache | scan ms | rebuilt MiB/s | protected range MiB/s | identical to pristine |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---|");
    for &bytes in sizes.iter().filter(|b| **b >= 4 * MIB) {
        let rs: &[u16] = if bytes >= 512 * MIB {
            &[10]
        } else {
            &[1, 10, 50]
        };
        for &r in rs {
            let pristine = dir.path().join(format!("rb-{bytes}-{r}.mochi"));
            let _ = write_archive(&pristine, bytes, r);
            let (l, _) = layout_of(&pristine, r);
            let damaged_file = dir.path().join(format!("rb-{bytes}-{r}-damaged.mochi"));
            std::fs::copy(&pristine, &damaged_file).unwrap();
            // m real data shards in every group, one flipped byte each.
            let mut hit = Vec::new();
            {
                let mut f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&damaged_file)
                    .unwrap();
                for g in 0..l.groups {
                    let mut done = 0;
                    for j in 0..l.data_shards {
                        let i = l.shard_index(g, j);
                        let (off, n) = l.shard_extent(i);
                        if n == 0 || done == l.parity_shards {
                            continue;
                        }
                        let at = off + n / 2;
                        let mut b = [0u8; 1];
                        f.seek(SeekFrom::Start(at)).unwrap();
                        std::io::Read::read_exact(&mut f, &mut b).unwrap();
                        f.seek(SeekFrom::Start(at)).unwrap();
                        f.write_all(&[b[0] ^ FLIP]).unwrap();
                        hit.push((off, n));
                        done += 1;
                    }
                }
                f.sync_all().unwrap();
            }
            for cache in ["cold", "warm"] {
                if cache == "cold" && !cold_ok {
                    println!(
                        "| {} | {r} | | | | | | | | cold | unavailable | | | |",
                        size_label(bytes)
                    );
                    continue;
                }
                if cache == "cold" {
                    drop_caches();
                }
                let rd = OsReadStorage::open(&damaged_file).unwrap();
                let token = CancellationToken::new();
                let ctx = JobContext {
                    progress: &NullProgress,
                    cancel: &token,
                };
                let t = Instant::now();
                let (sc, view) = scan(&rd, &ReadOptions::default(), &ctx).unwrap();
                let d = t.elapsed();
                let c = &sc.commits[0];
                // Every damaged shard of the view equals the pristine file.
                let good = OsReadStorage::open(&pristine).unwrap();
                let mut same = true;
                for &(off, n) in &hit {
                    let (mut a, mut b) = (vec![0u8; n as usize], vec![0u8; n as usize]);
                    view.read_exact_at(off, &mut a).unwrap();
                    good.read_exact_at(off, &mut b).unwrap();
                    same &= a == b;
                }
                assert!(same, "a rebuilt shard differs from the pristine file");
                assert_eq!(c.shards_damaged, hit.len() as u64);
                let rebuilt = c.shards_rebuilt * l.shard_size;
                println!(
                    "| {} | {r} | {} | {} | {} | {} | {} | {} | {:.2} | {cache} | {} | {:.0} | {:.0} | {} |",
                    size_label(bytes),
                    l.shard_size / KIB,
                    l.groups,
                    l.data_shards,
                    l.parity_shards,
                    c.shards_damaged,
                    c.shards_rebuilt,
                    mib(rebuilt),
                    ms(d),
                    mib(rebuilt) / d.as_secs_f64(),
                    mib(l.range_len) / d.as_secs_f64(),
                    if same { "yes" } else { "NO" },
                );
            }
            std::fs::remove_file(&damaged_file).unwrap();
            std::fs::remove_file(&pristine).unwrap();
        }
    }
    println!();
    println!(
        "Medians of 7 (≤ 4 MiB), 5 (≤ 64 MiB), or 1 (512 MiB) runs; verify uses at least 3. \
         Rebuild rows are one run per cache state."
    );
}

fn rr(r: u16) -> String {
    if r == 0 {
        "none".to_owned()
    } else {
        format!("{r} %")
    }
}
