//! D29: the archive comment (spec Annex B.2.19; `docs/schemas/commit-record-v4.cddl`).
//!
//! A comment is a value of the commit record (schema 4, key 13, required
//! feature 3): set by one commit, repeated by every later one until changed,
//! and history like everything else. These tests write real archives and read
//! them back through the reader, the verifier, and the rewrites.

use mochi_core::compact::{compact, CompactOptions, Keep};
use mochi_core::publish::{
    commit_history, open_head, ArchiveWriter, CheckpointPolicy, ReadOptions, TailPolicy,
    Transaction,
};
use mochi_core::repair::{self, RepairOptions};
use mochi_core::status::{Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::registry::FEATURE_COMMENT;
use mochi_testkit::archive::{encrypted_options, path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const OUT: &str = "out.mochi";
const NOTE: &str = "Holiday photos\nsorted by day\t(2026)";

fn put(tx: &mut Transaction, p: &str, seed: u64) {
    tx.put_file(path(p), deterministic_bytes(seed, 120), attrs(0o644, 1));
}

/// Six commits: 0 no comment; 1 sets it; 2 adds a file (keeps it); 3 changes
/// it; 4 clears it; 5 adds a file (still none).
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
    c(&mut w, &|t| put(t, "a", 1));
    c(&mut w, &|t| {
        t.set_comment(NOTE);
    });
    c(&mut w, &|t| put(t, "b", 2));
    c(&mut w, &|t| {
        t.set_comment("second");
        put(t, "c", 3);
    });
    c(&mut w, &|t| {
        t.clear_comment();
    });
    c(&mut w, &|t| put(t, "d", 4));
    w.close().unwrap();
    s
}

fn comments(s: &SimStorage) -> Vec<Option<String>> {
    commit_history(s, &ReadOptions::default())
        .unwrap()
        .into_iter()
        .map(|h| h.commit.comment)
        .collect()
}

fn expected() -> Vec<Option<String>> {
    vec![
        None,
        Some(NOTE.into()),
        Some(NOTE.into()),
        Some("second".into()),
        None,
        None,
    ]
}

#[test]
fn a_comment_is_set_carried_forward_changed_and_cleared_and_history_keeps_each() {
    let s = source();
    assert_eq!(comments(&s), expected());
    for h in commit_history(&s, &ReadOptions::default()).unwrap() {
        let schema = h.commit.schema_version();
        if h.commit.comment.is_some() {
            assert_eq!(
                (schema, h.commit.required_features.clone()),
                (4, vec![FEATURE_COMMENT])
            );
        } else {
            assert_eq!((schema, h.commit.required_features.clone()), (1, vec![]));
        }
    }
}

#[test]
fn a_reopened_writer_carries_the_head_comment_forward() {
    let s = SimStorage::new();
    let job = Job::new();
    {
        let mut w =
            ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
        let mut tx = Transaction::new();
        put(&mut tx, "a", 1);
        tx.set_comment("kept");
        w.commit(tx, &job.ctx()).unwrap();
        w.close().unwrap();
    }
    let (mut w, _) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(2)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "b", 2);
    w.commit(tx, &job.ctx()).unwrap();
    w.close().unwrap();
    assert_eq!(
        comments(&s),
        [Some("kept".to_owned()), Some("kept".to_owned())]
    );
    assert_eq!(
        open_head(&s, &ReadOptions::default())
            .unwrap()
            .commit
            .comment
            .as_deref(),
        Some("kept")
    );
}

/// Verification treats a commented archive exactly as one without comments:
/// the same overall status and dimensions at every level, and no finding.
#[test]
fn an_archive_with_comments_verifies_like_one_without() {
    let with = source();
    let without = {
        let s = SimStorage::new();
        let mut w =
            ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
        w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
        let job = Job::new();
        for (i, p) in ["a", "a", "b", "c", "c", "d"].iter().enumerate() {
            let mut tx = Transaction::new();
            put(&mut tx, p, i as u64 + 1);
            w.commit(tx, &job.ctx()).unwrap();
        }
        w.close().unwrap();
        s
    };
    for level in [
        VerificationLevel::Structural,
        VerificationLevel::Restoration,
    ] {
        let opts = VerifyOptions {
            level,
            ..VerifyOptions::default()
        };
        let a = verify(&with, &opts, &Job::new().ctx()).report;
        let b = verify(&without, &opts, &Job::new().ctx()).report;
        assert_eq!(a.overall_status, b.overall_status, "{level:?}");
        assert_ne!(a.overall_status, Status::Fail, "{level:?}");
        assert_eq!(a.dimensions, b.dimensions, "{level:?}");
    }
}

#[test]
fn a_bad_comment_is_refused_before_anything_is_written() {
    let s = SimStorage::new();
    let job = Job::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1);
    w.commit(tx, &job.ctx()).unwrap();
    let before = s.contents();
    for bad in ["", "a\r\nb", "\u{1b}[31m"] {
        let mut tx = Transaction::new();
        tx.set_comment(bad);
        let e = w.commit(tx, &job.ctx()).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad:?}");
        assert_eq!(s.contents(), before, "nothing written for {bad:?}");
    }
    let mut tx = Transaction::new();
    tx.set_comment("x".repeat(16 * 1024 + 1));
    assert_eq!(
        w.commit(tx, &job.ctx()).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

/// A commit record is readable without the passphrase, so an Encrypted
/// archive refuses a comment instead of leaking it.
#[test]
fn an_encrypted_archive_refuses_a_comment() {
    let s = SimStorage::new();
    let job = Job::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(1)),
        encrypted_options(&["pass phrase"]),
    )
    .unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1);
    tx.set_comment("secret plan");
    let e = w.commit(tx, &job.ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFeature);
    assert!(!s.contents().windows(11).any(|w| w == b"secret plan"));
}

#[test]
fn compaction_keeps_the_comment_of_every_version() {
    let src = source();
    let (writer, _) = ArchiveWriter::open_append(
        src.clone(),
        Box::new(SeqIds::new(5)),
        test_options(),
        TailPolicy::Refuse,
    )
    .unwrap();
    let mut dir = SimDir::new();
    compact(
        &writer,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(6)),
        &Keep::Every,
        &CompactOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    drop(writer);
    let out = dir.file(OUT).unwrap();
    assert_eq!(comments(&out), expected());
}

#[test]
fn repair_keeps_the_comment_of_every_version_it_recovers() {
    let src = source();
    let o = ReadOptions::default();
    let plan = repair::plan(&src, &o, &Job::new().ctx()).unwrap();
    let mut dir = SimDir::new();
    repair::apply(
        &src,
        &plan,
        &mut dir,
        OUT,
        Box::new(SeqIds::new(9)),
        &o,
        &RepairOptions::default(),
        &Job::new().ctx(),
    )
    .unwrap();
    let out = dir.file(OUT).unwrap();
    assert_eq!(comments(&out), expected());
}

/// An archive without a comment is written exactly as before D29: schema 1
/// throughout, and the same bytes as a writer that never heard of comments
/// would produce (the R8 golden set checks this byte for byte; here, no
/// commit lists the feature).
#[test]
fn an_archive_without_a_comment_has_no_schema_4_commit() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    for h in commit_history(&s, &ReadOptions::default()).unwrap() {
        assert_eq!(h.commit.schema_version(), 1);
        assert!(h.commit.required_features.is_empty());
    }
}

// ---- Golden vectors (fixtures/golden/d29): one schema-4 commit record and two
// refusals, as stored frames. They also seed the `commit_decode` fuzz target.

mod vectors {
    use std::path::PathBuf;

    use mochi_core::commit::{uuid_v4, CommitRecord, Metadata, ObjectRef};
    use mochi_core::object::ArchiveId;
    use mochi_core::ErrorCode;
    use mochi_format::cbor::CborLimits;
    use mochi_format::digest::StoredObjectHash;
    use mochi_format::frame::encode_skippable_frame_within;
    use mochi_format::registry::{FrameKind, FEATURE_COMMENT};
    use mochi_format::Limits;

    fn r(offset: u64, stored_len: u64, n: u8) -> ObjectRef {
        ObjectRef {
            offset,
            stored_len,
            stored_hash: StoredObjectHash::from_bytes([n; 32]),
        }
    }

    fn record(comment: &str) -> CommitRecord {
        CommitRecord {
            archive_id: ArchiveId::from_bytes([0xA1; 32]),
            seq: 0,
            transaction_id: uuid_v4([0x7E; 16]),
            parent: None,
            metadata: Metadata::Checkpoint {
                image: r(160, 4104, 3),
                snapshot: r(4264, 90, 5),
            },
            delta_manifest: r(64, 96, 4),
            required_features: vec![FEATURE_COMMENT],
            time: None,
            descriptor: r(0, 56, 6),
            key_envelopes: Vec::new(),
            data_region: None,
            comment: Some(comment.to_owned()),
        }
    }

    /// The stored frame of a schema-4 record whose comment (key 13, the last
    /// map entry) is replaced after the ID was computed for `ok`, with the
    /// ID fixed up, so only the comment rule refuses it.
    fn with_raw_comment(raw: &str) -> Vec<u8> {
        use mochi_format::cbor::{self, Value};
        use mochi_format::digest::commit_id;
        use mochi_format::repr::CanonicalCommitBody;
        let (bytes, _) = record("ok").encode().unwrap();
        let Value::Map(mut m) = cbor::decode(&bytes, &CborLimits::default()).unwrap() else {
            unreachable!()
        };
        let last = m.len() - 1;
        m[last].1 = Value::Text(raw.to_owned());
        let body: Vec<_> = m.iter().filter(|(k, _)| *k != 9).cloned().collect();
        let id = commit_id(CanonicalCommitBody::assume_canonical(
            &cbor::encode(&Value::Map(body)).unwrap(),
        ));
        let at = m.iter().position(|(k, _)| *k == 9).unwrap();
        m[at].1 = Value::Bytes(id.as_bytes().to_vec());
        let payload = cbor::encode(&Value::Map(m)).unwrap();
        encode_skippable_frame_within(FrameKind::CommitRecord, &payload, &Limits::default())
            .unwrap()
    }

    /// (file name, bytes, expected result: None = valid, else the code).
    fn all() -> Vec<(&'static str, Vec<u8>, Option<ErrorCode>)> {
        let valid = record("Holiday photos\nsorted by day\t(2026) 🙂")
            .to_stored()
            .unwrap()
            .0
            .as_bytes()
            .to_vec();
        vec![
            ("valid-commit-comment.bin", valid, None),
            (
                "reject-commit-comment-carriage-return.bin",
                with_raw_comment("line\r\nnext"),
                Some(ErrorCode::RecordInvalid),
            ),
            (
                "reject-commit-comment-empty.bin",
                with_raw_comment(""),
                Some(ErrorCode::RecordInvalid),
            ),
        ]
    }

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/d29")
    }

    #[test]
    fn d29_vectors_behave_as_expected() {
        for (name, bytes, want) in all() {
            let got = CommitRecord::from_stored(&bytes, &Limits::default(), &CborLimits::default());
            match (want, got) {
                (None, Ok((rec, _))) => {
                    assert_eq!(rec.to_stored().unwrap().0.as_bytes(), &bytes[..], "{name}")
                }
                (Some(code), Err(e)) => assert_eq!(e.code, code, "{name}: {}", e.message),
                (want, got) => panic!("{name}: expected {want:?}, got {got:?}"),
            }
        }
    }

    #[test]
    fn checked_in_d29_files_match_the_builders() {
        for (name, bytes, _) in all() {
            let on_disk = std::fs::read(dir().join(name))
                .unwrap_or_else(|e| panic!("{name}: {e} (run the ignored writer)"));
            assert_eq!(on_disk, bytes, "{name}");
        }
    }

    #[test]
    #[ignore = "writes new fixtures; run by hand and review the diff"]
    fn write_d29_golden_files() {
        std::fs::create_dir_all(dir()).unwrap();
        for (name, bytes, _) in all() {
            std::fs::write(dir().join(name), bytes).unwrap();
        }
    }
}
