//! C5 helpers: a scripted multi-commit history with an independent model of
//! the expected state after each commit, and a reader that reconstructs every
//! file of an opened commit and checks its file-content hash.
//!
//! The model is plain maps, built without the catalog, so a test comparing
//! "what the archive says" against it is not comparing the code with itself.

use std::collections::BTreeMap;

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::catalog::namespace::EntryKind;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::manifest::{Attributes, Mtime, PosixAttributes};
use mochi_core::object::{decode_verified, load_stored};
use mochi_core::publish::{
    ArchiveWriter, CommitOutcome, OpenedHead, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::storage::{ReadStorage, Storage};
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_format::digest::file_content_hash;
use mochi_format::repr::DecodedSlice;

use crate::{deterministic_bytes, SeqIds};

/// One entry of an expected snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Dir,
    File(Vec<u8>),
}

/// Path (stored form) → content.
pub type State = BTreeMap<Vec<u8>, Content>;

/// A progress sink and cancellation token for tests.
#[derive(Default)]
pub struct Job {
    pub progress: NullProgress,
    pub cancel: CancellationToken,
}

impl Job {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn ctx(&self) -> JobContext<'_> {
        JobContext {
            progress: &self.progress,
            cancel: &self.cancel,
        }
    }
}

pub fn path(s: &str) -> ArchivePath {
    ArchivePath::from_stored(s.as_bytes()).expect("valid test path")
}

/// Deterministic writer options: small chunks so files span several
/// objects, and no wall-clock time.
pub fn test_options() -> WriterOptions {
    WriterOptions {
        read: ReadOptions::default(),
        chunk_size: Some(64),
        zstd_level: Some(3),
        record_time: false,
        profile: None,
    }
}

fn attrs(mode: u32) -> Attributes {
    Attributes {
        posix: Some(PosixAttributes {
            mode,
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

/// One scripted commit and the state it must produce.
#[derive(Debug, Clone)]
pub struct Step {
    pub tx: Transaction,
    pub after: State,
}

/// A three-commit history exercising files spanning several chunks, an empty
/// file, replacement, rename into a directory, deletion, and a hostile name
/// (spec §23.3 #9 fixtures must include one).
pub fn scripted_history() -> Vec<Step> {
    let mut steps = Vec::new();
    let mut state = State::new();
    let put = |state: &mut State, tx: &mut Transaction, p: &str, c: Option<Vec<u8>>| match c {
        None => {
            tx.put_dir(path(p), attrs(0o755));
            state.insert(p.as_bytes().to_vec(), Content::Dir);
        }
        Some(bytes) => {
            tx.put_file(path(p), bytes.clone(), attrs(0o644));
            state.insert(p.as_bytes().to_vec(), Content::File(bytes));
        }
    };

    // Commit 0.
    let mut tx = Transaction::new();
    tx.at(Mtime {
        secs: 1_700_000_000,
        nanos: 0,
    });
    put(&mut state, &mut tx, "docs", None);
    put(
        &mut state,
        &mut tx,
        "docs/a.txt",
        Some(deterministic_bytes(1, 200)),
    );
    put(&mut state, &mut tx, "empty", Some(Vec::new()));
    put(
        &mut state,
        &mut tx,
        "<img src=x onerror=alert(1)>",
        Some(b"hostile name, plain content".to_vec()),
    );
    steps.push(Step {
        tx,
        after: state.clone(),
    });

    // Commit 1: replace, add, rename.
    let mut tx = Transaction::new();
    tx.at(Mtime {
        secs: 1_700_000_100,
        nanos: 5,
    });
    put(
        &mut state,
        &mut tx,
        "docs/a.txt",
        Some(deterministic_bytes(2, 130)),
    );
    put(&mut state, &mut tx, "docs/c.bin", Some(vec![0u8; 300]));
    tx.rename(path("empty"), path("docs/empty"));
    let moved = state.remove(b"empty".as_slice()).expect("empty exists");
    state.insert(b"docs/empty".to_vec(), moved);
    steps.push(Step {
        tx,
        after: state.clone(),
    });

    // Commit 2: delete, and a larger multi-chunk file.
    let mut tx = Transaction::new();
    tx.at(Mtime {
        secs: 1_700_000_200,
        nanos: 0,
    });
    tx.delete(path("docs/c.bin"));
    state.remove(b"docs/c.bin".as_slice());
    put(
        &mut state,
        &mut tx,
        "big",
        Some(deterministic_bytes(3, 1000)),
    );
    steps.push(Step {
        tx,
        after: state.clone(),
    });
    steps
}

/// Create an archive in `storage` and commit `steps` in order.
pub fn build<S: Storage>(storage: S, seed: u64, steps: &[Step]) -> Result<Vec<CommitOutcome>> {
    let mut w = ArchiveWriter::create(storage, Box::new(SeqIds::new(seed)), test_options())?;
    let job = Job::new();
    let mut out = Vec::new();
    for s in steps {
        out.push(w.commit(s.tx.clone(), &job.ctx())?);
    }
    w.close()?;
    Ok(out)
}

/// Every file and directory of an opened commit, with each file rebuilt
/// from its chunks (stored and content integrity checked per chunk) and its
/// file-content hash checked over the reassembled logical stream.
pub fn read_state(src: &dyn ReadStorage, head: &OpenedHead) -> Result<State> {
    let limits = ReadOptions::default().limits;
    let cat = &head.catalog;
    let mut out = State::new();
    for (p, entry) in cat.replay(None)?.iter() {
        let key = p.as_stored().to_vec();
        if entry.kind == EntryKind::Directory {
            out.insert(key, Content::Dir);
            continue;
        }
        let (version, extents) = cat.file_version(&entry.version)?.ok_or_else(|| {
            MochiError::new(
                ErrorCode::CatalogInvalid,
                "snapshot names a missing version",
            )
        })?;
        let len = usize::try_from(version.logical_len)
            .map_err(|_| MochiError::new(ErrorCode::LimitExceeded, "test file too large"))?;
        let mut buf = vec![0u8; len];
        for e in &extents {
            let ExtentSource::Chunk {
                chunk,
                chunk_offset,
            } = e.source
            else {
                continue; // holes are already zero
            };
            let record = cat.object(&chunk)?.ok_or_else(|| {
                MochiError::new(ErrorCode::CatalogInvalid, "extent names a missing chunk")
            })?;
            let at = cat.object_location(&chunk)?.ok_or_else(|| {
                MochiError::new(ErrorCode::CatalogInvalid, "chunk has no location")
            })?;
            let stored = load_stored(src, at, &record, &limits)?;
            let decoded = decode_verified(&record, &stored, &limits)?;
            let from = chunk_offset as usize;
            let to = from + e.length as usize;
            let lo = e.logical_offset as usize;
            buf[lo..lo + e.length as usize].copy_from_slice(&decoded.as_bytes()[from..to]);
        }
        if Some(file_content_hash(DecodedSlice::from_logical(&buf))) != version.content_hash {
            return Err(MochiError::new(
                ErrorCode::ContentIntegrityFailed,
                "reassembled file does not match its file-content hash",
            ));
        }
        out.insert(key, Content::File(buf));
    }
    Ok(out)
}
