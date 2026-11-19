//! C14 K1: file content streamed into the writer (`Transaction::put_file_from`).
//!
//! A streamed file must give the archive exactly what the in-memory
//! `put_file` gives (same extents, same content hash, same bytes read back);
//! the writer must never ask a source for more than one chunk at a time; a
//! failing source, or a changed length in the TAR-compatible profile, must
//! publish nothing; and the importer must no longer refuse inputs by size.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use mochi_core::catalog::Catalog;
use mochi_core::descriptor::Profile;
use mochi_core::import::{import, ImportOptions, SourceKind, SourceMeta, SourceTree};
use mochi_core::manifest::Attributes;
use mochi_core::publish::{
    open_head, ArchiveWriter, FileReader, FileSource, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::read::read_file_in;
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
    redundancy: None,
};

/// A source over bytes in memory that records the largest read asked of it,
/// and can fail after a number of bytes or claim another length.
struct Probe {
    bytes: Vec<u8>,
    declared: u64,
    fail_after: Option<usize>,
    largest_read: Arc<AtomicUsize>,
    opened: Arc<AtomicUsize>,
}

impl Probe {
    fn new(bytes: Vec<u8>) -> Self {
        Probe {
            declared: bytes.len() as u64,
            bytes,
            fail_after: None,
            largest_read: Arc::default(),
            opened: Arc::default(),
        }
    }
}

struct ProbeReader<'a> {
    probe: &'a Probe,
    at: usize,
}

impl FileReader for ProbeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.probe
            .largest_read
            .fetch_max(buf.len(), Ordering::SeqCst);
        if self.probe.fail_after.is_some_and(|f| self.at >= f) {
            return Err(MochiError::new(ErrorCode::IoError, "the disk went away"));
        }
        // Short reads on purpose: the writer must fill each chunk itself.
        let n = buf.len().min(7).min(self.probe.bytes.len() - self.at);
        buf[..n].copy_from_slice(&self.probe.bytes[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl FileSource for Probe {
    fn len(&self) -> u64 {
        self.declared
    }
    fn open(&self) -> Result<Box<dyn FileReader + '_>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ProbeReader { probe: self, at: 0 }))
    }
    fn describe(&self) -> String {
        "probe".into()
    }
}

fn read_back(s: &SimStorage, cat: &Catalog, p: &str) -> Vec<u8> {
    let mut out = Vec::new();
    read_file_in(
        s,
        cat,
        &path(p),
        &mut out,
        &ReadOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    out
}

fn version_of(cat: &Catalog, p: &str) -> (u64, String, usize) {
    let snap = cat.replay(None).unwrap();
    let entry = snap.get(&path(p)).unwrap();
    let (v, extents) = cat.file_version(&entry.version).unwrap().unwrap();
    (
        v.logical_len,
        format!("{:?}", v.content_hash),
        extents.len(),
    )
}

fn new_writer(s: &SimStorage, opts: WriterOptions) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), opts).unwrap()
}

#[test]
fn a_streamed_file_is_stored_as_the_same_bytes_as_one_in_memory() {
    // 64-byte chunks: 1000 bytes is 16 chunks, the last one short; plus an
    // empty file and one exactly a chunk long.
    for profile in [None, Some(TAR)] {
        let s = SimStorage::new();
        let mut w = new_writer(
            &s,
            WriterOptions {
                profile,
                ..test_options()
            },
        );
        let big = deterministic_bytes(7, 1000);
        let probe = Probe::new(big.clone());
        let largest = probe.largest_read.clone();
        let opened = probe.opened.clone();
        let mut tx = Transaction::new();
        tx.put_file_from(path("streamed"), Arc::new(probe), Attributes::default());
        tx.put_file(path("memory"), big.clone(), Attributes::default());
        tx.put_file_from(
            path("empty"),
            Arc::new(Probe::new(Vec::new())),
            Attributes::default(),
        );
        tx.put_file_from(
            path("one-chunk"),
            Arc::new(Probe::new(deterministic_bytes(8, 64))),
            Attributes::default(),
        );
        w.commit(tx, &Job::new().ctx()).unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert!(
            largest.load(Ordering::SeqCst) <= 64,
            "read more than a chunk"
        );

        let head = open_head(&s, &ReadOptions::default()).unwrap();
        assert_eq!(read_back(&s, &head.catalog, "streamed"), big);
        assert_eq!(
            version_of(&head.catalog, "streamed"),
            version_of(&head.catalog, "memory")
        );
        assert_eq!(version_of(&head.catalog, "streamed").2, 16);
        assert_eq!(read_back(&s, &head.catalog, "empty"), Vec::<u8>::new());
        assert_eq!(version_of(&head.catalog, "one-chunk").2, 1);
    }
}

#[test]
fn a_source_that_fails_midway_publishes_nothing() {
    let s = SimStorage::new();
    let mut w = new_writer(&s, test_options());
    let mut tx = Transaction::new();
    tx.put_file(path("first"), b"kept".to_vec(), Attributes::default());
    w.commit(tx, &Job::new().ctx()).unwrap();
    let before = s.contents();

    let mut probe = Probe::new(deterministic_bytes(9, 500));
    probe.fail_after = Some(300);
    let mut tx = Transaction::new();
    tx.put_file_from(path("broken"), Arc::new(probe), Attributes::default());
    let e = w.commit(tx, &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::IoError);
    assert_eq!(s.contents(), before, "the aborted commit left bytes behind");

    // The writer is still usable and the old head still reads.
    let mut tx = Transaction::new();
    tx.put_file(path("second"), b"ok".to_vec(), Attributes::default());
    w.commit(tx, &Job::new().ctx()).unwrap();
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert_eq!(read_back(&s, &head.catalog, "first"), b"kept");
    assert!(head
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("broken"))
        .is_none());
}

#[test]
fn a_changed_length_stores_what_was_read_except_in_the_tar_profile() {
    // Default profile: stored as read (as tar and zip do).
    let s = SimStorage::new();
    let mut w = new_writer(&s, test_options());
    let mut probe = Probe::new(deterministic_bytes(3, 200));
    probe.declared = 150;
    let mut tx = Transaction::new();
    tx.put_file_from(path("grew"), Arc::new(probe), Attributes::default());
    w.commit(tx, &Job::new().ctx()).unwrap();
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert_eq!(
        read_back(&s, &head.catalog, "grew"),
        deterministic_bytes(3, 200)
    );

    // TAR-compatible: the header already holds the length, so it fails and
    // nothing is published.
    let s = SimStorage::new();
    let mut w = new_writer(
        &s,
        WriterOptions {
            profile: Some(TAR),
            ..test_options()
        },
    );
    for declared in [150, 250] {
        let mut probe = Probe::new(deterministic_bytes(3, 200));
        probe.declared = declared;
        let mut tx = Transaction::new();
        tx.put_file_from(path("changed"), Arc::new(probe), Attributes::default());
        let e = w.commit(tx, &Job::new().ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::IoError);
        assert!(e.message.contains("changed while it was read"), "{e}");
        assert!(s.contents().is_empty());
    }
}

/// A tree whose one file claims 5 GiB and must never be read by the walk.
struct Huge;

impl SourceTree for Huge {
    type Node = ();
    fn meta(&self, _: &()) -> Result<SourceMeta> {
        Ok(SourceMeta {
            kind: SourceKind::File,
            len: 5 << 30,
            attributes: Attributes::default(),
        })
    }
    fn children(&self, _: &()) -> Result<Vec<(Vec<u8>, ())>> {
        Ok(Vec::new())
    }
    fn source(&self, _: &(), len: u64) -> Result<Arc<dyn FileSource>> {
        let mut p = Probe::new(Vec::new());
        p.declared = len;
        Ok(Arc::new(p))
    }
}

#[test]
fn the_importer_holds_no_content_and_sets_no_size_cap_by_default() {
    let i = import(
        &Huge,
        &[(path("huge"), ())],
        &ImportOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!((i.files, i.bytes), (1, 5 << 30));
    // A cap is still available when the caller asks for one.
    let e = import(
        &Huge,
        &[(path("huge"), ())],
        &ImportOptions {
            max_total_bytes: Some(1 << 30),
        },
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::LimitExceeded);
}
