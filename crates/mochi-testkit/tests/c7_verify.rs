//! C7 verification (plan C7; spec §20, §5.4, §5.7; Annex B.1 D8, B.2 D15).
//!
//! Exit criteria covered here: read-only proven by a before/after hash;
//! per-dimension results at every level; the fault-matrix rows *corrupted
//! content object* (detection) and *older valid archive substituted*
//! (freshness never `PASS` without an anchor, `FAIL` with one).

use mochi_core::exit;
use mochi_core::job::{CancellationToken, JobContext, NullProgress};
use mochi_core::publish::{commit_history, open_head, ArchiveWriter, ReadOptions};
use mochi_core::report::{FreshnessAnchorKind, Report};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::verify::{verify, FreshnessAnchor, Verification, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::footer::FOOTER_FRAME_LEN;
use mochi_testkit::archive::{build, scripted_history, test_options, Job};
use mochi_testkit::{Op, SeqIds, SimStorage};

const LEVELS: [VerificationLevel; 5] = [
    VerificationLevel::Structural,
    VerificationLevel::Referential,
    VerificationLevel::StoredIntegrity,
    VerificationLevel::ContentIntegrity,
    VerificationLevel::Restoration,
];

/// The scripted three-commit history, every commit a checkpoint.
fn archive() -> SimStorage {
    let s = SimStorage::new();
    build(s.clone(), 7, &scripted_history()).unwrap();
    s
}

/// The same history written with the production checkpoint trigger, so
/// commits 1 and 2 are deltas replayed onto commit 0.
fn delta_archive() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(9)), test_options()).unwrap();
    let job = Job::new();
    for step in scripted_history() {
        w.commit(step.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn run(s: &SimStorage, opts: VerifyOptions) -> Verification {
    let v = verify(s, &opts, &Job::new().ctx());
    v.report
        .validate()
        .unwrap_or_else(|e| panic!("report breaks the invariants: {e:?}"));
    v
}

fn at(level: VerificationLevel) -> VerifyOptions {
    VerifyOptions {
        level,
        ..VerifyOptions::default()
    }
}

fn dim(r: &Report, d: Dimension) -> Status {
    r.dimensions[&d]
}

fn has(r: &Report, code: ErrorCode) -> bool {
    r.findings.iter().any(|f| f.code == code)
}

/// Offset and length of one data object of the head catalog.
fn some_data_object(s: &SimStorage) -> (u64, u64) {
    let head = open_head(s, &ReadOptions::default()).unwrap();
    let id = head.catalog.object_ids().unwrap()[0];
    let rec = head.catalog.object(&id).unwrap().unwrap();
    (
        head.catalog.object_location(&id).unwrap().unwrap(),
        rec.stored_len,
    )
}

fn flipped(s: &SimStorage, offset: u64) -> SimStorage {
    let mut b = s.contents();
    b[offset as usize] ^= 0x40;
    SimStorage::from_bytes(b)
}

/// **C7 levels.** A healthy archive at every level: integrity and
/// recoverability `PASS` only where stored bytes were read; below that they
/// are `UNKNOWN` (exit 2), never `PASS`. Freshness without an anchor is
/// `UNKNOWN` and not required (first sight: exit 0 is allowed). Unbuilt
/// dimensions say `UNSUPPORTED`, so the evidence rollup is never `PASS`.
#[test]
fn c7_healthy_archive_at_every_level() {
    for s in [archive(), delta_archive()] {
        for level in LEVELS {
            let v = run(&s, at(level));
            let r = &v.report;
            let reads_bytes = !matches!(
                level,
                VerificationLevel::Structural | VerificationLevel::Referential
            );
            let want = if reads_bytes {
                Status::Pass
            } else {
                Status::Unknown
            };
            assert_eq!(dim(r, Dimension::Integrity), want, "{level:?}");
            assert_eq!(dim(r, Dimension::Recoverability), want, "{level:?}");
            assert_eq!(dim(r, Dimension::Freshness), Status::Unknown);
            assert_eq!(dim(r, Dimension::Durability), Status::Unknown);
            assert_eq!(dim(r, Dimension::KeyAvailability), Status::Pass);
            assert_eq!(dim(r, Dimension::Searchability), Status::Unsupported);
            assert_eq!(dim(r, Dimension::RetentionCompliance), Status::Unsupported);
            assert_eq!(r.overall_status, Status::Unsupported);
            assert_eq!(
                r.exit_code,
                if reads_bytes {
                    exit::OK
                } else {
                    exit::DEGRADED
                },
                "{level:?}"
            );
            assert!(!r.operational_error);
            assert_eq!(r.freshness_anchor, FreshnessAnchorKind::None);
            assert!(r.started_at.is_some() && r.completed_at.is_some());
            let head = v.head.unwrap();
            assert_eq!(head.seq, 2);
            assert_eq!(r.checked_commit, Some(head.commit_id.to_hex()));
            assert_eq!(r.archive_id, Some(head.archive_id.to_hex()));
            if reads_bytes {
                let c = &r.coverage;
                assert!(c.expected_objects.unwrap() > 0);
                assert_eq!(c.checked_objects, c.expected_objects);
                assert_eq!(c.checked_bytes, c.expected_bytes);
            }
            assert!(v.head_is_anchorable());
        }
    }
}

/// **C7 read-only (§20.2).** Verification at every level, deep included,
/// performs no write, sync, truncation, or lock on the storage, and leaves
/// its bytes identical. On the real filesystem the file hashes the same
/// before and after, through the read-only handle the CLI uses.
#[test]
fn c7_verification_is_read_only() {
    let s = archive();
    let before = s.contents();
    let trace_before = s.trace().len();
    for level in LEVELS {
        for deep in [false, true] {
            run(&s, VerifyOptions { deep, ..at(level) });
        }
    }
    assert_eq!(s.contents(), before);
    let new_ops: Vec<Op> = s.trace()[trace_before..].to_vec();
    assert!(new_ops.is_empty(), "verify mutated storage: {new_ops:?}");

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.mochi");
    std::fs::write(&file, &before).unwrap();
    let hash = || blake3::hash(&std::fs::read(&file).unwrap());
    let h0 = hash();
    let src = OsReadStorage::open(&file).unwrap();
    let v = verify(
        &src,
        &VerifyOptions {
            deep: true,
            ..VerifyOptions::default()
        },
        &Job::new().ctx(),
    );
    assert_eq!(v.report.exit_code, exit::OK);
    drop(src);
    assert_eq!(hash(), h0);
}

/// **Fault-matrix row "corrupted content object" (detection).** One flipped
/// bit in a data object: every level that reads stored bytes reports
/// integrity and recoverability `FAIL`, exit 1, with a stored-integrity
/// finding. Levels that do not read it report `UNKNOWN`, never `PASS`.
#[test]
fn c7_a_damaged_data_object_fails() {
    for s in [archive(), delta_archive()] {
        let (off, len) = some_data_object(&s);
        let bad = flipped(&s, off + len / 2);
        for level in LEVELS {
            let r = run(&bad, at(level)).report;
            match level {
                VerificationLevel::Structural | VerificationLevel::Referential => {
                    assert_eq!(dim(&r, Dimension::Integrity), Status::Unknown, "{level:?}");
                    assert_eq!(r.exit_code, exit::DEGRADED);
                }
                _ => {
                    assert_eq!(dim(&r, Dimension::Integrity), Status::Fail, "{level:?}");
                    assert_eq!(dim(&r, Dimension::Recoverability), Status::Fail);
                    assert_eq!(r.overall_status, Status::Fail);
                    assert_eq!(r.exit_code, exit::FAILED);
                    assert!(has(&r, ErrorCode::StoredIntegrityFailed), "{level:?}");
                    let c = &r.coverage;
                    assert_eq!(c.checked_objects.unwrap() + 1, c.expected_objects.unwrap());
                }
            }
        }
    }
}

/// A flipped bit in the head's catalog image is found at the structural
/// level, through the damage assessment: integrity `FAIL`, and reads
/// continue from the snapshot manifest (recoverability `DEGRADED`, D10.9).
#[test]
fn c7_a_damaged_catalog_image_is_found_structurally() {
    let s = archive();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    let head = history.last().unwrap();
    let mochi_core::commit::Metadata::Checkpoint { image, .. } = head.commit.metadata else {
        panic!("every commit of this archive is a checkpoint");
    };
    let bad = flipped(&s, image.offset + image.stored_len / 2);
    let v = run(&bad, at(VerificationLevel::Restoration));
    let r = &v.report;
    assert_eq!(dim(r, Dimension::Integrity), Status::Fail);
    assert_eq!(dim(r, Dimension::Recoverability), Status::Degraded);
    assert_eq!(r.exit_code, exit::FAILED);
    assert!(has(r, ErrorCode::StoredIntegrityFailed));
    assert!(
        !v.head_is_anchorable(),
        "a failing archive is never an anchor"
    );
}

/// **Freshness with a user anchor (D8).** The head passes; an ancestor
/// passes with a note that the archive advanced; an unknown commit fails.
/// The report names the anchor and the expected head.
#[test]
fn c7_freshness_against_a_user_supplied_head() {
    let s = archive();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    let anchor = |commit_id| VerifyOptions {
        anchor: FreshnessAnchor::User { commit_id },
        ..VerifyOptions::default()
    };
    let r = run(&s, anchor(history[2].commit_id)).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Pass);
    assert_eq!(r.exit_code, exit::OK);
    assert_eq!(r.freshness_anchor, FreshnessAnchorKind::User);
    assert_eq!(r.expected_head, Some(history[2].commit_id.to_hex()));
    assert!(r.policy.required.contains(&Dimension::Freshness));

    let r = run(&s, anchor(history[0].commit_id)).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Pass);
    assert!(has(&r, ErrorCode::FreshnessFailed), "the advance is noted");

    let stranger = mochi_format::digest::CommitId::from_bytes([7; 32]);
    let r = run(&s, anchor(stranger)).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Fail);
    assert_eq!(r.exit_code, exit::FAILED);
}

/// **Fault-matrix row "older valid archive substituted".** The archive cut
/// back to commit 1 is internally valid: with no anchor its freshness is
/// `UNKNOWN`, never `PASS` (§5.7). With the local-history anchor of the
/// full archive (commit 2) it is a rollback, `FAIL`. A different archive at
/// the anchor's sequence is a substitution, `FAIL`. The real archive passes.
#[test]
fn c7_an_older_archive_is_never_fresh() {
    let s = archive();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    let older_len = history[1].footer_offset + FOOTER_FRAME_LEN;
    let older = SimStorage::from_bytes(s.contents()[..older_len as usize].to_vec());

    let r = run(&older, VerifyOptions::default()).report;
    assert_eq!(
        dim(&r, Dimension::Integrity),
        Status::Pass,
        "internally valid"
    );
    assert_eq!(dim(&r, Dimension::Freshness), Status::Unknown);

    let local = |seq, commit_id| VerifyOptions {
        anchor: FreshnessAnchor::LocalHistory { seq, commit_id },
        ..VerifyOptions::default()
    };
    let seen = local(2, history[2].commit_id);
    let r = run(&older, seen.clone()).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Fail);
    assert_eq!(r.exit_code, exit::FAILED);
    assert_eq!(r.freshness_anchor, FreshnessAnchorKind::LocalHistory);

    let other = SimStorage::new();
    build(other.clone(), 99, &scripted_history()).unwrap();
    let r = run(&other, seen.clone()).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Fail, "substituted");

    let r = run(&s, seen).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Pass);
    assert_eq!(r.exit_code, exit::OK);
    // An earlier sighting of the same archive is not a rollback.
    let r = run(&s, local(1, history[1].commit_id)).report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Pass);
}

/// Explicitly requested freshness without an anchor is required and
/// `UNKNOWN`: exit 2, not 0 (D15).
#[test]
fn c7_requested_freshness_without_an_anchor_is_unknown() {
    let r = run(
        &archive(),
        VerifyOptions {
            require_freshness: true,
            ..VerifyOptions::default()
        },
    )
    .report;
    assert_eq!(dim(&r, Dimension::Freshness), Status::Unknown);
    assert_eq!(r.exit_code, exit::DEGRADED);
}

/// Levels 1.0 cannot perform run nothing and exit 4 (§20.1, §5.4).
#[test]
fn c7_unsupported_levels_exit_4() {
    for level in [
        VerificationLevel::Inventory,
        VerificationLevel::Search,
        VerificationLevel::DisasterRecovery,
    ] {
        let v = run(&archive(), at(level));
        assert_eq!(v.report.exit_code, exit::UNSUPPORTED, "{level:?}");
        assert_eq!(dim(&v.report, Dimension::Integrity), Status::Unsupported);
        assert!(v.head.is_none(), "nothing was checked");
    }
}

/// Cancellation is an operational error (exit 3), with the rest skipped,
/// never a pass.
#[test]
fn c7_cancellation_is_an_operational_error() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let progress = NullProgress;
    let ctx = JobContext {
        progress: &progress,
        cancel: &cancel,
    };
    let v = verify(&archive(), &VerifyOptions::default(), &ctx);
    v.report.validate().unwrap();
    assert!(v.report.operational_error);
    assert_eq!(v.report.exit_code, exit::ERROR);
    assert_ne!(dim(&v.report, Dimension::Integrity), Status::Pass);
    assert!(!v.report.skipped.is_empty());
    assert!(!v.head_is_anchorable());
}

/// Not an archive: no valid head is a structural `FAIL` (exit 1), with
/// everything later skipped.
#[test]
fn c7_no_valid_head_fails() {
    let s = SimStorage::from_bytes(b"this is not an archive at all, just text".to_vec());
    let v = run(&s, VerifyOptions::default());
    assert_eq!(dim(&v.report, Dimension::Integrity), Status::Fail);
    assert_eq!(v.report.exit_code, exit::FAILED);
    assert!(has(&v.report, ErrorCode::NoValidHead));
    assert!(!v.report.skipped.is_empty());
}

/// An interrupted append (an eligible tail) is reported but does not fail
/// the committed head; bytes that might be a damaged later commit do.
#[test]
fn c7_tails() {
    let s = archive();
    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    let bytes = s.contents();

    // Commit 2's own bytes without its footer: an interrupted write.
    let cut = history[2].commit_offset as usize + 3;
    let interrupted = SimStorage::from_bytes(bytes[..cut].to_vec());
    let r = run(&interrupted, VerifyOptions::default()).report;
    assert!(has(&r, ErrorCode::UncommittedTail));
    assert_eq!(dim(&r, Dimension::Integrity), Status::Pass);
    assert_eq!(r.exit_code, exit::OK);

    // Garbage after the head: not eligible, possibly a damaged commit.
    let mut junk = bytes.clone();
    junk.extend_from_slice(&[0xAB; 200]);
    let r = run(&SimStorage::from_bytes(junk), VerifyOptions::default()).report;
    assert!(has(&r, ErrorCode::TailUnresolved));
    assert_eq!(dim(&r, Dimension::Integrity), Status::Fail);
    assert_eq!(r.exit_code, exit::FAILED);
}

/// `deep` (fsck) opens every commit and agrees on a healthy archive, of
/// either checkpoint layout.
#[test]
fn c7_deep_checks_pass_on_healthy_archives() {
    for s in [archive(), delta_archive()] {
        let r = run(
            &s,
            VerifyOptions {
                deep: true,
                ..VerifyOptions::default()
            },
        )
        .report;
        assert_eq!(r.exit_code, exit::OK, "{:?}", r.findings);
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }
}

/// The JSON form carries schema v1 and the anchor field.
#[test]
fn c7_report_json_is_schema_v1() {
    let r = run(&archive(), VerifyOptions::default()).report;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["freshness_anchor"], "none");
    assert_eq!(v["level"], "restoration");
    assert_eq!(v["dimensions"]["integrity"], "PASS");
    let back: Report = serde_json::from_value(v).unwrap();
    assert_eq!(back, r);
}

// ---- catalog contents, on catalogs forged with the history writer ----------

use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use mochi_core::catalog::FileVersion;
use mochi_core::manifest::{Attributes, ManifestKind};
use mochi_core::object::{Dependency, ObjectId};
use mochi_core::verify::{check_catalog_contents, Bounds, CatalogCheck};
use mochi_format::digest::file_content_hash;
use mochi_format::repr::DecodedSlice;
use mochi_testkit::archive::path;
use mochi_testkit::history::HistoryWriter;

fn written() -> (HistoryWriter, FileVersionId) {
    let mut w = HistoryWriter::new(5).unwrap();
    let v = w
        .file(b"the real content", 0, Attributes::default())
        .unwrap();
    w.commit(
        vec![NamespaceOp::Put {
            path: path("f"),
            version: v,
        }],
        false,
    )
    .unwrap();
    (w, v)
}

fn contents(w: &HistoryWriter, bounds: Bounds, level: VerificationLevel) -> CatalogCheck {
    check_catalog_contents(
        &SimStorage::from_bytes(w.bytes.clone()),
        &w.catalog,
        bounds,
        level,
        &ReadOptions::default(),
        &Job::new().ctx(),
    )
}

fn whole(w: &HistoryWriter) -> Bounds {
    Bounds {
        commit_offset: w.bytes.len() as u64,
        committed_len: w.bytes.len() as u64,
    }
}

fn refs_invalid(c: &CatalogCheck) -> usize {
    c.findings
        .iter()
        .filter(|f| f.code == ErrorCode::ReferenceInvalid)
        .count()
}

/// **Referential level (§5.1, §5.2).** Each object must be exactly one data
/// frame of its recorded length, end before the commit frame, have a
/// location, and have its dependencies in the catalog. Each defect is
/// `REFERENCE_INVALID`, found without reading the stored bytes' hash.
#[test]
fn c7_referential_defects_are_found() {
    let (w, _) = written();
    let ok = contents(&w, whole(&w), VerificationLevel::Referential);
    assert!(!ok.violation && ok.complete, "{:?}", ok.findings);

    let id = w.catalog.object_ids().unwrap()[0];
    let real = w.catalog.object(&id).unwrap().unwrap();
    let at = w.catalog.object_location(&id).unwrap().unwrap();

    // Ends after the commit frame that references it.
    let early = Bounds {
        commit_offset: at + real.stored_len - 1,
        committed_len: w.bytes.len() as u64,
    };
    let c = contents(&w, early, VerificationLevel::Referential);
    assert!(c.violation);
    assert_eq!(refs_invalid(&c), 1);

    // Forged records: at a manifest frame; with an unknown dependency; with
    // no location; shorter than the frame at its location.
    let (mut w, _) = written();
    let m = *w.manifest(0, ManifestKind::Delta);
    let mut at_manifest = real.clone();
    at_manifest.id = ObjectId::from_bytes([0xA1; 32]);
    at_manifest.stored_len = m.len;
    w.catalog
        .insert_object(&at_manifest, Some(m.offset))
        .unwrap();
    let mut needs_missing = real.clone();
    needs_missing.id = ObjectId::from_bytes([0xA2; 32]);
    needs_missing.dependencies = vec![Dependency::Dictionary(ObjectId::from_bytes([0xEE; 32]))];
    w.catalog.insert_object(&needs_missing, Some(at)).unwrap();
    let mut nowhere = real.clone();
    nowhere.id = ObjectId::from_bytes([0xA3; 32]);
    w.catalog.insert_object(&nowhere, None).unwrap();
    let mut short = real.clone();
    short.id = ObjectId::from_bytes([0xA4; 32]);
    short.stored_len -= 1;
    w.catalog.insert_object(&short, Some(at)).unwrap();
    let c = contents(&w, whole(&w), VerificationLevel::Referential);
    assert!(c.violation);
    assert_eq!(refs_invalid(&c), 4, "{:?}", c.findings);
    assert_eq!(c.coverage.expected_objects, Some(5));
}

/// **Restoration level.** Every retained file version is reassembled, not
/// only the head's: a version no snapshot names, whose intact chunks do not
/// match its file-content hash, passes the content level and fails
/// restoration with `CONTENT_INTEGRITY_FAILED`.
#[test]
fn c7_restoration_reassembles_every_retained_version() {
    let (mut w, good) = written();
    let (_, extents) = w.catalog.file_version(&good).unwrap().unwrap();
    w.catalog
        .insert_file_version(
            &FileVersion {
                id: FileVersionId::from_bytes([0xF0; 32]),
                kind: EntryKind::File,
                logical_len: 16,
                content_hash: Some(file_content_hash(DecodedSlice::from_logical(
                    b"something else!!",
                ))),
            },
            &extents,
        )
        .unwrap();
    let c = contents(&w, whole(&w), VerificationLevel::ContentIntegrity);
    assert!(!c.violation && c.complete, "{:?}", c.findings);
    let c = contents(&w, whole(&w), VerificationLevel::Restoration);
    assert!(c.violation);
    assert!(c
        .findings
        .iter()
        .any(|f| f.code == ErrorCode::ContentIntegrityFailed));
}

/// **R8 golden archives.** Every `reject-archive-*` fixture fails
/// verification (a pre-batch draft is refused as unsupported, §26: exit 4;
/// the rest exit 1) and every `valid-archive-*` fixture passes, with no
/// error finding. Ties verification to the vectors the reader is held to.
#[test]
fn c7_golden_archives_verify_as_labelled() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c5");
    let mut seen = (0, 0);
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".mochi") {
            continue;
        }
        let s = SimStorage::from_bytes(std::fs::read(&path).unwrap());
        let r = run(&s, VerifyOptions::default()).report;
        if name.starts_with("valid-archive-") {
            seen.0 += 1;
            assert_eq!(r.exit_code, exit::OK, "{name}: {:?}", r.findings);
        } else if name.starts_with("reject-archive-") {
            seen.1 += 1;
            // The two frozen archives older than draft 2 are refused, not damaged.
            let want = if name == "reject-archive-legacy-v0.mochi"
                || name == "reject-archive-bare-image.mochi"
            {
                exit::UNSUPPORTED
            } else {
                exit::FAILED
            };
            assert_eq!(r.exit_code, want, "{name}: {:?}", r.findings);
            assert_ne!(dim(&r, Dimension::Integrity), Status::Pass, "{name}");
        }
    }
    assert!(seen.0 >= 3 && seen.1 >= 20, "fixtures found: {seen:?}");
}

// ---- W10 (plan K16, K17) ----------------------------------------------------

/// Archive `archive()` plus a forged commit 3 (an empty delta), with
/// `between` appended after the original head and before commit 3.
fn with_forged_commit(between: &[u8]) -> SimStorage {
    use mochi_testkit::forge::{empty_delta, rule_base, txid, Forge};
    let mut f = Forge::new(archive().contents());
    let head = f.history()[2].clone();
    f.bytes.extend_from_slice(between);
    let m = empty_delta(&head, txid(0x30));
    let r = f.append_manifest(&m);
    f.append_delta(&head, rule_base(&head), r, txid(0x30));
    f.storage()
}

/// **Footer-history frames are skipped, never read as history (plan K16;
/// spec §22 amendment).** 1.0 writes no `FOOTER_HISTORY` accelerator; a reader
/// that meets one walks it and moves on. The injected frame is bait: its
/// payload is a verbatim copy of commit 0's footer frame, so a reader that
/// looked inside would find a footer pointing at commit 0 and lose or
/// duplicate history. Control: the same forged commit without the frame.
/// Verified at every level, deep included (`fsck`).
#[test]
fn c7_footer_history_frames_are_skipped_never_interpreted() {
    use mochi_format::footer::encode_footer_frame;
    use mochi_format::frame::encode_skippable_frame;
    use mochi_format::registry::FrameKind;

    let original = commit_history(&archive(), &ReadOptions::default()).unwrap();
    let c0 = &original[0];
    let c0_frame =
        archive().contents()[c0.commit_offset as usize..(c0.footer_offset as usize)].to_vec();
    let bait_footer = encode_footer_frame(c0.commit_offset, c0.commit.seq, &c0_frame);
    assert_eq!(bait_footer.len() as u64, FOOTER_FRAME_LEN);
    // Both shapes: a whole footer frame inside the payload, and a bare
    // footer payload.
    for payload in [bait_footer.to_vec(), bait_footer[8..].to_vec()] {
        let frame = encode_skippable_frame(FrameKind::FooterHistory, &payload).unwrap();
        let control = with_forged_commit(&[]);
        let s = with_forged_commit(&frame);
        assert_eq!(s.contents().len(), control.contents().len() + frame.len());

        let history = commit_history(&s, &ReadOptions::default()).unwrap();
        assert_eq!(
            history.iter().map(|e| e.commit.seq).collect::<Vec<_>>(),
            [0, 1, 2, 3],
            "the frame is not a commit"
        );
        for (got, want) in history.iter().zip(&original) {
            assert_eq!(got.footer_offset, want.footer_offset);
            assert_eq!(got.commit_id, want.commit_id);
        }

        for level in LEVELS {
            for deep in [false, true] {
                let o = VerifyOptions {
                    level,
                    deep,
                    ..VerifyOptions::default()
                };
                let (a, b) = (run(&s, o.clone()), run(&control, o));
                assert_eq!(a.head.as_ref().unwrap().seq, 3, "{level:?} deep={deep}");
                assert_eq!(a.report.exit_code, b.report.exit_code, "{level:?}");
                assert_eq!(a.report.dimensions, b.report.dimensions, "{level:?}");
                assert_eq!(a.report.coverage, b.report.coverage, "{level:?}");
                assert!(
                    a.report.findings.is_empty(),
                    "{level:?} deep={deep}: {:?}",
                    a.report.findings
                );
            }
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `archive()` plus a forged commit 3 that puts `dep` with one intact chunk
/// depending on `dep` (`mochi_testkit::forge::put_with_dependency`).
fn with_dependency(
    dep: mochi_testkit::forge::DictionaryDependency,
) -> (SimStorage, mochi_core::catalog::namespace::FileVersionId) {
    let (bytes, id) = mochi_testkit::forge::put_with_dependency(archive().contents(), "dep", dep);
    (SimStorage::from_bytes(bytes), id)
}

/// **A dictionary dependency is reported, never skipped (plan K17; spec
/// §24.2 amendment, row "missing shared dictionary").** 1.0 writes no
/// dictionaries (O21), so a chunk that declares one is content this build
/// cannot decode. At the content levels `verify` must say `UNSUPPORTED`
/// (exit 4) with `UNSUPPORTED_FEATURE` naming the affected path, version
/// and snapshot; it must not report the dimensions `PASS` or `FAIL`, and
/// must not call the archive damaged. Control: the same commit without the
/// dependency verifies `PASS` at every level.
#[test]
fn c7_a_dictionary_dependency_is_unsupported_with_the_affected_versions() {
    use mochi_testkit::forge::DictionaryDependency;

    let (control, _) = with_dependency(DictionaryDependency::None);
    for level in LEVELS {
        let r = run(&control, at(level)).report;
        assert!(!has(&r, ErrorCode::UnsupportedFeature), "{level:?}");
    }
    assert_eq!(
        run(&control, at(VerificationLevel::Restoration))
            .report
            .exit_code,
        exit::OK
    );

    let (s, id) = with_dependency(DictionaryDependency::Present);
    for level in [
        VerificationLevel::ContentIntegrity,
        VerificationLevel::Restoration,
    ] {
        let v = run(&s, at(level));
        let r = &v.report;
        assert_eq!(r.exit_code, exit::UNSUPPORTED, "{level:?}");
        assert!(has(r, ErrorCode::UnsupportedFeature), "{level:?}");
        assert_eq!(dim(r, Dimension::Integrity), Status::Unsupported);
        assert_eq!(dim(r, Dimension::Recoverability), Status::Unsupported);
        assert!(
            !r.findings.iter().any(|f| matches!(
                f.code,
                ErrorCode::StoredIntegrityFailed | ErrorCode::ContentIntegrityFailed
            )),
            "undecodable is not damaged: {:?}",
            r.findings
        );
        // The affected version is named once: ID, path, and the snapshot
        // that holds it (only the forged commit 3 does).
        let versions: Vec<_> = r
            .findings
            .iter()
            .filter(|f| f.code == ErrorCode::UnsupportedFeature)
            .filter(|f| {
                f.message
                    .as_deref()
                    .is_some_and(|m| m.starts_with("file version"))
            })
            .collect();
        assert_eq!(versions.len(), 1, "{level:?}: {:?}", r.findings);
        let m = versions[0].message.as_deref().unwrap();
        assert!(m.contains(&hex(id.as_bytes())), "{m}");
        assert!(m.contains("\"dep\"") && m.contains("snapshot 3"), "{m}");
        assert_eq!(
            versions[0].affected,
            Some(mochi_core::report::SeqRange { first: 3, last: 3 })
        );
    }

    // Levels that read no decoded bytes claim nothing about decodability.
    let r = run(&s, at(VerificationLevel::StoredIntegrity)).report;
    assert!(!has(&r, ErrorCode::UnsupportedFeature), "{:?}", r.findings);

    // A dependency on an object the catalog does not hold is a dangling
    // reference (§5.2): a violation, FAIL, exit 1, not a missing feature.
    let (dangling, _) = with_dependency(DictionaryDependency::Missing);
    let r = run(&dangling, at(VerificationLevel::ContentIntegrity)).report;
    assert!(has(&r, ErrorCode::ReferenceInvalid), "{:?}", r.findings);
    assert_eq!(r.exit_code, exit::FAILED);
}

/// The forward scan for the head (taken when EOF is not a footer) walks the
/// same frames, so the accelerator must be skipped there too (plan K16). A
/// trailing frame whose payload is commit 1's footer payload (72 bytes in
/// all, a footer's own size) must not become the head: the real head stays
/// commit 2 and the history is unchanged. The tail screen refuses to call
/// the frame "uncommitted" because it holds the footer magic (D14: refusing
/// is always safe), so the run reports `TAIL_UNRESOLVED`; the head is the
/// point here, not the tail verdict.
#[test]
fn c7_a_trailing_footer_history_frame_is_not_the_head() {
    use mochi_format::footer::encode_footer_frame;
    use mochi_format::frame::encode_skippable_frame;
    use mochi_format::registry::FrameKind;

    let base = archive();
    let original = commit_history(&base, &ReadOptions::default()).unwrap();
    let c1 = &original[1];
    let c1_frame = base.contents()[c1.commit_offset as usize..c1.footer_offset as usize].to_vec();
    let bait = encode_footer_frame(c1.commit_offset, c1.commit.seq, &c1_frame);
    let frame = encode_skippable_frame(FrameKind::FooterHistory, &bait[8..]).unwrap();
    assert_eq!(frame.len() as u64, FOOTER_FRAME_LEN);
    let mut bytes = base.contents();
    bytes.extend_from_slice(&frame);
    let s = SimStorage::from_bytes(bytes);

    let history = commit_history(&s, &ReadOptions::default()).unwrap();
    assert_eq!(
        history.iter().map(|e| e.commit_id).collect::<Vec<_>>(),
        original.iter().map(|e| e.commit_id).collect::<Vec<_>>()
    );
    let v = run(&s, at(VerificationLevel::Restoration));
    assert_eq!(v.head.as_ref().unwrap().seq, 2);
    assert!(has(&v.report, ErrorCode::TailUnresolved));
}
