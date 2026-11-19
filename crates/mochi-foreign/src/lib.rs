//! Read-only foreign archive formats for MOCHI (plan D8): ZIP, tar, tar.gz,
//! tar.zst, 7z, and RAR.
//!
//! All parsing happens in the `mochi-foreign-helper` process; the app talks to
//! it through [`client::ForeignClient`]. This crate never writes a file, never
//! uses a library's extract-to-disk function, and never uses MOCHI's
//! verification wording (spec §20.4, §23.3). `mochi-core` and `mochi-format`
//! do not depend on it (`ci/check-invariants.sh`).

pub mod client;
#[cfg(windows)]
mod jobobject;
pub mod protocol;
pub mod types;

// Everything below parses archives and exists only with the `engine` feature,
// which the helper binary needs and the app does not.
#[cfg(feature = "engine")]
pub mod detect;
#[cfg(feature = "engine")]
pub mod engine;
#[cfg(feature = "engine")]
pub mod fuzz;
#[cfg(feature = "engine")]
pub mod helper;
#[cfg(feature = "engine")]
mod rarfmt;
#[cfg(feature = "engine")]
mod sevenfmt;
#[cfg(feature = "engine")]
mod tarfmt;
#[cfg(feature = "engine")]
pub mod volumes;
#[cfg(feature = "engine")]
mod zipfmt;
#[cfg(feature = "engine")]
pub(crate) mod zstdread;

pub use client::{
    default_helper_path, ForeignClient, ListReport, ReadManyReport, ReadManySink, ReadReport,
    RunLimits, TestEntry, TestReport,
};
pub use types::*;
