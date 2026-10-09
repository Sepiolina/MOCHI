//! The TAR-stream check of a TAR-compatible archive (spec Annex B.2.9 D19
//! rule 8; plan C10).
//!
//! For every commit, in order: the commit's data frames (the chunks its delta
//! manifest introduces, in physical order) are the commit's stream. The check
//! decodes the framing and re-emitted content, parses the stream with the
//! bounded [`crate::tar::Parser`], and requires that
//!
//! * a commit with no put has no data frame, and a commit with puts has one
//!   complete stream (two end blocks, nothing after them);
//! * the members are exactly the puts, in order: [`member_for`] of the put's
//!   path and its version's kind, length, and promised attributes. The writer
//!   builds its headers with the same function, so the two cannot disagree
//!   about what a put looks like;
//! * a fresh put's content is the version's own extent chunks, whole and in
//!   order, directly where the member's content belongs (those bytes are not
//!   decoded here: the data-object checks hash them);
//! * a put of a version held earlier (a rename's target, a re-put) re-emits
//!   its content, whose file-content hash, at `content_integrity` and
//!   deeper, is the version's (its length is the header's, which the member
//!   comparison pins to the version's).
//!
//! The first problem of a commit is the one reported for it. A mismatch is
//! `PROFILE_VIOLATION` (a violation, exit 1); a frame that cannot be read or
//! decoded keeps the code of that failure.

use std::collections::{BTreeMap, VecDeque};

use mochi_format::digest::{FileContentHash, FileContentHasher};
use mochi_format::repr::DecodedSlice;

use crate::catalog::extent::ExtentSource;
use crate::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use crate::catalog::path::ArchivePath;
use crate::catalog::Catalog;
use crate::error::{ErrorCode, MochiError};
use crate::job::JobContext;
use crate::manifest::{Attributes, Manifest, ManifestKind};
use crate::object::{decode_verified, load_stored, ObjectId};
use crate::publish::{read_bound_manifest, HistoryEntry, ReadOptions};
use crate::storage::ReadStorage;
use crate::tar::{member_for, Event, Member, Parser, DEFAULT_MAX_MEMBERS};

use super::{phase, Run};

/// What the stream needs to know about a version introduced earlier or now.
struct Known {
    kind: EntryKind,
    len: u64,
    hash: Option<FileContentHash>,
    attributes: Attributes,
}

fn violation(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::ProfileViolation, msg)
}

/// The member being read.
struct Current {
    path: Vec<u8>,
    /// Re-emitted content is hashed from `content_integrity` up.
    hasher: Option<(FileContentHasher, Option<FileContentHash>)>,
    /// A fresh put: the version's own chunks, in order, still to come.
    own: VecDeque<(ObjectId, u64)>,
    fresh: bool,
}

struct Check<'a> {
    puts: Vec<(&'a ArchivePath, FileVersionId)>,
    known: &'a BTreeMap<FileVersionId, Known>,
    fresh: BTreeMap<FileVersionId, &'a crate::manifest::FileVersionEntry>,
    hash_content: bool,
    seen: usize,
    cur: Option<Current>,
    err: Option<MochiError>,
}

impl Check<'_> {
    fn fail(&mut self, msg: impl Into<String>) {
        if self.err.is_none() {
            self.err = Some(violation(msg));
        }
    }

    fn on_member(&mut self, m: &Member) {
        self.end_member();
        let idx = self.seen;
        self.seen += 1;
        let Some((path, version)) = self.puts.get(idx).cloned() else {
            self.fail(format!(
                "the stream has more members than the commit has puts ({})",
                self.puts.len()
            ));
            return;
        };
        let Some(info) = self.known.get(&version) else {
            self.fail(format!(
                "put {idx} names version {version:?}, which no delta manifest introduces"
            ));
            return;
        };
        let want = member_for(path.as_stored(), info.kind, info.len, &info.attributes);
        if *m != want {
            self.fail(format!(
                "member {idx} ({}) is not the commit's put of that path: the header differs \
                 in type, size, mode, owner, or time from what the version promises",
                String::from_utf8_lossy(path.as_stored())
            ));
            return;
        }
        let entry = self.fresh.get(&version).copied();
        let mut own = VecDeque::new();
        if let Some(e) = entry {
            for x in &e.extents {
                match x.source {
                    ExtentSource::Chunk { chunk, .. } => own.push_back((chunk, x.length)),
                    _ => {
                        self.fail(format!(
                            "member {idx} is a version with a hole, which the profile cannot \
                             stream"
                        ));
                        return;
                    }
                }
            }
        }
        let hashed = self.hash_content && entry.is_none() && info.kind == EntryKind::File;
        self.cur = Some(Current {
            path: m.path.clone(),
            hasher: hashed.then(|| (FileContentHasher::new(), info.hash)),
            own,
            fresh: entry.is_some(),
        });
    }

    fn on_content(&mut self, bytes: &[u8]) {
        let Some(cur) = self.cur.as_mut() else {
            self.fail("content outside a member");
            return;
        };
        if cur.fresh {
            let name = String::from_utf8_lossy(&cur.path).into_owned();
            self.fail(format!(
                "the content of {name} is not the version's own chunks"
            ));
            return;
        }
        if let Some((h, _)) = cur.hasher.as_mut() {
            let _ = h.update(DecodedSlice::from_logical(bytes));
        }
    }

    /// The member ended (the next one began, or the stream did).
    fn end_member(&mut self) {
        let Some(cur) = self.cur.take() else { return };
        let name = String::from_utf8_lossy(&cur.path).into_owned();
        if cur.fresh {
            if !cur.own.is_empty() {
                self.fail(format!("{name}: the version's own chunks are missing"));
            }
            return;
        }
        if let Some((h, Some(want))) = cur.hasher {
            if h.finalize() != want {
                self.fail(format!(
                    "{name}: re-emitted content does not hash to the version's file-content hash"
                ));
            }
        }
    }
}

/// Check every commit's stream. `false` if cancelled.
pub(super) fn check(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    cat: &Catalog,
    history: &[HistoryEntry],
    depth: u8,
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    run.tar_streams = true;
    let total = history.len() as u64;
    ctx.report(phase::STREAMS, 0, Some(total));
    let mut known: BTreeMap<FileVersionId, Known> = BTreeMap::new();
    for (i, e) in history.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            return false;
        }
        let delta = match read_bound_manifest(
            src,
            &e.commit,
            &e.commit.delta_manifest,
            e.commit_offset,
            ManifestKind::Delta,
            ro,
        ) {
            Ok(d) => d,
            Err(err) => {
                run.error(
                    &format!("TAR stream of commit {i}: its delta manifest"),
                    &err,
                );
                run.skip(
                    "TAR streams",
                    format!("commit {i}'s delta manifest could not be read"),
                );
                return true;
            }
        };
        for v in &delta.file_versions {
            known.insert(
                v.version.id,
                Known {
                    kind: v.version.kind,
                    len: v.version.logical_len,
                    hash: v.version.content_hash,
                    attributes: v.attributes,
                },
            );
        }
        if let Err(err) = commit_stream(src, ro, cat, &delta, &known, depth, ctx) {
            if err.code == ErrorCode::Cancelled {
                return false;
            }
            run.error(&format!("TAR stream of commit {}", delta.commit_seq), &err);
        }
        ctx.report(phase::STREAMS, i as u64 + 1, Some(total));
    }
    true
}

fn commit_stream(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    cat: &Catalog,
    delta: &Manifest,
    known: &BTreeMap<FileVersionId, Known>,
    depth: u8,
    ctx: &JobContext<'_>,
) -> Result<(), MochiError> {
    let puts: Vec<_> = delta
        .ops
        .iter()
        .filter_map(|op| match op {
            NamespaceOp::Put { path, version } => Some((path, *version)),
            NamespaceOp::Delete { .. } => None,
        })
        .collect();

    // The commit's data frames in physical order.
    let mut frames = Vec::with_capacity(delta.chunks.len());
    for c in &delta.chunks {
        let at = match c.location {
            Some(at) => at,
            None => cat
                .object_location(&c.record.id)?
                .ok_or_else(|| violation("a chunk of the commit has no recorded location"))?,
        };
        frames.push((at, &c.record));
    }
    frames.sort_by_key(|(at, _)| *at);

    if puts.is_empty() {
        return if frames.is_empty() {
            Ok(())
        } else {
            Err(violation(
                "the commit has data frames but no put, so it should have no stream",
            ))
        };
    }

    let mut st = Check {
        puts,
        known,
        fresh: delta
            .file_versions
            .iter()
            .map(|v| (v.version.id, v))
            .collect(),
        hash_content: depth >= 4,
        seen: 0,
        cur: None,
        err: None,
    };
    let mut parser = Parser::new(DEFAULT_MAX_MEMBERS);
    for (at, record) in frames {
        ctx.check_cancelled()?;
        // A fresh member's content is its own chunk frame, taken on trust
        // here (the data-object checks hash it): account for it, don't decode.
        if parser.content_left().is_some_and(|n| n > 0) {
            if let Some(cur) = st.cur.as_mut().filter(|c| c.fresh) {
                match cur.own.pop_front() {
                    Some((id, len)) if id == record.id && len == record.decoded_len => {
                        parser.skip_content(len)?;
                        continue;
                    }
                    _ => {
                        return Err(violation(format!(
                            "{}: the data frame at offset {at} is not the next of the \
                             version's own chunks",
                            String::from_utf8_lossy(&cur.path)
                        )))
                    }
                }
            }
        }
        let stored = load_stored(src, at, record, &ro.limits)?;
        let decoded = decode_verified(record, &stored, &ro.limits)?;
        parser.feed(decoded.as_bytes(), &mut |ev| match ev {
            Event::Member(m) => st.on_member(m),
            Event::Content(b) => st.on_content(b),
        })?;
        if let Some(e) = st.err.take() {
            return Err(e);
        }
    }
    st.end_member();
    if let Some(e) = st.err.take() {
        return Err(e);
    }
    parser.finish()?;
    if st.seen != st.puts.len() {
        return Err(violation(format!(
            "the stream has {} members, the commit has {} puts",
            st.seen,
            st.puts.len()
        )));
    }
    Ok(())
}
