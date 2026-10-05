//! C5 exit criteria (plan C5; spec §12.2, §24.2 fault matrix):
//!
//! * *termination at each publication stage* → previous complete commit or
//!   new complete commit, never a mixed state;
//! * *truncation at every byte of small fixtures* → no invalid commit accepted;
//! * *torn, reordered, or lost writes* → no false durability acknowledgement;
//! * plus the C5-specific requirements: a value-level bit flip in a catalog
//!   image is refused; recovery takes its head from the footer; cancellation
//!   before the footer leaves the previous head; a second writer is refused;
//!   an uncommitted tail is removed only explicitly and with an audit record.
//!
//! "Never mixed" is checked the strong way: every file of the head commit is
//! rebuilt from its chunks and compared with an independent model.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};

use mochi_core::catalog::Catalog;
use mochi_core::commit::Metadata;
use mochi_core::image::{decode_image_record, IMAGE_RECORD_SCHEMA};
use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::object::IdSource;
use mochi_core::publish::{
    commit_history, locate_head, open_at_footer, open_head, read_commit, recover_with_trusted_head,
    ArchiveWriter, AuditEvent, CommitStatus, HeadSource, PublishDurability, ReadOptions,
    TailPolicy, TailState, Transaction,
};
use mochi_core::recovery::{recover_from_manifests, RecoveryScope};
use mochi_core::storage::{ReadStorage, Storage};
use mochi_core::ErrorCode;
use mochi_format::footer::encode_footer_frame;
use mochi_format::frame::Frames;
use mochi_format::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::Limits;
use mochi_testkit::archive::{
    build, path, read_state, scripted_history, test_options, Content, Job, State, Step,
};
use mochi_testkit::{CrashMode, Fault, Op, SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn ids(seed: u64) -> Box<dyn IdSource> {
    Box::new(SeqIds::new(seed))
}

/// An archive holding the first `n` scripted commits, as durable bytes.
fn base(n: usize) -> (Vec<u8>, Vec<Step>) {
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps[..n]).unwrap();
    (s.contents(), steps)
}

/// The head of `src` must be one of the model states in `allowed`, completely.
fn assert_complete_head(src: &dyn ReadStorage, steps: &[Step], allowed: &[u64], ctx: &str) -> u64 {
    let head = open_head(src, &opts()).unwrap_or_else(|e| panic!("{ctx}: open_head: {e}"));
    let seq = head.seq();
    assert!(
        allowed.contains(&seq),
        "{ctx}: head {seq} not in {allowed:?}"
    );
    let state = read_state(src, &head).unwrap_or_else(|e| panic!("{ctx}: read: {e}"));
    assert_eq!(
        state, steps[seq as usize].after,
        "{ctx}: head {seq} is mixed"
    );
    seq
}

/// Append the next scripted commit to `storage` (opened fresh).
fn try_commit(
    storage: SimStorage,
    step: &Step,
) -> mochi_core::Result<mochi_core::publish::CommitOutcome> {
    let (mut w, _) =
        ArchiveWriter::open_append(storage, ids(99), test_options(), TailPolicy::Refuse)?;
    let job = Job::new();
    w.commit(step.tx.clone(), &job.ctx())
}

fn is_mutation(op: &Op) -> bool {
    matches!(
        op,
        Op::Append { .. } | Op::SyncData | Op::SyncDirectory | Op::Truncate { .. }
    )
}

const CRASH_MODES: &[CrashMode] = &[
    CrashMode::KeepAll,
    CrashMode::SyncedOnly,
    CrashMode::TornTail { keep_unsynced: 1 },
    CrashMode::TornTail { keep_unsynced: 9 },
    CrashMode::TornTail { keep_unsynced: 777 },
    CrashMode::TornTail {
        keep_unsynced: 5000,
    },
    CrashMode::LostWrites {
        seed: 1,
        sector: 512,
    },
    CrashMode::LostWrites {
        seed: 2,
        sector: 512,
    },
    CrashMode::LostWrites {
        seed: 3,
        sector: 4096,
    },
];

// ---- happy path ------------------------------------------------------------------------

#[test]
fn three_commits_round_trip_and_every_historical_commit_opens() {
    let steps = scripted_history();
    let s = SimStorage::new();
    let outcomes = build(s.clone(), 7, &steps).unwrap();
    for (i, o) in outcomes.iter().enumerate() {
        assert_eq!(o.status, CommitStatus::LocalCommitted);
        assert_eq!(o.seq, i as u64);
        assert_eq!(o.durability, PublishDurability::Durable);
    }
    assert_eq!(outcomes[2].committed_len, s.contents().len() as u64);

    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.location.source, HeadSource::Eof);
    assert!(head.location.tail.is_clean());
    assert_eq!(head.commit_id, outcomes[2].commit_id);
    assert_eq!(read_state(&s, &head).unwrap(), steps[2].after);

    let history = commit_history(&s, &opts()).unwrap();
    assert_eq!(history.len(), 3);
    for (i, h) in history.iter().enumerate() {
        assert_eq!(h.commit_id, outcomes[i].commit_id);
        assert_eq!(h.footer_offset, outcomes[i].footer_offset);
        let opened = open_at_footer(&s, h.footer_offset, &opts()).unwrap();
        assert_eq!(
            read_state(&s, &opened).unwrap(),
            steps[i].after,
            "commit {i}"
        );
    }
}

#[test]
fn session_layout_follows_spec_8_1() {
    // Commit 0: [descriptor] [data objects] [delta manifest] [snapshot
    // manifest] [catalog image] [commit] [footer]. Later commits: the same
    // without the descriptor, which exists exactly once, at offset 0 (D12).
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let bytes = s.contents();
    let frames: Vec<_> = Frames::new(&bytes[..], 0, Limits::default())
        .map(|f| f.unwrap())
        .collect();
    assert_eq!(frames[0].kind, FrameKind::ArchiveDescriptor);
    assert_eq!(frames[0].offset, 0);
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.kind == FrameKind::ArchiveDescriptor)
            .count(),
        1
    );
    let tail = [
        FrameKind::RecoveryManifest,
        FrameKind::RecoveryManifest,
        FrameKind::MetadataDelta,
        FrameKind::CommitRecord,
        FrameKind::CommitFooter,
    ];
    let mut i = 1;
    for _commit in 0..3 {
        while frames[i].kind == FrameKind::ZstdData {
            i += 1;
        }
        let got: Vec<FrameKind> = frames[i..i + 5].iter().map(|f| f.kind).collect();
        assert_eq!(got, tail);
        i += 5;
    }
    assert_eq!(i, frames.len());
    // Every commit references the one descriptor, and its two manifests in
    // that order (delta, then snapshot).
    for h in commit_history(&s, &opts()).unwrap() {
        assert_eq!(h.commit.descriptor.offset, 0);
        assert_eq!(h.commit.descriptor.stored_len, frames[0].len);
        let Metadata::Checkpoint { image, snapshot } = h.commit.metadata else {
            panic!("checkpoint expected")
        };
        assert!(h.commit.delta_manifest.offset < snapshot.offset);
        assert!(snapshot.offset < image.offset);
    }
}

#[test]
fn sync_order_follows_spec_12_2() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()[..1]).unwrap();
    let muts: Vec<Op> = s.trace().into_iter().filter(is_mutation).collect();
    let n = muts.len();
    // ... commit record, sync (6), footer (7), sync (8), directory (9).
    assert!(matches!(muts[n - 5], Op::Append { .. }));
    assert_eq!(muts[n - 4], Op::SyncData);
    assert!(matches!(muts[n - 3], Op::Append { len: 72, .. }));
    assert_eq!(muts[n - 2], Op::SyncData);
    assert_eq!(muts[n - 1], Op::SyncDirectory);
    // Only the creating commit needs a directory sync.
    let s2 = SimStorage::from_bytes(s.contents());
    try_commit(s2.clone(), &scripted_history()[1]).unwrap();
    assert!(!s2.trace().contains(&Op::SyncDirectory));
}

// ---- termination at each publication stage --------------------------------------------

#[test]
fn termination_at_each_publication_stage_never_mixes_commits() {
    let (bytes, steps) = base(2);
    // Count the mutations of an uninterrupted commit 2.
    let probe = SimStorage::from_bytes(bytes.clone());
    try_commit(probe.clone(), &steps[2]).unwrap();
    let total = probe.trace().iter().filter(|o| is_mutation(o)).count();
    assert!(total > 10, "commit 2 should span many writes ({total})");

    for i in 0..=total {
        let s = SimStorage::from_bytes(bytes.clone());
        s.add_fault(Fault::HaltBeforeMutation { mutation_index: i });
        let outcome = try_commit(s.clone(), &steps[2]);
        if i < total {
            assert!(
                outcome.is_err(),
                "halt before mutation {i} still reported success"
            );
        }
        for mode in CRASH_MODES {
            let img = s.crash_image(*mode);
            let ctx = format!("halt before mutation {i}/{total}, {mode:?}");
            let seq = assert_complete_head(&img, &steps, &[1, 2], &ctx);
            if outcome.is_ok() && *mode == CrashMode::SyncedOnly {
                assert_eq!(seq, 2, "{ctx}: acknowledged commit lost");
            }
        }
    }
}

#[test]
fn torn_writes_at_every_append_never_mix_commits() {
    let (bytes, steps) = base(2);
    let probe = SimStorage::from_bytes(bytes.clone());
    try_commit(probe.clone(), &steps[2]).unwrap();
    let appends: Vec<u64> = probe
        .trace()
        .iter()
        .filter_map(|o| match o {
            Op::Append { len, .. } => Some(*len),
            _ => None,
        })
        .collect();
    for (k, len) in appends.iter().enumerate() {
        for keep in [0, 1, 8, (*len / 2) as usize, (*len - 1) as usize] {
            let s = SimStorage::from_bytes(bytes.clone());
            s.add_fault(Fault::TearAppend {
                append_index: k,
                keep,
            });
            assert!(try_commit(s.clone(), &steps[2]).is_err());
            for mode in [CrashMode::KeepAll, CrashMode::SyncedOnly] {
                let ctx = format!("append {k} torn at {keep}/{len}, {mode:?}");
                // A torn write never completes the commit.
                assert_complete_head(&s.crash_image(mode), &steps, &[1], &ctx);
            }
        }
    }
}

#[test]
fn a_lying_sync_degrades_only_to_never_mixed() {
    // A device that acknowledges a flush it did not perform makes MOCHI's
    // acknowledgement false too (storage::os docs). What must still hold is
    // that no crash image ever opens to a mixed state.
    let (bytes, steps) = base(2);
    for sync_index in 0..2 {
        let s = SimStorage::from_bytes(bytes.clone());
        s.add_fault(Fault::LieSync { sync_index });
        let o = try_commit(s.clone(), &steps[2]).unwrap();
        assert_eq!(o.status, CommitStatus::LocalCommitted);
        for mode in CRASH_MODES {
            let ctx = format!("lying sync {sync_index}, {mode:?}");
            assert_complete_head(&s.crash_image(*mode), &steps, &[1, 2], &ctx);
        }
    }
}

#[test]
fn a_failed_sync_is_never_acknowledged() {
    let (bytes, steps) = base(2);

    // Step 6 fails: nothing published, bytes rolled back, writer stopped.
    let s = SimStorage::from_bytes(bytes.clone());
    s.add_fault(Fault::FailSync { sync_index: 0 });
    let (mut w, _) =
        ArchiveWriter::open_append(s.clone(), ids(5), test_options(), TailPolicy::Refuse).unwrap();
    let job = Job::new();
    let e = w.commit(steps[2].tx.clone(), &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::IoError);
    assert_eq!(s.contents(), bytes, "unpublished bytes were not removed");
    assert!(matches!(
        w.audit_log().last(),
        Some(AuditEvent::RolledBack { .. })
    ));
    let e = w.commit(steps[2].tx.clone(), &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::WriterPoisoned);
    drop(w);
    assert_complete_head(&s, &steps, &[1], "after failed step-6 sync");

    // Step 8 fails: the footer is written but not known durable.
    let s = SimStorage::from_bytes(bytes.clone());
    s.add_fault(Fault::FailSync { sync_index: 1 });
    let e = try_commit(s.clone(), &steps[2]).unwrap_err();
    assert_eq!(e.code, ErrorCode::CommitUnconfirmed);
    assert_complete_head(
        &s.crash_image(CrashMode::KeepAll),
        &steps,
        &[1, 2],
        "step 8, kept",
    );
    assert_complete_head(
        &s.crash_image(CrashMode::SyncedOnly),
        &steps,
        &[1],
        "step 8, power loss",
    );
}

#[test]
fn directory_durability_is_reported_never_assumed() {
    let steps = scripted_history();
    // POSIX-style failure: an error, never a commit.
    let s = SimStorage::with_faults([Fault::FailSyncDirectory { index: 0 }]);
    let mut w = ArchiveWriter::create(s.clone(), ids(1), test_options()).unwrap();
    let job = Job::new();
    let e = w.commit(steps[0].tx.clone(), &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::CommitUnconfirmed);

    // Windows-style best effort (O12): committed, but degraded.
    let s = SimStorage::with_faults([Fault::UnconfirmedSyncDirectory { index: 0 }]);
    let mut w = ArchiveWriter::create(s.clone(), ids(1), test_options()).unwrap();
    let o = w.commit(steps[0].tx.clone(), &job.ctx()).unwrap();
    assert_eq!(o.status, CommitStatus::LocalCommitted);
    assert!(matches!(
        o.durability,
        PublishDurability::DirectoryUnconfirmed(_)
    ));
    // Appends need no directory sync, so they are durable.
    let o = w.commit(steps[1].tx.clone(), &job.ctx()).unwrap();
    assert_eq!(o.durability, PublishDurability::Durable);
}

// ---- truncation at every byte ------------------------------------------------------------

#[test]
fn truncation_at_every_byte_accepts_no_invalid_commit() {
    let steps = scripted_history();
    let s = SimStorage::new();
    let outcomes = build(s.clone(), 7, &steps).unwrap();
    let bytes = s.contents();
    let ends: Vec<u64> = outcomes.iter().map(|o| o.committed_len).collect();

    // Locating the head decides acceptance; everything open_head reads lies
    // before the head footer, so it is checked in full near every boundary
    // and on a stride, not at all ~10^5 lengths (SQLite opens dominate).
    let near_boundary = |l: u64| ends.iter().any(|e| l.abs_diff(*e) <= 80) || l.is_multiple_of(509);
    for l in 0..=bytes.len() as u64 {
        let prefix = SimStorage::from_bytes(bytes[..l as usize].to_vec());
        let expected = ends.iter().rposition(|e| *e <= l);
        match (locate_head(&prefix, &Limits::default()), expected) {
            (Err(e), None) => assert_eq!(e.code, ErrorCode::NoValidHead, "len {l}"),
            (Ok(loc), Some(k)) => {
                assert_eq!(loc.footer.fields.commit_sequence, k as u64, "len {l}");
                assert_eq!(loc.committed_len, ends[k], "len {l}");
                let exact = l == ends[k];
                assert_eq!(loc.source == HeadSource::Eof, exact, "len {l}");
                // A cut-short commit is always provably uncommitted.
                if !exact {
                    assert!(
                        matches!(loc.tail, TailState::Uncommitted { .. }),
                        "len {l}: {:?}",
                        loc.tail
                    );
                }
                if near_boundary(l) {
                    assert_complete_head(&prefix, &steps, &[k as u64], &format!("len {l}"));
                }
            }
            (got, want) => panic!("len {l}: expected head {want:?}, got {got:?}"),
        }
    }
}

// ---- tails --------------------------------------------------------------------------------

/// An archive with commits 0–1 and an interrupted commit 2 (no footer).
fn interrupted() -> (SimStorage, Vec<Step>, usize) {
    let (bytes, steps) = base(2);
    let s = SimStorage::from_bytes(bytes.clone());
    // Halt just before the footer: every other write of commit 2 happened.
    let probe = SimStorage::from_bytes(bytes.clone());
    try_commit(probe.clone(), &steps[2]).unwrap();
    let before_footer = probe.trace().iter().filter(|o| is_mutation(o)).count() - 2;
    s.add_fault(Fault::HaltBeforeMutation {
        mutation_index: before_footer,
    });
    try_commit(s.clone(), &steps[2]).unwrap_err();
    (s.crash_image(CrashMode::KeepAll), steps, bytes.len())
}

#[test]
fn an_uncommitted_tail_is_removed_only_explicitly_and_audited() {
    let (s, steps, committed) = interrupted();
    let tail_len = s.contents().len() - committed;
    assert!(tail_len > 0);

    let e = ArchiveWriter::open_append(s.clone(), ids(3), test_options(), TailPolicy::Refuse)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::UncommittedTail);
    assert_eq!(
        s.contents().len(),
        committed + tail_len,
        "refusal changed the file"
    );

    let (mut w, t) = ArchiveWriter::open_append(
        s.clone(),
        ids(3),
        test_options(),
        TailPolicy::TruncateUncommitted,
    )
    .unwrap();
    let t = t.expect("an audit record");
    assert_eq!(t.committed_len, committed as u64);
    assert_eq!(t.removed_len, tail_len as u64);
    assert_eq!(t.head_seq, 1);
    assert_eq!(t.removed_frames.last(), Some(&FrameKind::CommitRecord));
    assert!(!t.incomplete_final_frame);
    assert!(matches!(w.audit_log(), [AuditEvent::TailTruncated(x)] if *x == t));
    assert_eq!(s.contents().len(), committed);

    let job = Job::new();
    assert_eq!(w.commit(steps[2].tx.clone(), &job.ctx()).unwrap().seq, 2);
    drop(w);
    assert_complete_head(&s, &steps, &[2], "after truncation and retry");
}

#[test]
fn a_corrupted_latest_footer_is_never_truncated() {
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let mut bytes = s.contents();
    let n = bytes.len();
    bytes[n - 1] ^= 0x01; // inside the latest footer's digest
    let damaged = SimStorage::from_bytes(bytes.clone());

    // Readers fall back to the previous commit, and say so.
    let head = open_head(&damaged, &opts()).unwrap();
    assert_eq!(head.seq(), 1);
    assert_eq!(head.location.source, HeadSource::Scan);
    assert!(matches!(head.location.tail, TailState::Unresolved { .. }));

    // Writers refuse to destroy what may be a damaged commit.
    let e = ArchiveWriter::open_append(
        damaged.clone(),
        ids(3),
        test_options(),
        TailPolicy::TruncateUncommitted,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::TailUnresolved);
    assert_eq!(damaged.contents(), bytes);
}

#[test]
fn a_damaged_commit_hidden_from_the_walk_is_still_found() {
    // The case rule (b) of the tail classifier exists for: a later commit
    // whose footer the structural walk cannot reach because an earlier frame
    // length was corrupted to run past EOF. The walk then ends in
    // "truncated", which alone would look like an interrupted write.
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    let n = bytes.len();
    // Commit 2's manifest frame claims 200 MiB (under the 256 MiB limit,
    // past EOF), and commit 2's footer digest is damaged so EOF does not
    // validate either.
    let m = head.commit.delta_manifest.offset as usize;
    bytes[m + 4..m + 8].copy_from_slice(&(200u32 << 20).to_le_bytes());
    bytes[n - 1] ^= 1;
    let damaged = SimStorage::from_bytes(bytes);

    let loc = locate_head(&damaged, &Limits::default()).unwrap();
    assert_eq!(loc.footer.fields.commit_sequence, 1);
    assert!(
        matches!(loc.tail, TailState::Unresolved { .. }),
        "{:?}",
        loc.tail
    );
    let e = ArchiveWriter::open_append(
        damaged.clone(),
        ids(3),
        test_options(),
        TailPolicy::TruncateUncommitted,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::TailUnresolved);
}

#[test]
fn a_valid_eof_footer_is_accepted_even_if_content_is_damaged() {
    // Scope note: the footer authenticates the commit chain, not every data
    // object. A damaged chunk is found by content verification (C7) and by
    // read_state here, never by head location.
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps[..1]).unwrap();
    let mut bytes = s.contents();
    let first_data = Frames::new(&bytes[..], 0, Limits::default())
        .map(|f| f.unwrap())
        .find(|f| f.kind == FrameKind::ZstdData)
        .unwrap();
    bytes[first_data.offset as usize + 12] ^= 1; // inside the first data object
    let damaged = SimStorage::from_bytes(bytes);
    let head = open_head(&damaged, &opts()).unwrap();
    assert_eq!(head.location.source, HeadSource::Eof);
    let e = read_state(&damaged, &head).unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
}

// ---- integrity of what the footer names ---------------------------------------------------

#[test]
fn a_value_level_bit_flip_in_the_catalog_image_is_refused() {
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let Metadata::Checkpoint { image: meta, .. } = head.commit.metadata else {
        panic!("this writer emits checkpoints")
    };
    let mut bytes = s.contents();

    // Find a stored file-content hash inside the image and flip one bit of it.
    let (_, entry) = head
        .catalog
        .replay(None)
        .unwrap()
        .iter()
        .find(|(p, _)| p.as_stored() == b"big")
        .map(|(p, e)| (p.clone(), *e))
        .unwrap();
    let (version, _) = head.catalog.file_version(&entry.version).unwrap().unwrap();
    let needle = version.content_hash.unwrap();
    let image = &bytes[meta.offset as usize..(meta.offset + meta.stored_len) as usize];
    let at = image
        .windows(32)
        .position(|w| w == needle.as_bytes())
        .expect("hash stored in the image") as u64
        + meta.offset;
    bytes[at as usize + 5] ^= 0x10;

    // SQLite alone accepts the flipped image: it is a different valid catalog.
    // (The image starts after the frame header and the 80-byte binary
    // envelope, n = 0: B.2.2.)
    let flipped = &bytes[(meta.offset as usize + SKIPPABLE_HEADER_LEN + 80)
        ..(meta.offset + meta.stored_len) as usize];
    let cat = Catalog::open_image(flipped, &Default::default()).unwrap();
    assert_ne!(
        cat.file_version(&entry.version)
            .unwrap()
            .unwrap()
            .0
            .content_hash,
        Some(needle)
    );

    // The reader refuses it before SQLite sees it.
    let e = open_head(&SimStorage::from_bytes(bytes), &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
}

/// T10: every checkpoint's image is a binary envelope v0 bound to that
/// commit's identity, with the catalog's `user_version` as its schema.
#[test]
fn every_image_is_enveloped_and_bound_to_its_commit() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let bytes = s.contents();
    let history = commit_history(&s, &opts()).unwrap();
    assert_eq!(history.len(), 3);
    for h in &history {
        let Metadata::Checkpoint { image, .. } = h.commit.metadata else {
            panic!("checkpoint expected")
        };
        let stored = StoredObject::from_loaded(
            bytes[image.offset as usize..(image.offset + image.stored_len) as usize].to_vec(),
        );
        let env = &stored.as_bytes()[SKIPPABLE_HEADER_LEN..];
        assert_eq!(&env[0..4], &80u32.to_le_bytes(), "no required features");
        assert_eq!(&env[6..8], &IMAGE_RECORD_SCHEMA.to_le_bytes());
        let img = decode_image_record(&stored, &h.commit.identity(), &Limits::default()).unwrap();
        assert!(img.starts_with(b"SQLite format 3\0"));
        // Bound to this commit only.
        for other in history.iter().filter(|o| o.commit.seq != h.commit.seq) {
            assert_eq!(
                decode_image_record(&stored, &other.commit.identity(), &Limits::default())
                    .unwrap_err()
                    .code,
                ErrorCode::EnvelopeInvalid
            );
        }
    }
}

/// T10 / D11 identity, end to end: a commit that references, by a correct
/// hash, the image of an *earlier* commit is refused at the envelope with
/// `ENVELOPE_INVALID`, before SQLite sees the image (whose own head-commit
/// check would otherwise have said `RECORD_INVALID`).
#[test]
fn a_hash_valid_image_of_another_commit_is_refused_before_sqlite() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let history = commit_history(&s, &opts()).unwrap();
    let (head, prev) = (&history[0], &history[1]);
    let Metadata::Checkpoint {
        image: prev_image, ..
    } = prev.commit.metadata
    else {
        panic!("checkpoint expected")
    };
    let Metadata::Checkpoint { snapshot, .. } = head.commit.metadata else {
        panic!("checkpoint expected")
    };
    // A forged head: same commit, but its image ref names commit 1's image.
    let mut forged = head.commit.clone();
    forged.metadata = Metadata::Checkpoint {
        image: prev_image,
        snapshot,
    };
    let (frame, _) = forged.to_stored().unwrap();
    let mut bytes = s.contents();
    let commit_offset = bytes.len() as u64;
    bytes.extend_from_slice(frame.as_bytes());
    bytes.extend_from_slice(&encode_footer_frame(
        commit_offset,
        forged.seq,
        frame.as_bytes(),
    ));
    let e = open_head(&SimStorage::from_bytes(bytes), &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::EnvelopeInvalid, "{e}");
}

#[test]
fn a_damaged_manifest_is_refused() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let m = head.commit.delta_manifest;
    let mut bytes = s.contents();
    bytes[(m.offset + m.stored_len - 1) as usize] ^= 1;
    let e = open_head(&SimStorage::from_bytes(bytes), &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
}

fn manifests_in(bytes: &[u8]) -> Vec<StoredObject> {
    Frames::new(bytes, 0, Limits::default())
        .map(|f| f.unwrap())
        .filter(|f| f.kind == FrameKind::RecoveryManifest)
        .map(|f| StoredObject::from_loaded(bytes[f.offset as usize..f.end() as usize].to_vec()))
        .collect()
}

#[test]
fn a_destroyed_catalog_is_recovered_through_the_footer_verified_head() {
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let original = open_head(&s, &opts()).unwrap();
    let mut bytes = s.contents();
    // Zero every catalog image payload, keeping frame headers so the file
    // still walks: every SQLite catalog is destroyed.
    let spans: Vec<_> = Frames::new(&bytes[..], 0, Limits::default())
        .map(|f| f.unwrap())
        .filter(|f| f.kind == FrameKind::MetadataDelta)
        .collect();
    assert_eq!(spans.len(), 3);
    for f in spans {
        bytes[(f.offset as usize + SKIPPABLE_HEADER_LEN)..f.end() as usize].fill(0);
    }
    let damaged = SimStorage::from_bytes(bytes);
    assert_eq!(
        open_head(&damaged, &opts()).unwrap_err().code,
        ErrorCode::StoredIntegrityFailed
    );
    let rec = recover_with_trusted_head(&damaged, &opts()).unwrap();
    assert_eq!(rec.scope_for(2), RecoveryScope::SnapshotRecovery);
    assert_eq!(rec.scope_for(0), RecoveryScope::HistoricalRecovery);
    let cat = rec.catalog.as_ref().unwrap();
    for seq in 0..=2 {
        assert_eq!(
            cat.replay(Some(seq)).unwrap(),
            original.catalog.replay(Some(seq)).unwrap()
        );
    }
}

#[test]
fn a_substituted_manifest_chain_is_only_caught_with_the_footer_head() {
    // C4 found that a rewritten-and-relinked manifest chain is internally
    // consistent. Build one from a *different* history of the same archive
    // shape (another seed), and offer only it to recovery.
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let forged_src = SimStorage::new();
    build(forged_src.clone(), 8, &steps).unwrap();

    // Untrusted recovery accepts whatever consistent chain it is given.
    let forged = manifests_in(&forged_src.contents());
    let rec = recover_from_manifests(&forged, None, &Limits::default()).unwrap();
    assert_eq!(rec.scope_for(2), RecoveryScope::SnapshotRecovery);

    // With the head taken from this file's footer, the forged chain is not
    // this archive's history: the named head manifest is absent.
    let head = read_commit(
        &s,
        &locate_head(&s, &Limits::default()).unwrap().footer,
        &opts(),
    )
    .unwrap()
    .0;
    let e = recover_from_manifests(
        &forged,
        Some(head.delta_manifest.stored_hash),
        &Limits::default(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // And the real chain recovers.
    let real = manifests_in(&s.contents());
    assert!(recover_from_manifests(
        &real,
        Some(head.delta_manifest.stored_hash),
        &Limits::default()
    )
    .is_ok());
}

// ---- cancellation, locking, configuration ------------------------------------------------

/// Cancels its token at the `at`-th progress event.
struct CancelAt {
    at: usize,
    seen: AtomicUsize,
    token: CancellationToken,
    phases: std::sync::Mutex<Vec<&'static str>>,
}

impl ProgressSink for CancelAt {
    fn report(&self, e: &ProgressEvent) {
        self.phases.lock().unwrap().push(e.phase);
        if self.seen.fetch_add(1, Ordering::SeqCst) == self.at {
            self.token.cancel();
        }
    }
}

#[test]
fn cancellation_before_the_footer_leaves_the_previous_head() {
    let (bytes, steps) = base(2);
    let mut saw_cancel = 0;
    let mut at = 0;
    loop {
        let s = SimStorage::from_bytes(bytes.clone());
        let (mut w, _) =
            ArchiveWriter::open_append(s.clone(), ids(4), test_options(), TailPolicy::Refuse)
                .unwrap();
        let token = CancellationToken::new();
        let sink = CancelAt {
            at,
            seen: AtomicUsize::new(0),
            token: token.clone(),
            phases: Default::default(),
        };
        let ctx = JobContext {
            progress: &sink,
            cancel: &token,
        };
        match w.commit(steps[2].tx.clone(), &ctx) {
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Cancelled, "event {at}");
                assert_eq!(s.contents(), bytes, "event {at}: bytes left behind");
                // Not poisoned: the same writer commits once uncancelled.
                let job = Job::new();
                assert_eq!(w.commit(steps[2].tx.clone(), &job.ctx()).unwrap().seq, 2);
                drop(w);
                assert_complete_head(&s, &steps, &[2], "retry after cancel");
                saw_cancel += 1;
            }
            Ok(o) => {
                // Cancelled at or after the footer: the commit completes.
                let phases = sink.phases.lock().unwrap().clone();
                let footer_at = phases.iter().position(|p| *p == "footer").unwrap();
                assert!(
                    at > footer_at,
                    "event {at} was before the footer yet ignored"
                );
                assert_eq!(o.seq, 2);
                break;
            }
        }
        at += 1;
    }
    assert!(saw_cancel > 10, "only {saw_cancel} cancellation points");
}

#[test]
fn a_second_writer_is_refused_and_an_unlocked_write_is_detected() {
    let (bytes, steps) = base(2);
    let s = SimStorage::from_bytes(bytes);
    let (mut a, _) =
        ArchiveWriter::open_append(s.handle(), ids(1), test_options(), TailPolicy::Refuse).unwrap();
    let e = ArchiveWriter::open_append(s.handle(), ids(2), test_options(), TailPolicy::Refuse)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);

    // A process ignoring the advisory lock appends; the holder notices
    // before writing anything, so there is never a mixed publication.
    let mut rogue = s.handle();
    rogue.append(b"rogue").unwrap();
    let before = s.contents();
    let job = Job::new();
    let e = a.commit(steps[2].tx.clone(), &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    assert_eq!(s.contents(), before);
}

#[test]
fn writer_parameters_are_fixed_at_creation() {
    let (bytes, _) = base(1);
    let mut o = test_options();
    o.chunk_size = Some(65);
    let e = ArchiveWriter::open_append(
        SimStorage::from_bytes(bytes.clone()),
        ids(1),
        o,
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    let mut o = test_options();
    o.chunk_size = None;
    o.zstd_level = None;
    assert!(ArchiveWriter::open_append(
        SimStorage::from_bytes(bytes),
        ids(1),
        o,
        TailPolicy::Refuse
    )
    .is_ok());
}

#[test]
fn namespace_errors_publish_nothing() {
    let (bytes, _) = base(1);
    let s = SimStorage::from_bytes(bytes.clone());
    let (mut w, _) =
        ArchiveWriter::open_append(s.clone(), ids(1), test_options(), TailPolicy::Refuse).unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("nodir/x"), b"orphan".to_vec(), Default::default());
    let job = Job::new();
    let e = w.commit(tx, &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::NamespaceInvalid);
    assert_eq!(s.contents(), bytes);
    let mut tx = Transaction::new();
    tx.delete(path("absent"));
    assert_eq!(
        w.commit(tx, &job.ctx()).unwrap_err().code,
        ErrorCode::NamespaceInvalid
    );
    assert_eq!(s.contents(), bytes);
}

#[test]
fn an_empty_file_has_no_head_and_create_needs_empty_storage() {
    let s = SimStorage::new();
    let w = ArchiveWriter::create(s.clone(), ids(1), test_options()).unwrap();
    drop(w);
    assert_eq!(
        open_head(&s, &opts()).unwrap_err().code,
        ErrorCode::NoValidHead
    );
    let (bytes, _) = base(1);
    let e =
        ArchiveWriter::create(SimStorage::from_bytes(bytes), ids(1), test_options()).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

// ---- the real filesystem ----------------------------------------------------------------

#[test]
fn os_storage_end_to_end() {
    use mochi_core::storage::os::{OsReadStorage, OsStorage};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.mochi");
    let steps = scripted_history();
    build(OsStorage::create_new(&file).unwrap(), 7, &steps[..2]).unwrap();
    let (mut w, _) = ArchiveWriter::open_append(
        OsStorage::open_existing(&file).unwrap(),
        ids(9),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let e = ArchiveWriter::open_append(
        OsStorage::open_existing(&file).unwrap(),
        ids(9),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::LockConflict);
    let job = Job::new();
    w.commit(steps[2].tx.clone(), &job.ctx()).unwrap();
    w.close().unwrap();
    let r = OsReadStorage::open(&file).unwrap();
    let head = open_head(&r, &opts()).unwrap();
    let state: State = read_state(&r, &head).unwrap();
    assert_eq!(state, steps[2].after);
    assert!(matches!(
        state.get(b"<img src=x onerror=alert(1)>".as_slice()),
        Some(Content::File(_))
    ));
}

// ---- schema 1: descriptor, snapshots, delta commits (plan T8, T9) -------------------------

/// D12: a damaged descriptor refuses interpretation with DESCRIPTOR_INVALID,
/// while head discovery and commit validation still work. (Full D12 failure
/// behaviour, including verify reporting, is T18.)
#[test]
fn a_damaged_descriptor_is_descriptor_invalid_but_heads_are_still_found() {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let mut bytes = s.contents();
    bytes[20] ^= 1; // inside the descriptor's archive ID
    let damaged = SimStorage::from_bytes(bytes);
    let e = open_head(&damaged, &opts()).unwrap_err();
    assert_eq!(e.code, ErrorCode::DescriptorInvalid, "{e}");
    assert_eq!(commit_history(&damaged, &opts()).unwrap().len(), 3);
    let e = ArchiveWriter::open_append(damaged, ids(5), test_options(), TailPolicy::Refuse)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::DescriptorInvalid);
}

/// The snapshot manifest every commit binds is the complete state the
/// independent model says, with every promised attribute, and carries the
/// commit's identity (D10.2, D10.3, D11).
#[test]
fn every_snapshot_equals_the_model_including_attributes() {
    use mochi_core::manifest::ManifestKind;
    use mochi_core::publish::read_snapshot;
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    for (i, h) in commit_history(&s, &opts()).unwrap().iter().enumerate() {
        let opened = open_at_footer(&s, h.footer_offset, &opts()).unwrap();
        let snap = read_snapshot(&s, &opened, &opts()).unwrap();
        assert_eq!(snap.kind, ManifestKind::Snapshot);
        assert!(snap.parent.is_none());
        assert_eq!(snap.identity(), h.commit.identity());
        let paths: Vec<Vec<u8>> = snap
            .entries
            .iter()
            .map(|(p, _)| p.as_stored().to_vec())
            .collect();
        let want: Vec<Vec<u8>> = steps[i].after.keys().cloned().collect();
        assert_eq!(paths, want, "commit {i}");
        // Attributes: the scripted history gives dirs 0o755 and files 0o644,
        // uid/gid 1000, a fixed mtime. None may be dropped.
        for (p, v) in &snap.entries {
            let fv = snap
                .file_versions
                .iter()
                .find(|f| f.version.id == *v)
                .unwrap();
            let posix = fv.attributes.posix.expect("attributes kept");
            let want_mode = match steps[i].after[p.as_stored()] {
                Content::Dir => 0o755,
                Content::File(_) => 0o644,
            };
            assert_eq!(posix.mode, want_mode, "commit {i} {:?}", p);
            assert!(fv.attributes.mtime.is_some());
        }
        // And it names exactly the versions the delta chain and catalog do.
        let cat_snapshot = opened.catalog.replay(None).unwrap();
        assert_eq!(cat_snapshot.len(), snap.entries.len());
        for (p, v) in &snap.entries {
            assert_eq!(cat_snapshot.get(p).map(|e| e.version), Some(*v));
        }
    }
}

/// D10.9: a damaged snapshot manifest with an intact image leaves reads
/// working. Appending refuses (this build's choice: it could not write a
/// complete next snapshot; see the publish module docs).
#[test]
fn a_damaged_snapshot_leaves_reads_working_and_blocks_append() {
    use mochi_core::publish::read_snapshot;
    let steps = scripted_history();
    let s = SimStorage::new();
    build(s.clone(), 7, &steps).unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let Metadata::Checkpoint { snapshot, .. } = head.commit.metadata else {
        panic!("checkpoint expected")
    };
    let mut bytes = s.contents();
    bytes[(snapshot.offset + snapshot.stored_len - 1) as usize] ^= 1;
    let damaged = SimStorage::from_bytes(bytes);
    let head = open_head(&damaged, &opts()).unwrap();
    assert_eq!(read_state(&damaged, &head).unwrap(), steps[2].after);
    assert_eq!(
        read_snapshot(&damaged, &head, &opts()).unwrap_err().code,
        ErrorCode::StoredIntegrityFailed
    );
    let e = ArchiveWriter::open_append(damaged, ids(5), test_options(), TailPolicy::Refuse)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
    assert!(e.message.contains("snapshot"), "{}", e.message);
}

/// A first commit that is cancelled leaves an empty file, not an orphan
/// descriptor; the next attempt writes the descriptor at offset 0 again.
#[test]
fn a_cancelled_first_commit_leaves_no_descriptor_behind() {
    let steps = scripted_history();
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), ids(7), test_options()).unwrap();
    let token = CancellationToken::new();
    let sink = CancelAt {
        at: 2,
        seen: AtomicUsize::new(0),
        token: token.clone(),
        phases: Default::default(),
    };
    let ctx = JobContext {
        progress: &sink,
        cancel: &token,
    };
    assert_eq!(
        w.commit(steps[0].tx.clone(), &ctx).unwrap_err().code,
        ErrorCode::Cancelled
    );
    assert!(s.contents().is_empty(), "descriptor left behind");
    let job = Job::new();
    w.commit(steps[0].tx.clone(), &job.ctx()).unwrap();
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.commit.descriptor.offset, 0);
    assert_eq!(read_state(&s, &head).unwrap(), steps[0].after);
}
