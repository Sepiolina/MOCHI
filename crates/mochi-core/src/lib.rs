//! `mochi-core`: archive operations for MOCHI (plan §2.1).
//!
//! Dependency direction is one-way: `mochi-format` ← `mochi-core` ←
//! {`mochi-cli`, desktop backend}. Nothing here knows about Tauri.
//!
//! Rules enforced for this crate (AGENTS.md):
//! * all I/O goes through [`storage::Storage`]; the single OS-backed
//!   implementation lives in `storage/os.rs`, the only file allowed to touch
//!   `std::fs` (checked by `ci/check-invariants.sh`);
//! * no `unwrap`/`expect`/`panic!` on paths reachable from archive contents;
//! * long operations are jobs with a progress sink and a cancellation token.
//!
//! Phase status: C5. Harness pieces from C0 (error-code registry, status
//! values, report schema v0, jobs layer, storage trait); the C2 object model
//! ([`object`]); the C3 catalog ([`catalog`]); C4 recovery manifests
//! ([`manifest`], [`recovery`]); and C5 commit records ([`commit`]) and
//! single-file publication ([`publish`]): the §12.2 protocol, head location
//! with interrupted-tail detection, and hash-verified opening. Replay
//! segments ([`segment`]), the authoritative-state model ([`state`]), and
//! baseline recovery from a snapshot manifest (T16, in [`publish`]) follow
//! Annex B.2 D10.4–D10.9.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod catalog;
pub mod commit;
pub mod damage;
pub mod descriptor;
pub mod error;
pub mod image;
pub mod job;
pub mod manifest;
pub mod object;
pub mod publish;
pub mod recovery;
pub mod report;
pub mod segment;
pub mod state;
pub mod status;
pub mod storage;

pub use error::{ErrorCode, MochiError, Result};

/// Name reported in `tool` fields of reports.
pub const TOOL_NAME: &str = "mochi-core";
/// Version of this build.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
