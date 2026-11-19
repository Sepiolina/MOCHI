//! Automation exit codes (spec §23.2) with the Annex B.2 D15 precedence.
//!
//! The registry and the precedence live in [`mochi_core::exit`], because a
//! report carries its exit code as a field; this module re-exports them so
//! the CLI's code paths name one set of constants. See that module for the
//! precedence table.

pub use mochi_core::exit::*;
