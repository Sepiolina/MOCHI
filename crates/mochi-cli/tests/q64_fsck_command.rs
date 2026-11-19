//! C14 end-to-end test for `fsck` and the reference-scope check (Q64; spec
//! Annex B D18), run in-process through `mochi_cli::run` against the real
//! filesystem.
//!
//! The library behaviour is in `mochi-testkit/tests/q64_reference_scope.rs`;
//! this pins what the CLI adds: `fsck` exits 1 with the finding and the two
//! dimensions in JSON, `verify` (not deep) and reading are unaffected, and
//! the archive is left byte-identical.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use mochi_cli::{exit, run};
use mochi_core::catalog::namespace::NamespaceOp;
use mochi_core::publish::{
    open_at_footer, ArchiveWriter, CheckpointPolicy, ReadOptions, Transaction,
};
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::forge::{empty_delta, rule_base, txid, Forge};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};
use serde_json::Value;

fn go(state: &str, args: &[&str]) -> (u8, Value) {
    let mut argv = vec!["mochi", "--state-dir", state, "--json"];
    argv.extend_from_slice(args);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv, &mut out, &mut err);
    let out = String::from_utf8_lossy(&out).into_owned();
    let v = serde_json::from_str(out.trim()).unwrap_or_else(|e| {
        panic!(
            "{args:?}: not JSON ({e}): {out:?} {}",
            String::from_utf8_lossy(&err)
        )
    });
    (code, v)
}

/// Four commits with checkpoints at 0 and 2, then a forged commit 4 that
/// puts the version only commit 0 reaches (as in the library test).
fn forged_archive() -> Vec<u8> {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    for (i, (p, seed)) in [("a", 1), ("a", 2), ("b", 3), ("c", 4)].iter().enumerate() {
        if i == 2 {
            w.request_checkpoint();
        }
        let mut tx = Transaction::new();
        tx.put_file(path(p), deterministic_bytes(*seed, 150), attrs(0o644, 0));
        w.commit(tx, &job.ctx()).unwrap();
    }
    w.close().unwrap();

    let mut f = Forge::new(s.contents());
    let history = f.history();
    let head = history[3].clone();
    let first = open_at_footer(&s, history[0].footer_offset, &ReadOptions::default()).unwrap();
    let old = first
        .catalog
        .replay(None)
        .unwrap()
        .get(&path("a"))
        .unwrap()
        .version;
    let mut m = empty_delta(&head, txid(0x64));
    m.ops.push(NamespaceOp::Put {
        path: path("old-a"),
        version: old,
    });
    let r = f.append_manifest(&m);
    f.append_delta(&head, rule_base(&head), r, txid(0x64));
    f.bytes
}

#[test]
fn q64_fsck_exits_1_with_the_finding_and_verify_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("forged.mochi");
    let state = dir.path().join("state").display().to_string();
    fs::write(&archive, forged_archive()).unwrap();
    let archive = archive.display().to_string();
    let before = fs::read(&archive).unwrap();

    let (code, v) = go(&state, &["fsck", &archive, "--no-local-history"]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["dimensions"]["recoverability"], "FAIL", "{v}");
    assert_eq!(v["dimensions"]["integrity"], "PASS", "{v}");
    let findings = v["findings"].as_array().unwrap();
    let hits: Vec<_> = findings
        .iter()
        .filter(|f| f["code"] == "REFERENCE_INVALID")
        .collect();
    assert_eq!(hits.len(), 1, "{v}");
    assert!(hits[0]["message"].as_str().unwrap().contains("commit 4"));

    // Not deep: the same archive verifies as before, and lists and reads.
    let (code, v) = go(&state, &["verify", &archive, "--no-local-history"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["dimensions"]["recoverability"], "PASS", "{v}");
    let (code, _) = go(&state, &["list", &archive]);
    assert_eq!(code, exit::OK);

    // Verification is read-only.
    assert_eq!(fs::read(&archive).unwrap(), before);
}
