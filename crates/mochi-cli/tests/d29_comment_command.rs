//! The archive comment on the command line (spec Annex B.2.19, D29):
//! `create`/`append --comment`, `--comment-file`, `append --clear-comment`,
//! `mochi comment`, and the note `snapshot list` prints where it changes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use serde_json::Value;

/// `create`'s exit code on success: Windows reports the new file's directory
/// entry as not confirmed durable (plan O12, G6 policy B), exit 2, as in
/// `c14_commands.rs`.
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
        let a = Area {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::write(a.p("one.txt"), b"one").unwrap();
        fs::write(a.p("two.txt"), b"two").unwrap();
        a
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
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(argv, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    fn comment(&self, archive: &str, snapshot: Option<&str>) -> Value {
        let mut args = vec!["comment", archive, "--json"];
        if let Some(s) = snapshot {
            args.extend(["--snapshot", s]);
        }
        let (code, out, err) = self.go(&args);
        assert_eq!(code, exit::OK, "{err}");
        serde_json::from_str::<Value>(out.trim()).unwrap()["comment"].clone()
    }
}

#[test]
fn a_comment_is_set_kept_changed_and_cleared_and_each_version_keeps_its_own() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    let one = a.s("one.txt");
    let two = a.s("two.txt");
    let (code, _, err) = a.go(&["create", &arc, &one, "--comment", "first\n\tindented"]);
    assert_eq!(code, CREATED, "{err}");
    // A plain append keeps it.
    assert_eq!(a.go(&["append", &arc, &two]).0, exit::OK);
    // A comment alone is a commit of its own.
    let (code, _, err) = a.go(&["append", &arc, "--comment", "second"]);
    assert_eq!(code, exit::OK, "{err}");
    let (code, _, err) = a.go(&["append", &arc, "--clear-comment"]);
    assert_eq!(code, exit::OK, "{err}");

    assert_eq!(a.comment(&arc, Some("0")), "first\n\tindented");
    assert_eq!(a.comment(&arc, Some("1")), "first\n\tindented");
    assert_eq!(a.comment(&arc, Some("2")), "second");
    assert_eq!(a.comment(&arc, None), Value::Null);

    // Text: the comment as written, line by line; nothing for no comment.
    let (code, out, _) = a.go(&["comment", &arc, "--snapshot", "0"]);
    assert_eq!((code, out.as_str()), (exit::OK, "first\n\tindented\n"));
    assert_eq!(a.go(&["comment", &arc]).1, "");

    // `snapshot list` marks where the comment changes.
    let (_, out, _) = a.go(&["snapshot", "list", &arc]);
    let notes: Vec<bool> = out.lines().map(|l| l.contains("comment set")).collect();
    assert_eq!(notes, [true, false, true, false], "{out}");
    assert!(
        out.lines().nth(3).unwrap().contains("comment cleared"),
        "{out}"
    );

    // The archive verifies as any other.
    let (code, _, err) = a.go(&["verify", &arc]);
    assert_eq!(code, exit::OK, "{err}");
}

#[test]
fn a_comment_file_is_utf8_and_its_windows_line_ends_become_lf() {
    let a = Area::new();
    fs::write(a.p("note.txt"), "line one\r\nline two\r\n").unwrap();
    let arc = a.s("a.mochi");
    let (code, _, err) = a.go(&[
        "create",
        &arc,
        &a.s("one.txt"),
        "--comment-file",
        &a.s("note.txt"),
    ]);
    assert_eq!(code, CREATED, "{err}");
    assert_eq!(a.comment(&arc, None), "line one\nline two\n");

    fs::write(a.p("bad.txt"), [0xffu8, 0xfe, 0x00]).unwrap();
    let (code, _, err) = a.go(&["append", &arc, "--comment-file", &a.s("bad.txt")]);
    assert_eq!(code, exit::ERROR, "{err}");
    assert!(err.contains("not UTF-8"), "{err}");
}

#[test]
fn a_comment_that_breaks_the_rules_is_refused_and_nothing_is_written() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    assert_eq!(a.go(&["create", &arc, &a.s("one.txt")]).0, CREATED);
    let before = fs::read(&arc).unwrap();
    for bad in ["", "a\rb", "\u{1b}[31mred"] {
        let (code, _, err) = a.go(&["append", &arc, "--comment", bad]);
        assert_eq!(code, exit::ERROR, "{bad:?}: {err}");
    }
    let long = "x".repeat(16 * 1024 + 1);
    assert_eq!(a.go(&["append", &arc, "--comment", &long]).0, exit::ERROR);
    assert_eq!(fs::read(&arc).unwrap(), before, "nothing was appended");
    // And a new archive with a bad comment is not created.
    let (code, _, _) = a.go(&["create", &a.s("b.mochi"), &a.s("one.txt"), "--comment", ""]);
    assert_eq!(code, exit::ERROR);
    assert!(!a.p("b.mochi").exists());
}

/// A commit record is readable without the passphrase, so an Encrypted
/// archive cannot have a comment: exit 4, and nothing is created.
#[test]
fn an_encrypted_archive_refuses_a_comment_with_exit_4() {
    let a = Area::new();
    fs::write(a.p("pass"), "a long enough passphrase").unwrap();
    let arc = a.s("e.mochi");
    let (code, _, err) = a.go(&[
        "--passphrase-file",
        &a.s("pass"),
        "create",
        &arc,
        &a.s("one.txt"),
        "--encrypted",
        "--comment",
        "secret",
    ]);
    assert_eq!(code, exit::UNSUPPORTED, "{err}");
    assert!(err.contains("Encrypted"), "{err}");
    assert!(!a.p("e.mochi").exists());
}

/// Invisible formatting characters cannot reorder the terminal: they are
/// written as escapes in text, and kept exactly in JSON.
#[test]
fn bidirectional_overrides_are_escaped_in_text_and_exact_in_json() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    let hostile = "invoice\u{202e}fdp.exe";
    let (code, _, err) = a.go(&["create", &arc, &a.s("one.txt"), "--comment", hostile]);
    assert_eq!(code, CREATED, "{err}");
    assert_eq!(a.go(&["comment", &arc]).1, "invoice\\u{202e}fdp.exe\n");
    assert_eq!(a.comment(&arc, None), hostile);
}
