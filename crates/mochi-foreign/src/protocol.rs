//! Helper protocol, version 2 (v1 plus 7z and RAR; see the crate README).
//! Documented in the crate README.
//!
//! A frame is `len: u32 LE` (counts `kind` and body, 1..=MAX_FRAME_LEN), then
//! `kind: u8`, then the body. Integers are little-endian; a byte string is
//! `u32 len` + bytes; an option is a `u8` 0/1 flag then the value. Decoding is
//! strict: unknown kinds or codes, short bodies, and trailing bytes are all
//! protocol errors. Both sides treat the other as untrusted.

use std::io::{self, Read, Write};

use crate::types::*;

pub const PROTOCOL_VERSION: u16 = 2;
/// Entries a `ReadMany` request may name.
pub const MAX_READ_INDICES: usize = 1_000_000;
/// Largest frame either side accepts (kind + body).
pub const MAX_FRAME_LEN: usize = 1 << 20;
/// Entry bytes travel in frames of at most this many bytes.
pub const DATA_CHUNK: usize = 64 * 1024;
/// Longest error text carried.
pub const MAX_MESSAGE_TEXT: usize = 4096;

pub mod kind {
    // Parent -> helper.
    pub const REQUEST: u8 = 0x01;
    // Helper -> parent.
    pub const HELLO: u8 = 0x80;
    pub const FORMAT: u8 = 0x81;
    pub const ENTRY: u8 = 0x82;
    pub const RESULT: u8 = 0x83;
    pub const DATA: u8 = 0x84;
    pub const STREAM_CHECK: u8 = 0x85;
    pub const BREACH: u8 = 0x86;
    pub const DONE: u8 = 0x87;
    pub const ERROR: u8 = 0x8F;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolError(pub &'static str);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protocol error: {}", self.0)
    }
}

impl std::error::Error for ProtocolError {}

// ---- framing ----------------------------------------------------------------

pub fn write_frame<W: Write>(w: &mut W, kind: u8, body: &[u8]) -> io::Result<()> {
    let len = body.len() + 1;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(body)
}

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Protocol(ProtocolError),
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

/// Reads one frame. `Ok(None)` is a clean end of stream at a frame boundary.
/// The length is validated before anything is allocated.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>, FrameError> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_buf[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => {
                return Err(FrameError::Protocol(ProtocolError(
                    "truncated frame length",
                )))
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(FrameError::Protocol(ProtocolError(
            "frame length out of range",
        )));
    }
    let mut kind_buf = [0u8; 1];
    read_exact_proto(r, &mut kind_buf)?;
    let mut body = vec![0u8; len - 1];
    read_exact_proto(r, &mut body)?;
    Ok(Some((kind_buf[0], body)))
}

fn read_exact_proto<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), FrameError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            Err(FrameError::Protocol(ProtocolError("truncated frame")))
        }
        Err(e) => Err(e.into()),
    }
}

// ---- primitive codec --------------------------------------------------------

#[derive(Default)]
pub struct Enc(Vec<u8>);

impl Enc {
    pub fn new() -> Self {
        Enc(Vec::new())
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    pub fn text(&mut self, v: &str) -> &mut Self {
        let b = v.as_bytes();
        let b = &b[..floor_char_boundary(v, MAX_MESSAGE_TEXT.min(b.len()))];
        self.bytes(b)
    }
    pub fn opt_u64(&mut self, v: Option<u64>) -> &mut Self {
        match v {
            Some(x) => self.u8(1).u64(x),
            None => self.u8(0),
        }
    }
    pub fn opt_bytes(&mut self, v: Option<&[u8]>) -> &mut Self {
        match v {
            Some(x) => self.u8(1).bytes(x),
            None => self.u8(0),
        }
    }
    pub fn finish(self) -> Vec<u8> {
        self.0
    }
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.0.len() < n {
            return Err(ProtocolError("short body"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32, ProtocolError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64, ProtocolError> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    pub fn i64(&mut self) -> Result<i64, ProtocolError> {
        Ok(self.u64()? as i64)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], ProtocolError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn text(&mut self) -> Result<String, ProtocolError> {
        let b = self.bytes()?;
        if b.len() > MAX_MESSAGE_TEXT {
            return Err(ProtocolError("text too long"));
        }
        Ok(String::from_utf8_lossy(b).into_owned())
    }
    pub fn flag(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtocolError("bad flag")),
        }
    }
    pub fn opt_u64(&mut self) -> Result<Option<u64>, ProtocolError> {
        if self.flag()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }
    pub fn opt_bytes(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.flag()? {
            Ok(Some(self.bytes()?.to_vec()))
        } else {
            Ok(None)
        }
    }
    pub fn remaining(&self) -> usize {
        self.0.len()
    }
    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError("trailing bytes"))
        }
    }
}

// ---- request ----------------------------------------------------------------

/// What the parent asks of the helper. The helper opens `archive_path`
/// read-only itself; entry bytes come back over stdout, nothing is written to
/// disk.
#[derive(Debug, Clone)]
pub struct Request {
    pub op: Op,
    /// `Read` only: the entry index to stream.
    pub read_index: u64,
    /// `ReadMany` only: the entry indices to stream (any order; each is
    /// delivered once, in the order the archive stores them).
    pub read_indices: Vec<u64>,
    pub limits: Limits,
    /// Address-space limit the helper applies to itself (Unix `RLIMIT_AS`).
    pub memory_limit: Option<u64>,
    /// CPU-seconds the helper applies to itself (Unix `RLIMIT_CPU`), a backstop
    /// behind the parent's wall-clock kill.
    pub cpu_seconds: Option<u64>,
    /// Platform path bytes: raw bytes on Unix, UTF-16LE code units on Windows.
    pub archive_path: Vec<u8>,
    pub password: Option<Password>,
}

impl Request {
    /// Whether the operation handles entry `index`: every entry for a listing
    /// or test, the named ones for a read.
    pub fn wants(&self, index: u64) -> bool {
        match self.op {
            Op::List | Op::Test => true,
            Op::Read => index == self.read_index,
            Op::ReadMany => self.read_indices.binary_search(&index).is_ok(),
        }
    }

    /// How many entries a read must find (`None` for a listing or test).
    pub fn wanted_count(&self) -> Option<u64> {
        match self.op {
            Op::List | Op::Test => None,
            Op::Read => Some(1),
            Op::ReadMany => Some(self.read_indices.len() as u64),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.u16(PROTOCOL_VERSION)
            .u8(self.op.code())
            .u64(self.read_index);
        e.u32(self.read_indices.len() as u32);
        for i in &self.read_indices {
            e.u64(*i);
        }
        let l = &self.limits;
        e.u64(l.max_total_output)
            .u64(l.max_entry_size)
            .u64(l.max_entries)
            .u64(l.max_ratio)
            .u64(l.ratio_min_output)
            .u32(l.max_name_len);
        e.opt_u64(self.memory_limit).opt_u64(self.cpu_seconds);
        e.bytes(&self.archive_path);
        e.opt_bytes(self.password.as_ref().map(Password::as_bytes));
        e.finish()
    }

    pub fn decode(body: &[u8]) -> Result<Request, ProtocolError> {
        let mut d = Dec::new(body);
        if d.u16()? != PROTOCOL_VERSION {
            return Err(ProtocolError("unsupported protocol version"));
        }
        let op = Op::from_code(d.u8()?).ok_or(ProtocolError("unknown operation"))?;
        let read_index = d.u64()?;
        let n = d.u32()? as usize;
        if n > MAX_READ_INDICES || n.saturating_mul(8) > d.remaining() {
            return Err(ProtocolError("too many read indices"));
        }
        let mut read_indices = Vec::with_capacity(n);
        for _ in 0..n {
            read_indices.push(d.u64()?);
        }
        // Sorted and unique, so membership is a binary search and a duplicate
        // cannot make the helper deliver an entry twice.
        read_indices.sort_unstable();
        read_indices.dedup();
        let limits = Limits {
            max_total_output: d.u64()?,
            max_entry_size: d.u64()?,
            max_entries: d.u64()?,
            max_ratio: d.u64()?,
            ratio_min_output: d.u64()?,
            max_name_len: d.u32()?,
        };
        let memory_limit = d.opt_u64()?;
        let cpu_seconds = d.opt_u64()?;
        let archive_path = d.bytes()?.to_vec();
        let password = d.opt_bytes()?.map(Password::new);
        d.finish()?;
        Ok(Request {
            op,
            read_index,
            read_indices,
            limits,
            memory_limit,
            cpu_seconds,
            archive_path,
            password,
        })
    }
}

// ---- helper -> parent messages ---------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// False when the operation stopped early (limit, truncation).
    pub complete: bool,
    pub entries: u64,
    pub bytes_out: u64,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello { version: u16, id: String },
    Format(ForeignFormat),
    Entry(EntryInfo),
    Result { index: u64, outcome: EntryOutcome },
    Data(Vec<u8>),
    StreamCheck(StreamCheck),
    Breach(Breach),
    Done(Done),
    Error { code: ErrorCode, message: String },
}

fn enc_kind_entry(e: &mut Enc, i: &EntryInfo) {
    e.u64(i.index).bytes(&i.name).u8(i.kind.code());
    e.opt_u64(i.size).opt_u64(i.compressed_size);
    match i.encryption {
        Encryption::None => e.u8(0).u16(0),
        Encryption::ZipCryptoWeak => e.u8(1).u16(0),
        Encryption::Aes(b) => e.u8(2).u16(b),
    };
    e.text(&i.method);
    e.opt_bytes(i.link_target.as_deref());
    e.opt_u64(i.unix_mode.map(u64::from));
    match i.mtime_unix {
        Some(t) => e.u8(1).i64(t),
        None => e.u8(0),
    };
    match i.name_is_utf8_flag {
        None => e.u8(0),
        Some(false) => e.u8(1),
        Some(true) => e.u8(2),
    };
    e.opt_bytes(i.error.as_deref().map(str::as_bytes));
}

fn dec_entry(d: &mut Dec<'_>) -> Result<EntryInfo, ProtocolError> {
    let index = d.u64()?;
    let name = d.bytes()?.to_vec();
    let kind = EntryKind::from_code(d.u8()?).ok_or(ProtocolError("unknown entry kind"))?;
    let size = d.opt_u64()?;
    let compressed_size = d.opt_u64()?;
    let enc_tag = d.u8()?;
    let enc_bits = d.u16()?;
    let encryption = match (enc_tag, enc_bits) {
        (0, 0) => Encryption::None,
        (1, 0) => Encryption::ZipCryptoWeak,
        (2, 128 | 192 | 256) => Encryption::Aes(enc_bits),
        _ => return Err(ProtocolError("bad encryption")),
    };
    let method = d.text()?;
    let link_target = d.opt_bytes()?;
    let unix_mode = match d.opt_u64()? {
        None => None,
        Some(v) => Some(u32::try_from(v).map_err(|_| ProtocolError("mode out of range"))?),
    };
    let mtime_unix = if d.flag()? { Some(d.i64()?) } else { None };
    let name_is_utf8_flag = match d.u8()? {
        0 => None,
        1 => Some(false),
        2 => Some(true),
        _ => return Err(ProtocolError("bad utf8 flag")),
    };
    let error = d
        .opt_bytes()?
        .map(|b| String::from_utf8_lossy(&b[..b.len().min(MAX_MESSAGE_TEXT)]).into_owned());
    Ok(EntryInfo {
        index,
        name,
        kind,
        size,
        compressed_size,
        encryption,
        method,
        link_target,
        unix_mode,
        mtime_unix,
        name_is_utf8_flag,
        error,
    })
}

fn enc_outcome(e: &mut Enc, o: &EntryOutcome) {
    match o {
        EntryOutcome::Matched(s) => e.u8(1).u8(s.0),
        EntryOutcome::Mismatch {
            failed,
            wrong_password_possible,
        } => e.u8(2).u8(failed.0).u8(u8::from(*wrong_password_possible)),
        EntryOutcome::NoEntryCheck => e.u8(3),
        EntryOutcome::NoData => e.u8(4),
        EntryOutcome::PasswordRequired => e.u8(5),
        EntryOutcome::WrongPassword => e.u8(6),
        EntryOutcome::Unsupported(t) => e.u8(7).text(t),
        EntryOutcome::Breach(k) => e.u8(8).u8(k.code()),
        EntryOutcome::ReadError(t) => e.u8(9).text(t),
        EntryOutcome::NotRun => e.u8(10),
    };
}

fn check_set(v: u8) -> Result<CheckSet, ProtocolError> {
    if v == 0 || v > 31 {
        return Err(ProtocolError("bad check set"));
    }
    Ok(CheckSet(v))
}

fn dec_outcome(d: &mut Dec<'_>) -> Result<EntryOutcome, ProtocolError> {
    Ok(match d.u8()? {
        1 => EntryOutcome::Matched(check_set(d.u8()?)?),
        2 => EntryOutcome::Mismatch {
            failed: check_set(d.u8()?)?,
            wrong_password_possible: d.flag()?,
        },
        3 => EntryOutcome::NoEntryCheck,
        4 => EntryOutcome::NoData,
        5 => EntryOutcome::PasswordRequired,
        6 => EntryOutcome::WrongPassword,
        7 => EntryOutcome::Unsupported(d.text()?),
        8 => EntryOutcome::Breach(
            BreachKind::from_code(d.u8()?).ok_or(ProtocolError("unknown breach"))?,
        ),
        9 => EntryOutcome::ReadError(d.text()?),
        10 => EntryOutcome::NotRun,
        _ => return Err(ProtocolError("unknown outcome")),
    })
}

impl Message {
    pub fn encode(&self) -> (u8, Vec<u8>) {
        let mut e = Enc::new();
        let k = match self {
            Message::Hello { version, id } => {
                e.u16(*version).text(id);
                kind::HELLO
            }
            Message::Format(f) => {
                e.u8(f.code());
                kind::FORMAT
            }
            Message::Entry(i) => {
                enc_kind_entry(&mut e, i);
                kind::ENTRY
            }
            Message::Result { index, outcome } => {
                e.u64(*index);
                enc_outcome(&mut e, outcome);
                kind::RESULT
            }
            Message::Data(b) => return (kind::DATA, b.clone()),
            Message::StreamCheck(s) => {
                match s {
                    StreamCheck::NotApplicable => e.u8(0),
                    StreamCheck::Matched(c) => e.u8(1).u8(c.0),
                    StreamCheck::Failed(t) => e.u8(2).text(t),
                    StreamCheck::NotReached => e.u8(3),
                };
                kind::STREAM_CHECK
            }
            Message::Breach(b) => {
                e.u8(b.kind.code())
                    .opt_u64(b.entry)
                    .u64(b.limit)
                    .u64(b.observed);
                kind::BREACH
            }
            Message::Done(d) => {
                e.u8(u8::from(d.complete)).u64(d.entries).u64(d.bytes_out);
                e.opt_bytes(d.stop_reason.as_deref().map(str::as_bytes));
                kind::DONE
            }
            Message::Error { code, message } => {
                e.u8(code.code()).text(message);
                kind::ERROR
            }
        };
        (k, e.finish())
    }

    pub fn decode(kind_byte: u8, body: &[u8]) -> Result<Message, ProtocolError> {
        if kind_byte == kind::DATA {
            return Ok(Message::Data(body.to_vec()));
        }
        let mut d = Dec::new(body);
        let m = match kind_byte {
            kind::HELLO => Message::Hello {
                version: d.u16()?,
                id: d.text()?,
            },
            kind::FORMAT => Message::Format(
                ForeignFormat::from_code(d.u8()?).ok_or(ProtocolError("unknown format"))?,
            ),
            kind::ENTRY => Message::Entry(dec_entry(&mut d)?),
            kind::RESULT => Message::Result {
                index: d.u64()?,
                outcome: dec_outcome(&mut d)?,
            },
            kind::STREAM_CHECK => Message::StreamCheck(match d.u8()? {
                0 => StreamCheck::NotApplicable,
                1 => StreamCheck::Matched(check_set(d.u8()?)?),
                2 => StreamCheck::Failed(d.text()?),
                3 => StreamCheck::NotReached,
                _ => return Err(ProtocolError("unknown stream check")),
            }),
            kind::BREACH => Message::Breach(Breach {
                kind: BreachKind::from_code(d.u8()?).ok_or(ProtocolError("unknown breach"))?,
                entry: d.opt_u64()?,
                limit: d.u64()?,
                observed: d.u64()?,
            }),
            kind::DONE => {
                let complete = d.flag()?;
                let entries = d.u64()?;
                let bytes_out = d.u64()?;
                let stop_reason = d.opt_bytes()?.map(|b| {
                    String::from_utf8_lossy(&b[..b.len().min(MAX_MESSAGE_TEXT)]).into_owned()
                });
                Message::Done(Done {
                    complete,
                    entries,
                    bytes_out,
                    stop_reason,
                })
            }
            kind::ERROR => Message::Error {
                code: ErrorCode::from_wire(d.u8()?).ok_or(ProtocolError("unknown error code"))?,
                message: d.text()?,
            },
            _ => return Err(ProtocolError("unknown message kind")),
        };
        d.finish()?;
        Ok(m)
    }

    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let (k, body) = self.encode();
        write_frame(w, k, &body)
    }
}

// ---- platform path bytes ----------------------------------------------------

#[cfg(unix)]
pub fn path_to_bytes(p: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
pub fn path_from_bytes(b: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b))
}

#[cfg(windows)]
pub fn path_to_bytes(p: &std::path::Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
pub fn path_from_bytes(b: &[u8]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    std::path::PathBuf::from(std::ffi::OsString::from_wide(&units))
}
