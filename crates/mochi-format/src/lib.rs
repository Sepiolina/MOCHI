//! MOCHI wire-format primitives (spec §8–§9).
//!
//! This crate is pure: it has no filesystem policy and no I/O. Everything here
//! is **draft**: the wire format is not frozen (spec §28), so every build is
//! "experimental / draft-compatible".
//!
//! Phase status: C2. C1 framing primitives (structural frame walker, skippable
//! frame writer, framed footer, draft record envelope) plus the C2
//! representation types ([`repr`]), per-scope typed digests ([`digest`]), and
//! the unencrypted object codec ([`codec`]).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cbor;
pub mod codec;
pub mod digest;
pub mod envelope;
pub mod error;
pub mod footer;
pub mod frame;
pub mod kdf;
pub mod limits;
pub mod registry;
pub mod repr;
pub mod seal;
pub mod secret;
pub mod source;
pub mod version;

pub use error::{ErrorClass, FormatError};
pub use limits::Limits;
pub use source::{ReadAt, ReadError};
