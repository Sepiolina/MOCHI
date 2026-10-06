//! The `mochi` command-line tool (plan C14; skeleton at C0).
//!
//! Thin client of `mochi-core`: parse arguments, call the core, render a typed
//! result as text or JSON, and map it to an exit code (spec §23.2). No format
//! logic lives here.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod cli;
pub mod exit;

use std::ffi::OsString;
use std::io::Write;

use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches};
use mochi_core::{ErrorCode, MochiError};

use cli::{Cli, Command, Scope};

fn long_version() -> String {
    format!(
        "{}\nformat: {} (spec revision {}, wire generation {} proposed)\n\
         A .mochi file is not a backup. Do not use pre-1.0 builds as your only copy.",
        env!("CARGO_PKG_VERSION"),
        mochi_format::version::FORMAT_STATUS,
        mochi_format::version::SPEC_REVISION,
        mochi_format::version::WIRE_GENERATION,
    )
}

/// Map an error to its exit code (spec §23.2; Annex B.2.2 for the B.2 codes).
///
/// * **1**: evidence that the archive is wrong. Integrity failures, whatever
///   command found them, so `get` refusing a corrupt object exits like
///   `verify` finding it (plan O22); a missing, damaged, or mismatched
///   descriptor (D12); GC unable to rebuild retention state (D10 item 10).
/// * **4**: the build or the format cannot do what was asked: an unsupported
///   feature, or an in-place profile change (D12).
/// * **3**: everything else is operational, including `CHECKPOINT_MISMATCH`
///   raised by a *writer*. When `verify` finds a checkpoint mismatch it reports
///   a `FAIL` dimension, which exits 1 through the report (D15), not here.
///
/// The match is exhaustive on purpose: a new code must be placed explicitly.
pub fn exit_code_for(code: ErrorCode) -> u8 {
    match code {
        ErrorCode::StoredIntegrityFailed
        | ErrorCode::ContentIntegrityFailed
        | ErrorCode::DescriptorInvalid
        | ErrorCode::RetentionUnresolved
        | ErrorCode::ReferenceInvalid
        | ErrorCode::FreshnessFailed => exit::FAILED,
        ErrorCode::UnsupportedFeature | ErrorCode::ProfileChangeUnsupported => exit::UNSUPPORTED,
        ErrorCode::IoError
        | ErrorCode::OutOfBounds
        | ErrorCode::LimitExceeded
        | ErrorCode::LockConflict
        | ErrorCode::InvalidArgument
        | ErrorCode::Cancelled
        | ErrorCode::NotImplemented
        | ErrorCode::ReportInconsistent
        | ErrorCode::MalformedFrame
        | ErrorCode::Truncated
        | ErrorCode::FooterInvalid
        | ErrorCode::EnvelopeInvalid
        | ErrorCode::PathInvalid
        | ErrorCode::ExtentInvalid
        | ErrorCode::NamespaceInvalid
        | ErrorCode::CatalogInvalid
        | ErrorCode::IdentityConflict
        | ErrorCode::RecordInvalid
        | ErrorCode::NoValidHead
        | ErrorCode::UncommittedTail
        | ErrorCode::TailUnresolved
        | ErrorCode::CommitUnconfirmed
        | ErrorCode::WriterPoisoned
        | ErrorCode::DestinationExists
        | ErrorCode::CapacityExceeded
        | ErrorCode::CheckpointMismatch
        | ErrorCode::QuarantineFailed
        | ErrorCode::DurabilityUnconfirmed
        | ErrorCode::NameCollision
        | ErrorCode::NameUnsupported
        | ErrorCode::AttributeNotRestored => exit::ERROR,
    }
}

/// Options the spec names that this milestone defers. Each is refused by
/// name, never accepted and ignored, so nobody believes it took effect.
///
/// `create --exceed-default-limits` (spec Annex B.2.3, plan T29) is deferred
/// by owner decision Q10 (2026-10-06): B.2.3 does not yet say which limits a
/// writer may raise or by how much (spec Annex B, D16). Every writer keeps
/// the reader defaults. This stays in force when `create` itself is built.
fn deferred_option(command: &Command) -> Option<MochiError> {
    const EXCEED: &str = "--exceed-default-limits";
    let Command::Create(args) = command else {
        return None;
    };
    let asked = args
        .args
        .iter()
        .any(|a| a == EXCEED || a.starts_with("--exceed-default-limits="));
    asked.then(|| {
        MochiError::new(
            ErrorCode::NotImplemented,
            format!(
                "`{EXCEED}` is deferred in this build (spec Annex B, D16 is open); \
                 archives are created with the default limits only, and nothing was created"
            ),
        )
    })
}

fn error_for(command: &Command) -> MochiError {
    match command.scope() {
        Scope::PostOneDotZero => MochiError::new(
            ErrorCode::UnsupportedFeature,
            format!(
                "`mochi {}` is not part of MOCHI 1.0 (see spec Annex A); \
                 this build does not implement it",
                command.name()
            ),
        ),
        Scope::InScope => MochiError::new(
            ErrorCode::NotImplemented,
            format!(
                "`mochi {}` is in the 1.0 scope but is not implemented in this development build",
                command.name()
            ),
        ),
    }
}

fn render_error(err: &MochiError, json: bool, out: &mut dyn Write, errw: &mut dyn Write) {
    if json {
        let value = serde_json::json!({
            "error": { "code": err.code.as_str(), "message": err.message }
        });
        let _ = writeln!(out, "{value}");
    } else {
        let _ = writeln!(errw, "mochi: error[{}]: {}", err.code, err.message);
    }
}

/// Run the CLI. Returns the process exit code.
///
/// `args` includes the program name. Output goes to the supplied writers so the
/// whole surface is testable in-process.
pub fn run<I, T>(args: I, out: &mut dyn Write, errw: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cmd = Cli::command().long_version(long_version());
    let parsed = cmd
        .try_get_matches_from(args)
        .and_then(|m| Cli::from_arg_matches(&m));

    let cli = match parsed {
        Ok(cli) => cli,
        Err(e) => {
            let rendered = e.render().to_string();
            return match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    let _ = write!(out, "{rendered}");
                    exit::OK
                }
                // Everything else is an invocation error, including a missing
                // subcommand (clap prints help for it, but nothing was run).
                // clap's default usage-error exit code is 2, which the spec
                // reserves for degraded/unknown/overdue, so we use 3.
                _ => {
                    let _ = write!(errw, "{rendered}");
                    exit::ERROR
                }
            };
        }
    };

    // The spec's own examples put the flag last (`mochi verify a.mochi --json`,
    // spec §23.1), but until C14 defines real arguments the catch-all swallows
    // it, so honour `--json` wherever it appears.
    let json = cli.json || cli.command.pending_args().iter().any(|a| a == "--json");

    let err = deferred_option(&cli.command).unwrap_or_else(|| error_for(&cli.command));
    render_error(&err, json, out, errw);
    exit_code_for(err.code)
}
