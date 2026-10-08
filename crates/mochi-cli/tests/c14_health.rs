//! C14 end-to-end tests for `mochi health` (next-work-plan K5), run
//! in-process through `mochi_cli::run` against the real filesystem.
//!
//! The rules of the assessment itself (every status transition, the age
//! boundary, the policy file) are unit-tested in `mochi_core::health` with an
//! injectable clock. These tests pin what the CLI adds: that `verify`,
//! `fsck`, `restore-test`, and `repair apply` record evidence (and
//! `--no-local-history` does not), that `health` reads it for the archive's
//! current head, runs no check, writes nothing, and exits by the report.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use mochi_cli::{exit, run};
use mochi_core::catalog::extent::ExtentSource;
use mochi_core::publish::{open_head, ReadOptions};
use mochi_core::storage::os::OsReadStorage;
use serde_json::Value;

/// A new file with nothing else to report: Windows reports the directory
/// entry as not confirmed durable until gate G6 (plan O12), which is
/// `DEGRADED`.
const CREATED: u8 = if cfg!(windows) {
    exit::DEGRADED
} else {
    exit::OK
};

struct Area {
    dir: tempfile::TempDir,
}

impl Area {
    fn new() -> Self {
        Area {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).display().to_string()
    }

    fn state(&self) -> String {
        self.s("state")
    }

    fn go_in(&self, state: &str, args: &[&str]) -> (u8, String, String) {
        let mut argv = vec!["mochi", "--state-dir", state];
        argv.extend_from_slice(args);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(argv, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    fn go(&self, args: &[&str]) -> (u8, String, String) {
        self.go_in(&self.state(), args)
    }

    fn json(&self, args: &[&str]) -> (u8, Value) {
        let mut a = args.to_vec();
        a.push("--json");
        let (code, out, err) = self.go(&a);
        let v = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {out:?} {err}"));
        (code, v)
    }

    fn file(&self, name: &str, content: &[u8]) -> String {
        fs::create_dir_all(self.p("src")).unwrap();
        let p = self.p(&format!("src/{name}"));
        fs::write(&p, content).unwrap();
        p.display().to_string()
    }

    /// An archive of one commit; with `local` the state directory learns its
    /// head (as `create` does by default).
    fn archive(&self, local: bool) -> String {
        let archive = self.s("a.mochi");
        let f = self.file("a", &[1u8; 70_000]);
        let mut args = vec!["create", archive.as_str(), f.as_str()];
        if !local {
            args.push("--no-local-history");
        }
        let (code, v) = self.json(&args);
        assert_eq!(code, CREATED, "{v}");
        archive
    }

    fn append(&self, archive: &str, name: &str) {
        let f = self.file(name, name.as_bytes());
        let (code, v) = self.json(&["append", archive, &f]);
        assert_eq!(code, exit::OK, "{v}");
    }

    fn policy(&self, body: &str) -> String {
        let p = self.p("policy.json");
        fs::write(&p, body).unwrap();
        p.display().to_string()
    }
}

fn dim<'a>(v: &'a Value, d: &str) -> &'a str {
    v["dimensions"][d].as_str().unwrap()
}

fn skipped_for(v: &Value, item: &str) -> String {
    v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["item"] == item)
        .map(|s| s["reason"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn flip(path: &str, at: u64) {
    let mut b = fs::read(path).unwrap();
    b[at as usize] ^= 0x40;
    fs::write(path, b).unwrap();
}

/// Offset of a byte in the middle of the first chunk of the file named
/// `name` at the head.
fn chunk_byte(archive: &str, name: &str) -> u64 {
    let s = OsReadStorage::open(std::path::Path::new(archive)).unwrap();
    let h = open_head(&s, &ReadOptions::default()).unwrap();
    let snap = h.catalog.replay(None).unwrap();
    let (_, e) = snap
        .iter()
        .find(|(p, _)| p.as_stored().ends_with(name.as_bytes()))
        .unwrap();
    let (_, extents) = h.catalog.file_version(&e.version).unwrap().unwrap();
    let chunk = extents
        .iter()
        .find_map(|x| match x.source {
            ExtentSource::Chunk { chunk, .. } => Some(chunk),
            ExtentSource::Hole => None,
        })
        .unwrap();
    let r = h.catalog.object(&chunk).unwrap().unwrap();
    h.catalog.object_location(&chunk).unwrap().unwrap() + r.stored_len / 2
}

/// Run `health` on `archive` with `extra` arguments.
fn health_of(a: &Area, archive: &str, extra: &[&str]) -> (u8, Value) {
    let mut args = vec!["health", archive];
    args.extend_from_slice(extra);
    a.json(&args)
}

// ---- tests ---------------------------------------------------------------

/// **No evidence.** A machine that has never verified this archive knows
/// nothing about its health: integrity and recoverability are `UNKNOWN`,
/// the exit is 2, the report says why, and nothing is written.
#[test]
fn health_with_no_evidence_is_unknown_and_exits_2() {
    let a = Area::new();
    let archive = a.archive(false);
    let before = fs::read(&archive).unwrap();

    let (code, v) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["level"], "structural");
    assert!(v["scope"]
        .as_str()
        .unwrap()
        .contains("no check of the archive was run"));
    for d in [
        "integrity",
        "recoverability",
        "freshness",
        "durability",
        "key_availability",
    ] {
        assert_eq!(dim(&v, d), "UNKNOWN", "{d}: {v}");
    }
    assert_eq!(dim(&v, "searchability"), "UNSUPPORTED");
    assert_eq!(dim(&v, "retention_compliance"), "UNSUPPORTED");
    assert!(skipped_for(&v, "integrity").contains("no "), "{v}");
    assert_eq!(v["freshness_anchor"], "none");
    assert_eq!(
        v["policy"]["required"],
        serde_json::json!(["integrity", "recoverability"])
    );
    assert_eq!(v["policy_result"], "UNKNOWN");
    assert_ne!(v["overall_status"], "PASS");
    assert!(v["checked_commit"].is_string());

    // Text mode says the same, honestly.
    let (code, out, _) = a.go(&["health", &archive]);
    assert_eq!(code, exit::DEGRADED, "{out}");
    assert!(out.starts_with("health: "), "{out}");
    assert!(out.contains("no check of the archive was run"), "{out}");
    assert!(
        out.contains("integrity") && out.contains("UNKNOWN"),
        "{out}"
    );
    assert!(!out.contains("PASS"), "{out}");

    // It wrote nothing: not the archive, not the state directory.
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!a.p("state").exists(), "health created the state directory");
}

/// **verify, then health; append, then health.** A passing `verify` makes
/// integrity and recoverability `PASS` for that head (exit 0, and an overall
/// status that still is not `PASS`); commits added since make them `UNKNOWN`
/// again, for a stated reason; a new `verify` restores them. `health` never
/// changes anything it reads.
#[test]
fn verify_then_health_passes_and_new_commits_make_it_unknown() {
    let a = Area::new();
    let archive = a.archive(true);
    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");

    let heads = fs::read(a.p("state/heads.json")).unwrap();
    let evidence = fs::read_dir(a.p("state/evidence"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(evidence.extension().unwrap(), "jsonl");
    assert!(evidence
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with(v["archive_id"].as_str().unwrap()));
    let log = fs::read(&evidence).unwrap();
    let bytes = fs::read(&archive).unwrap();

    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK, "{h}");
    assert_eq!(dim(&h, "integrity"), "PASS");
    assert_eq!(dim(&h, "recoverability"), "PASS");
    assert_eq!(dim(&h, "freshness"), "PASS");
    assert_eq!(h["freshness_anchor"], "local-history");
    assert_eq!(h["policy_result"], "PASS");
    assert_eq!(h["checked_commit"], v["checked_commit"]);
    assert!(h["policy"]["required"]
        .as_array()
        .unwrap()
        .contains(&"freshness".into()));
    assert_ne!(
        h["overall_status"], "PASS",
        "durability etc. are not established"
    );
    assert_eq!(dim(&h, "durability"), "UNKNOWN");
    assert!(h["coverage"]["evidence_age_seconds"].is_u64());

    // Read-only: nothing it read changed.
    assert_eq!(fs::read(&archive).unwrap(), bytes);
    assert_eq!(fs::read(a.p("state/heads.json")).unwrap(), heads);
    assert_eq!(fs::read(&evidence).unwrap(), log);

    // A commit added since: the evidence is about an older head.
    a.append(&archive, "b");
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::DEGRADED, "{h}");
    assert_eq!(dim(&h, "integrity"), "UNKNOWN");
    assert_eq!(dim(&h, "recoverability"), "UNKNOWN");
    assert_eq!(
        dim(&h, "freshness"),
        "PASS",
        "the head is the one last seen"
    );
    let why = skipped_for(&h, "integrity");
    assert!(
        why.contains("commit 0") && why.contains("commit 1"),
        "{why}"
    );

    // Verifying the new head restores it.
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK, "{h}");
    assert_eq!(dim(&h, "integrity"), "PASS");
}

/// **A failure stands.** `fsck` finds a damaged object; `health` reports
/// `FAIL` with the recorded code and exits 1, until newer evidence replaces
/// it, and commits added later do not make it disappear.
#[test]
fn a_failure_found_by_fsck_is_reported_by_health() {
    let a = Area::new();
    let archive = a.archive(true);
    a.append(&archive, "b");
    flip(&archive, chunk_byte(&archive, "a"));
    let (code, v) = a.json(&["fsck", &archive]);
    assert_eq!(code, exit::FAILED, "{v}");

    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::FAILED, "{h}");
    assert_eq!(dim(&h, "integrity"), "FAIL");
    let codes: Vec<&str> = h["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"STORED_INTEGRITY_FAILED"), "{h}");
    assert_eq!(h["overall_status"], "FAIL");
    let (code, out, _) = a.go(&["health", &archive]);
    assert_eq!(code, exit::FAILED);
    assert!(out.contains("STORED_INTEGRITY_FAILED"), "{out}");
    assert!(out.contains("FAIL"), "{out}");
}

/// **Overdue.** With `max_age_days: 0`, any evidence is already too old:
/// `OVERDUE`, exit 2. With the default policy the same evidence is current.
#[test]
fn old_evidence_is_overdue_under_the_policy() {
    let a = Area::new();
    let archive = a.archive(true);
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);
    std::thread::sleep(Duration::from_millis(60));

    let strict = a.policy(
        r#"{"schema":"mochi-health-policy-v1","required":["integrity","recoverability"],
            "max_age_days":{"verify":0}}"#,
    );
    let (code, h) = health_of(&a, &archive, &["--policy", &strict]);
    assert_eq!(code, exit::DEGRADED, "{h}");
    assert_eq!(dim(&h, "integrity"), "OVERDUE");
    assert_eq!(dim(&h, "recoverability"), "OVERDUE");
    assert_eq!(h["policy_result"], "OVERDUE");
    assert!(
        skipped_for(&h, "integrity").contains("policy allows 0 day"),
        "{h}"
    );

    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK, "{h}");
    let relaxed = a.policy(
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],
            "max_age_days":{"verify":3650}}"#,
    );
    let (code, h) = health_of(&a, &archive, &["--policy", &relaxed]);
    assert_eq!(code, exit::OK, "{h}");
}

/// **A policy that cannot be used is a configuration error** (exit 3), not
/// a guess: unknown keys, unknown dimensions, the wrong schema, nothing
/// required, not JSON, and a missing file.
#[test]
fn a_bad_policy_file_is_a_configuration_error() {
    let a = Area::new();
    let archive = a.archive(true);
    for (body, code) in [
        (
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"extra":1}"#,
            "INVALID_ARGUMENT",
        ),
        (
            r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"scrub":3}}"#,
            "INVALID_ARGUMENT",
        ),
        (
            r#"{"schema":"mochi-health-policy-v1","required":["backup"]}"#,
            "INVALID_ARGUMENT",
        ),
        (
            r#"{"schema":"v2","required":["integrity"]}"#,
            "INVALID_ARGUMENT",
        ),
        (
            r#"{"schema":"mochi-health-policy-v1","required":[]}"#,
            "INVALID_ARGUMENT",
        ),
        ("{not json", "INVALID_ARGUMENT"),
    ] {
        let p = a.policy(body);
        let (exit_code, out, _) = a.go(&["health", &archive, "--policy", &p, "--json"]);
        assert_eq!(exit_code, exit::ERROR, "{body}: {out}");
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["error"]["code"], code, "{body}");
        assert!(v.get("dimensions").is_none(), "no report for a bad policy");
    }
    let (exit_code, out, _) = a.go(&[
        "health",
        &archive,
        "--policy",
        &a.s("missing.json"),
        "--json",
    ]);
    assert_eq!(exit_code, exit::ERROR);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["error"]["code"], "IO_ERROR");
}

/// **A damaged evidence line** is skipped with a warning and reported as
/// `unreadable_evidence`; while one exists no dimension can pass (it might
/// have recorded a failure).
#[test]
fn a_damaged_evidence_line_is_reported_and_blocks_a_pass() {
    let a = Area::new();
    let archive = a.archive(true);
    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);
    let log = a.p(&format!(
        "state/evidence/{}.jsonl",
        v["archive_id"].as_str().unwrap()
    ));
    assert!(log.exists());
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK, "{h}");

    let mut text = fs::read_to_string(&log).unwrap();
    text.push_str("{this line is not a record\n");
    fs::write(&log, text).unwrap();
    let (code, out, err) = a.go(&["health", &archive, "--json"]);
    let h: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(code, exit::DEGRADED, "{h}");
    assert_eq!(dim(&h, "integrity"), "UNKNOWN");
    assert!(
        skipped_for(&h, "unreadable_evidence").contains("1 evidence log line"),
        "{h}"
    );
    assert!(err.contains("could not be read"), "{err}");

    // The next verify appends on a fresh line and is read; the damaged line
    // still blocks a pass, because it still might hide a failure.
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);
    let (_, h) = health_of(&a, &archive, &[]);
    assert_eq!(dim(&h, "integrity"), "UNKNOWN");
    assert!(skipped_for(&h, "unreadable_evidence").contains("1 evidence log line"));
}

/// **`--no-local-history`** neither records evidence nor consults any.
#[test]
fn no_local_history_neither_records_nor_reads_evidence() {
    let a = Area::new();
    let archive = a.archive(true);
    let (code, v) = a.json(&["verify", &archive, "--no-local-history"]);
    assert_eq!(code, exit::OK, "{v}");
    assert!(
        !a.p("state/evidence").exists(),
        "verify wrote evidence under --no-local-history"
    );
    let (_, h) = health_of(&a, &archive, &[]);
    assert_eq!(dim(&h, "integrity"), "UNKNOWN", "nothing was recorded");

    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK);
    let (code, h2) = health_of(&a, &archive, &["--no-local-history"]);
    assert_eq!(code, exit::DEGRADED, "{h2}");
    assert_eq!(dim(&h2, "integrity"), "UNKNOWN");
    assert!(
        skipped_for(&h2, "integrity").contains("--no-local-history"),
        "{h2}"
    );
    assert_eq!(dim(&h2, "freshness"), "UNKNOWN");
    assert_eq!(dim(&h, "integrity"), "PASS");
}

/// **`restore-test` is evidence for recoverability** and a policy that names
/// `restore_test` requires it: a fresh `verify` alone leaves recoverability
/// `UNKNOWN`; a `restore-test` completes it.
#[test]
fn a_restore_test_satisfies_a_policy_that_requires_one() {
    let a = Area::new();
    let archive = a.archive(true);
    let policy = a.policy(
        r#"{"schema":"mochi-health-policy-v1","required":["integrity","recoverability"],
            "max_age_days":{"verify":30,"restore_test":90}}"#,
    );
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);
    let (code, h) = health_of(&a, &archive, &["--policy", &policy]);
    assert_eq!(code, exit::DEGRADED, "{h}");
    assert_eq!(dim(&h, "integrity"), "PASS");
    assert_eq!(dim(&h, "recoverability"), "UNKNOWN");
    assert!(
        skipped_for(&h, "recoverability").contains("restore_test"),
        "{h}"
    );

    let (code, r) = a.json(&["restore-test", &archive, "-C", &a.s("restored")]);
    assert_eq!(code, exit::OK, "{r}");
    let (code, h) = health_of(&a, &archive, &["--policy", &policy]);
    assert_eq!(code, exit::OK, "{h}");
    assert_eq!(dim(&h, "recoverability"), "PASS");
    // Under the default policy it was never required, and still passes.
    let (code, _) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::OK);
}

/// **A `restore-test` that finds damage is recoverability evidence too**:
/// the restore fails its integrity check, and `health` says `FAIL` with the
/// code, though no `verify` ever ran.
#[test]
fn a_failed_restore_test_is_a_recorded_failure() {
    let a = Area::new();
    let archive = a.archive(true);
    flip(&archive, chunk_byte(&archive, "a"));
    let (code, r) = a.json(&["restore-test", &archive, "-C", &a.s("restored")]);
    assert_eq!(code, exit::FAILED, "{r}");
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::FAILED, "{h}");
    assert_eq!(dim(&h, "recoverability"), "FAIL");
    assert_eq!(
        dim(&h, "integrity"),
        "UNKNOWN",
        "a restore does not assess stored bytes"
    );
    assert!(
        h["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "STORED_INTEGRITY_FAILED"),
        "{h}"
    );
}

fn paths(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect()
}

/// **A complete `repair apply` is evidence for the new archive**; a partial
/// one is not (the user verifies what was salvaged).
#[test]
fn repair_apply_records_evidence_only_for_a_complete_repair() {
    let a = Area::new();
    let archive = a.archive(true);
    a.append(&archive, "b");
    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::OK, "{v}");
    let new = a.s("new.mochi");
    let (code, v) = a.json(&[
        "repair", "apply", &archive, "--plan", &plan, "--output", &new,
    ]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["outcome"], "complete");
    let (code, h) = health_of(&a, &new, &[]);
    assert_eq!(code, exit::OK, "{h}");
    assert_eq!(dim(&h, "integrity"), "PASS");
    assert_eq!(dim(&h, "recoverability"), "PASS");
    assert_eq!(dim(&h, "freshness"), "PASS", "the new head was remembered");
    let (_, l1) = a.json(&["list", &archive]);
    let (_, l2) = a.json(&["list", &new]);
    assert_eq!(paths(&l1), paths(&l2));
    // The source has no evidence of its own from this.
    let (_, hs) = health_of(&a, &archive, &[]);
    assert_eq!(dim(&hs, "integrity"), "UNKNOWN");

    // A partial salvage records nothing.
    let a2 = Area::new();
    let damaged = a2.archive(true);
    a2.append(&damaged, "b");
    flip(&damaged, chunk_byte(&damaged, "b"));
    let plan2 = a2.s("plan.json");
    let (code, v) = a2.json(&["repair", "plan", &damaged, "--output", &plan2]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    let new2 = a2.s("new.mochi");
    let (code, v) = a2.json(&[
        "repair", "apply", &damaged, "--plan", &plan2, "--output", &new2,
    ]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(v["outcome"], "partial");
    let (code, h) = health_of(&a2, &new2, &[]);
    assert_eq!(code, exit::DEGRADED, "{h}");
    assert_eq!(
        dim(&h, "integrity"),
        "UNKNOWN",
        "a partial repair is not evidence"
    );
}

/// **Freshness is judged as `verify` judges it.** An archive rolled back to
/// an older copy fails freshness (exit 1, `FRESHNESS_FAILED`) in `health`
/// exactly as in `verify`.
#[test]
fn health_judges_freshness_like_verify() {
    let a = Area::new();
    let archive = a.archive(true);
    let old = fs::read(&archive).unwrap();
    a.append(&archive, "b");
    fs::write(&archive, &old).unwrap();

    let (vcode, v) = a.json(&["verify", &archive]);
    assert_eq!(vcode, exit::FAILED, "{v}");
    let (code, h) = health_of(&a, &archive, &[]);
    assert_eq!(code, exit::FAILED, "{h}");
    assert_eq!(dim(&h, "freshness"), "FAIL");
    assert!(h["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "FRESHNESS_FAILED"));
    let vm: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "FRESHNESS_FAILED")
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    let hm: Vec<&str> = h["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "FRESHNESS_FAILED")
        .map(|f| f["message"].as_str().unwrap())
        .collect();
    assert_eq!(vm.len(), 1, "{v}");
    for m in &vm {
        assert!(
            hm.contains(m),
            "verify's verdict, in verify's words: {m:?} not in {hm:?}"
        );
    }
    // `verify` also failed integrity for the rollback, and that failure was
    // recorded: it stands in `health` until newer evidence replaces it.
    assert_eq!(dim(&h, "integrity"), "FAIL");
}

/// **Wording (spec §23.3).** Whatever the outcome, `health` never says
/// "safe", "backed up", or "preserved".
#[test]
fn health_wording_never_claims_safety() {
    let a = Area::new();
    let archive = a.archive(true);
    let mut texts = Vec::new();
    texts.push(a.go(&["health", &archive]).1);
    a.json(&["verify", &archive]);
    texts.push(a.go(&["health", &archive]).1);
    a.append(&archive, "b");
    texts.push(a.go(&["health", &archive]).1);
    flip(&archive, chunk_byte(&archive, "b"));
    a.json(&["fsck", &archive]);
    texts.push(a.go(&["health", &archive]).1);
    texts.push(a.go(&["health", &archive, "--json"]).1);
    for t in texts {
        let lower = t.to_lowercase();
        for word in ["safe", "backed up", "preserved"] {
            assert!(!lower.contains(word), "{word:?} in {t}");
        }
    }
}
