//! Search (plan D5): `mochi_core::search` (C13) over an open archive, with
//! its coverage (spec §19.3, §23.3 #8) crossing IPC beside the hits.
//!
//! Thin over core: the query, the scopes, the matching, and the coverage are
//! `mochi_core::search`; this module only converts. Names are searched, never
//! file content (full-text search, §19.2, is not built, and the coverage says
//! so). Hit paths and error texts are archive-supplied: they cross as
//! escaped [`NameText`] and the UI renders text only. Read-only: the archive
//! is opened through `OsReadStorage` with the key session of its open
//! archive, if it is Encrypted.

use mochi_core::job::JobContext;
use mochi_core::publish::open_head;
use mochi_core::search::{search, Coverage, Hit, Query, SearchResult, SnapshotScope};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::{ErrorCode, MochiError, Result};

use crate::browse::{hex, kind, Archives};
use crate::ipc::{
    HitView, JobRequest, NameText, SearchCoverageView, SearchResultView, SearchScopeChoice,
    SeqRange, UnavailableView,
};
use crate::write::JobDone;

/// Hits that cross IPC. More are counted (`total_hits`) and the UI says so.
pub const MAX_HITS_SHOWN: usize = 1_000;

/// The longest query accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 1_024;

fn invalid(what: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::InvalidArgument, what)
}

/// Refuse a search that cannot work, before a job starts.
pub fn validate(archives: &Archives, request: &JobRequest) -> Result<()> {
    let JobRequest::SearchArchive { handle, query, .. } = request else {
        return Err(invalid("not a search request"));
    };
    archives.path(*handle)?;
    if query.len() > MAX_QUERY_BYTES {
        return Err(invalid(format!(
            "a search takes at most {MAX_QUERY_BYTES} bytes of text"
        )));
    }
    Ok(())
}

fn scope(s: SearchScopeChoice) -> SnapshotScope {
    match s {
        SearchScopeChoice::Head => SnapshotScope::Head,
        SearchScopeChoice::Retained => SnapshotScope::Retained,
        SearchScopeChoice::All => SnapshotScope::All,
        SearchScopeChoice::Commit { seq } => SnapshotScope::Commit(u64::from(seq)),
    }
}

/// Run a search job.
pub fn run(archives: &Archives, request: &JobRequest, ctx: &JobContext<'_>) -> Result<JobDone> {
    let JobRequest::SearchArchive {
        handle,
        query,
        ignore_case,
        scope: choice,
        entry_kind: wanted_kind,
    } = request
    else {
        return Err(invalid("not a search request"));
    };
    validate(archives, request)?;
    let path = archives.path(*handle)?;
    let opts = archives.read_options(*handle)?;
    let src = OsReadStorage::open(&path).map_err(MochiError::from)?;
    let head = open_head(&src, &opts)?;
    let q = Query {
        name: query.as_bytes().to_vec(),
        ascii_case_insensitive: *ignore_case,
        kind: wanted_kind.map(|k| match k {
            crate::ipc::EntryKindView::File => mochi_core::catalog::namespace::EntryKind::File,
            crate::ipc::EntryKindView::Directory => {
                mochi_core::catalog::namespace::EntryKind::Directory
            }
        }),
        ..Query::default()
    };
    let result = search(&src, &head, scope(*choice), &q, &opts, ctx)?;
    let view = result_view(*choice, &result);
    let summary = format!(
        "searched {} of {} requested snapshot{}: {} match{}, coverage {}",
        view.coverage.searched_count,
        view.coverage.requested_count,
        if view.coverage.requested_count == 1 {
            ""
        } else {
            "s"
        },
        view.total_hits,
        if view.total_hits == 1 { "" } else { "es" },
        if view.coverage.complete {
            "complete"
        } else {
            "PARTIAL"
        },
    );
    Ok(JobDone {
        summary,
        commit: None,
        extract: None,
        check: None,
        search: Some(Box::new(view)),
        rekey: None,
        export: None,
        scan: None,
        retrieve: None,
        foreign_test: None,
    })
}

/// Consecutive runs of an ascending list.
pub fn ranges(seqs: &[u64]) -> Vec<SeqRange> {
    let mut out: Vec<SeqRange> = Vec::new();
    for &s in seqs {
        match out.last_mut() {
            Some(r) if r.to.checked_add(1) == Some(s) => r.to = s,
            _ => out.push(SeqRange { from: s, to: s }),
        }
    }
    out
}

fn hit_view(h: &Hit) -> HitView {
    HitView {
        seq: h.seq,
        path: NameText::of(h.path.as_stored()),
        key: hex(h.path.as_stored()),
        parent: h.path.parent().map(|p| hex(p.as_stored())),
        kind: kind(h.kind),
        size: h.logical_len.to_string(),
    }
}

fn coverage_view(c: &Coverage) -> SearchCoverageView {
    SearchCoverageView {
        complete: c.complete(),
        requested: ranges(&c.requested),
        searched: ranges(&c.searched),
        requested_count: c.requested.len() as u64,
        searched_count: c.searched.len() as u64,
        unavailable: c
            .unavailable
            .iter()
            .map(|u| UnavailableView {
                seq: u.seq,
                code: u.error.code.as_str().to_owned(),
                message: NameText::of(u.error.message.as_bytes()),
            })
            .collect(),
        opened_separately: ranges(&c.opened_separately),
        entries_examined: c.entries_examined,
        indexed_seq: c.indexed_seq,
        catalog_from_manifest: matches!(
            c.catalog_source,
            mochi_core::publish::CatalogSource::SnapshotManifest { .. }
        ),
        content_not_searched: true,
    }
}

/// A result as the UI receives it: at most [`MAX_HITS_SHOWN`] hits.
pub fn result_view(choice: SearchScopeChoice, r: &SearchResult) -> SearchResultView {
    SearchResultView {
        scope: choice,
        hits: r.hits.iter().take(MAX_HITS_SHOWN).map(hit_view).collect(),
        total_hits: r.hits.len() as u64,
        coverage: coverage_view(&r.coverage),
    }
}
