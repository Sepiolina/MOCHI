//! W20: `migrate`, the draft-2 to 1.0 rewrite (spec §26, Annex B.2.14 D24;
//! plan K25; `mochi_core::migrate`).
//!
//! Sources are real draft-2 archives: the writer's test control
//! (`descriptor::set_legacy_draft_for_tests`) stamps descriptor key 3 = 2 on
//! archives created on this thread, which is the only difference between the
//! two layouts (a test in `r8_golden.rs` shows the control rebuilds the
//! checked-in draft-2 archives byte for byte). The oracle is the test kit's
//! own reassembly of every snapshot (`archive::read_state`), never the
//! migrator's checks.

use mochi_core::descriptor::{set_legacy_draft_for_tests, DraftPolicy, Profile};
use mochi_core::job::{CancellationToken, JobContext, ProgressEvent, ProgressSink};
use mochi_core::manifest::Mtime;
use mochi_core::migrate::{migrate, MigrateOptions, Tamper};
use mochi_core::publish::{
    commit_history, open_at_footer, open_head, promised_attributes, segment_state, ArchiveWriter,
    CheckpointPolicy, OpenedHead, ReadOptions, Transaction, WriterOptions,
};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{
    encrypted_options, keyed_read, path, read_state, test_options, Job, TEST_KDF,
};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimDir, SimStorage};

const OUT: &str = "migrated.mochi";
const PASS: &str = "w20 passphrase";

/// A thread-scoped draft-2 writer; restored on drop.
struct Draft2(Option<u64>);

impl Draft2 {
    fn on() -> Draft2 {
        Draft2(set_legacy_draft_for_tests(Some(2)))
    }
}

impl Drop for Draft2 {
    fn drop(&mut self) {
        set_legacy_draft_for_tests(self.0);
    }
}

#[derive(Clone, Copy, Debug)]
struct P {
    name: &'static str,
    tar: bool,
    enc: bool,
    red: Option<u16>,
}

const PROFILES: [P; 6] = [
    P {
        name: "core",
        tar: false,
        enc: false,
        red: None,
    },
    P {
        name: "tar",
        tar: true,
        enc: false,
        red: None,
    },
    P {
        name: "enc",
        tar: false,
        enc: true,
        red: None,
    },
    P {
        name: "red",
        tar: false,
        enc: false,
        red: Some(10),
    },
    P {
        name: "tar+red",
        tar: true,
        enc: false,
        red: Some(10),
    },
    P {
        name: "enc+red",
        tar: false,
        enc: true,
        red: Some(10),
    },
];

fn writer_options(p: P) -> WriterOptions {
    let mut o = if p.enc {
        encrypted_options(&[PASS])
    } else {
        test_options()
    };
    o.profile = Some(Profile {
        tar_compatible: p.tar,
        encrypted: p.enc,
        redundancy: p.red,
    });
    o
}

/// The options a reader of `p`'s archives needs (the passphrase, if any).
fn read_options(p: P) -> ReadOptions {
    if p.enc {
        keyed_read(&[PASS])
    } else {
        ReadOptions::default()
    }
}

/// The reader of the draft: what `migrate` reads the source with.
fn draft_reader(p: P) -> ReadOptions {
    ReadOptions {
        drafts: DraftPolicy::AllowLastDraft,
        ..read_options(p)
    }
}

fn migrate_options(p: P) -> MigrateOptions {
    MigrateOptions {
        read: read_options(p),
        kdf: if p.enc { Some(TEST_KDF) } else { None },
        ..MigrateOptions::default()
    }
}

fn put(tx: &mut Transaction, p: &str, seed: u64, len: usize, mode: u32) {
    tx.put_file(
        path(p),
        deterministic_bytes(seed, len),
        attrs(mode, seed as i64),
    );
}

/// A draft-2 archive of nine commits in profile `p`: a directory, files that
/// span several chunks, a replacement, a rename, a deletion, an empty file, a
/// hostile name, an expiry and a hold.
fn draft2_source(p: P) -> SimStorage {
    let _d = Draft2::on();
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), writer_options(p))
        .expect("create");
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let n = std::cell::Cell::new(0i64);
    let c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        // An informational commit time, which a rewrite copies.
        n.set(n.get() + 1);
        tx.at(Mtime {
            secs: 1_700_000_000 + n.get(),
            nanos: 7,
        });
        w.commit(tx, &job.ctx()).expect("commit");
    };
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1, 300, 0o644);
        put(t, "b", 2, 150, 0o600);
        put(t, "<img src=x onerror=alert(1)>", 8, 70, 0o644);
    }); // 0
    c(&mut w, &|t| put(t, "d/a", 3, 400, 0o640)); // 1
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 2
    w.request_checkpoint();
    c(&mut w, &|t| put(t, "c", 4, 0, 0o644)); // 3: checkpoint, an empty file
    c(&mut w, &|t| {
        t.delete(path("c"));
    }); // 4
    c(&mut w, &|t| put(t, "e", 5, 200, 0o644)); // 5
    c(&mut w, &|t| {
        t.expire(1).expire(2).expire(3).expire(4).hold(b"audit", 2);
    }); // 6
    c(&mut w, &|t| put(t, "d/a", 6, 150, 0o644)); // 7
    c(&mut w, &|t| put(t, "f", 7, 500, 0o644)); // 8
    w.close().unwrap();
    s
}

fn opened_all(s: &SimStorage, opts: &ReadOptions) -> Vec<OpenedHead> {
    commit_history(s, opts)
        .unwrap()
        .iter()
        .map(|h| open_at_footer(s, h.footer_offset, opts).unwrap())
        .collect()
}

fn run(
    p: P,
    src: &SimStorage,
    dir: &mut SimDir,
    seed: u64,
    ctx: &JobContext<'_>,
) -> mochi_core::Result<mochi_core::migrate::MigrateReport> {
    migrate(
        src,
        dir,
        OUT,
        Box::new(SeqIds::new(seed)),
        &migrate_options(p),
        ctx,
    )
}

#[test]
fn every_draft_2_profile_migrates_to_an_equal_1_0_archive() {
    for p in PROFILES {
        let src = draft2_source(p);
        let before = src.contents();

        // Every other reader refuses it, naming the draft and the command.
        let refused = open_head(&src, &read_options(p)).unwrap_err();
        assert_eq!(refused.code, ErrorCode::UnsupportedFeature, "{}", p.name);
        assert!(
            refused.message.contains("draft 2") && refused.message.contains("mochi migrate"),
            "{}: {}",
            p.name,
            refused.message
        );
        let v = verify(
            &src,
            &VerifyOptions {
                read: read_options(p),
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        );
        assert_eq!(v.report.exit_code, 4, "{}", p.name);

        let mut dir = SimDir::new();
        let r = run(p, &src, &mut dir, 50, &Job::new().ctx()).unwrap();
        assert_eq!(
            src.contents(),
            before,
            "{}: the source is never written",
            p.name
        );
        assert!(r.source_kept && r.new_ratified);
        assert_eq!(r.source_draft, 2);
        assert_eq!(dir.names(), [OUT], "{}", p.name);
        let out = dir.file(OUT).unwrap();

        // The new archive is a ratified one: this build's ordinary reader
        // opens it, and it verifies at the restoration level, deep.
        let new = opened_all(&out, &read_options(p));
        assert!(
            new.iter().all(|h| h.descriptor.draft.is_none()),
            "{}",
            p.name
        );
        let old = opened_all(&src, &draft_reader(p));
        assert!(
            old.iter().all(|h| h.descriptor.draft == Some(2)),
            "{}",
            p.name
        );
        assert_eq!(new.len(), old.len(), "{}", p.name);
        assert_eq!(new.len(), 9);
        let v = verify(
            &out,
            &VerifyOptions {
                read: read_options(p),
                deep: true,
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        );
        assert_eq!(v.report.exit_code, 0, "{}: {:?}", p.name, v.report.findings);

        // Same profile, new archive, new data key if sealed.
        assert_eq!(new[0].descriptor.profile(), old[0].descriptor.profile());
        assert_eq!(new[0].descriptor.profile().tar_compatible, p.tar);
        assert_eq!(new[0].descriptor.encrypted(), p.enc);
        assert_eq!(new[0].descriptor.redundancy.map(|r| r.percent), p.red);
        assert_ne!(r.new_archive_id, r.source_archive_id);
        assert_eq!(r.resealed, p.enc, "{}", p.name);
        assert_eq!(r.profile.redundancy_percent, p.red);
        assert_eq!(r.profile.encrypted, p.enc);
        assert_eq!(r.profile.tar_compatible, p.tar);

        // The oracle: every snapshot's files, rebuilt by the test kit from
        // each archive's own chunks, are equal; so are the version IDs, the
        // promised attributes and the commit times.
        let (so, no) = (draft_reader(p), read_options(p));
        for (i, (a, b)) in old.iter().zip(&new).enumerate() {
            assert_eq!(
                read_state(&out, b).unwrap(),
                read_state(&src, a).unwrap(),
                "{}: snapshot {i}",
                p.name
            );
            assert_eq!(
                promised_attributes(&out, b, &no).unwrap(),
                promised_attributes(&src, a, &so).unwrap(),
                "{}: attributes {i}",
                p.name
            );
            // The Encrypted profile records no commit times (D20 item 6).
            assert_eq!(a.commit.time.is_some(), !p.enc);
            assert_eq!(b.commit.time, a.commit.time, "{}", p.name);
            let ver = |h: &OpenedHead| {
                h.catalog
                    .replay(None)
                    .unwrap()
                    .iter()
                    .map(|(path, e)| (path.as_stored().to_vec(), format!("{:?}", e.version)))
                    .collect::<Vec<_>>()
            };
            assert_eq!(ver(b), ver(a), "{}: version IDs {i}", p.name);
        }
        let retention = |s: &SimStorage, o: &ReadOptions| {
            let h = open_head(s, o).unwrap();
            segment_state(s, &h, o).unwrap().retention
        };
        assert_eq!(retention(&out, &no), retention(&src, &so), "{}", p.name);
        assert!(!retention(&out, &no).holds.is_empty());

        // The report is the mapping and the evidence.
        assert_eq!(r.commits.len(), 9);
        for (i, m) in r.commits.iter().enumerate() {
            assert_eq!((m.source_seq, m.new_seq), (i as u64, i as u64));
        }
        assert_eq!(r.comparison.snapshots_compared, 9);
        assert!(r.comparison.file_versions_compared >= 8);
        assert!(r.comparison.retention_equal);
        assert!(r.comparison.versions_read_back > 0);
        assert_eq!(r.new_verification.exit_code, 0);
        assert_eq!(r.source_verification.exit_code, 0);
        assert!(r.not_done.contains("step 7"));
    }
}

#[test]
fn nothing_is_migrated_from_the_wrong_kind_of_archive() {
    let p = PROFILES[0];

    // A ratified archive: nothing to migrate.
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1, 10, 0o644);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    let mut dir = SimDir::new();
    let e = run(p, &s, &mut dir, 2, &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.message.contains("already a ratified"), "{}", e.message);
    assert!(dir.names().is_empty());

    // Draft 1 (and any other number): refused, without the hint.
    let draft1 = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/golden/b2/reject-archive-draft-1.mochi"),
    )
    .unwrap();
    let src = SimStorage::from_bytes(draft1);
    let e = run(p, &src, &mut dir, 3, &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::UnsupportedFeature);
    assert!(e.message.contains("draft 1"), "{}", e.message);
    assert!(dir.names().is_empty());
}

#[test]
fn the_wrong_passphrase_is_key_unavailable_and_creates_nothing() {
    for p in [PROFILES[2], PROFILES[5]] {
        let src = draft2_source(p);
        let before = src.contents();
        let mut dir = SimDir::new();
        for options in [
            MigrateOptions {
                read: keyed_read(&["not the passphrase"]),
                kdf: Some(TEST_KDF),
                ..MigrateOptions::default()
            },
            // No passphrase at all.
            MigrateOptions {
                kdf: Some(TEST_KDF),
                ..MigrateOptions::default()
            },
        ] {
            let e = migrate(
                &src,
                &mut dir,
                OUT,
                Box::new(SeqIds::new(9)),
                &options,
                &Job::new().ctx(),
            )
            .unwrap_err();
            assert_eq!(e.code, ErrorCode::KeyUnavailable, "{}", p.name);
            assert!(!e.message.contains(PASS));
            assert!(dir.names().is_empty(), "{}", p.name);
            assert_eq!(src.contents(), before);
        }
    }
}

/// A damaged source is refused whole: no output, and the error is the one
/// `verify` would report (a violation, exit 1), never a partial archive.
#[test]
fn a_damaged_source_is_refused_never_converted_partially() {
    for p in [PROFILES[0], PROFILES[2], PROFILES[3]] {
        let src = draft2_source(p);
        // Flip one byte inside the newest chunk's stored bytes.
        let head = open_head(&src, &draft_reader(p)).unwrap();
        let newest = head
            .catalog
            .replay(None)
            .unwrap()
            .iter()
            .filter_map(|(_, e)| head.catalog.file_version(&e.version).unwrap())
            .flat_map(|(_, extents)| extents)
            .filter_map(|x| match x.source {
                mochi_core::catalog::extent::ExtentSource::Chunk { chunk, .. } => Some(chunk),
                _ => None,
            })
            .last()
            .expect("a chunk");
        let at = head.catalog.object_location(&newest).unwrap().unwrap();
        let len = head.catalog.object(&newest).unwrap().unwrap().stored_len;
        let mut bytes = src.contents();
        bytes[(at + len / 2) as usize] ^= 0x40;
        let damaged = SimStorage::from_bytes(bytes.clone());

        let mut dir = SimDir::new();
        let e = run(p, &damaged, &mut dir, 4, &Job::new().ctx()).unwrap_err();
        assert!(
            matches!(
                e.code,
                ErrorCode::StoredIntegrityFailed
                    | ErrorCode::ContentIntegrityFailed
                    | ErrorCode::ParityDamaged
                    | ErrorCode::RecordInvalid
            ),
            "{}: {:?} {}",
            p.name,
            e.code,
            e.message
        );
        assert!(e.message.contains("Nothing was written"), "{}", e.message);
        assert!(dir.names().is_empty(), "{}: {:?}", p.name, dir.names());
        assert_eq!(damaged.contents(), bytes, "the damaged source is untouched");
    }
}

struct CancelAt {
    phase: &'static str,
    cancel: CancellationToken,
    seen: std::sync::Mutex<Vec<&'static str>>,
}

impl ProgressSink for CancelAt {
    fn report(&self, e: &ProgressEvent) {
        let mut seen = self.seen.lock().unwrap();
        if !seen.contains(&e.phase) {
            seen.push(e.phase);
        }
        if e.phase == self.phase {
            self.cancel.cancel();
        }
    }
}

#[test]
fn cancellation_at_every_phase_leaves_nothing_and_the_source_alone() {
    let p = PROFILES[0];
    let src = draft2_source(p);
    let before = src.contents();
    for phase in [
        mochi_core::migrate::phase::VERIFY_SOURCE,
        mochi_core::migrate::phase::COPY,
        mochi_core::migrate::phase::VERIFY,
    ] {
        let cancel = CancellationToken::new();
        let sink = CancelAt {
            phase,
            cancel: cancel.clone(),
            seen: Default::default(),
        };
        let ctx = JobContext {
            progress: &sink,
            cancel: &cancel,
        };
        let mut dir = SimDir::new();
        let e = run(p, &src, &mut dir, 6, &ctx).unwrap_err();
        assert_eq!(e.code, ErrorCode::Cancelled, "{phase}");
        assert!(dir.names().is_empty(), "{phase}: {:?}", dir.names());
        assert_eq!(src.contents(), before, "{phase}");
    }

    // A run that is not cancelled reports every phase, in order.
    let cancel = CancellationToken::new();
    let sink = CancelAt {
        phase: "never",
        cancel: cancel.clone(),
        seen: Default::default(),
    };
    let ctx = JobContext {
        progress: &sink,
        cancel: &cancel,
    };
    run(p, &src, &mut SimDir::new(), 7, &ctx).unwrap();
    let seen = sink.seen.lock().unwrap().clone();
    let at = |n: &str| seen.iter().position(|x| *x == n).unwrap();
    assert!(at("migrate-verify-source") < at("migrate-copy"));
    assert!(at("migrate-copy") < at("migrate-verify"));
    assert!(at("migrate-verify") < at("publish"));
    assert!(!cancel.is_cancelled());
}

#[test]
fn an_existing_output_is_refused_and_left_as_it_was() {
    let p = PROFILES[0];
    let src = draft2_source(p);
    let mut dir = SimDir::new();
    run(p, &src, &mut dir, 10, &Job::new().ctx()).unwrap();
    let first = dir.file(OUT).unwrap().contents();
    let e = run(p, &src, &mut dir, 11, &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(dir.file(OUT).unwrap().contents(), first);
    assert_eq!(dir.names(), [OUT]);
}

/// The control is scoped: once a draft-2 source is built, archives created on
/// the same thread are ratified again.
#[test]
fn the_legacy_control_is_restored_after_use() {
    let p = PROFILES[0];
    let s = draft2_source(p);
    assert_eq!(
        open_head(&s, &draft_reader(p)).unwrap().descriptor.draft,
        Some(2)
    );
    let t = SimStorage::new();
    let mut w = ArchiveWriter::create(t.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    let mut tx = Transaction::new();
    put(&mut tx, "a", 1, 10, 0o644);
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
    assert_eq!(
        open_head(&t, &read_options(p)).unwrap().descriptor.draft,
        None
    );
}

/// The comparison of §26 step 6 is not decoration: a copy that differs from
/// its source in a namespace, a promised attribute, or the retention state is
/// not published, and says what differed.
#[test]
fn a_copy_that_differs_from_its_source_is_not_published() {
    let p = PROFILES[0];
    let src = draft2_source(p);
    for (tamper, what) in [
        (Tamper::DropLastPath, "namespace"),
        (Tamper::ChangeAttributes, "attributes"),
        (Tamper::DropRetention, "retention"),
    ] {
        let mut dir = SimDir::new();
        let e = migrate(
            &src,
            &mut dir,
            OUT,
            Box::new(SeqIds::new(30)),
            &MigrateOptions {
                tamper: Some(tamper),
                ..migrate_options(p)
            },
            &Job::new().ctx(),
        )
        .unwrap_err();
        assert_eq!(
            e.code,
            ErrorCode::CheckpointMismatch,
            "{tamper:?}: {}",
            e.message
        );
        assert!(
            e.message.contains("does not reproduce its source") && e.message.contains(what),
            "{tamper:?}: {}",
            e.message
        );
        assert!(dir.names().is_empty(), "{tamper:?}");
    }
}

// ---- the checked-in draft-2 archives (R8 `legacy/`) -------------------------------

fn legacy_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/r8")
}

/// The three draft-2 archives kept as migrate cases. Each migrates to an
/// archive whose every snapshot has the content, in the same paths, of the
/// 1.0 R8 case built from the same recipe (`equal_in_content_to`): an oracle
/// that never ran the migrator.
#[test]
fn the_r8_legacy_archives_migrate_to_the_content_of_their_1_0_cases() {
    let cases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(legacy_dir().join("legacy/cases.json")).unwrap())
            .unwrap();
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 3);
    let profiles: Vec<&str> = cases
        .iter()
        .map(|c| c["profile"].as_str().unwrap())
        .collect();
    assert_eq!(
        profiles,
        ["core", "enc", "red"],
        "one per encryption state, one with parity"
    );
    for c in cases {
        let name = c["name"].as_str().unwrap();
        let pass = c["passphrase"].as_str();
        let reader = |drafts| {
            let mut r = match pass {
                Some(p) => keyed_read(&[p]),
                None => ReadOptions::default(),
            };
            r.drafts = drafts;
            r
        };
        let bytes = std::fs::read(
            legacy_dir()
                .join("legacy")
                .join(c["file"].as_str().unwrap()),
        )
        .unwrap();
        let src = SimStorage::from_bytes(bytes.clone());

        // Refused by an ordinary reader, naming the draft and the command.
        let e = open_head(&src, &reader(DraftPolicy::RatifiedOnly)).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature, "{name}");
        assert!(e.message.contains("draft 2") && e.message.contains("mochi migrate"));

        let mut dir = SimDir::new();
        let r = migrate(
            &src,
            &mut dir,
            OUT,
            Box::new(SeqIds::new(77)),
            &MigrateOptions {
                read: reader(DraftPolicy::RatifiedOnly),
                kdf: pass.map(|_| TEST_KDF),
                ..MigrateOptions::default()
            },
            &Job::new().ctx(),
        )
        .unwrap();
        assert_eq!(src.contents(), bytes, "{name}: the source is never written");
        assert_eq!(r.source_draft, 2);
        let out = dir.file(OUT).unwrap();

        let twin = SimStorage::from_bytes(
            std::fs::read(legacy_dir().join(format!(
                "{}.mochi",
                c["equal_in_content_to"].as_str().unwrap()
            )))
            .unwrap(),
        );
        let (ro, rt) = (
            reader(DraftPolicy::RatifiedOnly),
            reader(DraftPolicy::RatifiedOnly),
        );
        let (new, twin_h) = (opened_all(&out, &ro), opened_all(&twin, &rt));
        assert_eq!(new.len(), twin_h.len(), "{name}");
        for (i, (a, b)) in new.iter().zip(&twin_h).enumerate() {
            assert!(a.descriptor.draft.is_none() && b.descriptor.draft.is_none());
            assert_eq!(
                read_state(&out, a).unwrap(),
                read_state(&twin, b).unwrap(),
                "{name}: snapshot {i}"
            );
        }
        let v = verify(
            &out,
            &VerifyOptions {
                read: ro,
                deep: true,
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        );
        assert_eq!(v.report.exit_code, 0, "{name}");
    }
}
