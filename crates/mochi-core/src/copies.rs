//! Independent copies: ladder step 5 of repair (spec §22; Annex B.2.12 D22).
//!
//! A *copy* is a byte copy of the damaged archive that the caller names
//! (`--copy`). Repair consults copies after parity (step 6) and before the
//! survey (steps 1–4), laying whole frames from a copy over the source's
//! view ([`HealedSource::donate`]). **A copy supplies bytes only; the source
//! supplies the trust.** Nothing here vouches for a byte:
//!
//! * a frame is donated only inside the commits the copy *agrees* on with the
//!   source (D22 item 3), only where the copy's bytes verify against a hash
//!   taken from that agreed structure and the source's bytes do not (item 4);
//! * the survey then verifies everything it uses against the commit chain,
//!   exactly as without copies (item 5), so a forged donation costs a rejected
//!   version, never a wrong byte in the result;
//! * the source and the copies are opened read-only through
//!   [`ReadStorage`]; nothing is written to either.
//!
//! The work is two passes. [`analyze`] looks at each copy alone (is it a
//! copy, which of its commits agree, which frames could it donate and with
//! what expected hash) and changes nothing, so agreement is judged against
//! the source as parity left it, whatever the earlier copies donate. Then
//! each copy, in argument order, donates into the view: the first copy that
//! donates a frame wins, because a later copy then finds the source's frame
//! good. An I/O error on a copy disables that copy (its donations are taken
//! back) and is reported; an error on the source stays fatal.
//!
//! # Limits (recorded, not hidden)
//!
//! * A copy's own parity is not applied (D22 item 3).
//! * Agreement follows D22 literally, so a commit in the middle of the source
//!   whose footer or commit frame is damaged is not agreed (its sequence is
//!   not above the source's last self-verifying commit): copies do not
//!   rebuild it. Parity can, and so can a copy for everything the commit
//!   references (objects, manifests, images).
//! * A source with no self-verifying commit at all agrees with nothing.

use std::collections::{BTreeMap, BTreeSet};

use mochi_format::digest::{stored_object_hash, CommitId, StoredObjectHash};
use mochi_format::footer::{validate_footer, FOOTER_FRAME_LEN};
use mochi_format::frame::walk_frame;
use mochi_format::parity;
use mochi_format::registry::FrameKind;
use serde::{Deserialize, Serialize};

use crate::commit::{CommitRecord, ObjectRef};
use crate::descriptor::KNOWN_REQUIRED_FEATURES;
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::parity::HealedSource;
use crate::publish::{
    locate_head, open_at_footer, read_commit, walk_back_tolerant, HistoryEntry, ReadOptions,
};
use crate::storage::{ReadStorage, StorageError, StorageReader};
use crate::verify::{classify, ErrorClass};

/// Progress phase of the copy pass: `completed` counts copies.
pub const PHASE: &str = "repair-copies";

/// Source and copy are compared in blocks of this size (D22 item 4).
const BLOCK: u64 = 64 * 1024;

/// A copy given to repair, opened read-only. `label` is how the caller named
/// it (the path); it is recorded in the plan and must be the same in
/// `apply`.
pub struct CopySource<'a> {
    pub label: String,
    pub storage: &'a dyn ReadStorage,
}

/// Why a file counts as a copy of the source (D22 item 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMatch {
    /// Its frame at offset 0 is byte-identical to the source's descriptor.
    IdenticalDescriptor,
    /// The source's descriptor is damaged, and the copy's descriptor hashes
    /// to the descriptor hash a self-verifying source commit records.
    DescriptorHash,
    /// Neither: never used.
    NotACopy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyStatus {
    /// It donated at least one frame.
    Used,
    /// It is a copy and had nothing the source lacked.
    NotNeeded,
    /// It supplied nothing: not a copy, no commit agrees, or it could not
    /// supply what the source lacks.
    NotAvailable,
    /// An I/O error disabled it.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyReport {
    /// As given.
    pub path: String,
    pub len: u64,
    /// `None`: not determined (the copy was unreadable).
    #[serde(rename = "match")]
    pub matched: Option<CopyMatch>,
    /// The highest sequence this copy agrees on with the source.
    pub agreed_through: Option<u64>,
    /// The first copy commit the source disagrees with (D22 item 3).
    pub diverged_at: Option<u64>,
    /// Copy commits whose footer lies beyond the source's length:
    /// information only, never used.
    pub later_commits: u64,
    pub frames_donated: u64,
    pub bytes_donated: u64,
    pub status: CopyStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DonationKind {
    Descriptor,
    CommitRecord,
    Footer,
    ParityFrame,
    DeltaManifest,
    CatalogImage,
    SnapshotManifest,
    KeyEnvelope,
    DataObject,
}

/// One frame laid over the source (§22.2: the source of each reconstructed
/// object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Donation {
    pub offset: u64,
    pub len: u64,
    pub kind: DonationKind,
    /// Index into the plan's `copies`.
    pub copy: usize,
}

/// How a snapshot's commit is established (D22 item 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// The source's own chain (possibly with frames a copy supplied, which
    /// the chain's hashes verify).
    #[default]
    Chain,
    /// The commit's existence rests on a copy: the source cannot show it
    /// (D22 item 3 (ii)).
    CopyConfirmed,
    /// Recovered only with the help of the bounded salvage scan (ladder step
    /// 7, Annex B.2.13 D23): the scan established the commit's publication
    /// (its own footer, or a later commit's parent link), regenerated a
    /// footer on its chain, or re-encoded the descriptor that opens it. A
    /// snapshot the trusted chain recovers alone is never labelled so.
    Salvage,
}

/// What the copy pass did.
pub(crate) struct Outcome {
    pub copies: Vec<CopyReport>,
    pub donations: Vec<Donation>,
    /// Sequences of commits agreed by a copy alone.
    pub confirmed: BTreeSet<u64>,
}

/// A failure while working on one copy. Only the second disables the copy.
enum Fail {
    /// The source, or the run itself (cancellation): fatal.
    Run(MochiError),
    /// An I/O error on the copy: the copy is reported `unreadable`.
    Copy(MochiError),
}

type R<T> = std::result::Result<T, Fail>;

fn run<T>(r: Result<T>) -> R<T> {
    r.map_err(Fail::Run)
}

fn from_format<T>(r: std::result::Result<T, mochi_format::FormatError>) -> Result<T> {
    r.map_err(MochiError::from)
}

/// Source side: evidence about the archive is `None`; anything that
/// compromises the run is an error (as in `repair`).
fn soft_run<T>(r: Result<T>) -> R<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if classify(e.code) == ErrorClass::Operational => Err(Fail::Run(e)),
        Err(_) => Ok(None),
    }
}

/// Copy side: a failure to read the copy disables it; cancellation stops the
/// run; anything else is just something this copy cannot supply.
fn soft_copy<T>(r: Result<T>) -> R<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code == ErrorCode::IoError => Err(Fail::Copy(e)),
        Err(e) if e.code == ErrorCode::Cancelled => Err(Fail::Run(e)),
        Err(_) => Ok(None),
    }
}

fn names_commit(f: &mochi_format::footer::ValidatedFooter) -> bool {
    f.commit_kind == FrameKind::CommitRecord
}

/// `len` bytes at `at`, or `OUT_OF_BOUNDS` / `LIMIT_EXCEEDED`.
fn read_vec(s: &dyn ReadStorage, at: u64, len: u64, max: u64) -> Result<Vec<u8>> {
    if len > max {
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            format!("a frame of {len} bytes exceeds the limit {max}"),
        ));
    }
    let end = at
        .checked_add(len)
        .ok_or_else(|| MochiError::new(ErrorCode::OutOfBounds, "range overflows"))?;
    if end > s.size()? {
        return Err(MochiError::new(
            ErrorCode::OutOfBounds,
            "range is past the end",
        ));
    }
    let n = usize::try_from(len)
        .map_err(|_| MochiError::new(ErrorCode::LimitExceeded, "frame does not fit in memory"))?;
    let mut buf = vec![0u8; n];
    s.read_exact_at(at, &mut buf)?;
    Ok(buf)
}

/// A copy's storage with a memory of I/O errors. Head discovery
/// ([`locate_head`]) treats an unreadable footer as "not a footer" and keeps
/// scanning; the copy must still be reported unreadable, so every I/O error
/// is remembered whether or not the caller swallowed it.
struct Probe<'a> {
    inner: &'a dyn ReadStorage,
    failed: std::cell::RefCell<Option<MochiError>>,
}

impl<'a> Probe<'a> {
    fn new(inner: &'a dyn ReadStorage) -> Self {
        Probe {
            inner,
            failed: std::cell::RefCell::new(None),
        }
    }

    fn note<T>(
        &self,
        r: std::result::Result<T, StorageError>,
    ) -> std::result::Result<T, StorageError> {
        if let Err(StorageError::Io(e)) = &r {
            let mut slot = self.failed.borrow_mut();
            if slot.is_none() {
                *slot = Some(MochiError::new(ErrorCode::IoError, e.to_string()));
            }
        }
        r
    }

    /// The first I/O error seen, if any.
    fn check(&self) -> R<()> {
        match self.failed.borrow_mut().take() {
            Some(e) => Err(Fail::Copy(e)),
            None => Ok(()),
        }
    }
}

impl ReadStorage for Probe<'_> {
    fn size(&self) -> std::result::Result<u64, StorageError> {
        self.note(self.inner.size())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, StorageError> {
        self.note(self.inner.read_at(offset, buf))
    }
}

/// What the source says about itself, before any donation.
struct Facts {
    len: u64,
    /// The sequence of its latest self-verifying commit.
    head_seq: Option<u64>,
    /// The descriptor reference a self-verifying commit records (key 10).
    descriptor: Option<ObjectRef>,
    /// Sequence → (footer offset, commit ID), over the chain from the head.
    chain: BTreeMap<u64, (u64, CommitId)>,
}

fn source_facts(view: &HealedSource<'_>, opts: &ReadOptions) -> R<Facts> {
    let mut facts = Facts {
        len: run(view.size().map_err(MochiError::from))?,
        head_seq: None,
        descriptor: None,
        chain: BTreeMap::new(),
    };
    let Some(loc) = soft_run(locate_head(view, &opts.limits))? else {
        return Ok(facts);
    };
    let Some((commit, commit_id)) = soft_run(read_commit(view, &loc.footer, opts))? else {
        return Ok(facts);
    };
    facts.head_seq = Some(commit.seq);
    facts.descriptor = Some(commit.descriptor);
    let head = HistoryEntry {
        footer_offset: loc.footer.footer_offset,
        commit_offset: loc.footer.fields.commit_offset,
        commit,
        commit_id,
    };
    let (chain, _) = match soft_run(walk_back_tolerant(view, head, opts))? {
        Some(c) => c,
        None => return Ok(facts),
    };
    for e in chain {
        facts
            .chain
            .insert(e.commit.seq, (e.footer_offset, e.commit_id));
    }
    Ok(facts)
}

/// The source's self-verifying commit whose footer is at `footer_offset`: the
/// footer validates and names a commit frame that decodes (D22 item 3).
fn self_verifying_at(
    view: &HealedSource<'_>,
    footer_offset: u64,
    opts: &ReadOptions,
) -> R<Option<(u64, CommitId)>> {
    let r = run(StorageReader::new(view).map_err(MochiError::from))?;
    let Some(f) = soft_run(from_format(validate_footer(
        &r,
        footer_offset,
        &opts.limits,
    )))?
    else {
        return Ok(None);
    };
    if !names_commit(&f) {
        return Ok(None);
    }
    Ok(soft_run(read_commit(view, &f, opts))?.map(|(c, id)| (c.seq, id)))
}

/// What a frame must satisfy, and the hash or rule the copy's agreed
/// structure supplies.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Stored-object hash over exactly these bytes.
    Stored(StoredObjectHash),
    /// A commit frame whose commit ID is this.
    Commit(CommitId),
    /// A footer that validates.
    Footer,
    /// A parity frame whose own digest and layout decode.
    Parity,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    offset: u64,
    len: u64,
    kind: DonationKind,
    expect: Expect,
}

impl Candidate {
    fn end(&self) -> u64 {
        self.offset.saturating_add(self.len)
    }
}

/// One copy, looked at alone.
struct Analysis {
    report: CopyReport,
    /// Frames the copy's agreed structure assigns an expectation to, sorted
    /// by offset, disjoint.
    candidates: Vec<Candidate>,
    /// End of the last agreed footer.
    agreed_end: u64,
    /// Sequences agreed only through a copy (D22 item 3 (ii)).
    confirmed: BTreeSet<u64>,
}

fn blank_report(copy: &CopySource<'_>, len: u64) -> CopyReport {
    CopyReport {
        path: copy.label.clone(),
        len,
        matched: None,
        agreed_through: None,
        diverged_at: None,
        later_commits: 0,
        frames_donated: 0,
        bytes_donated: 0,
        status: CopyStatus::NotAvailable,
        detail: String::new(),
    }
}

fn is_copy(
    view: &HealedSource<'_>,
    copy: &dyn ReadStorage,
    facts: &Facts,
    opts: &ReadOptions,
) -> R<CopyMatch> {
    let max = opts.limits.max_frame_len;
    let rs = run(StorageReader::new(view).map_err(MochiError::from))?;
    let rc = StorageReader::new(copy).map_err(|e| Fail::Copy(e.into()))?;
    // (a) the frame at offset 0 is byte-identical.
    let src_span = soft_run(from_format(walk_frame(&rs, 0, &opts.limits)))?;
    let cp_span = soft_copy(from_format(walk_frame(&rc, 0, &opts.limits)))?;
    if let (Some(a), Some(b)) = (src_span, cp_span) {
        if a.len == b.len {
            let sa = soft_run(read_vec(view, 0, a.len, max))?;
            let sb = soft_copy(read_vec(copy, 0, b.len, max))?;
            if let (Some(sa), Some(sb)) = (sa, sb) {
                if sa == sb {
                    return Ok(CopyMatch::IdenticalDescriptor);
                }
            }
        }
    }
    // (b) the source's descriptor is damaged, and the copy's is the one a
    // self-verifying commit records.
    if let Some(d) = facts.descriptor.filter(|d| d.offset == 0) {
        let source_good = soft_run(read_vec(view, 0, d.stored_len, max))?
            .is_some_and(|b| stored_object_hash(stored(b).view()) == d.stored_hash);
        if !source_good {
            let copy_good = soft_copy(read_vec(copy, 0, d.stored_len, max))?
                .is_some_and(|b| stored_object_hash(stored(b).view()) == d.stored_hash);
            if copy_good {
                return Ok(CopyMatch::DescriptorHash);
            }
        }
    }
    Ok(CopyMatch::NotACopy)
}

fn stored(bytes: Vec<u8>) -> mochi_format::repr::StoredObject {
    mochi_format::repr::StoredObject::from_loaded(bytes)
}

fn ref_kind(name: &str) -> Option<DonationKind> {
    match name {
        "delta manifest" => Some(DonationKind::DeltaManifest),
        "catalog image" => Some(DonationKind::CatalogImage),
        "snapshot manifest" => Some(DonationKind::SnapshotManifest),
        "key envelope" => Some(DonationKind::KeyEnvelope),
        // The descriptor is handled on its own; the data region is a range
        // of data objects, each of which has its own hash.
        _ => None,
    }
}

fn analyze(
    view: &HealedSource<'_>,
    copy: &CopySource<'_>,
    facts: &Facts,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> R<Analysis> {
    let cp = copy.storage;
    let copy_len = cp.size().map_err(|e| Fail::Copy(e.into()))?;
    let mut report = blank_report(copy, copy_len);
    let none = |mut report: CopyReport, detail: &str| Analysis {
        report: {
            report.detail = detail.to_owned();
            report
        },
        candidates: Vec::new(),
        agreed_end: 0,
        confirmed: BTreeSet::new(),
    };

    let matched = is_copy(view, cp, facts, opts)?;
    report.matched = Some(matched);
    if matched == CopyMatch::NotACopy {
        return Ok(none(
            report,
            "not a copy of the source: its first frame is not the source's descriptor frame, \
             and its descriptor is not the one a self-verifying commit of the source records",
        ));
    }

    // The copy, opened as it is (its own parity is not applied).
    let rc = StorageReader::new(cp).map_err(|e| Fail::Copy(e.into()))?;
    let head = soft_copy(locate_head(cp, &opts.limits))?;
    let head = match head {
        Some(loc) => {
            soft_copy(read_commit(cp, &loc.footer, opts))?.map(|(commit, commit_id)| HistoryEntry {
                footer_offset: loc.footer.footer_offset,
                commit_offset: loc.footer.fields.commit_offset,
                commit,
                commit_id,
            })
        }
        None => None,
    };
    let chain: Vec<HistoryEntry> = match head {
        Some(h) => soft_copy(walk_back_tolerant(cp, h, opts))?
            .map(|(c, _)| c)
            .unwrap_or_default(),
        None => Vec::new(),
    };
    if chain.is_empty() {
        return Ok(none(report, "the copy holds no valid commit"));
    }

    // Agreement (D22 item 3), per copy commit. Divergence and the source's
    // length end it; a commit the source cannot show (and that does not
    // extend the source's latest commit) is simply not agreed, and nothing
    // chains through it.
    enum Verdict {
        Agreed { by_copy: bool },
        Diverged(String),
        Unagreed,
    }
    let mut agreed: Vec<usize> = Vec::new();
    let mut confirmed = BTreeSet::new();
    let mut unagreed: Vec<u64> = Vec::new();
    let mut stopped = false;
    let mut prev_agreed = false;
    let mut diverged_why = String::new();
    for (i, e) in chain.iter().enumerate() {
        ctx.check_cancelled().map_err(Fail::Run)?;
        let beyond = e
            .footer_offset
            .checked_add(FOOTER_FRAME_LEN)
            .is_none_or(|end| end > facts.len);
        if beyond {
            report.later_commits += 1;
            stopped = true;
            prev_agreed = false;
            continue;
        }
        if stopped {
            continue;
        }
        let here = self_verifying_at(view, e.footer_offset, opts)?;
        let same_seq = facts.chain.get(&e.commit.seq).copied();
        let verdict = match here {
            Some((seq, id)) if seq == e.commit.seq && id == e.commit_id => {
                Verdict::Agreed { by_copy: false }
            }
            Some(_) => Verdict::Diverged(format!(
                "the source holds a different commit at footer offset {}",
                e.footer_offset
            )),
            None => match same_seq {
                Some((off, id)) if (off, id) != (e.footer_offset, e.commit_id) => {
                    Verdict::Diverged(format!(
                        "the source's commit {} is a different commit (footer offset {off})",
                        e.commit.seq
                    ))
                }
                _ if prev_agreed && facts.head_seq.is_some_and(|h| e.commit.seq > h) => {
                    Verdict::Agreed { by_copy: true }
                }
                _ => Verdict::Unagreed,
            },
        };
        match verdict {
            Verdict::Agreed { by_copy } => {
                agreed.push(i);
                if by_copy {
                    confirmed.insert(e.commit.seq);
                }
                prev_agreed = true;
            }
            Verdict::Diverged(why) => {
                report.diverged_at = Some(e.commit.seq);
                diverged_why = why;
                stopped = true;
                prev_agreed = false;
            }
            Verdict::Unagreed => {
                unagreed.push(e.commit.seq);
                prev_agreed = false;
            }
        }
    }
    let unagreed_note = |list: &[u64]| {
        format!(
            "commit{} {} cannot be agreed: the source cannot show {} and {} not extend its \
             latest commit",
            if list.len() == 1 { "" } else { "s" },
            list.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            if list.len() == 1 { "it" } else { "them" },
            if list.len() == 1 {
                "it does"
            } else {
                "they do"
            },
        )
    };
    let diverged_at = report.diverged_at;
    let Some(&last) = agreed.last() else {
        return Ok(none(
            report,
            &format!(
                "no commit of the copy agrees with the source{}",
                if !diverged_why.is_empty() {
                    format!(
                        ": diverged at commit {}, {diverged_why}",
                        diverged_at.unwrap_or(0)
                    )
                } else if !unagreed.is_empty() {
                    format!(": {}", unagreed_note(&unagreed))
                } else {
                    String::new()
                }
            ),
        ));
    };
    report.agreed_through = Some(chain[last].commit.seq);
    let agreed_end = chain[last].footer_offset + FOOTER_FRAME_LEN;
    let mut notes = vec![format!("agrees through commit {}", chain[last].commit.seq)];
    if let Some(d) = report.diverged_at {
        notes.push(format!(
            "diverged at commit {d} ({diverged_why}); nothing above is taken from it"
        ));
    }
    if !unagreed.is_empty() {
        notes.push(unagreed_note(&unagreed));
    }
    if report.later_commits > 0 {
        notes.push(format!(
            "holds {} later commit{} beyond the source's length, which are never used",
            report.later_commits,
            if report.later_commits == 1 { "" } else { "s" }
        ));
    }
    if !confirmed.is_empty() {
        notes.push(format!(
            "commits {} are confirmed by this copy alone",
            confirmed
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    report.detail = notes.join("; ");

    // The frames the agreed structure gives an expectation for.
    let mut candidates: Vec<Candidate> = Vec::new();
    let desc_end = soft_copy(from_format(walk_frame(&rc, 0, &opts.limits)))?.map_or(0, |s| s.end());
    if matched == CopyMatch::DescriptorHash {
        if let Some(d) = facts.descriptor {
            candidates.push(Candidate {
                offset: 0,
                len: d.stored_len,
                kind: DonationKind::Descriptor,
                expect: Expect::Stored(d.stored_hash),
            });
        }
    }
    for &i in &agreed {
        ctx.check_cancelled().map_err(Fail::Run)?;
        let e = &chain[i];
        let Some(f) = soft_copy(from_format(validate_footer(
            &rc,
            e.footer_offset,
            &opts.limits,
        )))?
        else {
            continue;
        };
        let commit_end = f.fields.commit_offset.saturating_add(f.fields.commit_len);
        candidates.push(Candidate {
            offset: f.fields.commit_offset,
            len: f.fields.commit_len,
            kind: DonationKind::CommitRecord,
            expect: Expect::Commit(e.commit_id),
        });
        candidates.push(Candidate {
            offset: e.footer_offset,
            len: FOOTER_FRAME_LEN,
            kind: DonationKind::Footer,
            expect: Expect::Footer,
        });
        // Parity frames sit between the commit frame and its footer.
        let mut at = commit_end;
        while at < e.footer_offset {
            let Some(span) = soft_copy(from_format(walk_frame(&rc, at, &opts.limits)))? else {
                break;
            };
            if span.kind != FrameKind::ParityObject || span.end() > e.footer_offset {
                break;
            }
            candidates.push(Candidate {
                offset: span.offset,
                len: span.len,
                kind: DonationKind::ParityFrame,
                expect: Expect::Parity,
            });
            at = span.end();
        }
        for (name, r) in e.commit.object_refs() {
            let Some(kind) = ref_kind(name) else {
                continue;
            };
            if r.end().is_ok_and(|end| end <= e.commit_offset) {
                candidates.push(Candidate {
                    offset: r.offset,
                    len: r.stored_len,
                    kind,
                    expect: Expect::Stored(r.stored_hash),
                });
            }
        }
    }
    // Data objects: the copy's catalog at the highest agreed commit that
    // opens lists every object up to it, with its location and hash.
    for &i in agreed.iter().rev() {
        ctx.check_cancelled().map_err(Fail::Run)?;
        let Some(h) = soft_copy(open_at_footer(cp, chain[i].footer_offset, opts))? else {
            continue;
        };
        if let Ok(ids) = h.catalog.object_ids() {
            for id in ids {
                let (Ok(Some(rec)), Ok(Some(at))) =
                    (h.catalog.object(&id), h.catalog.object_location(&id))
                else {
                    continue;
                };
                candidates.push(Candidate {
                    offset: at,
                    len: rec.stored_len,
                    kind: DonationKind::DataObject,
                    expect: Expect::Stored(rec.stored_hash),
                });
            }
        }
        break;
    }
    // In range, outside the descriptor (unless it is the descriptor), sorted,
    // disjoint: a frame that overlaps one already kept is dropped.
    candidates.retain(|c| {
        c.len > 0
            && c.end() <= agreed_end
            && c.end() <= facts.len
            && c.end() <= copy_len
            && (c.kind == DonationKind::Descriptor || c.offset >= desc_end)
    });
    candidates.sort_by_key(|c| (c.offset, c.len));
    let mut kept: Vec<Candidate> = Vec::new();
    for c in candidates {
        if kept.last().is_none_or(|p| p.end() <= c.offset) {
            kept.push(c);
        }
    }
    Ok(Analysis {
        report,
        candidates: kept,
        agreed_end,
        confirmed,
    })
}

/// Blocks (of [`BLOCK`] bytes) of `[0, end)` where the view and the copy
/// differ.
fn differing_blocks(
    view: &HealedSource<'_>,
    copy: &dyn ReadStorage,
    end: u64,
    ctx: &JobContext<'_>,
) -> R<BTreeSet<u64>> {
    let mut out = BTreeSet::new();
    let mut a = vec![0u8; BLOCK as usize];
    let mut b = vec![0u8; BLOCK as usize];
    let mut at = 0u64;
    while at < end {
        ctx.check_cancelled().map_err(Fail::Run)?;
        let n = (end - at).min(BLOCK) as usize;
        run(view
            .read_exact_at(at, &mut a[..n])
            .map_err(MochiError::from))?;
        copy.read_exact_at(at, &mut b[..n])
            .map_err(|e| Fail::Copy(MochiError::from(e)))?;
        if a[..n] != b[..n] {
            out.insert(at / BLOCK);
        }
        at += n as u64;
    }
    Ok(out)
}

fn touches(blocks: &BTreeSet<u64>, c: &Candidate) -> bool {
    let first = c.offset / BLOCK;
    let last = (c.end() - 1) / BLOCK;
    blocks.range(first..=last).next().is_some()
}

/// Whether `bytes` (the frame of `c`, read from `storage`) satisfies the
/// candidate's expectation.
fn satisfies(
    c: &Candidate,
    bytes: &[u8],
    storage: &dyn ReadStorage,
    opts: &ReadOptions,
) -> Result<bool> {
    Ok(match c.expect {
        Expect::Stored(h) => {
            stored_object_hash(mochi_format::repr::StoredObject::from_loaded(bytes.to_vec()).view())
                == h
        }
        Expect::Commit(id) => CommitRecord::from_stored(bytes, &opts.limits, &opts.cbor)
            .is_ok_and(|(_, got)| got == id),
        Expect::Footer => {
            let r = StorageReader::new(storage)?;
            match validate_footer(&r, c.offset, &opts.limits) {
                Ok(_) => true,
                Err(mochi_format::FormatError::Source(m)) => {
                    return Err(MochiError::new(ErrorCode::IoError, m))
                }
                Err(_) => false,
            }
        }
        Expect::Parity => {
            parity::decode_frame(bytes, KNOWN_REQUIRED_FEATURES, &opts.limits).is_ok()
        }
    })
}

/// The candidate's bytes in the copy: `None` when they cannot be read for a
/// reason other than I/O (outside the file, over a limit).
fn copy_bytes(copy: &dyn ReadStorage, c: &Candidate, max: u64) -> R<Option<Vec<u8>>> {
    match read_vec(copy, c.offset, c.len, max) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.code == ErrorCode::IoError => Err(Fail::Copy(e)),
        Err(_) => Ok(None),
    }
}

/// Pass two for one copy: donate into the view. Returns the frames donated
/// and the number of damaged frames the copy could not supply.
fn donate_from(
    index: usize,
    copy: &dyn ReadStorage,
    a: &Analysis,
    view: &mut HealedSource<'_>,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
    done: &mut Vec<Donation>,
) -> R<(u64, bool)> {
    let blocks = differing_blocks(view, copy, a.agreed_end, ctx)?;
    if blocks.is_empty() {
        return Ok((0, false));
    }
    let max = opts
        .limits
        .max_frame_len
        .max(opts.limits.max_commit_frame_len);
    let mut unsupplied = 0u64;
    for c in &a.candidates {
        ctx.check_cancelled().map_err(Fail::Run)?;
        if !touches(&blocks, c) || c.len > max {
            continue;
        }
        let source_bytes = run(read_vec(view, c.offset, c.len, max))?;
        let source_good = match c.expect {
            // A footer is good when it validates and is the copy's footer
            // (the commit frame may have been donated just before it).
            Expect::Footer => {
                copy_bytes(copy, c, max)?.is_some_and(|b| b == source_bytes)
                    && run(satisfies(c, &source_bytes, view, opts))?
            }
            _ => run(satisfies(c, &source_bytes, view, opts))?,
        };
        if source_good {
            continue;
        }
        let Some(copy_bytes) = copy_bytes(copy, c, max)? else {
            unsupplied += 1;
            continue;
        };
        let copy_good = match satisfies(c, &copy_bytes, copy, opts) {
            Ok(g) => g,
            Err(e) if e.code == ErrorCode::IoError => return Err(Fail::Copy(e)),
            Err(_) => false,
        };
        if !copy_good {
            unsupplied += 1;
            continue;
        }
        view.donate(c.offset, copy_bytes);
        done.push(Donation {
            offset: c.offset,
            len: c.len,
            kind: c.kind,
            copy: index,
        });
    }
    Ok((unsupplied, true))
}

/// Run step 5: analyze every copy, then let each donate in argument order.
pub(crate) fn donate(
    view: &mut HealedSource<'_>,
    copies: &[CopySource<'_>],
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<Outcome> {
    let mut out = Outcome {
        copies: Vec::new(),
        donations: Vec::new(),
        confirmed: BTreeSet::new(),
    };
    if copies.is_empty() {
        return Ok(out);
    }
    let unreadable = |copy: &CopySource<'_>, e: MochiError| {
        let mut r = blank_report(copy, 0);
        r.len = copy.storage.size().unwrap_or(0);
        r.status = CopyStatus::Unreadable;
        r.detail = format!("disabled by an I/O error: {}", e.message);
        r
    };
    let flat = |f: Fail| -> MochiError {
        match f {
            Fail::Run(e) | Fail::Copy(e) => e,
        }
    };
    let facts = match source_facts(view, opts) {
        Ok(f) => f,
        Err(f) => return Err(flat(f)),
    };
    // Pass one: each copy alone, against the source as parity left it.
    let mut analyses: Vec<std::result::Result<Analysis, MochiError>> = Vec::new();
    for (i, copy) in copies.iter().enumerate() {
        ctx.report(PHASE, i as u64, Some(copies.len() as u64));
        let probe = Probe::new(copy.storage);
        let probed = CopySource {
            label: copy.label.clone(),
            storage: &probe,
        };
        match analyze(view, &probed, &facts, opts, ctx).and_then(|a| probe.check().map(|()| a)) {
            Ok(a) => analyses.push(Ok(a)),
            Err(Fail::Copy(e)) => analyses.push(Err(e)),
            Err(Fail::Run(e)) => return Err(e),
        }
    }
    // Pass two: donate, first copy first.
    for (i, (copy, analysis)) in copies.iter().zip(analyses).enumerate() {
        ctx.report(PHASE, i as u64, Some(copies.len() as u64));
        let mut a = match analysis {
            Ok(a) => a,
            Err(e) => {
                out.copies.push(unreadable(copy, e));
                continue;
            }
        };
        let mut done = Vec::new();
        let probe = Probe::new(copy.storage);
        let result = if a.candidates.is_empty() {
            Ok((0, false))
        } else {
            donate_from(i, &probe, &a, view, opts, ctx, &mut done)
        }
        .and_then(|r| probe.check().map(|()| r));
        match result {
            Err(Fail::Run(e)) => return Err(e),
            Err(Fail::Copy(e)) => {
                for d in &done {
                    view.undonate(d.offset);
                }
                out.copies.push(unreadable(copy, e));
            }
            Ok((unsupplied, differs)) => {
                a.report.frames_donated = done.len() as u64;
                a.report.bytes_donated = done.iter().map(|d| d.len).sum();
                if !done.is_empty() {
                    a.report.status = CopyStatus::Used;
                    out.confirmed.extend(a.confirmed.iter().copied());
                } else if a.candidates.is_empty() {
                    // Not a copy, or nothing agrees: the detail says why.
                    a.report.status = CopyStatus::NotAvailable;
                } else if !differs {
                    a.report.status = CopyStatus::NotNeeded;
                    a.report.detail.push_str(
                        "; byte-identical to the source over the agreed range, so it supplies \
                         nothing",
                    );
                } else if unsupplied > 0 {
                    a.report.status = CopyStatus::NotAvailable;
                    a.report.detail.push_str(&format!(
                        "; {unsupplied} damaged frame{} of the source could not be supplied \
                         (the copy's are damaged too)",
                        if unsupplied == 1 { "" } else { "s" }
                    ));
                } else {
                    a.report.status = CopyStatus::NotNeeded;
                    a.report
                        .detail
                        .push_str("; the frames that differ are good in the source");
                }
                out.donations.extend(done);
                out.copies.push(a.report);
            }
        }
    }
    out.donations.sort_by_key(|d| d.offset);
    Ok(out)
}
