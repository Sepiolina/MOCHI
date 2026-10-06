//! C14 end-to-end tests: each built command run in-process through
//! `mochi_cli::run` against the real filesystem, with its JSON output and
//! exit code (plan C14 exit: "an end-to-end test per command, including JSON
//! output and exit code").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

use mochi_cli::{exit, run};
use serde_json::Value;

/// An entry name that is hostile to an HTML renderer. Windows cannot hold
/// `<` or `>` in a name, so there it is a quote-and-ampersand variant (the
/// restore engine's Windows name rules are tested in mochi-testkit).
const HOSTILE: &str = if cfg!(windows) {
    "'onerror=alert(1)' & {x}"
} else {
    "<img src=x onerror=alert(1)>"
};

/// `create`'s exit code on success. Windows reports the new file's
/// directory entry as not confirmed durable until gate G6 (plan O12, T21),
/// which is `DEGRADED`, exit 2; never `PASS`-like there.
const CREATED: u8 = if cfg!(windows) {
    exit::DEGRADED
} else {
    exit::OK
};

/// A scratch area: a source tree, an archive path, and a private state dir.
struct Area {
    dir: tempfile::TempDir,
}

impl Area {
    fn new() -> Self {
        let a = Area {
            dir: tempfile::tempdir().unwrap(),
        };
        let src = a.p("src");
        fs::create_dir_all(src.join("docs/deep")).unwrap();
        fs::write(src.join("docs/a.txt"), b"alpha").unwrap();
        fs::write(src.join("docs/deep/b.bin"), vec![7u8; 200_000]).unwrap();
        fs::write(src.join("empty"), b"").unwrap();
        fs::write(src.join(HOSTILE), b"hostile name").unwrap();
        a
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        self.p(rel).display().to_string()
    }

    /// Run `mochi` with the area's state directory.
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

    fn create(&self) -> String {
        let archive = self.s("a.mochi");
        let (code, v) = self.json(&["create", &archive, &self.s("src")]);
        assert_eq!(code, CREATED, "{v}");
        archive
    }
}

fn hash(path: &Path) -> blake3::Hash {
    blake3::hash(&fs::read(path).unwrap())
}

/// **create, list.** The tree is committed (`LOCAL_COMMITTED`, never
/// "backed up"), listed with sizes and hashes; the hostile name is plain
/// data in JSON and in text.
#[test]
fn c14_create_and_list() {
    let a = Area::new();
    let archive = a.s("a.mochi");
    let (code, v) = a.json(&["create", &archive, &a.s("src")]);
    assert_eq!(code, CREATED, "{v}");
    assert_eq!(v["status"], "LOCAL_COMMITTED");
    assert_eq!(v["seq"], 0);
    assert_eq!(v["added"]["files"], 4);
    assert_eq!(v["added"]["directories"], 3);
    assert_eq!(v["added"]["bytes"], 5 + 200_000 + 12);
    assert_eq!(v["exit_code"], CREATED);

    let (code, v) = a.json(&["list", &archive]);
    assert_eq!(code, exit::OK);
    let paths: Vec<&str> = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    let hostile = format!("src/{HOSTILE}");
    assert_eq!(
        paths,
        [
            "src",
            hostile.as_str(),
            "src/docs",
            "src/docs/a.txt",
            "src/docs/deep",
            "src/docs/deep/b.bin",
            "src/empty"
        ]
    );
    let a_txt = &v["entries"][3];
    assert_eq!(a_txt["size"], 5);
    assert_eq!(
        a_txt["content_hash"],
        blake3::hash(b"alpha").to_hex().as_str()
    );

    let (code, out, _) = a.go(&["list", &archive, "src/docs"]);
    assert_eq!(code, exit::OK);
    assert_eq!(out.lines().count(), 4, "{out}");

    let (_, out, _) = a.go(&["create", &a.s("b.mochi"), &a.s("src")]);
    for banned in ["backed up", "safe", "preserved"] {
        assert!(!out.contains(banned), "§23.3 #6 wording: {out}");
    }
}

/// **create never replaces (D13).** An existing file is left byte-identical,
/// exit 3, `DESTINATION_EXISTS`.
#[test]
fn c14_create_refuses_an_existing_destination() {
    let a = Area::new();
    let archive = a.create();
    let before = hash(Path::new(&archive));
    let (code, v) = a.json(&["create", &archive, &a.s("src")]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
    assert_eq!(hash(Path::new(&archive)), before);
}

/// **append, snapshot list, get --snapshot.** Replace a file, delete a
/// subtree, add a file; the history keeps the old snapshot, which reads
/// back exactly.
#[test]
fn c14_append_and_read_history() {
    let a = Area::new();
    let archive = a.create();
    fs::write(a.p("src/docs/a.txt"), b"alpha, revised").unwrap();
    fs::write(a.p("extra.txt"), b"extra").unwrap();
    // Deletions apply first, then the inputs are added: a subtree still on
    // disk would be added again, so the source loses it too.
    fs::remove_dir_all(a.p("src/docs/deep")).unwrap();
    let (code, v) = a.json(&[
        "append",
        &archive,
        &a.s("src"),
        &a.s("extra.txt"),
        "--delete",
        "src/docs/deep",
    ]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 1);
    let deleted: Vec<&str> = v["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(deleted, ["src/docs/deep/b.bin", "src/docs/deep"]);

    let (code, v) = a.json(&["snapshot", "list", &archive]);
    assert_eq!(code, exit::OK);
    let commits = v["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0]["checkpoint"], true);
    assert!(commits[1]["time"].as_str().unwrap().ends_with('Z'));

    let (_, head) = a.json(&["list", &archive]);
    let head_paths: Vec<&str> = head["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(head_paths.contains(&"extra.txt"));
    assert!(!head_paths.contains(&"src/docs/deep"));

    let (code, out, _) = a.go(&["get", &archive, "src/docs/a.txt", "--stdout"]);
    assert_eq!(code, exit::OK);
    assert_eq!(out, "alpha, revised");
    let (code, out, _) = a.go(&[
        "get",
        &archive,
        "src/docs/a.txt",
        "--stdout",
        "--snapshot",
        "0",
    ]);
    assert_eq!(code, exit::OK);
    assert_eq!(out, "alpha");

    let (code, v) = a.json(&["get", &archive, "--snapshot", "0", "-C", &a.s("old")]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(
        fs::read(a.p("old/src/docs/deep/b.bin")).unwrap(),
        vec![7u8; 200_000]
    );

    let (code, v) = a.json(&["get", &archive, "--snapshot", "9", "-C", &a.s("x")]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    let (code, v) = a.json(&["append", &archive, "--delete", "nowhere"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, _) = a.json(&["append", &archive]);
    assert_eq!(code, exit::ERROR, "nothing to append");
}

/// **get.** Everything, or selected subtrees, restored byte-identically;
/// a second extraction into the same place overwrites nothing and reports
/// collisions (exit 2); `--refuse-on-conflict` writes nothing.
#[test]
fn c14_get_restores_and_never_overwrites() {
    let a = Area::new();
    let archive = a.create();
    let out = a.s("out");
    let (code, v) = a.json(&["get", &archive, "-C", &out]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["files"], 4);
    assert_eq!(v["complete"], true);
    for f in ["docs/a.txt", "docs/deep/b.bin", "empty"] {
        assert_eq!(
            hash(&a.p(&format!("out/src/{f}"))),
            hash(&a.p(&format!("src/{f}"))),
            "{f}"
        );
    }

    fs::write(a.p("out/src/docs/a.txt"), b"local edit").unwrap();
    let (code, v) = a.json(&["get", &archive, "-C", &out]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(v["complete"], false);
    assert!(v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "NAME_COLLISION"));
    assert_eq!(fs::read(a.p("out/src/docs/a.txt")).unwrap(), b"local edit");

    let (code, v) = a.json(&["get", &archive, "-C", &out, "--refuse-on-conflict"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "NAME_COLLISION");

    let sel = a.s("sel");
    let (code, v) = a.json(&["get", &archive, "src/docs/a.txt", "src/empty", "-C", &sel]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["files"], 2);
    assert!(a.p("sel/src/docs/a.txt").exists());
    assert!(!a.p("sel/src/docs/deep").exists());

    let (code, _, _) = a.go(&["get", &archive, "src/docs", "--stdout"]);
    assert_eq!(code, exit::ERROR, "a directory is not one file");
}

/// **verify.** A healthy archive passes at the default (restoration)
/// level, read-only (hash before and after); its report is schema v1 with
/// the local-history anchor this client recorded at creation.
#[test]
fn c14_verify_reports_and_is_read_only() {
    let a = Area::new();
    let archive = a.create();
    let before = hash(Path::new(&archive));
    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["level"], "restoration");
    assert_eq!(v["dimensions"]["integrity"], "PASS");
    assert_eq!(v["dimensions"]["freshness"], "PASS");
    assert_eq!(v["freshness_anchor"], "local-history");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(hash(Path::new(&archive)), before);

    for level in ["structural", "referential"] {
        let (code, v) = a.json(&["verify", &archive, "--level", level]);
        assert_eq!(
            code,
            exit::DEGRADED,
            "{level}: integrity is not established"
        );
        assert_eq!(v["dimensions"]["integrity"], "UNKNOWN");
    }
    for level in ["stored", "content"] {
        let (code, _) = a.json(&["verify", &archive, "--level", level]);
        assert_eq!(code, exit::OK, "{level}");
    }
    for level in ["inventory", "search", "disaster-recovery"] {
        let (code, _) = a.json(&["verify", &archive, "--level", level]);
        assert_eq!(code, exit::UNSUPPORTED, "{level}");
    }

    let (code, out, _) = a.go(&["fsck", &archive]);
    assert_eq!(code, exit::OK, "{out}");
    assert!(out.contains("integrity"), "{out}");
    assert_eq!(hash(Path::new(&archive)), before);
}

/// **Freshness end to end (D8).** The archive cut back to its first commit
/// is internally valid, but this client saw commit 1: exit 1,
/// `FRESHNESS_FAILED`. Without local history it is `UNKNOWN` (exit 0, not
/// required). An expected head that is not in the history fails.
#[test]
fn c14_verify_detects_a_rolled_back_archive() {
    let a = Area::new();
    let archive = a.create();
    let first = fs::read(&archive).unwrap();
    fs::write(a.p("more.txt"), b"more").unwrap();
    let (code, _) = a.json(&["append", &archive, &a.s("more.txt")]);
    assert_eq!(code, exit::OK);
    let (_, latest) = a.json(&["snapshot", "list", &archive]);
    let head_id = latest["commits"][1]["commit_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Roll the file back to its first commit's bytes.
    fs::write(&archive, &first).unwrap();
    let (code, v) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["dimensions"]["freshness"], "FAIL");
    assert_eq!(v["dimensions"]["integrity"], "FAIL");
    assert!(v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["code"] == "FRESHNESS_FAILED"));
    // The rolled-back head did not replace the anchor.
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::FAILED);

    let (code, v) = a.json(&["verify", &archive, "--no-local-history"]);
    assert_eq!(code, exit::OK);
    assert_eq!(v["dimensions"]["freshness"], "UNKNOWN");
    assert_eq!(v["freshness_anchor"], "none");

    let (code, v) = a.json(&[
        "verify",
        &archive,
        "--no-local-history",
        "--expected-head",
        &head_id,
    ]);
    assert_eq!(code, exit::FAILED);
    assert_eq!(v["freshness_anchor"], "user");

    let (code, v) = a.json(&["verify", &archive, "--expected-head", "abc"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");

    let (code, _) = a.json(&[
        "verify",
        &archive,
        "--no-local-history",
        "--require-freshness",
    ]);
    assert_eq!(code, exit::DEGRADED);
}

/// **Corrupted content (fault-matrix row, detection) through the CLI.** A
/// flipped byte inside the large file's data: verify exits 1; `get` leaves
/// that file absent (never partial) and exits 1; `--stdout` exits 1.
#[test]
fn c14_damage_is_reported_and_never_extracted() {
    let a = Area::new();
    let archive = a.create();
    let mut bytes = fs::read(&archive).unwrap();
    // The 200,000-byte file of 7s compresses to a run; find a data frame
    // magic and flip a byte well inside the first one.
    let magic = [0x28, 0xB5, 0x2F, 0xFD];
    let at = bytes
        .windows(4)
        .position(|w| w == magic)
        .expect("a data frame");
    bytes[at + 8] ^= 0x01;
    fs::write(&archive, &bytes).unwrap();

    let (code, v) = a.json(&["verify", &archive, "--no-local-history"]);
    assert_eq!(code, exit::FAILED, "{v}");
    assert_eq!(v["dimensions"]["integrity"], "FAIL");

    let (code, v) = a.json(&["get", &archive, "-C", &a.s("out")]);
    assert_eq!(code, exit::FAILED, "{v}");
    let restored: Vec<PathBuf> = [
        "src/docs/a.txt",
        "src/docs/deep/b.bin",
        "src/empty",
        &format!("src/{HOSTILE}"),
    ]
    .iter()
    .map(|f| a.p("out").join(f))
    .filter(|p| p.exists())
    .collect();
    assert_eq!(restored.len(), 3, "exactly the damaged file is absent");
    let failed_reads = [
        "src/docs/a.txt",
        "src/docs/deep/b.bin",
        "src/empty",
        &format!("src/{HOSTILE}"),
    ]
    .iter()
    .filter(|f| a.go(&["get", &archive, f, "--stdout"]).0 == exit::FAILED)
    .count();
    assert_eq!(failed_reads, 1, "--stdout of the damaged file exits 1");
    for p in &restored {
        let rel = p.strip_prefix(a.p("out")).unwrap();
        assert_eq!(hash(p), hash(&a.p(rel.to_str().unwrap())));
    }
}

/// **restore-test.** Needs a new directory; restores everything and checks
/// every file as it goes.
#[test]
fn c14_restore_test() {
    let a = Area::new();
    let archive = a.create();
    let (code, v) = a.json(&["restore-test", &archive, "-C", &a.s("rt")]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["files"], 4);
    assert_eq!(v["command"], "restore-test");
    let (code, v) = a.json(&["restore-test", &archive, "-C", &a.s("rt")]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "DESTINATION_EXISTS");
}

/// **append --truncate-tail (T29, G8).** An interrupted append leaves an
/// eligible tail: a plain append refuses (exit 3, `UNCOMMITTED_TAIL`) and
/// changes nothing; `--truncate-tail` quarantines the tail to a sidecar,
/// truncates it, and commits; `--no-quarantine` is recorded as a waiver.
#[test]
fn c14_append_truncates_an_eligible_tail_only_on_request() {
    let a = Area::new();
    let archive = a.create();
    let good = fs::read(&archive).unwrap();
    // The first half of a data frame copied from the archive: a frame cut
    // short by end of file, which is what an interrupted append leaves.
    let magic = [0x28, 0xB5, 0x2F, 0xFD];
    let at = good.windows(4).position(|w| w == magic).unwrap();
    let mut torn = good.clone();
    torn.extend_from_slice(&good[at..at + 9]);
    fs::write(&archive, &torn).unwrap();
    fs::write(a.p("n.txt"), b"n").unwrap();

    let (code, v) = a.json(&["append", &archive, &a.s("n.txt")]);
    assert_eq!(code, exit::ERROR, "{v}");
    assert_eq!(v["error"]["code"], "UNCOMMITTED_TAIL");
    assert_eq!(fs::read(&archive).unwrap(), torn);

    // Windows cannot confirm the sidecar's directory entry (before G6), so
    // truncation there needs the second, recorded waiver.
    let mut args = vec!["append", archive.as_str(), "--truncate-tail"];
    if cfg!(windows) {
        args.push("--accept-unconfirmed-durability");
    }
    let n = a.s("n.txt");
    args.insert(2, n.as_str());
    let (code, v) = a.json(&args);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["seq"], 1);
    let truncation = v["truncation"].as_array().unwrap();
    assert!(truncation[0]["message"]
        .as_str()
        .unwrap()
        .contains("quarantined"));
    let sidecars: Vec<_> = fs::read_dir(a.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".mochiq"))
        .collect();
    assert_eq!(sidecars.len(), 1);
    let (code, _) = a.json(&["verify", &archive]);
    assert_eq!(code, exit::OK);

    // Again, with the waiver and nothing else to commit.
    let now = fs::read(&archive).unwrap();
    let mut torn = now.clone();
    torn.extend_from_slice(&good[at..at + 9]);
    fs::write(&archive, &torn).unwrap();
    let (code, v) = a.json(&["append", &archive, "--truncate-tail", "--no-quarantine"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["committed"], false);
    assert!(v["truncation"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["message"].as_str().unwrap().contains("--no-quarantine")));
    assert_eq!(fs::read(&archive).unwrap(), now);

    let (code, _, err) = a.go(&["append", &archive, "--no-quarantine"]);
    assert_eq!(code, exit::ERROR, "a waiver needs --truncate-tail: {err}");
}

/// Options this build refuses: TAR compatibility (exit 4), a bad reader
/// limit (exit 3), an unknown level (usage, exit 3). A lowered reader
/// limit applies and makes the archive unreadable to that reader.
#[test]
fn c14_refused_and_limited_options() {
    let a = Area::new();
    let (code, v) = a.json(&["create", &a.s("t.mochi"), &a.s("src"), "--tar-compatible"]);
    assert_eq!(code, exit::UNSUPPORTED, "{v}");
    assert!(!a.p("t.mochi").exists(), "nothing was created");

    let archive = a.create();
    let (code, v) = a.json(&["list", &archive, "--limit", "max-frame-len"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, _) = a.json(&["list", &archive, "--limit", "no-such=1"]);
    assert_eq!(code, exit::ERROR);
    let (code, _, _) = a.go(&["verify", &archive, "--level", "bogus"]);
    assert_eq!(code, exit::ERROR);

    let (code, v) = a.json(&[
        "verify",
        &archive,
        "--no-local-history",
        "--limit",
        "max-decoded-object-len=10",
    ]);
    assert_ne!(code, exit::OK, "{v}");
    assert_ne!(v["dimensions"]["integrity"], "PASS");
}

/// A damaged local head history is reported, not mistaken for first sight
/// silently, and does not stop the command.
#[test]
fn c14_a_damaged_head_history_is_a_warning() {
    let a = Area::new();
    let archive = a.create();
    fs::write(a.p("state/heads.json"), b"{broken").unwrap();
    let (code, out, err) = a.go(&["verify", &archive, "--json"]);
    assert_eq!(code, exit::OK, "{out}");
    assert!(err.contains("warning"), "{err}");
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["freshness_anchor"], "none");
}

/// Symbolic links are skipped, reported, and never followed: exit 2.
#[cfg(unix)]
#[test]
fn c14_symbolic_links_are_skipped_and_reported() {
    let a = Area::new();
    std::os::unix::fs::symlink("/etc/passwd", a.p("src/link")).unwrap();
    let (code, v) = a.json(&["create", &a.s("l.mochi"), &a.s("src")]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(v["durability"], "durable");
    assert_eq!(v["skipped"][0]["path"], "src/link");
    assert_eq!(v["skipped"][0]["kind"], "symbolic link");
    let (_, l) = a.json(&["list", &a.s("l.mochi")]);
    assert!(!l.to_string().contains("src/link"));
}
