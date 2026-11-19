//! W10 / plan K17 end to end: `mochi verify` on an archive whose newest
//! commit holds a file whose chunk depends on a shared dictionary (1.0
//! writes none, O21). The library behaviour is tested in
//! `mochi-testkit/tests/c7_verify.rs`; this pins what the CLI adds: exit 4,
//! the affected version, path, and snapshot in JSON and in text, and the
//! dangling case as a violation (exit 1).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use mochi_cli::{exit, run};
use mochi_testkit::forge::{put_with_dependency, DictionaryDependency};
use serde_json::Value;

/// `create` exit code: Windows reports a new directory entry as not
/// confirmed durable in 1.0 (plan O12, G6 policy B).
const CREATED: u8 = if cfg!(windows) {
    exit::DEGRADED
} else {
    exit::OK
};

fn go(args: &[&str]) -> (u8, String) {
    let mut argv = vec!["mochi", "--no-local-history"];
    argv.extend_from_slice(args);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv, &mut out, &mut err);
    (
        code,
        String::from_utf8_lossy(&out).into_owned() + &String::from_utf8_lossy(&err),
    )
}

/// A one-commit archive made by the CLI, plus the forged commit 1.
fn forged(dir: &tempfile::TempDir, dep: DictionaryDependency) -> (String, String) {
    let src = dir.path().join("a.txt");
    fs::write(&src, b"alpha").unwrap();
    let archive = dir.path().join("a.mochi").display().to_string();
    let (code, out) = go(&["create", &archive, &src.display().to_string()]);
    assert_eq!(code, CREATED, "{out}");
    let (bytes, id) = put_with_dependency(fs::read(&archive).unwrap(), "dep", dep);
    fs::write(&archive, bytes).unwrap();
    let hex = id.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    (archive, hex)
}

#[test]
fn c14_verify_lists_versions_that_need_a_dictionary() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, version) = forged(&dir, DictionaryDependency::Present);

    let (code, out) = go(&["verify", &archive, "--json"]);
    assert_eq!(code, exit::UNSUPPORTED, "{out}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["exit_code"], 4);
    assert_eq!(v["dimensions"]["integrity"], "UNSUPPORTED", "{v}");
    let listed: Vec<&Value> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "UNSUPPORTED_FEATURE")
        .filter(|f| f["message"].as_str().unwrap().contains(&version))
        .collect();
    assert_eq!(listed.len(), 1, "{v}");
    assert!(listed[0]["message"].as_str().unwrap().contains("\"dep\""));
    assert_eq!(listed[0]["affected"]["first"], 1);
    assert_eq!(listed[0]["affected"]["last"], 1);

    let (code, text) = go(&["verify", &archive]);
    assert_eq!(code, exit::UNSUPPORTED);
    assert!(
        text.contains(&version) && text.contains("\"dep\""),
        "{text}"
    );
}

#[test]
fn c14_verify_fails_a_dangling_dictionary_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (archive, _) = forged(&dir, DictionaryDependency::Missing);
    let (code, out) = go(&["verify", &archive, "--json"]);
    assert_eq!(code, exit::FAILED, "{out}");
    assert!(out.contains("REFERENCE_INVALID"), "{out}");
}
