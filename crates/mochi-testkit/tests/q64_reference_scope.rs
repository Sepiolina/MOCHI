//! Q64: references outside a segment's baseline view (spec Annex B D10.4,
//! D10.8, D18 "reference scope"; `mochi_core::verify`, `deep`).
//!
//! A delta may reference an existing version, but baseline recovery sees only
//! S(*b*) plus the deltas after *b*. The expectations here are written from
//! that rule: a forged delta puts a version that was reachable only before
//! the segment's base, and the oracle for "fine" is the stored archive of the
//! real writer (deduplicating, compacting), read back by a fresh reader.

use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::job::JobContext;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, recover_baseline_at_footer, ArchiveWriter,
    CheckpointPolicy, Dedup, ReadOptions, TailPolicy, Transaction, WriterOptions,
};
use mochi_core::report::Report;
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::forge::{empty_delta, rule_base, txid, Forge};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn fsck(s: &SimStorage, level: VerificationLevel, deep: bool) -> Report {
    let o = VerifyOptions {
        level,
        deep,
        ..VerifyOptions::default()
    };
    let r = verify(s, &o, &Job::new().ctx()).report;
    r.validate()
        .unwrap_or_else(|e| panic!("report breaks the invariants: {e:?}"));
    r
}

fn count(r: &Report, code: ErrorCode) -> usize {
    r.findings.iter().filter(|f| f.code == code).count()
}

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(path(p), deterministic_bytes(seed, 150), attrs(0o644, 0));
}

/// Four commits, checkpoints at 0 and 2: `a` (seed 1) is replaced at 1 and
/// so is not reachable at the second base.
fn source() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&mut w, &|t| put(t, "a", 1)); // 0: checkpoint
    c(&mut w, &|t| put(t, "a", 2)); // 1: delta, replaces version 1 of a
    w.request_checkpoint();
    c(&mut w, &|t| put(t, "b", 3)); // 2: checkpoint, S(2) has no version 1 of a
    c(&mut w, &|t| put(t, "c", 4)); // 3: delta
    w.close().unwrap();
    s
}

/// Which kind of out-of-view reference the forged commit 4 makes.
#[derive(Clone, Copy)]
enum Reach {
    /// A put of the existing version that only commit 0 reaches.
    Version,
    /// A put of a *new* version whose extent names the chunk that only
    /// commit 0 reaches.
    Chunk,
}

/// `source()` plus forged commit 4: a delta on base 2 with one out-of-view
/// reference of the given kind. Returns the archive and the commit's seq.
fn forged(reach: Reach) -> (SimStorage, u64) {
    use mochi_core::catalog::extent::{Extent, ExtentSource};
    use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
    use mochi_core::catalog::FileVersion;
    use mochi_core::manifest::FileVersionEntry;

    let pristine = source();
    let mut f = Forge::new(pristine.contents());
    let head = f.history()[3].clone();
    let first = open_at_footer(&pristine, f.history()[0].footer_offset, &opts()).unwrap();
    let old = first
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("a"))
        .unwrap()
        .version;
    let mut m = empty_delta(&head, txid(0x64));
    match reach {
        Reach::Version => m.ops.push(NamespaceOp::Put {
            path: path("old-a"),
            version: old,
        }),
        Reach::Chunk => {
            let (version, extents) = first.catalog.file_version(&old).unwrap().unwrap();
            // A different version ID over the same bytes and chunks: the
            // version is introduced here, the chunk is not.
            let new = FileVersionId::from_bytes([0x64; 32]);
            assert_ne!(new, old);
            m.file_versions.push(FileVersionEntry {
                version: FileVersion {
                    id: new,
                    kind: EntryKind::File,
                    logical_len: version.logical_len,
                    content_hash: version.content_hash,
                },
                extents: extents
                    .iter()
                    .map(|e| Extent {
                        source: match e.source {
                            ExtentSource::Chunk {
                                chunk,
                                chunk_offset,
                            } => ExtentSource::Chunk {
                                chunk,
                                chunk_offset,
                            },
                            ExtentSource::Hole => ExtentSource::Hole,
                        },
                        ..*e
                    })
                    .collect(),
                attributes: attrs(0o644, 5),
            });
            m.ops.push(NamespaceOp::Put {
                path: path("old-a"),
                version: new,
            });
        }
    }
    let r = f.append_manifest(&m);
    let entry = f.append_delta(&head, rule_base(&head), r, txid(0x64));
    (f.storage(), entry.commit.seq)
}

/// **The check finds a reference outside the baseline view.** The forged
/// commit opens (its image-based catalog holds every version), a reader
/// reads it, and `verify` without `deep` is unchanged; `fsck` reports
/// `REFERENCE_INVALID` against that commit, Recoverability `FAIL`, and
/// Integrity untouched.
#[test]
fn q64_fsck_reports_a_reference_outside_the_baseline_view() {
    let (s, seq) = forged(Reach::Version);
    assert_eq!(seq, 4);

    // It opens, and the version is readable: nothing refuses it.
    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.seq(), 4);
    assert!(head
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("old-a"))
        .is_some());
    // The oracle for "baseline recovery cannot": the real recovery says so.
    let footer = head.location.footer.footer_offset;
    assert!(recover_baseline_at_footer(&s, footer, &opts()).is_err());

    // Not deep: unchanged. No baseline check runs.
    for level in [
        VerificationLevel::Structural,
        VerificationLevel::Restoration,
    ] {
        let r = fsck(&s, level, false);
        assert_eq!(count(&r, ErrorCode::ReferenceInvalid), 0, "{level:?}");
    }

    // Deep: found, once, against commit 4.
    let r = fsck(&s, VerificationLevel::Restoration, true);
    assert_eq!(
        count(&r, ErrorCode::ReferenceInvalid),
        1,
        "{:?}",
        r.findings
    );
    let f = r
        .findings
        .iter()
        .find(|f| f.code == ErrorCode::ReferenceInvalid)
        .unwrap();
    assert!(
        f.message.as_deref().unwrap().contains("commit 4"),
        "{:?}",
        f.message
    );
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Fail);
    assert_eq!(
        r.dimensions[&Dimension::Integrity],
        Status::Pass,
        "the bytes are intact: {:?}",
        r.findings
    );
    assert_eq!(r.exit_code, 1, "a required dimension fails");
}

/// **An extent naming a chunk outside the view is found too.** The version is
/// new (introduced by the forged commit), but its extents name a chunk that
/// only commit 0 reaches, so a baseline replay cannot resolve it either.
#[test]
fn q64_fsck_reports_an_extent_naming_a_chunk_outside_the_view() {
    let (s, seq) = forged(Reach::Chunk);
    assert_eq!(seq, 4);
    let head = open_head(&s, &opts()).unwrap();
    assert!(head
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("old-a"))
        .is_some());
    assert!(recover_baseline_at_footer(&s, head.location.footer.footer_offset, &opts()).is_err());

    let r = fsck(&s, VerificationLevel::Restoration, true);
    assert_eq!(
        count(&r, ErrorCode::ReferenceInvalid),
        1,
        "{:?}",
        r.findings
    );
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Fail);
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Pass);
    assert_eq!(r.exit_code, 1);
}

/// **The same fixture without the forged commit is clean**, so the finding
/// above is the reference's and nothing else's.
#[test]
fn q64_the_unforged_archive_has_no_finding() {
    let r = fsck(&source(), VerificationLevel::Restoration, true);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Pass);
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Pass);
    assert_eq!(r.exit_code, 0);
}

fn content(seed: u64) -> Vec<u8> {
    deterministic_bytes(seed, 200)
}

/// Deduplication across checkpoints, as in `c9_dedup.rs`: reuse is limited to
/// what a baseline replay sees (a chunk unreachable at the base is stored
/// again), and a chunk introduced after the base is reused.
fn dedup_archive() -> SimStorage {
    let s = SimStorage::new();
    let (x, y) = (content(6), content(7));
    let o = WriterOptions {
        dedup: Dedup::InArchive,
        ..test_options()
    };
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(80)), o).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let commit = |w: &mut ArchiveWriter<SimStorage>, files: &[(&str, &[u8])]| {
        let mut tx = Transaction::new();
        for (p, b) in files {
            tx.put_file(path(p), b.to_vec(), attrs(0o644, 0));
        }
        w.commit(tx, &job.ctx()).unwrap()
    };
    commit(&mut w, &[("a", &x)]); // 0: checkpoint
    commit(&mut w, &[("c", &y)]); // 1
    let mut tx = Transaction::new();
    tx.delete(path("a"));
    w.request_checkpoint();
    w.commit(tx, &job.ctx()).unwrap(); // 2: checkpoint without x
    commit(&mut w, &[("b", &x)]); // 3: x stored again
    let mut tx = Transaction::new();
    tx.delete(path("b"));
    w.commit(tx, &job.ctx()).unwrap(); // 4
    let out = commit(&mut w, &[("d", &x)]); // 5: reuses b's chunks
    assert_eq!(out.dedup.chunks_reused, 4);
    w.close().unwrap();
    s
}

/// **Writers never produce one (spec Annex B D10.4; plan C9).** A
/// deduplicating archive with reuse inside a segment and a re-store across a
/// checkpoint has no finding; nor has its compaction, which copies every
/// snapshot and forces a checkpoint where a chunk would be out of view.
#[test]
fn q64_writer_output_has_no_finding_with_dedup_and_compaction() {
    let s = dedup_archive();
    let r = fsck(&s, VerificationLevel::Restoration, true);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Pass);

    let writer = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(81)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0;
    let job = Job::new();
    let mut dir = SimDir::new();
    compact(
        &writer,
        &mut dir,
        "out.mochi",
        Box::new(SeqIds::new(82)),
        &Keep::Every,
        &CompactOptions {
            checkpoint_policy: Some(CheckpointPolicy::Every(2)),
            ..CompactOptions::default()
        },
        &job.ctx(),
    )
    .unwrap();
    drop(writer);
    let out = dir.file("out.mochi").unwrap();
    assert!(commit_history(&out, &opts()).unwrap().len() >= 6);
    let r = fsck(&out, VerificationLevel::Restoration, true);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Pass);
    assert_eq!(r.exit_code, 0);
}

/// **Damage is reported as damage, not as a reference defect.** A flipped
/// byte in commit 1's delta manifest (a middle commit: the head still opens,
/// so `verify` reaches the deep checks) stops that commit opening at all, and
/// baseline recovery of its segment with it. The deep check already reports
/// that, and the reference check must not add a second, wrong diagnosis.
#[test]
fn q64_a_commit_that_does_not_open_is_not_blamed_for_references() {
    let s = source();
    let h = commit_history(&s, &opts()).unwrap();
    let m = h[1].commit.delta_manifest;
    let mut bytes = s.contents();
    let at = (m.offset + m.stored_len / 2) as usize;
    bytes[at] ^= 0xFF;
    let damaged = SimStorage::from_bytes(bytes);
    assert!(open_at_footer(&damaged, h[1].footer_offset, &opts()).is_err());
    assert!(open_head(&damaged, &opts()).is_ok(), "the head still opens");

    let r = fsck(&damaged, VerificationLevel::Restoration, true);
    assert_eq!(
        count(&r, ErrorCode::ReferenceInvalid),
        0,
        "{:?}",
        r.findings
    );
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Fail);
    assert_eq!(r.dimensions[&Dimension::Recoverability], Status::Fail);
}

/// Cancellation inside the new phase is an operational stop, not a finding.
#[test]
fn q64_cancellation_stops_the_check() {
    use mochi_core::job::{CancellationToken, NullProgress};
    let (s, _) = forged(Reach::Version);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let o = VerifyOptions {
        deep: true,
        ..VerifyOptions::default()
    };
    let r = verify(&s, &o, &ctx).report;
    assert_eq!(count(&r, ErrorCode::ReferenceInvalid), 0);
    assert_eq!(r.exit_code, 3, "{:?}", r.findings);
}
