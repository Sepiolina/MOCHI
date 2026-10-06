//! Gate G7 power-loss helper (spec Annex B.2.6 G7: "power-loss tests run
//! separately from process-crash tests (Linux, via device-mapper, where
//! runners allow)"). Driven by `ci/g7-powercut.sh`, which runs it as root on
//! an ext4 filesystem over a `dm-flakey` device.
//!
//! ```text
//! g7_powercut count <dir>        # create once; print the number of cut points
//! g7_powercut create <dir> <n>   # create, cutting power before mutation n
//! g7_powercut check <dir> <log>  # after remount: judge what survived
//! ```
//!
//! **Cut points.** Every mutating storage operation of D13 creation is
//! counted: each `create_exclusive`, `append`, `sync_data`,
//! `publish_no_replace`, `sync_directory`, `remove_if_unlocked`, `discard`,
//! and `truncate`, on the directory and on the file. `create … n` runs the
//! command in `POWERCUT_CMD` immediately before operation *n* (the script
//! switches the device to `drop_writes`, so nothing written after that point,
//! including the kernel's later writeback of unsynced pages, reaches the
//! disk), then aborts the process. With *n* equal to the count, the cut
//! comes after `create_in` has returned, and its acknowledgement is printed
//! first.
//!
//! **Verdict** (`check`), from D13 and §12.2: after the power cut, the
//! final name holds either nothing or the complete first commit, never
//! anything else; an acknowledged `Durable` creation is never lost; and any
//! leftover temporary file is removed by cleanup, because nobody holds its
//! lock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use mochi_core::publish::{open_head, ArchiveWriter, PublishDurability, ReadOptions};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsStorage};
use mochi_core::storage::{
    DirectoryDurability, ReadStorage, RemoveOutcome, Storage, StorageDir, StorageError,
};
use mochi_testkit::archive::{read_state, scripted_history, test_options, Job};
use mochi_testkit::SeqIds;

const NAME: &str = "a.mochi";
/// Printed (and flushed) before the post-completion cut, so the log outside
/// the cut device records what was acknowledged.
const ACK_DURABLE: &str = "ACK Durable";

static OPS: AtomicU64 = AtomicU64::new(0);
static CUT_AT: AtomicU64 = AtomicU64::new(u64::MAX);

/// Cut the power and stop, as a power loss does.
fn cut(at: u64) -> ! {
    eprintln!("g7_powercut: cutting power before operation {at}");
    if let Ok(cmd) = std::env::var("POWERCUT_CMD") {
        let status = Command::new("sh").arg("-c").arg(&cmd).status().unwrap();
        if !status.success() {
            eprintln!("g7_powercut: POWERCUT_CMD failed: {status}");
            std::process::exit(2);
        }
    }
    std::process::abort();
}

/// Count one mutation; cut before it if it is the chosen one.
fn gate() {
    let n = OPS.fetch_add(1, Ordering::SeqCst);
    if n == CUT_AT.load(Ordering::SeqCst) {
        cut(n);
    }
}

struct CutFile(OsStorage);

impl ReadStorage for CutFile {
    fn size(&self) -> Result<u64, StorageError> {
        self.0.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        self.0.read_at(offset, buf)
    }
}

impl Storage for CutFile {
    fn append(&mut self, data: &[u8]) -> Result<u64, StorageError> {
        gate();
        self.0.append(data)
    }
    fn sync_data(&mut self) -> Result<(), StorageError> {
        gate();
        self.0.sync_data()
    }
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        gate();
        self.0.sync_directory()
    }
    fn try_lock_exclusive(&mut self) -> Result<(), StorageError> {
        self.0.try_lock_exclusive()
    }
    fn unlock(&mut self) -> Result<(), StorageError> {
        self.0.unlock()
    }
    fn truncate(&mut self, new_len: u64) -> Result<(), StorageError> {
        gate();
        self.0.truncate(new_len)
    }
}

struct CutDir(OsDir);

impl StorageDir for CutDir {
    type File = CutFile;
    fn open(&mut self, name: &str) -> Result<CutFile, StorageError> {
        self.0.open(name).map(CutFile)
    }
    fn create_exclusive(&mut self, name: &str) -> Result<CutFile, StorageError> {
        gate();
        self.0.create_exclusive(name).map(CutFile)
    }
    fn publish_no_replace(&mut self, from: &str, to: &str) -> Result<(), StorageError> {
        gate();
        self.0.publish_no_replace(from, to)
    }
    fn discard(&mut self, file: CutFile, name: &str) -> Result<(), StorageError> {
        gate();
        self.0.discard(file.0, name)
    }
    fn remove_if_unlocked(&mut self, name: &str) -> Result<RemoveOutcome, StorageError> {
        gate();
        self.0.remove_if_unlocked(name)
    }
    fn sync_directory(&mut self) -> Result<DirectoryDurability, StorageError> {
        gate();
        self.0.sync_directory()
    }
}

/// Create `NAME` in `dir` from the first scripted commit.
fn create(dir: &Path) {
    let mut d = CutDir(OsDir::open(dir).unwrap());
    let (w, outcome) = ArchiveWriter::create_in(
        &mut d,
        NAME,
        Box::new(SeqIds::new(7)),
        test_options(),
        scripted_history()[0].tx.clone(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(outcome.seq, 0);
    let mut out = std::io::stdout().lock();
    match outcome.durability {
        PublishDurability::Durable => writeln!(out, "{ACK_DURABLE}").unwrap(),
        PublishDurability::DirectoryUnconfirmed(why) => {
            writeln!(out, "ACK DirectoryUnconfirmed ({why})").unwrap()
        }
    }
    out.flush().unwrap();
    drop(out);
    if CUT_AT.load(Ordering::SeqCst) != u64::MAX {
        // Cut after the acknowledgement, with the writer still open.
        let n = OPS.load(Ordering::SeqCst);
        let _keep = w;
        cut(n);
    }
    drop(w);
}

fn is_temp(name: &str) -> bool {
    name.starts_with(&format!(".{NAME}.")) && name.ends_with(".mochi-tmp")
}

/// Judge the directory after the power cut; panics on a violation.
fn check(dir: &Path, log: &Path) {
    let acked = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .any(|l| l == ACK_DURABLE);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != "lost+found")
        .collect();
    names.sort();
    for n in &names {
        assert!(
            n == NAME || is_temp(n),
            "unexpected entry {n:?} in {names:?}"
        );
    }

    let at_name = names.iter().any(|n| n == NAME);
    let verdict = if at_name {
        // Never anything but the complete first commit.
        let file = OsReadStorage::open(dir.join(NAME)).unwrap();
        let head = open_head(&file, &ReadOptions::default())
            .unwrap_or_else(|e| panic!("{NAME} exists but does not open: {e}"));
        assert_eq!(head.seq(), 0);
        assert_eq!(
            read_state(&file, &head).unwrap(),
            scripted_history()[0].after,
            "{NAME} opens to the wrong state"
        );
        "complete"
    } else {
        assert!(!acked, "{ACK_DURABLE} was printed, but {NAME} is gone");
        "absent"
    };

    // Nobody holds a leftover temporary file's lock: cleanup removes it.
    let mut d = OsDir::open(dir).unwrap();
    let temps: Vec<&String> = names.iter().filter(|n| is_temp(n)).collect();
    for t in &temps {
        assert_eq!(
            d.remove_if_unlocked(t).unwrap(),
            RemoveOutcome::Removed,
            "{t}"
        );
    }
    println!(
        "ok: {NAME} {verdict}; acknowledged durable: {acked}; temporary files removed: {}",
        temps.len()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: g7_powercut count <dir> | create <dir> <n> | check <dir> <log>";
    let dir = PathBuf::from(args.get(2).expect(usage));
    match args.get(1).map(String::as_str) {
        Some("count") => {
            create(&dir);
            println!("ops {}", OPS.load(Ordering::SeqCst));
        }
        Some("create") => {
            let n: u64 = args.get(3).expect(usage).parse().expect(usage);
            CUT_AT.store(n, Ordering::SeqCst);
            create(&dir);
            panic!("cut point {n} was never reached");
        }
        Some("check") => check(&dir, Path::new(args.get(3).expect(usage))),
        _ => panic!("{usage}"),
    }
}
