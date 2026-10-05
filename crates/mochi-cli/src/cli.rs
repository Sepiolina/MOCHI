//! Command surface (spec §23, §23.2 "1.0 command scope").
//!
//! At C0 no command is implemented. The *shape* is fixed so behaviour is
//! correct from day one:
//!
//! * commands in 1.0 scope that are not built yet exit 3 with `NOT_IMPLEMENTED`
//!   (a development-build condition, never a success);
//! * post-1.0 commands (`inventory`, `split`, `join`, `mount`) exit **4** with
//!   `UNSUPPORTED_FEATURE`, not 3 (spec §23.2);
//! * per-command arguments are accepted and ignored for now, so a post-1.0
//!   command with any arguments still reports 4, not a usage error. The real
//!   argument surface lands in phase C14.

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "mochi",
    version,
    about = "MOCHI archiver (experimental / draft-compatible; not a backup tool)",
    arg_required_else_help = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Arguments accepted (and ignored) until the real surface lands in C14.
#[derive(Debug, Args)]
pub struct PendingArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum RepairCommand {
    /// Produce a proposed repair plan (never mutates).
    Plan(PendingArgs),
    /// Apply an explicitly approved repair plan.
    Apply(PendingArgs),
}

#[derive(Debug, Subcommand)]
pub enum GcCommand {
    /// Identify collection candidates (never deletes).
    Plan(PendingArgs),
    /// Apply an approved collection plan.
    Apply(PendingArgs),
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an archive.
    Create(PendingArgs),
    /// Publish a new commit.
    Append(PendingArgs),
    /// Extract selected files.
    Get(PendingArgs),
    /// List a snapshot namespace.
    List(PendingArgs),
    /// Inspect or retain snapshots.
    Snapshot(PendingArgs),
    /// Search names, metadata, or content.
    Search(PendingArgs),
    /// Perform read-only verification.
    Verify(PendingArgs),
    /// Perform deep read-only consistency analysis.
    Fsck(PendingArgs),
    /// Summarize local health evidence.
    Health(PendingArgs),
    /// Restore into an isolated test destination.
    RestoreTest(PendingArgs),
    /// Plan or apply a repair.
    #[command(subcommand)]
    Repair(RepairCommand),
    /// Create a verified metadata checkpoint.
    Checkpoint(PendingArgs),
    /// Create a compacted representation.
    Compact(PendingArgs),
    /// Plan or apply garbage collection.
    #[command(subcommand)]
    Gc(GcCommand),
    /// Rotate or rewrap keys (Encrypted profile).
    Rekey(PendingArgs),
    /// Inspect catalog or index records.
    DumpIndex(PendingArgs),

    /// Post-1.0 (Preservation profile): exits 4 in this release line.
    Inventory(PendingArgs),
    /// Post-1.0 (Segmented profile): exits 4 in this release line.
    Split(PendingArgs),
    /// Post-1.0 (Segmented profile): exits 4 in this release line.
    Join(PendingArgs),
    /// Post-1.0: exits 4 in this release line.
    Mount(PendingArgs),
}

/// Whether a command exists in the 1.0 scope (spec §23.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// In 1.0 scope. Not yet implemented in this build.
    InScope,
    /// Explicitly post-1.0. Must exit 4.
    PostOneDotZero,
}

impl Command {
    /// The user-visible command name, as typed.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Create(_) => "create",
            Command::Append(_) => "append",
            Command::Get(_) => "get",
            Command::List(_) => "list",
            Command::Snapshot(_) => "snapshot",
            Command::Search(_) => "search",
            Command::Verify(_) => "verify",
            Command::Fsck(_) => "fsck",
            Command::Health(_) => "health",
            Command::RestoreTest(_) => "restore-test",
            Command::Repair(RepairCommand::Plan(_)) => "repair plan",
            Command::Repair(RepairCommand::Apply(_)) => "repair apply",
            Command::Checkpoint(_) => "checkpoint",
            Command::Compact(_) => "compact",
            Command::Gc(GcCommand::Plan(_)) => "gc plan",
            Command::Gc(GcCommand::Apply(_)) => "gc apply",
            Command::Rekey(_) => "rekey",
            Command::DumpIndex(_) => "dump-index",
            Command::Inventory(_) => "inventory",
            Command::Split(_) => "split",
            Command::Join(_) => "join",
            Command::Mount(_) => "mount",
        }
    }

    /// The catch-all arguments given after the subcommand.
    pub fn pending_args(&self) -> &[String] {
        match self {
            Command::Create(a)
            | Command::Append(a)
            | Command::Get(a)
            | Command::List(a)
            | Command::Snapshot(a)
            | Command::Search(a)
            | Command::Verify(a)
            | Command::Fsck(a)
            | Command::Health(a)
            | Command::RestoreTest(a)
            | Command::Repair(RepairCommand::Plan(a))
            | Command::Repair(RepairCommand::Apply(a))
            | Command::Checkpoint(a)
            | Command::Compact(a)
            | Command::Gc(GcCommand::Plan(a))
            | Command::Gc(GcCommand::Apply(a))
            | Command::Rekey(a)
            | Command::DumpIndex(a)
            | Command::Inventory(a)
            | Command::Split(a)
            | Command::Join(a)
            | Command::Mount(a) => &a.args,
        }
    }

    pub fn scope(&self) -> Scope {
        match self {
            Command::Inventory(_) | Command::Split(_) | Command::Join(_) | Command::Mount(_) => {
                Scope::PostOneDotZero
            }
            _ => Scope::InScope,
        }
    }
}
