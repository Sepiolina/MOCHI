//! The only `unsafe` MOCHI has outside `mochi-foreign` (AGENTS.md; owner
//! decision 2026-10-15, superseding T21 and checklist Q63).
//!
//! `mochi-core` keeps `#![forbid(unsafe_code)]`, so the Win32 calls it needs
//! live here, behind safe functions:
//!
//! * [`move_no_replace_write_through`]: `MoveFileExW(src, dst,
//!   MOVEFILE_WRITE_THROUGH)`, **without** `MOVEFILE_REPLACE_EXISTING`, so it
//!   fails if `dst` exists. This publishes a created archive (spec Annex B
//!   D13). Microsoft documents `MOVEFILE_WRITE_THROUGH` as: "The function does
//!   not return until the file is actually moved on the disk." The next
//!   sentence of the same page ("Setting this value guarantees that a move
//!   performed as a copy and delete operation is flushed to disk before the
//!   function returns.") is about the copy-and-delete case; a same-directory
//!   rename is not that case, so this crate claims only the first sentence.
//!   The G6b crash-replay evidence (`docs/dev/windows-crash-replay.md`) shows
//!   that on NTFS that sentence alone does not make the new name durable; the
//!   directory flush in `mochi-core` does, and Windows still reports
//!   `Unconfirmed` by the G6 policy ([owner, 2026-10-15]; spec Annex B.3).
//! * [`volume_filesystem_name`]: `GetVolumeInformationByHandleW` on a handle
//!   to a directory, e.g. `"NTFS"`.
//! * [`try_lock_byte`] / [`unlock_byte`]: `LockFileEx` (exclusive, fail
//!   immediately) and `UnlockFileEx` on **one byte** at a caller-chosen
//!   offset. This is the writer lock (spec Annex B.2.17, D27): MOCHI locks one
//!   byte far beyond any practical file size, so the lock lives on the archive
//!   itself and still blocks no read or write of the file's real bytes
//!   (`std`'s `File::try_lock` locks the whole range, which fails every other
//!   handle's reads with error 33).
//! * [`file_identity`]: `GetFileInformationByHandle`, the volume serial number
//!   and file index of an open file. Two handles with the same pair are the
//!   same file; `mochi-core` uses it to check, after locking, that a path
//!   still names the file that was locked (spec Annex B.2.17 item 4).
//!
//! Every `unsafe` block carries a `// SAFETY:` comment, and
//! `ci/check-invariants.sh` checks both that and that `unsafe` appears in no
//! other file. The crate is empty on non-Windows targets.
#![cfg(windows)]
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetVolumeInformationByHandleW, LockFileEx, MoveFileExW,
    UnlockFileEx, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, LOCKFILE_EXCLUSIVE_LOCK,
    LOCKFILE_FAIL_IMMEDIATELY, MOVEFILE_WRITE_THROUGH,
};
use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};

/// `path` as a NUL-terminated UTF-16 string; an interior NUL is refused
/// (Win32 would silently stop at it and act on a shorter name).
fn wide_nul_terminated(path: &Path) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = OsStr::new(path).encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// Move `src` to `dst` and return only once the move is on disk
/// (`MOVEFILE_WRITE_THROUGH`), failing if `dst` exists.
///
/// An existing `dst` is `ERROR_ALREADY_EXISTS` / `ERROR_FILE_EXISTS`, which
/// `std` maps to [`io::ErrorKind::AlreadyExists`]; `src` and `dst` are then
/// untouched. `MOVEFILE_REPLACE_EXISTING` is deliberately never passed.
pub fn move_no_replace_write_through(src: &Path, dst: &Path) -> io::Result<()> {
    let (src, dst) = (wide_nul_terminated(src)?, wide_nul_terminated(dst)?);
    // SAFETY: both pointers come from live `Vec<u16>`s that outlive the call
    // and end in exactly one NUL (interior NULs were refused above), as
    // `PCWSTR` requires. The flags are a valid `MOVE_FILE_FLAGS` value.
    let ok = unsafe { MoveFileExW(src.as_ptr(), dst.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The filesystem name (e.g. `"NTFS"`) of the volume that holds the directory
/// `path`.
pub fn volume_filesystem_name(path: &Path) -> io::Result<String> {
    // The handle is opened by `std` (attribute access only, shared with
    // everyone, backup semantics so a directory can be opened) and closed by
    // its `Drop`, so this function has no handle management of its own.
    let dir = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // MAX_PATH + 1 UTF-16 units: the documented size of the name buffer.
    let mut name = [0u16; 261];
    // SAFETY: the handle is open for the duration of the call (`dir` is alive
    // until the end of the function). The volume-name, serial-number,
    // component-length and flags outputs are optional and passed null with a
    // zero size. `name` is a writable buffer of exactly the length passed.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            dir.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(
        name.get(..len).unwrap_or_default(),
    ))
}

/// The identity of the file `file` is open on: its volume serial number and
/// its 64-bit file index (`GetFileInformationByHandle`). Two open handles
/// that report the same pair are open on the same file, whatever names they
/// were opened by (hard links included); the pair is stable while a handle is
/// open. A filesystem that has no file index reports 0 for it, which callers
/// must treat as "unknown".
pub fn file_identity(file: &File) -> io::Result<(u32, u64)> {
    // Plain integers, zero-initialised; the call fills them in.
    let mut info = BY_HANDLE_FILE_INFORMATION {
        dwFileAttributes: 0,
        ftCreationTime: Default::default(),
        ftLastAccessTime: Default::default(),
        ftLastWriteTime: Default::default(),
        dwVolumeSerialNumber: 0,
        nFileSizeHigh: 0,
        nFileSizeLow: 0,
        nNumberOfLinks: 0,
        nFileIndexHigh: 0,
        nFileIndexLow: 0,
    };
    // SAFETY: the handle is open for the duration of the call (`file` is
    // borrowed), and `info` is a valid, initialised, writable
    // `BY_HANDLE_FILE_INFORMATION` that lives until the call returns.
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

/// An `OVERLAPPED` that only carries a 64-bit file offset: what
/// `LockFileEx`/`UnlockFileEx` read from it for a synchronous handle.
fn overlapped_at(offset: u64) -> OVERLAPPED {
    OVERLAPPED {
        Internal: 0,
        InternalHigh: 0,
        Anonymous: OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                // Low and high halves of the offset.
                Offset: (offset & 0xFFFF_FFFF) as u32,
                OffsetHigh: (offset >> 32) as u32,
            },
        },
        hEvent: std::ptr::null_mut(),
    }
}

/// Try to take an exclusive lock on the single byte at `offset` of `file`,
/// without waiting: `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK |
/// LOCKFILE_FAIL_IMMEDIATELY)` over a length of one.
///
/// * `Ok(true)`: the lock is held now.
/// * `Ok(false)`: another handle (in this process or another) holds an
///   overlapping range (`ERROR_LOCK_VIOLATION`, or `ERROR_IO_PENDING`, which
///   is what `std`'s own `File::try_lock` also treats as "would block").
/// * `Err`: any other failure.
///
/// A byte-range lock is mandatory for the bytes it covers and for nothing
/// else. Locking a byte beyond the end of the file is allowed and does not
/// conflict with reads or writes of the real contents; it needs the handle
/// to be open for reading or writing. The lock belongs to the file, not to
/// the name, so hard links of one file share it. The OS drops it when the
/// handle is closed or the process ends, but only some time later (Microsoft
/// documents the delay as depending on system resources), so call
/// [`unlock_byte`] to release it at once.
pub fn try_lock_byte(file: &File, offset: u64) -> io::Result<bool> {
    let mut overlapped = overlapped_at(offset);
    // SAFETY: the handle is open for the duration of the call (`file` is
    // borrowed). `overlapped` is a valid, initialised `OVERLAPPED` that lives
    // until the call returns; the handle is synchronous (opened by `std`
    // without `FILE_FLAG_OVERLAPPED`), so it is used only for the offset. The
    // flags are a valid `LOCK_FILE_FLAGS` value and the reserved argument is 0.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error().map(|c| c as u32) {
        Some(ERROR_LOCK_VIOLATION | ERROR_IO_PENDING) => Ok(false),
        _ => Err(error),
    }
}

/// Release the one-byte lock [`try_lock_byte`] took at `offset` on `file`
/// (`UnlockFileEx`). Unlocking a byte this handle does not hold is an error
/// (`ERROR_NOT_LOCKED`).
pub fn unlock_byte(file: &File, offset: u64) -> io::Result<()> {
    let mut overlapped = overlapped_at(offset);
    // SAFETY: as in `try_lock_byte`: the handle is open for the call, the
    // `OVERLAPPED` is initialised and outlives it, the reserved argument is 0.
    let ok = unsafe { UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut overlapped) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset MOCHI uses (spec B.2.17): far beyond any file.
    const FAR: u64 = 1 << 62;

    fn open_rw(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_byte_lock_excludes_other_handles_but_not_reads_or_writes() {
        use std::io::{Read, Write};
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("f");
        std::fs::write(&path, b"contents").unwrap();
        let (a, b) = (open_rw(&path), open_rw(&path));
        assert!(try_lock_byte(&a, FAR).unwrap());
        assert!(!try_lock_byte(&b, FAR).unwrap(), "a second holder");
        // A different byte is a different lock.
        assert!(try_lock_byte(&b, FAR + 1).unwrap());
        unlock_byte(&b, FAR + 1).unwrap();
        // The real bytes stay readable and writable through other handles.
        let mut buf = String::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "contents");
        let mut c = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        c.write_all(b"!").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"contents!");
        unlock_byte(&a, FAR).unwrap();
        assert!(try_lock_byte(&b, FAR).unwrap());
        unlock_byte(&b, FAR).unwrap();
    }

    #[test]
    fn hard_links_share_the_lock() {
        let td = tempfile::tempdir().unwrap();
        let (one, two) = (td.path().join("one"), td.path().join("two"));
        std::fs::write(&one, b"x").unwrap();
        std::fs::hard_link(&one, &two).unwrap();
        let (a, b) = (open_rw(&one), open_rw(&two));
        assert!(try_lock_byte(&a, FAR).unwrap());
        assert!(!try_lock_byte(&b, FAR).unwrap());
        unlock_byte(&a, FAR).unwrap();
    }

    #[test]
    fn identity_is_the_file_not_the_name() {
        let td = tempfile::tempdir().unwrap();
        let (one, two, other) = (
            td.path().join("one"),
            td.path().join("two"),
            td.path().join("other"),
        );
        std::fs::write(&one, b"x").unwrap();
        std::fs::hard_link(&one, &two).unwrap();
        std::fs::write(&other, b"x").unwrap();
        let id = |p: &Path| file_identity(&open_rw(p)).unwrap();
        assert_eq!(id(&one), id(&two));
        assert_ne!(id(&one), id(&other));
        assert_ne!(id(&one).1, 0);
    }

    #[test]
    fn unlocking_what_is_not_locked_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        assert!(unlock_byte(&open_rw(&path), FAR).is_err());
    }

    #[test]
    fn the_offset_is_split_into_low_and_high_halves() {
        let o = overlapped_at(0x4000_0000_0000_0001);
        // SAFETY: both union views are plain integers and `overlapped_at`
        // initialised the `Anonymous` view.
        let (lo, hi) = unsafe {
            (
                o.Anonymous.Anonymous.Offset,
                o.Anonymous.Anonymous.OffsetHigh,
            )
        };
        assert_eq!((lo, hi), (1, 0x4000_0000));
    }

    #[test]
    fn moves_and_refuses_to_replace() {
        let td = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            td.path().join("a"),
            td.path().join("b"),
            td.path().join("c"),
        );
        std::fs::write(&a, b"first").unwrap();
        move_no_replace_write_through(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"first");

        std::fs::write(&c, b"second").unwrap();
        let e = move_no_replace_write_through(&c, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert_eq!(std::fs::read(&b).unwrap(), b"first");
        assert_eq!(std::fs::read(&c).unwrap(), b"second");
    }

    #[test]
    fn a_missing_source_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let e = move_no_replace_write_through(&td.path().join("nope"), &td.path().join("x"))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }

    #[test]
    fn interior_nul_is_refused_before_any_call() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("a");
        std::fs::write(&src, b"x").unwrap();
        let bad = std::path::PathBuf::from("a\u{0}b");
        let e = move_no_replace_write_through(&src, &bad).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        let e = move_no_replace_write_through(&bad, &td.path().join("y")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        assert!(src.exists());
    }

    /// The runner's temp directory is on NTFS (gate G6 is about NTFS).
    #[test]
    fn temp_directory_is_ntfs() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(volume_filesystem_name(td.path()).unwrap(), "NTFS");
    }

    #[test]
    fn filesystem_name_of_a_missing_directory_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        assert!(volume_filesystem_name(&td.path().join("nope")).is_err());
    }
}
