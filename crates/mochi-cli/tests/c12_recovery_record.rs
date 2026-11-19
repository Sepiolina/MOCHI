//! C12 end-to-end: `mochi create --recovery-record PERCENT` (the Redundancy
//! profile, spec Annex B.2.11 D21), then `verify` and `repair` on damage
//! within the record's capacity, run in-process against real files. The
//! library behaviour is tested in `mochi-testkit/tests/c12_redundancy.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use mochi_core::publish::{open_head, ReadOptions};
use mochi_core::storage::os::OsReadStorage;
use serde_json::Value;

/// Exit code of a command that creates a new file: Windows reports its
/// directory entry as not confirmed durable in 1.0 (plan O12, G6 policy B).
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

    fn s(&self, rel: &str) -> String {
        self.p(rel).display().to_string()
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
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
}

fn content(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 131 % 251) as u8).collect()
}

/// The first commit's catalog image: damage there makes the head unreadable
/// to a reader without parity.
fn image_range(archive: &str) -> (u64, u64) {
    let s = OsReadStorage::open(std::path::Path::new(archive)).unwrap();
    let h = open_head(&s, &ReadOptions::default()).unwrap();
    match h.commit.metadata {
        mochi_core::commit::Metadata::Checkpoint { image, .. } => (image.offset, image.stored_len),
        _ => panic!("commit 0 is a checkpoint"),
    }
}

#[test]
fn c12_create_with_a_recovery_record_then_verify_and_repair_damage() {
    let a = Area::new();
    let archive = a.s("a.mochi");
    let (code, v) = a.json(&[
        "create",
        &archive,
        "--recovery-record",
        "30",
        &a.file("one", &content(200_000)),
        &a.file("two", b"two"),
    ]);
    assert_eq!(code, CREATED, "{v}");
    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    assert!(
        v["scope"].as_str().unwrap().contains("Redundancy profile"),
        "{v}"
    );

    // Invert 100 bytes of the catalog image.
    let (off, len) = image_range(&archive);
    let mut b = fs::read(&archive).unwrap();
    let mid = (off + len / 2) as usize;
    for x in &mut b[mid..mid + 100] {
        *x ^= 0xFF;
    }
    fs::write(&archive, &b).unwrap();
    let before = fs::read(&archive).unwrap();

    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["dimensions"]["integrity"], "FAIL", "{v}");
    assert_eq!(v["dimensions"]["recoverability"], "DEGRADED", "{v}");

    let (code, out, _) = a.go(&["repair", "plan", &archive]);
    assert_eq!(code, exit::OK, "{out}");
    assert!(
        out.contains("step 6 parity: used (commits checked: 1; damaged shards: 1, rebuilt: 1"),
        "{out}"
    );

    let plan = a.s("plan.json");
    let (code, v) = a.json(&["repair", "plan", &archive, "-o", &plan]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["outcome"], "complete");
    let new = a.s("new.mochi");
    let (code, v) = a.json(&[
        "repair", "apply", &archive, "--plan", &plan, "--output", &new,
    ]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["outcome"], "complete");
    assert_eq!(fs::read(&archive).unwrap(), before, "source untouched");
    let (code, v) = a.json(&["verify", &new]);
    assert_eq!(code, exit::OK, "{v}");
    assert!(
        v["scope"].as_str().unwrap().contains("Redundancy profile"),
        "{v}"
    );
    let out_dir = a.s("out");
    let (code, v) = a.json(&["get", &new, "-C", &out_dir]);
    assert!(code == exit::OK || code == CREATED, "{v}");
    let restored = fs::read(a.p("out").join("one")).unwrap();
    assert_eq!(restored, content(200_000));
}

#[test]
fn c12_recovery_record_percentage_is_checked() {
    let a = Area::new();
    for bad in ["0", "101", "x"] {
        let (code, _, err) = a.go(&[
            "create",
            &a.s("a.mochi"),
            "--recovery-record",
            bad,
            &a.file("one", b"1"),
        ]);
        assert_ne!(code, exit::OK, "{bad}: {err}");
        assert!(!a.p("a.mochi").exists());
    }
}
