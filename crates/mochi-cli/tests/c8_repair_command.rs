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
/// until gate G6 (plan O12), which is `DEGRADED`.
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
