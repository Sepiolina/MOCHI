//! Which sources may be removed after archiving them (spec Annex B.2.23
//! item 3, "Delete files after archiving").
//!
//! Removing a source destroys the only other copy, so this decides from
//! evidence, never from the commit having succeeded. After the commit is
//! published, [`check_removable`] opens the archive's new head and, for each
//! file the import recorded ([`crate::import::import_recording`]):
//!
//! 1. the head holds a file at the path it was imported as;
//! 2. that version **reads back**: every chunk is loaded, verified, and the
//!    reassembled stream hashes to the version's recorded file-content hash
//!    ([`crate::read::read_version`]);
//! 3. the **source still hashes to that same value** and has that length:
//!    a file changed while (or after) it was archived is kept, because what
//!    would be removed is not what the archive holds.
//!
//! Only a file that passes all three is [`Verdict::Remove`]. A directory is
//! `Remove` only if every file and directory the import found under it is;
//! the caller still removes it only once it is empty, so something added to
//! it meanwhile is never taken along. Everything kept carries its reason.
//!
//! This module decides; it removes nothing. The caller moves what may go to
//! the Recycle Bin or the Trash, never deleting outright (B.2.23 item 3), and
//! reports what it kept. It reads the archive through [`ReadStorage`] and
//! the sources through [`FileSource`], so it does no I/O of its own.

use std::sync::Arc;

use mochi_format::digest::FileContentHasher;
use mochi_format::repr::DecodedSlice;

use crate::catalog::namespace::EntryKind;
use crate::catalog::path::ArchivePath;
use crate::error::{ErrorCode, MochiError, Result};
use crate::import::{ImportedEntry, SourceKind};
use crate::job::JobContext;
use crate::publish::{open_head, FileSource, ReadOptions};
use crate::read::read_version;
use crate::storage::ReadStorage;

/// Progress phase of [`check_removable`]: `completed` of `total` entries.
pub const PHASE: &str = "check-sources";

/// Whether one source may be removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Its archived copy read back and matches it.
    Remove,
    /// Kept, and why.
    Keep(String),
}

/// One imported source and its verdict, in import order (parents first).
#[derive(Debug, Clone)]
pub struct Checked<N> {
    pub path: ArchivePath,
    pub node: N,
    pub kind: SourceKind,
    pub verdict: Verdict,
}

/// Decide, for each entry an import recorded, whether its source may be
/// removed now that the commit is published (see the module rules). An
/// error opening the archive's head is returned (nothing may be removed);
/// a problem with one entry only keeps that entry.
pub fn check_removable<N: Clone>(
    src: &dyn ReadStorage,
    read: &ReadOptions,
    entries: &[ImportedEntry<N>],
    ctx: &JobContext<'_>,
) -> Result<Vec<Checked<N>>> {
    let head = open_head(src, read)?;
    let snapshot = head.catalog.replay(None)?;
    let total = entries.len() as u64;
    let mut out: Vec<Checked<N>> = Vec::with_capacity(entries.len());
    for (i, e) in entries.iter().enumerate() {
        ctx.check_cancelled()?;
        let verdict = match e.kind {
            SourceKind::File => match e.source.as_ref() {
                Some(source) => match snapshot.get(&e.path) {
                    Some(entry) if entry.kind == EntryKind::File => {
                        file_verdict(src, &head.catalog, &entry.version, source, read, ctx)?
                    }
                    Some(_) => Verdict::Keep("the archive holds a folder at this name".into()),
                    None => Verdict::Keep("the archive does not hold this file".into()),
                },
                None => Verdict::Keep("its content was not recorded by the import".into()),
            },
            SourceKind::Directory => match snapshot.get(&e.path) {
                // Decided below, once everything under it is.
                Some(entry) if entry.kind == EntryKind::Directory => Verdict::Remove,
                _ => Verdict::Keep("the archive does not hold this folder".into()),
            },
            SourceKind::Symlink | SourceKind::Other => {
                Verdict::Keep("only files and folders are archived".into())
            }
        };
        out.push(Checked {
            path: e.path.clone(),
            node: e.node.clone(),
            kind: e.kind,
            verdict,
        });
        ctx.report(PHASE, i as u64 + 1, Some(total));
    }
    // A folder goes only if everything the import found under it may go.
    let kept: Vec<ArchivePath> = out
        .iter()
        .filter(|c| c.verdict != Verdict::Remove)
        .map(|c| c.path.clone())
        .collect();
    for c in out.iter_mut().filter(|c| c.kind == SourceKind::Directory) {
        if c.verdict == Verdict::Remove && kept.iter().any(|k| k.is_descendant_of(&c.path)) {
            c.verdict = Verdict::Keep("something in it is kept".into());
        }
    }
    Ok(out)
}

/// Rules 2 and 3 for one file. Errors that say the archive or the source
/// is wrong keep the file; cancellation and I/O on the archive stop the
/// whole check.
fn file_verdict(
    src: &dyn ReadStorage,
    catalog: &crate::catalog::Catalog,
    version: &crate::catalog::namespace::FileVersionId,
    source: &Arc<dyn FileSource>,
    read: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Verdict> {
    let Some((recorded, _)) = catalog.file_version(version)? else {
        return Ok(Verdict::Keep(
            "the archive does not describe this file's version".into(),
        ));
    };
    let Some(want) = recorded.content_hash else {
        return Ok(Verdict::Keep(
            "the archived version has no content hash".into(),
        ));
    };
    // 2. The archived copy reads back and verifies.
    let back = match read_version(src, catalog, version, &mut std::io::sink(), read, ctx) {
        Ok(r) => r,
        Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
        Err(e) => {
            return Ok(Verdict::Keep(format!(
                "its archived copy did not read back ({}: {})",
                e.code, e.message
            )))
        }
    };
    if back.content_hash != want || back.logical_len != recorded.logical_len {
        return Ok(Verdict::Keep(
            "its archived copy does not match what was recorded".into(),
        ));
    }
    // 3. The source still is what the archive holds.
    match hash_source(source.as_ref(), ctx) {
        Ok((len, hash)) if len == recorded.logical_len && hash == want => Ok(Verdict::Remove),
        Ok(_) => Ok(Verdict::Keep(
            "it changed after it was read for the archive".into(),
        )),
        Err(e) if e.code == ErrorCode::Cancelled => Err(e),
        Err(e) => Ok(Verdict::Keep(format!(
            "it could not be read again to compare ({})",
            e.message
        ))),
    }
}

fn hash_source(
    source: &dyn FileSource,
    ctx: &JobContext<'_>,
) -> Result<(u64, mochi_format::digest::FileContentHash)> {
    let mut reader = source.open()?;
    let mut hasher = FileContentHasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        ctx.check_cancelled()?;
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let part = buf.get(..n).ok_or_else(|| {
            MochiError::new(ErrorCode::IoError, "a source read more than it was asked")
        })?;
        hasher
            .update(DecodedSlice::from_logical(part))
            .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "the source is too long"))?;
    }
    Ok((hasher.len(), hasher.finalize()))
}
