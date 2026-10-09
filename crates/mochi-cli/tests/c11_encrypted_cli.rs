//! C11 end to end through `mochi_cli::run`: the Encrypted profile and `rekey`
//! on the real filesystem, with JSON output and exit codes (spec Annex B.2.10
//! D20 items 9 to 12).
//!
//! The KDF is made cheap with `--kdf-memory-kib 64 --kdf-iterations 1
//! --kdf-lanes 1`; the default cost is covered by the format tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use serde_json::Value;

const SECRET_NAME: &str = "very-secret-plans.txt";
const CHEAP: [&str; 6] = [
    "--kdf-memory-kib",
    "64",
    "--kdf-iterations",
    "1",
    "--kdf-lanes",
    "1",
];

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
        let src = a.p("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join(SECRET_NAME), b"the launch code is 0000").unwrap();
        fs::write(src.join("blob"), vec![9u8; 100_000]).unwrap();
        a
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).display().to_string()
    }

    fn pass_file(&self, name: &str, text: &str) -> String {
        let f = self.p(name);
        fs::write(&f, format!("{text}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        }
        f.display().to_string()
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

    /// `a.mochi`, encrypted under the passphrase in file `pass`.
    fn create_encrypted(&self, pass: &str) -> String {
        let archive = self.s("a.mochi");
        let src = self.s("src");
        let mut args = vec![
            "create",
            "--encrypted",
            "--passphrase-file",
            pass,
            &archive,
            &src,
        ];
        args.extend_from_slice(&CHEAP);
        let (code, v) = self.json(&args);
        assert_eq!(code, CREATED, "{v}");
        assert_eq!(v["encrypted"], true);
        archive
    }
}

#[test]
fn create_list_get_search_and_restore_test_with_a_passphrase_file() {
    let a = Area::new();
    let pass = a.pass_file("p1", "correct horse battery staple");
    let archive = a.create_encrypted(&pass);

    // The file holds no name and no content.
    let raw = fs::read(&archive).unwrap();
    let has = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
    assert!(!has(SECRET_NAME.as_bytes()) && !has(b"launch code"));

    let (code, v) = a.json(&["list", &archive, "--passphrase-file", &pass]);
    assert_eq!(code, exit::OK, "{v}");
    let names: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&format!("src/{SECRET_NAME}").as_str()),
        "{names:?}"
    );

    let dest = a.s("out");
    let (code, _, err) = a.go(&["get", &archive, "-C", &dest, "--passphrase-file", &pass]);
    assert_eq!(code, exit::OK, "{err}");
    assert_eq!(
        fs::read(a.p("out/src").join(SECRET_NAME)).unwrap(),
        b"the launch code is 0000"
    );

    let (code, v) = a.json(&["search", &archive, "secret", "--passphrase-file", &pass]);
    assert_eq!(code, exit::OK, "{v}");

    let (code, v) = a.json(&[
        "restore-test",
        &archive,
        "-C",
        &a.s("rt"),
        "--passphrase-file",
        &pass,
    ]);
    assert_eq!(code, exit::OK, "{v}");

    let (code, v) = a.json(&["snapshot", "list", &archive, "--passphrase-file", &pass]);
    assert_eq!(code, exit::OK, "{v}");
}

#[test]
fn no_passphrase_or_a_wrong_one_is_key_unavailable_exit_3_and_nothing_is_written() {
    let a = Area::new();
    let pass = a.pass_file("p1", "right one");
    let archive = a.create_encrypted(&pass);
    let wrong = a.pass_file("p2", "wrong one");

    // No source and no terminal (tests have none): refused, not hung.
    let (code, out, _) = a.go(&["list", &archive, "--json"]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("KEY_UNAVAILABLE"), "{out}");

    let before = fs::read(&archive).unwrap();
    for cmd in [
        vec!["list", &archive],
        vec!["get", &archive, "-C", "/nonexistent-never-created"],
        vec!["append", &archive, "src"],
        vec!["checkpoint", &archive],
    ] {
        let mut args = cmd.clone();
        args.extend_from_slice(&["--passphrase-file", &wrong, "--json"]);
        let (code, out, err) = a.go(&args);
        assert_eq!(code, exit::ERROR, "{cmd:?}: {out} {err}");
        assert!(out.contains("KEY_UNAVAILABLE"), "{cmd:?}: {out}");
        // The wrong passphrase appears nowhere in what was printed.
        assert!(!out.contains("wrong one") && !err.contains("wrong one"));
    }
    assert_eq!(fs::read(&archive).unwrap(), before);
    assert!(!a.p("/nonexistent-never-created").exists());
}

#[test]
fn verify_without_a_passphrase_is_keyless_and_never_a_clean_pass() {
    let a = Area::new();
    let pass = a.pass_file("p1", "right one");
    let archive = a.create_encrypted(&pass);

    let (code, v) = a.json(&[
        "verify",
        &archive,
        "--level",
        "stored",
        "--no-local-history",
    ]);
    assert_eq!(v["dimensions"]["integrity"], "PASS", "{v}");
    assert_eq!(v["dimensions"]["recoverability"], "UNKNOWN");
    assert_eq!(v["dimensions"]["key_availability"], "UNKNOWN");
    assert_ne!(v["overall_status"], "PASS");
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert!(v["scope"].as_str().unwrap().contains("WITHOUT a key"));
    assert!(v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["reason"].as_str().unwrap().contains("no key supplied")));

    // Corruption in a data object is found without a key.
    let mut raw = fs::read(&archive).unwrap();
    let mid = raw.len() / 2;
    raw[mid] ^= 0xFF;
    let bad = a.s("bad.mochi");
    fs::write(&bad, &raw).unwrap();
    let (code, v) = a.json(&["verify", &bad, "--level", "stored", "--no-local-history"]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["dimensions"]["integrity"], "FAIL");

    // With the passphrase: key availability passes, and so does the rest.
    let (code, v) = a.json(&[
        "verify",
        &archive,
        "--passphrase-file",
        &pass,
        "--no-local-history",
    ]);
    assert_eq!(v["dimensions"]["key_availability"], "PASS", "{v}");
    assert_eq!(v["dimensions"]["integrity"], "PASS");
    assert_eq!(v["dimensions"]["recoverability"], "PASS");
    assert_eq!(code, exit::OK, "{v}");
}

#[test]
fn append_compact_and_the_environment_switch() {
    let a = Area::new();
    let pass = a.pass_file("p1", "right one");
    let archive = a.create_encrypted(&pass);
    fs::write(a.p("src").join("more"), b"more content").unwrap();
    let (code, v) = a.json(&[
        "append",
        &archive,
        &a.s("src/more"),
        "--passphrase-file",
        &pass,
    ]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 1);
    assert_eq!(v["encrypted"], true);

    let out = a.s("compact.mochi");
    let mut args = vec!["compact", &archive, "-o", &out, "--passphrase-file", &pass];
    args.extend_from_slice(&CHEAP);
    let (code, v) = a.json(&args);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["resealed"], true);
    assert_ne!(v["new_archive_id"], v["source_archive_id"]);

    // The environment switch is the only way MOCHI_PASSPHRASE is read.
    std::env::set_var(mochi_cli::passphrase::ENV_VAR, "right one");
    let (code, _, _) = a.go(&["list", &out]);
    assert_eq!(code, exit::ERROR, "the variable alone is not read");
    let (code, _, err) = a.go(&["list", &out, "--passphrase-env-for-automation"]);
    assert_eq!(code, exit::OK, "{err}");
    std::env::remove_var(mochi_cli::passphrase::ENV_VAR);
}

#[test]
fn rekey_list_add_remove_and_reencrypt() {
    let a = Area::new();
    let p1 = a.pass_file("p1", "first");
    let p2 = a.pass_file("p2", "second");
    let archive = a.create_encrypted(&p1);

    // --list needs no passphrase.
    let (code, v) = a.json(&["rekey", &archive, "--list"]);
    assert_eq!(code, exit::OK, "{v}");
    let envs = v["envelopes"].as_array().unwrap();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0]["created_at_seq"], 0);
    assert_eq!(envs[0]["kdf"]["algorithm"], "argon2id");
    assert_eq!(envs[0]["kdf"]["memory_kib"], 64);
    let first_id = envs[0]["envelope_id"].as_str().unwrap().to_owned();

    // Add a passphrase: one commit; both open the archive.
    let mut args = vec![
        "rekey",
        &archive,
        "--add-passphrase",
        "--new-passphrase-file",
        &p2,
        "--passphrase-file",
        &p1,
    ];
    args.extend_from_slice(&CHEAP);
    let (code, v) = a.json(&args);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 1);
    assert_eq!(v["change"]["added"], 1);
    assert_eq!(v["change"]["revocation"], false);
    for p in [&p1, &p2] {
        let (code, _, err) = a.go(&["list", &archive, "--passphrase-file", p]);
        assert_eq!(code, exit::OK, "{err}");
    }

    // Remove the first: the head no longer opens with it. Not revocation.
    let (code, v) = a.json(&[
        "rekey",
        &archive,
        "--remove-passphrase",
        &first_id,
        "--passphrase-file",
        &p2,
    ]);
    assert_eq!(code, exit::OK, "{v}");
    assert!(v["change"]["removed"].as_array().unwrap().len() == 1);
    let (code, out, _) = a.go(&["list", &archive, "--passphrase-file", &p1, "--json"]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("KEY_UNAVAILABLE"));
    let (code, _, _) = a.go(&["list", &archive, "--passphrase-file", &p2]);
    assert_eq!(code, exit::OK);

    // Removing the last envelope is refused.
    let (_, v) = a.json(&["rekey", &archive, "--list"]);
    let only = v["envelopes"][0]["envelope_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, out, _) = a.go(&[
        "rekey",
        &archive,
        "--remove-passphrase",
        &only,
        "--passphrase-file",
        &p2,
        "--json",
    ]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("INVALID_ARGUMENT"), "{out}");

    // Re-encrypt into a new archive under a third passphrase.
    let p3 = a.pass_file("p3", "third");
    let new = a.s("new.mochi");
    let mut args = vec![
        "rekey",
        &archive,
        "--reencrypt",
        "-o",
        &new,
        "--new-passphrase-file",
        &p3,
        "--passphrase-file",
        &p2,
    ];
    args.extend_from_slice(&CHEAP);
    let (code, v) = a.json(&args);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["revocation"], false);
    let (code, _, err) = a.go(&["list", &new, "--passphrase-file", &p3]);
    assert_eq!(code, exit::OK, "{err}");
    let (code, out, _) = a.go(&["list", &new, "--passphrase-file", &p2, "--json"]);
    assert_eq!(code, exit::ERROR, "{out}");
    // The source still opens with its own passphrase and is unchanged.
    let (code, _, _) = a.go(&["list", &archive, "--passphrase-file", &p2]);
    assert_eq!(code, exit::OK);
}

#[test]
fn rekey_asks_for_a_choice_and_refuses_a_core_archive() {
    let a = Area::new();
    let core = a.s("core.mochi");
    let (code, v) = a.json(&["create", &core, &a.s("src")]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["encrypted"], false);
    let (code, out, _) = a.go(&["rekey", &core, "--list", "--json"]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("INVALID_ARGUMENT"), "{out}");
    let p = a.pass_file("p", "x");
    let (code, out, _) = a.go(&[
        "rekey",
        &core,
        "--add-passphrase",
        "--new-passphrase-file",
        &p,
        "--json",
    ]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("not an Encrypted archive"), "{out}");
    // --encrypted conflicts with --tar-compatible, and needs a passphrase.
    let (code, _, _) = a.go(&[
        "create",
        "--encrypted",
        "--tar-compatible",
        &a.s("x.mochi"),
        &a.s("src"),
    ]);
    assert_eq!(code, exit::ERROR);
    let (code, out, _) = a.go(&[
        "create",
        "--encrypted",
        &a.s("y.mochi"),
        &a.s("src"),
        "--json",
    ]);
    assert_eq!(code, exit::ERROR);
    assert!(out.contains("needs a passphrase"), "{out}");
    assert!(!a.p("y.mochi").exists(), "nothing was created");
}
