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
//!
//! The same walk runs with a key, before the keyed checks. In both, each
//! distinct key envelope any commit lists whose Argon2id cost is below the
//! writer default gets one informational `KDF_COST_BELOW_DEFAULT` finding (D20
//! item 14): readers enforce no minimum, so it changes no status and no exit
//! code.

use std::collections::BTreeSet;

use mochi_format::digest::{StoredBytesHasher, StoredObjectHash};
use mochi_format::frame::walk_frame;
use mochi_format::kdf::KdfParams;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObjectBytes;
use mochi_format::seal::{parse_sealed, sealed_frame_payload, KeyId, SealKind};

use crate::commit::{CommitRecord, Metadata, ObjectRef};
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::keys::read_envelopes;
use crate::publish::{load_verified, read_descriptor, HistoryEntry, ReadOptions};
use crate::report::{Finding, Severity};
use crate::storage::{ReadStorage, StorageReader};

use super::{classify, phase, ErrorClass, Run};

const HASH_BUFFER: usize = 1 << 20;

/// The reason every skipped item names.
pub(super) const NO_KEY: &str = "no key supplied";

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

fn cost(k: &KdfParams) -> String {
    format!(
        "m = {} KiB, t = {}, p = {}",
        k.memory_kib, k.iterations, k.lanes
    )
}

/// An envelope's ID, its KDF cost, and whether the head lists it.
type Envelope = ([u8; 16], KdfParams, bool);

fn below_default(k: &KdfParams) -> bool {
    let d = KdfParams::WRITER_DEFAULT;
    k.memory_kib < d.memory_kib || k.iterations < d.iterations
}

/// One informational finding per distinct key envelope in the file whose
/// Argon2id memory or pass count is below the writer default (D20 item 14;
/// R5 §12, open item c). Every envelope any commit lists counts, not only the
/// head's: a removed envelope stays in the file and still yields the data key
/// (removal is not revocation, item 10), so a guess against it is as good as
/// one against the head's. `in_head` says whether the head still lists it.
/// Lanes do not enter: they change parallelism, not the cost per guess. The
/// wording says "below the MOCHI writer default", never "weak".
fn kdf_cost_findings(run: &mut Run, envelopes: impl IntoIterator<Item = Envelope>) {
    let default = KdfParams::WRITER_DEFAULT;
    for (envelope_id, kdf, in_head) in envelopes {
        if !below_default(&kdf) {
            continue;
        }
        let id: String = envelope_id.iter().map(|b| format!("{b:02x}")).collect();
        let listed = if in_head {
            String::new()
        } else {
            " (the head no longer lists it, but it is still in the file and still opens \
             the data key)"
                .to_owned()
        };
        run.report.findings.push(Finding {
            code: ErrorCode::KdfCostBelowDefault,
            severity: Severity::Info,
            message: Some(format!(
                "key envelope {id}{listed} declares an Argon2id cost below the MOCHI writer \
                 default; readers accept it, and a passphrase guess against it costs less"
            )),
            expected: Some(format!("at least {}", cost(&default))),
            observed: Some(cost(&kdf)),
            affected: None,
        });
    }
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
#[allow(clippy::too_many_arguments)]
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
    // For the KDF-cost finding: every distinct envelope ID of the file, the
    // below-default ones in order of first listing, and the head's.
    let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
    let mut cheap: Vec<([u8; 16], KdfParams)> = Vec::new();
    let mut head_ids: Vec<[u8; 16]> = Vec::new();
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
            Ok(envs) => {
                for (_, env) in &envs {
                    if seen.insert(env.envelope_id) && below_default(&env.kdf) {
                        cheap.push((env.envelope_id, env.kdf));
                    }
                }
                if i + 1 == history.len() {
                    head_ids = envs.iter().map(|(_, env)| env.envelope_id).collect();
                }
                envs.first().map(|(_, env)| env.key_id)
            }
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
    kdf_cost_findings(
        run,
        cheap
            .into_iter()
            .map(|(id, kdf)| (id, kdf, head_ids.contains(&id))),
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Report;
    use crate::status::VerificationLevel;

    fn run() -> Run {
        Run {
            report: Report::new(VerificationLevel::StoredIntegrity),
            violation: false,
            unsupported: false,
            operational: false,
            data_complete: false,
            data_failed: false,
            recovery_failed: false,
            tar_streams: false,
            keyless: false,
        }
    }

    /// The threshold is the writer default, inclusive: exactly the default
    /// (or more) is silent; one KiB or one pass less is one finding per
    /// envelope; lanes never matter. The finding sets no run flag.
    #[test]
    fn below_default_cost_is_one_informational_finding_per_envelope() {
        let d = KdfParams::WRITER_DEFAULT;
        let with = |m: u64, t: u64, p: u64| KdfParams {
            memory_kib: m,
            iterations: t,
            lanes: p,
        };
        let cases = [
            (d, false),
            (with(d.memory_kib, d.iterations, 1), false),
            (with(d.memory_kib * 4, d.iterations + 1, 16), false),
            (with(d.memory_kib - 1, d.iterations, d.lanes), true),
            (with(d.memory_kib, d.iterations - 1, d.lanes), true),
            (with(64, 2, 1), true),
        ];
        for (i, (kdf, flagged)) in cases.into_iter().enumerate() {
            let mut r = run();
            kdf_cost_findings(&mut r, [([i as u8; 16], kdf, true)]);
            assert_eq!(r.report.findings.len(), usize::from(flagged), "{kdf:?}");
            assert!(!r.violation && !r.unsupported && !r.operational);
            if let Some(f) = r.report.findings.first() {
                assert_eq!(f.code, ErrorCode::KdfCostBelowDefault);
                assert_eq!(f.severity, Severity::Info);
                assert!(f
                    .message
                    .as_deref()
                    .unwrap()
                    .contains(&format!("{:02x}", i)));
            }
        }
        let mut r = run();
        kdf_cost_findings(
            &mut r,
            [
                ([1; 16], with(64, 2, 1), true),
                ([2; 16], d, true),
                ([3; 16], with(1024, 3, 1), false),
            ],
        );
        assert_eq!(r.report.findings.len(), 2);
        // An envelope the head no longer lists is still judged, and says so.
        let msg = r.report.findings[1].message.as_deref().unwrap();
        assert!(msg.contains("no longer lists it"), "{msg}");
        let msg = r.report.findings[0].message.as_deref().unwrap();
        assert!(!msg.contains("no longer lists it"), "{msg}");
    }
}
