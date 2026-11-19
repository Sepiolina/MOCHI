//! RAR 4 and RAR 5 listing, testing and reading on the vendored UnRAR source
//! (RARLAB's, **not** MIT/Apache: see `THIRD-PARTY-NOTICES.md`), reached
//! through `unrar_sys`, the FFI crate of the `unrar` project.
//!
//! # Rules this module keeps
//!
//! * **Read-only, always.** MOCHI never writes RAR, and this module never
//!   asks UnRAR to put a byte on disk: every entry is processed with
//!   `RAR_TEST`, which decodes and verifies without extracting, and the bytes
//!   arrive through the data callback (`UCM_PROCESSDATA`). `RAR_EXTRACT` and
//!   the destination arguments of `RARProcessFile` are never used
//!   (`ci/check-invariants.sh` greps for them).
//! * **UnRAR's own path handling is bypassed.** The helper reports the name
//!   the archive stored; the C6 restore engine decides what may be created.
//! * **Streaming, with limits.** The safe `unrar` wrapper buffers a whole
//!   entry and hides the hash type, so this module drives the C API itself.
//!   A limit breach returns `-1` from the callback, which stops UnRAR.
//!
//! # Unsafe
//!
//! The FFI is the only `unsafe` in this crate besides the Windows job object
//! (`jobobject`). Each block has a `SAFETY:` comment. The contract relied on:
//! UnRAR calls the registered callback only on the thread that called into it
//! and only during `RAROpenArchiveEx`, `RARReadHeaderEx` and `RARProcessFile`;
//! the callback's user-data is the address of a heap [`Callback`] that the
//! owning [`Rar`] keeps alive and does not move until it is dropped.
//!
//! # What RAR checks
//!
//! RAR 4: CRC32 per entry. RAR 5: CRC32 or BLAKE2sp per entry, as the entry's
//! header says (`hash_type`); the outcome names which. Neither format has a
//! whole-archive check this module can use. Data-encrypted entries are held
//! in the helper until their check passes (a RAR 4 wrong password has no
//! verifier and shows up as a failed CRC32; RAR 5 stores a password check).

use std::cell::Cell;
#[cfg(unix)]
use std::ffi::CString;
use std::ffi::{c_int, c_uint, c_void};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::rc::Rc;

use unrar_sys as sys;
use zeroize::Zeroizing;

use crate::engine::{emit_breach, ratio_exceeded, truncate_text, EngineError, Origin, Out, Tally};
use crate::protocol::{Message, Request, DATA_CHUNK};
use crate::types::*;

/// Dictionary (KB in the header) one entry may need before it is refused.
const MAX_DICT_KB: u32 = 1 << 20; // 1 GiB
/// The largest encrypted entry held in memory until its check passes.
const MAX_HELD_ENTRY: u64 = 512 << 20;
/// UnRAR's header API truncates names to this many UTF-16/32 units.
const NAME_UNITS: usize = 1024;
/// A volume name UnRAR asks for is at most this many units.
const VOLUME_NAME_UNITS: usize = 2048;

// UnRAR's Windows build reads the registry (RegOpenKeyExW) and the system
// random source (CryptGenRandom), but unrar_sys links neither library, which
// fails the link of the helper binary on MSVC. An empty extern block with a
// link attribute adds the import library and declares no symbol.
#[cfg(windows)]
#[link(name = "advapi32")]
extern "C" {}

/// UnRAR decodes even a skipped entry of a solid archive to keep its window,
/// which allocates the entry's dictionary. An entry whose dictionary is over
/// the limit is therefore never handed to UnRAR, and nothing after it is
/// reachable.
fn dict_blocked(h: &Header) -> bool {
    !h.is_dir() && h.redir_type == 0 && h.dict_kb > MAX_DICT_KB
}

fn dict_stop_text(h: &Header) -> String {
    format!(
        "stopped at an entry that needs a {} MiB dictionary, over the {} MiB limit",
        h.dict_kb >> 10,
        MAX_DICT_KB >> 10
    )
}

const HOST_UNIX: c_uint = 3;
const FSREDIR_UNIXSYMLINK: c_uint = 1;
const FSREDIR_WINSYMLINK: c_uint = 2;
const FSREDIR_JUNCTION: c_uint = 3;
const FSREDIR_HARDLINK: c_uint = 4;
const FSREDIR_FILECOPY: c_uint = 5;
/// 100 ns ticks per second, and seconds from 1601-01-01 to 1970-01-01.
const TICKS: u64 = 10_000_000;
const EPOCH_DIFF_SECS: u64 = 11_644_473_600;

fn archive_err(code: ErrorCode, msg: impl Into<String>) -> EngineError {
    EngineError::Archive(ForeignError::new(code, truncate_text(msg.into())))
}

// ---- names ------------------------------------------------------------------

/// Appends one code point as UTF-8, or as WTF-8 when it is a lone surrogate
/// (so a name that is not valid Unicode keeps its identity). Values above
/// U+10FFFF cannot be encoded and become U+FFFD.
fn push_wtf8(code: u32, out: &mut Vec<u8>) {
    let c = if code > 0x10FFFF { 0xFFFD } else { code };
    match c {
        0..=0x7F => out.push(c as u8),
        0x80..=0x7FF => out.extend_from_slice(&[0xC0 | (c >> 6) as u8, 0x80 | (c & 0x3F) as u8]),
        0x800..=0xFFFF => out.extend_from_slice(&[
            0xE0 | (c >> 12) as u8,
            0x80 | ((c >> 6) & 0x3F) as u8,
            0x80 | (c & 0x3F) as u8,
        ]),
        _ => out.extend_from_slice(&[
            0xF0 | (c >> 18) as u8,
            0x80 | ((c >> 12) & 0x3F) as u8,
            0x80 | ((c >> 6) & 0x3F) as u8,
            0x80 | (c & 0x3F) as u8,
        ]),
    }
}

/// A NUL-terminated wide string as WTF-8 bytes, and the number of units read
/// (so a name that filled the buffer is recognised as truncated).
fn wide_to_wtf8(w: &[sys::WCHAR]) -> (Vec<u8>, usize) {
    let units: Vec<u32> = w
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u32)
        .collect();
    let n = units.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let u = units[i];
        // UTF-16 platforms (Windows): combine a valid surrogate pair.
        if cfg!(windows) && (0xD800..0xDC00).contains(&u) && i + 1 < n {
            let lo = units[i + 1];
            if (0xDC00..0xE000).contains(&lo) {
                push_wtf8(0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00), &mut out);
                i += 2;
                continue;
            }
        }
        push_wtf8(u, &mut out);
        i += 1;
    }
    // RAR names use the platform separator once UnRAR has converted them; on
    // Windows that is a backslash, which is a separator and nothing else.
    #[cfg(windows)]
    for b in &mut out {
        if *b == b'\\' {
            *b = b'/';
        }
    }
    (out, n)
}

/// UTF-8 text as the platform's wide characters.
fn text_to_wide(s: &str) -> Vec<sys::WCHAR> {
    #[cfg(windows)]
    {
        s.encode_utf16().map(|u| u as sys::WCHAR).collect()
    }
    #[cfg(not(windows))]
    {
        s.chars().map(|c| c as u32 as sys::WCHAR).collect()
    }
}

// ---- the callback -----------------------------------------------------------

/// State UnRAR's callback reaches through its user-data pointer.
struct Callback {
    password_wide: Option<Zeroizing<Vec<sys::WCHAR>>>,
    password_narrow: Option<Zeroizing<Vec<u8>>>,
    /// UnRAR asked for a password and none was supplied.
    password_asked: bool,
    /// The next volume UnRAR asked for and could not find (file name only).
    missing_volume: Option<String>,
    /// Bytes of volume files UnRAR has opened so far (for the ratio limit).
    volume_bytes: Rc<Cell<u64>>,
    /// `*mut &mut dyn FnMut(&[u8]) -> bool` while an entry is processed.
    data: *mut c_void,
    /// The data sink stopped the entry (limit, or the consumer is gone).
    stopped_by_sink: bool,
    panicked: bool,
}

impl Callback {
    fn handle(&mut self, msg: sys::UINT, p1: sys::LPARAM, p2: sys::LPARAM) -> c_int {
        match msg {
            sys::UCM_CHANGEVOLUMEW => {
                // SAFETY: for this message UnRAR passes the address of a
                // NUL-terminated wide string it owns, valid for the call.
                let name = unsafe { read_wide(p1 as *const sys::WCHAR, VOLUME_NAME_UNITS) };
                let (bytes, _) = wide_to_wtf8(&name);
                if p2 == sys::RAR_VOL_ASK {
                    let full = String::from_utf8_lossy(&bytes).into_owned();
                    let file = Path::new(&full)
                        .file_name()
                        .map_or(full.clone(), |n| n.to_string_lossy().into_owned());
                    self.missing_volume = Some(file);
                    -1
                } else {
                    if let Ok(m) = std::fs::metadata(path_of_wide(&name)) {
                        self.volume_bytes
                            .set(self.volume_bytes.get().saturating_add(m.len()));
                    }
                    0
                }
            }
            sys::UCM_PROCESSDATA => {
                if self.data.is_null() {
                    return 0;
                }
                let Ok(len) = usize::try_from(p2) else {
                    return -1;
                };
                // UnRAR may call with a null buffer (found by fuzzing). A
                // slice from a null pointer is undefined behaviour even when
                // empty, so an empty chunk never touches `p1`, and a null
                // buffer with bytes claimed is damage, not data.
                let chunk: &[u8] = if len == 0 {
                    &[]
                } else if p1 == 0 {
                    return -1;
                } else {
                    // SAFETY: `p1` is non-null and UnRAR passes a buffer of
                    // `p2` (= `len`) readable bytes at it, valid for the
                    // duration of the call; `u8` needs no alignment.
                    unsafe { std::slice::from_raw_parts(p1 as *const u8, len) }
                };
                // SAFETY: `data` was set by `Rar::process` to the address of a
                // `&mut dyn FnMut(&[u8]) -> bool` on its stack, and is reset to
                // null before that frame returns.
                let f = unsafe { &mut *(self.data as *mut &mut dyn FnMut(&[u8]) -> bool) };
                if f(chunk) {
                    0
                } else {
                    self.stopped_by_sink = true;
                    -1
                }
            }
            sys::UCM_NEEDPASSWORDW => {
                let Some(pw) = &self.password_wide else {
                    self.password_asked = true;
                    return -1;
                };
                let Ok(cap) = usize::try_from(p2) else {
                    return -1;
                };
                if p1 == 0 || cap == 0 || pw.len() >= cap {
                    return -1;
                }
                // SAFETY: UnRAR gives a writable buffer of `p2` wide units at
                // `p1`; we write `pw.len() + 1 <= p2` units.
                unsafe {
                    let dst = p1 as *mut sys::WCHAR;
                    std::ptr::copy_nonoverlapping(pw.as_ptr(), dst, pw.len());
                    *dst.add(pw.len()) = 0;
                }
                1
            }
            sys::UCM_NEEDPASSWORD => {
                let Some(pw) = &self.password_narrow else {
                    self.password_asked = true;
                    return -1;
                };
                let Ok(cap) = usize::try_from(p2) else {
                    return -1;
                };
                if p1 == 0 || cap == 0 || pw.len() >= cap {
                    return -1;
                }
                // SAFETY: as above, for a byte buffer.
                unsafe {
                    let dst = p1 as *mut u8;
                    std::ptr::copy_nonoverlapping(pw.as_ptr(), dst, pw.len());
                    *dst.add(pw.len()) = 0;
                }
                1
            }
            _ => 0,
        }
    }
}

/// Reads a NUL-terminated wide string of at most `max` units.
///
/// # Safety
/// `p` must point to a readable, NUL-terminated wide string, or to `max`
/// readable units.
unsafe fn read_wide(p: *const sys::WCHAR, max: usize) -> Vec<sys::WCHAR> {
    let mut v = Vec::new();
    if p.is_null() {
        return v;
    }
    for i in 0..max {
        let c = *p.add(i);
        if c == 0 {
            break;
        }
        v.push(c);
    }
    v
}

fn path_of_wide(w: &[sys::WCHAR]) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<u16> = w.to_vec();
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&units))
    }
    #[cfg(not(windows))]
    {
        let (bytes, _) = wide_to_wtf8(w);
        // Unix paths are bytes; the wide string came from a path UnRAR built
        // from the archive's own name plus a volume number.
        std::path::PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
    }
}

extern "C" fn callback(
    msg: sys::UINT,
    user: sys::LPARAM,
    p1: sys::LPARAM,
    p2: sys::LPARAM,
) -> c_int {
    if user == 0 {
        return 0;
    }
    let cb = user as *mut Callback;
    // A panic must not unwind through UnRAR's C++ frames.
    // SAFETY: `user` is the address of the `Callback` owned by the `Rar` that
    // registered it (see the module's Unsafe section); no other reference to
    // it is live while UnRAR runs.
    match catch_unwind(AssertUnwindSafe(|| unsafe { (*cb).handle(msg, p1, p2) })) {
        Ok(r) => r,
        Err(_) => {
            // SAFETY: as above.
            unsafe { (*cb).panicked = true };
            -1
        }
    }
}

// ---- the C structs -----------------------------------------------------------
//
// `unrar_sys` 0.5.8 declares these structs with the default `repr(C)` layout,
// but UnRAR's `dll.hpp` is compiled under `#pragma pack(push, 4)`: every
// field after the first pointer sits 4 bytes earlier than `repr(C)` puts it.
// Reading `hash_type`, `redir_*` or `mtime_*` through the crate's struct reads
// the wrong bytes, and the crate's struct is also 8 bytes shorter than what
// the DLL fills. These two mirror the vendored `dll.hpp` exactly (7.x), and
// the layout test below pins the offsets against a C++ compile of that header.

#[repr(C, packed(4))]
struct RawHeader {
    arc_name: [u8; 1024],
    arc_name_w: [sys::WCHAR; 1024],
    file_name: [u8; 1024],
    file_name_w: [sys::WCHAR; 1024],
    flags: c_uint,
    pack_size: c_uint,
    pack_size_high: c_uint,
    unp_size: c_uint,
    unp_size_high: c_uint,
    host_os: c_uint,
    file_crc: c_uint,
    file_time: c_uint,
    unp_ver: c_uint,
    method: c_uint,
    file_attr: c_uint,
    cmt_buf: *mut u8,
    cmt_buf_size: c_uint,
    cmt_size: c_uint,
    cmt_state: c_uint,
    dict_size: c_uint,
    hash_type: c_uint,
    hash: [u8; 32],
    redir_type: c_uint,
    redir_name: *mut sys::WCHAR,
    redir_name_size: c_uint,
    dir_target: c_uint,
    mtime_low: c_uint,
    mtime_high: c_uint,
    ctime_low: c_uint,
    ctime_high: c_uint,
    atime_low: c_uint,
    atime_high: c_uint,
    arc_name_ex: *mut sys::WCHAR,
    arc_name_ex_size: c_uint,
    file_name_ex: *mut sys::WCHAR,
    file_name_ex_size: c_uint,
    reserved: [c_uint; 982],
}

#[repr(C, packed(4))]
struct RawOpen {
    arc_name: *const std::ffi::c_char,
    arc_name_w: *const sys::WCHAR,
    open_mode: c_uint,
    open_result: c_uint,
    cmt_buf: *mut u8,
    cmt_buf_size: c_uint,
    cmt_size: c_uint,
    cmt_state: c_uint,
    flags: c_uint,
    callback: Option<sys::Callback>,
    user_data: sys::LPARAM,
    op_flags: c_uint,
    cmt_buf_w: *mut sys::WCHAR,
    reserved: [c_uint; 25],
}

// ---- the archive handle ------------------------------------------------------

struct Rar {
    handle: *const sys::Handle,
    cb: Box<Callback>,
    /// `ROADF_*` flags read at open.
    flags: c_uint,
}

impl Drop for Rar {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: the handle came from RAROpenArchiveEx and is closed once.
            unsafe { sys::RARCloseArchive(self.handle) };
        }
    }
}

/// Why opening failed.
struct OpenFailure {
    code: c_int,
    password_asked: bool,
    missing_volume: Option<String>,
}

impl Rar {
    fn open(path: &Path, mode: c_uint, password: Option<&str>) -> Result<Rar, OpenFailure> {
        let mut cb = Box::new(Callback {
            password_wide: password.map(|p| Zeroizing::new(text_to_wide(p))),
            password_narrow: password.map(|p| Zeroizing::new(p.as_bytes().to_vec())),
            password_asked: false,
            missing_volume: None,
            volume_bytes: Rc::new(Cell::new(0)),
            data: std::ptr::null_mut(),
            stopped_by_sink: false,
            panicked: false,
        });
        let user = (&mut *cb as *mut Callback) as sys::LPARAM;

        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            CString::new(path.as_os_str().as_bytes()).map_err(|_| OpenFailure {
                code: sys::ERAR_EOPEN,
                password_asked: false,
                missing_volume: None,
            })?
        };
        #[cfg(windows)]
        let name: Vec<sys::WCHAR> = {
            use std::os::windows::ffi::OsStrExt;
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .map(|u| u as sys::WCHAR)
                .collect()
        };

        // SAFETY: every field is an integer, a raw pointer, or an `Option` of
        // a function pointer, for all of which all-zero bytes are valid (null,
        // 0, `None`); zeroing also clears the padding the DLL may read.
        let mut data: RawOpen = unsafe { std::mem::zeroed() };
        data.open_mode = mode;
        data.callback = Some(callback);
        data.user_data = user;
        #[cfg(unix)]
        {
            data.arc_name = name.as_ptr();
        }
        #[cfg(windows)]
        {
            data.arc_name_w = name.as_ptr();
        }
        // SAFETY: `data` mirrors `RAROpenArchiveDataEx` (see `RawOpen`) and,
        // with `name`, outlives the call; the callback and its user-data
        // (`cb`, boxed, moved into the result below without moving the
        // allocation) stay valid for the life of the handle.
        let handle = unsafe {
            sys::RAROpenArchiveEx((&mut data as *mut RawOpen).cast::<sys::OpenArchiveDataEx>())
        };
        let code = { data.open_result } as c_int;
        let open_flags = { data.flags };
        let mut rar = Rar {
            handle,
            cb,
            flags: open_flags,
        };
        if handle.is_null() || code != sys::ERAR_SUCCESS {
            let failure = OpenFailure {
                code: if code == 0 { sys::ERAR_EOPEN } else { code },
                password_asked: rar.cb.password_asked,
                missing_volume: rar.cb.missing_volume.take(),
            };
            return Err(failure);
        }
        Ok(rar)
    }

    fn read_header(&mut self) -> Result<Option<Header>, c_int> {
        let mut redir = vec![0 as sys::WCHAR; NAME_UNITS];
        // SAFETY: as for `RawOpen`: integers and raw pointers only. Boxed:
        // it is 14 KB.
        let mut raw: Box<RawHeader> = Box::new(unsafe { std::mem::zeroed() });
        raw.redir_name = redir.as_mut_ptr();
        raw.redir_name_size = redir.len() as c_uint;
        // SAFETY: the handle is open; `raw` mirrors `RARHeaderDataEx` (see
        // `RawHeader`) and, with `redir`, outlives the call.
        let code = unsafe {
            sys::RARReadHeaderEx(
                self.handle,
                (&mut *raw as *mut RawHeader).cast::<sys::HeaderDataEx>(),
            )
        };
        match code {
            sys::ERAR_SUCCESS => Ok(Some(Header::from_raw(&raw, &redir))),
            sys::ERAR_END_ARCHIVE => Ok(None),
            other => Err(other),
        }
    }

    /// Runs `op` (`RAR_SKIP` or `RAR_TEST`) on the entry the last header
    /// described. `sink` receives decoded bytes; returning `false` stops
    /// UnRAR. No destination is ever passed: nothing is written.
    fn process(&mut self, op: c_int, sink: &mut dyn FnMut(&[u8]) -> bool) -> c_int {
        let mut dynf: &mut dyn FnMut(&[u8]) -> bool = sink;
        self.cb.data = (&mut dynf as *mut &mut dyn FnMut(&[u8]) -> bool).cast::<c_void>();
        self.cb.stopped_by_sink = false;
        // SAFETY: the handle is open; null destinations mean "do not write".
        let code =
            unsafe { sys::RARProcessFile(self.handle, op, std::ptr::null(), std::ptr::null()) };
        self.cb.data = std::ptr::null_mut();
        code
    }
}

// ---- headers ------------------------------------------------------------------

struct Header {
    name: Vec<u8>,
    name_truncated: bool,
    flags: c_uint,
    pack_size: u64,
    unp_size: u64,
    host_os: c_uint,
    method: c_uint,
    file_attr: c_uint,
    unp_ver: c_uint,
    dict_kb: c_uint,
    hash_type: c_uint,
    redir_type: c_uint,
    redir_target: Option<Vec<u8>>,
    mtime: Option<i64>,
}

impl Header {
    fn from_raw(r: &RawHeader, redir: &[sys::WCHAR]) -> Header {
        let file_name_w = r.file_name_w;
        let (name, units) = wide_to_wtf8(&file_name_w);
        let redir_type = r.redir_type;
        let redir_target = (redir_type != 0 && redir.first().is_some_and(|c| *c != 0))
            .then(|| wide_to_wtf8(redir).0);
        let ticks = (u64::from(r.mtime_high) << 32) | u64::from(r.mtime_low);
        let mtime = (ticks != 0)
            .then(|| i64::try_from(ticks / TICKS).ok())
            .flatten()
            .map(|s| s - EPOCH_DIFF_SECS as i64);
        Header {
            // A name that fills UnRAR's buffer was cut; a cut name is a wrong
            // name, so it is not carried.
            name_truncated: units >= NAME_UNITS - 1,
            name,
            flags: r.flags,
            pack_size: (u64::from(r.pack_size_high) << 32) | u64::from(r.pack_size),
            unp_size: (u64::from(r.unp_size_high) << 32) | u64::from(r.unp_size),
            host_os: r.host_os,
            method: r.method,
            file_attr: r.file_attr,
            unp_ver: r.unp_ver,
            dict_kb: r.dict_size,
            hash_type: r.hash_type,
            redir_type,
            redir_target,
            mtime,
        }
    }

    fn is_dir(&self) -> bool {
        self.flags & sys::RHDF_DIRECTORY != 0
    }

    fn is_encrypted(&self) -> bool {
        self.flags & sys::RHDF_ENCRYPTED != 0
    }

    fn kind(&self) -> EntryKind {
        if self.is_dir() {
            return EntryKind::Directory;
        }
        match self.redir_type {
            FSREDIR_UNIXSYMLINK | FSREDIR_WINSYMLINK | FSREDIR_JUNCTION => EntryKind::Symlink,
            FSREDIR_HARDLINK | FSREDIR_FILECOPY => EntryKind::HardLink,
            _ if self.host_os == HOST_UNIX && self.file_attr & 0xF000 == 0xA000 => {
                EntryKind::Symlink
            }
            _ => EntryKind::File,
        }
    }

    /// `RAR_HASH_*` as the check that ran.
    fn check(&self) -> Option<CheckKind> {
        match self.hash_type {
            sys::RAR_HASH_CRC32 => Some(CheckKind::Crc32),
            sys::RAR_HASH_BLAKE2 => Some(CheckKind::Blake2sp),
            _ => None,
        }
    }

    fn info(&self, index: u64, limits: &Limits) -> (EntryInfo, Option<u64>) {
        let mut error = None;
        let mut long = None;
        let name = if self.name_truncated {
            long = Some(NAME_UNITS as u64);
            error = Some(format!(
                "entry name is {NAME_UNITS} characters or more, which this reader cannot return whole; not carried"
            ));
            Vec::new()
        } else if self.name.len() as u64 > u64::from(limits.max_name_len) {
            long = Some(self.name.len() as u64);
            error = Some(format!(
                "entry name is {} bytes, over the {}-byte limit; not carried",
                self.name.len(),
                limits.max_name_len
            ));
            Vec::new()
        } else {
            self.name.clone()
        };
        let encryption = if !self.is_encrypted() {
            Encryption::None
        } else if self.unp_ver >= 50 {
            Encryption::Aes(256)
        } else {
            Encryption::Aes(128)
        };
        let version = if self.unp_ver >= 50 { "RAR5" } else { "RAR4" };
        (
            EntryInfo {
                index,
                name,
                kind: self.kind(),
                size: Some(self.unp_size),
                compressed_size: Some(self.pack_size),
                encryption,
                method: format!("{version} method {}", self.method_label()),
                link_target: self.redir_target.clone(),
                unix_mode: (self.host_os == HOST_UNIX).then_some(self.file_attr),
                mtime_unix: self.mtime,
                name_is_utf8_flag: None,
                error,
            },
            long,
        )
    }

    fn method_label(&self) -> &'static str {
        // The DLL reports the method as 0x30 (store) .. 0x35 (best) for both
        // RAR versions.
        match self.method {
            0x30 => "store",
            0x31 => "fastest",
            0x32 => "fast",
            0x33 => "normal",
            0x34 => "good",
            0x35 => "best",
            _ => "unknown",
        }
    }
}

// ---- running ------------------------------------------------------------------

fn password_text(req: &Request) -> Result<Option<String>, EngineError> {
    match &req.password {
        None => Ok(None),
        Some(p) => String::from_utf8(p.as_bytes().to_vec())
            .map(Some)
            .map_err(|_| {
                archive_err(
                    ErrorCode::BadRequest,
                    "a RAR password must be valid UTF-8 text",
                )
            }),
    }
}

fn open_error(f: &OpenFailure, had_password: bool) -> EngineError {
    match f.code {
        sys::ERAR_MISSING_PASSWORD => archive_err(
            ErrorCode::PasswordRequired,
            "the archive's names are encrypted; a password is needed to list it",
        ),
        sys::ERAR_BAD_PASSWORD => archive_err(
            ErrorCode::WrongPassword,
            "the password did not open the archive's names",
        ),
        // RAR 4 header encryption has no password check: a wrong password
        // yields headers that fail their own checks.
        sys::ERAR_BAD_ARCHIVE | sys::ERAR_BAD_DATA | sys::ERAR_UNKNOWN_FORMAT
            if had_password && f.password_asked =>
        {
            archive_err(
                ErrorCode::WrongPassword,
                "the archive's headers did not check out: a wrong password, or damaged headers",
            )
        }
        _ if f.password_asked && !had_password => archive_err(
            ErrorCode::PasswordRequired,
            "the archive's names are encrypted; a password is needed to list it",
        ),
        sys::ERAR_EOPEN if f.missing_volume.is_some() => archive_err(
            ErrorCode::MissingVolume,
            format!(
                "volume {} of this multi-volume set is missing",
                f.missing_volume.as_deref().unwrap_or("?")
            ),
        ),
        sys::ERAR_BAD_ARCHIVE | sys::ERAR_BAD_DATA => archive_err(
            ErrorCode::MalformedArchive,
            "not a valid RAR archive, or its headers are damaged",
        ),
        sys::ERAR_UNKNOWN_FORMAT => archive_err(
            ErrorCode::UnsupportedFormat,
            "this RAR format version is not supported",
        ),
        sys::ERAR_EOPEN => archive_err(ErrorCode::OpenFailed, "cannot open the archive"),
        sys::ERAR_NO_MEMORY => archive_err(
            ErrorCode::MalformedArchive,
            "out of memory while opening the archive",
        ),
        other => archive_err(
            ErrorCode::MalformedArchive,
            format!("the RAR reader failed to open the archive (code {other})"),
        ),
    }
}

/// Describes a header-read failure in the middle of an archive.
fn stop_text(code: c_int, missing: Option<&str>) -> String {
    match (code, missing) {
        (_, Some(v)) => format!("volume {v} is missing; the archive is incomplete"),
        (sys::ERAR_BAD_DATA | sys::ERAR_BAD_ARCHIVE, _) => {
            "an entry header is damaged or the archive is truncated".to_string()
        }
        (sys::ERAR_EREAD, _) => "the archive could not be read to its end (truncated?)".to_string(),
        (sys::ERAR_MISSING_PASSWORD, _) => "a password is needed to read further".to_string(),
        (sys::ERAR_BAD_PASSWORD, _) => "the password did not match".to_string(),
        (c, _) => format!("the RAR reader stopped (code {c})"),
    }
}

pub fn run(origin: &Origin<'_>, req: &Request, out: &mut Out) -> Result<Tally, EngineError> {
    let Some(path) = origin.path else {
        return Err(archive_err(
            ErrorCode::UnsupportedFormat,
            "RAR archives can only be read from a file",
        ));
    };
    let l = req.limits;
    let password = password_text(req)?;
    let list = req.op == Op::List;
    let mode = if list {
        sys::RAR_OM_LIST
    } else {
        sys::RAR_OM_EXTRACT
    };
    let mut rar = Rar::open(path, mode, password.as_deref())
        .map_err(|f| open_error(&f, req.password.is_some()))?;
    if rar.flags & sys::ROADF_VOLUME != 0 && rar.flags & sys::ROADF_FIRSTVOLUME == 0 {
        return Err(archive_err(
            ErrorCode::MissingVolume,
            "this is not the first volume of a multi-volume RAR set; open the first volume (.part1.rar, or .rar)",
        ));
    }
    let own_len = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut tally = Tally {
        complete: true,
        ..Tally::default()
    };
    let wanted = req.wanted_count();
    let mut found: u64 = 0;
    let mut index: u64 = 0;
    let mut bytes_out: u64 = 0;
    let mut stop_all = false;
    let mut sink_err: Option<io::Error> = None;
    let mut meter = Meter {
        limits: l,
        decoded: 0,
        own_len,
        volumes: Rc::clone(&rar.cb.volume_bytes),
        breach: None,
        sink_failed: None,
    };

    loop {
        if index >= l.max_entries {
            emit_breach(out, BreachKind::EntryCount, None, l.max_entries, index + 1)?;
            tally.complete = false;
            tally.stop_reason = Some(format!("more than {} entries", l.max_entries));
            break;
        }
        let header = match rar.read_header() {
            Ok(Some(h)) => h,
            Ok(None) => break,
            Err(code) => {
                if index == 0 {
                    return Err(open_error(
                        &OpenFailure {
                            code,
                            password_asked: rar.cb.password_asked,
                            missing_volume: rar.cb.missing_volume.clone(),
                        },
                        req.password.is_some(),
                    ));
                }
                tally.complete = false;
                tally.stop_reason = Some(stop_text(code, rar.cb.missing_volume.as_deref()));
                break;
            }
        };
        let (info, long) = header.info(index, &l);
        let want = req.wants(index);
        if let (Some(observed), true) = (long, want) {
            emit_breach(
                out,
                BreachKind::NameLength,
                Some(index),
                u64::from(l.max_name_len),
                observed,
            )?;
        }
        tally.entries += 1;

        if list {
            out(Message::Entry(info))?;
            if dict_blocked(&header) {
                tally.complete = false;
                tally.stop_reason = Some(dict_stop_text(&header));
                break;
            }
            let code = rar.process(sys::RAR_SKIP, &mut |_| true);
            index += 1;
            if code != sys::ERAR_SUCCESS {
                tally.complete = false;
                tally.stop_reason = Some(stop_text(code, rar.cb.missing_volume.as_deref()));
                break;
            }
            continue;
        }

        if !want {
            // Not asked for. A solid archive still decodes it to keep its
            // dictionary in step, and that work counts against the limits.
            if dict_blocked(&header) {
                tally.complete = false;
                tally.stop_reason = Some(dict_stop_text(&header));
                break;
            }
            let code = rar.process(sys::RAR_SKIP, &mut |chunk: &[u8]| {
                meter.add(chunk.len()).is_none()
            });
            index += 1;
            if code != sys::ERAR_SUCCESS && code != sys::ERAR_BAD_DATA {
                tally.complete = false;
                tally.stop_reason = Some(if let Some(k) = meter.tripped(&mut sink_err) {
                    format!("stopped by limit {}", k.as_str())
                } else {
                    stop_text(code, rar.cb.missing_volume.as_deref())
                });
                break;
            }
            continue;
        }

        if req.op != Op::Read {
            out(Message::Entry(info.clone()))?;
        }
        let encrypted = info.encryption != Encryption::None;
        let declared = info.size.unwrap_or(0);

        // Entries decided without decoding them.
        let pre: Option<EntryOutcome> = if let Some(e) = &info.error {
            Some(EntryOutcome::ReadError(e.clone()))
        } else if header.is_dir() || header.redir_type != 0 {
            Some(EntryOutcome::NoData)
        } else if encrypted && password.is_none() {
            Some(EntryOutcome::PasswordRequired)
        } else if header.dict_kb > MAX_DICT_KB {
            Some(EntryOutcome::Unsupported(format!(
                "the entry needs a {} MiB dictionary, over the {} MiB limit",
                header.dict_kb >> 10,
                MAX_DICT_KB >> 10
            )))
        } else if declared > l.max_entry_size {
            emit_breach(
                out,
                BreachKind::EntrySize,
                Some(index),
                l.max_entry_size,
                declared,
            )?;
            Some(EntryOutcome::Breach(BreachKind::EntrySize))
        } else if req.op.is_read() && encrypted && declared > MAX_HELD_ENTRY {
            Some(EntryOutcome::Unsupported(format!(
                "encrypted entry of {declared} bytes is over the {MAX_HELD_ENTRY}-byte limit this build holds in memory to keep a wrong password from delivering partial output"
            )))
        } else {
            None
        };

        let outcome = if let Some(o) = pre {
            // Skipping an entry in a solid archive makes UnRAR decode it to
            // keep the window, which allocates the entry's dictionary. An
            // entry over the dictionary limit is therefore never handed to
            // UnRAR at all, and nothing after it can be reached safely.
            let unreachable_dict = dict_blocked(&header);
            if unreachable_dict {
                stop_all = true;
                tally.stop_reason = Some(dict_stop_text(&header));
            } else {
                let code = rar.process(sys::RAR_SKIP, &mut |chunk: &[u8]| {
                    meter.add(chunk.len()).is_none()
                });
                if code != sys::ERAR_SUCCESS && code != sys::ERAR_BAD_DATA {
                    stop_all = true;
                    tally.stop_reason = Some(stop_text(code, rar.cb.missing_volume.as_deref()));
                }
            }
            o
        } else {
            let deliver = req.op.is_read();
            let hold = deliver && encrypted;
            let mut held: Vec<u8> = Vec::new();
            let mut entry_out: u64 = 0;
            let code = rar.process(sys::RAR_TEST, &mut |chunk: &[u8]| {
                let n = chunk.len() as u64;
                entry_out += n;
                if meter.add(chunk.len()).is_some() {
                    return false;
                }
                if entry_out > l.max_entry_size {
                    meter.trip(BreachKind::EntrySize);
                    return false;
                }
                if deliver {
                    if hold {
                        held.extend_from_slice(chunk);
                    } else {
                        for part in chunk.chunks(DATA_CHUNK) {
                            if let Err(e) = out(Message::Data(part.to_vec())) {
                                meter.sink_failed = Some(e);
                                return false;
                            }
                        }
                    }
                }
                true
            });
            bytes_out += entry_out;
            if let Some(e) = meter.sink_failed.take() {
                sink_err = Some(e);
                break;
            }
            if let Some(k) = meter.breach.take() {
                let (limit, observed) = match k {
                    BreachKind::TotalOutput => (l.max_total_output, meter.decoded),
                    BreachKind::EntrySize => (l.max_entry_size, entry_out),
                    _ => (l.max_ratio, meter.decoded / meter.denominator().max(1)),
                };
                emit_breach(out, k, Some(index), limit, observed)?;
                stop_all = true;
                EntryOutcome::Breach(k)
            } else {
                let check = header.check();
                match code {
                    sys::ERAR_SUCCESS => {
                        if hold {
                            for chunk in held.chunks(DATA_CHUNK) {
                                out(Message::Data(chunk.to_vec()))?;
                            }
                        }
                        match check {
                            Some(c) => EntryOutcome::Matched(CheckSet::of(&[c])),
                            None => EntryOutcome::NoEntryCheck,
                        }
                    }
                    sys::ERAR_BAD_DATA if entry_out < declared => {
                        // Fewer bytes than the header declares: the data ran
                        // out (a truncated volume, or a damaged stream). That
                        // is not a CRC verdict, so it is not reported as one.
                        let mut t = format!(
                            "the data ended after {entry_out} of {declared} bytes (truncated or damaged)"
                        );
                        if encrypted {
                            t.push_str(", or a wrong password");
                        }
                        EntryOutcome::ReadError(t)
                    }
                    sys::ERAR_BAD_DATA => match check {
                        Some(c) => EntryOutcome::Mismatch {
                            failed: CheckSet::of(&[c]),
                            wrong_password_possible: encrypted,
                        },
                        None => EntryOutcome::ReadError(if encrypted {
                            "data error (damaged data, or a wrong password)".to_string()
                        } else {
                            "data error".to_string()
                        }),
                    },
                    sys::ERAR_BAD_PASSWORD => EntryOutcome::WrongPassword,
                    sys::ERAR_MISSING_PASSWORD => EntryOutcome::PasswordRequired,
                    sys::ERAR_NO_MEMORY => EntryOutcome::ReadError(
                        "out of memory while decoding the entry".to_string(),
                    ),
                    other => {
                        stop_all = true;
                        let why = stop_text(other, rar.cb.missing_volume.as_deref());
                        tally.stop_reason = Some(why.clone());
                        EntryOutcome::ReadError(why)
                    }
                }
            }
        };
        out(Message::Result { index, outcome })?;
        found += 1;
        index += 1;
        if stop_all {
            tally.complete = false;
            tally.stop_reason.get_or_insert_with(|| {
                "the operation stopped (limit or damage); remaining entries were not run"
                    .to_string()
            });
            break;
        }
        if wanted.is_some_and(|n| found >= n) {
            break;
        }
    }

    if let Some(e) = sink_err {
        return Err(EngineError::Sink(e));
    }
    if wanted.is_some_and(|n| found < n) && tally.complete {
        return Err(archive_err(
            ErrorCode::EntryNotFound,
            "a requested entry was not found",
        ));
    }
    tally.bytes_out = bytes_out;
    out(Message::StreamCheck(StreamCheck::NotApplicable))?;
    Ok(tally)
}

/// Counts every decoded byte (wanted or not) against the whole-operation
/// limits.
struct Meter {
    limits: Limits,
    decoded: u64,
    own_len: u64,
    /// Bytes of the volume files UnRAR has opened (the callback adds to it).
    volumes: Rc<Cell<u64>>,
    breach: Option<BreachKind>,
    sink_failed: Option<io::Error>,
}

impl Meter {
    /// What the decoded bytes are compared with for the ratio limit: the
    /// archive file, or every volume UnRAR has opened for a multi-volume set.
    fn denominator(&self) -> u64 {
        self.own_len
            .max(self.volumes.get().saturating_add(self.own_len))
    }

    fn trip(&mut self, k: BreachKind) {
        self.breach.get_or_insert(k);
    }

    /// Counts `n` more decoded bytes; `Some` when a whole-operation limit is
    /// passed (and the decoder must be stopped).
    fn add(&mut self, n: usize) -> Option<BreachKind> {
        self.decoded = self.decoded.saturating_add(n as u64);
        if self.decoded > self.limits.max_total_output {
            self.trip(BreachKind::TotalOutput);
        } else if ratio_exceeded(&self.limits, self.decoded, self.denominator()) {
            self.trip(BreachKind::Ratio);
        }
        self.breach
    }

    fn tripped(&mut self, _sink_err: &mut Option<io::Error>) -> Option<BreachKind> {
        self.breach
    }
}

#[cfg(all(test, target_os = "linux", target_pointer_width = "64"))]
mod layout {
    //! The offsets `dll.hpp` has under `#pragma pack(push, 4)`, taken from a
    //! C++ compile of the vendored header (`offsetof`), pinned here so a
    //! change to either struct or an UnRAR update that moves a field fails.
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn header_layout_matches_dll_hpp() {
        assert_eq!(offset_of!(RawHeader, arc_name_w), 1024);
        assert_eq!(offset_of!(RawHeader, file_name), 5120);
        assert_eq!(offset_of!(RawHeader, file_name_w), 6144);
        assert_eq!(offset_of!(RawHeader, flags), 10240);
        assert_eq!(offset_of!(RawHeader, file_attr), 10280);
        // The first pointer sits at a 4-byte offset: the packing in effect.
        assert_eq!(offset_of!(RawHeader, cmt_buf), 10284);
        assert_eq!(offset_of!(RawHeader, dict_size), 10304);
        assert_eq!(offset_of!(RawHeader, hash_type), 10308);
        assert_eq!(offset_of!(RawHeader, hash), 10312);
        assert_eq!(offset_of!(RawHeader, redir_type), 10344);
        assert_eq!(offset_of!(RawHeader, redir_name), 10348);
        assert_eq!(offset_of!(RawHeader, redir_name_size), 10356);
        assert_eq!(offset_of!(RawHeader, mtime_low), 10364);
        assert_eq!(offset_of!(RawHeader, mtime_high), 10368);
        assert_eq!(offset_of!(RawHeader, atime_high), 10384);
        assert_eq!(offset_of!(RawHeader, arc_name_ex), 10388);
        assert_eq!(offset_of!(RawHeader, reserved), 10412);
        assert_eq!(size_of::<RawHeader>(), 14340);
    }

    #[test]
    fn open_layout_matches_dll_hpp() {
        assert_eq!(offset_of!(RawOpen, flags), 44);
        assert_eq!(offset_of!(RawOpen, callback), 48);
        assert_eq!(offset_of!(RawOpen, user_data), 56);
        assert_eq!(offset_of!(RawOpen, op_flags), 64);
        assert_eq!(offset_of!(RawOpen, cmt_buf_w), 68);
        assert_eq!(size_of::<RawOpen>(), 176);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn callback(data: *mut c_void) -> Callback {
        Callback {
            password_wide: None,
            password_narrow: None,
            password_asked: false,
            missing_volume: None,
            volume_bytes: Rc::new(Cell::new(0)),
            data,
            stopped_by_sink: false,
            panicked: false,
        }
    }

    /// UnRAR was seen calling `UCM_PROCESSDATA` with a null buffer.
    #[test]
    fn null_data_buffer_is_an_empty_chunk_or_damage() {
        let mut seen = 0usize;
        let mut sink = |c: &[u8]| {
            seen += c.len();
            true
        };
        let mut sink_ref: &mut dyn FnMut(&[u8]) -> bool = &mut sink;
        let mut cb = callback(&mut sink_ref as *mut _ as *mut c_void);
        assert_eq!(cb.handle(sys::UCM_PROCESSDATA, 0, 0), 0);
        assert_eq!(cb.handle(sys::UCM_PROCESSDATA, 0, 16), -1);
        assert_eq!(cb.handle(sys::UCM_PROCESSDATA, 0, -1), -1);
        let bytes = [1u8, 2, 3];
        assert_eq!(cb.handle(sys::UCM_PROCESSDATA, bytes.as_ptr() as _, 3), 0);
        drop(cb);
        assert_eq!(seen, 3);
    }
}
