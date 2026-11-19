//! Ladder step 7: the bounded salvage scan (spec §22 step 7, §22.1, §5.1;
//! Annex B.2.13 D23; plan C8, workstream W12).
//!
//! Salvage runs only when the caller asks for it (`repair plan|apply
//! --salvage`), after parity (step 6), copies (step 5), and steps 1–4 have
//! left something unrecovered ([`crate::repair`]). It **finds commits and
//! decides which of them were published**; it never interprets anything
//! itself. What it produces is a set of bytes laid over the healed view
//! ([`crate::parity::HealedSource`]): the footers of commits whose
//! publication is established, and the archive descriptor when it can be
//! re-encoded to the exact hash every confirmed commit records. The ordinary
//! survey of `repair` then runs over that view and verifies everything it
//! uses against the commit chain, so salvage can only add what the chain's
//! own checks accept.
//!
//! # The scan (§22.1, D23 item 2)
//!
//! Frames are walked forward from offset 0 ([`walk_frame`]). Where the walk
//! fails it **resynchronizes**: each later byte offset is tested as a
//! candidate frame start, and a candidate that validates resumes the walk
//! after it. The walk **never resynchronizes inside a frame that walked
//! validly**, which is what keeps an archive stored *inside* the archive from
//! being read as the archive. Work is bounded by the reader limit
//! `max-salvage-candidates`: every magic number a resynchronization tests,
//! and every commit record or footer the scan evaluates, counts as one
//! candidate. At the limit the scan stops and the step is reported
//! *incomplete*, never a pass. Memory is a fixed-size index entry per
//! candidate kept (about 200 bytes) plus 24 bytes per frame walked; time is
//! linear in the file plus the validation of each candidate. Cancellation is
//! checked once per MiB scanned.
//!
//! # What is kept (D23 item 3)
//!
//! * A **commit frame** whose commit ID recomputes, whose envelope decodes,
//!   and whose parent footer offset (if any) lies before it.
//! * A **footer** whose digest validates over the commit frame it names; its
//!   recorded commit offset, length, and sequence must equal that commit
//!   frame's actual position, length, and sequence.
//! * Other frames are kept by position only. A magic-number match alone is
//!   never a candidate kept.
//!
//! # Identity, contradiction, and confirmation (D23 items 4 and 5)
//!
//! The expected archive ID comes from the source descriptor, else from
//! `--archive-id`, else from the kept commits; several IDs among them is
//! `ARCHIVE_ID_AMBIGUOUS`. Commits of another archive are *foreign* and
//! ignored. Two kept commits with one sequence and different commit IDs are
//! *contradictory*: neither is used, both are listed.
//!
//! A kept commit is **confirmed** when its own footer is kept, or when a kept
//! commit's parent link names it **and the link's position is consistent**:
//! the linked footer offset *H* must lie after the commit frame, at least a
//! footer's length before the linking commit, overlap no frame that walked
//! validly, and, if a valid footer stands at *H*, name this commit. A copy
//! of an earlier state stored inside the archive fails exactly this test:
//! its offsets are those of the original, not of its position, so it is
//! *rejected*, not merely unconfirmed. **An unconfirmed commit is never
//! recovered**: it may be an interrupted publication (§5.1); it is listed.
//!
//! The link rule is this build's reading of D23 item 5 (see the Annex B.2.13
//! clarification): a parent link proves the parent was published, because a
//! writer starts a commit only after its parent's footer is synced (§12.2),
//! whether or not the *child* itself is later shown to have been published.
//! That is what lets every commit below a damaged head be confirmed while
//! the head itself stays unconfirmed.
//!
//! # What is laid over the view
//!
//! * For each confirmed commit that has no valid footer where its linking
//!   child says the footer was, the footer is re-encoded
//!   ([`mochi_format::footer::encode_footer_frame`]) at that offset. A footer
//!   is a pure function of the commit frame and its position, so this
//!   regenerates exactly the bytes the writer synced.
//! * When the descriptor frame does not hash to what the confirmed commits
//!   record, candidate descriptors are re-encoded from the archive ID, the
//!   Encrypted profile (required feature 1 in the commits, which must
//!   agree), and the unknown choices (TAR compatibility, recovery-record
//!   percentage 1–100), and the one whose stored-object hash equals the
//!   recorded hash is laid at offset 0. The hash is the oracle, so nothing is
//!   guessed. Per D12 as amended, the *repaired* archive then gets an
//!   ordinary new descriptor and TAR compatibility is not carried; it is
//!   listed as a lost constraint (D23 item 8).

use std::collections::{BTreeMap, BTreeSet};

use mochi_format::digest::{stored_object_hash, StoredObjectBytes};
use mochi_format::footer::{encode_footer_frame, validate_footer, FOOTER_FRAME_LEN};
use mochi_format::frame::{walk_frame, FrameSpan};
use mochi_format::parity::{MAX_PERCENT, MIN_PERCENT};
use mochi_format::registry::{self, FrameKind};
use mochi_format::FormatError;
use serde::{Deserialize, Serialize};

use crate::commit::{CommitRecord, ObjectRef};
use crate::descriptor::{Descriptor, Profile};
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::object::ArchiveId;
use crate::parity::{read_descriptor_raw, HealedSource};
use crate::publish::ReadOptions;
use crate::storage::{ReadStorage, StorageReader};

/// What the caller asks of step 7.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SalvageOptions {
    /// The archive to recover when the source descriptor cannot say
    /// (`--archive-id`). Ignored nowhere: if the descriptor is readable and
    /// names another archive, the request is refused.
    pub archive_id: Option<ArchiveId>,
}

/// The longest list a [`SalvageReport`] carries; the counts stay exact.
pub const MAX_LISTED: usize = 1024;

/// One commit the scan kept, by where it lies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SalvagedCommit {
    pub seq: u64,
    pub commit_id: String,
    /// Where the commit frame lies in the source.
    pub offset: u64,
    /// `footer` (its own footer was kept), `link` (a kept commit's parent
    /// link names it), or, in the unconfirmed and rejected lists, why not.
    pub by: String,
}

/// Several commits claiming one sequence (D23 item 4): none is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contradiction {
    pub seq: u64,
    pub commits: Vec<SalvagedCommit>,
    pub reason: String,
}

/// A chain of confirmed commits linked by parent ID, opened from its top
/// like a head (D23 item 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub top_seq: u64,
    pub top_commit_id: String,
    pub bottom_seq: u64,
    pub commits: u64,
    /// The footer offset the segment is opened at.
    pub footer_offset: u64,
    /// Whether that footer was regenerated (the source's is damaged).
    pub footer_regenerated: bool,
}

/// How the profile was inferred when the descriptor was not restored by
/// parity or copies (D23 item 8; plan K20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInference {
    /// Whether a descriptor was re-encoded and laid over the view.
    pub descriptor_reconstructed: bool,
    /// Why, or why not.
    pub evidence: String,
    pub encrypted: bool,
    pub redundancy_percent: Option<u16>,
    /// Constraints the repaired archive does not carry. `tar_compatible`
    /// when the source was TAR-compatible: the new archive is not (D12 as
    /// amended).
    pub profile_constraints_lost: Vec<String>,
    /// Where the writer parameters came from: `catalog image` or the build's
    /// defaults (filled in by the survey).
    pub writer_parameters: String,
}

/// Everything step 7 found (D23 item 7). Counts are exact; lists stop at
/// [`MAX_LISTED`] entries (`lists_truncated`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SalvageReport {
    /// The archive recovered, and where the ID came from: `descriptor`,
    /// `option`, or `commits`.
    pub archive_id: Option<String>,
    pub archive_id_source: String,
    pub candidates: u64,
    pub candidate_limit: u64,
    pub limit_reached: bool,
    pub frames_walked: u64,
    /// Walk failures the scan resynchronized after.
    pub resyncs: u64,
    pub kept_commits: u64,
    pub kept_footers: u64,
    pub confirmed: Vec<SalvagedCommit>,
    /// Never recovered: no footer or later commit confirms publication
    /// (§5.1).
    pub unconfirmed: Vec<SalvagedCommit>,
    pub unconfirmed_count: u64,
    /// Kept commits whose recorded offsets disagree with where they lie, or
    /// footers whose recorded commit position disagrees with the commit
    /// (`by` says which).
    pub rejected: Vec<SalvagedCommit>,
    pub rejected_count: u64,
    pub contradictory: Vec<Contradiction>,
    pub contradictory_count: u64,
    pub foreign_commits: u64,
    /// Data objects no recovered commit references: counted, never placed in
    /// a namespace (D23 item 7).
    pub orphan_data_objects: u64,
    pub orphan_bytes: u64,
    pub segments: Vec<Segment>,
    /// Offsets of the footers regenerated.
    pub footers_regenerated: Vec<u64>,
    pub profile_inference: Option<ProfileInference>,
    pub lists_truncated: bool,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---- the scan --------------------------------------------------------------------

/// A commit frame the scan kept. About 200 bytes.
#[derive(Debug, Clone)]
struct Node {
    offset: u64,
    len: u64,
    seq: u64,
    id: [u8; 32],
    archive: [u8; 32],
    parent: Option<Link>,
    features: Vec<u64>,
    descriptor: ObjectRef,
}

#[derive(Debug, Clone, Copy)]
struct Link {
    seq: u64,
    id: [u8; 32],
    footer: u64,
}

/// A footer the scan validated.
#[derive(Debug, Clone, Copy)]
struct Foot {
    commit_offset: u64,
    commit_len: u64,
    seq: u64,
}

struct Span {
    offset: u64,
    len: u64,
    kind: FrameKind,
}

struct Scan {
    nodes: Vec<Node>,
    footers: BTreeMap<u64, Foot>,
    /// Frames that walked validly, ascending and disjoint.
    spans: Vec<Span>,
    /// Commit frames whose own parent hint does not precede them.
    bad_hint: Vec<SalvagedCommit>,
    candidates: u64,
    limit: u64,
    limit_reached: bool,
    resyncs: u64,
}

enum Resync {
    At(u64),
    End,
    Limit,
}

fn io(msg: &str) -> MochiError {
    MochiError::new(ErrorCode::IoError, msg.to_owned())
}

fn is_magic(m: u32) -> bool {
    (registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX).contains(&m)
        || m == registry::ZSTD_DATA_FRAME
}

/// The next offset at or after `from` where four bytes are a skippable or
/// Zstandard magic number. Reads in 1 MiB blocks (overlapping by three bytes)
/// and checks cancellation once per block.
fn next_magic(
    view: &dyn ReadStorage,
    from: u64,
    size: u64,
    ctx: &JobContext<'_>,
) -> Result<Option<u64>> {
    const BLOCK: u64 = 1 << 20;
    let mut at = from;
    let mut buf = Vec::new();
    while at.checked_add(4).is_some_and(|e| e <= size) {
        ctx.check_cancelled()?;
        let n = (size - at).min(BLOCK + 3);
        buf.resize(usize::try_from(n).map_err(|_| io("salvage block"))?, 0);
        view.read_exact_at(at, &mut buf)?;
        for i in 0..buf.len().saturating_sub(3) {
            let m = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            if is_magic(m) {
                return Ok(Some(at + i as u64));
            }
        }
        if n <= 3 {
            break;
        }
        at += n - 3;
        ctx.report(crate::repair::phase::SALVAGE, at, Some(size));
    }
    Ok(None)
}

impl Scan {
    /// Count one candidate; `false` at the limit (the scan stops).
    fn bump(&mut self) -> bool {
        if self.candidates >= self.limit {
            self.limit_reached = true;
            return false;
        }
        self.candidates += 1;
        true
    }

    fn read_frame(view: &dyn ReadStorage, span: &FrameSpan, max: u64) -> Result<Option<Vec<u8>>> {
        if span.len > max {
            return Ok(None);
        }
        let len = usize::try_from(span.len).map_err(|_| io("salvage frame"))?;
        let mut buf = vec![0u8; len];
        view.read_exact_at(span.offset, &mut buf)?;
        Ok(Some(buf))
    }

    /// Decode the commit frame at `span`, if it is one that D23 item 3 keeps.
    fn commit_at(
        view: &dyn ReadStorage,
        span: &FrameSpan,
        opts: &ReadOptions,
    ) -> Result<Option<Node>> {
        let Some(buf) = Self::read_frame(view, span, opts.limits.max_commit_frame_len)? else {
            return Ok(None);
        };
        let (rec, id): (CommitRecord, _) =
            match CommitRecord::from_stored(&buf, &opts.limits, &opts.cbor) {
                Ok(x) => x,
                Err(e) if e.code == ErrorCode::IoError => return Err(e),
                Err(_) => return Ok(None),
            };
        Ok(Some(Node {
            offset: span.offset,
            len: span.len,
            seq: rec.seq,
            id: *id.as_bytes(),
            archive: *rec.archive_id.as_bytes(),
            parent: rec.parent.map(|p| Link {
                seq: p.seq,
                id: *p.commit_id.as_bytes(),
                footer: p.footer_offset,
            }),
            features: rec.required_features.clone(),
            descriptor: rec.descriptor,
        }))
    }

    fn footer_at(r: &StorageReader<'_>, at: u64, opts: &ReadOptions) -> Result<Option<Foot>> {
        match validate_footer(r, at, &opts.limits) {
            Ok(f) if f.commit_kind == FrameKind::CommitRecord => Ok(Some(Foot {
                commit_offset: f.fields.commit_offset,
                commit_len: f.fields.commit_len,
                seq: f.fields.commit_sequence,
            })),
            Ok(_) => Ok(None),
            Err(FormatError::Source(m)) => Err(MochiError::new(ErrorCode::IoError, m)),
            Err(_) => Ok(None),
        }
    }

    /// Would a resynchronization resume the walk at this validly walked
    /// frame? Commit records and footers must validate; data frames walked
    /// structurally; any other control frame must be followed by another
    /// magic number or the end of the file.
    fn acceptable(
        &self,
        r: &StorageReader<'_>,
        view: &dyn ReadStorage,
        span: &FrameSpan,
        size: u64,
        opts: &ReadOptions,
    ) -> Result<bool> {
        Ok(match span.kind {
            FrameKind::CommitRecord => Self::commit_at(view, span, opts)?.is_some(),
            FrameKind::CommitFooter => {
                span.len == FOOTER_FRAME_LEN && Self::footer_at(r, span.offset, opts)?.is_some()
            }
            FrameKind::ZstdData => true,
            _ => {
                let Some(end) = span.offset.checked_add(span.len) else {
                    return Ok(false);
                };
                if end == size {
                    true
                } else if end.checked_add(4).is_some_and(|e| e <= size) {
                    let mut b = [0u8; 4];
                    view.read_exact_at(end, &mut b)?;
                    is_magic(u32::from_le_bytes(b))
                } else {
                    false
                }
            }
        })
    }

    fn resync(
        &mut self,
        r: &StorageReader<'_>,
        view: &dyn ReadStorage,
        from: u64,
        size: u64,
        opts: &ReadOptions,
        ctx: &JobContext<'_>,
    ) -> Result<Resync> {
        let mut at = from;
        loop {
            let Some(o) = next_magic(view, at, size, ctx)? else {
                return Ok(Resync::End);
            };
            if !self.bump() {
                return Ok(Resync::Limit);
            }
            match walk_frame(r, o, &opts.limits) {
                Err(FormatError::Source(m)) => return Err(MochiError::new(ErrorCode::IoError, m)),
                Err(_) => {}
                Ok(span) => {
                    if self.acceptable(r, view, &span, size, opts)? {
                        return Ok(Resync::At(o));
                    }
                }
            }
            at = o + 1;
        }
    }

    /// Keep what a validly walked frame is. `count`: charge the candidate
    /// (not when a resynchronization already did). `false`: the limit was
    /// reached.
    fn visit(
        &mut self,
        r: &StorageReader<'_>,
        view: &dyn ReadStorage,
        span: &FrameSpan,
        count: bool,
        opts: &ReadOptions,
    ) -> Result<bool> {
        match span.kind {
            FrameKind::CommitRecord => {
                if count && !self.bump() {
                    return Ok(false);
                }
                if let Some(n) = Self::commit_at(view, span, opts)? {
                    match n.parent {
                        Some(p) if p.footer >= n.offset => self.bad_hint.push(SalvagedCommit {
                            seq: n.seq,
                            commit_id: hex(&n.id),
                            offset: n.offset,
                            by: "its parent footer offset does not precede it".into(),
                        }),
                        _ => self.nodes.push(n),
                    }
                }
            }
            FrameKind::CommitFooter if span.len == FOOTER_FRAME_LEN => {
                if count && !self.bump() {
                    return Ok(false);
                }
                if let Some(f) = Self::footer_at(r, span.offset, opts)? {
                    self.footers.insert(span.offset, f);
                }
            }
            _ => {}
        }
        Ok(true)
    }
}

fn scan(view: &dyn ReadStorage, opts: &ReadOptions, ctx: &JobContext<'_>) -> Result<(Scan, u64)> {
    let r = StorageReader::new(view)?;
    let size = view.size()?;
    let mut sc = Scan {
        nodes: Vec::new(),
        footers: BTreeMap::new(),
        spans: Vec::new(),
        bad_hint: Vec::new(),
        candidates: 0,
        limit: opts.limits.max_salvage_candidates,
        limit_reached: false,
        resyncs: 0,
    };
    let mut frames = 0u64;
    let mut pos = 0u64;
    let mut counted = false;
    let mut mib = u64::MAX;
    while pos < size {
        if pos >> 20 != mib {
            mib = pos >> 20;
            ctx.check_cancelled()?;
            ctx.report(crate::repair::phase::SALVAGE, pos, Some(size));
        }
        let walked = match walk_frame(&r, pos, &opts.limits) {
            Ok(span) => span
                .offset
                .checked_add(span.len)
                .filter(|e| *e > pos && *e <= size)
                .map(|end| (span, end)),
            Err(FormatError::Source(m)) => return Err(MochiError::new(ErrorCode::IoError, m)),
            Err(_) => None,
        };
        match walked {
            Some((span, end)) => {
                if !sc.visit(&r, view, &span, !counted, opts)? {
                    break;
                }
                counted = false;
                frames += 1;
                sc.spans.push(Span {
                    offset: span.offset,
                    len: span.len,
                    kind: span.kind,
                });
                pos = end;
            }
            None => {
                sc.resyncs += 1;
                let from = pos.checked_add(1).ok_or_else(|| io("salvage offset"))?;
                match sc.resync(&r, view, from, size, opts, ctx)? {
                    Resync::At(o) => {
                        pos = o;
                        counted = true;
                    }
                    Resync::End | Resync::Limit => break,
                }
            }
        }
    }
    Ok((sc, frames))
}

// ---- analysis --------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    Live,
    Foreign,
    Contradictory,
}

/// Whether the 72 bytes at `h` can hold a footer: no frame that walked
/// validly overlaps them, except a footer frame exactly there.
fn region_free(spans: &[Span], h: u64) -> bool {
    let Some(end) = h.checked_add(FOOTER_FRAME_LEN) else {
        return false;
    };
    let idx = spans.partition_point(|s| s.offset < end);
    for s in spans[..idx].iter().rev() {
        let s_end = s.offset.saturating_add(s.len);
        if s_end <= h {
            break;
        }
        let is_footer_here =
            s.kind == FrameKind::CommitFooter && s.offset == h && s.len == FOOTER_FRAME_LEN;
        if !is_footer_here {
            return false;
        }
    }
    true
}

/// What salvage hands to the survey.
pub(crate) struct Prepared {
    pub report: SalvageReport,
    /// Footer offsets (in the view, after regeneration) to open as heads,
    /// highest top sequence first.
    pub roots: Vec<u64>,
    pub footers_regenerated: usize,
    pub descriptor_restored: bool,
    /// Data frames of the source: `(offset, length)`.
    pub data_frames: Vec<(u64, u64)>,
    /// Every confirmed commit, uncapped: `(commit frame offset, commit ID)`.
    /// A chain opened from a root may hold only these (D23 items 5, 11(c)).
    pub confirmed: BTreeSet<(u64, [u8; 32])>,
}

fn push_capped<T>(list: &mut Vec<T>, item: T, truncated: &mut bool) {
    if list.len() < MAX_LISTED {
        list.push(item);
    } else {
        *truncated = true;
    }
}

/// Scan `view`, decide which commits were published, and lay the footers
/// and descriptor it can re-derive over the view. See the module docs.
pub(crate) fn prepare(
    view: &mut HealedSource<'_>,
    sv: &SalvageOptions,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Prepared> {
    let (sc, frames) = scan(&*view, opts, ctx)?;
    let mut trunc = false;

    // Identity (D23 item 4).
    let from_descriptor = read_descriptor_raw(&*view, opts).map(|d| *d.archive_id.as_bytes());
    let asked = sv.archive_id.map(|a| *a.as_bytes());
    let present: BTreeSet<[u8; 32]> = sc.nodes.iter().map(|n| n.archive).collect();
    let (expected, source) = match (from_descriptor, asked) {
        (Some(d), Some(a)) if d != a => {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "--archive-id {} is not the archive the source descriptor names ({}); \
                     nothing was changed",
                    hex(&a),
                    hex(&d)
                ),
            ))
        }
        (Some(d), _) => (Some(d), "descriptor"),
        (None, Some(a)) => (Some(a), "option"),
        (None, None) if present.len() > 1 => {
            let ids: Vec<String> = present.iter().map(|i| hex(i)).collect();
            return Err(MochiError::new(
                ErrorCode::ArchiveIdAmbiguous,
                format!(
                    "the scan found commits of {} archives ({}) and the source descriptor \
                     cannot say which to recover: name one with --archive-id",
                    ids.len(),
                    ids.join(", ")
                ),
            ));
        }
        (None, None) => (present.iter().next().copied(), "commits"),
    };

    let mut standing: Vec<Standing> = vec![Standing::Live; sc.nodes.len()];
    let mut foreign = 0u64;
    for (i, n) in sc.nodes.iter().enumerate() {
        if Some(n.archive) != expected {
            standing[i] = Standing::Foreign;
            foreign += 1;
        }
    }
    // Contradictory identities: one sequence, different commit IDs.
    let mut by_seq: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, n) in sc.nodes.iter().enumerate() {
        if standing[i] == Standing::Live {
            by_seq.entry(n.seq).or_default().push(i);
        }
    }
    let mut contradictory = Vec::new();
    let mut contradictory_count = 0u64;
    for (seq, idxs) in &by_seq {
        let ids: BTreeSet<[u8; 32]> = idxs.iter().map(|i| sc.nodes[*i].id).collect();
        if ids.len() > 1 {
            contradictory_count += 1;
            let mut commits = Vec::new();
            for i in idxs {
                standing[*i] = Standing::Contradictory;
                commits.push(SalvagedCommit {
                    seq: *seq,
                    commit_id: hex(&sc.nodes[*i].id),
                    offset: sc.nodes[*i].offset,
                    by: "contradictory".into(),
                });
            }
            push_capped(
                &mut contradictory,
                Contradiction {
                    seq: *seq,
                    commits,
                    reason: "several commits claim this sequence with different commit IDs; \
                             none is used (§22.1)"
                        .into(),
                },
                &mut trunc,
            );
        }
    }

    // Index the live commits.
    let mut by_key: BTreeMap<(u64, [u8; 32]), Vec<usize>> = BTreeMap::new();
    for (i, n) in sc.nodes.iter().enumerate() {
        if standing[i] == Standing::Live {
            by_key.entry((n.seq, n.id)).or_default().push(i);
        }
    }

    // Own footers: a kept footer whose recorded position, length, and
    // sequence equal the commit frame's (D23 item 3).
    let mut by_offset: BTreeMap<u64, usize> = BTreeMap::new();
    for (i, n) in sc.nodes.iter().enumerate() {
        if standing[i] == Standing::Live {
            by_offset.insert(n.offset, i);
        }
    }
    let mut own_footer: Vec<Option<u64>> = vec![None; sc.nodes.len()];
    let mut rejected: Vec<SalvagedCommit> = Vec::new();
    let mut rejected_count = 0u64;
    for (fo, f) in &sc.footers {
        match by_offset.get(&f.commit_offset) {
            Some(&i) if sc.nodes[i].len == f.commit_len && sc.nodes[i].seq == f.seq => {
                let slot = &mut own_footer[i];
                if slot.is_none_or(|have| *fo < have) {
                    *slot = Some(*fo);
                }
            }
            Some(&i) => {
                rejected_count += 1;
                push_capped(
                    &mut rejected,
                    SalvagedCommit {
                        seq: f.seq,
                        commit_id: hex(&sc.nodes[i].id),
                        offset: *fo,
                        by: "footer: its recorded commit length or sequence disagrees with \
                             the commit frame at that offset"
                            .into(),
                    },
                    &mut trunc,
                );
            }
            None => {
                // Names a commit this scan did not keep (another archive's,
                // a contradictory one, or one beyond the limit): not usable,
                // and not an error.
            }
        }
    }

    // Links (D23 item 5): each kept commit's parent link, resolved by
    // position.
    let n = sc.nodes.len();
    let mut link_to: Vec<Option<(usize, u64)>> = vec![None; n];
    let mut failed: BTreeSet<usize> = BTreeSet::new();
    for (ci, c) in sc.nodes.iter().enumerate() {
        if standing[ci] != Standing::Live {
            continue;
        }
        let Some(p) = c.parent else { continue };
        let Some(cands) = by_key.get(&(p.seq, p.id)) else {
            continue;
        };
        let h = p.footer;
        let mut pass = Vec::new();
        for &qi in cands {
            let q = &sc.nodes[qi];
            let after_commit = q.offset.checked_add(q.len).is_some_and(|e| e <= h);
            let before_child = h
                .checked_add(FOOTER_FRAME_LEN)
                .is_some_and(|e| e <= c.offset);
            let footer_ok = sc.footers.get(&h).is_none_or(|f| {
                f.commit_offset == q.offset && f.commit_len == q.len && f.seq == q.seq
            });
            if p.seq < c.seq
                && qi != ci
                && after_commit
                && before_child
                && region_free(&sc.spans, h)
                && footer_ok
            {
                pass.push(qi);
            } else {
                failed.insert(qi);
            }
        }
        if pass.len() == 1 {
            link_to[ci] = Some((pass[0], h));
        } else if pass.len() > 1 {
            // Two places both fit: ambiguous, so neither is used.
            for qi in pass {
                failed.insert(qi);
            }
        }
    }

    // Confirmation. Hints: the footer offsets the linking commits record.
    let mut hints: Vec<BTreeSet<u64>> = vec![BTreeSet::new(); n];
    for (ci, l) in link_to.iter().enumerate() {
        if let Some((qi, h)) = l {
            if standing[ci] == Standing::Live {
                hints[*qi].insert(*h);
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum How {
        Footer,
        Link,
    }
    let mut confirmed: Vec<Option<How>> = vec![None; n];
    for i in 0..n {
        if standing[i] != Standing::Live {
            continue;
        }
        if own_footer[i].is_some() {
            confirmed[i] = Some(How::Footer);
        } else if hints[i].len() == 1 {
            confirmed[i] = Some(How::Link);
        }
    }

    // Segments: maximal chains of confirmed commits linked by parent ID. A
    // top is a confirmed commit no confirmed commit links to.
    let mut linked_from_confirmed = vec![false; n];
    for (ci, l) in link_to.iter().enumerate() {
        if let Some((qi, _)) = l {
            if confirmed[ci].is_some() && confirmed[*qi].is_some() {
                linked_from_confirmed[*qi] = true;
            }
        }
    }
    let mut tops: Vec<usize> = (0..n)
        .filter(|i| confirmed[*i].is_some() && !linked_from_confirmed[*i])
        .collect();
    tops.sort_by(|a, b| {
        sc.nodes[*b]
            .seq
            .cmp(&sc.nodes[*a].seq)
            .then(sc.nodes[*a].offset.cmp(&sc.nodes[*b].offset))
    });

    // Footers to regenerate: every confirmed commit whose linking child says
    // its footer lies where no valid footer does.
    let mut to_restore: BTreeMap<u64, usize> = BTreeMap::new();
    for i in 0..n {
        if confirmed[i].is_none() {
            continue;
        }
        if let Some(&h) = hints[i].iter().next() {
            if hints[i].len() == 1 && !sc.footers.contains_key(&h) {
                to_restore.entry(h).or_insert(i);
            }
        }
    }

    let mut segments = Vec::new();
    let mut roots = Vec::new();
    for &t in &tops {
        let mut count = 1u64;
        let mut cur = t;
        let mut seen = BTreeSet::from([t]);
        let mut bottom = sc.nodes[t].seq;
        while let Some((qi, _)) = link_to[cur] {
            if confirmed[qi].is_none() || !seen.insert(qi) {
                break;
            }
            count += 1;
            bottom = sc.nodes[qi].seq;
            cur = qi;
        }
        let (fo, regenerated) = match hints[t].iter().next().filter(|_| hints[t].len() == 1) {
            Some(&h) => (h, !sc.footers.contains_key(&h)),
            None => match own_footer[t] {
                Some(f) => (f, false),
                None => continue,
            },
        };
        roots.push(fo);
        push_capped(
            &mut segments,
            Segment {
                top_seq: sc.nodes[t].seq,
                top_commit_id: hex(&sc.nodes[t].id),
                bottom_seq: bottom,
                commits: count,
                footer_offset: fo,
                footer_regenerated: regenerated,
            },
            &mut trunc,
        );
    }
    roots.dedup();

    // Lay the regenerated footers over the view.
    let size = view.size()?;
    let mut regenerated = Vec::new();
    for (h, i) in &to_restore {
        let q = &sc.nodes[*i];
        if h.checked_add(FOOTER_FRAME_LEN).is_none_or(|e| e > size) {
            continue;
        }
        let len = usize::try_from(q.len).map_err(|_| io("salvage commit"))?;
        let mut frame = vec![0u8; len];
        view.read_exact_at(q.offset, &mut frame)?;
        view.restore(*h, encode_footer_frame(q.offset, q.seq, &frame).to_vec());
        regenerated.push(*h);
    }

    // The descriptor (D23 item 8).
    let (profile_inference, descriptor_restored) = {
        let conf: Vec<&Node> = (0..n)
            .filter(|i| confirmed[*i].is_some())
            .map(|i| &sc.nodes[i])
            .collect();
        infer_profile(view, expected, &conf, opts)?
    };

    // The lists.
    let mut confirmed_list = Vec::new();
    let mut unconfirmed = Vec::new();
    let mut unconfirmed_count = 0u64;
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|i| (sc.nodes[*i].seq, sc.nodes[*i].offset));
    for i in order {
        if standing[i] != Standing::Live {
            continue;
        }
        let node = &sc.nodes[i];
        let mk = |by: &str| SalvagedCommit {
            seq: node.seq,
            commit_id: hex(&node.id),
            offset: node.offset,
            by: by.to_owned(),
        };
        match confirmed[i] {
            Some(How::Footer) => push_capped(&mut confirmed_list, mk("footer"), &mut trunc),
            Some(How::Link) => push_capped(&mut confirmed_list, mk("link"), &mut trunc),
            None if failed.contains(&i) => {
                rejected_count += 1;
                push_capped(
                    &mut rejected,
                    mk(
                        "the footer offset a later commit's parent link records for it cannot \
                        be this commit's: it lies inside another frame, or not after this \
                        commit (a copy of an earlier state is not the archive)",
                    ),
                    &mut trunc,
                );
            }
            None => {
                unconfirmed_count += 1;
                push_capped(
                    &mut unconfirmed,
                    mk("no footer or later commit confirms publication (§5.1)"),
                    &mut trunc,
                );
            }
        }
    }
    for b in sc.bad_hint {
        rejected_count += 1;
        push_capped(&mut rejected, b, &mut trunc);
    }

    let data_frames: Vec<(u64, u64)> = sc
        .spans
        .iter()
        .filter(|s| matches!(s.kind, FrameKind::ZstdData | FrameKind::EncryptedObject))
        .map(|s| (s.offset, s.len))
        .collect();

    let report = SalvageReport {
        archive_id: expected.map(|e| hex(&e)),
        archive_id_source: source.to_owned(),
        candidates: sc.candidates,
        candidate_limit: sc.limit,
        limit_reached: sc.limit_reached,
        frames_walked: frames,
        resyncs: sc.resyncs,
        kept_commits: sc.nodes.len() as u64,
        kept_footers: sc.footers.len() as u64,
        confirmed: confirmed_list,
        unconfirmed,
        unconfirmed_count,
        rejected,
        rejected_count,
        contradictory,
        contradictory_count,
        foreign_commits: foreign,
        orphan_data_objects: 0,
        orphan_bytes: 0,
        segments,
        footers_regenerated: regenerated.clone(),
        profile_inference,
        lists_truncated: trunc,
    };
    let confirmed = (0..n)
        .filter(|i| confirmed[*i].is_some())
        .map(|i| (sc.nodes[i].offset, sc.nodes[i].id))
        .collect();
    Ok(Prepared {
        report,
        roots,
        footers_regenerated: regenerated.len(),
        descriptor_restored,
        data_frames,
        confirmed,
    })
}

/// Re-encode the descriptor when it does not hash to what the confirmed
/// commits record (D23 item 8). Returns the inference to report, and whether
/// a descriptor was laid over the view.
fn infer_profile(
    view: &mut HealedSource<'_>,
    expected: Option<[u8; 32]>,
    confirmed: &[&Node],
    opts: &ReadOptions,
) -> Result<(Option<ProfileInference>, bool)> {
    let (Some(archive), Some(first)) = (expected, confirmed.first()) else {
        return Ok((None, false));
    };
    // Every confirmed commit references one descriptor (D10.6).
    let want = first.descriptor;
    if confirmed.iter().any(|c| c.descriptor != want) || want.offset != 0 {
        return Ok((None, false));
    }
    // Already good (or restored by parity or a copy): nothing to infer.
    if want.stored_len <= opts.limits.max_frame_len && want.stored_len <= view.size()? {
        let mut have = vec![0u8; usize::try_from(want.stored_len).map_err(|_| io("descriptor"))?];
        view.read_exact_at(0, &mut have)?;
        if stored_object_hash(StoredObjectBytes::new(&have)) == want.stored_hash {
            return Ok((None, false));
        }
    }
    let encrypted = first
        .features
        .contains(&mochi_format::seal::FEATURE_ENCRYPTED);
    if confirmed
        .iter()
        .any(|c| c.features.contains(&mochi_format::seal::FEATURE_ENCRYPTED) != encrypted)
    {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "the confirmed commits disagree about the Encrypted profile (required feature 1), \
             so no profile can be inferred; nothing was changed",
        ));
    }
    let id = ArchiveId::from_bytes(archive);
    let tar_choices: &[bool] = if encrypted { &[false] } else { &[false, true] };
    for &tar in tar_choices {
        let percents = std::iter::once(None).chain((MIN_PERCENT..=MAX_PERCENT).map(Some));
        for redundancy in percents {
            let d = Descriptor::for_profile(
                id,
                Profile {
                    tar_compatible: tar,
                    encrypted,
                    redundancy,
                },
            );
            let Ok(stored) = d.to_stored() else { continue };
            let bytes = stored.as_bytes();
            if bytes.len() as u64 == want.stored_len
                && stored_object_hash(StoredObjectBytes::new(bytes)) == want.stored_hash
            {
                view.restore(0, bytes.to_vec());
                return Ok((
                    Some(ProfileInference {
                        descriptor_reconstructed: true,
                        evidence: "the descriptor was re-encoded from the archive ID and the \
                                   profile choices; its stored-object hash equals the one \
                                   every confirmed commit records"
                            .into(),
                        encrypted,
                        redundancy_percent: redundancy,
                        profile_constraints_lost: if tar {
                            vec!["tar_compatible".to_owned()]
                        } else {
                            Vec::new()
                        },
                        writer_parameters: String::new(),
                    }),
                    true,
                ));
            }
        }
    }
    Ok((
        Some(ProfileInference {
            descriptor_reconstructed: false,
            evidence: "the descriptor is damaged and no profile choice re-encodes to the hash \
                       the confirmed commits record (a descriptor that declared limits cannot \
                       be inferred), so their catalogs cannot be opened"
                .into(),
            encrypted,
            redundancy_percent: None,
            profile_constraints_lost: Vec::new(),
            writer_parameters: String::new(),
        }),
        false,
    ))
}

/// Data objects no recovered commit references (D23 item 7): their count and
/// total stored length. `referenced` holds the offsets of every object the
/// opened catalogs know.
pub(crate) fn orphans(frames: &[(u64, u64)], referenced: &BTreeSet<u64>) -> (u64, u64) {
    let mut count = 0u64;
    let mut bytes = 0u64;
    for (offset, len) in frames {
        if !referenced.contains(offset) {
            count += 1;
            bytes = bytes.saturating_add(*len);
        }
    }
    (count, bytes)
}
