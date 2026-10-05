//! Test harness for MOCHI: fault-injecting storage and fixture helpers
//! (plan §2.1, C0).
//!
//! [`SimStorage`] implements `mochi_core::storage::Storage` in memory, models
//! the difference between *written* and *durable* bytes, and can be told to
//! halt, tear, fail, or lie at exact points. That is what makes the §24.2 fault
//! matrix testable. This crate is test infrastructure, so it may unwrap freely;
//! `mochi-core` and `mochi-format` may not.

pub mod archive;
mod fixtures;
pub mod forge;
pub mod fuzz;
pub mod golden;
pub mod history;
pub mod replay;
mod sim;

pub use fixtures::{deterministic_bytes, SeqIds};
pub use sim::{is_halted, CrashMode, Fault, Halted, Op, SimStorage};
