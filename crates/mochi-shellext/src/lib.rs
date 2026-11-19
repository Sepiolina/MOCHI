//! The MOCHI Explorer context-menu handler (plan D6, decision O10, owner
//! decision 2026-10-20): a classic `IContextMenu` shell extension, an
//! in-process COM DLL that the per-user installer registers under
//! `HKCU\Software\Classes` for every file and folder, and for a folder's
//! background. It adds up to three entries: "Add to archive..." and the dynamic
//! "Add to \"<Name>.mochi\"" (any selection), and "Find in MOCHI archives" (only
//! when the selection holds a folder, and alone on a folder's background).
//! Windows 11 shows them under "Show more options"; the top-level menu needs
//! a signed package and stays 1.x.
//!
//! The DLL only starts `mochi-desktop.exe` (next to the DLL) with
//! `--add|--add-now|--find -- PATH...` through `std::process::Command`, never a
//! shell string. It parses no archive and writes nothing itself. Empty on other
//! platforms.

#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_op_in_unsafe_fn))]

pub mod plan;

#[cfg(windows)]
mod shell;

// The DLL's exports (also forced by `#[no_mangle]`; re-exported so they are plainly public).
#[cfg(windows)]
pub use shell::{DllCanUnloadNow, DllGetClassObject, DllMain};
