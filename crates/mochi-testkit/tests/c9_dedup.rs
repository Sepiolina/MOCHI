//! C9: write-path deduplication (spec §9.4, §9.5; `publish::Dedup`).
//!
//! The expectations come from the stored archive, opened by a fresh reader:
//! which chunk each extent names, and what reading and baseline recovery
//! return. The writer's own counters are checked against that, never used
//! as the oracle.

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::catalog::Catalog;
use mochi_core::object::ObjectId;
use mochi_core::publish::{
    open_head, recover_baseline_at_footer, recover_with_trusted_head, ArchiveWriter,
    CheckpointPolicy, CommitOutcome, Dedup, ReadOptions, TailPolicy, Transaction, WriterOptions,
};
use mochi_core::read::read_file_in;
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

/// 200 bytes: four chunks at the test chunk size of 64 (64, 64, 64, 8).
fn content(seed: u64) -> Vec<u8> {
    deterministic_bytes(seed, 200)
}

fn put(tx: &mut Transaction, p: &str, bytes: &[u8]) {
    tx.put_file(path(p), bytes.to_vec(), attrs(0o644, 0));
}

fn commit(w: &mut ArchiveWriter<SimStorage>, files: &[(&str, &[u8])]) -> CommitOutcome {
    let mut tx = Transaction::new();
    for (p, b) in files {
        put(&mut tx, p, b);
    }
    w.commit(tx, &Job::new().ctx()).unwrap()
}

fn create(s: &SimStorage, seed: u64, dedup: Dedup) -> ArchiveWriter<SimStorage> {
    let o = WriterOptions {
        dedup,
        ..test_options()
    };
    ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(seed)), o).unwrap()
}

fn reopen(s: &SimStorage, seed: u64) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0
}

/// The chunk each extent of `p` names, from `cat`'s head.
fn chunks_of(cat: &Catalog, p: &str) -> Vec<ObjectId> {
    let snap = cat.replay(None).unwrap();
    let entry = snap.get(&path(p)).unwrap();
    let (_, extents) = cat.file_version(&entry.version).unwrap().unwrap();
    extents
        .iter()
        .map(|e| match e.source {
            ExtentSource::Chunk { chunk, .. } => chunk,
            ExtentSource::Hole => panic!("no holes here"),
        })
        .collect()
}

fn read(s: &SimStorage, cat: &Catalog, p: &str) -> mochi_core::Result<Vec<u8>> {
    let mut out = Vec::new();
    read_file_in(s, cat, &path(p), &mut out, &opts(), &Job::new().ctx())?;
    Ok(out)
}

/// **C9 §9.4.** The same bytes in a later commit are referenced, not stored
/// again: in the same writer session (the index extended after publishing)
/// and in a later session (the index rebuilt from the head catalog). Every
/// copy reads back exactly.
#[test]
fn c9_identical_content_in_a_later_commit_is_referenced() {
    let s = SimStorage::new();
    let x = content(1);
    let mut w = create(&s, 10, Dedup::InArchive);
    let first = commit(&mut w, &[("a", &x)]);
    assert_eq!(first.dedup.chunks_reused, 0, "nothing to reuse at creation");
    let len_after_a = s.contents().len();
    let second = commit(&mut w, &[("b", &x)]);
    assert_eq!(
        (second.dedup.chunks_reused, second.dedup.bytes_reused),
        (4, 200)
    );
    w.close().unwrap();
    let mut w = reopen(&s, 20);
    let third = commit(&mut w, &[("c", &x)]);
    assert_eq!(
        third.dedup.chunks_reused, 4,
        "index rebuilt after reopening"
    );
    w.close().unwrap();

    let head = open_head(&s, &opts()).unwrap();
    let a = chunks_of(&head.catalog, "a");
    assert_eq!(a.len(), 4);
    assert_eq!(chunks_of(&head.catalog, "b"), a);
    assert_eq!(chunks_of(&head.catalog, "c"), a);
    for p in ["a", "b", "c"] {
        assert_eq!(read(&s, &head.catalog, p).unwrap(), x, "{p}");
    }
    // Commit 1 stored no chunk: it grew by metadata only, which is smaller
    // than the 200 bytes of incompressible content commit 0 stored.
    let growth = s.contents().len() - len_after_a;
    assert!(second.objects_written < first.objects_written);
    assert!(growth > 0);
}

/// **§9.5 rule 2.** Identical chunks within one transaction are each
/// stored: across files and within one file. Lookups see only the head.
#[test]
fn c9_identical_chunks_within_one_commit_are_each_stored() {
    let s = SimStorage::new();
    let x = content(2);
    let zeros = vec![0u8; 128];
    let mut w = create(&s, 30, Dedup::InArchive);
    commit(&mut w, &[("seed", b"something else")]);
    let out = commit(&mut w, &[("a", &x), ("b", &x), ("z", &zeros)]);
    assert_eq!(out.dedup.chunks_reused, 0);
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let a = chunks_of(&head.catalog, "a");
    let b = chunks_of(&head.catalog, "b");
    assert!(a.iter().all(|c| !b.contains(c)), "a and b share no chunk");
    let z = chunks_of(&head.catalog, "z");
    assert_ne!(z[0], z[1], "two identical chunks of one file, both stored");
    assert_eq!(read(&s, &head.catalog, "b").unwrap(), x);
}

/// Deduplication is per chunk: a file that differs only in its last chunk
/// references the three it shares.
#[test]
fn c9_partial_overlap_reuses_the_matching_chunks() {
    let s = SimStorage::new();
    let x = content(3);
    let mut y = x.clone();
    *y.last_mut().unwrap() ^= 0xFF;
    let mut w = create(&s, 40, Dedup::InArchive);
    commit(&mut w, &[("x", &x)]);
    let out = commit(&mut w, &[("y", &y)]);
    assert_eq!((out.dedup.chunks_reused, out.dedup.bytes_reused), (3, 192));
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let cx = chunks_of(&head.catalog, "x");
    let cy = chunks_of(&head.catalog, "y");
    assert_eq!(cx[..3], cy[..3]);
    assert_ne!(cx[3], cy[3]);
    assert_eq!(read(&s, &head.catalog, "y").unwrap(), y);
}

/// `Dedup::Off` stores every chunk.
#[test]
fn c9_off_stores_every_chunk() {
    let s = SimStorage::new();
    let x = content(4);
    let mut w = create(&s, 50, Dedup::Off);
    commit(&mut w, &[("a", &x)]);
    let out = commit(&mut w, &[("b", &x)]);
    assert_eq!(out.dedup, Default::default());
    w.close().unwrap();
    let head = open_head(&s, &opts()).unwrap();
    let a = chunks_of(&head.catalog, "a");
    assert!(chunks_of(&head.catalog, "b").iter().all(|c| !a.contains(c)));
}

/// **§9.4 validation.** A candidate is read back and verified before it is
/// referenced: a damaged chunk is never spread to a new file version. The
/// chunk is stored anew, the rejection is counted, and the damaged
/// candidate leaves the index, so the next commit reuses the good copy.
#[test]
fn c9_a_damaged_candidate_is_never_referenced() {
    let s = SimStorage::new();
    let x = content(5);
    let mut w = create(&s, 60, Dedup::InArchive);
    commit(&mut w, &[("a", &x)]);
    w.close().unwrap();
    let a = chunks_of(&open_head(&s, &opts()).unwrap().catalog, "a");
    let at = open_head(&s, &opts())
        .unwrap()
        .catalog
        .object_location(&a[1])
        .unwrap()
        .unwrap();
    let mut bytes = s.contents();
    bytes[at as usize + 20] ^= 0x01;
    let s = SimStorage::from_bytes(bytes);

    let mut w = reopen(&s, 70);
    let out = commit(&mut w, &[("b", &x)]);
    assert_eq!(
        (out.dedup.chunks_reused, out.dedup.candidates_rejected),
        (3, 1)
    );
    let again = commit(&mut w, &[("c", &x)]);
    assert_eq!(
        (again.dedup.chunks_reused, again.dedup.candidates_rejected),
        (4, 0),
        "the damaged candidate left the index; b's good copy replaced it"
    );
    w.close().unwrap();

    let head = open_head(&s, &opts()).unwrap();
    let b = chunks_of(&head.catalog, "b");
    assert_eq!((b[0], b[2], b[3]), (a[0], a[2], a[3]));
    assert_ne!(b[1], a[1]);
    assert_eq!(chunks_of(&head.catalog, "c"), b);
    assert_eq!(read(&s, &head.catalog, "b").unwrap(), x);
    assert_eq!(read(&s, &head.catalog, "c").unwrap(), x);
    let e = read(&s, &head.catalog, "a").unwrap_err();
    assert_eq!(e.code, ErrorCode::StoredIntegrityFailed);
}

/// **D10.8 with deduplication.** A snapshot manifest holds only the chunks
/// its namespace reaches, and baseline recovery replays from it alone. So a
/// chunk stored before the base and unreachable at it must not be
/// referenced by a later delta, while a chunk introduced within the segment
/// may be, even after its file is deleted. Baseline recovery and recovery
/// from manifests then succeed and read every file exactly.
#[test]
fn c9_reuse_stays_within_what_baseline_recovery_sees() {
    let s = SimStorage::new();
    let x = content(6);
    let y = content(7);
    let mut w = create(&s, 80, Dedup::InArchive);
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    commit(&mut w, &[("a", &x)]); // 0: checkpoint
    commit(&mut w, &[("c", &y)]); // 1: delta, builds the index (x)
    let mut tx = Transaction::new();
    tx.delete(path("a"));
    w.request_checkpoint();
    w.commit(tx, &Job::new().ctx()).unwrap(); // 2: checkpoint without x
    let out = commit(&mut w, &[("b", &x)]); // 3: delta
    assert_eq!(
        out.dedup.chunks_reused, 0,
        "x is unreachable at base 2: S(2) does not hold its chunks"
    );
    let mut tx = Transaction::new();
    tx.delete(path("b"));
    w.commit(tx, &Job::new().ctx()).unwrap(); // 4: delta, b gone
    let out = commit(&mut w, &[("d", &x)]); // 5: delta
    assert_eq!(
        out.dedup.chunks_reused, 4,
        "b's chunks were introduced in this segment: replay sees them"
    );
    w.close().unwrap();

    let head = open_head(&s, &opts()).unwrap();
    assert_eq!(head.seq(), 5);
    let footer = head.location.footer.footer_offset;
    let baseline = recover_baseline_at_footer(&s, footer, &opts()).unwrap();
    assert_eq!(baseline.segment.base_seq, 2);
    for (p, want) in [("c", &y), ("d", &x)] {
        assert_eq!(&read(&s, &baseline.catalog, p).unwrap(), want, "{p}");
        assert_eq!(&read(&s, &head.catalog, p).unwrap(), want, "{p}");
    }
    let trusted = recover_with_trusted_head(&s, &opts()).unwrap();
    let cat = trusted.head_catalog().expect("the head recovers");
    assert_eq!(read(&s, cat, "d").unwrap(), x);
}
