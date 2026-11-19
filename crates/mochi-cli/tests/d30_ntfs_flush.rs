//! `--trust-ntfs-directory-flush` (spec Annex B.2.20, D30): the user's choice
//! of what a folder flush counts as when an archive is created on Windows.
//! Without it (policy B, the default) Windows reports the new archive's
//! directory entry as unconfirmed, exit 2; with it, on a local NTFS drive (the
//! CI runners' temporary folder), the creation is reported durable, exit 0.
//! Elsewhere the switch changes nothing: `fsync` on a directory is the
//! documented call, and creation exits 0 either way.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use mochi_cli::{exit, run};
use serde_json::Value;

fn go(args: &[&str]) -> (u8, Value) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let mut argv = vec!["mochi", "--no-local-history", "--json"];
    argv.extend_from_slice(args);
    let code = run(argv, &mut out, &mut err);
    let text = String::from_utf8_lossy(&out);
    let v = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {text} {}", String::from_utf8_lossy(&err)));
    (code, v)
}

#[test]
fn creation_is_unconfirmed_on_windows_unless_the_user_trusts_the_ntfs_flush() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("a.txt");
    fs::write(&input, b"a").unwrap();
    let input = input.display().to_string();

    let plain = dir.path().join("plain.mochi").display().to_string();
    let (code, v) = go(&["create", &plain, &input]);
    let trusted = dir.path().join("trusted.mochi").display().to_string();
    let (code_t, v_t) = go(&["--trust-ntfs-directory-flush", "create", &trusted, &input]);

    if cfg!(windows) {
        // Policy B: the default never claims what Windows does not document.
        assert_eq!(code, exit::DEGRADED, "{v}");
        assert_ne!(v["durability"], "durable", "{v}");
        // The user's choice: a local NTFS drive's flush counts as saved.
        assert_eq!(code_t, exit::OK, "{v_t}");
        assert_eq!(v_t["durability"], "durable", "{v_t}");
    } else {
        assert_eq!((code, code_t), (exit::OK, exit::OK), "{v} {v_t}");
        assert_eq!(v["durability"], v_t["durability"]);
    }
}
