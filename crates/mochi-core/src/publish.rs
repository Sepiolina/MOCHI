//! Commit and single-file publication (spec §12.2; plan C5).
//!
//! # Reading the head
//!
//! [`locate_head`] finds the latest commit a reader may accept: the footer at
//! EOF if it validates (§8.4), otherwise the last valid footer found by a
//! forward structural scan, because "a previous valid footer may lie before
//! an incomplete tail" (§12.2). Bytes after that footer are the *tail*;
//! [`TailState`] says whether they are *eligible* for explicit truncation
//! (Annex B.2 D14: a conservative screen, not proof). [`open_head`]
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

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::codec::{EncodeParams, Protection};
use mochi_format::digest::{
    chunk_content_hash, file_content_hash, stored_object_hash, ChunkContentHash, CommitId,
    StoredBytesHasher, StoredObjectHash, StoredObjectScope, TailQuarantineHash,
};
use mochi_format::envelope::RecordIdentity;
use mochi_format::error::FormatError;
use mochi_format::footer::{encode_footer_frame, validate_footer, validate_footer_at_eof};
use mochi_format::footer::{ValidatedFooter, FOOTER_FRAME_LEN};
use mochi_format::frame::{walk_frame, FrameDetail, Frames};
use mochi_format::kdf::KdfParams;
use mochi_format::registry::{self, FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::{DecodedBytes, DecodedSlice, StoredObject};
use mochi_format::seal::KeyId;
use mochi_format::seal::{SealContext, SealTarget, FEATURE_ENCRYPTED};
use mochi_format::secret::{DataKey, OsRandom, Passphrase, Random};
use mochi_format::Limits;
use serde::Serialize;

use crate::catalog::extent::{Extent, ExtentSource};
use crate::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp, Snapshot};
use crate::catalog::path::ArchivePath;
use crate::catalog::{Catalog, CatalogLimits, Commit, FileVersion, SegmentApplier};
use crate::commit::{uuid_v4, CommitLink, CommitParent, CommitRecord, Metadata, ObjectRef};
use crate::descriptor::{Descriptor, Profile};
use crate::error::{ErrorCode, MochiError, Result};
use crate::image::{decode_image_payload, decode_image_record, encode_image_record, image_payload};
use crate::job::JobContext;
use crate::keys::{
    open_sealed_record, read_envelopes, seal_record, unlock_commit, KeyEnvelope, Unlocked,
};
use crate::manifest::{
    Attributes, ChunkEntry, FileVersionEntry, KeyOp, Manifest, ManifestKind, Mtime, ParentLink,
    Provenance,
};
use crate::object::{
    build_object, build_object_sealed, decode_verified, load_stored, seal_object, verify_stored,
    ArchiveId, IdSource, ObjectId, ObjectRecord,
};
use crate::quarantine::SidecarMetadata;
use crate::recovery::{
    catalog_from_snapshot, recover_from_manifests, ManifestRecovery, RecoveryScope,
};
use crate::report::{Finding, Severity};
use crate::retention::{RetentionOp, RetentionState};
use crate::segment::{check_delta_parent_link, walk_segment, SegmentInfo};
use crate::state::AuthoritativeState;
use crate::storage::{
    check_file_name, DirectoryDurability, ReadStorage, Storage, StorageDir, StorageError,
    StorageReader,
};
use crate::tar::{self, encode_header, member_for};

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
#[derive(Debug, Clone, Default)]
pub struct ReadOptions {
    pub limits: Limits,
    pub catalog: CatalogLimits,
    pub cbor: CborLimits,
    /// The passphrases of an Encrypted-profile archive and the data keys they
    /// have opened (Annex B.2.10 D20). `None` reads such an archive keylessly:
    /// structure, hashes, and key envelopes, never names or content.
    pub keys: Option<std::sync::Arc<crate::keys::KeySession>>,
}

/// Writer configuration. `None` parameters mean "the archive's recorded
/// value" when appending, and the product default when creating. A value
/// that differs from what the archive recorded at creation is refused rather
/// than silently ignored or silently changed.
#[derive(Debug, Clone, Default)]
pub struct WriterOptions {
    pub read: ReadOptions,
    pub chunk_size: Option<u64>,
    pub zstd_level: Option<i32>,
    /// Record the system time in each commit (informational, §12.1).
    /// Ignored when a transaction carries an explicit time.
    pub record_time: bool,
    /// The profile asked for (spec §7, D4, D12). `None` means the default
    /// profile when creating and the archive's own when appending. Appending
    /// with a different profile is `PROFILE_CHANGE_UNSUPPORTED`: a profile is
    /// fixed at creation (D12). Creating with one this build cannot write is
    /// `UNSUPPORTED_FEATURE`.
    pub profile: Option<Profile>,
    /// The checkpoint trigger's α and *F* (Annex B.2.3). `None` means the
    /// defaults. Writer policy: not recorded in the archive.
    pub checkpoint_trigger: Option<CheckpointTrigger>,
    /// In-archive deduplication (spec §9.4, §9.5). Writer policy: not
    /// recorded in the archive, and readers cannot tell the difference.
    pub dedup: Dedup,
    /// Argon2id cost for the key envelopes this writer creates (Annex B.2.10
    /// item 1). `None` is the writer default (m = 64 MiB, t = 3, p = 4), which
    /// is what every product surface uses; a value above the reader defaults
    /// is `LIMIT_EXCEEDED`. The passphrases come from `read.keys`.
    pub kdf: Option<KdfParams>,
}

/// Write-path deduplication (spec §9.4, §9.5; plan C9). A chunk whose
/// decoded bytes are already stored, unprotected and without dependencies,
/// at the published head is referenced instead of stored again (reuse by
/// reference, D10.4). The rules:
///
/// - **Head only (§9.5 rule 1).** Candidates come from the head catalog the
///   writer validated under its lock, never from the transaction being
///   written: two identical chunks within one commit are each stored (rule
///   2; the writer streams chunks before publication, so it does not take
///   the buffered-transaction exception).
/// - **Validated (§9.4).** A candidate must match on decoded length and
///   chunk content hash, and then on the bytes themselves: it is read back,
///   hash-verified as stored and decoded, and compared byte for byte. A
///   candidate that fails is never referenced; the chunk is stored anew and
///   the failure is counted ([`DedupStats::candidates_rejected`]), so a
///   damaged chunk is not spread to new file versions.
/// - **In-archive only.** Cross-archive deduplication does not exist (§9.4).
/// - **Unprotected only.** Encrypted archives (C11) must disclose that
///   deduplication reveals content equality before enabling it (§9.5 rule
///   3); until then protected chunks are never candidates.
///
/// The index is a rebuildable accelerator built from the head catalog on
/// first use and extended with each published commit's chunks; it is
/// never consulted for reachability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dedup {
    /// The profile decides (the default): in-archive deduplication in the
    /// default profile (spec D4 keeps it there), none in the TAR-compatible
    /// profile (Annex B.2.9 D19 rule 2).
    #[default]
    Auto,
    /// Reference identical chunks already at the head. Asking for it in the
    /// TAR-compatible profile is `INVALID_ARGUMENT`.
    InArchive,
    /// Store every chunk.
    Off,
}

impl Dedup {
    /// What the writer does for an archive of this profile.
    fn resolve(self, tar_compatible: bool) -> Result<Dedup> {
        match (self, tar_compatible) {
            (Dedup::Auto, false) => Ok(Dedup::InArchive),
            (Dedup::Auto, true) | (Dedup::Off, _) => Ok(Dedup::Off),
            (Dedup::InArchive, false) => Ok(Dedup::InArchive),
            (Dedup::InArchive, true) => Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "in-archive deduplication is not available in the TAR-compatible profile \
                 (Annex B.2.9 D19 rule 2): the stream needs each file's bytes where its \
                 member is",
            )),
        }
    }
}

/// What deduplication did in one commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DedupStats {
    /// Chunks referenced instead of stored.
    pub chunks_reused: u64,
    /// Decoded bytes those chunks hold.
    pub bytes_reused: u64,
    /// Candidates that matched the index but failed read-back validation
    /// (damaged or inconsistent); their chunks were stored anew.
    pub candidates_rejected: u64,
}

/// The checkpoint trigger (Annex B.2.3, writer policy, not wire format): a
/// commit is a checkpoint when Δ ≥ α·max(*B*, *F*), where Δ is the stored
/// bytes of the delta manifests, commit records, and footers since the base
/// and *B* is the base's image plus snapshot manifest. α is held as a ratio
/// so the decision is exact integer arithmetic, the same on every platform.
/// The defaults are α = 1 and *F* = 1 MiB, confirmed by the gate G3
/// measurements (T32, `docs/benchmarks/t32-scaling.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointTrigger {
    alpha_num: u64,
    alpha_den: u64,
    floor: u64,
}

impl Default for CheckpointTrigger {
    fn default() -> Self {
        CheckpointTrigger {
            alpha_num: 1,
            alpha_den: 1,
            floor: DEFAULT_CHECKPOINT_FLOOR,
        }
    }
}

/// *F*'s default (Annex B.2.3; confirmed by G3, T32).
pub const DEFAULT_CHECKPOINT_FLOOR: u64 = 1 << 20;

impl CheckpointTrigger {
    /// α = `alpha_num` / `alpha_den`, *F* = `floor` bytes. α must be
    /// positive: α = 0 would make every commit a checkpoint and removes the
    /// storage bound's 1/α term (B.2.3), so it is refused rather than
    /// silently meaning "every commit".
    pub fn new(alpha_num: u64, alpha_den: u64, floor: u64) -> Result<Self> {
        if alpha_num == 0 || alpha_den == 0 {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "checkpoint trigger: α must be a positive ratio (numerator and denominator \
                 at least 1)",
            ));
        }
        Ok(CheckpointTrigger {
            alpha_num,
            alpha_den,
            floor,
        })
    }

    pub fn alpha(&self) -> (u64, u64) {
        (self.alpha_num, self.alpha_den)
    }

    pub fn floor(&self) -> u64 {
        self.floor
    }

    /// Whether Δ ≥ α·max(*B*, *F*), exactly (`u128`, no overflow).
    pub fn requires_checkpoint(&self, delta_bytes: u64, base_bytes: u64) -> bool {
        let threshold = u128::from(base_bytes.max(self.floor));
        u128::from(delta_bytes) * u128::from(self.alpha_den)
            >= threshold * u128::from(self.alpha_num)
    }
}

/// Which commits the writer makes checkpoints (Annex B.2 D10).
///
/// **Production writes [`CheckpointPolicy::Trigger`]** (T14, B.2.3:
/// Δ ≥ α·max(*B*, *F*)), with α and *F* from
/// [`WriterOptions::checkpoint_trigger`]. The other policies exist to
/// produce fixed checkpoint schedules for tests (review decision 14) and can
/// only be selected through [`ArchiveWriter::set_checkpoint_policy`], which
/// is compiled in with the non-default `test-controls` feature. Cargo
/// features are additive, so that feature is not an isolation boundary.
///
/// Whatever the policy, commit 0 is a checkpoint (D10), a commit after
/// [`ArchiveWriter::request_checkpoint`] is one, and a delta's base is
/// derived by the base rule: the parent if the parent is a checkpoint, else
/// the parent's base (D10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointPolicy {
    /// The B.2.3 trigger (production).
    Trigger(CheckpointTrigger),
    /// Every commit is a checkpoint (the C5 writer; the replay oracle).
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

    /// Whether commit `seq` is written as a checkpoint, given the trigger's
    /// Δ and *B* at the current head. Commit 0 always is.
    fn is_checkpoint(self, seq: u64, delta_bytes: u64, base_bytes: u64) -> bool {
        seq == 0
            || match self {
                CheckpointPolicy::Trigger(t) => t.requires_checkpoint(delta_bytes, base_bytes),
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

/// The writer parameters (chunk size, zstd level) a catalog image records,
/// as [`WriterOptions`] takes them; `None` for a catalog without them (one
/// rebuilt from a snapshot manifest, review decision Q29). Repair (C8)
/// carries them into the archive it writes.
pub fn recorded_writer_parameters(catalog: &Catalog) -> Result<Option<(u64, i32)>> {
    catalog
        .meta(META_WRITER_PARAMS)?
        .map(|b| WriterParams::decode(&b).map(|p| (p.chunk_size, p.zstd_level)))
        .transpose()
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
    /// Eligible for explicit truncation (Annex B.2 D14): complete frames
    /// other than footers and descriptors, then at most one frame cut short
    /// by end of file, no footer marker where a complete footer could fit,
    /// and no unrecognised bytes. This is what an interrupted write leaves,
    /// but it is a conservative screen, **not proof**: one corruption event
    /// can erase both footer markers of a later commit. (The variant keeps
    /// its C5 name; the error code `UNCOMMITTED_TAIL` is stable.)
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

/// Decide whether `src[from..]` is *eligible* for explicit truncation
/// (Annex B.2 D14). Eligibility is a conservative screen, not proof that the
/// tail is uncommitted.
///
/// Conservative by design, because a wrong verdict licenses destroying a
/// commit. A crash under the §12.2 protocol leaves complete frames followed
/// by at most one frame cut short at EOF, and never a *complete* footer (the
/// footer is written last, after a sync). So the tail is eligible
/// (`Uncommitted`) only if (the D14 conditions):
///
/// (a) it walks as complete frames that are neither footers nor descriptors,
///     optionally ending in one frame that runs past EOF, and contains no
///     unrecognised bytes;
/// (b) no *complete* footer could be hiding in it: neither the footer's
///     skippable header nor its payload magic occurs at any byte position
///     from which a whole 72-byte footer would still fit before EOF; and
/// (c) it contains no descriptor frame. The only descriptor is at offset 0
///     (D12), so one in the tail is damage or a forgery, and a footer whose
///     header was flipped to the descriptor kind (`0x57`) must not make the
///     tail look like ordinary frames.
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
            // D14 (c). The walker already refuses a descriptor away from
            // offset 0 (below); this arm keeps the rule if that ever changes.
            Ok(span) if span.kind == FrameKind::ArchiveDescriptor => {
                return unresolved(descriptor_in_tail(span.offset));
            }
            Err(FormatError::MisplacedFrame { offset, magic })
                if magic == registry::ARCHIVE_DESCRIPTOR =>
            {
                return unresolved(descriptor_in_tail(offset));
            }
            Ok(span) => frames.push(span.kind),
            Err(FormatError::Truncated { .. }) => {
                // D14 (c) for the frame cut short by EOF too: no commit writes
                // a descriptor after offset 0, so a cut-short one is not a torn
                // write. (The walker refuses it first today; checked here so
                // the rule does not rest on that.)
                let at = walker.position();
                let mut magic = [0u8; 4];
                if mochi_format::ReadAt::read_at(r, at, &mut magic).is_ok()
                    && u32::from_le_bytes(magic) == registry::ARCHIVE_DESCRIPTOR
                {
                    return unresolved(descriptor_in_tail(at));
                }
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

fn descriptor_in_tail(offset: u64) -> String {
    format!(
        "a descriptor frame at offset {offset} follows the last valid commit; a tail \
         containing one is never eligible for truncation (D14)"
    )
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
pub(crate) fn load_verified(
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
    /// The archive's data key, for an Encrypted-profile archive opened with a
    /// passphrase that opens one of this commit's key envelopes (Annex B.2.10
    /// D20). `None` for every other archive.
    pub unlocked: Option<std::sync::Arc<Unlocked>>,
}

impl OpenedHead {
    pub fn seq(&self) -> u64 {
        self.commit.seq
    }

    /// The key and archive sealed chunks are read under, if this archive is
    /// encrypted; `None` for Core, where [`crate::object::decode_verified`]
    /// needs none.
    pub fn seal_context(&self) -> Option<SealContext<'_>> {
        self.unlocked.as_deref().map(Unlocked::context)
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

/// The promised attributes (spec §10.4.1, D6) of every version reachable in
/// an opened commit, for restoration (plan C6).
///
/// The catalog does not hold attributes (B.2 checklist question 6); the
/// manifests do. They are rebuilt as appending rebuilds them: the segment
/// base's snapshot manifest S(*b*), then each delta of the segment in order,
/// every manifest hash-verified against its commit before it is decoded. A
/// version introduced twice is `RECORD_INVALID` (D10.4), and a reachable
/// version without attributes is `RECORD_INVALID` (D10.3). A damaged S(*b*)
/// is an error here, although the commit's files remain readable (Q31):
/// the caller reports attributes as unavailable rather than guessing them.
pub fn promised_attributes(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    opts: &ReadOptions,
) -> Result<BTreeMap<FileVersionId, Attributes>> {
    Ok(segment_state(src, head, opts)?.attributes)
}

/// An opened commit's [`SegmentState`]: promised attributes and retention,
/// rebuilt from S(*b*) and the segment's deltas, every manifest
/// hash-verified against its commit before it is decoded. Any missing,
/// unverified, or invalid manifest is an error; nothing is guessed and no
/// earlier checkpoint is tried (D10.10).
pub fn segment_state(
    src: &dyn ReadStorage,
    head: &OpenedHead,
    opts: &ReadOptions,
) -> Result<SegmentState> {
    let head_entry = HistoryEntry {
        footer_offset: head.location.footer.footer_offset,
        commit_offset: head.location.footer.fields.commit_offset,
        commit: head.commit.clone(),
        commit_id: head.commit_id,
    };
    let (entries, _) = walk_segment(src, head_entry, opts)?;
    let Some(base) = entries.first() else {
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
    let mut state = SegmentState::from_snapshot(&s_b);
    for pair in entries.windows(2) {
        let (prev, e) = (&pair[0], &pair[1]);
        let delta = read_bound_manifest(
            src,
            &e.commit,
            &e.commit.delta_manifest,
            e.commit_offset,
            ManifestKind::Delta,
            opts,
        )?;
        check_delta_parent_link(&delta, &prev.commit)?;
        state.apply_delta(&delta)?;
    }
    let state = state.complete(&head.catalog.replay(None)?)?;
    check_key_state(
        src,
        &head.commit,
        head.location.footer.fields.commit_offset,
        &state.keys,
        opts,
    )?;
    Ok(state)
}

/// D20 item 10: the key envelopes a commit lists are the key state replayed
/// from S(*b*) and the segment's key operations. A commit that lists others
/// disagrees with its own manifests (`RECORD_INVALID`). Checks nothing outside
/// the Encrypted profile.
pub(crate) fn check_key_state(
    src: &dyn ReadStorage,
    commit: &CommitRecord,
    commit_offset: u64,
    replayed: &[[u8; 16]],
    opts: &ReadOptions,
) -> Result<()> {
    if !commit.encrypted() {
        return Ok(());
    }
    let listed: Vec<[u8; 16]> = read_envelopes(src, commit, commit_offset, opts)?
        .into_iter()
        .map(|(_, e)| e.envelope_id)
        .collect();
    if listed != replayed {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            format!(
                "commit {} lists {} key envelope(s) that differ from the key state its \
                 manifests replay to ({} envelope(s)) (D20 item 10)",
                commit.seq,
                listed.len(),
                replayed.len()
            ),
        ));
    }
    Ok(())
}

/// Encrypted profile (D20 item 10), for `verify`: for **every** commit of
/// `history`, the envelopes it lists equal the key state its segment's
/// manifests replay to (S(*b*), then each delta's key operations). Returns each
/// commit that disagrees or whose manifests cannot be replayed, with the
/// reason; a commit after a failed checkpoint is not blamed again. Opening for
/// reading does not do this (it never reads S(*b*), D10.9); appending, baseline
/// recovery, and [`segment_state`] do, for the commit they open. `None` if
/// cancelled.
pub(crate) fn check_history_key_states(
    src: &dyn ReadStorage,
    history: &[HistoryEntry],
    opts: &ReadOptions,
    cancelled: &dyn Fn() -> bool,
) -> Option<Vec<(u64, MochiError)>> {
    let mut problems = Vec::new();
    let mut state: Option<SegmentState> = None;
    for e in history {
        if cancelled() {
            return None;
        }
        let step = (|| -> Result<()> {
            match e.commit.metadata {
                Metadata::Checkpoint { snapshot, .. } => {
                    let s_b = read_bound_manifest(
                        src,
                        &e.commit,
                        &snapshot,
                        e.commit_offset,
                        ManifestKind::Snapshot,
                        opts,
                    );
                    state = s_b.as_ref().ok().map(SegmentState::from_snapshot);
                    s_b?;
                }
                Metadata::Delta { .. } => {
                    let delta = read_bound_manifest(
                        src,
                        &e.commit,
                        &e.commit.delta_manifest,
                        e.commit_offset,
                        ManifestKind::Delta,
                        opts,
                    )?;
                    match state.as_mut() {
                        Some(st) => st.apply_delta(&delta)?,
                        // Blamed at the checkpoint that failed.
                        None => return Ok(()),
                    }
                }
            }
            match &state {
                Some(st) => check_key_state(src, &e.commit, e.commit_offset, &st.keys, opts),
                None => Ok(()),
            }
        })();
        if let Err(err) = step {
            problems.push((e.commit.seq, err));
        }
    }
    Some(problems)
}

/// D12: a descriptor that cannot be loaded, hash-verified, or decoded, or
/// that names another archive, is `DESCRIPTOR_INVALID`. Refusals
/// (`UNSUPPORTED_FEATURE`), reader limits, and I/O keep their codes.
pub(crate) fn read_descriptor(
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
pub fn read_bound_manifest(
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
    let manifest = if commit.encrypted() {
        // D20 item 5: a manifest of an Encrypted archive is a sealed object,
        // bound to its commit's sequence and transaction ID; the plaintext is
        // exactly what a Core manifest frame would carry as its payload.
        // Unlocks from the commit's own envelopes when the session has not
        // opened this archive yet; a cached key (the head's) is reused.
        let key = unlock_commit(src, commit, limit, opts, false)?;
        let target = match kind {
            ManifestKind::Delta => SealTarget::DeltaManifest {
                sequence: commit.seq,
                transaction_id: commit.transaction_id,
            },
            ManifestKind::Snapshot => SealTarget::SnapshotManifest {
                sequence: commit.seq,
                transaction_id: commit.transaction_id,
            },
        };
        let plaintext = open_sealed_record(&stored, &key, &target, &opts.limits)?;
        Manifest::decode(&plaintext, &opts.limits, &opts.cbor)?
    } else {
        Manifest::from_stored(&stored, &opts.limits, &opts.cbor)?.0
    };
    if manifest.encrypted() != commit.encrypted() {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            format!(
                "the {what} and the commit disagree about the Encrypted profile (required \
                 feature {FEATURE_ENCRYPTED})"
            ),
        ));
    }
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
type Replayed = (Catalog, SegmentInfo, Option<SegmentState>, CatalogSource);

/// State the catalog image does not hold and the manifests do: promised
/// attributes (B.2 checklist question 6) and retention (spec Annex B D18).
/// Rebuilt the same way for both: the segment base's snapshot manifest
/// S(*b*), then each delta of the segment in order (D10.4, D10.10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentState {
    /// Promised attributes. While replaying, every version S(*b*) and the
    /// deltas introduce; once complete, exactly the versions the head
    /// reaches.
    pub attributes: BTreeMap<FileVersionId, Attributes>,
    pub retention: RetentionState,
    /// Encrypted profile (D20 item 10): the IDs of the key envelopes valid at
    /// the commit, replayed from S(*b*) and the segment's key operations. Empty
    /// outside the profile. The commit's own key list must equal it.
    pub keys: Vec<[u8; 16]>,
}

impl SegmentState {
    fn from_snapshot(s_b: &Manifest) -> Self {
        SegmentState {
            attributes: s_b
                .file_versions
                .iter()
                .map(|v| (v.version.id, v.attributes))
                .collect(),
            retention: s_b.retention.clone(),
            keys: s_b.keys.state.clone(),
        }
    }

    /// Add what delta manifest `delta` changes. Versions are immutable and
    /// introduced once: a delta's attributes are those of the versions it
    /// introduces, and a reintroduction is `RECORD_INVALID` (D10.4).
    /// Retention operations apply in order, atomically.
    fn apply_delta(&mut self, delta: &Manifest) -> Result<()> {
        for v in &delta.file_versions {
            if self.attributes.contains_key(&v.version.id) {
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
        let mut retention = self.retention.clone();
        retention.apply(&delta.retention_ops, delta.commit_seq)?;
        let mut keys = self.keys.clone();
        crate::manifest::ManifestKeys::apply(&mut keys, &delta.keys.ops)?;
        for v in &delta.file_versions {
            self.attributes.insert(v.version.id, v.attributes);
        }
        self.retention = retention;
        self.keys = keys;
        Ok(())
    }

    /// Keep only the attributes of versions `reachable` names, refusing if
    /// any of them has none (D10.3).
    fn complete(self, reachable: &Snapshot) -> Result<Self> {
        Ok(SegmentState {
            attributes: reachable_attributes(reachable, self.attributes)?,
            retention: self.retention,
            keys: self.keys,
        })
    }
}

struct Opened {
    head: OpenedHead,
    /// `Some` exactly in [`OpenMode::Append`].
    state: Option<SegmentState>,
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
    // D20 item 4: the descriptor and the commit agree about the profile, or
    // the descriptor is mismatched (D12). An Encrypted archive then needs a
    // passphrase that opens one of *this commit's* key envelopes.
    if descriptor.encrypted() != commit.encrypted() {
        return Err(MochiError::new(
            ErrorCode::DescriptorInvalid,
            "the archive descriptor and the commit disagree about the Encrypted profile",
        ));
    }
    let unlocked = if commit.encrypted() {
        // The head needs one of its own envelopes opened; a historical commit
        // may reuse the archive's data key (see `unlock_commit`).
        let strict = location.source != HeadSource::Explicit;
        Some(unlock_commit(src, &commit, limit, opts, strict)?)
    } else {
        None
    };

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
    let (catalog, segment, state, catalog_source) = match commit.metadata {
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
            let (delta_bytes, base_bytes) =
                crate::segment::segment_accounting(std::slice::from_ref(&head_entry));
            let segment = SegmentInfo {
                base_seq: commit.seq,
                base_commit_id: commit_id,
                base_footer_offset: head_entry.footer_offset,
                base_hint_mismatch: None,
                delta_bytes,
                base_bytes,
            };
            let state = match mode {
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
                    // Same completeness rule as for a delta head (decision
                    // 21, checklist Q24): refuse at open, not at the next
                    // checkpoint.
                    let reachable = catalog.replay(None)?;
                    Some(
                        SegmentState::from_snapshot(&snapshot)
                            .complete(&reachable)
                            .map_err(attributes_incomplete)?,
                    )
                }
            };
            (catalog, segment, state, source)
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
    if let Some(st) = &state {
        check_key_state(src, &commit, limit, &st.keys, opts)?;
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
            unlocked,
        },
        state,
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
    let plaintext;
    let image = if cp.commit.encrypted() {
        let key = unlock_commit(src, &cp.commit, cp.commit_offset, opts, false)?;
        let target = SealTarget::Image {
            sequence: cp.commit.seq,
            transaction_id: cp.commit.transaction_id,
        };
        plaintext = open_sealed_record(&stored, &key, &target, &opts.limits)?;
        decode_image_payload(&plaintext, &cp.commit.identity(), &opts.limits, true)?
    } else {
        decode_image_record(&stored, &cp.commit.identity(), &opts.limits)?
    };
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
    let mut state = match mode {
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
            Some(SegmentState::from_snapshot(&s_b))
        }
    };

    apply_segment_deltas(
        src,
        &entries,
        Some(head_manifest),
        &mut applier,
        state.as_mut(),
        opts,
    )?;

    if applier.head() != last.commit.seq {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "replay did not reach the head commit",
        ));
    }
    let state = match state {
        None => None,
        Some(st) => Some(
            st.complete(applier.namespace())
                .map_err(attributes_incomplete)?,
        ),
    };
    let catalog = applier.into_catalog();
    if mode == OpenMode::Read {
        catalog.make_query_only()?;
    }
    Ok((catalog, info, state, catalog_source))
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
    mut state: Option<&mut SegmentState>,
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
        if let Some(st) = state.as_deref_mut() {
            st.apply_delta(delta)?;
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
        let Some(parent) = parent_entry(src, &r, child, opts)? else {
            break;
        };
        out.push(parent);
    }
    out.reverse();
    Ok(out)
}

/// The commit `child` names as its parent, validated as [`walk_back`]
/// validates it; `None` for commit 0.
fn parent_entry(
    src: &dyn ReadStorage,
    r: &StorageReader<'_>,
    child: &HistoryEntry,
    opts: &ReadOptions,
) -> Result<Option<HistoryEntry>> {
    let Some(p) = child.commit.parent else {
        return Ok(None);
    };
    if p.footer_offset >= child.footer_offset {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "parent footer hint does not precede the child",
        ));
    }
    let footer = validate_footer(r, p.footer_offset, &opts.limits)?;
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
    Ok(Some(HistoryEntry {
        footer_offset: p.footer_offset,
        commit_offset: footer.fields.commit_offset,
        commit,
        commit_id,
    }))
}

/// The published chain as far back from the head as it can be followed
/// (repair, C8; spec §22 step 2): the commits from the first one whose
/// parent cannot be validated up to the head, in ascending order, and that
/// failure. Every commit returned is linked to the head; nothing before a
/// break is trusted, whatever a scan would find (§22.1). Errors that
/// compromise the run (operational, [`crate::verify::classify`]: I/O,
/// cancellation, limits) are returned as errors, not as a break.
pub(crate) fn walk_back_tolerant(
    src: &dyn ReadStorage,
    head: HistoryEntry,
    opts: &ReadOptions,
) -> Result<(Vec<HistoryEntry>, Option<MochiError>)> {
    let r = reader(src)?;
    let mut out = vec![head];
    let mut broken = None;
    loop {
        let child = &out[out.len() - 1];
        match parent_entry(src, &r, child, opts) {
            Ok(Some(parent)) => out.push(parent),
            Ok(None) => break,
            Err(e) if crate::verify::classify(e.code) == crate::verify::ErrorClass::Operational => {
                return Err(e)
            }
            Err(e) => {
                broken = Some(e);
                break;
            }
        }
    }
    out.reverse();
    Ok((out, broken))
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
    /// Retention state at the head (spec Annex B D18).
    pub retention: RetentionState,
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
    let mut state = SegmentState::from_snapshot(&s_b);
    let mut applier = SegmentApplier::new(catalog)?;
    apply_segment_deltas(src, &entries, None, &mut applier, Some(&mut state), opts)?;
    if applier.head() != last.commit.seq {
        return Err(MochiError::new(
            ErrorCode::RecordInvalid,
            "replay did not reach the head commit",
        ));
    }
    // §11.1: snapshot recovery includes promised attributes, so a version
    // with none is not a partial success (same code as checklist Q24).
    let SegmentState {
        attributes,
        retention,
        keys,
    } = state.complete(applier.namespace()).map_err(|e| {
        MochiError::new(
            e.code,
            format!(
                "cannot recover: promised attributes could not be reconstructed from the \
                 base snapshot and the segment's deltas ({})",
                e.message
            ),
        )
    })?;
    check_key_state(src, &last.commit, last.commit_offset, &keys, opts)?;
    let catalog = applier.into_catalog();
    catalog.make_query_only()?;
    Ok(BaselineRecovery {
        head_seq,
        head_commit_id,
        segment,
        catalog,
        attributes,
        retention,
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

/// What [`ArchiveWriter::open_append`] does with an eligible tail (§12.2,
/// D14). It has no access to the archive's directory, so it cannot write a
/// quarantine sidecar; truncating through it is the `--no-quarantine`
/// waiver, recorded as such. [`ArchiveWriter::open_append_in`] quarantines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPolicy {
    /// Refuse with [`ErrorCode::UncommittedTail`].
    Refuse,
    /// Truncate an *eligible* tail (D14; [`TailState::Uncommitted`])
    /// **without** a quarantine copy, recording [`Waiver::NoQuarantine`].
    /// An unresolved tail is still refused ([`ErrorCode::TailUnresolved`]).
    /// Eligibility is not proof.
    TruncateWithoutQuarantine,
}

/// The D14 waivers, each explicit and recorded (plan T25).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TruncationWaivers {
    /// `--no-quarantine`: truncate without a sidecar copy.
    pub no_quarantine: bool,
    /// `--accept-unconfirmed-durability`: truncate although the sidecar's
    /// directory flush was not confirmed (always the case on Windows before
    /// G6). The sidecar is still written, verified, and synced.
    pub accept_unconfirmed_durability: bool,
}

/// What [`ArchiveWriter::open_append_in`] does with an eligible tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailRepair {
    Refuse,
    /// Quarantine (unless waived), then truncate.
    Truncate(TruncationWaivers),
}

/// A waiver that was used, for the report (D14 "Every waiver is recorded as
/// a report finding").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiver {
    NoQuarantine,
    AcceptUnconfirmedDurability { why: String },
}

/// Where a truncated tail was quarantined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineRecord {
    /// The sidecar's file name, in the archive's directory.
    pub sidecar: String,
    pub tail_hash: TailQuarantineHash,
    /// The sidecar's directory flush.
    pub directory: DirectoryDurability,
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
    /// The verified, synced sidecar holding the removed bytes; `None` only
    /// under [`Waiver::NoQuarantine`].
    pub quarantine: Option<QuarantineRecord>,
    /// Waivers this truncation used.
    pub waivers: Vec<Waiver>,
}

impl TailTruncation {
    /// Report findings (D14): the truncation itself, and one per waiver.
    pub fn findings(&self) -> Vec<Finding> {
        let mut out = vec![Finding {
            code: ErrorCode::UncommittedTail,
            severity: Severity::Info,
            message: Some(format!(
                "removed {} bytes after commit {} (an eligible tail, D14; eligibility is not \
                 proof){}",
                self.removed_len,
                self.head_seq,
                match &self.quarantine {
                    Some(q) => format!("; quarantined to {}", q.sidecar),
                    None => String::new(),
                }
            )),
            expected: None,
            observed: None,
            affected: None,
        }];
        for w in &self.waivers {
            let (code, message) = match w {
                Waiver::NoQuarantine => (
                    ErrorCode::UncommittedTail,
                    "waiver --no-quarantine: the removed bytes were not copied anywhere"
                        .to_string(),
                ),
                Waiver::AcceptUnconfirmedDurability { why } => (
                    ErrorCode::DurabilityUnconfirmed,
                    format!(
                        "waiver --accept-unconfirmed-durability: the sidecar's directory entry \
                         is not confirmed durable ({why})"
                    ),
                ),
            };
            out.push(Finding {
                code,
                severity: Severity::Warning,
                message: Some(message),
                expected: None,
                observed: None,
                affected: None,
            });
        }
        out
    }
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
    pub dedup: DedupStats,
    /// Whether this commit is a checkpoint (D10).
    pub checkpoint: bool,
}

/// Namespace changes for one commit, applied in order.
#[derive(Debug, Clone, Default)]
pub struct Transaction {
    entries: Vec<TxEntry>,
    time: Option<Mtime>,
    /// Retention operations (spec Annex B D18), applied after the
    /// namespace changes, in order.
    retention: Vec<RetentionOp>,
    /// Compaction only: delta(0)'s provenance.
    provenance: Option<Provenance>,
    /// Encrypted profile only (D20 item 10): passphrases to add as new key
    /// envelopes, and envelope IDs to remove. A rewrap.
    key_adds: Vec<std::sync::Arc<Passphrase>>,
    key_removes: Vec<[u8; 16]>,
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
    /// Compaction (C9): put an existing version, identity preserved, with
    /// the stored chunks it needs that the archive does not hold yet.
    Copied {
        path: ArchivePath,
        version: FileVersionEntry,
        chunks: Vec<(ObjectRecord, StoredObject)>,
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

    /// Compaction (C9): put `version` at `path` with its ID, extents, and
    /// attributes preserved. `chunks` are the stored objects its extents
    /// need, with their records; any the archive already holds are
    /// referenced, the rest are appended byte for byte after their stored
    /// hash is checked. A version the archive already holds is referenced
    /// and must be identical.
    pub(crate) fn put_copied(
        &mut self,
        path: ArchivePath,
        version: FileVersionEntry,
        chunks: Vec<(ObjectRecord, StoredObject)>,
    ) -> &mut Self {
        self.entries.push(TxEntry::Copied {
            path,
            version,
            chunks,
        });
        self
    }

    /// Compaction (C9): a retention operation as is.
    pub(crate) fn push_retention(&mut self, op: RetentionOp) -> &mut Self {
        self.retention.push(op);
        self
    }

    /// Compaction (C9): delta(0)'s provenance.
    pub(crate) fn set_provenance(&mut self, p: Provenance) -> &mut Self {
        self.provenance = Some(p);
        self
    }

    /// Encrypted profile (D20 item 10): wrap the archive's data key under
    /// `passphrase` in a new key envelope. With [`remove_envelope`] this is a
    /// rewrap commit: the data key and the data do not change, and the key
    /// operations are its audit record. Refused in other profiles.
    ///
    /// [`remove_envelope`]: Self::remove_envelope
    pub fn add_passphrase(&mut self, passphrase: std::sync::Arc<Passphrase>) -> &mut Self {
        self.key_adds.push(passphrase);
        self
    }

    /// Encrypted profile: take a key envelope out of the set valid from this
    /// commit on. **Not revocation**: the frame stays in the file's history and
    /// any earlier copy still opens with it. The set never becomes empty.
    pub fn remove_envelope(&mut self, envelope_id: [u8; 16]) -> &mut Self {
        self.key_removes.push(envelope_id);
        self
    }

    /// Record this informational time instead of the system clock.
    pub fn at(&mut self, time: Mtime) -> &mut Self {
        self.time = Some(time);
        self
    }

    /// Expire snapshot `seq`: it stops being retained by default (§16.3).
    /// Only an earlier commit can be expired; a held one stays a root until
    /// its holds are released.
    pub fn expire(&mut self, seq: u64) -> &mut Self {
        self.retention.push(RetentionOp::Expire { seq });
        self
    }

    /// Place legal hold `label` (1–255 bytes, unique among active holds) on
    /// snapshot `seq`, which may be this commit's own. A held snapshot is
    /// never collected (§16.3).
    pub fn hold(&mut self, label: &[u8], seq: u64) -> &mut Self {
        self.retention.push(RetentionOp::Hold {
            label: label.to_vec(),
            seq,
        });
        self
    }

    /// Release legal hold `label`.
    pub fn release(&mut self, label: &[u8]) -> &mut Self {
        self.retention.push(RetentionOp::Release {
            label: label.to_vec(),
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.retention.is_empty()
            && self.key_adds.is_empty()
            && self.key_removes.is_empty()
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
    /// Checkpoint-trigger Δ and *B* at the head (B.2.3; see
    /// [`SegmentInfo::delta_bytes`]).
    delta_bytes: u64,
    base_bytes: u64,
}

/// Bytes written before the footer, ready to publish.
struct Prepared {
    seq: u64,
    next_base: CommitLink,
    delta_bytes: u64,
    base_bytes: u64,
    commit_offset: u64,
    commit_frame: StoredObject,
    commit_id: CommitId,
    delta_manifest_hash: StoredObjectHash,
    descriptor: ObjectRef,
    catalog: Catalog,
    attributes: BTreeMap<FileVersionId, Attributes>,
    retention: RetentionState,
    objects: u64,
    dedup: DedupStats,
    /// Chunks this commit stored, for the dedup index once it is published.
    new_chunks: Vec<(DedupKey, ObjectId)>,
    checkpoint: bool,
    /// The key envelopes valid from this commit (Encrypted profile; empty
    /// otherwise).
    envelopes: Vec<EnvelopeRef>,
}

/// The contiguous byte range of one commit's data objects and the running
/// stored-object-scope hash over it (Annex B.2.10 D20 item 7). Data objects
/// are appended back to back; anything between them would break the range, so
/// `extend` refuses it.
struct DataRegion {
    start: u64,
    end: u64,
    hasher: StoredBytesHasher<StoredObjectScope>,
}

impl DataRegion {
    fn start(offset: u64, stored: &StoredObject) -> Self {
        let mut hasher = StoredBytesHasher::stored_object();
        hasher.update(stored.view());
        DataRegion {
            start: offset,
            end: offset.saturating_add(stored.len()),
            hasher,
        }
    }

    fn extend(&mut self, offset: u64, stored: &StoredObject) -> Result<()> {
        if offset != self.end {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "internal: a data object was not appended where the previous one ended",
            ));
        }
        self.hasher.update(stored.view());
        self.end = self.end.saturating_add(stored.len());
        Ok(())
    }

    fn finish(&self) -> ObjectRef {
        ObjectRef {
            offset: self.start,
            stored_len: self.end - self.start,
            stored_hash: self.hasher.finalize(),
        }
    }
}

type DedupKey = (u64, ChunkContentHash);

/// The dedup index for a writer at `catalog`'s head: every unprotected,
/// dependency-free, located chunk the head's namespace reaches.
///
/// **Why only reachable chunks.** Baseline recovery (D10.8) replays the
/// segment from the base's snapshot manifest alone, which holds only the
/// chunks its namespace reaches (`Manifest::snapshot_from_catalog`); the
/// catalog image holds every chunk ever stored. A delta that referenced a
/// chunk reachable at no point a baseline replay sees would open through the
/// image but fail baseline recovery. What a segment's replay sees is S(*b*)
/// plus every chunk introduced since *b*; the head's reachable chunks are a
/// subset of that in any archive whose baseline recovery works, and the
/// writer adds each delta's own chunks after publishing it and rebuilds
/// after each checkpoint. Content of a file deleted before the base is
/// therefore stored again if it returns, which costs space, never recovery.
fn dedup_index_at_head(catalog: &Catalog) -> Result<HashMap<DedupKey, ObjectId>> {
    let mut reached = std::collections::BTreeSet::new();
    for (_, entry) in catalog.replay(None)?.iter() {
        let Some((_, extents)) = catalog.file_version(&entry.version)? else {
            return Err(MochiError::new(
                ErrorCode::CatalogInvalid,
                "the head names a version the catalog does not hold",
            ));
        };
        for e in extents {
            if let ExtentSource::Chunk { chunk, .. } = e.source {
                reached.insert(chunk);
            }
        }
    }
    let mut index = HashMap::new();
    for id in reached {
        let Some(record) = catalog.object(&id)? else {
            return Err(MochiError::new(
                ErrorCode::CatalogInvalid,
                "an extent names a chunk the catalog does not hold",
            ));
        };
        // Deduplication under encryption keeps working (D20 item 8): equality
        // is judged by the plaintext hash in the sealed catalog, and a
        // duplicate reuses the existing sealed chunk. Every chunk of an
        // archive has its profile's protection, so none is skipped for it.
        if !record.dependencies.is_empty() || catalog.object_location(&id)?.is_none() {
            continue;
        }
        index
            .entry((record.decoded_len, record.content_hash))
            .or_insert(id);
    }
    Ok(index)
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
    /// Creation only (D13): publishing the temporary file at its final name.
    pub const PUBLISH: &str = "publish";
}

/// What `open_locked` hands to `open_append`: the verified head, the
/// recorded writer parameters, the head snapshot's promised attributes, and
/// any audited tail truncation.
type OpenedForAppend = (
    OpenedHead,
    WriterParams,
    SegmentState,
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
    /// Retention state at `head` (spec Annex B D18; manifests only).
    retention: RetentionState,
    /// Written with the first commit; `None` once a head exists.
    new_descriptor: Option<Descriptor>,
    /// `EveryCommit` unless a test changed it (see [`CheckpointPolicy`]).
    policy: CheckpointPolicy,
    /// Set by [`ArchiveWriter::request_checkpoint`]; cleared when a commit
    /// is published.
    checkpoint_requested: bool,
    /// Test control: damage what a checkpoint serializes (see
    /// [`CheckpointTamper`]).
    #[cfg(any(test, feature = "test-controls"))]
    tamper: Option<CheckpointTamper>,
    /// Test control: damage the TAR stream a commit writes (see [`TarTamper`]).
    #[cfg(any(test, feature = "test-controls"))]
    tar_tamper: Option<TarTamper>,
    needs_directory_sync: bool,
    /// The TAR-compatible profile (descriptor constraint 0): every commit
    /// with a put also writes one TAR stream (Annex B.2.9 D19).
    tar: bool,
    dedup: Dedup,
    /// Dedup index over the head (see [`Dedup`]); `None` until first used.
    dedup_index: Option<HashMap<DedupKey, ObjectId>>,
    poisoned: Option<String>,
    audit: Vec<AuditEvent>,
    /// The Encrypted profile's key state (Annex B.2.10 D20); `None` for every
    /// other profile.
    crypto: Option<WriterCrypto>,
}

/// One key envelope valid at the head: its ID and where its frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnvelopeRef {
    id: [u8; 16],
    object: ObjectRef,
}

/// The writer's key state: the archive's data key, the random source every
/// nonce and identifier comes from, the key cost for new envelopes, and the
/// envelopes valid at the head, in increasing ID order.
struct WriterCrypto {
    key: std::sync::Arc<Unlocked>,
    rng: Box<dyn Random>,
    kdf: KdfParams,
    envelopes: Vec<EnvelopeRef>,
}

impl WriterCrypto {
    /// A fresh data key for a new archive: the key, its ID, and a source of
    /// randomness. The envelopes wrapping it are written by the first commit.
    fn create(archive_id: ArchiveId, kdf: KdfParams, mut rng: Box<dyn Random>) -> Result<Self> {
        let dek = DataKey::generate(rng.as_mut())?;
        let key_id = KeyId::generate(rng.as_mut())?;
        Ok(WriterCrypto {
            key: std::sync::Arc::new(Unlocked::new(archive_id, key_id, [0; 16], dek)),
            rng,
            kdf,
            envelopes: Vec::new(),
        })
    }
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

/// The profiles this build can write a new archive in: the default (Core),
/// the TAR-compatible profile (Annex B.2.9 D19), and the Encrypted profile
/// (Annex B.2.10 D20); the last two exclude each other.
fn check_create_profile(asked: Profile) -> Result<()> {
    if asked.encrypted && asked.tar_compatible {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "the Encrypted and TAR-compatible profiles cannot be combined: generic tools \
             cannot extract an encrypted archive (Annex B.2.10 D20 item 4)",
        ));
    }
    Ok(())
}

/// D12: a profile is fixed at creation, so an append that asks for a
/// different one is refused, whatever this build can write; then an archive
/// whose profile this build cannot write is refused too.
fn check_append_profile(descriptor: &Descriptor, asked: Option<Profile>) -> Result<()> {
    let have = descriptor.profile();
    if let Some(asked) = asked {
        let mut changes = Vec::new();
        if asked.encrypted != have.encrypted {
            changes.push(if asked.encrypted {
                "enabling the Encrypted profile"
            } else {
                "removing the Encrypted profile"
            });
        }
        if asked.tar_compatible != have.tar_compatible {
            changes.push(if asked.tar_compatible {
                "enabling the TAR-compatible profile"
            } else {
                "removing the TAR-compatible profile"
            });
        }
        if !changes.is_empty() {
            return Err(MochiError::new(
                ErrorCode::ProfileChangeUnsupported,
                format!(
                    "{} in place is not supported: an archive's profile is fixed at creation \
                     (spec D12). Write a new archive with the profile you want.",
                    changes.join(" and ")
                ),
            ));
        }
    }
    Ok(())
}

/// Writes the sidecar for a tail (D14): see [`ArchiveWriter::open_append_in`].
type Quarantiner<'a> =
    &'a mut dyn FnMut(&dyn ReadStorage, &SidecarMetadata) -> Result<QuarantineRecord>;

/// What `open_locked` does with an eligible tail.
enum TailAction<'a> {
    Refuse,
    Truncate {
        quarantine: Option<Quarantiner<'a>>,
        waivers: TruncationWaivers,
    },
}

/// D14 quarantine: exclusive sidecar, exact copy, sync, re-read and hash
/// compare, directory flush. On any failure the partial sidecar is removed
/// (so a retry is not refused by its own leftover) and nothing is truncated.
fn quarantine_tail<D: StorageDir>(
    dir: &mut D,
    name: &str,
    src: &dyn ReadStorage,
    meta: &SidecarMetadata,
    waivers: TruncationWaivers,
) -> Result<QuarantineRecord> {
    use crate::quarantine::{for_each_chunk, read_sidecar, sidecar_name};
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        MochiError::new(
            ErrorCode::QuarantineFailed,
            format!("quarantine failed ({what}: {e}); nothing was truncated"),
        )
    };
    let sidecar = sidecar_name(name, meta.tail_offset, &meta.tail_hash);
    let mut f = dir.create_exclusive(&sidecar).map_err(|e| match e {
        StorageError::Exists { .. } => MochiError::new(
            ErrorCode::QuarantineFailed,
            format!(
                "the quarantine sidecar {sidecar:?} already exists and is never replaced \
                 (D14); nothing was truncated. Inspect or move it, then retry."
            ),
        ),
        other => failed("creating the sidecar", &other),
    })?;
    let written = (|| -> Result<()> {
        let header = meta.header()?;
        f.append(&header)?;
        for_each_chunk(src, meta.tail_offset, meta.tail_len, |c| {
            f.append(c)?;
            Ok(())
        })?;
        f.sync_data()?;
        let (back, _) = read_sidecar(&f)?;
        if back != *meta {
            return Err(MochiError::new(
                ErrorCode::QuarantineFailed,
                "the sidecar read back differs from what was written",
            ));
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = dir.discard(f, &sidecar);
        return Err(failed("writing and verifying the sidecar", &e));
    }
    let _ = f.unlock();
    drop(f);
    let directory = match dir.sync_directory() {
        Ok(d) => d,
        Err(e) => {
            let _ = dir.remove_if_unlocked(&sidecar);
            return Err(failed("flushing the directory", &e));
        }
    };
    if let DirectoryDurability::Unconfirmed(why) = &directory {
        if !waivers.accept_unconfirmed_durability {
            let _ = dir.remove_if_unlocked(&sidecar);
            return Err(MochiError::new(
                ErrorCode::DurabilityUnconfirmed,
                format!(
                    "the quarantine sidecar's directory entry is not confirmed durable ({why}); \
                     nothing was truncated. Pass --accept-unconfirmed-durability to proceed \
                     (the sidecar is still written, verified, and synced)."
                ),
            ));
        }
    }
    Ok(QuarantineRecord {
        sidecar,
        tail_hash: meta.tail_hash,
        directory,
    })
}

/// The temporary name a D13 creation of `name` writes to.
pub fn temporary_name(name: &str, tag: &[u8; 32]) -> String {
    let hex: String = tag[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!(".{name}.{hex}.mochi-tmp")
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
        let mut dedup = Dedup::Off;
        let mut crypto = None;
        let result = (|| -> Result<(ArchiveId, WriterParams, Catalog)> {
            if storage.size()? != 0 {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "create needs empty storage; use open_append for an existing archive",
                ));
            }
            let profile = opts.profile.unwrap_or_default();
            check_create_profile(profile)?;
            dedup = opts.dedup.resolve(profile.tar_compatible)?;
            let params = WriterParams {
                chunk_size: opts.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE),
                zstd_level: opts.zstd_level.unwrap_or(EncodeParams::default().level),
            };
            params.validate(&opts.read.limits)?;
            let archive_id = ArchiveId::generate(ids.as_mut())?;
            if profile.encrypted {
                if opts.read.keys.is_none() {
                    return Err(MochiError::new(
                        ErrorCode::InvalidArgument,
                        "an Encrypted archive needs a passphrase to wrap its data key",
                    ));
                }
                let kdf = opts.kdf.unwrap_or(KdfParams::WRITER_DEFAULT);
                kdf.check(&Limits::WRITER_DEFAULT)
                    .map_err(MochiError::from)?;
                crypto = Some(WriterCrypto::create(archive_id, kdf, Box::new(OsRandom))?);
            }
            Ok((archive_id, params, Catalog::new_working()?))
        })();
        let policy = CheckpointPolicy::Trigger(opts.checkpoint_trigger.unwrap_or_default());
        let tar = opts.profile.is_some_and(|p| p.tar_compatible);
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
            // The Encrypted profile records no commit times (D20 item 6).
            record_time: opts.record_time && crypto.is_none(),
            archive_id,
            head: None,
            catalog,
            attributes: BTreeMap::new(),
            retention: RetentionState::default(),
            // The default profile is not TAR-compatible (spec D4); choosing
            // it is explicit and fixed at creation (D12). The Encrypted
            // profile is the required feature of D20 item 4.
            new_descriptor: Some(if crypto.is_some() {
                Descriptor::new_encrypted(archive_id)
            } else {
                Descriptor::new(archive_id, tar)
            }),
            policy,
            checkpoint_requested: false,
            #[cfg(any(test, feature = "test-controls"))]
            tamper: None,
            #[cfg(any(test, feature = "test-controls"))]
            tar_tamper: None,
            needs_directory_sync: true,
            tar,
            dedup,
            dedup_index: None,
            poisoned: None,
            audit: Vec::new(),
            crypto,
        })
    }

    /// Create the archive `name` in `dir` by the Annex B.2 D13 mechanism:
    ///
    /// 1. an exclusively created, locked temporary file in `dir`
    ///    (`.NAME.<16 hex>.mochi-tmp`, from a fresh ID);
    /// 2. the first commit, `first`, written and synced into it (the §12.2
    ///    steps; the directory is not flushed yet);
    /// 3. publication at `name` **without replacing** an existing file;
    /// 4. a flush of `dir`.
    ///
    /// Outcomes (D13; the C5 commit outcomes otherwise):
    ///
    /// | Where it stops | Result | At `name` | Temporary file |
    /// |---|---|---|---|
    /// | `name` already exists | `DESTINATION_EXISTS` | unchanged | removed |
    /// | any failure before publication (including a commit failure) | that error; a commit-unconfirmed failure becomes `IO_ERROR`, because nothing was published | nothing | removed (or left for cleanup if removal fails) |
    /// | directory flush `Unconfirmed` | `LOCAL_COMMITTED`, durability `DirectoryUnconfirmed` (report `DEGRADED`) | the archive | gone |
    /// | directory flush error | `COMMIT_UNCONFIRMED`; the writer is poisoned | the archive, left in place | gone |
    /// | success | `LOCAL_COMMITTED`, `Durable` | the archive | gone |
    ///
    /// A crash leaves either nothing at `name` (and possibly the temporary
    /// file, which nobody holds and cleanup may remove) or the complete,
    /// synced first commit at `name`. The returned writer holds the
    /// archive's publication lock. Overwriting is not offered here: D13
    /// requires an explicit request and a separate code path.
    pub fn create_in<D: StorageDir<File = S>>(
        dir: &mut D,
        name: &str,
        ids: Box<dyn IdSource>,
        opts: WriterOptions,
        first: Transaction,
        ctx: &JobContext<'_>,
    ) -> Result<(Self, CommitOutcome)> {
        let (w, mut outcome, durability) =
            Self::build_in(dir, name, ids, opts, ctx, |w| w.commit(first, ctx))?;
        if let Some(d) = durability {
            outcome.durability = d;
        }
        Ok((w, outcome))
    }

    /// [`ArchiveWriter::create_in`] with any number of commits: `build`
    /// runs against the writer on the temporary file (it may commit several
    /// times and check what it wrote), and only if it succeeds is the file
    /// published at `name`, without replacing anything, and the directory
    /// flushed. Every outcome in `create_in`'s table holds, with "the first
    /// commit" read as "everything `build` wrote": a crash or any failure
    /// before publication leaves nothing at `name`. Compaction (C9) builds a
    /// whole new archive this way. Returns the writer, `build`'s result, and
    /// `Some` durability when the directory flush was unconfirmed.
    pub fn build_in<D: StorageDir<File = S>, T>(
        dir: &mut D,
        name: &str,
        mut ids: Box<dyn IdSource>,
        opts: WriterOptions,
        ctx: &JobContext<'_>,
        build: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<(Self, T, Option<PublishDurability>)> {
        check_file_name(name)?;
        let tag = ids.next_id()?;
        let temp = temporary_name(name, &tag);
        let nothing_created = |e: MochiError| {
            let code = match e.code {
                // Nothing was published: the outcome is known.
                ErrorCode::CommitUnconfirmed | ErrorCode::WriterPoisoned => ErrorCode::IoError,
                c => c,
            };
            MochiError::new(
                code,
                format!("{}; nothing was created at {name:?}", e.message),
            )
        };

        let file = dir.create_exclusive(&temp)?;
        let mut w = match Self::create(file, ids, opts) {
            Ok(w) => w,
            Err(e) => {
                let _ = dir.remove_if_unlocked(&temp);
                return Err(nothing_created(e));
            }
        };
        // The new entry is flushed once, after publication (step 4).
        w.needs_directory_sync = false;
        let built = match build(&mut w) {
            Ok(t) => t,
            Err(e) => {
                drop(w);
                let _ = dir.remove_if_unlocked(&temp);
                return Err(nothing_created(e));
            }
        };
        if w.head.is_none() {
            drop(w);
            let _ = dir.remove_if_unlocked(&temp);
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                format!("nothing was committed, so nothing was created at {name:?}"),
            ));
        }

        ctx.report(phase::PUBLISH, 0, None);
        if let Err(e) = dir.publish_archive(&mut w.storage, &temp, name) {
            drop(w);
            let _ = dir.remove_if_unlocked(&temp);
            return Err(match e {
                StorageError::Exists { .. } => MochiError::new(
                    ErrorCode::DestinationExists,
                    format!(
                        "{name:?} already exists; creating an archive never replaces a file \
                         (D13). Nothing was changed."
                    ),
                ),
                StorageError::LockHeld => MochiError::new(
                    ErrorCode::LockConflict,
                    format!(
                        "another writer holds the publication lock of {name:?}; nothing was \
                         created (spec §12.5: fail on conflict)"
                    ),
                ),
                other => nothing_created(other.into()),
            });
        }

        ctx.report(phase::DIRECTORY, 0, None);
        let durability = match dir.sync_directory() {
            Ok(DirectoryDurability::Confirmed) => None,
            Ok(DirectoryDurability::Unconfirmed(why)) => {
                Some(PublishDurability::DirectoryUnconfirmed(why))
            }
            Err(e) => {
                let msg = format!(
                    "the archive was published at {name:?}, but persisting its directory entry \
                     failed ({e}); it may not survive a power loss. The file is left in place."
                );
                w.poison(msg.clone());
                return Err(MochiError::new(ErrorCode::CommitUnconfirmed, msg));
            }
        };
        Ok((w, built, durability))
    }

    /// The storage this writer holds (and locks). Read access only.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// The writer parameters recorded at creation: chunk size and zstd
    /// level, as [`WriterOptions`] takes them.
    pub fn recorded_parameters(&self) -> (u64, i32) {
        (self.params.chunk_size, self.params.zstd_level)
    }

    /// Open an existing archive for appending (§12.2 steps 1–2).
    pub fn open_append(
        storage: S,
        ids: Box<dyn IdSource>,
        opts: WriterOptions,
        tail: TailPolicy,
    ) -> Result<(Self, Option<TailTruncation>)> {
        let action = match tail {
            TailPolicy::Refuse => TailAction::Refuse,
            TailPolicy::TruncateWithoutQuarantine => TailAction::Truncate {
                quarantine: None,
                waivers: TruncationWaivers {
                    no_quarantine: true,
                    accept_unconfirmed_durability: false,
                },
            },
        };
        Self::open_with(storage, ids, opts, action)
    }

    /// Open the archive `name` in `dir` for appending. With
    /// [`TailRepair::Truncate`], an eligible tail is first quarantined
    /// (Annex B.2 D14, plan T24): copied exactly into the no-clobber sidecar
    /// `<name>.tail-<offset>-<16 hex>.mochiq`, synced, re-read and checked
    /// against the tail's hash, and the directory flushed; only then is the
    /// tail truncated. Any quarantine failure is `QUARANTINE_FAILED` and
    /// truncates nothing; an existing sidecar is refused, never replaced. A
    /// directory flush that is not confirmed is `DURABILITY_UNCONFIRMED`
    /// (and truncates nothing) unless `accept_unconfirmed_durability` is
    /// set. Every waiver used is in the returned record
    /// ([`TailTruncation::findings`]).
    pub fn open_append_in<D: StorageDir<File = S>>(
        dir: &mut D,
        name: &str,
        ids: Box<dyn IdSource>,
        opts: WriterOptions,
        tail: TailRepair,
    ) -> Result<(Self, Option<TailTruncation>)> {
        check_file_name(name)?;
        let storage = dir.open(name)?;
        match tail {
            TailRepair::Refuse => Self::open_with(storage, ids, opts, TailAction::Refuse),
            TailRepair::Truncate(waivers) if waivers.no_quarantine => Self::open_with(
                storage,
                ids,
                opts,
                TailAction::Truncate {
                    quarantine: None,
                    waivers,
                },
            ),
            TailRepair::Truncate(waivers) => {
                let mut q = |src: &dyn ReadStorage, meta: &SidecarMetadata| {
                    quarantine_tail(dir, name, src, meta, waivers)
                };
                Self::open_with(
                    storage,
                    ids,
                    opts,
                    TailAction::Truncate {
                        quarantine: Some(&mut q),
                        waivers,
                    },
                )
            }
        }
    }

    fn open_with(
        mut storage: S,
        ids: Box<dyn IdSource>,
        opts: WriterOptions,
        tail: TailAction<'_>,
    ) -> Result<(Self, Option<TailTruncation>)> {
        lock(&mut storage)?;
        match Self::open_locked(&mut storage, &opts, tail) {
            Ok((head, params, state, truncation)) => {
                // The profile is the archive's own (D12); an append that asks
                // for another was refused in `open_locked`.
                let tar = head.descriptor.tar_compatible;
                let dedup = match opts.dedup.resolve(tar) {
                    Ok(d) => d,
                    Err(e) => {
                        let _ = storage.unlock();
                        return Err(e);
                    }
                };
                let mut audit = Vec::new();
                if let Some(t) = &truncation {
                    audit.push(AuditEvent::TailTruncated(t.clone()));
                }
                // The Encrypted profile (D20): the head open unlocked the data
                // key; the writer keeps it and the envelopes valid at the head.
                let crypto = if head.descriptor.encrypted() {
                    let built = (|| -> Result<WriterCrypto> {
                        let key = head.unlocked.clone().ok_or_else(|| {
                            MochiError::new(
                                ErrorCode::KeyUnavailable,
                                "this archive is encrypted: a passphrase is required to append",
                            )
                        })?;
                        let envelopes = read_envelopes(
                            &storage,
                            &head.commit,
                            head.location.footer.fields.commit_offset,
                            &opts.read,
                        )?
                        .into_iter()
                        .map(|(object, e)| EnvelopeRef {
                            id: e.envelope_id,
                            object,
                        })
                        .collect();
                        Ok(WriterCrypto {
                            key,
                            rng: Box::new(OsRandom),
                            kdf: opts.kdf.unwrap_or(KdfParams::WRITER_DEFAULT),
                            envelopes,
                        })
                    })();
                    match built {
                        Ok(c) => Some(c),
                        Err(e) => {
                            let _ = storage.unlock();
                            return Err(e);
                        }
                    }
                } else {
                    None
                };
                Ok((
                    ArchiveWriter {
                        storage,
                        ids,
                        read: opts.read,
                        params,
                        record_time: opts.record_time && crypto.is_none(),
                        archive_id: head.commit.archive_id,
                        head: Some(WriterHead {
                            seq: head.commit.seq,
                            commit_id: head.commit_id,
                            footer_offset: head.location.footer.footer_offset,
                            committed_len: head.location.committed_len,
                            delta_manifest_hash: head.commit.delta_manifest.stored_hash,
                            descriptor: head.commit.descriptor,
                            next_base: next_base_after(&head),
                            delta_bytes: head.segment.delta_bytes,
                            base_bytes: head.segment.base_bytes,
                        }),
                        catalog: head.catalog,
                        attributes: state.attributes,
                        retention: state.retention,
                        new_descriptor: None,
                        policy: CheckpointPolicy::Trigger(
                            opts.checkpoint_trigger.unwrap_or_default(),
                        ),
                        checkpoint_requested: false,
                        #[cfg(any(test, feature = "test-controls"))]
                        tamper: None,
                        #[cfg(any(test, feature = "test-controls"))]
                        tar_tamper: None,
                        needs_directory_sync: false,
                        tar,
                        dedup,
                        dedup_index: None,
                        poisoned: None,
                        audit,
                        crypto,
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
        tail: TailAction<'_>,
    ) -> Result<OpenedForAppend> {
        let mut location = locate_head(storage, &opts.read.limits)?;
        // D12, before anything is written (a tail truncation included):
        // append needs a valid descriptor bound to the head, and refuses a
        // profile change and a profile this build cannot write. open_at
        // checks the descriptor again on the same head.
        let (head_commit, head_id) = read_commit(storage, &location.footer, &opts.read)?;
        let descriptor = read_descriptor(
            storage,
            &head_commit,
            location.footer.fields.commit_offset,
            &opts.read,
        )?;
        check_append_profile(&descriptor, opts.profile)?;
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
            (TailState::Uncommitted { len, .. }, TailAction::Refuse) => {
                return Err(MochiError::new(
                    ErrorCode::UncommittedTail,
                    format!(
                        "{len} bytes follow the last valid commit; they look like an interrupted \
                         write (eligible for explicit truncation, D14), and appending needs them \
                         removed first"
                    ),
                ));
            }
            (
                TailState::Uncommitted {
                    len,
                    frames,
                    incomplete_final_frame,
                },
                TailAction::Truncate {
                    quarantine,
                    waivers,
                },
            ) => {
                // D14: the tail is copied, verified, and synced (unless the
                // waiver says otherwise) *before* a byte is removed.
                let tail_hash =
                    crate::quarantine::hash_range(&*storage, location.committed_len, *len)?;
                let meta = SidecarMetadata {
                    archive_id: head_commit.archive_id,
                    head_commit_id: head_id,
                    head_seq: head_commit.seq,
                    tail_offset: location.committed_len,
                    tail_len: *len,
                    tail_hash,
                    frame_magics: frames.iter().filter_map(|k| k.magic()).collect(),
                    incomplete_final_frame: *incomplete_final_frame,
                    tool: format!("{} {}", crate::TOOL_NAME, crate::TOOL_VERSION),
                    time: crate::timestamp::Timestamp::now()
                        .map_err(|e| {
                            MochiError::new(
                                ErrorCode::QuarantineFailed,
                                format!("no valid time for the sidecar: {}", e.message),
                            )
                        })?
                        .to_string(),
                };
                let mut used = Vec::new();
                let record = match (quarantine, waivers.no_quarantine) {
                    (Some(q), false) => {
                        let r = q(&*storage, &meta)?;
                        if let DirectoryDurability::Unconfirmed(why) = &r.directory {
                            used.push(Waiver::AcceptUnconfirmedDurability { why: why.clone() });
                        }
                        Some(r)
                    }
                    (_, true) => {
                        used.push(Waiver::NoQuarantine);
                        None
                    }
                    (None, false) => {
                        return Err(MochiError::new(
                            ErrorCode::InvalidArgument,
                            "internal: truncation without a quarantine and without the waiver",
                        ))
                    }
                };
                let t = TailTruncation {
                    committed_len: location.committed_len,
                    removed_len: *len,
                    removed_frames: frames.clone(),
                    incomplete_final_frame: *incomplete_final_frame,
                    head_seq: head_commit.seq,
                    head_commit_id: head_id,
                    quarantine: record,
                    waivers: used,
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
        let Opened { head, state } = open_at(storage, location, &opts.read, OpenMode::Append)?;
        // The next snapshot must carry every reachable version's promised
        // attributes, which only snapshot manifests (and the deltas after
        // them) hold until C6 moves them into the catalog. open_at refused
        // already if they could not be reconstructed.
        let state = state.ok_or_else(|| {
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
        Ok((head, params, state, truncation))
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

    /// Make the next published commit a checkpoint, whatever the policy (a
    /// forced checkpoint; the `checkpoint` command). It resets the base like
    /// any checkpoint (B.2.3). The request stays until a commit is
    /// published, so a commit that fails or is cancelled does not use it up.
    pub fn request_checkpoint(&mut self) {
        self.checkpoint_requested = true;
    }

    /// Whether the next commit will be a checkpoint because of
    /// [`request_checkpoint`](Self::request_checkpoint).
    pub fn checkpoint_requested(&self) -> bool {
        self.checkpoint_requested
    }

    /// The checkpoint trigger's Δ and *B* at the current head (B.2.3), or
    /// `None` before the first commit.
    pub fn trigger_accounting(&self) -> Option<(u64, u64)> {
        self.head.map(|h| (h.delta_bytes, h.base_bytes))
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
            delta_bytes: prepared.delta_bytes,
            base_bytes: prepared.base_bytes,
        });
        self.checkpoint_requested = false;
        // A checkpoint's snapshot manifest holds only what its namespace
        // reaches, so the index is rebuilt from it; after a delta, the
        // delta's own chunks join (see `dedup_index_at_head`).
        if prepared.checkpoint {
            self.dedup_index = None;
        } else if let Some(index) = self.dedup_index.as_mut() {
            for (key, id) in prepared.new_chunks {
                index.entry(key).or_insert(id);
            }
        }
        self.catalog = prepared.catalog;
        self.attributes = prepared.attributes;
        self.retention = prepared.retention;
        self.new_descriptor = None;
        if let Some(c) = self.crypto.as_mut() {
            c.envelopes = prepared.envelopes;
            // Reading this archive back in this process needs no second
            // derivation: the session learns the key under each envelope.
            if let Some(keys) = &self.read.keys {
                let ids: Vec<[u8; 16]> = c.envelopes.iter().map(|e| e.id).collect();
                keys.register_envelopes(c.key.clone(), &ids);
            }
        }
        Ok(CommitOutcome {
            status: CommitStatus::LocalCommitted,
            durability,
            seq: prepared.seq,
            commit_id: prepared.commit_id,
            footer_offset,
            committed_len,
            objects_written: prepared.objects,
            dedup: prepared.dedup,
            checkpoint: prepared.checkpoint,
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
        // Retention operations are checked before anything is written: an
        // invalid one is the caller's error, and the archive is untouched.
        if tx.provenance.is_some() && seq != 0 {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "provenance belongs to a new archive's first commit",
            ));
        }
        let mut retention = self.retention.clone();
        retention.apply(&tx.retention, seq).map_err(|e| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                format!("retention: {}", e.message),
            )
        })?;
        if self.crypto.is_none() && (!tx.key_adds.is_empty() || !tx.key_removes.is_empty()) {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "key envelopes belong to the Encrypted profile; this archive is not encrypted",
            ));
        }
        // The Encrypted profile's transaction ID is drawn first, because the
        // key envelopes this commit writes carry it (D11 identity) and are
        // written before any data. Other profiles draw it after content, as
        // they always have.
        let early_tx = if self.crypto.is_some() {
            Some(self.draw_transaction_id()?)
        } else {
            None
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
        let mut new_chunks = Vec::new();
        let mut dedup = DedupStats::default();
        let mut versions = Vec::new();
        let mut attributes = self.attributes.clone();
        // TAR-compatible profile (Annex B.2.9 D19): framing bytes waiting for
        // the next data frame, and the members written in this commit.
        let tar = self.tar;
        let mut pending: Vec<u8> = Vec::new();
        let mut members = 0u64;

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

        // Key envelopes (Encrypted profile, D20 items 3 and 10): the first
        // commit wraps the data key under each supplied passphrase; a rewrap
        // adds and removes envelopes. Written before any data, so a commit's
        // data region is one contiguous range. The commit lists the complete
        // resulting set, in increasing envelope-ID order.
        let mut key_ops: Vec<KeyOp> = Vec::new();
        let mut envelopes: Vec<EnvelopeRef> = Vec::new();
        if let (Some(tx_id), true) = (early_tx, self.crypto.is_some()) {
            envelopes = self.prepare_envelopes(seq, tx_id, &tx, &mut key_ops)?;
        }
        let mut region: Option<DataRegion> = None;

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
                    if tar {
                        let m = self.tampered_member(
                            member_for(
                                path.as_stored(),
                                EntryKind::File,
                                content.len() as u64,
                                attrs,
                            ),
                            members,
                        );
                        pending.extend(encode_header(&m)?);
                        self.flush_framing(
                            &mut pending,
                            &mut cat,
                            &mut chunks,
                            &mut objects,
                            &encode,
                        )?;
                        members += 1;
                    }
                    let piece = usize::try_from(self.params.chunk_size).unwrap_or(usize::MAX);
                    for (i, part) in content.chunks(piece).enumerate() {
                        let decoded = DecodedBytes::new(part.to_vec());
                        let key = (decoded.len(), chunk_content_hash(&decoded));
                        let chunk = match self.reusable_chunk(&key, part, &mut dedup)? {
                            Some(id) => {
                                dedup.chunks_reused += 1;
                                dedup.bytes_reused += decoded.len();
                                id
                            }
                            None => {
                                let obj = self.build_chunk(&decoded, &encode)?;
                                let offset = self.append_data(&obj.stored, &mut region)?;
                                cat.insert_object(&obj.record, Some(offset))?;
                                let id = obj.record.id;
                                new_chunks.push((key, id));
                                chunks.push(ChunkEntry {
                                    record: obj.record,
                                    location: Some(offset),
                                });
                                objects += 1;
                                id
                            }
                        };
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
                                chunk,
                                chunk_offset: 0,
                            },
                        });
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
                    if tar {
                        // The content chunks above are the member's bytes.
                        pending = vec![0; tar::padding(content.len() as u64)];
                    }
                }
                TxEntry::Dir {
                    path,
                    attributes: attrs,
                } => {
                    if tar {
                        let m = self.tampered_member(
                            member_for(path.as_stored(), EntryKind::Directory, 0, attrs),
                            members,
                        );
                        if !self.drops_directory_member() {
                            pending.extend(encode_header(&m)?);
                        }
                        self.flush_framing(
                            &mut pending,
                            &mut cat,
                            &mut chunks,
                            &mut objects,
                            &encode,
                        )?;
                        members += 1;
                    }
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
                TxEntry::Copied {
                    path,
                    version: entry,
                    chunks: stored,
                } => {
                    let id = entry.version.id;
                    let held = cat.file_version(&id)?;
                    if tar {
                        let m = self.tampered_member(
                            member_for(
                                path.as_stored(),
                                entry.version.kind,
                                entry.version.logical_len,
                                &entry.attributes,
                            ),
                            members,
                        );
                        pending.extend(encode_header(&m)?);
                        self.flush_framing(
                            &mut pending,
                            &mut cat,
                            &mut chunks,
                            &mut objects,
                            &encode,
                        )?;
                        members += 1;
                    }
                    match held {
                        Some((v, ex)) => {
                            if v != entry.version || ex != entry.extents {
                                return Err(MochiError::new(
                                    ErrorCode::InvalidArgument,
                                    format!("copied version {id:?} differs from the one held"),
                                ));
                            }
                            // Its chunks are old frames: the member's bytes
                            // are written again (D19 rule 6).
                            if tar {
                                self.reemit_content(
                                    &ex,
                                    &mut cat,
                                    &mut chunks,
                                    &mut objects,
                                    &encode,
                                    ctx,
                                )?;
                            }
                        }
                        None => {
                            // In the TAR profile the version's own chunk
                            // frames are the member's content, so they go in
                            // extent order and nothing else may land between.
                            let ordered: Vec<&(ObjectRecord, StoredObject)> = if tar {
                                order_copied_chunks(entry, stored, &cat)?
                            } else {
                                stored.iter().collect()
                            };
                            for (record, bytes) in ordered {
                                if cat.object(&record.id)?.is_some() {
                                    continue;
                                }
                                verify_stored(record, bytes)?;
                                // Into an Encrypted archive a chunk arrives as
                                // its plain Zstandard frame and is sealed anew:
                                // the new archive has its own ID and key (D20
                                // item 10), so no sealed byte is carried over.
                                let (record, bytes) = self.seal_copied(record, bytes)?;
                                let offset = self.append_data(&bytes, &mut region)?;
                                cat.insert_object(&record, Some(offset))?;
                                chunks.push(ChunkEntry {
                                    record,
                                    location: Some(offset),
                                });
                                objects += 1;
                                ctx.check_cancelled()?;
                            }
                            cat.insert_file_version(&entry.version, &entry.extents)?;
                            versions.push(entry.clone());
                        }
                    }
                    if tar {
                        pending = vec![0; tar::padding(entry.version.logical_len)];
                    }
                    if let Some(a) = attributes.insert(id, entry.attributes) {
                        if a != entry.attributes {
                            return Err(MochiError::new(
                                ErrorCode::InvalidArgument,
                                format!("copied version {id:?} has other promised attributes"),
                            ));
                        }
                    }
                    ops.push(NamespaceOp::Put {
                        path: path.clone(),
                        version: id,
                    });
                }
                TxEntry::Rename { from, to } => {
                    let version = before.get(from).map(|e| e.version).ok_or_else(|| {
                        MochiError::new(
                            ErrorCode::NamespaceInvalid,
                            "rename source does not exist in the head snapshot",
                        )
                    })?;
                    if tar {
                        // A rename puts an existing version at a new path:
                        // its member's bytes are written again (D19 rule 6).
                        let (v, extents) = cat.file_version(&version)?.ok_or_else(|| {
                            MochiError::new(
                                ErrorCode::CatalogInvalid,
                                "the renamed version is not in the catalog",
                            )
                        })?;
                        let attrs = attributes.get(&version).copied().unwrap_or_default();
                        let m = self.tampered_member(
                            member_for(to.as_stored(), v.kind, v.logical_len, &attrs),
                            members,
                        );
                        pending.extend(encode_header(&m)?);
                        self.flush_framing(
                            &mut pending,
                            &mut cat,
                            &mut chunks,
                            &mut objects,
                            &encode,
                        )?;
                        members += 1;
                        self.reemit_content(
                            &extents,
                            &mut cat,
                            &mut chunks,
                            &mut objects,
                            &encode,
                            ctx,
                        )?;
                        pending = vec![0; tar::padding(v.logical_len)];
                    }
                    ops.push(NamespaceOp::Delete { path: from.clone() });
                    ops.push(NamespaceOp::Put {
                        path: to.clone(),
                        version,
                    });
                }
            }
        }

        // The commit's stream ends with two zero blocks (D19 rule 3); a commit
        // without a put has no stream.
        if tar && members > 0 {
            self.tamper_end(&mut pending);
            self.flush_framing(&mut pending, &mut cat, &mut chunks, &mut objects, &encode)?;
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
        let transaction_id = match early_tx {
            Some(t) => t,
            None => self.draw_transaction_id()?,
        };

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
            required_features: self.profile_features(),
            retention_ops: tx.retention.clone(),
            retention: Default::default(),
            provenance: tx.provenance.clone(),
            keys: crate::manifest::ManifestKeys {
                ops: key_ops.clone(),
                state: Vec::new(),
            },
        };
        manifest.canonicalize();
        let delta_manifest = self.store_manifest(&manifest, seq, transaction_id)?;
        ctx.check_cancelled()?;

        // Checkpoint or delta (D10). Commit 0 is always a checkpoint, and so
        // is a requested one; otherwise the policy decides (production: the
        // B.2.3 trigger on Δ and B as published so far, so a delta's replay
        // reads less than α·max(B, F) plus its own metadata).
        let checkpoint = match self.head {
            None => true,
            Some(h) => {
                self.checkpoint_requested
                    || self.policy.is_checkpoint(seq, h.delta_bytes, h.base_bytes)
            }
        };
        let (metadata, attributes) = if checkpoint {
            ctx.report(phase::CHECKPOINT, 0, None);
            #[allow(unused_mut)]
            let mut snapshot = Manifest::snapshot_from_catalog_in(
                &cat,
                self.archive_id,
                seq,
                transaction_id,
                &attributes,
                self.crypto
                    .as_ref()
                    .map(|_| envelopes.iter().map(|e| e.id).collect()),
            )?;
            snapshot.retention = retention.clone();
            // The source of truth for adoption (D10.7): the writer's own
            // state, never re-derived from what is about to be serialized.
            // Attributes are kept only for versions still reachable.
            let reachable = reachable_attributes(&after, attributes.clone())?;
            let source = AuthoritativeState::from_catalog(&cat, seq)?
                .with_attributes(reachable.clone())
                .with_retention(retention.clone());
            #[cfg(any(test, feature = "test-controls"))]
            if let Some(t) = self.tamper {
                t.apply_to_snapshot(&mut snapshot)?;
            }
            let snapshot_ref = self.store_manifest(&snapshot, seq, transaction_id)?;
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
            let image_ref = self.store_image(image.as_bytes(), identity)?;
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
                self.crypto.as_ref().map(|c| &*c.key),
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
        // The Encrypted profile records no commit time at all (D20 item 6):
        // not the clock, and not a caller's explicit time either.
        let time = match (tx.time, self.record_time, self.crypto.is_some()) {
            (_, _, true) => None,
            (Some(t), _, false) => Some(t),
            (None, true, false) => system_time(),
            (None, false, false) => None,
        };
        let data_region = region.as_ref().map(DataRegion::finish);
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
            required_features: self.profile_features(),
            time,
            descriptor,
            key_envelopes: envelopes.iter().map(|e| e.object).collect(),
            data_region,
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
        // B.2.3 accounting for the next decision. A checkpoint resets the
        // base; a delta adds its manifest, record, and footer. Generated
        // checkpoint bytes and data objects never count.
        let (delta_bytes, base_bytes) = match (record.metadata, self.head) {
            (Metadata::Checkpoint { image, snapshot }, _) => {
                (0, image.stored_len.saturating_add(snapshot.stored_len))
            }
            (Metadata::Delta { .. }, Some(h)) => (
                h.delta_bytes
                    .saturating_add(delta_manifest.stored_len)
                    .saturating_add(commit_frame.len())
                    .saturating_add(FOOTER_FRAME_LEN),
                h.base_bytes,
            ),
            (Metadata::Delta { .. }, None) => {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "internal: commit 0 must be a checkpoint",
                ))
            }
        };
        Ok(Prepared {
            seq,
            next_base,
            delta_bytes,
            base_bytes,
            commit_offset,
            commit_frame,
            commit_id,
            delta_manifest_hash: delta_manifest.stored_hash,
            descriptor,
            catalog: cat,
            attributes,
            objects,
            dedup,
            new_chunks,
            checkpoint,
            retention,
            envelopes,
        })
    }

    /// The required features every record this writer emits lists: the
    /// Encrypted profile's identifier, or none (D20 item 4).
    fn profile_features(&self) -> Vec<u64> {
        if self.crypto.is_some() {
            vec![FEATURE_ENCRYPTED]
        } else {
            Vec::new()
        }
    }

    /// A fresh transaction ID (RFC 9562 version 4) from the ID source.
    fn draw_transaction_id(&mut self) -> Result<[u8; 16]> {
        let mut txid = [0u8; 16];
        txid.copy_from_slice(&self.ids.next_id()?[..16]);
        Ok(uuid_v4(txid))
    }

    /// Encode one chunk: unprotected, or sealed for its own object ID in the
    /// Encrypted profile.
    fn build_chunk(
        &mut self,
        decoded: &DecodedBytes,
        encode: &EncodeParams,
    ) -> Result<crate::object::EncodedObject> {
        match self.crypto.as_mut() {
            Some(c) => build_object_sealed(
                decoded,
                encode,
                &c.key.context(),
                self.ids.as_mut(),
                c.rng.as_mut(),
                &self.read.limits,
            ),
            None => build_object(
                decoded,
                encode,
                Protection::None,
                self.ids.as_mut(),
                &self.read.limits,
            ),
        }
    }

    /// Append a data object and, in the Encrypted profile, extend the commit's
    /// **data region** (D20 item 7): data objects are contiguous, so one range
    /// and one hash cover them all.
    fn append_data(
        &mut self,
        stored: &StoredObject,
        region: &mut Option<DataRegion>,
    ) -> Result<u64> {
        let offset = self.storage.append(stored.as_bytes())?;
        if self.crypto.is_some() {
            match region {
                Some(r) => r.extend(offset, stored)?,
                None => *region = Some(DataRegion::start(offset, stored)),
            }
        }
        Ok(offset)
    }

    /// A chunk a rewrite hands over as its plain Zstandard frame, ready to
    /// store: sealed anew for this archive in the Encrypted profile, as it is
    /// otherwise.
    fn seal_copied(
        &mut self,
        record: &ObjectRecord,
        bytes: &StoredObject,
    ) -> Result<(ObjectRecord, StoredObject)> {
        match self.crypto.as_mut() {
            Some(c) => seal_object(
                record,
                bytes,
                &c.key.context(),
                c.rng.as_mut(),
                &Limits::WRITER_DEFAULT,
            ),
            None => Ok((record.clone(), bytes.clone())),
        }
    }

    /// Store a manifest: as a recovery-manifest frame, or in the Encrypted
    /// profile as a sealed object bound to its commit.
    fn store_manifest(
        &mut self,
        manifest: &Manifest,
        seq: u64,
        transaction_id: [u8; 16],
    ) -> Result<ObjectRef> {
        let frame = match self.crypto.as_mut() {
            None => manifest.to_stored()?,
            Some(c) => {
                let target = match manifest.kind {
                    ManifestKind::Delta => SealTarget::DeltaManifest {
                        sequence: seq,
                        transaction_id,
                    },
                    ManifestKind::Snapshot => SealTarget::SnapshotManifest {
                        sequence: seq,
                        transaction_id,
                    },
                };
                seal_record(&c.key, &target, &manifest.encode()?, c.rng.as_mut())?
            }
        };
        self.append_object(&frame)
    }

    /// Store a catalog image: as an image frame, or sealed in the Encrypted
    /// profile (the plaintext is the frame's payload, D20 item 5).
    fn store_image(&mut self, image: &[u8], identity: RecordIdentity) -> Result<ObjectRef> {
        let frame = encode_image_record(image, identity, self.crypto.is_some())?;
        let frame = match self.crypto.as_mut() {
            None => frame,
            Some(c) => {
                let target = SealTarget::Image {
                    sequence: identity.commit_sequence,
                    transaction_id: identity.transaction_id,
                };
                seal_record(&c.key, &target, image_payload(&frame)?, c.rng.as_mut())?
            }
        };
        self.append_object(&frame)
    }

    /// Write the key envelopes this commit introduces and return the complete
    /// set valid from it, in increasing envelope-ID order (D20 item 10). The
    /// first commit wraps the data key under every passphrase of the session;
    /// a rewrap adds the transaction's passphrases and removes its envelope
    /// IDs. Operations are recorded as the manifest's key operations: all
    /// additions, then all removals, so the set is never empty in between.
    fn prepare_envelopes(
        &mut self,
        seq: u64,
        transaction_id: [u8; 16],
        tx: &Transaction,
        key_ops: &mut Vec<KeyOp>,
    ) -> Result<Vec<EnvelopeRef>> {
        let creating = self.head.is_none();
        if creating && (!tx.key_adds.is_empty() || !tx.key_removes.is_empty()) {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "the first commit of an archive writes its key envelopes from the session's \
                 passphrases; a rewrap needs an existing archive",
            ));
        }
        let archive_id = self.archive_id;
        let initial: Vec<&Passphrase> = if creating {
            self.read
                .keys
                .as_ref()
                .map(|k| k.passphrases().iter().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let adds: Vec<&Passphrase> = if creating {
            initial
        } else {
            tx.key_adds.iter().map(|p| &**p).collect()
        };
        let Some(c) = self.crypto.as_mut() else {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "internal: key envelopes in an archive without keys",
            ));
        };
        let mut set = c.envelopes.clone();
        let mut written: Vec<([u8; 16], StoredObject)> = Vec::new();
        for p in adds {
            let e = KeyEnvelope::create(
                archive_id,
                seq,
                transaction_id,
                c.key.key_id,
                c.key.dek(),
                p,
                c.kdf,
                c.rng.as_mut(),
                &Limits::WRITER_DEFAULT,
            )?;
            key_ops.push(KeyOp::Add(e.envelope_id));
            written.push((e.envelope_id, e.to_stored()?));
        }
        for id in &tx.key_removes {
            if !set.iter().any(|e| e.id == *id) {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "the key envelope to remove is not valid at the head",
                ));
            }
            key_ops.push(KeyOp::Remove(*id));
        }
        set.retain(|e| !tx.key_removes.contains(&e.id));
        // Write the new frames now (the set needs their references).
        for (id, frame) in &written {
            set.push(EnvelopeRef {
                id: *id,
                object: ObjectRef {
                    offset: self.storage.append(frame.as_bytes())?,
                    stored_len: frame.len(),
                    stored_hash: stored_object_hash(frame.view()),
                },
            });
        }
        if set.is_empty() {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "an Encrypted archive keeps at least one key envelope",
            ));
        }
        if set.len() as u64 > Limits::WRITER_DEFAULT.max_key_envelopes {
            return Err(MochiError::new(
                ErrorCode::LimitExceeded,
                format!(
                    "an archive lists at most {} key envelopes",
                    Limits::WRITER_DEFAULT.max_key_envelopes
                ),
            ));
        }
        set.sort_by_key(|e| e.id);
        Ok(set)
    }

    /// A chunk at the published head holding exactly `part`, validated by
    /// read-back (see [`Dedup`]); `None` means store it. Only an I/O error
    /// or cancellation propagates: a candidate that fails any check is
    /// dropped from the index and counted, and the chunk is stored anew,
    /// which is always safe.
    fn reusable_chunk(
        &mut self,
        key: &DedupKey,
        part: &[u8],
        stats: &mut DedupStats,
    ) -> Result<Option<ObjectId>> {
        let Some(head) = self.head else {
            return Ok(None);
        };
        if self.dedup == Dedup::Off {
            return Ok(None);
        }
        if self.dedup_index.is_none() {
            self.dedup_index = Some(dedup_index_at_head(&self.catalog)?);
        }
        let Some(id) = self.dedup_index.as_ref().and_then(|i| i.get(key)).copied() else {
            return Ok(None);
        };
        let check = (|| -> Result<bool> {
            let record = self.catalog.object(&id)?.ok_or_else(|| {
                MochiError::new(ErrorCode::CatalogInvalid, "dedup candidate has no record")
            })?;
            let offset = self.catalog.object_location(&id)?.ok_or_else(|| {
                MochiError::new(ErrorCode::CatalogInvalid, "dedup candidate has no location")
            })?;
            let end = offset.checked_add(record.stored_len);
            if end.is_none_or(|end| end > head.committed_len) {
                return Err(MochiError::new(
                    ErrorCode::OutOfBounds,
                    "dedup candidate lies outside the committed archive",
                ));
            }
            let stored = load_stored(&self.storage, offset, &record, &self.read.limits)?;
            let ctx = self.crypto.as_ref().map(|c| c.key.context());
            let decoded = decode_verified(&record, &stored, &self.read.limits, ctx.as_ref())?;
            Ok(decoded.as_bytes() == part)
        })();
        match check {
            Ok(true) => Ok(Some(id)),
            Err(e) if matches!(e.code, ErrorCode::IoError | ErrorCode::Cancelled) => Err(e),
            Ok(false) | Err(_) => {
                stats.candidates_rejected += 1;
                if let Some(index) = self.dedup_index.as_mut() {
                    index.remove(key);
                }
                Ok(None)
            }
        }
    }

    /// Append one stored object and return its reference.
    fn append_object(&mut self, frame: &StoredObject) -> Result<ObjectRef> {
        Ok(ObjectRef {
            offset: self.storage.append(frame.as_bytes())?,
            stored_len: frame.len(),
            stored_hash: stored_object_hash(frame.view()),
        })
    }
    #[cfg(any(test, feature = "test-controls"))]
    fn tampered_member(&self, mut m: tar::Member, index: u64) -> tar::Member {
        if self.tar_tamper == Some(TarTamper::ModeOff) && index == 0 {
            m.mode ^= 1;
        }
        m
    }

    #[cfg(not(any(test, feature = "test-controls")))]
    fn tampered_member(&self, m: tar::Member, _index: u64) -> tar::Member {
        m
    }

    #[cfg(any(test, feature = "test-controls"))]
    fn drops_directory_member(&self) -> bool {
        self.tar_tamper == Some(TarTamper::DropDirectoryMember)
    }

    #[cfg(not(any(test, feature = "test-controls")))]
    fn drops_directory_member(&self) -> bool {
        false
    }

    #[cfg(any(test, feature = "test-controls"))]
    fn tamper_end(&self, pending: &mut Vec<u8>) {
        match self.tar_tamper {
            Some(TarTamper::NoEndBlocks) => {}
            Some(TarTamper::ExtraBlocks) => {
                pending.extend_from_slice(&tar::END_BLOCKS);
                pending.extend_from_slice(&[0; tar::BLOCK]);
            }
            Some(TarTamper::ExtraMember) => {
                let m = tar::Member {
                    path: b"extra".to_vec(),
                    kind: tar::MemberKind::File,
                    size: 0,
                    mode: 0o644,
                    uid: 0,
                    gid: 0,
                    mtime: (0, 0),
                };
                if let Ok(h) = encode_header(&m) {
                    pending.extend_from_slice(&h);
                }
                pending.extend_from_slice(&tar::END_BLOCKS);
            }
            _ => pending.extend_from_slice(&tar::END_BLOCKS),
        }
    }

    #[cfg(not(any(test, feature = "test-controls")))]
    fn tamper_end(&self, pending: &mut Vec<u8>) {
        pending.extend_from_slice(&tar::END_BLOCKS);
    }

    #[cfg(any(test, feature = "test-controls"))]
    fn tamper_reemitted(&self, buf: &mut [u8], flipped: &mut bool) {
        if self.tar_tamper == Some(TarTamper::ReemitFlipped) && !*flipped {
            if let Some(b) = buf.first_mut() {
                *b ^= 1;
                *flipped = true;
            }
        }
    }

    #[cfg(not(any(test, feature = "test-controls")))]
    fn tamper_reemitted(&self, _buf: &mut [u8], _flipped: &mut bool) {}

    /// TAR profile: write `pending` as one **stream-only chunk** (Annex B.2.9
    /// D19 rule 5): an ordinary data object that no extent references, in the
    /// catalog and in this commit's delta manifest like any introduced chunk.
    fn flush_framing(
        &mut self,
        pending: &mut Vec<u8>,
        cat: &mut Catalog,
        chunks: &mut Vec<ChunkEntry>,
        objects: &mut u64,
        encode: &EncodeParams,
    ) -> Result<()> {
        if pending.is_empty() {
            return Ok(());
        }
        let decoded = DecodedBytes::new(std::mem::take(pending));
        let obj = build_object(
            &decoded,
            encode,
            Protection::None,
            self.ids.as_mut(),
            &self.read.limits,
        )?;
        let offset = self.storage.append(obj.stored.as_bytes())?;
        cat.insert_object(&obj.record, Some(offset))?;
        chunks.push(ChunkEntry {
            record: obj.record,
            location: Some(offset),
        });
        *objects += 1;
        Ok(())
    }

    /// TAR profile: write the bytes of an existing version again, in order,
    /// as stream-only chunks of at most the archive's chunk size (D19 rule 6).
    /// One chunk is decoded at a time. A hole cannot be written (D19 rule 2).
    fn reemit_content(
        &mut self,
        extents: &[Extent],
        cat: &mut Catalog,
        chunks: &mut Vec<ChunkEntry>,
        objects: &mut u64,
        encode: &EncodeParams,
        ctx: &JobContext<'_>,
    ) -> Result<()> {
        let piece = usize::try_from(self.params.chunk_size)
            .unwrap_or(usize::MAX)
            .max(1);
        let mut buf: Vec<u8> = Vec::new();
        let mut flipped = false;
        for e in extents {
            ctx.check_cancelled()?;
            let ExtentSource::Chunk {
                chunk,
                chunk_offset,
            } = e.source
            else {
                return Err(MochiError::new(
                    ErrorCode::InvalidArgument,
                    "a hole cannot be written into a TAR-compatible archive's stream",
                ));
            };
            let unknown = || {
                MochiError::new(
                    ErrorCode::CatalogInvalid,
                    "an extent names a chunk the catalog cannot read",
                )
            };
            let record = cat.object(&chunk)?.ok_or_else(unknown)?;
            let at = cat.object_location(&chunk)?.ok_or_else(unknown)?;
            let stored = load_stored(&self.storage, at, &record, &self.read.limits)?;
            let decoded = decode_verified(&record, &stored, &self.read.limits, None)?;
            let range = usize::try_from(chunk_offset)
                .ok()
                .zip(usize::try_from(e.length).ok())
                .and_then(|(start, len)| Some(start..start.checked_add(len)?));
            let part = range
                .and_then(|r| decoded.as_bytes().get(r))
                .ok_or_else(|| {
                    MochiError::new(ErrorCode::ExtentInvalid, "an extent reads past its chunk")
                })?;
            buf.extend_from_slice(part);
            self.tamper_reemitted(&mut buf, &mut flipped);
            while buf.len() >= piece {
                let rest = buf.split_off(piece);
                let mut head = std::mem::replace(&mut buf, rest);
                self.flush_framing(&mut head, cat, chunks, objects, encode)?;
            }
        }
        self.flush_framing(&mut buf, cat, chunks, objects, encode)
    }
}

/// TAR profile, a copied version the archive does not hold: its own chunk
/// frames are the member's content, so they must be whole chunks, used once,
/// in logical order, none already held (D19 rule 6). Returns them in extent
/// order; anything else is `INVALID_ARGUMENT` and nothing is written.
fn order_copied_chunks<'a>(
    entry: &FileVersionEntry,
    stored: &'a [(ObjectRecord, StoredObject)],
    cat: &Catalog,
) -> Result<Vec<&'a (ObjectRecord, StoredObject)>> {
    let bad = |why: &str| {
        MochiError::new(
            ErrorCode::InvalidArgument,
            format!("cannot copy this version into a TAR-compatible archive: {why}"),
        )
    };
    let mut ordered = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut logical = 0u64;
    for (i, e) in entry.extents.iter().enumerate() {
        let ExtentSource::Chunk {
            chunk,
            chunk_offset,
        } = e.source
        else {
            return Err(bad("it has a hole"));
        };
        if usize::try_from(e.ordinal) != Ok(i) || e.logical_offset != logical {
            return Err(bad("its extents are not contiguous and in order"));
        }
        let Some(item) = stored.iter().find(|(r, _)| r.id == chunk) else {
            return Err(bad(
                "a chunk it needs is already held, so it cannot sit in the stream",
            ));
        };
        if chunk_offset != 0 || e.length != item.0.decoded_len {
            return Err(bad("a chunk is not used whole"));
        }
        if !seen.insert(chunk) {
            return Err(bad("a chunk is used twice"));
        }
        if cat.object(&chunk)?.is_some() {
            return Err(bad("a chunk it needs is already held"));
        }
        logical = logical
            .checked_add(e.length)
            .ok_or_else(|| bad("its length overflows"))?;
        ordered.push(item);
    }
    if ordered.len() != stored.len() {
        return Err(bad("it brings chunks its extents do not use"));
    }
    Ok(ordered)
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
#[allow(clippy::too_many_arguments)]
fn adopt_checkpoint(
    storage: &dyn ReadStorage,
    source: &AuthoritativeState,
    snapshot_ref: &ObjectRef,
    image_ref: &ObjectRef,
    identity: &RecordIdentity,
    archive_id: ArchiveId,
    writer_params: &[u8],
    key: Option<&Unlocked>,
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
    let manifest = match key {
        None => Manifest::from_stored(&stored, &Limits::WRITER_DEFAULT, &CborLimits::default())
            .map(|(m, _)| m),
        Some(k) => open_sealed_record(
            &stored,
            k,
            &SealTarget::SnapshotManifest {
                sequence: identity.commit_sequence,
                transaction_id: identity.transaction_id,
            },
            &Limits::WRITER_DEFAULT,
        )
        .and_then(|pt| Manifest::decode(&pt, &Limits::WRITER_DEFAULT, &CborLimits::default())),
    }
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
    let plaintext;
    let bytes = match key {
        None => decode_image_record(&stored, identity, &Limits::WRITER_DEFAULT),
        Some(k) => match open_sealed_record(
            &stored,
            k,
            &SealTarget::Image {
                sequence: identity.commit_sequence,
                transaction_id: identity.transaction_id,
            },
            &Limits::WRITER_DEFAULT,
        ) {
            Ok(pt) => {
                plaintext = pt;
                decode_image_payload(&plaintext, identity, &Limits::WRITER_DEFAULT, true)
            }
            Err(e) => Err(e),
        },
    }
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
    let want = source.clone().without_manifest_only_state();
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
    let from_snapshot = AuthoritativeState::from_snapshot(snapshot)?.without_manifest_only_state();
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
    /// this; it writes the B.2.3 trigger.
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

    /// Damage the TAR stream of the next commits. `None` turns it off.
    pub fn set_tar_tamper(&mut self, t: Option<TarTamper>) {
        self.tar_tamper = t;
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

/// How [`ArchiveWriter::set_tar_tamper`] damages a commit's TAR stream
/// (test controls only). Each is a violation `verify` must name.
#[cfg(any(test, feature = "test-controls"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TarTamper {
    /// The first member's header carries a different mode than its version.
    ModeOff,
    /// The stream lacks its two end blocks.
    NoEndBlocks,
    /// A third zero block follows the two end blocks.
    ExtraBlocks,
    /// One more (empty) member than the commit has puts.
    ExtraMember,
    /// A re-emitted member's first byte differs from the version's.
    ReemitFlipped,
    /// A directory put has no member in the stream.
    DropDirectoryMember,
}

/// How [`ArchiveWriter::set_checkpoint_tamper`] damages a checkpoint's
/// serialized form (test controls only).
#[cfg(any(test, feature = "test-controls"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTamper {
    /// XOR the POSIX mode of the first file version (by ID) that has POSIX
    /// attributes, in the serialized snapshot only.
    SnapshotAttributes,
    /// Drop every hold from the serialized snapshot's retention state, or,
    /// with none, mark snapshot 0 expired (a commit after 0 only).
    SnapshotRetention,
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
            CheckpointTamper::SnapshotRetention => {
                if snapshot.retention.holds.is_empty() {
                    snapshot.retention.expired.insert(0);
                } else {
                    snapshot.retention.holds.clear();
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
