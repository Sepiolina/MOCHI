//! Plan D1: the archives the desktop E2E test browses, written into `dir`.
//!
//! ```text
//! cargo run --release -p mochi-testkit --example d1_browse_fixtures -- DIR
//! ```
//!
//! * `browse.mochi`: the scripted three-commit history (a directory, files
//!   over several chunks, replacement, rename, deletion, and the hostile name
//!   `<img src=x onerror=alert(1)>`), then a fourth commit of names that are
//!   markup, script, URL, control, and bidi text (spec §23.3 #9).
//! * `large.mochi`: 100,000 empty files in `many/` plus `readme.txt` (plan
//!   D1 exit: browsing without loading the whole namespace into the webview).
//! * `damaged.mochi` (plan D4): the scripted three-commit history with one
//!   byte flipped in the first chunk of `big` (in commit 2 only). Its head
//!   opens; `verify` reports integrity `FAIL`; `repair plan` is partial and
//!   leaves out `big` from snapshot 2 alone.
//!
//! Existing files are not replaced. After writing, it opens each archive the
//! way the desktop app does (head, history, segment state, index) and prints
//! how long that took: one sample, warm cache, this machine; a smoke figure,
//! not a §27 benchmark.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Instant;

use mochi_core::browse::SnapshotIndex;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::publish::{
    commit_history, open_head, segment_state, ArchiveWriter, CheckpointPolicy, ReadOptions,
    Transaction,
};
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_testkit::archive::{build, first_chunk_middle, scripted_history, test_options, Job};
use mochi_testkit::SeqIds;

/// Names whose text is markup, script, or a URL, plus controls and bidi.
pub const HOSTILE: &[&str] = &[
    "<script>alert(1)",
    "\"><svg onload=alert(1)>",
    "javascript:alert(1)",
    "&lt;b&gt;entity",
    "rtl\u{202E}gpj.exe",
    "esc\u{1b}[31mred",
];

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

fn p(s: &str) -> ArchivePath {
    ArchivePath::from_stored(s.as_bytes()).unwrap()
}

fn browse(path: &Path) {
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(path).unwrap(),
        Box::new(SeqIds::new(41)),
        test_options(),
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::EveryCommit)
        .unwrap();
    let job = Job::new();
    for step in scripted_history() {
        w.commit(step.tx, &job.ctx()).unwrap();
    }
    let mut tx = Transaction::new();
    tx.put_dir(p("hostile"), attrs());
    for name in HOSTILE {
        tx.put_file(
            p(&format!("hostile/{name}")),
            name.as_bytes().to_vec(),
            attrs(),
        );
    }
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();
}

fn large(path: &Path) {
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(path).unwrap(),
        Box::new(SeqIds::new(43)),
        test_options(),
    )
    .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(
        p("readme.txt"),
        b"100,000 empty files in many/".to_vec(),
        attrs(),
    );
    tx.put_dir(p("many"), attrs());
    for i in 0..100_000u32 {
        tx.put_file(p(&format!("many/f{i:06}")), Vec::new(), attrs());
    }
    let job = Job::new();
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();
}

fn damaged(path: &Path) {
    let pristine = path.with_extension("pristine.tmp");
    let _ = std::fs::remove_file(&pristine);
    build(
        OsStorage::create_new(&pristine).unwrap(),
        7,
        &scripted_history(),
    )
    .unwrap();
    let src = OsReadStorage::open(&pristine).unwrap();
    let at = first_chunk_middle(&open_head(&src, &ReadOptions::default()).unwrap(), "big").unwrap();
    drop(src);
    let mut bytes = std::fs::read(&pristine).unwrap();
    bytes[usize::try_from(at).unwrap()] ^= 0x01;
    std::fs::remove_file(&pristine).unwrap();
    // A new file, never replacing one.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &bytes))
        .unwrap();
}

fn open_like_the_app(path: &Path) {
    let t = Instant::now();
    let src = OsReadStorage::open(path).unwrap();
    let opts = ReadOptions::default();
    let history = commit_history(&src, &opts).unwrap();
    let head = open_head(&src, &opts).unwrap();
    segment_state(&src, &head, &opts).unwrap();
    let index = SnapshotIndex::build(&head.catalog).unwrap();
    println!(
        "{}: {} commits, {} entries at the head, opened and indexed in {:?}",
        path.display(),
        history.len(),
        index.len(),
        t.elapsed()
    );
}

fn main() {
    let dir = std::env::args_os()
        .nth(1)
        .expect("usage: d1_browse_fixtures DIR");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    for (name, write) in [
        ("browse.mochi", browse as fn(&Path)),
        ("large.mochi", large),
        ("damaged.mochi", damaged),
    ] {
        let path = dir.join(name);
        if !path.exists() {
            write(&path);
        }
        open_like_the_app(&path);
    }
}
