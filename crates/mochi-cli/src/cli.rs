//! Command surface (spec §23, §23.2 "1.0 command scope"; plan C14).
//!
//! * Built: `create`, `append`, `list`, `get`, `snapshot list`, `snapshot
//!   retain`/`expire`/`release`, `search`, `verify`, `fsck`, `restore-test`,
//!   `checkpoint`, `compact`, `gc plan`, `gc apply`, `repair plan`,
//!   `repair apply`, `rekey`.
//! * In 1.0 scope but not built yet: exit 3 with `NOT_IMPLEMENTED` (a
//!   development-build condition, never a success). Their arguments are
//!   accepted and ignored, so the refusal names the command.
//! * Post-1.0 (`inventory`, `split`, `join`, `mount`): exit **4** with
//!   `UNSUPPORTED_FEATURE`, not 3 (spec §23.2), whatever their arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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

    /// Directory for this client's local evidence: the last-seen head of
    /// each archive, used as a freshness anchor (spec Annex B.1 D8).
    /// Default: `$MOCHI_STATE_DIR`, else the platform's per-user state
    /// directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    /// Neither read nor record last-seen heads (D8 opt-out). Freshness is
    /// then `UNKNOWN` unless `--expected-head` is given.
    #[arg(long, global = true)]
    pub no_local_history: bool,

    /// Reader limit for untrusted archives (spec §8.5), repeatable:
    /// `max-skippable-payload`, `max-frame-len`, `max-blocks-per-frame`,
    /// `max-window-size`, `max-commit-frame-len`, `max-decoded-object-len`,
    /// `max-required-features`. Example: `--limit max-frame-len=1048576`.
    #[arg(long = "limit", global = true, value_name = "NAME=VALUE")]
    pub limits: Vec<String>,

    /// A file whose first line is a passphrase for an Encrypted archive
    /// (read once), repeatable. A passphrase is never an argument value. For
    /// `create --encrypted` and the rewrites, each file adds one key
    /// envelope to the new archive. Without a file or the environment
    /// switch, commands that need a passphrase ask for it on the terminal.
    #[arg(long = "passphrase-file", global = true, value_name = "PATH")]
    pub passphrase_files: Vec<PathBuf>,

    /// Read the passphrase from the environment variable MOCHI_PASSPHRASE.
    /// For automation only: a process environment is readable by more than
    /// its owner, so prefer --passphrase-file or the prompt.
    #[arg(long, global = true)]
    pub passphrase_env_for_automation: bool,

    #[command(subcommand)]
    pub command: Command,
}

/// Argon2id cost of the key envelopes a command writes (spec Annex B.2.10 D20
/// item 1). Defaults: 64 MiB, 3 passes, 4 lanes. Readers accept what the
/// archive declares, up to their limits.
#[derive(Debug, Args, Clone, Copy, Default)]
pub struct KdfArgs {
    /// Argon2id memory in KiB.
    #[arg(long, value_name = "KIB")]
    pub kdf_memory_kib: Option<u64>,
    /// Argon2id passes.
    #[arg(long, value_name = "N")]
    pub kdf_iterations: Option<u64>,
    /// Argon2id lanes (and threads).
    #[arg(long, value_name = "N")]
    pub kdf_lanes: Option<u64>,
}

/// Arguments accepted (and ignored) by commands that are not built yet.
#[derive(Debug, Args)]
pub struct PendingArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct CreateArgs {
    /// The archive to create. It must not exist; nothing is ever replaced.
    pub archive: PathBuf,
    /// Files and directories to add. Each becomes a top-level entry named
    /// after its last component.
    #[arg(num_args = 0..)]
    pub inputs: Vec<PathBuf>,
    /// Zstandard compression level.
    #[arg(long, value_name = "N", allow_negative_numbers = true)]
    pub compression_level: Option<i32>,
    /// Chunk size in bytes (fixed for the archive's life).
    #[arg(long, value_name = "BYTES")]
    pub chunk_size: Option<u64>,
    /// Create a TAR-compatible archive (spec 7.2, Annex B.2.9 D19): every
    /// commit also writes a TAR stream, so `zstd -dc` of the file yields the
    /// commits' streams. The choice is fixed for the archive's life, turns
    /// deduplication off, and costs space (see docs/c10-tar-compat.md).
    #[arg(long, conflicts_with = "encrypted")]
    pub tar_compatible: bool,
    /// Create an Encrypted archive (spec Annex B.2.10 D20): names, content,
    /// sizes, and structure are sealed under a random data key that each
    /// passphrase wraps. The choice is fixed for the archive's life. The
    /// passphrase comes from --passphrase-file (repeatable), the environment
    /// switch, or a prompt asked twice. Losing every passphrase loses the
    /// archive: there is no recovery.
    #[arg(long)]
    pub encrypted: bool,
    #[command(flatten)]
    pub kdf: KdfArgs,
    /// Most content bytes one commit may hold in memory.
    #[arg(long, value_name = "BYTES")]
    pub max_memory: Option<u64>,
    /// Deferred (plan Q10, spec Annex B D16): refused by name.
    #[arg(long, hide = true, num_args = 0..=1, require_equals = true, value_name = "V")]
    pub exceed_default_limits: Option<Option<String>>,
}

#[derive(Debug, Args)]
pub struct AppendArgs {
    /// The archive to append a commit to.
    pub archive: PathBuf,
    /// Files and directories to add or replace (top-level entries named
    /// after their last component).
    #[arg(num_args = 0..)]
    pub inputs: Vec<PathBuf>,
    /// Remove an entry (and nothing else) from the new snapshot. Its bytes
    /// stay in the archive's history (spec §10.2, §23.3 #5). Repeatable.
    #[arg(long, value_name = "ARCHIVE_PATH")]
    pub delete: Vec<String>,
    /// Most content bytes one commit may hold in memory.
    #[arg(long, value_name = "BYTES")]
    pub max_memory: Option<u64>,
    /// Remove an eligible uncommitted tail first (Annex B.2 D14), after
    /// copying it to a verified quarantine sidecar.
    #[arg(long)]
    pub truncate_tail: bool,
    /// Waiver: truncate without the quarantine copy. Recorded in the output.
    #[arg(long, requires = "truncate_tail")]
    pub no_quarantine: bool,
    /// Waiver: truncate although the sidecar's directory entry is not
    /// confirmed durable (always so on Windows before gate G6). Recorded.
    #[arg(long, requires = "truncate_tail")]
    pub accept_unconfirmed_durability: bool,
}

/// Which commit to read.
#[derive(Debug, Args)]
pub struct SnapshotSel {
    /// Commit sequence to read instead of the head.
    #[arg(long, value_name = "SEQ")]
    pub snapshot: Option<u64>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    pub archive: PathBuf,
    /// Only this entry and what lies under it.
    pub under: Option<String>,
    #[command(flatten)]
    pub sel: SnapshotSel,
}

#[derive(Debug, Args)]
pub struct HealthArgs {
    pub archive: PathBuf,
    /// A JSON health policy (schema `mochi-health-policy-v1`): the required
    /// dimensions and how old each kind of evidence may be. Without it:
    /// integrity and recoverability required, `verify` evidence at most 30
    /// days old.
    #[arg(long, value_name = "FILE.json")]
    pub policy: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DumpIndexArgs {
    pub archive: PathBuf,
    #[command(flatten)]
    pub sel: SnapshotSel,
    /// Show the rows of this table (repeatable). Without it, text output
    /// lists each table with its row count and `--json` carries every
    /// table. A name that is not a table of the catalog is refused.
    #[arg(long = "table", value_name = "NAME")]
    pub tables: Vec<String>,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    pub archive: PathBuf,
    /// Entries to extract, with everything under them. None: all.
    #[arg(num_args = 0..)]
    pub paths: Vec<String>,
    /// Directory to extract into (created if missing; its parent must
    /// exist). Nothing in it is ever overwritten.
    #[arg(long, short = 'C', value_name = "DIR", conflicts_with = "stdout")]
    pub destination: Option<PathBuf>,
    /// Write one file's bytes to standard output instead.
    #[arg(long)]
    pub stdout: bool,
    /// Restore setuid and setgid bits (off by default: from an untrusted
    /// archive they are a privilege-escalation risk, plan O6).
    #[arg(long)]
    pub restore_setid: bool,
    /// Write nothing if any entry would collide or has a name the
    /// destination cannot hold.
    #[arg(long)]
    pub refuse_on_conflict: bool,
    #[command(flatten)]
    pub sel: SnapshotSel,
}

#[derive(Debug, Args)]
pub struct RestoreTestArgs {
    pub archive: PathBuf,
    /// A new, isolated directory to restore into. It must not exist.
    #[arg(long, short = 'C', value_name = "DIR")]
    pub destination: PathBuf,
    #[command(flatten)]
    pub sel: SnapshotSel,
}

/// Which snapshots `search` looks in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchSnapshots {
    Head,
    Retained,
    All,
    Commit(u64),
}

fn parse_search_snapshots(s: &str) -> Result<SearchSnapshots, String> {
    match s {
        "HEAD" | "head" => Ok(SearchSnapshots::Head),
        "retained" => Ok(SearchSnapshots::Retained),
        "all" => Ok(SearchSnapshots::All),
        _ => s
            .parse()
            .map(SearchSnapshots::Commit)
            .map_err(|_| "expected HEAD, retained, all, or a commit sequence".to_string()),
    }
}

/// Entry kinds, as `search --kind` takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    File,
    Dir,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    pub archive: PathBuf,
    /// Bytes that must occur in the entry's path. Omitted: every entry.
    pub pattern: Option<String>,
    /// Snapshots to search: `HEAD`, `retained` (every snapshot retained at
    /// the head), `all` (every commit, expired or not), or a sequence.
    #[arg(long, value_name = "SCOPE", default_value = "HEAD", value_parser = parse_search_snapshots)]
    pub snapshot: SearchSnapshots,
    /// Fold ASCII letters when matching PATTERN (results keep exact names).
    #[arg(long, short = 'i')]
    pub ignore_case: bool,
    /// Only this exact archive path.
    #[arg(long, value_name = "ARCHIVE_PATH", conflicts_with = "under")]
    pub path: Option<String>,
    /// Only this archive path and what lies under it.
    #[arg(long, value_name = "ARCHIVE_PATH")]
    pub under: Option<String>,
    /// Only this file version (64 hex digits, as `search --json` prints).
    #[arg(long, value_name = "VERSION_ID")]
    pub version: Option<String>,
    /// Only files with this file-content hash: plain BLAKE3 of the file,
    /// as `b3sum` prints it (64 hex digits).
    #[arg(long, value_name = "HASH")]
    pub content_hash: Option<String>,
    /// Only files or only directories.
    #[arg(long, value_enum)]
    pub kind: Option<KindArg>,
    /// Search file content. Full-text search (spec §19.2) is not built:
    /// refused with exit 4, never answered from names.
    #[arg(long, value_name = "TEXT")]
    pub content: Option<String>,
    /// Treat partial coverage as a failure (exit 1) instead of degraded
    /// evidence (exit 2).
    #[arg(long)]
    pub require_complete: bool,
}

/// Verification levels (spec §20.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Level {
    Structural,
    Referential,
    Stored,
    Content,
    Restoration,
    Inventory,
    Search,
    DisasterRecovery,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    pub archive: PathBuf,
    /// How deep to check; each level includes the ones before it.
    #[arg(long, value_enum, default_value = "restoration")]
    pub level: Level,
    /// The head you expect (a commit ID, 64 hex digits): freshness passes
    /// only if this archive's history contains it (spec §5.7).
    #[arg(long, value_name = "COMMIT_ID")]
    pub expected_head: Option<String>,
    /// Require freshness evidence even without an anchor (then exit 2).
    #[arg(long)]
    pub require_freshness: bool,
    /// Ask for the passphrase of an Encrypted archive. Without a passphrase
    /// source (this, --passphrase-file, or the environment switch) an
    /// Encrypted archive is verified without a key: stored integrity only,
    /// with content, recoverability, and key availability `UNKNOWN`.
    #[arg(long)]
    pub ask_passphrase: bool,
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// List the archive's commits, oldest first, with their retention.
    List(SnapshotListArgs),
    /// Place a legal hold: the snapshot stays retained, expired or not,
    /// until the hold is released (spec §16.3). One new commit.
    #[command(visible_alias = "hold")]
    Retain(RetainArgs),
    /// Expire earlier snapshots: they stop being retained unless held, and
    /// a later `gc` may leave them out of a new archive. Irreversible; needs
    /// `--confirm`. One new commit.
    Expire(ExpireArgs),
    /// Release a legal hold. Needs `--confirm`. One new commit.
    Release(ReleaseArgs),
}

#[derive(Debug, Args)]
pub struct RetainArgs {
    pub archive: PathBuf,
    /// The commit sequence to hold.
    pub snapshot: u64,
    /// The hold's label (1 to 255 bytes, unique among active holds).
    #[arg(long, value_name = "LABEL")]
    pub label: String,
}

#[derive(Debug, Args)]
pub struct ExpireArgs {
    pub archive: PathBuf,
    /// Commit sequences to expire (earlier than the head).
    #[arg(required = true, num_args = 1..)]
    pub snapshots: Vec<u64>,
    /// Confirm the retention reduction (spec §16.3).
    #[arg(long)]
    pub confirm: bool,
}

#[derive(Debug, Args)]
pub struct ReleaseArgs {
    pub archive: PathBuf,
    /// The label of the hold to release.
    #[arg(long, value_name = "LABEL")]
    pub label: String,
    /// Confirm the retention reduction (spec §16.3).
    #[arg(long)]
    pub confirm: bool,
}

#[derive(Debug, Args)]
pub struct CheckpointArgs {
    /// The archive to write a checkpoint commit to.
    pub archive: PathBuf,
}

#[derive(Debug, Args)]
pub struct CompactArgs {
    /// The source archive. It is read under its publication lock and never
    /// written or removed.
    pub archive: PathBuf,
    /// The new archive. It must not exist; nothing is ever replaced.
    #[arg(long, short = 'o', value_name = "NEW_ARCHIVE")]
    pub output: PathBuf,
    /// Skip reading back and verifying every file version before
    /// publication (spec §18.2 step 3). Recorded in the output.
    #[arg(long)]
    pub no_verify_content: bool,
    #[command(flatten)]
    pub kdf: KdfArgs,
}

#[derive(Debug, Args)]
pub struct GcPlanArgs {
    pub archive: PathBuf,
    /// Write the plan (JSON) to this file, which must not exist. Without
    /// it, `--json` prints the plan and text mode summarizes it.
    #[arg(long, short = 'o', value_name = "PLAN_FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct GcApplyArgs {
    /// The source archive the plan was made for.
    pub archive: PathBuf,
    /// The plan written by `mochi gc plan --output`.
    #[arg(long, value_name = "PLAN_FILE")]
    pub plan: PathBuf,
    /// The new archive. It must not exist; nothing is ever replaced.
    #[arg(long, short = 'o', value_name = "NEW_ARCHIVE")]
    pub output: PathBuf,
    /// Skip reading back and verifying every file version before
    /// publication (spec §18.2 step 3). Recorded in the output.
    #[arg(long)]
    pub no_verify_content: bool,
    #[command(flatten)]
    pub kdf: KdfArgs,
}

#[derive(Debug, Args)]
pub struct RekeyArgs {
    pub archive: PathBuf,
    /// Show the head's key envelopes (ID, the commit that wrote it, KDF
    /// cost). Needs no passphrase.
    #[arg(long, conflicts_with_all = ["add_passphrase", "remove_passphrase", "reencrypt"])]
    pub list: bool,
    /// Add a passphrase (a rewrap): one new commit holding a new key
    /// envelope. The data key does not change and no data is rewritten. The
    /// new passphrase comes from --new-passphrase-file or a prompt asked
    /// twice; the passphrase that opens the archive is the global one.
    #[arg(long, conflicts_with = "reencrypt")]
    pub add_passphrase: bool,
    /// Remove the key envelope with this ID (a rewrap; see --list). This is
    /// NOT revocation: the envelope stays in the file's history and any copy
    /// made before still opens with that passphrase. Repeatable.
    #[arg(long, value_name = "ENVELOPE_ID", conflicts_with = "reencrypt")]
    pub remove_passphrase: Vec<String>,
    /// Write a new archive (`--output`) under a new data key and the new
    /// passphrases, every snapshot kept. Never in place. Does not recall
    /// copies already made.
    #[arg(long, requires = "output")]
    pub reencrypt: bool,
    /// The new archive for --reencrypt. It must not exist.
    #[arg(long, short = 'o', value_name = "NEW_ARCHIVE")]
    pub output: Option<PathBuf>,
    /// A file whose first line is a new passphrase for --add-passphrase or
    /// --reencrypt, repeatable.
    #[arg(long, value_name = "PATH")]
    pub new_passphrase_file: Vec<PathBuf>,
    #[command(flatten)]
    pub kdf: KdfArgs,
}

#[derive(Debug, Args)]
pub struct SnapshotListArgs {
    pub archive: PathBuf,
}

#[derive(Debug, Args)]
pub struct RepairPlanArgs {
    pub archive: PathBuf,
    /// Write the plan (JSON) to this file, which must not exist. Without
    /// it, `--json` prints the plan and text mode summarizes it.
    #[arg(long, short = 'o', value_name = "PLAN_FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct RepairApplyArgs {
    /// The damaged source archive the plan was made for. It is only read.
    pub archive: PathBuf,
    /// The plan written by `mochi repair plan --output`: the approval.
    #[arg(long, value_name = "PLAN_FILE")]
    pub plan: PathBuf,
    /// The new archive. It must not exist; nothing is ever replaced.
    #[arg(long, short = 'o', value_name = "NEW_ARCHIVE")]
    pub output: PathBuf,
    /// Write the repair although the source's retention state cannot be
    /// rebuilt: the new archive then has no legal holds and nothing
    /// expired. Recorded in the output.
    #[arg(long)]
    pub accept_retention_loss: bool,
    #[command(flatten)]
    pub kdf: KdfArgs,
}

#[derive(Debug, Subcommand)]
pub enum RepairCommand {
    /// Produce a proposed repair plan (never mutates).
    Plan(RepairPlanArgs),
    /// Apply an explicitly approved repair plan, writing a new archive.
    /// The source is kept.
    Apply(RepairApplyArgs),
}

#[derive(Debug, Subcommand)]
pub enum GcCommand {
    /// Identify collection candidates (never deletes).
    Plan(GcPlanArgs),
    /// Apply an approved collection plan by writing a new archive without
    /// what it collects. The source is kept.
    Apply(GcApplyArgs),
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an archive.
    Create(CreateArgs),
    /// Publish a new commit.
    Append(AppendArgs),
    /// Extract selected files.
    Get(GetArgs),
    /// List a snapshot namespace.
    List(ListArgs),
    /// Inspect or retain snapshots.
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Find entries by name, path, file version, or content hash, in one
    /// snapshot or across snapshots.
    Search(SearchArgs),
    /// Perform read-only verification.
    Verify(VerifyArgs),
    /// Perform deep read-only consistency analysis.
    Fsck(VerifyArgs),
    /// Summarize local health evidence (runs no check).
    Health(HealthArgs),
    /// Restore into an isolated test destination.
    RestoreTest(RestoreTestArgs),
    /// Plan or apply a repair.
    #[command(subcommand)]
    Repair(RepairCommand),
    /// Create a verified metadata checkpoint.
    Checkpoint(CheckpointArgs),
    /// Create a compacted representation (a new archive; the source is kept).
    Compact(CompactArgs),
    /// Plan or apply garbage collection.
    #[command(subcommand)]
    Gc(GcCommand),
    /// List, add, or remove passphrases of an Encrypted archive, or re-encrypt
    /// it into a new archive.
    Rekey(RekeyArgs),
    /// Dump the catalog of a commit, table by table (read-only).
    DumpIndex(DumpIndexArgs),

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
    /// Built in this build.
    Built,
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
            Command::Snapshot(SnapshotCommand::List(_)) => "snapshot list",
            Command::Snapshot(SnapshotCommand::Retain(_)) => "snapshot retain",
            Command::Snapshot(SnapshotCommand::Expire(_)) => "snapshot expire",
            Command::Snapshot(SnapshotCommand::Release(_)) => "snapshot release",
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

    /// The catch-all arguments of a command that is not built.
    pub fn pending_args(&self) -> &[String] {
        match self {
            Command::Inventory(a) | Command::Split(a) | Command::Join(a) | Command::Mount(a) => {
                &a.args
            }
            _ => &[],
        }
    }

    pub fn scope(&self) -> Scope {
        match self {
            Command::Inventory(_) | Command::Split(_) | Command::Join(_) | Command::Mount(_) => {
                Scope::PostOneDotZero
            }
            Command::Create(_)
            | Command::Append(_)
            | Command::Get(_)
            | Command::List(_)
            | Command::Search(_)
            | Command::Snapshot(_)
            | Command::Verify(_)
            | Command::Fsck(_)
            | Command::DumpIndex(_)
            | Command::Health(_)
            | Command::RestoreTest(_)
            | Command::Checkpoint(_)
            | Command::Compact(_)
            | Command::Gc(_)
            | Command::Rekey(_)
            | Command::Repair(_) => Scope::Built,
        }
    }
}
