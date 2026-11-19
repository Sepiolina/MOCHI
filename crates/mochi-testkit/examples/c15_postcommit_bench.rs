//! Post-commit cost benchmark (desktop feedback: "verifying the committed
//! archive took very long"; spec §27 decomposition). Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example c15_postcommit_bench [dir] [small_files] [large_mib]
//! ```
//!
//! Dataset: `small_files` (default 10 000) files of 1-8 KiB pseudo-random
//! bytes in 100 folders, plus three incompressible files of `large_mib`
//! (default 64) MiB each, in one create commit. Then:
//!
//! 1. append one 4 KiB file the way the desktop does (open_append + commit +
//!    close), phases from the commit's progress events;
//! 2. what the desktop does next, `reopen`: commit history, `open_head`,
//!    `segment_state`, the snapshot index;
//! 3. the scoped post-commit check (`verify_commits_since`) of that new commit;
//! 4. a full `verify` at each level, for comparison.
//!
//! Cache: warm (the archive was just written; the OS page cache holds it).
//! Real `OsStorage` with real `fdatasync`; the directory's filesystem is
//! the one given (default: the system temp dir).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Mutex;
use std::time::Instant;

use mochi_core::browse::SnapshotIndex;
use mochi_core::job::{CancellationToken, JobContext, NullProgress, ProgressEvent, ProgressSink};
use mochi_core::publish::{
    commit_history, open_head, segment_state, ArchiveWriter, ReadOptions, TailPolicy, Transaction,
    WriterOptions,
};
use mochi_core::status::VerificationLevel;
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_core::storage::ReadStorage;
use mochi_core::verify::{verify, verify_commits_since, VerifiedHead, VerifyOptions};
use mochi_testkit::archive::path;
use mochi_testkit::{deterministic_bytes, SeqIds};

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
        record_time: true,
        ..WriterOptions::default()
    }
}

fn show_phases(p: &Phases, end: Instant) {
    let p = p.0.lock().unwrap().clone();
    let mut line = String::from("   phases (ms):");
    for pair in p.windows(2) {
        line += &format!(" {}={}", pair[0].0, ms(pair[1].1 - pair[0].1));
    }
    if let Some(last) = p.last() {
        line += &format!(" {}={}", last.0, ms(end - last.1));
    }
    println!("{line}");
}

fn ms(d: std::time::Duration) -> String {
    format!("{:.1}", d.as_secs_f64() * 1e3)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let small: u64 = args.next().map_or(10_000, |s| s.parse().unwrap());
    let large_mib: usize = args.next().map_or(64, |s| s.parse().unwrap());
    let dir = tempfile::tempdir_in(&base).unwrap();
    let file = dir.path().join("bench.mochi");
    let token = CancellationToken::new();
    let quiet = NullProgress;
    let ctx = JobContext {
        progress: &quiet,
        cancel: &token,
    };
    println!("# C15 post-commit benchmark");
    println!(
        "dir: {}  small files: {small}  large: 3 x {large_mib} MiB",
        dir.path().display()
    );

    // Create.
    let t = Instant::now();
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(&file).unwrap(),
        Box::new(SeqIds::new(1)),
        options(),
    )
    .unwrap();
    let mut tx = Transaction::new();
    for d in 0..100 {
        tx.put_dir(path(&format!("d{d:03}")), Default::default());
    }
    for i in 0..small {
        let len = 1024 + (i as usize * 997) % 7168;
        tx.put_file(
            path(&format!("d{:03}/f{i:06}", i % 100)),
            deterministic_bytes(i + 1, len),
            Default::default(),
        );
    }
    for k in 0..3u64 {
        tx.put_file(
            path(&format!("big{k}.bin")),
            deterministic_bytes(1_000_000 + k, large_mib << 20),
            Default::default(),
        );
    }
    let cp = Phases::default();
    w.commit(
        tx,
        &JobContext {
            progress: &cp,
            cancel: &token,
        },
    )
    .unwrap();
    show_phases(&cp, Instant::now());
    w.close().unwrap();
    let size = OsReadStorage::open(&file).unwrap().size().unwrap();
    println!(
        "create (1 commit): {} ms, archive {:.1} MiB",
        ms(t.elapsed()),
        size as f64 / (1 << 20) as f64
    );

    // Append one file, desktop-style, a few times.
    for round in 0..4 {
        let start = Instant::now();
        let (mut aw, _) = ArchiveWriter::open_append(
            OsStorage::open_existing(&file).unwrap(),
            Box::new(SeqIds::new(100 + round)),
            options(),
            TailPolicy::Refuse,
        )
        .unwrap();
        let opened = Instant::now();
        if round == 3 {
            aw.request_checkpoint();
        }
        let phases = Phases::default();
        let pctx = JobContext {
            progress: &phases,
            cancel: &token,
        };
        let mut tx = Transaction::new();
        tx.put_file(
            path(&format!("d000/new{round}")),
            deterministic_bytes(9_000_000 + round, 4096),
            Default::default(),
        );
        let out = aw.commit(tx, &pctx).unwrap();
        let committed = Instant::now();
        aw.close().unwrap();
        println!(
            "append {round}: open_append {} ms; commit {} ms (seq {}, checkpoint {}); close {} ms; total {} ms",
            ms(opened - start),
            ms(committed - opened),
            out.seq,
            out.checkpoint,
            ms(committed.elapsed()),
            ms(start.elapsed())
        );
        show_phases(&phases, committed);
    }

    // Reopen, as the desktop's `reopen` does.
    let opts = ReadOptions::default();
    let t = Instant::now();
    let src = OsReadStorage::open(&file).unwrap();
    let history = commit_history(&src, &opts).unwrap();
    let t_hist = t.elapsed();
    let t = Instant::now();
    let head = open_head(&src, &opts).unwrap();
    let t_head = t.elapsed();
    let t = Instant::now();
    let _ = segment_state(&src, &head, &opts).unwrap();
    let t_seg = t.elapsed();
    let t = Instant::now();
    let idx = SnapshotIndex::build(&head.catalog).unwrap();
    let t_idx = t.elapsed();
    drop(idx);
    println!(
        "reopen: history {} ms ({} commits), open_head {} ms, segment_state {} ms, index {} ms",
        ms(t_hist),
        history.len(),
        ms(t_head),
        ms(t_seg),
        ms(t_idx)
    );

    // Full verify, per level.
    for level in [
        VerificationLevel::Structural,
        VerificationLevel::Referential,
        VerificationLevel::StoredIntegrity,
        VerificationLevel::ContentIntegrity,
        VerificationLevel::Restoration,
    ] {
        let t = Instant::now();
        let v = verify(
            &src,
            &VerifyOptions {
                level,
                ..VerifyOptions::default()
            },
            &ctx,
        );
        println!(
            "full verify {level:?}: {} ms (overall {:?})",
            ms(t.elapsed()),
            v.report.overall_status
        );
    }

    // Scoped check of the newest commit alone, and of all four appends.
    for back in [1usize, 4] {
        let e = &history[history.len() - 1 - back];
        let since = VerifiedHead {
            archive_id: e.commit.archive_id,
            seq: e.commit.seq,
            commit_id: e.commit_id,
        };
        let t = Instant::now();
        let v = verify_commits_since(&src, &opts, &since, &ctx);
        println!(
            "scoped check since commit {}: {} ms ({} of {} new objects, {} bytes; scope {:?}, whole archive {:?})",
            since.seq,
            ms(t.elapsed()),
            v.objects_checked,
            v.objects_expected,
            v.bytes_checked,
            v.scope_status(),
            v.whole_archive
        );
    }
}
