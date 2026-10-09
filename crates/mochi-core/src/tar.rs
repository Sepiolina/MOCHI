//! POSIX pax TAR framing for the TAR-stream compatibility profile (spec 7.2,
//! Annex B.2.9 D19; plan C10): a deterministic **encoder** for member headers
//! and a bounded, streaming, read-only **parser** that `verify` uses to check
//! a commit's stream.
//!
//! The encoder writes exactly the subset D19 rule 4 names, so that the same
//! put always encodes to the same bytes:
//!
//! * a ustar header (`ustar\0` / `00`, typeflag `0` for a file and `5` for a
//!   directory, empty user and group names, no device fields, no prefix);
//! * a preceding pax extended header (typeflag `x`, name `PaxHeader`) only
//!   when a value does not fit the ustar field: `path`, `mtime`, `uid`,
//!   `gid`, `size`, preceded by `hdrcharset=BINARY` when a value is not
//!   UTF-8.
//!
//! The parser accepts what the encoder writes and nothing else (it is not a
//! general TAR reader; each header it reads must equal the encoding of the
//! member it describes): any other typeflag, a global pax header, an unknown
//! pax keyword, a bad checksum, non-zero padding, or bytes after the two end
//! blocks is a [`ErrorCode::ProfileViolation`]. It is bounded by
//! [`MAX_HEADER_BYTES`] and a member limit, never allocates from a size an
//! archive claims for content, and cannot panic on any input.

use crate::catalog::namespace::EntryKind;
use crate::error::{ErrorCode, MochiError, Result};
use crate::manifest::Attributes;

/// The TAR block size.
pub const BLOCK: usize = 512;
/// The end-of-archive marker: two zero blocks.
pub const END_BLOCKS: [u8; 2 * BLOCK] = [0; 2 * BLOCK];
/// The most bytes of path and pax records one member's header may take
/// (D19 rule 4).
pub const MAX_HEADER_BYTES: usize = 1 << 20;
/// The most members the parser accepts in one stream by default.
pub const DEFAULT_MAX_MEMBERS: u64 = 1 << 24;

/// Largest value of the 12-byte ustar `size` and `mtime` fields.
pub const USTAR_MAX_SIZE: u64 = 0o77_777_777_777;
/// Largest value of the 8-byte ustar `uid` and `gid` fields.
pub const USTAR_MAX_ID: u32 = 0o7_777_777;

const NAME_LEN: usize = 100;
const PAX_NAME: &[u8] = b"PaxHeader";

fn violation(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::ProfileViolation, msg)
}

/// What a member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    File,
    Directory,
}

/// One member's header fields, as the profile writes and reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The archive path's bytes: no trailing slash, even for a directory.
    pub path: Vec<u8>,
    pub kind: MemberKind,
    /// Logical length (0 for a directory).
    pub size: u64,
    /// Permission bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Seconds and nanoseconds since the epoch. "No promised time" is
    /// `(0, 0)`: the two are the same bytes on the wire.
    pub mtime: (i64, u32),
}

/// The member a put of `path` is written as (D19 rule 4), from the version's
/// kind, logical length, and promised attributes: the permission bits
/// (default `0644` for a file and `0755` for a directory), numeric owner
/// (default 0), and modification time (default 0). The writer and `verify`
/// both call this, so they cannot disagree about what a put should look like.
pub fn member_for(path: &[u8], kind: EntryKind, size: u64, attrs: &Attributes) -> Member {
    let (member_kind, default_mode) = match kind {
        EntryKind::File => (MemberKind::File, 0o644),
        EntryKind::Directory => (MemberKind::Directory, 0o755),
    };
    Member {
        path: path.to_vec(),
        kind: member_kind,
        size,
        mode: attrs.posix.map_or(default_mode, |p| p.mode & 0o7777),
        uid: attrs.posix.map_or(0, |p| p.uid),
        gid: attrs.posix.map_or(0, |p| p.gid),
        mtime: attrs.mtime.map_or((0, 0), |t| (t.secs, t.nanos)),
    }
}

/// Zero bytes that follow `size` bytes of content to reach a block boundary.
pub fn padding(size: u64) -> usize {
    // `size % 512` is below 512, so the cast cannot truncate.
    let r = (size % BLOCK as u64) as usize;
    (BLOCK - r) % BLOCK
}

fn put_octal(field: &mut [u8], v: u64) {
    // `width - 1` digits and a NUL, zero padded.
    let width = field.len();
    let s = format!("{:0w$o}", v, w = width - 1);
    field[..width - 1].copy_from_slice(s.as_bytes());
    field[width - 1] = 0;
}

fn ustar_block(
    name: &[u8],
    mode: u32,
    uid: u64,
    gid: u64,
    size: u64,
    mtime: u64,
    typeflag: u8,
) -> [u8; BLOCK] {
    let mut h = [0u8; BLOCK];
    let n = name.len().min(NAME_LEN);
    h[..n].copy_from_slice(&name[..n]);
    put_octal(&mut h[100..108], u64::from(mode));
    put_octal(&mut h[108..116], uid);
    put_octal(&mut h[116..124], gid);
    put_octal(&mut h[124..136], size);
    put_octal(&mut h[136..148], mtime);
    h[148..156].copy_from_slice(b"        ");
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    let s = format!("{sum:06o}");
    h[148..154].copy_from_slice(s.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    h
}

fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    // "<len> <key>=<value>\n", where <len> counts the whole record,
    // including its own digits.
    let body = key.len() + 1 + value.len() + 2; // ' ', '=', '\n' counted below
    let mut len = body + 1;
    loop {
        let total = body + len.to_string().len();
        if total == len {
            break;
        }
        len = total;
    }
    let mut out = format!("{len} {key}=").into_bytes();
    out.extend_from_slice(value);
    out.push(b'\n');
    out
}

fn format_mtime(secs: i64, nanos: u32) -> Vec<u8> {
    let total = i128::from(secs) * 1_000_000_000 + i128::from(nanos);
    let (neg, abs) = (total < 0, total.unsigned_abs());
    let (whole, frac) = (abs / 1_000_000_000, abs % 1_000_000_000);
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push_str(&whole.to_string());
    if frac != 0 {
        let f = format!("{frac:09}");
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    s.into_bytes()
}

/// The header bytes of `m`: an optional pax extended header, then the ustar
/// header, a multiple of 512 bytes. The content and its padding follow.
pub fn encode_header(m: &Member) -> Result<Vec<u8>> {
    if m.path.is_empty() || m.path.contains(&0) {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "a TAR member needs a non-empty path without NUL bytes",
        ));
    }
    if m.kind == MemberKind::Directory && m.size != 0 {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "a directory member has size 0",
        ));
    }
    let mut name = m.path.clone();
    if m.kind == MemberKind::Directory {
        name.push(b'/');
    }
    let mut records: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut binary = false;
    if name.len() > NAME_LEN || !name.iter().all(|b| (0x20..=0x7e).contains(b)) {
        binary |= std::str::from_utf8(&name).is_err();
        records.push(("path", name.clone()));
    }
    let (secs, nanos) = m.mtime;
    let in_range = u64::try_from(secs).is_ok_and(|s| s <= USTAR_MAX_SIZE);
    if nanos != 0 || !in_range {
        records.push(("mtime", format_mtime(secs, nanos)));
    }
    let mtime_field = if in_range { secs as u64 } else { 0 };
    let uid_field = if m.uid > USTAR_MAX_ID {
        records.push(("uid", m.uid.to_string().into_bytes()));
        0
    } else {
        u64::from(m.uid)
    };
    let gid_field = if m.gid > USTAR_MAX_ID {
        records.push(("gid", m.gid.to_string().into_bytes()));
        0
    } else {
        u64::from(m.gid)
    };
    let size_field = if m.size > USTAR_MAX_SIZE {
        records.push(("size", m.size.to_string().into_bytes()));
        0
    } else {
        m.size
    };
    let typeflag = match m.kind {
        MemberKind::File => b'0',
        MemberKind::Directory => b'5',
    };
    let mut out = Vec::new();
    if !records.is_empty() {
        let mut body = Vec::new();
        if binary {
            body.extend(pax_record("hdrcharset", b"BINARY"));
        }
        for (k, v) in &records {
            body.extend(pax_record(k, v));
        }
        if body.len() > MAX_HEADER_BYTES {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "a TAR member's header exceeds the 1 MiB limit",
            ));
        }
        out.extend_from_slice(&ustar_block(
            PAX_NAME,
            0o644,
            0,
            0,
            body.len() as u64,
            0,
            b'x',
        ));
        out.extend_from_slice(&body);
        out.resize(out.len() + padding(body.len() as u64), 0);
    }
    out.extend_from_slice(&ustar_block(
        &name,
        m.mode & 0o7777,
        uid_field,
        gid_field,
        size_field,
        mtime_field,
        typeflag,
    ));
    Ok(out)
}

// ---- parser ----------------------------------------------------------------

/// What [`Parser::feed`] reports.
#[derive(Debug)]
pub enum Event<'a> {
    /// A member's header was read; its content follows.
    Member(&'a Member),
    /// The next bytes of the current member's content.
    Content(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Header,
    PaxBody { left: u64 },
    PaxPad { left: usize },
    Content { left: u64 },
    Pad { left: usize },
    SecondEnd,
    Ended,
}

#[derive(Debug, Default)]
struct Pax {
    path: Option<Vec<u8>>,
    mtime: Option<(i64, u32)>,
    uid: Option<u32>,
    gid: Option<u32>,
    size: Option<u64>,
}

/// A streaming parser of one commit's stream. Feed it the decoded bytes of
/// the commit's data frames in physical order.
#[derive(Debug)]
pub struct Parser {
    state: State,
    block: Vec<u8>,
    pax_body: Vec<u8>,
    /// The pax header's block and body, padded: compared with the encoding
    /// of the member it precedes.
    pax_raw: Vec<u8>,
    pax: Option<Pax>,
    /// Padding owed after the current member's content.
    pad_after: usize,
    members: u64,
    max_members: u64,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_MEMBERS)
    }
}

fn parse_octal(field: &[u8], what: &str) -> Result<u64> {
    let s: &[u8] = {
        let start = field.iter().position(|b| *b != b' ').unwrap_or(field.len());
        let end = field[start..]
            .iter()
            .position(|b| *b == 0 || *b == b' ')
            .map_or(field.len(), |e| start + e);
        &field[start..end]
    };
    if s.is_empty() {
        return Ok(0);
    }
    if s.len() > 22 {
        return Err(violation(format!("the {what} field is too long")));
    }
    let mut v: u64 = 0;
    for b in s {
        if !(b'0'..=b'7').contains(b) {
            return Err(violation(format!("the {what} field is not octal")));
        }
        v = v
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(b - b'0')))
            .ok_or_else(|| violation(format!("the {what} field overflows")))?;
    }
    Ok(v)
}

fn parse_decimal(s: &[u8], what: &str) -> Result<u64> {
    if s.is_empty() || s.len() > 20 || !s.iter().all(u8::is_ascii_digit) {
        return Err(violation(format!("the pax {what} is not a decimal number")));
    }
    std::str::from_utf8(s)
        .ok()
        .and_then(|t| t.parse::<u64>().ok())
        .ok_or_else(|| violation(format!("the pax {what} overflows")))
}

fn parse_pax_mtime(v: &[u8]) -> Result<(i64, u32)> {
    let (neg, rest) = match v.split_first() {
        Some((b'-', r)) => (true, r),
        _ => (false, v),
    };
    let (whole, frac) = match rest.iter().position(|b| *b == b'.') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &rest[..0]),
    };
    if frac.len() > 9 || (rest.contains(&b'.') && frac.is_empty()) {
        return Err(violation("the pax mtime has a bad fraction"));
    }
    let whole = parse_decimal(whole, "mtime")?;
    let mut nanos: u64 = 0;
    if !frac.is_empty() {
        nanos = parse_decimal(frac, "mtime fraction")?;
        for _ in frac.len()..9 {
            nanos *= 10;
        }
    }
    let total = i128::from(whole) * 1_000_000_000 + i128::from(nanos);
    let total = if neg { -total } else { total };
    let secs = i64::try_from(total.div_euclid(1_000_000_000))
        .map_err(|_| violation("the pax mtime is out of range"))?;
    let n = u32::try_from(total.rem_euclid(1_000_000_000))
        .map_err(|_| violation("the pax mtime is out of range"))?;
    Ok((secs, n))
}

fn parse_pax(body: &[u8]) -> Result<Pax> {
    let mut pax = Pax::default();
    let mut binary = false;
    let mut seen_hdrcharset = false;
    let mut rest = body;
    while !rest.is_empty() {
        let sp = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| violation("a pax record has no length"))?;
        let len = usize::try_from(parse_decimal(&rest[..sp], "record length")?)
            .map_err(|_| violation("a pax record length overflows"))?;
        if len <= sp + 1 || len > rest.len() {
            return Err(violation("a pax record length is out of range"));
        }
        let record = &rest[sp + 1..len];
        rest = &rest[len..];
        let Some((&b'\n', record)) = record.split_last() else {
            return Err(violation("a pax record does not end in a newline"));
        };
        let eq = record
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| violation("a pax record has no '='"))?;
        let (key, value) = (&record[..eq], &record[eq + 1..]);
        let dup = |present: bool| {
            if present {
                Err(violation("a pax keyword is repeated"))
            } else {
                Ok(())
            }
        };
        match key {
            b"hdrcharset" => {
                dup(seen_hdrcharset)?;
                seen_hdrcharset = true;
                if value != b"BINARY" {
                    return Err(violation("an unsupported pax hdrcharset"));
                }
                binary = true;
            }
            b"path" => {
                dup(pax.path.is_some())?;
                if value.is_empty() || value.contains(&0) {
                    return Err(violation("a pax path is empty or holds a NUL"));
                }
                pax.path = Some(value.to_vec());
            }
            b"mtime" => {
                dup(pax.mtime.is_some())?;
                pax.mtime = Some(parse_pax_mtime(value)?);
            }
            b"uid" => {
                dup(pax.uid.is_some())?;
                pax.uid = Some(
                    u32::try_from(parse_decimal(value, "uid")?)
                        .map_err(|_| violation("the pax uid is out of range"))?,
                );
            }
            b"gid" => {
                dup(pax.gid.is_some())?;
                pax.gid = Some(
                    u32::try_from(parse_decimal(value, "gid")?)
                        .map_err(|_| violation("the pax gid is out of range"))?,
                );
            }
            b"size" => {
                dup(pax.size.is_some())?;
                pax.size = Some(parse_decimal(value, "size")?);
            }
            _ => return Err(violation("a pax keyword this profile does not write")),
        }
    }
    // `hdrcharset=BINARY` is only meaningful (and only written) when a value
    // is not UTF-8; a UTF-8 path under it is still accepted as bytes.
    let _ = binary;
    Ok(pax)
}

impl Parser {
    pub fn new(max_members: u64) -> Self {
        Parser {
            state: State::Header,
            block: Vec::with_capacity(BLOCK),
            pax_body: Vec::new(),
            pax_raw: Vec::new(),
            pax: None,
            pad_after: 0,
            members: 0,
            max_members,
        }
    }

    /// The stream is complete: both end blocks were read and nothing else.
    pub fn is_complete(&self) -> bool {
        self.state == State::Ended
    }

    /// Bytes of the current member's content still owed, if inside one.
    pub fn content_left(&self) -> Option<u64> {
        match self.state {
            State::Content { left } => Some(left),
            _ => None,
        }
    }

    /// Members read so far.
    pub fn members(&self) -> u64 {
        self.members
    }

    /// Accept the end of the stream: an error unless it is complete.
    pub fn finish(&self) -> Result<()> {
        if self.is_complete() {
            Ok(())
        } else {
            Err(violation(
                "the stream ends before its two end-of-archive blocks",
            ))
        }
    }

    /// Account for `n` bytes of the current member's content without seeing
    /// them (the caller knows them from the catalog). Only valid inside
    /// content with at least `n` bytes left.
    pub fn skip_content(&mut self, n: u64) -> Result<()> {
        match self.state {
            State::Content { left } if n <= left => {
                let left = left - n;
                self.state = if left > 0 {
                    State::Content { left }
                } else {
                    self.after_content()
                };
                Ok(())
            }
            _ => Err(violation(
                "a data frame supplies more content than the member's header declares",
            )),
        }
    }

    fn after_content(&self) -> State {
        if self.pad_after > 0 {
            State::Pad {
                left: self.pad_after,
            }
        } else {
            State::Header
        }
    }

    /// Feed the next decoded bytes. Events are reported in order through
    /// `on`. Any violation stops the parse; do not feed it again.
    pub fn feed(&mut self, mut data: &[u8], on: &mut dyn FnMut(Event<'_>)) -> Result<()> {
        while !data.is_empty() {
            match self.state {
                State::Ended => {
                    return Err(violation(
                        "bytes follow the two end-of-archive blocks of the stream",
                    ))
                }
                State::Header | State::SecondEnd => {
                    let take = (BLOCK - self.block.len()).min(data.len());
                    self.block.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    if self.block.len() == BLOCK {
                        let block = std::mem::take(&mut self.block);
                        self.block = Vec::with_capacity(BLOCK);
                        self.header_block(&block, on)?;
                    }
                }
                State::PaxBody { left } => {
                    let take = usize::try_from(left).unwrap_or(usize::MAX).min(data.len());
                    self.pax_body.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    let left = left - take as u64;
                    if left > 0 {
                        self.state = State::PaxBody { left };
                    } else {
                        let size = self.pax_body.len() as u64;
                        self.pax = Some(parse_pax(&self.pax_body)?);
                        self.pax_raw.extend_from_slice(&self.pax_body);
                        self.pax_raw.resize(self.pax_raw.len() + padding(size), 0);
                        self.pax_body = Vec::new();
                        self.state = State::PaxPad {
                            left: padding(size),
                        };
                        if padding(size) == 0 {
                            self.state = State::Header;
                        }
                    }
                }
                State::PaxPad { left } | State::Pad { left } => {
                    let take = left.min(data.len());
                    if data[..take].iter().any(|b| *b != 0) {
                        return Err(violation("a member's padding is not zero"));
                    }
                    data = &data[take..];
                    let left = left - take;
                    self.state = if left > 0 {
                        match self.state {
                            State::PaxPad { .. } => State::PaxPad { left },
                            _ => State::Pad { left },
                        }
                    } else {
                        State::Header
                    };
                }
                State::Content { left } => {
                    let take = usize::try_from(left).unwrap_or(usize::MAX).min(data.len());
                    on(Event::Content(&data[..take]));
                    data = &data[take..];
                    let left = left - take as u64;
                    self.state = if left > 0 {
                        State::Content { left }
                    } else {
                        self.after_content()
                    };
                }
            }
        }
        Ok(())
    }

    fn header_block(&mut self, b: &[u8], on: &mut dyn FnMut(Event<'_>)) -> Result<()> {
        let zero = b.iter().all(|x| *x == 0);
        if self.state == State::SecondEnd {
            if !zero {
                return Err(violation(
                    "the first end-of-archive block is not followed by a second",
                ));
            }
            self.state = State::Ended;
            return Ok(());
        }
        if zero {
            if self.pax.is_some() {
                return Err(violation(
                    "a pax extended header is not followed by a member",
                ));
            }
            self.state = State::SecondEnd;
            return Ok(());
        }
        // Checksum: the sum of the block with the field read as spaces.
        let stored = parse_octal(&b[148..156], "checksum")?;
        let sum: u64 = b
            .iter()
            .enumerate()
            .map(|(i, x)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u64::from(*x)
                }
            })
            .sum();
        if stored != sum {
            return Err(violation("a header checksum is wrong"));
        }
        if &b[257..263] != b"ustar\0" || &b[263..265] != b"00" {
            return Err(violation("a header is not ustar version 00"));
        }
        let size_field = parse_octal(&b[124..136], "size")?;
        match b[156] {
            b'x' => {
                if self.pax.is_some() {
                    return Err(violation("two pax extended headers in a row"));
                }
                if size_field == 0 || size_field > MAX_HEADER_BYTES as u64 {
                    return Err(violation("a pax extended header has a bad size"));
                }
                self.pax_body = Vec::new();
                self.pax_raw = b.to_vec();
                self.state = State::PaxBody { left: size_field };
                Ok(())
            }
            flag @ (b'0' | b'5') => {
                let pax = self.pax.take().unwrap_or_default();
                let kind = if flag == b'5' {
                    MemberKind::Directory
                } else {
                    MemberKind::File
                };
                let name_end = b[..NAME_LEN]
                    .iter()
                    .position(|x| *x == 0)
                    .unwrap_or(NAME_LEN);
                let mut path = pax.path.unwrap_or_else(|| b[..name_end].to_vec());
                match (kind, path.last()) {
                    (MemberKind::Directory, Some(b'/')) => {
                        path.pop();
                    }
                    (MemberKind::File, Some(b'/')) => {
                        return Err(violation("a file member's path ends in '/'"));
                    }
                    _ => {}
                }
                if path.is_empty() || path.contains(&0) {
                    return Err(violation("a member's path is empty"));
                }
                let size = pax.size.unwrap_or(size_field);
                if kind == MemberKind::Directory && size != 0 {
                    return Err(violation("a directory member has content"));
                }
                let id = |v: u64, what: &str| {
                    u32::try_from(v).map_err(|_| violation(format!("the {what} is out of range")))
                };
                let mode = u32::try_from(parse_octal(&b[100..108], "mode")?)
                    .map_err(|_| violation("the mode is out of range"))?;
                let uid = match pax.uid {
                    Some(v) => v,
                    None => id(parse_octal(&b[108..116], "uid")?, "uid")?,
                };
                let gid = match pax.gid {
                    Some(v) => v,
                    None => id(parse_octal(&b[116..124], "gid")?, "gid")?,
                };
                let mtime = match pax.mtime {
                    Some(t) => t,
                    None => {
                        let s = parse_octal(&b[136..148], "mtime")?;
                        (
                            i64::try_from(s).map_err(|_| violation("the mtime is out of range"))?,
                            0,
                        )
                    }
                };
                self.members += 1;
                if self.members > self.max_members {
                    return Err(violation("the stream has more members than the limit"));
                }
                let member = Member {
                    path,
                    kind,
                    size,
                    mode,
                    uid,
                    gid,
                    mtime,
                };
                // The profile writes one encoding of a member and the parser
                // accepts only that: every field a reader might skip or
                // read two ways (a ustar value a pax record overrides, name
                // bytes after the first NUL, octal spellings) is pinned.
                let mut actual = std::mem::take(&mut self.pax_raw);
                actual.extend_from_slice(b);
                if encode_header(&member)? != actual {
                    return Err(violation(
                        "a member's header is not the encoding this profile writes",
                    ));
                }
                on(Event::Member(&member));
                self.pad_after = padding(size);
                self.state = if size > 0 {
                    State::Content { left: size }
                } else {
                    self.after_content()
                };
                Ok(())
            }
            _ => Err(violation("a typeflag this profile does not write")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(path: &[u8], kind: MemberKind, size: u64) -> Member {
        Member {
            path: path.to_vec(),
            kind,
            size,
            mode: 0o644,
            uid: 1000,
            gid: 100,
            mtime: (1_700_000_000, 0),
        }
    }

    /// A whole stream: members with their content, then the end blocks.
    fn stream(members: &[(Member, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (m, content) in members {
            assert_eq!(m.size, content.len() as u64);
            out.extend(encode_header(m).unwrap());
            out.extend_from_slice(content);
            out.resize(out.len() + padding(m.size), 0);
        }
        out.extend_from_slice(&END_BLOCKS);
        out
    }

    fn parse(bytes: &[u8]) -> Result<Vec<(Member, Vec<u8>)>> {
        let mut p = Parser::default();
        let mut out: Vec<(Member, Vec<u8>)> = Vec::new();
        p.feed(bytes, &mut |e| match e {
            Event::Member(m) => out.push((m.clone(), Vec::new())),
            Event::Content(c) => out.last_mut().unwrap().1.extend_from_slice(c),
        })?;
        p.finish()?;
        Ok(out)
    }

    #[test]
    fn plain_members_round_trip_and_match_the_documented_layout() {
        let f = member(b"d/a.txt", MemberKind::File, 5);
        let h = encode_header(&f).unwrap();
        assert_eq!(h.len(), BLOCK, "no pax header for a plain member");
        assert_eq!(&h[..7], b"d/a.txt");
        assert_eq!(&h[100..108], b"0000644\0");
        assert_eq!(&h[108..116], b"0001750\0"); // 1000
        assert_eq!(&h[116..124], b"0000144\0"); // 100
        assert_eq!(&h[124..136], b"00000000005\0");
        assert_eq!(&h[136..148], b"14524770400\0"); // 1_700_000_000
        assert_eq!(h[156], b'0');
        assert_eq!(&h[257..265], b"ustar\x0000");
        assert!(h[265..345].iter().all(|b| *b == 0), "no names, no devices");
        assert!(h[345..].iter().all(|b| *b == 0), "no prefix");
        // The checksum is six octal digits, a NUL, and a space.
        assert_eq!(h[154], 0);
        assert_eq!(h[155], b' ');
        let sum: u32 = h
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u32::from(*b)
                }
            })
            .sum();
        assert_eq!(
            std::str::from_utf8(&h[148..154]).unwrap(),
            format!("{sum:06o}")
        );

        let d = member(b"d", MemberKind::Directory, 0);
        let dh = encode_header(&d).unwrap();
        assert_eq!(&dh[..2], b"d/");
        assert_eq!(dh[156], b'5');

        let got = parse(&stream(&[
            (d.clone(), vec![]),
            (f.clone(), b"hello".to_vec()),
            (member(b"empty", MemberKind::File, 0), vec![]),
        ]))
        .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, d);
        assert_eq!(got[1], (f, b"hello".to_vec()));
        assert_eq!(got[2].0.size, 0);
    }

    #[test]
    fn values_that_do_not_fit_a_ustar_field_go_in_a_pax_header() {
        // A long path, a non-ASCII path, a non-UTF-8 path, a fractional and
        // an out-of-range time, a big id, a size above 8 GiB - 1.
        let long = vec![b'a'; 300];
        let cases: Vec<Member> = vec![
            member(&long, MemberKind::File, 1),
            member("héllo/wörld".as_bytes(), MemberKind::File, 1),
            member(b"bad\xff\xfename", MemberKind::File, 1),
            Member {
                mtime: (1_700_000_000, 123_456_789),
                ..member(b"t", MemberKind::File, 1)
            },
            Member {
                mtime: (-5, 500_000_000),
                ..member(b"neg", MemberKind::File, 1)
            },
            Member {
                mtime: (USTAR_MAX_SIZE as i64 + 1, 0),
                ..member(b"far", MemberKind::File, 1)
            },
            Member {
                uid: USTAR_MAX_ID + 1,
                gid: u32::MAX,
                ..member(b"id", MemberKind::File, 1)
            },
            Member {
                size: USTAR_MAX_SIZE + 1,
                ..member(b"big", MemberKind::File, USTAR_MAX_SIZE + 1)
            },
            member(&long, MemberKind::Directory, 0),
        ];
        for m in cases {
            let h = encode_header(&m).unwrap();
            assert!(h.len() > BLOCK, "{:?} needs a pax header", m.path.len());
            assert_eq!(h[156], b'x');
            assert_eq!(&h[..9], b"PaxHeader");
            // Parse header-only (skip the huge content by size).
            let mut bytes = h.clone();
            let mut p = Parser::default();
            let mut seen = None;
            p.feed(&bytes, &mut |e| {
                if let Event::Member(x) = e {
                    seen = Some(x.clone());
                }
            })
            .unwrap();
            assert_eq!(seen.as_ref(), Some(&m));
            // Content owed matches the (possibly huge) size, with no allocation.
            assert_eq!(p.content_left().unwrap_or(0), m.size);
            bytes.clear();
        }
    }

    #[test]
    fn a_binary_path_declares_hdrcharset_and_a_utf8_one_does_not() {
        let bin = encode_header(&member(b"a\xffb", MemberKind::File, 0)).unwrap();
        let utf8 = encode_header(&member("é".as_bytes(), MemberKind::File, 0)).unwrap();
        let has = |h: &[u8]| h.windows(10).any(|w| w == b"hdrcharset");
        assert!(has(&bin));
        assert!(!has(&utf8));
    }

    #[test]
    fn pax_record_lengths_count_their_own_digits() {
        // The classic fixed point: 9-byte body gives a 2-digit length.
        for key_len in 0..40usize {
            let key = "k".repeat(key_len);
            let rec = pax_record(&key, b"v");
            let n: usize =
                std::str::from_utf8(&rec[..rec.iter().position(|b| *b == b' ').unwrap()])
                    .unwrap()
                    .parse()
                    .unwrap();
            assert_eq!(n, rec.len(), "{key_len}");
        }
        assert_eq!(pax_record("path", b"x"), b"9 path=x\n");
    }

    #[test]
    fn mtimes_format_and_parse_exactly() {
        for (s, n, text) in [
            (0, 0, "0"),
            (5, 500_000_000, "5.5"),
            (1_700_000_000, 123_456_789, "1700000000.123456789"),
            (-1, 500_000_000, "-0.5"),
            (-5, 0, "-5"),
            (-5, 250_000_000, "-4.75"),
        ] {
            assert_eq!(format_mtime(s, n), text.as_bytes(), "{s}.{n}");
            assert_eq!(parse_pax_mtime(text.as_bytes()).unwrap(), (s, n), "{text}");
        }
        for bad in ["", ".5", "5.", "1.1234567890", "x", "--1", "1e3", "+1"] {
            assert!(parse_pax_mtime(bad.as_bytes()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_encoder_refuses_what_it_cannot_write() {
        for m in [
            member(b"", MemberKind::File, 0),
            member(b"a\0b", MemberKind::File, 0),
            member(b"d", MemberKind::Directory, 3),
            member(&vec![b'x'; MAX_HEADER_BYTES + 1], MemberKind::File, 0),
        ] {
            assert_eq!(
                encode_header(&m).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
        }
    }

    #[test]
    fn the_parser_rejects_every_deviation() {
        let good = stream(&[(member(b"a", MemberKind::File, 3), b"abc".to_vec())]);
        parse(&good).unwrap();
        let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
        // Truncated anywhere inside, and the end blocks missing.
        for cut in [0, 1, 100, 511, 512, 513, 1024, good.len() - 1] {
            cases.push(("truncated", good[..cut].to_vec()));
        }
        // Bytes after the end.
        let mut v = good.clone();
        v.push(0);
        cases.push(("trailing zero byte", v));
        let mut v = good.clone();
        v.extend_from_slice(&good);
        cases.push(("a second stream appended", v));
        // One end block only, then a header.
        let mut v = good[..good.len() - 512].to_vec();
        v.extend_from_slice(&good[..512]);
        cases.push(("one end block then a header", v));
        // Flipped bits in the header: checksum, typeflag, magic, version.
        for at in [0, 100, 124, 148, 156, 257, 263] {
            let mut v = good.clone();
            v[at] ^= 0x01;
            cases.push(("flipped header byte", v));
        }
        // Non-zero padding.
        let mut v = good.clone();
        v[512 + 3 + 1] = 1;
        cases.push(("non-zero padding", v));
        // Wrong typeflag with a fixed checksum is still refused.
        let mut v = good.clone();
        v[156] = b'2';
        let sum: u32 = v[..512]
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    u32::from(*b)
                }
            })
            .sum();
        v[148..154].copy_from_slice(format!("{sum:06o}").as_bytes());
        cases.push(("symlink typeflag", v));
        for (what, bytes) in cases {
            let e = parse(&bytes).expect_err(what);
            assert_eq!(e.code, ErrorCode::ProfileViolation, "{what}");
        }
    }

    #[test]
    fn pax_headers_are_checked_too() {
        let h = encode_header(&Member {
            mtime: (1, 5),
            ..member(b"a", MemberKind::File, 0)
        })
        .unwrap();
        let mut ok = h.clone();
        ok.extend_from_slice(&END_BLOCKS);
        parse(&ok).unwrap();
        // A pax header with nothing after it.
        let mut v = h[..h.len() - 512].to_vec();
        v.extend_from_slice(&END_BLOCKS);
        assert!(parse(&v).is_err());
        // Two pax headers in a row.
        let mut v = h[..h.len() - 512].to_vec();
        v.extend_from_slice(&h);
        v.extend_from_slice(&END_BLOCKS);
        assert!(parse(&v).is_err());
        // An unknown keyword, a repeated one, and a bad length.
        for body in [
            &b"10 ctime=1\n"[..],
            b"10 mtime=1\n10 mtime=2\n",
            b"99 mtime=1\n",
            b"5 a=b\n",
            b"10 mtime=1",
        ] {
            assert!(
                parse_pax(body).is_err(),
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn the_parser_never_trusts_a_claimed_size() {
        // A header claiming 2^60 bytes: reported, never allocated.
        let big = Member {
            size: 1 << 60,
            ..member(b"huge", MemberKind::File, 1 << 60)
        };
        let mut p = Parser::default();
        let mut got = None;
        p.feed(&encode_header(&big).unwrap(), &mut |e| {
            if let Event::Member(m) = e {
                got = Some(m.size);
            }
        })
        .unwrap();
        assert_eq!(got, Some(1 << 60));
        assert_eq!(p.content_left(), Some(1 << 60));
        // The caller may skip content it knows from elsewhere; too much is refused.
        p.skip_content(1 << 59).unwrap();
        assert_eq!(p.content_left(), Some(1 << 59));
        assert!(p.skip_content((1 << 59) + 1).is_err());
    }

    #[test]
    fn the_member_limit_holds() {
        let s = stream(&[
            (member(b"a", MemberKind::File, 0), vec![]),
            (member(b"b", MemberKind::File, 0), vec![]),
        ]);
        let mut p = Parser::new(1);
        assert!(p.feed(&s, &mut |_| {}).is_err());
        let mut p = Parser::new(2);
        p.feed(&s, &mut |_| {}).unwrap();
        p.finish().unwrap();
    }

    #[test]
    fn skipping_content_equals_feeding_it() {
        let m = member(b"f", MemberKind::File, 1000);
        let mut bytes = encode_header(&m).unwrap();
        let header_len = bytes.len();
        bytes.extend(vec![7u8; 1000]);
        bytes.resize(bytes.len() + padding(1000), 0);
        bytes.extend_from_slice(&END_BLOCKS);
        let mut p = Parser::default();
        p.feed(&bytes[..header_len], &mut |_| {}).unwrap();
        p.skip_content(1000).unwrap();
        p.feed(&bytes[header_len + 1000..], &mut |_| {}).unwrap();
        p.finish().unwrap();
        assert_eq!(p.members(), 1);
    }

    /// Any byte split of a valid stream parses the same.
    #[test]
    fn splits_do_not_matter() {
        let s = stream(&[
            (
                Member {
                    mtime: (3, 7),
                    ..member(&[b'p'; 150], MemberKind::File, 600)
                },
                vec![9u8; 600],
            ),
            (member(b"d", MemberKind::Directory, 0), vec![]),
        ]);
        let want = parse(&s).unwrap();
        for step in [1usize, 7, 511, 512, 513, 2048] {
            let mut p = Parser::default();
            let mut got: Vec<(Member, Vec<u8>)> = Vec::new();
            for piece in s.chunks(step) {
                p.feed(piece, &mut |e| match e {
                    Event::Member(m) => got.push((m.clone(), Vec::new())),
                    Event::Content(c) => got.last_mut().unwrap().1.extend_from_slice(c),
                })
                .unwrap();
            }
            p.finish().unwrap();
            assert_eq!(got, want, "step {step}");
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        fn any_member() -> impl Strategy<Value = (Member, Vec<u8>)> {
            (
                proptest::collection::vec(1u8..=255, 1..400),
                any::<bool>(),
                0u32..=0o7777,
                any::<u32>(),
                any::<u32>(),
                prop_oneof![
                    Just(0i64),
                    0i64..2_000_000_000,
                    -5_000_000_000i64..5_000_000_000
                ],
                0u32..1_000_000_000,
                proptest::collection::vec(any::<u8>(), 0..1500),
            )
                .prop_map(|(path, dir, mode, uid, gid, secs, nanos, content)| {
                    let content = if dir { Vec::new() } else { content };
                    (
                        Member {
                            path,
                            kind: if dir {
                                MemberKind::Directory
                            } else {
                                MemberKind::File
                            },
                            size: content.len() as u64,
                            mode,
                            uid,
                            gid,
                            mtime: (secs, nanos),
                        },
                        content,
                    )
                })
        }

        proptest! {
            /// Whatever the encoder writes, the parser reads back exactly,
            /// whatever the path bytes, ids, times, and content.
            #[test]
            fn encode_then_parse_is_the_identity(
                members in proptest::collection::vec(any_member(), 0..6)
            ) {
                // A path ending in '/' is not a member path.
                let members: Vec<_> = members
                    .into_iter()
                    .filter(|(m, _)| m.path.last() != Some(&b'/'))
                    .collect();
                let got = parse(&stream(&members)).unwrap();
                prop_assert_eq!(got, members);
            }

            /// No byte string makes the parser panic, and it never accepts
            /// anything that is not exactly a complete stream.
            #[test]
            fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..3000)) {
                let mut p = Parser::default();
                let r = p.feed(&bytes, &mut |_| {});
                if r.is_ok() && p.finish().is_ok() {
                    // The only complete streams are well-formed ones, which
                    // re-encode to the same bytes.
                    prop_assert!(bytes.len() >= 2 * BLOCK);
                }
            }
        }
    }
}
