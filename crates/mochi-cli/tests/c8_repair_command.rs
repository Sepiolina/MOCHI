//! C14 end-to-end tests for `repair plan` and `repair apply` (plan C8),
//! run in-process through `mochi_cli::run` against the real filesystem.
//!
//! The library behaviour (the ladder, what is recovered or left out,
//! retention) is tested in `mochi-testkit/tests/c8_repair.rs`; these tests
//! pin what the CLI adds: the saved plan as the approval, its re-check,
//! the exit codes, the partial labelling in text and JSON, the retention
//! waiver flag, refusing to replace anything, the source left
//! byte-identical, and the new archive's freshness anchor.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use mochi_core::catalog::extent::ExtentSource;
use mochi_core::publish::{commit_history, open_head, ReadOptions};
use mochi_core::storage::os::OsReadStorage;
use serde_json::Value;

/// Exit code of a command that creates a new file with nothing else to
/// report: Windows reports its directory entry as not confirmed durable
/// in 1.0 (plan O12, G6 policy B), which is `DEGRADED`.
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

    fn go(&self, args: &[&str]) -> (u8, String, String) {
        let state = self.s("state");
        let mut argv = vec!["mochi", "--state-dir", state.as_str()];
        argv.extend_from_slice(args);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(argv, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
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

    /// Three commits: 0 adds `a` and `b`, 1 replaces `a`, 2 adds `c`.
    fn history(&self) -> String {
        let archive = self.s("a.mochi");
        let (code, v) = self.json(&[
            "create",
            &archive,
            &self.file("a", &[1u8; 70_000]),
            &self.file("b", b"bravo"),
        ]);
        assert_eq!(code, CREATED, "{v}");
        let (code, v) = self.json(&["append", &archive, &self.file("a", &[2u8; 90_000])]);
        assert_eq!(code, exit::OK, "{v}");
        let (code, v) = self.json(&["append", &archive, &self.file("c", b"charlie")]);
        assert_eq!(code, exit::OK, "{v}");
        archive
    }
}

fn flip(path: &str, at: u64) {
    let mut b = fs::read(path).unwrap();
    b[at as usize] ^= 0x40;
    fs::write(path, b).unwrap();
}

/// Offset of a byte in the middle of the first chunk of `name` at the head.
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

fn paths(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap().to_owned())
        .collect()
}

/// **Clean archive.** The plan is saved (never replacing a file) and is
/// complete (exit 0); applying it writes a new archive that lists the same
/// entries, whose head is remembered as its freshness anchor, and leaves
/// the source byte-identical.
#[test]
fn c8_repair_plan_and_apply_on_a_clean_archive() {
    let a = Area::new();
    let archive = a.history();
    let before = fs::read(&archive).unwrap();
    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["outcome"], "complete");
    assert_eq!(v["plan"]["damage_found"], false);
    // A plan file is never replaced.
    let (code, v) = a.json(&["repair", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");

    let new = a.s("new.mochi");
    let (code, v) = a.json(&[
        "repair", "apply", &archive, "--plan", &plan, "--output", &new,
    ]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["outcome"], "complete");
    assert_eq!(v["source_kept"], true);
    assert_eq!(v["commits"].as_array().unwrap().len(), 3);
    assert_eq!(fs::read(&archive).unwrap(), before, "source untouched");
    let (_, l1) = a.json(&["list", &archive]);
    let (_, l2) = a.json(&["list", &new]);
    assert_eq!(paths(&l1), paths(&l2));
    // The new head was recorded, so verify has an anchor and passes.
    let (code, v) = a.json(&["verify", &new]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["freshness_anchor"], "local-history");

    // Never replaces: the same output again is refused, untouched.
    let len = fs::metadata(&new).unwrap().len();
    let (code, v) = a.json(&[
        "repair", "apply", &archive, "--plan", &plan, "--output", &new,
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert_eq!(fs::metadata(&new).unwrap().len(), len);
}

/// **Partial salvage is labelled partial**, in text and JSON and by exit
/// code 2, for both plan and apply; the left-out file is named, and the
/// repaired archive holds everything else.
#[test]
fn c8_repair_of_a_damaged_file_is_labelled_partial() {
    let a = Area::new();
    let archive = a.history();
    flip(&archive, chunk_byte(&archive, "c"));

    let (code, out, _) = a.go(&["repair", "plan", &archive]);
    assert_eq!(code, exit::DEGRADED, "{out}");
    assert!(out.contains("PARTIAL"), "{out}");
    assert!(out.contains("left out file "), "{out}");
    assert!(out.contains("STORED_INTEGRITY_FAILED"), "{out}");
    assert!(
        out.contains("step 7 bounded salvage scan: NOT ATTEMPTED"),
        "{out}"
    );

    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "-o", &plan]);
    assert_eq!(code, exit::DEGRADED);
    assert_eq!(v["outcome"], "partial");
    let omitted = v["plan"]["omitted"].as_array().unwrap();
    assert_eq!(omitted.len(), 1);
    assert!(omitted[0]["path"].as_str().unwrap().ends_with('c'));

    let new = a.s("new.mochi");
    let (code, out, err) = a.go(&["repair", "apply", &archive, "--plan", &plan, "-o", &new]);
    assert_eq!(code, exit::DEGRADED, "{out}{err}");
    assert!(out.starts_with("PARTIAL repair (salvage)"), "{out}");
    assert!(out.contains("NOT recovered: 1 entry left out"), "{out}");
    for word in ["backed up", "safe", "preserved"] {
        assert!(!out.contains(word), "{out}");
    }
    let (_, l1) = a.json(&["list", &archive]);
    let (_, l2) = a.json(&["list", &new]);
    let mut want = paths(&l1);
    want.retain(|p| !p.ends_with('c'));
    assert_eq!(paths(&l2), want);
    let (code, v) = a.json(&["verify", &new]);
    assert_eq!(code, exit::OK, "{v}");
}

/// **The saved plan is the approval.** After another commit lands, or
/// with a plan file that is not a repair plan, apply refuses and creates
/// nothing.
#[test]
fn c8_repair_apply_refuses_a_stale_or_foreign_plan() {
    let a = Area::new();
    let archive = a.history();
    let plan = a.s("plan.json");
    let (code, _) = a.json(&["repair", "plan", &archive, "-o", &plan]);
    assert_eq!(code, exit::OK);
    let (code, _) = a.json(&["append", &archive, &a.file("d", b"delta")]);
    assert_eq!(code, exit::OK);
    let new = a.s("new.mochi");
    let (code, v) = a.json(&["repair", "apply", &archive, "--plan", &plan, "-o", &new]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(!a.p("new.mochi").exists());

    let gc_plan = a.s("gc.json");
    let (code, _) = a.json(&["gc", "plan", &archive, "-o", &gc_plan]);
    assert_eq!(code, exit::OK);
    let (code, v) = a.json(&["repair", "apply", &archive, "--plan", &gc_plan, "-o", &new]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a repair plan"));
    assert!(!a.p("new.mochi").exists());
}

/// **Retention is never dropped silently.** With the head's delta
/// manifest damaged, the head snapshot is lost and its retention cannot be
/// rebuilt: apply refuses (exit 1) until `--accept-retention-loss`, which
/// the output then reports.
#[test]
fn c8_repair_apply_needs_the_waiver_when_retention_is_unresolved() {
    let a = Area::new();
    let archive = a.history();
    let s = OsReadStorage::open(std::path::Path::new(&archive)).unwrap();
    let h = commit_history(&s, &ReadOptions::default()).unwrap();
    let head = h.last().unwrap();
    assert!(
        !head.commit.metadata.is_checkpoint(),
        "the fixture's head is a delta"
    );
    let m = head.commit.delta_manifest;
    drop(s);
    flip(&archive, m.offset + m.stored_len / 2);

    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "-o", &plan]);
    assert_eq!(code, exit::DEGRADED);
    assert_eq!(v["plan"]["retention"]["state"], "unresolved");
    assert_eq!(v["plan"]["lost_snapshots"][0]["seq"], 2);

    let new = a.s("new.mochi");
    let (code, v) = a.json(&["repair", "apply", &archive, "--plan", &plan, "-o", &new]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["error"]["code"], "RETENTION_UNRESOLVED");
    assert!(v["error"]["message"].as_str().unwrap().contains("accept"));
    assert!(!a.p("new.mochi").exists());

    let (code, out, err) = a.go(&[
        "repair",
        "apply",
        &archive,
        "--plan",
        &plan,
        "-o",
        &new,
        "--accept-retention-loss",
    ]);
    assert_eq!(code, exit::DEGRADED, "{out}{err}");
    assert!(out.contains("retention NOT carried"), "{out}");
    assert!(out.contains("NOT recovered: snapshot 2"), "{out}");
}

fn blake3_of(path: &str) -> String {
    blake3::hash(&fs::read(path).unwrap()).to_hex().to_string()
}

/// **Independent copies (§22 step 5, Annex B.2.12).** `--copy` donates a
/// verified frame, the plan names it and the copy, `apply` needs the same
/// copies (a plan made with one is refused without it), a copy is never
/// written (whole-file BLAKE3 before and after both commands), the archive
/// itself is not a copy, and a v1 plan says to plan again.
#[test]
fn c8_repair_copy_takes_a_verified_frame_from_a_byte_copy() {
    let a = Area::new();
    let archive = a.history();
    let good = a.s("good.mochi");
    fs::copy(&archive, &good).unwrap();
    flip(&archive, chunk_byte(&archive, "c"));
    let (src_hash, copy_hash) = (blake3_of(&archive), blake3_of(&good));

    // Text: the ladder names step 5 as used, the copy, and the frame.
    let (code, out, err) = a.go(&["repair", "plan", &archive, "--copy", &good]);
    assert_eq!(code, exit::OK, "{out}{err}");
    assert!(out.contains("step 5 independent copies: used"), "{out}");
    assert!(out.contains("copy 1 "), "{out}");
    assert!(out.contains("identical descriptor"), "{out}");
    assert!(out.contains("comes from copy 1"), "{out}");
    assert!(out.starts_with("repair plan for archive"), "{out}");
    assert!(out.contains("outcome: complete"), "{out}");

    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "--copy", &good, "-o", &plan]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["outcome"], "complete");
    assert_eq!(v["plan"]["format"], "mochi-repair-plan-v2");
    assert_eq!(v["plan"]["donations"].as_array().unwrap().len(), 1);
    assert_eq!(v["plan"]["donations"][0]["kind"], "data_object");
    assert_eq!(v["plan"]["donations"][0]["copy"], 0);
    assert_eq!(v["plan"]["copies"][0]["match"], "identical_descriptor");
    assert_eq!(v["plan"]["copies"][0]["status"], "used");
    assert_eq!(v["plan"]["copies"][0]["path"], good.as_str());
    assert_eq!(blake3_of(&good), copy_hash, "plan reads a copy only");
    assert_eq!(blake3_of(&archive), src_hash);

    // Without the copy the plan does not match any more.
    let new = a.s("new.mochi");
    let (code, v) = a.json(&["repair", "apply", &archive, "--plan", &plan, "-o", &new]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(!a.p("new.mochi").exists());

    let (code, v) = a.json(&[
        "repair", "apply", &archive, "--plan", &plan, "--copy", &good, "-o", &new,
    ]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["outcome"], "complete");
    assert_eq!(v["donations"].as_array().unwrap().len(), 1);
    assert_eq!(v["copies"][0]["frames_donated"], 1);
    assert_eq!(blake3_of(&good), copy_hash, "apply reads a copy only");
    assert_eq!(blake3_of(&archive), src_hash);
    // Everything is back: the pristine copy lists what the repair lists.
    let (_, l1) = a.json(&["list", &good]);
    let (_, l2) = a.json(&["list", &new]);
    assert_eq!(paths(&l1), paths(&l2));
    let (code, v) = a.json(&["verify", &new]);
    assert_eq!(code, exit::OK, "{v}");

    // The archive is not its own copy; a missing copy is an error.
    let (code, v) = a.json(&["repair", "plan", &archive, "--copy", &archive]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, v) = a.json(&["repair", "plan", &archive, "--copy", &a.s("missing.mochi")]);
    assert_eq!(code, exit::ERROR, "{v}");

    // A plan from before copies (v1) is refused with advice.
    let mut old: Value = serde_json::from_str(&fs::read_to_string(&plan).unwrap()).unwrap();
    old["format"] = "mochi-repair-plan-v1".into();
    let v1 = a.s("v1.json");
    fs::write(&v1, old.to_string()).unwrap();
    let (code, v) = a.json(&[
        "repair",
        "apply",
        &archive,
        "--plan",
        &v1,
        "--copy",
        &good,
        "-o",
        &a.s("v1.mochi"),
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("plan again"));
    assert!(!a.p("v1.mochi").exists());
}

fn footer_offsets(archive: &str) -> Vec<u64> {
    let s = OsReadStorage::open(std::path::Path::new(archive)).unwrap();
    commit_history(&s, &ReadOptions::default())
        .unwrap()
        .iter()
        .map(|e| e.footer_offset)
        .collect()
}

/// **The bounded salvage scan (§22 step 7, Annex B.2.13 D23).** With every
/// footer damaged there is no head: without `--salvage` nothing is
/// recoverable (exit 1) and the ladder says the scan may recover more; with
/// it the commits the scan can confirm are recovered, the unconfirmed head is
/// listed and left out, the plan is partial (exit 2) in text and JSON, a
/// plan made with it is bound to it, retention needs its waiver, and the new
/// archive verifies. `--archive-id` needs `--salvage` and valid hex.
#[test]
fn c8_repair_salvage_recovers_confirmed_commits_and_is_always_partial() {
    let a = Area::new();
    let archive = a.history();
    let source_before = blake3_of(&archive);
    for f in footer_offsets(&archive) {
        flip(&archive, f + 30);
    }
    let damaged_hash = blake3_of(&archive);
    assert_ne!(source_before, damaged_hash);

    // Without --salvage: nothing, and the detail points at the flag.
    let (code, out, err) = a.go(&["repair", "plan", &archive]);
    assert_eq!(code, exit::FAILED, "{out}{err}");
    assert!(
        out.contains("step 7 bounded salvage scan: NOT ATTEMPTED"),
        "{out}"
    );
    assert!(out.contains("--salvage"), "{out}");

    // Text with --salvage.
    let (code, out, err) = a.go(&["repair", "plan", &archive, "--salvage"]);
    assert_eq!(code, exit::DEGRADED, "{out}{err}");
    assert!(out.contains("step 7 bounded salvage scan: used"), "{out}");
    assert!(out.contains("salvage scan: "), "{out}");
    assert!(out.contains("NOT recovered: commit 2"), "{out}");
    assert!(
        out.contains("no footer or later commit confirms publication"),
        "{out}"
    );
    assert!(out.contains("snapshots found by the salvage scan"), "{out}");
    assert!(out.contains("outcome: PARTIAL"), "{out}");

    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "--salvage", "-o", &plan]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(v["outcome"], "partial");
    assert_eq!(v["plan"]["ladder"][6]["status"], "used");
    assert_eq!(v["plan"]["snapshots"].as_array().unwrap().len(), 2);
    assert_eq!(v["plan"]["snapshots"][0]["trust"], "salvage");
    assert_eq!(v["plan"]["salvage"]["unconfirmed"][0]["seq"], 2);
    assert_eq!(v["plan"]["salvage"]["confirmed"][0]["by"], "link");
    assert_eq!(v["plan"]["salvage"]["limit_reached"], false);
    assert_eq!(blake3_of(&archive), damaged_hash, "plan only reads");

    // Applying needs the same options and the retention waiver.
    let new = a.s("new.mochi");
    let (code, v) = a.json(&["repair", "apply", &archive, "--plan", &plan, "-o", &new]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, v) = a.json(&[
        "repair",
        "apply",
        &archive,
        "--plan",
        &plan,
        "--salvage",
        "-o",
        &new,
    ]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["error"]["code"], "RETENTION_UNRESOLVED");
    assert!(!a.p("new.mochi").exists());
    let (code, out, err) = a.go(&[
        "repair",
        "apply",
        &archive,
        "--plan",
        &plan,
        "--salvage",
        "--accept-retention-loss",
        "-o",
        &new,
    ]);
    assert_eq!(code, exit::DEGRADED, "{out}{err}");
    assert!(out.contains("PARTIAL repair (salvage)"), "{out}");
    assert!(out.contains("NOT recovered: commit 2"), "{out}");
    assert_eq!(blake3_of(&archive), damaged_hash, "apply only reads");
    let (code, v) = a.json(&["verify", &new]);
    assert_eq!(code, exit::OK, "{v}");
    let (_, l) = a.json(&["list", &new]);
    assert_eq!(paths(&l), ["a", "b"]);

    // The candidate limit: incomplete, never a pass, still partial.
    let (code, out, err) = a.go(&[
        "--limit",
        "max-salvage-candidates=3",
        "repair",
        "plan",
        &archive,
        "--salvage",
    ]);
    assert_eq!(code, exit::DEGRADED, "{out}{err}");
    assert!(out.contains("recoverable snapshots (1): 0"), "{out}");
    assert!(
        out.contains("step 7 bounded salvage scan: INCOMPLETE"),
        "{out}"
    );
    assert!(out.contains("REACHED"), "{out}");

    // Arguments.
    let (code, out, err) = a.go(&["repair", "plan", &archive, "--archive-id", &"ab".repeat(32)]);
    assert_ne!(code, exit::OK, "{out}");
    assert!(err.contains("--salvage"), "{err}");
    let (code, v) = a.json(&[
        "repair",
        "plan",
        &archive,
        "--salvage",
        "--archive-id",
        "xyz",
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    // A readable descriptor that names another archive.
    let (code, v) = a.json(&[
        "repair",
        "plan",
        &archive,
        "--salvage",
        "--archive-id",
        &"ab".repeat(32),
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
}

/// Two archives in one file and a destroyed descriptor: `ARCHIVE_ID_AMBIGUOUS`
/// (exit 3) names both, and `--archive-id` picks one.
#[test]
fn c8_repair_salvage_two_archives_need_an_archive_id() {
    let a = Area::new();
    let first = a.history();
    let second = a.s("second.mochi");
    let (code, v) = a.json(&["create", &second, &a.file("z", b"zulu")]);
    assert_eq!(code, CREATED, "{v}");
    let s = OsReadStorage::open(std::path::Path::new(&first)).unwrap();
    let h = commit_history(&s, &ReadOptions::default()).unwrap();
    let id_first = h[0].commit.archive_id.to_hex();
    let desc_len = h[0].commit.descriptor.stored_len as usize;
    let s2 = OsReadStorage::open(std::path::Path::new(&second)).unwrap();
    let id_second = commit_history(&s2, &ReadOptions::default()).unwrap()[0]
        .commit
        .archive_id
        .to_hex();
    drop((s, s2));
    let both = a.s("both.mochi");
    let mut bytes = fs::read(&first).unwrap();
    bytes.extend_from_slice(&fs::read(&second).unwrap());
    bytes[..desc_len].fill(0);
    fs::write(&both, bytes).unwrap();

    let (code, v) = a.json(&["repair", "plan", &both, "--salvage"]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "ARCHIVE_ID_AMBIGUOUS");
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains(&id_first) && msg.contains(&id_second), "{msg}");

    let (code, v) = a.json(&[
        "repair",
        "plan",
        &both,
        "--salvage",
        "--archive-id",
        &id_first,
    ]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    // Without -o, --json prints the plan itself.
    assert_eq!(v["snapshots"].as_array().unwrap().len(), 3);
    assert_eq!(v["salvage"]["archive_id_source"], "option");
    assert_eq!(v["salvage"]["foreign_commits"], 1);
    assert_eq!(
        v["salvage"]["profile_inference"]["descriptor_reconstructed"],
        true
    );
}
