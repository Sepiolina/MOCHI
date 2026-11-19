//! 7z listing, testing and reading on `sevenz-rust2` (decoders only: its
//! `compress` feature is off). Entry bytes only ever go to the message sink;
//! no extract-to-disk function of the library is used.
//!
//! What 7z checks: a CRC32 per entry when the archive recorded one. There is
//! no whole-archive stream check. The start header and the (possibly
//! compressed) header have their own CRCs, which the library verifies while
//! opening; that is a statement about the header, not about any entry.
//!
//! A 7z block is decoded front to back (a solid block holds many entries), so
//! the library hands entries out **block by block**, not in listing order.
//! `Test` collects each entry's outcome and reports them in index order;
//! `ReadMany` delivers in the order the blocks are decoded (the client's
//! collector accepts any order of the requested set).

use std::io::{self, Read, Seek, SeekFrom};

use sevenz_rust2::{
    Archive, ArchiveEntry, BlockDecoder, EncoderMethod, Error as SzError, Password,
};

use crate::engine::{emit_breach, ratio_exceeded, truncate_text, EngineError, Origin, Out, Tally};
use crate::protocol::{Message, Request, DATA_CHUNK};
use crate::types::*;
use crate::volumes::SplitName;

/// Memory one coder may ask for (an LZMA/LZMA2 dictionary, PPMd memory). A
/// hostile header can claim 4 GiB; refusing is an `Unsupported` outcome for
/// the entries in that block, not an allocation failure.
pub const MAX_CODER_MEMORY: u64 = 1 << 30;

/// The largest encrypted entry that is held in memory until its checks pass,
/// so a wrong password delivers nothing (plan D8 rule 7).
pub const MAX_HELD_ENTRY: u64 = 512 << 20;

const AES_ID: &[u8] = EncoderMethod::ID_AES256_SHA256;
const WIN_ATTR_UNIX_EXTENSION: u32 = 0x8000;
const WIN_ATTR_REPARSE_POINT: u32 = 0x400;
/// 100 ns ticks from 1601-01-01 to 1970-01-01.
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

fn malformed(e: impl std::fmt::Display) -> EngineError {
    EngineError::Archive(ForeignError::new(
        ErrorCode::MalformedArchive,
        truncate_text(format!("cannot read 7z structure: {e}")),
    ))
}

fn archive_err(code: ErrorCode, msg: impl Into<String>) -> EngineError {
    EngineError::Archive(ForeignError::new(code, truncate_text(msg.into())))
}

/// Maps the library's error from opening the archive. `had_password`: the
/// caller supplied one.
fn open_error(e: SzError, had_password: bool) -> EngineError {
    match e {
        SzError::PasswordRequired => archive_err(
            ErrorCode::PasswordRequired,
            "the archive's names are encrypted; a password is needed to list it",
        ),
        SzError::MaybeBadPassword(_) => archive_err(
            ErrorCode::WrongPassword,
            "the password did not open the archive's names (or its header is damaged)",
        ),
        // With encrypted names, a wrong password decrypts to noise and the
        // header's own CRC rejects it: indistinguishable from damage.
        SzError::ChecksumVerificationFailed | SzError::NextHeaderCrcMismatch if had_password => {
            archive_err(
                ErrorCode::WrongPassword,
                "the archive header did not check out: a wrong password, or a damaged header",
            )
        }
        SzError::BadSignature(_) => archive_err(ErrorCode::NotAnArchive, "not a 7z archive"),
        SzError::UnsupportedVersion { major, minor } => archive_err(
            ErrorCode::UnsupportedFormat,
            format!("7z format version {major}.{minor} is not supported"),
        ),
        other => malformed(other),
    }
}

/// The start header says where the next header ends; if that is past the end
/// of the data we have, the archive is truncated, or (for a `.7z.001` set) a
/// volume is missing. Said plainly, before the library reports an I/O error.
fn check_truncation<R: Read + Seek>(
    src: &mut R,
    len: u64,
    origin: &Origin<'_>,
) -> Result<(), EngineError> {
    let mut head = [0u8; 32];
    src.seek(SeekFrom::Start(0))
        .and_then(|_| src.read_exact(&mut head))
        .map_err(|_| {
            archive_err(
                ErrorCode::MalformedArchive,
                "the 7z start header is truncated",
            )
        })?;
    // A zeroed start header with a zero CRC is how 7-Zip leaves an archive it
    // never finished. sevenz-rust2 then scans backwards for anything that looks
    // like a header and tries to parse each candidate, which on hostile bytes
    // can spin for minutes (audit F16). A magic byte is a candidate, not proof:
    // refuse rather than guess.
    if head[8..32].iter().all(|b| *b == 0) {
        return Err(archive_err(
            ErrorCode::MalformedArchive,
            "the 7z start header is empty (an unfinished or damaged archive); its headers are not guessed at",
        ));
    }
    let off = u64::from_le_bytes(head[12..20].try_into().unwrap_or([0; 8]));
    let size = u64::from_le_bytes(head[20..28].try_into().unwrap_or([0; 8]));
    if off == 0 && size == 0 {
        return Ok(()); // an empty archive
    }
    let end = 32u64.checked_add(off).and_then(|v| v.checked_add(size));
    if end.is_none_or(|e| e > len) {
        let needs = end.map_or_else(|| "more than 2^64".to_string(), |e| e.to_string());
        let msg = if origin.split == SplitName::First {
            format!(
                "this looks like a split 7z set but the {} volume(s) found hold {len} bytes and the archive needs at least {needs}: a later volume is missing or truncated",
                origin.volumes
            )
        } else {
            format!(
                "the archive is truncated: its header lies beyond the end of the file ({len} bytes; at least {needs} are needed)"
            )
        };
        let code = if origin.split == SplitName::First {
            ErrorCode::MissingVolume
        } else {
            ErrorCode::MalformedArchive
        };
        return Err(archive_err(code, msg));
    }
    Ok(())
}

fn method_label(archive: &Archive, block: Option<usize>) -> String {
    let Some(b) = block.and_then(|b| archive.blocks.get(b)) else {
        return "empty".to_string();
    };
    let names: Vec<String> = b
        .coders
        .iter()
        .map(|c| {
            EncoderMethod::by_id(c.encoder_method_id()).map_or_else(
                || {
                    c.encoder_method_id()
                        .iter()
                        .map(|x| format!("{x:02X}"))
                        .collect::<String>()
                },
                |m| m.name().to_string(),
            )
        })
        .collect();
    names.join("+")
}

/// A coder's method properties. The library keeps them private in the
/// version this build pins (0.20), so they are read from its `Debug` form;
/// `tests/sevenz_fixtures.rs` pins that the parse yields the dictionary size
/// the fixture was written with, so a library update that changes the form
/// fails loudly instead of silently disabling the memory pre-check.
fn coder_properties(c: &sevenz_rust2::Coder) -> Vec<u8> {
    let dbg = format!("{c:?}");
    let Some(at) = dbg.find("properties: [") else {
        return Vec::new();
    };
    let rest = &dbg[at + "properties: [".len()..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .filter_map(|n| n.trim().parse::<u8>().ok())
        .collect()
}

/// The largest memory request among a block's coders, in bytes, when one is
/// over the limit.
fn oversized_coder(archive: &Archive, block: usize) -> Option<u64> {
    let b = archive.blocks.get(block)?;
    for c in &b.coders {
        let p = coder_properties(c);
        let p = p.as_slice();
        let id = c.encoder_method_id();
        let need = if id == EncoderMethod::ID_LZMA && p.len() == 5 {
            u64::from(u32::from_le_bytes([p[1], p[2], p[3], p[4]]))
        } else if id == EncoderMethod::ID_LZMA2 && p.len() == 1 {
            match p[0] {
                40 => u64::from(u32::MAX),
                v if v < 40 => (2 | u64::from(v & 1)) << (v / 2 + 11),
                _ => u64::MAX,
            }
        } else if id == EncoderMethod::ID_PPMD && p.len() == 5 {
            u64::from(u32::from_le_bytes([p[1], p[2], p[3], p[4]]))
        } else {
            0
        };
        if need > MAX_CODER_MEMORY {
            return Some(need);
        }
    }
    None
}

fn is_encrypted(archive: &Archive, block: Option<usize>) -> bool {
    block
        .and_then(|b| archive.blocks.get(b))
        .is_some_and(|b| b.coders.iter().any(|c| c.encoder_method_id() == AES_ID))
}

fn mtime_unix(e: &ArchiveEntry) -> Option<i64> {
    if !e.has_last_modified_date {
        return None;
    }
    let ticks: u64 = e.last_modified_date().into();
    // Before 1970 as well: signed seconds.
    let secs = (i128::from(ticks) - i128::from(UNIX_EPOCH_TICKS)).div_euclid(10_000_000);
    i64::try_from(secs).ok()
}

fn read_info(
    archive: &Archive,
    i: usize,
    limits: &Limits,
) -> (EntryInfo, Option<u64> /* over-long name */) {
    let f = &archive.files[i];
    let block = archive
        .stream_map
        .file_block_index
        .get(i)
        .copied()
        .flatten();
    let encryption = if is_encrypted(archive, block) {
        Encryption::Aes(256)
    } else {
        Encryption::None
    };
    let attr = f.has_windows_attributes.then_some(f.windows_attributes);
    let unix_mode = attr
        .filter(|a| a & WIN_ATTR_UNIX_EXTENSION != 0)
        .map(|a| a >> 16);
    let kind = if f.is_anti_item {
        EntryKind::Other
    } else if f.is_directory {
        EntryKind::Directory
    } else {
        match unix_mode.map(|m| m & 0o170000) {
            Some(0o120000) => EntryKind::Symlink,
            Some(0o100000) | None => {
                if attr.is_some_and(|a| a & WIN_ATTR_REPARSE_POINT != 0) {
                    EntryKind::Symlink
                } else {
                    EntryKind::File
                }
            }
            Some(0o040000) => EntryKind::Directory,
            Some(_) => EntryKind::Other,
        }
    };
    let raw = f.name().as_bytes();
    let (name, error, long) = if raw.len() as u64 > u64::from(limits.max_name_len) {
        (
            Vec::new(),
            Some(format!(
                "entry name is {} bytes, over the {}-byte limit; not carried",
                raw.len(),
                limits.max_name_len
            )),
            Some(raw.len() as u64),
        )
    } else {
        (raw.to_vec(), None, None)
    };
    (
        EntryInfo {
            index: i as u64,
            name,
            kind,
            size: Some(f.size()),
            compressed_size: None,
            encryption,
            method: method_label(archive, block),
            link_target: None,
            unix_mode,
            mtime_unix: mtime_unix(f),
            name_is_utf8_flag: None,
            error,
        },
        long,
    )
}

/// What the entry reader's error means.
fn classify_read_error(e: &io::Error, encrypted: bool) -> EntryOutcome {
    let is_crc = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<SzError>())
        .is_some_and(|se| matches!(se, SzError::ChecksumVerificationFailed));
    if is_crc {
        return EntryOutcome::Mismatch {
            failed: CheckSet::of(&[CheckKind::Crc32]),
            wrong_password_possible: encrypted,
        };
    }
    let mut text = truncate_text(e.to_string());
    if encrypted {
        text.push_str(" (damaged data, or a wrong password)");
    }
    EntryOutcome::ReadError(text)
}

struct Run<'a> {
    req: &'a Request,
    limits: Limits,
    archive_len: u64,
    infos: Vec<EntryInfo>,
    /// Whether the archive recorded a CRC32 for each entry.
    has_crc: Vec<bool>,
    /// Outcomes by index; `None` until the entry has been handled.
    outcomes: Vec<Option<EntryOutcome>>,
    /// Entries already announced (`ENTRY` sent) in a read operation.
    announced: Vec<bool>,
    /// Every byte decoded, wanted or not (solid blocks decode what precedes
    /// the entry asked for): the ratio and total-output limits count it.
    decoded: u64,
    /// Bytes of wanted entries.
    bytes_out: u64,
    buf: Vec<u8>,
    sink_err: Option<io::Error>,
    stop_all: bool,
}

impl Run<'_> {
    fn deliver(&self) -> bool {
        self.req.op.is_read()
    }

    fn announce(&mut self, i: usize, out: &mut Out) -> io::Result<()> {
        if self.req.op == Op::ReadMany && !self.announced[i] {
            self.announced[i] = true;
            out(Message::Entry(self.infos[i].clone()))?;
        }
        Ok(())
    }

    /// Records the final outcome of entry `i`; in a read, also sends it.
    fn finish(&mut self, i: usize, outcome: EntryOutcome, out: &mut Out) -> io::Result<()> {
        if self.outcomes[i].is_some() {
            return Ok(());
        }
        if self.req.op.is_read() {
            self.announce(i, out)?;
            out(Message::Result {
                index: i as u64,
                outcome: outcome.clone(),
            })?;
        }
        self.outcomes[i] = Some(outcome);
        Ok(())
    }

    /// Reads one entry's stream to its end (or a limit) and returns its
    /// outcome. `want`: the operation handles this entry (otherwise it is
    /// only drained to keep a solid block in step).
    fn stream_one(
        &mut self,
        i: usize,
        want: bool,
        reader: &mut dyn Read,
        out: &mut Out,
    ) -> Result<(EntryOutcome, bool /* block still usable */), io::Error> {
        let l = self.limits;
        let info_size = self.infos[i].size.unwrap_or(0);
        let encrypted = self.infos[i].encryption != Encryption::None;
        if want && info_size > l.max_entry_size {
            emit_breach(
                out,
                BreachKind::EntrySize,
                Some(i as u64),
                l.max_entry_size,
                info_size,
            )?;
            return Ok((EntryOutcome::Breach(BreachKind::EntrySize), false));
        }
        let hold = want && self.deliver() && encrypted;
        if hold && info_size > MAX_HELD_ENTRY {
            // Fail-closed extraction holds the entry until its checks pass;
            // this one is too large to hold. Said, not delivered unchecked.
            return Ok((
                EntryOutcome::Unsupported(format!(
                    "encrypted entry of {info_size} bytes is over the {MAX_HELD_ENTRY}-byte limit this build holds in memory to keep a wrong password from delivering partial output"
                )),
                false,
            ));
        }
        if want {
            self.announce(i, out)?;
        }
        let mut held: Vec<u8> = Vec::new();
        let mut entry_out: u64 = 0;
        loop {
            let n = match reader.read(&mut self.buf) {
                Ok(0) => break,
                Ok(n) => n as u64,
                Err(e) => return Ok((classify_read_error(&e, encrypted), false)),
            };
            entry_out += n;
            self.decoded += n;
            if want {
                self.bytes_out += n;
            }
            if self.decoded > l.max_total_output {
                emit_breach(
                    out,
                    BreachKind::TotalOutput,
                    Some(i as u64),
                    l.max_total_output,
                    self.decoded,
                )?;
                self.stop_all = true;
                return Ok((EntryOutcome::Breach(BreachKind::TotalOutput), false));
            }
            if want && entry_out > l.max_entry_size {
                emit_breach(
                    out,
                    BreachKind::EntrySize,
                    Some(i as u64),
                    l.max_entry_size,
                    entry_out,
                )?;
                return Ok((EntryOutcome::Breach(BreachKind::EntrySize), false));
            }
            if ratio_exceeded(&l, self.decoded, self.archive_len) {
                emit_breach(
                    out,
                    BreachKind::Ratio,
                    Some(i as u64),
                    l.max_ratio,
                    self.decoded / self.archive_len.max(1),
                )?;
                self.stop_all = true;
                return Ok((EntryOutcome::Breach(BreachKind::Ratio), false));
            }
            if want && self.deliver() {
                let chunk = &self.buf[..n as usize];
                if hold {
                    held.extend_from_slice(chunk);
                } else {
                    out(Message::Data(chunk.to_vec()))?;
                }
            }
        }
        // The stream ended without an error: the CRC check, if the archive
        // recorded one, passed (the library raises it at the final read).
        if hold {
            for chunk in held.chunks(DATA_CHUNK) {
                out(Message::Data(chunk.to_vec()))?;
            }
        }
        let outcome = if self.archive_has_crc(i) {
            EntryOutcome::Matched(CheckSet::of(&[CheckKind::Crc32]))
        } else {
            EntryOutcome::NoEntryCheck
        };
        Ok((outcome, true))
    }

    fn archive_has_crc(&self, i: usize) -> bool {
        self.has_crc.get(i).copied().unwrap_or(false)
    }
}

/// The password as the library wants it: 7z passwords are text (UTF-16 on
/// the wire of the format), so bytes that are not UTF-8 cannot be one.
fn password_of(req: &Request) -> Result<Password, EngineError> {
    match &req.password {
        None => Ok(Password::empty()),
        Some(p) => std::str::from_utf8(p.as_bytes())
            .map(Password::new)
            .map_err(|_| {
                archive_err(
                    ErrorCode::BadRequest,
                    "a 7z password must be valid UTF-8 text",
                )
            }),
    }
}

pub fn run<R: Read + Seek>(
    mut src: R,
    archive_len: u64,
    origin: &Origin<'_>,
    req: &Request,
    out: &mut Out,
) -> Result<Tally, EngineError> {
    check_truncation(&mut src, archive_len, origin)?;
    let password = password_of(req)?;
    // sevenz-rust2 sizes some vectors straight from header counts, which can
    // overflow a capacity and panic. A panic while reading the header is a
    // malformed archive, not a crash of the helper.
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Archive::read(&mut src, &password)
    }));
    let archive = match read {
        Ok(r) => r.map_err(|e| open_error(e, req.password.is_some()))?,
        Err(_) => {
            return Err(archive_err(
                ErrorCode::MalformedArchive,
                "the 7z header claims more entries or streams than can be held",
            ))
        }
    };
    let l = req.limits;
    let total = archive.files.len();
    if req.op.is_read() {
        let wanted: Vec<u64> = if req.op == Op::Read {
            vec![req.read_index]
        } else {
            req.read_indices.clone()
        };
        if let Some(bad) = wanted.iter().find(|i| **i >= total as u64) {
            return Err(archive_err(
                ErrorCode::EntryNotFound,
                format!("entry {bad} does not exist ({total} entries)"),
            ));
        }
    }
    let cap = usize::try_from(l.max_entries)
        .unwrap_or(usize::MAX)
        .min(total);
    let mut tally = Tally {
        complete: true,
        ..Tally::default()
    };
    if cap < total {
        emit_breach(
            out,
            BreachKind::EntryCount,
            None,
            l.max_entries,
            total as u64,
        )?;
        tally.complete = false;
        tally.stop_reason = Some(format!(
            "entry count {total} exceeds the limit {}",
            l.max_entries
        ));
    }
    let mut infos = Vec::with_capacity(cap);
    for i in 0..cap {
        let (info, long) = read_info(&archive, i, &l);
        if let (Some(observed), true) = (long, req.wants(i as u64)) {
            emit_breach(
                out,
                BreachKind::NameLength,
                Some(i as u64),
                u64::from(l.max_name_len),
                observed,
            )?;
        }
        infos.push(info);
    }
    if req.op == Op::List {
        for info in &infos {
            out(Message::Entry(info.clone()))?;
        }
        tally.entries = cap as u64;
        out(Message::StreamCheck(StreamCheck::NotApplicable))?;
        return Ok(tally);
    }

    let has_crc: Vec<bool> = (0..cap)
        .map(|i| {
            let f = &archive.files[i];
            f.has_crc && f.has_stream && f.size() > 0
        })
        .collect();
    let mut run = Run {
        req,
        limits: l,
        archive_len,
        infos,
        has_crc,
        outcomes: vec![None; cap],
        announced: vec![false; cap],
        decoded: 0,
        bytes_out: 0,
        buf: vec![0u8; DATA_CHUNK],
        sink_err: None,
        stop_all: false,
    };

    for bi in 0..archive.blocks.len() {
        if run.stop_all {
            break;
        }
        let Some(&start) = archive.stream_map.block_first_file_index.get(bi) else {
            continue;
        };
        let count = BlockDecoder::new(1, bi, &archive, &password, &mut src).entry_count();
        let end = start.saturating_add(count).min(cap);
        if start >= end {
            continue;
        }
        let wants = |i: usize| req.wants(i as u64);
        if req.op.is_read() && !(start..end).any(wants) {
            continue; // nothing in this block is asked for: not decoded at all
        }
        if is_encrypted(&archive, Some(bi)) && req.password.is_none() {
            for i in (start..end).filter(|i| wants(*i)) {
                run.finish(i, EntryOutcome::PasswordRequired, out)?;
            }
            continue;
        }
        if let Some(need) = oversized_coder(&archive, bi) {
            for i in (start..end).filter(|i| wants(*i)) {
                run.finish(
                    i,
                    EntryOutcome::Unsupported(format!(
                        "the block needs {} MiB of memory for its decoder, over the {} MiB limit",
                        need >> 20,
                        MAX_CODER_MEMORY >> 20
                    )),
                    out,
                )?;
            }
            continue;
        }
        let mut k = 0usize;
        let result = BlockDecoder::new(1, bi, &archive, &password, &mut src).for_each_entries(
            &mut |_entry: &ArchiveEntry, reader: &mut dyn Read| {
                let i = start + k;
                k += 1;
                if i >= end {
                    return Ok(true);
                }
                let want = wants(i);
                let step = run.stream_one(i, want, reader, out);
                match step {
                    Err(e) => {
                        run.sink_err = Some(e);
                        Ok(false)
                    }
                    Ok((outcome, usable)) => {
                        if want {
                            if let Err(e) = run.finish(i, outcome, out) {
                                run.sink_err = Some(e);
                                return Ok(false);
                            }
                        }
                        Ok(usable && !run.stop_all)
                    }
                }
            },
        );
        if let Some(e) = run.sink_err.take() {
            return Err(EngineError::Sink(e));
        }
        // Whatever of the block was not reached: the stream stopped inside it.
        let why = match &result {
            Err(e) => Some(truncate_text(format!(
                "the block could not be decoded further: {e}"
            ))),
            Ok(false) if !run.stop_all => Some("the block was not decoded further".to_string()),
            _ => None,
        };
        let encrypted = is_encrypted(&archive, Some(bi));
        let missing: Vec<usize> = (start..end)
            .filter(|i| wants(*i) && run.outcomes[*i].is_none())
            .collect();
        for i in missing {
            let outcome = match (&why, run.stop_all) {
                (_, true) => EntryOutcome::NotRun,
                (Some(w), _) => {
                    let mut w = w.clone();
                    if encrypted {
                        w.push_str(" (damaged data, or a wrong password)");
                    }
                    EntryOutcome::ReadError(w)
                }
                (None, _) => EntryOutcome::ReadError("the entry was not reached".to_string()),
            };
            run.finish(i, outcome, out)?;
        }
        if why.is_some() {
            tally.complete = false;
            tally.stop_reason.get_or_insert_with(|| {
                "a block could not be decoded; its remaining entries were not read".to_string()
            });
        }
    }

    // Entries with no data stream (directories, empty files, anti-items).
    for i in 0..cap {
        if run.outcomes[i].is_some() || !req.wants(i as u64) {
            continue;
        }
        let block = archive
            .stream_map
            .file_block_index
            .get(i)
            .copied()
            .flatten();
        let f = &archive.files[i];
        let outcome = if block.is_none() && f.has_stream && f.size() > 0 {
            EntryOutcome::ReadError("the entry has data but belongs to no block".to_string())
        } else if block.is_none() {
            EntryOutcome::NoData
        } else if run.stop_all {
            EntryOutcome::NotRun
        } else {
            continue;
        };
        run.finish(i, outcome, out)?;
    }

    if run.stop_all {
        tally.complete = false;
        tally.stop_reason =
            Some("a whole-operation limit was reached; remaining entries were not run".to_string());
    }
    if req.op == Op::Test {
        for i in 0..cap {
            out(Message::Entry(run.infos[i].clone()))?;
            out(Message::Result {
                index: i as u64,
                outcome: run.outcomes[i].clone().unwrap_or(EntryOutcome::NotRun),
            })?;
        }
        tally.entries = cap as u64;
    } else {
        tally.entries = run.outcomes.iter().filter(|o| o.is_some()).count() as u64;
    }
    tally.bytes_out = run.bytes_out;
    out(Message::StreamCheck(StreamCheck::NotApplicable))?;
    Ok(tally)
}
