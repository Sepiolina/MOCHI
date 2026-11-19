//! Release 0.9.11: file content read, hashed and compressed ahead of the
//! writer on worker threads (`WriterOptions::threads`).
//!
//! The thread count is writer policy: the archive must be byte for byte the
//! same for every value (object IDs drawn in the same order, the same frames,
//! the same dedup decisions), a failing source must still publish nothing,
//! and the Encrypted profile (sealed by the writer itself) must round-trip.

use std::sync::Arc;

use mochi_core::descriptor::Profile;
use mochi_core::manifest::Attributes;
use mochi_core::publish::{
    open_head, ArchiveWriter, FileReader, FileSource, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::read::read_file;
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_testkit::archive::{encrypted_options, keyed_read, path, test_options, Job};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
    redundancy: None,
};

/// Bytes in memory, read back in short pieces; optionally failing.
struct Src {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
}

struct SrcReader<'a> {
    src: &'a Src,
    at: usize,
}

impl FileReader for SrcReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.src.fail_after.is_some_and(|f| self.at >= f) {
            return Err(MochiError::new(ErrorCode::IoError, "the disk went away"));
        }
        let n = buf.len().min(13).min(self.src.bytes.len() - self.at);
        buf[..n].copy_from_slice(&self.src.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl FileSource for Src {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn open(&self) -> Result<Box<dyn FileReader + '_>> {
        Ok(Box::new(SrcReader { src: self, at: 0 }))
    }
    fn describe(&self) -> String {
        "src".into()
    }
}

fn src(bytes: Vec<u8>) -> Arc<dyn FileSource> {
    Arc::new(Src {
        bytes,
        fail_after: None,
    })
}

/// Two commits: mixed in-memory and streamed files of many sizes (64-byte
/// chunks), then an append that repeats earlier content (dedup) and adds
/// new content.
fn write(profile: Option<Profile>, threads: usize) -> Vec<u8> {
    let s = SimStorage::new();
    let opts = WriterOptions {
        profile,
        threads: Some(threads),
        ..test_options()
    };
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(9)), opts).unwrap();
    let job = Job::new();

    let mut tx = Transaction::new();
    tx.put_dir(path("d"), Attributes::default());
    for (i, len) in [0usize, 1, 63, 64, 65, 1000, 5000, 129]
        .into_iter()
        .enumerate()
    {
        let bytes = deterministic_bytes(i as u64, len);
        if i % 2 == 0 {
            tx.put_file(path(&format!("d/m{i}")), bytes, Attributes::default());
        } else {
            tx.put_file_from(path(&format!("d/s{i}")), src(bytes), Attributes::default());
        }
    }
    w.commit(tx, &job.ctx()).unwrap();

    let mut tx = Transaction::new();
    tx.put_file_from(
        path("again"),
        src(deterministic_bytes(5, 1000)),
        Attributes::default(),
    );
    tx.put_file(
        path("new"),
        deterministic_bytes(77, 3000),
        Attributes::default(),
    );
    tx.delete(path("d/m0"));
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();
    s.contents()
}

#[test]
fn the_archive_is_the_same_bytes_for_every_thread_count() {
    for profile in [None, Some(TAR)] {
        let one = write(profile, 1);
        for threads in [2, 4, 7] {
            assert!(
                write(profile, threads) == one,
                "{threads} threads wrote different bytes (profile {profile:?})"
            );
        }
    }
}

#[test]
fn a_failing_source_publishes_nothing_with_threads() {
    let s = SimStorage::new();
    let opts = WriterOptions {
        threads: Some(4),
        ..test_options()
    };
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), opts).unwrap();
    let job = Job::new();
    let mut tx = Transaction::new();
    tx.put_file(
        path("first"),
        deterministic_bytes(1, 500),
        Attributes::default(),
    );
    w.commit(tx, &job.ctx()).unwrap();

    let mut tx = Transaction::new();
    tx.put_file(
        path("a"),
        deterministic_bytes(2, 4000),
        Attributes::default(),
    );
    tx.put_file_from(
        path("b"),
        Arc::new(Src {
            bytes: deterministic_bytes(3, 4000),
            fail_after: Some(1500),
        }),
        Attributes::default(),
    );
    tx.put_file(
        path("c"),
        deterministic_bytes(4, 4000),
        Attributes::default(),
    );
    let err = w.commit(tx, &job.ctx()).unwrap_err();
    assert_eq!(err.code, ErrorCode::IoError, "{err:?}");
    drop(w);

    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert_eq!(head.seq(), 0, "the failed commit must not be published");
}

#[test]
fn an_encrypted_archive_round_trips_with_threads() {
    let s = SimStorage::new();
    let opts = WriterOptions {
        threads: Some(4),
        chunk_size: Some(64),
        ..encrypted_options(&["pw"])
    };
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(3)), opts).unwrap();
    let job = Job::new();
    let big = deterministic_bytes(8, 7000);
    let mut tx = Transaction::new();
    tx.put_file_from(path("big"), src(big.clone()), Attributes::default());
    tx.put_file(path("small"), b"hi".to_vec(), Attributes::default());
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();

    let ro = keyed_read(&["pw"]);
    let head = open_head(&s, &ro).unwrap();
    let mut out = Vec::new();
    read_file(&s, &head, &path("big"), &mut out, &ro, &job.ctx()).unwrap();
    assert_eq!(out, big);
}
