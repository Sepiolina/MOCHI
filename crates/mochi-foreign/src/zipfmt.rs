//! ZIP listing, testing and reading on the `zip` crate. Entry bytes only ever
//! go to the message sink; no extract-to-disk function of the library is used.

use std::io::{self, Read, Seek};

use zip::read::HasZipMetadata;
use zip::result::ZipError;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipReadOptions};

use crate::detect::declared_zip_entries;
use crate::engine::{emit_breach, ratio_exceeded, truncate_text, EngineError, Out, Tally};
use crate::protocol::{Message, Request, DATA_CHUNK};
use crate::types::*;

fn malformed(e: impl std::fmt::Display) -> EngineError {
    EngineError::Archive(ForeignError::new(
        ErrorCode::MalformedArchive,
        truncate_text(format!("cannot read ZIP structure: {e}")),
    ))
}

fn method_label(m: CompressionMethod) -> String {
    match m {
        CompressionMethod::Stored => "stored".to_string(),
        CompressionMethod::Deflated => "deflate".to_string(),
        CompressionMethod::Deflate64 => "deflate64".to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Reads an entry's header fields without decrypting or decompressing.
fn read_info<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    i: usize,
    limits: &Limits,
) -> (EntryInfo, Option<u64>) {
    let index = i as u64;
    let mut long_name = None;
    let info = match archive.by_index_raw(i) {
        Err(e) => EntryInfo {
            index,
            name: Vec::new(),
            kind: EntryKind::Other,
            size: None,
            compressed_size: None,
            encryption: Encryption::None,
            method: String::new(),
            link_target: None,
            unix_mode: None,
            mtime_unix: None,
            name_is_utf8_flag: None,
            error: Some(truncate_text(format!("entry header unreadable: {e}"))),
        },
        Ok(f) => {
            let meta = f.get_metadata();
            let aes = meta.aes_settings();
            let encryption = if meta.is_encrypted() {
                match aes {
                    Some((AesMode::Aes128, _)) => Encryption::Aes(128),
                    Some((AesMode::Aes192, _)) => Encryption::Aes(192),
                    Some((AesMode::Aes256, _)) => Encryption::Aes(256),
                    None => Encryption::ZipCryptoWeak,
                }
            } else {
                Encryption::None
            };
            // For an AES entry the metadata carries the inner method from the
            // AE-x extra field (zip 9), not method 99.
            let method = match aes {
                Some(_) => format!("aes+{}", method_label(f.compression())),
                None => method_label(f.compression()),
            };
            let kind = if f.is_dir() {
                EntryKind::Directory
            } else if f.is_symlink() {
                EntryKind::Symlink
            } else {
                EntryKind::File
            };
            let raw = f.name_raw();
            let (name, error) = if raw.len() as u64 > u64::from(limits.max_name_len) {
                long_name = Some(raw.len() as u64);
                (
                    Vec::new(),
                    Some(format!(
                        "entry name is {} bytes, over the {}-byte limit; not carried",
                        raw.len(),
                        limits.max_name_len
                    )),
                )
            } else {
                (raw.to_vec(), None)
            };
            EntryInfo {
                index,
                name,
                kind,
                size: Some(f.size()),
                compressed_size: Some(f.compressed_size()),
                encryption,
                method,
                link_target: None,
                unix_mode: f.unix_mode(),
                mtime_unix: None,
                name_is_utf8_flag: Some(meta.flags.as_u16() & (1 << 11) != 0),
                error,
            }
        }
    };
    (info, long_name)
}

/// What streaming one entry produced.
struct Streamed {
    outcome: EntryOutcome,
    /// The whole operation must stop (total output or archive-wide ratio).
    stop_all: bool,
}

struct Ctx<'a> {
    limits: Limits,
    password: Option<&'a [u8]>,
    archive_len: u64,
    total_out: u64,
    /// Read: deliver entry bytes as `Data`. Test: count and drop.
    deliver: bool,
    buf: Vec<u8>,
}

fn stream_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    info: &EntryInfo,
    ctx: &mut Ctx<'_>,
    out: &mut Out,
) -> Result<Streamed, EngineError> {
    let done = |outcome| {
        Ok(Streamed {
            outcome,
            stop_all: false,
        })
    };
    let index = info.index;
    if let Some(e) = &info.error {
        return done(EntryOutcome::ReadError(e.clone()));
    }
    if info.kind == EntryKind::Directory {
        return done(EntryOutcome::NoData);
    }
    let l = ctx.limits;
    let declared = info.size.unwrap_or(0);
    if declared > l.max_entry_size {
        emit_breach(
            out,
            BreachKind::EntrySize,
            Some(index),
            l.max_entry_size,
            declared,
        )?;
        return done(EntryOutcome::Breach(BreachKind::EntrySize));
    }
    if ratio_exceeded(&l, declared, info.compressed_size.unwrap_or(0)) {
        emit_breach(
            out,
            BreachKind::Ratio,
            Some(index),
            l.max_ratio,
            declared / info.compressed_size.unwrap_or(0).max(1),
        )?;
        return done(EntryOutcome::Breach(BreachKind::Ratio));
    }
    if info.encryption != Encryption::None && ctx.password.is_none() {
        return done(EntryOutcome::PasswordRequired);
    }

    let opts = ZipReadOptions::new().password(ctx.password);
    let mut file = match archive.by_index_with_options(i_usize(index), opts) {
        Ok(f) => f,
        Err(ZipError::InvalidPassword) => return done(EntryOutcome::WrongPassword),
        Err(ZipError::UnsupportedArchive(m)) if m == ZipError::PASSWORD_REQUIRED => {
            return done(EntryOutcome::PasswordRequired)
        }
        Err(ZipError::UnsupportedArchive(m)) => {
            return done(EntryOutcome::Unsupported(m.to_string()))
        }
        Err(ZipError::CompressionMethodNotSupported(c)) => {
            return done(EntryOutcome::Unsupported(format!(
                "compression method {c} is not supported by this build"
            )))
        }
        Err(e) => return done(EntryOutcome::ReadError(truncate_text(e.to_string()))),
    };
    let ae2 = is_ae2(&file.get_metadata().aes_settings());
    let compressed = info.compressed_size.unwrap_or(0);
    let encrypted = info.encryption != Encryption::None;
    // Encrypted entries are held until their authentication passes, so a wrong
    // password never delivers a byte (plan D8 rule 7). Bounded by
    // max_entry_size, which is enforced below before every append.
    let mut held: Vec<u8> = Vec::new();
    let mut entry_out: u64 = 0;
    loop {
        let n = match file.read(&mut ctx.buf) {
            Ok(0) => break,
            Ok(n) => n as u64,
            Err(e) => return done(classify_read_error(&e, info)),
        };
        entry_out += n;
        ctx.total_out += n;
        if entry_out > l.max_entry_size {
            emit_breach(
                out,
                BreachKind::EntrySize,
                Some(index),
                l.max_entry_size,
                entry_out,
            )?;
            return done(EntryOutcome::Breach(BreachKind::EntrySize));
        }
        if ctx.total_out > l.max_total_output {
            emit_breach(
                out,
                BreachKind::TotalOutput,
                Some(index),
                l.max_total_output,
                ctx.total_out,
            )?;
            return Ok(Streamed {
                outcome: EntryOutcome::Breach(BreachKind::TotalOutput),
                stop_all: true,
            });
        }
        if ratio_exceeded(&l, entry_out, compressed) {
            emit_breach(
                out,
                BreachKind::Ratio,
                Some(index),
                l.max_ratio,
                entry_out / compressed.max(1),
            )?;
            return done(EntryOutcome::Breach(BreachKind::Ratio));
        }
        if ratio_exceeded(&l, ctx.total_out, ctx.archive_len) {
            emit_breach(
                out,
                BreachKind::Ratio,
                Some(index),
                l.max_ratio,
                ctx.total_out / ctx.archive_len.max(1),
            )?;
            return Ok(Streamed {
                outcome: EntryOutcome::Breach(BreachKind::Ratio),
                stop_all: true,
            });
        }
        if ctx.deliver {
            let chunk = &ctx.buf[..n as usize];
            if encrypted {
                held.extend_from_slice(chunk);
            } else {
                out(Message::Data(chunk.to_vec()))?;
            }
        }
    }
    // The reader reached its end without an error: every check the library
    // runs at end of entry passed.
    if ctx.deliver && encrypted {
        for chunk in held.chunks(DATA_CHUNK) {
            out(Message::Data(chunk.to_vec()))?;
        }
    }
    let mut set = Vec::new();
    if matches!(info.encryption, Encryption::Aes(_)) {
        set.push(CheckKind::AesAuthCode);
    }
    if !ae2 {
        set.push(CheckKind::Crc32);
    }
    done(EntryOutcome::Matched(CheckSet::of(&set)))
}

fn i_usize(i: u64) -> usize {
    usize::try_from(i).unwrap_or(usize::MAX)
}

/// AE-2 stores no CRC32 (the authentication code replaces it).
fn is_ae2<V: std::fmt::Debug>(aes: &Option<(AesMode, V)>) -> bool {
    aes.as_ref()
        .is_some_and(|(_, v)| format!("{v:?}").contains("Ae2"))
}

/// The `zip` crate reports a failed check as an `io::Error` with a fixed
/// message; `tests/zip_fixtures.rs` pins both strings against corrupted
/// fixtures so a library update that changes them fails loudly.
fn classify_read_error(e: &io::Error, info: &EntryInfo) -> EntryOutcome {
    let msg = e.to_string();
    let weak = info.encryption == Encryption::ZipCryptoWeak;
    if msg.contains("Invalid checksum") {
        return EntryOutcome::Mismatch {
            failed: CheckSet::of(&[CheckKind::Crc32]),
            wrong_password_possible: weak,
        };
    }
    if msg.contains("Invalid authentication code") {
        return EntryOutcome::Mismatch {
            failed: CheckSet::of(&[CheckKind::AesAuthCode]),
            wrong_password_possible: true,
        };
    }
    EntryOutcome::ReadError(truncate_text(msg))
}

pub fn run<R: Read + Seek>(
    mut src: R,
    archive_len: u64,
    req: &Request,
    out: &mut Out,
) -> Result<Tally, EngineError> {
    let declared = declared_zip_entries(&mut src, archive_len);
    let mut archive = ZipArchive::new(src).map_err(malformed)?;
    let l = req.limits;
    let total = archive.len() as u64;
    let mut tally = Tally {
        complete: true,
        ..Tally::default()
    };
    let indices: Vec<u64> = match req.op {
        Op::Read | Op::ReadMany => {
            let wanted: Vec<u64> = if req.op == Op::Read {
                vec![req.read_index]
            } else {
                req.read_indices.clone()
            };
            if let Some(bad) = wanted.iter().find(|i| **i >= total) {
                return Err(EngineError::Archive(ForeignError::new(
                    ErrorCode::EntryNotFound,
                    format!("entry {bad} does not exist ({total} entries)"),
                )));
            }
            wanted
        }
        _ => (0..total).collect(),
    };
    if let Some(d) = declared.filter(|d| *d > total) {
        // The library keeps one entry per name. Say so rather than present a
        // smaller archive as the whole one (a shadowed entry could differ).
        tally.complete = false;
        tally.stop_reason = Some(format!(
            "the archive declares {d} entries but {total} distinct names; entries that share a name with another entry are not listed individually"
        ));
    }
    let capped = usize::try_from(l.max_entries)
        .unwrap_or(usize::MAX)
        .min(indices.len());
    if capped < indices.len() {
        emit_breach(out, BreachKind::EntryCount, None, l.max_entries, total)?;
        tally.complete = false;
        tally.stop_reason = Some(format!(
            "entry count {total} exceeds the limit {}",
            l.max_entries
        ));
    }
    let mut ctx = Ctx {
        limits: l,
        password: req.password.as_ref().map(|p| p.as_bytes()),
        archive_len,
        total_out: 0,
        deliver: req.op.is_read(),
        buf: vec![0u8; DATA_CHUNK],
    };
    for &index in &indices[..capped] {
        let (info, long_name) = read_info(&mut archive, i_usize(index), &l);
        if let Some(observed) = long_name {
            emit_breach(
                out,
                BreachKind::NameLength,
                Some(index),
                u64::from(l.max_name_len),
                observed,
            )?;
        }
        if req.op != Op::Read {
            out(Message::Entry(info.clone()))?;
        }
        tally.entries += 1;
        if req.op == Op::List {
            continue;
        }
        let r = stream_entry(&mut archive, &info, &mut ctx, out)?;
        out(Message::Result {
            index,
            outcome: r.outcome,
        })?;
        if r.stop_all {
            tally.complete = false;
            tally.stop_reason = Some(
                "a whole-operation limit was reached; remaining entries were not run".to_string(),
            );
            break;
        }
    }
    tally.bytes_out = ctx.total_out;
    out(Message::StreamCheck(StreamCheck::NotApplicable))?;
    Ok(tally)
}
