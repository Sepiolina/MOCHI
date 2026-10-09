//! C11: `rekey` (spec Annex B.2.10 D20 item 10). A rewrap is one ordinary commit
//! with key operations as its audit record; re-encryption is a rewrite with a
//! provenance reason; neither is revocation. Nonces are never reused.
//!
//! Oracles: the commit record's envelope list (plaintext), the key state
//! replayed from the manifests, the testkit's reassembly of each snapshot, and
//! the nonces read from the raw bytes of every sealed frame.

use std::collections::BTreeSet;

use mochi_core::commit::Metadata;
use mochi_core::compact::read_provenance;
use mochi_core::manifest::{KeyOp, ManifestKind, RewriteReason};
use mochi_core::publish::{
    commit_history, open_head, read_bound_manifest, segment_state, ArchiveWriter, ReadOptions,
    TailPolicy, Transaction,
};
use mochi_core::rekey::{list, parse_envelope_id, reencrypt, rewrap};
use mochi_core::ErrorCode;
use mochi_format::secret::Passphrase;
use mochi_testkit::archive::{encrypted_options, keyed_read, path, read_state, Job, TEST_KDF};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};
use proptest::prelude::*;

const A: &str = "alpha passphrase";
const B: &str = "bravo passphrase";
const C: &str = "charlie passphrase";

fn pass(p: &str) -> Passphrase {
    Passphrase::new(p).unwrap()
}

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(path(p), deterministic_bytes(seed, 200), attrs(0o644, 1));
}

fn archive(passphrases: &[&str]) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        encrypted_options(passphrases),
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "one", 1);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    s
}

fn lock(s: &SimStorage, with: &[&str], seed: u64) -> ArchiveWriter<SimStorage> {
    ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(seed)),
        encrypted_options(with),
        TailPolicy::Refuse,
    )
    .unwrap()
    .0
}

fn envelope_ids(s: &SimStorage) -> Vec<String> {
    list(s, &ReadOptions::default())
        .unwrap()
        .into_iter()
        .map(|e| e.envelope_id)
        .collect()
}

#[test]
fn list_shows_the_envelopes_without_a_key() {
    let s = archive(&[A, B]);
    let l = list(&s, &ReadOptions::default()).unwrap();
    assert_eq!(l.len(), 2);
    assert!(l[0].envelope_id < l[1].envelope_id, "increasing ID order");
    assert!(l.iter().all(|e| e.created_at_seq == 0 && e.kdf == TEST_KDF));
    // An archive that is not Encrypted has none.
    let core = SimStorage::new();
    let mut w = ArchiveWriter::create(
        core.clone(),
        Box::new(SeqIds::new(1)),
        mochi_testkit::archive::test_options(),
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "x", 1);
    w.commit(tx, &Job::new().ctx()).unwrap();
    assert_eq!(
        list(&core, &ReadOptions::default()).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn adding_a_passphrase_is_one_ordinary_commit_with_an_audit_record() {
    let s = archive(&[A]);
    let before_len = s.contents().len();
    let before_ids = envelope_ids(&s);
    let o = keyed_read(&[A]);
    let key_before = open_head(&s, &o).unwrap().commit.key_envelopes.len();
    assert_eq!(key_before, 1);

    let mut w = lock(&s, &[A], 2);
    let out = rewrap(&mut w, vec![pass(C)], vec![], &Job::new().ctx()).unwrap();
    assert_eq!(out.seq, 1);
    w.close().unwrap();

    // Small: an envelope, a delta manifest, a commit; no data rewritten.
    assert!(s.contents().len() - before_len < 4096);
    let ids = envelope_ids(&s);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&before_ids[0]));
    let new_id = ids.iter().find(|i| **i != before_ids[0]).unwrap();

    // Both passphrases open the head; the namespace is unchanged.
    for p in [A, C] {
        let o = keyed_read(&[p]);
        let head = open_head(&s, &o).unwrap();
        assert_eq!(read_state(&s, &head).unwrap().len(), 1, "{p}");
    }
    // The audit record: the delta manifest's key operations, and the replayed
    // key state equals the commit's list.
    let o = keyed_read(&[A]);
    let head = commit_history(&s, &o).unwrap().pop().unwrap();
    let delta = read_bound_manifest(
        &s,
        &head.commit,
        &head.commit.delta_manifest,
        head.commit_offset,
        ManifestKind::Delta,
        &o,
    )
    .unwrap();
    assert_eq!(
        delta.keys.ops,
        vec![KeyOp::Add(parse_envelope_id(new_id).unwrap())]
    );
    assert!(delta.ops.is_empty(), "no namespace operations");
    assert!(matches!(head.commit.metadata, Metadata::Delta { .. }));
    let opened = open_head(&s, &o).unwrap();
    let state = segment_state(&s, &opened, &o).unwrap();
    assert_eq!(
        state.keys,
        head.commit
            .key_envelopes
            .iter()
            .map(|_| ())
            .zip(ids.iter())
            .map(|(_, i)| parse_envelope_id(i).unwrap())
            .collect::<Vec<_>>()
    );
}

#[test]
fn removing_an_envelope_closes_the_head_to_that_passphrase_but_is_not_revocation() {
    let s = archive(&[A, B]);
    let ids = envelope_ids(&s);
    // Which envelope is A's? The one that opens with A alone.
    let copy_before = SimStorage::from_bytes(s.contents());

    // Remove both-but-B by trial: removing the wrong one is fine too, so find
    // the one A opens by removing each on a scratch copy.
    let mut a_envelope = None;
    for id in &ids {
        let scratch = SimStorage::from_bytes(s.contents());
        let mut w = lock(&scratch, &[B], 5);
        rewrap(
            &mut w,
            vec![],
            vec![parse_envelope_id(id).unwrap()],
            &Job::new().ctx(),
        )
        .unwrap();
        w.close().unwrap();
        if open_head(&scratch, &keyed_read(&[A])).is_err() {
            a_envelope = Some(id.clone());
        }
    }
    let a_envelope = a_envelope.expect("one envelope is A's");

    let mut w = lock(&s, &[B], 6);
    rewrap(
        &mut w,
        vec![],
        vec![parse_envelope_id(&a_envelope).unwrap()],
        &Job::new().ctx(),
    )
    .unwrap();
    w.close().unwrap();

    // The head no longer opens with A, and does with B.
    assert_eq!(
        open_head(&s, &keyed_read(&[A])).unwrap_err().code,
        ErrorCode::KeyUnavailable
    );
    open_head(&s, &keyed_read(&[B])).unwrap();
    assert_eq!(envelope_ids(&s).len(), 1);
    assert!(!envelope_ids(&s).contains(&a_envelope));
    // The head's envelope set decides, even for a session that already holds
    // the data key: A opened the earlier copy (and cached the key), and still
    // does not open the head that no longer lists A's envelope.
    let session = keyed_read(&[A]);
    open_head(&copy_before, &session).unwrap();
    assert_eq!(
        open_head(&s, &session).unwrap_err().code,
        ErrorCode::KeyUnavailable,
        "a removed passphrase does not open the head through a cached key"
    );
    // Removal is not revocation: the file's history still holds the envelope
    // frame, and the copy made before still opens with A.
    open_head(&copy_before, &keyed_read(&[A])).unwrap();
    let raw = s.contents();
    assert!(raw.len() > copy_before.contents().len());
    assert_eq!(
        &raw[..copy_before.contents().len()],
        &copy_before.contents()[..]
    );
}

#[test]
fn a_rewrap_that_would_leave_nothing_or_names_nothing_writes_nothing() {
    let s = archive(&[A]);
    let only = envelope_ids(&s).remove(0);
    let before = s.contents();
    for (add, remove) in [
        (vec![], vec![parse_envelope_id(&only).unwrap()]), // empties the set
        (vec![], vec![[9u8; 16]]),                         // not valid at the head
        (vec![], vec![]),                                  // changes nothing
    ] {
        let mut w = lock(&s, &[A], 7);
        let e = rewrap(&mut w, add, remove, &Job::new().ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{e}");
        drop(w);
        assert_eq!(s.contents(), before, "nothing was written");
    }
    // Adding and removing in one commit is fine, and ends with the new one.
    let mut w = lock(&s, &[A], 8);
    rewrap(
        &mut w,
        vec![pass(B)],
        vec![parse_envelope_id(&only).unwrap()],
        &Job::new().ctx(),
    )
    .unwrap();
    w.close().unwrap();
    open_head(&s, &keyed_read(&[B])).unwrap();
    assert_eq!(
        open_head(&s, &keyed_read(&[A])).unwrap_err().code,
        ErrorCode::KeyUnavailable
    );
}

#[test]
fn reencryption_writes_a_new_archive_with_a_new_key_and_its_own_audit_record() {
    let src = archive(&[A]);
    {
        let mut w = lock(&src, &[A], 2);
        let mut tx = Transaction::new();
        put(&mut tx, "two", 2);
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
    }
    let before = src.contents();
    let w = lock(&src, &[A], 3);
    let mut dir = SimDir::new();
    let report = reencrypt(
        &w,
        &mut dir,
        "new.mochi",
        Box::new(SeqIds::new(4)),
        keyed_read(&[A]),
        vec![pass(B)],
        Some(TEST_KDF),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(w);
    assert_eq!(src.contents(), before, "never in place");
    assert_ne!(report.new_archive_id, report.source_archive_id);
    let out = dir.file("new.mochi").unwrap();

    // The new passphrase opens it and the old one does not.
    let o = keyed_read(&[B]);
    let head = open_head(&out, &o).unwrap();
    assert_eq!(read_state(&out, &head).unwrap().len(), 2);
    assert_eq!(
        open_head(&out, &keyed_read(&[A])).unwrap_err().code,
        ErrorCode::KeyUnavailable
    );
    // Every snapshot kept, and the audit record says why.
    assert_eq!(commit_history(&out, &o).unwrap().len(), 2);
    let p = read_provenance(&out, &o).unwrap().unwrap();
    assert_eq!(p.reason, RewriteReason::Reencryption);
    assert_eq!(p.commits.len(), 2);
    // A rewrap leaves no such record; compaction records the other reason.
    assert!(read_provenance(&src, &keyed_read(&[A])).unwrap().is_none());
}

// ---- nonces --------------------------------------------------------------------

/// Every sealed frame's nonce in the file, found through the commit records
/// (data regions, manifests, images) and the envelope references. A frame is
/// counted once however many commits reference it.
fn nonces(s: &SimStorage) -> Vec<[u8; 24]> {
    let raw = s.contents();
    let o = ReadOptions::default();
    let mut seen_offsets = BTreeSet::new();
    let mut out = Vec::new();
    let mut take = |offset: u64| {
        if seen_offsets.insert(offset) {
            // frame: 8 bytes of skippable header, then the sealed header whose
            // nonce is bytes 24..48.
            let at = offset as usize + 8 + 24;
            out.push(raw[at..at + 24].try_into().unwrap());
        }
    };
    for h in commit_history(s, &o).unwrap() {
        let c = h.commit;
        take(c.delta_manifest.offset);
        if let Metadata::Checkpoint { image, snapshot } = c.metadata {
            take(image.offset);
            take(snapshot.offset);
        }
        if let Some(r) = c.data_region {
            let mut at = r.offset as usize;
            let end = (r.offset + r.stored_len) as usize;
            while at < end {
                take(at as u64);
                let len = u32::from_le_bytes(raw[at + 4..at + 8].try_into().unwrap()) as usize;
                at += 8 + len;
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
enum Step {
    Put(u64),
    /// The same plaintext again under another name: with dedup off, the
    /// ciphertext must differ.
    PutAgain,
    Reopen,
    AddPassphrase,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        prop_oneof![
            (0u64..4).prop_map(Step::Put),
            Just(Step::PutAgain),
            Just(Step::Reopen),
            Just(Step::AddPassphrase),
        ],
        1..8,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Over writes, repeated plaintext, reopen, rewraps, and re-encryption,
    /// every 24-byte nonce in the file, and in the re-encrypted copy, is
    /// distinct (D20 item 2: fresh random nonces, no counters).
    #[test]
    fn nonces_are_never_reused(plan in steps()) {
        let s = archive(&[A]);
        let mut w = lock(&s, &[A], 2);
        let mut n = 0u64;
        let mut added = 0u32;
        let mut seed = 20u64;
        for step in plan {
            n += 1;
            match step {
                Step::Put(k) => {
                    let mut tx = Transaction::new();
                    put(&mut tx, &format!("f{n}"), k);
                    w.commit(tx, &Job::new().ctx()).unwrap();
                }
                Step::PutAgain => {
                    let mut tx = Transaction::new();
                    put(&mut tx, &format!("g{n}"), 99);
                    put(&mut tx, &format!("h{n}"), 99);
                    w.commit(tx, &Job::new().ctx()).unwrap();
                }
                Step::Reopen => {
                    w.close().unwrap();
                    seed += 1;
                    w = lock(&s, &[A], seed);
                }
                Step::AddPassphrase => {
                    added += 1;
                    rewrap(&mut w, vec![pass(&format!("extra {added}"))], vec![], &Job::new().ctx())
                        .unwrap();
                }
            }
        }
        let mut dir = SimDir::new();
        reencrypt(
            &w,
            &mut dir,
            "new.mochi",
            Box::new(SeqIds::new(500)),
            keyed_read(&[A]),
            vec![pass(B)],
            Some(TEST_KDF),
            &Job::new().ctx(),
        )
        .unwrap();
        drop(w);
        let out = dir.file("new.mochi").unwrap();
        let (old, new) = (nonces(&s), nonces(&out));
        let all: BTreeSet<[u8; 24]> = old.iter().chain(new.iter()).copied().collect();
        prop_assert_eq!(all.len(), old.len() + new.len());
    }
}
