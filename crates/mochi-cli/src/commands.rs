//! The built commands (plan C14). Each is a thin adapter: parse, call
//! `mochi-core`, render a typed result as text or JSON, return the exit
//! code (spec §23.2). Rules the CLI adds on top of the core are listed in
//! `docs/cli.md`.

use std::io::Write;
use std::path::{Path, PathBuf};

use mochi_core::catalog::dump::DumpValue;
use mochi_core::catalog::namespace::EntryKind;
use mochi_core::catalog::namespace::FileVersionId;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::compact::{self, CompactOptions, Keep};
use mochi_core::copies::CopySource;
use mochi_core::descriptor::{DraftPolicy, Profile};
use mochi_core::exit;
use mochi_core::find::{
    EntryFilter, Limits as FindLimits, MatchOptions, Matcher, NoCache, Scan, Skip, TermSet,
    MAX_QUERY_BYTES, MAX_SCOPES, MAX_TERMS,
};
use mochi_core::gc::{self, GcPlan, PlanHead};
use mochi_core::health::{
    self, AnchorRef, CurrentHead, EvidenceCommand, EvidenceHead, EvidenceRecord, HealthPolicy,
    Inputs, EVIDENCE_SCHEMA,
};
use mochi_core::import::{import_recording, ImportOptions, ImportedEntry, SourceKind};
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::migrate::{self, MigrateOptions};
use mochi_core::object::{ArchiveId, OsIds};
use mochi_core::publish::CatalogSource;
use mochi_core::publish::{
    commit_history, locate_head, open_at_footer, open_head, read_commit, segment_state,
    ArchiveWriter, CommitOutcome, OpenedHead, PublishDurability, ReadOptions, TailRepair,
    TailTruncation, Transaction, TruncationWaivers, WriterOptions,
};
use mochi_core::read::{list, read_file};
use mochi_core::repair::{self, Outcome, RepairOptions, RepairPlan, RetentionPlan};
use mochi_core::report::{Finding, Report, Severity};
use mochi_core::restore::{restore_selected, ExceptionKind, RestoreOptions, RestoreReport};
use mochi_core::salvage::SalvageOptions;
use mochi_core::search::{search, FullText, PathMatch, Query, SnapshotScope};
use mochi_core::sources::check_removable;
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::storage::os::find::normalize_scopes;
use mochi_core::storage::os::move_sources::{move_removable, MovedSources};
use mochi_core::storage::os::SetAside;
use mochi_core::storage::os::WindowsDirectoryFlush;
use mochi_core::storage::os::{
    OsDir, OsFindTree, OsReadStorage, OsRestoreDir, OsSourceTree, OsStorage,
};
use mochi_core::timestamp::Timestamp;
use mochi_core::verify::{
    judge_freshness_of, parse_commit_id, verify, FreshnessAnchor, FreshnessJudgement, VerifyOptions,
};
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_format::digest::FileContentHash;
use serde_json::{json, Value};

use crate::cli::{
    AppendArgs, CheckpointArgs, CommentArgs, CompactArgs, CreateArgs, DumpIndexArgs, ExpireArgs,
    FindArgs, GcApplyArgs, GcPlanArgs, GetArgs, HealthArgs, KindArg, Level, ListArgs, MigrateArgs,
    RekeyArgs, ReleaseArgs, RepairApplyArgs, RepairPlanArgs, RestoreTestArgs, RetainArgs,
    SearchArgs, SearchSnapshots, SnapshotListArgs, VerifyArgs,
};
use crate::passphrase::{self, Sources};
use crate::render::{archive_path_arg, hex, os_path_text, path_fields, text};
use crate::state::{HeadStore, SeenHead};

/// What every command gets from the global options.
pub struct Env<'a> {
    pub json: bool,
    /// What a directory flush counts as on Windows when an archive or sidecar
    /// is created (spec Annex B.2.20, D30). Policy B unless the user chose.
    pub flush: WindowsDirectoryFlush,
    /// `--wait-for-lock`: how long an append waits for another writer.
    pub lock_wait: Option<std::time::Duration>,
    /// Where `--move-sources-to-bin` moves a source: the Recycle Bin or
    /// Trash (`mochi_cli::run`), or a folder (`mochi_cli::run_with_bin`, tests).
    pub set_aside: std::sync::Arc<SetAside>,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub read: ReadOptions,
    /// `None` with `--no-local-history`, or when no location is known.
    pub store: Option<HeadStore>,
    /// Where passphrases may come from (`--passphrase-file`, the
    /// environment switch).
    pub pass: Sources,
}

/// Whether a command needs the passphrase of an Encrypted archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// Yes: from the sources named, else a prompt, else `KEY_UNAVAILABLE`.
    Required,
    /// Only if a source was named (`verify` without one runs keyless).
    IfGiven,
}

/// Whether the head commit of `archive` says it is Encrypted. Needs no key;
/// anything that goes wrong reads as "no" and is the command's own error to
/// report.
fn is_encrypted(archive: &Path, ro: &ReadOptions) -> bool {
    let Ok(src) = open_archive(archive) else {
        return false;
    };
    let Ok(loc) = locate_head(&src, &ro.limits) else {
        return false;
    };
    read_commit(&src, &loc.footer, ro).is_ok_and(|(c, _)| c.encrypted())
}

impl Env<'_> {
    /// Open the passphrase session for `archive` if it is Encrypted and the
    /// command needs one: the named sources, else a prompt on a terminal, else
    /// `KEY_UNAVAILABLE` from the core (exit 3) with nothing done. A Core
    /// archive never asks.
    fn unlock(&mut self, archive: &Path, need: Need) -> Result<()> {
        if self.read.keys.is_some() {
            return Ok(());
        }
        if need == Need::IfGiven && !self.pass.explicit() {
            return Ok(());
        }
        if !is_encrypted(archive, &self.read) {
            return Ok(());
        }
        let mut list = passphrase::explicit(&self.pass, self.err)?;
        if list.is_empty() {
            if !passphrase::can_prompt() {
                return Err(MochiError::new(
                    ErrorCode::KeyUnavailable,
                    "this archive is encrypted: give --passphrase-file PATH (or run on a \
                     terminal to be asked); a passphrase is never a command-line argument",
                ));
            }
            list.push(passphrase::ask(archive, self.err)?);
        }
        self.read.keys = Some(mochi_core::keys::KeySession::new(list)?);
        Ok(())
    }

    fn warn(&mut self, msg: impl std::fmt::Display) {
        let _ = writeln!(self.err, "mochi: warning: {msg}");
    }

    fn emit_json(&mut self, v: &Value) {
        let _ = writeln!(self.out, "{v}");
    }

    /// Emit a command-output document with its in-band schema tag (R7 O2,
    /// owner decision 2026-10-14). The tag names the document's JSON Schema
    /// file (`cli-<name>.schema.json` carries `const "mochi-cli-<name>-v1"`).
    fn emit_tagged(&mut self, tag: &str, mut v: Value) {
        if let Value::Object(m) = &mut v {
            m.insert("schema".into(), json!(tag));
        }
        self.emit_json(&v);
    }

    fn line(&mut self, s: impl std::fmt::Display) {
        let _ = writeln!(self.out, "{s}");
    }

    /// Append a completed run to the archive's evidence log, for
    /// `mochi health`. Nothing is recorded under `--no-local-history`, and a
    /// failure to record is a warning, never a failure of the command.
    fn record_evidence(&mut self, archive: ArchiveId, record: Option<EvidenceRecord>) {
        let (Some(store), Some(record)) = (self.store.clone(), record) else {
            return;
        };
        if let Err(e) = store.append_evidence(&archive, &record) {
            self.warn(format!(
                "the result was not recorded as health evidence: {e}"
            ));
        }
    }

    /// Record a head this client published: the freshness anchor (D8) and a
    /// `commit` evidence record with what the publication knew of its
    /// durability (Annex B.2.22), which `mochi health` reads.
    fn remember_commit(
        &mut self,
        archive: ArchiveId,
        seq: u64,
        commit_id: mochi_format::digest::CommitId,
        durability: &PublishDurability,
    ) {
        self.remember(archive, seq, commit_id);
        let exit_code = match durability {
            PublishDurability::Durable => exit::OK,
            PublishDurability::DirectoryUnconfirmed(_) => exit::DEGRADED,
        };
        let head = EvidenceHead {
            seq,
            commit_id: commit_id.to_hex(),
        };
        match Timestamp::now() {
            Ok(now) => self.record_evidence(
                archive,
                Some(EvidenceRecord::from_commit(
                    head, durability, &now, exit_code,
                )),
            ),
            Err(e) => self.warn(format!(
                "the commit was not recorded as health evidence: {e}"
            )),
        }
    }

    /// The same for the new archive a rewrite published (`compact`,
    /// `gc apply`, `migrate`, `rekey --reencrypt`). The head's commit record
    /// is read without a key (an Encrypted rewrite has a new data key).
    fn remember_rewrite(&mut self, output: &Path, unconfirmed: Option<&String>) {
        let head = open_archive(output).and_then(|s| {
            let loc = locate_head(&s, &self.read.limits)?;
            read_commit(&s, &loc.footer, &self.read)
        });
        match head {
            Ok((commit, commit_id)) => {
                let durability = match unconfirmed {
                    None => PublishDurability::Durable,
                    Some(why) => PublishDurability::DirectoryUnconfirmed(why.clone()),
                };
                self.remember_commit(commit.archive_id, commit.seq, commit_id, &durability);
            }
            Err(e) => self.warn(format!("the new archive's head was not recorded: {e}")),
        }
    }

    /// `--move-sources-to-bin` (Annex B.2.23 item 3), after the commit is
    /// published: decide from the new head which inputs may go and move
    /// them. Never fails the command: the commit stands.
    fn move_sources(&mut self, archive: &Path, imported: &mut Imported) {
        let (progress, cancel) = job();
        let ctx = JobContext {
            progress: &progress,
            cancel: &cancel,
        };
        let checked = open_archive(archive)
            .and_then(|s| check_removable(&s, &self.read, &imported.entries, &ctx));
        imported.moved = Some(match checked {
            Ok(c) => move_removable(&c, self.set_aside.as_ref()),
            Err(e) => MovedSources::all_kept(
                imported.entries.iter().map(|e| &e.path),
                &format!(
                    "nothing was moved: the new archive could not be read back ({})",
                    e.message
                ),
            ),
        });
    }

    /// Record a head this client saw, as a later freshness anchor (D8).
    /// Failing to record is a warning, never a failure of the command.
    fn remember(
        &mut self,
        archive: ArchiveId,
        seq: u64,
        commit_id: mochi_format::digest::CommitId,
    ) {
        if let Some(store) = self.store.clone() {
            if let Err(e) = store.record(&archive, SeenHead { seq, commit_id }) {
                self.warn(format!(
                    "the head was not recorded for freshness checks: {e}"
                ));
            }
        }
    }
}

fn job() -> (NullProgress, CancellationToken) {
    (NullProgress, CancellationToken::new())
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

/// Parse `--limit NAME=VALUE` options over the reader defaults.
pub fn read_options(limits: &[String]) -> Result<ReadOptions> {
    let mut o = ReadOptions::default();
    for spec in limits {
        let (name, value) = spec
            .split_once('=')
            .ok_or_else(|| invalid(format!("--limit {spec:?}: expected NAME=VALUE")))?;
        let value: u64 = value
            .parse()
            .map_err(|_| invalid(format!("--limit {spec:?}: the value is not a number")))?;
        let l = &mut o.limits;
        let slot = match name {
            "max-skippable-payload" => &mut l.max_skippable_payload,
            "max-frame-len" => &mut l.max_frame_len,
            "max-blocks-per-frame" => &mut l.max_blocks_per_frame,
            "max-window-size" => &mut l.max_window_size,
            "max-commit-frame-len" => &mut l.max_commit_frame_len,
            "max-decoded-object-len" => &mut l.max_decoded_object_len,
            "max-required-features" => &mut l.max_required_features,
            "max-kdf-memory-kib" => &mut l.max_kdf_memory_kib,
            "max-kdf-iterations" => &mut l.max_kdf_iterations,
            "max-kdf-lanes" => &mut l.max_kdf_lanes,
            "max-key-envelopes" => &mut l.max_key_envelopes,
            "max-salvage-candidates" => &mut l.max_salvage_candidates,
            _ => return Err(invalid(format!("--limit: unknown limit {name:?}"))),
        };
        *slot = value;
    }
    Ok(o)
}

/// The directory holding `archive` and its file name.
/// The archive's directory and file name. `flush` is what a directory flush
/// counts as on Windows (`--trust-ntfs-directory-flush`, spec Annex B.2.20).
fn location(archive: &Path, flush: WindowsDirectoryFlush) -> Result<(OsDir, String)> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            invalid(format!(
                "{}: the archive's file name must be valid Unicode",
                archive.display()
            ))
        })?
        .to_owned();
    Ok((OsDir::containing(archive)?.with_windows_flush(flush), name))
}

fn kind_name(k: SourceKind) -> &'static str {
    match k {
        SourceKind::File => "file",
        SourceKind::Directory => "directory",
        SourceKind::Symlink => "symbolic link",
        SourceKind::Other => "special file",
    }
}

fn entry_kind(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
    }
}

// In-band schema tags of the command-output documents (R7 O2, owner
// decision 2026-10-14). Each names `docs/schemas/json/cli-<name>.schema.json`.
// They freeze at the 1.0 flip (W20), not before.
const SCHEMA_COMMIT: &str = "mochi-cli-commit-v1";
/// `create` / `append` with `--move-sources-to-bin` (Annex B.2.23): v1 plus
/// `sources_moved` and `sources_kept`.
const SCHEMA_COMMIT_V2: &str = "mochi-cli-commit-v2";
const SCHEMA_MAINTENANCE: &str = "mochi-cli-maintenance-v1";
const SCHEMA_LIST: &str = "mochi-cli-list-v1";
const SCHEMA_SNAPSHOT_LIST: &str = "mochi-cli-snapshot-list-v1";
const SCHEMA_SEARCH: &str = "mochi-cli-search-v1";
const SCHEMA_DUMP_INDEX: &str = "mochi-cli-dump-index-v1";
const SCHEMA_RESTORE: &str = "mochi-cli-restore-v1";
const SCHEMA_REWRITE: &str = "mochi-cli-rewrite-v1";
const SCHEMA_MIGRATE: &str = "mochi-cli-migrate-v1";
const SCHEMA_REPAIR_APPLY: &str = "mochi-cli-repair-apply-v1";
const SCHEMA_REKEY_LIST: &str = "mochi-cli-rekey-list-v1";
const SCHEMA_PLAN_SAVED: &str = "mochi-cli-plan-saved-v1";
const SCHEMA_FIND: &str = "mochi-cli-find-v1";
const SCHEMA_FIND_V2: &str = "mochi-cli-find-v2";
const SCHEMA_COMMENT: &str = "mochi-cli-comment-v1";

fn findings_json(f: &[Finding]) -> Value {
    serde_json::to_value(f).unwrap_or(Value::Null)
}

fn severity(s: Severity) -> &'static str {
    match s {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

fn print_findings(env: &mut Env<'_>, findings: &[Finding]) {
    for f in findings {
        let msg = f.message.as_deref().unwrap_or("");
        env.line(format!(
            "  [{}] {}: {}",
            severity(f.severity),
            f.code,
            text(msg.as_bytes())
        ));
    }
}

// ---- create / append ----------------------------------------------------------

struct Imported {
    import: mochi_core::import::Import,
    deleted: Vec<ArchivePath>,
    /// Where each imported entry came from (for `--move-sources-to-bin`).
    entries: Vec<ImportedEntry<std::path::PathBuf>>,
    /// What `--move-sources-to-bin` did, once the commit is published.
    moved: Option<MovedSources>,
}

fn gather(
    inputs: &[std::path::PathBuf],
    tx: Transaction,
    max_bytes: Option<u64>,
    deleted: Vec<ArchivePath>,
    ctx: &JobContext<'_>,
) -> Result<Imported> {
    let tree = OsSourceTree;
    let mut named = Vec::with_capacity(inputs.len());
    for i in inputs {
        named.push(tree.input(i)?);
    }
    let (import, entries) = import_recording(
        &tree,
        &named,
        tx,
        &ImportOptions {
            max_total_bytes: max_bytes,
        },
        ctx,
    )?;
    Ok(Imported {
        import,
        deleted,
        entries,
        moved: None,
    })
}

fn emit_commit(
    env: &mut Env<'_>,
    command: &str,
    archive: &Path,
    archive_id: ArchiveId,
    outcome: &CommitOutcome,
    imported: &Imported,
    truncation: Option<&TailTruncation>,
) -> u8 {
    let i = &imported.import;
    let unconfirmed = match &outcome.durability {
        PublishDurability::Durable => None,
        PublishDurability::DirectoryUnconfirmed(why) => Some(why.clone()),
    };
    let kept_sources = imported.moved.as_ref().is_some_and(|m| !m.kept.is_empty());
    let code = if i.complete() && unconfirmed.is_none() && !kept_sources {
        exit::OK
    } else {
        exit::DEGRADED
    };
    if env.json {
        let skipped: Vec<Value> = i
            .skipped
            .iter()
            .map(|s| {
                let mut m = path_fields(s.path.as_stored());
                m.insert("kind".into(), json!(kind_name(s.kind)));
                m.insert("reason".into(), json!(s.reason));
                Value::Object(m)
            })
            .collect();
        let deleted: Vec<Value> = imported
            .deleted
            .iter()
            .map(|p| Value::Object(path_fields(p.as_stored())))
            .collect();
        let v = json!({
            "command": command,
            "archive": archive.display().to_string(),
            "status": outcome.status.as_str(),
            "archive_id": archive_id.to_hex(),
            "encrypted": env.read.keys.is_some(),
            "seq": outcome.seq,
            "commit_id": outcome.commit_id.to_hex(),
            "durability": if unconfirmed.is_some() { "directory_unconfirmed" } else { "durable" },
            "durability_note": unconfirmed,
            "added": {"files": i.files, "directories": i.directories, "bytes": i.bytes},
            "deleted": deleted,
            "skipped": skipped,
            "truncation": truncation.map(|t| findings_json(&t.findings())),
            "exit_code": code,
        });
        match &imported.moved {
            // v1 is frozen (W20): the move result is commit v2 (Annex B.2.23).
            Some(m) => {
                let mut v = v;
                if let Value::Object(o) = &mut v {
                    o.insert("sources_moved".into(), json!(m.moved));
                    let kept: Vec<Value> = m
                        .kept
                        .iter()
                        .map(|(p, why)| {
                            let mut f = path_fields(p.as_stored());
                            f.insert("reason".into(), json!(why));
                            Value::Object(f)
                        })
                        .collect();
                    o.insert("sources_kept".into(), Value::Array(kept));
                }
                env.emit_tagged(SCHEMA_COMMIT_V2, v);
            }
            None => env.emit_tagged(SCHEMA_COMMIT, v),
        }
    } else {
        if let Some(t) = truncation {
            env.line("tail truncated before appending:");
            print_findings(env, &t.findings());
        }
        env.line(format!(
            "committed commit {} to {} ({}): {} file{}, {} director{}, {} bytes{}",
            outcome.seq,
            archive.display(),
            outcome.status.as_str(),
            i.files,
            if i.files == 1 { "" } else { "s" },
            i.directories,
            if i.directories == 1 { "y" } else { "ies" },
            i.bytes,
            if imported.deleted.is_empty() {
                String::new()
            } else {
                format!(
                    "; {} entr{} removed from this snapshot (still in history)",
                    imported.deleted.len(),
                    if imported.deleted.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                )
            }
        ));
        env.line(format!("commit id {}", outcome.commit_id.to_hex()));
        for s in &i.skipped {
            env.line(format!(
                "skipped {} {}: {}",
                kind_name(s.kind),
                text(s.path.as_stored()),
                s.reason
            ));
        }
        if let Some(m) = &imported.moved {
            env.line(format!(
                "moved {} source{} to the Recycle Bin or Trash (nothing was deleted outright)",
                m.moved,
                if m.moved == 1 { "" } else { "s" }
            ));
            for (p, why) in &m.kept {
                env.line(format!("kept {}: {why}", text(p.as_stored())));
            }
        }
        if let Some(why) = &unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
        }
        if env.read.keys.is_some() {
            env.line(
                "encrypted: names, content, and sizes in this archive are sealed under a key \
                 the passphrase unlocks. There is no recovery if every passphrase is lost",
            );
        }
        env.line(
            "this is a local commit in one file; keep an independent copy of anything you \
             cannot lose",
        );
    }
    code
}

pub fn create(env: &mut Env<'_>, a: &CreateArgs) -> Result<u8> {
    if a.exceed_default_limits.is_some() {
        return Err(MochiError::new(
            ErrorCode::UnsupportedFeature,
            "`--exceed-default-limits` is a 1.x feature, not part of MOCHI 1.0 (spec Annex B, \
             D16); archives are created with the default limits only, and nothing was created",
        ));
    }
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let (mut dir, name) = location(&a.archive, env.flush)?;
    let mut tx = Transaction::new();
    if let Some(text) = comment_arg(a.comment.as_deref(), a.comment_file.as_deref())? {
        tx.set_comment(text);
    }
    let mut imported = gather(&a.inputs, tx, a.max_bytes, Vec::new(), &ctx)?;
    // The passphrases the key envelopes wrap the data key under: the named
    // sources, else a prompt asked twice. Nothing is created without one.
    let mut read = env.read.clone();
    if a.encrypted {
        let mut list = passphrase::explicit(&env.pass, env.err)?;
        if list.is_empty() {
            if !passphrase::can_prompt() {
                return Err(invalid(
                    "--encrypted needs a passphrase: give --passphrase-file PATH (repeatable, \
                     one per key envelope) or run on a terminal to be asked. Nothing was created",
                ));
            }
            list.push(passphrase::ask_new(
                &a.archive.display().to_string(),
                env.err,
            )?);
        }
        read.keys = Some(mochi_core::keys::KeySession::new(list)?);
        env.read.keys = read.keys.clone();
    }
    let opts = WriterOptions {
        read,
        chunk_size: a.chunk_size,
        zstd_level: a.compression_level,
        record_time: true,
        profile: (a.tar_compatible || a.encrypted || a.recovery_record.is_some()).then_some(
            Profile {
                tar_compatible: a.tar_compatible,
                encrypted: a.encrypted,
                redundancy: a.recovery_record,
            },
        ),
        kdf: None,
        ..WriterOptions::default()
    };
    let (w, outcome) = ArchiveWriter::create_in(
        &mut dir,
        &name,
        Box::new(OsIds),
        opts,
        imported.import.transaction.clone(),
        &ctx,
    )?;
    let archive_id = w.archive_id();
    if let Err(e) = w.close() {
        env.warn(format!("closing the archive after the commit: {e}"));
    }
    env.remember_commit(
        archive_id,
        outcome.seq,
        outcome.commit_id,
        &outcome.durability,
    );
    if a.move_sources_to_bin {
        env.move_sources(&a.archive, &mut imported);
    }
    Ok(emit_commit(
        env, "create", &a.archive, archive_id, &outcome, &imported, None,
    ))
}

pub fn append(env: &mut Env<'_>, a: &AppendArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let deletes: Vec<ArchivePath> = a
        .delete
        .iter()
        .map(|d| archive_path_arg(d))
        .collect::<Result<_>>()?;
    env.unlock(&a.archive, Need::Required)?;
    let (mut dir, name) = location(&a.archive, env.flush)?;
    let tail = if a.truncate_tail {
        TailRepair::Truncate(TruncationWaivers {
            no_quarantine: a.no_quarantine,
            accept_unconfirmed_durability: a.accept_unconfirmed_durability,
        })
    } else {
        TailRepair::Refuse
    };
    let opts = WriterOptions {
        read: env.read.clone(),
        record_time: true,
        lock_wait: env.lock_wait,
        ..WriterOptions::default()
    };
    let (mut w, truncation) =
        ArchiveWriter::open_append_in(&mut dir, &name, Box::new(OsIds), opts, tail)?;

    // Deletions first, each with everything under it (children before
    // their parent), then the inputs.
    let snapshot = w.catalog().replay(None)?;
    let mut tx = Transaction::new();
    if let Some(text) = comment_arg(a.comment.as_deref(), a.comment_file.as_deref())? {
        tx.set_comment(text);
    } else if a.clear_comment {
        tx.clear_comment();
    }
    let mut deleted = Vec::new();
    for d in &deletes {
        if snapshot.get(d).is_none() {
            return Err(invalid(format!(
                "--delete {}: no such entry in the archive's head; nothing was committed",
                text(d.as_stored())
            )));
        }
        let mut under: Vec<ArchivePath> = snapshot
            .iter()
            .map(|(p, _)| p)
            .filter(|p| p.is_descendant_of(d))
            .cloned()
            .collect();
        under.reverse();
        for p in under.into_iter().chain([d.clone()]) {
            tx.delete(p.clone());
            deleted.push(p);
        }
    }
    let mut imported = gather(&a.inputs, tx, a.max_bytes, deleted, &ctx)?;
    let archive_id = w.archive_id();
    if imported.import.transaction.is_empty() {
        if let Some(t) = &truncation {
            // Only the requested truncation; no commit.
            if env.json {
                let v = json!({
                    "command": "append",
                    "archive": a.archive.display().to_string(),
                    "committed": false,
                    "truncation": findings_json(&t.findings()),
                    "exit_code": exit::OK,
                });
                env.emit_tagged(SCHEMA_COMMIT, v);
            } else {
                env.line("tail truncated; nothing else to commit:");
                print_findings(env, &t.findings());
            }
            let _ = w.close();
            return Ok(exit::OK);
        }
        return Err(invalid(
            "nothing to append: give files or directories to add, --delete, --comment, or \
             --clear-comment",
        ));
    }
    let outcome = w.commit(imported.import.transaction.clone(), &ctx)?;
    if let Err(e) = w.close() {
        env.warn(format!("closing the archive after the commit: {e}"));
    }
    env.remember_commit(
        archive_id,
        outcome.seq,
        outcome.commit_id,
        &outcome.durability,
    );
    if a.move_sources_to_bin {
        env.move_sources(&a.archive, &mut imported);
    }
    Ok(emit_commit(
        env,
        "append",
        &a.archive,
        archive_id,
        &outcome,
        &imported,
        truncation.as_ref(),
    ))
}

/// The comment `--comment` or `--comment-file` gives (Annex B.2.19). A file
/// is UTF-8; its Windows line ends (CR LF) become LF. The rules themselves
/// are checked by the writer, before anything is written.
fn comment_arg(text: Option<&str>, file: Option<&Path>) -> Result<Option<String>> {
    if let Some(t) = text {
        return Ok(Some(t.to_owned()));
    }
    let Some(path) = file else {
        return Ok(None);
    };
    let bytes = std::fs::read(path).map_err(|e| {
        MochiError::new(
            ErrorCode::IoError,
            format!("--comment-file {}: {e}", os_path_text(path)),
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        invalid("--comment-file: the file is not UTF-8 text; nothing was committed")
    })?;
    Ok(Some(text.replace("\r\n", "\n")))
}

/// A comment for a terminal: shown as written, except that invisible
/// formatting characters (bidirectional overrides, zero-width marks; Unicode
/// category Cf) are written as `\u{...}` so they cannot reorder what follows.
fn comment_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let format = matches!(c,
            '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}');
        if format {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `mochi comment`: the archive comment at the head or at `--snapshot`
/// (Annex B.2.19). Reads the commit records only, so an Encrypted archive
/// needs no passphrase (it never has a comment).
pub fn comment_cmd(env: &mut Env<'_>, a: &CommentArgs) -> Result<u8> {
    let src = open_archive(&a.archive)?;
    let history = commit_history(&src, &env.read)?;
    let entry = match a.sel.snapshot {
        None => history.last(),
        Some(seq) => history.iter().find(|e| e.commit.seq == seq),
    }
    .ok_or_else(|| {
        invalid(format!(
            "no commit {}: this archive has commits 0 to {}",
            a.sel.snapshot.unwrap_or_default(),
            history.len().saturating_sub(1)
        ))
    })?;
    let comment = entry.commit.comment.as_deref();
    if env.json {
        env.emit_tagged(
            SCHEMA_COMMENT,
            json!({
                "archive_id": entry.commit.archive_id.to_hex(),
                "seq": entry.commit.seq,
                "commit_id": entry.commit_id.to_hex(),
                "comment": comment,
            }),
        );
    } else if let Some(c) = comment {
        for line in comment_text(c).split('\n') {
            env.line(line);
        }
    }
    Ok(exit::OK)
}

// ---- reading ----------------------------------------------------------------

fn open_selected(
    src: &OsReadStorage,
    snapshot: Option<u64>,
    ro: &ReadOptions,
) -> Result<OpenedHead> {
    match snapshot {
        None => open_head(src, ro),
        Some(seq) => {
            let history = commit_history(src, ro)?;
            let e = history
                .iter()
                .find(|e| e.commit.seq == seq)
                .ok_or_else(|| {
                    invalid(format!(
                        "no commit {seq}: this archive has commits 0 to {}",
                        history.len().saturating_sub(1)
                    ))
                })?;
            open_at_footer(src, e.footer_offset, ro)
        }
    }
}

fn open_archive(path: &Path) -> Result<OsReadStorage> {
    OsReadStorage::open(path).map_err(|e| {
        let e = MochiError::from(e);
        MochiError::new(e.code, format!("{}: {}", path.display(), e.message))
    })
}

pub fn list_cmd(env: &mut Env<'_>, a: &ListArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let head = open_selected(&src, a.sel.snapshot, &env.read)?;
    let under = a.under.as_deref().map(archive_path_arg).transpose()?;
    let entries = list(&head, under.as_ref())?;
    if env.json {
        let items: Vec<Value> = entries
            .iter()
            .map(|e| {
                let mut m = path_fields(e.path.as_stored());
                m.insert("kind".into(), json!(entry_kind(e.kind)));
                m.insert("size".into(), json!(e.logical_len));
                m.insert(
                    "content_hash".into(),
                    json!(e.content_hash.map(|h| h.to_hex())),
                );
                Value::Object(m)
            })
            .collect();
        let v = json!({
            "archive_id": head.commit.archive_id.to_hex(),
            "seq": head.seq(),
            "commit_id": head.commit_id.to_hex(),
            "entries": items,
        });
        env.emit_tagged(SCHEMA_LIST, v);
    } else {
        for e in &entries {
            match e.kind {
                EntryKind::Directory => {
                    env.line(format!("d {:>14}  {}/", "-", text(e.path.as_stored())))
                }
                EntryKind::File => env.line(format!(
                    "f {:>14}  {}",
                    e.logical_len,
                    text(e.path.as_stored())
                )),
            }
        }
    }
    Ok(exit::OK)
}

/// One catalog value as JSON: numbers and null as themselves, text as a
/// string when it is UTF-8 and `{"text_hex": ...}` when it is not, and a
/// BLOB always as `{"blob_hex": ...}`, so the three never look alike.
fn dump_value_json(v: &DumpValue) -> Value {
    match v {
        DumpValue::Null => Value::Null,
        DumpValue::Integer(n) => json!(n),
        DumpValue::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => json!(s),
            Err(_) => json!({ "text_hex": hex(t) }),
        },
        DumpValue::Blob(b) => json!({ "blob_hex": hex(b) }),
    }
}

/// One catalog value for a terminal: text escaped like every archive
/// string, BLOBs as `x'hex'`, null as `NULL`.
fn dump_value_text(v: &DumpValue) -> String {
    match v {
        DumpValue::Null => "NULL".into(),
        DumpValue::Integer(n) => n.to_string(),
        DumpValue::Text(t) => text(t),
        DumpValue::Blob(b) => format!("x'{}'", hex(b)),
    }
}

/// `mochi dump-index` (plan C14, concession K6): the catalog of the
/// opened commit, hash-verified as every open is, dumped table by table.
/// Read-only; table names come only from the catalog's own schema.
pub fn dump_index(env: &mut Env<'_>, a: &DumpIndexArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let head = open_selected(&src, a.sel.snapshot, &env.read)?;
    let source = match &head.catalog_source {
        CatalogSource::Image => "image",
        CatalogSource::SnapshotManifest { .. } => "snapshot-manifest",
    };
    let requested = (!a.tables.is_empty()).then_some(a.tables.as_slice());
    if env.json {
        let dump = head.catalog.dump(requested)?;
        let tables: serde_json::Map<String, Value> = dump
            .tables
            .iter()
            .map(|t| {
                let rows: Vec<Value> = t
                    .rows
                    .iter()
                    .map(|r| Value::Array(r.iter().map(dump_value_json).collect()))
                    .collect();
                (
                    t.name.clone(),
                    json!({ "columns": t.columns, "rows": rows }),
                )
            })
            .collect();
        env.emit_tagged(
            SCHEMA_DUMP_INDEX,
            json!({
                "archive_id": head.commit.archive_id.to_hex(),
                "commit": {
                    "seq": head.seq(),
                    "commit_id": head.commit_id.to_hex(),
                    "catalog_source": source,
                },
                "tables": tables,
            }),
        );
        return Ok(exit::OK);
    }
    env.line(format!(
        "commit {} ({}), catalog from {source}",
        head.seq(),
        head.commit_id.to_hex()
    ));
    if requested.is_none() {
        for (name, n) in head.catalog.table_counts()? {
            env.line(format!("{name:<20} {n:>10} rows"));
        }
        return Ok(exit::OK);
    }
    for t in head.catalog.dump(requested)?.tables {
        env.line(format!("table {} ({} rows)", t.name, t.rows.len()));
        env.line(format!("  {}", t.columns.join("\t")));
        for r in &t.rows {
            let cells: Vec<String> = r.iter().map(dump_value_text).collect();
            env.line(format!("  {}", cells.join("\t")));
        }
    }
    Ok(exit::OK)
}

/// 32 bytes from 64 hexadecimal digits.
fn parse_hex32(s: &str, what: &str) -> Result<[u8; 32]> {
    let bad = || invalid(format!("{s:?} is not a {what} (64 hexadecimal digits)"));
    let b = s.as_bytes();
    if b.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(b.chunks_exact(2)) {
        let digit = |c: u8| (c as char).to_digit(16).ok_or_else(bad);
        let (hi, lo) = (digit(pair[0])?, digit(pair[1])?);
        *slot = u8::try_from(hi * 16 + lo).map_err(|_| bad())?;
    }
    Ok(out)
}

/// `mochi search` (spec §19.1, §19.3; plan C13). Exit 0 with complete
/// coverage, whatever the number of hits; 2 with partial coverage (1 with
/// `--require-complete`), because zero hits then prove nothing.
pub fn search_cmd(env: &mut Env<'_>, a: &SearchArgs) -> Result<u8> {
    if a.content.is_some() {
        return Err(MochiError::new(
            ErrorCode::UnsupportedFeature,
            "full-text search (spec §19.2) is not built: file content cannot be searched, \
             and names are not searched in its place",
        ));
    }
    let query = Query {
        name: a.pattern.clone().unwrap_or_default().into_bytes(),
        ascii_case_insensitive: a.ignore_case,
        path: match (&a.path, &a.under) {
            (Some(p), _) => Some(PathMatch::Exact(archive_path_arg(p)?)),
            (None, Some(p)) => Some(PathMatch::Under(archive_path_arg(p)?)),
            (None, None) => None,
        },
        version: a
            .version
            .as_deref()
            .map(|v| parse_hex32(v, "file version ID").map(FileVersionId::from_bytes))
            .transpose()?,
        content_hash: a
            .content_hash
            .as_deref()
            .map(|h| parse_hex32(h, "file-content hash").map(FileContentHash::from_bytes))
            .transpose()?,
        kind: a.kind.map(|k| match k {
            KindArg::File => EntryKind::File,
            KindArg::Dir => EntryKind::Directory,
        }),
    };
    let scope = match a.snapshot {
        SearchSnapshots::Head => SnapshotScope::Head,
        SearchSnapshots::Retained => SnapshotScope::Retained,
        SearchSnapshots::All => SnapshotScope::All,
        SearchSnapshots::Commit(s) => SnapshotScope::Commit(s),
    };
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let head = open_head(&src, &env.read)?;
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let r = search(&src, &head, scope, &query, &env.read, &ctx)?;
    let c = &r.coverage;
    let complete = c.complete();
    let code = match (complete, a.require_complete) {
        (true, _) => exit::OK,
        (false, false) => exit::DEGRADED,
        (false, true) => exit::FAILED,
    };
    let source = match &c.catalog_source {
        CatalogSource::Image => "image",
        CatalogSource::SnapshotManifest { .. } => "snapshot-manifest",
    };
    if env.json {
        let hits: Vec<Value> = r
            .hits
            .iter()
            .map(|h| {
                let mut m = path_fields(h.path.as_stored());
                m.insert("seq".into(), json!(h.seq));
                m.insert("kind".into(), json!(entry_kind(h.kind)));
                m.insert("size".into(), json!(h.logical_len));
                m.insert("file_version_id".into(), json!(hex(h.version.as_bytes())));
                m.insert(
                    "content_hash".into(),
                    json!(h.content_hash.map(|x| x.to_hex())),
                );
                Value::Object(m)
            })
            .collect();
        let scope_v = match scope {
            SnapshotScope::Head => json!("head"),
            SnapshotScope::Retained => json!("retained"),
            SnapshotScope::All => json!("all"),
            SnapshotScope::Commit(s) => json!({ "commit": s }),
        };
        env.emit_tagged(
            SCHEMA_SEARCH,
            json!({
                "archive_id": head.commit.archive_id.to_hex(),
                "scope": scope_v,
                "coverage": {
                    "complete": complete,
                    "indexed_seq": c.indexed_seq,
                    "indexed_commit_id": head.commit_id.to_hex(),
                    "catalog_source": source,
                    "requested": c.requested,
                    "searched": c.searched,
                    "opened_separately": c.opened_separately,
                    "unavailable": c.unavailable.iter().map(|u| json!({
                        "seq": u.seq,
                        "code": u.error.code.as_str(),
                        "message": u.error.message,
                    })).collect::<Vec<_>>(),
                    "entries_examined": c.entries_examined,
                    "pending": c.pending,
                    "failed": c.failed,
                    "unsupported": c.unsupported,
                    "excluded": c.excluded,
                    "full_text": match c.full_text { FullText::NotBuilt => "not-built" },
                },
                "hits": hits,
            }),
        );
        return Ok(code);
    }
    for h in &r.hits {
        match h.kind {
            EntryKind::Directory => env.line(format!(
                "{:>6}  d {:>14}  {}/",
                h.seq,
                "-",
                text(h.path.as_stored())
            )),
            EntryKind::File => env.line(format!(
                "{:>6}  f {:>14}  {}",
                h.seq,
                h.logical_len,
                text(h.path.as_stored())
            )),
        }
    }
    let n = r.hits.len();
    let found = format!("{n} {}", if n == 1 { "match" } else { "matches" });
    if complete {
        env.line(format!(
            "{found}; coverage complete: {} of {} requested snapshots searched (catalog at \
             commit {}, from its {source}); file content was not searched (no full-text index)",
            c.searched.len(),
            c.requested.len(),
            c.indexed_seq
        ));
    } else {
        env.line(format!(
            "{found}; coverage PARTIAL: {} of {} requested snapshots searched; matches in the \
             others are unknown, so this is not proof that no matching entry exists",
            c.searched.len(),
            c.requested.len()
        ));
        for u in &c.unavailable {
            env.line(format!(
                "  snapshot {} not searched: {}: {}",
                u.seq, u.error.code, u.error.message
            ));
        }
    }
    if !c.opened_separately.is_empty() {
        env.warn(format!(
            "the head's catalog image is damaged and was rebuilt from its snapshot manifest; \
             {} earlier snapshot(s) were read from their own commits",
            c.opened_separately.len()
        ));
    }
    Ok(code)
}

/// Why an archive was not searched, as one line of text.
fn skip_text(s: &Skip) -> String {
    match s {
        Skip::NotMochi => "not a MOCHI archive".to_owned(),
        Skip::Encrypted => "Encrypted: its names need the passphrase, and `mochi find` takes none \
                           (use `mochi search ARCHIVE` with --passphrase-file)"
            .to_owned(),
        Skip::Unsupported { code, message } => {
            format!(
                "in a format this build does not read: {code}: {}",
                text(message.as_bytes())
            )
        }
        Skip::Damaged { code, message } => format!(
            "damaged, could not be opened: {code}: {} (`mochi verify` checks it)",
            text(message.as_bytes())
        ),
        Skip::TooLarge => "too many names to hold in memory".to_owned(),
        Skip::Unreadable { message } => format!("could not be read: {}", text(message.as_bytes())),
    }
}

/// `mochi find` (spec Annex B.2.18, D28): names across the archives under
/// some folders, read-only, through `mochi_core::find`, the engine the
/// desktop app's Find uses. Exit 0 when nothing was left out, whatever
/// matched; 2 when anything was (never "no matches" then).
/// The names `mochi find` looks for and the folders it searches: the
/// positional `PATTERN FOLDER...`, or `FOLDER...` with `--name` and
/// `--names-from` (spec Annex B.2.21).
struct FindAsk {
    names: Vec<String>,
    folders: Vec<std::path::PathBuf>,
    /// `mochi find PATTERN FOLDER...` with no option: the v1 form, unchanged.
    v1: bool,
}

fn find_ask(a: &FindArgs) -> Result<FindAsk> {
    let mut names: Vec<String> = a.names.clone();
    if let Some(file) = &a.names_from {
        let text = if file.as_os_str() == "-" {
            let mut t = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut t)
                .map_err(|e| invalid(format!("standard input: {e}")))?;
            t
        } else {
            std::fs::read_to_string(file)
                .map_err(|e| invalid(format!("{}: {e}", os_path_text(file))))?
        };
        names.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned),
        );
        if names.is_empty() {
            return Err(invalid(format!(
                "{}: no names in it (one name per line)",
                os_path_text(file)
            )));
        }
    }
    let listed = !a.names.is_empty() || a.names_from.is_some();
    let mut args = a.args.iter();
    if !listed {
        let pattern = args
            .next()
            .ok_or_else(|| invalid("give a name to find (`*` matches every name)"))?;
        let pattern = pattern.to_str().ok_or_else(|| {
            invalid("the name to find must be text (names are compared as UTF-8 here)")
        })?;
        names.push(pattern.to_owned());
    }
    let folders: Vec<std::path::PathBuf> = args.cloned().collect();
    if folders.is_empty() {
        return Err(invalid(if listed {
            "give at least one folder to search"
        } else {
            "give a name to find, then at least one folder to search"
        }));
    }
    // Each name once, in the order given.
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| {
        seen.insert(if a.match_case {
            n.trim().to_owned()
        } else {
            n.trim().to_lowercase()
        })
    });
    for n in &names {
        if n.trim().is_empty() {
            return Err(invalid("give a name to find (`*` matches every name)"));
        }
        if n.len() > MAX_QUERY_BYTES {
            return Err(invalid(format!(
                "a name to find has at most {MAX_QUERY_BYTES} bytes"
            )));
        }
    }
    if names.len() > MAX_TERMS {
        return Err(invalid(format!(
            "at most {MAX_TERMS} names can be found at once"
        )));
    }
    let v1 = !listed
        && !a.match_case
        && !a.whole_name
        && !a.files_only
        && !a.folders_only
        && !a.no_subfolders;
    Ok(FindAsk { names, folders, v1 })
}

pub fn find_cmd(env: &mut Env<'_>, a: &FindArgs) -> Result<u8> {
    if env.pass.explicit() {
        return Err(invalid(
            "`mochi find` never opens an Encrypted archive and takes no passphrase; it lists \
             Encrypted archives as not searched (use `mochi search ARCHIVE` with a passphrase)",
        ));
    }
    let ask = find_ask(a)?;
    let mut given = Vec::with_capacity(ask.folders.len());
    for f in &ask.folders {
        let abs =
            std::path::absolute(f).map_err(|e| invalid(format!("{}: {e}", os_path_text(f))))?;
        if !abs.is_dir() {
            return Err(invalid(format!(
                "{}: not a folder (find searches the archives under folders; to search one \
                 archive use `mochi search`)",
                os_path_text(f)
            )));
        }
        given.push(abs);
    }
    let scopes = normalize_scopes(&given, &[]);
    if scopes.is_empty() {
        return Err(invalid("none of the given paths is a folder"));
    }
    if scopes.len() > MAX_SCOPES {
        return Err(invalid(format!(
            "at most {MAX_SCOPES} folders can be searched at once"
        )));
    }
    let scan = Scan::new(scopes);
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    scan.run(
        &OsFindTree,
        &env.read,
        &FindLimits {
            subfolders: !a.no_subfolders,
            ..FindLimits::default()
        },
        &NoCache,
        &ctx,
    )?;
    if !ask.v1 {
        return find_v2(env, a, &ask, &scan);
    }
    let pattern = ask.names.first().map(String::as_str).unwrap_or_default();
    let c = scan.coverage();
    let complete = c.nothing_left_out();
    let matcher = Matcher::new(pattern, true);
    let found = scan.query(&matcher, usize::MAX);
    let total: u64 = found.iter().map(|m| m.count).sum();
    let code = if complete { exit::OK } else { exit::DEGRADED };
    if env.json {
        let mut hits = Vec::new();
        for m in &found {
            for e in m.entries.iter().filter_map(|i| m.names.get(*i)) {
                let mut h = serde_json::Map::new();
                h.insert("archive".into(), json!(m.path.display().to_string()));
                h.extend(path_fields(e.path));
                h.insert("kind".into(), json!(entry_kind(e.kind)));
                h.insert("size".into(), json!(e.size));
                hits.push(Value::Object(h));
            }
        }
        let skipped: Vec<Value> = c
            .skipped
            .iter()
            .map(|s| {
                json!({
                    "archive": s.path.display().to_string(),
                    "reason": s.skip.reason(),
                    "code": s.skip.code().map(|c| c.as_str()),
                    "message": s.skip.message(),
                })
            })
            .collect();
        env.emit_tagged(
            SCHEMA_FIND,
            json!({
                "query": {
                    "text": pattern.trim(),
                    "match": if matcher.is_glob() { "glob" } else { "substring" },
                    "whole_path": matcher.is_whole_path(),
                    "ignore_case": true,
                },
                "scopes": scan.scopes().iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "coverage": {
                    "complete": complete,
                    "folders_visited": c.folders_visited,
                    "folders_unreadable": c.folders_unreadable,
                    "links_not_followed": c.links_not_followed,
                    "limit": c.limit.map(|l| l.as_str()),
                    "archives_found": c.archives_found,
                    "archives_searched": c.archives_searched,
                    "archives_skipped": c.skipped.len(),
                    "skipped": skipped,
                    "names_searched": c.names_searched,
                    "content_searched": false,
                },
                "total_hits": total,
                "hits": hits,
            }),
        );
        return Ok(code);
    }
    for m in &found {
        let archive = os_path_text(&m.path);
        for e in m.entries.iter().filter_map(|i| m.names.get(*i)) {
            let slash = if e.kind == EntryKind::Directory {
                "/"
            } else {
                ""
            };
            env.line(format!("{archive}: {}{slash}", text(e.path)));
        }
    }
    let archives_with = found.len();
    let matches = if total == 1 { "match" } else { "matches" };
    let summary = format!(
        "{total} {matches} in {archives_with} of {} archives searched ({} names); file content \
         was not searched",
        c.archives_searched, c.names_searched
    );
    if complete {
        if total == 0 {
            env.line(format!(
                "no matches; coverage complete: every one of the {} archives found ({} folders \
                 walked) was searched (names of each archive's newest version; file content \
                 was not searched)",
                c.archives_found, c.folders_visited
            ));
        } else {
            env.line(format!(
                "{summary}; coverage complete: every one of the {} archives found ({} folders \
                 walked) was searched",
                c.archives_found, c.folders_visited
            ));
        }
        return Ok(code);
    }
    env.line(format!(
        "{summary}; coverage INCOMPLETE: {} of {} archives found were searched, and names in \
         what was left out are unknown, so this is not proof that no matching name exists",
        c.archives_searched, c.archives_found
    ));
    find_left_out(env, &c);
    Ok(code)
}

/// The text lines that say what an incomplete search left out.
fn find_left_out(env: &mut Env<'_>, c: &mochi_core::find::Coverage) {
    for s in &c.skipped {
        env.line(format!(
            "  not searched: {}: {}",
            os_path_text(&s.path),
            skip_text(&s.skip)
        ));
    }
    if c.folders_unreadable > 0 {
        env.line(format!(
            "  {} folders or entries could not be read",
            c.folders_unreadable
        ));
    }
    if c.links_not_followed > 0 {
        env.line(format!(
            "  {} links to folders or archives were not followed",
            c.links_not_followed
        ));
    }
    if let Some(l) = c.limit {
        env.line(format!(
            "  the walk stopped at its {} limit; folders after that were not searched",
            l.as_str()
        ));
    }
}

fn entry_filter_name(f: EntryFilter) -> &'static str {
    match f {
        EntryFilter::All => "all",
        EntryFilter::Files => "files",
        EntryFilter::Folders => "folders",
    }
}

/// `mochi find` with several names or any option (spec Annex B.2.21): every
/// entry that matches any name, with the names it matched, and what each name
/// found. JSON: `mochi-cli-find-v2`.
fn find_v2(env: &mut Env<'_>, a: &FindArgs, ask: &FindAsk, scan: &Scan) -> Result<u8> {
    let options = MatchOptions {
        ignore_case: !a.match_case,
        whole_name: a.whole_name,
    };
    let filter = if a.files_only {
        EntryFilter::Files
    } else if a.folders_only {
        EntryFilter::Folders
    } else {
        EntryFilter::All
    };
    let set = TermSet::new(&ask.names, options, filter)?;
    let found = scan.query_terms(&set, usize::MAX);
    let c = scan.coverage();
    let complete = c.nothing_left_out();
    let code = if complete { exit::OK } else { exit::DEGRADED };
    let total: u64 = found.iter().map(|m| m.count).sum();
    let mut per_term = vec![(0u64, 0u64); ask.names.len()];
    for m in &found {
        for (t, &n) in m.per_term.iter().enumerate() {
            if let Some(slot) = per_term.get_mut(t) {
                slot.0 += n;
                slot.1 += u64::from(n > 0);
            }
        }
    }
    if env.json {
        let terms: Vec<Value> = ask
            .names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let m = set.term(i);
                let (hits, archives) = per_term.get(i).copied().unwrap_or_default();
                json!({
                    "text": n.trim(),
                    "match": if m.is_some_and(Matcher::is_glob) {
                        "glob"
                    } else if a.whole_name {
                        "whole_name"
                    } else {
                        "substring"
                    },
                    "whole_path": m.is_some_and(Matcher::is_whole_path),
                    "hits": hits,
                    "archives": archives,
                })
            })
            .collect();
        let mut hits = Vec::new();
        for m in &found {
            for (i, ts) in &m.entries {
                let Some(e) = m.names.get(*i) else { continue };
                let mut h = serde_json::Map::new();
                h.insert("archive".into(), json!(m.path.display().to_string()));
                h.extend(path_fields(e.path));
                h.insert("kind".into(), json!(entry_kind(e.kind)));
                h.insert("size".into(), json!(e.size));
                h.insert("terms".into(), json!(ts));
                hits.push(Value::Object(h));
            }
        }
        let skipped: Vec<Value> = c
            .skipped
            .iter()
            .map(|s| {
                json!({
                    "archive": s.path.display().to_string(),
                    "reason": s.skip.reason(),
                    "code": s.skip.code().map(|c| c.as_str()),
                    "message": s.skip.message(),
                })
            })
            .collect();
        env.emit_tagged(
            SCHEMA_FIND_V2,
            json!({
                "query": {
                    "terms": terms,
                    "ignore_case": !a.match_case,
                    "whole_name": a.whole_name,
                    "entries": entry_filter_name(filter),
                    "subfolders": !a.no_subfolders,
                },
                "scopes": scan.scopes().iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "coverage": {
                    "complete": complete,
                    "folders_visited": c.folders_visited,
                    "folders_unreadable": c.folders_unreadable,
                    "links_not_followed": c.links_not_followed,
                    "limit": c.limit.map(|l| l.as_str()),
                    "archives_found": c.archives_found,
                    "archives_searched": c.archives_searched,
                    "archives_skipped": c.skipped.len(),
                    "skipped": skipped,
                    "names_searched": c.names_searched,
                    "content_searched": false,
                },
                "total_hits": total,
                "hits": hits,
            }),
        );
        return Ok(code);
    }
    for m in &found {
        let archive = os_path_text(&m.path);
        for (i, _) in &m.entries {
            let Some(e) = m.names.get(*i) else { continue };
            let slash = if e.kind == EntryKind::Directory {
                "/"
            } else {
                ""
            };
            env.line(format!("{archive}: {}{slash}", text(e.path)));
        }
    }
    // What each name found; a name with no match is "not found" only when
    // everything was searched (spec 23.3 #8).
    for (n, (hits, archives)) in ask.names.iter().zip(&per_term) {
        let name = text(n.trim().as_bytes());
        if *hits == 0 {
            env.line(format!(
                "name {name}: {}",
                if complete {
                    "not found"
                } else {
                    "not found in what was searched"
                }
            ));
        } else {
            env.line(format!(
                "name {name}: {hits} match{} in {archives} archive{}",
                if *hits == 1 { "" } else { "es" },
                if *archives == 1 { "" } else { "s" }
            ));
        }
    }
    let found_names = per_term.iter().filter(|(h, _)| *h > 0).count();
    let matches = if total == 1 { "match" } else { "matches" };
    let summary = format!(
        "{total} {matches} for {found_names} of {} names in {} of {} archives searched ({} \
         names){}; file content was not searched",
        ask.names.len(),
        found.len(),
        c.archives_searched,
        c.names_searched,
        if a.no_subfolders {
            "; subfolders were not searched (--no-subfolders)"
        } else {
            ""
        }
    );
    if complete {
        env.line(format!(
            "{summary}; coverage complete: every one of the {} archives found ({} folders \
             walked) was searched",
            c.archives_found, c.folders_visited
        ));
        return Ok(code);
    }
    env.line(format!(
        "{summary}; coverage INCOMPLETE: {} of {} archives found were searched, and names in \
         what was left out are unknown, so this is not proof that no matching name exists",
        c.archives_searched, c.archives_found
    ));
    find_left_out(env, &c);
    Ok(code)
}

pub fn snapshot_list(env: &mut Env<'_>, a: &SnapshotListArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let history = commit_history(&src, &env.read)?;
    // Retention at the head, rebuilt from its segment's manifests (D10.10).
    // If it cannot be, the commits are still listed, with retention unknown.
    let retention =
        match open_head(&src, &env.read).and_then(|h| segment_state(&src, &h, &env.read)) {
            Ok(s) => Some(s.retention),
            // A refusal (a draft archive, an unknown feature) is not damage:
            // the archive is not listed at all, as every other command.
            Err(e) if e.code == ErrorCode::UnsupportedFeature => return Err(e),
            Err(e) => {
                env.warn(format!("retention is unknown: {e}"));
                None
            }
        };
    let head_seq = history.last().map(|e| e.commit.seq);
    let time = |e: &mochi_core::publish::HistoryEntry| {
        e.commit
            .time
            .and_then(|t| Timestamp::from_unix(t.secs, t.nanos).ok())
            .map(|t| t.to_string())
    };
    let holds = |seq: u64| -> Vec<&[u8]> {
        retention
            .iter()
            .flat_map(|r| r.holds.iter())
            .filter(|(_, s)| **s == seq)
            .map(|(l, _)| l.as_slice())
            .collect()
    };
    let expired = |seq: u64| retention.as_ref().map(|r| r.expired.contains(&seq));
    let retained = |seq: u64| {
        retention
            .as_ref()
            .zip(head_seq)
            .map(|(r, h)| r.roots(h).contains(&seq))
    };
    if env.json {
        let items: Vec<Value> = history
            .iter()
            .map(|e| {
                let seq = e.commit.seq;
                json!({
                    "seq": seq,
                    "commit_id": e.commit_id.to_hex(),
                    "time": time(e),
                    "checkpoint": e.commit.metadata.is_checkpoint(),
                    "footer_offset": e.footer_offset,
                    "expired": expired(seq),
                    "holds": holds(seq).iter().map(|l| String::from_utf8_lossy(l)).collect::<Vec<_>>(),
                    "retained": retained(seq),
                })
            })
            .collect();
        let archive_id = history.last().map(|e| e.commit.archive_id.to_hex());
        env.emit_tagged(
            SCHEMA_SNAPSHOT_LIST,
            json!({
                "archive_id": archive_id,
                "retention_known": retention.is_some(),
                "commits": items,
            }),
        );
    } else {
        let mut previous: Option<&str> = None;
        for e in &history {
            let seq = e.commit.seq;
            let mut notes = Vec::new();
            // The comment (B.2.19) where it changes; `mochi comment` shows it.
            let comment = e.commit.comment.as_deref();
            if comment != previous {
                notes.push(
                    if comment.is_some() {
                        "comment set"
                    } else {
                        "comment cleared"
                    }
                    .to_string(),
                );
            }
            previous = comment;
            match expired(seq) {
                Some(true) => notes.push("expired".to_string()),
                Some(false) => {}
                None => notes.push("retention unknown".to_string()),
            }
            for l in holds(seq) {
                notes.push(format!("hold {}", text(l)));
            }
            let row = format!(
                "{:>6}  {}  {}  {:<10}  {}",
                seq,
                e.commit_id.to_hex(),
                time(e).unwrap_or_else(|| "-".repeat(30)),
                if e.commit.metadata.is_checkpoint() {
                    "checkpoint"
                } else {
                    "delta"
                },
                notes.join(", ")
            );
            env.line(row.trim_end());
        }
    }
    Ok(exit::OK)
}

/// The exit code of a restoration: 1 if any entry failed verification
/// (O22), 2 if any entry was not restored for another reason, else 0.
/// Attributes that could not be applied are warnings only (an unprivileged
/// user can never restore ownership).
fn restore_exit(r: &RestoreReport) -> u8 {
    if r.exceptions
        .iter()
        .any(|e| matches!(e.kind, ExceptionKind::Integrity { .. }))
    {
        exit::FAILED
    } else if !r.complete() {
        exit::DEGRADED
    } else {
        exit::OK
    }
}

fn emit_restore(
    env: &mut Env<'_>,
    command: &str,
    head: &OpenedHead,
    dest: &Path,
    r: &RestoreReport,
) -> u8 {
    let code = restore_exit(r);
    let findings = r.findings();
    if env.json {
        let v = json!({
            "command": command,
            "seq": head.seq(),
            "commit_id": head.commit_id.to_hex(),
            "destination": dest.display().to_string(),
            "files": r.files,
            "directories": r.directories,
            "bytes": r.bytes,
            "complete": r.complete(),
            "attributes_complete": r.attributes_complete(),
            "case_insensitive_destination":
                r.case_behavior == mochi_core::storage::CaseBehavior::Insensitive,
            "findings": findings_json(&findings),
            "exit_code": code,
        });
        env.emit_tagged(SCHEMA_RESTORE, v);
    } else {
        env.line(format!(
            "{}: commit {} into {}: {} file{}, {} director{}, {} bytes, every file verified \
             before it was published",
            command,
            head.seq(),
            dest.display(),
            r.files,
            if r.files == 1 { "" } else { "s" },
            r.directories,
            if r.directories == 1 { "y" } else { "ies" },
            r.bytes
        ));
        if !findings.is_empty() {
            print_findings(env, &findings);
        }
        if !r.complete() {
            env.line(format!(
                "incomplete: {} entr{} not restored (see above); nothing was overwritten",
                r.exceptions.len(),
                if r.exceptions.len() == 1 { "y" } else { "ies" }
            ));
        }
    }
    code
}

pub fn get(env: &mut Env<'_>, a: &GetArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let head = open_selected(&src, a.sel.snapshot, &env.read)?;
    let paths: Vec<ArchivePath> = a
        .paths
        .iter()
        .map(|p| archive_path_arg(p))
        .collect::<Result<_>>()?;
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    if a.stdout {
        if env.json {
            return Err(invalid(
                "--stdout carries the file's bytes; it cannot be combined with --json",
            ));
        }
        let [path] = paths.as_slice() else {
            return Err(invalid("--stdout takes exactly one file path"));
        };
        let result = read_file(&src, &head, path, env.out, &env.read, &ctx);
        return match result {
            Ok(_) => Ok(exit::OK),
            Err(e) => Err(MochiError::new(
                e.code,
                format!(
                    "{}; anything already written to standard output is unverified and must \
                     be discarded",
                    e.message
                ),
            )),
        };
    }
    let dest = a
        .destination
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match std::fs::create_dir(&dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dest.is_dir() => {}
        Err(e) => {
            return Err(MochiError::new(
                ErrorCode::IoError,
                format!("destination {}: {e}", dest.display()),
            ))
        }
    }
    let root = OsRestoreDir::open(&dest)?;
    let options = RestoreOptions {
        restore_setid: a.restore_setid,
        refuse_on_preflight_exceptions: a.refuse_on_conflict,
        skip_file_sync: false,
        // The CLI never merges into or replaces what is there (spec §10.4).
        on_existing: mochi_core::restore::OnExisting::Keep,
    };
    let r = restore_selected(&src, &head, &paths, root, &options, &env.read, &ctx)?;
    Ok(emit_restore(env, "get", &head, &dest, &r))
}

pub fn restore_test(env: &mut Env<'_>, a: &RestoreTestArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let head = open_selected(&src, a.sel.snapshot, &env.read)?;
    std::fs::create_dir(&a.destination).map_err(|e| {
        MochiError::new(
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                ErrorCode::DestinationExists
            } else {
                ErrorCode::IoError
            },
            format!(
                "restore-test needs a new, isolated destination; {}: {e}",
                a.destination.display()
            ),
        )
    })?;
    let root = OsRestoreDir::open(&a.destination)?;
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let r = restore_selected(
        &src,
        &head,
        &[],
        root,
        &RestoreOptions::default(),
        &env.read,
        &ctx,
    )?;
    env.record_evidence(
        head.commit.archive_id,
        restore_evidence(head.seq(), head.commit_id.to_hex(), &r),
    );
    Ok(emit_restore(env, "restore-test", &head, &a.destination, &r))
}

/// Health evidence from a `restore-test`: recoverability `PASS` when every
/// entry of the commit was restored and verified, `FAIL` when an entry failed
/// its integrity check, and nothing when the restore was incomplete for a
/// reason that says nothing about the archive (a name the destination cannot
/// hold, for example).
fn restore_evidence(seq: u64, commit_id: String, r: &RestoreReport) -> Option<EvidenceRecord> {
    let (status, finding_codes) = if r.complete() {
        (Status::Pass, Vec::new())
    } else if r
        .exceptions
        .iter()
        .any(|e| matches!(e.kind, ExceptionKind::Integrity { .. }))
    {
        let mut codes = Vec::new();
        for f in r
            .findings()
            .iter()
            .filter(|f| f.severity == Severity::Error)
        {
            if !codes.contains(&f.code) {
                codes.push(f.code);
            }
        }
        (Status::Fail, codes)
    } else {
        return None;
    };
    Some(EvidenceRecord {
        schema: EVIDENCE_SCHEMA,
        command: EvidenceCommand::RestoreTest,
        level: Some(VerificationLevel::Restoration),
        completed_at: Timestamp::now().ok()?.to_string(),
        head: EvidenceHead { seq, commit_id },
        dimensions: [(Dimension::Recoverability, status)].into(),
        exit_code: restore_exit(r),
        scope: Some(
            "the whole commit restored into an isolated destination, every file verified \
             before it was published"
                .into(),
        ),
        finding_codes,
    })
}

// ---- verification -----------------------------------------------------------

fn level(l: Level) -> VerificationLevel {
    match l {
        Level::Structural => VerificationLevel::Structural,
        Level::Referential => VerificationLevel::Referential,
        Level::Stored => VerificationLevel::StoredIntegrity,
        Level::Content => VerificationLevel::ContentIntegrity,
        Level::Restoration => VerificationLevel::Restoration,
        Level::Inventory => VerificationLevel::Inventory,
        Level::Search => VerificationLevel::Search,
        Level::DisasterRecovery => VerificationLevel::DisasterRecovery,
    }
}

/// The archive ID the archive claims, read from its head commit record
/// only, to look up the local anchor. Any failure: no anchor (verification
/// reports the failure itself).
fn claimed_archive_id(src: &OsReadStorage, ro: &ReadOptions) -> Option<ArchiveId> {
    let loc = locate_head(src, &ro.limits).ok()?;
    read_commit(src, &loc.footer, ro)
        .ok()
        .map(|(c, _)| c.archive_id)
}

fn print_report(env: &mut Env<'_>, command: &str, r: &Report) {
    let heading = format!(
        "{command}: level {}",
        serde_json::to_value(r.level)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    );
    print_report_body(env, &heading, "skipped", r);
}

fn print_report_body(env: &mut Env<'_>, heading: &str, skipped_label: &str, r: &Report) {
    env.line(heading);
    if let Some(id) = &r.archive_id {
        env.line(format!("archive   {id}"));
    }
    if let Some(c) = &r.checked_commit {
        env.line(format!("commit    {c}"));
    }
    let anchor = serde_json::to_value(r.freshness_anchor)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    env.line(format!(
        "freshness anchor: {anchor}{}",
        r.expected_head
            .as_ref()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default()
    ));
    for (d, s) in &r.dimensions {
        let name = serde_json::to_value(d)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let required = if r.policy.required.contains(d) {
            "required"
        } else {
            ""
        };
        let status = serde_json::to_value(s)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        env.line(format!("  {name:<22}{status:<13}{required}"));
    }
    if let (Some(e), Some(c)) = (r.coverage.expected_objects, r.coverage.checked_objects) {
        env.line(format!(
            "data objects: {c} of {e} checked ({} of {} stored bytes)",
            r.coverage.checked_bytes.unwrap_or(0),
            r.coverage.expected_bytes.unwrap_or(0)
        ));
    }
    if !r.findings.is_empty() {
        env.line("findings:");
        print_findings(env, &r.findings);
    }
    for s in &r.skipped {
        env.line(format!("{skipped_label}: {} ({})", s.item, s.reason));
    }
    let policy = serde_json::to_value(r.policy_result)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    env.line(format!("result: {policy} (exit {})", r.exit_code));
    if r.dimensions
        .get(&Dimension::Freshness)
        .is_some_and(|s| !s.is_pass())
    {
        env.line(
            "freshness is not established: internal validity does not show this is the latest \
             copy (spec §5.7)",
        );
    }
}

pub fn verify_cmd(env: &mut Env<'_>, a: &VerifyArgs, deep: bool) -> Result<u8> {
    let command = if deep { "fsck" } else { "verify" };
    // A key only when the user gave one: otherwise an Encrypted archive is
    // verified without it (stored integrity; D20 item 12).
    env.unlock(
        &a.archive,
        if a.ask_passphrase {
            Need::Required
        } else {
            Need::IfGiven
        },
    )?;
    let src = open_archive(&a.archive)?;
    let anchor = if let Some(h) = &a.expected_head {
        FreshnessAnchor::User {
            commit_id: parse_commit_id(h)?,
        }
    } else {
        let mut anchor = FreshnessAnchor::None;
        if let Some(store) = env.store.clone() {
            if let Some(id) = claimed_archive_id(&src, &env.read) {
                match store.get(&id) {
                    Ok(Some(seen)) => {
                        anchor = FreshnessAnchor::LocalHistory {
                            seq: seen.seq,
                            commit_id: seen.commit_id,
                        }
                    }
                    Ok(None) => {}
                    Err(e) => env.warn(format!(
                        "the local head history could not be read, so freshness has no \
                         anchor: {e}"
                    )),
                }
            }
        }
        anchor
    };
    let opts = VerifyOptions {
        level: level(a.level),
        read: env.read.clone(),
        anchor,
        require_freshness: a.require_freshness,
        deep,
    };
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let v = verify(&src, &opts, &ctx);
    if let Err(violations) = v.report.validate() {
        // A report that breaks the invariants is never shown as a result.
        let msg: Vec<String> = violations.iter().map(ToString::to_string).collect();
        return Err(MochiError::new(
            ErrorCode::ReportInconsistent,
            format!("internal: the report is inconsistent: {}", msg.join("; ")),
        ));
    }
    if v.head_is_anchorable() {
        if let Some(h) = v.head {
            env.remember(h.archive_id, h.seq, h.commit_id);
        }
    }
    // Health evidence: every completed run with a verified head, failures
    // included (a failure must outlive the run that found it).
    if let Some(h) = v.head {
        let command = if deep {
            EvidenceCommand::Fsck
        } else {
            EvidenceCommand::Verify
        };
        let head = EvidenceHead {
            seq: h.seq,
            commit_id: h.commit_id.to_hex(),
        };
        env.record_evidence(
            h.archive_id,
            EvidenceRecord::from_report(command, &v.report, head),
        );
    }
    if env.json {
        let value = serde_json::to_value(&v.report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("rendering the report: {e}"))
        })?;
        env.emit_json(&value);
    } else {
        print_report(env, command, &v.report);
    }
    Ok(v.report.exit_code)
}

/// `mochi health` (spec §20, §21, §23.2; plan K5): what this machine has
/// recorded about the archive's health, judged against a policy for the
/// archive's current head. It runs no check and writes nothing; it opens the
/// archive only to locate the head and read the commit history that
/// freshness needs (no stored object is read or hashed).
pub fn health_cmd(env: &mut Env<'_>, a: &HealthArgs) -> Result<u8> {
    let policy = match &a.policy {
        None => HealthPolicy::default(),
        Some(p) => {
            let bytes = std::fs::read(p).map_err(|e| {
                MochiError::new(ErrorCode::IoError, format!("{}: {e}", p.display()))
            })?;
            let policy: HealthPolicy = serde_json::from_slice(&bytes)
                .map_err(|e| invalid(format!("{}: not a health policy ({e})", p.display())))?;
            policy
                .validate()
                .map_err(|e| invalid(format!("{}: {}", p.display(), e.message)))?;
            policy
        }
    };
    let src = open_archive(&a.archive)?;
    let loc = locate_head(&src, &env.read.limits)?;
    let (commit, commit_id) = read_commit(&src, &loc.footer, &env.read)?;
    let archive_id = commit.archive_id;
    let current = CurrentHead {
        archive_id: archive_id.to_hex(),
        seq: commit.seq,
        commit_id: commit_id.to_hex(),
    };

    // This machine's evidence and the head it last saw.
    let mut evidence = crate::state::EvidenceRead::default();
    let mut unavailable = None;
    let mut seen = None;
    match env.store.clone() {
        None => {
            unavailable = Some(
                "local history is off (--no-local-history) or has no location on this machine, \
                 so no recorded evidence was consulted"
                    .to_owned(),
            );
        }
        Some(store) => {
            match store.read_evidence(&archive_id) {
                Ok(e) => evidence = e,
                Err(e) => {
                    unavailable =
                        Some(format!("the evidence log could not be read: {}", e.message));
                    env.warn(format!("{e}"));
                }
            }
            if evidence.unreadable > 0 {
                env.warn(format!(
                    "{} line(s) of {} could not be read and were ignored; no dimension can pass \
                     until they are removed or the log is replaced",
                    evidence.unreadable,
                    store.evidence_path(&archive_id).display()
                ));
            }
            match store.get(&archive_id) {
                Ok(h) => seen = h,
                Err(e) => env.warn(format!(
                    "the local head history could not be read, so freshness has no anchor: {e}"
                )),
            }
        }
    }

    // Freshness as `verify` judges it: does the history contain the head
    // this client last saw?
    let mut freshness = FreshnessJudgement {
        status: Status::Unknown,
        findings: Vec::new(),
    };
    let mut freshness_note = None;
    if let Some(h) = seen {
        match commit_history(&src, &env.read) {
            Ok(history) => {
                freshness = judge_freshness_of(
                    &FreshnessAnchor::LocalHistory {
                        seq: h.seq,
                        commit_id: h.commit_id,
                    },
                    &history,
                );
            }
            Err(e) => {
                freshness_note = Some(format!(
                    "the commit history could not be read, so the head cannot be compared with \
                     the one last seen: {}",
                    e.message
                ));
            }
        }
    }

    let now = Timestamp::now()?;
    let report = health::assess(
        &Inputs {
            evidence: &evidence.records,
            unreadable_evidence: evidence.unreadable,
            evidence_unavailable: unavailable,
            current: &current,
            anchor: seen.map(|h| AnchorRef {
                seq: h.seq,
                commit_id: h.commit_id.to_hex(),
            }),
            freshness,
            freshness_note,
            now,
        },
        &policy,
    );
    if let Err(violations) = report.validate() {
        let msg: Vec<String> = violations.iter().map(ToString::to_string).collect();
        return Err(MochiError::new(
            ErrorCode::ReportInconsistent,
            format!("internal: the report is inconsistent: {}", msg.join("; ")),
        ));
    }
    if env.json {
        let value = serde_json::to_value(&report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("rendering the report: {e}"))
        })?;
        env.emit_json(&value);
    } else {
        let heading = format!(
            "health: {}",
            report.scope.as_deref().unwrap_or("recorded evidence only")
        );
        print_report_body(env, &heading, "note", &report);
        if let Some(age) = report.coverage.evidence_age_seconds {
            env.line(format!("oldest evidence used: {} day(s) old", age / 86_400));
        }
    }
    Ok(report.exit_code)
}

// ---- retention, checkpoint, compaction, collection (plan C9) ---------------------

/// Open `archive` for one new commit under its publication lock.
fn open_writer(
    archive: &Path,
    read: ReadOptions,
    kdf: Option<mochi_format::kdf::KdfParams>,
    lock_wait: Option<std::time::Duration>,
) -> Result<ArchiveWriter<OsStorage>> {
    // An append never needs a directory flush (plan O12); the default stays.
    let (mut dir, name) = location(archive, WindowsDirectoryFlush::default())?;
    let opts = WriterOptions {
        read,
        kdf,
        record_time: true,
        lock_wait,
        ..WriterOptions::default()
    };
    Ok(
        ArchiveWriter::open_append_in(&mut dir, &name, Box::new(OsIds), opts, TailRepair::Refuse)?
            .0,
    )
}

/// Publish `tx` (retention operations, or nothing for a forced checkpoint)
/// and report it. `what` describes the change for the text output.
fn commit_maintenance(
    env: &mut Env<'_>,
    command: &str,
    archive: &Path,
    mut w: ArchiveWriter<OsStorage>,
    tx: Transaction,
    what: &str,
    detail: Value,
) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let archive_id = w.archive_id();
    let outcome = w.commit(tx, &ctx)?;
    if let Err(e) = w.close() {
        env.warn(format!("closing the archive after the commit: {e}"));
    }
    env.remember_commit(
        archive_id,
        outcome.seq,
        outcome.commit_id,
        &outcome.durability,
    );
    let unconfirmed = match &outcome.durability {
        PublishDurability::Durable => None,
        PublishDurability::DirectoryUnconfirmed(why) => Some(why.clone()),
    };
    let code = if unconfirmed.is_none() {
        exit::OK
    } else {
        exit::DEGRADED
    };
    if env.json {
        env.emit_tagged(SCHEMA_MAINTENANCE, json!({
            "command": command,
            "archive": archive.display().to_string(),
            "status": outcome.status.as_str(),
            "archive_id": archive_id.to_hex(),
            "seq": outcome.seq,
            "commit_id": outcome.commit_id.to_hex(),
            "checkpoint": outcome.checkpoint,
            "durability": if unconfirmed.is_some() { "directory_unconfirmed" } else { "durable" },
            "durability_note": unconfirmed,
            "change": detail,
            "exit_code": code,
        }));
    } else {
        env.line(format!(
            "committed commit {} to {} ({}): {what}",
            outcome.seq,
            archive.display(),
            outcome.status.as_str(),
        ));
        env.line(format!("commit id {}", outcome.commit_id.to_hex()));
        if let Some(why) = &unconfirmed {
            env.line(format!(
                "degraded: the archive's directory entry is not confirmed durable ({why})"
            ));
        }
    }
    Ok(code)
}

fn require_confirm(confirm: bool, what: &str) -> Result<()> {
    if confirm {
        Ok(())
    } else {
        Err(invalid(format!(
            "{what} reduces retention (spec §16.3); repeat with --confirm to proceed. \
             Nothing was committed"
        )))
    }
}

pub fn snapshot_retain(env: &mut Env<'_>, a: &RetainArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    let mut tx = Transaction::new();
    tx.hold(a.label.as_bytes(), a.snapshot);
    let detail = json!({"hold": {"label": a.label, "seq": a.snapshot}});
    let what = format!(
        "legal hold {} placed on snapshot {}; it stays retained until the hold is released",
        text(a.label.as_bytes()),
        a.snapshot
    );
    commit_maintenance(env, "snapshot retain", &a.archive, w, tx, &what, detail)
}

pub fn snapshot_expire(env: &mut Env<'_>, a: &ExpireArgs) -> Result<u8> {
    require_confirm(a.confirm, "expiring a snapshot")?;
    env.unlock(&a.archive, Need::Required)?;
    let w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    let mut tx = Transaction::new();
    for s in &a.snapshots {
        tx.expire(*s);
    }
    let list = a
        .snapshots
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let what = format!(
        "snapshot{} {list} expired; still in this file, and kept by any legal hold. \
         `mochi gc plan` shows what a new archive could leave out",
        if a.snapshots.len() == 1 { "" } else { "s" }
    );
    let detail = json!({"expire": a.snapshots});
    commit_maintenance(env, "snapshot expire", &a.archive, w, tx, &what, detail)
}

pub fn snapshot_release(env: &mut Env<'_>, a: &ReleaseArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    require_confirm(a.confirm, "releasing a legal hold")?;
    let w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    let mut tx = Transaction::new();
    tx.release(a.label.as_bytes());
    let what = format!("legal hold {} released", text(a.label.as_bytes()));
    let detail = json!({"release": {"label": a.label}});
    commit_maintenance(env, "snapshot release", &a.archive, w, tx, &what, detail)
}

pub fn checkpoint(env: &mut Env<'_>, a: &CheckpointArgs) -> Result<u8> {
    env.unlock(&a.archive, Need::Required)?;
    let mut w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    w.request_checkpoint();
    commit_maintenance(
        env,
        "checkpoint",
        &a.archive,
        w,
        Transaction::new(),
        "checkpoint written and verified against its snapshot before adoption (spec §18.1); \
         the namespace is unchanged",
        Value::Null,
    )
}

/// Totals and snapshots of a GC plan, for the text output.
fn print_gc_plan(env: &mut Env<'_>, p: &GcPlan) {
    env.line(format!(
        "plan for archive {} at commit {} ({})",
        p.archive_id, p.head.seq, p.head.commit_id
    ));
    let seqs = |v: &[u64]| {
        if v.is_empty() {
            "none".to_string()
        } else {
            v.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
        }
    };
    env.line(format!("retained snapshots: {}", seqs(&p.roots)));
    env.line(format!("expired: {}", seqs(&p.expired)));
    for h in &p.holds {
        env.line(format!(
            "legal hold {} on snapshot {}",
            text(h.label.as_bytes()),
            h.seq
        ));
    }
    for c in &p.collectable_snapshots {
        env.line(format!("collectable snapshot {}: {}", c.seq, c.reason));
    }
    env.line(format!(
        "kept: {} file versions, {} chunks, {} stored bytes",
        p.retained.file_versions, p.retained.chunks, p.retained.stored_bytes
    ));
    env.line(format!(
        "collectable: {} file versions, {} chunks, {} stored bytes",
        p.collectable.file_versions, p.collectable.chunks, p.collectable.stored_bytes
    ));
    if p.stream_framing.chunks > 0 {
        env.line(format!(
            "stream framing (TAR stream bytes, not collectable): {} chunks, {} stored bytes",
            p.stream_framing.chunks, p.stream_framing.stored_bytes
        ));
    }
}

pub fn gc_plan(env: &mut Env<'_>, a: &GcPlanArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let p = gc::plan(&src, &env.read, &ctx)?;
    let value = serde_json::to_value(&p)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("encoding the plan: {e}")))?;
    if let Some(out) = &a.output {
        write_new_file(out, format!("{value:#}\n").as_bytes())?;
    }
    if env.json {
        if let Some(out) = &a.output {
            env.emit_tagged(
                SCHEMA_PLAN_SAVED,
                json!({
                    "command": "gc plan",
                    "plan_file": out.display().to_string(),
                    "collects_anything": p.collects_anything(),
                    "plan": value,
                }),
            );
        } else {
            env.emit_json(&value);
        }
    } else {
        print_gc_plan(env, &p);
        match &a.output {
            Some(out) => env.line(format!(
                "plan written to {}; nothing was changed. Apply it with \
                 `mochi gc apply {} --plan {} --output NEW_ARCHIVE`",
                out.display(),
                a.archive.display(),
                out.display()
            )),
            None => env.line("nothing was changed; save a plan with --output to apply it"),
        }
    }
    Ok(exit::OK)
}

/// Create `path` with `bytes`; never replaces an existing file.
fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::AlreadyExists {
                ErrorCode::DestinationExists
            } else {
                ErrorCode::IoError
            };
            MochiError::new(code, format!("{}: {e}", path.display()))
        })?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{}: {e}", path.display())))
}

/// Read a saved plan: its JSON value, archive ID, and head.
fn read_plan(path: &Path) -> Result<(Value, String, PlanHead)> {
    let bad = |why: String| invalid(format!("{}: not a gc plan ({why})", path.display()));
    let bytes = std::fs::read(path)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{}: {e}", path.display())))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    let archive_id = value
        .get("archive_id")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no archive_id".into()))?
        .to_owned();
    let head: PlanHead = value
        .get("head")
        .cloned()
        .ok_or_else(|| bad("no head".into()))
        .and_then(|h| serde_json::from_value(h).map_err(|e| bad(e.to_string())))?;
    Ok((value, archive_id, head))
}

fn rewrite(
    env: &mut Env<'_>,
    command: &str,
    archive: &Path,
    output: &Path,
    no_verify_content: bool,
    plan: Option<&Path>,
) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let saved = plan.map(read_plan).transpose()?;
    env.unlock(archive, Need::Required)?;
    // Checked first so that naming the source itself (whose lock is about
    // to be taken) reads as what it is. Publication refuses an existing
    // name again, without a race (D13).
    if std::fs::symlink_metadata(output).is_ok() {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            format!(
                "{} exists; a new archive never replaces anything. Nothing was written",
                output.display()
            ),
        ));
    }
    let (mut out_dir, out_name) = location(output, env.flush)?;
    // The source's publication lock is held from here until the new archive
    // is published, so no commit can land in between (D18).
    let w = open_writer(archive, env.read.clone(), None, env.lock_wait)?;
    let keep = match &saved {
        None => Keep::Every,
        Some((value, archive_id, head)) => {
            if *archive_id != w.archive_id().to_hex() {
                return Err(invalid(format!(
                    "the plan is for archive {archive_id}, not {} ({})",
                    w.archive_id().to_hex(),
                    archive.display()
                )));
            }
            // Apply exactly what was approved: plan again under the lock and
            // refuse any difference, a moved head included.
            let now = gc::plan(w.storage(), &env.read, &ctx)?;
            let now = serde_json::to_value(&now).map_err(|e| {
                MochiError::new(ErrorCode::IoError, format!("encoding the plan: {e}"))
            })?;
            if now != *value {
                return Err(invalid(format!(
                    "the plan no longer matches the archive (it was made at commit {}; the \
                     archive or its retention changed since): run `mochi gc plan` again. \
                     Nothing was written",
                    head.seq
                )));
            }
            Keep::Roots(head.clone())
        }
    };
    let options = CompactOptions {
        // An Encrypted source is opened with the passphrases given, and the
        // new archive is wrapped under those same ones (D20 item 10).
        read: env.read.clone(),
        kdf: None,
        verify_content: !no_verify_content,
        ..CompactOptions::default()
    };
    let report = compact::compact(
        &w,
        &mut out_dir,
        &out_name,
        Box::new(OsIds),
        &keep,
        &options,
        &ctx,
    )?;
    if let Err(e) = w.close() {
        env.warn(format!("releasing the source archive's lock: {e}"));
    }
    // Remember the new archive's head: a later verify of it then has an
    // anchor (D8), and health knows how its publication went.
    env.remember_rewrite(output, report.durability_unconfirmed.as_ref());
    let code = if report.durability_unconfirmed.is_none() {
        exit::OK
    } else {
        exit::DEGRADED
    };
    if env.json {
        let mut v = serde_json::to_value(&report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("encoding the report: {e}"))
        })?;
        if let Value::Object(m) = &mut v {
            m.insert("command".into(), json!(command));
            m.insert("source".into(), json!(archive.display().to_string()));
            m.insert("output".into(), json!(output.display().to_string()));
            m.insert("exit_code".into(), json!(code));
        }
        env.emit_tagged(SCHEMA_REWRITE, v);
    } else {
        env.line(format!(
            "wrote {} ({} bytes, archive {}): {} commit{} reproducing source snapshot{} {}",
            output.display(),
            report.new_len,
            report.new_archive_id,
            report.commits.len(),
            if report.commits.len() == 1 { "" } else { "s" },
            if report.commits.len() == 1 { "" } else { "s" },
            report
                .commits
                .iter()
                .map(|c| c.source_seq.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !report.collected.is_empty() {
            env.line(format!(
                "left out snapshot{}: {}",
                if report.collected.len() == 1 { "" } else { "s" },
                report
                    .collected
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        env.line(format!(
            "{} chunks ({} stored bytes) {}; {} file versions{}",
            report.chunks_copied,
            report.stored_bytes_copied,
            if report.resealed {
                "opened and sealed again under the new archive's key"
            } else {
                "copied byte for byte"
            },
            report.file_versions,
            match report.versions_verified {
                Some(n) => format!(", {n} read back and verified"),
                None => ", NOT read back (--no-verify-content)".to_string(),
            }
        ));
        if let Some(why) = &report.durability_unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
        }
        env.line(format!(
            "the source {} is unchanged and kept; the new archive has a new archive ID, so \
             its first verification has no freshness anchor. Remove the source yourself only \
             once you no longer need it",
            archive.display()
        ));
    }
    Ok(code)
}

/// `mochi migrate SRC --output DST` (spec §26, B.2.14; plan K25): the one
/// command that reads the last draft. The source is opened read-only, takes
/// no lock, and is never written; an Encrypted source needs its passphrase
/// before anything is created (the new archive is sealed under a new data key).
pub fn migrate_cmd(env: &mut Env<'_>, a: &MigrateArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    // The one reader of the last draft; every other command keeps the default
    // and refuses a draft archive.
    env.read.drafts = DraftPolicy::AllowLastDraft;
    env.unlock(&a.archive, Need::Required)?;
    // Checked first, so that naming the source itself reads as what it is.
    // Publication refuses an existing name again, without a race (D13).
    if std::fs::symlink_metadata(&a.output).is_ok() {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            format!(
                "{} exists; a new archive never replaces anything. Nothing was written",
                a.output.display()
            ),
        ));
    }
    let (mut out_dir, out_name) = location(&a.output, env.flush)?;
    let src = open_archive(&a.archive)?;
    let options = MigrateOptions {
        read: env.read.clone(),
        ..MigrateOptions::default()
    };
    let report = migrate::migrate(
        &src,
        &mut out_dir,
        &out_name,
        Box::new(OsIds),
        &options,
        &ctx,
    )?;
    // Remember the new archive's head: a later verify of it then has an
    // anchor (D8), and health knows how its publication went.
    env.remember_rewrite(&a.output, report.durability_unconfirmed.as_ref());
    let code = if report.durability_unconfirmed.is_none() {
        exit::OK
    } else {
        exit::DEGRADED
    };
    if env.json {
        let mut v = serde_json::to_value(&report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("encoding the report: {e}"))
        })?;
        if let Value::Object(m) = &mut v {
            m.insert("command".into(), json!("migrate"));
            m.insert("source".into(), json!(a.archive.display().to_string()));
            m.insert("output".into(), json!(a.output.display().to_string()));
            m.insert("exit_code".into(), json!(code));
        }
        env.emit_tagged(SCHEMA_MIGRATE, v);
    } else {
        let n = report.commits.len();
        env.line(format!(
            "wrote {} ({} bytes, archive {}): a 1.0 archive reproducing the draft {} archive {}",
            a.output.display(),
            report.new_len,
            report.new_archive_id,
            report.source_draft,
            report.source_archive_id,
        ));
        env.line(format!(
            "{n} commit{} reproduced one for one; {} chunks ({} stored bytes) {}; {} file versions",
            if n == 1 { "" } else { "s" },
            report.chunks_copied,
            report.stored_bytes_copied,
            if report.resealed {
                "opened and sealed again under the new archive's key"
            } else {
                "copied byte for byte"
            },
            report.file_versions,
        ));
        let c = &report.comparison;
        env.line(format!(
            "compared with the source: {} snapshot namespaces, {} file versions (record, hash, \
             extents), {} attribute sets, retention state identical; {} file versions read back \
             and hash-verified",
            c.snapshots_compared,
            c.file_versions_compared,
            c.attributes_compared,
            c.versions_read_back,
        ));
        env.line(format!(
            "the source verified at the {} level before conversion (exit {}), and the new \
             archive at the {} level before it was published (exit {})",
            report.source_verification.level,
            report.source_verification.exit_code,
            report.new_verification.level,
            report.new_verification.exit_code,
        ));
        if let Some(why) = &report.durability_unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
        }
        env.line(format!(
            "the source {} is unchanged and kept; the new archive has a new archive ID, so its \
             first verification has no freshness anchor. {}",
            a.archive.display(),
            report.not_done
        ));
    }
    Ok(code)
}

pub fn compact_cmd(env: &mut Env<'_>, a: &CompactArgs) -> Result<u8> {
    rewrite(
        env,
        "compact",
        &a.archive,
        &a.output,
        a.no_verify_content,
        None,
    )
}

pub fn gc_apply(env: &mut Env<'_>, a: &GcApplyArgs) -> Result<u8> {
    rewrite(
        env,
        "gc apply",
        &a.archive,
        &a.output,
        a.no_verify_content,
        Some(&a.plan),
    )
}

// ---- repair (C8) ---------------------------------------------------------------

/// Bytes from the plan's lowercase hex fields (paths, labels), for display
/// through [`text`]; anything malformed is dropped.
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .filter_map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect()
}

fn seq_list(v: &[u64]) -> String {
    if v.is_empty() {
        "none".to_string()
    } else {
        v.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
    }
}

fn outcome_line(o: Outcome) -> &'static str {
    match o {
        Outcome::Complete => {
            "complete: every snapshot and every entry can be written to a new archive"
        }
        Outcome::Partial => {
            "PARTIAL: some snapshots, entries, retention, or bytes after the head cannot be \
             recovered (listed above); a repair would be a partial salvage"
        }
        Outcome::NothingRecoverable => {
            "nothing recoverable through §22 steps 1–6; the salvage scan (step 7) is not built"
        }
    }
}

/// The copies named with `--copy`, opened read-only. A copy that is the
/// archive itself is refused (D22 item 7).
fn open_copies(archive: &Path, paths: &[PathBuf]) -> Result<Vec<(String, OsReadStorage)>> {
    let own = std::fs::canonicalize(archive).ok();
    let mut out = Vec::new();
    for path in paths {
        if own.is_some() && std::fs::canonicalize(path).ok() == own {
            return Err(invalid(format!(
                "--copy {} is the archive being repaired; a copy is a different file",
                path.display()
            )));
        }
        out.push((path.display().to_string(), open_archive(path)?));
    }
    Ok(out)
}

fn copy_sources(opened: &[(String, OsReadStorage)]) -> Vec<CopySource<'_>> {
    opened
        .iter()
        .map(|(label, storage)| CopySource {
            label: label.clone(),
            storage,
        })
        .collect()
}

fn print_repair_plan(env: &mut Env<'_>, p: &RepairPlan) {
    env.line(format!(
        "repair plan for archive {} ({} bytes)",
        p.archive_id.as_deref().unwrap_or("(unknown: no head)"),
        p.source_len
    ));
    if let Some(h) = &p.head {
        env.line(format!(
            "head: commit {} ({}), found {}",
            h.seq,
            h.commit_id,
            if h.found_by == "eof" {
                "at the end of the file"
            } else {
                "by scanning for the latest valid footer"
            }
        ));
    }
    if let Some(t) = p.tail.as_ref().filter(|t| t.state != "clean") {
        env.line(format!(
            "tail: {} bytes after the head's footer, {}{}",
            t.len,
            t.state,
            t.detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ));
    }
    if let Some(b) = &p.chain_break {
        env.line(format!(
            "chain: breaks below commit {} ({}: {}); earlier commit records are not trusted",
            b.first_trusted_seq, b.reason.code, b.reason.message
        ));
    }
    for s in &p.ladder {
        let status = match s.status {
            repair::StepStatus::Used => "used",
            repair::StepStatus::NotNeeded => "not needed",
            repair::StepStatus::NotAvailable => "not available",
            repair::StepStatus::NotAttempted => "NOT ATTEMPTED",
            repair::StepStatus::Incomplete => "INCOMPLETE",
        };
        let detail = if s.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", s.detail)
        };
        env.line(format!("step {} {}: {status}{detail}", s.step, s.name));
    }
    for (i, c) in p.copies.iter().enumerate() {
        let m = match c.matched {
            Some(mochi_core::copies::CopyMatch::IdenticalDescriptor) => "identical descriptor",
            Some(mochi_core::copies::CopyMatch::DescriptorHash) => "descriptor hash",
            Some(mochi_core::copies::CopyMatch::NotACopy) => "NOT A COPY",
            None => "undetermined",
        };
        let st = match c.status {
            mochi_core::copies::CopyStatus::Used => "used",
            mochi_core::copies::CopyStatus::NotNeeded => "not needed",
            mochi_core::copies::CopyStatus::NotAvailable => "not available",
            mochi_core::copies::CopyStatus::Unreadable => "UNREADABLE",
        };
        env.line(format!(
            "copy {} {} ({} bytes, {m}): {st}; donated {} frame{} ({} bytes); {}",
            i + 1,
            text(c.path.as_bytes()),
            c.len,
            c.frames_donated,
            if c.frames_donated == 1 { "" } else { "s" },
            c.bytes_donated,
            text(c.detail.as_bytes())
        ));
    }
    for d in &p.donations {
        env.line(format!(
            "frame at offset {} ({} bytes, {:?}) comes from copy {}",
            d.offset,
            d.len,
            d.kind,
            d.copy + 1
        ));
    }
    let seqs: Vec<u64> = p.snapshots.iter().map(|s| s.seq).collect();
    env.line(format!(
        "recoverable snapshots ({}): {}",
        seqs.len(),
        seq_list(&seqs)
    ));
    let by_salvage: Vec<u64> = p
        .snapshots
        .iter()
        .filter(|s| s.trust == mochi_core::copies::Trust::Salvage)
        .map(|s| s.seq)
        .collect();
    if !by_salvage.is_empty() {
        env.line(format!(
            "snapshots found by the salvage scan (no trusted head vouches for them; the repair \
             is partial): {}",
            seq_list(&by_salvage)
        ));
    }
    if let Some(v) = &p.salvage {
        print_salvage(env, v);
    }
    let by_copy: Vec<u64> = p
        .snapshots
        .iter()
        .filter(|s| s.trust == mochi_core::copies::Trust::CopyConfirmed)
        .map(|s| s.seq)
        .collect();
    if !by_copy.is_empty() {
        env.line(format!(
            "snapshots confirmed by a copy alone (the source cannot show the commit): {}",
            seq_list(&by_copy)
        ));
    }
    for l in &p.lost_snapshots {
        env.line(format!(
            "lost snapshot {}: {}: {}",
            l.seq, l.reason.code, l.reason.message
        ));
    }
    for o in &p.omitted {
        env.line(format!(
            "left out {} {} (version {}) of snapshot{} {}: {}: {}",
            o.kind,
            text(&unhex(&o.path_hex)),
            o.version_id,
            if o.snapshots.len() == 1 { "" } else { "s" },
            seq_list(&o.snapshots),
            o.reason.code,
            o.reason.message
        ));
    }
    for m in &p.manifest_failures {
        env.line(format!(
            "damaged {} manifest of commit {}: {}: {}",
            m.kind, m.seq, m.reason.code, m.reason.message
        ));
    }
    match &p.retention {
        Some(RetentionPlan::Carried { holds, expired }) => {
            env.line(format!(
                "retention: carried; expired: {}",
                seq_list(expired)
            ));
            for h in holds {
                let label = text(&unhex(&h.label_hex));
                match h.new_seq {
                    Some(n) => env.line(format!(
                        "legal hold {label} on snapshot {} carried to new commit {n}",
                        h.seq
                    )),
                    None => env.line(format!(
                        "legal hold {label} is LOST with snapshot {}",
                        h.seq
                    )),
                }
            }
        }
        Some(RetentionPlan::Unresolved { reason }) => env.line(format!(
            "retention: UNRESOLVED ({}: {}); applying needs --accept-retention-loss, and the \
             new archive would have no legal holds and nothing expired",
            reason.code, reason.message
        )),
        None => {}
    }
    env.line(format!("outcome: {}", outcome_line(p.outcome)));
}

/// What the salvage scan found (Annex B.2.13 D23 item 7).
fn print_salvage(env: &mut Env<'_>, v: &mochi_core::salvage::SalvageReport) {
    env.line(format!(
        "salvage scan: {} candidate{} examined (limit {}{}), {} frame{} walked, {} \
         resynchronization{}; archive {} (from the {})",
        v.candidates,
        if v.candidates == 1 { "" } else { "s" },
        v.candidate_limit,
        if v.limit_reached { ", REACHED" } else { "" },
        v.frames_walked,
        if v.frames_walked == 1 { "" } else { "s" },
        v.resyncs,
        if v.resyncs == 1 { "" } else { "s" },
        v.archive_id.as_deref().unwrap_or("none"),
        v.archive_id_source
    ));
    for c in &v.confirmed {
        env.line(format!(
            "  confirmed commit {} ({}) at offset {}, by {}",
            c.seq, c.commit_id, c.offset, c.by
        ));
    }
    for c in &v.unconfirmed {
        env.line(format!(
            "  NOT recovered: commit {} ({}) at offset {}: {}",
            c.seq, c.commit_id, c.offset, c.by
        ));
    }
    for c in &v.rejected {
        env.line(format!(
            "  rejected: commit {} ({}) at offset {}: {}",
            c.seq, c.commit_id, c.offset, c.by
        ));
    }
    for c in &v.contradictory {
        let ids: Vec<String> = c
            .commits
            .iter()
            .map(|x| format!("{} at {}", x.commit_id, x.offset))
            .collect();
        env.line(format!(
            "  NOT recovered: contradictory commits at sequence {}: {}",
            c.seq,
            ids.join("; ")
        ));
    }
    if v.foreign_commits > 0 {
        env.line(format!(
            "  {} commit{} of another archive ignored",
            v.foreign_commits,
            if v.foreign_commits == 1 { "" } else { "s" }
        ));
    }
    if v.orphan_data_objects > 0 {
        env.line(format!(
            "  {} data object{} ({} bytes) belong{} to no recovered commit: counted, never \
             placed in a namespace",
            v.orphan_data_objects,
            if v.orphan_data_objects == 1 { "" } else { "s" },
            v.orphan_bytes,
            if v.orphan_data_objects == 1 { "s" } else { "" }
        ));
    }
    if let Some(i) = &v.profile_inference {
        env.line(format!(
            "  profile: {}; encrypted: {}, recovery record: {}; writer parameters: {}",
            i.evidence,
            i.encrypted,
            i.redundancy_percent
                .map_or("none".to_owned(), |p| format!("{p} %")),
            i.writer_parameters
        ));
        if !i.profile_constraints_lost.is_empty() {
            env.line(format!(
                "  LOST in the repaired archive: {} (a repaired archive has its own ordinary \
                 descriptor)",
                i.profile_constraints_lost.join(", ")
            ));
        }
    }
    if v.lists_truncated {
        env.line("  (lists are cut short; the counts and --json carry the totals)");
    }
}

/// `--salvage` and `--archive-id` as core takes them.
fn salvage_options(salvage: bool, archive_id: &Option<String>) -> Result<Option<SalvageOptions>> {
    if !salvage {
        return Ok(None);
    }
    let archive_id = match archive_id {
        None => None,
        Some(h) => {
            let h = h.trim();
            let bytes: Option<Vec<u8>> = (h.len() == 64 && h.is_ascii())
                .then(|| {
                    (0..64)
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok())
                        .collect()
                })
                .flatten();
            let bytes: [u8; 32] = bytes
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| invalid("--archive-id: expected 64 hexadecimal digits"))?;
            Some(ArchiveId::from_bytes(bytes))
        }
    };
    Ok(Some(SalvageOptions { archive_id }))
}

pub fn repair_plan(env: &mut Env<'_>, a: &RepairPlanArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    env.unlock(&a.archive, Need::Required)?;
    let src = open_archive(&a.archive)?;
    let opened = open_copies(&a.archive, &a.copy)?;
    let salvage = salvage_options(a.salvage, &a.archive_id)?;
    let p = repair::plan_with_salvage(
        &src,
        &copy_sources(&opened),
        salvage.as_ref(),
        &env.read,
        &ctx,
    )?;
    let value = serde_json::to_value(&p)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("encoding the plan: {e}")))?;
    if let Some(out) = &a.output {
        write_new_file(out, format!("{value:#}\n").as_bytes())?;
    }
    if env.json {
        if let Some(out) = &a.output {
            env.emit_tagged(
                SCHEMA_PLAN_SAVED,
                json!({
                    "command": "repair plan",
                    "plan_file": out.display().to_string(),
                    "outcome": p.outcome,
                    "exit_code": p.exit_code,
                    "plan": value,
                }),
            );
        } else {
            env.emit_json(&value);
        }
    } else {
        print_repair_plan(env, &p);
        match (&a.output, p.outcome) {
            (_, Outcome::NothingRecoverable) => {
                env.line("nothing was changed; there is nothing to apply")
            }
            (Some(out), _) => env.line(format!(
                "plan written to {}; nothing was changed. Apply it with \
                 `mochi repair apply {} --plan {} --output NEW_ARCHIVE`",
                out.display(),
                a.archive.display(),
                out.display()
            )),
            (None, _) => env.line("nothing was changed; save a plan with --output to apply it"),
        }
    }
    Ok(p.exit_code)
}

pub fn repair_apply(env: &mut Env<'_>, a: &RepairApplyArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    env.unlock(&a.archive, Need::Required)?;
    let bytes = std::fs::read(&a.plan)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{}: {e}", a.plan.display())))?;
    let approved: RepairPlan = serde_json::from_slice(&bytes).map_err(|e| {
        invalid(format!(
            "{}: not a repair plan of this build ({e})",
            a.plan.display()
        ))
    })?;
    if std::fs::symlink_metadata(&a.output).is_ok() {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            format!(
                "{} exists; a new archive never replaces anything. Nothing was written",
                a.output.display()
            ),
        ));
    }
    let (mut out_dir, out_name) = location(&a.output, env.flush)?;
    // The source is only read: repair never writes to it (§22.2).
    let src = open_archive(&a.archive)?;
    let opened = open_copies(&a.archive, &a.copy)?;
    let options = RepairOptions {
        accept_retention_loss: a.accept_retention_loss,
        kdf: None,
        ..RepairOptions::default()
    };
    let salvage = salvage_options(a.salvage, &a.archive_id)?;
    let report = repair::apply_with_salvage(
        &src,
        &copy_sources(&opened),
        salvage.as_ref(),
        &approved,
        &mut out_dir,
        &out_name,
        Box::new(OsIds),
        &env.read,
        &options,
        &ctx,
    )?;
    // Remember the new archive's head: a later verify of it then has an
    // anchor (D8).
    match open_archive(&a.output).and_then(|s| open_head(&s, &env.read)) {
        Ok(h) => {
            let durability = match &report.durability_unconfirmed {
                None => PublishDurability::Durable,
                Some(why) => PublishDurability::DirectoryUnconfirmed(why.clone()),
            };
            env.remember_commit(h.commit.archive_id, h.seq(), h.commit_id, &durability);
            // The re-verification before publication is evidence about the
            // new archive, but only a complete repair says nothing was left
            // out: a partial one is not recorded (the user runs `verify`).
            if report.outcome == Outcome::Complete
                && report.reverification.policy_result == Status::Pass
            {
                if let Ok(now) = Timestamp::now() {
                    env.record_evidence(
                        h.commit.archive_id,
                        Some(EvidenceRecord {
                            schema: EVIDENCE_SCHEMA,
                            command: EvidenceCommand::RepairApply,
                            level: Some(VerificationLevel::Restoration),
                            completed_at: now.to_string(),
                            head: EvidenceHead {
                                seq: h.seq(),
                                commit_id: h.commit_id.to_hex(),
                            },
                            dimensions: [
                                (Dimension::Integrity, Status::Pass),
                                (Dimension::Recoverability, Status::Pass),
                            ]
                            .into(),
                            exit_code: report.reverification.exit_code,
                            scope: Some(
                                "the new archive re-verified before publication (restoration \
                                 level, fsck depth)"
                                    .into(),
                            ),
                            finding_codes: Vec::new(),
                        }),
                    );
                }
            }
        }
        Err(e) => env.warn(format!("the new archive's head was not recorded: {e}")),
    }
    if env.json {
        let mut v = serde_json::to_value(&report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("encoding the report: {e}"))
        })?;
        if let Value::Object(m) = &mut v {
            m.insert("command".into(), json!("repair apply"));
            m.insert("source".into(), json!(a.archive.display().to_string()));
            m.insert("output".into(), json!(a.output.display().to_string()));
        }
        env.emit_tagged(SCHEMA_REPAIR_APPLY, v);
    } else {
        let label = match report.outcome {
            Outcome::Complete => "repaired (complete)",
            _ => "PARTIAL repair (salvage)",
        };
        env.line(format!(
            "{label}: wrote {} ({} bytes, archive {}) with {} commit{} from source snapshot{} {}",
            a.output.display(),
            report.new_len,
            report.new_archive_id,
            report.commits.len(),
            if report.commits.len() == 1 { "" } else { "s" },
            if report.commits.len() == 1 { "" } else { "s" },
            seq_list(
                &report
                    .commits
                    .iter()
                    .map(|c| c.source_seq)
                    .collect::<Vec<_>>()
            )
        ));
        if !report.lost_snapshots.is_empty() {
            let lost: Vec<u64> = report.lost_snapshots.iter().map(|l| l.seq).collect();
            env.line(format!(
                "NOT recovered: snapshot{} {}",
                if lost.len() == 1 { "" } else { "s" },
                seq_list(&lost)
            ));
        }
        if !report.omitted.is_empty() {
            env.line(format!(
                "NOT recovered: {} entr{} left out (listed in the plan and in --json output)",
                report.omitted.len(),
                if report.omitted.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ));
        }
        if let Some(v) = &report.salvage {
            print_salvage(env, v);
        }
        if report.retention_loss_accepted {
            env.line(
                "retention NOT carried (--accept-retention-loss): the new archive has no legal \
                 holds and nothing expired",
            );
        }
        env.line(format!(
            "re-verified before publication ({}): {:?}",
            report.reverification.level, report.reverification.overall_status
        ));
        if let Some(why) = &report.durability_unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
        }
        env.line(format!(
            "the source {} is unchanged and kept; the new archive has a new archive ID, so \
             its first verification has no freshness anchor",
            a.archive.display()
        ));
    }
    Ok(report.exit_code)
}

// ---- rekey (C11) ---------------------------------------------------------------

fn kdf_text(k: &mochi_format::kdf::KdfParams) -> String {
    format!(
        "argon2id m={} MiB t={} p={}",
        k.memory_kib / 1024,
        k.iterations,
        k.lanes
    )
}

/// The new passphrases of `--add-passphrase` and `--reencrypt`: the named
/// files, else a prompt asked twice.
fn new_passphrases(
    env: &mut Env<'_>,
    a: &RekeyArgs,
) -> Result<Vec<mochi_format::secret::Passphrase>> {
    let named = Sources {
        files: a.new_passphrase_file.clone(),
        env: false,
    };
    let mut list = passphrase::explicit(&named, env.err)?;
    if list.is_empty() {
        if !passphrase::can_prompt() {
            return Err(invalid(
                "a new passphrase is needed: give --new-passphrase-file PATH or run on a \
                 terminal to be asked. Nothing was done",
            ));
        }
        list.push(passphrase::ask_new(
            &a.archive.display().to_string(),
            env.err,
        )?);
    }
    Ok(list)
}

pub fn rekey(env: &mut Env<'_>, a: &RekeyArgs) -> Result<u8> {
    use mochi_core::rekey;
    if a.list {
        let src = open_archive(&a.archive)?;
        let envelopes = rekey::list(&src, &env.read)?;
        let archive_id = commit_history(&src, &env.read)?
            .pop()
            .map(|h| h.commit.archive_id.to_hex());
        if env.json {
            let items: Vec<Value> = envelopes
                .iter()
                .map(|e| {
                    json!({
                        "envelope_id": e.envelope_id,
                        "created_at_seq": e.created_at_seq,
                        "kdf": {
                            "algorithm": "argon2id",
                            "memory_kib": e.kdf.memory_kib,
                            "iterations": e.kdf.iterations,
                            "lanes": e.kdf.lanes,
                        },
                    })
                })
                .collect();
            env.emit_tagged(
                SCHEMA_REKEY_LIST,
                json!({
                    "command": "rekey",
                    "action": "list",
                    "archive": a.archive.display().to_string(),
                    "archive_id": archive_id,
                    "envelopes": items,
                    "exit_code": exit::OK,
                }),
            );
        } else {
            env.line(format!(
                "{} key envelope{} valid at the head of {}:",
                envelopes.len(),
                if envelopes.len() == 1 { "" } else { "s" },
                a.archive.display()
            ));
            for e in &envelopes {
                env.line(format!(
                    "  {}  written by commit {}  {}",
                    e.envelope_id,
                    e.created_at_seq,
                    kdf_text(&e.kdf)
                ));
            }
        }
        return Ok(exit::OK);
    }

    if a.reencrypt {
        return reencrypt_cmd(env, a);
    }

    let remove: Vec<[u8; 16]> = a
        .remove_passphrase
        .iter()
        .map(|s| rekey::parse_envelope_id(s))
        .collect::<Result<_>>()?;
    if !a.add_passphrase && remove.is_empty() {
        return Err(invalid(
            "choose one of --list, --add-passphrase, --remove-passphrase ENVELOPE_ID, or \
             --reencrypt -o NEW_ARCHIVE",
        ));
    }
    env.unlock(&a.archive, Need::Required)?;
    if !is_encrypted(&a.archive, &env.read) {
        return Err(invalid(format!(
            "{} is not an Encrypted archive: it has no key envelopes to change",
            a.archive.display()
        )));
    }
    let add = if a.add_passphrase {
        new_passphrases(env, a)?
    } else {
        Vec::new()
    };
    let added = add.len();
    let tx = rekey::rewrap_transaction(add, remove.clone())?;
    let w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    let what = format!(
        "key envelopes changed: {added} added, {} removed; the data key and the data are \
         unchanged. Removing an envelope is not revocation: the envelope stays in the file's \
         history, so the removed passphrase still yields the data key from this file and from \
         any copy, and that key also decrypts content added later. Only `rekey --reencrypt` \
         writes a file under a new data key",
        remove.len()
    );
    let detail = json!({
        "added": added,
        "removed": remove.iter().map(|id| hex(id)).collect::<Vec<_>>(),
        "revocation": false,
    });
    commit_maintenance(env, "rekey", &a.archive, w, tx, &what, detail)
}

fn reencrypt_cmd(env: &mut Env<'_>, a: &RekeyArgs) -> Result<u8> {
    let Some(output) = a.output.as_deref() else {
        return Err(invalid("--reencrypt needs --output NEW_ARCHIVE"));
    };
    env.unlock(&a.archive, Need::Required)?;
    if !is_encrypted(&a.archive, &env.read) {
        return Err(invalid(format!(
            "{} is not an Encrypted archive: there is nothing to re-encrypt",
            a.archive.display()
        )));
    }
    let new = new_passphrases(env, a)?;
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    if std::fs::symlink_metadata(output).is_ok() {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            format!(
                "{} exists; a new archive never replaces anything. Nothing was written",
                output.display()
            ),
        ));
    }
    let (mut out_dir, out_name) = location(output, env.flush)?;
    let w = open_writer(&a.archive, env.read.clone(), None, env.lock_wait)?;
    let report = mochi_core::rekey::reencrypt(
        &w,
        &mut out_dir,
        &out_name,
        Box::new(OsIds),
        env.read.clone(),
        new,
        None,
        &ctx,
    )?;
    if let Err(e) = w.close() {
        env.warn(format!("releasing the source archive's lock: {e}"));
    }
    env.remember_rewrite(output, report.durability_unconfirmed.as_ref());
    let code = if report.durability_unconfirmed.is_none() {
        exit::OK
    } else {
        exit::DEGRADED
    };
    if env.json {
        let mut v = serde_json::to_value(&report).map_err(|e| {
            MochiError::new(ErrorCode::IoError, format!("encoding the report: {e}"))
        })?;
        if let Value::Object(m) = &mut v {
            m.insert("command".into(), json!("rekey"));
            m.insert("action".into(), json!("reencrypt"));
            m.insert("source".into(), json!(a.archive.display().to_string()));
            m.insert("output".into(), json!(output.display().to_string()));
            m.insert("revocation".into(), json!(false));
            m.insert("exit_code".into(), json!(code));
        }
        env.emit_tagged(SCHEMA_REWRITE, v);
    } else {
        env.line(format!(
            "wrote {} ({} bytes, archive {}): {} snapshot{} re-encrypted under a new data key \
             and the new passphrase{}",
            output.display(),
            report.new_len,
            report.new_archive_id,
            report.commits.len(),
            if report.commits.len() == 1 { "" } else { "s" },
            if a.new_passphrase_file.len() > 1 {
                "s"
            } else {
                ""
            },
        ));
        if let Some(why) = &report.durability_unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
        }
        env.line(format!(
            "the source {} is unchanged and kept, and still opens with its old passphrases. \
             Re-encryption does not recall copies already made",
            a.archive.display()
        ));
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A catalog's TEXT cell is archive-controlled bytes: escaped for a
    /// terminal, exact (as hex) in JSON when it is not UTF-8, and typed
    /// apart from a BLOB of the same bytes.
    #[test]
    fn dump_values_are_safe_for_terminals_and_exact_in_json() {
        let hostile = b"a\x1b[31m\nb\tc\xffd".to_vec();
        let shown = dump_value_text(&DumpValue::Text(hostile.clone()));
        assert_eq!(shown, "a\\u{1b}[31m\\u{a}b\\u{9}c\\xffd");

        assert_eq!(
            dump_value_json(&DumpValue::Text(hostile.clone())),
            json!({ "text_hex": hex(&hostile) })
        );
        assert_eq!(
            dump_value_json(&DumpValue::Text(b"plain".to_vec())),
            json!("plain")
        );
        assert_eq!(
            dump_value_json(&DumpValue::Blob(b"plain".to_vec())),
            json!({ "blob_hex": hex(b"plain") })
        );
        assert_eq!(dump_value_json(&DumpValue::Null), Value::Null);
        assert_eq!(dump_value_json(&DumpValue::Integer(-7)), json!(-7));
        assert_eq!(
            dump_value_text(&DumpValue::Blob(vec![0xab, 0x01])),
            "x'ab01'"
        );
        assert_eq!(dump_value_text(&DumpValue::Null), "NULL");
        assert_eq!(dump_value_text(&DumpValue::Integer(42)), "42");
    }

    fn restore_report(exceptions: Vec<mochi_core::restore::RestoreException>) -> RestoreReport {
        RestoreReport {
            files: 1,
            directories: 0,
            bytes: 1,
            exceptions,
            directory_durability: mochi_core::storage::DirectoryDurability::Confirmed,
            file_data_synced: true,
            attributes_unavailable: None,
            attribute_exceptions: Vec::new(),
            case_behavior: mochi_core::storage::CaseBehavior::Sensitive,
            merged_directories: 0,
            replaced: 0,
        }
    }

    /// What a `restore-test` says about recoverability: `PASS` only when
    /// everything was restored and verified, `FAIL` (with the code) when an
    /// entry failed its integrity check, and nothing when the restore fell
    /// short for a reason that says nothing about the archive.
    #[test]
    fn restore_evidence_follows_what_the_restore_proved() {
        use mochi_core::restore::RestoreException;
        let path = |s: &str| ArchivePath::from_stored(s.as_bytes()).unwrap();
        let id = "ab".repeat(32);

        let ok = restore_evidence(3, id.clone(), &restore_report(Vec::new())).unwrap();
        assert_eq!(ok.command, EvidenceCommand::RestoreTest);
        assert_eq!(ok.dimensions[&Dimension::Recoverability], Status::Pass);
        assert_eq!(
            ok.dimensions.len(),
            1,
            "restoring says nothing about integrity"
        );
        assert_eq!((ok.head.seq, ok.head.commit_id.as_str()), (3, id.as_str()));
        assert!(ok.finding_codes.is_empty());
        ok.validate().unwrap();

        let integrity = restore_report(vec![RestoreException {
            path: path("a"),
            kind: ExceptionKind::Integrity {
                code: ErrorCode::StoredIntegrityFailed,
                message: "x".into(),
            },
        }]);
        let bad = restore_evidence(3, id.clone(), &integrity).unwrap();
        assert_eq!(bad.dimensions[&Dimension::Recoverability], Status::Fail);
        assert_eq!(bad.finding_codes, [ErrorCode::StoredIntegrityFailed]);

        for kind in [
            ExceptionKind::Collision,
            ExceptionKind::Failed {
                code: ErrorCode::IoError,
                message: "x".into(),
            },
            ExceptionKind::ParentNotRestored {
                cause: ErrorCode::NameCollision,
            },
        ] {
            let r = restore_report(vec![RestoreException {
                path: path("a"),
                kind,
            }]);
            assert!(
                restore_evidence(3, id.clone(), &r).is_none(),
                "an incomplete restore for a reason unrelated to the archive is not evidence"
            );
        }
    }
}
