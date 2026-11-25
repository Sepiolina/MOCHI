//! Spec Annex B.2.23 item 3 on the command line: `--move-sources-to-bin`.
//!
//! Run in-process through `mochi_cli::run_with_bin` with a stand-in Bin, so
//! these tests never touch the machine's Recycle Bin or Trash. The rules
//! themselves (read back, source unchanged, folders once empty) are tested in
//! `mochi-testkit/tests/b2_23_sources.rs`; these pin what the CLI adds: off
//! unless asked, exit 0 when everything moved, exit 2 with every kept source
//! listed when something could not go (here: a Bin that refuses), and the
//! commit standing either way.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mochi_cli::{exit, run_with_bin};
use serde_json::Value;

struct Area {
    dir: tempfile::TempDir,
}

impl Area {
    fn new() -> Self {
        let a = Area {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(a.p("src/inner")).unwrap();
        fs::write(a.p("src/one.txt"), b"one").unwrap();
        fs::write(a.p("src/inner/two.txt"), b"two").unwrap();
        fs::write(a.p("loose.txt"), b"loose").unwrap();
        a
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).display().to_string()
    }

    /// Run with `bin` as the Bin; the state directory is the area's own.
    fn go(&self, args: &[&str], bin: Arc<mochi_core::storage::os::SetAside>) -> (u8, String) {
        let state = self.s("state");
        let mut argv: Vec<OsString> = vec!["mochi".into(), "--state-dir".into(), state.into()];
        argv.extend(args.iter().map(OsString::from));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_with_bin(argv, &mut out, &mut err, bin);
        (code, String::from_utf8(out).unwrap())
    }

    /// A Bin that moves into `bin/` under the area.
    fn folder_bin(&self) -> Arc<mochi_core::storage::os::SetAside> {
        let dir = self.p("bin");
        Arc::new(move |path: &Path| {
            fs::create_dir_all(&dir)?;
            let to = dir.join(path.file_name().unwrap_or_default());
            fs::rename(path, to)
        })
    }
}

/// A Bin that always refuses, as on a system without one.
fn no_bin() -> Arc<mochi_core::storage::os::SetAside> {
    Arc::new(|_: &Path| Err(std::io::Error::other("no Trash on this system")))
}

fn json(out: &str) -> Value {
    serde_json::from_str(out.trim()).unwrap()
}

#[test]
fn b2_23_moves_every_source_to_the_bin_once_the_archive_holds_it() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    let (code, out) = a.go(
        &[
            "create",
            &arc,
            &a.s("src"),
            &a.s("loose.txt"),
            "--move-sources-to-bin",
            "--json",
        ],
        a.folder_bin(),
    );
    let v = json(&out);
    // Windows reports a new file's directory entry as unconfirmed (exit 2).
    if !cfg!(windows) {
        assert_eq!(code, exit::OK, "{v}");
    }
    assert_eq!(v["schema"], "mochi-cli-commit-v2");
    // src, src/inner, two files in them, and loose.txt.
    assert_eq!(v["sources_moved"], 5, "{v}");
    assert_eq!(v["sources_kept"], Value::Array(Vec::new()));
    assert!(!a.p("src").exists() && !a.p("loose.txt").exists());
    // Files first, one by one, then each emptied folder: in the Bin, not
    // deleted.
    assert_eq!(fs::read(a.p("bin/two.txt")).unwrap(), b"two");
    assert_eq!(fs::read(a.p("bin/one.txt")).unwrap(), b"one");
    assert!(a.p("bin/src").is_dir() && a.p("bin/inner").is_dir());
    assert!(a.p("bin/loose.txt").exists());

    // The archive holds them: they come back out.
    let (code, out) = a.go(
        &["get", &arc, "src/inner/two.txt", "-C", &a.s("back")],
        a.folder_bin(),
    );
    assert_eq!(code, exit::OK, "{out}");
    assert_eq!(fs::read(a.p("back/src/inner/two.txt")).unwrap(), b"two");
}

#[test]
fn b2_23_off_by_default_and_the_output_stays_v1() {
    let a = Area::new();
    let (_, out) = a.go(
        &["create", &a.s("a.mochi"), &a.s("src"), "--json"],
        a.folder_bin(),
    );
    let v = json(&out);
    assert_eq!(v["schema"], "mochi-cli-commit-v1");
    assert!(v.get("sources_moved").is_none());
    assert!(
        a.p("src/one.txt").exists(),
        "nothing moved without the switch"
    );
    assert!(!a.p("bin").exists());
}

#[test]
fn b2_23_a_bin_that_refuses_keeps_every_source_and_exits_2() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    a.go(&["create", &arc, &a.s("loose.txt")], a.folder_bin());
    fs::write(a.p("loose.txt"), b"loose again").unwrap();
    let (code, out) = a.go(
        &["append", &arc, &a.s("src"), "--move-sources-to-bin"],
        no_bin(),
    );
    assert_eq!(code, exit::DEGRADED, "{out}");
    assert!(out.contains("moved 0 sources"), "{out}");
    assert!(
        out.contains("kept src/one.txt: it could not be moved to the Bin: no Trash on this system"),
        "{out}"
    );
    // A folder whose files stayed is kept for that reason.
    assert!(
        out.contains("kept src: something in it is kept")
            || out.contains("kept src: it is not empty"),
        "{out}"
    );
    assert!(
        a.p("src/inner/two.txt").exists(),
        "every source is still there"
    );
    // The commit stands.
    let (code, out) = a.go(&["list", &arc], a.folder_bin());
    assert_eq!(code, exit::OK);
    assert!(out.contains("src/inner/two.txt"), "{out}");
}
