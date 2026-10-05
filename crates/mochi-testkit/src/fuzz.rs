//! Parser exercisers shared by the cargo-fuzz targets (`fuzz/`) and the
//! deterministic smoke test. A panic here is a finding: every function asserts
//! the invariants the framing layer promises for *any* input.

use mochi_core::catalog::{Catalog, CatalogLimits};
use mochi_core::manifest::Manifest;
use mochi_format::cbor::{self, CborLimits};
use mochi_format::codec::{decode_object, Protection};
use mochi_format::envelope::{
    decode_binary_record, encode_binary_record, image_payload_budget, EnvelopeRules,
    BINARY_ENVELOPE_FIXED_LEN,
};
use mochi_format::footer::{validate_footer, validate_footer_at_eof};
use mochi_format::frame::{walk_frame, FrameDetail, Frames};
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObject;
use mochi_format::Limits;

/// Small limits so hostile inputs finish quickly; the point is bounded work.
pub fn fuzz_limits() -> Limits {
    Limits {
        max_skippable_payload: 1 << 16,
        max_frame_len: 1 << 20,
        max_blocks_per_frame: 4096,
        max_window_size: 1 << 24,
        max_commit_frame_len: 1 << 16,
        max_decoded_object_len: 1 << 20,
        max_required_features: 64,
    }
}

/// Frame walker: spans are contiguous, in bounds, and never empty.
pub fn exercise_walker(data: &[u8]) {
    let limits = fuzz_limits();
    let mut expected_start = 0u64;
    for item in Frames::new(data, 0, limits) {
        match item {
            Ok(span) => {
                assert_eq!(span.offset, expected_start);
                assert!(span.len >= 8 || span.kind == FrameKind::ZstdData);
                assert!(span.len > 0 && span.end() <= data.len() as u64);
                if let FrameDetail::Skippable { payload_len } = span.detail {
                    assert_eq!(span.len, 8 + u64::from(payload_len));
                }
                expected_start = span.end();
            }
            Err(_) => break,
        }
    }
    // Arbitrary start offsets must be safe too (scanners probe candidates).
    for offset in 0..data.len().min(256) as u64 {
        let _ = walk_frame(data, offset, &limits);
    }
    let _ = walk_frame(data, u64::MAX, &limits);
}

/// Footer validation at EOF and at every candidate footer frame.
pub fn exercise_footer(data: &[u8]) {
    let limits = fuzz_limits();
    let _ = validate_footer_at_eof(data, &limits);
    for item in Frames::new(data, 0, limits) {
        match item {
            Ok(span) if span.kind == FrameKind::CommitFooter => {
                if let Ok(v) = validate_footer(data, span.offset, &limits) {
                    // An accepted footer names a range fully before itself.
                    assert!(v.fields.commit_offset + v.fields.commit_len <= v.footer_offset);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = validate_footer(data, u64::MAX, &limits);
}

/// Features the envelope exerciser treats as known, so accepted inputs can
/// carry some (the build itself knows none).
const FUZZ_KNOWN_FEATURES: &[u64] = &[1, 2, 3, 1 << 40];

fn exercise_binary_payload(magic: u32, payload: &[u8], limits: &Limits) {
    let rules = EnvelopeRules {
        kind: FrameKind::MetadataDelta,
        schema_versions: &[0, 1, 7],
        known_features: FUZZ_KNOWN_FEATURES,
    };
    let Ok(candidate) = decode_binary_record(magic, payload, &rules, limits) else {
        return;
    };
    let env = candidate.unbound_envelope().clone();
    let n = env.required_features.len();
    assert!(n <= 64);
    assert!(env.required_features.windows(2).all(|w| w[0] < w[1]));
    let (bound, body) = candidate
        .bind(&env.identity)
        .expect("a candidate binds to its own identity");
    assert_eq!(bound, env);
    assert_eq!(
        body.len() + BINARY_ENVELOPE_FIXED_LEN + 8 * n,
        payload.len()
    );
    assert!(body.len() as u64 <= image_payload_budget(limits));
    // Canonical: re-encoding what was accepted gives back the same bytes, so
    // no byte of an accepted header is free.
    let frame = encode_binary_record(
        FrameKind::MetadataDelta,
        &env,
        body,
        FUZZ_KNOWN_FEATURES,
        limits,
    )
    .expect("the writer accepts what the reader accepted");
    assert_eq!(&frame[8..], payload);
}

/// Binary envelopes (spec Annex B.2.2) on every skippable payload, and on
/// the raw input itself.
pub fn exercise_envelope(data: &[u8]) {
    let limits = fuzz_limits();
    if data.len() >= 4 {
        let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        exercise_binary_payload(magic, &data[4..], &limits);
        exercise_binary_payload(mochi_format::registry::METADATA_DELTA, data, &limits);
    }
    for item in Frames::new(data, 0, limits) {
        match item {
            Ok(span) => {
                if let FrameDetail::Skippable { .. } = span.detail {
                    let start = (span.offset + 8) as usize;
                    let end = span.end() as usize;
                    exercise_binary_payload(span.magic, &data[start..end], &limits);
                }
            }
            Err(_) => break,
        }
    }
}

/// Object decoding (C2, spec §9.1, §9.3). The first two bytes pick the
/// expected decoded length; the rest is the stored object. Whatever the input,
/// decoding either fails or yields *exactly* the expected length: never a
/// short, padded, or overlong success, and never unbounded work or memory.
pub fn exercise_object_decode(data: &[u8]) {
    let limits = fuzz_limits();
    let (expected, stored) = match data {
        [a, b, rest @ ..] => (u64::from(u16::from_le_bytes([*a, *b])), rest),
        _ => (0, data),
    };
    let stored = StoredObject::from_loaded(stored.to_vec());
    for want in [expected, expected.saturating_mul(16), 0] {
        if let Ok(decoded) = decode_object(&stored, Protection::None, want, &limits) {
            assert_eq!(decoded.len(), want);
        }
    }
    // Also decode the raw input with no length prefix stripped, since the
    // golden vectors used as seeds are bare frames.
    let bare = StoredObject::from_loaded(data.to_vec());
    for want in [0, 5, 10, 4096] {
        if let Ok(decoded) = decode_object(&bare, Protection::None, want, &limits) {
            assert_eq!(decoded.len(), want);
        }
    }
}

/// Catalog images (C3, spec §10.5, §22.1). Any input either is refused or
/// opens to a catalog that passes full verification and replays; never a
/// panic, never a half-open catalog.
pub fn exercise_catalog_image(data: &[u8]) {
    let limits = CatalogLimits {
        max_image_len: 1 << 20,
    };
    if let Ok(catalog) = Catalog::open_image(data, &limits) {
        assert!(catalog.verify().is_ok());
        assert!(catalog.replay(None).is_ok());
    }
}

/// Canonical CBOR (C4, spec D2). Whatever the input, decoding either fails or
/// yields a value that re-encodes to exactly the input bytes.
pub fn exercise_cbor(data: &[u8]) {
    let limits = CborLimits {
        max_depth: 32,
        max_items: 1 << 16,
    };
    if let Ok(v) = cbor::decode(data, &limits) {
        assert_eq!(cbor::encode(&v).ok().as_deref(), Some(data));
    }
}

/// The shape rules of manifest schema 1 that anything accepted must obey
/// (`recovery-manifest-v1.cddl`): separate delta and snapshot shapes, the
/// exact parent rule, and the D11 envelope keys.
fn assert_manifest_v1_shape(m: &Manifest) {
    use mochi_core::manifest::ManifestKind;
    match m.kind {
        ManifestKind::Delta => {
            assert!(m.entries.is_empty(), "accepted a delta with entries");
            assert_eq!(m.parent.is_none(), m.commit_seq == 0);
            if let Some(p) = m.parent {
                assert_eq!(p.seq + 1, m.commit_seq);
            }
        }
        ManifestKind::Snapshot => {
            assert!(m.parent.is_none(), "accepted a snapshot with a parent");
            assert!(m.ops.is_empty(), "accepted a snapshot with operations");
        }
    }
    assert!(
        m.required_features.is_empty(),
        "accepted an unknown feature"
    );
}

/// Recovery manifests (C4, spec §11), both as a bare payload and as a stored
/// frame. An accepted manifest re-encodes to exactly its input and has a
/// schema-1 shape.
pub fn exercise_manifest(data: &[u8]) {
    let cbor_limits = CborLimits {
        max_depth: 32,
        max_items: 1 << 16,
    };
    if let Ok(m) = Manifest::decode(data, &fuzz_limits(), &cbor_limits) {
        assert_manifest_v1_shape(&m);
        assert_eq!(m.encode().ok().as_deref(), Some(data));
    }
    let stored = StoredObject::from_loaded(data.to_vec());
    if let Ok((m, _)) = Manifest::from_stored(&stored, &fuzz_limits(), &cbor_limits) {
        assert_manifest_v1_shape(&m);
        assert_eq!(m.to_stored().ok().as_ref(), Some(&stored));
    }
}

/// Commit records (C5, spec §12.1). An accepted record re-encodes to exactly
/// its input, and its ID is the recomputed one.
pub fn exercise_commit(data: &[u8]) {
    let cbor_limits = CborLimits {
        max_depth: 32,
        max_items: 1 << 16,
    };
    if let Ok((r, id)) =
        mochi_core::commit::CommitRecord::decode(data, &fuzz_limits(), &cbor_limits)
    {
        assert_commit_v1_shape(&r);
        let (bytes, id2) = r.encode().expect("accepted record re-encodes");
        assert_eq!(bytes, data);
        assert_eq!(id, id2);
    }
    if let Ok((r, _)) =
        mochi_core::commit::CommitRecord::from_stored(data, &fuzz_limits(), &cbor_limits)
    {
        assert_commit_v1_shape(&r);
    }
}

/// The self-contained rules of commit schema 1 that anything accepted must
/// obey (`commit-record-v1.cddl`).
fn assert_commit_v1_shape(r: &mochi_core::commit::CommitRecord) {
    use mochi_core::commit::Metadata;
    assert_eq!(r.parent.is_none(), r.seq == 0);
    if let Some(p) = r.parent {
        assert_eq!(p.seq + 1, r.seq);
    }
    match r.metadata {
        Metadata::Checkpoint { .. } => {}
        Metadata::Delta { base } => {
            assert!(r.seq > 0, "accepted a delta at sequence 0");
            assert!(base.seq < r.seq);
        }
    }
    assert_eq!(
        r.descriptor.offset, 0,
        "accepted a descriptor away from offset 0"
    );
    for (_, o) in r.object_refs() {
        assert!(o.stored_len >= 8);
    }
}

/// Archive descriptors (spec Annex B.2 D12): anything accepted re-encodes to
/// the same frame (canonical; no free bytes), and is accepted only at offset 0.
pub fn exercise_descriptor(data: &[u8]) {
    use mochi_core::descriptor::Descriptor;
    let cbor_limits = CborLimits {
        max_depth: 32,
        max_items: 1 << 16,
    };
    let _ = Descriptor::decode(data, &fuzz_limits(), &cbor_limits);
    if let Ok(d) = Descriptor::from_stored(data, 0, &fuzz_limits(), &cbor_limits) {
        let again = d.to_stored().expect("accepted descriptor re-encodes");
        assert_eq!(again.as_bytes(), data);
        assert!(Descriptor::from_stored(data, 1, &fuzz_limits(), &cbor_limits).is_err());
    }
}

/// What [`exercise_archive_open`] did with one input. Lets tests prove the
/// segment assertions were reached (they run only for opened delta heads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOpenOutcome {
    /// `open_head` refused the input.
    Refused,
    /// Opened at a checkpoint head; the segment is the head itself.
    OpenedCheckpoint,
    /// Opened at a delta head; the independent segment walk and every
    /// segment assertion ran and held.
    OpenedDelta,
}

/// Whole-archive opening (C5): head location, tail classification, and the
/// footer → commit → manifest → catalog path over arbitrary bytes. Must
/// return, bounded, and never accept a head whose referenced objects do not
/// hash-verify (`open_head` checks that itself; here we only require no panic
/// and that an opened head's commit is the one its footer names).
///
/// T11/T12: for anything `open_head` opens (`docs/t11-t12-acceptance.md`,
/// "Fuzz"), also: the catalog head equals the footer's sequence, and the head's
/// replay segment obeys the T11 rules. The segment is re-walked here
/// independently of `mochi_core::segment` (footers via `mochi-format`,
/// following the parent hints, which an opened head's segment must match
/// exactly), and checked against the D10.6 base rule:
///
/// * contiguous sequences, each commit's ID equal to its child's parent ID;
/// * the walk ends at a checkpoint *b*, every commit above *b* is a delta,
///   and each delta's base equals the rule (the parent if it is a
///   checkpoint, else the parent's base) by ID and sequence;
/// * one descriptor reference across the segment;
/// * `OpenedHead::segment` names *b* (sequence, ID, footer offset).
pub fn exercise_archive_open(data: &[u8]) -> ArchiveOpenOutcome {
    use mochi_core::commit::{CommitLink, CommitRecord, Metadata};
    use mochi_format::footer::validate_footer;

    let storage = crate::SimStorage::from_bytes(data.to_vec());
    let opts = mochi_core::publish::ReadOptions {
        limits: fuzz_limits(),
        ..Default::default()
    };
    // T16: baseline recovery runs on anything with a locatable head, whether
    // or not the head opens, and must never return a state for another commit.
    baseline_recovery_is_sound(&storage, &opts);
    let Ok(head) = mochi_core::publish::open_head(&storage, &opts) else {
        return ArchiveOpenOutcome::Refused;
    };
    let footer_seq = head.location.footer.fields.commit_sequence;
    assert_eq!(head.commit.seq, footer_seq);
    assert_eq!(
        head.catalog.head_commit().unwrap(),
        Some(footer_seq),
        "the catalog head equals the footer's sequence"
    );
    // Bound to a descriptor of the same archive at offset 0, and to a delta
    // manifest with the commit's identity.
    assert_eq!(head.commit.descriptor.offset, 0);
    assert_eq!(head.descriptor.archive_id, head.commit.archive_id);
    assert_eq!(head.manifest.identity(), head.commit.identity());
    // Every referenced object lies before the commit frame.
    for (_, o) in head.commit.object_refs() {
        assert!(o.offset + o.stored_len <= head.location.footer.fields.commit_offset);
    }

    // Independent segment walk, head first.
    let mut seg: Vec<(CommitRecord, CommitLink)> = vec![(
        head.commit.clone(),
        CommitLink {
            commit_id: head.commit_id,
            seq: head.commit.seq,
            footer_offset: head.location.footer.footer_offset,
        },
    )];
    while !seg.last().unwrap().0.metadata.is_checkpoint() {
        let (child, _) = seg.last().unwrap();
        let parent = child
            .parent
            .expect("an opened delta has a parent (commit 0 is a checkpoint)");
        let f = validate_footer(data, parent.footer_offset, &opts.limits)
            .expect("an opened head's segment parent hints are exact");
        let start = f.fields.commit_offset as usize;
        let end = start + f.fields.commit_len as usize;
        let (rec, id) =
            CommitRecord::from_stored(&data[start..end], &opts.limits, &Default::default())
                .expect("segment commits decode");
        assert_eq!(id, parent.commit_id, "parent ID");
        assert_eq!(rec.seq + 1, child.seq, "contiguous sequences");
        assert_eq!(rec.seq, parent.seq);
        seg.push((
            rec,
            CommitLink {
                commit_id: id,
                seq: parent.seq,
                footer_offset: parent.footer_offset,
            },
        ));
    }
    seg.reverse(); // base first
    let base = seg[0].1;
    for w in seg.windows(2) {
        let ((prev, prev_link), (cur, _)) = (&w[0], &w[1]);
        let Metadata::Delta { base: named } = cur.metadata else {
            panic!("commit {} above the base is not a delta", cur.seq);
        };
        let rule = match prev.metadata {
            Metadata::Checkpoint { .. } => *prev_link,
            Metadata::Delta { base } => base,
        };
        assert_eq!(
            (named.commit_id, named.seq),
            (rule.commit_id, rule.seq),
            "commit {}: base rule",
            cur.seq
        );
        assert_eq!((named.commit_id, named.seq), (base.commit_id, base.seq));
        assert_eq!(
            cur.descriptor, prev.descriptor,
            "one descriptor per segment"
        );
    }
    assert_eq!(head.segment.base_seq, base.seq);
    assert_eq!(head.segment.base_commit_id, base.commit_id);
    assert_eq!(head.segment.base_footer_offset, base.footer_offset);
    if seg.len() > 1 {
        ArchiveOpenOutcome::OpenedDelta
    } else {
        ArchiveOpenOutcome::OpenedCheckpoint
    }
}

/// D10.8: whatever baseline recovery returns for the located head is a
/// catalog that materializes exactly that head, from a base at or below it.
/// An error is always acceptable (untrusted input); a wrong state is not.
fn baseline_recovery_is_sound(
    storage: &crate::SimStorage,
    opts: &mochi_core::publish::ReadOptions,
) {
    let Ok(loc) = mochi_core::publish::locate_head(storage, &opts.limits) else {
        return;
    };
    if let Ok(b) =
        mochi_core::publish::recover_baseline_at_footer(storage, loc.footer.footer_offset, opts)
    {
        assert_eq!(b.head_seq, loc.footer.fields.commit_sequence);
        assert_eq!(b.catalog.head_commit().unwrap(), Some(b.head_seq));
        assert!(b.segment.base_seq <= b.head_seq);
    }
}

pub fn exercise_all(data: &[u8]) {
    exercise_walker(data);
    exercise_footer(data);
    exercise_envelope(data);
    exercise_object_decode(data);
    exercise_cbor(data);
    exercise_manifest(data);
    exercise_commit(data);
    exercise_descriptor(data);
    exercise_archive_open(data);
}
