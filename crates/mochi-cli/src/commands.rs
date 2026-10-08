//! The built commands (plan C14). Each is a thin adapter: parse, call
//! `mochi-core`, render a typed result as text or JSON, return the exit
//! code (spec §23.2). Rules the CLI adds on top of the core are listed in
//! `docs/c14-cli.md`.

use std::io::Write;
use std::path::Path;

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::descriptor::Profile;
use mochi_core::exit;
use mochi_core::import::{import_into, ImportOptions, SourceKind, DEFAULT_MAX_TOTAL_BYTES};
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::object::{ArchiveId, OsIds};
use mochi_core::publish::{
    commit_history, locate_head, open_at_footer, open_head, read_commit, ArchiveWriter,
    CommitOutcome, OpenedHead, PublishDurability, ReadOptions, TailRepair, TailTruncation,
    Transaction, TruncationWaivers, WriterOptions,
};
use mochi_core::read::{list, read_file};
use mochi_core::report::{Finding, Report, Severity};
use mochi_core::restore::{restore_selected, ExceptionKind, RestoreOptions, RestoreReport};
use mochi_core::status::{Dimension, VerificationLevel};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsRestoreDir, OsSourceTree};
use mochi_core::timestamp::Timestamp;
use mochi_core::verify::{parse_commit_id, verify, FreshnessAnchor, VerifyOptions};
use mochi_core::{ErrorCode, MochiError, Result};
use serde_json::{json, Value};

use crate::cli::{
    AppendArgs, CreateArgs, GetArgs, Level, ListArgs, RestoreTestArgs, SnapshotListArgs, VerifyArgs,
};
use crate::render::{archive_path_arg, path_fields, text};
use crate::state::{HeadStore, SeenHead};

/// What every command gets from the global options.
pub struct Env<'a> {
    pub json: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub read: ReadOptions,
    /// `None` with `--no-local-history`, or when no location is known.
    pub store: Option<HeadStore>,
}

impl Env<'_> {
    fn warn(&mut self, msg: impl std::fmt::Display) {
        let _ = writeln!(self.err, "mochi: warning: {msg}");
    }

    fn emit_json(&mut self, v: &Value) {
        let _ = writeln!(self.out, "{v}");
    }

    fn line(&mut self, s: impl std::fmt::Display) {
        let _ = writeln!(self.out, "{s}");
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
            _ => return Err(invalid(format!("--limit: unknown limit {name:?}"))),
        };
        *slot = value;
    }
    Ok(o)
}

/// The directory holding `archive` and its file name.
fn location(archive: &Path) -> Result<(OsDir, String)> {
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
    Ok((OsDir::containing(archive)?, name))
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
}

fn gather(
    inputs: &[std::path::PathBuf],
    tx: Transaction,
    max_memory: Option<u64>,
    deleted: Vec<ArchivePath>,
    ctx: &JobContext<'_>,
) -> Result<Imported> {
    let tree = OsSourceTree;
    let mut named = Vec::with_capacity(inputs.len());
    for i in inputs {
        named.push(tree.input(i)?);
    }
    let import = import_into(
        &tree,
        &named,
        tx,
        &ImportOptions {
            max_total_bytes: max_memory.unwrap_or(DEFAULT_MAX_TOTAL_BYTES),
        },
        ctx,
    )?;
    Ok(Imported { import, deleted })
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
    let code = if i.complete() && unconfirmed.is_none() {
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
        env.emit_json(&v);
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
        if let Some(why) = &unconfirmed {
            env.line(format!(
                "degraded: the new file's directory entry is not confirmed durable ({why})"
            ));
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
            ErrorCode::NotImplemented,
            "`--exceed-default-limits` is deferred in this build (spec Annex B, D16 is open); \
             archives are created with the default limits only, and nothing was created",
        ));
    }
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let (mut dir, name) = location(&a.archive)?;
    let imported = gather(
        &a.inputs,
        Transaction::new(),
        a.max_memory,
        Vec::new(),
        &ctx,
    )?;
    let opts = WriterOptions {
        read: env.read,
        chunk_size: a.chunk_size,
        zstd_level: a.compression_level,
        record_time: true,
        profile: a.tar_compatible.then_some(Profile {
            tar_compatible: true,
            encrypted: false,
        }),
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
    env.remember(archive_id, outcome.seq, outcome.commit_id);
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
    let (mut dir, name) = location(&a.archive)?;
    let tail = if a.truncate_tail {
        TailRepair::Truncate(TruncationWaivers {
            no_quarantine: a.no_quarantine,
            accept_unconfirmed_durability: a.accept_unconfirmed_durability,
        })
    } else {
        TailRepair::Refuse
    };
    let opts = WriterOptions {
        read: env.read,
        record_time: true,
        ..WriterOptions::default()
    };
    let (mut w, truncation) =
        ArchiveWriter::open_append_in(&mut dir, &name, Box::new(OsIds), opts, tail)?;

    // Deletions first, each with everything under it (children before
    // their parent), then the inputs.
    let snapshot = w.catalog().replay(None)?;
    let mut tx = Transaction::new();
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
    let imported = gather(&a.inputs, tx, a.max_memory, deleted, &ctx)?;
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
                env.emit_json(&v);
            } else {
                env.line("tail truncated; nothing else to commit:");
                print_findings(env, &t.findings());
            }
            let _ = w.close();
            return Ok(exit::OK);
        }
        return Err(invalid(
            "nothing to append: give files or directories to add, or --delete",
        ));
    }
    let outcome = w.commit(imported.import.transaction.clone(), &ctx)?;
    if let Err(e) = w.close() {
        env.warn(format!("closing the archive after the commit: {e}"));
    }
    env.remember(archive_id, outcome.seq, outcome.commit_id);
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
        env.emit_json(&v);
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

pub fn snapshot_list(env: &mut Env<'_>, a: &SnapshotListArgs) -> Result<u8> {
    let src = open_archive(&a.archive)?;
    let history = commit_history(&src, &env.read)?;
    let time = |e: &mochi_core::publish::HistoryEntry| {
        e.commit
            .time
            .and_then(|t| Timestamp::from_unix(t.secs, t.nanos).ok())
            .map(|t| t.to_string())
    };
    if env.json {
        let items: Vec<Value> = history
            .iter()
            .map(|e| {
                json!({
                    "seq": e.commit.seq,
                    "commit_id": e.commit_id.to_hex(),
                    "time": time(e),
                    "checkpoint": e.commit.metadata.is_checkpoint(),
                    "footer_offset": e.footer_offset,
                })
            })
            .collect();
        let archive_id = history.last().map(|e| e.commit.archive_id.to_hex());
        env.emit_json(&json!({"archive_id": archive_id, "commits": items}));
    } else {
        for e in &history {
            env.line(format!(
                "{:>6}  {}  {}  {}",
                e.commit.seq,
                e.commit_id.to_hex(),
                time(e).unwrap_or_else(|| "-".repeat(30)),
                if e.commit.metadata.is_checkpoint() {
                    "checkpoint"
                } else {
                    "delta"
                }
            ));
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
        env.emit_json(&v);
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
    };
    let r = restore_selected(&src, &head, &paths, root, &options, &env.read, &ctx)?;
    Ok(emit_restore(env, "get", &head, &dest, &r))
}

pub fn restore_test(env: &mut Env<'_>, a: &RestoreTestArgs) -> Result<u8> {
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
    Ok(emit_restore(env, "restore-test", &head, &a.destination, &r))
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
    env.line(format!(
        "{command}: level {}",
        serde_json::to_value(r.level)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    ));
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
        env.line(format!("skipped: {} ({})", s.item, s.reason));
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
        read: env.read,
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
