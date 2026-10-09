//! C10: `verify` checks a TAR-compatible archive's streams (spec Annex B.2.9
//! D19 rule 8). A violation is `PROFILE_VIOLATION`, a `FAIL` of integrity,
//! exit 1, found at every level; re-emitted content is hashed from
//! `content_integrity` up.

use mochi_core::descriptor::Profile;
use mochi_core::exit;
use mochi_core::publish::{
    commit_history, open_head, ArchiveWriter, CheckpointPolicy, ReadOptions, TarTamper,
    WriterOptions,
};
use mochi_core::report::Report;
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::digest::stored_object_hash;
use mochi_testkit::archive::{test_options, Job};
use mochi_testkit::forge::Forge;
use mochi_testkit::replay::{attrs, Model};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const LEVELS: [VerificationLevel; 5] = [
    VerificationLevel::Structural,
    VerificationLevel::Referential,
    VerificationLevel::StoredIntegrity,
    VerificationLevel::ContentIntegrity,
    VerificationLevel::Restoration,
];

const TAR: Profile = Profile {
    tar_compatible: true,
    encrypted: false,
};

fn tar_options() -> WriterOptions {
    WriterOptions {
        profile: Some(TAR),
        ..test_options()
    }
}

/// Fresh files over several chunks, an empty file, a directory, a rename
/// (re-emitted content), a replacement, and a delete-only commit.
fn steps() -> Vec<mochi_testkit::replay::Step> {
    let mut m = Model::default();
    m.dir("d", attrs(0o750, 0));
    m.file("d/a", deterministic_bytes(1, 200), attrs(0o640, 1));
    m.file("b", deterministic_bytes(2, 70), attrs(0o600, 2));
    m.file("c", Vec::new(), attrs(0o444, 3));
    m.end(0);
    m.file("d/a", deterministic_bytes(3, 130), attrs(0o641, 11));
    m.end(10);
    m.rename("b", "d/b");
    m.delete("c");
    m.end(20);
    m.delete("d/a");
    m.end(30);
    m.steps
}

fn write(tamper: Option<TarTamper>, policy: CheckpointPolicy) -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), tar_options()).unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    w.set_tar_tamper(tamper);
    let job = Job::new();
    for st in steps() {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn run(s: &SimStorage, level: VerificationLevel, deep: bool) -> Report {
    let opts = VerifyOptions {
        level,
        deep,
        ..VerifyOptions::default()
    };
    let r = verify(s, &opts, &Job::new().ctx()).report;
    r.validate().unwrap_or_else(|e| panic!("{e:?}"));
    r
}

fn violations(r: &Report) -> usize {
    r.findings
        .iter()
        .filter(|f| f.code == ErrorCode::ProfileViolation)
        .count()
}

/// A healthy TAR-compatible archive verifies at every level, `fsck` depth
/// included, with every checkpoint policy; its scope says the streams were
/// parsed, and a Core archive's does not.
#[test]
fn c10_healthy_tar_archive_verifies_at_every_level() {
    for policy in [CheckpointPolicy::EveryCommit, CheckpointPolicy::Every(3)] {
        let s = write(None, policy);
        for (i, level) in LEVELS.into_iter().enumerate() {
            for deep in [false, true] {
                let r = run(&s, level, deep);
                assert_eq!(violations(&r), 0, "{level:?}: {:?}", r.findings);
                assert!(r.findings.is_empty(), "{level:?}: {:?}", r.findings);
                assert!(
                    r.scope.as_deref().unwrap().contains("TAR stream"),
                    "{:?}",
                    r.scope
                );
                if i >= 2 {
                    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Pass);
                }
            }
        }
    }
    let sc = SimStorage::new();
    let mut w =
        ArchiveWriter::create(sc.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    let job = Job::new();
    for st in steps() {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    let r = run(&sc, VerificationLevel::Restoration, false);
    assert!(!r.scope.as_deref().unwrap().contains("TAR"));
}

/// Each way a stream can differ from its commit's puts is found, at every
/// level, as `PROFILE_VIOLATION` under integrity (exit 1). The code name
/// stays stable (it is pinned by the registry snapshot).
#[test]
fn c10_a_stream_that_differs_from_the_puts_is_a_violation() {
    for t in [
        TarTamper::ModeOff,
        TarTamper::NoEndBlocks,
        TarTamper::ExtraBlocks,
        TarTamper::ExtraMember,
        TarTamper::DropDirectoryMember,
    ] {
        let s = write(Some(t), CheckpointPolicy::EveryCommit);
        for level in LEVELS {
            let r = run(&s, level, false);
            assert!(violations(&r) > 0, "{t:?} {level:?}: {:?}", r.findings);
            assert_eq!(
                r.dimensions[&Dimension::Integrity],
                Status::Fail,
                "{t:?} {level:?}"
            );
            assert_eq!(r.exit_code, exit::FAILED, "{t:?} {level:?}");
        }
    }
}

/// Re-emitted content is length-checked everywhere but hashed only from
/// `content_integrity`: a flipped byte in a rename's re-written bytes is
/// invisible below that level and a violation at it.
#[test]
fn c10_reemitted_content_is_hashed_from_content_integrity_up() {
    let s = write(
        Some(TarTamper::ReemitFlipped),
        CheckpointPolicy::EveryCommit,
    );
    for (i, level) in LEVELS.into_iter().enumerate() {
        let r = run(&s, level, false);
        let want = i >= 3;
        assert_eq!(violations(&r) > 0, want, "{level:?}: {:?}", r.findings);
        if want {
            assert_eq!(r.dimensions[&Dimension::Integrity], Status::Fail);
            assert!(r
                .findings
                .iter()
                .any(|f| f.message.as_deref().unwrap().contains("commit 2")));
        }
    }
}

/// A Core archive that merely *claims* the profile has no streams: its
/// chunks are file bytes, so the check fails it.
#[test]
fn c10_a_core_archive_claiming_the_profile_fails() {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), test_options()).unwrap();
    w.commit(steps()[0].tx.clone(), &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let h = commit_history(&s, &ReadOptions::default()).unwrap();
    let mut d = open_head(&s, &ReadOptions::default()).unwrap().descriptor;
    d.tar_compatible = true;
    let desc = d.to_stored().unwrap();
    let mut bytes = s.contents();
    let r0 = h[0].commit.descriptor;
    bytes[..desc.as_bytes().len()].copy_from_slice(desc.as_bytes());
    bytes.truncate(h[0].commit_offset as usize);
    let mut f = Forge::new(bytes);
    let mut rec = h[0].commit.clone();
    rec.descriptor.stored_hash = stored_object_hash(desc.view());
    assert_eq!(r0.offset, 0);
    f.append_commit(&rec);
    let fs = f.storage();
    for level in LEVELS {
        let r = run(&fs, level, false);
        assert!(violations(&r) > 0, "{level:?}: {:?}", r.findings);
        assert_eq!(r.dimensions[&Dimension::Integrity], Status::Fail);
    }
}

/// A stream with fewer members than the commit has puts is a violation even
/// when every member it has is right: the last put (a directory here, so
/// there is no content to misparse) has none.
#[test]
fn c10_a_stream_missing_its_last_member_is_a_violation() {
    let mut m = Model::default();
    m.file("a", deterministic_bytes(1, 100), attrs(0o644, 1));
    m.dir("z", attrs(0o755, 2));
    m.end(0);
    for (tamper, bad) in [(None, false), (Some(TarTamper::DropDirectoryMember), true)] {
        let s = SimStorage::new();
        let mut w =
            ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(7)), tar_options()).unwrap();
        w.set_tar_tamper(tamper);
        w.commit(m.steps[0].tx.clone(), &Job::new().ctx()).unwrap();
        w.close().unwrap();
        let r = run(&s, VerificationLevel::Structural, false);
        assert_eq!(violations(&r) > 0, bad, "{tamper:?}: {:?}", r.findings);
        if bad {
            assert!(
                r.findings.iter().any(|f| f
                    .message
                    .as_deref()
                    .unwrap()
                    .contains("1 members, the commit has 2 puts")),
                "{:?}",
                r.findings
            );
        }
    }
}
