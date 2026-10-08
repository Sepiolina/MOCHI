//! C13/C14 end-to-end tests for `mochi search`, run in-process through
//! `mochi_cli::run` against the real filesystem, with JSON output and exit
//! code (plan C14 exit).
//!
//! The matching rules and coverage are tested in
//! `mochi-testkit/tests/c13_search.rs`; these tests pin what the CLI adds:
//! the scope and criteria arguments, the JSON fields of spec §19.3, the exit
//! code for partial coverage, and the refusal of content search.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, run};
use serde_json::Value;

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

    fn file(&self, rel: &str, content: &[u8]) -> String {
        let p = self.p(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p.display().to_string()
    }

    /// 0: `docs/{Invoice.pdf,notes.txt}`; 1: `docs/notes.txt` replaced;
    /// 2: `docs/` deleted with everything under it, `copy.pdf` added
    /// (same bytes as the invoice).
    fn history(&self) -> String {
        let archive = self.s("a.mochi");
        self.file("src/docs/Invoice.pdf", b"invoice bytes");
        self.file("src/docs/notes.txt", b"notes v0");
        let (code, v) = self.json(&["create", &archive, &self.s("src/docs")]);
        assert_eq!(code, CREATED, "{v}");
        self.file("src/docs/notes.txt", b"notes v1");
        let (code, v) = self.json(&["append", &archive, &self.s("src/docs")]);
        assert_eq!(code, exit::OK, "{v}");
        let copy = self.file("src/copy.pdf", b"invoice bytes");
        let (code, v) = self.json(&["append", &archive, &copy, "--delete", "docs"]);
        assert_eq!(code, exit::OK, "{v}");
        archive
    }
}

fn paths(v: &Value) -> Vec<(u64, String)> {
    v["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["seq"].as_u64().unwrap(),
                h["path"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pairs(v: &[(u64, &str)]) -> Vec<(u64, String)> {
    v.iter().map(|(s, p)| (*s, p.to_string())).collect()
}

#[test]
fn c14_search_reports_hits_and_complete_coverage() {
    let a = Area::new();
    let archive = a.history();

    // Head: the invoice is gone; zero hits with complete coverage exit 0.
    let (code, v) = a.json(&["search", &archive, "Invoice"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(paths(&v), Vec::<(u64, String)>::new());
    let c = &v["coverage"];
    assert_eq!(c["complete"], true);
    assert_eq!(c["requested"], serde_json::json!([2]));
    assert_eq!(c["indexed_seq"], 2);
    assert_eq!(c["catalog_source"], "image");
    assert_eq!(c["full_text"], "not-built");
    for k in ["pending", "failed", "unsupported", "excluded"] {
        assert_eq!(c[k], 0, "{k}");
    }
    assert_eq!(v["scope"], "head");

    let (code, v) = a.json(&["search", &archive, "invoice", "-i", "--snapshot", "all"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(
        paths(&v),
        pairs(&[(0, "docs/Invoice.pdf"), (1, "docs/Invoice.pdf")])
    );
    // `b3sum` of the file is its content hash; search by it finds the copy.
    let invoice_hash = blake3::hash(b"invoice bytes").to_hex().to_string();
    assert_eq!(v["hits"][0]["content_hash"], invoice_hash.as_str());
    let (code, v) = a.json(&[
        "search",
        &archive,
        "--content-hash",
        &invoice_hash,
        "--snapshot",
        "retained",
    ]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(
        paths(&v),
        pairs(&[
            (0, "docs/Invoice.pdf"),
            (1, "docs/Invoice.pdf"),
            (2, "copy.pdf")
        ])
    );

    // By file version, as printed by an earlier search.
    let (_, v) = a.json(&[
        "search",
        &archive,
        "--path",
        "docs/notes.txt",
        "--snapshot",
        "0",
    ]);
    let version = v["hits"][0]["file_version_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, v) = a.json(&[
        "search",
        &archive,
        "--version",
        &version,
        "--snapshot",
        "all",
    ]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(paths(&v), pairs(&[(0, "docs/notes.txt")]));
    assert_eq!(v["scope"], "all");

    // Subtree and kind.
    let (_, v) = a.json(&[
        "search",
        &archive,
        "--under",
        "docs",
        "--kind",
        "dir",
        "--snapshot",
        "1",
    ]);
    assert_eq!(paths(&v), pairs(&[(1, "docs")]));
    assert_eq!(v["scope"]["commit"], 1);

    // Text mode says what was covered.
    let (code, out, _) = a.go(&["search", &archive, "copy"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("copy.pdf"), "{out}");
    assert!(out.contains("1 match; coverage complete"), "{out}");
    assert!(out.contains("file content was not searched"), "{out}");
}

/// **§19.3, §23.3 #8: partial coverage is never "no matches".** Every
/// catalog image and the snapshot manifest of commit 0 are damaged, so
/// snapshot 0 cannot be searched: exit 2, or 1 with `--require-complete`.
#[test]
fn c14_search_partial_coverage_is_degraded() {
    use mochi_core::commit::Metadata;
    use mochi_core::publish::{commit_history, ReadOptions};
    use mochi_core::storage::os::OsReadStorage;

    let a = Area::new();
    let archive = a.history();
    let (code, _, _) = a.go(&["checkpoint", &archive]); // 3
    assert_eq!(code, exit::OK);
    let refs: Vec<_> = {
        let src = OsReadStorage::open(a.p("a.mochi")).unwrap();
        commit_history(&src, &ReadOptions::default())
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.commit.metadata {
                Metadata::Checkpoint { image, snapshot } => Some((e.commit.seq, image, snapshot)),
                Metadata::Delta { .. } => None,
            })
            .collect()
    };
    assert_eq!(refs.iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 3]);
    let mut bytes = fs::read(a.p("a.mochi")).unwrap();
    let mut flip = |r: mochi_core::commit::ObjectRef| {
        bytes[(r.offset + r.stored_len / 2) as usize] ^= 0x40;
    };
    flip(refs[0].1);
    flip(refs[0].2);
    flip(refs[1].1);
    fs::write(a.p("a.mochi"), bytes).unwrap();

    let (code, v) = a.json(&["search", &archive, "Invoice", "--snapshot", "all"]);
    assert_eq!(code, exit::DEGRADED, "{v}");
    assert_eq!(paths(&v), Vec::<(u64, String)>::new());
    let c = &v["coverage"];
    assert_eq!(c["complete"], false);
    assert_eq!(c["catalog_source"], "snapshot-manifest");
    assert_eq!(c["searched"], serde_json::json!([3]));
    let unavailable: Vec<u64> = c["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(unavailable, vec![0, 1, 2]);
    assert_eq!(c["unavailable"][0]["code"], "STORED_INTEGRITY_FAILED");

    let (code, _) = a.json(&[
        "search",
        &archive,
        "Invoice",
        "--snapshot",
        "all",
        "--require-complete",
    ]);
    assert_eq!(code, exit::FAILED);

    let (code, out, _) = a.go(&["search", &archive, "Invoice", "--snapshot", "all"]);
    assert_eq!(code, exit::DEGRADED);
    assert!(out.contains("coverage PARTIAL"), "{out}");
    assert!(
        out.contains("not proof that no matching entry exists"),
        "{out}"
    );
    assert!(!out.contains("coverage complete"), "{out}");
}

#[test]
fn c14_search_refuses_content_search_and_bad_arguments() {
    let a = Area::new();
    let archive = a.history();
    let (code, v) = a.json(&["search", &archive, "--content", "invoice"]);
    assert_eq!(code, exit::UNSUPPORTED, "{v}");
    assert_eq!(v["error"]["code"], "UNSUPPORTED_FEATURE");

    let (code, v) = a.json(&["search", &archive, "--snapshot", "9"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{v}");
    let (code, v) = a.json(&["search", &archive, "--version", "abc"]);
    assert_eq!(code, exit::ERROR);
    assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{v}");
    let (code, _, _) = a.go(&["search", &archive, "--snapshot", "latest"]);
    assert_eq!(code, exit::ERROR);
    let (code, _, _) = a.go(&["search", &archive, "--path", "a", "--under", "b"]);
    assert_eq!(code, exit::ERROR);
}

/// Archive strings are untrusted (§23.3 #9): a hostile name prints as one
/// inert line.
#[test]
#[cfg(unix)]
fn c14_search_prints_hostile_names_inertly() {
    let a = Area::new();
    let archive = a.s("h.mochi");
    let evil = a.file("src/\x1b[31mred\nline", b"x");
    let (code, _, err) = a.go(&["create", &archive, &evil]);
    assert_eq!(code, CREATED, "{err}");
    let (code, out, _) = a.go(&["search", &archive, "red"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("\\u{1b}[31mred\\u{a}line"), "{out}");
    assert!(!out.contains('\x1b'));
}
