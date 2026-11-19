//! Essential file discovery (spec §19.1, §19.3; plan C13, required part).
//!
//! [`search`] finds entries of retained snapshots by path, by file version,
//! by file-content hash, and by snapshot, from the catalog of an opened
//! head. It reads metadata only, never file content, and writes nothing.
//!
//! # Where the answers come from
//!
//! The catalog of the opened head is authoritative metadata: its image is
//! hash-verified before it is opened, and its namespace operations replay
//! every snapshot it holds (§10.6). If the image's stored bytes are damaged,
//! [`crate::publish::open_head`] rebuilds the same commit's catalog from its
//! snapshot manifest ([`crate::publish::CatalogSource::SnapshotManifest`]),
//! so discovery is rebuildable from recovery manifests (§19.1) without a
//! separate index. Such a catalog holds its own replay segment only, not
//! the history before it: earlier requested snapshots are opened at their
//! own footers, and any that still cannot be read are reported as
//! *unavailable*, with coverage partial, never as "no match".
//!
//! # Coverage (§19.3)
//!
//! Every [`SearchResult`] names the requested snapshots, the commit the
//! catalog was opened at (the watermark), the snapshots actually searched,
//! and whether coverage is complete. Metadata discovery examines every
//! entry of every searched snapshot, so its pending, failed, unsupported,
//! and excluded document counts are zero by construction. Full-text search
//! (§19.2, optional) is not built: [`FullText::NotBuilt`] says so, and a
//! content query is not accepted at all rather than answered from names.
//!
//! # Decisions [delegated] (recorded in the plan, C13 status)
//!
//! * Names match as **bytes** of the stored path (O24). The only search
//!   normalization is optional ASCII case folding, applied to the query and
//!   to a copy of the path for comparison; results carry the exact stored
//!   bytes (§10.4: search normalization never alters identity). No Unicode
//!   normalization or case folding.
//! * Criteria combine with AND. An empty query matches every entry (a
//!   listing across snapshots).
//! * Hits are per snapshot: an entry present unchanged in five snapshots is
//!   five hits, each naming its commit, so "which snapshots hold this
//!   version" is answered directly.
//! * "File identity" (§19.1) has no representation of its own in the Core
//!   catalog (renames are delete plus put, §10.2): discovery by identity is
//!   by path across snapshots until plan O29 is decided.

use std::collections::BTreeMap;

use mochi_format::digest::FileContentHash;

use crate::catalog::namespace::Snapshot;
use crate::catalog::namespace::{EntryKind, FileVersionId};
use crate::catalog::path::{ArchivePath, SEPARATOR};
use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError, Result};
use crate::gc::resolve_retention;
use crate::job::JobContext;
use crate::publish::{commit_history, open_at_footer, CatalogSource, OpenedHead, ReadOptions};
use crate::storage::ReadStorage;

/// Progress phase of [`search`]: `completed` is snapshots visited.
pub const SEARCH_PHASE: &str = "search";

/// Which snapshots to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotScope {
    /// The opened head only.
    Head,
    /// One commit, by sequence (at most the head's).
    Commit(u64),
    /// Every snapshot retained at the head (the head, every commit not
    /// expired, every held commit; Annex B D18). Needs the retention state,
    /// rebuilt from manifests (D10.10); if it cannot be, the search fails
    /// with `RETENTION_UNRESOLVED` rather than guessing a scope.
    Retained,
    /// Every commit up to the head, expired or not.
    All,
}

/// How a path criterion matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathMatch {
    /// Exactly this path.
    Exact(ArchivePath),
    /// This path and everything under it.
    Under(ArchivePath),
}

/// What to find. Every criterion that is set must hold (AND).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Bytes that must occur in the stored path. Empty matches every path.
    pub name: Vec<u8>,
    /// Fold ASCII letters when comparing `name` (search normalization
    /// only; identity is untouched).
    pub ascii_case_insensitive: bool,
    pub path: Option<PathMatch>,
    pub version: Option<FileVersionId>,
    pub content_hash: Option<FileContentHash>,
    pub kind: Option<EntryKind>,
}

/// One entry of one snapshot that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The commit whose snapshot holds the entry.
    pub seq: u64,
    pub path: ArchivePath,
    pub kind: EntryKind,
    pub version: FileVersionId,
    /// 0 for a directory.
    pub logical_len: u64,
    /// `None` exactly for directories.
    pub content_hash: Option<FileContentHash>,
}

/// Full-text search state (§19.2). Only one value exists in this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullText {
    /// No full-text index exists; content was not searched.
    NotBuilt,
}

/// A requested snapshot that could not be searched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub seq: u64,
    pub error: MochiError,
}

/// What was searched and how completely (§19.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    /// The commit the catalog was opened at: the watermark of what the
    /// metadata covers.
    pub indexed_seq: u64,
    /// Where that catalog came from.
    pub catalog_source: CatalogSource,
    /// The snapshots the scope asked for, ascending.
    pub requested: Vec<u64>,
    /// The requested snapshots that were searched, ascending.
    pub searched: Vec<u64>,
    /// Searched snapshots the head's catalog does not hold (it was rebuilt
    /// from a snapshot manifest, which holds its own segment only), each
    /// opened at its own footer instead, ascending.
    pub opened_separately: Vec<u64>,
    /// Requested snapshots that could not be searched, with why, ascending.
    pub unavailable: Vec<Unavailable>,
    /// Entries examined across the searched snapshots.
    pub entries_examined: u64,
    /// Document counts of §19.3. Metadata discovery examines every entry,
    /// so these are zero; they exist so the report always states them.
    pub pending: u64,
    pub failed: u64,
    pub unsupported: u64,
    pub excluded: u64,
    pub full_text: FullText,
}

impl Coverage {
    /// `true` only if every requested snapshot was searched (so none is
    /// unavailable) and no document is pending, failed, unsupported, or
    /// excluded.
    pub fn complete(&self) -> bool {
        self.searched == self.requested
            && self.pending == 0
            && self.failed == 0
            && self.unsupported == 0
            && self.excluded == 0
    }
}

/// Hits in snapshot order, then path byte order, and their coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub scope: SnapshotScope,
    pub hits: Vec<Hit>,
    pub coverage: Coverage,
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, msg)
}

fn missing(what: &str) -> MochiError {
    MochiError::new(ErrorCode::CatalogInvalid, what.to_string())
}

/// `true` if `path` is `dir` or lies under it, component-wise.
fn within(path: &[u8], dir: &[u8]) -> bool {
    path == dir || (path.len() > dir.len() && path.starts_with(dir) && path[dir.len()] == SEPARATOR)
}

fn contains(hay: &[u8], needle: &[u8], fold: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| {
        if fold {
            w.eq_ignore_ascii_case(needle)
        } else {
            w == needle
        }
    })
}

impl Query {
    fn path_matches(&self, path: &ArchivePath) -> bool {
        let p = path.as_stored();
        if !contains(p, &self.name, self.ascii_case_insensitive) {
            return false;
        }
        match &self.path {
            None => true,
            Some(PathMatch::Exact(x)) => p == x.as_stored(),
            Some(PathMatch::Under(d)) => within(p, d.as_stored()),
        }
    }
}

/// The requested snapshots, ascending.
fn requested(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    scope: SnapshotScope,
    opts: &ReadOptions,
) -> Result<Vec<u64>> {
    let head_seq = head.seq();
    Ok(match scope {
        SnapshotScope::Head => vec![head_seq],
        SnapshotScope::Commit(s) if s <= head_seq => vec![s],
        SnapshotScope::Commit(s) => {
            return Err(invalid(format!(
                "no commit {s}: the head is commit {head_seq}"
            )))
        }
        SnapshotScope::Retained => resolve_retention(src, head, opts)?
            .roots(head_seq)
            .into_iter()
            .collect(),
        SnapshotScope::All => (0..=head_seq).collect(),
    })
}

type VersionCache = BTreeMap<FileVersionId, (u64, Option<FileContentHash>)>;

/// Examine every entry of one snapshot against `query`, appending matches.
fn examine(
    cat: &Catalog,
    seq: u64,
    snapshot: &Snapshot,
    query: &Query,
    versions: &mut VersionCache,
    hits: &mut Vec<Hit>,
) -> Result<u64> {
    let mut examined = 0u64;
    for (path, entry) in snapshot.iter() {
        examined = examined.saturating_add(1);
        if query.kind.is_some_and(|k| k != entry.kind)
            || query.version.is_some_and(|v| v != entry.version)
            || !query.path_matches(path)
        {
            continue;
        }
        let (logical_len, content_hash) = match versions.get(&entry.version) {
            Some(v) => *v,
            None => {
                let (v, _) = cat
                    .file_version(&entry.version)?
                    .ok_or_else(|| missing("a snapshot names a missing file version"))?;
                versions.insert(entry.version, (v.logical_len, v.content_hash));
                (v.logical_len, v.content_hash)
            }
        };
        if query.content_hash.is_some_and(|h| content_hash != Some(h)) {
            continue;
        }
        hits.push(Hit {
            seq,
            path: path.clone(),
            kind: entry.kind,
            version: entry.version,
            logical_len,
            content_hash,
        });
    }
    Ok(examined)
}

/// Find the entries of the requested snapshots that match `query` (see the
/// module note). Read-only. A long operation: it reports [`SEARCH_PHASE`]
/// progress per snapshot searched and honours cancellation between
/// snapshots.
///
/// The head's catalog answers every snapshot it holds, in one replay. A
/// requested snapshot it does not hold (its catalog was rebuilt from a
/// snapshot manifest) is opened at its own footer, with every check of
/// [`open_at_footer`]; if that fails too, it is reported in
/// [`Coverage::unavailable`] with the reason, and coverage is partial.
///
/// Errors: `INVALID_ARGUMENT` for a commit after the head;
/// `RETENTION_UNRESOLVED` if the retained scope cannot be rebuilt;
/// `CATALOG_INVALID` / `NAMESPACE_INVALID` for an inconsistent head
/// catalog; `CANCELLED`.
pub fn search(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    scope: SnapshotScope,
    query: &Query,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<SearchResult> {
    let wanted = requested(src, head, scope, opts)?;
    let cat = &head.catalog;
    let total = Some(wanted.len() as u64);
    let mut hits = Vec::new();
    let mut searched = Vec::new();
    let mut examined = 0u64;
    let mut versions = VersionCache::new();
    ctx.report(SEARCH_PHASE, 0, total);
    // One replay visits every snapshot the catalog holds; only the wanted
    // ones are examined.
    cat.replay_each(|seq, snapshot| {
        if wanted.binary_search(&seq).is_err() {
            return Ok(());
        }
        ctx.check_cancelled()?;
        let n = examine(cat, seq, snapshot, query, &mut versions, &mut hits)?;
        examined = examined.saturating_add(n);
        searched.push(seq);
        ctx.report(SEARCH_PHASE, searched.len() as u64, total);
        Ok(())
    })?;

    let mut opened_separately = Vec::new();
    let mut unavailable = Vec::new();
    let rest: Vec<u64> = wanted
        .iter()
        .copied()
        .filter(|s| searched.binary_search(s).is_err())
        .collect();
    if !rest.is_empty() {
        let history = commit_history(src, opts);
        for seq in rest {
            ctx.check_cancelled()?;
            let opened = history
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|h| {
                    h.iter().find(|e| e.commit.seq == seq).ok_or_else(|| {
                        MochiError::new(
                            ErrorCode::RecordInvalid,
                            format!("commit {seq} is missing from the history"),
                        )
                    })
                })
                .and_then(|e| open_at_footer(src, e.footer_offset, opts))
                .and_then(|o| {
                    let snapshot = o.catalog.replay(None)?;
                    // Version IDs are immutable, but this is another
                    // catalog: look its versions up in it.
                    let mut own = VersionCache::new();
                    examine(&o.catalog, seq, &snapshot, query, &mut own, &mut hits)
                });
            match opened {
                Ok(n) => {
                    examined = examined.saturating_add(n);
                    searched.push(seq);
                    opened_separately.push(seq);
                    ctx.report(SEARCH_PHASE, searched.len() as u64, total);
                }
                Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                Err(error) => unavailable.push(Unavailable { seq, error }),
            }
        }
        searched.sort_unstable();
        hits.sort_by(|a, b| (a.seq, &a.path).cmp(&(b.seq, &b.path)));
    }
    Ok(SearchResult {
        scope,
        hits,
        coverage: Coverage {
            indexed_seq: head.seq(),
            catalog_source: head.catalog_source.clone(),
            requested: wanted,
            searched,
            opened_separately,
            unavailable,
            entries_examined: examined,
            pending: 0,
            failed: 0,
            unsupported: 0,
            excluded: 0,
            full_text: FullText::NotBuilt,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_matches_bytes_and_folds_ascii_only() {
        assert!(contains(b"docs/Invoice.pdf", b"Invoice", false));
        assert!(!contains(b"docs/Invoice.pdf", b"invoice", false));
        assert!(contains(b"docs/Invoice.pdf", b"invoice", true));
        assert!(contains(b"anything", b"", false));
        assert!(!contains(b"ab", b"abc", true));
        // Non-ASCII bytes are compared exactly, folded or not.
        assert!(!contains("É.txt".as_bytes(), "é".as_bytes(), true));
        assert!(contains(b"bad\xff/x", b"\xff", true));
    }

    #[test]
    fn under_is_component_wise() {
        assert!(within(b"docs", b"docs"));
        assert!(within(b"docs/a", b"docs"));
        assert!(!within(b"docsx", b"docs"));
        assert!(!within(b"other/docs", b"docs"));
    }

    fn cov(requested: Vec<u64>, searched: Vec<u64>, unavailable: Vec<u64>) -> Coverage {
        Coverage {
            indexed_seq: 0,
            catalog_source: CatalogSource::Image,
            requested,
            searched,
            opened_separately: Vec::new(),
            unavailable: unavailable
                .into_iter()
                .map(|seq| Unavailable {
                    seq,
                    error: MochiError::new(ErrorCode::StoredIntegrityFailed, "test"),
                })
                .collect(),
            entries_examined: 0,
            pending: 0,
            failed: 0,
            unsupported: 0,
            excluded: 0,
            full_text: FullText::NotBuilt,
        }
    }

    #[test]
    fn coverage_is_complete_only_when_everything_requested_was_searched() {
        assert!(cov(vec![0, 1], vec![0, 1], vec![]).complete());
        assert!(!cov(vec![0, 1], vec![1], vec![0]).complete());
        let mut c = cov(vec![0], vec![0], vec![]);
        c.failed = 1;
        assert!(!c.complete());
    }
}
