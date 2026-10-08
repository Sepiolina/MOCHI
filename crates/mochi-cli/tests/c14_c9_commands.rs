//! C14 end-to-end tests for the C9 commands: `snapshot retain`/`expire`/
//! `release`, `checkpoint`, `gc plan`, `gc apply`, and `compact`, each run
//! in-process through `mochi_cli::run` against the real filesystem, with
//! JSON output and exit code (plan C14 exit).
//!
//! The library behaviour (marking, rewriting, interruption) is tested in
//! `mochi-testkit/tests/c9_*.rs`; these tests pin what the CLI adds: the
//! confirmation for retention reductions, the saved plan and its re-check
//! under the lock, refusing to replace anything, the source left
//! byte-identical, and the freshness anchor of the new archive.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use serde_json::Value;

/// Exit code of a command that creates a new file: Windows reports its
/// directory entry as not confirmed durable until gate G6 (plan O12), which
/// is `DEGRADED`.
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

    /// Write `name` in the source directory with `content`.
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

    fn commits(&self, archive: &str) -> Vec<Value> {
        let (code, v) = self.json(&["snapshot", "list", archive]);
        assert_eq!(code, exit::OK, "{v}");
        v["commits"].as_array().unwrap().clone()
    }
}

fn names(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| format!("{} {} {}", e["path"], e["size"], e["content_hash"]))
        .collect()
}

/// **checkpoint.** One new commit, a checkpoint, with the namespace
/// unchanged; the archive still verifies.
#[test]
fn c14_checkpoint_writes_a_checkpoint_commit() {
    let a = Area::new();
    let archive = a.history();
    let before = a.commits(&archive);
    assert_eq!(before[2]["checkpoint"], false);
    let (_, list_before) = a.json(&["list", &archive]);

    let (code, v) = a.json(&["checkpoint", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["status"], "LOCAL_COMMITTED");
    assert_eq!(v["seq"], 3);
    assert_eq!(v["checkpoint"], true);

    let after = a.commits(&archive);
    assert_eq!(after.len(), 4);
    assert_eq!(after[3]["checkpoint"], true);
    let (_, list_after) = a.json(&["list", &archive]);
    assert_eq!(names(&list_before), names(&list_after));

    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");

    // Text output names the commit and never claims more than a local commit.
    let (code, out, _) = a.go(&["checkpoint", &archive]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("LOCAL_COMMITTED"), "{out}");
    for word in ["backed up", "safe", "preserved"] {
        assert!(!out.contains(word), "{out}");
    }
}

/// **snapshot retain / expire / release.** Reductions need `--confirm`
/// and commit nothing without it; invalid operations commit nothing;
/// `snapshot list` shows the state.
#[test]
fn c14_retention_commands() {
    let a = Area::new();
    let archive = a.history();
    let len = fs::metadata(&archive).unwrap().len();

    // Without --confirm: refused, nothing written.
    let (code, v) = a.json(&["snapshot", "expire", &archive, "0"]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("--confirm"));
    let (code, _) = a.json(&["snapshot", "release", &archive, "--label", "x"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(fs::metadata(&archive).unwrap().len(), len);

    // Invalid: the head (and anything later) cannot expire; an unknown hold
    // cannot be released. Nothing is written.
    for args in [
        &["snapshot", "expire", &archive, "3", "--confirm"][..],
        &["snapshot", "expire", &archive, "0", "0", "--confirm"],
        &[
            "snapshot",
            "release",
            &archive,
            "--label",
            "none",
            "--confirm",
        ],
        &["snapshot", "retain", &archive, "9", "--label", "x"],
        &["snapshot", "retain", &archive, "1", "--label", ""],
    ] {
        let (code, v) = a.json(args);
        assert_eq!(code, exit::ERROR, "{args:?}: {v}");
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{args:?}: {v}");
    }
    assert_eq!(fs::metadata(&archive).unwrap().len(), len);

    let (code, v) = a.json(&["snapshot", "retain", &archive, "1", "--label", "audit"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 3);
    assert_eq!(v["change"]["hold"]["label"], "audit");
    // `hold` is the same command.
    let (code, _) = a.json(&["snapshot", "hold", &archive, "2", "--label", "x"]);
    assert_eq!(code, exit::OK);
    let (code, v) = a.json(&["snapshot", "expire", &archive, "0", "1", "--confirm"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 5);

    let c = a.commits(&archive);
    assert_eq!(c.len(), 6);
    assert_eq!(c[0]["expired"], true);
    assert_eq!(c[0]["retained"], false);
    assert_eq!(c[1]["expired"], true);
    assert_eq!(c[1]["holds"], serde_json::json!(["audit"]));
    assert_eq!(c[1]["retained"], true, "a hold keeps an expired snapshot");
    assert_eq!(c[2]["holds"], serde_json::json!(["x"]));
    assert_eq!(c[2]["expired"], false);
    assert_eq!(c[5]["retained"], true);

    let (code, v) = a.json(&[
        "snapshot",
        "release",
        &archive,
        "--label",
        "audit",
        "--confirm",
    ]);
    assert_eq!(code, exit::OK, "{v}");
    let c = a.commits(&archive);
    assert_eq!(c[1]["retained"], false);
    assert_eq!(c[1]["holds"], serde_json::json!([]));

    let (code, out, _) = a.go(&["snapshot", "list", &archive]);
    assert_eq!(code, exit::OK);
    assert!(out.lines().next().unwrap().ends_with("expired"), "{out}");
    assert!(out.lines().nth(2).unwrap().ends_with("hold x"), "{out}");

    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");
}

/// **gc plan, gc apply.** The plan names what is collected and why and
/// changes nothing; applying it writes a new archive without the expired
/// snapshot, restoring the head identically; the source stays
/// byte-identical; nothing is ever replaced; a stale plan is refused.
#[test]
fn c14_gc_plan_and_apply() {
    let a = Area::new();
    let archive = a.history();
    let (code, _) = a.json(&["snapshot", "expire", &archive, "0", "--confirm"]);
    assert_eq!(code, exit::OK);
    let source = fs::read(&archive).unwrap();

    // A plan printed, and the same plan saved.
    let (code, printed) = a.json(&["gc", "plan", &archive]);
    assert_eq!(code, exit::OK, "{printed}");
    assert_eq!(printed["roots"], serde_json::json!([1, 2, 3]));
    assert_eq!(printed["collectable_snapshots"][0]["seq"], 0);
    assert_eq!(
        printed["collectable_snapshots"][0]["reason"],
        "expired; not the head; no active hold"
    );
    // Snapshot 0's version of `a` (and its chunk) is reached by no root.
    assert_eq!(printed["collectable"]["file_versions"], 1);
    assert!(printed["collectable"]["chunks"].as_u64().unwrap() >= 1);
    let plan = a.s("plan.json");
    let (code, v) = a.json(&["gc", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["collects_anything"], true);
    assert_eq!(v["plan"], printed);
    let saved: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    assert_eq!(saved, printed);
    // A plan file is never replaced.
    let (code, v) = a.json(&["gc", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert_eq!(fs::read(&archive).unwrap(), source, "planning is read-only");

    // Applying refuses to replace anything at the output name.
    fs::write(a.p("taken.mochi"), b"keep me").unwrap();
    let (code, v) = a.json(&[
        "gc",
        "apply",
        &archive,
        "--plan",
        &plan,
        "--output",
        &a.s("taken.mochi"),
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert_eq!(fs::read(a.p("taken.mochi")).unwrap(), b"keep me");

    let out = a.s("collected.mochi");
    let (code, v) = a.json(&["gc", "apply", &archive, "--plan", &plan, "--output", &out]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["command"], "gc apply");
    assert_eq!(v["collected"], serde_json::json!([0]));
    assert_eq!(v["source_kept"], true);
    assert_eq!(v["versions_verified"], 3);
    assert_ne!(v["new_archive_id"], v["source_archive_id"]);
    let mapping: Vec<(u64, u64)> = v["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["source_seq"].as_u64().unwrap(),
                c["new_seq"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(mapping, [(1, 0), (2, 1), (3, 2)]);
    assert_eq!(
        fs::read(&archive).unwrap(),
        source,
        "the source is kept as is"
    );

    // Each kept snapshot lists identically in the new archive.
    for (src_seq, new_seq) in mapping {
        let (_, s) = a.json(&["list", &archive, "--snapshot", &src_seq.to_string()]);
        let (_, n) = a.json(&["list", &out, "--snapshot", &new_seq.to_string()]);
        assert_eq!(names(&s), names(&n), "source {src_seq}");
    }
    // The new archive's head was recorded, so it verifies with an anchor.
    let (code, v) = a.json(&["verify", &out]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["freshness_anchor"], "local-history");
    assert_eq!(v["dimensions"]["freshness"], "PASS");
    // The new archive keeps the expiry of nothing it holds: 0 is gone.
    let c = a.commits(&out);
    assert_eq!(c.len(), 3);
    assert!(c.iter().all(|c| c["retained"] == true));
    // Restoring the head yields the head's files.
    let (code, v) = a.json(&["get", &out, "-C", &a.s("restored")]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(fs::read(a.p("restored/a")).unwrap(), vec![2u8; 90_000]);
    assert_eq!(fs::read(a.p("restored/c")).unwrap(), b"charlie");

    // A plan made before a later commit is stale: refused, nothing written.
    let (code, _) = a.json(&["append", &archive, &a.file("d", b"delta")]);
    assert_eq!(code, exit::OK);
    let out2 = a.s("stale.mochi");
    let (code, v) = a.json(&["gc", "apply", &archive, "--plan", &plan, "--output", &out2]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(v["error"]["message"].as_str().unwrap().contains("plan"));
    assert!(!a.p("stale.mochi").exists());

    // A plan for another archive is refused.
    let (code, v) = a.json(&["gc", "apply", &out, "--plan", &plan, "--output", &out2]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(v["error"]["message"].as_str().unwrap().contains("archive"));
    assert!(!a.p("stale.mochi").exists());

    // Something that is not a plan.
    let (code, v) = a.json(&[
        "gc", "apply", &archive, "--plan", &archive, "--output", &out2,
    ]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(!a.p("stale.mochi").exists());
}

/// A plan edited after it was saved (here, claiming nothing is collected)
/// is not what the archive would collect: refused.
#[test]
fn c14_gc_apply_refuses_an_edited_plan() {
    let a = Area::new();
    let archive = a.history();
    let (code, _) = a.json(&["snapshot", "expire", &archive, "0", "--confirm"]);
    assert_eq!(code, exit::OK);
    let plan = a.s("plan.json");
    let (code, _) = a.json(&["gc", "plan", &archive, "--output", &plan]);
    assert_eq!(code, exit::OK);
    let mut v: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    v["collectable_snapshots"] = serde_json::json!([]);
    fs::write(&plan, v.to_string()).unwrap();
    let out = a.s("out.mochi");
    let (code, v) = a.json(&["gc", "apply", &archive, "--plan", &plan, "--output", &out]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    assert!(!a.p("out.mochi").exists());
}

/// **compact.** Every snapshot is kept, one commit each, restoring
/// identically; retention carries over; the source is byte-identical.
#[test]
fn c14_compact_keeps_every_snapshot() {
    let a = Area::new();
    let archive = a.history();
    let (code, _) = a.json(&["snapshot", "retain", &archive, "1", "--label", "keep"]);
    assert_eq!(code, exit::OK);
    let source = fs::read(&archive).unwrap();

    let out = a.s("compact.mochi");
    let (code, v) = a.json(&["compact", &archive, "--output", &out]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["command"], "compact");
    assert_eq!(v["collected"], serde_json::json!([]));
    assert_eq!(v["commits"].as_array().unwrap().len(), 4);
    assert_eq!(fs::read(&archive).unwrap(), source);
    for seq in 0..4 {
        let (_, s) = a.json(&["list", &archive, "--snapshot", &seq.to_string()]);
        let (_, n) = a.json(&["list", &out, "--snapshot", &seq.to_string()]);
        assert_eq!(names(&s), names(&n), "snapshot {seq}");
    }
    let c = a.commits(&out);
    assert_eq!(c[1]["holds"], serde_json::json!(["keep"]));

    // --no-verify-content is reported, not hidden.
    let (code, v) = a.json(&[
        "compact",
        &archive,
        "--output",
        &a.s("fast.mochi"),
        "--no-verify-content",
    ]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["versions_verified"], Value::Null);
    let (code, out_text, _) = a.go(&["compact", &archive, "-o", &a.s("t.mochi")]);
    assert_eq!(code, CREATED);
    assert!(out_text.contains("is unchanged and kept"), "{out_text}");

    // Never over an existing file, the source included.
    let (code, v) = a.json(&["compact", &archive, "--output", &archive]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert_eq!(fs::read(&archive).unwrap(), source);
}
