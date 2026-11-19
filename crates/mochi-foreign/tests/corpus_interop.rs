//! The pinned corpus extracts bit-identically (plan D8 exit): every archive in
//! the corpus below is read by the helper (`read_many`) and, where the tool is
//! installed, extracted by an independent implementation (7-Zip, Info-ZIP
//! `unzip`, GNU `tar`); the two trees must hold the same files with the same
//! bytes. A pin file (`fixtures/foreign/CORPUS.sha256`) fixes the archives
//! themselves, so the corpus cannot change without a reviewed diff.
//!
//! Locally a missing tool skips its cases (and says so). CI sets
//! `MOCHI_REQUIRE_TOOLS=1`, which turns a missing tool into a failure, so the
//! comparison cannot silently stop running. RAR is not in this tool-based
//! comparison: Debian/Ubuntu's 7-Zip builds omit the non-free RAR decoder, and
//! no RAR tool is a CI dependency; `rar_contents_match_what_the_fixtures_authors_assert`
//! checks them against expectations written down by the authors of the archives
//! instead. Provenance and versions of the
//! writers are in `fixtures/foreign/PROVENANCE.md`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use common::*;
use mochi_foreign::*;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// (fixture, password, which tool reads it independently)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    SevenZip,
    Unzip,
    Tar,
}

const CORPUS: &[(&str, Option<&str>, Tool)] = &[
    // Info-ZIP's own writer, read by Info-ZIP.
    ("basic_infozip.zip", None, Tool::Unzip),
    ("zip64_forced.zip", None, Tool::Unzip),
    ("basic_infozip.zip", None, Tool::SevenZip),
    // GNU tar's own writer.
    ("basic.tar", None, Tool::Tar),
    ("basic.tar.gz", None, Tool::Tar),
    ("pax.tar", None, Tool::Tar),
    // 7-Zip's own writer (23.01), read by 7-Zip.
    ("7z/basic.7z", None, Tool::SevenZip),
    ("7z/nonsolid.7z", None, Tool::SevenZip),
    ("7z/copy.7z", None, Tool::SevenZip),
    ("7z/ppmd.7z", None, Tool::SevenZip),
    ("7z/bzip2.7z", None, Tool::SevenZip),
    ("7z/lzma_d16m.7z", None, Tool::SevenZip),
    ("7z/enc_data.7z", Some("SECRET"), Tool::SevenZip),
    ("7z/enc_names.7z", Some("SECRET"), Tool::SevenZip),
    ("7z/split.7z.001", None, Tool::SevenZip),
    // WinZip-style AES-256 ZIP written by 7-Zip (interop for the zip reader).
    ("7z/zip_aes256_7zip.zip", Some("SECRET"), Tool::SevenZip),
];

fn find_tool(t: Tool) -> Option<PathBuf> {
    let names: &[&str] = match t {
        Tool::SevenZip => &["7zz", "7z", "7za", "7zr"],
        Tool::Unzip => &["unzip"],
        Tool::Tar => &["tar"],
    };
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    if cfg!(windows) {
        dirs.push(PathBuf::from(r"C:\Program Files\7-Zip"));
        dirs.push(PathBuf::from(r"C:\Program Files (x86)\7-Zip"));
    }
    for d in dirs {
        for n in names {
            for ext in ["", ".exe"] {
                let p = d.join(format!("{n}{ext}"));
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn files_under(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                walk(base, &e.path(), out);
            } else if t.is_file() {
                let rel = e
                    .path()
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, std::fs::read(e.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The helper's reading of the archive: every regular file's bytes by name,
/// each entry's check required to have passed.
fn mochi_files(path: &Path, password: Option<&str>) -> BTreeMap<String, Vec<u8>> {
    let c = client();
    let p = password.map(pw);
    let l = c.list(path, p.as_ref()).unwrap();
    let all: Vec<u64> = l
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .map(|e| e.index)
        .collect();
    let (_, gots) = read_many(&c, path, &all, p.as_ref()).unwrap();
    let mut out = BTreeMap::new();
    for g in gots {
        assert!(
            g.outcome.is_clean(),
            "{path:?}: {} -> {:?}",
            String::from_utf8_lossy(&g.info.name),
            g.outcome
        );
        let name = String::from_utf8(g.info.name.clone()).unwrap();
        out.insert(name.trim_start_matches("./").to_string(), g.bytes);
    }
    out
}

fn tool_files(tool: Tool, exe: &Path, archive: &Path, password: Option<&str>, into: &Path) {
    let status = match tool {
        Tool::SevenZip => {
            let mut c = Command::new(exe);
            c.arg("x").arg("-y").arg(format!("-o{}", into.display()));
            c.arg(format!("-p{}", password.unwrap_or("")));
            c.arg(archive);
            c.output().unwrap()
        }
        Tool::Unzip => Command::new(exe)
            .args(["-qq", "-o"])
            .arg(archive)
            .arg("-d")
            .arg(into)
            .output()
            .unwrap(),
        Tool::Tar => Command::new(exe)
            .arg("-xf")
            .arg(archive)
            .arg("-C")
            .arg(into)
            .output()
            .unwrap(),
    };
    assert!(
        status.status.success(),
        "{tool:?} failed on {archive:?}: {}{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
fn the_corpus_extracts_bit_identically_to_an_independent_implementation() {
    let require = std::env::var_os("MOCHI_REQUIRE_TOOLS").is_some();
    let mut compared = 0;
    let mut skipped = Vec::new();
    for (name, password, tool) in CORPUS {
        let archive = fixture(name);
        let Some(exe) = find_tool(*tool) else {
            assert!(
                !require,
                "MOCHI_REQUIRE_TOOLS is set but {tool:?} is not installed"
            );
            skipped.push(format!("{name} ({tool:?})"));
            continue;
        };
        let ours = mochi_files(&archive, *password);
        assert!(!ours.is_empty(), "{name}: nothing read");
        let dir = tempfile::tempdir().unwrap();
        tool_files(*tool, &exe, &archive, *password, dir.path());
        let theirs = files_under(dir.path());
        assert_eq!(
            ours.keys().collect::<Vec<_>>(),
            theirs.keys().collect::<Vec<_>>(),
            "{name}: the two readers found different files"
        );
        for (k, v) in &ours {
            assert!(
                v == &theirs[k],
                "{name}: {k} differs from {tool:?}'s extraction"
            );
        }
        compared += 1;
    }
    eprintln!("compared {compared} corpus archives; skipped (tool not installed): {skipped:?}");
    if require {
        assert_eq!(compared, CORPUS.len());
    }
}

/// SHA-256 of a file, with the platform's own tool (the test needs no hash
/// crate): `sha256sum` where it exists, else `certutil` (Windows).
fn sha256(path: &Path) -> String {
    // Read from stdin: coreutils on Windows prefixes a line with `\` when the
    // file name contains a backslash, and no name is printed for stdin.
    let stdin = std::fs::File::open(path).unwrap();
    if let Ok(o) = Command::new("sha256sum").stdin(stdin).output() {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap()
                .to_string();
        }
    }
    let o = Command::new("certutil")
        .arg("-hashfile")
        .arg(path)
        .arg("SHA256")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .nth(1)
        .unwrap()
        .replace(' ', "")
        .to_lowercase()
}

#[test]
fn the_corpus_files_are_the_ones_that_were_reviewed() {
    let pins = std::fs::read_to_string(fixture("CORPUS.sha256")).unwrap();
    let mut seen = 0;
    for line in pins
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
    {
        let (hash, name) = line.split_once("  ").unwrap();
        assert_eq!(
            sha256(&fixture(name)),
            hash,
            "{name} changed: regenerate deliberately and explain the diff"
        );
        seen += 1;
    }
    for (name, _, _) in CORPUS {
        assert!(
            pins.contains(&format!("  {name}\n")),
            "{name} is not pinned in CORPUS.sha256"
        );
    }
    assert!(seen >= 20);
}

/// RAR: the content each archive holds, as asserted by the authors of the
/// archives in their own tests (the `unrar` crate's `tests/`, which read the
/// same files): `version.rar` holds "unrar-0.4.0", and the `.gitignore` of the
/// crate is "target\nCargo.lock\n". The archives were written by RARLAB's
/// `rar`; nothing here is derived from the reader under test.
#[test]
fn rar_contents_match_what_the_fixtures_authors_assert() {
    let gitignore = b"target\nCargo.lock\n".to_vec();
    type Case<'a> = (&'a str, Option<&'a str>, &'a str, Option<Vec<u8>>);
    let cases: [Case; 5] = [
        (
            "rar/version.rar",
            None,
            "VERSION",
            Some(b"unrar-0.4.0".to_vec()),
        ),
        ("rar/solid.rar", None, ".gitignore", Some(gitignore.clone())),
        (
            "rar/crypted.rar",
            Some("unrar"),
            ".gitignore",
            Some(gitignore.clone()),
        ),
        (
            "rar/comment-hpw-password.rar",
            Some("password"),
            ".gitignore",
            Some(gitignore),
        ),
        ("rar/unicode.rar", None, "te…―st✌", None), // content not asserted by the authors; size is
    ];
    for (name, password, entry, expected) in cases {
        let files = mochi_files(&fixture(name), password);
        let got = files
            .get(entry)
            .unwrap_or_else(|| panic!("{name}: no {entry} in {:?}", files.keys()));
        match expected {
            Some(e) => assert_eq!(got, &e, "{name}"),
            None => assert_eq!(got.len(), 17, "{name}"),
        }
        assert_eq!(files.len(), 1, "{name}");
    }
}
