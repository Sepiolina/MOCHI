//! tar, tar.gz and tar.zst listing, testing and reading on `tar`, `flate2` and
//! `ruzstd`.
//!
//! tar has no per-entry check and no index. For `.tar.gz` the only check is the
//! gzip trailer (CRC-32 and length of the whole decompressed stream), which is
//! reached only by reading the stream to its end, so listing a `.tar.gz` reads
//! all of it, under the same limits as testing. A `.tar.zst` is the same shape:
//! its only check is each Zstandard frame's optional content checksum, compared
//! as the frame ends.

use std::cell::Cell;
use std::io::{self, Read, Seek, SeekFrom};
use std::rc::Rc;

use flate2::read::MultiGzDecoder;
use tar::{Archive, Entries, EntryType};

use crate::engine::{emit_breach, ratio_exceeded, truncate_text, EngineError, Out, Tally};
use crate::protocol::{Message, Request, DATA_CHUNK};
use crate::types::*;
use crate::zstdread::ZstdReader;

#[derive(Clone, Copy)]
struct Trip {
    kind: BreachKind,
    limit: u64,
    observed: u64,
}

#[derive(Default)]
struct Shared {
    decoded: Cell<u64>,
    compressed: Cell<u64>,
    trip: Cell<Option<Trip>>,
}

/// Counts the compressed bytes consumed (the denominator of the stream ratio).
struct Counting<R> {
    inner: R,
    shared: Rc<Shared>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.shared
            .compressed
            .set(self.shared.compressed.get() + n as u64);
        Ok(n)
    }
}

/// Enforces the total-output and stream-ratio limits on every byte the tar
/// layer reads, including bytes it skips. A trip is recorded and surfaces as an
/// I/O error that the walker converts into a reported breach.
struct Guard<R> {
    inner: R,
    shared: Rc<Shared>,
    limits: Limits,
    ratio: bool,
}

impl<R: Read> Read for Guard<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.shared.trip.get().is_some() {
            return Err(io::Error::other("limit already breached"));
        }
        let n = self.inner.read(buf)?;
        let decoded = self.shared.decoded.get().saturating_add(n as u64);
        self.shared.decoded.set(decoded);
        if decoded > self.limits.max_total_output {
            self.shared.trip.set(Some(Trip {
                kind: BreachKind::TotalOutput,
                limit: self.limits.max_total_output,
                observed: decoded,
            }));
            return Err(io::Error::other("total output limit"));
        }
        let compressed = self.shared.compressed.get();
        if self.ratio && ratio_exceeded(&self.limits, decoded, compressed) {
            self.shared.trip.set(Some(Trip {
                kind: BreachKind::Ratio,
                limit: self.limits.max_ratio,
                observed: decoded / compressed.max(1),
            }));
            return Err(io::Error::other("compression ratio limit"));
        }
        Ok(n)
    }
}

impl<R: Seek> Seek for Guard<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

struct Walk {
    tally: Tally,
    /// Stopped before the end of the tar stream (limit, error, or Read done).
    stopped: bool,
    /// A non-limit I/O error came out of the stream layer.
    stream_error: Option<String>,
    found: u64,
}

fn entry_kind(t: EntryType) -> EntryKind {
    match t {
        EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => EntryKind::File,
        EntryType::Directory => EntryKind::Directory,
        EntryType::Symlink => EntryKind::Symlink,
        EntryType::Link => EntryKind::HardLink,
        _ => EntryKind::Other,
    }
}

fn report_trip(
    shared: &Shared,
    entry: Option<u64>,
    out: &mut Out,
) -> Result<Option<Trip>, EngineError> {
    match shared.trip.get() {
        Some(t) => {
            emit_breach(out, t.kind, entry, t.limit, t.observed)?;
            Ok(Some(t))
        }
        None => Ok(None),
    }
}

#[allow(clippy::too_many_arguments)]
fn walk<G: Read>(
    entries: Entries<'_, G>,
    shared: &Shared,
    archive_len: Option<u64>,
    req: &Request,
    out: &mut Out,
) -> Result<Walk, EngineError> {
    let l = req.limits;
    let mut w = Walk {
        tally: Tally {
            complete: true,
            ..Tally::default()
        },
        stopped: false,
        stream_error: None,
        found: 0,
    };
    let mut buf = vec![0u8; DATA_CHUNK];
    let mut index: u64 = 0;
    for item in entries {
        let mut entry = match item {
            Ok(e) => e,
            Err(e) => {
                let last = index.checked_sub(1);
                if report_trip(shared, last, out)?.is_none() {
                    w.stream_error = Some(truncate_text(e.to_string()));
                    w.tally.stop_reason = Some(truncate_text(format!(
                        "archive damaged or truncated after {index} entries: {e}"
                    )));
                } else {
                    w.tally.stop_reason = Some("a whole-operation limit was reached".to_string());
                }
                w.tally.complete = false;
                w.stopped = true;
                break;
            }
        };
        if index >= l.max_entries {
            emit_breach(out, BreachKind::EntryCount, None, l.max_entries, index + 1)?;
            w.tally.complete = false;
            w.tally.stop_reason = Some(format!("more than {} entries", l.max_entries));
            w.stopped = true;
            break;
        }
        let selected = req.wants(index);
        if !selected {
            index += 1;
            continue;
        }
        let ty = entry.header().entry_type();
        let kind = entry_kind(ty);
        let raw_name = entry.path_bytes().into_owned();
        let size = entry.size();
        let mut error = None;
        let name = if raw_name.len() as u64 > u64::from(l.max_name_len) {
            emit_breach(
                out,
                BreachKind::NameLength,
                Some(index),
                u64::from(l.max_name_len),
                raw_name.len() as u64,
            )?;
            error = Some(format!(
                "entry name is {} bytes, over the {}-byte limit; not carried",
                raw_name.len(),
                l.max_name_len
            ));
            Vec::new()
        } else {
            raw_name
        };
        if let (Some(len), true) = (archive_len, kind == EntryKind::File) {
            let end = entry.raw_file_position().checked_add(size);
            if end.is_none_or(|e| e > len) {
                error =
                    Some("entry data extends past the end of the archive (truncated)".to_string());
            }
        }
        let link_target = entry.link_name_bytes().map(|b| b.into_owned());
        let info = EntryInfo {
            index,
            name,
            kind,
            size: Some(size),
            compressed_size: None,
            encryption: Encryption::None,
            method: if ty.is_pax_global_extensions() {
                "pax-global-header"
            } else {
                "tar"
            }
            .to_string(),
            link_target,
            unix_mode: entry.header().mode().ok(),
            mtime_unix: entry
                .header()
                .mtime()
                .ok()
                .and_then(|t| i64::try_from(t).ok()),
            name_is_utf8_flag: None,
            error,
        };
        w.tally.entries += 1;
        if req.op != Op::Read {
            out(Message::Entry(info.clone()))?;
        }
        if req.op == Op::List {
            if info.error.is_some() {
                w.tally.complete = false;
                w.tally.stop_reason = info.error.clone();
                w.stopped = true;
                break;
            }
            index += 1;
            continue;
        }

        // Test or Read of this entry.
        let mut entry_out: u64 = 0;
        let mut stop_all = false;
        let outcome = if let Some(e) = &info.error {
            stop_all = e.contains("truncated");
            EntryOutcome::ReadError(e.clone())
        } else if kind != EntryKind::File {
            EntryOutcome::NoData
        } else if size > l.max_entry_size {
            emit_breach(
                out,
                BreachKind::EntrySize,
                Some(index),
                l.max_entry_size,
                size,
            )?;
            EntryOutcome::Breach(BreachKind::EntrySize)
        } else {
            loop {
                match entry.read(&mut buf) {
                    Ok(0) => break EntryOutcome::NoEntryCheck,
                    Ok(n) => {
                        entry_out += n as u64;
                        w.tally.bytes_out += n as u64;
                        if entry_out > l.max_entry_size {
                            emit_breach(
                                out,
                                BreachKind::EntrySize,
                                Some(index),
                                l.max_entry_size,
                                entry_out,
                            )?;
                            break EntryOutcome::Breach(BreachKind::EntrySize);
                        }
                        if req.op.is_read() {
                            out(Message::Data(buf[..n].to_vec()))?;
                        }
                    }
                    Err(e) => {
                        if let Some(t) = report_trip(shared, Some(index), out)? {
                            stop_all = true;
                            break EntryOutcome::Breach(t.kind);
                        }
                        stop_all = true;
                        w.stream_error = Some(truncate_text(e.to_string()));
                        break EntryOutcome::ReadError(truncate_text(e.to_string()));
                    }
                }
            }
        };
        out(Message::Result { index, outcome })?;
        w.found += 1;
        if stop_all {
            w.tally.complete = false;
            w.tally.stop_reason.get_or_insert_with(|| {
                "the stream stopped (limit or damage); remaining entries were not run".to_string()
            });
            w.stopped = true;
            break;
        }
        if req.wanted_count().is_some_and(|n| w.found >= n) {
            w.stopped = true;
            break;
        }
        index += 1;
    }
    Ok(w)
}

fn finish_gz(
    mut guard: Guard<MultiGzDecoder<Counting<impl Read>>>,
    w: &Walk,
    shared: &Shared,
    out: &mut Out,
) -> Result<(), EngineError> {
    let check = if let Some(e) = &w.stream_error {
        StreamCheck::Failed(e.clone())
    } else if w.stopped {
        StreamCheck::NotReached
    } else {
        match io::copy(&mut guard, &mut io::sink()) {
            Ok(_) => StreamCheck::Matched(CheckSet::of(&[CheckKind::GzipStreamCrc32])),
            Err(e) => {
                if report_trip(shared, None, out)?.is_some() {
                    StreamCheck::NotReached
                } else {
                    StreamCheck::Failed(truncate_text(e.to_string()))
                }
            }
        }
    };
    out(Message::StreamCheck(check))?;
    Ok(())
}

/// The compression layer around the tar stream.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    None,
    Gzip,
    Zstd,
}

fn finish_zst(
    mut guard: Guard<ZstdReader<Counting<impl Read>>>,
    w: &Walk,
    shared: &Shared,
    out: &mut Out,
) -> Result<(), EngineError> {
    let check = if let Some(e) = &w.stream_error {
        StreamCheck::Failed(e.clone())
    } else if w.stopped {
        StreamCheck::NotReached
    } else {
        match io::copy(&mut guard, &mut io::sink()) {
            Ok(_) if guard.inner.fully_checksummed() => {
                StreamCheck::Matched(CheckSet::of(&[CheckKind::ZstdFrameChecksum]))
            }
            // The stream ended cleanly but a frame declared no checksum, so
            // there is nothing to report as matched.
            Ok(_) => StreamCheck::NotApplicable,
            Err(e) => {
                if report_trip(shared, None, out)?.is_some() {
                    StreamCheck::NotReached
                } else {
                    StreamCheck::Failed(truncate_text(e.to_string()))
                }
            }
        }
    };
    out(Message::StreamCheck(check))?;
    Ok(())
}

pub fn run<R: Read + Seek>(
    src: R,
    archive_len: u64,
    layer: Layer,
    req: &Request,
    out: &mut Out,
) -> Result<Tally, EngineError> {
    let shared = Rc::new(Shared::default());
    let bad = |e: io::Error| {
        EngineError::Archive(ForeignError::new(
            ErrorCode::MalformedArchive,
            truncate_text(format!("cannot read tar structure: {e}")),
        ))
    };
    let w = if layer == Layer::Zstd {
        let counting = Counting {
            inner: src,
            shared: Rc::clone(&shared),
        };
        let guard = Guard {
            inner: ZstdReader::new(counting),
            shared: Rc::clone(&shared),
            limits: req.limits,
            ratio: true,
        };
        let mut archive = Archive::new(guard);
        let w = walk(archive.entries().map_err(bad)?, &shared, None, req, out)?;
        finish_zst(archive.into_inner(), &w, &shared, out)?;
        w
    } else if layer == Layer::Gzip {
        let counting = Counting {
            inner: src,
            shared: Rc::clone(&shared),
        };
        let guard = Guard {
            inner: MultiGzDecoder::new(counting),
            shared: Rc::clone(&shared),
            limits: req.limits,
            ratio: true,
        };
        let mut archive = Archive::new(guard);
        let w = walk(archive.entries().map_err(bad)?, &shared, None, req, out)?;
        finish_gz(archive.into_inner(), &w, &shared, out)?;
        w
    } else {
        let guard = Guard {
            inner: src,
            shared: Rc::clone(&shared),
            limits: req.limits,
            ratio: false,
        };
        let mut archive = Archive::new(guard);
        let w = walk(
            archive.entries_with_seek().map_err(bad)?,
            &shared,
            Some(archive_len),
            req,
            out,
        )?;
        out(Message::StreamCheck(StreamCheck::NotApplicable))?;
        w
    };
    if req.wanted_count().is_some_and(|n| w.found < n) {
        return Err(EngineError::Archive(ForeignError::new(
            ErrorCode::EntryNotFound,
            "a requested entry was not found",
        )));
    }
    Ok(w.tally)
}
