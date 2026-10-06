//! T32 / gate G3 (spec Annex B.2.3, B.2.6; §27): checkpoint-trigger scaling.
//! Four workloads from 10 to 10,000 commits, plus a forced-checkpoint run,
//! under the production trigger (default α = 1, *F* = 1 MiB unless given).
//! Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example t32_scaling [max_n] [alpha_num/alpha_den] [floor]
//! ```
//!
//! Every figure is computed from the published archive (history entries
//! and object references), not from the writer's counters, as the T14
//! oracle does. Reported per run: commits and checkpoints, ΣΔ, the final
//! *B*, total metadata against the B.2.3 storage bound
//! (1 + 1/α + *k*)·ΣΔ + *B*₀ + Σ*B*_forced, *k* per interval (max and
//! median), the longest replay (deltas and bytes) against
//! α·max(*B*, *F*) plus one commit, and open time (head, and the commit
//! with the longest replay).
//!
//! Storage is `SimStorage` (in memory): no syncs, warm cache. Write time is
//! therefore CPU and SQLite cost only; the C5 benchmark covers syncs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use mochi_core::commit::Metadata;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, ArchiveWriter, CheckpointTrigger, HistoryEntry,
    ReadOptions, Transaction, WriterOptions,
};
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{path, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const SIZES: &[u64] = &[10, 100, 1000, 10_000];
/// Small files: data volume is not what is measured, metadata is.
const FILE_BYTES: usize = 256;
/// Files in the large import's commit 0.
const IMPORT_FILES: u64 = 10_000;
/// Files the repeated-updates workload rewrites in turn.
const UPDATE_SET: u64 = 100;
/// The forced run requests this many checkpoints per run, evenly spaced
/// (every N/10 commits). A fixed short interval would make Σ*B*_forced
/// quadratic in N by construction (1,000 full checkpoints of a growing
/// catalog at N = 10,000), which measures nothing the shorter runs do not.
const FORCED_PER_RUN: u64 = 10;
const OPEN_SAMPLES: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Workload {
    Growing,
    Updates,
    Tiny,
    Import,
    Forced,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Workload::Growing => "growing catalog (one new file per commit)",
            Workload::Updates => "repeated updates (100 files, one rewritten per commit)",
            Workload::Tiny => "tiny commits (empty transactions)",
            Workload::Import => "large import (10,000 files at commit 0, then one file per commit)",
            Workload::Forced => {
                "forced checkpoints (growing catalog, `checkpoint` every N/10 commits)"
            }
        }
    }

    fn tx(self, i: u64) -> Transaction {
        let mut tx = Transaction::new();
        let file = |tx: &mut Transaction, name: String, seed: u64| {
            tx.put_file(
                path(&name),
                deterministic_bytes(seed, FILE_BYTES),
                attrs(0o644, (seed % 1000) as i64),
            );
        };
        match self {
            Workload::Growing | Workload::Forced => file(&mut tx, format!("f{i:06}"), i),
            Workload::Updates if i == 0 => {
                for j in 0..UPDATE_SET {
                    file(&mut tx, format!("u{j:03}"), j);
                }
            }
            Workload::Updates => file(&mut tx, format!("u{:03}", i % UPDATE_SET), 1_000_000 + i),
            Workload::Tiny => {}
            Workload::Import if i == 0 => {
                // Directories are explicit (C3): create the parents first.
                tx.put_dir(path("import"), attrs(0o755, 0));
                tx.put_dir(path("new"), attrs(0o755, 0));
                for d in 0..IMPORT_FILES.div_ceil(100) {
                    tx.put_dir(path(&format!("import/{d:03}")), attrs(0o755, 0));
                }
                for j in 0..IMPORT_FILES {
                    file(&mut tx, format!("import/{:03}/{j:06}", j / 100), j);
                }
            }
            Workload::Import => file(&mut tx, format!("new/f{i:06}"), 2_000_000 + i),
        }
        tx
    }
}

fn delta_of(e: &HistoryEntry) -> u64 {
    e.commit.delta_manifest.stored_len + (e.footer_offset - e.commit_offset) + FOOTER_FRAME_LEN
}

fn base_of(e: &HistoryEntry) -> Option<u64> {
    match e.commit.metadata {
        Metadata::Checkpoint { image, snapshot } => Some(image.stored_len + snapshot.stored_len),
        Metadata::Delta { .. } => None,
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn time_open(f: impl Fn()) -> f64 {
    median(
        (0..OPEN_SAMPLES)
            .map(|_| {
                let t = Instant::now();
                f();
                ms(t.elapsed())
            })
            .collect(),
    )
}

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * 1024.0;

fn run(w: Workload, n: u64, t: CheckpointTrigger) -> String {
    let s = SimStorage::new();
    let opts = WriterOptions {
        chunk_size: None,
        zstd_level: None,
        checkpoint_trigger: Some(t),
        ..WriterOptions::default()
    };
    let job = Job::new();
    let started = Instant::now();
    let mut wr = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), opts).unwrap();
    let mut forced = Vec::new();
    for i in 0..n {
        let every = (n / FORCED_PER_RUN).max(1);
        if w == Workload::Forced && i > 0 && i % every == 0 {
            wr.request_checkpoint();
            forced.push(i);
        }
        wr.commit(w.tx(i), &job.ctx()).unwrap();
    }
    wr.close().unwrap();
    let write = started.elapsed();

    let ro = ReadOptions::default();
    let h = commit_history(&s, &ro).unwrap();
    assert_eq!(h.len() as u64, n);
    let total = s.contents().len() as u64;
    let (num, den) = t.alpha();
    let alpha = num as f64 / den as f64;

    // Walk the history as the trigger saw it.
    let mut metadata = h[0].commit.descriptor.stored_len;
    // ΣΔ as the trigger counts it (delta commits only), and the whole stream
    // of non-generated metadata after commit 0 (checkpoint commits' own
    // manifests, records, and footers too).
    let mut sum_delta = 0u64;
    let mut sum_stream = 0u64;
    // B₀ as B is defined (image + snapshot), and all of commit 0's metadata.
    let mut b0_full = h[0].commit.descriptor.stored_len;
    let mut delta = 0u64;
    let mut base = 0u64;
    let mut b0 = 0u64;
    let mut b_forced = 0u64;
    let mut checkpoints = 0u64;
    let mut ks = Vec::new();
    let mut seg_len = 0u64;
    let (mut max_seg, mut max_replay, mut max_replay_at, mut replay_bound) =
        (0u64, 0u64, 0usize, 0u64);
    for (i, e) in h.iter().enumerate() {
        let own = delta_of(e);
        metadata += own;
        if e.commit.seq == 0 {
            b0_full += own;
        } else {
            sum_stream += own;
        }
        match base_of(e) {
            Some(b) => {
                metadata += b;
                checkpoints += 1;
                let seq = e.commit.seq;
                if seq == 0 {
                    b0 = b;
                    b0_full += b;
                } else if forced.contains(&seq) {
                    b_forced += b;
                } else if delta > 0 {
                    ks.push((b as f64 - base as f64) / delta as f64);
                }
                base = b;
                delta = 0;
                seg_len = 0;
            }
            None => {
                delta += own;
                sum_delta += own;
                seg_len += 1;
                if delta > max_replay {
                    max_replay = delta;
                    max_replay_at = i;
                    replay_bound = (alpha * base.max(t.floor()) as f64) as u64 + own;
                }
                max_seg = max_seg.max(seg_len);
            }
        }
    }
    let k_max = ks.iter().cloned().fold(0.0f64, f64::max).max(0.0);
    let k_med = median(ks.clone());
    // B.2.3 as worded (B₀ = image + snapshot, ΣΔ as the trigger counts it),
    // and with B₀ = all of commit 0's metadata and ΣΔ = every later commit's
    // manifest, record, and footer (the reading the derivation needs).
    let bound_narrow = (1.0 + 1.0 / alpha + k_max) * sum_delta as f64 + b0 as f64 + b_forced as f64;
    let bound = (1.0 + 1.0 / alpha + k_max) * sum_stream as f64 + b0_full as f64 + b_forced as f64;
    let holds = |b: f64| if metadata as f64 <= b { "" } else { " **✗**" };

    let open_head_ms = time_open(|| {
        open_head(&s, &ro).unwrap();
    });
    let worst = h[max_replay_at].footer_offset;
    let open_worst_ms = time_open(|| {
        open_at_footer(&s, worst, &ro).unwrap();
    });

    format!(
        "| {n} | {checkpoints} | {:.1} | {:.1} | {:.3} | {:.3}{} | {:.3}{} | {:.2} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
        sum_delta as f64 / KIB,
        base as f64 / KIB,
        metadata as f64 / MIB,
        bound_narrow / MIB,
        holds(bound_narrow),
        bound / MIB,
        holds(bound),
        (total - metadata) as f64 / MIB,
        if ks.is_empty() { "–".to_string() } else { format!("{k_max:.2}") },
        if ks.is_empty() { "–".to_string() } else { format!("{k_med:.2}") },
        max_seg,
        max_replay as f64 / KIB,
        replay_bound as f64 / KIB,
        open_head_ms,
        open_worst_ms,
        write.as_secs_f64(),
    )
}

fn main() {
    // Optional: MOCHI_T32_ONLY=growing|updates|tiny|import|forced runs one.
    let only = std::env::var("MOCHI_T32_ONLY").ok();
    let args: Vec<String> = std::env::args().collect();
    let max_n: u64 = args.get(1).map_or(10_000, |a| a.parse().unwrap());
    let t = match args.get(2) {
        None => CheckpointTrigger::default(),
        Some(a) => {
            let (n, d) = a.split_once('/').unwrap();
            let floor = args
                .get(3)
                .map_or(CheckpointTrigger::default().floor(), |f| f.parse().unwrap());
            CheckpointTrigger::new(n.parse().unwrap(), d.parse().unwrap(), floor).unwrap()
        }
    };
    println!(
        "Trigger: α = {}/{}, F = {} bytes. Storage: in memory (SimStorage), warm, no syncs.",
        t.alpha().0,
        t.alpha().1,
        t.floor()
    );
    for w in [
        Workload::Growing,
        Workload::Updates,
        Workload::Tiny,
        Workload::Import,
        Workload::Forced,
    ] {
        if only
            .as_deref()
            .is_some_and(|o| !format!("{w:?}").eq_ignore_ascii_case(o))
        {
            continue;
        }
        println!();
        println!("### {}", w.name());
        println!();
        println!("| commits | checkpoints | ΣΔ KiB | final B KiB | metadata MiB | bound as worded MiB | bound, full B₀ MiB | data MiB | k max | k median | longest replay (deltas) | longest replay KiB | replay limit KiB | open head ms | open longest-replay ms | write s |");
        println!(
            "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
        );
        for &n in SIZES.iter().filter(|&&n| n <= max_n) {
            println!("{}", run(w, n, t));
        }
    }
}
