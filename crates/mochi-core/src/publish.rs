//! Commit and single-file publication (spec §12.2; plan C5).
//!
//! # Reading the head
//!
//! [`locate_head`] finds the latest commit a reader may accept: the footer at
//! EOF if it validates (§8.4), otherwise the last valid footer found by a
//! forward structural scan, because "a previous valid footer may lie before
//! an incomplete tail" (§12.2). Bytes after that footer are the *tail*;
//! [`TailState`] says whether they are provably uncommitted. [`open_head`]
//! then follows footer → commit record → archive descriptor, delta manifest,
//! and catalog image, **verifying each referenced object's stored-object hash
//! before parsing it**; the image's binary envelope is then bound to the
//! commit's identity (D11) before SQLite sees the image. For the catalog image that ordering is load-bearing: SQLite
//! has no page checksums, so a flipped bit inside a stored value yields a
//! different, fully valid catalog (found in C3); only the hash catches it.
//!
//! Choosing an earlier footer is never silent: [`HeadLocation`] reports how
//! the head was found and what the tail holds, so verification (C7) can
//! report freshness honestly. A reader never falls back past a head that
//! validates but fails to open; that is the repair ladder's job (C8, §22).
//!
//! # Writing
//!
//! [`ArchiveWriter`] follows the §12.2 steps:
//!
//! | Step | Here |
//! |---|---|
//! | 1 lock | [`ArchiveWriter::create`] / [`ArchiveWriter::open_append`] take the exclusive lock and hold it for the session |
//! | 2 head + tail | `open_append` locates and fully opens the head; an uncommitted tail is refused, or truncated only on explicit request with an audit record ([`TailPolicy`]); each commit re-checks the file length |
//! | 3 content | the archive descriptor at offset 0 (first commit only, D12), then data objects, one Zstandard frame per chunk |
//! | 4 recovery + metadata | delta manifest, snapshot manifest, then the catalog image in its binary envelope ([`crate::image`]) |
//! | 4b adopt | D10.7: re-read the snapshot manifest and the image from storage, hash-verify, decode, and compare each with the source state; on a mismatch roll back, no new head ([`ErrorCode::CheckpointMismatch`]) |
//! | 5 commit | the commit record (schema 1) |
//! | 6 persist | `sync_data` |
//! | 7 footer | appended directly after the commit frame |
//! | 8 persist | `sync_data` |
//! | 9 directory | `sync_directory`, only for the commit that created the file |
//! | 10 report | [`CommitOutcome`] with [`CommitStatus::LocalCommitted`] |
//!
//! Before step 7 nothing is published: on an error or cancellation the
//! writer truncates back to the previous committed length (it holds the
//! lock, and it never wrote a footer, so the bytes are provably its own and
//! uncommitted) and records that in its audit log. From step 7 on, a failure
//! is [`ErrorCode::CommitUnconfirmed`]: the footer may or may not be durable,
//! and only reopening tells. Any sync failure stops the writer
//! ([`ErrorCode::WriterPoisoned`]): after a failed `fsync` the state of
//! unsynced pages is unknown (see `storage::os`).
//!
//! # Choices made here, and what is not here yet
//!
//! * **Production writes every commit as a checkpoint** (commit schema 1,
//!   key 5 form 0): delta manifest, snapshot manifest, and catalog image.
//!   Annex B.2 D10 makes commit 0 a checkpoint and lets later commits be
//!   deltas on a base; the trigger that decides when (B.2.3: Δ ≥
//!   α·max(*B*, *F*)) is plan T14. Until then [`CheckpointPolicy`] stays
//!   `EveryCommit` in production, and this writer is the "C5 full-checkpoint
//!   writer" that G2 uses as the replay oracle. Tests select `Never` or
//!   `Every(n)` through the `test-controls` feature (review decision 14); a
//!   delta commit carries only its delta manifest and the commit record,
//!   and names the base the D10.6 rule derives.
//! * **Opening a delta commit** (T11, T12) walks authenticated parent links
//!   to its base checkpoint *b* ([`crate::segment::walk_segment`]), opens
//!   *b*'s image as for a checkpoint, and replays delta manifests *b*+1 …
//!   *h* through [`crate::catalog::SegmentApplier`], checking each one's
//!   parent link against the preceding commit's key 6 (checklist Q22). It
//!   never searches for another checkpoint and never returns an earlier
//!   state on failure. A reader's replayed catalog is made query-only.
//! * **Physical order** of a checkpoint's objects is delta manifest,
//!   snapshot manifest, image (spec §8.1: recovery records, then metadata;
//!   "physical ordering MAY vary"). Not a wire rule; readers use the refs.
//! * **The snapshot manifest is not read when opening, unless the image's
//!   stored bytes are damaged** (D10.9, review decisions Q31, Q32). A damaged
//!   snapshot with an intact image leaves reads working (recoverability
//!   `DEGRADED`, [`crate::damage`]). A reader whose base image fails with
//!   [`is_stored_damage`] rebuilds the catalog from the same commit's
//!   snapshot manifest, hash-verified and identity-bound, without SQLite
//!   ([`CatalogSource::SnapshotManifest`]); it never uses an earlier
//!   checkpoint, and any other failure (an invalid record, an unsupported
//!   envelope, a limit, I/O) is refused as before. A checkpoint head
//!   likewise tolerates stored damage to its own delta manifest, which no
//!   read needs ([`OpenedHead::manifest_error`]). [`read_snapshot`] reads
//!   the snapshot on demand. Appending does read it, for promised
//!   attributes (below), and refuses on any failure rather than falling
//!   back (Q34).
//! * **Promised attributes** are not in the catalog until C6, so the image
//!   cannot carry them, but a snapshot must (D10.3). The writer keeps them
//!   per reachable version. On `open_append` it takes them from the base
//!   checkpoint's snapshot manifest S(*b*) plus the versions the segment's
//!   deltas introduce, pruned to what is reachable at the head (review
//!   decision 21; for a checkpoint head, *b* is the head). It refuses to
//!   append if S(*b*) cannot be read (it could not write a complete next
//!   snapshot); reads are unaffected (D10.9).
//! * **Creation** writes the descriptor as part of the first commit, into
//!   the storage it was given. The D13 mechanism (exclusive temporary file,
//!   publish without replacing) is T19–T22.
//! * Records larger than the default limits are refused, never split
//!   (D10.11): `CAPACITY_EXCEEDED`, before the footer, so the previous head
//!   stays.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::codec::{EncodeParams, Protection};
use mochi_format::digest::{file_content_hash, stored_object_hash, CommitId, StoredObjectHash};
use mochi_format::envelope::RecordIdentity;
use mochi_format::error::FormatError;
use mochi_format::footer::{encode_footer_frame, validate_footer, validate_footer_at_eof};
use mochi_format::footer::{ValidatedFooter, FOOTER_FRAME_LEN};
use mochi_format::frame::{walk_frame, FrameDetail, Frames};
use mochi_format::registry::{self, FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::{DecodedBytes, DecodedSlice, StoredObject};
use mochi_format::Limits;
use serde::Serialize;

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp, Snapshot};
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, CatalogLimits, Commit, FileVersion, SegmentApplier};
use crate::commit::{uuid_v4, CommitLink, CommitParent, CommitRecord, Metadata, ObjectRef};
use crate::descriptor::Descriptor;
use crate::error::{ErrorCode, MochiError, Result};
use crate::image::{decode_image_record, encode_image_record};
use crate::job::JobContext;
use crate::manifest::{
    Attributes, ChunkEntry, FileVersionEntry, Manifest, ManifestKind, Mtime, ParentLink,
};
use crate::object::{build_object, ArchiveId, IdSource};
use crate::recovery::{
    catalog_from_snapshot, recover_from_manifests, ManifestRecovery, RecoveryScope,
};
use crate::segment::{check_delta_parent_link, walk_segment, SegmentInfo};
use crate::state::AuthoritativeState;
use crate::storage::{DirectoryDurability, ReadStorage, Storage, StorageError, StorageReader};

pub use crate::catalog::META_ARCHIVE_ID;
/// `archive_meta` key holding the writer parameters recorded at creation
/// (spec §13: chunking parameters must be recorded per archive). Canonical
/// CBOR `{0: 0 (fixed-size chunking), 1: max chunk bytes, 2: zstd level}`.
pub const META_WRITER_PARAMS: &str = "writer_params";

/// Product default chunk size: 8 MiB, the low end of the interactive preset
/// spec §13 permits. An implementation default, not a format default.
pub const DEFAULT_CHUNK_SIZE: u64 = 8 << 20;

// ---- options -------------------------------------------------------------------

/// Limits for reading untrusted archives (spec §8.5).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOptions {
    pub limits: Limits,
    pub catalog: CatalogLimits,
    pub cbor: CborLimits,
}

/// Writer configuration. `None` parameters mean "the archive's recorded
/// value" when appending, and the product default when creating. A value
/// that differs from what the archive recorded at creation is refused rather
/// than silently ignored or silently changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct WriterOptions {
    pub read: ReadOptions,
    pub chunk_size: Option<u64>,
    pub zstd_level: Option<i32>,
    /// Record the system time in each commit (informational, §12.1).
    /// Ignored when a transaction carries an explicit time.
    pub record_time: bool,
}

/// Which commits the writer makes checkpoints (Annex B.2 D10).
///
/// **Production writes [`CheckpointPolicy::EveryCommit`] until the T14
/// trigger exists** (B.2.3: Δ ≥ α·max(*B*, *F*)). The other policies exist
/// to produce delta commits for replay tests (review decision 14) and can
/// only be selected through [`ArchiveWriter::set_checkpoint_policy`], which
/// is compiled in with the non-default `test-controls` feature. Cargo
/// features are additive, so that feature is not an isolation boundary.
///
/// Whatever the policy, commit 0 is a checkpoint (D10), and a delta's base
/// is derived by the base rule: the parent if the parent is a checkpoint,
/// else the parent's base (D10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckpointPolicy {
    /// Every commit is a checkpoint (the C5 writer; the replay oracle).
    #[default]
    EveryCommit,
    /// Commit 0 is a checkpoint; every later commit is a delta on it.
    Never,
    /// Commit *s* is a checkpoint iff *s* mod *n* = 0. A test schedule,
    /// not the T14 trigger. `Every(0)` is refused; `Every(1)` behaves as
    /// `EveryCommit`.
    Every(u64),
}

impl CheckpointPolicy {
    #[cfg(any(test, feature = "test-controls"))]
    fn validate(self) -> Result<Self> {
        if self == CheckpointPolicy::Every(0) {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "checkpoint policy Every(0) has no meaning; use Never or Every(n) with n >= 1",
            ));
        }
        Ok(self)
    }

    /// Whether commit `seq` is written as a checkpoint. Commit 0 always is.
    fn is_checkpoint(self, seq: u64) -> bool {
        seq == 0
            || match self {
                CheckpointPolicy::EveryCommit => true,
                CheckpointPolicy::Never => false,
                // Every(0) is refused by `validate`; treat it as EveryCommit
                // rather than divide by zero should it ever get here.
                CheckpointPolicy::Every(n) => n == 0 || seq.is_multiple_of(n),
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WriterParams {
    chunk_size: u64,
    zstd_level: i32,
}

impl WriterParams {
    fn encode(&self) -> Result<Vec<u8>> {
        Ok(cbor::encode(&Value::Map(vec![
            (0, Value::Uint(0)),
            (1, Value::Uint(self.chunk_size)),
            (2, crate::manifest::int_value(i64::from(self.zstd_level))),
        ]))?)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let v = cbor::decode(bytes, &CborLimits::default())?;
        let mut f = cbor::Fields::of(&v, "writer parameters")?;
        if f.req(0)?.uint("chunking kind")? != 0 {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "unknown chunking kind in archive writer parameters",
            ));
        }
        let chunk_size = f.req(1)?.uint("chunk size")?;
        let level = crate::manifest::decode_int(f.req(2)?, "zstd level")?;
        f.finish()?;
        Ok(WriterParams {
            chunk_size,
            zstd_level: i32::try_from(level)
                .map_err(|_| MochiError::new(ErrorCode::CatalogInvalid, "zstd level range"))?,
        })
    }

    fn validate(&self, limits: &Limits) -> Result<()> {
        if self.chunk_size == 0 || self.chunk_size > limits.max_decoded_object_len {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "chunk size {} must be between 1 and the decoded-object limit {}",
                    self.chunk_size, limits.max_decoded_object_len
                ),
            ));
        }
        Ok(())
    }
}

// ---- locating the head -----------------------------------------------------------

/// How the head footer was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadSource {
    /// The footer at physical EOF validated: the clean case.
    Eof,
    /// EOF did not end in a valid footer; the latest valid footer was found
    /// by a forward scan (§12.2). Freshness cannot be assumed.
    Scan,
    /// The caller named this footer (a historical commit, C6).
    Explicit,
}

/// What lies after the last valid commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailState {
    Clean,
    /// Provably uncommitted (an interrupted write): complete frames other
    /// than footers, then at most one frame cut short by end of file, and no
    /// footer pattern anywhere in its bytes.
    Uncommitted {
        len: u64,
        frames: Vec<FrameKind>,
        incomplete_final_frame: bool,
    },
    /// Could hold a damaged later commit; never truncated automatically.
    Unresolved {
        len: u64,
        reason: String,
    },
}

impl TailState {
    /// Bytes after the last valid commit.
    pub fn byte_len(&self) -> u64 {
        match self {
            TailState::Clean => 0,
            TailState::Uncommitted { len, .. } | TailState::Unresolved { len, .. } => *len,
        }
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, TailState::Clean)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadLocation {
    pub footer: ValidatedFooter,
    /// End of the head footer: the committed prefix.
    pub committed_len: u64,
    pub source: HeadSource,
    pub tail: TailState,
}

fn reader(src: &dyn ReadStorage) -> Result<StorageReader<'_>> {
    Ok(StorageReader::new(src)?)
}

/// A footer is a head candidate only if it names a commit-record frame.
fn footer_names_commit(f: &ValidatedFooter) -> bool {
    f.commit_kind == FrameKind::CommitRecord
}

/// Find the latest committed footer (spec §8.4, §12.2).
pub fn locate_head(src: &dyn ReadStorage, limits: &Limits) -> Result<HeadLocation> {
    let r = reader(src)?;
    let size = mochi_format::ReadAt::len(&r);
    if let Ok(f) = validate_footer_at_eof(&r, limits) {
        if footer_names_commit(&f) {
            return Ok(HeadLocation {
                footer: f,
                committed_len: size,
                source: HeadSource::Eof,
                tail: TailState::Clean,
            });
        }
    }
    // Forward scan. A footer is a 72-byte skippable frame of kind
    // COMMIT_FOOTER; only one that fully validates counts. The scan stops at
    // the first bytes that are not a frame: offsets after that are unknown.
    let mut best: Option<ValidatedFooter> = None;
    for span in Frames::new(&r, 0, *limits) {
        let Ok(span) = span else { break };
        if span.kind == FrameKind::CommitFooter && span.len == FOOTER_FRAME_LEN {
            match validate_footer(&r, span.offset, limits) {
                Ok(f) if footer_names_commit(&f) => best = Some(f),
                Ok(_) => {}
                Err(FormatError::Source(msg)) => {
                    return Err(MochiError::new(ErrorCode::IoError, msg));
                }
                Err(_) => {}
            }
        }
    }
    let Some(footer) = best else {
        return Err(MochiError::new(
            ErrorCode::NoValidHead,
            if size == 0 {
                "the archive is empty: nothing has been committed".to_string()
            } else {
                format!("no valid committed footer in {size} bytes")
            },
        ));
    };
    let committed_len = footer.footer_offset + FOOTER_FRAME_LEN;
    let tail = classify_tail(&r, committed_len, limits)?;
    Ok(HeadLocation {
        footer,
        committed_len,
        source: HeadSource::Scan,
        tail,
    })
}

/// Decide whether `src[from..]` is provably uncommitted.
///
/// Conservative by design, because a wrong "uncommitted" verdict licenses
/// destroying a commit. A crash under the §12.2 protocol leaves complete
/// frames followed by at most one frame cut short at EOF, and never a
/// *complete* footer (the footer is written last, after a sync). So the tail
/// is `Uncommitted` only if:
///
/// (a) it walks as complete non-footer frames, optionally ending in one frame
///     that runs past EOF; and
/// (b) no *complete* footer could be hiding in it: neither the footer's
///     skippable header nor its payload magic occurs at any byte position
///     from which a whole 72-byte footer would still fit before EOF.
///
/// Rule (b) catches a later, damaged commit whose footer the walk cannot
/// reach (for example after a bit flip in an earlier frame's length field).
/// A pattern cut off by EOF is exempt: it can only be the torn final write,
/// because a published footer is complete. Content that happens to contain
/// the pattern only causes a refusal, which is safe. Zero-filled regions
/// (lost writes) are not frames, so they are `Unresolved` too: safe, if
/// conservative.
fn classify_tail(r: &StorageReader<'_>, from: u64, limits: &Limits) -> Result<TailState> {
    let size = mochi_format::ReadAt::len(r);
    let len = size.saturating_sub(from);
    if len == 0 {
        return Ok(TailState::Clean);
    }
    let unresolved = |reason: String| Ok(TailState::Unresolved { len, reason });

    let mut header = [0u8; SKIPPABLE_HEADER_LEN];
    header[..4].copy_from_slice(&registry::COMMIT_FOOTER.to_le_bytes());
    header[4..].copy_from_slice(&(registry::FOOTER_PAYLOAD_LEN as u32).to_le_bytes());
    // A footer starting at p is complete iff p + 72 <= size; its payload
    // magic sits at p + 8.
    let last_footer_start = size.checked_sub(FOOTER_FRAME_LEN);
    if let Some(last) = last_footer_start.filter(|l| *l >= from) {
        if let Some(at) = find_pattern(r, from, last + header.len() as u64, &header)? {
            return unresolved(format!(
                "a footer header appears at offset {at}, after the last valid commit; \
                 it may belong to a damaged later commit"
            ));
        }
    }
    let magic_from = from.max(SKIPPABLE_HEADER_LEN as u64);
    if let Some(last) = last_footer_start.map(|l| l + SKIPPABLE_HEADER_LEN as u64) {
        if last >= magic_from {
            let end = last + registry::FOOTER_MAGIC.len() as u64;
            if let Some(at) = find_pattern(r, magic_from, end, &registry::FOOTER_MAGIC)? {
                return unresolved(format!(
                    "footer payload magic appears at offset {at}, after the last valid commit"
                ));
            }
        }
    }

    let mut frames = Vec::new();
    let mut walker = Frames::new(r, from, *limits);
    for span in &mut walker {
        match span {
            Ok(span) if span.kind == FrameKind::CommitFooter => {
                return unresolved(format!(
                    "a footer frame at offset {} follows the last valid commit",
                    span.offset
                ));
            }
            Ok(span) => frames.push(span.kind),
            Err(FormatError::Truncated { .. }) => {
                return Ok(TailState::Uncommitted {
                    len,
                    frames,
                    incomplete_final_frame: true,
                });
            }
            Err(FormatError::Source(msg)) => {
                return Err(MochiError::new(ErrorCode::IoError, msg));
            }
            Err(e) => {
                return unresolved(format!(
                    "bytes at offset {} after the last valid commit are not a frame: {e}",
                    walker.position()
                ));
            }
        }
    }
    Ok(TailState::Uncommitted {
        len,
        frames,
        incomplete_final_frame: false,
    })
}

/// First offset `p` with `from <= p` and `p + pat.len() <= end` where `pat`
/// occurs. Streams in blocks.
fn find_pattern(r: &StorageReader<'_>, from: u64, end: u64, pat: &[u8]) -> Result<Option<u64>> {
    let end = end.min(mochi_format::ReadAt::len(r));
    let block = 64 * 1024u64;
    let overlap = pat.len().saturating_sub(1) as u64;
    let mut at = from;
    while at < end {
        let n = (end - at).min(block + overlap);
        let mut buf = vec![0u8; usize::try_from(n).unwrap_or(0)];
        mochi_format::ReadAt::read_at(r, at, &mut buf)
            .map_err(|e| MochiError::new(ErrorCode::IoError, format!("{e:?}")))?;
        if let Some(i) = buf.windows(pat.len()).position(|w| w == pat) {
            return Ok(Some(at + i as u64));
        }
        if n < block + overlap {
            break;
        }
        at += block;
    }
    Ok(None)
}

// ---- opening the head ------------------------------------------------------------

/// Read and decode the commit record named by a validated footer.
pub fn read_commit(
    src: &dyn ReadStorage,
    footer: &ValidatedFooter,
    opts: &ReadOptions,
) -> Result<(CommitRecord, CommitId)> {
    let f = &footer.fields;
    // validate_footer already bounded commit_len by max_commit_frame_len.
    let len = usize::try_from(f.commit_len).map_err(|_| {
        MochiError::new(
            ErrorCode::LimitExceeded,
            "commit frame does not fit in memory",
        )
    })?;
    let mut buf = vec![0u8; len];
    src.read_exact_at(f.commit_offset, &mut buf)?;
    let (record, id) = CommitRecord::from_stored(&buf, &opts.limits, &opts.cbor)?;
    if record.seq != f.commit_sequence {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            format!(
                "footer names commit sequence {} but the commit record says {}",
                f.commit_sequence, record.seq
            ),
        ));
    }
    Ok((record, id))
}

/// Load a referenced object and check its stored-object hash **before**
/// anything parses it. The range must end at or before `limit` (the commit
/// frame's offset: a commit references only bytes its footer publishes).
fn load_verified(
    src: &dyn ReadStorage,
    r: &ObjectRef,
    limit: u64,
    max_len: u64,
    what: &str,
) -> Result<StoredObject> {
    if r.end()? > limit {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            format!("{what} reference extends past the commit frame it belongs to"),
        ));
    }
    if r.stored_len > max_len {
        return Err(MochiError::new(
            ErrorCode::LimitExceeded,
            format!(
                "{what} of {} bytes exceeds the limit {max_len}",
                r.stored_len
            ),
        ));
    }
    let len = usize::try_from(r.stored_len)
        .map_err(|_| MochiError::new(ErrorCode::LimitExceeded, "object does not fit in memory"))?;
    let mut buf = vec![0u8; len];
    src.read_exact_at(r.offset, &mut buf)?;
    let stored = StoredObject::from_loaded(buf);
    if stored_object_hash(stored.view()) != r.stored_hash {
        return Err(MochiError::new(
            ErrorCode::StoredIntegrityFailed,
            format!("{what} at offset {}: stored-object hash mismatch", r.offset),
        ));
    }
    Ok(stored)
}

/// The payload of a stored object that must be exactly one skippable frame
/// of `kind`.
pub(crate) fn skippable_payload<'a>(
    stored: &'a StoredObject,
    kind: FrameKind,
    limits: &Limits,
) -> Result<&'a [u8]> {
    let bytes = stored.as_bytes();
    let span = walk_frame(bytes, 0, limits)?;
    match span.detail {
        FrameDetail::Skippable { .. } if span.kind == kind && span.len == bytes.len() as u64 => {
            bytes.get(SKIPPABLE_HEADER_LEN..).ok_or_else(|| {
                MochiError::new(ErrorCode::MalformedFrame, "skippable payload out of range")
            })
        }
        _ => Err(MochiError::new(
            ErrorCode::MalformedFrame,
            format!("expected exactly one {kind:?} frame"),
        )),
    }
}

/// The only failure treated as damage to stored bytes (review decisions
/// Q31, Q32). Every object is hash-checked before it is parsed, so any change
/// to its stored bytes (a flipped bit, zeroed bytes, a deleted payload) is
/// this code, before any decoder runs. An object that passes its hash and
/// then fails to decode or validate carries exactly the bytes its commit was
/// published with: an invalid record, which D10.4 refuses, not damage that a
/// twin representation may stand in for. `IO_ERROR` and `OUT_OF_BOUNDS` are
/// operational; `LIMIT_EXCEEDED` and `UNSUPPORTED_FEATURE` are "cannot", not
/// "damaged".
pub fn is_stored_damage(e: &MochiError) -> bool {
    e.code == ErrorCode::StoredIntegrityFailed
}

/// Where an opened commit's catalog came from (D10.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogSource {
    /// The base checkpoint's catalog image: the normal path.
    Image,
    /// Rebuilt from the base checkpoint's snapshot manifest because the
    /// image's stored bytes are damaged. The same commit's state, never an
    /// earlier one.
    SnapshotManifest { image_error: MochiError },
}

/// A fully verified head: its commit, descriptor, delta manifest, and
/// catalog. The snapshot manifest of a checkpoint is not loaded (see
/// [`read_snapshot`]).
#[derive(Debug)]
pub struct OpenedHead {
    pub location: HeadLocation,
    pub commit: CommitRecord,
    pub commit_id: CommitId,
    pub descriptor: Descriptor,
    /// This commit's delta manifest (commit key 6). `None` only for a
    /// checkpoint opened for reading whose delta manifest's stored bytes are
    /// damaged (review decision Q31, D10.9: nothing reads it, so the
    /// commit's reads are unaffected). A delta head always has it: it is in
    /// its own replay segment.
    pub manifest: Option<Manifest>,
    /// Why `manifest` is `None`.
    pub manifest_error: Option<MochiError>,
    /// The catalog at this commit: the checkpoint's image, or for a delta
    /// commit its base checkpoint's image with the segment's delta
    /// manifests replayed onto it (D10.4); if the image's stored bytes are
    /// damaged, the same commit's snapshot manifest stands in for it
    /// ([`CatalogSource`]).
    pub catalog: Catalog,
    /// Where `catalog`'s base came from.
    pub catalog_source: CatalogSource,
    /// The replay segment this commit was opened through. For a checkpoint
    /// it is the commit itself. `base_hint_mismatch` is a diagnostic only
    /// (review decision 17).
    pub segment: SegmentInfo,
}

impl OpenedHead {
    pub fn seq(&self) -> u64 {
        self.commit.seq
    }
}

/// Locate and fully open the head (footer → commit → manifest → catalog).
pub fn open_head(src: &dyn ReadStorage, opts: &ReadOptions) -> Result<OpenedHead> {
    let location = locate_head(src, &opts.limits)?;
    Ok(open_at(src, location, opts, OpenMode::Read)?.head)
}

/// Open the commit whose footer is at `footer_offset` (a historical commit,
/// for example one listed by [`commit_history`]). Same checks as
/// [`open_head`]; the tail is everything after this footer and is not
/// classified, because later commits are expected there.
pub fn open_at_footer(
    src: &dyn ReadStorage,
    footer_offset: u64,
    opts: &ReadOptions,
) -> Result<OpenedHead> {
    let r = reader(src)?;
    let footer = validate_footer(&r, footer_offset, &opts.limits)?;
    if !footer_names_commit(&footer) {
        return Err(MochiError::new(
            ErrorCode::FooterInvalid,
            "the footer does not name a commit record",
        ));
    }
    let location = HeadLocation {
        footer,
        committed_len: footer_offset + FOOTER_FRAME_LEN,
        source: HeadSource::Explicit,
        tail: TailState::Clean,
    };
    Ok(open_at(src, location, opts, OpenMode::Read)?.head)
}

/// D12: a descriptor that cannot be loaded, hash-verified, or decoded, or
/// that names another archive, is `DESCRIPTOR_INVALID`. Refusals
/// (`UNSUPPORTED_FEATURE`), reader limits, and I/O keep their codes.
fn read_descriptor(
    src: &dyn ReadStorage,
    commit: &CommitRecord,
    limit: u64,
    opts: &ReadOptions,
) -> Result<Descriptor> {
    let as_invalid = |e: MochiError| match e.code {
        ErrorCode::UnsupportedFeature
        | ErrorCode::LimitExceeded
        | ErrorCode::IoError
        | ErrorCode::DescriptorInvalid => e,
        _ => MochiError::new(
            ErrorCode::DescriptorInvalid,
            format!("archive descriptor: {}", e.message),
        ),
    };
    let stored = load_verified(
        src,
        &commit.descriptor,
        limit,
        opts.limits.max_frame_len,
        "archive descriptor",
    )
    .map_err(as_invalid)?;
    let d = Descriptor::from_stored(
        stored.as_bytes(),
        commit.descriptor.offset,
        &opts.limits,
        &opts.cbor,
    )?;
    d.check_archive_id(&commit.archive_id)?;
    Ok(d)
}

/// Load, hash-verify, and decode a manifest a commit references, and check
/// that it is of `kind` and carries the commit's D11 identity
/// (`ENVELOPE_INVALID` on a mismatch, the same fault codes as the binary
/// envelope).
pub(crate) fn read_bound_manifest(
    src: &dyn ReadStorage,
    commit: &CommitRecord,
    r: &ObjectRef,
    limit: u64,
    kind: ManifestKind,
    opts: &ReadOptions,
) -> Result<Manifest> {
    let what = match kind {
        ManifestKind::Delta => "delta manifest",
        ManifestKind::Snapshot => "snapshot manifest",
    };
    let stored = load_verified(src, r, limit, opts.limits.max_frame_len, what)?;
    let (manifest, _) = Manifest::from_stored(&stored, &opts.limits, &opts.cbor)?;
    if manifest.kind != kind {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            format!("the commit's {what} reference names a manifest of the other kind"),
        ));
    }
    manifest.identity().check(&commit.identity())?;
    if kind == ManifestKind::Delta && manifest.parent.map(|p| p.seq) != commit.parent.map(|p| p.seq)
    {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "the delta manifest's parent does not match the commit's parent",
        ));
    }
    Ok(manifest)
}

/// The snapshot manifest of a checkpoint commit, hash-verified against the
/// commit's reference and bound to it by identity (D10.2, D11). Not part of
/// opening (D10.9: reads survive a damaged snapshot); baseline recovery
/// (T16), adoption checks (T15), and appending use it.
pub fn read_snapshot(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    opts: &ReadOptions,
) -> Result<Manifest> {
    let Metadata::Checkpoint { snapshot, .. } = head.commit.metadata else {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "this commit is not a checkpoint and has no snapshot manifest",
        ));
    };
    read_bound_manifest(
        src,
        &head.commit,
        &snapshot,
        head.location.footer.fields.commit_offset,
        ManifestKind::Snapshot,
        opts,
    )
}

/// Why a commit is being opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// A reader: the catalog is query-only, and the snapshot manifest is not
    /// read (D10.9).
    Read,
    /// The writer: the catalog stays writable, and the promised attributes
    /// of every reachable version are reconstructed (review decision 21).
    Append,
}

/// What [`replay_segment`] produces: the catalog at the head, the segment
/// it was opened through, and (append only) the reconstructed attributes.
type Replayed = (
    Catalog,
    SegmentInfo,
    Option<BTreeMap<FileVersionId, Attributes>>,
    CatalogSource,
);

struct Opened {
    head: OpenedHead,
    /// `Some` exactly in [`OpenMode::Append`].
    attributes: Option<BTreeMap<FileVersionId, Attributes>>,
}

fn open_at(
    src: &dyn ReadStorage,
    location: HeadLocation,
    opts: &ReadOptions,
    mode: OpenMode,
) -> Result<Opened> {
    let (commit, commit_id) = read_commit(src, &location.footer, opts)?;
    let limit = location.footer.fields.commit_offset;

    // D12: interpretation needs a valid descriptor bound to this commit.
    let descriptor = read_descriptor(src, &commit, limit, opts)?;

    // Q31: a checkpoint opened for reading does not need its own delta
    // manifest (the snapshot's base is the checkpoint itself, D10.9), so
    // damage to its stored bytes is recorded, not fatal. Any other failure
    // (an invalid record, a limit, I/O) is refused as before, and so is every
    // failure on a delta head or in append mode.
    let (manifest, manifest_error) = match read_bound_manifest(
        src,
        &commit,
        &commit.delta_manifest,
        limit,
        ManifestKind::Delta,
        opts,
    ) {
        Ok(m) => (Some(m), None),
        Err(e)
            if mode == OpenMode::Read
                && commit.metadata.is_checkpoint()
                && is_stored_damage(&e) =>
        {
            (None, Some(e))
        }
        Err(e) => return Err(e),
    };

    let head_entry = HistoryEntry {
        footer_offset: location.footer.footer_offset,
        commit_offset: limit,
        commit: commit.clone(),
        commit_id,
    };
    let (catalog, segment, attributes, catalog_source) = match commit.metadata {
        Metadata::Checkpoint { image, .. } => {
            let (catalog, source) = base_catalog(
                src,
                &head_entry,
                &image,
                opts,
                mode,
                mode == OpenMode::Append,
            )?;
            if mode == OpenMode::Read {
                // A catalog rebuilt from S(b) is writable; a reader's never is.
                catalog.make_query_only()?;
            }
            let segment = SegmentInfo {
                base_seq: commit.seq,
                base_commit_id: commit_id,
                base_footer_offset: head_entry.footer_offset,
                base_hint_mismatch: None,
            };
            let attributes = match mode {
                OpenMode::Read => None,
                OpenMode::Append => {
                    let snapshot = read_bound_manifest(
                        src,
                        &commit,
                        &checkpoint_snapshot_ref(&commit)?,
                        limit,
                        ManifestKind::Snapshot,
                        opts,
                    )
                    .map_err(append_needs_snapshot)?;
                    let map = snapshot
                        .file_versions
                        .iter()
                        .map(|v| (v.version.id, v.attributes))
                        .collect();
                    // Same completeness rule as for a delta head (decision
                    // 21, checklist Q24): refuse at open, not at the next
                    // checkpoint.
                    let reachable = catalog.replay(None)?;
                    Some(reachable_attributes(&reachable, map).map_err(attributes_incomplete)?)
                }
            };
            (catalog, segment, attributes, source)
        }
        Metadata::Delta { .. } => {
            // A delta head's own delta manifest is in its segment, so the
            // tolerance above never applies to it.
            let own = manifest.as_ref().ok_or_else(|| {
                MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: a delta head without its delta manifest",
                )
            })?;
            replay_segment(src, head_entry, own, opts, mode)?
        }
    };

    if catalog.head_commit()? != Some(commit.seq) {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "the catalog does not materialize the commit being opened (§10.6)",
        ));
    }
    Ok(Opened {
        head: OpenedHead {
            location,
            commit,
            commit_id,
            descriptor,
            manifest,
            manifest_error,
            catalog,
            catalog_source,
            segment,
        },
        attributes,
    })
}

pub(crate) fn checkpoint_snapshot_ref(commit: &CommitRecord) -> Result<ObjectRef> {
    match commit.metadata {
        Metadata::Checkpoint { snapshot, .. } => Ok(snapshot),
        Metadata::Delta { .. } => Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "this commit is not a checkpoint and has no snapshot manifest",
        )),
    }
}

fn append_needs_snapshot(e: MochiError) -> MochiError {
    MochiError::new(
        e.code,
        format!(
            "cannot append: the base checkpoint's snapshot manifest is needed for promised \
             attributes and could not be read ({})",
            e.message
        ),
    )
}

/// Checklist Q24 (code provisional): a version reachable at the head has no
/// promised attributes in S(*b*) or the segment's deltas.
fn attributes_incomplete(e: MochiError) -> MochiError {
    MochiError::new(
        e.code,
        format!(
            "cannot append: promised attributes could not be reconstructed from the base \
             snapshot and the segment's deltas ({})",
            e.message
        ),
    )
}

/// Checkpoint `cp`'s catalog image: stored hash first, then the binary
/// envelope bound to `cp` (D11), and only then SQLite (plan C5). The catalog
/// must materialize `cp` and belong to its archive. `writable` selects a
/// catalog that accepts writes (replay and append) over a query-only one.
pub(crate) fn check_image(
    src: &dyn ReadStorage,
    cp: &HistoryEntry,
    image_ref: &ObjectRef,
    opts: &ReadOptions,
    writable: bool,
) -> Result<Catalog> {
    let stored = load_verified(
        src,
        image_ref,
        cp.commit_offset,
        opts.limits.max_frame_len,
        "catalog checkpoint",
    )?;
    let image = decode_image_record(&stored, &cp.commit.identity(), &opts.limits)?;
    let catalog = if writable {
        Catalog::open_image_writable(image, &opts.catalog)?
    } else {
        Catalog::open_image(image, &opts.catalog)?
    };
    if catalog.head_commit()? != Some(cp.commit.seq) {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "the catalog checkpoint does not materialize the commit that references it (§10.6)",
        ));
    }
    if catalog.meta(META_ARCHIVE_ID)?.as_deref() != Some(&cp.commit.archive_id.as_bytes()[..]) {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "the catalog checkpoint belongs to a different archive",
        ));
    }
    Ok(catalog)
}

/// Base checkpoint `base`'s catalog (D10.9).
///
/// * The image is used when it is intact: `(catalog, Image)`. The snapshot
///   manifest is **not** read.
/// * A reader ([`OpenMode::Read`]) whose image fails with stored damage
///   ([`is_stored_damage`]) rebuilds the catalog from the same commit's
///   snapshot manifest, hash-verified and identity-bound, without SQLite
///   ([`catalog_from_snapshot`]). If that fails too, the image's error is
///   returned with the snapshot's failure named. Never an earlier
///   checkpoint (D10.4, D10.6).
/// * Any other image failure, and every failure when appending, is
///   returned as it is (review decision Q34): the writer needs the image's
///   writer parameters, and repair is plan-then-apply (§22.2).
///
/// `writable` as for [`check_image`]. A catalog rebuilt from the snapshot is
/// always writable; a reader makes it query-only afterwards.
fn base_catalog(
    src: &dyn ReadStorage,
    base: &HistoryEntry,
    image_ref: &ObjectRef,
    opts: &ReadOptions,
    mode: OpenMode,
    writable: bool,
) -> Result<(Catalog, CatalogSource)> {
    match check_image(src, base, image_ref, opts, writable) {
        Ok(c) => Ok((c, CatalogSource::Image)),
        Err(image_error) if mode == OpenMode::Read && is_stored_damage(&image_error) => {
            let rebuilt = read_bound_manifest(
                src,
                &base.commit,
                &checkpoint_snapshot_ref(&base.commit)?,
                base.commit_offset,
                ManifestKind::Snapshot,
                opts,
            )
            .and_then(|s_b| catalog_from_snapshot(&s_b));
            match rebuilt {
                Ok(c) => Ok((c, CatalogSource::SnapshotManifest { image_error })),
                Err(snapshot_error) => Err(MochiError::new(
                    image_error.code,
                    format!(
                        "{}; the snapshot manifest could not stand in for it: {}",
                        image_error.message, snapshot_error.message
                    ),
                )),
            }
        }
        Err(e) => Err(e),
    }
}

/// T11 + T12: open a delta commit through its segment.
///
/// 1. Walk authenticated parent links from the head to its base *b* and
///    check the segment ([`walk_segment`]: base rule for every delta, the
///    reached checkpoint's ID and kind, one descriptor). No search: only the
///    segment's footers and commit records are read.
/// 2. Open *b*'s image as for a checkpoint head.
/// 3. For *j* = *b*+1 … *h*: load delta manifest *j* (stored hash first,
///    then decode, then D11 identity binding to commit *j*), check its
///    parent link against commit *j* − 1's key 6 (Q22), and apply it
///    atomically. The first failure is returned; no earlier state is.
///
/// In [`OpenMode::Append`], also reconstruct promised attributes from
/// S(*b*) plus the ordered deltas (review decision 21).
fn replay_segment(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    head_manifest: &Manifest,
    opts: &ReadOptions,
    mode: OpenMode,
) -> Result<Replayed> {
    let (entries, info) = walk_segment(src, head, opts)?;
    let (Some(base), Some(last)) = (entries.first(), entries.last()) else {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "internal: an empty replay segment",
        ));
    };
    let image = match base.commit.metadata {
        Metadata::Checkpoint { image, .. } => image,
        // walk_segment already refused this; never interpret a delta's
        // fields as a checkpoint's.
        Metadata::Delta { .. } => {
            return Err(MochiError::new(
                ErrorCode::RecordInvalid,
                "the segment's base is not a checkpoint (D10.6)",
            ))
        }
    };
    // Replay needs a writable connection; a reader's is made query-only
    // once replay is done.
    let (base_cat, catalog_source) = base_catalog(src, base, &image, opts, mode, true)?;
    let mut applier = SegmentApplier::new(base_cat)?;

    // Attributes, append only: S(b) first, before any delta is applied, so
    // a damaged S(b) refuses append without doing the replay work.
    let mut attributes = match mode {
        OpenMode::Read => None,
        OpenMode::Append => {
            let s_b = read_bound_manifest(
                src,
                &base.commit,
                &checkpoint_snapshot_ref(&base.commit)?,
                base.commit_offset,
                ManifestKind::Snapshot,
                opts,
            )
            .map_err(append_needs_snapshot)?;
            Some(
                s_b.file_versions
                    .iter()
                    .map(|v| (v.version.id, v.attributes))
                    .collect::<BTreeMap<FileVersionId, Attributes>>(),
            )
        }
    };

    apply_segment_deltas(
        src,
        &entries,
        Some(head_manifest),
        &mut applier,
        attributes.as_mut(),
        opts,
    )?;

    if applier.head() != last.commit.seq {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "replay did not reach the head commit",
        ));
    }
    let attributes = match attributes {
        None => None,
        Some(map) => {
            Some(reachable_attributes(applier.namespace(), map).map_err(attributes_incomplete)?)
        }
    };
    let catalog = applier.into_catalog();
    if mode == OpenMode::Read {
        catalog.make_query_only()?;
    }
    Ok((catalog, info, attributes, catalog_source))
}

/// Apply deltas *b*+1 … *h* (from `entries`, which [`walk_segment`] produced,
/// *b* first) onto `applier`, which holds *b*. For each *j*: load delta *j*
/// (stored hash, decode, D11 identity, parent sequence) unless *j* = *h* and
/// `head_manifest` is given; check its parent link against commit *j* − 1's
/// key 6 (Q22); apply it atomically; and, with `attributes`, add the versions
/// it introduces (a reintroduction is `RECORD_INVALID`, D10.4). Stops at the
/// first failure. Shared by opening ([`replay_segment`]) and baseline
/// recovery ([`recover_baseline`]), so the replay rules exist once.
fn apply_segment_deltas(
    src: &dyn ReadStorage,
    entries: &[HistoryEntry],
    head_manifest: Option<&Manifest>,
    applier: &mut SegmentApplier,
    mut attributes: Option<&mut BTreeMap<FileVersionId, Attributes>>,
    opts: &ReadOptions,
) -> Result<()> {
    let Some(last) = entries.last() else {
        return Ok(());
    };
    for pair in entries.windows(2) {
        let (prev, e) = (&pair[0], &pair[1]);
        let loaded;
        let delta = match head_manifest {
            Some(m) if e.commit.seq == last.commit.seq => m,
            _ => {
                loaded = read_bound_manifest(
                    src,
                    &e.commit,
                    &e.commit.delta_manifest,
                    e.commit_offset,
                    ManifestKind::Delta,
                    opts,
                )?;
                &loaded
            }
        };
        check_delta_parent_link(delta, &prev.commit)?;
        applier.apply(delta)?;
        if let Some(map) = attributes.as_deref_mut() {
            // Versions are immutable and introduced once (the applier has
            // just refused any reintroduction, D10.4): a delta's attributes
            // are those of the versions it introduces, nothing else.
            for v in &delta.file_versions {
                if map.insert(v.version.id, v.attributes).is_some() {
                    return Err(MochiError::new(
                        ErrorCode::RecordInvalid,
                        format!(
                            "delta manifest {} introduces a version the base snapshot already \
                             lists (D10.4)",
                            delta.commit_seq
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One commit in the published history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub footer_offset: u64,
    /// Where the footer says the commit frame starts. Objects the commit
    /// references must end at or before it.
    pub commit_offset: u64,
    pub commit: CommitRecord,
    pub commit_id: CommitId,
}

/// Walk the parent chain from the head back to commit 0, validating each
/// parent footer at its hint and checking that its commit ID and sequence are
/// the ones the child names. Cost is proportional to history length. Commit
/// records only; objects are verified by C7.
pub fn commit_history(src: &dyn ReadStorage, opts: &ReadOptions) -> Result<Vec<HistoryEntry>> {
    let loc = locate_head(src, &opts.limits)?;
    let (commit, commit_id) = read_commit(src, &loc.footer, opts)?;
    let head = HistoryEntry {
        footer_offset: loc.footer.footer_offset,
        commit_offset: loc.footer.fields.commit_offset,
        commit,
        commit_id,
    };
    walk_back(src, head, 0, opts)
}

/// Walk parent links from `head` back to sequence `down_to`, validating each
/// parent footer at its hint (a parent "not at its hint" is
/// `RECORD_INVALID`) and checking the commit ID, sequence, and archive ID
/// the child names. Returns the commits in ascending order, `down_to` first.
/// Reads only those footers and commit records (including a bad hint's
/// target, which is how it is found to be bad); never scans.
pub(crate) fn walk_back(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    down_to: u64,
    opts: &ReadOptions,
) -> Result<Vec<HistoryEntry>> {
    if down_to > head.commit.seq {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            format!(
                "cannot walk from commit {} back to the later commit {down_to}",
                head.commit.seq
            ),
        ));
    }
    let r = reader(src)?;
    let mut out = vec![head];
    loop {
        let child = &out[out.len() - 1];
        if child.commit.seq == down_to {
            break;
        }
        let Some(p) = child.commit.parent else { break };
        if p.footer_offset >= child.footer_offset {
            return Err(MochiError::new(
                ErrorCode::RecordInvalid,
                "parent footer hint does not precede the child",
            ));
        }
        let footer = validate_footer(&r, p.footer_offset, &opts.limits)?;
        let (commit, commit_id) = read_commit(src, &footer, opts)?;
        if commit_id != p.commit_id
            || commit.seq != p.seq
            || commit.archive_id != child.commit.archive_id
        {
            return Err(MochiError::new(
                ErrorCode::RecordInvalid,
                format!(
                    "commit {} names a parent that is not at its hint",
                    child.commit.seq
                ),
            ));
        }
        out.push(HistoryEntry {
            footer_offset: p.footer_offset,
            commit_offset: footer.fields.commit_offset,
            commit,
            commit_id,
        });
    }
    out.reverse();
    Ok(out)
}

/// Result of baseline recovery (Annex B.2 D10.8) for one head.
#[derive(Debug)]
pub struct BaselineRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    /// The replay segment the head belongs to; `segment.base_seq` is *b*.
    pub segment: SegmentInfo,
    /// Query-only. Materializes commits *b* … *h*: `replay(Some(s))` works for
    /// those, not for earlier commits. It was built from S(*b*), not from an
    /// image, so it carries `META_ARCHIVE_ID` but no writer parameters
    /// (review decision Q29).
    pub catalog: Catalog,
    /// Promised attributes of every version reachable at the head.
    pub attributes: BTreeMap<FileVersionId, Attributes>,
}

/// Baseline recovery for the commit whose footer is at `footer_offset`
/// (D10.8). Same footer checks as [`open_at_footer`]; the tail is not
/// classified.
pub fn recover_baseline_at_footer(
    src: &dyn ReadStorage,
    footer_offset: u64,
    opts: &ReadOptions,
) -> Result<BaselineRecovery> {
    let r = reader(src)?;
    let footer = validate_footer(&r, footer_offset, &opts.limits)?;
    if !footer_names_commit(&footer) {
        return Err(MochiError::new(
            ErrorCode::FooterInvalid,
            "the footer does not name a commit record",
        ));
    }
    let (commit, commit_id) = read_commit(src, &footer, opts)?;
    let head = HistoryEntry {
        footer_offset: footer.footer_offset,
        commit_offset: footer.fields.commit_offset,
        commit,
        commit_id,
    };
    recover_baseline(src, head, opts)
}

/// D10.8: recover the state of `head` from S(*b*) plus the delta manifests
/// after *b*. Needs commit *b*'s record (the first later delta's parent link
/// is checked against the delta-manifest hash it holds) and **neither SQLite,
/// nor delta manifest *b*, nor any earlier manifest**.
///
/// Reads, in order: the descriptor (D12: this is interpretation, so a bad
/// descriptor is `DESCRIPTOR_INVALID`); the footers and commit records of the
/// segment *b* … *h*; S(*b*); delta manifests *b*+1 … *h*. Any failure is
/// returned as it is: no partial result, no earlier checkpoint, no search
/// (D10.4, D10.6).
fn recover_baseline(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    opts: &ReadOptions,
) -> Result<BaselineRecovery> {
    let head_seq = head.commit.seq;
    let head_commit_id = head.commit_id;
    read_descriptor(src, &head.commit, head.commit_offset, opts)?;
    let (entries, segment) = walk_segment(src, head, opts)?;
    let (Some(base), Some(last)) = (entries.first(), entries.last()) else {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "internal: an empty replay segment",
        ));
    };
    let s_b = read_bound_manifest(
        src,
        &base.commit,
        &checkpoint_snapshot_ref(&base.commit)?,
        base.commit_offset,
        ManifestKind::Snapshot,
        opts,
    )?;
    let catalog = catalog_from_snapshot(&s_b)?;
    let mut attributes: BTreeMap<FileVersionId, Attributes> = s_b
        .file_versions
        .iter()
        .map(|v| (v.version.id, v.attributes))
        .collect();
    let mut applier = SegmentApplier::new(catalog)?;
    apply_segment_deltas(
        src,
        &entries,
        None,
        &mut applier,
        Some(&mut attributes),
        opts,
    )?;
    if applier.head() != last.commit.seq {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "replay did not reach the head commit",
        ));
    }
    // §11.1: snapshot recovery includes promised attributes, so a version
    // with none is not a partial success (same code as checklist Q24).
    let attributes = reachable_attributes(applier.namespace(), attributes).map_err(|e| {
        MochiError::new(
            e.code,
            format!(
                "cannot recover: promised attributes could not be reconstructed from the \
                 base snapshot and the segment's deltas ({})",
                e.message
            ),
        )
    })?;
    let catalog = applier.into_catalog();
    catalog.make_query_only()?;
    Ok(BaselineRecovery {
        head_seq,
        head_commit_id,
        segment,
        catalog,
        attributes,
    })
}

/// What [`recover_with_trusted_head`] found for the footer-verified head.
#[derive(Debug)]
pub struct TrustedRecovery {
    pub head_seq: u64,
    pub head_commit_id: CommitId,
    /// Manifest-chain recovery (C4/C5), anchored on the head commit's key-6
    /// hash. `Err` when it could not run at all (for example the head's delta
    /// manifest is gone, or no manifest frame could be scanned).
    pub chain: std::result::Result<ManifestRecovery, MochiError>,
    /// D10.8 baseline recovery, attempted only when `chain` does not reach
    /// the head (review decision Q30). `None`: not attempted.
    pub baseline: Option<std::result::Result<BaselineRecovery, MochiError>>,
}

impl TrustedRecovery {
    /// The §11.1 scope the metadata supports for commit `seq`: from the
    /// baseline if it covers `seq` (snapshot recovery at the head, historical
    /// below it), else from the chain, else payload salvage.
    pub fn scope_for(&self, seq: u64) -> RecoveryScope {
        if let Some(Ok(b)) = &self.baseline {
            if (b.segment.base_seq..=b.head_seq).contains(&seq) {
                return if seq == b.head_seq {
                    RecoveryScope::SnapshotRecovery
                } else {
                    RecoveryScope::HistoricalRecovery
                };
            }
        }
        match &self.chain {
            Ok(c) => c.scope_for(seq),
            Err(_) => RecoveryScope::PayloadSalvage,
        }
    }

    /// The catalog that reaches the head: the baseline's, else the chain's if
    /// its rebuilt range ends at the head.
    pub fn head_catalog(&self) -> Option<&Catalog> {
        if let Some(Ok(b)) = &self.baseline {
            return Some(&b.catalog);
        }
        match &self.chain {
            Ok(c) if c.snapshot_range.map(|(_, last)| last) == Some(self.head_seq) => {
                c.catalog.as_ref()
            }
            _ => None,
        }
    }

    /// Promised attributes at the head, from the same source as
    /// [`TrustedRecovery::head_catalog`]. (The chain's map covers every
    /// version it saw, not only the reachable ones.)
    pub fn head_attributes(&self) -> Option<&BTreeMap<FileVersionId, Attributes>> {
        if let Some(Ok(b)) = &self.baseline {
            return Some(&b.attributes);
        }
        match &self.chain {
            Ok(c) if c.snapshot_range.map(|(_, last)| last) == Some(self.head_seq) => {
                Some(&c.attributes)
            }
            _ => None,
        }
    }
}

/// Recover what the footer-verified head makes recoverable, when the catalog
/// checkpoint cannot be used (plan C5, T16).
///
/// 1. Manifest-chain recovery, trusting only the chain that ends at the
///    manifest the head commit names (closing the gap found in C4): manifests
///    that disagree with the published commit chain (substituted, forged, or
///    from another archive) are ignored. It is the only path that recovers
///    history before the head's base.
/// 2. If that chain does not reach the head, baseline recovery (D10.8):
///    S(*b*) plus the later deltas, from the head's own segment only.
///
/// A forger who rewrites the commit records and footers as well produces a
/// different, self-consistent file; only an external anchor (spec §5.7, D8)
/// detects that. This function fails only when the head itself cannot be
/// located or its commit record cannot be read.
pub fn recover_with_trusted_head(
    src: &dyn ReadStorage,
    opts: &ReadOptions,
) -> Result<TrustedRecovery> {
    let loc = locate_head(src, &opts.limits)?;
    let (commit, commit_id) = read_commit(src, &loc.footer, opts)?;
    let r = StorageReader::prefix(src, loc.committed_len)?;
    let mut found = Vec::new();
    for span in Frames::new(&r, 0, opts.limits) {
        let Ok(span) = span else { break };
        if span.kind == FrameKind::RecoveryManifest {
            let len = usize::try_from(span.len).map_err(|_| {
                MochiError::new(ErrorCode::LimitExceeded, "manifest does not fit in memory")
            })?;
            let mut buf = vec![0u8; len];
            src.read_exact_at(span.offset, &mut buf)?;
            found.push(StoredObject::from_loaded(buf));
        }
    }
    let head_seq = commit.seq;
    let head = HistoryEntry {
        footer_offset: loc.footer.footer_offset,
        commit_offset: loc.footer.fields.commit_offset,
        commit: commit.clone(),
        commit_id,
    };
    let chain = recover_from_manifests(
        &found,
        Some(commit.delta_manifest.stored_hash),
        &opts.limits,
    );
    let reached = matches!(
        &chain,
        Ok(c) if c.snapshot_range.map(|(_, last)| last) == Some(head_seq)
    );
    let baseline = (!reached).then(|| recover_baseline(src, head, opts));
    Ok(TrustedRecovery {
        head_seq,
        head_commit_id: commit_id,
        chain,
        baseline,
    })
}

// ---- writing ---------------------------------------------------------------------

/// What to do with an uncommitted tail when opening for append (§12.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPolicy {
    /// Refuse with [`ErrorCode::UncommittedTail`].
    Refuse,
    /// Truncate a *provably* uncommitted tail and return an audit record. An
    /// unresolved tail is still refused ([`ErrorCode::TailUnresolved`]).
    TruncateUncommitted,
}

/// Audit record of an explicit tail truncation (§12.2: "an auditable
/// recovery action"). Spec does not say where it must be kept (plan §9,
/// O28); the caller is responsible for persisting it in its report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailTruncation {
    pub committed_len: u64,
    pub removed_len: u64,
    pub removed_frames: Vec<FrameKind>,
    pub incomplete_final_frame: bool,
    pub head_seq: u64,
    pub head_commit_id: CommitId,
}

/// Entries in a writer's audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    TailTruncated(TailTruncation),
    /// The writer removed its own unpublished bytes after an error or
    /// cancellation before the footer.
    RolledBack {
        from_len: u64,
        to_len: u64,
        reason: String,
    },
}

/// §12.4 commit status. **Only `LOCAL_COMMITTED` exists in 1.0.** The
/// Preservation states (`REPLICATION_PENDING`, `PRESERVED`,
/// `PRESERVATION_DEGRADED`) need the Preservation profile, which 1.0 does not
/// ship, so they are deliberately not representable (AGENTS.md: nothing in
/// 1.0 may report `PRESERVED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CommitStatus {
    #[serde(rename = "LOCAL_COMMITTED")]
    LocalCommitted,
}

impl CommitStatus {
    pub const ALL: &'static [CommitStatus] = &[CommitStatus::LocalCommitted];

    pub const fn as_str(self) -> &'static str {
        match self {
            CommitStatus::LocalCommitted => "LOCAL_COMMITTED",
        }
    }
}

/// Durability of a publish (plan O12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishDurability {
    /// Every required sync was performed with a documented mechanism.
    Durable,
    /// File data is synced, but the new file's directory entry is not
    /// confirmed durable. Report as degraded, never as durable.
    DirectoryUnconfirmed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub status: CommitStatus,
    pub durability: PublishDurability,
    pub seq: u64,
    pub commit_id: CommitId,
    pub footer_offset: u64,
    pub committed_len: u64,
    pub objects_written: u64,
}

/// Namespace changes for one commit, applied in order.
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    entries: Vec<TxEntry>,
    time: Option<Mtime>,
}

#[derive(Debug, Clone)]
enum TxEntry {
    File {
        path: ArchivePath,
        content: Vec<u8>,
        attributes: Attributes,
    },
    Dir {
        path: ArchivePath,
        attributes: Attributes,
    },
    Delete(ArchivePath),
    Rename {
        from: ArchivePath,
        to: ArchivePath,
    },
}

impl Transaction {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create or replace a regular file. Content is held in memory for now.
    pub fn put_file(
        &mut self,
        path: ArchivePath,
        content: Vec<u8>,
        attributes: Attributes,
    ) -> &mut Self {
        self.entries.push(TxEntry::File {
            path,
            content,
            attributes,
        });
        self
    }

    /// Create or replace a directory entry (directories are explicit, C3).
    pub fn put_dir(&mut self, path: ArchivePath, attributes: Attributes) -> &mut Self {
        self.entries.push(TxEntry::Dir { path, attributes });
        self
    }

    /// Remove an entry from the new snapshot. History keeps it (§10.2).
    pub fn delete(&mut self, path: ArchivePath) -> &mut Self {
        self.entries.push(TxEntry::Delete(path));
        self
    }

    /// Delete `from` and put its head version at `to`, atomically (§10.2).
    /// Non-recursive: renaming a non-empty directory fails validation.
    pub fn rename(&mut self, from: ArchivePath, to: ArchivePath) -> &mut Self {
        self.entries.push(TxEntry::Rename { from, to });
        self
    }

    /// Record this informational time instead of the system clock.
    pub fn at(&mut self, time: Mtime) -> &mut Self {
        self.time = Some(time);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Copy)]
struct WriterHead {
    seq: u64,
    commit_id: CommitId,
    footer_offset: u64,
    committed_len: u64,
    /// The head commit's key 6: the next delta links to it.
    delta_manifest_hash: StoredObjectHash,
    /// The head commit's key 10; every later commit repeats it.
    descriptor: ObjectRef,
    /// The base a delta written next would name (D10.6 base rule): the head
    /// itself if it is a checkpoint, else the head's own base.
    next_base: CommitLink,
}

/// Bytes written before the footer, ready to publish.
struct Prepared {
    seq: u64,
    next_base: CommitLink,
    commit_offset: u64,
    commit_frame: StoredObject,
    commit_id: CommitId,
    delta_manifest_hash: StoredObjectHash,
    descriptor: ObjectRef,
    catalog: Catalog,
    attributes: BTreeMap<FileVersionId, Attributes>,
    objects: u64,
}

/// Progress phases reported by [`ArchiveWriter::commit`], in order.
pub mod phase {
    /// Reported once before any content is written (after the head catalog
    /// is copied), then after each chunk.
    pub const CONTENT: &str = "content";
    pub const CATALOG: &str = "catalog";
    pub const MANIFEST: &str = "manifest";
    /// The snapshot manifest and the catalog image.
    pub const CHECKPOINT: &str = "checkpoint";
    /// Re-reading and checking both checkpoint representations (D10.7).
    pub const ADOPT: &str = "adopt";
    pub const COMMIT_RECORD: &str = "commit-record";
    pub const SYNC_CONTENT: &str = "sync-content";
    pub const FOOTER: &str = "footer";
    pub const SYNC_FOOTER: &str = "sync-footer";
    pub const DIRECTORY: &str = "directory";
}

/// What `open_locked` hands to `open_append`: the verified head, the
/// recorded writer parameters, the head snapshot's promised attributes, and
/// any audited tail truncation.
type OpenedForAppend = (
    OpenedHead,
    WriterParams,
    BTreeMap<FileVersionId, Attributes>,
    Option<TailTruncation>,
);

/// Single writer for one archive file (spec §12.2, §12.5).
pub struct ArchiveWriter<S: Storage> {
    storage: S,
    ids: Box<dyn IdSource>,
    read: ReadOptions,
    params: WriterParams,
    record_time: bool,
    archive_id: ArchiveId,
    head: Option<WriterHead>,
    /// Verified catalog at `head`; never mutated, duplicated per commit.
    catalog: Catalog,
    /// Promised attributes of every version reachable at `head` (the catalog
    /// holds none until C6; snapshot manifests must, D10.3).
    attributes: BTreeMap<FileVersionId, Attributes>,
    /// Written with the first commit; `None` once a head exists.
    new_descriptor: Option<Descriptor>,
    /// `EveryCommit` unless a test changed it (see [`CheckpointPolicy`]).
    policy: CheckpointPolicy,
    /// Test control: damage what a checkpoint serializes (see
    /// [`CheckpointTamper`]).
    #[cfg(any(test, feature = "test-controls"))]
    tamper: Option<CheckpointTamper>,
    needs_directory_sync: bool,
    poisoned: Option<String>,
    audit: Vec<AuditEvent>,
}

impl<S: Storage> std::fmt::Debug for ArchiveWriter<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveWriter")
            .field("archive_id", &self.archive_id)
            .field("head_seq", &self.head.map(|h| h.seq))
            .field("poisoned", &self.poisoned)
            .finish()
    }
}

fn lock(storage: &mut dyn Storage) -> Result<()> {
    storage.try_lock_exclusive().map_err(|e| match e {
        StorageError::LockHeld => MochiError::new(
            ErrorCode::LockConflict,
            "another writer holds this archive's publication lock (spec §12.5: fail on conflict)",
        ),
        other => other.into(),
    })
}

impl<S: Storage> ArchiveWriter<S> {
    /// Start a new archive in empty storage. Nothing is published until the
    /// first [`commit`](Self::commit); a file left empty is not an archive.
    pub fn create(mut storage: S, mut ids: Box<dyn IdSource>, opts: WriterOptions) -> Result<Self> {
        lock(&mut storage)?;
        let result = (|| -> Result<(ArchiveId, WriterParams, Catalog)> {
            if storage.size()? != 0 {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "create needs empty storage; use open_append for an existing archive",
                ));
            }
            let params = WriterParams {
                chunk_size: opts.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE),
                zstd_level: opts.zstd_level.unwrap_or(EncodeParams::default().level),
            };
            params.validate(&opts.read.limits)?;
            Ok((
                ArchiveId::generate(ids.as_mut())?,
                params,
                Catalog::new_working()?,
            ))
        })();
        let (archive_id, params, catalog) = match result {
            Ok(v) => v,
            Err(e) => {
                let _ = storage.unlock();
                return Err(e);
            }
        };
        Ok(ArchiveWriter {
            storage,
            ids,
            read: opts.read,
            params,
            record_time: opts.record_time,
            archive_id,
            head: None,
            catalog,
            attributes: BTreeMap::new(),
            // The default profile is not TAR-compatible (spec D4); choosing
            // TAR compatibility at creation arrives with that profile.
            new_descriptor: Some(Descriptor::new(archive_id, false)),
            policy: CheckpointPolicy::EveryCommit,
            #[cfg(any(test, feature = "test-controls"))]
            tamper: None,
            needs_directory_sync: true,
            poisoned: None,
            audit: Vec::new(),
        })
    }

    /// Open an existing archive for appending (§12.2 steps 1–2).
    pub fn open_append(
        mut storage: S,
        ids: Box<dyn IdSource>,
        opts: WriterOptions,
        tail: TailPolicy,
    ) -> Result<(Self, Option<TailTruncation>)> {
        lock(&mut storage)?;
        match Self::open_locked(&mut storage, &opts, tail) {
            Ok((head, params, attributes, truncation)) => {
                let mut audit = Vec::new();
                if let Some(t) = &truncation {
                    audit.push(AuditEvent::TailTruncated(t.clone()));
                }
                Ok((
                    ArchiveWriter {
                        storage,
                        ids,
                        read: opts.read,
                        params,
                        record_time: opts.record_time,
                        archive_id: head.commit.archive_id,
                        head: Some(WriterHead {
                            seq: head.commit.seq,
                            commit_id: head.commit_id,
                            footer_offset: head.location.footer.footer_offset,
                            committed_len: head.location.committed_len,
                            delta_manifest_hash: head.commit.delta_manifest.stored_hash,
                            descriptor: head.commit.descriptor,
                            next_base: next_base_after(&head),
                        }),
                        catalog: head.catalog,
                        attributes,
                        new_descriptor: None,
                        policy: CheckpointPolicy::EveryCommit,
                        #[cfg(any(test, feature = "test-controls"))]
                        tamper: None,
                        needs_directory_sync: false,
                        poisoned: None,
                        audit,
                    },
                    truncation,
                ))
            }
            Err(e) => {
                let _ = storage.unlock();
                Err(e)
            }
        }
    }

    fn open_locked(
        storage: &mut S,
        opts: &WriterOptions,
        tail: TailPolicy,
    ) -> Result<OpenedForAppend> {
        let mut location = locate_head(storage, &opts.read.limits)?;
        let mut truncation = None;
        match (&location.tail, tail) {
            (TailState::Clean, _) => {}
            (TailState::Unresolved { len, reason }, _) => {
                return Err(MochiError::new(
                    ErrorCode::TailUnresolved,
                    format!(
                        "{len} bytes follow the last valid commit and may hold a damaged \
                         commit ({reason}); not truncating. Use the repair workflow."
                    ),
                ));
            }
            (TailState::Uncommitted { len, .. }, TailPolicy::Refuse) => {
                return Err(MochiError::new(
                    ErrorCode::UncommittedTail,
                    format!(
                        "{len} uncommitted bytes follow the last valid commit (an interrupted \
                         write); appending needs them removed first"
                    ),
                ));
            }
            (
                TailState::Uncommitted {
                    len,
                    frames,
                    incomplete_final_frame,
                },
                TailPolicy::TruncateUncommitted,
            ) => {
                let (head_commit, head_id) = read_commit(storage, &location.footer, &opts.read)?;
                let t = TailTruncation {
                    committed_len: location.committed_len,
                    removed_len: *len,
                    removed_frames: frames.clone(),
                    incomplete_final_frame: *incomplete_final_frame,
                    head_seq: head_commit.seq,
                    head_commit_id: head_id,
                };
                storage.truncate(location.committed_len)?;
                storage.sync_data()?;
                let again = locate_head(storage, &opts.read.limits)?;
                if again.footer != location.footer || !again.tail.is_clean() {
                    return Err(MochiError::new(
                        ErrorCode::TailUnresolved,
                        "the head changed while truncating the tail",
                    ));
                }
                location = again;
                truncation = Some(t);
            }
        }
        let Opened { head, attributes } = open_at(storage, location, &opts.read, OpenMode::Append)?;
        if head.descriptor.tar_compatible {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "this archive was created with the TAR-compatible profile (spec D4), which this \
                 build cannot write; appending would break that constraint",
            ));
        }
        // The next snapshot must carry every reachable version's promised
        // attributes, which only snapshot manifests (and the deltas after
        // them) hold until C6 moves them into the catalog. open_at refused
        // already if they could not be reconstructed.
        let attributes = attributes.ok_or_else(|| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                "internal: an append open produced no attributes",
            )
        })?;
        let recorded = head.catalog.meta(META_WRITER_PARAMS)?.ok_or_else(|| {
            MochiError::new(
                ErrorCode::CatalogInvalid,
                "archive writer parameters are missing",
            )
        })?;
        let params = WriterParams::decode(&recorded)?;
        for (asked, have, what) in [
            (opts.chunk_size, params.chunk_size, "chunk size"),
            (
                opts.zstd_level.map(|l| l as u64),
                params.zstd_level as u64,
                "zstd level",
            ),
        ] {
            if asked.is_some_and(|a| a != have) {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    format!("{what} is recorded per archive at creation (spec §13) and differs"),
                ));
            }
        }
        params.validate(&opts.read.limits)?;
        Ok((head, params, attributes, truncation))
    }

    pub fn archive_id(&self) -> ArchiveId {
        self.archive_id
    }

    pub fn head_seq(&self) -> Option<u64> {
        self.head.map(|h| h.seq)
    }

    pub fn head_commit_id(&self) -> Option<CommitId> {
        self.head.map(|h| h.commit_id)
    }

    /// The catalog at the current head.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn audit_log(&self) -> &[AuditEvent] {
        &self.audit
    }

    /// The checkpoint policy the next commit will follow.
    pub fn checkpoint_policy(&self) -> CheckpointPolicy {
        self.policy
    }

    /// Release the publication lock, reporting a failure to do so (dropping
    /// the writer also releases it, but cannot report errors).
    pub fn close(mut self) -> Result<()> {
        Ok(self.storage.unlock()?)
    }

    fn committed_len(&self) -> u64 {
        self.head.map_or(0, |h| h.committed_len)
    }

    fn poison(&mut self, why: String) {
        if self.poisoned.is_none() {
            self.poisoned = Some(why);
        }
    }

    /// Remove our own unpublished bytes (before step 7 only).
    fn roll_back(&mut self, reason: &MochiError) {
        let to_len = self.committed_len();
        let from_len = self.storage.size().unwrap_or(u64::MAX);
        if from_len == to_len {
            return;
        }
        match self
            .storage
            .truncate(to_len)
            .and_then(|()| self.storage.sync_data())
        {
            Ok(()) => self.audit.push(AuditEvent::RolledBack {
                from_len,
                to_len,
                reason: reason.to_string(),
            }),
            Err(e) => self.poison(format!(
                "could not remove unpublished bytes after an aborted commit ({e}); \
                 the archive has an uncommitted tail"
            )),
        }
    }

    /// Publish one commit (§12.2 steps 3–10).
    pub fn commit(&mut self, tx: Transaction, ctx: &JobContext<'_>) -> Result<CommitOutcome> {
        if let Some(why) = &self.poisoned {
            return Err(MochiError::new(
                ErrorCode::WriterPoisoned,
                format!("this writer stopped earlier: {why}; reopen the archive"),
            ));
        }
        // Step 2, again: nothing may have changed under our lock.
        let size = self.storage.size()?;
        if size != self.committed_len() {
            return Err(MochiError::new(
                ErrorCode::LockConflict,
                format!(
                    "the archive is {size} bytes but its last commit ends at {}: something \
                     wrote to it without the publication lock",
                    self.committed_len()
                ),
            ));
        }

        // Steps 3–5. Nothing is published yet; any failure rolls back.
        let prepared = match ctx
            .check_cancelled()
            .and_then(|()| self.prepare(tx, ctx))
            .and_then(|p| {
                ctx.report(phase::SYNC_CONTENT, 0, None);
                ctx.check_cancelled().map(|()| p)
            }) {
            Ok(p) => p,
            Err(e) => {
                self.roll_back(&e);
                return Err(e);
            }
        };

        // Step 6.
        if let Err(e) = self.storage.sync_data() {
            let err = MochiError::new(
                ErrorCode::IoError,
                format!("persisting the commit's objects failed ({e}); nothing was published"),
            );
            self.roll_back(&err);
            self.poison(format!("sync failed before the footer: {e}"));
            return Err(err);
        }
        // Last cancellation point: after this the footer is written.
        ctx.report(phase::FOOTER, 0, None);
        if let Err(e) = ctx.check_cancelled() {
            self.roll_back(&e);
            return Err(e);
        }

        // Step 7.
        let footer = encode_footer_frame(
            prepared.commit_offset,
            prepared.seq,
            prepared.commit_frame.as_bytes(),
        );
        let expected_footer_offset = prepared.commit_offset + prepared.commit_frame.len();
        let unconfirmed = |what: String| MochiError::new(ErrorCode::CommitUnconfirmed, what);
        match self.storage.append(&footer) {
            Ok(at) if at == expected_footer_offset => {}
            Ok(at) => {
                let msg = format!("footer landed at {at}, not {expected_footer_offset}");
                self.poison(msg.clone());
                return Err(unconfirmed(msg));
            }
            Err(e) => {
                let msg = format!(
                    "writing the footer failed ({e}); the commit may or may not be published"
                );
                self.poison(msg.clone());
                return Err(unconfirmed(msg));
            }
        }
        // Step 8.
        ctx.report(phase::SYNC_FOOTER, 0, None);
        if let Err(e) = self.storage.sync_data() {
            let msg =
                format!("persisting the footer failed ({e}); the commit may or may not be durable");
            self.poison(msg.clone());
            return Err(unconfirmed(msg));
        }
        // Step 9.
        let mut durability = PublishDurability::Durable;
        if self.needs_directory_sync {
            ctx.report(phase::DIRECTORY, 0, None);
            match self.storage.sync_directory() {
                Ok(DirectoryDurability::Confirmed) => {}
                Ok(DirectoryDurability::Unconfirmed(why)) => {
                    durability = PublishDurability::DirectoryUnconfirmed(why);
                }
                Err(e) => {
                    let msg = format!(
                        "persisting the new archive's directory entry failed ({e}); \
                         the archive may not survive a power loss"
                    );
                    self.poison(msg.clone());
                    return Err(unconfirmed(msg));
                }
            }
            self.needs_directory_sync = false;
        }

        // Step 10.
        let footer_offset = expected_footer_offset;
        let committed_len = footer_offset + FOOTER_FRAME_LEN;
        self.head = Some(WriterHead {
            seq: prepared.seq,
            commit_id: prepared.commit_id,
            footer_offset,
            committed_len,
            delta_manifest_hash: prepared.delta_manifest_hash,
            descriptor: prepared.descriptor,
            next_base: prepared.next_base,
        });
        self.catalog = prepared.catalog;
        self.attributes = prepared.attributes;
        self.new_descriptor = None;
        Ok(CommitOutcome {
            status: CommitStatus::LocalCommitted,
            durability,
            seq: prepared.seq,
            commit_id: prepared.commit_id,
            footer_offset,
            committed_len,
            objects_written: prepared.objects,
        })
    }

    /// Steps 3–5: append content, manifest, checkpoint, and commit record.
    fn prepare(&mut self, tx: Transaction, ctx: &JobContext<'_>) -> Result<Prepared> {
        let seq = match self.head {
            None => 0,
            Some(h) => h.seq.checked_add(1).ok_or_else(|| {
                MochiError::new(ErrorCode::LimitExceeded, "commit sequence exhausted")
            })?,
        };
        let mut cat = match self.head {
            None => Catalog::new_working()?,
            Some(_) => self.catalog.duplicate()?,
        };
        let before = match self.head {
            None => Default::default(),
            Some(_) => cat.replay(None)?,
        };
        let encode = EncodeParams {
            level: self.params.zstd_level,
            ..EncodeParams::default()
        };
        let total: u64 = tx
            .entries
            .iter()
            .map(|e| match e {
                TxEntry::File { content, .. } => content.len() as u64,
                _ => 0,
            })
            .sum();
        let mut done = 0u64;
        let mut objects = 0u64;
        let mut ops = Vec::new();
        let mut chunks = Vec::new();
        let mut versions = Vec::new();
        let mut attributes = self.attributes.clone();

        // The archive descriptor, first byte of the file (D12). Written with
        // the first commit so that a failed or cancelled first commit rolls
        // back to an empty file, which is not an archive.
        let descriptor = match (&self.head, &self.new_descriptor) {
            (Some(h), _) => h.descriptor,
            (None, Some(d)) => {
                let frame = d.to_stored()?;
                let offset = self.storage.append(frame.as_bytes())?;
                if offset != mochi_format::registry::DESCRIPTOR_OFFSET {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        format!("the archive descriptor landed at offset {offset}, not 0"),
                    ));
                }
                ObjectRef {
                    offset,
                    stored_len: frame.len(),
                    stored_hash: stored_object_hash(frame.view()),
                }
            }
            (None, None) => {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: a new archive has no descriptor to write",
                ))
            }
        };

        // Step 3: content.
        ctx.report(phase::CONTENT, 0, Some(total));
        for entry in &tx.entries {
            ctx.check_cancelled()?;
            match entry {
                TxEntry::File {
                    path,
                    content,
                    attributes: attrs,
                } => {
                    let mut extents = Vec::new();
                    let piece = usize::try_from(self.params.chunk_size).unwrap_or(usize::MAX);
                    for (i, part) in content.chunks(piece).enumerate() {
                        let obj = build_object(
                            &DecodedBytes::new(part.to_vec()),
                            &encode,
                            Protection::None,
                            self.ids.as_mut(),
                            &self.read.limits,
                        )?;
                        let offset = self.storage.append(obj.stored.as_bytes())?;
                        cat.insert_object(&obj.record, Some(offset))?;
                        extents.push(Extent {
                            ordinal: u32::try_from(i).map_err(|_| {
                                MochiError::new(
                                    ErrorCode::LimitExceeded,
                                    "too many chunks in one file",
                                )
                            })?,
                            logical_offset: done_offset(i, self.params.chunk_size)?,
                            length: part.len() as u64,
                            source: ExtentSource::Chunk {
                                chunk: obj.record.id,
                                chunk_offset: 0,
                            },
                        });
                        chunks.push(ChunkEntry {
                            record: obj.record,
                            location: Some(offset),
                        });
                        objects += 1;
                        done += part.len() as u64;
                        ctx.report(phase::CONTENT, done, Some(total));
                        ctx.check_cancelled()?;
                    }
                    let version = FileVersion {
                        id: FileVersionId::from_bytes(self.ids.next_id()?),
                        kind: EntryKind::File,
                        logical_len: content.len() as u64,
                        content_hash: Some(file_content_hash(DecodedSlice::from_logical(content))),
                    };
                    cat.insert_file_version(&version, &extents)?;
                    ops.push(NamespaceOp::Put {
                        path: path.clone(),
                        version: version.id,
                    });
                    attributes.insert(version.id, *attrs);
                    versions.push(FileVersionEntry {
                        version,
                        extents,
                        attributes: *attrs,
                    });
                }
                TxEntry::Dir {
                    path,
                    attributes: attrs,
                } => {
                    let version = FileVersion {
                        id: FileVersionId::from_bytes(self.ids.next_id()?),
                        kind: EntryKind::Directory,
                        logical_len: 0,
                        content_hash: None,
                    };
                    cat.insert_file_version(&version, &[])?;
                    ops.push(NamespaceOp::Put {
                        path: path.clone(),
                        version: version.id,
                    });
                    attributes.insert(version.id, *attrs);
                    versions.push(FileVersionEntry {
                        version,
                        extents: Vec::new(),
                        attributes: *attrs,
                    });
                }
                TxEntry::Delete(path) => ops.push(NamespaceOp::Delete { path: path.clone() }),
                TxEntry::Rename { from, to } => {
                    let version = before.get(from).map(|e| e.version).ok_or_else(|| {
                        MochiError::new(
                            ErrorCode::NamespaceInvalid,
                            "rename source does not exist in the head snapshot",
                        )
                    })?;
                    ops.push(NamespaceOp::Delete { path: from.clone() });
                    ops.push(NamespaceOp::Put {
                        path: to.clone(),
                        version,
                    });
                }
            }
        }

        // Namespace validity against the completed commit state (§10.2).
        ctx.report(phase::CATALOG, 0, None);
        let after = cat.append_commit(&Commit {
            seq,
            parent: self.head.map(|h| h.seq),
            ops: ops.clone(),
        })?;
        if self.head.is_none() {
            cat.set_meta(META_ARCHIVE_ID, self.archive_id.as_bytes())?;
            cat.set_meta(META_WRITER_PARAMS, &self.params.encode()?)?;
        }
        ctx.check_cancelled()?;

        // The transaction ID is drawn here, after content (so the IDs drawn
        // for objects and versions are unchanged from schema 0) and before
        // any record that carries it (D11 identity: manifests, image).
        let mut txid = [0u8; 16];
        txid.copy_from_slice(&self.ids.next_id()?[..16]);
        let transaction_id = uuid_v4(txid);

        // Step 4: delta manifest, snapshot manifest, catalog image. Each is
        // bounded by the reader defaults, not self.read.limits (B.2.3); one
        // that does not fit is CAPACITY_EXCEEDED and nothing is published
        // (D10.11).
        ctx.report(phase::MANIFEST, 0, None);
        let mut manifest = Manifest {
            archive_id: self.archive_id,
            commit_seq: seq,
            transaction_id,
            parent: self.head.map(|h| ParentLink {
                seq: h.seq,
                delta_manifest_hash: h.delta_manifest_hash,
            }),
            kind: ManifestKind::Delta,
            chunks,
            file_versions: versions,
            ops,
            entries: Vec::new(),
            required_features: Vec::new(),
        };
        manifest.canonicalize();
        let delta_manifest = self.append_object(&manifest.to_stored()?)?;
        ctx.check_cancelled()?;

        // Checkpoint or delta (D10). Commit 0 is always a checkpoint; after
        // that the policy decides (production: every commit, until T14).
        let checkpoint = match self.head {
            None => true,
            Some(_) => self.policy.is_checkpoint(seq),
        };
        let (metadata, attributes) = if checkpoint {
            ctx.report(phase::CHECKPOINT, 0, None);
            #[allow(unused_mut)]
            let mut snapshot = Manifest::snapshot_from_catalog(
                &cat,
                self.archive_id,
                seq,
                transaction_id,
                &attributes,
            )?;
            // The source of truth for adoption (D10.7): the writer's own
            // state, never re-derived from what is about to be serialized.
            // Attributes are kept only for versions still reachable.
            let reachable = reachable_attributes(&after, attributes.clone())?;
            let source =
                AuthoritativeState::from_catalog(&cat, seq)?.with_attributes(reachable.clone());
            #[cfg(any(test, feature = "test-controls"))]
            if let Some(t) = self.tamper {
                t.apply_to_snapshot(&mut snapshot)?;
            }
            let snapshot_ref = self.append_object(&snapshot.to_stored()?)?;
            ctx.check_cancelled()?;

            // Binary envelope v0 (B.2.2, T10), bounded by the reader defaults
            // (B.2.3 writer default rule), not by self.read: an image over the
            // S − 592 budget is CAPACITY_EXCEEDED and nothing is published.
            #[cfg(any(test, feature = "test-controls"))]
            let tampered_cat = match self.tamper {
                Some(CheckpointTamper::ImageOmitsLastOp) => {
                    Some(self.catalog_without_last_op(&manifest, seq)?)
                }
                _ => None,
            };
            #[cfg(any(test, feature = "test-controls"))]
            let image = tampered_cat.as_ref().unwrap_or(&cat).publish()?;
            #[cfg(not(any(test, feature = "test-controls")))]
            let image = cat.publish()?;
            let identity = RecordIdentity {
                archive_id: *self.archive_id.as_bytes(),
                commit_sequence: seq,
                transaction_id,
            };
            let image_ref =
                self.append_object(&encode_image_record(image.as_bytes(), identity)?)?;
            ctx.check_cancelled()?;

            // D10.7 adoption (§18.1): re-read both representations from the
            // storage they were just written to, check their hashes, decode
            // them, and compare each with the source. Any mismatch fails the
            // commit before the commit record: no new head, and the existing
            // roll-back removes the unpublished bytes. The writer is not
            // poisoned (nothing was published, no sync failed; Q40).
            ctx.report(phase::ADOPT, 0, None);
            adopt_checkpoint(
                &self.storage,
                &source,
                &snapshot_ref,
                &image_ref,
                &identity,
                self.archive_id,
                &self.params.encode()?,
            )?;
            ctx.check_cancelled()?;
            (
                Metadata::Checkpoint {
                    image: image_ref,
                    snapshot: snapshot_ref,
                },
                reachable,
            )
        } else {
            // A delta on the base the D10.6 rule derives. No snapshot and
            // no image; the delta manifest above is the commit's state
            // change. Attributes are pruned to reachable versions here, as
            // the snapshot does for a checkpoint.
            let Some(h) = self.head else {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: commit 0 must be a checkpoint",
                ));
            };
            let attributes = reachable_attributes(&after, attributes)?;
            (Metadata::Delta { base: h.next_base }, attributes)
        };

        // Step 5: the commit record.
        ctx.report(phase::COMMIT_RECORD, 0, None);
        let time = match (tx.time, self.record_time) {
            (Some(t), _) => Some(t),
            (None, true) => system_time(),
            (None, false) => None,
        };
        let record = CommitRecord {
            archive_id: self.archive_id,
            seq,
            transaction_id,
            parent: self.head.map(|h| CommitParent {
                commit_id: h.commit_id,
                seq: h.seq,
                footer_offset: h.footer_offset,
            }),
            metadata,
            delta_manifest,
            required_features: Vec::new(),
            time,
            descriptor,
        };
        let (commit_frame, commit_id) = record.to_stored()?;
        if commit_frame.len() > self.read.limits.max_commit_frame_len {
            return Err(MochiError::new(
                ErrorCode::LimitExceeded,
                "commit record exceeds the commit-frame limit",
            ));
        }
        let commit_offset = self.storage.append(commit_frame.as_bytes())?;
        // The footer lands directly after the commit frame (step 7 checks).
        let footer_offset = commit_offset
            .checked_add(commit_frame.len())
            .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "offset overflow"))?;
        let next_base = match record.metadata {
            Metadata::Checkpoint { .. } => CommitLink {
                commit_id,
                seq,
                footer_offset,
            },
            Metadata::Delta { base } => base,
        };
        Ok(Prepared {
            seq,
            next_base,
            commit_offset,
            commit_frame,
            commit_id,
            delta_manifest_hash: delta_manifest.stored_hash,
            descriptor,
            catalog: cat,
            attributes,
            objects,
        })
    }

    /// Append one stored object and return its reference.
    fn append_object(&mut self, frame: &StoredObject) -> Result<ObjectRef> {
        Ok(ObjectRef {
            offset: self.storage.append(frame.as_bytes())?,
            stored_len: frame.len(),
            stored_hash: stored_object_hash(frame.view()),
        })
    }
}

/// D10.7, §18.1: re-read the two checkpoint representations from `storage`,
/// check their hashes against the references just written, decode them, and
/// compare each with `source`. The snapshot is decoded without SQLite and
/// compared **with** attributes; the image is opened as a reader opens it and
/// compared without them (the image holds none until C6, checklist Q6, Q38).
///
/// Bounds are the writer defaults (B.2.3), not the writer's own read limits:
/// the writer bounded its output by those, and a lower user limit must not
/// make it reject its own valid output.
///
/// A semantic disagreement is `CHECKPOINT_MISMATCH` (Q37). A decoder
/// rejecting hash-valid bytes the writer just produced is also a mismatch
/// (the writer wrote something its own reader refuses). A hash failure or a
/// failed read is a storage fault and keeps its code.
fn adopt_checkpoint(
    storage: &dyn ReadStorage,
    source: &AuthoritativeState,
    snapshot_ref: &ObjectRef,
    image_ref: &ObjectRef,
    identity: &RecordIdentity,
    archive_id: ArchiveId,
    writer_params: &[u8],
) -> Result<()> {
    let seq = source.seq;
    let max = Limits::WRITER_DEFAULT.max_frame_len;
    let mismatch = |what: &str, detail: String| {
        MochiError::new(
            ErrorCode::CheckpointMismatch,
            format!("the {what} of commit {seq} disagrees with the source state: {detail}"),
        )
    };
    let remap = |what: &str, e: MochiError| match e.code {
        ErrorCode::StoredIntegrityFailed | ErrorCode::IoError | ErrorCode::OutOfBounds => e,
        _ => mismatch(
            what,
            format!("it could not be read back ({}: {})", e.code, e.message),
        ),
    };

    // Snapshot: hash, canonical-CBOR decode, identity; no SQLite.
    let stored = load_verified(
        storage,
        snapshot_ref,
        snapshot_ref.end()?,
        max,
        "snapshot manifest",
    )?;
    let (manifest, _) =
        Manifest::from_stored(&stored, &Limits::WRITER_DEFAULT, &CborLimits::default())
            .map_err(|e| remap("snapshot manifest", e))?;
    if manifest.kind != ManifestKind::Snapshot || manifest.identity().check(identity).is_err() {
        return Err(mismatch(
            "snapshot manifest",
            "it is not the snapshot bound to this commit".to_string(),
        ));
    }
    let got =
        AuthoritativeState::from_snapshot(&manifest).map_err(|e| remap("snapshot manifest", e))?;
    if got != *source {
        return Err(mismatch(
            "snapshot manifest",
            got.differences(source, 8).join("; "),
        ));
    }

    // Image: hash, envelope bound to this commit, then SQLite exactly as a
    // reader opens it (integrity, foreign keys, extents, namespace).
    let stored = load_verified(storage, image_ref, image_ref.end()?, max, "catalog image")?;
    let bytes = decode_image_record(&stored, identity, &Limits::WRITER_DEFAULT)
        .map_err(|e| remap("catalog image", e))?;
    let img = Catalog::open_image(bytes, &CatalogLimits::default())
        .map_err(|e| remap("catalog image", e))?;
    let head = img.head_commit().map_err(|e| remap("catalog image", e))?;
    if head != Some(seq) {
        return Err(mismatch(
            "catalog image",
            format!("it materializes commit {head:?}"),
        ));
    }
    if img
        .meta(META_ARCHIVE_ID)
        .map_err(|e| remap("catalog image", e))?
        .as_deref()
        != Some(&archive_id.as_bytes()[..])
    {
        return Err(mismatch(
            "catalog image",
            "it names another archive".to_string(),
        ));
    }
    if img
        .meta(META_WRITER_PARAMS)
        .map_err(|e| remap("catalog image", e))?
        .as_deref()
        != Some(writer_params)
    {
        return Err(mismatch(
            "catalog image",
            "its writer parameters differ".to_string(),
        ));
    }
    let got = AuthoritativeState::from_catalog(&img, seq).map_err(|e| remap("catalog image", e))?;
    // C6: include attributes once the image stores them (Q6, Q38).
    let want = source.clone().without_attributes();
    if got != want {
        return Err(mismatch(
            "catalog image",
            got.differences(&want, 8).join("; "),
        ));
    }
    Ok(())
}

/// D10.7 for a published checkpoint, as `verify` repeats it: both
/// representations hash-verified and decoded, then compared without
/// attributes (the image holds none, Q38). A disagreement is
/// `CHECKPOINT_MISMATCH`. A representation that fails to load returns that
/// failure unchanged ([`crate::damage`] reports it as object damage).
pub fn check_checkpoint_representations(
    src: &dyn ReadStorage,
    cp: &HistoryEntry,
    opts: &ReadOptions,
) -> Result<()> {
    let Metadata::Checkpoint { image, snapshot } = cp.commit.metadata else {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "this commit is not a checkpoint",
        ));
    };
    let snap = read_bound_manifest(
        src,
        &cp.commit,
        &snapshot,
        cp.commit_offset,
        ManifestKind::Snapshot,
        opts,
    )?;
    let img = check_image(src, cp, &image, opts, false)?;
    compare_representations(&snap, &img, cp.commit.seq)
}

/// The shared comparison: snapshot (decoded, without SQLite) against image
/// (as a reader opened it), without attributes.
pub(crate) fn compare_representations(
    snapshot: &Manifest,
    image: &Catalog,
    seq: u64,
) -> Result<()> {
    let from_snapshot = AuthoritativeState::from_snapshot(snapshot)?.without_attributes();
    let from_image = AuthoritativeState::from_catalog(image, seq)?;
    if from_snapshot != from_image {
        return Err(MochiError::new(
            ErrorCode::CheckpointMismatch,
            format!(
                "the snapshot manifest and the catalog image of commit {seq} disagree: {}",
                from_snapshot.differences(&from_image, 8).join("; ")
            ),
        ));
    }
    Ok(())
}

/// The base a delta written after `head` names (D10.6 base rule).
fn next_base_after(head: &OpenedHead) -> CommitLink {
    match head.commit.metadata {
        Metadata::Checkpoint { .. } => CommitLink {
            commit_id: head.commit_id,
            seq: head.commit.seq,
            footer_offset: head.location.footer.footer_offset,
        },
        Metadata::Delta { base } => base,
    }
}

/// `attributes` restricted to the versions `reachable` names. Every
/// reachable version must have an entry: a snapshot covers all
/// authoritative state (D10.3), so an entry missing here would surface as
/// an incomplete snapshot at the next checkpoint.
fn reachable_attributes(
    reachable: &Snapshot,
    mut attributes: BTreeMap<FileVersionId, Attributes>,
) -> Result<BTreeMap<FileVersionId, Attributes>> {
    let mut out = BTreeMap::new();
    for (path, entry) in reachable.iter() {
        let a = attributes
            .remove(&entry.version)
            .or_else(|| out.get(&entry.version).copied());
        match a {
            Some(a) => {
                out.insert(entry.version, a);
            }
            None => {
                return Err(MochiError::new(
                    ErrorCode::RecordInvalid,
                    format!(
                        "no promised attributes are known for the version at {path:?}; \
                         a complete next snapshot could not be written (D10.3)"
                    ),
                ))
            }
        }
    }
    Ok(out)
}

/// Test controls (unit tests, and the non-default `test-controls` feature;
/// review decision 14). Enabling the feature enables these for the whole
/// build; it is not an isolation boundary.
#[cfg(any(test, feature = "test-controls"))]
impl<S: Storage> ArchiveWriter<S> {
    /// Choose which later commits are checkpoints. Commit 0 is a checkpoint
    /// under every policy. `Every(0)` is refused with `INVALID_ARGUMENT` and
    /// leaves the current policy unchanged. Production code cannot call
    /// this; it writes `EveryCommit` until the T14 trigger exists.
    pub fn set_checkpoint_policy(&mut self, policy: CheckpointPolicy) -> Result<()> {
        self.policy = policy.validate()?;
        Ok(())
    }

    /// Damage what the next checkpoints *serialize* (never the writer's own
    /// state), to show that adoption blocks the head (D10.7). `None` turns it
    /// off. Needs a commit with at least one namespace operation for
    /// [`CheckpointTamper::ImageOmitsLastOp`] and at least one entry for
    /// [`CheckpointTamper::SnapshotOmitsEntry`]; otherwise it changes nothing.
    pub fn set_checkpoint_tamper(&mut self, t: Option<CheckpointTamper>) {
        self.tamper = t;
    }

    /// A catalog like the one about to be published, except that commit `seq`
    /// omits the transaction's last namespace operation.
    fn catalog_without_last_op(&self, manifest: &Manifest, seq: u64) -> Result<Catalog> {
        let mut c = match self.head {
            None => {
                let mut c = Catalog::new_working()?;
                c.set_meta(META_ARCHIVE_ID, self.archive_id.as_bytes())?;
                c.set_meta(META_WRITER_PARAMS, &self.params.encode()?)?;
                c
            }
            Some(_) => self.catalog.duplicate()?,
        };
        for ch in &manifest.chunks {
            c.insert_object(&ch.record, ch.location)?;
        }
        for v in &manifest.file_versions {
            c.insert_file_version(&v.version, &v.extents)?;
        }
        let keep = manifest.ops.len().saturating_sub(1);
        c.append_commit(&Commit {
            seq,
            parent: self.head.map(|h| h.seq),
            ops: manifest.ops[..keep].to_vec(),
        })?;
        Ok(c)
    }
}

/// How [`ArchiveWriter::set_checkpoint_tamper`] damages a checkpoint's
/// serialized form (test controls only).
#[cfg(any(test, feature = "test-controls"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTamper {
    /// XOR the POSIX mode of the first file version (by ID) that has POSIX
    /// attributes, in the serialized snapshot only.
    SnapshotAttributes,
    /// Drop the last namespace entry (by path) from the serialized snapshot,
    /// and its version and chunks when nothing else uses them. The result
    /// still passes the manifest's structure checks: the divergence is
    /// semantic, not a decode error.
    SnapshotOmitsEntry,
    /// Serialize an image whose head commit omits the transaction's last
    /// namespace operation.
    ImageOmitsLastOp,
}

#[cfg(any(test, feature = "test-controls"))]
impl CheckpointTamper {
    fn apply_to_snapshot(self, snapshot: &mut Manifest) -> Result<()> {
        match self {
            CheckpointTamper::SnapshotAttributes => {
                if let Some(p) = snapshot
                    .file_versions
                    .iter_mut()
                    .find_map(|v| v.attributes.posix.as_mut())
                {
                    p.mode ^= 0o7;
                }
            }
            CheckpointTamper::SnapshotOmitsEntry => {
                if let Some((_, version)) = snapshot.entries.pop() {
                    if !snapshot.entries.iter().any(|(_, v)| *v == version) {
                        if let Some(pos) = snapshot
                            .file_versions
                            .iter()
                            .position(|v| v.version.id == version)
                        {
                            let dropped = snapshot.file_versions.remove(pos);
                            let used = |id: &crate::object::ObjectId| {
                                snapshot.file_versions.iter().any(|v| {
                                    v.extents.iter().any(|e| {
                                        matches!(e.source, ExtentSource::Chunk { chunk, .. } if chunk == *id)
                                    })
                                })
                            };
                            let gone: Vec<_> = dropped
                                .extents
                                .iter()
                                .filter_map(|e| match e.source {
                                    ExtentSource::Chunk { chunk, .. } if !used(&chunk) => {
                                        Some(chunk)
                                    }
                                    _ => None,
                                })
                                .collect();
                            snapshot.chunks.retain(|c| !gone.contains(&c.record.id));
                        }
                    }
                }
                snapshot.canonicalize();
            }
            CheckpointTamper::ImageOmitsLastOp => {}
        }
        Ok(())
    }
}

fn done_offset(index: usize, chunk_size: u64) -> Result<u64> {
    (index as u64)
        .checked_mul(chunk_size)
        .ok_or_else(|| MochiError::new(ErrorCode::LimitExceeded, "file offset overflows"))
}

fn system_time() -> Option<Mtime> {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    Some(Mtime {
        secs: i64::try_from(d.as_secs()).ok()?,
        nanos: d.subsec_nanos(),
    })
}

impl<S: Storage> Drop for ArchiveWriter<S> {
    fn drop(&mut self) {
        // Best effort; the OS also releases the lock when the file closes.
        let _ = self.storage.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_committed_is_representable() {
        for s in CommitStatus::ALL {
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            for forbidden in ["PRESERVED", "REPLICATION_PENDING", "PRESERVATION_DEGRADED"] {
                assert!(!json.contains(forbidden));
            }
        }
        assert_eq!(CommitStatus::ALL, &[CommitStatus::LocalCommitted]);
    }

    #[test]
    fn writer_params_round_trip() {
        let p = WriterParams {
            chunk_size: 123,
            zstd_level: -5,
        };
        assert_eq!(WriterParams::decode(&p.encode().unwrap()).unwrap(), p);
    }
}
