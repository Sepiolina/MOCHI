//! Plan C6 exit / spec §27: the read path, decomposed. Four measurements,
//! each reported separately: **footer lookup**, **catalog open**,
//! **metadata replay**, and **selected-file read**. Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example c6_read_bench [dir] [--cold]
//! ```
//!
//! The archive is written to a real file in `dir` (default: a temporary
//! directory) with the production writer defaults, then read through
//! `OsReadStorage`. With `--cold`, the guest page cache is dropped
//! (`sync`; `echo 3 > /proc/sys/vm/drop_caches`, root only) before every
//! sample; without it, every sample follows a discarded warm-up.
//!
//! What each measurement covers, all through the public API:
//! * **footer lookup**: `publish::locate_head`. Validates the footer at end
//!   of file (the archive has a clean tail, so no scan).
//! * **catalog open**: `publish::open_at_footer` at the segment's base
//!   checkpoint: commit record, descriptor, and delta manifest read and
//!   hash-verified; the catalog image read, hash-verified, envelope-checked,
//!   deserialized, and checked (SQLite integrity and MOCHI rules). No replay.
//!   Includes validating one footer at a known offset (no search).
//! * **metadata replay**: `open_head` (lookup + base open + replay of every
//!   delta in the segment) minus the two above, medians. Derived, not timed
//!   on its own: the public API has no replay-only entry point.
//! * **selected-file read**: `read::read_file` of one file into a sink after
//!   the head is open (not timed): extent lookup, every chunk read and
//!   checked as stored and decoded bytes, decompression, and the whole-file
//!   content hash. In `--cold` mode the cache is dropped after the open.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mochi_core::catalog::path::ArchivePath;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{
    locate_head, open_at_footer, open_head, ArchiveWriter, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::read::read_file;
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_testkit::archive::{path, Job};
use mochi_testkit::{deterministic_bytes, SeqIds};

/// Small files in commit 0.
const SMALL_FILES: u64 = 1_000;
const SMALL_BYTES: usize = 4 << 10;
/// One large, incompressible file in commit 0.
const LARGE_BYTES: usize = 64 << 20;
/// Delta commits after commit 0, one small file each. Under the default
/// trigger (α = 1, F = 1 MiB) they stay in commit 0's segment.
const DELTAS: u64 = 200;
const SAMPLES: usize = 7;

fn attrs() -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode: 0o644,
            uid: 1000,
            gid: 1000,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 1_700_000_000,
            nanos: 0,
        }),
    }
}

fn build(file: &Path) {
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(file).unwrap(),
        Box::new(SeqIds::new(1)),
        WriterOptions::default(),
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_dir(path("small"), attrs());
    for i in 0..SMALL_FILES {
        tx.put_file(
            path(&format!("small/{i:05}")),
            deterministic_bytes(i, SMALL_BYTES),
            attrs(),
        );
    }
    tx.put_file(
        path("large.bin"),
        deterministic_bytes(u64::MAX, LARGE_BYTES),
        attrs(),
    );
    w.commit(tx, &Job::new().ctx()).unwrap();
    for i in 0..DELTAS {
        let mut tx = Transaction::new();
        tx.put_file(
            path(&format!("small/d{i:05}")),
            deterministic_bytes(1_000_000 + i, SMALL_BYTES),
            attrs(),
        );
        w.commit(tx, &Job::new().ctx()).unwrap();
    }
    w.close().unwrap();
}

fn drop_caches() {
    let ok = std::process::Command::new("sync")
        .status()
        .unwrap()
        .success()
        && std::fs::write("/proc/sys/vm/drop_caches", b"3").is_ok();
    assert!(
        ok,
        "--cold needs root and a writable /proc/sys/vm/drop_caches"
    );
}

/// Median, minimum, maximum of `SAMPLES` runs of `f`, each on a fresh
/// read-only handle. `prepare` runs untimed before each sample.
fn sample<T>(
    file: &Path,
    cold: bool,
    mut prepare: impl FnMut(&OsReadStorage) -> T,
    mut f: impl FnMut(&OsReadStorage, &T),
) -> [Duration; 3] {
    if !cold {
        let r = OsReadStorage::open(file).unwrap();
        let t = prepare(&r);
        f(&r, &t); // warm-up, discarded
    }
    let mut v: Vec<Duration> = (0..SAMPLES)
        .map(|_| {
            if cold {
                drop_caches();
            }
            let r = OsReadStorage::open(file).unwrap();
            let t = prepare(&r);
            if cold {
                drop_caches();
            }
            let start = Instant::now();
            f(&r, &t);
            start.elapsed()
        })
        .collect();
    v.sort();
    [v[SAMPLES / 2], v[0], v[SAMPLES - 1]]
}

fn ms(d: Duration) -> String {
    format!("{:.3}", d.as_secs_f64() * 1e3)
}

fn row(name: &str, d: [Duration; 3], what: &str) {
    println!(
        "| {name} | {} | {} | {} | {what} |",
        ms(d[0]),
        ms(d[1]),
        ms(d[2])
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cold = args.iter().any(|a| a == "--cold");
    let tmp;
    let dir: PathBuf = match args.iter().find(|a| !a.starts_with("--")) {
        Some(d) => PathBuf::from(d),
        None => {
            tmp = tempfile::tempdir().unwrap();
            tmp.path().to_path_buf()
        }
    };
    let file = dir.join("c6-read-bench.mochi");
    let _ = std::fs::remove_file(&file);
    let t = Instant::now();
    build(&file);
    let built = t.elapsed();

    let opts = ReadOptions::default();
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &opts).unwrap();
    let base_footer = head.segment.base_footer_offset;
    assert_eq!(head.seq(), DELTAS, "head is the last delta");
    assert_eq!(
        head.segment.base_seq, 0,
        "one segment: commit 0 is the base"
    );
    let base = open_at_footer(&r, base_footer, &opts).unwrap();
    assert_eq!(base.seq(), 0);
    let image_len = match base.commit.metadata {
        mochi_core::commit::Metadata::Checkpoint { image, .. } => image.stored_len,
        mochi_core::commit::Metadata::Delta { .. } => unreachable!("commit 0 is a checkpoint"),
    };
    let archive_len = std::fs::metadata(&file).unwrap().len();
    drop((head, base, r));

    println!(
        "archive {} bytes; base image {} bytes; {} files at the head; {} deltas replayed; \
         built in {:.1} s; cache {}",
        archive_len,
        image_len,
        SMALL_FILES + 1 + DELTAS,
        DELTAS,
        built.as_secs_f64(),
        if cold {
            "cold (dropped before each sample)"
        } else {
            "warm"
        }
    );
    println!();
    println!("| measurement | median ms | min ms | max ms | covers |");
    println!("|---|---:|---:|---:|---|");

    let footer = sample(
        &file,
        cold,
        |_| (),
        |r, _| {
            locate_head(r, &opts.limits).unwrap();
        },
    );
    row(
        "footer lookup",
        footer,
        "`locate_head`: footer at EOF validated",
    );

    let catalog = sample(
        &file,
        cold,
        |_| (),
        |r, _| {
            open_at_footer(r, base_footer, &opts).unwrap();
        },
    );
    row(
        "catalog open",
        catalog,
        "`open_at_footer` at the base checkpoint, no replay",
    );

    let head_open = sample(
        &file,
        cold,
        |_| (),
        |r, _| {
            open_head(r, &opts).unwrap();
        },
    );
    row(
        "(open head)",
        head_open,
        "`open_head`: lookup + base open + replay",
    );
    let replay = head_open[0]
        .saturating_sub(catalog[0])
        .saturating_sub(footer[0]);
    println!(
        "| metadata replay | {} | – | – | derived: open head − catalog open − footer lookup (medians), {DELTAS} deltas |",
        ms(replay)
    );

    for (name, p, len) in [
        ("selected-file read, 4 KiB", "small/00500", SMALL_BYTES),
        ("selected-file read, 64 MiB", "large.bin", LARGE_BYTES),
    ] {
        let target: ArchivePath = path(p);
        let d = sample(
            &file,
            cold,
            |r| open_head(r, &opts).unwrap(),
            |r, head| {
                let mut sink = std::io::sink();
                let got = read_file(r, head, &target, &mut sink, &opts, &Job::new().ctx()).unwrap();
                assert_eq!(got.logical_len, len as u64);
            },
        );
        let mut what = "`read_file` to a sink, verified".to_string();
        if len >= 1 << 20 {
            let mib_s = len as f64 / (1 << 20) as f64 / d[0].as_secs_f64();
            what.push_str(&format!("; {mib_s:.0} MiB/s at the median"));
        }
        row(name, d, &what);
    }
}
