//! C11: rewrites of an Encrypted archive (spec Annex B.2.10 D20 item 10):
//! `compact`, `gc apply`, and `repair apply` re-seal. The new archive has its
//! own ID, data key, and key ID; every sealed object is opened and sealed
//! again; only the passphrases given carry over; and without a passphrase
//! none of them (plans included) starts.
//!
//! Oracles: the testkit's own reassembly of each snapshot through the
//! passphrase, the raw bytes of both files (no sealed frame of the source
//! reappears in the new archive; no plaintext in either), and the key IDs read
//! from the envelopes in the commit records.

use std::collections::BTreeSet;

use mochi_core::commit::CommitRecord;
use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::gc;
use mochi_core::keys::read_envelopes;
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, ArchiveWriter, CheckpointPolicy, OpenedHead,
    ReadOptions, TailPolicy, Transaction,
};
use mochi_core::repair::{self, Outcome, RepairOptions};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{encrypted_options, keyed_read, path, read_state, Job};
use mochi_testkit::replay::{attrs, flip};
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const A: &str = "first passphrase";
const B: &str = "second passphrase";
const OUT: &str = "out.mochi";

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(path(p), deterministic_bytes(seed, 150), attrs(0o644, 1));
}

/// Six commits: replacements, a rename, a deletion, and retention (1 to 3
/// expired). The passphrases `A` and `B` both open it.
fn source() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        encrypted_options(&[A, B]),
    )
    .unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1);
        put(t, "b", 2);
    }); // 0
    c(&mut w, &|t| put(t, "d/a", 3)); // 1
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    c(&mut w, &|t| put(t, "c", 4)); // 3
    c(&mut w, &|t| {
        t.expire(1).expire(2).expire(3);
    }); // 4
    c(&mut w, &|t| put(t, "d/a", 5)); // 5
    w.close().unwrap();
    s
}

fn lock(s: &SimStorage, passphrases: &[&str], seed: u64) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        encrypted_options(passphrases),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0
}

fn compact_opts(passphrases: &[&str]) -> CompactOptions {
    CompactOptions {
        read: keyed_read(passphrases),
        kdf: Some(mochi_testkit::archive::TEST_KDF),
        ..CompactOptions::default()
    }
}

fn history(s: &SimStorage, o: &ReadOptions) -> Vec<OpenedHead> {
    commit_history(s, o)
        .unwrap()
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, o).unwrap())
        .collect()
}

fn head_record(s: &SimStorage, o: &ReadOptions) -> CommitRecord {
    commit_history(s, o).unwrap().pop().unwrap().commit
}

/// The key ID and envelope count of the head commit (from the plaintext
/// envelopes).
fn key_of(s: &SimStorage) -> ([u8; 16], usize) {
    let o = ReadOptions::default();
    let h = commit_history(s, &o).unwrap().pop().unwrap();
    let envs = read_envelopes(s, &h.commit, h.commit_offset, &o).unwrap();
    (*envs[0].1.key_id.as_bytes(), envs.len())
}

/// Every sealed frame (magic `0x184D2A59`) of the file, by its bytes: the
/// frames are found through the commit records' data regions and manifests,
/// so the walk is independent of the catalog.
fn sealed_frames(s: &SimStorage) -> BTreeSet<Vec<u8>> {
    let raw = s.contents();
    let o = ReadOptions::default();
    let mut out = BTreeSet::new();
    for h in commit_history(s, &o).unwrap() {
        if let Some(r) = h.commit.data_region {
            let bytes = &raw[r.offset as usize..(r.offset + r.stored_len) as usize];
            let mut at = 0;
            while at < bytes.len() {
                let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
                out.insert(bytes[at..at + 8 + len].to_vec());
                at += 8 + len;
            }
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn assert_reproduces(src: &SimStorage, out: &SimStorage, kept: &[u64], pass: &str) {
    let o = keyed_read(&[pass]);
    let a = history(src, &o);
    let b = history(out, &o);
    assert_eq!(b.len(), kept.len());
    for (i, r) in kept.iter().enumerate() {
        assert_eq!(
            read_state(out, &b[i]).unwrap(),
            read_state(src, &a[*r as usize]).unwrap(),
            "new {i} vs source {r}"
        );
    }
}

#[test]
fn compact_re_seals_into_a_new_archive_with_a_new_key_and_only_the_passphrases_given() {
    let src = source();
    let before = src.contents();
    let (src_key, src_envs) = key_of(&src);
    assert_eq!(src_envs, 2);

    let w = lock(&src, &[A], 2);
    let mut dir = SimDir::new();
    let r = compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &compact_opts(&[A]),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    assert_eq!(src.contents(), before, "the source is never written");
    assert_ne!(r.new_archive_id, r.source_archive_id);
    let out = dir.file(OUT).unwrap();

    // A new data key, and one envelope for the one passphrase given.
    let (new_key, new_envs) = key_of(&out);
    assert_ne!(new_key, src_key);
    assert_eq!(new_envs, 1);

    // Every sealed frame is fresh: no frame, so no nonce, repeats the
    // source's, though the plaintext chunks are the same.
    let (old, new) = (sealed_frames(&src), sealed_frames(&out));
    assert!(!new.is_empty());
    assert!(old.is_disjoint(&new));
    let raw = out.contents();
    assert!(!contains(&raw, b"d/a") && !contains(&raw, &[0x28, 0xB5, 0x2F, 0xFD]));

    // The content is reproduced; the passphrase that was not given again does
    // not open the new archive, and the one that was given does.
    assert_reproduces(&src, &out, &(0..=5).collect::<Vec<_>>(), A);
    assert_eq!(
        open_head(&out, &keyed_read(&[B])).unwrap_err().code,
        ErrorCode::KeyUnavailable
    );
    // The source still opens with either.
    open_head(&src, &keyed_read(&[B])).unwrap();
}

#[test]
fn a_rewrite_without_a_passphrase_does_not_start() {
    let src = source();
    // A writer cannot even be locked without one.
    let e = ArchiveWriter::open_append(
        src.clone(),
        Box::new(SeqIds::new(2)),
        mochi_testkit::archive::test_options(),
        TailPolicy::Refuse,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::KeyUnavailable);

    // With a writer but compact options that carry no passphrase.
    let w = lock(&src, &[A], 2);
    let mut dir = SimDir::new();
    let e = compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::KeyUnavailable);
    assert!(dir.file(OUT).is_none(), "nothing was published");

    // Plans need the key too: retention is sealed.
    let e = gc::plan(&src, &ReadOptions::default(), &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::KeyUnavailable);
    let e = repair::plan(&src, &ReadOptions::default(), &Job::new().ctx());
    // A repair plan reports what it could not open rather than failing; it
    // must not claim anything is recoverable without the key.
    if let Ok(p) = e {
        assert_ne!(p.outcome, Outcome::Complete);
    }
}

#[test]
fn gc_apply_keeps_exactly_the_retained_roots_re_sealed() {
    let src = source();
    let o = keyed_read(&[A]);
    let p = gc::plan(&src, &o, &Job::new().ctx()).unwrap();
    assert_eq!(p.roots, [0, 4, 5]);
    let w = lock(&src, &[A], 2);
    let mut dir = SimDir::new();
    compact(
        &w,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(3)),
        &Keep::Roots(p.head.clone()),
        &compact_opts(&[A]),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    let out = dir.file(OUT).unwrap();
    assert_reproduces(&src, &out, &p.roots, A);
    assert!(sealed_frames(&src).is_disjoint(&sealed_frames(&out)));
}

#[test]
fn repair_re_seals_what_it_recovers_and_leaves_out_a_damaged_chunk() {
    let pristine = source();
    // Damage a chunk of the replaced `d/a` of commit 0.
    let o = keyed_read(&[A]);
    let h = history(&pristine, &o);
    let snap = h[0].catalog.replay(None).unwrap();
    let entry = snap.get(&path("b")).unwrap();
    let (_, extents) = h[0].catalog.file_version(&entry.version).unwrap().unwrap();
    let chunk = extents
        .iter()
        .find_map(|e| match e.source {
            mochi_core::catalog::extent::ExtentSource::Chunk { chunk, .. } => Some(chunk),
            _ => None,
        })
        .unwrap();
    let rec = h[0].catalog.object(&chunk).unwrap().unwrap();
    let at = h[0].catalog.object_location(&chunk).unwrap().unwrap();
    let mut bytes = pristine.contents();
    flip(&mut bytes, at + rec.stored_len / 2);
    let src = SimStorage::from_bytes(bytes);

    let plan = repair::plan(&src, &o, &Job::new().ctx()).unwrap();
    assert!(plan.damage_found);
    let mut dir = SimDir::new();
    let report = repair::apply(
        &src,
        &plan,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &o,
        &RepairOptions {
            kdf: Some(mochi_testkit::archive::TEST_KDF),
            ..RepairOptions::default()
        },
        &Job::new().ctx(),
    )
    .unwrap();
    assert_ne!(
        report.new_archive_id,
        report.source_archive_id.clone().unwrap()
    );
    let out = dir.file(OUT).unwrap();
    assert_ne!(key_of(&out).0, key_of(&src).0);
    // The repaired archive opens with the passphrase and verifies.
    let head = open_head(&out, &o).unwrap();
    assert_eq!(head.commit.seq, head_record(&src, &o).seq);
    assert!(sealed_frames(&src).is_disjoint(&sealed_frames(&out)));
    assert_eq!(report.reverification.exit_code, mochi_core::exit::OK);
}
