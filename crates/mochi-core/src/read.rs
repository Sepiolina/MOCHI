//! Read path (plan C6; spec §9.3, §10.3, §10.6, §20.1): list the namespace
//! of an opened commit and stream one file out of it.
//!
//! The commit is whatever [`OpenedHead`] was opened: the head
//! ([`crate::publish::open_head`]) or an earlier commit
//! ([`crate::publish::open_at_footer`] with an offset from
//! [`crate::publish::commit_history`]). Snapshot selection is choosing which
//! one to open.
//!
//! # What a successful read proves
//!
//! [`read_file`] rebuilds the logical stream from the file's extents. Every
//! chunk is hash-verified as stored bytes before it is decoded, and as
//! decoded bytes after ([`decode_verified`]). The file-content hash is
//! computed over the whole logical stream, holes included, and compared
//! with the catalog's. Only `Ok` means all of that matched.
//!
//! **Bytes reach `out` before the final comparison**, because files are
//! streamed, not buffered. A caller that writes them somewhere durable must
//! treat that output as unverified until `read_file` returns `Ok`, and
//! discard it otherwise. The restore engine (C6) writes to a temporary file
//! and publishes only on `Ok`. `mochi get` to standard output reports the
//! failure and exits 1.

use std::io::Write;

use mochi_format::digest::{FileContentHash, FileContentHasher};

use crate::catalog::extent::{validate_extents, ExtentSource};
use crate::catalog::namespace::EntryKind;
use crate::catalog::path::{ArchivePath, SEPARATOR};
use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::object::{decode_verified, load_stored, ObjectId};
use crate::publish::{OpenedHead, ReadOptions};
use crate::storage::ReadStorage;

/// Progress phase of [`read_file`]: `completed` is logical bytes written.
pub const READ_PHASE: &str = "read";

/// Zeros written for holes, a block at a time.
static ZERO_BLOCK: [u8; 64 * 1024] = [0u8; 64 * 1024];

/// One entry of a listed snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub path: ArchivePath,
    pub kind: EntryKind,
    /// Logical length in bytes; 0 for a directory.
    pub logical_len: u64,
    /// Plain BLAKE3 of the logical stream (O20). `None` exactly for
    /// directories.
    pub content_hash: Option<FileContentHash>,
}

/// What [`read_file`] read and verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRead {
    pub logical_len: u64,
    /// The verified file-content hash.
    pub content_hash: FileContentHash,
    /// Chunks loaded and verified. A chunk referenced by consecutive extents
    /// is loaded once.
    pub chunks_loaded: u64,
    /// Bytes that came from holes (sparse regions, §9.3).
    pub hole_bytes: u64,
}

fn missing(what: &str) -> MochiError {
    MochiError::new(ErrorCode::CatalogInvalid, what.to_string())
}

/// `true` if `path` is `dir` or lies under it.
fn within(path: &ArchivePath, dir: &ArchivePath) -> bool {
    let (p, d) = (path.as_stored(), dir.as_stored());
    p == d || (p.len() > d.len() && p.starts_with(d) && p[d.len()] == SEPARATOR)
}

/// The entries of the opened commit's namespace in path byte order: all of
/// them, or only `under` and its descendants. Listing reads the catalog
/// only, no file content.
pub fn list(head: &OpenedHead, under: Option<&ArchivePath>) -> Result<Vec<ListEntry>> {
    let cat = &head.catalog;
    let snapshot = cat.replay(None)?;
    let mut out = Vec::new();
    for (path, entry) in snapshot.iter() {
        if under.is_some_and(|d| !within(path, d)) {
            continue;
        }
        let (version, _) = cat
            .file_version(&entry.version)?
            .ok_or_else(|| missing("snapshot names a missing file version"))?;
        out.push(ListEntry {
            path: path.clone(),
            kind: entry.kind,
            logical_len: version.logical_len,
            content_hash: version.content_hash,
        });
    }
    Ok(out)
}

/// Stream the file at `path` in the opened commit to `out`, verifying every
/// chunk and the whole file (see the module note: output is unverified
/// until `Ok`). A long operation: it reports [`READ_PHASE`] progress after
/// each extent and honours cancellation before each one.
///
/// Errors: `INVALID_ARGUMENT` if `path` is absent from the snapshot or is a
/// directory; `STORED_INTEGRITY_FAILED` / `CONTENT_INTEGRITY_FAILED` for
/// damaged chunks or a file that does not match its hash; `EXTENT_INVALID`
/// or `CATALOG_INVALID` for an inconsistent catalog; `IO_ERROR` if `out`
/// fails; `CANCELLED`.
pub fn read_file(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    path: &ArchivePath,
    out: &mut dyn Write,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<FileRead> {
    read_file_in(src, &head.catalog, path, out, opts, ctx)
}

/// [`read_file`] against a catalog directly: the namespace at its latest
/// commit, with chunks at the locations it records in `src`. For callers
/// that hold a catalog without an [`OpenedHead`] (recovery, tests).
pub fn read_file_in(
    src: &dyn ReadStorage,
    cat: &Catalog,
    path: &ArchivePath,
    out: &mut dyn Write,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<FileRead> {
    let entry = cat.replay(None)?.get(path).copied().ok_or_else(|| {
        MochiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "no entry {:?} in the snapshot",
                String::from_utf8_lossy(path.as_stored())
            ),
        )
    })?;
    if entry.kind == EntryKind::Directory {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "{:?} is a directory",
                String::from_utf8_lossy(path.as_stored())
            ),
        ));
    }
    let (version, extents) = cat
        .file_version(&entry.version)?
        .ok_or_else(|| missing("snapshot names a missing file version"))?;
    let expected = version
        .content_hash
        .ok_or_else(|| missing("a file version has no content hash"))?;
    // The catalog was validated when it was opened; check again here, where
    // the offsets are used, so that nothing below can index out of range.
    validate_extents(version.logical_len, &extents, |id| {
        cat.object(id).ok().flatten().map(|r| r.decoded_len)
    })?;

    let io = |e: std::io::Error| {
        MochiError::new(ErrorCode::IoError, format!("writing the file failed: {e}"))
    };
    let mut hasher = FileContentHasher::new();
    let mut cached: Option<(ObjectId, mochi_format::repr::DecodedBytes)> = None;
    let mut chunks_loaded = 0u64;
    let mut hole_bytes = 0u64;
    let overflow = || MochiError::new(ErrorCode::ExtentInvalid, "file length overflows");
    ctx.report(READ_PHASE, 0, Some(version.logical_len));
    for e in &extents {
        ctx.check_cancelled()?;
        match e.source {
            ExtentSource::Hole => {
                hasher.hole(e.length).ok_or_else(overflow)?;
                let mut left = e.length;
                while left > 0 {
                    let n = left.min(ZERO_BLOCK.len() as u64);
                    // n ≤ ZERO_BLOCK.len(), so the index cannot fail.
                    out.write_all(ZERO_BLOCK.get(..n as usize).unwrap_or(&ZERO_BLOCK))
                        .map_err(io)?;
                    left -= n;
                }
                hole_bytes = hole_bytes.checked_add(e.length).ok_or_else(overflow)?;
            }
            ExtentSource::Chunk {
                chunk,
                chunk_offset,
            } => {
                if cached.as_ref().map(|(id, _)| id) != Some(&chunk) {
                    let record = cat
                        .object(&chunk)?
                        .ok_or_else(|| missing("an extent names a missing chunk"))?;
                    let at = cat
                        .object_location(&chunk)?
                        .ok_or_else(|| missing("a chunk has no location"))?;
                    let stored = load_stored(src, at, &record, &opts.limits)?;
                    let decoded = decode_verified(&record, &stored, &opts.limits)?;
                    chunks_loaded += 1;
                    cached = Some((chunk, decoded));
                }
                let decoded = cached
                    .as_ref()
                    .map(|(_, d)| d)
                    .ok_or_else(|| missing("chunk cache empty"))?;
                let past = || {
                    MochiError::new(
                        ErrorCode::ExtentInvalid,
                        format!("extent {} reads past its chunk", e.ordinal),
                    )
                };
                let end = chunk_offset.checked_add(e.length).ok_or_else(overflow)?;
                let slice = decoded.slice(chunk_offset..end).ok_or_else(past)?;
                let raw = usize::try_from(chunk_offset)
                    .ok()
                    .zip(usize::try_from(end).ok())
                    .and_then(|(a, b)| decoded.as_bytes().get(a..b))
                    .ok_or_else(past)?;
                hasher.update(slice).ok_or_else(overflow)?;
                out.write_all(raw).map_err(io)?;
            }
        }
        ctx.report(READ_PHASE, hasher.len(), Some(version.logical_len));
    }
    if hasher.len() != version.logical_len {
        return Err(MochiError::new(
            ErrorCode::ExtentInvalid,
            "the extents do not cover the file's length",
        ));
    }
    let actual = hasher.finalize();
    if actual != expected {
        return Err(MochiError::new(
            ErrorCode::ContentIntegrityFailed,
            format!(
                "{:?}: the reassembled file does not match its file-content hash",
                String::from_utf8_lossy(path.as_stored())
            ),
        ));
    }
    out.flush().map_err(io)?;
    Ok(FileRead {
        logical_len: version.logical_len,
        content_hash: actual,
        chunks_loaded,
        hole_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ArchivePath {
        ArchivePath::from_stored(s.as_bytes()).unwrap()
    }

    #[test]
    fn within_is_component_wise() {
        assert!(within(&p("docs"), &p("docs")));
        assert!(within(&p("docs/a.txt"), &p("docs")));
        assert!(within(&p("docs/sub/b"), &p("docs")));
        assert!(!within(&p("docsx"), &p("docs")));
        assert!(!within(&p("doc"), &p("docs")));
        assert!(!within(&p("other/docs"), &p("docs")));
    }
}
