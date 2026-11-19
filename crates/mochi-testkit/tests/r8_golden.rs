//! R8: golden valid, corrupt, and interrupted archives (gate G13; plan W16,
//! K21). The set is in `fixtures/golden/r8/`:
//!
//! * `cases.json`: the index, one object per case;
//! * `<case>.mochi` (and `<case>.copyN.mochi`): the archives, **frozen when
//!   written** (`write_r8_golden_files` writes only missing files and refuses
//!   to overwrite a different one);
//! * `<case>.summary.json`: the expected `mochi-interop-summary-v1`, produced
//!   by `mochi_testkit::interop` and reviewed.
//!
//! The ordinary tests rebuild nothing. For every case they check that this
//! build's `verify` exit code, repair outcome, and summary equal the index and
//! the stored summary; the coverage test fails when a profile lacks a class,
//! a §24.2 row about archive bytes has no case, or a violation-class error
//! code appears in no case; and the whole set must fit in 12 MiB.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "r8_golden/g1.rs"]
mod g1;
#[path = "r8_golden/recipes.rs"]
mod recipes;
#[path = "r8_golden/recipes2.rs"]
mod recipes2;
#[path = "r8_golden/recipes3.rs"]
mod recipes3;

use std::collections::{BTreeMap, BTreeSet};

use mochi_core::copies::CopySource;
use mochi_core::publish::{commit_history, locate_head};
use mochi_core::repair::plan_with_salvage;
use mochi_core::salvage::SalvageOptions;
use mochi_core::verify::{classify, ErrorClass};
use mochi_core::ErrorCode;
use mochi_testkit::archive::Job;
use mochi_testkit::interop::{self, hex, Input};
use mochi_testkit::SimStorage;
use serde_json::{json, Value};

use recipes::{dir, specs, Built, Ctx, Spec};

/// The whole set must fit in this (plan K21).
const BUDGET: u64 = 12 * 1024 * 1024;

fn copy_name(name: &str, n: usize) -> String {
    format!("{name}.copy{n}.mochi")
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap();
    s.push('\n');
    s
}

fn head_commit_hex(bytes: &[u8]) -> String {
    let s = SimStorage::from_bytes(bytes.to_vec());
    commit_history(&s, &Default::default())
        .unwrap()
        .last()
        .unwrap()
        .commit_id
        .to_hex()
}

// ---- the reference run ---------------------------------------------------------------

struct Reference {
    verify_exit: u8,
    verify_level: String,
    repair_outcome: String,
    repair_exit: u8,
    summary: Value,
}

fn reference(
    name: &str,
    passphrase: Option<&str>,
    expected_head: Option<&str>,
    salvage: bool,
    main: &[u8],
    copies: &[Vec<u8>],
) -> Reference {
    let storage = SimStorage::from_bytes(main.to_vec());
    let input = Input {
        name,
        storage: &storage,
        passphrase,
        expected_head,
    };
    let report = interop::verify_case(&input);
    let summary = interop::summarize(&input);
    let copy_storages: Vec<SimStorage> = copies
        .iter()
        .map(|c| SimStorage::from_bytes(c.clone()))
        .collect();
    let sources: Vec<CopySource<'_>> = copy_storages
        .iter()
        .enumerate()
        .map(|(i, s)| CopySource {
            label: copy_name(name, i + 1),
            storage: s,
        })
        .collect();
    let opts = SalvageOptions::default();
    let plan = plan_with_salvage(
        &storage,
        &sources,
        salvage.then_some(&opts),
        &interop::read_options(passphrase),
        &Job::new().ctx(),
    );
    let (repair_outcome, repair_exit) = match plan {
        Ok(p) => (
            serde_json::to_value(p.outcome)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned(),
            p.exit_code,
        ),
        Err(e) => (
            format!("error:{}", e.code.as_str()),
            match classify(e.code) {
                ErrorClass::Violation => 1,
                ErrorClass::Unsupported => 4,
                ErrorClass::Operational => 3,
            },
        ),
    };
    Reference {
        verify_exit: report.exit_code,
        verify_level: serde_json::to_value(report.level)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned(),
        repair_outcome,
        repair_exit,
        summary,
    }
}

// ---- cases.json -------------------------------------------------------------------

fn load_index() -> Vec<Value> {
    let text = std::fs::read_to_string(dir().join("cases.json")).expect("cases.json");
    serde_json::from_str::<Vec<Value>>(&text).unwrap()
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn case_files(c: &Value) -> (Vec<u8>, Vec<Vec<u8>>) {
    let name = c["name"].as_str().unwrap();
    let copies = c["copies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| read(f.as_str().unwrap()))
        .collect();
    (read(&format!("{name}.mochi")), copies)
}

fn run_indexed(c: &Value) -> Reference {
    let (main, copies) = case_files(c);
    reference(
        c["name"].as_str().unwrap(),
        c["passphrase"].as_str(),
        c["expected_head"].as_str(),
        c["salvage"].as_bool().unwrap(),
        &main,
        &copies,
    )
}

#[test]
fn the_index_lists_exactly_the_specified_cases_and_files() {
    let index = load_index();
    let want: Vec<&str> = specs().iter().map(|s| s.name).collect();
    let got: Vec<&str> = index.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        got, want,
        "cases.json and the recipes list the same cases in the same order"
    );

    let mut files = BTreeSet::new();
    for c in &index {
        let name = c["name"].as_str().unwrap();
        files.insert(format!("{name}.mochi"));
        files.insert(format!("{name}.summary.json"));
        for f in c["copies"].as_array().unwrap() {
            files.insert(f.as_str().unwrap().to_owned());
        }
        for k in [
            "profile",
            "class",
            "fault",
            "spec",
            "fault_matrix_row",
            "passphrase",
            "expected_head",
            "copies",
            "expect",
            "salvage",
        ] {
            assert!(c.get(k).is_some(), "{name}: key {k}");
        }
    }
    files.insert("cases.json".into());
    files.insert("g1".into());
    files.insert("legacy".into());
    let on_disk: BTreeSet<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(on_disk, files, "no file outside the index, none missing");
}

#[test]
fn every_case_matches_its_index_entry_and_stored_summary() {
    let mut bad = Vec::new();
    for c in load_index() {
        let name = c["name"].as_str().unwrap().to_owned();
        let r = run_indexed(&c);
        let e = &c["expect"];
        let got = json!({
            "verify_exit": r.verify_exit,
            "verify_level": r.verify_level,
            "repair_outcome": r.repair_outcome,
            "repair_exit": r.repair_exit,
        });
        if &got != e {
            bad.push(format!("{name}: expect {e} but this build gives {got}"));
        }
        let stored: Value = serde_json::from_slice(&read(&format!("{name}.summary.json"))).unwrap();
        if stored != r.summary {
            bad.push(format!(
                "{name}: the stored summary differs from this build's"
            ));
        }
        assert_eq!(stored["case"], name);
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

// ---- coverage ---------------------------------------------------------------------

const PROFILES: [&str; 6] = ["core", "tar", "enc", "red", "tar+red", "enc+red"];

/// §24.2 rows about archive bytes in 1.0: each needs a case naming it.
const ROWS_WITH_CASES: [&str; 15] = [
    recipes::ROW_TERMINATION,
    recipes::ROW_TRUNCATION,
    recipes::ROW_TORN,
    recipes::ROW_FOOTER,
    recipes::ROW_MAGIC,
    recipes::ROW_OVERSIZED,
    recipes::ROW_CATALOG,
    recipes::ROW_DICT,
    recipes::ROW_CONTENT,
    recipes::ROW_PARITY,
    recipes::ROW_OLDER,
    recipes::ROW_KEY,
    recipes::ROW_COMPACTION,
    recipes::ROW_HOLD,
    recipes::ROW_PATH,
];

/// §24.2 rows with no archive-bytes case, and why.
const EXEMPT_ROWS: [(&str, &str); 8] = [
    ("Deleted primary archive", "Preservation profile inventory; not in 1.0 (spec Annex A)"),
    ("Original machine unavailable", "restore on a clean machine is a process, tested by the restore and E2E tests; no archive bytes differ"),
    ("Index deleted", "search indexes are not archive bytes (C13, `c13_search.rs`)"),
    ("Interrupted indexing", "search indexes are not archive bytes (C13, `c13_search.rs`)"),
    ("Concurrent conflicting commits", "a lock and timing property, `q54_writer_lock.rs`; one writer leaves one set of bytes"),
    ("Stale writer resumes", "fencing is a lock and timing property, `c5_publication.rs`"),
    ("GC overlaps publication", "a concurrency property; GC output is core-valid-gc"),
    ("Corrupted inventory", "Preservation profile inventory; not in 1.0 (spec Annex A)"),
];

/// Violation-class codes no `verify` of existing archive bytes can report,
/// and why. Every other violation-class code must appear in some case's
/// summary (as the head's refusal or as a finding).
const UNREACHABLE_VIOLATIONS: [(ErrorCode, &str); 3] = [
    (
        ErrorCode::CheckpointMismatch,
        "raised only while writing: the writer, `compact`, and `repair apply` re-read what they just wrote and roll back on a mismatch (D10 item 7), so no committed byte sequence carries it; `t15_adoption.rs` and `c9_retention.rs` produce it with the checkpoint tamper controls",
    ),
    (
        ErrorCode::RetentionUnresolved,
        "reported by `gc plan` and `repair plan|apply`, never by `verify`, and the interop summary holds `verify`'s findings; `c9_gc_plan.rs` and `c8_repair.rs` hold the cases",
    ),
    (
        ErrorCode::IdentityConflict,
        "raised when a writer inserts an ID that exists with different content; every reader path that applies manifests meets the duplicate first as RECORD_INVALID (D10.4), which core-corrupt-reused-chunk-id shows; `c3_catalog.rs` holds the catalog-level test",
    ),
];

fn all_violation_codes() -> Vec<ErrorCode> {
    ErrorCode::ALL
        .iter()
        .copied()
        .filter(|c| classify(*c) == ErrorClass::Violation)
        .collect()
}

#[test]
fn coverage_every_profile_has_valid_corrupt_and_interrupted_cases() {
    let mut have: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for s in specs() {
        *have.entry((s.profile, s.class)).or_default() += 1;
    }
    for p in PROFILES {
        for class in ["valid", "corrupt", "interrupted"] {
            assert!(
                have.contains_key(&(p, class)),
                "profile {p} has no {class} case"
            );
        }
    }
    for class in ["copy", "salvage", "legacy"] {
        assert!(specs().iter().any(|s| s.class == class), "no {class} case");
    }
    for s in specs() {
        assert!(PROFILES.contains(&s.profile), "{}", s.name);
        assert!(
            [
                "valid",
                "corrupt",
                "interrupted",
                "copy",
                "salvage",
                "legacy"
            ]
            .contains(&s.class),
            "{}",
            s.name
        );
    }
}

#[test]
fn coverage_every_fault_matrix_row_about_archive_bytes_has_a_case() {
    let named: BTreeSet<&str> = specs().iter().filter_map(|s| s.row).collect();
    for row in ROWS_WITH_CASES {
        assert!(named.contains(row), "§24.2 row {row:?} has no case");
    }
    // Every row a case names is one the test knows (a typo would hide a gap).
    for r in &named {
        assert!(ROWS_WITH_CASES.contains(r), "unknown row {r:?}");
    }
    // The exemptions give a reason, and no exempt row also has a case.
    for (row, why) in EXEMPT_ROWS {
        assert!(!why.is_empty());
        assert!(!named.contains(row), "{row:?} is exempt and has a case");
    }
    // The spec's table: every row is either covered or exempt.
    let spec_text = include_str!("../../../docs/spec.md");
    let table: Vec<&str> = spec_text
        .split("### 24.2 Release-blocking fault matrix")
        .nth(1)
        .unwrap()
        .split("### 24.3")
        .next()
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("| ") && !l.starts_with("| Injected") && !l.starts_with("|---"))
        .map(|l| l.trim_start_matches("| ").split(" | ").next().unwrap())
        .collect();
    assert_eq!(table.len(), 23, "the spec's matrix has 23 rows");
    for row in table {
        let covered = ROWS_WITH_CASES.contains(&row);
        let exempt = EXEMPT_ROWS.iter().any(|(r, _)| *r == row);
        assert!(
            covered || exempt,
            "§24.2 row {row:?} is neither covered nor exempt"
        );
    }
}

#[test]
fn coverage_every_reachable_violation_code_appears_in_a_summary() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for c in load_index() {
        let name = c["name"].as_str().unwrap();
        let v: Value = serde_json::from_slice(&read(&format!("{name}.summary.json"))).unwrap();
        if let Some(code) = v["head"]["code"].as_str() {
            seen.insert(code.to_owned());
        }
        for f in v["findings"].as_array().unwrap() {
            seen.insert(f["code"].as_str().unwrap().to_owned());
        }
    }
    let exempt: BTreeSet<&str> = UNREACHABLE_VIOLATIONS
        .iter()
        .map(|(c, why)| {
            assert!(!why.is_empty());
            c.as_str()
        })
        .collect();
    for c in &exempt {
        assert!(
            !seen.contains(*c),
            "{c} is exempt but a case reaches it: remove the exemption"
        );
    }
    let missing: Vec<&str> = all_violation_codes()
        .into_iter()
        .map(|c| c.as_str())
        .filter(|c| !seen.contains(*c) && !exempt.contains(c))
        .collect();
    assert!(
        missing.is_empty(),
        "violation-class codes in no case's summary: {missing:?}"
    );
}

// ---- budget, truncation, oracles ---------------------------------------------------------

#[test]
fn the_set_fits_the_12_mib_budget() {
    fn size(d: &std::path::Path) -> u64 {
        std::fs::read_dir(d)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let m = e.metadata().unwrap();
                if m.is_dir() {
                    size(&e.path())
                } else {
                    m.len()
                }
            })
            .sum()
    }
    let total = size(&dir());
    assert!(
        total <= BUDGET,
        "fixtures/golden/r8 is {total} bytes; the budget is {BUDGET}"
    );
}

/// §24.2 "truncation at every byte of small fixtures": cut the small valid
/// case at every length. A head found at any length must be one of the
/// original's commits, with the original's ID, at the original's offsets.
#[test]
fn truncation_at_every_byte_of_the_small_case_accepts_no_foreign_commit() {
    let full = read("core-valid-small.mochi");
    let s = SimStorage::from_bytes(full.clone());
    let original = commit_history(&s, &Default::default()).unwrap();
    let ids: BTreeMap<u64, (String, u64)> = original
        .iter()
        .map(|e| (e.commit.seq, (e.commit_id.to_hex(), e.footer_offset)))
        .collect();
    let mut heads = BTreeSet::new();
    for n in 0..=full.len() {
        let cut = SimStorage::from_bytes(full[..n].to_vec());
        let Ok(loc) = locate_head(&cut, &Default::default()) else {
            continue;
        };
        let Ok(h) = commit_history(&cut, &Default::default()) else {
            continue;
        };
        let top = h.last().unwrap();
        assert_eq!(top.footer_offset, loc.footer.footer_offset);
        for e in &h {
            let want = ids
                .get(&e.commit.seq)
                .unwrap_or_else(|| panic!("cut {n}: foreign sequence"));
            assert_eq!(&(e.commit_id.to_hex(), e.footer_offset), want, "cut {n}");
        }
        heads.insert(top.commit.seq);
    }
    assert_eq!(
        heads,
        BTreeSet::from([0, 1]),
        "both commits are heads at some length"
    );
}

/// An independent oracle for the history cases: the summary's final
/// namespace equals the paths the scripts produce, computed here from the
/// transactions' effect, not from any reader.
#[test]
fn history_summaries_agree_with_the_script() {
    let want: Vec<&str> = vec!["d", "d/b", "d/g", "e", "e/a", "e/n", "h"];
    for name in [
        "core-valid-history",
        "tar-valid-history",
        "enc-valid-history",
        "red-valid-50pct",
    ] {
        let v: Value = serde_json::from_slice(&read(&format!("{name}.summary.json"))).unwrap();
        let snaps = v["snapshots"].as_array().unwrap();
        assert_eq!(snaps.len(), 7, "{name}");
        let last: Vec<String> = snaps[6]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_owned())
            .collect();
        let mut want = want.clone();
        if name == "red-valid-50pct" {
            // `history_big`: the first commit also puts a 4,000-byte `pad`.
            want.push("pad");
        }
        let want_hex: Vec<String> = want.iter().map(|p| hex(p.as_bytes())).collect();
        assert_eq!(last, want_hex, "{name}");
        assert_eq!(v["head"]["status"], "opened", "{name}");
        assert!(v["findings"].as_array().unwrap().is_empty(), "{name}");
    }
}

// ---- the G1 audit -------------------------------------------------------------------

fn g1_dir() -> std::path::PathBuf {
    dir().join("g1")
}

fn g1_list() -> Vec<g1::V> {
    let bytes = std::fs::read(dir().join("enc-valid-reencrypted.mochi"))
        .expect("enc-valid-reencrypted.mochi (write the cases first)");
    g1::vectors(Some(recipes3::delta0_plaintext(&bytes, recipes::PASS2)))
}

fn g1_line(v: &g1::V) -> String {
    match v.class {
        g1::Class::Valid => format!("{} valid", v.name),
        g1::Class::Schema => format!("{} rejects {} (schema)", v.name, v.code),
        g1::Class::Semantic => format!("{} rejects {} (semantic)", v.name, v.code),
    }
}

fn payload_of(frame: &[u8]) -> &[u8] {
    &frame[8..]
}

fn cddl_validate(schema: &str, cbor: &[u8]) -> Result<(), String> {
    use cddl::validator::Validator;
    let text = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/schemas")
            .join(format!("{schema}.cddl")),
    )
    .unwrap();
    let parsed = cddl::cddl_from_str(&text, true).unwrap_or_else(|e| panic!("{schema}: {e}"));
    let value = cddl::validator::cbor_value::decode_cbor(cbor).map_err(|e| format!("{e:?}"))?;
    cddl::validator::cbor::CBORValidator::new(&parsed, value, None)
        .validate()
        .map_err(|e| format!("{e}"))
}

/// The production decoder's verdict: `Ok`, or the error code.
fn decoder_verdict(v: &g1::V) -> Result<(), String> {
    use mochi_core::commit::CommitRecord;
    use mochi_core::descriptor::Descriptor;
    use mochi_core::keys::KeyEnvelope;
    use mochi_core::manifest::Manifest;
    use mochi_format::cbor::CborLimits;
    use mochi_format::Limits;
    let (l, c) = (Limits::default(), CborLimits::default());
    let p = payload_of(&v.bytes);
    let code = |e: mochi_core::MochiError| e.code.as_str().to_owned();
    match v.kind {
        g1::K::Descriptor => Descriptor::decode(p, &l, &c).map(|_| ()).map_err(code),
        g1::K::Commit => CommitRecord::decode(p, &l, &c).map(|_| ()).map_err(code),
        g1::K::Manifest => Manifest::decode(p, &l, &c).map(|_| ()).map_err(code),
        g1::K::KeyEnvelope => KeyEnvelope::decode(p, &l, &c).map(|_| ()).map_err(code),
    }
}

#[test]
fn the_g1_audit_vectors_match_their_files_and_verdicts() {
    let list = g1_list();
    let mut files = BTreeSet::from(["vectors.txt".to_owned()]);
    for v in &list {
        files.insert(format!("{}.bin", v.name));
        let on_disk = std::fs::read(g1_dir().join(format!("{}.bin", v.name)))
            .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        assert_eq!(
            on_disk, v.bytes,
            "{}: the file differs from its builder",
            v.name
        );

        let verdict = decoder_verdict(v);
        match v.class {
            g1::Class::Valid => assert_eq!(verdict, Ok(()), "{}", v.name),
            _ => assert_eq!(verdict, Err(v.code.to_owned()), "{} ({})", v.name, v.why),
        }
        let version = {
            let t = mochi_format::cbor::decode(payload_of(&v.bytes), &Default::default()).unwrap();
            match t {
                mochi_format::cbor::Value::Map(m) => match &m[0].1 {
                    mochi_format::cbor::Value::Uint(n) => *n,
                    _ => panic!(),
                },
                _ => panic!(),
            }
        };
        let _ = version;
        let schema = cddl_validate(v.schema, payload_of(&v.bytes));
        match v.class {
            g1::Class::Valid | g1::Class::Semantic => {
                assert_eq!(schema, Ok(()), "{}: the schema should accept it", v.name)
            }
            g1::Class::Schema => assert!(
                schema.is_err(),
                "{}: the defect is schema-level but {} accepts it",
                v.name,
                v.schema
            ),
        }
    }
    let text: String = list.iter().map(|v| g1_line(v) + "\n").collect();
    assert_eq!(
        std::fs::read_to_string(g1_dir().join("vectors.txt")).unwrap(),
        text,
        "g1/vectors.txt"
    );
    let on_disk: BTreeSet<String> = std::fs::read_dir(g1_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(on_disk, files);
    assert!(list.len() >= 40, "{} vectors", list.len());
}

/// The hostile names survive byte for byte into every profile's summary: the
/// expected set is written out here from the script, not read from a reader.
#[test]
fn hostile_names_are_kept_as_bytes() {
    let mut want: BTreeSet<String> = recipes::HOSTILE.iter().map(|n| hex(n)).collect();
    want.insert(hex(b"dir"));
    let mut nested = b"dir/".to_vec();
    nested.extend_from_slice(recipes::HOSTILE[0]);
    want.insert(hex(&nested));
    for name in [
        "core-valid-hostile-names",
        "tar-valid-hostile-names",
        "enc-valid-hostile-names",
    ] {
        let v: Value = serde_json::from_slice(&read(&format!("{name}.summary.json"))).unwrap();
        let last = v["snapshots"].as_array().unwrap().last().unwrap();
        let got: BTreeSet<String> = last["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(got, want, "{name}");
    }
}

// ---- writing -----------------------------------------------------------------------

fn expected_head_hex(cx: &mut Ctx, spec: &Spec) -> Option<String> {
    spec.expected_head_of.map(|n| head_commit_hex(&cx.base(n)))
}

#[test]
#[ignore = "writes fixtures/golden/r8; run once, deliberately"]
fn write_r8_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    let mut cx = Ctx::new();
    let mut index = Vec::new();
    for spec in specs() {
        let main_path = dir().join(format!("{}.mochi", spec.name));
        // Copies are built with the case; if the case exists they exist.
        let (main, copies) = if main_path.exists() {
            let copies: Vec<Vec<u8>> = (1..=spec.copies)
                .map(|n| read(&copy_name(spec.name, n)))
                .collect();
            (read(&format!("{}.mochi", spec.name)), copies)
        } else {
            let Built { main, copies } = recipes::build(&mut cx, spec.name);
            (main, copies)
        };
        assert_eq!(copies.len(), spec.copies, "{}", spec.name);
        let mut files: Vec<(String, Vec<u8>)> =
            vec![(format!("{}.mochi", spec.name), main.clone())];
        for (i, c) in copies.iter().enumerate() {
            files.push((copy_name(spec.name, i + 1), c.clone()));
        }
        let head = expected_head_hex(&mut cx, &spec);
        let r = reference(
            spec.name,
            spec.passphrase,
            head.as_deref(),
            spec.salvage,
            &main,
            &copies,
        );
        files.push((
            format!("{}.summary.json", spec.name),
            pretty(&r.summary).into_bytes(),
        ));
        for (name, bytes) in files {
            let path = dir().join(&name);
            if let Ok(old) = std::fs::read(&path) {
                assert!(
                    old == bytes,
                    "{name} exists and differs: fixtures are frozen"
                );
                continue;
            }
            std::fs::write(&path, bytes).unwrap();
        }
        index.push(json!({
            "name": spec.name,
            "profile": spec.profile,
            "class": spec.class,
            "fault": spec.fault,
            "spec": spec.spec,
            "fault_matrix_row": spec.row,
            "passphrase": spec.passphrase,
            "expected_head": head,
            "copies": (1..=spec.copies).map(|n| copy_name(spec.name, n)).collect::<Vec<_>>(),
            "salvage": spec.salvage,
            "expect": {
                "verify_exit": r.verify_exit,
                "verify_level": r.verify_level,
                "repair_outcome": r.repair_outcome,
                "repair_exit": r.repair_exit,
            },
        }));
    }
    // The G1 audit vectors (built from the frozen cases above).
    std::fs::create_dir_all(g1_dir()).unwrap();
    let list = g1_list();
    for v in &list {
        let path = g1_dir().join(format!("{}.bin", v.name));
        if let Ok(old) = std::fs::read(&path) {
            assert!(
                old == v.bytes,
                "g1/{}.bin exists and differs: fixtures are frozen",
                v.name
            );
        } else {
            std::fs::write(&path, &v.bytes).unwrap();
        }
    }
    let lines: String = list.iter().map(|v| g1_line(v) + "\n").collect();
    let vp = g1_dir().join("vectors.txt");
    match std::fs::read_to_string(&vp) {
        Ok(old) => assert_eq!(old, lines, "g1/vectors.txt exists and differs"),
        Err(_) => std::fs::write(&vp, lines).unwrap(),
    }
    let text = pretty(&Value::Array(index));
    let path = dir().join("cases.json");
    if let Ok(old) = std::fs::read_to_string(&path) {
        assert_eq!(
            old, text,
            "cases.json exists and differs: fixtures are frozen"
        );
    } else {
        std::fs::write(&path, text).unwrap();
    }
}

// ---- legacy: draft-2 migrate cases (W20) -------------------------------------------

/// `legacy/` holds three archives written under draft 2 (descriptor key 3 =
/// 2), kept as `mochi migrate` cases. The writer's test control for the last
/// draft rebuilds them: the Core and Redundancy archives byte for byte from
/// their recipes (so the control stamps exactly what draft 2 had and nothing
/// else), the Encrypted one up to its random nonces and salts (its descriptor
/// frame and length are equal).
#[test]
fn the_legacy_control_rebuilds_the_checked_in_draft_2_archives() {
    use mochi_core::descriptor::set_legacy_draft_for_tests;
    let cases: Value =
        serde_json::from_slice(&std::fs::read(dir().join("legacy/cases.json")).unwrap()).unwrap();
    let previous = set_legacy_draft_for_tests(Some(2));
    let mut cx = Ctx::new();
    let built: Vec<(String, Vec<u8>, Vec<u8>, bool)> = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let twin = c["equal_in_content_to"].as_str().unwrap();
            let main = recipes::build(&mut cx, twin).main;
            let kept =
                std::fs::read(dir().join("legacy").join(c["file"].as_str().unwrap())).unwrap();
            (
                c["name"].as_str().unwrap().to_owned(),
                main,
                kept,
                c["profile"] == "enc",
            )
        })
        .collect();
    set_legacy_draft_for_tests(previous);
    assert_eq!(built.len(), 3);
    for (name, main, kept, sealed) in built {
        // The descriptor is the first frame: 8 header bytes and its payload.
        let n = 8 + u32::from_le_bytes(kept[4..8].try_into().unwrap()) as usize;
        assert_eq!(main[..n], kept[..n], "{name}: the descriptor frame");
        if sealed {
            assert_eq!(main.len(), kept.len(), "{name}");
        } else {
            assert!(main == kept, "{name}: byte for byte");
        }
    }
}
