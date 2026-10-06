//! T32 / gates G4 and G5 (spec Annex B.2.3, B.2.4): capacity and memory,
//! measured from the MOCHI writer and codec. Run with:
//!
//! ```text
//! cargo run --release -p mochi-testkit --example t32_capacity
//! ```
//!
//! * **Capacity (G5).** Archives whose commit 0 imports N single-chunk files
//!   with POSIX attributes and 22-byte paths (the B.2.4 estimate's shape),
//!   written by the real writer. Per-file snapshot bytes, snapshot CBOR items
//!   (every data item, map keys included, as `CborLimits::max_items`
//!   counts them; T4), and image bytes are the slopes between sizes; the
//!   binding limit follows from the reader defaults. Multi-chunk cost is the
//!   slope in chunks per file at fixed N.
//! * **Memory (G4).** A counting global allocator records the peak heap
//!   growth while decoding a snapshot manifest (load + CBOR decode + manifest
//!   build: `read_snapshot`) and while opening the head (which loads the
//!   image into in-memory SQLite: `open_head`). Reported per decoded item and
//!   per stored byte.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use mochi_core::commit::Metadata;
use mochi_core::publish::{
    commit_history, open_head, read_snapshot, ArchiveWriter, ReadOptions, Transaction,
    WriterOptions,
};
use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::limits::{DEFAULT_IMAGE_PAYLOAD, DEFAULT_SKIPPABLE_PAYLOAD};
use mochi_testkit::archive::{path, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

/// Heap accounting for this benchmark process only.
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to `System` with the caller's layout and
// pointer unchanged, so `System`'s guarantees carry over; the only addition
// is atomic bookkeeping, which neither allocates nor touches the memory.
// Needed because heap peak cannot be observed without a global allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = CURRENT.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        CURRENT.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new >= l.size() {
                let now = CURRENT.fetch_add(new - l.size(), Ordering::Relaxed) + new - l.size();
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Peak heap growth above the level at the call, while `f` runs.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    (out, PEAK.load(Ordering::Relaxed) - base)
}

/// Data items as `CborLimits::max_items` counts them: every item, map keys
/// included (T4).
fn items(v: &Value) -> u64 {
    match v {
        Value::Array(a) => 1 + a.iter().map(items).sum::<u64>(),
        Value::Map(m) => 1 + m.iter().map(|(_, v)| 1 + items(v)).sum::<u64>(),
        _ => 1,
    }
}

struct Measure {
    snapshot_bytes: u64,
    snapshot_items: u64,
    image_bytes: u64,
    decode_peak: usize,
    open_peak: usize,
}

/// One archive: commit 0 imports `n` files of `chunks` chunks each.
fn measure(n: u64, chunks: u64) -> Measure {
    let chunk = 64u64;
    let s = SimStorage::new();
    let opts = WriterOptions {
        chunk_size: Some(chunk),
        ..WriterOptions::default()
    };
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(3)), opts).unwrap();
    let mut tx = Transaction::new();
    tx.put_dir(path("dir00"), attrs(0o755, 0));
    for i in 0..n {
        // 22 bytes: "dir00/" + "file-" + 11 digits.
        let p = format!("dir00/file-{i:011}");
        debug_assert_eq!(p.len(), 22);
        tx.put_file(
            path(&p),
            deterministic_bytes(i, (chunk * chunks) as usize),
            attrs(0o644, (i % 1000) as i64),
        );
    }
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();

    let ro = ReadOptions::default();
    let h = commit_history(&s, &ro).unwrap();
    let Metadata::Checkpoint { image, snapshot } = h[0].commit.metadata else {
        panic!("commit 0 is a checkpoint")
    };
    let bytes = s.contents();
    let payload =
        &bytes[(snapshot.offset + 8) as usize..(snapshot.offset + snapshot.stored_len) as usize];
    let v = cbor::decode(
        payload,
        &CborLimits {
            max_depth: 64,
            max_items: usize::MAX,
        },
    )
    .unwrap();
    let snapshot_items = items(&v);
    drop(v);
    drop(bytes);

    let (head, open_peak) = peak_during(|| open_head(&s, &ro).unwrap());
    let (snap, decode_peak) = peak_during(|| read_snapshot(&s, &head, &ro).unwrap());
    drop(snap);
    drop(head);
    Measure {
        snapshot_bytes: snapshot.stored_len,
        snapshot_items,
        image_bytes: image.stored_len,
        decode_peak,
        open_peak,
    }
}

fn main() {
    let sizes = [1_000u64, 4_000, 16_000];
    println!("## Capacity (G5): single-chunk files, POSIX attributes, 22-byte paths");
    println!();
    println!("| files | snapshot bytes | snapshot items | image bytes | decode peak bytes | open peak bytes |");
    println!("|---:|---:|---:|---:|---:|---:|");
    let ms: Vec<Measure> = sizes.iter().map(|&n| measure(n, 1)).collect();
    for (n, m) in sizes.iter().zip(&ms) {
        println!(
            "| {n} | {} | {} | {} | {} | {} |",
            m.snapshot_bytes, m.snapshot_items, m.image_bytes, m.decode_peak, m.open_peak
        );
    }
    let (a, b) = (&ms[0], &ms[2]);
    let dn = (sizes[2] - sizes[0]) as f64;
    let per_bytes = (b.snapshot_bytes - a.snapshot_bytes) as f64 / dn;
    let per_items = (b.snapshot_items - a.snapshot_items) as f64 / dn;
    let per_image = (b.image_bytes - a.image_bytes) as f64 / dn;
    let fixed_items = a.snapshot_items as f64 - per_items * sizes[0] as f64;
    let fixed_bytes = a.snapshot_bytes as f64 - per_bytes * sizes[0] as f64;
    let fixed_image = a.image_bytes as f64 - per_image * sizes[0] as f64;
    let item_limit = CborLimits::default().max_items as f64;
    let at_items = (item_limit - fixed_items) / per_items;
    let at_bytes = (DEFAULT_SKIPPABLE_PAYLOAD as f64 - fixed_bytes) / per_bytes;
    let at_image = (DEFAULT_IMAGE_PAYLOAD as f64 - fixed_image) / per_image;
    println!();
    println!("Per file (slope {}–{} files): **{per_bytes:.1} snapshot bytes, {per_items:.2} items, {per_image:.1} image bytes**.", sizes[0], sizes[2]);
    println!("Files at each reader-default limit: items ({} Mi) **{at_items:.0}**; snapshot payload ({} bytes) **{at_bytes:.0}**; image budget ({} bytes) **{at_image:.0}**.", (item_limit as u64) >> 20, DEFAULT_SKIPPABLE_PAYLOAD, DEFAULT_IMAGE_PAYLOAD);

    println!();
    println!("## Multi-chunk files (1,000 files)");
    println!();
    println!("| chunks per file | snapshot bytes | snapshot items | image bytes |");
    println!("|---:|---:|---:|---:|");
    let cs = [1u64, 2, 5];
    let mc: Vec<Measure> = cs.iter().map(|&c| measure(1_000, c)).collect();
    for (c, m) in cs.iter().zip(&mc) {
        println!(
            "| {c} | {} | {} | {} |",
            m.snapshot_bytes, m.snapshot_items, m.image_bytes
        );
    }
    let dc = 1_000.0 * (cs[2] - cs[0]) as f64;
    println!();
    println!(
        "Per extra chunk: **{:.2} items, {:.1} snapshot bytes, {:.1} image bytes**.",
        (mc[2].snapshot_items - mc[0].snapshot_items) as f64 / dc,
        (mc[2].snapshot_bytes - mc[0].snapshot_bytes) as f64 / dc,
        (mc[2].image_bytes - mc[0].image_bytes) as f64 / dc,
    );

    println!();
    println!("## Memory (G4)");
    println!();
    println!(
        "| files | snapshot decode peak / item | / stored byte | head open peak / image byte |"
    );
    println!("|---:|---:|---:|---:|");
    for (n, m) in sizes.iter().zip(&ms) {
        println!(
            "| {n} | {:.1} B | {:.2} | {:.2} |",
            m.decode_peak as f64 / m.snapshot_items as f64,
            m.decode_peak as f64 / m.snapshot_bytes as f64,
            m.open_peak as f64 / m.image_bytes as f64,
        );
    }
}
