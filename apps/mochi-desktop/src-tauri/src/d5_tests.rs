//! Plan D5 tests: passphrase entry, Encrypted create, rekey, recovery record,
//! and search, end to end through the same Rust objects the commands call
//! (`Archives`, `Checks`, `Services`), against archives written by the
//! production writer. Oracles are independent of the code under test:
//! `mochi_core::verify` run directly, `mochi_testkit`'s model, raw file bytes,
//! and a BLAKE3 of the file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use mochi_core::job::{CancellationToken, NullProgress};
use mochi_core::publish::{open_head, ReadOptions};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::{ErrorCode, Result};
use mochi_testkit::archive::{first_chunk_middle, Job, TEST_KDF};

use crate::browse::{Archives, WRONG_PASSPHRASE};
use crate::check::{Checks, SubjectPick};
use crate::ipc::{
    CheckLevel, CheckResult, CommitReport, EntryKindView, JobEvent, JobOutcome, JobRequest,
    OpenOutcome, PendingId, ProfileView, RekeyAction, ReportView, SearchResultView,
    SearchScopeChoice, Secret, StatusView, SubjectId,
};
use crate::jobs::{EventSink, JobRegistry};
use crate::write::{JobDone, Services};

const PASS: &str = "correct horse battery staple 7731";
const OTHER: &str = "a second, different passphrase 5521";
const WRONG: &str = "definitely not the passphrase 9904";

/// A cheap Argon2id cost for every archive these tests create (the product
/// default, 64 MiB × 3, would spend seconds per command).
fn cheap_kdf() {
    mochi_core::publish::test_controls::set_default_kdf(Some(TEST_KDF));
}

struct Fx {
    dir: tempfile::TempDir,
    svc: Services,
}

fn fx() -> Fx {
    cheap_kdf();
    Fx {
        dir: tempfile::tempdir().unwrap(),
        svc: Services::default(),
    }
}

impl Fx {
    fn at(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Source files under `src/`, as paths to stage.
    fn sources(&self, files: &[(&str, &[u8])]) -> Vec<PathBuf> {
        files
            .iter()
            .map(|(name, bytes)| {
                let p = self.at("src").join(name);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, bytes).unwrap();
                p
            })
            .collect()
    }

    fn run(&self, request: &JobRequest) -> Result<JobDone> {
        self.svc.validate(request)?;
        let job = Job::new();
        self.svc.run(request, &job.ctx()).expect("a routed request")
    }

    /// Create `name`, then close the handle the job left open.
    fn create(
        &self,
        name: &str,
        files: &[(&str, &[u8])],
        passphrase: Option<&str>,
        recovery: Option<u16>,
    ) -> Result<(PathBuf, CommitReport)> {
        let (path, mut report) = self.create_keep(name, files, passphrase, recovery)?;
        if let Some(a) = report.archive.take() {
            self.svc.archives.close(a.handle);
        }
        Ok((path, report))
    }

    /// Create `name` through the production create job; the new archive
    /// stays open.
    fn create_keep(
        &self,
        name: &str,
        files: &[(&str, &[u8])],
        passphrase: Option<&str>,
        recovery: Option<u16>,
    ) -> Result<(PathBuf, CommitReport)> {
        let paths = self.sources(files);
        self.svc.staging.clear();
        self.svc.staging.add(&paths);
        let dest = self.at(name);
        self.svc.staging.set_destination(dest.clone());
        let done = self.run(&JobRequest::CreateArchive {
            preset: crate::ipc::CompressionPreset::Balanced,
            tar_compatible: false,
            passphrase: passphrase.map(Secret::new),
            recovery_percent: recovery,
        })?;
        Ok((dest, *done.commit.unwrap()))
    }

    fn pend(&self, path: &Path) -> PendingId {
        match self.svc.archives.open_or_pend(path).unwrap() {
            OpenOutcome::NeedsPassphrase { pending, .. } => pending,
            other => panic!("expected a passphrase prompt, got {other:?}"),
        }
    }

    /// Open an Encrypted archive with its passphrase.
    fn open_with(&self, path: &Path, pass: &str) -> crate::ipc::ArchiveSummary {
        let pending = self.pend(path);
        self.svc
            .archives
            .unlock(pending, &Secret::new(pass))
            .unwrap()
    }

    fn names(&self, handle: u32, seq: u64) -> Vec<String> {
        self.svc
            .archives
            .children(handle, seq, None, false, 0, 100)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name.text)
            .collect()
    }
}

fn file_hash(p: &Path) -> String {
    blake3::hash(&std::fs::read(p).unwrap())
        .to_hex()
        .to_string()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Every passphrase must be absent from the text that can reach a log, an
/// error, or the screen.
fn assert_no_secret(what: &str, text: &str) {
    for secret in [PASS, OTHER, WRONG] {
        assert!(
            !text.contains(secret),
            "{what} contains a passphrase: {text}"
        );
    }
}

fn verify_overall(path: &Path, keys: Option<&str>) -> (StatusView, u8) {
    let checks = Checks::default();
    let archives = Archives::default();
    let id = checks
        .choose_subject(&archives, SubjectPick::Path(path.to_path_buf()))
        .unwrap()
        .id;
    if let Some(p) = keys {
        checks.unlock_subject(id, &Secret::new(p)).unwrap();
    }
    let r = test_report(&checks, id, CheckLevel::Restoration);
    (r.overall_status, r.exit_code)
}

fn test_report(checks: &Checks, id: SubjectId, level: CheckLevel) -> ReportView {
    let req = JobRequest::TestArchive {
        subject: id,
        level,
        deep: false,
    };
    checks.validate(&req).unwrap();
    match checks
        .run(&req, &Job::new().ctx())
        .unwrap()
        .check
        .map(|c| *c)
    {
        Some(CheckResult::Test { report }) => report,
        other => panic!("{other:?}"),
    }
}

fn dim(r: &ReportView, d: &str) -> StatusView {
    r.dimensions
        .iter()
        .find(|x| x.dimension == d)
        .unwrap()
        .status
}

// ---- the secret type ----------------------------------------------------------

#[test]
fn a_secret_and_every_request_holding_one_print_redacted() {
    let s = Secret::new(PASS);
    assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
    assert_eq!(format!("{s:#?}"), "Secret(<redacted>)");
    let requests = [
        JobRequest::CreateArchive {
            preset: crate::ipc::CompressionPreset::Fast,
            tar_compatible: false,
            passphrase: Some(Secret::new(PASS)),
            recovery_percent: Some(10),
        },
        JobRequest::AddPassphrase {
            handle: 1,
            passphrase: Secret::new(OTHER),
        },
        JobRequest::Reencrypt {
            handle: 1,
            passphrase: Secret::new(WRONG),
        },
    ];
    for r in &requests {
        assert_no_secret("a request's Debug text", &format!("{r:?}"));
        assert_no_secret("a request's pretty Debug text", &format!("{r:#?}"));
    }
}

#[test]
fn a_passphrase_arrives_as_json_into_a_secret_and_an_invalid_one_is_refused_without_echo() {
    let req: JobRequest = serde_json::from_str(&format!(
        r#"{{"kind":"add_passphrase","handle":2,"passphrase":"{OTHER}"}}"#
    ))
    .unwrap();
    assert_no_secret("the parsed request", &format!("{req:?}"));
    let JobRequest::AddPassphrase { passphrase, .. } = &req else {
        panic!("{req:?}");
    };
    assert!(passphrase.passphrase().is_ok());

    // An empty passphrase, and one past 4,096 bytes, are refused; the error
    // names the rule and never the text.
    let empty = Secret::new("");
    let e = empty.passphrase().unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    let long = "p".repeat(5000);
    let e = Secret::new(&long).passphrase().unwrap_err();
    assert!(!e.message.contains("pppppppp"), "{e:?}");
}

// ---- open, browse: the prompt and the wrong passphrase ---------------------------

#[test]
fn an_encrypted_archive_asks_for_a_passphrase_and_a_core_archive_does_not() {
    let f = fx();
    let (core, _) = f
        .create("core.mochi", &[("a.txt", b"plain")], None, None)
        .unwrap();
    match f.svc.archives.open_or_pend(&core).unwrap() {
        OpenOutcome::Opened { archive } => {
            assert!(!archive.profile.encrypted);
            f.svc.archives.close(archive.handle);
        }
        other => panic!("{other:?}"),
    }
    let (enc, _) = f
        .create(
            "enc.mochi",
            &[("secret-name.txt", b"secret content")],
            Some(PASS),
            None,
        )
        .unwrap();
    match f.svc.archives.open_or_pend(&enc).unwrap() {
        OpenOutcome::NeedsPassphrase { file_name, .. } => assert_eq!(file_name, "enc.mochi"),
        other => panic!("{other:?}"),
    }
    // Nothing was opened while it waits.
    assert_eq!(f.svc.archives.open_count(), 0);
    assert_eq!(f.svc.archives.pending_count(), 1);
}

#[test]
fn the_right_passphrase_opens_and_browses_and_the_file_hides_names_and_content() {
    let f = fx();
    let (path, report) = f
        .create_keep(
            "vault.mochi",
            &[("secret-name.txt", b"very secret content xyzzy")],
            Some(PASS),
            None,
        )
        .unwrap();
    assert_eq!(report.status, "LOCAL_COMMITTED");
    // Created encrypted, and the new archive was opened with the same key
    // session: no second prompt after a create.
    let created = report.archive.expect("opened after the create");
    assert!(created.profile.encrypted);
    assert_eq!(f.names(created.handle, 0), ["secret-name.txt"]);
    f.svc.archives.close(created.handle);

    // The sealed file shows neither the name, the content, nor the passphrase.
    let bytes = std::fs::read(&path).unwrap();
    assert!(!contains(&bytes, b"secret-name"));
    assert!(!contains(&bytes, b"xyzzy"));
    assert!(!contains(&bytes, PASS.as_bytes()));

    let summary = f.open_with(&path, PASS);
    assert!(summary.profile.encrypted);
    assert_eq!(f.names(summary.handle, 0), ["secret-name.txt"]);
    // The pending entry is gone once it opened.
    assert_eq!(f.svc.archives.pending_count(), 0);
}

#[test]
fn a_wrong_passphrase_is_key_unavailable_never_damage_and_the_archive_keeps_waiting() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let before = file_hash(&path);
    let pending = f.pend(&path);
    for attempt in 0..2 {
        let e = f
            .svc
            .archives
            .unlock(pending, &Secret::new(WRONG))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::KeyUnavailable, "attempt {attempt}");
        assert_eq!(e.message, WRONG_PASSPHRASE);
        assert_eq!(WRONG_PASSPHRASE, "the passphrase did not open this archive");
        let shown = format!("{e:?} {}", e.message);
        assert_no_secret("the error", &shown);
        for word in ["damage", "corrupt", "invalid", "tamper"] {
            assert!(!shown.to_lowercase().contains(word), "{shown}");
        }
        assert_eq!(f.svc.archives.open_count(), 0);
        assert_eq!(f.svc.archives.pending_count(), 1, "still waiting");
    }
    // Then the right one still works, and the file never changed.
    let ok = f.svc.archives.unlock(pending, &Secret::new(PASS)).unwrap();
    assert!(ok.profile.encrypted);
    assert_eq!(file_hash(&path), before);
    // Used up: a second unlock of the same wait is refused.
    assert_eq!(
        f.svc
            .archives
            .unlock(pending, &Secret::new(PASS))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn cancelling_the_prompt_forgets_the_archive_and_the_wait_is_bounded() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let p = f.pend(&path);
    assert!(f.svc.archives.cancel_pending(p));
    assert!(!f.svc.archives.cancel_pending(p));
    assert_eq!(f.svc.archives.pending_count(), 0);
    let e = f.svc.archives.unlock(p, &Secret::new(PASS)).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);

    let first = f.pend(&path);
    for _ in 0..crate::browse::MAX_PENDING {
        f.pend(&path);
    }
    assert_eq!(f.svc.archives.pending_count(), crate::browse::MAX_PENDING);
    // The oldest wait was dropped to make room.
    assert!(!f.svc.archives.cancel_pending(first));
}

#[test]
fn only_a_wrong_passphrase_keeps_the_wait_any_other_outcome_ends_it() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let pending = f.pend(&path);
    std::fs::remove_file(&path).unwrap();
    let e = f
        .svc
        .archives
        .unlock(pending, &Secret::new(PASS))
        .unwrap_err();
    assert_ne!(e.code, ErrorCode::KeyUnavailable, "{e:?}");
    assert_eq!(
        f.svc.archives.pending_count(),
        0,
        "the wait ended with the failure"
    );
}

// ---- create ----------------------------------------------------------------------

#[test]
fn create_refuses_what_cannot_work_before_creating_anything() {
    let f = fx();
    let paths = f.sources(&[("a.txt", b"x")]);
    f.svc.staging.add(&paths);
    let dest = f.at("never.mochi");
    f.svc.staging.set_destination(dest.clone());
    let req = |tar: bool, pass: Option<&str>, rr: Option<u16>| JobRequest::CreateArchive {
        preset: crate::ipc::CompressionPreset::Balanced,
        tar_compatible: tar,
        passphrase: pass.map(Secret::new),
        recovery_percent: rr,
    };
    // Encrypted + TAR-compatible: sealed objects are skipped by generic tools.
    let e = f.svc.validate(&req(true, Some(PASS), None)).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_no_secret("the error", &e.message);
    // An empty passphrase.
    assert!(f.svc.validate(&req(false, Some(""), None)).is_err());
    // Recovery percentages outside 1..=100.
    for bad in [0u16, 101, 1000] {
        assert!(
            f.svc.validate(&req(false, None, Some(bad))).is_err(),
            "{bad}"
        );
    }
    for good in [1u16, 10, 100] {
        assert!(
            f.svc.validate(&req(false, None, Some(good))).is_ok(),
            "{good}"
        );
    }
    assert!(!dest.exists(), "nothing was created");
}

#[test]
fn an_existing_destination_is_never_replaced_and_leaks_no_passphrase() {
    let f = fx();
    let paths = f.sources(&[("a.txt", b"x")]);
    f.svc.staging.add(&paths);
    let dest = f.at("exists.mochi");
    std::fs::write(&dest, b"not an archive").unwrap();
    f.svc.staging.set_destination(dest.clone());
    let e = f
        .run(&JobRequest::CreateArchive {
            preset: crate::ipc::CompressionPreset::Balanced,
            tar_compatible: false,
            passphrase: Some(Secret::new(PASS)),
            recovery_percent: None,
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_no_secret("the error", &format!("{e:?}"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"not an archive");
}

#[test]
fn add_and_remove_work_on_an_unlocked_encrypted_archive_and_keep_it_sealed() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("one.txt", b"first")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let extra = f.sources(&[("two-secret.txt", b"second thing")]);
    f.svc.staging.clear();
    f.svc.staging.add(&extra);
    let added = f
        .run(&JobRequest::AddToArchive { handle: a.handle })
        .unwrap()
        .commit
        .unwrap();
    assert_eq!(added.seq, 1);
    let reopened = added.archive.unwrap();
    assert!(reopened.profile.encrypted);
    assert_eq!(f.names(a.handle, 1), ["one.txt", "two-secret.txt"]);

    let key = f
        .svc
        .archives
        .children(a.handle, 1, None, false, 0, 10)
        .unwrap()
        .entries[0]
        .key
        .clone();
    let removed = f
        .run(&JobRequest::DeleteFromArchive {
            handle: a.handle,
            keys: vec![key],
        })
        .unwrap()
        .commit
        .unwrap();
    assert_eq!(removed.seq, 2);
    assert_eq!(f.names(a.handle, 2), ["two-secret.txt"]);
    // Still sealed, and a real verification with the passphrase passes
    // integrity and key availability.
    let bytes = std::fs::read(&path).unwrap();
    assert!(!contains(&bytes, b"two-secret"));
    assert!(!contains(&bytes, b"second thing"));
    let checks = Checks::default();
    let id = checks
        .choose_subject(&Archives::default(), SubjectPick::Path(path.clone()))
        .unwrap()
        .id;
    checks.unlock_subject(id, &Secret::new(PASS)).unwrap();
    let r = test_report(&checks, id, CheckLevel::Restoration);
    assert_eq!(dim(&r, "integrity"), StatusView::Pass);
    assert_eq!(dim(&r, "key_availability"), StatusView::Pass);
}

// ---- test and repair with the passphrase ----------------------------------------------

#[test]
fn a_subject_asks_for_the_passphrase_and_a_keyed_test_runs_every_level() {
    let f = fx();
    let (path, _) = f
        .create(
            "vault.mochi",
            &[("a.txt", b"hello hello")],
            Some(PASS),
            None,
        )
        .unwrap();
    let checks = Checks::default();
    let archives = Archives::default();
    let s = checks
        .choose_subject(&archives, SubjectPick::Path(path.clone()))
        .unwrap();
    assert!(s.encrypted && !s.unlocked);

    // Keyless: integrity at most, never PASS overall, key availability UNKNOWN.
    let keyless = test_report(&checks, s.id, CheckLevel::Restoration);
    assert!(keyless.keyless);
    assert_eq!(dim(&keyless, "key_availability"), StatusView::Unknown);
    assert_ne!(keyless.overall_status, StatusView::Pass);

    // A wrong passphrase changes nothing and is not a verification result.
    let e = checks
        .unlock_subject(s.id, &Secret::new(WRONG))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::KeyUnavailable);
    assert_eq!(e.message, WRONG_PASSPHRASE);
    assert_no_secret("the error", &format!("{e:?}"));
    let again = test_report(&checks, s.id, CheckLevel::Restoration);
    assert!(again.keyless, "still keyless after a wrong passphrase");

    let unlocked = checks.unlock_subject(s.id, &Secret::new(PASS)).unwrap();
    assert!(unlocked.encrypted && unlocked.unlocked);
    let keyed = test_report(&checks, s.id, CheckLevel::Restoration);
    assert!(!keyed.keyless);
    assert_eq!(dim(&keyed, "integrity"), StatusView::Pass);
    assert_eq!(dim(&keyed, "recoverability"), StatusView::Pass);
    assert_eq!(dim(&keyed, "key_availability"), StatusView::Pass);
    // Freshness has no anchor in the app, so the overall result still is not
    // a PASS: it is never claimed.
    assert_ne!(keyed.overall_status, StatusView::Pass);
    assert_no_secret("the report JSON", &keyed.json);
}

#[test]
fn a_core_archive_has_no_passphrase_to_unlock() {
    let f = fx();
    let (path, _) = f
        .create("core.mochi", &[("a.txt", b"x")], None, None)
        .unwrap();
    let checks = Checks::default();
    let s = checks
        .choose_subject(&Archives::default(), SubjectPick::Path(path))
        .unwrap();
    assert!(!s.encrypted && !s.unlocked);
    let e = checks.unlock_subject(s.id, &Secret::new(PASS)).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // And a stale subject ID is refused.
    assert!(checks.unlock_subject(s.id + 7, &Secret::new(PASS)).is_err());
}

#[test]
fn the_browsers_open_archive_hands_its_key_to_the_checker() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let checks = Checks::default();
    let s = checks
        .choose_subject(&f.svc.archives, SubjectPick::OpenArchive(a.handle))
        .unwrap();
    assert!(
        s.encrypted && s.unlocked,
        "no second prompt for an open archive"
    );
}

#[test]
fn a_repair_of_an_encrypted_archive_needs_the_key_and_writes_a_new_sealed_archive() {
    let f = fx();
    let (path, _) = f
        .create(
            "vault.mochi",
            &[("a.txt", b"alpha alpha"), ("b.txt", b"bravo bravo")],
            Some(PASS),
            None,
        )
        .unwrap();
    let before = file_hash(&path);
    let checks = Checks::default();
    let archives = Archives::default();
    let s = checks
        .choose_subject(&archives, SubjectPick::Path(path.clone()))
        .unwrap();
    let plan_req = JobRequest::PlanRepair {
        subject: s.id,
        salvage: false,
        archive_id: None,
    };
    // Without the key: refused before anything is read.
    let e = checks.run(&plan_req, &Job::new().ctx()).unwrap_err();
    assert_eq!(e.code, ErrorCode::KeyUnavailable);
    assert!(e.message.contains("needs the passphrase"), "{e:?}");

    checks.unlock_subject(s.id, &Secret::new(PASS)).unwrap();
    let plan = match checks
        .run(&plan_req, &Job::new().ctx())
        .unwrap()
        .check
        .map(|c| *c)
    {
        Some(CheckResult::Plan { plan }) => plan,
        other => panic!("{other:?}"),
    };
    let out = f.at("repaired.mochi");
    checks
        .setup(
            crate::ipc::RepairSetupAction::ChooseOutput,
            Some(out.clone()),
        )
        .unwrap();
    let apply = JobRequest::ApplyRepair {
        subject: s.id,
        plan: plan.plan_id,
        accept_retention_loss: false,
    };
    checks.validate(&apply).unwrap();
    let result = match checks
        .run(&apply, &Job::new().ctx())
        .unwrap()
        .check
        .map(|c| *c)
    {
        Some(CheckResult::Repair { result }) => result,
        other => panic!("{other:?}"),
    };
    assert_eq!(result.file_name, "repaired.mochi");
    // The source is untouched; the new archive is sealed and opens with the
    // same passphrase, and not with another.
    assert_eq!(file_hash(&path), before);
    let bytes = std::fs::read(&out).unwrap();
    assert!(!contains(&bytes, b"alpha alpha"));
    let a = f.open_with(&out, PASS);
    assert_eq!(f.names(a.handle, 0), ["a.txt", "b.txt"]);
    let pending = f.pend(&out);
    assert_eq!(
        f.svc
            .archives
            .unlock(pending, &Secret::new(WRONG))
            .unwrap_err()
            .code,
        ErrorCode::KeyUnavailable
    );
}

// ---- recovery record ---------------------------------------------------------------------

#[test]
fn a_recovery_record_is_fixed_at_creation_and_reported_and_survives_appends() {
    let f = fx();
    let big: Vec<u8> = (0..120_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let (path, _) = f
        .create("rr.mochi", &[("big.bin", &big)], None, Some(10))
        .unwrap();
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    assert_eq!(a.profile.redundancy_percent, Some(10));
    // Without one, none.
    let (plain, _) = f
        .create("plain.mochi", &[("big.bin", &big)], None, None)
        .unwrap();
    let p = f.svc.archives.open(&plain, ReadOptions::default()).unwrap();
    assert_eq!(p.profile.redundancy_percent, None);
    // The record costs space (about the percentage), measured on the files.
    let with = std::fs::metadata(&path).unwrap().len();
    let without = std::fs::metadata(&plain).unwrap().len();
    assert!(with > without + without / 20, "{with} vs {without}");

    // An append keeps the record (the percentage is fixed at creation).
    let extra = f.sources(&[("more.bin", &big[..40_000])]);
    f.svc.staging.clear();
    f.svc.staging.add(&extra);
    let added = f
        .run(&JobRequest::AddToArchive { handle: a.handle })
        .unwrap()
        .commit
        .unwrap();
    assert_eq!(
        added.archive.unwrap().profile,
        ProfileView {
            tar_compatible: false,
            encrypted: false,
            redundancy_percent: Some(10)
        }
    );
}

#[test]
fn damage_inside_the_record_is_degraded_not_pass_and_the_source_is_not_edited() {
    let f = fx();
    let big: Vec<u8> = (0..200_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
        .collect();
    let (path, _) = f
        .create("rr.mochi", &[("big.bin", &big)], None, Some(20))
        .unwrap();
    let src = OsReadStorage::open(&path).unwrap();
    let at = first_chunk_middle(
        &open_head(&src, &ReadOptions::default()).unwrap(),
        "big.bin",
    )
    .unwrap();
    drop(src);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[at as usize] ^= 0xff;
    let damaged = f.at("rr-damaged.mochi");
    std::fs::write(&damaged, &bytes).unwrap();
    let before = file_hash(&damaged);

    let checks = Checks::default();
    let s = checks
        .choose_subject(&Archives::default(), SubjectPick::Path(damaged.clone()))
        .unwrap();
    let r = test_report(&checks, s.id, CheckLevel::Restoration);
    // The stored bytes really are damaged, so integrity fails and the whole
    // result is not a pass; parity can rebuild them, so recoverability is
    // DEGRADED, not FAIL (spec §20.4; R6).
    assert_eq!(dim(&r, "integrity"), StatusView::Fail);
    assert_eq!(dim(&r, "recoverability"), StatusView::Degraded);
    assert_ne!(r.overall_status, StatusView::Pass);
    // Verification is read-only.
    assert_eq!(file_hash(&damaged), before);
}

// ---- rekey --------------------------------------------------------------------------------

#[test]
fn the_envelopes_list_needs_no_key_and_marks_the_one_in_use() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let list = crate::rekey::envelopes(&f.svc.archives, a.handle).unwrap();
    assert_eq!(list.envelopes.len(), 1);
    let e = &list.envelopes[0];
    assert_eq!(e.envelope_id.len(), 32);
    assert_eq!(e.written_by_commit, 0);
    assert!(e.opened_with);
    assert!(e.kdf.starts_with("argon2id m="), "{}", e.kdf);
    // A Core archive has none.
    let (core, _) = f
        .create("core.mochi", &[("a.txt", b"x")], None, None)
        .unwrap();
    let c = f.svc.archives.open(&core, ReadOptions::default()).unwrap();
    assert_eq!(
        crate::rekey::envelopes(&f.svc.archives, c.handle)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

fn rekey_report(done: JobDone) -> crate::ipc::RekeyReport {
    *done.rekey.expect("a rekey report")
}

#[test]
fn adding_a_passphrase_is_one_commit_and_both_open_the_archive() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let r = rekey_report(
        f.run(&JobRequest::AddPassphrase {
            handle: a.handle,
            passphrase: Secret::new(OTHER),
        })
        .unwrap(),
    );
    assert_eq!(r.action, RekeyAction::Added);
    assert_eq!(r.seq, Some(1));
    assert_eq!(r.status.as_deref(), Some("LOCAL_COMMITTED"));
    assert!(r.reopen_error.is_none());
    assert_eq!(r.archive.clone().unwrap().commits.len(), 2);
    assert_eq!(
        crate::rekey::envelopes(&f.svc.archives, a.handle)
            .unwrap()
            .envelopes
            .len(),
        2
    );
    // Each opens it, in a fresh prompt.
    for pass in [PASS, OTHER] {
        let again = f.open_with(&path, pass);
        assert_eq!(f.names(again.handle, 1), ["a.txt"]);
    }
    assert_no_secret("the report", &format!("{r:?}"));
}

#[test]
fn removing_a_passphrase_is_not_revocation_and_the_last_one_cannot_go() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    f.run(&JobRequest::AddPassphrase {
        handle: a.handle,
        passphrase: Secret::new(OTHER),
    })
    .unwrap();
    let envelopes = crate::rekey::envelopes(&f.svc.archives, a.handle)
        .unwrap()
        .envelopes;
    let mine = envelopes.iter().find(|e| e.opened_with).unwrap().clone();
    let other = envelopes.iter().find(|e| !e.opened_with).unwrap().clone();
    let id_bytes: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&mine.envelope_id[2 * i..2 * i + 2], 16).unwrap())
        .collect();

    let r = rekey_report(
        f.run(&JobRequest::RemovePassphrase {
            handle: a.handle,
            envelope_id: mine.envelope_id.clone(),
        })
        .unwrap(),
    );
    assert_eq!(r.action, RekeyAction::Removed);
    assert_eq!(r.seq, Some(2));
    // This session already holds the data key, so it may keep browsing; a
    // reopen error, if there is one, is KEY_UNAVAILABLE and never damage.
    if let Some(err) = &r.reopen_error {
        assert_eq!(err.code, "KEY_UNAVAILABLE");
    }
    // Not revocation, checked on the bytes: the removed envelope is still in
    // the file, so anyone who kept a copy still holds it.
    assert!(contains(&std::fs::read(&path).unwrap(), &id_bytes));
    // The remaining passphrase opens it; the removed one does not (policy).
    let ok = f.open_with(&path, OTHER);
    assert_eq!(f.names(ok.handle, 2), ["a.txt"]);
    let pending = f.pend(&path);
    assert_eq!(
        f.svc
            .archives
            .unlock(pending, &Secret::new(PASS))
            .unwrap_err()
            .code,
        ErrorCode::KeyUnavailable
    );

    // The set never becomes empty: nothing is committed.
    let before = file_hash(&path);
    let e = f
        .run(&JobRequest::RemovePassphrase {
            handle: ok.handle,
            envelope_id: other.envelope_id,
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
    assert_eq!(file_hash(&path), before, "a refused removal writes nothing");
}

#[test]
fn rekey_requests_that_cannot_work_are_refused_before_starting() {
    let f = fx();
    let (core, _) = f
        .create("core.mochi", &[("a.txt", b"x")], None, None)
        .unwrap();
    let c = f.svc.archives.open(&core, ReadOptions::default()).unwrap();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let bad = [
        // A Core archive has no envelopes.
        JobRequest::AddPassphrase {
            handle: c.handle,
            passphrase: Secret::new(OTHER),
        },
        // Not a 32-digit envelope ID.
        JobRequest::RemovePassphrase {
            handle: a.handle,
            envelope_id: "../etc/passwd".into(),
        },
        // An empty passphrase.
        JobRequest::AddPassphrase {
            handle: a.handle,
            passphrase: Secret::new(""),
        },
        // No destination chosen for a re-encryption.
        JobRequest::Reencrypt {
            handle: a.handle,
            passphrase: Secret::new(OTHER),
        },
        // A handle that is not open.
        JobRequest::AddPassphrase {
            handle: 9999,
            passphrase: Secret::new(OTHER),
        },
    ];
    for r in &bad {
        let e = f.svc.validate(r).unwrap_err();
        assert_no_secret("the refusal", &format!("{e:?}"));
    }
    // A Core archive is refused for what it is, not for lacking a key.
    assert_eq!(
        f.svc.validate(&bad[0]).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
    // An envelope the head does not list is refused by core at run time.
    let e = f
        .run(&JobRequest::RemovePassphrase {
            handle: a.handle,
            envelope_id: "00".repeat(16),
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

#[test]
fn re_encryption_writes_a_new_file_under_a_new_key_and_leaves_the_source_alone() {
    let f = fx();
    let (path, _) = f
        .create(
            "vault.mochi",
            &[("a.txt", b"alpha content"), ("b.txt", b"bravo content")],
            Some(PASS),
            None,
        )
        .unwrap();
    let a = f.open_with(&path, PASS);
    let extra = f.sources(&[("c.txt", b"charlie")]);
    f.svc.staging.clear();
    f.svc.staging.add(&extra);
    f.run(&JobRequest::AddToArchive { handle: a.handle })
        .unwrap();
    let before = file_hash(&path);
    let old_id = a.archive_id.clone();

    // An existing destination is never replaced.
    let taken = f.at("taken.mochi");
    std::fs::write(&taken, b"mine").unwrap();
    f.svc.staging.set_destination(taken.clone());
    let e = f
        .run(&JobRequest::Reencrypt {
            handle: a.handle,
            passphrase: Secret::new(OTHER),
        })
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::DestinationExists);
    assert_eq!(std::fs::read(&taken).unwrap(), b"mine");

    let new = f.at("fresh.mochi");
    f.svc.staging.set_destination(new.clone());
    let r = rekey_report(
        f.run(&JobRequest::Reencrypt {
            handle: a.handle,
            passphrase: Secret::new(OTHER),
        })
        .unwrap(),
    );
    assert_eq!(r.action, RekeyAction::Reencrypted);
    assert_eq!(r.file_name, "fresh.mochi");
    assert_eq!(r.snapshots, 2, "every snapshot is kept");
    assert_ne!(r.new_archive_id.as_deref(), Some(old_id.as_str()));
    assert_no_secret("the report", &format!("{r:?}"));
    // The source is byte-identical and still opens with its old passphrase.
    assert_eq!(file_hash(&path), before);
    let again = f.open_with(&path, PASS);
    f.svc.archives.close(again.handle);
    // The destination is consumed, so a second run needs a new choice.
    assert!(f.svc.staging.destination().is_none());
    // The new file opens only with the new passphrase and has both snapshots.
    let n = f.open_with(&new, OTHER);
    assert_eq!(f.names(n.handle, 0), ["a.txt", "b.txt"]);
    assert_eq!(f.names(n.handle, 1), ["a.txt", "b.txt", "c.txt"]);
    let pending = f.pend(&new);
    assert_eq!(
        f.svc
            .archives
            .unlock(pending, &Secret::new(PASS))
            .unwrap_err()
            .code,
        ErrorCode::KeyUnavailable
    );
    // It shares no sealed bytes with the source's content and verifies.
    assert!(!contains(&std::fs::read(&new).unwrap(), b"alpha content"));
    assert_ne!(verify_overall(&new, Some(OTHER)).0, StatusView::Pass);
    let checks = Checks::default();
    let id = checks
        .choose_subject(&Archives::default(), SubjectPick::Path(new))
        .unwrap()
        .id;
    checks.unlock_subject(id, &Secret::new(OTHER)).unwrap();
    let rep = test_report(&checks, id, CheckLevel::Restoration);
    assert_eq!(dim(&rep, "integrity"), StatusView::Pass);
}

// ---- search --------------------------------------------------------------------------------

fn search(
    f: &Fx,
    handle: u32,
    query: &str,
    scope: SearchScopeChoice,
    kind: Option<EntryKindView>,
    ignore_case: bool,
) -> Result<SearchResultView> {
    f.run(&JobRequest::SearchArchive {
        handle,
        query: query.to_owned(),
        ignore_case,
        scope,
        entry_kind: kind,
    })
    .map(|d| *d.search.expect("a search result"))
}

#[test]
fn a_search_finds_names_in_the_head_and_states_exactly_what_it_covered() {
    let f = fx();
    let (path, _) = f
        .create(
            "docs.mochi",
            &[
                ("report-2023.txt", b"one"),
                ("Report-draft.txt", b"two"),
                ("notes/todo.txt", b"three"),
            ],
            None,
            None,
        )
        .unwrap();
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    let r = search(&f, a.handle, "report", SearchScopeChoice::Head, None, false).unwrap();
    // Case-sensitive: only the lower-case name; case-insensitive finds both.
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path.text, "report-2023.txt");
    assert_eq!(r.hits[0].seq, 0);
    assert_eq!(r.hits[0].size, "3");
    assert_eq!(r.hits[0].kind, EntryKindView::File);
    let ci = search(&f, a.handle, "report", SearchScopeChoice::Head, None, true).unwrap();
    assert_eq!(ci.total_hits, 2);
    // Files only, and folders only (there are none: inputs are top-level).
    let files = search(
        &f,
        a.handle,
        "",
        SearchScopeChoice::Head,
        Some(EntryKindView::File),
        false,
    )
    .unwrap();
    assert_eq!(files.total_hits, 3);
    let dirs = search(
        &f,
        a.handle,
        "",
        SearchScopeChoice::Head,
        Some(EntryKindView::Directory),
        false,
    )
    .unwrap();
    assert_eq!(dirs.total_hits, 0);

    let c = &r.coverage;
    assert!(c.complete);
    assert_eq!((c.requested_count, c.searched_count), (1, 1));
    assert_eq!(c.searched, [crate::ipc::SeqRange { from: 0, to: 0 }]);
    assert!(c.unavailable.is_empty());
    assert_eq!(c.entries_examined, 3);
    // File content is never searched, and the coverage says so.
    assert!(c.content_not_searched);
}

#[test]
fn search_scopes_cover_every_snapshot_they_name_and_a_commit_after_the_head_is_refused() {
    let f = fx();
    let (path, _) = f
        .create("h.mochi", &[("old-name.txt", b"v1")], None, None)
        .unwrap();
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    // Commit 1 deletes old-name.txt and adds new-name.txt.
    let key = f
        .svc
        .archives
        .children(a.handle, 0, None, false, 0, 10)
        .unwrap()
        .entries[0]
        .key
        .clone();
    f.run(&JobRequest::DeleteFromArchive {
        handle: a.handle,
        keys: vec![key],
    })
    .unwrap();
    let extra = f.sources(&[("new-name.txt", b"v2")]);
    f.svc.staging.clear();
    f.svc.staging.add(&extra);
    f.run(&JobRequest::AddToArchive { handle: a.handle })
        .unwrap();

    let head = search(&f, a.handle, "name", SearchScopeChoice::Head, None, false).unwrap();
    assert_eq!(
        head.hits
            .iter()
            .map(|h| h.path.text.as_str())
            .collect::<Vec<_>>(),
        ["new-name.txt"]
    );
    // The earlier snapshot still shows the deleted name: deletion removed it
    // from the head only.
    let all = search(&f, a.handle, "name", SearchScopeChoice::All, None, false).unwrap();
    assert_eq!(all.coverage.requested_count, 3);
    assert_eq!(
        all.coverage.searched,
        [crate::ipc::SeqRange { from: 0, to: 2 }]
    );
    assert!(all.coverage.complete);
    assert!(all
        .hits
        .iter()
        .any(|h| h.seq == 0 && h.path.text == "old-name.txt"));
    assert!(all
        .hits
        .iter()
        .any(|h| h.seq == 2 && h.path.text == "new-name.txt"));
    let one = search(
        &f,
        a.handle,
        "name",
        SearchScopeChoice::Commit { seq: 0 },
        None,
        false,
    )
    .unwrap();
    assert_eq!(one.total_hits, 1);
    assert_eq!(one.hits[0].path.text, "old-name.txt");
    assert!(search(
        &f,
        a.handle,
        "x",
        SearchScopeChoice::Commit { seq: 99 },
        None,
        false
    )
    .is_err());
    let retained = search(
        &f,
        a.handle,
        "name",
        SearchScopeChoice::Retained,
        None,
        false,
    )
    .unwrap();
    assert!(retained.coverage.requested_count >= 1);
}

#[test]
fn hostile_names_in_hits_cross_as_escaped_text() {
    let f = fx();
    // Written straight through the archive writer: a file system (Windows)
    // may refuse such names, an archive may carry them.
    let path = f.at("evil.mochi");
    {
        use mochi_core::catalog::path::ArchivePath;
        use mochi_core::manifest::Attributes;
        use mochi_core::publish::{ArchiveWriter, Transaction};
        use mochi_core::storage::os::OsStorage;
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(mochi_testkit::SeqIds::new(9)),
            mochi_testkit::archive::test_options(),
        )
        .unwrap();
        let mut tx = Transaction::new();
        for name in [
            &b"<img src=x onerror=alert(1)>.txt"[..],
            &b"tab\there.txt"[..],
        ] {
            tx.put_file(
                ArchivePath::from_stored(name).unwrap(),
                b"a".to_vec(),
                Attributes::default(),
            );
        }
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
    }
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    let r = search(
        &f,
        a.handle,
        "",
        SearchScopeChoice::Head,
        Some(EntryKindView::File),
        false,
    )
    .unwrap();
    let tab = r
        .hits
        .iter()
        .find(|h| h.path.text.starts_with("tab"))
        .unwrap();
    assert!(
        !tab.path.verbatim && tab.path.text.contains("\\u{9}"),
        "{:?}",
        tab.path
    );
    // The markup name is plain text; the UI never builds HTML from it.
    assert!(r
        .hits
        .iter()
        .any(|h| h.path.text == "<img src=x onerror=alert(1)>.txt"));
}

#[test]
fn a_hit_carries_the_key_of_its_entry_and_of_its_folder() {
    use mochi_core::catalog::path::ArchivePath;
    use mochi_core::manifest::Attributes;
    use mochi_core::publish::{ArchiveWriter, Transaction};
    use mochi_core::storage::os::OsStorage;
    let f = fx();
    let path = f.at("nested.mochi");
    {
        let mut w = ArchiveWriter::create(
            OsStorage::create_new(&path).unwrap(),
            Box::new(mochi_testkit::SeqIds::new(5)),
            mochi_testkit::archive::test_options(),
        )
        .unwrap();
        let mut tx = Transaction::new();
        for dir in ["a", "a/b"] {
            tx.put_dir(
                ArchivePath::from_stored(dir.as_bytes()).unwrap(),
                Attributes::default(),
            );
        }
        for name in ["top-note.txt", "a/b/deep-note.txt", "a/mid-note.txt"] {
            tx.put_file(
                ArchivePath::from_stored(name.as_bytes()).unwrap(),
                b"x".to_vec(),
                Attributes::default(),
            );
        }
        w.commit(tx, &Job::new().ctx()).unwrap();
        w.close().unwrap();
    }
    let hex = |s: &str| -> String { s.bytes().map(|b| format!("{b:02x}")).collect() };
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    let r = search(&f, a.handle, "note", SearchScopeChoice::Head, None, false).unwrap();
    let hit = |name: &str| r.hits.iter().find(|h| h.path.text == name).unwrap();
    let top = hit("top-note.txt");
    assert_eq!(top.key, hex("top-note.txt"));
    assert_eq!(top.parent, None, "a root-level entry has no parent");
    let deep = hit("a/b/deep-note.txt");
    assert_eq!(deep.key, hex("a/b/deep-note.txt"));
    assert_eq!(deep.parent, Some(hex("a/b")));
    assert_eq!(hit("a/mid-note.txt").parent, Some(hex("a")));
    // The folder hits come back too, and a folder's parent is its parent.
    let dirs = search(
        &f,
        a.handle,
        "b",
        SearchScopeChoice::Head,
        Some(EntryKindView::Directory),
        false,
    )
    .unwrap();
    let b = dirs.hits.iter().find(|h| h.path.text == "a/b").unwrap();
    assert_eq!(
        (b.key.as_str(), b.parent.as_deref()),
        (hex("a/b").as_str(), Some(hex("a").as_str()))
    );
    // The key is the one list_children returns, in the hit's own snapshot.
    let page = f
        .svc
        .archives
        .children(a.handle, deep.seq, deep.parent.as_deref(), false, 0, 100)
        .unwrap();
    assert!(page.entries.iter().any(|e| e.key == deep.key));
}

#[test]
fn an_encrypted_archive_is_searched_with_the_key_session_it_was_opened_with() {
    let f = fx();
    let (path, _) = f
        .create(
            "vault.mochi",
            &[("tax-return.pdf", b"x"), ("holiday.jpg", b"y")],
            Some(PASS),
            None,
        )
        .unwrap();
    // Names are sealed in the file, so only an unlocked session can search them.
    assert!(!contains(&std::fs::read(&path).unwrap(), b"tax-return"));
    let a = f.open_with(&path, PASS);
    let r = search(&f, a.handle, "tax", SearchScopeChoice::All, None, false).unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].path.text, "tax-return.pdf");
    assert!(r.coverage.complete);
}

#[test]
fn partial_coverage_is_never_complete_and_lists_what_was_not_searched() {
    use mochi_core::publish::CatalogSource;
    use mochi_core::search::{Coverage, FullText, Unavailable};
    let cov = Coverage {
        indexed_seq: 9,
        catalog_source: CatalogSource::Image,
        requested: vec![0, 1, 2, 3, 4, 7, 8, 9],
        searched: vec![0, 1, 2, 4, 7, 8, 9],
        opened_separately: vec![7],
        unavailable: vec![Unavailable {
            seq: 3,
            error: mochi_core::MochiError::new(
                ErrorCode::CatalogInvalid,
                "commit 3 <b>damaged</b>",
            ),
        }],
        entries_examined: 70,
        pending: 0,
        failed: 0,
        unsupported: 0,
        excluded: 0,
        full_text: FullText::NotBuilt,
    };
    let v = crate::search::result_view(
        SearchScopeChoice::All,
        &mochi_core::search::SearchResult {
            scope: mochi_core::search::SnapshotScope::All,
            hits: Vec::new(),
            coverage: cov,
        },
    );
    let c = v.coverage;
    assert!(!c.complete, "a snapshot was not searched");
    assert_eq!((c.requested_count, c.searched_count), (8, 7));
    assert_eq!(
        c.requested,
        [
            crate::ipc::SeqRange { from: 0, to: 4 },
            crate::ipc::SeqRange { from: 7, to: 9 }
        ]
    );
    assert_eq!(c.searched.len(), 3);
    assert_eq!(
        c.opened_separately,
        [crate::ipc::SeqRange { from: 7, to: 7 }]
    );
    assert_eq!(c.unavailable.len(), 1);
    assert_eq!(c.unavailable[0].seq, 3);
    assert_eq!(c.unavailable[0].code, "CATALOG_INVALID");
    assert_eq!(c.unavailable[0].message.text, "commit 3 <b>damaged</b>");
    assert!(c.content_not_searched);
}

#[test]
fn hits_are_capped_and_the_total_still_counts_every_one() {
    use mochi_core::catalog::namespace::{EntryKind, FileVersionId};
    use mochi_core::publish::CatalogSource;
    use mochi_core::search::{Coverage, FullText, Hit, SearchResult, SnapshotScope};
    let n = crate::search::MAX_HITS_SHOWN + 25;
    let hits: Vec<Hit> = (0..n)
        .map(|i| Hit {
            seq: 0,
            path: mochi_testkit::archive::path(&format!("f{i:05}.txt")),
            kind: EntryKind::File,
            version: FileVersionId::from_bytes([0; 32]),
            logical_len: 1,
            content_hash: None,
        })
        .collect();
    let v = crate::search::result_view(
        SearchScopeChoice::Head,
        &SearchResult {
            scope: SnapshotScope::Head,
            hits,
            coverage: Coverage {
                indexed_seq: 0,
                catalog_source: CatalogSource::Image,
                requested: vec![0],
                searched: vec![0],
                opened_separately: vec![],
                unavailable: vec![],
                entries_examined: n as u64,
                pending: 0,
                failed: 0,
                unsupported: 0,
                excluded: 0,
                full_text: FullText::NotBuilt,
            },
        },
    );
    assert_eq!(v.hits.len(), crate::search::MAX_HITS_SHOWN);
    assert_eq!(v.total_hits, n as u64);
    assert_eq!(v.hits[0].path.text, "f00000.txt");
}

#[test]
fn ranges_collapse_consecutive_sequence_numbers() {
    use crate::ipc::SeqRange as R;
    assert!(crate::search::ranges(&[]).is_empty());
    assert_eq!(crate::search::ranges(&[4]), [R { from: 4, to: 4 }]);
    assert_eq!(
        crate::search::ranges(&[0, 1, 2, 5, 7, 8]),
        [
            R { from: 0, to: 2 },
            R { from: 5, to: 5 },
            R { from: 7, to: 8 }
        ]
    );
}

#[test]
fn an_oversized_query_and_a_closed_archive_are_refused() {
    let f = fx();
    let (path, _) = f.create("a.mochi", &[("a.txt", b"x")], None, None).unwrap();
    let a = f.svc.archives.open(&path, ReadOptions::default()).unwrap();
    let long = "q".repeat(crate::search::MAX_QUERY_BYTES + 1);
    assert!(f
        .svc
        .validate(&JobRequest::SearchArchive {
            handle: a.handle,
            query: long,
            ignore_case: false,
            scope: SearchScopeChoice::Head,
            entry_kind: None,
        })
        .is_err());
    f.svc.archives.close(a.handle);
    assert!(f
        .svc
        .validate(&JobRequest::SearchArchive {
            handle: a.handle,
            query: "a".into(),
            ignore_case: false,
            scope: SearchScopeChoice::Head,
            entry_kind: None,
        })
        .is_err());
}

// ---- the job runner: events and failures never carry a passphrase -----------------------------

struct Collect(std::sync::Mutex<Vec<JobEvent>>);

impl EventSink for Collect {
    fn send(&self, e: JobEvent) -> bool {
        self.0.lock().unwrap().push(e);
        true
    }
}

#[test]
fn job_events_for_a_wrong_target_never_carry_the_passphrase() {
    let f = fx();
    let (path, _) = f
        .create("vault.mochi", &[("a.txt", b"x")], Some(PASS), None)
        .unwrap();
    let a = f.open_with(&path, PASS);
    let registry = std::sync::Arc::new(JobRegistry::with_services(f.svc.clone()));
    let sink = std::sync::Arc::new(Collect(Default::default()));
    // Re-encrypt with no destination chosen: refused at start, nothing runs.
    let err = registry
        .start(
            JobRequest::Reencrypt {
                handle: a.handle,
                passphrase: Secret::new(WRONG),
            },
            sink.clone(),
            Box::new(|work| work()),
        )
        .unwrap_err();
    assert_no_secret("the start error", &format!("{err:?}"));
    // A job that fails while running reports a code and message without it.
    f.svc.staging.set_destination(path.clone());
    let id = registry
        .start(
            JobRequest::Reencrypt {
                handle: a.handle,
                passphrase: Secret::new(WRONG),
            },
            sink.clone(),
            Box::new(|work| work()),
        )
        .unwrap();
    let events = sink.0.lock().unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        JobEvent::Finished { job_id, outcome: JobOutcome::Failed { code, .. } }
            if *job_id == id && code == "DESTINATION_EXISTS"
    )));
    for e in events.iter() {
        assert_no_secret("a job event", &serde_json::to_string(e).unwrap());
    }
    drop(events);
    let _ = (NullProgress, CancellationToken::new());
}
