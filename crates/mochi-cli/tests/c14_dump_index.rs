//! C14 end-to-end tests for `mochi dump-index` (next-work-plan K6), run
//! in-process through `mochi_cli::run` against the real filesystem.
//!
//! The oracles are independent of the dump code: the table names come from
//! the DDL text, row counts from `snapshot list`, file-version IDs from
//! `search`, object IDs from the catalog's own `object_ids`, and the
//! hostile name's exact bytes from the transaction that wrote it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use mochi_cli::{exit, exit_code_for, run};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::catalog::schema::DDL;
use mochi_core::commit::Metadata;
use mochi_core::publish::{
    commit_history, open_head, ArchiveWriter, CheckpointPolicy, ReadOptions, Transaction,
};
use mochi_core::storage::os::OsReadStorage;
use mochi_core::ErrorCode;
use mochi_testkit::archive::{path, test_options, Job};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};
use serde_json::Value;

/// A name no terminal should ever see raw: an escape sequence, a newline,
/// a tab, and a byte that is not UTF-8.
const HOSTILE: &[u8] = b"h\x1b[2Jx\ny\tz\xff";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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

    fn s(&self, rel: &str) -> String {
        self.dir.path().join(rel).display().to_string()
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
        let v = serde_json::from_str(out.trim())
            .unwrap_or_else(|e| panic!("{args:?}: not JSON ({e}): {out:?} {err}"));
        (code, v)
    }

    fn write(&self, name: &str, bytes: &[u8]) -> String {
        let p: PathBuf = self.dir.path().join(name);
        fs::write(&p, bytes).unwrap();
        p.display().to_string()
    }
}

/// Nine commits, checkpoints at 0 and 3 only, with a hostile name in 1:
/// directories, replacements, a rename, a deletion. No retention, so every
/// version stays in some snapshot.
fn source() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(s.clone(), Box::new(SeqIds::new(1)), test_options()).unwrap();
    w.set_checkpoint_policy(CheckpointPolicy::Never).unwrap();
    let job = Job::new();
    let put = |t: &mut Transaction, p: &str, seed: u64| {
        t.put_file(path(p), deterministic_bytes(seed, 150), attrs(0o644, 0));
    };
    let mut c = |w: &mut ArchiveWriter<SimStorage>, f: &dyn Fn(&mut Transaction)| {
        let mut tx = Transaction::new();
        f(&mut tx);
        w.commit(tx, &job.ctx()).unwrap();
    };
    c(&mut w, &|t| {
        t.put_dir(path("d"), attrs(0o755, 1));
        put(t, "d/a", 1);
        put(t, "b", 2);
    }); // 0: checkpoint
    c(&mut w, &|t| {
        t.put_file(
            ArchivePath::from_stored(HOSTILE).unwrap(),
            deterministic_bytes(9, 80),
            attrs(0o600, 0),
        );
    }); // 1
    c(&mut w, &|t| put(t, "d/a", 3)); // 2
    w.request_checkpoint();
    c(&mut w, &|t| {
        t.rename(path("b"), path("d/b"));
    }); // 3: checkpoint
    c(&mut w, &|t| put(t, "c", 4)); // 4
    c(&mut w, &|t| {
        t.delete(path("c"));
    }); // 5
    c(&mut w, &|t| put(t, "e", 5)); // 6
    w.close().unwrap();
    s
}

fn tables_of(v: &Value) -> &serde_json::Map<String, Value> {
    v["tables"].as_object().unwrap()
}

fn rows<'a>(v: &'a Value, table: &str) -> &'a Vec<Value> {
    v["tables"][table]["rows"].as_array().unwrap()
}

/// Every table of the DDL appears, with the row counts the rest of the CLI
/// agrees with.
#[test]
fn dump_index_shows_every_table_and_agrees_with_the_other_commands() {
    let a = Area::new();
    let src = source();
    let archive = a.write("x.mochi", &src.contents());

    let (code, v) = a.json(&["dump-index", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    let want: BTreeSet<String> = DDL
        .lines()
        .filter_map(|l| l.strip_prefix("CREATE TABLE "))
        .map(|l| l.split_whitespace().next().unwrap().to_owned())
        .collect();
    assert_eq!(want.len(), 9);
    let got: BTreeSet<String> = tables_of(&v).keys().cloned().collect();
    assert_eq!(got, want);
    assert_eq!(v["commit"]["seq"], 6);
    assert_eq!(v["commit"]["catalog_source"], "image");

    // `commits`: one row per commit, as `snapshot list` counts them.
    let (_, listing) = a.json(&["snapshot", "list", &archive]);
    let n = listing["commits"].as_array().unwrap().len();
    assert_eq!(n, 7);
    assert_eq!(rows(&v, "commits").len(), n);
    assert_eq!(
        v["tables"]["commits"]["columns"],
        serde_json::json!(["seq", "parent_seq"])
    );

    // `file_versions`: exactly the version IDs `search` finds across all
    // snapshots (nothing here is expired, so every version is in one).
    let (_, found) = a.json(&["search", &archive, "--snapshot", "all"]);
    let from_search: BTreeSet<String> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["file_version_id"].as_str().unwrap().to_owned())
        .collect();
    let from_dump: BTreeSet<String> = rows(&v, "file_versions")
        .iter()
        .map(|r| r[0]["blob_hex"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(from_dump, from_search);

    // `objects`: the catalog's own object IDs, read through its API.
    let head = open_head(
        &OsReadStorage::open(&archive).unwrap(),
        &ReadOptions::default(),
    )
    .unwrap();
    let ids: BTreeSet<String> = head
        .catalog
        .object_ids()
        .unwrap()
        .iter()
        .map(|i| hex(i.as_bytes()))
        .collect();
    let dumped: BTreeSet<String> = rows(&v, "objects")
        .iter()
        .map(|r| r[0]["blob_hex"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(dumped, ids);
    assert_eq!(rows(&v, "chunks").len(), ids.len());

    // Text mode: counts per table, no rows.
    let (code, out, _) = a.go(&["dump-index", &archive]);
    assert_eq!(code, exit::OK);
    for name in &want {
        assert!(out.contains(name.as_str()), "{name}: {out}");
    }
    assert!(out.contains("catalog from image"), "{out}");
    assert!(!out.contains("x'"), "no rows without --table: {out}");
    assert!(
        out.lines()
            .any(|l| l.starts_with("commits") && l.contains(&format!("{n} rows"))),
        "{out}"
    );
}

/// `--snapshot SEQ` dumps that commit's catalog, and `--table` selects
/// tables (in table order, once each, whatever the request order).
#[test]
fn dump_index_reads_the_chosen_commit_and_tables() {
    let a = Area::new();
    let archive = a.write("x.mochi", &source().contents());
    let (code, v) = a.json(&["dump-index", &archive, "--snapshot", "1"]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["commit"]["seq"], 1);
    assert_eq!(rows(&v, "commits").len(), 2, "commits 0 and 1");

    let (code, v) = a.json(&[
        "dump-index",
        &archive,
        "--table",
        "objects",
        "--table",
        "commits",
        "--table",
        "objects",
    ]);
    assert_eq!(code, exit::OK);
    let names: Vec<&String> = tables_of(&v).keys().collect();
    assert_eq!(names, ["commits", "objects"]);

    let (code, out, _) = a.go(&["dump-index", &archive, "--table", "commits"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("table commits (7 rows)"), "{out}");
    assert!(out.contains("seq\tparent_seq"), "{out}");
    assert!(out.contains("0\tNULL"), "{out}");
}

/// Unknown tables, SQL, and quoting tricks are refused as invalid
/// arguments, before anything is printed; the archive is untouched.
#[test]
fn dump_index_refuses_names_that_are_not_tables() {
    let a = Area::new();
    let archive = a.write("x.mochi", &source().contents());
    let before = fs::read(&archive).unwrap();
    for bad in [
        "nope",
        "sqlite_schema",
        "commits; DROP TABLE commits",
        "commits\" --",
        "COMMITS",
    ] {
        let (code, out, err) = a.go(&["dump-index", &archive, "--table", bad, "--json"]);
        assert_eq!(code, exit_code_for(ErrorCode::InvalidArgument), "{bad:?}");
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{bad:?}: {err}");
        assert!(v["error"]["message"].as_str().unwrap().contains("commits"));
        assert!(v.get("tables").is_none());
    }
    assert_eq!(fs::read(&archive).unwrap(), before);
}

/// A hostile name is exact in JSON (the BLOB's hex) and never raw in text.
#[test]
fn dump_index_shows_a_hostile_name_exactly_and_safely() {
    let a = Area::new();
    let archive = a.write("x.mochi", &source().contents());
    let want = hex(HOSTILE);

    let (code, v) = a.json(&["dump-index", &archive, "--table", "namespace_ops"]);
    assert_eq!(code, exit::OK);
    let paths: Vec<&str> = rows(&v, "namespace_ops")
        .iter()
        .filter_map(|r| r[3]["blob_hex"].as_str())
        .collect();
    assert_eq!(
        paths.iter().filter(|p| **p == want).count(),
        1,
        "the exact bytes, once: {paths:?}"
    );

    let (code, out, _) = a.go(&["dump-index", &archive, "--table", "namespace_ops"]);
    assert_eq!(code, exit::OK);
    assert!(out.contains(&format!("x'{want}'")), "{out}");
    for raw in ['\x1b', '\u{ff}', '\u{fffd}'] {
        assert!(!out.contains(raw), "{raw:?} leaked into: {out:?}");
    }
    // Every row is one line: the newline and tab in the name did not split it.
    let ops = rows(&v, "namespace_ops").len();
    let lines = out
        .lines()
        .skip_while(|l| !l.starts_with("table namespace_ops"))
        .skip(2)
        .count();
    assert_eq!(lines, ops, "{out}");
}

/// With the head's catalog image damaged, the dump comes from the snapshot
/// manifest and says so; nothing is written.
#[test]
fn dump_index_falls_back_to_the_snapshot_manifest_and_says_so() {
    let a = Area::new();
    let src = source();
    let history = commit_history(&src, &ReadOptions::default()).unwrap();
    let Metadata::Checkpoint { image, .. } = history[3].commit.metadata else {
        panic!("commit 3 is a checkpoint");
    };
    let mut bytes = src.contents();
    bytes[(image.offset + image.stored_len / 2) as usize] ^= 0xFF;
    let archive = a.write("damaged.mochi", &bytes);
    let before = fs::read(&archive).unwrap();

    let (code, v) = a.json(&["dump-index", &archive]);
    assert_eq!(code, exit::OK, "{v}");
    assert_eq!(v["commit"]["catalog_source"], "snapshot-manifest");
    assert_eq!(v["commit"]["seq"], 6);
    assert_eq!(
        tables_of(&v).len(),
        9,
        "every table is still there: {}",
        v["commit"]
    );

    let (code, out, _) = a.go(&["dump-index", &archive]);
    assert_eq!(code, exit::OK);
    assert!(out.contains("catalog from snapshot-manifest"), "{out}");
    assert_eq!(fs::read(&archive).unwrap(), before, "read-only");
}

/// A missing archive is an error, never an empty dump.
#[test]
fn dump_index_of_a_missing_archive_is_an_error() {
    let a = Area::new();
    let (code, out, _) = a.go(&["dump-index", &a.s("nope.mochi"), "--json"]);
    assert_eq!(code, exit::ERROR);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["error"]["code"], "IO_ERROR");
}
