//! C5 benchmark (plan C5 exit; spec §27): cost of one append versus the
//! number of prior commits, across three orders of magnitude, decomposed by
//! phase. Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example c5_append_bench [dir]
//! ```
//!
//! Dataset: commit 0 creates `/d`; every later commit adds one new 4 KiB
//! pseudo-random (incompressible) file under it. So both history length and
//! catalog size grow linearly with N, which is the realistic case and the one
//! that exposes the full-checkpoint-per-commit choice (plan O26).
//! Each measured append is a fresh writer session (open + commit + close),
//! as a CLI invocation would be. Cache: warm (the archive was just written).
//! Real `OsStorage` with real `fdatasync` on the directory given (default:
//! the system temp dir); the filesystem is printed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::publish::{
    locate_head, open_head, ArchiveWriter, ReadOptions, TailPolicy, Transaction, WriterOptions,
};
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_core::storage::ReadStorage;
use mochi_testkit::archive::path;
use mochi_testkit::{deterministic_bytes, SeqIds};

const SIZES: &[u64] = &[1, 10, 100, 1000];
const SAMPLES: usize = 7;
const FILE_BYTES: usize = 4096;

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

fn options() -> WriterOptions {
    WriterOptions {
        read: ReadOptions::default(),
        chunk_size: None,
        zstd_level: None,
        record_time: false,
    }
}

fn tx_for(i: u64) -> Transaction {
    let mut tx = Transaction::new();
    if i == 0 {
        tx.put_dir(path("d"), Default::default());
    } else {
        tx.put_file(
            path(&format!("d/f{i:06}")),
            deterministic_bytes(i, FILE_BYTES),
            Default::default(),
        );
    }
    tx
}

/// Per-sample timings, in the §27 categories that apply to an append.
#[derive(Default, Clone)]
struct Sample {
    footer_lookup: Duration,
    open_total: Duration,
    append_open: Duration,
    content: Duration,
    catalog_copy: Duration,
    catalog_update: Duration,
    manifest: Duration,
    checkpoint: Duration,
    commit_record: Duration,
    sync_objects: Duration,
    footer_and_sync: Duration,
    total: Duration,
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> String {
    format!("{:.2}", d.as_secs_f64() * 1e3)
}

fn main() {
    let base = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = tempfile::tempdir_in(&base).unwrap();
    let file = dir.path().join("bench.mochi");
    println!("# C5 append benchmark");
    println!("archive dir: {}", dir.path().display());

    let token = CancellationToken::new();
    let quiet = mochi_core::job::NullProgress;
    let mut ids = 0u64;
    let mut next_ids = || {
        ids += 1;
        Box::new(SeqIds::new(ids)) as Box<dyn mochi_core::object::IdSource>
    };

    let mut w = ArchiveWriter::create(OsStorage::create_new(&file).unwrap(), next_ids(), options())
        .unwrap();
    let mut committed = 0u64;
    println!();
    println!("| prior commits | archive MiB | catalog image KiB | snapshot manifest KiB | footer lookup | open (verify + catalog) | append open (+ snapshot read) | head catalog copy + replay | content | catalog update | delta manifest | checkpoint (snapshot + image) | commit record | sync objects | footer + sync | **append total** (ms) |");
    println!("|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for &n in SIZES {
        // Grow to n commits (unmeasured).
        while committed < n {
            let ctx = JobContext {
                progress: &quiet,
                cancel: &token,
            };
            w.commit(tx_for(committed), &ctx).unwrap();
            committed += 1;
        }
        w.close().unwrap();

        let mut samples = Vec::new();
        for _ in 0..SAMPLES {
            let mut s = Sample::default();
            let r = OsReadStorage::open(&file).unwrap();
            let t = Instant::now();
            locate_head(&r, &ReadOptions::default().limits).unwrap();
            s.footer_lookup = t.elapsed();
            let t = Instant::now();
            open_head(&r, &ReadOptions::default()).unwrap();
            s.open_total = t.elapsed();

            let start = Instant::now();
            let (mut aw, _) = ArchiveWriter::open_append(
                OsStorage::open_existing(&file).unwrap(),
                next_ids(),
                options(),
                TailPolicy::Refuse,
            )
            .unwrap();
            let phases = Phases::default();
            let ctx = JobContext {
                progress: &phases,
                cancel: &token,
            };
            let commit_start = Instant::now();
            s.append_open = commit_start - start;
            aw.commit(tx_for(committed), &ctx).unwrap();
            let end = Instant::now();
            aw.close().unwrap();
            committed += 1;
            s.total = start.elapsed();

            let p = phases.0.lock().unwrap().clone();
            let at = |name: &str| p.iter().find(|(n, _)| *n == name).map(|(_, t)| *t).unwrap();
            s.catalog_copy = at("content") - commit_start;
            s.content = at("catalog") - at("content");
            s.catalog_update = at("manifest") - at("catalog");
            s.manifest = at("checkpoint") - at("manifest");
            s.checkpoint = at("commit-record") - at("checkpoint");
            s.commit_record = at("sync-content") - at("commit-record");
            s.sync_objects = at("footer") - at("sync-content");
            s.footer_and_sync = end - at("footer");
            samples.push(s);
        }
        let (image_kib, snapshot_kib) = {
            let r = OsReadStorage::open(&file).unwrap();
            let h = open_head(&r, &ReadOptions::default()).unwrap();
            let mochi_core::commit::Metadata::Checkpoint { image, snapshot } = h.commit.metadata
            else {
                panic!("this writer emits checkpoints")
            };
            (
                image.stored_len as f64 / 1024.0,
                snapshot.stored_len as f64 / 1024.0,
            )
        };
        let size_mib =
            OsReadStorage::open(&file).unwrap().size().unwrap() as f64 / (1 << 20) as f64;
        let col = |f: fn(&Sample) -> Duration| ms(median(samples.iter().map(f).collect()));
        println!(
            "| {n} | {size_mib:.1} | {image_kib:.0} | {snapshot_kib:.0} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | **{}** |",
            col(|s| s.footer_lookup),
            col(|s| s.open_total),
            col(|s| s.append_open),
            col(|s| s.catalog_copy),
            col(|s| s.content),
            col(|s| s.catalog_update),
            col(|s| s.manifest),
            col(|s| s.checkpoint),
            col(|s| s.commit_record),
            col(|s| s.sync_objects),
            col(|s| s.footer_and_sync),
            col(|s| s.total),
        );
        w = ArchiveWriter::open_append(
            OsStorage::open_existing(&file).unwrap(),
            next_ids(),
            options(),
            TailPolicy::Refuse,
        )
        .unwrap()
        .0;
    }
    println!();
    println!(
        "Medians of {SAMPLES} appends per row. \"Prior commits\" is the count at the first sample."
    );
}
