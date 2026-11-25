//! R7 freeze (gate G15, plan K24, W19): every machine-readable JSON the CLI
//! writes has a JSON Schema (2020-12) in `docs/schemas/json/`, and real
//! outputs are validated against them here.
//!
//! The outputs come from running the CLI in-process (`mochi_cli::run`) over
//! the R8 golden archives (`fixtures/golden/r8`, all 76 cases) and over
//! temporary archives for the commands R8 cannot exercise (anything that
//! writes). The schemas are checked in the other direction too: mutated
//! outputs must fail (`mutated_outputs_fail`), the enumerations must be the
//! code's (`enumerations_are_the_code_registries`), and the README must list
//! every place in the CLI that writes JSON. Spec §20.6's example is generated
//! from an R8 case and compared (`spec_20_6_example_is_generated_from_r8`).
//!
//! The `jsonschema` crate (MIT) is a dev-dependency of this crate only, built
//! without HTTP or file retrieval: the schemas' `$id`s are under the reserved
//! `.invalid` domain and are resolved from the directory by `Local`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use mochi_cli::exit_code_for;
use mochi_core::report::{Report, Severity};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::ErrorCode;
use serde_json::{json, Value};

const BASE: &str = "https://mochi.invalid/schemas/json/";
const DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Every schema document, by file name. The README and the directory must
/// list the same set.
const SCHEMAS: &[&str] = &[
    "common.schema.json",
    "report-v1.schema.json",
    "error.schema.json",
    "repair-plan-v2.schema.json",
    "gc-plan-v1.schema.json",
    "health-evidence-v1.schema.json",
    "health-evidence-v2.schema.json",
    "health-policy-v1.schema.json",
    "local-heads-v1.schema.json",
    "cli-commit.schema.json",
    "cli-commit-v2.schema.json",
    "cli-maintenance.schema.json",
    "cli-list.schema.json",
    "cli-snapshot-list.schema.json",
    "cli-search.schema.json",
    "cli-dump-index.schema.json",
    "cli-restore.schema.json",
    "cli-rewrite.schema.json",
    "cli-repair-apply.schema.json",
    "cli-rekey-list.schema.json",
    "cli-plan-saved.schema.json",
    "cli-migrate.schema.json",
    "cli-find.schema.json",
    "cli-comment.schema.json",
    "cli-find-v2.schema.json",
];

/// The R8 case spec §20.6's example is generated from.
const SPEC_EXAMPLE_CASE: &str = "core-corrupt-chunk-older";

// ---- files ----------------------------------------------------------------------

/// The schema of one evidence log line, by its in-band number: a commit
/// record is schema 2 (Annex B.2.22), every other record schema 1.
fn evidence_schema(v: &Value) -> &'static str {
    if v["schema"] == 2 {
        "health-evidence-v2.schema.json"
    } else {
        "health-evidence-v1.schema.json"
    }
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn repo_file(rel: &str) -> String {
    let path = repo().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn schema_dir() -> PathBuf {
    repo().join("docs/schemas/json")
}

fn r8() -> PathBuf {
    repo().join("fixtures/golden/r8")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// ---- validation -----------------------------------------------------------------

/// Resolves `$ref`s to the schema directory, never to the network.
struct Local(BTreeMap<String, Value>);

impl jsonschema::Retrieve for Local {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let s = uri.as_str();
        s.strip_prefix(BASE)
            .and_then(|name| self.0.get(name))
            .cloned()
            .ok_or_else(|| format!("{s} is not a schema in docs/schemas/json").into())
    }
}

fn documents() -> BTreeMap<String, Value> {
    SCHEMAS
        .iter()
        .map(|n| (n.to_string(), read_json(&schema_dir().join(n))))
        .collect()
}

fn validators() -> &'static BTreeMap<String, jsonschema::Validator> {
    static V: OnceLock<BTreeMap<String, jsonschema::Validator>> = OnceLock::new();
    V.get_or_init(|| {
        let docs = documents();
        docs.iter()
            .map(|(name, schema)| {
                let v = jsonschema::options()
                    .with_draft(jsonschema::Draft::Draft202012)
                    .with_retriever(Local(docs.clone()))
                    .build(schema)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                (name.clone(), v)
            })
            .collect()
    })
}

/// The schema's complaints about `v`; empty when it is valid.
fn complaints(schema: &str, v: &Value) -> Vec<String> {
    let validator = validators()
        .get(schema)
        .unwrap_or_else(|| panic!("no schema {schema}"));
    validator
        .iter_errors(v)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

fn valid(schema: &str, v: &Value, what: &str) {
    let c = complaints(schema, v);
    assert!(
        c.is_empty(),
        "{what}: does not match {schema}:\n  {}\n{v:#}",
        c.join("\n  ")
    );
}

// ---- running the CLI ------------------------------------------------------------

/// The stand-in Bin: move `path` into a folder of this test process under a
/// free name.
fn test_bin(path: &Path) -> std::io::Result<()> {
    let dir = std::env::temp_dir().join(format!("mochi-r7-bin-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    for n in 0..1000 {
        let to = dir.join(format!("{n}-{name}"));
        if !to.exists() {
            return fs::rename(path, to);
        }
    }
    Err(std::io::Error::other("no free name in the stand-in bin"))
}

struct Out {
    code: u8,
    stdout: String,
    stderr: String,
}

fn mochi<S: AsRef<str>>(args: &[S]) -> Out {
    let mut argv: Vec<OsString> = vec!["mochi".into()];
    argv.extend(args.iter().map(|a| OsString::from(a.as_ref())));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // `--move-sources-to-bin` moves into a folder of this test process, never
    // into the machine's Recycle Bin or Trash.
    let code = mochi_cli::run_with_bin(argv, &mut out, &mut err, std::sync::Arc::new(test_bin));
    Out {
        code,
        stdout: String::from_utf8(out).expect("JSON output is UTF-8"),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    }
}

/// Run with `--json`: standard output is exactly one line holding one JSON
/// value (docs/cli.md). An error line is checked against the error
/// schema and the exit code against `exit_code_for`; `None` then.
fn json_out<S: AsRef<str>>(args: &[S]) -> (u8, Option<Value>) {
    let mut a: Vec<String> = args.iter().map(|s| s.as_ref().to_owned()).collect();
    a.push("--json".into());
    let o = mochi(&a);
    let what = format!("mochi {}", a.join(" "));
    assert_eq!(
        o.stdout.lines().count(),
        1,
        "{what}: one line of JSON expected, got {:?} (stderr {})",
        o.stdout,
        o.stderr
    );
    let v: Value = serde_json::from_str(o.stdout.trim_end())
        .unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {:?}", o.stdout));
    if v.get("error").is_some() {
        valid("error.schema.json", &v, &what);
        let code = v["error"]["code"].as_str().unwrap();
        let code = ErrorCode::ALL
            .iter()
            .find(|c| c.as_str() == code)
            .unwrap_or_else(|| panic!("{what}: {code} is not registered"));
        assert_eq!(o.code, exit_code_for(*code), "{what}: exit for {code}");
        eprintln!("{what}: {v}");
        return (o.code, None);
    }
    (o.code, Some(v))
}

/// As [`json_out`], and the value must match `schema`.
fn checked<S: AsRef<str>>(schema: &str, args: &[S]) -> (u8, Option<Value>) {
    let (code, v) = json_out(args);
    if let Some(v) = &v {
        let what = args
            .iter()
            .map(|s| s.as_ref().to_owned())
            .collect::<Vec<_>>()
            .join(" ");
        valid(schema, v, &what);
        if let Some(e) = v.get("exit_code").and_then(Value::as_u64) {
            assert_eq!(u64::from(code), e, "{what}: exit_code is the exit");
        }
    }
    (code, v)
}

/// As [`checked`], and the command must not have failed with an error line.
fn ok<S: AsRef<str>>(schema: &str, args: &[S]) -> (u8, Value) {
    let (code, v) = checked(schema, args);
    let what: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    (code, v.unwrap_or_else(|| panic!("{what:?}: failed")))
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn passphrase_file(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    fs::write(&p, format!("{text}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    }
    s(&p)
}

/// Every Encrypted archive the tests create wraps its key with a tiny
/// Argon2id cost (the core's `test-controls`; the shipped CLI has no knob).
fn cheap_kdf() {
    mochi_core::publish::test_controls::set_default_kdf(Some(mochi_format::kdf::KdfParams {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    }));
}

// ---- R8 cases --------------------------------------------------------------------

struct Case {
    name: String,
    passphrase: Option<String>,
    copies: Vec<String>,
    salvage: bool,
    expected_head: Option<String>,
}

fn cases() -> Vec<Case> {
    let v = read_json(&r8().join("cases.json"));
    let cases: Vec<Case> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| Case {
            name: c["name"].as_str().unwrap().to_owned(),
            passphrase: c["passphrase"].as_str().map(str::to_owned),
            copies: c["copies"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_owned())
                .collect(),
            salvage: c["salvage"].as_bool().unwrap(),
            expected_head: c["expected_head"].as_str().map(str::to_owned),
        })
        .collect();
    assert_eq!(cases.len(), 76, "R8 has 76 cases");
    cases
}

impl Case {
    fn archive(&self) -> String {
        s(&r8().join(format!("{}.mochi", self.name)))
    }

    /// Global options: the passphrase file, when the case has one.
    fn globals(&self, dir: &Path) -> Vec<String> {
        match &self.passphrase {
            Some(p) => vec![
                "--passphrase-file".into(),
                passphrase_file(dir, &format!("{}.pass", self.name), p),
            ],
            None => Vec::new(),
        }
    }
}

fn with(base: &[String], more: &[&str]) -> Vec<String> {
    base.iter()
        .cloned()
        .chain(more.iter().map(|s| (*s).to_owned()))
        .collect()
}

// ---- the schema set ---------------------------------------------------------------

#[test]
fn every_schema_is_a_valid_2020_12_document() {
    let on_disk: BTreeSet<String> = fs::read_dir(schema_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .collect();
    let listed: BTreeSet<String> = SCHEMAS.iter().map(|s| s.to_string()).collect();
    assert_eq!(on_disk, listed, "docs/schemas/json holds exactly SCHEMAS");
    for (name, doc) in documents() {
        assert_eq!(doc["$schema"], DRAFT, "{name}");
        assert_eq!(doc["$id"], format!("{BASE}{name}"), "{name}");
        assert!(
            doc["title"].as_str().is_some_and(|t| !t.is_empty()),
            "{name}"
        );
        assert!(
            doc["description"].as_str().is_some_and(|t| !t.is_empty()),
            "{name}"
        );
        if let Err(e) = jsonschema::meta::validate(&doc) {
            panic!("{name} is not a valid JSON Schema: {e}");
        }
    }
    assert_eq!(validators().len(), SCHEMAS.len());
}

/// The table in docs/schemas/json/README.md: schema file, and the functions in
/// `crates/mochi-cli/src` that write it.
fn readme_table() -> Vec<(String, BTreeSet<String>)> {
    let doc = repo_file("docs/schemas/json/README.md");
    let lines: Vec<&str> = doc.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with("| Schema |"))
        .expect("the README's schema table");
    lines[start + 2..]
        .iter()
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            // `\|` inside a cell is a literal bar, not a separator.
            let l = l.replace("\\|", "/");
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let name = cells[1].trim_matches('`').to_owned();
            let sources = cells[3]
                .split(',')
                .map(|c| c.trim().trim_matches('`').to_owned())
                .filter(|c| !c.is_empty() && c != "—")
                .collect();
            (name, sources)
        })
        .collect()
}

/// Every function in the CLI's sources that writes JSON: it emits a value,
/// serializes one to bytes, or prints the error line. Test modules excluded.
/// The local head history and evidence log the CLI writes live in
/// `mochi-core` (shared with the desktop app, Annex B.2.22) and are scanned
/// too.
fn json_writing_functions() -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for file in [
        "crates/mochi-cli/src/commands.rs",
        "crates/mochi-cli/src/lib.rs",
        "crates/mochi-cli/src/state.rs",
        "crates/mochi-cli/src/render.rs",
        "crates/mochi-cli/src/passphrase.rs",
        "crates/mochi-core/src/storage/os/history.rs",
    ] {
        let text = repo_file(file);
        let mut current = String::new();
        for line in text.lines() {
            if line.starts_with("#[cfg(test)]") {
                break;
            }
            let t = line.trim_start();
            let def = t
                .strip_prefix("pub fn ")
                .or_else(|| t.strip_prefix("pub(crate) fn "))
                .or_else(|| t.strip_prefix("fn "));
            if let Some(rest) = def {
                current = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap()
                    .to_owned();
                continue;
            }
            // `emit_tagged` is the one helper that adds the in-band tag and
            // then calls `emit_json`; its callers are the writers.
            if current == "emit_tagged" {
                continue;
            }
            if t.contains("emit_json(")
                || t.contains("emit_tagged(")
                || t.contains("serde_json::to_vec")
                || t.contains("\"{value}\"")
            {
                found.insert(current.clone());
            }
        }
    }
    found
}

#[test]
fn readme_lists_every_schema_and_every_place_that_writes_json() {
    let table = readme_table();
    let names: BTreeSet<String> = table.iter().map(|(n, _)| n.clone()).collect();
    let listed: BTreeSet<String> = SCHEMAS.iter().map(|s| s.to_string()).collect();
    assert_eq!(names, listed, "README table rows");
    let sources: BTreeSet<String> = table.into_iter().flat_map(|(_, s)| s).collect();
    assert_eq!(
        sources,
        json_writing_functions(),
        "every function that writes JSON is named in the README (a new output needs a schema)"
    );
}

/// R7 O2 (owner decision 2026-10-14): each of the command-output
/// schemas requires an in-band tag, a `const` named after its file
/// (`cli-list.schema.json` -> `mochi-cli-list-v1`), in every branch of a
/// `oneOf`. The eight frozen documents carry no such property.
#[test]
fn command_output_schemas_require_their_in_band_tag() {
    let docs = documents();
    let mut seen = 0;
    for name in SCHEMAS {
        let doc = &docs[*name];
        let Some(stem) = name
            .strip_prefix("cli-")
            .and_then(|n| n.strip_suffix(".schema.json"))
        else {
            // The frozen documents are not touched by O2.
            let has_tag = doc["properties"]["schema"]["const"]
                .as_str()
                .is_some_and(|t| t.starts_with("mochi-cli-"));
            assert!(!has_tag, "{name} is frozen and must not carry an O2 tag");
            continue;
        };
        // `cli-NAME.schema.json` carries `mochi-cli-NAME-v1`; a later version is
        // its own file, `cli-NAME-vN.schema.json`, carrying `mochi-cli-NAME-vN`.
        let versioned = stem
            .rsplit_once("-v")
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        let tag = if versioned {
            format!("mochi-cli-{stem}")
        } else {
            format!("mochi-cli-{stem}-v1")
        };
        let branches: Vec<&Value> = match doc.get("oneOf") {
            Some(Value::Array(b)) => b.iter().collect(),
            _ => vec![doc],
        };
        for b in branches {
            assert_eq!(b["properties"]["schema"]["const"], json!(tag), "{name}");
            let req = b["required"].as_array().expect("required");
            assert!(req.contains(&json!("schema")), "{name} requires its tag");
        }
        seen += 1;
    }
    assert_eq!(seen, 16, "the sixteen command-output schemas");
}

// ---- enumerations ------------------------------------------------------------------

fn all_statuses() -> Vec<Status> {
    let all = [
        Status::Pass,
        Status::Fail,
        Status::Degraded,
        Status::Unknown,
        Status::Overdue,
        Status::Unsupported,
    ];
    for s in all {
        match s {
            Status::Pass
            | Status::Fail
            | Status::Degraded
            | Status::Unknown
            | Status::Overdue
            | Status::Unsupported => {}
        }
    }
    all.to_vec()
}

fn all_levels() -> Vec<VerificationLevel> {
    use VerificationLevel::*;
    let all = [
        Structural,
        Referential,
        StoredIntegrity,
        ContentIntegrity,
        Restoration,
        Search,
        Inventory,
        DisasterRecovery,
    ];
    for l in all {
        match l {
            Structural | Referential | StoredIntegrity | ContentIntegrity | Restoration
            | Search | Inventory | DisasterRecovery => {}
        }
    }
    all.to_vec()
}

/// Each item's serialized string (`serde` is not a dependency of this crate).
macro_rules! strings {
    ($items:expr) => {
        $items
            .iter()
            .map(|i| {
                serde_json::to_value(i)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<String>>()
    };
}

fn enum_of(def: &str) -> Vec<String> {
    let common = read_json(&schema_dir().join("common.schema.json"));
    common["$defs"][def]["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("common $defs/{def} has no enum"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

/// R7 section 6's first column, in order.
fn r7_codes() -> Vec<String> {
    let doc = repo_file("docs/ratification/R7-reports-and-error-codes-draft.md");
    let lines: Vec<&str> = doc.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with("## 6. Error codes"))
        .unwrap();
    lines[start + 1..]
        .iter()
        .take_while(|l| !l.starts_with("## "))
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            l.split('|')
                .nth(1)
                .unwrap()
                .trim()
                .trim_matches('`')
                .to_owned()
        })
        .collect()
}

#[test]
fn enumerations_are_the_code_registries() {
    let registry: Vec<String> = ErrorCode::ALL
        .iter()
        .map(|c| c.as_str().to_owned())
        .collect();
    assert_eq!(
        enum_of("error-code"),
        registry,
        "error codes, in registry order"
    );
    assert_eq!(r7_codes(), registry, "R7 section 6 is the registry");
    // Each code's serialized form is its registered string.
    assert_eq!(strings!(ErrorCode::ALL), registry);

    let statuses: BTreeSet<String> = strings!(&all_statuses()).into_iter().collect();
    assert_eq!(
        enum_of("status").into_iter().collect::<BTreeSet<_>>(),
        statuses
    );
    assert_eq!(enum_of("dimension"), strings!(&Dimension::ALL));
    assert_eq!(enum_of("level"), strings!(&all_levels()));
    assert_eq!(
        enum_of("severity"),
        strings!(&[Severity::Info, Severity::Warning, Severity::Error])
    );
    use mochi_core::report::FreshnessAnchorKind as A;
    let report = read_json(&schema_dir().join("report-v1.schema.json"));
    let anchors: Vec<String> = report["properties"]["freshness_anchor"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(anchors, strings!(&[A::None, A::User, A::LocalHistory]));
    let exits: Vec<u8> = read_json(&schema_dir().join("common.schema.json"))["$defs"]["exit-code"]
        ["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| u8::try_from(v.as_u64().unwrap()).unwrap())
        .collect();
    use mochi_cli::exit;
    assert_eq!(
        exits,
        [
            exit::OK,
            exit::FAILED,
            exit::DEGRADED,
            exit::ERROR,
            exit::UNSUPPORTED
        ]
    );
    // The report's required fields are what `Report` serializes.
    let fields: BTreeSet<String> = report["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let serialized = serde_json::to_value(Report::new(VerificationLevel::Structural)).unwrap();
    let keys: BTreeSet<String> = serialized.as_object().unwrap().keys().cloned().collect();
    assert_eq!(fields, keys);
}

// ---- R8: reports, evidence, local heads ------------------------------------------

/// verify and fsck over every case, recording evidence in a state directory
/// per case; then health for the case's head, the evidence lines, and the
/// local head history.
#[test]
fn r8_reports_evidence_and_heads_validate() {
    let levels = [
        "structural",
        "referential",
        "stored",
        "content",
        "restoration",
        "search",
        "inventory",
        "disaster-recovery",
    ];
    let tmp = tempfile::tempdir().unwrap();
    let (mut reports, mut evidence, mut heads, mut health) = (0, 0, 0, 0);
    for (i, c) in cases().iter().enumerate() {
        let state = tmp.path().join(format!("state-{}", c.name));
        let mut g = vec!["--state-dir".to_owned(), s(&state)];
        g.extend(c.globals(tmp.path()));
        let a = c.archive();
        let mut verify = with(&g, &["verify", &a]);
        if let Some(h) = &c.expected_head {
            verify.extend(["--expected-head".to_owned(), h.clone()]);
        }
        for args in [
            verify,
            with(&g, &["fsck", &a, "--level", levels[i % levels.len()]]),
            with(&g, &["verify", &a, "--require-freshness"]),
        ] {
            let (_, v) = checked("report-v1.schema.json", &args);
            reports += usize::from(v.is_some());
        }
        let (_, v) = checked("report-v1.schema.json", &with(&g, &["health", &a]));
        health += usize::from(v.is_some());
        if let Ok(dir) = fs::read_dir(state.join("evidence")) {
            for e in dir {
                let text = fs::read_to_string(e.unwrap().path()).unwrap();
                for line in text.lines() {
                    let v: Value = serde_json::from_str(line).unwrap();
                    valid(evidence_schema(&v), &v, &c.name);
                    evidence += 1;
                }
            }
        }
        let h = state.join("heads.json");
        if h.exists() {
            valid("local-heads-v1.schema.json", &read_json(&h), &c.name);
            heads += 1;
        }
    }
    println!(
        "R7 over R8: {reports} verify/fsck reports, {health} health reports, \
         {evidence} evidence lines, {heads} head histories"
    );
    // Most cases open far enough for a report; health needs a locatable head.
    assert!(reports >= 3 * 70, "{reports} reports");
    assert!(health >= 60, "{health} health reports");
    assert!(evidence >= 100, "{evidence} evidence lines");
    assert!(heads >= 20, "{heads} head histories");
}

#[test]
fn r8_repair_plans_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let mut plans = 0;
    let mut outcomes = BTreeSet::new();
    let mut with_parity_copies_salvage = [false; 3];
    for c in cases() {
        let mut args = c.globals(tmp.path());
        args.extend([
            "--no-local-history".into(),
            "repair".into(),
            "plan".into(),
            c.archive(),
        ]);
        for copy in &c.copies {
            args.extend(["--copy".to_owned(), s(&r8().join(copy))]);
        }
        if c.salvage {
            args.push("--salvage".into());
        }
        let (_, v) = checked("repair-plan-v2.schema.json", &args);
        if let Some(v) = v {
            plans += 1;
            outcomes.insert(v["outcome"].as_str().unwrap().to_owned());
            with_parity_copies_salvage[0] |= !v["parity"].is_null();
            with_parity_copies_salvage[1] |= !v["copies"].as_array().unwrap().is_empty();
            with_parity_copies_salvage[2] |= !v["salvage"].is_null();
        }
    }
    println!("R7 over R8: {plans} repair plans, outcomes {outcomes:?}");
    assert!(plans >= 70, "{plans} plans");
    assert_eq!(outcomes.len(), 3, "every outcome seen: {outcomes:?}");
    assert_eq!(with_parity_copies_salvage, [true; 3]);
}

/// The read-only commands over every case: each output is its schema's or
/// the error line.
#[test]
fn r8_read_only_outputs_validate() {
    let tmp = tempfile::tempdir().unwrap();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for c in cases() {
        let g = with(&c.globals(tmp.path()), &["--no-local-history"]);
        let a = c.archive();
        let runs: Vec<(&str, Vec<String>)> = vec![
            ("gc-plan-v1.schema.json", with(&g, &["gc", "plan", &a])),
            ("cli-list.schema.json", with(&g, &["list", &a])),
            (
                "cli-list.schema.json",
                with(&g, &["list", &a, "--snapshot", "0"]),
            ),
            (
                "cli-snapshot-list.schema.json",
                with(&g, &["snapshot", "list", &a]),
            ),
            ("cli-search.schema.json", with(&g, &["search", &a])),
            (
                "cli-search.schema.json",
                with(&g, &["search", &a, "--snapshot", "all", "-i", "A"]),
            ),
            (
                "cli-search.schema.json",
                with(
                    &g,
                    &["search", &a, "--snapshot", "retained", "--kind", "file"],
                ),
            ),
            (
                "cli-search.schema.json",
                with(&g, &["search", &a, "--snapshot", "1"]),
            ),
            ("cli-dump-index.schema.json", with(&g, &["dump-index", &a])),
            (
                "cli-rekey-list.schema.json",
                with(&g, &["rekey", &a, "--list"]),
            ),
        ];
        for (schema, args) in runs {
            let (_, v) = checked(schema, &args);
            *seen.entry(schema).or_default() += usize::from(v.is_some());
        }
    }
    println!("R7 over R8: real outputs per schema {seen:?}");
    for (schema, n) in &seen {
        assert!(*n >= 10, "{schema}: only {n} real outputs");
    }
}

/// `mochi find` over the whole R8 set: every case's names, the draft-2
/// archives as unsupported, the Encrypted ones as not searched, the damaged
/// ones with their codes. One real output, validated.
#[test]
fn r8_find_output_validates() {
    let (code, v) = checked(
        "cli-find.schema.json",
        &[
            "--no-local-history".to_owned(),
            "find".into(),
            "*".into(),
            s(&r8()),
        ],
    );
    let v = v.expect("find writes its report, not an error");
    assert_eq!(code, 2, "R8 holds Encrypted, damaged and draft archives");
    let c = &v["coverage"];
    assert_eq!(c["complete"], false);
    let reasons: BTreeSet<&str> = c["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["reason"].as_str().unwrap())
        .collect();
    for r in ["encrypted", "unsupported"] {
        assert!(reasons.contains(r), "{r}: {reasons:?}");
    }
    assert!(c["archives_searched"].as_u64().unwrap() >= 40, "{c}");
}

// ---- temporary archives: everything that writes ----------------------------------

/// A scratch area: a state directory, inputs, and the CLI run with them.
struct Area {
    dir: tempfile::TempDir,
}

impl Area {
    fn new() -> Self {
        cheap_kdf();
        let a = Area {
            dir: tempfile::tempdir().unwrap(),
        };
        let src = a.p("src");
        fs::create_dir_all(src.join("d")).unwrap();
        fs::write(src.join("a.txt"), b"alpha").unwrap();
        fs::write(src.join("d/b.txt"), vec![7u8; 70_000]).unwrap();
        fs::write(a.p("more.txt"), b"more").unwrap();
        a
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn s(&self, rel: &str) -> String {
        s(&self.p(rel))
    }

    fn g(&self, more: &[&str]) -> Vec<String> {
        with(&["--state-dir".to_owned(), self.s("state")], more)
    }

    fn copy_r8(&self, case: &str) -> String {
        let to = self.p(&format!("{case}.mochi"));
        fs::copy(r8().join(format!("{case}.mochi")), &to).unwrap();
        s(&to)
    }
}

/// One real instance of every schema, from the R8 cases and temporary
/// archives; the corpus the mutation test works on. Every instance is
/// validated as it is produced.
fn corpus() -> Vec<(&'static str, Value)> {
    let a = Area::new();
    let mut out: Vec<(&'static str, Value)> = Vec::new();
    let mut keep = |schema: &'static str, v: Value| out.push((schema, v));
    let arc = a.s("a.mochi");

    // create, with a skipped entry where the platform has symbolic links.
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", a.p("src/link")).unwrap();
    let (_, v) = ok(
        "cli-commit.schema.json",
        &a.g(&["create", &arc, &a.s("src")]),
    );
    #[cfg(unix)]
    assert_eq!(v["skipped"].as_array().unwrap().len(), 1);
    keep("cli-commit.schema.json", v);
    let (_, v) = ok(
        "cli-commit.schema.json",
        &a.g(&["append", &arc, &a.s("more.txt"), "--delete", "src/d"]),
    );
    assert_eq!(v["deleted"].as_array().unwrap().len(), 2);
    keep("cli-commit.schema.json", v);

    // --move-sources-to-bin: commit v2 (Annex B.2.23), one folder moved whole
    // and one kept because it still holds what was not archived.
    fs::create_dir_all(a.p("tobin/inner")).unwrap();
    fs::write(a.p("tobin/inner/x.txt"), b"x").unwrap();
    fs::create_dir_all(a.p("stays")).unwrap();
    fs::write(a.p("stays/y.txt"), b"y").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("y.txt", a.p("stays/link")).unwrap();
    let (_, v) = ok(
        "cli-commit-v2.schema.json",
        &a.g(&[
            "create",
            &a.s("moved.mochi"),
            &a.s("tobin"),
            &a.s("stays"),
            "--move-sources-to-bin",
        ]),
    );
    assert!(!a.p("tobin").exists());
    #[cfg(unix)]
    assert_eq!(v["sources_kept"][0]["reason"], "it is not empty");
    keep("cli-commit-v2.schema.json", v);

    // An interrupted write's tail truncated, with nothing else to commit.
    let torn = a.copy_r8("core-interrupted-after-data");
    let (_, v) = ok(
        "cli-commit.schema.json",
        &a.g(&[
            "append",
            &torn,
            "--truncate-tail",
            // Windows cannot confirm directory durability (G6 policy B).
            "--accept-unconfirmed-durability",
        ]),
    );
    assert_eq!(v["committed"], false);
    keep("cli-commit.schema.json", v);
    // And then a commit after a truncation.
    let torn2 = a.copy_r8("core-interrupted-torn-footer");
    let (_, v) = ok(
        "cli-commit.schema.json",
        &a.g(&[
            "append",
            &torn2,
            &a.s("more.txt"),
            "--truncate-tail",
            // Windows cannot confirm directory durability (G6 policy B).
            "--accept-unconfirmed-durability",
        ]),
    );
    assert!(v["truncation"].is_array());
    keep("cli-commit.schema.json", v);

    for args in [
        vec!["snapshot", "retain", &arc, "0", "--label", "audit"],
        vec!["snapshot", "expire", &arc, "0", "1", "--confirm"],
        vec!["snapshot", "release", &arc, "--label", "audit", "--confirm"],
        vec!["checkpoint", &arc],
    ] {
        let (_, v) = ok("cli-maintenance.schema.json", &a.g(&args));
        keep("cli-maintenance.schema.json", v);
    }
    let (_, v) = ok("cli-list.schema.json", &a.g(&["list", &arc]));
    keep("cli-list.schema.json", v);
    let (_, v) = ok(
        "cli-snapshot-list.schema.json",
        &a.g(&["snapshot", "list", &arc]),
    );
    keep("cli-snapshot-list.schema.json", v);
    let (_, v) = ok(
        "cli-search.schema.json",
        &a.g(&["search", &arc, "b", "--snapshot", "all"]),
    );
    keep("cli-search.schema.json", v);
    let (_, v) = ok(
        "cli-dump-index.schema.json",
        &a.g(&[
            "dump-index",
            &arc,
            "--table",
            "commits",
            "--table",
            "file_versions",
        ]),
    );
    keep("cli-dump-index.schema.json", v);

    // verify, fsck, restore-test, and health: reports, evidence, heads.
    let (_, v) = ok("report-v1.schema.json", &a.g(&["verify", &arc]));
    keep("report-v1.schema.json", v);
    let (_, v) = ok("report-v1.schema.json", &a.g(&["fsck", &arc]));
    keep("report-v1.schema.json", v);
    // A non-default reader limit: an info finding with expected and observed.
    let (_, v) = ok(
        "report-v1.schema.json",
        &a.g(&[
            "--limit",
            "max-frame-len=999999",
            "verify",
            &arc,
            "--level",
            "structural",
        ]),
    );
    assert_eq!(v["findings"][0]["code"], "LIMIT_NOT_DEFAULT");
    keep("report-v1.schema.json", v);
    let (_, v) = ok(
        "cli-restore.schema.json",
        &a.g(&["restore-test", &arc, "-C", &a.s("restored")]),
    );
    keep("cli-restore.schema.json", v);
    fs::create_dir_all(a.p("busy")).unwrap();
    fs::write(a.p("busy/more.txt"), b"in the way").unwrap();
    let (code, v) = ok(
        "cli-restore.schema.json",
        &a.g(&["get", &arc, "-C", &a.s("busy")]),
    );
    assert_eq!(
        code,
        mochi_cli::exit::DEGRADED,
        "a collision is not restored"
    );
    keep("cli-restore.schema.json", v);
    let policy = a.p("policy.json");
    let p = json!({"schema": "mochi-health-policy-v1", "required": ["integrity", "recoverability"],
                   "max_age_days": {"verify": 30, "restore_test": 90}});
    fs::write(&policy, p.to_string()).unwrap();
    valid("health-policy-v1.schema.json", &p, "policy");
    keep("health-policy-v1.schema.json", p);
    let (_, v) = ok(
        "report-v1.schema.json",
        &a.g(&["health", &arc, "--policy", &s(&policy)]),
    );
    keep("report-v1.schema.json", v);

    // Plans saved with --output, and the rewrites they approve.
    let (_, v) = ok(
        "cli-plan-saved.schema.json",
        &a.g(&["gc", "plan", &arc, "--output", &a.s("gc.json")]),
    );
    assert_eq!(v["collects_anything"], true);
    keep("cli-plan-saved.schema.json", v);
    let saved = read_json(&a.p("gc.json"));
    valid("gc-plan-v1.schema.json", &saved, "saved gc plan");
    keep("gc-plan-v1.schema.json", saved);
    let (_, v) = ok(
        "cli-rewrite.schema.json",
        &a.g(&[
            "gc",
            "apply",
            &arc,
            "--plan",
            &a.s("gc.json"),
            "--output",
            &a.s("gc.mochi"),
        ]),
    );
    assert!(!v["collected"].as_array().unwrap().is_empty());
    keep("cli-rewrite.schema.json", v);
    let (_, v) = ok(
        "cli-rewrite.schema.json",
        &a.g(&[
            "compact",
            &arc,
            "--output",
            &a.s("compact.mochi"),
            "--no-verify-content",
        ]),
    );
    keep("cli-rewrite.schema.json", v);

    let damaged = a.copy_r8("core-corrupt-chunk-head");
    // A failed verification: its evidence line carries finding_codes.
    let (code, v) = ok("report-v1.schema.json", &a.g(&["verify", &damaged]));
    assert_eq!(code, mochi_cli::exit::FAILED);
    keep("report-v1.schema.json", v);
    let (_, v) = ok(
        "cli-plan-saved.schema.json",
        &a.g(&["repair", "plan", &damaged, "--output", &a.s("repair.json")]),
    );
    keep("cli-plan-saved.schema.json", v);
    let saved = read_json(&a.p("repair.json"));
    valid("repair-plan-v2.schema.json", &saved, "saved repair plan");
    let (code, v) = ok(
        "cli-repair-apply.schema.json",
        &a.g(&[
            "repair",
            "apply",
            &damaged,
            "--plan",
            &a.s("repair.json"),
            "--output",
            &a.s("repaired.mochi"),
        ]),
    );
    assert_eq!(code, mochi_cli::exit::DEGRADED, "a partial repair");
    keep("cli-repair-apply.schema.json", v);
    ok(
        "cli-plan-saved.schema.json",
        &a.g(&["repair", "plan", &arc, "--output", &a.s("repair-a.json")]),
    );
    let (_, v) = ok(
        "cli-repair-apply.schema.json",
        &a.g(&[
            "repair",
            "apply",
            &arc,
            "--plan",
            &a.s("repair-a.json"),
            "--output",
            &a.s("repaired-a.mochi"),
        ]),
    );
    assert_eq!(v["outcome"], "complete");
    keep("cli-repair-apply.schema.json", v);

    // A richer repair plan, with copies, parity, and salvage.
    for (case, extra) in [
        (
            "copy-damaged-chunk",
            vec![
                "--copy".to_owned(),
                s(&r8().join("copy-damaged-chunk.copy1.mochi")),
            ],
        ),
        ("red-corrupt-beyond-capacity", vec![]),
        ("salvage-destroyed-footers", vec!["--salvage".to_owned()]),
    ] {
        let mut args = vec![
            "--no-local-history".to_owned(),
            "repair".into(),
            "plan".into(),
            s(&r8().join(format!("{case}.mochi"))),
        ];
        args.extend(extra);
        let (_, v) = ok("repair-plan-v2.schema.json", &args);
        keep("repair-plan-v2.schema.json", v);
    }
    let (_, v) = ok(
        "gc-plan-v1.schema.json",
        &[
            "--no-local-history".to_owned(),
            "gc".into(),
            "plan".into(),
            s(&r8().join("core-valid-retention.mochi")),
        ],
    );
    keep("gc-plan-v1.schema.json", v);
    let (_, v) = ok(
        "report-v1.schema.json",
        &[
            "--no-local-history".to_owned(),
            "verify".into(),
            s(&r8().join("red-corrupt-within-capacity.mochi")),
        ],
    );
    keep("report-v1.schema.json", v);
    let (_, v) = ok(
        "cli-list.schema.json",
        &[
            "--no-local-history".to_owned(),
            "list".into(),
            s(&r8().join("core-valid-hostile-names.mochi")),
        ],
    );
    keep("cli-list.schema.json", v);

    // Encrypted: create, rekey add / list / remove, re-encrypt.
    let pass = passphrase_file(a.dir.path(), "p1", "first passphrase");
    let pass2 = passphrase_file(a.dir.path(), "p2", "second passphrase");
    let enc = a.s("e.mochi");
    let mut pg = a.g(&["--passphrase-file", &pass]);
    let (_, v) = ok(
        "cli-commit.schema.json",
        &with(&pg, &["create", "--encrypted", &enc, &a.s("src/a.txt")]),
    );
    assert_eq!(v["encrypted"], true);
    keep("cli-commit.schema.json", v);
    let (_, v) = ok(
        "cli-maintenance.schema.json",
        &with(
            &pg,
            &[
                "rekey",
                &enc,
                "--add-passphrase",
                "--new-passphrase-file",
                &pass2,
            ],
        ),
    );
    keep("cli-maintenance.schema.json", v);
    let (_, v) = ok(
        "cli-rekey-list.schema.json",
        &with(&pg, &["rekey", &enc, "--list"]),
    );
    // The first passphrase's envelope, written by commit 0 (the list is in
    // envelope-ID order, which is random).
    let first = v["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["created_at_seq"] == 0)
        .unwrap()["envelope_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(v["envelopes"].as_array().unwrap().len(), 2);
    keep("cli-rekey-list.schema.json", v);
    pg = a.g(&["--passphrase-file", &pass2]);
    let (_, v) = ok(
        "cli-maintenance.schema.json",
        &with(&pg, &["rekey", &enc, "--remove-passphrase", &first]),
    );
    assert_eq!(v["change"]["revocation"], false);
    keep("cli-maintenance.schema.json", v);
    let (_, v) = ok(
        "cli-rewrite.schema.json",
        &with(
            &pg,
            &[
                "rekey",
                &enc,
                "--reencrypt",
                "-o",
                &a.s("e2.mochi"),
                "--new-passphrase-file",
                &pass,
            ],
        ),
    );
    assert_eq!(v["action"], "reencrypt");
    keep("cli-rewrite.schema.json", v);

    // `migrate`: the three draft-2 archives kept in R8's `legacy/` (Core,
    // Encrypted, and with a recovery record).
    for (file, pass) in [
        ("draft2-core-history.mochi", None),
        ("draft2-enc-history.mochi", Some("r8 golden passphrase")),
        ("draft2-red-5pct.mochi", None),
    ] {
        let from = r8().join("legacy").join(file);
        let mut args = vec![
            "migrate".to_owned(),
            s(&from),
            "--output".into(),
            a.s(&format!("migrated-{file}")),
        ];
        if let Some(p) = pass {
            args.push("--passphrase-file".into());
            args.push(passphrase_file(&a.p(""), "legacy-pass", p));
        }
        let (_, v) = ok(
            "cli-migrate.schema.json",
            &a.g(&args.iter().map(String::as_str).collect::<Vec<_>>()),
        );
        assert_eq!(v["resealed"], pass.is_some());
        keep("cli-migrate.schema.json", v);
    }

    // find (B.2.18): a complete search with hits, and an incomplete one
    // with every kind of skip that carries a code and one that does not.
    fs::create_dir_all(a.p("find-ok/sub")).unwrap();
    fs::copy(a.p("a.mochi"), a.p("find-ok/sub/a.mochi")).unwrap();
    let (code, v) = ok(
        "cli-find.schema.json",
        &a.g(&["find", "*", &a.s("find-ok")]),
    );
    assert_eq!(code, 0);
    assert_eq!(v["coverage"]["complete"], true);
    assert!(v["total_hits"].as_u64().unwrap() > 0, "{v}");
    keep("cli-find.schema.json", v);
    fs::create_dir_all(a.p("find-mixed")).unwrap();
    fs::copy(a.p("a.mochi"), a.p("find-mixed/a.mochi")).unwrap();
    fs::copy(
        r8().join("legacy/draft2-core-history.mochi"),
        a.p("find-mixed/draft.mochi"),
    )
    .unwrap();
    fs::copy(
        r8().join("enc-valid-history.mochi"),
        a.p("find-mixed/enc.mochi"),
    )
    .unwrap();
    fs::write(a.p("find-mixed/fake.mochi"), b"not an archive").unwrap();
    let (code, v) = ok(
        "cli-find.schema.json",
        &a.g(&["find", "*.txt", &a.s("find-mixed")]),
    );
    assert_eq!(code, 2);
    assert_eq!(v["coverage"]["archives_skipped"], 3, "{v}");
    keep("cli-find.schema.json", v);

    // find with several names and options (B.2.21): v2, complete and incomplete.
    let (code, v) = ok(
        "cli-find-v2.schema.json",
        &a.g(&[
            "find",
            "--name",
            "*",
            "--name",
            "no-such-name",
            &a.s("find-ok"),
        ]),
    );
    assert_eq!(code, 0);
    assert_eq!(v["query"]["terms"][1]["hits"], 0, "{v}");
    keep("cli-find-v2.schema.json", v);
    let (code, v) = ok(
        "cli-find-v2.schema.json",
        &a.g(&[
            "find",
            "--match-case",
            "--whole-name",
            "--files-only",
            "--no-subfolders",
            "--name",
            "*.txt",
            "--name",
            "a",
            &a.s("find-mixed"),
        ]),
    );
    assert_eq!(code, 2);
    assert_eq!(v["query"]["subfolders"], false);
    keep("cli-find-v2.schema.json", v);

    // comment (B.2.19): set with append, then read at the head and at a
    // version without one.
    let (_, v) = ok(
        "cli-commit.schema.json",
        &a.g(&["append", &arc, "--comment", "Holiday photos\n\tday 1"]),
    );
    keep("cli-commit.schema.json", v);
    let (_, v) = ok("cli-comment.schema.json", &a.g(&["comment", &arc]));
    assert_eq!(v["comment"], "Holiday photos\n\tday 1");
    keep("cli-comment.schema.json", v);
    let (_, v) = ok(
        "cli-comment.schema.json",
        &a.g(&["comment", &arc, "--snapshot", "0"]),
    );
    assert_eq!(v["comment"], Value::Null);
    keep("cli-comment.schema.json", v);

    // Evidence lines and the local head history.
    for e in fs::read_dir(a.p("state/evidence")).unwrap() {
        let text = fs::read_to_string(e.unwrap().path()).unwrap();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            valid(evidence_schema(&v), &v, line);
            keep(evidence_schema(&v), v);
        }
    }
    let heads = read_json(&a.p("state/heads.json"));
    valid("local-heads-v1.schema.json", &heads, "heads.json");
    keep("local-heads-v1.schema.json", heads);

    // An error line.
    let o = mochi(&a.g(&["verify", &a.s("absent.mochi"), "--json"]));
    let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
    valid("error.schema.json", &v, "error");
    keep("error.schema.json", v);

    let covered: BTreeSet<&str> = out.iter().map(|(s, _)| *s).collect();
    let all: BTreeSet<&str> = SCHEMAS
        .iter()
        .copied()
        .filter(|s| *s != "common.schema.json")
        .collect();
    assert_eq!(
        covered, all,
        "the corpus holds a real instance of every schema"
    );
    out
}

#[test]
fn outputs_of_commands_that_write_validate() {
    let c = corpus();
    assert!(c.len() > 40, "{} instances", c.len());
}

/// The CLI accepts a policy file exactly when the schema does.
#[test]
fn health_policy_schema_agrees_with_the_cli() {
    let a = Area::new();
    let arc = a.s("a.mochi");
    ok(
        "cli-commit.schema.json",
        &a.g(&["create", &arc, &a.s("src/a.txt")]),
    );
    let policies = [
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"]}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity","integrity"]}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["freshness"],"max_age_days":{}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["recoverability"],"max_age_days":{"restore_test":0}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity","recoverability","durability","searchability","freshness","retention_compliance","key_availability"],"max_age_days":{"verify":4294967295,"restore_test":1}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":[]}"#,
        r#"{"schema":"mochi-health-policy-v2","required":["integrity"]}"#,
        r#"{"required":["integrity"]}"#,
        r#"{"schema":"mochi-health-policy-v1"}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"surprise":1}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["speed"]}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"weekly":7}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"verify":-1}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"verify":4294967296}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["integrity"],"max_age_days":{"verify":"30"}}"#,
        r#"{"schema":"mochi-health-policy-v1","required":"integrity"}"#,
        r#"{"schema":"mochi-health-policy-v1","required":["INTEGRITY"]}"#,
        r#"["mochi-health-policy-v1"]"#,
    ];
    let (mut accepted, mut refused) = (0, 0);
    for (i, text) in policies.iter().enumerate() {
        let path = a.p(&format!("policy-{i}.json"));
        fs::write(&path, text).unwrap();
        let o = mochi(&a.g(&["health", &arc, "--policy", &s(&path), "--json"]));
        let v: Value = serde_json::from_str(o.stdout.trim()).unwrap();
        let cli_accepts = v.get("error").is_none();
        if !cli_accepts {
            assert_eq!(v["error"]["code"], "INVALID_ARGUMENT", "{text}");
        }
        let schema_accepts = complaints(
            "health-policy-v1.schema.json",
            &serde_json::from_str(text).unwrap(),
        )
        .is_empty();
        assert_eq!(schema_accepts, cli_accepts, "{text}");
        if cli_accepts {
            accepted += 1;
        } else {
            refused += 1;
        }
    }
    assert_eq!((accepted, refused), (5, 13));
}

// ---- mutations ---------------------------------------------------------------------

/// Every node of `v` with its JSON pointer.
fn nodes<'a>(v: &'a Value, at: String, out: &mut Vec<(String, &'a Value)>) {
    out.push((at.clone(), v));
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                nodes(x, format!("{at}/{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                nodes(x, format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// The pointer with array indices, map keys chosen by the data (table names,
/// archive IDs), and the like replaced by `*`, so that one allowance covers
/// every element.
fn pattern(schema: &str, pointer: &str) -> String {
    let parts: Vec<&str> = pointer.split('/').collect();
    let mut out = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let free_key = (schema == "cli-dump-index.schema.json"
            && i > 0
            && parts[i - 1] == "tables")
            || (schema == "local-heads-v1.schema.json" && i > 0 && parts[i - 1] == "heads")
            || (schema == "health-evidence-v1.schema.json" && i == 2 && parts[1] == "dimensions");
        if p.parse::<u64>().is_ok() || free_key {
            out.push("*");
        } else {
            out.push(p);
        }
    }
    out.join("/")
}

fn set(v: &mut Value, pointer: &str, new: Value) {
    *v.pointer_mut(pointer)
        .unwrap_or_else(|| panic!("no {pointer}")) = new;
}

fn remove(v: &mut Value, pointer: &str) {
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    v.pointer_mut(parent)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove(key);
}

/// A value of another JSON type than `v`.
fn other_type(v: &Value) -> Value {
    match v {
        Value::String(_) => json!(7),
        Value::Number(_) => json!("7"),
        Value::Bool(_) => json!("true"),
        Value::Null => json!({ "surprise": true }),
        Value::Array(_) => json!({ "surprise": true }),
        Value::Object(_) => json!(["surprise"]),
    }
}

/// Mutations that survive validation by design: the schema allows the
/// mutated value. Each is (schema, mutation, pattern) with the reason.
const SURVIVORS: &[(&str, &str, &str, &str)] = &[
    // Optional fields: omitted when absent (never null).
    (
        "*",
        "remove",
        "/findings/*/message",
        "optional finding field",
    ),
    (
        "*",
        "remove",
        "/findings/*/expected",
        "optional finding field",
    ),
    (
        "*",
        "remove",
        "/findings/*/observed",
        "optional finding field",
    ),
    (
        "*",
        "remove",
        "/findings/*/affected",
        "optional finding field",
    ),
    (
        "*",
        "remove",
        "/truncation/*/message",
        "optional finding field",
    ),
    (
        "*",
        "remove",
        "/entries/*/path_hex",
        "present only when the name is not UTF-8",
    ),
    (
        "health-evidence-v1.schema.json",
        "remove",
        "/scope",
        "optional, omitted when absent",
    ),
    (
        "health-evidence-v1.schema.json",
        "remove",
        "/finding_codes",
        "optional, omitted when empty",
    ),
    (
        "health-policy-v1.schema.json",
        "remove",
        "/max_age_days",
        "optional: no kind has an age limit",
    ),
    (
        "health-policy-v1.schema.json",
        "remove",
        "/max_age_days/verify",
        "optional kind",
    ),
    (
        "health-policy-v1.schema.json",
        "remove",
        "/max_age_days/restore_test",
        "optional kind",
    ),
    // The evidence log's dimensions are those the run assessed.
    (
        "health-evidence-v1.schema.json",
        "remove",
        "/dimensions/*",
        "any non-empty subset",
    ),
    // A free-form map: any table may be asked for.
    (
        "cli-dump-index.schema.json",
        "remove",
        "/tables/*",
        "tables are selected by --table",
    ),
    (
        "local-heads-v1.schema.json",
        "remove",
        "/heads/*",
        "any set of archives",
    ),
    // Catalog cells hold integers, text, or null.
    (
        "cli-dump-index.schema.json",
        "retype",
        "/tables/*/rows/*/*",
        "a cell may be any SQLite type",
    ),
    // Booleans that may be null when retention is unknown are retyped to a
    // string, which fails; nothing here.
];

fn allowed(schema: &str, kind: &str, pat: &str) -> bool {
    SURVIVORS
        .iter()
        .any(|(s, k, p, _)| (*s == "*" || *s == schema) && *k == kind && *p == pat)
}

/// Picks the instance a targeted mutation is applied to.
type Pick = fn(&Value) -> bool;

fn any(_: &Value) -> bool {
    true
}

fn no_anchor(v: &Value) -> bool {
    v["freshness_anchor"] == "none"
}

fn exits_0(v: &Value) -> bool {
    v["exit_code"] == 0
}

fn durable(v: &Value) -> bool {
    v["durability"] == "durable"
}

fn not_complete(v: &Value) -> bool {
    v["outcome"] != "complete"
}

/// Targeted mutations of particular meaning: each must fail. (schema,
/// pointer, value, why, which instance).
fn targeted() -> Vec<(&'static str, &'static str, Value, &'static str, Pick)> {
    vec![
        (
            "report-v1.schema.json",
            "/schema_version",
            json!(2),
            "another schema version",
            any,
        ),
        (
            "report-v1.schema.json",
            "/dimensions/integrity",
            json!("OK"),
            "not a 20.4 status",
            any,
        ),
        (
            "report-v1.schema.json",
            "/dimensions/integrity",
            json!("pass"),
            "statuses are upper case",
            any,
        ),
        (
            "report-v1.schema.json",
            "/overall_status",
            json!("PRESERVED"),
            "not a 20.4 status",
            any,
        ),
        (
            "report-v1.schema.json",
            "/level",
            json!("full"),
            "not a 20.1 level",
            any,
        ),
        (
            "report-v1.schema.json",
            "/freshness_anchor",
            json!("local_history"),
            "kebab-case",
            any,
        ),
        (
            "report-v1.schema.json",
            "/exit_code",
            json!(5),
            "not a 23.2 exit code",
            any,
        ),
        (
            "report-v1.schema.json",
            "/archive_id",
            json!("ABCDEF"),
            "not 64 lower-case hex digits",
            any,
        ),
        (
            "report-v1.schema.json",
            "/archive_id",
            json!("0".repeat(63)),
            "63 digits",
            any,
        ),
        (
            "report-v1.schema.json",
            "/started_at",
            json!("2026-10-14T00:00:00Z"),
            "not D15",
            any,
        ),
        (
            "report-v1.schema.json",
            "/findings/0/code",
            json!("REPLICA_BELOW_POLICY"),
            "not registered",
            any,
        ),
        (
            "report-v1.schema.json",
            "/findings/0/severity",
            json!("fatal"),
            "not a severity",
            any,
        ),
        (
            "report-v1.schema.json",
            "/findings/0/affected",
            json!({"first": 1}),
            "range without last",
            any,
        ),
        (
            "report-v1.schema.json",
            "/repair_actions",
            json!(["rebuilt"]),
            "verification never repairs",
            any,
        ),
        (
            "report-v1.schema.json",
            "/overall_status",
            json!("PASS"),
            "PASS hides a dimension",
            any,
        ),
        (
            "report-v1.schema.json",
            "/dimensions/freshness",
            json!("PASS"),
            "freshness PASS without an anchor",
            no_anchor,
        ),
        (
            "report-v1.schema.json",
            "/policy/freshness/requested",
            json!(true),
            "freshness requested but not required",
            no_anchor,
        ),
        (
            "report-v1.schema.json",
            "/expected_head",
            json!("0".repeat(64)),
            "an expected head without an anchor",
            no_anchor,
        ),
        (
            "report-v1.schema.json",
            "/dimensions/durability",
            json!("FAIL"),
            "FAIL concealed overall",
            any,
        ),
        (
            "report-v1.schema.json",
            "/operational_error",
            json!(true),
            "operational error exiting 0",
            exits_0,
        ),
        (
            "report-v1.schema.json",
            "/policy/required",
            json!(["integrity", "integrity"]),
            "duplicate",
            any,
        ),
        (
            "report-v1.schema.json",
            "/policy/required/0",
            json!("speed"),
            "not a dimension",
            any,
        ),
        (
            "report-v1.schema.json",
            "/tool/wire_generation",
            json!(-1),
            "negative",
            any,
        ),
        (
            "report-v1.schema.json",
            "/coverage/checked_objects",
            json!(-1),
            "negative",
            any,
        ),
        (
            "error.schema.json",
            "/error/code",
            json!("OOPS"),
            "not registered",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/format",
            json!("mochi-repair-plan-v1"),
            "the v1 format",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/ladder/6/status",
            json!("passed"),
            "not a step status",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/ladder/0/step",
            json!(8),
            "no step 8",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/ladder/1/step",
            json!(1),
            "steps out of order",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/outcome",
            json!("restored"),
            "not an outcome",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/exit_code",
            json!(0),
            "exit disagrees with the outcome",
            not_complete,
        ),
        (
            "repair-plan-v2.schema.json",
            "/snapshots/0/trust",
            json!("trusted"),
            "not a trust label",
            any,
        ),
        (
            "repair-plan-v2.schema.json",
            "/snapshots/0/catalog",
            json!("snapshot-manifest"),
            "snake_case here",
            any,
        ),
        (
            "gc-plan-v1.schema.json",
            "/head/commit_id",
            json!("x"),
            "not hex",
            any,
        ),
        (
            "gc-plan-v1.schema.json",
            "/roots",
            json!([0, 0]),
            "duplicate root",
            any,
        ),
        (
            "health-evidence-v1.schema.json",
            "/schema",
            json!(2),
            "another schema",
            any,
        ),
        (
            "health-evidence-v1.schema.json",
            "/command",
            json!("list"),
            "list records nothing",
            any,
        ),
        (
            "health-evidence-v1.schema.json",
            "/dimensions",
            json!({}),
            "nothing assessed",
            any,
        ),
        (
            "health-evidence-v1.schema.json",
            "/dimensions/integrity",
            json!("GOOD"),
            "not a status",
            any,
        ),
        (
            "health-evidence-v1.schema.json",
            "/finding_codes",
            json!([]),
            "omitted when empty",
            any,
        ),
        (
            "health-policy-v1.schema.json",
            "/schema",
            json!("mochi-health-policy-v2"),
            "another schema",
            any,
        ),
        (
            "health-policy-v1.schema.json",
            "/required",
            json!([]),
            "requires nothing",
            any,
        ),
        (
            "health-policy-v1.schema.json",
            "/max_age_days/verify",
            json!(-1),
            "negative",
            any,
        ),
        (
            "local-heads-v1.schema.json",
            "/version",
            json!(2),
            "another version",
            any,
        ),
        (
            "cli-commit.schema.json",
            "/status",
            json!("PRESERVED"),
            "nothing in 1.0 reports PRESERVED",
            any,
        ),
        (
            "cli-commit.schema.json",
            "/status",
            json!("BACKED_UP"),
            "spec 23.3 wording",
            any,
        ),
        (
            "cli-commit.schema.json",
            "/durability",
            json!("directory_unconfirmed"),
            "unconfirmed without a note",
            durable,
        ),
        (
            "cli-commit.schema.json",
            "/exit_code",
            json!(1),
            "a commit never exits 1",
            any,
        ),
        (
            "cli-maintenance.schema.json",
            "/change",
            json!({"expire": [0]}),
            "change of another command",
            any,
        ),
        (
            "cli-rewrite.schema.json",
            "/source_kept",
            json!(false),
            "the source is always kept",
            any,
        ),
        (
            "cli-repair-apply.schema.json",
            "/source_kept",
            json!(false),
            "the source is always kept",
            any,
        ),
        (
            "cli-repair-apply.schema.json",
            "/outcome",
            json!("nothing_recoverable"),
            "never written",
            any,
        ),
        (
            "cli-search.schema.json",
            "/coverage/full_text",
            json!("built"),
            "not built in 1.0",
            any,
        ),
        (
            "cli-search.schema.json",
            "/coverage/catalog_source",
            json!("snapshot_manifest"),
            "kebab-case here",
            any,
        ),
        (
            "cli-list.schema.json",
            "/entries/0/kind",
            json!("symlink"),
            "not an entry kind",
            any,
        ),
        (
            "cli-rekey-list.schema.json",
            "/envelopes/0/kdf/algorithm",
            json!("scrypt"),
            "Argon2id only",
            any,
        ),
        (
            "cli-rekey-list.schema.json",
            "/envelopes/0/envelope_id",
            json!("0".repeat(64)),
            "16 bytes",
            any,
        ),
        (
            "cli-restore.schema.json",
            "/exit_code",
            json!(3),
            "not a restore exit",
            any,
        ),
        (
            "cli-snapshot-list.schema.json",
            "/commits/0/time",
            json!("yesterday"),
            "not D15",
            any,
        ),
        (
            "cli-plan-saved.schema.json",
            "/command",
            json!("compact"),
            "not a planning command",
            any,
        ),
        (
            "cli-commit.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-commit.schema.json",
            "/schema",
            json!("mochi-cli-commit-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-maintenance.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-maintenance.schema.json",
            "/schema",
            json!("mochi-cli-maintenance-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-list.schema.json",
            "/schema",
            json!("mochi-cli-search-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-list.schema.json",
            "/schema",
            json!("mochi-cli-list-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-snapshot-list.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-snapshot-list.schema.json",
            "/schema",
            json!("mochi-cli-snapshot-list-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-search.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-search.schema.json",
            "/schema",
            json!("mochi-cli-search-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-dump-index.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-dump-index.schema.json",
            "/schema",
            json!("mochi-cli-dump-index-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-restore.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-restore.schema.json",
            "/schema",
            json!("mochi-cli-restore-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-rewrite.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-rewrite.schema.json",
            "/schema",
            json!("mochi-cli-rewrite-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-repair-apply.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-repair-apply.schema.json",
            "/schema",
            json!("mochi-cli-repair-apply-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-rekey-list.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-rekey-list.schema.json",
            "/schema",
            json!("mochi-cli-rekey-list-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-plan-saved.schema.json",
            "/schema",
            json!("mochi-cli-list-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-plan-saved.schema.json",
            "/schema",
            json!("mochi-cli-plan-saved-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/schema",
            json!("mochi-cli-rewrite-v1"),
            "another document's tag",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/schema",
            json!("mochi-cli-migrate-v2"),
            "another version of the same document",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/source_draft",
            json!(1),
            "only the last draft is read",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/new_ratified",
            json!(false),
            "a migration writes a ratified archive",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/source_kept",
            json!(false),
            "the source is never removed",
            any,
        ),
        (
            "cli-find.schema.json",
            "/coverage/content_searched",
            json!(true),
            "find searches names only",
            any,
        ),
        (
            "cli-find.schema.json",
            "/coverage/folders_unreadable",
            json!(1),
            "complete means every folder was read",
            |v| v["coverage"]["complete"] == true,
        ),
        (
            "cli-find.schema.json",
            "/coverage/complete",
            json!(true),
            "an archive left out makes the search incomplete",
            |v| v["coverage"]["archives_skipped"] != 0,
        ),
        (
            "cli-find.schema.json",
            "/query/match",
            json!("regex"),
            "substring or glob only",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/comparison/retention_equal",
            json!(false),
            "a retention difference is a failure, not a report",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/source_verification/exit_code",
            json!(1),
            "a source that does not verify is not converted",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/new_verification/exit_code",
            json!(1),
            "a new archive that does not verify is not published",
            any,
        ),
        (
            "cli-migrate.schema.json",
            "/resealed",
            json!(true),
            "resealed exactly when the profile is Encrypted",
            |v| v["profile"]["encrypted"] == false,
        ),
        (
            "cli-migrate.schema.json",
            "/exit_code",
            json!(2),
            "unconfirmed without a note",
            |v| v["durability_unconfirmed"].is_null(),
        ),
    ]
}

#[test]
fn mutated_outputs_fail() {
    let corpus = corpus();
    let (mut tried, mut killed) = (0usize, 0usize);
    let mut survivors: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut used: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut try_one = |schema: &str, kind: &str, at: &str, m: &Value| {
        tried += 1;
        if complaints(schema, m).is_empty() {
            let pat = pattern(schema, at);
            if allowed(schema, kind, &pat) {
                used.insert((schema.to_owned(), kind.to_owned(), pat));
            } else {
                survivors.insert((schema.to_owned(), kind.to_owned(), pat));
            }
        } else {
            killed += 1;
        }
    };
    for (schema, v) in &corpus {
        let mut all = Vec::new();
        nodes(v, String::new(), &mut all);
        for (at, node) in &all {
            match node {
                Value::Object(m) => {
                    let mut x = v.clone();
                    x.pointer_mut(at)
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert("surprise".into(), json!(1));
                    try_one(schema, "add-key", at, &x);
                    for k in m.keys() {
                        let mut x = v.clone();
                        remove(&mut x, &format!("{at}/{k}"));
                        try_one(schema, "remove", &format!("{at}/{k}"), &x);
                    }
                }
                Value::Array(_) => {
                    let mut x = v.clone();
                    x.pointer_mut(at)
                        .unwrap()
                        .as_array_mut()
                        .unwrap()
                        .push(json!({ "surprise": true }));
                    try_one(schema, "push", at, &x);
                }
                _ => {}
            }
            if !at.is_empty() {
                let mut x = v.clone();
                set(&mut x, at, other_type(node));
                try_one(schema, "retype", at, &x);
            }
        }
    }
    // Targeted mutations, on the first instance that has the pointer.
    let mut targeted_killed = 0;
    for (schema, pointer, value, why, pick) in targeted() {
        let Some(base) = corpus
            .iter()
            .find(|(s, v)| *s == schema && v.pointer(pointer).is_some() && pick(v))
        else {
            // Only where the platform gives no such instance: a durable
            // directory entry is unconfirmed on Windows (G6 policy B).
            assert!(
                cfg!(windows) && why == "unconfirmed without a note",
                "no {schema} instance for {pointer}"
            );
            continue;
        };
        let mut x = base.1.clone();
        set(&mut x, pointer, value.clone());
        tried += 1;
        let c = complaints(schema, &x);
        assert!(
            !c.is_empty(),
            "{schema} {pointer} = {value}: survived ({why})"
        );
        killed += 1;
        targeted_killed += 1;
    }
    println!(
        "R7 mutation test: {tried} mutations over {} real outputs, {killed} killed \
         ({targeted_killed} targeted), {} allowed survivors",
        corpus.len(),
        tried - killed
    );
    assert!(
        survivors.is_empty(),
        "mutations the schemas accept but should not: {survivors:#?}"
    );
    let stale: Vec<_> = SURVIVORS
        .iter()
        .filter(|(s, k, p, _)| {
            !used
                .iter()
                .any(|(us, uk, up)| (*s == "*" || s == us) && k == uk && p == up)
        })
        .collect();
    assert!(
        stale.is_empty(),
        "allowances no mutation needed: {stale:#?}"
    );
}

// ---- spec §20.6 ---------------------------------------------------------------------

/// The abridged report §20.6 shows: `mochi verify --json` of the R8 case,
/// in `Report`'s field order, with the two wall-clock times and the tool
/// version fixed (so a version bump does not make the spec stale).
fn spec_example() -> String {
    let a = s(&r8().join(format!("{SPEC_EXAMPLE_CASE}.mochi")));
    let (_, v) = ok(
        "report-v1.schema.json",
        &["--no-local-history", "verify", &a],
    );
    let mut report: Report = serde_json::from_value(v).unwrap();
    report.started_at = Some("2026-10-14T09:00:00.000000000Z".into());
    report.completed_at = Some("2026-10-14T09:00:00.250000000Z".into());
    assert_eq!(report.tool.version, mochi_core::TOOL_VERSION);
    report.tool.version = "1.0.0".into();
    let text = serde_json::to_string_pretty(&report).unwrap();
    valid(
        "report-v1.schema.json",
        &serde_json::from_str(&text).unwrap(),
        "the spec example",
    );
    text
}

#[test]
fn spec_20_6_example_is_generated_from_r8() {
    let spec = repo_file("docs/spec.md");
    let section = spec
        .split("### 20.6 Example report")
        .nth(1)
        .and_then(|s| s.split("\n## 21.").next())
        .expect("spec §20.6");
    assert!(
        section.contains(&format!("fixtures/golden/r8/{SPEC_EXAMPLE_CASE}.mochi")),
        "§20.6 names its R8 case"
    );
    let block = section
        .split("```json\n")
        .nth(1)
        .and_then(|s| s.split("\n```").next())
        .expect("a json block in §20.6");
    let generated = spec_example();
    assert_eq!(
        block, generated,
        "spec §20.6 is stale; replace its JSON block with:\n{generated}"
    );
}
