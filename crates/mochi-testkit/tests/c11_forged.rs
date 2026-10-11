//! C11: forged commits on top of a real Encrypted archive (spec Annex B.2.10
//! D20 items 5, 7, 10). Each case changes one thing a reader must notice; a
//! control with nothing changed proves the forgery is otherwise well formed.
//!
//! The forger holds the passphrase (it seals the manifest itself), so what is
//! rejected is rejected by structure and binding, not by the lack of a key.

use mochi_core::commit::ObjectRef;
use mochi_core::manifest::KeyOp;
use mochi_core::publish::{
    open_head, ArchiveWriter, HistoryEntry, ReadOptions, TailPolicy, Transaction,
};
use mochi_core::rekey::{list, parse_envelope_id, rewrap};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::seal::{seal_frame, SealTarget};
use mochi_format::secret::{OsRandom, Passphrase};
use mochi_format::Limits;
use mochi_testkit::archive::{encrypted_options, keyed_read, path, Job};
use mochi_testkit::forge::{delta_record, empty_delta, rule_base, txid, Forge};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds};

const A: &str = "forger's passphrase";
const B: &str = "second passphrase";

/// Commit 0 (one file) and commit 1 (a rewrap adding `B`): two envelopes.
fn base() -> Forge {
    let s = mochi_testkit::SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), encrypted_options(&[A]))
        .unwrap();
    let mut tx = Transaction::new();
    tx.put_file(path("f"), deterministic_bytes(1, 100), attrs(0o644, 0));
    w.commit(tx, &Job::new().ctx()).unwrap();
    rewrap(
        &mut w,
        vec![Passphrase::new(B).unwrap()],
        vec![],
        &Job::new().ctx(),
    )
    .unwrap();
    w.close().unwrap();
    Forge::new(s.contents())
}

struct Plan {
    /// Sequence the manifest is sealed for (the commit's is `prev + 1`).
    sealed_for: Option<u64>,
    /// Key operations the manifest records, by index into the previous
    /// commit's envelope list (`None`: an ID no envelope has).
    ops: Vec<(bool, Option<usize>)>,
    /// The envelopes the commit lists (by index into the previous list).
    keep: Vec<usize>,
    /// Seal the manifest at all (false: a plain Core-style manifest frame).
    seal: bool,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            sealed_for: None,
            ops: Vec::new(),
            keep: vec![0, 1],
            seal: true,
        }
    }
}

/// Forge commit 2 on the base and return the forged archive.
fn forge(plan: Plan) -> mochi_testkit::SimStorage {
    let mut f = base();
    let prev: HistoryEntry = f.history().pop().unwrap();
    assert_eq!(prev.commit.key_envelopes.len(), 2);
    let session = keyed_read(&[A]);
    let head = open_head(&f.storage(), &session).unwrap();
    let key = head
        .unlocked
        .clone()
        .expect("an Encrypted head is unlocked");

    let id = txid(7);
    let mut m = empty_delta(&prev, id);
    m.required_features = vec![1];
    let ids: Vec<[u8; 16]> = list(&f.storage(), &ReadOptions::default())
        .unwrap()
        .into_iter()
        .map(|e| parse_envelope_id(&e.envelope_id).unwrap())
        .collect();
    m.keys.ops = plan
        .ops
        .iter()
        .map(|(add, i)| {
            let id = i.map_or([0xEE; 16], |i| ids[i]);
            if *add {
                KeyOp::Add(id)
            } else {
                KeyOp::Remove(id)
            }
        })
        .collect();
    let stored = if plan.seal {
        let target = SealTarget::DeltaManifest {
            sequence: plan.sealed_for.unwrap_or(prev.commit.seq + 1),
            transaction_id: id,
        };
        seal_frame(
            &key.context(),
            &target,
            &m.encode().unwrap(),
            &mut OsRandom,
            &Limits::WRITER_DEFAULT,
        )
        .unwrap()
    } else {
        m.to_stored().unwrap()
    };
    let r: ObjectRef = f.append_object(&stored);
    let mut record = delta_record(&prev, rule_base(&prev), r, id);
    record.required_features = vec![1];
    record.data_region = None;
    record.key_envelopes = plan
        .keep
        .iter()
        .map(|i| prev.commit.key_envelopes[*i])
        .collect();
    f.append_commit(&record);
    f.storage()
}

fn open(s: &mochi_testkit::SimStorage) -> mochi_core::Result<u64> {
    open_head(s, &keyed_read(&[A, B])).map(|h| h.commit.seq)
}

/// What `verify` finds, with the key, at the structural level.
fn verified(s: &mochi_testkit::SimStorage) -> mochi_core::report::Report {
    verify(
        s,
        &VerifyOptions {
            level: VerificationLevel::Structural,
            read: keyed_read(&[A, B]),
            ..VerifyOptions::default()
        },
        &Job::new().ctx(),
    )
    .report
}

fn record_invalid(s: &mochi_testkit::SimStorage) -> bool {
    let r = verified(s);
    r.dimensions[&Dimension::Integrity] == Status::Fail
        && r.findings
            .iter()
            .any(|f| f.code == ErrorCode::RecordInvalid)
}

#[test]
fn the_control_forgery_is_well_formed() {
    let s = forge(Plan::default());
    assert_eq!(open(&s).unwrap(), 2);
    let r = verified(&s);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
}

#[test]
fn a_commit_whose_envelope_list_disagrees_with_the_replayed_key_state_is_refused() {
    // Lists one envelope, while the manifests replay to two. A reader that
    // never replays S(b) opens it (D10.9); `verify` and the writer do not.
    let s = forge(Plan {
        keep: vec![0],
        ..Plan::default()
    });
    assert!(record_invalid(&s), "{:?}", verified(&s).findings);
    let e = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(9)),
        encrypted_options(&[A, B]),
        TailPolicy::Refuse,
    )
    .map(|_| ())
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::RecordInvalid, "{e}");

    // The manifest records a removal that the commit's list does not make,
    // and the reverse: a removal made that no manifest records.
    let s = forge(Plan {
        ops: vec![(false, Some(1))],
        ..Plan::default()
    });
    assert!(record_invalid(&s));

    // Both agree: a removal recorded and made. The control for the pair.
    let s = forge(Plan {
        ops: vec![(false, Some(1))],
        keep: vec![0],
        ..Plan::default()
    });
    assert_eq!(open(&s).unwrap(), 2);
    let r = verified(&s);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
}

#[test]
fn key_operations_that_do_not_apply_are_refused() {
    // Removing a missing envelope, adding one already present, and emptying
    // the set (the commit lists one that the manifest has removed).
    for (ops, keep) in [
        (vec![(false, None)], vec![0, 1]),
        (vec![(true, Some(0))], vec![0, 1]),
        (vec![(false, Some(0)), (false, Some(1))], vec![0]),
    ] {
        let s = forge(Plan {
            ops: ops.clone(),
            keep,
            ..Plan::default()
        });
        assert!(record_invalid(&s), "{ops:?}: {:?}", verified(&s).findings);
    }
}

#[test]
fn a_manifest_sealed_for_another_commit_or_not_sealed_is_refused() {
    let s = forge(Plan {
        sealed_for: Some(5),
        ..Plan::default()
    });
    assert_eq!(
        open(&s).unwrap_err().code,
        ErrorCode::ContentIntegrityFailed,
        "bound to its commit's sequence"
    );
    let s = forge(Plan {
        seal: false,
        ..Plan::default()
    });
    assert!(
        open(&s).is_err(),
        "a plain manifest frame in an Encrypted archive is refused"
    );
}
