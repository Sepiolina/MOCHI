//! The built commands (plan C14). Each is a thin adapter: parse, call
//! `mochi-core`, render a typed result as text or JSON, return the exit
//! code (spec §23.2). Rules the CLI adds on top of the core are listed in
//! `docs/c14-cli.md`.

use std::io::Write;
use std::path::Path;

use mochi_core::catalog::namespace::EntryKind;
use mochi_core::catalog::namespace::FileVersionId;
use mochi_core::catalog::path::ArchivePath;
use mochi_core::compact::{self, CompactOptions, Keep};
use mochi_core::descriptor::Profile;
use mochi_core::exit;
use mochi_core::gc::{self, GcPlan, PlanHead};
use mochi_core::import::{import_into, ImportOptions, SourceKind, DEFAULT_MAX_TOTAL_BYTES};
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
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
use mochi_core::search::{search, FullText, PathMatch, Query, SnapshotScope};
use mochi_core::status::{Dimension, VerificationLevel};
use mochi_core::storage::os::{OsDir, OsReadStorage, OsRestoreDir, OsSourceTree, OsStorage};
use mochi_core::timestamp::Timestamp;
use mochi_core::verify::{parse_commit_id, verify, FreshnessAnchor, VerifyOptions};
use mochi_core::{ErrorCode, MochiError, Result};
use mochi_format::digest::FileContentHash;
use serde_json::{json, Value};

use crate::cli::{
    AppendArgs, CheckpointArgs, CompactArgs, CreateArgs, ExpireArgs, GcApplyArgs, GcPlanArgs,
    GetArgs, KindArg, Level, ListArgs, ReleaseArgs, RepairApplyArgs, RepairPlanArgs,
    RestoreTestArgs, RetainArgs, SearchArgs, SearchSnapshots, SnapshotListArgs, VerifyArgs,
};
use crate::render::{archive_path_arg, hex, path_fields, text};
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
        env.emit_json(&json!({
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
        }));
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

pub fn snapshot_list(env: &mut Env<'_>, a: &SnapshotListArgs) -> Result<u8> {
    let src = open_archive(&a.archive)?;
    let history = commit_history(&src, &env.read)?;
    // Retention at the head, rebuilt from its segment's manifests (D10.10).
    // If it cannot be, the commits are still listed, with retention unknown.
    let retention =
        match open_head(&src, &env.read).and_then(|h| segment_state(&src, &h, &env.read)) {
            Ok(s) => Some(s.retention),
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
        env.emit_json(&json!({
            "archive_id": archive_id,
            "retention_known": retention.is_some(),
            "commits": items,
        }));
    } else {
        for e in &history {
            let seq = e.commit.seq;
            let mut notes = Vec::new();
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

// ---- retention, checkpoint, compaction, collection (plan C9) ---------------------

/// Open `archive` for one new commit under its publication lock.
fn open_writer(archive: &Path, read: ReadOptions) -> Result<ArchiveWriter<OsStorage>> {
    let (mut dir, name) = location(archive)?;
    let opts = WriterOptions {
        read,
        record_time: true,
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
    env.remember(archive_id, outcome.seq, outcome.commit_id);
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
        env.emit_json(&json!({
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
    let w = open_writer(&a.archive, env.read)?;
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
    let w = open_writer(&a.archive, env.read)?;
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
    require_confirm(a.confirm, "releasing a legal hold")?;
    let w = open_writer(&a.archive, env.read)?;
    let mut tx = Transaction::new();
    tx.release(a.label.as_bytes());
    let what = format!("legal hold {} released", text(a.label.as_bytes()));
    let detail = json!({"release": {"label": a.label}});
    commit_maintenance(env, "snapshot release", &a.archive, w, tx, &what, detail)
}

pub fn checkpoint(env: &mut Env<'_>, a: &CheckpointArgs) -> Result<u8> {
    let mut w = open_writer(&a.archive, env.read)?;
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
}

pub fn gc_plan(env: &mut Env<'_>, a: &GcPlanArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let src = open_archive(&a.archive)?;
    let p = gc::plan(&src, &env.read, &ctx)?;
    let value = serde_json::to_value(&p)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("encoding the plan: {e}")))?;
    if let Some(out) = &a.output {
        write_new_file(out, format!("{value:#}\n").as_bytes())?;
    }
    if env.json {
        if let Some(out) = &a.output {
            env.emit_json(&json!({
                "command": "gc plan",
                "plan_file": out.display().to_string(),
                "collects_anything": p.collects_anything(),
                "plan": value,
            }));
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
    let (mut out_dir, out_name) = location(output)?;
    // The source's publication lock is held from here until the new archive
    // is published, so no commit can land in between (D18).
    let w = open_writer(archive, env.read)?;
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
    // anchor (D8).
    match open_archive(output).and_then(|s| open_head(&s, &env.read)) {
        Ok(h) => env.remember(h.commit.archive_id, h.seq(), h.commit_id),
        Err(e) => env.warn(format!("the new archive's head was not recorded: {e}")),
    }
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
        env.emit_json(&v);
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
            "{} chunks ({} stored bytes) copied byte for byte; {} file versions{}",
            report.chunks_copied,
            report.stored_bytes_copied,
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
            "nothing recoverable through §22 steps 1–4; the salvage scan (step 7) is not built"
        }
    }
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
        };
        let detail = if s.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", s.detail)
        };
        env.line(format!("step {} {}: {status}{detail}", s.step, s.name));
    }
    let seqs: Vec<u64> = p.snapshots.iter().map(|s| s.seq).collect();
    env.line(format!(
        "recoverable snapshots ({}): {}",
        seqs.len(),
        seq_list(&seqs)
    ));
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

pub fn repair_plan(env: &mut Env<'_>, a: &RepairPlanArgs) -> Result<u8> {
    let (progress, cancel) = job();
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let src = open_archive(&a.archive)?;
    let p = repair::plan(&src, &env.read, &ctx)?;
    let value = serde_json::to_value(&p)
        .map_err(|e| MochiError::new(ErrorCode::IoError, format!("encoding the plan: {e}")))?;
    if let Some(out) = &a.output {
        write_new_file(out, format!("{value:#}\n").as_bytes())?;
    }
    if env.json {
        if let Some(out) = &a.output {
            env.emit_json(&json!({
                "command": "repair plan",
                "plan_file": out.display().to_string(),
                "outcome": p.outcome,
                "exit_code": p.exit_code,
                "plan": value,
            }));
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
    let (mut out_dir, out_name) = location(&a.output)?;
    // The source is only read: repair never writes to it (§22.2).
    let src = open_archive(&a.archive)?;
    let options = RepairOptions {
        accept_retention_loss: a.accept_retention_loss,
        ..RepairOptions::default()
    };
    let report = repair::apply(
        &src,
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
        Ok(h) => env.remember(h.commit.archive_id, h.seq(), h.commit_id),
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
        env.emit_json(&v);
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
