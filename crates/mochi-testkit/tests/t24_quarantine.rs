//! T24 (quarantine) and T25 (waivers): spec Annex B.2 D14; gate G8.
//!
//! Before an eligible tail is truncated, it is copied exactly into a
//! no-clobber sidecar, synced, re-read and hash-compared, and the directory
//! flushed. Any failure truncates nothing. Waivers are explicit and
//! recorded.

use mochi_core::publish::{
    open_head, ArchiveWriter, QuarantineRecord, ReadOptions, TailPolicy, TailRepair,
    TailTruncation, Transaction, TruncationWaivers, Waiver,
};
use mochi_core::quarantine::{read_sidecar, sidecar_name};
use mochi_core::report::Severity;
use mochi_core::storage::os::{OsDir, OsStorage};
use mochi_core::storage::{DirectoryDurability, ReadStorage};
use mochi_core::ErrorCode;
use mochi_format::digest::tail_quarantine_hash;
use mochi_format::registry::FrameKind;
use mochi_testkit::archive::{
    build, path, read_state, scripted_history, test_options, Content, Job,
};
use mochi_testkit::{DirFault, Fault, SeqIds, SimDir, SimStorage};

const NAME: &str = "a.mochi";

/// A three-commit archive followed by an eligible tail: a complete copy of
/// the head's delta-manifest frame, then half of it (cut short by EOF).
fn archive_with_tail() -> (Vec<u8>, usize) {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    let mut bytes = s.contents();
    let committed = bytes.len();
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    let r = head.commit.delta_manifest;
    let frame = bytes[r.offset as usize..(r.offset + r.stored_len) as usize].to_vec();
    bytes.extend_from_slice(&frame);
    bytes.extend_from_slice(&frame[..frame.len() / 2]);
    (bytes, committed)
}

fn dir_with(bytes: &[u8]) -> SimDir {
    let d = SimDir::new();
    d.insert(NAME, SimStorage::from_bytes(bytes.to_vec()));
    d
}

fn repair(
    dir: &mut SimDir,
    waivers: TruncationWaivers,
) -> mochi_core::Result<(ArchiveWriter<SimStorage>, Option<TailTruncation>)> {
    ArchiveWriter::open_append_in(
        dir,
        NAME,
        Box::new(SeqIds::new(500)),
        test_options(),
        TailRepair::Truncate(waivers),
    )
}

fn sidecars(dir: &SimDir) -> Vec<String> {
    dir.names()
        .into_iter()
        .filter(|n| n.ends_with(".mochiq"))
        .collect()
}

/// **Checklist DoD.** The sidecar holds exactly the removed bytes (its hash
/// matches them), with the schema's metadata and name; then the archive is
/// truncated to its last commit and keeps working.
#[test]
fn t24_quarantine_then_truncate() {
    let (bytes, committed) = archive_with_tail();
    let tail = &bytes[committed..];
    let mut dir = dir_with(&bytes);
    let (mut w, t) = repair(&mut dir, TruncationWaivers::default()).unwrap();
    let t = t.expect("a truncation record");
    let q: QuarantineRecord = t.quarantine.clone().expect("quarantined");
    assert_eq!(q.directory, DirectoryDurability::Confirmed);
    assert!(t.waivers.is_empty());
    assert_eq!(q.tail_hash, tail_quarantine_hash(tail));
    assert_eq!(
        q.sidecar,
        sidecar_name(NAME, committed as u64, &q.tail_hash)
    );
    assert_eq!(sidecars(&dir), std::slice::from_ref(&q.sidecar));
    assert!(
        dir.durable_names().contains(&q.sidecar),
        "the directory was flushed"
    );

    let side = dir.file(&q.sidecar).unwrap();
    let (m, at) = read_sidecar(&side).unwrap();
    assert_eq!(
        &side.contents()[at as usize..],
        tail,
        "exactly the removed bytes"
    );
    assert_eq!(m.tail_offset, committed as u64);
    assert_eq!(m.tail_len, tail.len() as u64);
    assert_eq!(m.head_seq, 2);
    assert_eq!(m.head_commit_id, t.head_commit_id);
    assert_eq!(
        m.frame_magics,
        [FrameKind::RecoveryManifest.magic().unwrap()]
    );
    assert!(m.incomplete_final_frame);
    assert!(m.time.ends_with('Z') && m.time.len() == 30);

    let archive = dir.file(NAME).unwrap();
    assert_eq!(archive.contents(), &bytes[..committed]);
    let job = Job::new();
    let mut tx = Transaction::new();
    tx.put_file(path("after-truncation"), b"x".to_vec(), Default::default());
    w.commit(tx, &job.ctx()).unwrap();
    drop(w);
    let head = open_head(&archive, &ReadOptions::default()).unwrap();
    assert_eq!(head.seq(), 3);
    let mut want = scripted_history()[2].after.clone();
    want.insert(b"after-truncation".to_vec(), Content::File(b"x".to_vec()));
    assert_eq!(read_state(&archive, &head).unwrap(), want);

    let f = t.findings();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Info);
    assert!(f[0].message.as_ref().unwrap().contains(&q.sidecar));
}

/// **Checklist DoD.** A failure injected at each quarantine step truncates
/// nothing and leaves no partial sidecar behind.
#[test]
fn t24_a_failure_at_each_step_truncates_nothing() {
    let (bytes, _) = archive_with_tail();
    type Setup = Box<dyn Fn(&SimDir)>;
    let cases: Vec<(&str, Setup, ErrorCode)> = vec![
        (
            "creating the sidecar",
            Box::new(|d: &SimDir| d.add_fault(DirFault::FailCreate { index: 0 })),
            ErrorCode::QuarantineFailed,
        ),
        (
            "writing the header",
            Box::new(|d: &SimDir| {
                d.add_next_file_fault(Fault::HaltBeforeMutation { mutation_index: 0 })
            }),
            ErrorCode::QuarantineFailed,
        ),
        (
            "copying the tail",
            Box::new(|d: &SimDir| {
                d.add_next_file_fault(Fault::HaltBeforeMutation { mutation_index: 1 })
            }),
            ErrorCode::QuarantineFailed,
        ),
        (
            "syncing the sidecar",
            Box::new(|d: &SimDir| d.add_next_file_fault(Fault::FailSync { sync_index: 0 })),
            ErrorCode::QuarantineFailed,
        ),
        (
            "silent corruption, found on re-read",
            Box::new(|d: &SimDir| {
                d.add_next_file_fault(Fault::CorruptAppend {
                    append_index: 1,
                    at: 3,
                    xor: 1,
                })
            }),
            ErrorCode::QuarantineFailed,
        ),
        (
            "flushing the directory",
            Box::new(|d: &SimDir| d.add_fault(DirFault::FailSyncDirectory { index: 0 })),
            ErrorCode::QuarantineFailed,
        ),
        (
            "an unconfirmed flush, without the waiver",
            Box::new(|d: &SimDir| d.add_fault(DirFault::UnconfirmedSyncDirectory { index: 0 })),
            ErrorCode::DurabilityUnconfirmed,
        ),
    ];
    for (what, setup, code) in cases {
        let mut dir = dir_with(&bytes);
        setup(&dir);
        let e = repair(&mut dir, TruncationWaivers::default())
            .map(|_| ())
            .unwrap_err();
        assert_eq!(e.code, code, "{what}: {e}");
        assert_eq!(
            dir.file(NAME).unwrap().contents(),
            bytes,
            "{what}: truncated"
        );
        assert!(sidecars(&dir).is_empty(), "{what}: {:?}", sidecars(&dir));
        // Nothing stays locked: the archive opens again.
        let mut again = dir.clone();
        assert!(
            repair(&mut again, TruncationWaivers::default()).is_ok(),
            "{what}"
        );
    }
}

/// A process kill before any directory operation of the quarantine leaves
/// the archive untouched.
#[test]
fn t24_a_crash_during_quarantine_truncates_nothing() {
    let (bytes, _) = archive_with_tail();
    for k in 0..2 {
        let mut dir = dir_with(&bytes);
        dir.add_fault(DirFault::HaltBeforeOp { index: k });
        assert!(
            repair(&mut dir, TruncationWaivers::default()).is_err(),
            "{k}"
        );
        let after = dir.crash_image(mochi_testkit::CrashMode::KeepAll);
        assert_eq!(after.file(NAME).unwrap().contents(), bytes, "{k}");
    }
}

/// **Checklist DoD.** An existing sidecar is refused, never replaced, and
/// nothing is truncated.
#[test]
fn t24_an_existing_sidecar_is_refused() {
    let (bytes, committed) = archive_with_tail();
    let name = sidecar_name(
        NAME,
        committed as u64,
        &tail_quarantine_hash(&bytes[committed..]),
    );
    let mut dir = dir_with(&bytes);
    dir.insert(&name, SimStorage::from_bytes(b"earlier sidecar".to_vec()));
    let e = repair(&mut dir, TruncationWaivers::default())
        .map(|_| ())
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::QuarantineFailed, "{e}");
    assert!(e.message.contains("already exists"), "{e}");
    assert_eq!(dir.file(&name).unwrap().contents(), b"earlier sidecar");
    assert_eq!(dir.file(NAME).unwrap().contents(), bytes);
}

/// **T25.** `--no-quarantine`: truncated, no sidecar, the waiver recorded as
/// a warning finding.
#[test]
fn t25_no_quarantine_waiver() {
    let (bytes, committed) = archive_with_tail();
    let mut dir = dir_with(&bytes);
    let (_w, t) = repair(
        &mut dir,
        TruncationWaivers {
            no_quarantine: true,
            accept_unconfirmed_durability: false,
        },
    )
    .unwrap();
    let t = t.unwrap();
    assert!(t.quarantine.is_none());
    assert_eq!(t.waivers, [Waiver::NoQuarantine]);
    assert!(sidecars(&dir).is_empty());
    assert_eq!(dir.file(NAME).unwrap().contents(), &bytes[..committed]);
    let f = t.findings();
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].severity, Severity::Warning);
    assert!(f[1].message.as_ref().unwrap().contains("--no-quarantine"));

    // open_append has no directory, so truncating through it is the same
    // recorded waiver.
    let s = SimStorage::from_bytes(bytes.clone());
    let (_w, t) = ArchiveWriter::open_append(
        s.clone(),
        Box::new(SeqIds::new(501)),
        test_options(),
        TailPolicy::TruncateWithoutQuarantine,
    )
    .unwrap();
    assert_eq!(t.unwrap().waivers, [Waiver::NoQuarantine]);
}

/// **T25.** `--accept-unconfirmed-durability`: the sidecar is still written,
/// verified, and kept; the truncation proceeds; the waiver is recorded with
/// the reason.
#[test]
fn t25_accept_unconfirmed_durability_waiver() {
    let (bytes, committed) = archive_with_tail();
    let mut dir = dir_with(&bytes);
    dir.add_fault(DirFault::UnconfirmedSyncDirectory { index: 0 });
    let (_w, t) = repair(
        &mut dir,
        TruncationWaivers {
            no_quarantine: false,
            accept_unconfirmed_durability: true,
        },
    )
    .unwrap();
    let t = t.unwrap();
    let q = t.quarantine.clone().unwrap();
    assert!(matches!(q.directory, DirectoryDurability::Unconfirmed(_)));
    assert!(matches!(
        t.waivers.as_slice(),
        [Waiver::AcceptUnconfirmedDurability { .. }]
    ));
    let side = dir.file(&q.sidecar).unwrap();
    assert_eq!(
        read_sidecar(&side).unwrap().0.tail_len,
        (bytes.len() - committed) as u64
    );
    assert_eq!(dir.file(NAME).unwrap().contents(), &bytes[..committed]);
    let f = t.findings();
    assert!(f
        .iter()
        .any(|x| x.code == ErrorCode::DurabilityUnconfirmed && x.severity == Severity::Warning));
}

/// **T25 DoD.** On the real OS: on Linux the flush is confirmed and the
/// truncation proceeds; on Windows (directory flush always unconfirmed until
/// G6) truncation without the override is refused with
/// `DURABILITY_UNCONFIRMED`, and with it proceeds.
#[test]
fn t25_os_durability_rule() {
    let (bytes, committed) = archive_with_tail();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join(NAME), &bytes).unwrap();
    let mut dir = OsDir::open(tmp.path()).unwrap();
    let r = ArchiveWriter::<OsStorage>::open_append_in(
        &mut dir,
        NAME,
        Box::new(SeqIds::new(502)),
        test_options(),
        TailRepair::Truncate(TruncationWaivers::default()),
    );
    let len = || std::fs::metadata(tmp.path().join(NAME)).unwrap().len() as usize;
    if cfg!(windows) {
        let e = r.map(|_| ()).unwrap_err();
        assert_eq!(e.code, ErrorCode::DurabilityUnconfirmed, "{e}");
        assert_eq!(len(), bytes.len(), "nothing truncated");
        let (w, t) = ArchiveWriter::<OsStorage>::open_append_in(
            &mut dir,
            NAME,
            Box::new(SeqIds::new(503)),
            test_options(),
            TailRepair::Truncate(TruncationWaivers {
                no_quarantine: false,
                accept_unconfirmed_durability: true,
            }),
        )
        .unwrap();
        drop(w);
        assert!(t.unwrap().quarantine.is_some());
    } else {
        let (w, t) = r.unwrap();
        drop(w);
        let t = t.unwrap();
        let q = t.quarantine.unwrap();
        assert_eq!(q.directory, DirectoryDurability::Confirmed);
        let side =
            mochi_core::storage::os::OsReadStorage::open(tmp.path().join(&q.sidecar)).unwrap();
        let (m, at) = read_sidecar(&side).unwrap();
        let mut back = vec![0; m.tail_len as usize];
        side.read_exact_at(at, &mut back).unwrap();
        assert_eq!(back, &bytes[committed..]);
    }
    assert_eq!(len(), committed);
}
