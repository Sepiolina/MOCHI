//! Verification of an Encrypted archive with no passphrase (spec Annex B.2.10
//! D20 items 7 and 12; plan C11).
//!
//! Stored integrity needs no key: every stored-object hash covers the whole
//! sealed frame, and a commit's data region (commit key 12) names one byte
//! range and one hash for all of its data objects, because the per-object
//! hashes live in the sealed manifest. For every commit, in order, this check
//!
//! * reads the descriptor and the key envelopes it lists (both hash-verified
//!   before they are parsed, both plaintext records);
//! * hash-verifies the delta manifest and, for a checkpoint, the snapshot
//!   manifest and the catalog image, and requires each to be one sealed frame
//!   of the right kind under the archive's key ID. Their contents are sealed
//!   and are **not** read;
//! * walks the data region as complete sealed frames of kind 0 (data chunks),
//!   `RECORD_INVALID` otherwise (`structural` and deeper reach the shape at
//!   `referential`), and, from `stored_integrity` up, hashes it in a bounded
//!   buffer against the commit's hash (`STORED_INTEGRITY_FAILED`).
//!
//! Whatever needs the key is skipped with the reason "no key supplied", and
//! the dimensions that depend on it stay `UNKNOWN`: nothing here is a claim
//! about content, names, or the catalog.

use mochi_format::digest::{StoredBytesHasher, StoredObjectHash};
use mochi_format::frame::walk_frame;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObjectBytes;
use mochi_format::seal::{parse_sealed, sealed_frame_payload, KeyId, SealKind};

use crate::commit::{CommitRecord, Metadata, ObjectRef};
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::keys::read_envelopes;
use crate::publish::{load_verified, read_descriptor, HistoryEntry, ReadOptions};
use crate::storage::{ReadStorage, StorageReader};

use super::{classify, phase, ErrorClass, Run};

const HASH_BUFFER: usize = 1 << 20;

/// The reason every skipped item names.
pub(super) const NO_KEY: &str = "no key supplied";

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

/// A walk error that is evidence about the archive is a shape fault of the
/// region (`RECORD_INVALID`); an I/O or limit error keeps its code.
fn shape(e: MochiError) -> MochiError {
    if classify(e.code) == ErrorClass::Violation {
        invalid(format!("data region: {}", e.message))
    } else {
        e
    }
}

/// A control object of an Encrypted archive: hash-verified, then exactly one
/// sealed frame of `kind` under `key_id`. Nothing is opened.
fn sealed_control_object(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    r: &ObjectRef,
    limit: u64,
    kind: SealKind,
    key_id: &KeyId,
    what: &str,
) -> Result<()> {
    let stored = load_verified(src, r, limit, ro.limits.max_frame_len, what)?;
    let payload = sealed_frame_payload(&stored, &ro.limits)?;
    let (header, _) = parse_sealed(payload)?;
    if header.kind != kind {
        return Err(invalid(format!("the {what} is not sealed as a {what}")));
    }
    if &header.key_id != key_id {
        return Err(invalid(format!(
            "the {what} is sealed under a key the commit's envelopes do not name"
        )));
    }
    Ok(())
}

/// The shape of the data region and, at `hash`, its stored-object hash.
/// Returns the number of sealed frames and the bytes hashed.
fn data_region(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    commit: &CommitRecord,
    commit_offset: u64,
    r: &ObjectRef,
    key_id: &KeyId,
    hash: bool,
    ctx: &JobContext<'_>,
) -> Result<(u64, u64)> {
    let end = r.end()?;
    if end > commit_offset || end > commit.delta_manifest.offset {
        return Err(invalid(
            "the data region extends past the manifest or commit it precedes",
        ));
    }
    let reader = StorageReader::prefix(src, end).map_err(MochiError::from)?;
    let mut at = r.offset;
    let mut frames = 0u64;
    while at < end {
        ctx.check_cancelled()?;
        let span = walk_frame(&reader, at, &ro.limits)
            .map_err(MochiError::from)
            .map_err(shape)?;
        if span.kind != FrameKind::EncryptedObject {
            return Err(invalid(format!(
                "the data region holds a frame at offset {at} that is not a sealed object"
            )));
        }
        // payload bytes 4..8: the object kind; 8..24: the key ID.
        let mut head = [0u8; 24];
        src.read_exact_at(
            at + mochi_format::registry::SKIPPABLE_HEADER_LEN as u64,
            &mut head,
        )
        .map_err(MochiError::from)?;
        let kind = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
        if kind != SealKind::Chunk.code() {
            return Err(invalid(format!(
                "the data region holds a sealed object at offset {at} that is not a data chunk"
            )));
        }
        if head[8..24] != *key_id.as_bytes() {
            return Err(invalid(format!(
                "the sealed object at offset {at} names a key the commit's envelopes do not"
            )));
        }
        frames += 1;
        at = span.end();
    }
    if at != end {
        return Err(invalid("the data region does not end on a frame boundary"));
    }
    if !hash {
        return Ok((frames, 0));
    }
    let mut hasher = StoredBytesHasher::stored_object();
    let mut buf = vec![0u8; HASH_BUFFER.min(usize::try_from(r.stored_len).unwrap_or(HASH_BUFFER))];
    let mut pos = r.offset;
    while pos < end {
        ctx.check_cancelled()?;
        let n = usize::try_from(end - pos).map_or(buf.len(), |left| left.min(buf.len()));
        src.read_exact_at(pos, &mut buf[..n])
            .map_err(MochiError::from)?;
        hasher.update(StoredObjectBytes::new(&buf[..n]));
        pos += n as u64;
    }
    let got: StoredObjectHash = hasher.finalize();
    if got != r.stored_hash {
        return Err(MochiError::new(
            ErrorCode::StoredIntegrityFailed,
            format!(
                "data region at offset {}: stored-object hash mismatch",
                r.offset
            ),
        ));
    }
    Ok((frames, r.stored_len))
}

/// Check every commit's key-free evidence. `false` if cancelled.
///
/// With `keyless` (no passphrase was supplied) this is the whole check: it
/// records coverage and what was skipped for want of a key. Otherwise it is an
/// addition to the keyed checks, which then also see the commit's own data
/// region record agree with the bytes.
pub(super) fn check(
    src: &dyn ReadStorage,
    ro: &ReadOptions,
    history: &[HistoryEntry],
    depth: u8,
    keyless: bool,
    ctx: &JobContext<'_>,
    run: &mut Run,
) -> bool {
    run.keyless = keyless;
    let total = history.len() as u64;
    ctx.report(phase::KEYLESS, 0, Some(total));
    let (mut region_bytes, mut hashed_bytes, mut frames_seen) = (0u64, 0u64, 0u64);
    for (i, e) in history.iter().enumerate() {
        if ctx.check_cancelled().is_err() {
            return false;
        }
        let c = &e.commit;
        let at = e.commit_offset;
        let what = format!("commit {}", c.seq);
        if let Err(err) = read_descriptor(src, c, at, ro) {
            run.error(&format!("{what}: its archive descriptor"), &err);
        }
        let key_id = match read_envelopes(src, c, at, ro) {
            Ok(envs) => envs.first().map(|(_, env)| env.key_id),
            Err(err) => {
                run.error(&format!("{what}: its key envelopes"), &err);
                None
            }
        };
        if let Some(key_id) = key_id {
            let mut objects = vec![("delta manifest", SealKind::DeltaManifest, c.delta_manifest)];
            if let Metadata::Checkpoint { image, snapshot } = c.metadata {
                objects.push(("snapshot manifest", SealKind::SnapshotManifest, snapshot));
                objects.push(("catalog image", SealKind::Image, image));
            }
            for (name, kind, r) in objects {
                if let Err(err) = sealed_control_object(src, ro, &r, at, kind, &key_id, name) {
                    run.error(&format!("{what}: its {name}"), &err);
                }
            }
            if let (Some(r), true) = (c.data_region.as_ref(), depth >= 2) {
                region_bytes = region_bytes.saturating_add(r.stored_len);
                match data_region(src, ro, c, at, r, &key_id, depth >= 3, ctx) {
                    Ok((frames, bytes)) => {
                        frames_seen += frames;
                        hashed_bytes = hashed_bytes.saturating_add(bytes);
                    }
                    Err(err) if err.code == ErrorCode::Cancelled => return false,
                    Err(err) => {
                        run.data_failed |= run.error(&format!("{what}: its data region"), &err)
                            == ErrorClass::Violation;
                    }
                }
            }
        }
        ctx.report(phase::KEYLESS, i as u64 + 1, Some(total));
    }
    if !keyless {
        return true;
    }
    let cov = &mut run.report.coverage;
    cov.expected_objects = Some(frames_seen);
    cov.expected_bytes = Some(region_bytes);
    if depth >= 3 {
        cov.checked_objects = Some(frames_seen);
        cov.checked_bytes = Some(hashed_bytes);
    }

    run.skip("head catalog", NO_KEY);
    if depth >= 2 {
        run.skip("data objects", "only each commit's data region was checked: the per-object hashes are in the sealed manifests");
    }
    if depth >= 4 {
        run.skip("content integrity", NO_KEY);
    }
    if depth >= 5 {
        run.skip("file versions", NO_KEY);
    }
    run.skip("recoverability", NO_KEY);
    true
}
