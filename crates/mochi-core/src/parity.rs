//! Redundancy profile, read side (spec §15, §22 step 6; Annex B.2.11 D21;
//! the wire layout is in [`mochi_format::parity`]).
//!
//! [`scan`] walks the commits from the head back to commit 0. For each it
//! finds the parity frames between the commit frame and its footer, checks
//! every data shard of the protected range against the hash table those
//! frames carry, and rebuilds the damaged shards of every group that has
//! enough intact shards. Rebuilt bytes go into an overlay
//! ([`HealedSource`]), never into the archive: `verify` stays read-only and
//! `repair` writes a new archive (§22.2).
//!
//! # Why the walk can heal its own path
//!
//! Commit *n* protects \[footer *n* − 1, end of commit frame *n*): its own
//! objects and commit record, **and its parent's footer**. So once commit
//! *n*'s range is healed, the parent's footer reads correctly, and it locates
//! the parent's commit frame and parity. Footers are therefore parsed, not
//! validated, to find the parity (a footer's digest covers its commit frame,
//! which may itself be what needs healing); each is validated after its
//! commit's range is healed, and the walk stops at the first that still
//! fails. The head's own footer is in no range: if it is damaged, the head
//! is found by the forward scan of [`locate_head`], as without parity.
//!
//! # Never wrong bytes
//!
//! A shard is replaced only when its hash differs from the table, the group
//! has at least *k* intact shards, and the rebuilt shard hashes to the
//! table's value. Tables come from frames whose own digest verified. And a
//! rebuilt byte is still only data: everything that reads it checks the
//! commit chain's hashes as usual, which no parity frame can satisfy for
//! bytes the writer did not write. A group with too many damaged shards is
//! left alone and reported (`PARITY_CAPACITY_EXCEEDED`).
//!
//! A commit's frames must agree with the commit (identity, range, layout);
//! frames that verify but disagree are a profile violation, and their
//! patches are dropped.

use std::collections::BTreeMap;

use mochi_format::footer::{parse_footer_frame, validate_footer, FOOTER_FRAME_LEN};
use mochi_format::frame::walk_frame;
use mochi_format::parity;
use mochi_format::registry;
use mochi_format::Limits;
use serde::{Deserialize, Serialize};

use crate::commit::CommitRecord;
use crate::descriptor::{Descriptor, KNOWN_REQUIRED_FEATURES};
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::publish::{locate_head, ReadOptions};
use crate::storage::{ReadStorage, StorageError, StorageReader};

/// Progress phase of [`scan`]: `completed` counts commits.
pub const PHASE: &str = "parity";

/// Most candidate positions tried when a commit's first parity frame does
/// not decode (bounded work on hostile input).
const MAX_CANDIDATES: usize = 4096;

/// A read-only view of an archive with rebuilt shards laid over it, and on
/// top of those, whole frames donated by independent copies (ladder step 5,
/// Annex B.2.12 D22; [`crate::copies`]).
pub struct HealedSource<'a> {
    inner: &'a dyn ReadStorage,
    /// Offset → bytes. Patches never overlap: each is one shard's stored
    /// part, and the protected ranges of different commits are disjoint.
    patches: BTreeMap<u64, Vec<u8>>,
    /// Offset → bytes of a whole frame a copy supplied. Frames never overlap
    /// each other; they read over `patches`.
    donated: BTreeMap<u64, Vec<u8>>,
    /// Offset → bytes the salvage scan re-derived (ladder step 7, Annex B.2.13
    /// D23): footers regenerated for commits whose publication a later
    /// commit's parent link confirms, and the descriptor re-encoded when its
    /// hash matches the one every confirmed commit records. Read over
    /// everything else; never trusted without the checks that follow.
    restored: BTreeMap<u64, Vec<u8>>,
}

impl<'a> HealedSource<'a> {
    /// No patches: reads exactly `inner`.
    pub fn new(inner: &'a dyn ReadStorage) -> Self {
        HealedSource {
            inner,
            patches: BTreeMap::new(),
            donated: BTreeMap::new(),
            restored: BTreeMap::new(),
        }
    }

    /// Whether any byte differs from the archive.
    pub fn is_patched(&self) -> bool {
        !self.patches.is_empty() || !self.donated.is_empty() || !self.restored.is_empty()
    }

    /// Lay bytes the salvage scan re-derived over the view (D23).
    pub(crate) fn restore(&mut self, offset: u64, bytes: Vec<u8>) {
        self.restored.insert(offset, bytes);
    }

    /// Bytes replaced by parity.
    pub fn patched_bytes(&self) -> u64 {
        self.patches.values().map(|p| p.len() as u64).sum()
    }

    /// Lay a whole frame from a copy over the view (D22 item 4).
    pub(crate) fn donate(&mut self, offset: u64, bytes: Vec<u8>) {
        self.donated.insert(offset, bytes);
    }

    /// Take back a donation (a copy that failed part-way is dropped whole).
    pub(crate) fn undonate(&mut self, offset: u64) {
        self.donated.remove(&offset);
    }
}

/// Lay the non-overlapping `patches` over `buf`, which holds the bytes at
/// `offset`.
fn overlay(patches: &BTreeMap<u64, Vec<u8>>, offset: u64, buf: &mut [u8], n: usize) {
    let end = offset.saturating_add(n as u64);
    // The patch starting at or before `offset`, then every one inside.
    let first = patches
        .range(..=offset)
        .next_back()
        .map_or(offset, |(at, _)| *at);
    for (at, bytes) in patches.range(first..end) {
        let p_end = at.saturating_add(bytes.len() as u64);
        let lo = (*at).max(offset);
        let hi = p_end.min(end);
        if lo >= hi {
            continue;
        }
        let dst = &mut buf[(lo - offset) as usize..(hi - offset) as usize];
        dst.copy_from_slice(&bytes[(lo - at) as usize..(hi - at) as usize]);
    }
}

impl ReadStorage for HealedSource<'_> {
    fn size(&self) -> std::result::Result<u64, StorageError> {
        self.inner.size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::result::Result<usize, StorageError> {
        let n = self.inner.read_at(offset, buf)?;
        overlay(&self.patches, offset, buf, n);
        overlay(&self.donated, offset, buf, n);
        overlay(&self.restored, offset, buf, n);
        Ok(n)
    }
}

/// A parity group whose damage its parity cannot rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupLoss {
    pub group: u64,
    /// Damaged data shards in the group.
    pub damaged_shards: u64,
    /// Intact parity shards in the group (fewer than `damaged_shards`).
    pub intact_parity: u64,
    /// The damaged shards' byte ranges, `[offset, offset + len)`.
    pub ranges: Vec<(u64, u64)>,
}

/// What [`scan`] found for one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitParity {
    /// The sequence the commit's footer names.
    pub seq: u64,
    pub footer_offset: u64,
    /// The protected range, from the frames; `None` when no frame decoded.
    pub range_offset: Option<u64>,
    pub range_len: Option<u64>,
    /// Parity frames the layout calls for, and how many were unusable.
    pub frames: u64,
    pub frames_damaged: u64,
    /// Real data shards whose bytes did not match their hash, and how many
    /// of them were rebuilt.
    pub shards_damaged: u64,
    pub shards_rebuilt: u64,
    /// Data shards that could not be checked: their group has no intact
    /// frame to give the hash table.
    pub shards_unchecked: u64,
    pub beyond_capacity: Vec<GroupLoss>,
    /// Frames that verify but disagree with the commit or with each other,
    /// or a commit with no parity at all: a profile violation (the patches
    /// of such a commit are not used).
    pub violation: Option<String>,
}

impl CommitParity {
    fn new(seq: u64, footer_offset: u64) -> Self {
        CommitParity {
            seq,
            footer_offset,
            range_offset: None,
            range_len: None,
            frames: 0,
            frames_damaged: 0,
            shards_damaged: 0,
            shards_rebuilt: 0,
            shards_unchecked: 0,
            beyond_capacity: Vec::new(),
            violation: None,
        }
    }
}

/// The result of [`scan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParityScan {
    /// The descriptor's parity percentage, read through the healed view;
    /// `None` if the archive declares no recovery record or the descriptor
    /// could not be read.
    pub declared_percent: Option<u16>,
    /// From the head down.
    pub commits: Vec<CommitParity>,
    /// Why the walk ended before commit 0, if it did.
    pub stopped: Option<String>,
}

impl ParityScan {
    pub fn frames_damaged(&self) -> u64 {
        self.commits.iter().map(|c| c.frames_damaged).sum()
    }
    pub fn shards_damaged(&self) -> u64 {
        self.commits.iter().map(|c| c.shards_damaged).sum()
    }
    pub fn shards_rebuilt(&self) -> u64 {
        self.commits.iter().map(|c| c.shards_rebuilt).sum()
    }
    pub fn shards_unchecked(&self) -> u64 {
        self.commits.iter().map(|c| c.shards_unchecked).sum()
    }
    pub fn beyond_capacity(&self) -> impl Iterator<Item = (u64, &GroupLoss)> {
        self.commits
            .iter()
            .flat_map(|c| c.beyond_capacity.iter().map(move |g| (c.seq, g)))
    }
    pub fn violations(&self) -> impl Iterator<Item = (u64, &str)> {
        self.commits
            .iter()
            .filter_map(|c| c.violation.as_deref().map(|v| (c.seq, v)))
    }
}

/// The footer to start from: the one at end of file if it parses, else the
/// latest valid one the forward scan finds. `None`: no footer at all.
fn start_footer(src: &dyn ReadStorage, limits: &Limits) -> Result<Option<(u64, String)>> {
    let size = src.size()?;
    if let Some(at) = size.checked_sub(FOOTER_FRAME_LEN) {
        let r = StorageReader::new(src)?;
        if parse_footer_frame(&r, at).is_ok() {
            return Ok(Some((at, String::new())));
        }
    }
    match locate_head(src, limits) {
        Ok(loc) => Ok(Some((
            loc.footer.footer_offset,
            "the end of the file is not a footer; parity was checked from the latest valid \
             footer, and a later commit, if there was one, is not covered"
                .to_owned(),
        ))),
        Err(e) if e.code == ErrorCode::NoValidHead => Ok(None),
        Err(e) => Err(e),
    }
}

fn io(e: &mochi_format::FormatError) -> Option<MochiError> {
    match e {
        mochi_format::FormatError::Source(msg) => {
            Some(MochiError::new(ErrorCode::IoError, msg.clone()))
        }
        _ => None,
    }
}

/// Read `len` bytes at `at` through the view; `None` when out of range.
fn read(view: &HealedSource<'_>, at: u64, len: u64) -> Result<Option<Vec<u8>>> {
    let size = view.size()?;
    if at.checked_add(len).is_none_or(|e| e > size) {
        return Ok(None);
    }
    let n = usize::try_from(len)
        .map_err(|_| MochiError::new(ErrorCode::LimitExceeded, "frame does not fit in memory"))?;
    let mut buf = vec![0u8; n];
    view.read_exact_at(at, &mut buf)?;
    Ok(Some(buf))
}

/// Largest parity frame the layout rule can produce (any features count up
/// to the envelope's 64): the bound on what a candidate may make us read.
fn max_frame_len() -> u64 {
    registry::SKIPPABLE_HEADER_LEN as u64
        + 80
        + 8 * 64
        + parity::PAYLOAD_FIXED_LEN as u64
        + 32 * parity::MAX_DATA_SHARDS
        + parity::MAX_SHARD
        + 32
}

/// The first parity frame in `[from, to)` that decodes. Tried in order: the
/// frame at `from`; if its header walks, the slots one frame length apart
/// (all frames of a commit have one length); then each position holding the
/// parity magic, found by reading the region in blocks. At most
/// [`MAX_CANDIDATES`] decodes are attempted (bounded work on hostile input).
fn find_template(
    view: &HealedSource<'_>,
    from: u64,
    to: u64,
    opts: &ReadOptions,
) -> Result<Option<(u64, Vec<u8>)>> {
    let r = StorageReader::prefix(view, to)?;
    let mut tried = 0usize;
    let attempt = |at: u64, tried: &mut usize| -> Result<Option<(Vec<u8>, u64)>> {
        *tried += 1;
        match walk_frame(&r, at, &opts.limits) {
            Ok(span) if span.end() <= to && span.len <= max_frame_len() => {
                let Some(bytes) = read(view, at, span.len)? else {
                    return Ok(None);
                };
                let ok =
                    parity::decode_frame(&bytes, KNOWN_REQUIRED_FEATURES, &opts.limits).is_ok();
                Ok(Some((if ok { bytes } else { Vec::new() }, span.len)))
            }
            Ok(_) => Ok(None),
            Err(e) => match io(&e) {
                Some(e) => Err(e),
                None => Ok(None),
            },
        }
    };
    // The first slot, and the stride its header gives.
    if let Some((bytes, len)) = attempt(from, &mut tried)? {
        if !bytes.is_empty() {
            return Ok(Some((from, bytes)));
        }
        let mut at = from + len;
        while at < to && tried < MAX_CANDIDATES {
            if let Some((bytes, _)) = attempt(at, &mut tried)? {
                if !bytes.is_empty() {
                    return Ok(Some((at, bytes)));
                }
            }
            at += len;
        }
    }
    // Every position holding the magic.
    let magic = registry::PARITY_OBJECT.to_le_bytes();
    const BLOCK: u64 = 1 << 20;
    let mut base = from;
    while base < to && tried < MAX_CANDIDATES {
        let n = (to - base).min(BLOCK + magic.len() as u64 - 1);
        let mut buf = vec![0u8; n as usize];
        view.read_exact_at(base, &mut buf)?;
        for (i, w) in buf.windows(magic.len()).enumerate() {
            if w != magic || i as u64 >= BLOCK {
                continue;
            }
            let at = base + i as u64;
            if at == from || tried >= MAX_CANDIDATES {
                continue;
            }
            if let Some((bytes, _)) = attempt(at, &mut tried)? {
                if !bytes.is_empty() {
                    return Ok(Some((at, bytes)));
                }
            }
        }
        base += BLOCK;
    }
    Ok(None)
}

/// One commit's findings, the patches it would apply (not yet applied), and
/// what its frames say about it.
type Healed = (
    CommitParity,
    BTreeMap<u64, Vec<u8>>,
    Option<parity::ParityFrameInfo>,
);

/// Check and heal one commit's range.
fn heal_commit(
    view: &HealedSource<'_>,
    seq: u64,
    footer_offset: u64,
    commit_end: u64,
    opts: &ReadOptions,
) -> Result<Healed> {
    let mut cp = CommitParity::new(seq, footer_offset);
    let mut patches = BTreeMap::new();
    if commit_end >= footer_offset {
        cp.violation = Some("the commit has no parity frames".to_owned());
        return Ok((cp, patches, None));
    }
    let Some((_, bytes)) = find_template(view, commit_end, footer_offset, opts)? else {
        // Nothing to go on: every frame is damaged (or there never were
        // any). The region exists, so the writer wrote something there.
        cp.frames_damaged = 1;
        cp.violation = None;
        return Ok((cp, patches, None));
    };
    let template = parity::decode_frame(&bytes, KNOWN_REQUIRED_FEATURES, &opts.limits)
        .map_err(MochiError::from)?;
    let layout = template.layout;
    let info = parity::ParityFrameInfo::of(&template);
    cp.range_offset = Some(layout.range_offset);
    cp.range_len = Some(layout.range_len);
    cp.frames = layout.frame_count();
    let frame_len = layout.frame_len(template.envelope.required_features.len());
    if layout.range_offset + layout.range_len != commit_end {
        cp.violation = Some(format!(
            "the parity frames protect a range ending at {}, but the commit frame ends at \
             {commit_end}",
            layout.range_offset + layout.range_len
        ));
        return Ok((cp, patches, Some(info)));
    }
    if footer_offset - commit_end != cp.frames * frame_len {
        cp.violation = Some(format!(
            "{} bytes lie between the commit frame and its footer; the layout calls for {} \
             parity frames of {frame_len} bytes",
            footer_offset - commit_end,
            cp.frames
        ));
        return Ok((cp, patches, Some(info)));
    }

    let m = layout.parity_shards;
    let k = layout.data_shards;
    let shard_len = layout.shard_size as usize;
    // Memory: a clean group holds one frame and one shard at a time; only a
    // group with damage loads its k + m shards to solve them.
    for g in 0..layout.groups {
        // Pass 1: which of the group's frames are intact, and its table.
        let mut intact: Vec<bool> = vec![false; m as usize];
        let mut table: Option<Vec<u8>> = None;
        for p in 0..m {
            let at = commit_end + (g * m + p) * frame_len;
            let Some(fb) = read(view, at, frame_len)? else {
                cp.frames_damaged += 1;
                continue;
            };
            let f = match parity::decode_frame(&fb, KNOWN_REQUIRED_FEATURES, &opts.limits) {
                Ok(f) => f,
                Err(e) => {
                    if let Some(e) = io(&e) {
                        return Err(e);
                    }
                    cp.frames_damaged += 1;
                    continue;
                }
            };
            if parity::ParityFrameInfo::of(&f) != info || f.group != g || f.parity_index != p {
                cp.violation = Some(format!(
                    "parity frame at offset {at} verifies but does not belong in its place \
                     (group {}, parity {} of a different layout or commit)",
                    f.group, f.parity_index
                ));
                return Ok((cp, BTreeMap::new(), Some(info)));
            }
            match &table {
                None => table = Some(f.hash_table().to_vec()),
                Some(t) if t[..] != *f.hash_table() => {
                    cp.violation = Some(format!(
                        "two parity frames of group {g} carry different hash tables"
                    ));
                    return Ok((cp, BTreeMap::new(), Some(info)));
                }
                Some(_) => {}
            }
            intact[p as usize] = true;
        }
        let Some(table) = table else {
            cp.shards_unchecked += (0..k)
                .filter(|j| layout.shard_extent(layout.shard_index(g, *j)).1 > 0)
                .count() as u64;
            continue;
        };
        let hash_of = |j: u64| -> [u8; 32] {
            let mut h = [0u8; 32];
            let at = (j * 32) as usize;
            h.copy_from_slice(&table[at..at + 32]);
            h
        };
        // Pass 2: hash every data shard, one at a time.
        let mut damaged = Vec::new();
        let mut buf = vec![0u8; shard_len];
        for j in 0..k {
            let (offset, n) = layout.shard_extent(layout.shard_index(g, j));
            if n > 0 {
                view.read_exact_at(offset, &mut buf[..n as usize])?;
            }
            if parity::data_shard_hash(&buf[..n as usize], &layout) == hash_of(j) {
                continue;
            }
            if n == 0 {
                cp.violation = Some(format!(
                    "group {g}'s hash table does not match its virtual all-zero shard {j}"
                ));
                return Ok((cp, BTreeMap::new(), Some(info)));
            }
            damaged.push((j, offset, n));
        }
        if damaged.is_empty() {
            continue;
        }
        cp.shards_damaged += damaged.len() as u64;
        let intact_parity = intact.iter().filter(|i| **i).count() as u64;
        let lost = || GroupLoss {
            group: g,
            damaged_shards: damaged.len() as u64,
            intact_parity,
            ranges: damaged.iter().map(|(_, o, n)| (*o, *n)).collect(),
        };
        if (damaged.len() as u64) > intact_parity {
            cp.beyond_capacity.push(lost());
            continue;
        }
        // Pass 3: load the group and solve.
        let mut shards: Vec<Option<Vec<u8>>> = Vec::with_capacity((k + m) as usize);
        for j in 0..k {
            if damaged.iter().any(|(d, _, _)| *d == j) {
                shards.push(None);
                continue;
            }
            let (offset, n) = layout.shard_extent(layout.shard_index(g, j));
            let mut b = vec![0u8; shard_len];
            view.read_exact_at(offset, &mut b[..n as usize])?;
            shards.push(Some(b));
        }
        for (p, ok) in intact.iter().enumerate() {
            let shard = if *ok {
                let at = commit_end + (g * m + p as u64) * frame_len;
                read(view, at, frame_len)?.and_then(|fb| {
                    parity::decode_frame(&fb, KNOWN_REQUIRED_FEATURES, &opts.limits)
                        .ok()
                        .map(|f| f.shard.to_vec())
                })
            } else {
                None
            };
            shards.push(shard);
        }
        match parity::reconstruct(k as usize, m as usize, &mut shards, shard_len) {
            Ok(()) => {}
            Err(mochi_format::FormatError::Parity(_)) => {
                cp.beyond_capacity.push(lost());
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        // A rebuilt shard must hash to its table entry, or nothing is used.
        let rebuilt: Vec<(u64, Vec<u8>)> = damaged
            .iter()
            .filter_map(|(j, offset, n)| {
                let s = shards[*j as usize].as_ref()?;
                let part = &s[..*n as usize];
                (parity::data_shard_hash(part, &layout) == hash_of(*j))
                    .then(|| (*offset, part.to_vec()))
            })
            .collect();
        if rebuilt.len() != damaged.len() {
            cp.beyond_capacity.push(lost());
            continue;
        }
        cp.shards_rebuilt += rebuilt.len() as u64;
        patches.extend(rebuilt);
    }
    Ok((cp, patches, Some(info)))
}

/// Walk the commits from the head back to commit 0, checking and healing
/// each one's parity (module docs). Read-only; a job. Errors only for what
/// compromises the run (I/O, cancellation); everything about the archive is
/// in the result.
pub fn scan<'a>(
    src: &'a dyn ReadStorage,
    opts: &ReadOptions,
    ctx: &JobContext<'_>,
) -> Result<(ParityScan, HealedSource<'a>)> {
    let mut view = HealedSource::new(src);
    let mut out = ParityScan {
        declared_percent: None,
        commits: Vec::new(),
        stopped: None,
    };
    let Some((mut footer_offset, note)) = start_footer(src, &opts.limits)? else {
        out.stopped = Some("no footer: nothing is committed".to_owned());
        return Ok((out, view));
    };
    if !note.is_empty() {
        out.stopped = Some(note);
    }
    loop {
        ctx.check_cancelled()?;
        ctx.report(PHASE, out.commits.len() as u64, None);
        let r = StorageReader::new(&view)?;
        let fields = match parse_footer_frame(&r, footer_offset) {
            Ok(f) => f,
            Err(e) => {
                if let Some(e) = io(&e) {
                    return Err(e);
                }
                out.stopped = Some(format!(
                    "the footer at offset {footer_offset} could not be read ({e})"
                ));
                break;
            }
        };
        let Some(commit_end) = fields
            .commit_offset
            .checked_add(fields.commit_len)
            .filter(|e| *e <= footer_offset)
        else {
            out.stopped = Some(format!(
                "the footer at offset {footer_offset} names a commit frame that does not \
                 precede it"
            ));
            break;
        };
        let (mut cp, patches, info) = heal_commit(
            &view,
            fields.commit_sequence,
            footer_offset,
            commit_end,
            opts,
        )?;
        let before: Vec<u64> = patches.keys().copied().collect();
        view.patches.extend(patches);
        // Now the commit frame and footer must verify, and the frames must
        // agree with the commit.
        let r = StorageReader::new(&view)?;
        if let Err(e) = validate_footer(&r, footer_offset, &opts.limits) {
            if let Some(e) = io(&e) {
                return Err(e);
            }
            out.commits.push(cp);
            out.stopped = Some(format!(
                "the footer at offset {footer_offset} does not validate after parity repair \
                 ({e})"
            ));
            break;
        }
        let record = read(&view, fields.commit_offset, fields.commit_len)?
            .ok_or_else(|| MochiError::new(ErrorCode::OutOfBounds, "commit frame"))
            .and_then(|b| CommitRecord::from_stored(&b, &opts.limits, &opts.cbor));
        let record = match record {
            Ok((record, _)) => record,
            Err(e) if e.code == ErrorCode::IoError => return Err(e),
            Err(e) => {
                out.commits.push(cp);
                out.stopped = Some(format!(
                    "commit {} could not be read after parity repair ({})",
                    fields.commit_sequence, e.message
                ));
                break;
            }
        };
        let range_start = record.parent.map_or(0, |p| p.footer_offset);
        if let Some(info) = info {
            let disagreement = if info.archive_id != *record.archive_id.as_bytes()
                || info.seq != record.seq
                || info.transaction_id != record.transaction_id
            {
                Some("the parity frames carry another commit's identity".to_owned())
            } else if info.required_features != record.required_features {
                Some("the parity frames list other required features than the commit".to_owned())
            } else if cp.range_offset != Some(range_start) {
                Some(format!(
                    "the parity frames protect a range starting at {:?}, not at the parent's \
                     footer ({range_start})",
                    cp.range_offset
                ))
            } else {
                None
            };
            if disagreement.is_some() && cp.violation.is_none() {
                cp.violation = disagreement;
                for at in before {
                    view.patches.remove(&at);
                }
                cp.shards_rebuilt = 0;
            }
        }
        out.commits.push(cp);
        match record.parent {
            None => break,
            Some(p) => footer_offset = p.footer_offset,
        }
    }
    // The descriptor, read through the healed view: it decides whether the
    // archive promises parity at all.
    out.declared_percent =
        read_descriptor_raw(&view, opts).and_then(|d| d.redundancy.map(|r| r.percent));
    ctx.report(
        PHASE,
        out.commits.len() as u64,
        Some(out.commits.len() as u64),
    );
    Ok((out, view))
}

/// The descriptor frame at offset 0, decoded without a commit to bind it.
/// Only for deciding whether to look for parity; every use of the
/// descriptor's content still goes through the commit-bound read.
pub fn read_descriptor_raw(src: &dyn ReadStorage, opts: &ReadOptions) -> Option<Descriptor> {
    let r = StorageReader::new(src).ok()?;
    let span = walk_frame(&r, 0, &opts.limits).ok()?;
    let mut buf = vec![0u8; usize::try_from(span.len).ok()?];
    src.read_exact_at(0, &mut buf).ok()?;
    Descriptor::from_stored_with(&buf, 0, &opts.limits, &opts.cbor, opts.drafts).ok()
}
