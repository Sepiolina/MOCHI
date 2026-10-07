//! C9: compaction and `gc apply` by rewriting into a new archive (spec
//! §18.2, §18.3, Annex B D18; `mochi_core::compact`).
//!
//! The oracle for "reproduces its source" is the test kit's own
//! reassembly (`archive::read_state`) of each source snapshot and each new
//! commit, plus IDs and attributes read back through public APIs. It never
//! uses the compactor's own checks.

use std::collections::BTreeSet;

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::compact::{compact, read_provenance, CompactOptions, Keep};
use mochi_core::gc::plan;
use mochi_core::manifest::Mtime;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, promised_attributes, recover_baseline_at_footer,
    segment_state, ArchiveWriter, CheckpointPolicy, OpenedHead, ReadOptions, TailPolicy,
    Transaction,
};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, read_state, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, CrashMode, DirFault, Fault, SeqIds, SimDir, SimStorage};

const OUT: &str = "out.mochi";

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn put(tx: &mut Transaction, p: &str, seed: u64, mode: u32) {
    tx.put_file(
        path(p),
        deterministic_bytes(seed, 150),
        attrs(mode, seed as i64),
    );
}

/// Nine commits with a directory, replacements, a rename, deletions, a
/// forced checkpoint in the middle, and retention: snapshots 1–4 expired,
/// snapshot 2 also held.
fn source() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut n = 0i64;
    let mut c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        // An informational commit time, which compaction copies.
        n += 1;
        tx.at(Mtime {
            secs: 1_700_000_000 + n,
            nanos: 7,
        });
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1, 0o644);
        put(t, "b", 2, 0o600);
    }); // 0
    c(&mut w, &|t| put(t, "d/a", 3, 0o640)); // 1
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    w.request_checkpoint();
    c(&mut w, &|t| put(t, "c", 4, 0o644)); // 3: checkpoint
    c(&mut w, &|t| {
        t.delete(path("c"));
    }); // 4
    c(&mut w, &|t| put(t, "e", 5, 0o644)); // 5
    c(&mut w, &|t| {
        t.expire(1).expire(2).expire(3).expire(4).hold(b"audit", 2);
    }); // 6
    c(&mut w, &|t| put(t, "d/a", 6, 0o644)); // 7
    c(&mut w, &|t| put(t, "f", 7, 0o644)); // 8
    w.close().unwrap();
    s
}

fn lock(s: &SimStorage, seed: u64) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0
}

fn opened_all(s: &SimStorage) -> Vec<OpenedHead> {
    commit_history(s, &opts())
        .unwrap()
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, &opts()).unwrap())
        .collect()
}

/// Object IDs and version IDs a commit reaches.
fn ids(h: &OpenedHead) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut versions = BTreeSet::new();
    let mut chunks = BTreeSet::new();
    for (_, e) in h.catalog.replay(None).unwrap().iter() {
        versions.insert(format!("{:?}", e.version));
        let (_, extents) = h.catalog.file_version(&e.version).unwrap().unwrap();
        for x in extents {
            if let ExtentSource::Chunk { chunk, .. } = x.source {
                chunks.insert(chunk.to_hex());
            }
        }
    }
    (versions, chunks)
}

/// Each new commit reproduces its source snapshot: content, IDs, and
/// promised attributes.
fn assert_reproduces(src: &SimStorage, out: &SimStorage, kept: &[u64]) {
    let source = opened_all(src);
    let new = opened_all(out);
    assert_eq!(new.len(), kept.len());
    for (i, r) in kept.iter().enumerate() {
        let (a, b) = (&source[*r as usize], &new[i]);
        assert_eq!(
            read_state(out, b).unwrap(),
            read_state(src, a).unwrap(),
            "new {i} vs source {r}"
        );
        assert_eq!(ids(b), ids(a), "IDs, new {i} vs source {r}");
        assert_eq!(
            promised_attributes(out, b, &opts()).unwrap(),
            promised_attributes(src, a, &opts()).unwrap(),
            "attributes, new {i} vs source {r}"
        );
        assert_eq!(b.commit.time, a.commit.time);
    }
}

/// **§18.2 compaction.** Every snapshot kept, one commit each, IDs and
/// attributes preserved, retention and provenance carried; a new archive
/// ID; the source untouched; published without replacing anything.
#[test]
fn c9_compact_reproduces_every_snapshot_in_a_new_archive() {
    let src = source();
    let before = src.contents();
    let source_writer = lock(&src, 2);
    let mut dir = SimDir::new();
    let r = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(source_writer);
    assert_eq!(src.contents(), before, "the source is never written");
    assert!(r.source_kept);
    assert_eq!(dir.names(), [OUT]);
    let out = dir.file(OUT).unwrap();
    let kept: Vec<u64> = (0..=8).collect();
    assert_reproduces(&src, &out, &kept);
    assert_ne!(r.new_archive_id, r.source_archive_id);
    assert_eq!(r.commits.len(), 9);
    assert!(r.collected.is_empty());
    assert!(r.versions_verified.unwrap() > 0);

    let head = open_head(&out, &opts()).unwrap();
    let retention = segment_state(&out, &head, &opts()).unwrap().retention;
    let source_retention = {
        let h = open_head(&src, &opts()).unwrap();
        segment_state(&src, &h, &opts()).unwrap().retention
    };
    assert_eq!(
        retention, source_retention,
        "identity mapping when all are kept"
    );
    let p = read_provenance(&out, &opts()).unwrap().unwrap();
    assert_eq!(p.commits.len(), 9);
    assert_eq!(
        p.commits.last().unwrap().1,
        open_head(&src, &opts()).unwrap().commit_id
    );
    assert!(read_provenance(&src, &opts()).unwrap().is_none());

    // Never replaces: the same destination again is refused.
    let source_writer = lock(&src, 4);
    let e = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(5)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(dir.file(OUT).unwrap().contents(), out.contents());
}

/// **§18.3 collection (`gc apply`).** The kept snapshots are the plan's
/// roots: 0, 2 (expired but held), and 5–8. The new archive holds exactly
/// the chunks the plan retains, none it collects; the hold and snapshot 2's
/// expiry carry over to new commit 1; provenance names what was collected.
/// Releasing the hold in the new archive makes that snapshot collectable
/// there too.
#[test]
fn c9_gc_apply_writes_exactly_the_retained_roots() {
    let src = source();
    let p = plan(&src, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.roots, [0, 2, 5, 6, 7, 8]);
    let source_writer = lock(&src, 2);
    let mut dir = SimDir::new();
    let r = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Roots(p.head.clone()),
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(source_writer);
    let out = dir.file(OUT).unwrap();
    assert_reproduces(&src, &out, &p.roots);
    assert_eq!(r.collected, [1, 3, 4]);

    let held: BTreeSet<String> = open_head(&out, &opts())
        .unwrap()
        .catalog
        .object_ids()
        .unwrap()
        .iter()
        .map(|i| i.to_hex())
        .collect();
    let collected: BTreeSet<String> = p.collectable_chunks.iter().cloned().collect();
    assert!(!collected.is_empty());
    assert!(held.is_disjoint(&collected), "no collected chunk survives");
    assert_eq!(held.len() as u64, p.retained.chunks);
    assert!(out.contents().len() < src.contents().len());

    let head = open_head(&out, &opts()).unwrap();
    let ret = segment_state(&out, &head, &opts()).unwrap().retention;
    assert_eq!(ret.expired, BTreeSet::from([1]));
    assert_eq!(ret.holds.get(b"audit".as_slice()), Some(&1));
    let prov = read_provenance(&out, &opts()).unwrap().unwrap();
    assert_eq!(prov.collected, [1, 3, 4]);
    let kept: Vec<u64> = prov.commits.iter().map(|(s, _)| *s).collect();
    assert_eq!(kept, p.roots);

    // Release in the new archive: new snapshot 1 (source 2) is collectable.
    let mut w = lock(&out, 6);
    let mut tx = Transaction::new();
    tx.release(b"audit");
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let p2 = plan(&out, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(
        p2.collectable_snapshots
            .iter()
            .map(|c| c.seq)
            .collect::<Vec<_>>(),
        [1]
    );
}

/// **Fault-matrix row "GC overlaps publication".** While compaction runs,
/// the source's writer lock is held, so no other writer can publish. A plan
/// made before another commit landed is stale and refused, and nothing is
/// created.
#[test]
fn c9_gc_never_overlaps_publication() {
    let src = source();
    let p = plan(&src, &opts(), &Job::new().ctx()).unwrap();
    let source_writer = lock(&src, 2);
    let Err(e) = ArchiveWriter::open_append(
        src.handle(), // a second open of the same file
        Box::new(SeqIds::new(9)),
        test_options(),
        TailPolicy::Refuse,
    ) else {
        panic!("the lock is held");
    };
    assert_eq!(e.code, ErrorCode::LockConflict);
    drop(source_writer);

    // A commit lands after the plan.
    let mut w = lock(&src, 3);
    let mut tx = Transaction::new();
    put(&mut tx, "late", 8, 0o644);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();

    let source_writer = lock(&src, 4);
    let mut dir = SimDir::new();
    let e = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(5)),
        &Keep::Roots(p.head.clone()),
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
    assert!(e.message.contains("plan again"), "{e}");
    assert!(dir.names().is_empty());
}

/// **Fault-matrix row "compaction interrupted".** A halt before each
/// mutating step of the new archive's file, and before each directory
/// operation, in turn: the source stays byte-identical and opens, and the
/// destination name holds nothing, after the process dies and after a
/// power loss. Only the run that completes publishes.
#[test]
fn c9_interrupted_compaction_leaves_the_source_and_no_archive() {
    let src = source();
    let before = src.contents();
    let mut file_halts = 0;
    for k in 0.. {
        let source_writer = lock(&src, 2);
        let mut dir = SimDir::new();
        dir.add_next_file_fault(Fault::HaltBeforeMutation { mutation_index: k });
        let r = compact(
            &source_writer,
            &mut dir,
            OUT,
            Box::new(SeqIds::new(3)),
            &Keep::Every,
            &CompactOptions::default(),
            &Job::new().ctx(),
        );
        drop(source_writer);
        assert_eq!(src.contents(), before, "halt {k}");
        open_head(&src, &opts()).unwrap();
        match r {
            Err(_) => {
                file_halts += 1;
                assert!(dir.file(OUT).is_none(), "halt {k}: nothing published");
                for mode in [CrashMode::KeepAll, CrashMode::SyncedOnly] {
                    assert!(dir.crash_image(mode).file(OUT).is_none(), "halt {k}");
                }
            }
            Ok(_) => {
                assert!(dir.file(OUT).is_some());
                break;
            }
        }
    }
    assert!(
        file_halts > 20,
        "every append and sync was interrupted once"
    );

    for index in 0..3 {
        let source_writer = lock(&src, 2);
        let mut dir = SimDir::with_faults([DirFault::HaltBeforeOp { index }]);
        let r = compact(
            &source_writer,
            &mut dir,
            OUT,
            Box::new(SeqIds::new(3)),
            &Keep::Every,
            &CompactOptions::default(),
            &Job::new().ctx(),
        );
        drop(source_writer);
        assert_eq!(src.contents(), before);
        if r.is_err() {
            let after = dir.crash_image(CrashMode::SyncedOnly);
            if let Some(f) = after.file(OUT) {
                // Halted after publication, before the directory flush
                // returned: what is there is the complete new archive.
                assert_eq!(open_head(&f, &opts()).unwrap().seq(), 8);
            }
        }
    }
}

/// **Checklist Q64 in compaction.** A source delta references a chunk
/// stored in its segment before its file was deleted (dedup within the
/// segment). If the new archive checkpoints where that chunk is unreachable,
/// the next commit's reference would be invisible to baseline recovery, so
/// the compactor forces a checkpoint there. Baseline recovery then works at
/// every new commit.
#[test]
fn c9_compaction_keeps_every_commit_baseline_recoverable() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let x = deterministic_bytes(77, 200);
    let mut tx = Transaction::new();
    tx.put_dir(path("d"), attrs(0o755, 0));
    w.commit(tx, &job.ctx()).unwrap(); // 0
    let mut tx = Transaction::new();
    tx.put_file(path("a"), x.clone(), attrs(0o644, 0));
    w.commit(tx, &job.ctx()).unwrap(); // 1: introduces x
    let mut tx = Transaction::new();
    tx.delete(path("a"));
    w.commit(tx, &job.ctx()).unwrap(); // 2
    let mut tx = Transaction::new();
    tx.put_file(path("b"), x.clone(), attrs(0o644, 0));
    let o = w.commit(tx, &job.ctx()).unwrap(); // 3: reuses x
    assert_eq!(o.dedup.chunks_reused, 4);
    w.close().unwrap();

    let source_writer = lock(&s, 2);
    let mut dir = SimDir::new();
    compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions {
            // Checkpoints at 0 and 2: 2 is where x is unreachable.
            checkpoint_policy: Some(CheckpointPolicy::Every(2)),
            ..CompactOptions::default()
        },
        &job.ctx(),
    )
    .unwrap();
    drop(source_writer);
    let out = dir.file(OUT).unwrap();
    let history = commit_history(&out, &opts()).unwrap();
    assert!(
        history[3].commit.metadata.is_checkpoint(),
        "forced: x is not visible from base 2"
    );
    for h in &history {
        recover_baseline_at_footer(&out, h.footer_offset, &opts()).unwrap();
    }
    assert_reproduces(&s, &out, &[0, 1, 2, 3]);
}

/// **§18.2 step 3.** A chunk silently corrupted on its way into the new
/// archive (the write reports success) is found by reading the copy back,
/// and nothing is published. Without content verification the corruption
/// would be published, which is why it is on by default.
#[test]
fn c9_silent_corruption_in_the_copy_is_caught_before_publication() {
    let src = source();
    // Append 0 is the new descriptor; append 1 is the first copied chunk.
    let fault = Fault::CorruptAppend {
        append_index: 1,
        at: 20,
        xor: 0x01,
    };
    let source_writer = lock(&src, 2);
    let mut dir = SimDir::new();
    dir.add_next_file_fault(fault.clone());
    let e = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed, "{e}");
    assert!(dir.file(OUT).is_none());

    let mut dir = SimDir::new();
    dir.add_next_file_fault(fault);
    let r = compact(
        &source_writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions {
            verify_content: false,
            ..CompactOptions::default()
        },
        &Job::new().ctx(),
    )
    .unwrap();
    assert_eq!(r.versions_verified, None);
    let out = dir.file(OUT).unwrap();
    let head = open_head(&out, &opts()).unwrap();
    let damaged = head
        .catalog
        .file_version_ids()
        .unwrap()
        .iter()
        .filter(|id| {
            mochi_core::read::read_version_in(
                &out,
                &head.catalog,
                id,
                &mut std::io::sink(),
                &opts(),
                &Job::new().ctx(),
            )
            .is_err_and(|e| e.code == ErrorCode::StoredIntegrityFailed)
        })
        .count();
    assert_eq!(damaged, 1, "published unverified, with the damage in it");
}
