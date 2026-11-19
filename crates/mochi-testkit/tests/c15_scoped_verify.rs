//! Scoped verification (`verify_commits_since`; spec §20.1, §20.3, §20.4,
//! desktop feedback "verifying a committed archive took very long").
//!
//! A scoped check covers the commits after one already-verified head and
//! says so: its whole-archive status is always `UNKNOWN`. Fault-injection
//! rows: a corrupted byte in an object the new commit wrote is a `FAIL`; a
//! corrupted byte in an old object is neither read nor claimed to be `PASS`
//! (the full `verify` still finds it); a history that does not contain the
//! previous head is a `FAIL`.

use std::cell::RefCell;

use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::manifest::ManifestKind;
use mochi_core::publish::{
    commit_history, open_head, read_bound_manifest, ArchiveWriter, ReadOptions, Transaction,
};
use mochi_core::status::Status;
use mochi_core::storage::{ReadStorage, StorageError};
use mochi_core::verify::{
    verify, verify_commits_since, ScopedVerification, VerifiedHead, VerifyOptions,
};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

/// Three commits (0 a checkpoint, 1 and 2 deltas), each adding one file of
/// several incompressible chunks.
fn archive() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(3)), test_options()).unwrap();
    let job = Job::new();
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let mut tx = Transaction::new();
        tx.put_file(
            path(name),
            deterministic_bytes(40 + i as u64, 400),
            Default::default(),
        );
        w.commit(tx, &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn history(s: &SimStorage) -> Vec<mochi_core::publish::HistoryEntry> {
    commit_history(s, &ReadOptions::default()).unwrap()
}

fn verified(s: &SimStorage, seq: u64) -> VerifiedHead {
    let h = history(s);
    let e = h.iter().find(|e| e.commit.seq == seq).unwrap();
    VerifiedHead {
        archive_id: e.commit.archive_id,
        seq,
        commit_id: e.commit_id,
    }
}

/// `(offset, stored_len)` of every data object commit `seq` introduces.
fn objects_of(s: &SimStorage, seq: u64) -> Vec<(u64, u64)> {
    let opts = ReadOptions::default();
    let head = open_head(s, &opts).unwrap();
    let h = history(s);
    let e = h.iter().find(|e| e.commit.seq == seq).unwrap();
    let m = read_bound_manifest(
        s,
        &e.commit,
        &e.commit.delta_manifest,
        e.commit_offset,
        ManifestKind::Delta,
        &opts,
    )
    .unwrap();
    m.chunks
        .iter()
        .map(|c| {
            (
                head.catalog.object_location(&c.record.id).unwrap().unwrap(),
                c.record.stored_len,
            )
        })
        .collect()
}

fn scoped(s: &dyn ReadStorage, since: &VerifiedHead) -> ScopedVerification {
    let job = Job::new();
    verify_commits_since(s, &ReadOptions::default(), since, &job.ctx())
}

fn flipped(s: &SimStorage, offset: u64) -> SimStorage {
    let mut b = s.contents();
    b[offset as usize] ^= 0x40;
    SimStorage::from_bytes(b)
}

/// Records every byte range read.
struct Counting<'a> {
    inner: &'a SimStorage,
    reads: RefCell<Vec<(u64, u64)>>,
}

impl ReadStorage for Counting<'_> {
    fn size(&self) -> Result<u64, StorageError> {
        self.inner.size()
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, StorageError> {
        let n = self.inner.read_at(offset, buf)?;
        self.reads.borrow_mut().push((offset, n as u64));
        Ok(n)
    }
}

fn overlaps(reads: &[(u64, u64)], (at, len): (u64, u64)) -> bool {
    reads.iter().any(|&(o, n)| o < at + len && at < o + n)
}

#[test]
fn healthy_new_commit_passes_its_scope_and_claims_nothing_wider() {
    let s = archive();
    let v = scoped(&s, &verified(&s, 1));
    assert_eq!(v.chain, Status::Pass);
    assert_eq!(v.new_objects, Status::Pass);
    assert_eq!(v.scope_status(), Status::Pass);
    assert!(v.head_is_anchorable());
    assert_eq!(v.from_seq, 1);
    assert_eq!(v.to_seq, Some(2));
    let new = objects_of(&s, 2);
    assert_eq!(v.objects_expected, new.len() as u64);
    assert_eq!(v.objects_checked, new.len() as u64);
    assert_eq!(v.bytes_checked, new.iter().map(|o| o.1).sum::<u64>());
    // Never a statement about the archive as a whole, and it says so.
    assert_eq!(v.whole_archive, Status::Unknown);
    assert!(v.not_checked.iter().any(|i| i.item.contains("commits 0")));
    assert!(v.findings.is_empty());
    // More commits in scope: more objects.
    let wider = scoped(&s, &verified(&s, 0));
    assert_eq!(
        wider.objects_checked,
        (objects_of(&s, 1).len() + new.len()) as u64
    );
    // Nothing new to check: the head itself is the starting point.
    let none = scoped(&s, &verified(&s, 2));
    assert_eq!(
        (none.objects_expected, none.scope_status()),
        (0, Status::Pass)
    );
}

#[test]
fn a_flipped_byte_in_a_new_object_fails_the_scope() {
    let s = archive();
    let since = verified(&s, 1);
    for (at, len) in objects_of(&s, 2) {
        let bad = flipped(&s, at + len / 2);
        let v = scoped(&bad, &since);
        assert_eq!(v.new_objects, Status::Fail, "object at {at}");
        assert_eq!(v.scope_status(), Status::Fail);
        assert!(!v.head_is_anchorable());
        assert_eq!(v.whole_archive, Status::Unknown);
        assert!(
            v.findings.iter().any(|f| matches!(
                f.code,
                ErrorCode::StoredIntegrityFailed | ErrorCode::ReferenceInvalid
            )),
            "{:?}",
            v.findings
        );
        let range = v.findings[0].affected.unwrap();
        assert_eq!((range.first, range.last), (2, 2));
    }
}

#[test]
fn a_flipped_byte_in_an_old_object_is_not_read_and_not_claimed() {
    let s = archive();
    let since = verified(&s, 1);
    let old = objects_of(&s, 0);
    let bad = flipped(&s, old[0].0 + old[0].1 / 2);

    // The scope is the new commit: it passes, it never touched the old
    // bytes, and it still does not say the archive is sound.
    let counting = Counting {
        inner: &bad,
        reads: RefCell::new(Vec::new()),
    };
    let v = scoped(&counting, &since);
    assert_eq!(v.scope_status(), Status::Pass);
    assert_eq!(v.whole_archive, Status::Unknown);
    for o in objects_of(&bad, 0).into_iter().chain(objects_of(&bad, 1)) {
        assert!(
            !overlaps(&counting.reads.borrow(), o),
            "the scoped check read an object outside its scope: {o:?}"
        );
    }
    assert!(
        v.not_checked.iter().any(|i| i.item.contains("commits 0")),
        "{:?}",
        v.not_checked
    );

    // The full verification is what finds it.
    let full = verify(&bad, &VerifyOptions::default(), &Job::new().ctx());
    assert!(!full.head_is_anchorable());
}

#[test]
fn a_history_without_the_previous_head_fails_the_chain() {
    let s = archive();
    // A different archive's head.
    let other = archive();
    let mut foreign = verified(&other, 1);
    let mut id = *foreign.archive_id.as_bytes();
    id[0] ^= 1;
    foreign.archive_id = mochi_core::object::ArchiveId::from_bytes(id);
    let v = scoped(&s, &foreign);
    assert_eq!(v.chain, Status::Fail);
    assert_eq!(v.scope_status(), Status::Fail);
    assert_eq!(v.objects_checked, 0);
    assert!(v
        .findings
        .iter()
        .any(|f| f.code == ErrorCode::FreshnessFailed));

    // Same archive, a commit ID that is not the one at that sequence.
    let mut wrong = verified(&s, 1);
    wrong.commit_id = verified(&s, 0).commit_id;
    assert_eq!(scoped(&s, &wrong).chain, Status::Fail);

    // A rolled-back archive: the verified head is newer than the head.
    let h = history(&s);
    let cut = h[1].footer_offset + mochi_format::footer::FOOTER_FRAME_LEN;
    let rolled = SimStorage::from_bytes(s.contents()[..cut as usize].to_vec());
    let v = scoped(&rolled, &verified(&s, 2));
    assert_eq!(v.chain, Status::Fail);
    assert!(!v.head_is_anchorable());
}

#[test]
fn a_cancelled_scoped_check_is_unknown_not_pass() {
    let s = archive();
    let since = verified(&s, 0);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let ctx = JobContext {
        progress: &NullProgress,
        cancel: &cancel,
    };
    let v = verify_commits_since(&s, &ReadOptions::default(), &since, &ctx);
    assert!(v.operational_error);
    assert_ne!(v.scope_status(), Status::Pass);
    assert!(!v.head_is_anchorable());
}
