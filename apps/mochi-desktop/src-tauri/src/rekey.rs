//! Rekey (plan D5; `docs/dev/encrypted-profile.md`): add a passphrase to an
//! Encrypted archive, take one out, and write the archive again as a new file
//! under a new data key. Thin over `mochi_core::rekey`: the envelopes, the
//! rewrap commit, and the re-encryption are core's; this module routes the
//! request, keeps the open archive's key session, and converts the result.
//!
//! # Rules (AGENTS.md "Encrypted profile")
//!
//! * A new passphrase crosses IPC once, in the job request, and goes straight
//!   into a zeroizing type. It is in no result, no error, and no `Debug`.
//! * **Removing a passphrase is not revocation.** The result carries that
//!   statement; the envelope stays in the file's history, so anyone who knew
//!   the passphrase and kept a copy of the file can still open that copy.
//!   Only a re-encryption writes a file under a new data key, and it does not
//!   recall copies already made.
//! * Re-encryption writes a **new file** at the destination chosen in the
//!   save dialog, never over the source and never over an existing file.
//! * A rewrap is one ordinary commit: `LOCAL_COMMITTED`, nothing more.

use mochi_core::job::JobContext;
use mochi_core::object::OsIds;
use mochi_core::publish::{ArchiveWriter, PublishDurability, TailRepair, WriterOptions};
use mochi_core::rekey;
use mochi_core::storage::os::OsReadStorage;
use mochi_core::{ErrorCode, MochiError, Result};

use crate::browse::{head_encrypted, Archives};
use crate::ipc::{
    ArchiveHandle, EnvelopeList, EnvelopeView, IpcError, JobRequest, RekeyAction, RekeyReport,
};
use crate::write::{location, JobDone, Staging};

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn kdf_text(k: &mochi_format::kdf::KdfParams) -> String {
    format!(
        "argon2id m={} MiB t={} p={}",
        k.memory_kib / 1024,
        k.iterations,
        k.lanes
    )
}

/// The head's key envelopes of an open Encrypted archive. Needs no key to
/// read; `opened_with` marks the one this session's passphrase opened.
pub fn envelopes(archives: &Archives, handle: ArchiveHandle) -> Result<EnvelopeList> {
    let path = archives.path(handle)?;
    let opts = archives.read_options(handle)?;
    let mine = archives.unlocked_envelope(handle)?;
    let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
    let list = rekey::list(&src, &opts)?;
    Ok(EnvelopeList {
        envelopes: list
            .into_iter()
            .map(|e| EnvelopeView {
                opened_with: mine.is_some_and(|m| hex(&m) == e.envelope_id),
                written_by_commit: e.created_at_seq,
                kdf: kdf_text(&e.kdf),
                envelope_id: e.envelope_id,
            })
            .collect(),
    })
}

/// Refuse a rekey request that cannot work, before a job starts.
pub fn validate(archives: &Archives, staging: &Staging, request: &JobRequest) -> Result<()> {
    let handle = match request {
        JobRequest::AddPassphrase { handle, passphrase } => {
            passphrase.passphrase()?;
            *handle
        }
        JobRequest::RemovePassphrase {
            handle,
            envelope_id,
        } => {
            rekey::parse_envelope_id(envelope_id)?;
            *handle
        }
        JobRequest::Reencrypt { handle, passphrase } => {
            passphrase.passphrase()?;
            if staging.destination().is_none() {
                return Err(invalid("choose where to save the new archive first"));
            }
            *handle
        }
        _ => return Err(invalid("not a rekey request")),
    };
    let path = archives.path(handle)?;
    if !head_encrypted(&path) {
        return Err(invalid(
            "this archive is not Encrypted: it has no key envelopes to change",
        ));
    }
    if archives.read_options(handle)?.keys.is_none() {
        return Err(MochiError::new(
            ErrorCode::KeyUnavailable,
            "the archive is not unlocked: open it with its passphrase first",
        ));
    }
    Ok(())
}

/// Run a rekey job.
pub fn run(
    archives: &Archives,
    staging: &Staging,
    request: &JobRequest,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    validate(archives, staging, request)?;
    match request {
        JobRequest::AddPassphrase { handle, passphrase } => rewrap(
            archives,
            *handle,
            vec![passphrase.passphrase()?],
            Vec::new(),
            ctx,
        ),
        JobRequest::RemovePassphrase {
            handle,
            envelope_id,
        } => rewrap(
            archives,
            *handle,
            Vec::new(),
            vec![rekey::parse_envelope_id(envelope_id)?],
            ctx,
        ),
        JobRequest::Reencrypt { handle, passphrase } => {
            reencrypt(archives, staging, *handle, passphrase.passphrase()?, ctx)
        }
        _ => Err(invalid("not a rekey request")),
    }
}

fn rewrap(
    archives: &Archives,
    handle: ArchiveHandle,
    add: Vec<mochi_format::secret::Passphrase>,
    remove: Vec<[u8; 16]>,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    let action = if add.is_empty() {
        RekeyAction::Removed
    } else {
        RekeyAction::Added
    };
    let path = archives.path(handle)?;
    let (mut dir, name) = location(&path)?;
    let (mut w, _) = ArchiveWriter::open_append_in(
        &mut dir,
        &name,
        Box::new(OsIds),
        WriterOptions {
            read: archives.read_options(handle)?,
            record_time: true,
            ..WriterOptions::default()
        },
        TailRepair::Refuse,
    )?;
    let outcome = rekey::rewrap(&mut w, add, remove, ctx)?;
    // The commit is published; a failure to close does not undo it.
    let _ = w.close();
    // The head now lists a different set. A passphrase that was removed no
    // longer opens the head, even though this session holds the data key
    // (the tool's policy, not a cryptographic barrier), so the reopen can
    // fail with KEY_UNAVAILABLE; that is reported, not hidden.
    let (archive, reopen_error) = match archives.reopen(handle) {
        Ok(a) => (Some(a), None),
        Err(e) => (None, Some(IpcError::from(e))),
    };
    let report = RekeyReport {
        action,
        seq: Some(outcome.seq),
        status: Some(outcome.status.as_str().to_owned()),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        snapshots: 0,
        new_archive_id: None,
        durability_note: match &outcome.durability {
            PublishDurability::Durable => None,
            PublishDurability::DirectoryUnconfirmed(why) => Some(why.clone()),
        },
        archive,
        reopen_error,
    };
    Ok(JobDone {
        summary: format!(
            "committed commit {} ({}): key envelopes {}",
            outcome.seq,
            outcome.status.as_str(),
            if action == RekeyAction::Added {
                "added"
            } else {
                "removed"
            }
        ),
        commit: None,
        extract: None,
        check: None,
        search: None,
        rekey: Some(Box::new(report)),
        export: None,
        scan: None,
        retrieve: None,
        foreign_test: None,
    })
}

fn reencrypt(
    archives: &Archives,
    staging: &Staging,
    handle: ArchiveHandle,
    passphrase: mochi_format::secret::Passphrase,
    ctx: &JobContext<'_>,
) -> Result<JobDone> {
    let path = archives.path(handle)?;
    let destination = staging
        .destination()
        .ok_or_else(|| invalid("choose where to save the new archive first"))?;
    if std::fs::symlink_metadata(&destination).is_ok() {
        return Err(MochiError::new(
            ErrorCode::DestinationExists,
            "that file exists; a re-encrypted archive is a new file and never replaces one. \
             Nothing was written",
        ));
    }
    let read = archives.read_options(handle)?;
    let (mut src_dir, src_name) = location(&path)?;
    let (mut out_dir, out_name) = location(&destination)?;
    let (w, _) = ArchiveWriter::open_append_in(
        &mut src_dir,
        &src_name,
        Box::new(OsIds),
        WriterOptions {
            read: read.clone(),
            record_time: true,
            ..WriterOptions::default()
        },
        TailRepair::Refuse,
    )?;
    let report = rekey::reencrypt(
        &w,
        &mut out_dir,
        &out_name,
        Box::new(OsIds),
        read,
        vec![passphrase],
        None,
        ctx,
    );
    let _ = w.close();
    let report = report?;
    staging.consume_destination(&destination);
    let out = RekeyReport {
        action: RekeyAction::Reencrypted,
        seq: None,
        status: None,
        file_name: out_name,
        snapshots: report.commits.len() as u64,
        new_archive_id: Some(report.new_archive_id.to_string()),
        durability_note: report.durability_unconfirmed.clone(),
        archive: None,
        reopen_error: None,
    };
    Ok(JobDone {
        summary: format!(
            "wrote {} snapshot{} to a new file under a new data key",
            out.snapshots,
            if out.snapshots == 1 { "" } else { "s" }
        ),
        commit: None,
        extract: None,
        check: None,
        search: None,
        rekey: Some(Box::new(out)),
        export: None,
        scan: None,
        retrieve: None,
        foreign_test: None,
    })
}
