//! W20: the 1.0 flip at the command line (spec §23, §26, Annex B.2.14 D24;
//! plan K25; `docs/cli.md`). Every command but `migrate` refuses a draft
//! archive (`UNSUPPORTED_FEATURE`, exit 4, naming the draft; only draft 2
//! suggests `mochi migrate`), and `mochi migrate` converts the three draft-2
//! archives of `fixtures/golden/r8/legacy/`, one per encryption state and one
//! with a recovery record.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use mochi_cli::{exit, run};
use serde_json::Value;

const PASSPHRASE: &str = "r8 golden passphrase";

/// Exit code of a command that creates a new file: Windows reports its
/// directory entry as not confirmed durable in 1.0 (plan O12, G6 policy B), which
/// is `DEGRADED`.
const CREATED: u8 = if cfg!(windows) {
    exit::DEGRADED
} else {
    exit::OK
};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn legacy(file: &str) -> PathBuf {
    repo().join("fixtures/golden/r8/legacy").join(file)
}

fn r8(file: &str) -> PathBuf {
    repo().join("fixtures/golden/r8").join(file)
}

struct Out {
    code: u8,
    out: String,
    err: String,
}

impl Out {
    fn all(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

fn mochi<S: AsRef<str>>(args: &[S]) -> Out {
    let mut argv: Vec<String> = vec!["mochi".into(), "--no-local-history".into()];
    argv.extend(args.iter().map(|a| a.as_ref().to_owned()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(argv, &mut out, &mut err);
    Out {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8_lossy(&err).into_owned(),
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn passphrase_file(dir: &Path, text: &str) -> String {
    let p = dir.join(format!("pw-{}", text.len()));
    fs::write(&p, format!("{text}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    s(&p)
}

/// Encrypted archives the tests write wrap their key with a tiny Argon2id
/// cost (the core's `test-controls`; the shipped CLI has no knob).
fn cheap_kdf() {
    mochi_core::publish::test_controls::set_default_kdf(Some(mochi_format::kdf::KdfParams {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    }));
}

/// A copy of `from` in a fresh directory, so a command that wrongly wrote
/// would show.
fn copy_of(dir: &Path, from: &Path) -> PathBuf {
    let to = dir.join(from.file_name().unwrap());
    fs::copy(from, &to).unwrap();
    to
}

fn entries(archive: &Path, pass: Option<&str>, dir: &Path) -> Value {
    let mut a = vec!["list".to_owned(), s(archive), "--json".into()];
    if let Some(p) = pass {
        a.push("--passphrase-file".into());
        a.push(passphrase_file(dir, p));
    }
    let o = mochi(&a);
    assert_eq!(o.code, 0, "{}", o.all());
    serde_json::from_str::<Value>(o.out.trim()).unwrap()["entries"].clone()
}

#[test]
fn every_command_but_migrate_refuses_a_draft_2_archive_naming_the_draft() {
    let tmp = tempfile::tempdir().unwrap();
    for file in ["draft2-core-history.mochi", "draft2-red-5pct.mochi"] {
        let a = s(&copy_of(tmp.path(), &legacy(file)));
        let before = fs::read(&a).unwrap();
        let dest = s(&tmp.path().join("dest"));
        let new = s(&tmp.path().join("new.mochi"));
        for cmd in [
            vec!["verify", &a],
            vec!["fsck", &a],
            vec!["list", &a],
            vec!["get", &a, "--stdout", "d/b"],
            vec!["search", &a],
            vec!["snapshot", "list", &a],
            vec!["snapshot", "retain", &a, "1", "--label", "x"],
            vec!["snapshot", "expire", &a, "1", "--confirm"],
            vec!["snapshot", "release", &a, "--label", "x", "--confirm"],
            vec!["restore-test", &a, "--destination", &dest],
            vec!["dump-index", &a],
            vec!["checkpoint", &a],
            vec!["compact", &a, "--output", &new],
            vec!["gc", "plan", &a],
        ] {
            let o = mochi(&cmd);
            assert_eq!(o.code, exit::UNSUPPORTED, "{cmd:?}: {}", o.all());
            let all = o.all();
            assert!(all.contains("UNSUPPORTED_FEATURE"), "{cmd:?}: {all}");
            assert!(all.contains("draft 2"), "{cmd:?}: {all}");
            assert!(all.contains("mochi migrate"), "{cmd:?}: {all}");
        }
        // `append` refuses before it writes a byte.
        let input = tmp.path().join("in.txt");
        fs::write(&input, b"x").unwrap();
        let o = mochi(&["append", &a, &s(&input)]);
        assert_eq!(o.code, exit::UNSUPPORTED, "{}", o.all());
        assert!(o.all().contains("draft 2"));
        assert!(!Path::new(&new).exists() && !Path::new(&dest).exists());
        assert_eq!(fs::read(&a).unwrap(), before, "{file}: never written");
    }
}

/// `repair` is a refusal too, in the form it always has for an archive this
/// build cannot interpret: nothing is recoverable, each lost snapshot says why
/// (`UNSUPPORTED_FEATURE` naming the draft), and nothing is ever converted,
/// with or without the salvage scan.
#[test]
fn repair_never_converts_a_draft_2_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let a = s(&copy_of(tmp.path(), &legacy("draft2-core-history.mochi")));
    for salvage in [false, true] {
        let mut cmd = vec!["repair", "plan", &a, "--json"];
        if salvage {
            cmd.push("--salvage");
        }
        let o = mochi(&cmd);
        assert_eq!(o.code, exit::FAILED, "{}", o.all());
        let v: Value = serde_json::from_str(o.out.trim()).unwrap();
        assert_eq!(v["outcome"], "nothing_recoverable", "salvage {salvage}");
        assert!(v["snapshots"].as_array().is_none_or(Vec::is_empty));
        assert!(o.out.contains("draft 2"));
    }
    // An approved plan cannot convert it either: there is nothing to apply.
    let new = s(&tmp.path().join("fixed.mochi"));
    let plan = s(&tmp.path().join("plan.json"));
    let o = mochi(&["repair", "plan", &a, "--output", &plan]);
    assert!(o.all().contains("draft 2"), "{}", o.all());
    let o = mochi(&["repair", "apply", &a, "--plan", &plan, "--output", &new]);
    assert_ne!(o.code, exit::OK, "{}", o.all());
    assert!(!Path::new(&new).exists());
}

#[test]
fn older_drafts_are_refused_without_the_migrate_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let a = s(&copy_of(
        tmp.path(),
        &repo().join("fixtures/golden/b2/reject-archive-draft-1.mochi"),
    ));
    for cmd in [vec!["verify", &a], vec!["list", &a], vec!["fsck", &a]] {
        let o = mochi(&cmd);
        assert_eq!(o.code, exit::UNSUPPORTED, "{cmd:?}: {}", o.all());
        assert!(o.all().contains("draft 1"), "{cmd:?}: {}", o.all());
        assert!(!o.all().contains("migrate"), "{cmd:?}: {}", o.all());
    }
    // `migrate` itself reads only draft 2: it refuses draft 1 and says why.
    let new = s(&tmp.path().join("new.mochi"));
    let o = mochi(&["migrate", &a, "--output", &new]);
    assert_eq!(o.code, exit::UNSUPPORTED, "{}", o.all());
    assert!(o.all().contains("draft 1") && o.all().contains("draft 2"));
    assert!(!Path::new(&new).exists());
}

#[test]
fn migrate_converts_each_legacy_archive_and_leaves_the_source_alone() {
    cheap_kdf();
    let tmp = tempfile::tempdir().unwrap();
    for (file, twin, pass) in [
        (
            "draft2-core-history.mochi",
            "core-valid-history.mochi",
            None,
        ),
        (
            "draft2-enc-history.mochi",
            "enc-valid-history.mochi",
            Some(PASSPHRASE),
        ),
        ("draft2-red-5pct.mochi", "red-valid-5pct.mochi", None),
    ] {
        let src = copy_of(tmp.path(), &legacy(file));
        let before = fs::read(&src).unwrap();
        let out = tmp.path().join(format!("1.0-{file}"));
        let mut cmd = vec![
            "migrate".to_owned(),
            s(&src),
            "--output".into(),
            s(&out),
            "--json".into(),
        ];
        if let Some(p) = pass {
            cmd.push("--passphrase-file".into());
            cmd.push(passphrase_file(tmp.path(), p));
        }
        let o = mochi(&cmd);
        assert_eq!(o.code, CREATED, "{file}: {}", o.all());
        assert_eq!(o.out.lines().count(), 1, "one JSON value");
        let v: Value = serde_json::from_str(o.out.trim()).unwrap();
        assert_eq!(v["schema"], "mochi-cli-migrate-v1");
        assert_eq!(v["command"], "migrate");
        assert_eq!(v["source_draft"], 2);
        assert_eq!(v["new_ratified"], true);
        assert_eq!(v["source_kept"], true);
        assert_eq!(v["resealed"], pass.is_some());
        assert_eq!(v["commits"].as_array().unwrap().len(), 7);
        assert_ne!(v["new_archive_id"], v["source_archive_id"]);
        assert_eq!(v["source_verification"]["exit_code"], 0);
        assert_eq!(v["new_verification"]["exit_code"], 0);
        assert_eq!(v["comparison"]["retention_equal"], true);
        assert!(v["not_done"].as_str().unwrap().contains("step 7"));

        assert_eq!(fs::read(&src).unwrap(), before, "{file}: never written");

        // The new archive is an ordinary 1.0 archive: it verifies, and lists
        // exactly what the 1.0 R8 case built from the same recipe lists.
        let mut verify = vec!["verify".to_owned(), s(&out), "--json".into()];
        if let Some(p) = pass {
            verify.push("--passphrase-file".into());
            verify.push(passphrase_file(tmp.path(), p));
        }
        let o = mochi(&verify);
        assert_eq!(o.code, exit::OK, "{file}: {}", o.all());
        assert_eq!(
            entries(&out, pass, tmp.path()),
            entries(&r8(twin), pass, tmp.path()),
            "{file}"
        );

        // A text run says what was and was not done, without success styling
        // words the spec forbids.
        let again = tmp.path().join(format!("text-{file}"));
        let mut cmd = vec!["migrate".to_owned(), s(&src), "-o".into(), s(&again)];
        if let Some(p) = pass {
            cmd.push("--passphrase-file".into());
            cmd.push(passphrase_file(tmp.path(), p));
        }
        let o = mochi(&cmd);
        assert_eq!(o.code, CREATED, "{}", o.all());
        for w in ["backed up", "backup is", "safe", "preserved", "secure"] {
            assert!(!o.out.to_lowercase().contains(w), "{w:?}: {}", o.out);
        }
        assert!(o.out.contains("unchanged and kept"), "{}", o.out);
        assert!(o.out.contains("step 7"), "{}", o.out);
    }
}

#[test]
fn a_wrong_or_missing_passphrase_is_key_unavailable_exit_3_and_writes_nothing() {
    cheap_kdf();
    let tmp = tempfile::tempdir().unwrap();
    let src = copy_of(tmp.path(), &legacy("draft2-enc-history.mochi"));
    let out = tmp.path().join("new.mochi");
    let bad = passphrase_file(tmp.path(), "not the passphrase");
    let o = mochi(&[
        "migrate",
        &s(&src),
        "--output",
        &s(&out),
        "--passphrase-file",
        &bad,
    ]);
    assert_eq!(o.code, exit::ERROR, "{}", o.all());
    assert!(o.all().contains("KEY_UNAVAILABLE"), "{}", o.all());
    assert!(!o.all().contains("not the passphrase"));
    // None given, and no terminal to ask on.
    let o = mochi(&["migrate", &s(&src), "--output", &s(&out)]);
    assert_eq!(o.code, exit::ERROR, "{}", o.all());
    assert!(o.all().contains("KEY_UNAVAILABLE"), "{}", o.all());
    assert!(!out.exists());
    assert!(fs::read_dir(tmp.path()).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".mochi.tmp")));

    // A passphrase is never an argument.
    let o = mochi(&[
        "migrate",
        &s(&src),
        "--output",
        &s(&out),
        "--passphrase",
        PASSPHRASE,
    ]);
    assert_eq!(o.code, exit::ERROR);
    assert!(!out.exists());
}

#[test]
fn an_existing_output_is_refused_and_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let src = copy_of(tmp.path(), &legacy("draft2-core-history.mochi"));
    let out = tmp.path().join("new.mochi");
    fs::write(&out, b"precious").unwrap();
    let o = mochi(&["migrate", &s(&src), "--output", &s(&out)]);
    assert_eq!(o.code, exit::ERROR, "{}", o.all());
    assert!(o.all().contains("DESTINATION_EXISTS"), "{}", o.all());
    assert_eq!(fs::read(&out).unwrap(), b"precious");
    // Naming the source itself as the output is the same refusal.
    let o = mochi(&["migrate", &s(&src), "--output", &s(&src)]);
    assert_eq!(o.code, exit::ERROR, "{}", o.all());
    assert!(o.all().contains("DESTINATION_EXISTS"), "{}", o.all());
}

#[test]
fn a_damaged_source_is_refused_never_converted_partially() {
    let tmp = tempfile::tempdir().unwrap();
    let mut bytes = fs::read(legacy("draft2-core-history.mochi")).unwrap();
    // Inside the stored data of the first chunks (after the descriptor).
    let at = bytes.len() / 4;
    bytes[at] ^= 0x55;
    let src = tmp.path().join("damaged.mochi");
    fs::write(&src, &bytes).unwrap();
    let out = tmp.path().join("new.mochi");
    let o = mochi(&["migrate", &s(&src), "--output", &s(&out), "--json"]);
    assert_ne!(o.code, exit::OK, "{}", o.all());
    let v: Value = serde_json::from_str(o.out.trim()).unwrap();
    assert!(v.get("error").is_some(), "{v}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Nothing was written"),
        "{v}"
    );
    assert!(!out.exists());
    assert_eq!(fs::read(&src).unwrap(), bytes);
}

#[test]
fn a_ratified_archive_has_nothing_to_migrate() {
    let tmp = tempfile::tempdir().unwrap();
    let src = copy_of(tmp.path(), &r8("core-valid-history.mochi"));
    let out = tmp.path().join("new.mochi");
    let o = mochi(&["migrate", &s(&src), "--output", &s(&out)]);
    assert_eq!(o.code, exit::ERROR, "{}", o.all());
    assert!(o.all().contains("already a ratified"), "{}", o.all());
    assert!(!out.exists());
}

#[test]
fn the_status_strings_say_1_0_and_the_help_lists_migrate() {
    let o = mochi(&["--version"]);
    assert!(!o.out.contains("draft-compatible"), "{}", o.out);
    assert!(!o.out.contains("proposed"), "{}", o.out);
    assert!(o.out.contains("not a backup"), "{}", o.out);
    let o = mochi(&["--help"]);
    assert!(o.out.contains("migrate"), "{}", o.out);
    assert!(!o.out.contains("draft-compatible"), "{}", o.out);
    let o = mochi(&["migrate", "--help"]);
    assert!(o.out.contains("--output"), "{}", o.out);
}
