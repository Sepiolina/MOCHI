//! Release 0.9.11 benchmark: committing large content with the encode-ahead
//! threads (`WriterOptions::threads`) against one thread. Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example c14_threads_bench [dir] [MiB]
//! ```
//!
//! Dataset: one file of `MiB` (default 512) of log-like, compressible text,
//! streamed from disk (`put_file_from`), then the same size as 64 files of
//! `MiB / 64`. Product defaults otherwise (8 MiB chunks, default level).
//! Each case is a fresh archive on real `OsStorage` with `fdatasync`; cache
//! warm (the source was just written). The archives are compared byte for
//! byte across thread counts (IDs from `SeqIds`, no recorded time).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::manifest::Attributes;
use mochi_core::publish::{ArchiveWriter, Transaction, WriterOptions};
use mochi_core::storage::os::{OsFile, OsStorage};
use mochi_testkit::archive::path;
use mochi_testkit::SeqIds;

fn write_source(p: &Path, bytes: usize) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(p).unwrap());
    let mut n = 0usize;
    let mut i = 0u64;
    while n < bytes {
        let line = format!(
            "2026-10-26 12:{:02}:{:02}.{:03} unit=BC{:08} step={} value={:.5} PASS\n",
            (i / 60) % 60,
            i % 60,
            i % 1000,
            i / 7,
            i % 97,
            (i as f64).sqrt()
        );
        f.write_all(line.as_bytes()).unwrap();
        n += line.len();
        i += 1;
    }
}

fn commit(dir: &Path, files: &[PathBuf], threads: usize) -> (f64, Vec<u8>) {
    let archive = dir.join(format!("t{threads}.mochi"));
    let _ = std::fs::remove_file(&archive);
    let mut tx = Transaction::new();
    for (i, f) in files.iter().enumerate() {
        let len = std::fs::metadata(f).unwrap().len();
        tx.put_file_from(
            path(&format!("f{i}")),
            Arc::new(OsFile::new(f.clone(), len)),
            Attributes::default(),
        );
    }
    let opts = WriterOptions {
        threads: Some(threads),
        ..WriterOptions::default()
    };
    let (p, c) = (NullProgress, CancellationToken::new());
    let ctx = JobContext {
        progress: &p,
        cancel: &c,
    };
    let t = Instant::now();
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(&archive).unwrap(),
        Box::new(SeqIds::new(1)),
        opts,
    )
    .unwrap();
    w.commit(tx, &ctx).unwrap();
    w.close().unwrap();
    let secs = t.elapsed().as_secs_f64();
    let bytes = std::fs::read(&archive).unwrap();
    std::fs::remove_file(&archive).unwrap();
    (secs, bytes)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("c14_threads_bench"));
    let mib: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(512);
    std::fs::create_dir_all(&dir).unwrap();
    let auto = mochi_core::publish::default_writer_threads();
    println!("dir {}, {mib} MiB, auto threads = {auto}", dir.display());

    let one = dir.join("one.log");
    write_source(&one, mib << 20);
    let many: Vec<PathBuf> = (0..64)
        .map(|i| {
            let p = dir.join(format!("part{i}.log"));
            write_source(&p, (mib << 20) / 64);
            p
        })
        .collect();

    for (name, files) in [("1 file", vec![one.clone()]), ("64 files", many.clone())] {
        let (base, reference) = commit(&dir, &files, 1);
        println!(
            "{name:>9}: 1 thread {base:.2} s ({:.0} MiB/s)",
            mib as f64 / base
        );
        for threads in [2, auto] {
            let (secs, bytes) = commit(&dir, &files, threads);
            assert!(
                bytes == reference,
                "{threads} threads wrote different bytes"
            );
            println!(
                "{name:>9}: {threads} threads {secs:.2} s ({:.0} MiB/s, {:.1}x), same bytes",
                mib as f64 / secs,
                base / secs
            );
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
