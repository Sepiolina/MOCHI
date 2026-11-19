//! Reading, hashing and compressing a commit's file content ahead of the
//! writer, on worker threads (release 0.9.11).
//!
//! One reader thread takes the transaction's files in order and cuts them
//! into pieces of the chunk size, exactly as the writer does; worker threads
//! compute each piece's chunk-content hash and, when the writer will store
//! it unprotected, its Zstandard frame. The writer takes the pieces strictly
//! in order ([`EncodeAhead::next`]) and still decides everything itself:
//! dedup, object IDs (drawn as it stores each object, in the same order as
//! the one-thread path), offsets and the catalog. The archive written is
//! therefore byte for byte the one the one-thread path writes; only the
//! compression work moves to other cores. A piece the writer deduplicates
//! after all was compressed for nothing, which costs CPU, not correctness.
//!
//! At most `in_flight` pieces are read and not yet taken, which bounds the
//! memory to about `in_flight` × (chunk size + its frame).

use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::Scope;

use mochi_format::codec::{self, EncodeParams, Protection};
use mochi_format::digest::{chunk_content_hash, ChunkContentHash};
use mochi_format::repr::{DecodedBytes, StoredObject};
use mochi_format::Limits;

use crate::error::{ErrorCode, MochiError, Result};

/// Threads [`crate::publish::WriterOptions::threads`] means by `None`: the
/// processor count, at most 8 (more mostly adds memory and disk contention).
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
}

/// A file's content as the reader sees it.
pub(crate) enum Content<'a> {
    Memory(&'a [u8]),
    Source(&'a dyn crate::publish::FileSource),
}

/// One piece of a file, hashed and possibly compressed.
pub(crate) struct Piece {
    pub decoded: DecodedBytes,
    pub key: (u64, ChunkContentHash),
    /// The unprotected Zstandard frame, when the workers made one.
    pub stored: Option<StoredObject>,
}

/// What [`EncodeAhead::next`] hands over, in order.
pub(crate) enum Item {
    Piece(Piece),
    /// The current file has no more pieces.
    End,
}

/// The writer's end of the pipeline. Dropping it stops the threads.
pub(crate) struct EncodeAhead {
    results: Receiver<(u64, Result<Item>)>,
    pending: BTreeMap<u64, Result<Item>>,
    next: u64,
    permits: SyncSender<()>,
}

impl EncodeAhead {
    /// Starts the reader and `threads` workers in `scope` over `files` (the
    /// transaction's files, in order). `encode`: whether workers compress
    /// (only for unprotected objects; a sealed one is built by the writer).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start<'scope, 'env>(
        scope: &'scope Scope<'scope, 'env>,
        files: Vec<Content<'env>>,
        piece: usize,
        params: EncodeParams,
        limits: &'env Limits,
        encode: bool,
        threads: usize,
    ) -> EncodeAhead {
        let threads = threads.max(1);
        let in_flight = threads.saturating_mul(2).max(2);
        let (permits, take) = sync_channel::<()>(in_flight);
        for _ in 0..in_flight {
            // The channel has room for exactly these.
            let _ = permits.try_send(());
        }
        let (results_tx, results) = channel::<(u64, Result<Item>)>();
        let (jobs_tx, jobs_rx) = channel::<(u64, Vec<u8>)>();
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));

        for _ in 0..threads {
            let (jobs_rx, results_tx) = (Arc::clone(&jobs_rx), results_tx.clone());
            scope.spawn(move || worker(&jobs_rx, results_tx, params, limits, encode));
        }
        scope.spawn(move || reader(files, piece, take, jobs_tx, results_tx));
        EncodeAhead {
            results,
            pending: BTreeMap::new(),
            next: 0,
            permits,
        }
    }

    /// The next item, in order.
    pub(crate) fn next(&mut self) -> Result<Item> {
        loop {
            if let Some(item) = self.pending.remove(&self.next) {
                self.next += 1;
                // Never blocks: at most `in_flight` permits are out.
                let _ = self.permits.try_send(());
                return item;
            }
            match self.results.recv() {
                Ok((seq, item)) => {
                    self.pending.insert(seq, item);
                }
                Err(_) => {
                    return Err(MochiError::new(
                        ErrorCode::IoError,
                        "internal: the content reader stopped before the commit's content ended",
                    ))
                }
            }
        }
    }
}

fn reader(
    files: Vec<Content<'_>>,
    piece: usize,
    take: Receiver<()>,
    jobs: Sender<(u64, Vec<u8>)>,
    results: Sender<(u64, Result<Item>)>,
) {
    let mut seq = 0u64;
    // `false` once the writer is gone (or an error was handed over).
    let mut emit = |item: Option<Result<Item>>, bytes: Option<Vec<u8>>| -> bool {
        if take.recv().is_err() {
            return false;
        }
        let s = seq;
        seq += 1;
        match (item, bytes) {
            (_, Some(b)) => jobs.send((s, b)).is_ok(),
            (Some(i), None) => {
                let stop = i.is_err();
                results.send((s, i)).is_ok() && !stop
            }
            (None, None) => false,
        }
    };
    for file in files {
        match file {
            Content::Memory(bytes) => {
                for part in bytes.chunks(piece) {
                    if !emit(None, Some(part.to_vec())) {
                        return;
                    }
                }
            }
            Content::Source(source) => {
                let mut r = match source.open() {
                    Ok(r) => r,
                    Err(e) => {
                        emit(Some(Err(e)), None);
                        return;
                    }
                };
                loop {
                    let mut buf = Vec::new();
                    if let Err(e) = crate::publish::read_piece(r.as_mut(), &mut buf, piece) {
                        emit(Some(Err(e)), None);
                        return;
                    }
                    if buf.is_empty() {
                        break;
                    }
                    if !emit(None, Some(buf)) {
                        return;
                    }
                }
            }
        }
        if !emit(Some(Ok(Item::End)), None) {
            return;
        }
    }
}

fn worker(
    jobs: &Mutex<Receiver<(u64, Vec<u8>)>>,
    results: Sender<(u64, Result<Item>)>,
    params: EncodeParams,
    limits: &Limits,
    encode: bool,
) {
    loop {
        let job = match jobs.lock() {
            Ok(rx) => rx.recv(),
            Err(_) => return,
        };
        let Ok((seq, bytes)) = job else { return };
        let item = catch_unwind(AssertUnwindSafe(|| -> Result<Item> {
            let decoded = DecodedBytes::new(bytes);
            let key = (decoded.len(), chunk_content_hash(&decoded));
            let stored = if encode {
                Some(codec::encode_object(
                    &decoded,
                    &params,
                    Protection::None,
                    limits,
                )?)
            } else {
                None
            };
            Ok(Item::Piece(Piece {
                decoded,
                key,
                stored,
            }))
        }))
        .unwrap_or_else(|_| {
            Err(MochiError::new(
                ErrorCode::IoError,
                "internal: compressing a chunk failed unexpectedly",
            ))
        });
        if results.send((seq, item)).is_err() {
            return;
        }
    }
}
