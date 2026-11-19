//! One Open for every format (plan D1, D8): which reader a file the user
//! chose belongs to, and the single entry point the open dialog, the recent
//! list, a dropped file, and the shell's open verb share.
//!
//! The rule is **MOCHI wins**: a file that starts with the MOCHI archive
//! descriptor frame (spec Annex B.2 D12: the only valid offset is 0) is a
//! MOCHI archive whatever its name says. This matters because a MOCHI archive
//! in the TAR-compatible profile is also a valid zstd stream holding a tar
//! stream, so `x.tar.zst` may be either. Everything else is routed by name
//! ([`foreign_kind`]) and, for a name that says nothing, by the first bytes of
//! the common foreign formats. Both are hints only: the isolated helper
//! decides what the bytes are and answers `NOT_AN_ARCHIVE` or
//! `UNSUPPORTED_FORMAT` for anything it will not read. No archive is parsed in
//! this process.

use std::io::Read;
use std::path::Path;

use mochi_format::registry::ARCHIVE_DESCRIPTOR;

use crate::browse::Archives;
use crate::foreign::Foreign;
use crate::ipc::{IpcError, OpenOutcome};
use crate::launch::foreign_kind;

/// The reader a file goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Mochi,
    Foreign,
}

/// Whether the file starts with the MOCHI archive descriptor frame. `false`
/// when the file cannot be read: opening reports that.
pub fn has_mochi_magic(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 4];
    f.read_exact(&mut head).is_ok() && head == ARCHIVE_DESCRIPTOR.to_le_bytes()
}

/// The first bytes of a foreign format the helper reads, for a file whose
/// name says nothing. A routing hint only; the helper checks the rest.
fn foreign_magic(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::with_capacity(8);
    if f.by_ref().take(8).read_to_end(&mut head).is_err() {
        return false;
    }
    [
        &b"PK\x03\x04"[..],
        b"PK\x05\x06",
        b"7z\xBC\xAF\x27\x1C",
        b"Rar!\x1A\x07",
        &[0x1f, 0x8b],
        &[0x28, 0xB5, 0x2F, 0xFD],
    ]
    .iter()
    .any(|magic| head.starts_with(magic))
}

/// Which reader opens `path`: MOCHI by its magic, then foreign by name, then
/// foreign by magic, else MOCHI (whose open reports what is wrong with it).
pub fn route(path: &Path) -> Route {
    if has_mochi_magic(path) {
        Route::Mochi
    } else if foreign_kind(path).is_some() || foreign_magic(path) {
        Route::Foreign
    } else {
        Route::Mochi
    }
}

/// What a single dropped file is, judged by its name alone: dropping a file
/// to *add* it must not be hijacked by a `.docx` (a ZIP inside) or a lone
/// `.gz`, so the first bytes are not consulted for foreign formats. `None`
/// means "an ordinary file to stage". MOCHI still wins by magic.
pub fn dropped_archive(path: &Path) -> Option<Route> {
    if !path.is_file() {
        return None;
    }
    if has_mochi_magic(path) {
        return Some(Route::Mochi);
    }
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mochi"))
    {
        return Some(Route::Mochi);
    }
    // A bare `.gz` is a gzipped single file, not an archive of several.
    match foreign_kind(path) {
        Some("gzip") | None => None,
        Some(_) => Some(Route::Foreign),
    }
}

/// Open `path` with the reader it belongs to. A foreign archive that needs a
/// password is [`OpenOutcome::ForeignPassword`] (Rust keeps the file; the
/// window retries with `open_foreign_archive`), as an Encrypted MOCHI archive
/// is [`OpenOutcome::NeedsPassphrase`].
pub fn open_path(
    archives: &Archives,
    foreign: &Foreign,
    path: &Path,
) -> Result<OpenOutcome, IpcError> {
    open_path_as(archives, foreign, crate::browse::UNOWNED, path)
}

/// [`open_path`] for the window `owner`: the archive (or the wait for its
/// passphrase or password) belongs to that window.
pub fn open_path_as(
    archives: &Archives,
    foreign: &Foreign,
    owner: &str,
    path: &Path,
) -> Result<OpenOutcome, IpcError> {
    match route(path) {
        Route::Mochi => archives
            .open_or_pend_as(owner, path)
            .map_err(IpcError::from),
        Route::Foreign => open_foreign_as(foreign, owner, path),
    }
}

/// Open `path` as a foreign archive, answering a password request as an
/// outcome rather than an error.
pub fn open_foreign(foreign: &Foreign, path: &Path) -> Result<OpenOutcome, IpcError> {
    open_foreign_as(foreign, crate::browse::UNOWNED, path)
}

/// [`open_foreign`] for the window `owner`.
pub fn open_foreign_as(
    foreign: &Foreign,
    owner: &str,
    path: &Path,
) -> Result<OpenOutcome, IpcError> {
    match foreign.open_as(owner, path, None) {
        Ok(archive) => Ok(OpenOutcome::Foreign { archive }),
        Err(e) => {
            let wrong = e.code == mochi_foreign::ErrorCode::WrongPassword.as_str();
            if wrong || e.code == mochi_foreign::ErrorCode::PasswordRequired.as_str() {
                Ok(OpenOutcome::ForeignPassword {
                    wrong,
                    file_name: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                })
            } else {
                Err(e)
            }
        }
    }
}
