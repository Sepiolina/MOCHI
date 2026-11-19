//! `mochi find` (spec Annex B.2.18, D28), end to end through
//! `mochi_cli::run` on the real filesystem: names across the archives under
//! some folders, read-only, with its coverage and exit codes.
//!
//! The walk, the matching and the classification of failures are
//! `mochi_core::find` (unit tests there; the desktop app's Find tests run the
//! same engine). These tests pin what the command promises: hits across
//! nested folders, the three query forms, case folding, every reason an
//! archive is not searched, links not followed, escaped names in text and
//! exact ones in JSON, the exit codes (0 complete whatever matched, 2
//! incomplete, 3 invocation), and that nothing is written.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use mochi_cli::{exit, run};
use mochi_core::commit::Metadata;
use mochi_core::manifest::Attributes;
use mochi_core::publish::{commit_history, ArchiveWriter, ReadOptions, Transaction, WriterOptions};
use mochi_core::storage::os::{OsReadStorage, OsStorage};
use mochi_testkit::archive::{encrypted_options, path as apath, test_options, Job};
use mochi_testkit::SeqIds;
use serde_json::Value;

static SEED: AtomicU64 = AtomicU64::new(7_000);

/// Write an archive holding `files` (with each parent folder as an entry).
fn write_archive(at: &Path, files: &[&str], opts: Option<WriterOptions>) {
    fs::create_dir_all(at.parent().unwrap()).unwrap();
    let mut w = ArchiveWriter::create(
        OsStorage::create_new(at).unwrap(),
        Box::new(SeqIds::new(SEED.fetch_add(1, Ordering::SeqCst))),
        opts.unwrap_or_else(test_options),
    )
    .unwrap();
    let mut tx = Transaction::new();
    let mut dirs = std::collections::BTreeSet::new();
    for f in files {
        let parts: Vec<&str> = f.split('/').collect();
        for i in 1..parts.len() {
            dirs.insert(parts[..i].join("/"));
        }
    }
    for d in &dirs {
        tx.put_dir(apath(d), Attributes::default());
    }
    for f in files {
        tx.put_file(apath(f), f.as_bytes().to_vec(), Attributes::default());
    }
    w.commit(tx, &Job::new().ctx()).unwrap();
    w.close().unwrap();
}

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
        let (mut out, mut err) = (Vec::new(), Vec::new());
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
        assert_eq!(out.lines().count(), 1, "{args:?}: one line: {out:?} {err}");
        let v = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {out:?} {err}"));
        (code, v)
    }

    /// root/{a.mochi, deep/er/b.mochi, other/c.mochi}: three good archives.
    fn good_tree(&self) {
        write_archive(
            &self.p("root/a.mochi"),
            &["Report.pdf", "docs/invoice.txt"],
            None,
        );
        write_archive(
            &self.p("root/deep/er/b.mochi"),
            &["photos/Holiday.JPG", "report-2.txt"],
            None,
        );
        write_archive(&self.p("root/other/C.MOCHI"), &["école/Été.txt"], None);
        fs::write(self.p("root/note.txt"), b"report").unwrap();
    }
}

/// `(archive file name, path)` of each hit.
fn hits(v: &Value) -> Vec<(String, String)> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let a = Path::new(h["archive"].as_str().unwrap())
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (a, h["path"].as_str().unwrap().to_owned())
        })
        .collect()
}

/// `(archive file name, reason)` of each archive not searched.
fn skipped(v: &Value) -> Vec<(String, String)> {
    let mut s: Vec<(String, String)> = v["coverage"]["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                Path::new(s["archive"].as_str().unwrap())
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                s["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    s.sort();
    s
}

/// **Matches across archives in nested folders; the three query forms;
/// case folding.** Every archive is searched, so the exit is 0, with or
/// without matches.
#[test]
fn c16_find_matches_names_across_nested_archives() {
    let a = Area::new();
    a.good_tree();
    let root = a.s("root");

    let (code, v) = a.json(&["find", "REPORT", &root]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(
        hits(&v),
        [
            ("a.mochi".to_owned(), "Report.pdf".to_owned()),
            ("b.mochi".to_owned(), "report-2.txt".to_owned())
        ]
    );
    assert_eq!(v["schema"], "mochi-cli-find-v1");
    assert_eq!(v["total_hits"], 2);
    let c = &v["coverage"];
    assert_eq!(c["complete"], true);
    assert_eq!(
        c["archives_found"], 3,
        "C.MOCHI counts: the extension is any case"
    );
    assert_eq!(c["archives_searched"], 3);
    assert_eq!(c["archives_skipped"], 0);
    assert_eq!(c["content_searched"], false);
    assert_eq!(c["limit"], Value::Null);
    assert_eq!(v["query"]["match"], "substring");
    assert_eq!(v["query"]["whole_path"], false);
    assert_eq!(v["scopes"].as_array().unwrap().len(), 1);
    let h = &v["hits"][0];
    assert_eq!(
        (h["kind"].as_str(), h["size"].as_u64()),
        (Some("file"), Some(10))
    );

    // A glob covers the whole last component.
    let (_, v) = a.json(&["find", "*.jpg", &root]);
    assert_eq!(hits(&v), [("b.mochi".into(), "photos/Holiday.JPG".into())]);
    assert_eq!(v["query"]["match"], "glob");
    let (_, v) = a.json(&["find", "report-?.txt", &root]);
    assert_eq!(hits(&v), [("b.mochi".into(), "report-2.txt".into())]);
    let (_, v) = a.json(&["find", "ep*", &root]);
    assert_eq!(v["total_hits"], 0);
    // Folders are names too.
    let (_, v) = a.json(&["find", "photos", &root]);
    assert_eq!(v["hits"][0]["kind"], "directory");
    assert_eq!(v["hits"][0]["size"], 0);
    // A slash: the whole path inside the archive.
    let (_, v) = a.json(&["find", "docs/inv", &root]);
    assert_eq!(hits(&v), [("a.mochi".into(), "docs/invoice.txt".into())]);
    assert_eq!(v["query"]["whole_path"], true);
    let (_, v) = a.json(&["find", "inv/docs", &root]);
    assert_eq!(v["total_hits"], 0);
    // Unicode case folding.
    let (_, v) = a.json(&["find", "ÉTÉ", &root]);
    assert_eq!(hits(&v), [("C.MOCHI".into(), "école/Été.txt".into())]);

    // Several folders; one inside another is walked once.
    let (code, v) = a.json(&["find", "*", &a.s("root/deep"), &root, &a.s("root/other")]);
    assert_eq!(code, exit::OK);
    assert_eq!(v["scopes"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["coverage"]["archives_found"], 3);

    // Text: one line per hit, then the coverage.
    let (code, out, _) = a.go(&["find", "report", &root]);
    assert_eq!(code, exit::OK);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].ends_with("a.mochi: Report.pdf"), "{out}");
    assert!(lines[1].ends_with("b.mochi: report-2.txt"), "{out}");
    assert!(
        lines[2].contains("2 matches") && lines[2].contains("coverage complete"),
        "{out}"
    );
    let (_, out, _) = a.go(&["find", "photos", &root]);
    assert!(
        out.lines().next().unwrap().ends_with("b.mochi: photos/"),
        "{out}"
    );
}

/// **A complete search with no match says "no matches" and exits 0**; an
/// empty folder is complete with nothing in it.
#[test]
fn c16_find_zero_matches_with_complete_coverage_exits_0() {
    let a = Area::new();
    a.good_tree();
    let (code, out, _) = a.go(&["find", "zzz-nothing", &a.s("root")]);
    assert_eq!(code, exit::OK);
    assert!(out.starts_with("no matches; coverage complete"), "{out}");
    assert!(out.contains("file content was not searched"), "{out}");

    fs::create_dir_all(a.p("empty")).unwrap();
    let (code, v) = a.json(&["find", "x", &a.s("empty")]);
    assert_eq!(code, exit::OK);
    assert_eq!(v["coverage"]["complete"], true);
    assert_eq!(v["coverage"]["archives_found"], 0);
    assert_eq!(v["coverage"]["folders_visited"], 1);
    assert_eq!(v["hits"], serde_json::json!([]));
}

/// **Every archive that cannot be searched is reported with its reason, and
/// the search is then incomplete (exit 2), never "no matches"**: Encrypted
/// (no passphrase in find), truncated, not MOCHI, a draft-2 archive.
#[test]
fn c16_find_reports_what_it_could_not_search() {
    let a = Area::new();
    a.good_tree();
    write_archive(
        &a.p("root/enc/secret.mochi"),
        &["hidden-report.txt"],
        Some(encrypted_options(&["pw one"])),
    );
    write_archive(&a.p("root/whole.mochi"), &["x.txt"], None);
    let bytes = fs::read(a.p("root/whole.mochi")).unwrap();
    fs::remove_file(a.p("root/whole.mochi")).unwrap();
    fs::write(a.p("root/cut.mochi"), &bytes[..bytes.len() / 2]).unwrap();
    fs::write(a.p("root/fake.mochi"), b"this is not an archive at all").unwrap();
    fs::write(a.p("root/tiny.mochi"), b"").unwrap();
    let draft = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden/r8/legacy/draft2-core-history.mochi");
    fs::copy(&draft, a.p("root/old.mochi")).unwrap();

    let (code, v) = a.json(&["find", "report", &a.s("root")]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(
        v["total_hits"], 2,
        "the readable archives are still searched"
    );
    let c = &v["coverage"];
    assert_eq!(c["complete"], false);
    assert_eq!(c["archives_found"], 8);
    assert_eq!(c["archives_searched"], 3);
    assert_eq!(c["archives_skipped"], 5);
    assert_eq!(
        skipped(&v),
        [
            ("cut.mochi".to_owned(), "damaged".to_owned()),
            ("fake.mochi".to_owned(), "not_mochi".to_owned()),
            ("old.mochi".to_owned(), "unsupported".to_owned()),
            ("secret.mochi".to_owned(), "encrypted".to_owned()),
            ("tiny.mochi".to_owned(), "not_mochi".to_owned()),
        ]
    );
    for s in c["skipped"].as_array().unwrap() {
        match s["reason"].as_str().unwrap() {
            "damaged" | "unsupported" => {
                assert!(s["code"].is_string(), "{s}");
                assert!(s["message"].is_string(), "{s}");
            }
            _ => assert_eq!(
                (s["code"].clone(), s["message"].clone()),
                (Value::Null, Value::Null)
            ),
        }
        if s["reason"] == "unsupported" {
            assert_eq!(s["code"], "UNSUPPORTED_FEATURE");
        }
    }

    // Zero matches under incomplete coverage: never "no matches".
    let (code, out, _) = a.go(&["find", "zzz-nothing", &a.s("root")]);
    assert_eq!(code, exit::DEGRADED);
    assert!(!out.contains("no matches"), "{out}");
    assert!(out.contains("0 matches"), "{out}");
    assert!(out.contains("coverage INCOMPLETE"), "{out}");
    assert!(
        out.contains("not proof that no matching name exists"),
        "{out}"
    );
    for (name, why) in [
        ("secret.mochi", "Encrypted"),
        ("cut.mochi", "damaged"),
        ("fake.mochi", "not a MOCHI archive"),
        ("old.mochi", "UNSUPPORTED_FEATURE"),
    ] {
        assert!(
            out.lines()
                .any(|l| l.starts_with("  not searched: ") && l.contains(name) && l.contains(why)),
            "{name}: {out}"
        );
    }
    for word in ["safe", "backed up", "preserved", "verified"] {
        assert!(!out.contains(word), "{word}: {out}");
    }
}

/// **A passphrase is never used by find**: naming one is refused, so nobody
/// believes an Encrypted archive was searched with it.
#[test]
fn c16_find_takes_no_passphrase() {
    let a = Area::new();
    a.good_tree();
    let pass = a.p("pass.txt");
    fs::write(&pass, "pw one\n").unwrap();
    let (code, v) = a.json(&[
        "--passphrase-file",
        &pass.display().to_string(),
        "find",
        "x",
        &a.s("root"),
    ]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, _) = a.json(&["--passphrase-env-for-automation", "find", "x", &a.s("root")]);
    assert_eq!(code, exit::ERROR);
}

/// **A damaged catalog.** With only the head's catalog image damaged, the
/// open rebuilds the catalog from the snapshot manifest (as every open does)
/// and the names are searched; with the manifest damaged too, the archive is
/// reported as damaged, with its code, and nothing crashes.
#[test]
fn c16_find_damaged_catalog_is_reported_not_fatal() {
    let a = Area::new();
    let arc = a.p("root/a.mochi");
    write_archive(&arc, &["Report.pdf", "docs/invoice.txt"], None);
    let (image, snapshot) = {
        let src = OsReadStorage::open(&arc).unwrap();
        let h = commit_history(&src, &ReadOptions::default()).unwrap();
        match h[0].commit.metadata {
            Metadata::Checkpoint { image, snapshot } => (image, snapshot),
            Metadata::Delta { .. } => panic!("the first commit is a checkpoint"),
        }
    };
    let mut bytes = fs::read(&arc).unwrap();
    bytes[(image.offset + image.stored_len / 2) as usize] ^= 0x40;
    fs::write(&arc, &bytes).unwrap();
    let (code, v) = a.json(&["find", "report", &a.s("root")]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(hits(&v), [("a.mochi".into(), "Report.pdf".into())]);

    bytes[(snapshot.offset + snapshot.stored_len / 2) as usize] ^= 0x40;
    fs::write(&arc, &bytes).unwrap();
    let (code, v) = a.json(&["find", "report", &a.s("root")]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(skipped(&v), [("a.mochi".into(), "damaged".into())]);
    let s = &v["coverage"]["skipped"][0];
    assert!(s["code"].as_str().is_some_and(|c| !c.is_empty()), "{s}");
}

/// **Hostile names**: escaped in text (one inert line each), exact in JSON.
#[cfg(unix)]
#[test]
fn c16_find_escapes_hostile_names() {
    let a = Area::new();
    write_archive(
        &a.p("root/h.mochi"),
        &[
            "\u{1b}[31mred.txt",
            "evil\u{202e}gpj.exe",
            "tab\tnew\nline.txt",
        ],
        None,
    );
    // A hostile archive file name on disk is escaped too.
    write_archive(&a.p("root/x\u{1b}]8;;.mochi"), &["plain.txt"], None);
    let (code, out, _) = a.go(&["find", "*", &a.s("root")]);
    assert_eq!(code, exit::OK);
    assert!(
        !out.contains('\u{1b}') && !out.contains('\u{202e}') && !out.contains('\t'),
        "{out:?}"
    );
    assert!(out.contains("\\u{1b}[31mred.txt"), "{out}");
    assert!(out.contains("evil\\u{202e}gpj.exe"), "{out}");
    assert!(out.contains("tab\\u{9}new\\u{a}line.txt"), "{out}");
    assert!(out.contains("x\\u{1b}]8;;.mochi: plain.txt"), "{out}");
    assert_eq!(out.lines().count(), 5, "4 hits and the coverage: {out}");

    let (_, v) = a.json(&["find", "*", &a.s("root")]);
    let names: Vec<String> = hits(&v).into_iter().map(|h| h.1).collect();
    assert!(
        names.contains(&"tab\tnew\nline.txt".to_owned()),
        "{names:?}"
    );
    assert!(
        names.contains(&"evil\u{202e}gpj.exe".to_owned()),
        "{names:?}"
    );
}

/// **Links are counted, never followed**; a loop cannot hang the walk.
#[cfg(unix)]
#[test]
fn c16_find_does_not_follow_links() {
    let a = Area::new();
    write_archive(&a.p("root/sub/a.mochi"), &["a-name.txt"], None);
    write_archive(&a.p("outside/o.mochi"), &["outside-name.txt"], None);
    std::os::unix::fs::symlink(a.p("root"), a.p("root/sub/loop")).unwrap();
    std::os::unix::fs::symlink(a.p("outside"), a.p("root/link-out")).unwrap();
    std::os::unix::fs::symlink(a.p("outside/o.mochi"), a.p("root/o-link.mochi")).unwrap();
    let (code, v) = a.json(&["find", "name", &a.s("root")]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(hits(&v), [("a.mochi".into(), "a-name.txt".into())]);
    assert_eq!(v["coverage"]["links_not_followed"], 3);
    assert_eq!(v["coverage"]["archives_found"], 1);
    let (_, out, _) = a.go(&["find", "outside-name", &a.s("root")]);
    assert!(
        out.contains("3 links to folders or archives were not followed"),
        "{out}"
    );
    assert!(!out.contains("no matches"), "{out}");

    // A folder named by the user may itself be a link: it is the scope.
    let (code, v) = a.json(&["find", "outside", &a.s("root/link-out")]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["total_hits"], 1);
}

/// **Invocation errors exit 3**: a folder that does not exist, a file, an
/// empty or overlong name, no folder at all.
#[test]
fn c16_find_invocation_errors() {
    let a = Area::new();
    a.good_tree();
    for args in [
        vec!["find", "x", "MISSING"],
        vec!["find", "x", "ROOT/a.mochi"],
        vec!["find", "  ", "ROOT"],
    ] {
        let args: Vec<String> = args
            .iter()
            .map(|s| {
                s.replace("MISSING", &a.s("missing"))
                    .replace("ROOT", &a.s("root"))
            })
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, v) = a.json(&args);
        assert_eq!(code, exit::ERROR, "{args:?}");
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{args:?}");
    }
    let long = "a".repeat(mochi_core::find::MAX_QUERY_BYTES + 1);
    let (code, _) = a.json(&["find", &long, &a.s("root")]);
    assert_eq!(code, exit::ERROR);
    let (code, _, err) = a.go(&["find", "x"]);
    assert_eq!(code, exit::ERROR, "{err}");
}

/// **Read-only**: no archive or folder is changed and nothing is written
/// beside them or in the state directory.
#[test]
fn c16_find_writes_nothing() {
    let a = Area::new();
    a.good_tree();
    let snapshot = |root: &Path| {
        let mut all = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let p = e.path();
                if e.file_type().unwrap().is_dir() {
                    stack.push(p);
                } else {
                    all.push((p.clone(), fs::read(&p).unwrap()));
                }
            }
        }
        all.sort();
        all
    };
    let before = snapshot(a.dir.path());
    let (code, _) = a.json(&["find", "*", &a.s("root")]);
    assert_eq!(code, exit::OK);
    let (code, _, _) = a.go(&["find", "report", &a.s("root")]);
    assert_eq!(code, exit::OK);
    assert_eq!(snapshot(a.dir.path()), before);
    assert!(!a.p("state").exists(), "find records no local history");
}

/// **Several names and the options (spec Annex B.2.21).** `--name` (each
/// positional is then a folder) and `--names-from`; each hit says which names
/// it matched and each name what it found; `--match-case`, `--whole-name`,
/// `--files-only`, `--folders-only`, `--no-subfolders`. The output is
/// `mochi-cli-find-v2`; the plain `PATTERN FOLDER...` form stays v1.
#[test]
fn c16_find_several_names_and_options() {
    let a = Area::new();
    a.good_tree();
    let root = a.s("root");
    let (code, v) = a.json(&[
        "find",
        "--name",
        "report",
        "--name",
        "*.jpg",
        "--name",
        "missing.doc",
        "--name",
        "REPORT",
        &root,
    ]);
    assert_eq!(code, exit::OK);
    assert_eq!(v["schema"], "mochi-cli-find-v2");
    // A repeat (case ignored) is asked once.
    let terms: Vec<(String, u64, u64)> = v["query"]["terms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["text"].as_str().unwrap().to_owned(),
                t["hits"].as_u64().unwrap(),
                t["archives"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        terms,
        [
            ("report".to_owned(), 2, 2),
            ("*.jpg".to_owned(), 1, 1),
            ("missing.doc".to_owned(), 0, 0)
        ]
    );
    assert_eq!(v["query"]["terms"][1]["match"], "glob");
    assert_eq!(v["total_hits"], 3);
    let jpg = v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["path"] == "photos/Holiday.JPG")
        .unwrap();
    assert_eq!(jpg["terms"], serde_json::json!([1]));
    assert_eq!(v["query"]["subfolders"], true);

    // Text: each name's result, "not found" only because everything was searched.
    let (code, out, _) = a.go(&["find", "--name", "report", "--name", "missing.doc", &root]);
    assert_eq!(code, exit::OK);
    assert!(
        out.contains("name report: 2 matches in 2 archives"),
        "{out}"
    );
    assert!(out.contains("name missing.doc: not found\n"), "{out}");
    assert!(out.contains("for 1 of 2 names"), "{out}");

    // Options.
    let paths = |args: &[&str]| -> Vec<String> {
        let (code, v) = a.json(args);
        assert_eq!(code, exit::OK, "{args:?} {v}");
        assert_eq!(v["schema"], "mochi-cli-find-v2");
        let mut p: Vec<String> = hits(&v).into_iter().map(|(_, p)| p).collect();
        p.sort();
        p
    };
    assert_eq!(
        paths(&["find", "--match-case", "Report", &root]),
        ["Report.pdf"]
    );
    assert_eq!(
        paths(&[
            "find",
            "--whole-name",
            "--name",
            "report.PDF",
            "--name",
            "report",
            &root
        ]),
        ["Report.pdf"]
    );
    assert_eq!(
        paths(&[
            "find",
            "--files-only",
            "--name",
            "docs",
            "--name",
            "photos",
            &root
        ]),
        Vec::<String>::new()
    );
    assert_eq!(
        paths(&[
            "find",
            "--folders-only",
            "--name",
            "docs",
            "--name",
            "photos",
            &root
        ]),
        ["docs", "photos"]
    );
    // Only the archives directly in the folder: a choice, still complete.
    let (code, v) = a.json(&["find", "--no-subfolders", "*", &root]);
    assert_eq!(code, exit::OK);
    assert_eq!(v["coverage"]["archives_found"], 1);
    assert_eq!(v["coverage"]["complete"], true);
    assert_eq!(v["query"]["subfolders"], false);

    // Names from a file, one per line.
    fs::write(a.p("names.txt"), "invoice\r\n\r\n  holiday.jpg  \nnope\n").unwrap();
    let (code, v) = a.json(&["find", "--names-from", &a.s("names.txt"), &root]);
    assert_eq!(code, exit::OK);
    let texts: Vec<&str> = v["query"]["terms"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["invoice", "holiday.jpg", "nope"]);
    assert_eq!(v["total_hits"], 2);

    // The plain form is v1, unchanged.
    let (_, v) = a.json(&["find", "report", &root]);
    assert_eq!(v["schema"], "mochi-cli-find-v1");
}

/// **A name not found while the search is incomplete is "not found in what
/// was searched"**, and invocation errors of the new form exit 3.
#[test]
fn c16_find_several_names_incomplete_and_errors() {
    let a = Area::new();
    a.good_tree();
    fs::write(a.p("root/fake.mochi"), b"not an archive").unwrap();
    let root = a.s("root");
    let (code, out, _) = a.go(&["find", "--name", "nope", "--name", "report", &root]);
    assert_eq!(code, exit::DEGRADED);
    assert!(
        out.contains("name nope: not found in what was searched"),
        "{out}"
    );
    assert!(out.contains("coverage INCOMPLETE"), "{out}");

    // No folder after --name; an empty names file; too many names; files and folders only.
    for args in [
        vec!["find".to_owned(), "--name".into(), "x".into()],
        vec![
            "find".to_owned(),
            "--names-from".into(),
            a.s("empty.txt"),
            root.clone(),
        ],
    ] {
        fs::write(a.p("empty.txt"), "\n\n").unwrap();
        let (code, _, err) = a.go(&args.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(code, exit::ERROR, "{args:?} {err}");
    }
    let many: String = (0..=mochi_core::find::MAX_TERMS)
        .map(|i| format!("n{i}\n"))
        .collect();
    fs::write(a.p("many.txt"), many).unwrap();
    let (code, v) = a.json(&["find", "--names-from", &a.s("many.txt"), &root]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT");
    let (code, _, _) = a.go(&["find", "--files-only", "--folders-only", "x", &root]);
    assert_eq!(code, exit::ERROR);
}
