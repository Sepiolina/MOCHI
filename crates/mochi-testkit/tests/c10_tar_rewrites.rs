//! C10: GC accounting and rewrites of a TAR-compatible archive (spec Annex
//! B.2.9 D19 rules 7 and 9). `gc plan` reports the streams' framing in its
//! own total and never as collectable; `compact`, `gc apply`, and `repair
//! apply` write the new archive in the source's profile, regenerating the
//! framing through the writer, and the result verifies.

use mochi_core::catalog::extent::ExtentSource;
use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::gc::plan as gc_plan;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, ArchiveWriter, CheckpointPolicy, ReadOptions,
    TailPolicy, Transaction, WriterOptions,
};
use mochi_core::repair::{apply, plan as repair_plan, RepairOptions};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_testkit::archive::{path, read_state, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

use mochi_core::descriptor::Profile;

const OUT: &str = "out.mochi";

const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
};

fn opts() -> ReadOptions {
    ReadOptions::default()
}

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(
        path(p),
        deterministic_bytes(seed, 150),
        attrs(0o644, seed as i64),
    );
}

/// Six commits: a directory, replacements, a rename, a delete, and
/// retention that expires snapshots 0 to 2.
fn source(profile: Option<Profile>) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        WriterOptions {
            profile,
            ..test_options()
        },
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let mut c = |f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1);
        put(t, "b", 2);
    }); // 0
    c(&|t| put(t, "d/a", 3)); // 1
    c(&|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    c(&|t| {
        t.delete(path("d/b"));
    }); // 3
    c(&|t| {
        t.expire(0).expire(1).expire(2);
    }); // 4
    c(&|t| put(t, "e", 5)); // 5
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

fn assert_verifies(s: &SimStorage) {
    for deep in [false, true] {
        let v = verify(
            s,
            &VerifyOptions {
                level: VerificationLevel::Restoration,
                deep,
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        );
        assert_eq!(
            v.report.dimensions[&Dimension::Integrity],
            Status::Pass,
            "{:?}",
            v.report.findings
        );
        assert!(v.report.findings.is_empty(), "{:?}", v.report.findings);
    }
}

/// **Rule 7.** The plan counts stream-only chunks apart from collectable
/// ones, and an archive whose only unreferenced chunks are stream framing
/// has nothing to collect. A Core archive has no such total.
#[test]
fn c10_gc_plan_reports_stream_framing_apart_from_collectable() {
    let s = source(Some(TAR));
    let p = gc_plan(&s, &opts(), &Job::new().ctx()).unwrap();
    assert!(p.stream_framing.chunks > 0);
    assert!(p.stream_framing.stored_bytes > 0);
    assert_eq!(p.stream_framing.file_versions, 0);
    // Snapshots 0 to 2 expired: their versions' chunks (and nothing of the
    // framing) are collectable. Independent count: chunks that some extent
    // references minus those the retained snapshots reach.
    let head = open_head(&s, &opts()).unwrap();
    let mut referenced = std::collections::BTreeSet::new();
    for id in head.catalog.file_version_ids().unwrap() {
        let (_, ex) = head.catalog.file_version(&id).unwrap().unwrap();
        for e in ex {
            if let ExtentSource::Chunk { chunk, .. } = e.source {
                referenced.insert(chunk.to_hex());
            }
        }
    }
    let collectable: std::collections::BTreeSet<_> = p.collectable_chunks.iter().cloned().collect();
    assert!(!collectable.is_empty());
    assert!(
        collectable.is_subset(&referenced),
        "framing is never collectable"
    );
    let all = head.catalog.object_ids().unwrap().len() as u64;
    assert_eq!(
        p.retained.chunks + p.collectable.chunks + p.stream_framing.chunks,
        all
    );
    assert_eq!(p.stream_framing.chunks, all - referenced.len() as u64);

    // Nothing to collect but framing: an archive that never expired anything.
    let t = SimStorage::new();
    let mut w = ArchiveWriter::create(
        t.clone(),
        Box::new(SeqIds::new(1)),
        WriterOptions {
            profile: Some(TAR),
            ..test_options()
        },
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "x", 1);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let p = gc_plan(&t, &opts(), &Job::new().ctx()).unwrap();
    assert!(p.stream_framing.chunks > 0);
    assert!(!p.collects_anything());

    let core = source(None);
    let p = gc_plan(&core, &opts(), &Job::new().ctx()).unwrap();
    assert_eq!(p.stream_framing.chunks, 0);
    assert_eq!(p.stream_framing.stored_bytes, 0);
}

/// **Rule 9, compact.** Every snapshot is reproduced in a new archive that
/// is TAR-compatible and verifies, framing regenerated; the source is
/// untouched.
#[test]
fn c10_compact_keeps_the_profile_and_regenerates_the_streams() {
    let src = source(Some(TAR));
    let before = src.contents();
    let w = lock(&src, 2);
    let mut dir = SimDir::new();
    compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    assert_eq!(src.contents(), before);
    let out = dir.file(OUT).unwrap();
    let head = open_head(&out, &opts()).unwrap();
    assert!(head.descriptor.tar_compatible);
    assert_verifies(&out);
    // Each source snapshot has its twin.
    for (i, h) in commit_history(&src, &opts()).unwrap().iter().enumerate() {
        let a = open_at_footer(&src, h.footer_offset, &opts()).unwrap();
        let b = open_at_footer(
            &out,
            commit_history(&out, &opts()).unwrap()[i].footer_offset,
            &opts(),
        )
        .unwrap();
        assert_eq!(read_state(&out, &b).unwrap(), read_state(&src, &a).unwrap());
    }
}

/// **Rule 9, gc apply.** Collecting expired snapshots rewrites in the same
/// profile; the new archive's stream no longer carries what was collected
/// and verifies.
#[test]
fn c10_gc_apply_keeps_the_profile() {
    let src = source(Some(TAR));
    let p = gc_plan(&src, &opts(), &Job::new().ctx()).unwrap();
    let w = lock(&src, 2);
    let mut dir = SimDir::new();
    compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Roots(p.head.clone()),
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    let out = dir.file(OUT).unwrap();
    assert!(open_head(&out, &opts()).unwrap().descriptor.tar_compatible);
    assert_verifies(&out);
    assert!(
        out.contents().len() < src.contents().len(),
        "collected bytes left"
    );
}

/// **Rule 9, repair apply.** A damaged content chunk is omitted from the
/// repaired archive, which is TAR-compatible and verifies.
#[test]
fn c10_repair_apply_keeps_the_profile() {
    let src = source(Some(TAR));
    let head = open_head(&src, &opts()).unwrap();
    let snap = head.catalog.replay(None).unwrap();
    let entry = snap.get(&path("e")).unwrap();
    let (_, ex) = head.catalog.file_version(&entry.version).unwrap().unwrap();
    let chunk = ex
        .iter()
        .find_map(|e| match e.source {
            ExtentSource::Chunk { chunk, .. } => Some(chunk),
            ExtentSource::Hole => None,
        })
        .unwrap();
    let r = head.catalog.object(&chunk).unwrap().unwrap();
    let at = head.catalog.object_location(&chunk).unwrap().unwrap();
    let mut bytes = src.contents();
    bytes[(at + r.stored_len / 2) as usize] ^= 0x40;
    let damaged = SimStorage::from_bytes(bytes);

    let p = repair_plan(&damaged, &opts(), &Job::new().ctx()).unwrap();
    let mut dir = SimDir::new();
    apply(
        &damaged,
        &p,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &opts(),
        &RepairOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    let out = dir.file(OUT).unwrap();
    assert!(open_head(&out, &opts()).unwrap().descriptor.tar_compatible);
    assert_verifies(&out);
}
