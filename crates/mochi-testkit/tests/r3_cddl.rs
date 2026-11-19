//! R3 mechanical CDDL check (ratification item R3, plan W13, K14 d).
//!
//! `docs/schemas/*.cddl` are the normative wire schemas of every CBOR record.
//! This test makes sure they describe the bytes the code actually writes and
//! reads, using the `cddl` crate (MIT; a dev-dependency of this crate only) as
//! an independent validator:
//!
//! 1. every schema file parses and is *standalone*: it defines every rule it
//!    uses and never defers to another file;
//! 2. every valid CBOR record in `fixtures/golden/` (the `.bin` vectors and the
//!    records inside the whole-archive fixtures) validates against the schema
//!    its key 0 selects, and the legacy v0 vectors validate against the legacy
//!    schemas and against no current one;
//! 3. every reject vector is classified, and the ones whose defect is
//!    schema-level fail validation, while the ones whose defect lies outside
//!    what CDDL can say still validate (so the schema is not stricter than the
//!    code);
//! 4. a differential run over mutants of the valid records: where the code
//!    accepts a mutant, the CDDL accepts it; where the mutation is structural
//!    (a required key removed, an unknown key added, a value of another CBOR
//!    type) and the code rejects it, the CDDL rejects it too.
//!
//! Rules CDDL cannot express are listed in R3 §7 with a hand check in the
//! code, and are exactly the "semantic" classifications below. A separate
//! implementation (R9) must check them itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use mochi_core::commit::CommitRecord;
use mochi_core::descriptor::Descriptor;
use mochi_core::keys::KeyEnvelope;
use mochi_core::manifest::Manifest;
use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::digest::commit_id;
use mochi_format::frame::walk_frame;
use mochi_format::registry::{self, FrameKind};
use mochi_format::repr::CanonicalCommitBody;
use mochi_format::seal::{open_payload, SealContext, SealTarget};
use mochi_format::secret::Passphrase;
use mochi_format::Limits;

// ---- the schema files ---------------------------------------------------------------

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every file in `docs/schemas/`, with the record it describes.
const SCHEMAS: &[&str] = &[
    "archive-descriptor-v0",
    "commit-record-v0",
    "commit-record-v1",
    "commit-record-v2",
    "commit-record-v4",
    "key-envelope-v0",
    "recovery-manifest-v0",
    "recovery-manifest-v1",
    "recovery-manifest-v2",
    "recovery-manifest-v3",
    "tail-quarantine-v0",
];

/// The legacy (refused) schemas: kept for reference, never selected for a
/// current record.
const LEGACY: &[&str] = &["commit-record-v0", "recovery-manifest-v0"];

fn schema_text(name: &str) -> String {
    let path = root().join("docs/schemas").join(format!("{name}.cddl"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

thread_local! {
    /// Each schema is parsed once per test thread (parsing costs more than
    /// validating one record).
    static PARSED: std::cell::RefCell<BTreeMap<String, &'static cddl::ast::CDDL<'static>>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

fn parsed(name: &str) -> &'static cddl::ast::CDDL<'static> {
    PARSED.with(|cache| {
        *cache
            .borrow_mut()
            .entry(name.to_owned())
            .or_insert_with(|| {
                let text: &'static str = Box::leak(schema_text(name).into_boxed_str());
                Box::leak(Box::new(
                    cddl::cddl_from_str(text, true).unwrap_or_else(|e| panic!("{name}: {e}")),
                ))
            })
    })
}

/// Validate `cbor` against the first rule of schema file `schema`.
fn validate(schema: &str, cbor: &[u8]) -> Result<(), String> {
    let value = cddl::validator::cbor_value::decode_cbor(cbor).map_err(|e| format!("{e:?}"))?;
    use cddl::validator::Validator;
    cddl::validator::cbor::CBORValidator::new(parsed(schema), value, None)
        .validate()
        .map_err(|e| format!("{e}"))
}

// ---- the record kinds ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rec {
    Descriptor,
    Commit,
    Manifest,
    KeyEnvelope,
}

impl Rec {
    fn of_magic(magic: u32) -> Option<Rec> {
        match FrameKind::from_magic(magic)? {
            FrameKind::ArchiveDescriptor => Some(Rec::Descriptor),
            FrameKind::CommitRecord => Some(Rec::Commit),
            FrameKind::RecoveryManifest => Some(Rec::Manifest),
            FrameKind::KeyEnvelope => Some(Rec::KeyEnvelope),
            _ => None,
        }
    }

    /// The schema file for this record at key-0 value `version`, if any.
    fn schema(self, version: u64) -> Option<&'static str> {
        Some(match (self, version) {
            (Rec::Descriptor, 0) => "archive-descriptor-v0",
            (Rec::Commit, 0) => "commit-record-v0",
            (Rec::Commit, 1) => "commit-record-v1",
            (Rec::Commit, 2) => "commit-record-v2",
            (Rec::Commit, 4) => "commit-record-v4",
            (Rec::Manifest, 0) => "recovery-manifest-v0",
            (Rec::Manifest, 1) => "recovery-manifest-v1",
            (Rec::Manifest, 2) => "recovery-manifest-v2",
            (Rec::Manifest, 3) => "recovery-manifest-v3",
            (Rec::KeyEnvelope, 0) => "key-envelope-v0",
            _ => return None,
        })
    }

    /// The code's verdict on a payload: whether the production decoder
    /// accepts it.
    fn code_accepts(self, payload: &[u8]) -> bool {
        let (limits, cl) = (Limits::default(), CborLimits::default());
        match self {
            Rec::Descriptor => Descriptor::decode(payload, &limits, &cl).is_ok(),
            Rec::Commit => CommitRecord::decode(payload, &limits, &cl).is_ok(),
            Rec::Manifest => Manifest::decode(payload, &limits, &cl).is_ok(),
            // An envelope's cost parameters are checked when the passphrase is
            // applied (`unwrap`), before any Argon2 memory is allocated; a
            // record whose parameters fail there is as refused as one that
            // fails to decode. A wrong passphrase is not a refusal.
            Rec::KeyEnvelope => KeyEnvelope::decode(payload, &limits, &cl).is_ok_and(|e| {
                e.unwrap(&Passphrase::new(GOLDEN_PASSPHRASE).unwrap(), &limits)
                    .is_ok()
            }),
        }
    }
}

/// The key-0 value of a payload, if it is a CBOR map that starts with one.
fn schema_version(payload: &[u8]) -> Option<u64> {
    match cbor::decode(payload, &CborLimits::default()).ok()? {
        Value::Map(m) => match m.first() {
            Some((0, Value::Uint(v))) => Some(*v),
            _ => None,
        },
        _ => None,
    }
}

// ---- the corpus ---------------------------------------------------------------------

struct Record {
    origin: String,
    kind: Rec,
    payload: Vec<u8>,
}

/// The payload of a bare skippable frame, whatever its length field says is
/// there (a reject vector may carry a trailing frame or be cut short).
fn bare_payload(bytes: &[u8]) -> Option<(u32, Vec<u8>)> {
    let magic = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?);
    if !(registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX).contains(&magic) {
        return None;
    }
    let len = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    Some((magic, bytes.get(8..8 + len)?.to_vec()))
}

/// Every skippable frame of a whole archive, in file order.
fn archive_frames(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let limits = Limits::default();
    let (mut off, mut out) = (0u64, Vec::new());
    while off < bytes.len() as u64 {
        let Ok(span) = walk_frame(bytes, off, &limits) else {
            break;
        };
        if span.kind.is_skippable() {
            let at = off as usize;
            if let Some(f) = bare_payload(&bytes[at..at + span.len as usize]) {
                out.push(f);
            }
        }
        off = span.end();
    }
    out
}

fn golden(dir: &str) -> PathBuf {
    root().join("fixtures/golden").join(dir)
}

fn files(dir: &str, ext: &str) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(golden(dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == ext))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

/// Valid single-record `.bin` vectors: `(dir, file)` pairs.
fn valid_bins() -> Vec<(&'static str, String, Rec, Vec<u8>)> {
    let mut out = Vec::new();
    for dir in ["b2", "c4", "c5", "c11", "d29"] {
        for (name, bytes) in files(dir, "bin") {
            if !name.starts_with("valid-") {
                continue;
            }
            let Some((magic, payload)) = bare_payload(&bytes) else {
                continue;
            };
            if let Some(kind) = Rec::of_magic(magic) {
                out.push((dir, name, kind, payload));
            }
        }
    }
    out
}

/// Every CBOR record inside the whole-archive fixtures that are valid
/// (`valid-*.mochi`). Sealed (Encrypted-profile) manifests are ciphertext
/// here; their plaintext schema is checked through [`encrypted_manifests`].
fn archive_records() -> Vec<Record> {
    let mut out = Vec::new();
    for dir in ["c5", "c10", "c11", "c12"] {
        for (name, bytes) in files(dir, "mochi") {
            if !name.starts_with("valid-") {
                continue;
            }
            for (i, (magic, payload)) in archive_frames(&bytes).into_iter().enumerate() {
                if let Some(kind) = Rec::of_magic(magic) {
                    out.push(Record {
                        origin: format!("{dir}/{name}#{i}"),
                        kind,
                        payload,
                    });
                }
            }
        }
    }
    out
}

fn corpus() -> Vec<Record> {
    let mut out: Vec<Record> = valid_bins()
        .into_iter()
        .map(|(dir, name, kind, payload)| Record {
            origin: format!("{dir}/{name}"),
            kind,
            payload,
        })
        .collect();
    out.extend(archive_records());
    out
}

// ---- 2. the valid corpus validates --------------------------------------------------

#[test]
fn every_valid_golden_record_validates_against_its_schema() {
    let mut per_schema: BTreeMap<&str, usize> = BTreeMap::new();
    for r in corpus() {
        let v = schema_version(&r.payload).unwrap_or_else(|| panic!("{}: no key 0", r.origin));
        let schema = r
            .kind
            .schema(v)
            .unwrap_or_else(|| panic!("{}: no schema for {:?} v{v}", r.origin, r.kind));
        assert!(
            r.kind.code_accepts(&r.payload),
            "{}: the code rejects a record the corpus calls valid",
            r.origin
        );
        if let Err(e) = validate(schema, &r.payload) {
            panic!("{} does not validate against {schema}: {e}", r.origin);
        }
        *per_schema.entry(schema).or_default() += 1;
    }
    println!("validated per schema: {per_schema:?}");
    // The corpus must exercise every current schema except the encrypted
    // manifest, which is sealed in every archive (covered below).
    for s in [
        "archive-descriptor-v0",
        "commit-record-v1",
        "commit-record-v2",
        "commit-record-v4",
        "key-envelope-v0",
        "recovery-manifest-v1",
        "recovery-manifest-v2",
    ] {
        assert!(per_schema.contains_key(s), "no valid golden record for {s}");
    }
}

// ---- 1. the schema files are standalone ---------------------------------------------

/// The CDDL standard prelude names the schemas use.
const PRELUDE: &[&str] = &[
    "uint", "int", "bstr", "tstr", "bool", "null", "true", "false",
];

/// `(defined, used)` rule names of a schema. A definition is a name at the
/// start of a line followed by `=`; everything else that looks like a name is
/// a use. Comments, numbers, ranges, and control operators (`.size`) are not
/// names.
fn rule_names(text: &str) -> (Vec<String>, BTreeSet<String>) {
    let (mut defined, mut used) = (Vec::new(), BTreeSet::new());
    for line in text.lines() {
        let code = line.split(';').next().unwrap_or("");
        let chars: Vec<char> = code.chars().collect();
        let mut i = 0;
        let mut first = true;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                // A text literal (a `.regexp` pattern): not rule names.
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
            } else if c == '.' {
                // `..` range or a control operator `.size`: skip the word.
                i += 1;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
            } else if c.is_ascii_digit() {
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            } else if c.is_ascii_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '-' || chars[i] == '_')
                {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let at_line_start = first && start == 0;
                let rest: String = chars[i..].iter().collect();
                if at_line_start && rest.trim_start().starts_with('=') {
                    defined.push(name);
                } else {
                    used.insert(name);
                }
            } else {
                i += 1;
            }
            first = false;
        }
    }
    (defined, used)
}

#[test]
fn every_schema_file_is_complete_and_standalone() {
    let mut problems = Vec::new();
    for name in SCHEMAS {
        let text = schema_text(name);
        // The file parses with the independent CDDL parser.
        if let Err(e) = cddl::cddl_from_str(&text, true) {
            problems.push(format!("{name}: does not parse: {e}"));
            continue;
        }

        let (defined, used) = rule_names(&text);
        let defined_set: BTreeSet<&String> = defined.iter().collect();
        if defined_set.len() != defined.len() {
            problems.push(format!("{name}: a rule is defined twice"));
        }
        for u in &used {
            if !PRELUDE.contains(&u.as_str()) && !defined.contains(u) {
                problems.push(format!(
                    "{name}: uses `{u}`, which no rule in this file defines"
                ));
            }
        }
        for d in defined.iter().skip(1) {
            if !used.contains(d) {
                problems.push(format!("{name}: defines `{d}`, which nothing uses"));
            }
        }
        // K14 (c): no "restated from": a file draws everything it needs.
        for banned in ["restated", "exactly as in"] {
            if text.contains(banned) {
                problems.push(format!("{name}: defers to another file ({banned:?})"));
            }
        }
        if LEGACY.contains(name) != text.contains("LEGACY (refused)") {
            problems.push(format!(
                "{name}: legacy schemas, and only they, say LEGACY (refused)"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    // CDDL files only: R7's JSON Schemas live in `docs/schemas/json/`.
    let on_disk: BTreeSet<String> = std::fs::read_dir(root().join("docs/schemas"))
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().to_string_lossy().into_owned();
            name.strip_suffix(".cddl").map(str::to_owned)
        })
        .collect();
    let listed: BTreeSet<String> = SCHEMAS.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        on_disk, listed,
        "every file in docs/schemas is checked here"
    );
}

// ---- 2b. the Encrypted profile's manifests (sealed in every archive) ----------------

const GOLDEN_PASSPHRASE: &str = "golden passphrase";

/// Every skippable frame of an archive with its offset, in file order.
fn frames_at(bytes: &[u8]) -> Vec<(u64, u32, Vec<u8>)> {
    let limits = Limits::default();
    let (mut off, mut out) = (0u64, Vec::new());
    while off < bytes.len() as u64 {
        let Ok(span) = walk_frame(bytes, off, &limits) else {
            break;
        };
        if span.kind.is_skippable() {
            let at = off as usize;
            if let Some((magic, payload)) = bare_payload(&bytes[at..at + span.len as usize]) {
                out.push((off, magic, payload));
            }
        }
        off = span.end();
    }
    out
}

/// The plaintext of every manifest in `c11/valid-archive-encrypted.mochi`,
/// opened with the golden passphrase: the bytes a reader holding the key sees
/// and the only place schema 3 exists on disk.
fn encrypted_manifests() -> Vec<Record> {
    let bytes = std::fs::read(golden("c11").join("valid-archive-encrypted.mochi")).unwrap();
    let frames = frames_at(&bytes);
    let limits = Limits::default();
    let cl = CborLimits::default();
    let pass = Passphrase::new(GOLDEN_PASSPHRASE).unwrap();
    let (mut key, mut key_id) = (None, None);
    for (_, magic, payload) in &frames {
        if *magic == registry::KEY_ENVELOPE {
            let env = KeyEnvelope::decode(payload, &limits, &cl).unwrap();
            if let Some(dek) = env.unwrap(&pass, &limits).unwrap() {
                key = Some(dek);
                key_id = Some(env.key_id);
                break;
            }
        }
    }
    let (key, key_id) = (
        key.expect("the golden passphrase opens an envelope"),
        key_id.unwrap(),
    );
    let mut out = Vec::new();
    for (_, magic, payload) in &frames {
        if *magic != registry::COMMIT_RECORD {
            continue;
        }
        let (commit, _) = CommitRecord::decode(payload, &limits, &cl).unwrap();
        let ctx = SealContext {
            key: &key,
            key_id,
            archive_id: *commit.archive_id.as_bytes(),
        };
        let mut refs = vec![(
            commit.delta_manifest,
            SealTarget::DeltaManifest {
                sequence: commit.seq,
                transaction_id: commit.transaction_id,
            },
            "delta",
        )];
        if let mochi_core::commit::Metadata::Checkpoint { snapshot, .. } = commit.metadata {
            refs.push((
                snapshot,
                SealTarget::SnapshotManifest {
                    sequence: commit.seq,
                    transaction_id: commit.transaction_id,
                },
                "snapshot",
            ));
        }
        for (r, target, what) in refs {
            let at = r.offset as usize;
            let frame = &bytes[at..at + r.stored_len as usize];
            let (_, sealed) = bare_payload(frame).unwrap();
            out.push(Record {
                origin: format!(
                    "c11/valid-archive-encrypted.mochi commit {} {what}",
                    commit.seq
                ),
                kind: Rec::Manifest,
                payload: open_payload(&ctx, &target, &sealed).unwrap(),
            });
        }
    }
    out
}

/// Schema-3 manifests the golden archive does not contain: a rewritten
/// archive's delta(0) with each provenance reason.
fn encrypted_provenance_variants() -> Vec<Record> {
    use mochi_core::manifest::{Provenance, RewriteReason};
    let root = encrypted_manifests()
        .into_iter()
        .find(|r| r.origin.ends_with("commit 0 delta"))
        .unwrap();
    let base = Manifest::decode(&root.payload, &Limits::default(), &CborLimits::default()).unwrap();
    [RewriteReason::Collection, RewriteReason::Reencryption]
        .into_iter()
        .map(|reason| {
            let mut m = base.clone();
            m.provenance = Some(Provenance {
                source_archive_id: mochi_core::object::ArchiveId::from_bytes([0x5A; 32]),
                commits: vec![(0, mochi_format::digest::CommitId::from_bytes([0xC0; 32]))],
                collected: vec![],
                reason,
            });
            Record {
                origin: format!("generated: encrypted delta(0) with provenance {reason:?}"),
                kind: Rec::Manifest,
                payload: m.encode().unwrap(),
            }
        })
        .collect()
}

#[test]
fn the_encrypted_profiles_manifests_validate_against_schema_3() {
    let mut all = encrypted_manifests();
    all.extend(encrypted_provenance_variants());
    let (mut deltas, mut snapshots, mut with_ops) = (0, 0, 0);
    for r in &all {
        assert_eq!(schema_version(&r.payload), Some(3), "{}", r.origin);
        assert!(
            r.kind.code_accepts(&r.payload),
            "{}: the code rejects it",
            r.origin
        );
        if let Err(e) = validate("recovery-manifest-v3", &r.payload) {
            panic!(
                "{} does not validate against recovery-manifest-v3: {e}",
                r.origin
            );
        }
        // The same record must NOT validate under the other schemas.
        for other in ["recovery-manifest-v1", "recovery-manifest-v2"] {
            assert!(
                validate(other, &r.payload).is_err(),
                "{}: {other}",
                r.origin
            );
        }
        match r.origin.rsplit(' ').next().unwrap() {
            "snapshot" => snapshots += 1,
            _ => deltas += 1,
        }
        if let Value::Map(m) = cbor::decode(&r.payload, &CborLimits::default()).unwrap() {
            if let Some((_, Value::Array(ops))) = m.iter().find(|(k, _)| *k == 13) {
                if !ops.is_empty() && matches!(ops[0], Value::Array(_)) {
                    with_ops += 1;
                }
            }
        }
    }
    assert!(
        deltas >= 3 && snapshots >= 1,
        "{deltas} deltas, {snapshots} snapshots"
    );
    assert!(
        with_ops >= 2,
        "key operations (an add and a rewrap) are exercised"
    );
}

/// Whether the production code decodes `payload` as a key envelope and opens
/// it with the golden passphrase under the default reader limits.
fn envelope_opens(payload: &[u8]) -> bool {
    let limits = Limits::default();
    KeyEnvelope::decode(payload, &limits, &CborLimits::default())
        .ok()
        .and_then(|e| {
            e.unwrap(&Passphrase::new(GOLDEN_PASSPHRASE).unwrap(), &limits)
                .ok()
        })
        .is_some_and(|k| k.is_some())
}

// ---- 3. the reject vectors ----------------------------------------------------------

/// What kind of defect a reject vector exercises, and so what the schema must
/// say about its payload.
#[derive(Debug, Clone, Copy)]
enum Defect {
    /// A schema-level defect: CDDL rejects the payload.
    Schema,
    /// The payload is as the schema describes and the code still refuses it,
    /// for a reason CDDL cannot state. These are R3 section 7's hand checks
    /// ("R9 must check").
    Semantic(&'static str),
    /// The payload is valid; the frame around it is wrong. CDDL describes
    /// payloads, so it accepts the payload.
    Frame(&'static str),
    /// The payload is not canonical CBOR. The codec's rule (Annex B.1 D2), not
    /// a schema's: the validator is handed canonical bytes only.
    Codec(&'static str),
    /// A schema version this build does not know: no schema applies.
    Version,
    /// The pre-batch draft: the legacy schema accepts it, no current one does.
    Legacy,
    /// Not a complete frame, so no payload to validate.
    NoPayload(&'static str),
}

use Defect::*;

/// Every CBOR-record reject vector, with the record it was meant to be.
/// (`b2/reject-envelope-*` are binary envelopes, R2, and `c11/reject-sealed-*`
/// are sealed objects, R5: neither is CBOR.)
const REJECTS: &[(&str, Rec, Defect)] = &[
    (
        "b2/reject-descriptor-constraint-type.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-constraint-unknown-key.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-declared-limits-missing-key.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-draft-1.bin",
        Rec::Descriptor,
        Semantic("a draft identifier is refused by a 1.0 build (D24); 1 is the pre-freeze draft"),
    ),
    (
        "b2/reject-descriptor-draft-2.bin",
        Rec::Descriptor,
        Semantic("a draft identifier is refused by a 1.0 build (D24); only `migrate` reads 2"),
    ),
    (
        "b2/reject-descriptor-draft-3.bin",
        Rec::Descriptor,
        Semantic("a draft identifier is refused by a 1.0 build (D24)"),
    ),
    (
        "b2/reject-descriptor-draft-type.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-features-not-increasing.bin",
        Rec::Descriptor,
        Semantic("strictly increasing features (D11)"),
    ),
    (
        "b2/reject-descriptor-generation.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-missing-key.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-noncanonical-cbor.bin",
        Rec::Descriptor,
        Codec("a key in non-shortest form"),
    ),
    (
        "b2/reject-descriptor-payload-not-consumed.bin",
        Rec::Descriptor,
        Codec("bytes after the map"),
    ),
    (
        "b2/reject-descriptor-schema-version.bin",
        Rec::Descriptor,
        Version,
    ),
    (
        "b2/reject-descriptor-short-archive-id.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-trailing-frame.bin",
        Rec::Descriptor,
        Frame("a second frame after the descriptor"),
    ),
    (
        "b2/reject-descriptor-unknown-feature.bin",
        Rec::Descriptor,
        Semantic("an unknown required feature is refused, not malformed"),
    ),
    (
        "b2/reject-descriptor-unknown-key.bin",
        Rec::Descriptor,
        Schema,
    ),
    (
        "b2/reject-descriptor-wrong-frame-kind.bin",
        Rec::Descriptor,
        Frame("a descriptor payload in a manifest frame"),
    ),
    (
        "c4/reject-manifest-delta-with-entries.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-features-not-increasing.bin",
        Rec::Manifest,
        Semantic("strictly increasing features (D11)"),
    ),
    ("c4/reject-manifest-legacy-v0.bin", Rec::Manifest, Legacy),
    ("c4/reject-manifest-missing-key.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-missing-transaction-id.bin",
        Rec::Manifest,
        Schema,
    ),
    ("c4/reject-manifest-mode-bits.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-noncanonical-cbor.bin",
        Rec::Manifest,
        Codec("a key in non-shortest form"),
    ),
    ("c4/reject-manifest-orphan-delta.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-parent-seq-gap.bin",
        Rec::Manifest,
        Semantic("parent sequence = sequence - 1"),
    ),
    (
        "c4/reject-manifest-payload-not-consumed.bin",
        Rec::Manifest,
        Codec("bytes after the map"),
    ),
    (
        "c4/reject-manifest-provenance-collected-head.bin",
        Rec::Manifest,
        Semantic("a collected snapshot is before the source head"),
    ),
    (
        "c4/reject-manifest-provenance-collected-kept.bin",
        Rec::Manifest,
        Semantic("a collected snapshot is not also kept"),
    ),
    (
        "c4/reject-manifest-provenance-empty.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-provenance-not-root.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-provenance-unsorted.bin",
        Rec::Manifest,
        Semantic("provenance sequences strictly increase"),
    ),
    (
        "c4/reject-manifest-required-feature.bin",
        Rec::Manifest,
        Semantic("an unknown required feature is refused, not malformed"),
    ),
    (
        "c4/reject-manifest-retention-empty-label.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-retention-expire-self.bin",
        Rec::Manifest,
        Semantic("an expiry names an earlier sequence"),
    ),
    (
        "c4/reject-manifest-retention-expired-duplicate.bin",
        Rec::Manifest,
        Semantic("expired sequences strictly increase"),
    ),
    (
        "c4/reject-manifest-retention-expired-not-earlier.bin",
        Rec::Manifest,
        Semantic("an expired sequence is before this commit"),
    ),
    (
        "c4/reject-manifest-retention-hold-future.bin",
        Rec::Manifest,
        Semantic("a hold names a sequence up to this commit"),
    ),
    (
        "c4/reject-manifest-retention-holds-unsorted.bin",
        Rec::Manifest,
        Semantic("holds strictly increase by label"),
    ),
    (
        "c4/reject-manifest-retention-ops-in-snapshot.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-retention-schema-2-empty.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-retention-schema-2-missing-key.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-retention-state-in-delta.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-retention-unknown-op.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-root-delta-with-parent.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-schema-version.bin",
        Rec::Manifest,
        Version,
    ),
    (
        "c4/reject-manifest-short-transaction-id.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-snapshot-not-self-contained.bin",
        Rec::Manifest,
        Semantic("a snapshot contains everything it references"),
    ),
    (
        "c4/reject-manifest-snapshot-with-ops.bin",
        Rec::Manifest,
        Schema,
    ),
    (
        "c4/reject-manifest-snapshot-with-parent.bin",
        Rec::Manifest,
        Schema,
    ),
    ("c4/reject-manifest-symlink-kind.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-trailing-frame.bin",
        Rec::Manifest,
        Frame("a second frame after the manifest"),
    ),
    (
        "c4/reject-manifest-traversal-path.bin",
        Rec::Manifest,
        Semantic("stored path rules (spec 10.4)"),
    ),
    ("c4/reject-manifest-unknown-key.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-unsorted-versions.bin",
        Rec::Manifest,
        Semantic("arrays 5, 6, 8 sorted by first field"),
    ),
    ("c4/reject-manifest-windows-bits.bin", Rec::Manifest, Schema),
    (
        "c4/reject-manifest-wrong-frame-kind.bin",
        Rec::Manifest,
        Frame("a manifest payload in a catalog-image frame"),
    ),
    (
        "c5/reject-commit-base-not-before.bin",
        Rec::Commit,
        Semantic("the base sequence is below this sequence"),
    ),
    (
        "c5/reject-commit-checkpoint-missing-snapshot.bin",
        Rec::Commit,
        Schema,
    ),
    (
        "c5/reject-commit-delta-with-snapshot.bin",
        Rec::Commit,
        Schema,
    ),
    (
        "c5/reject-commit-descriptor-offset.bin",
        Rec::Commit,
        Semantic("the descriptor is at offset 0"),
    ),
    (
        "c5/reject-commit-features-not-increasing.bin",
        Rec::Commit,
        Semantic("strictly increasing features (D11)"),
    ),
    (
        "c5/reject-commit-id-mismatch.bin",
        Rec::Commit,
        Semantic("key 9 equals the recomputed commit ID"),
    ),
    ("c5/reject-commit-legacy-v0.bin", Rec::Commit, Legacy),
    ("c5/reject-commit-metadata-form.bin", Rec::Commit, Schema),
    (
        "c5/reject-commit-missing-descriptor.bin",
        Rec::Commit,
        Schema,
    ),
    ("c5/reject-commit-missing-key.bin", Rec::Commit, Schema),
    (
        "c5/reject-commit-noncanonical-cbor.bin",
        Rec::Commit,
        Codec("a key in non-shortest form"),
    ),
    ("c5/reject-commit-orphan.bin", Rec::Commit, Schema),
    (
        "c5/reject-commit-parent-seq-gap.bin",
        Rec::Commit,
        Semantic("parent sequence = sequence - 1"),
    ),
    (
        "c5/reject-commit-payload-not-consumed.bin",
        Rec::Commit,
        Codec("bytes after the map"),
    ),
    (
        "c5/reject-commit-required-feature.bin",
        Rec::Commit,
        Semantic("an unknown required feature is refused, not malformed"),
    ),
    ("c5/reject-commit-root-delta.bin", Rec::Commit, Schema),
    ("c5/reject-commit-root-with-parent.bin", Rec::Commit, Schema),
    ("c5/reject-commit-schema-version.bin", Rec::Commit, Version),
    // D29 (Annex B.2.19): the comment rules. An empty comment is outside
    // `tstr .size (1..16384)`; a carriage return is a rule CDDL cannot state.
    (
        "d29/reject-commit-comment-carriage-return.bin",
        Rec::Commit,
        Semantic("a comment has no control character but tab and line feed"),
    ),
    ("d29/reject-commit-comment-empty.bin", Rec::Commit, Schema),
    (
        "c5/reject-commit-short-transaction-id.bin",
        Rec::Commit,
        Schema,
    ),
    (
        "c5/reject-commit-trailing-frame.bin",
        Rec::Commit,
        Frame("a second frame after the commit"),
    ),
    ("c5/reject-commit-unknown-key.bin", Rec::Commit, Schema),
    (
        "c5/reject-commit-wrong-frame-kind.bin",
        Rec::Commit,
        Frame("a commit payload in a manifest frame"),
    ),
    (
        "c11/reject-envelope-kdf-iterations.bin",
        Rec::KeyEnvelope,
        Semantic("iterations above the reader's limit (LIMIT_EXCEEDED)"),
    ),
    (
        "c11/reject-envelope-kdf-lanes.bin",
        Rec::KeyEnvelope,
        Semantic("lanes above the reader's limit (LIMIT_EXCEEDED)"),
    ),
    (
        "c11/reject-envelope-kdf-memory-below-8-per-lane.bin",
        Rec::KeyEnvelope,
        Schema,
    ),
    (
        "c11/reject-envelope-kdf-memory.bin",
        Rec::KeyEnvelope,
        Semantic("memory above the reader's limit (LIMIT_EXCEEDED)"),
    ),
    (
        "c11/reject-envelope-kdf-zero-lanes.bin",
        Rec::KeyEnvelope,
        Schema,
    ),
    (
        "c11/reject-envelope-truncated.bin",
        Rec::KeyEnvelope,
        NoPayload("the frame is cut in half"),
    ),
    (
        "c11/reject-envelope-wrapped-key-flipped.bin",
        Rec::KeyEnvelope,
        Semantic("the AEAD tag of the wrapped key (KEY_UNAVAILABLE)"),
    ),
    (
        "c11/reject-envelope-wrong-frame-kind.bin",
        Rec::KeyEnvelope,
        Frame("an envelope payload in a sealed-object frame"),
    ),
];

/// Reject vectors whose payload is not CBOR (R2's binary envelopes, R5's sealed
/// objects), which this test does not cover.
fn not_cbor(dir: &str, name: &str) -> bool {
    (dir == "b2" && name.starts_with("reject-envelope-"))
        || (dir == "c11" && name.starts_with("reject-sealed-"))
}

#[test]
fn every_cbor_reject_vector_is_classified_and_checked() {
    // The table lists every CBOR reject vector, and nothing else.
    let mut on_disk = BTreeSet::new();
    for dir in ["b2", "c4", "c5", "c11", "d29"] {
        for (name, _) in files(dir, "bin") {
            if name.starts_with("reject-") && !not_cbor(dir, &name) {
                on_disk.insert(format!("{dir}/{name}"));
            }
        }
    }
    let listed: BTreeSet<String> = REJECTS.iter().map(|(n, _, _)| (*n).to_owned()).collect();
    assert_eq!(listed.len(), REJECTS.len(), "a vector is classified twice");
    assert_eq!(
        on_disk, listed,
        "docs: every CBOR reject vector is classified in REJECTS"
    );

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (path, kind, defect) in REJECTS {
        let (dir, name) = path.split_once('/').unwrap();
        let why = match defect {
            Semantic(w) | Frame(w) | Codec(w) | NoPayload(w) => *w,
            Schema | Version | Legacy => "",
        };
        let ctx = format!("{path} ({why})");
        let bytes = std::fs::read(golden(dir).join(name)).unwrap();
        let payload = bare_payload(&bytes).map(|(_, p)| p);
        let key = match defect {
            Schema => "schema",
            Semantic(_) => "semantic",
            Frame(_) => "frame",
            Codec(_) => "codec",
            Version => "version",
            Legacy => "legacy",
            NoPayload(_) => "no payload",
        };
        *counts.entry(key).or_default() += 1;
        if let NoPayload(_) = defect {
            assert!(payload.is_none(), "{ctx}: has a payload after all");
            continue;
        }
        let payload = payload.unwrap_or_else(|| panic!("{ctx}: not a skippable frame"));
        let version = schema_version(&payload);
        match defect {
            Schema => {
                let schema = kind.schema(version.unwrap()).unwrap();
                assert!(
                    validate(schema, &payload).is_err(),
                    "{ctx}: the defect is schema-level, but {schema} accepts it"
                );
                assert!(!kind.code_accepts(&payload), "{ctx}: the code accepts it");
            }
            Semantic(why) => {
                let schema = kind.schema(version.unwrap()).unwrap();
                if let Err(e) = validate(schema, &payload) {
                    panic!("{ctx}: classified semantic ({why}) but {schema} rejects it: {e}");
                }
                if *kind == Rec::KeyEnvelope {
                    // The envelope decodes; its cost limits and its AEAD tag
                    // are checked when the passphrase is applied.
                    assert!(
                        !envelope_opens(&payload),
                        "{ctx}: the code opens the envelope"
                    );
                } else {
                    assert!(!kind.code_accepts(&payload), "{ctx}: the code accepts it");
                }
            }
            Frame(_) => {
                let schema = kind.schema(version.unwrap()).unwrap();
                if let Err(e) = validate(schema, &payload) {
                    panic!("{ctx}: the payload should be valid but {schema} rejects it: {e}");
                }
                assert!(
                    kind.code_accepts(&payload),
                    "{ctx}: the payload should decode; only its frame is wrong"
                );
            }
            Codec(_) => {
                assert!(
                    cbor::decode(&payload, &CborLimits::default()).is_err(),
                    "{ctx}: the payload is canonical after all"
                );
                assert!(!kind.code_accepts(&payload), "{ctx}: the code accepts it");
            }
            Version => {
                let v = version.unwrap();
                assert!(kind.schema(v).is_none(), "{ctx}: v{v} has a schema");
                assert!(!kind.code_accepts(&payload), "{ctx}: the code accepts it");
            }
            Legacy => {
                assert_eq!(version, Some(0), "{ctx}");
                let legacy = kind.schema(0).unwrap();
                assert!(LEGACY.contains(&legacy), "{ctx}");
                if let Err(e) = validate(legacy, &payload) {
                    panic!("{ctx}: the legacy schema {legacy} rejects it: {e}");
                }
                assert!(!kind.code_accepts(&payload), "{ctx}: the code accepts it");
            }
            NoPayload(_) => unreachable!(),
        }
    }
    println!("reject vectors by defect: {counts:?}");
}

// ---- the tail-quarantine sidecar (a tool file, not archive format) ------------------

fn sidecar_metadata() -> Vec<u8> {
    use mochi_core::quarantine::SidecarMetadata;
    let m = SidecarMetadata {
        archive_id: mochi_core::object::ArchiveId::from_bytes([0xA1; 32]),
        head_commit_id: mochi_format::digest::CommitId::from_bytes([0xC2; 32]),
        head_seq: 7,
        tail_offset: 4096,
        tail_len: 100,
        tail_hash: mochi_format::digest::Digest::from_bytes([0x7A; 32]),
        frame_magics: vec![registry::COMMIT_RECORD, registry::RECOVERY_MANIFEST],
        incomplete_final_frame: true,
        tool: "mochi 0.1.0".into(),
        time: "2026-10-12T01:02:03.123456789Z".into(),
    };
    let header = m.header().unwrap();
    assert_eq!(&header[..8], &registry::TAIL_QUARANTINE_MAGIC);
    let len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    assert_eq!(header.len(), 12 + len);
    header[12..].to_vec()
}

#[test]
fn the_tail_quarantine_sidecar_metadata_validates() {
    let meta = sidecar_metadata();
    validate("tail-quarantine-v0", &meta).unwrap();
    // Every key is required; an unknown one is refused; the timestamp format
    // and the bool are enforced.
    let Value::Map(entries) = cbor::decode(&meta, &CborLimits::default()).unwrap() else {
        panic!("a map")
    };
    for (k, _) in &entries {
        let mut less = entries.clone();
        less.retain(|(x, _)| x != k);
        let bytes = cbor::encode(&Value::Map(less)).unwrap();
        assert!(
            validate("tail-quarantine-v0", &bytes).is_err(),
            "key {k} removed"
        );
    }
    // Every key's type is pinned: any other CBOR type is refused.
    for (i, (k, v)) in entries.iter().enumerate() {
        for alt in alternatives(v) {
            let mut other = entries.clone();
            other[i].1 = alt.clone();
            let bytes = cbor::encode(&Value::Map(other)).unwrap();
            assert!(
                validate("tail-quarantine-v0", &bytes).is_err(),
                "key {k} as {}",
                variant(&alt)
            );
        }
    }
    let mut extra = entries.clone();
    extra.push((11, Value::Uint(0)));
    let bytes = cbor::encode(&Value::Map(extra)).unwrap();
    assert!(
        validate("tail-quarantine-v0", &bytes).is_err(),
        "unknown key"
    );
    let mut bad_time = entries.clone();
    bad_time.last_mut().unwrap().1 = Value::Text("2026-10-12 01:02:03Z".into());
    let bytes = cbor::encode(&Value::Map(bad_time)).unwrap();
    assert!(
        validate("tail-quarantine-v0", &bytes).is_err(),
        "timestamp format"
    );
    // (The `cddl` crate matches `.regexp` as a search, not RFC 8610's implicit
    // full match, so a stray prefix or suffix is not caught here; the code's
    // own parser catches it, and R3 section 7 lists the pattern as a hand check.)
    for wrong in [
        "2026-10-12T01:02:03.12345678Z",
        "2026-10-12T01:02:03.123456789+00:00",
        "2026-10-12 01:02:03.123456789Z",
    ] {
        let mut v = entries.clone();
        v.last_mut().unwrap().1 = Value::Text(wrong.into());
        let bytes = cbor::encode(&Value::Map(v)).unwrap();
        assert!(validate("tail-quarantine-v0", &bytes).is_err(), "{wrong}");
    }
}

// ---- 4. differential run over mutants of the valid records --------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(u64),
    Idx(usize),
}

fn variant(v: &Value) -> &'static str {
    match v {
        Value::Uint(_) => "uint",
        Value::Nint(_) => "nint",
        Value::Bytes(_) => "bytes",
        Value::Text(_) => "text",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
        Value::Bool(_) => "bool",
        Value::Null => "null",
    }
}

fn at_mut<'a>(v: &'a mut Value, path: &[Step]) -> &'a mut Value {
    let Some((first, rest)) = path.split_first() else {
        return v;
    };
    match (first, v) {
        (Step::Key(k), Value::Map(m)) => {
            let e = m.iter_mut().find(|(x, _)| x == k).unwrap();
            at_mut(&mut e.1, rest)
        }
        (Step::Idx(i), Value::Array(a)) => at_mut(&mut a[*i], rest),
        _ => panic!("bad path"),
    }
}

/// The CBOR types of a record's direct parts, in order: the signature that
/// tells one kind of record in a list from another.
fn describe(v: &Value) -> String {
    match v {
        Value::Array(a) => a.iter().map(variant).collect::<Vec<_>>().join(","),
        Value::Map(m) => m
            .iter()
            .map(|(k, x)| format!("{k}={}", variant(x)))
            .collect::<Vec<_>>()
            .join(","),
        other => variant(other).to_owned(),
    }
}

/// Every node with its path and a "shape": the path with the index inside a
/// list of records (an array whose elements are maps or arrays) collapsed, so
/// that a mutation is tried once per kind of position, not once per element.
fn nodes(v: &Value, path: &mut Vec<Step>, shape: &mut String, out: &mut Vec<(Vec<Step>, String)>) {
    out.push((path.clone(), shape.clone()));
    match v {
        Value::Map(m) => {
            for (k, x) in m {
                path.push(Step::Key(*k));
                let n = shape.len();
                shape.push_str(&format!("/{k}"));
                nodes(x, path, shape, out);
                shape.truncate(n);
                path.pop();
            }
        }
        Value::Array(a) => {
            let list_of_records = matches!(a.first(), Some(Value::Map(_) | Value::Array(_)));
            for (i, x) in a.iter().enumerate() {
                path.push(Step::Idx(i));
                let n = shape.len();
                if list_of_records {
                    // One position per *kind* of record: a hole and a data
                    // extent, a regular file and a directory, a put and a
                    // delete differ in the types of their parts.
                    shape.push_str(&format!("/*<{}>", describe(x)));
                } else {
                    shape.push_str(&format!("/[{i}]"));
                }
                nodes(x, path, shape, out);
                shape.truncate(n);
                path.pop();
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A key removed, an unknown key added, or a value of another CBOR type:
    /// a defect any schema must see.
    Structural,
    /// Another value of the same type: only the schema's shape and ranges can
    /// object.
    Value,
}

struct Mutant {
    what: String,
    kind: Kind,
    value: Value,
}

fn alternatives(v: &Value) -> Vec<Value> {
    [
        Value::Null,
        Value::Uint(0),
        Value::Nint(0),
        Value::Bytes(vec![]),
        Value::Text(String::new()),
        Value::Array(vec![]),
        Value::Map(vec![]),
        Value::Bool(true),
    ]
    .into_iter()
    .filter(|a| variant(a) != variant(v))
    .collect()
}

/// The mutants of `root` not yet tried for `schema`: `seen` remembers the
/// kinds of position already mutated, across records of one schema.
fn mutants(root: &Value, schema: &str, seen: &mut BTreeSet<String>) -> Vec<Mutant> {
    let mut all = Vec::new();
    nodes(root, &mut Vec::new(), &mut String::new(), &mut all);
    let mut out = Vec::new();
    let mut push = |what: String, kind: Kind, value: Value, seen_key: String| {
        if seen.insert(format!("{schema}|{seen_key}")) {
            out.push(Mutant { what, kind, value });
        }
    };
    for (path, shape) in all {
        let mut probe = root.clone();
        let node = at_mut(&mut probe, &path).clone();
        let label = |op: &str| format!("{op} at {shape}");
        // Another CBOR type here.
        for alt in alternatives(&node) {
            let mut m = root.clone();
            *at_mut(&mut m, &path) = alt.clone();
            push(
                label(&format!("retype to {}", variant(&alt))),
                Kind::Structural,
                m,
                format!("retype {} {shape}", variant(&alt)),
            );
        }
        match &node {
            Value::Map(entries) => {
                // An unknown key at the end of this map.
                let mut m = root.clone();
                if let Value::Map(e) = at_mut(&mut m, &path) {
                    e.push((99, Value::Uint(0)));
                }
                push(
                    label("unknown key 99"),
                    Kind::Structural,
                    m,
                    format!("add {shape}"),
                );
                // Each key removed.
                for (k, _) in entries {
                    let mut m = root.clone();
                    if let Value::Map(e) = at_mut(&mut m, &path) {
                        e.retain(|(x, _)| x != k);
                    }
                    push(
                        label(&format!("remove key {k}")),
                        Kind::Structural,
                        m,
                        format!("remove {k} {shape}"),
                    );
                }
            }
            Value::Uint(n) => {
                for (tag, new) in [("+1", n.wrapping_add(1)), ("-1", n.wrapping_sub(1))] {
                    if *n == 0 && tag == "-1" {
                        continue;
                    }
                    let mut m = root.clone();
                    *at_mut(&mut m, &path) = Value::Uint(new);
                    push(
                        label(&format!("value {tag}")),
                        Kind::Value,
                        m,
                        format!("{tag} {shape}"),
                    );
                }
            }
            Value::Bytes(b) => {
                let mut shorter = b.clone();
                shorter.pop();
                let mut longer = b.clone();
                longer.push(0);
                for (tag, new) in [("shorter", shorter), ("longer", longer)] {
                    if tag == "shorter" && b.is_empty() {
                        continue;
                    }
                    let mut m = root.clone();
                    *at_mut(&mut m, &path) = Value::Bytes(new);
                    push(label(tag), Kind::Value, m, format!("{tag} {shape}"));
                }
            }
            _ => {}
        }
    }
    out
}

/// Recompute the commit ID (key 9) after a mutation that did not target it,
/// so the code's verdict is about the mutation, not about the stale ID.
fn refresh_commit_id(v: &mut Value) {
    let Value::Map(m) = v else { return };
    if !m.iter().any(|(k, _)| *k == 9) {
        return;
    }
    let body: Vec<(u64, Value)> = m.iter().filter(|(k, _)| *k != 9).cloned().collect();
    let Ok(bytes) = cbor::encode(&Value::Map(body)) else {
        return;
    };
    let id = commit_id(CanonicalCommitBody::assume_canonical(&bytes));
    for (k, x) in m.iter_mut() {
        if *k == 9 {
            *x = Value::Bytes(id.as_bytes().to_vec());
        }
    }
}

/// Structural mutants the code refuses and a schema rightly accepts: each is a
/// value the *schema* admits and a *rule of the code* excludes. The list is
/// the evidence that the schemas are tight everywhere else.
fn expected_structural_gap(kind: Rec, what: &str) -> Option<&'static str> {
    match (kind, what) {
        // `draft = uint / null`: null is the ratified archive's identifier
        // (D24); a number is a draft, which a 1.0 build refuses (only
        // `migrate` reads draft 2).
        (Rec::Descriptor, "retype to uint at /3") => {
            Some("a number is a draft identifier; a 1.0 build refuses it")
        }
        _ => None,
    }
}

/// Value mutants (the same CBOR type, another value) that the code refuses and
/// a schema admits: the position, and the R3 section 7.1 rule that refuses
/// them. Any other position where the schema is looser than the code fails the
/// test, so the list is the whole of "what a range or a literal in the schema
/// does not already say".
fn expected_value_gap(kind: Rec, what: &str) -> Option<&'static str> {
    let at = what.rsplit_once(" at ")?.1;
    // Collapse the record signatures: `/6/*<..>/5/0` is the position `/6/*/5/0`.
    let mut pos = String::new();
    let mut depth = 0;
    for ch in at.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            c if depth == 0 => pos.push(c),
            _ => {}
        }
    }
    match (kind, pos.as_str()) {
        (Rec::Descriptor, "/3") => Some("a 1.0 archive's draft identifier is null (D24)"),
        (Rec::Descriptor, "/4/[0]") => Some("a required feature is known"),
        (Rec::Descriptor, "/5/1/0") => Some("the redundancy suite is known"),
        (Rec::Commit, "/10/0") => Some("the descriptor is referenced at offset 0"),
        (Rec::Commit, "/2" | "/4/1") => Some("parent sequence = sequence - 1"),
        (Rec::Commit, "/6/0" | "/12/0" | "/12/1") => {
            Some("a data region ends before the delta manifest")
        }
        (Rec::Manifest, "/7/*/[1]" | "/8/*/[0]") => Some("stored paths are valid"),
        (Rec::Manifest, "/2" | "/3/0") => Some("parent sequence = sequence - 1"),
        (Rec::Manifest, "/11/*/[1]" | "/11/*/[2]" | "/11/0/[0]" | "/11/1/*/[1]") => {
            Some("retention operations and state are bounded and ordered")
        }
        (Rec::Manifest, "/12/1/*/[0]" | "/12/2/[0]" | "/12/2/[1]" | "/12/2/[2]" | "/12/2/[3]") => {
            Some("provenance sequences are ordered and consistent")
        }
        _ => None,
    }
}

#[test]
fn the_schemas_and_the_code_agree_on_mutants_of_every_valid_record() {
    let mut all: Vec<Record> = corpus();
    all.extend(encrypted_manifests());
    all.extend(encrypted_provenance_variants());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let (mut total, mut code_ok, mut both_reject, mut gaps) = (0usize, 0usize, 0usize, 0usize);
    let mut failures = Vec::new();
    let mut gap_notes: BTreeMap<String, usize> = BTreeMap::new();
    let mut value_notes: BTreeMap<String, usize> = BTreeMap::new();
    for r in &all {
        let Some(version) = schema_version(&r.payload) else {
            continue;
        };
        let Some(own_schema) = r.kind.schema(version) else {
            continue;
        };
        let Ok(root) = cbor::decode(&r.payload, &CborLimits::default()) else {
            continue;
        };
        for m in mutants(&root, own_schema, &mut seen) {
            let mut value = m.value.clone();
            // Leave key 9 alone when the mutation targets the ID itself.
            if r.kind == Rec::Commit && !m.what.contains("key 9") && !m.what.ends_with("/9") {
                refresh_commit_id(&mut value);
            }
            let Ok(bytes) = cbor::encode(&value) else {
                continue;
            };
            total += 1;
            // The schema the mutant claims by its own key 0 (or the original's
            // when key 0 is gone); an unknown version has no schema.
            let schema = match schema_version(&bytes) {
                Some(v) => match r.kind.schema(v) {
                    Some(s) => s,
                    None => continue,
                },
                None => own_schema,
            };
            let code = r.kind.code_accepts(&bytes);
            let cddl_ok = validate(schema, &bytes).is_ok();
            let label = format!("{} [{}] {}", r.origin, schema, m.what);
            if code {
                code_ok += 1;
                if !cddl_ok {
                    failures.push(format!("code accepts, {schema} rejects: {label}"));
                }
            } else if cddl_ok && m.kind == Kind::Structural {
                if expected_structural_gap(r.kind, &m.what).is_none() {
                    gaps += 1;
                    *gap_notes
                        .entry(format!("{schema}: {}", m.what))
                        .or_default() += 1;
                }
            } else if cddl_ok {
                // A value of the same type that the schema admits and the code
                // refuses: one of the hand-checked rules of R3 section 7.1.
                if expected_value_gap(r.kind, &m.what).is_none() {
                    *value_notes
                        .entry(format!("{schema}: {}", m.what))
                        .or_default() += 1;
                }
            } else {
                both_reject += 1;
            }
        }
    }
    println!(
        "{total} mutants: code accepts {code_ok}, both reject {both_reject}, \
         structural gaps {gaps}"
    );
    for (k, n) in &gap_notes {
        println!("GAP {n}x {k}");
    }
    for (k, n) in &value_notes {
        println!("VALUE {n}x {k}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(
        gap_notes.is_empty(),
        "structural mutants the schema accepts: {gap_notes:#?}"
    );
    assert!(
        value_notes.is_empty(),
        "value mutants the schema accepts and the code refuses, with no rule in \
         expected_value_gap: {value_notes:#?}"
    );
}

// ---- the vector index (R3 section 7) ------------------------------------------------

/// Prints, per schema, which key positions the valid vectors exercise and
/// where each schema-level reject fails. It is the data behind R3 section 7's
/// vector index and the input to the G1 audit (W16):
/// `cargo test -p mochi-testkit --test r3_cddl print_vector_index -- --ignored --nocapture`.
#[test]
#[ignore = "prints the data behind R3 section 7"]
fn print_vector_index() {
    let mut valid: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut all = corpus();
    all.extend(encrypted_manifests());
    all.extend(encrypted_provenance_variants());
    for r in &all {
        let v = schema_version(&r.payload).unwrap();
        let schema = r.kind.schema(v).unwrap().to_owned();
        let root = cbor::decode(&r.payload, &CborLimits::default()).unwrap();
        let mut shapes = Vec::new();
        nodes(&root, &mut Vec::new(), &mut String::new(), &mut shapes);
        for (_, shape) in shapes {
            let origin = r.origin.split('#').next().unwrap().to_owned();
            valid
                .entry((schema.clone(), shape))
                .or_default()
                .insert(origin);
        }
    }
    for ((schema, shape), who) in &valid {
        let who: Vec<_> = who.iter().take(3).cloned().collect();
        println!("VALID {schema} {shape} <- {}", who.join(", "));
    }
    for (path, kind, defect) in REJECTS {
        if !matches!(defect, Schema) {
            continue;
        }
        let (dir, name) = path.split_once('/').unwrap();
        let bytes = std::fs::read(golden(dir).join(name)).unwrap();
        let (_, payload) = bare_payload(&bytes).unwrap();
        let schema = kind.schema(schema_version(&payload).unwrap()).unwrap();
        let err = validate(schema, &payload).unwrap_err();
        println!(
            "REJECT {path} [{schema}] {}",
            err.lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(150)
                .collect::<String>()
        );
    }
}

// ---- the bounded values of the schemas are the code's bounds ------------------------

/// A CBOR map's entries.
type Entries = Vec<(u64, Value)>;

/// The generated mutants move a value by one, which cannot find a bound that
/// is too wide by more. These cases walk the bounds of the attribute, time, and
/// number types on both sides of each edge, in every schema that has them, and
/// require the schema and the production decoder to agree.
#[test]
fn the_bounded_values_of_the_schemas_are_exactly_the_codes_bounds() {
    use mochi_core::manifest::WINDOWS_PROMISED_MASK;
    let mut all: Vec<Record> = corpus();
    all.extend(encrypted_manifests());

    // (schema, path to a file version's attributes map) for each manifest schema.
    let mut attribute_homes: BTreeMap<&str, (Value, Vec<Step>)> = BTreeMap::new();
    for r in all.iter().filter(|r| r.kind == Rec::Manifest) {
        let schema = r.kind.schema(schema_version(&r.payload).unwrap()).unwrap();
        if attribute_homes.contains_key(schema) {
            continue;
        }
        let root = cbor::decode(&r.payload, &CborLimits::default()).unwrap();
        let mut found = Vec::new();
        nodes(&root, &mut Vec::new(), &mut String::new(), &mut found);
        // `/6/<i>/5`: the attributes of the i-th file version.
        if let Some((path, _)) = found
            .into_iter()
            .find(|(p, _)| matches!(p.as_slice(), [Step::Key(6), Step::Idx(_), Step::Key(5)]))
        {
            attribute_homes.insert(schema, (root, path));
        }
    }
    assert_eq!(
        attribute_homes.len(),
        3,
        "a record with attributes for each manifest schema"
    );

    let i64_min_below = (1u64 << 63, "−2^63 − 1");
    let mut cases = 0;
    for (schema, (root, path)) in &attribute_homes {
        let run = |edit: &dyn Fn(&mut Entries)| -> (bool, bool) {
            let mut m = root.clone();
            let Value::Map(attrs) = at_mut(&mut m, path) else {
                panic!("attributes are a map")
            };
            attrs.retain(|(k, _)| !matches!(k, 0..=2));
            edit(attrs);
            attrs.sort_by_key(|(k, _)| *k);
            let bytes = cbor::encode(&m).unwrap();
            (
                Rec::Manifest.code_accepts(&bytes),
                validate(schema, &bytes).is_ok(),
            )
        };
        let u = Value::Uint;
        let u32max = u64::from(u32::MAX);

        // POSIX: permission bits at most 0o7777; numeric uid and gid at most u32.
        for (mode, uid, gid) in [
            (0, 0, 0),
            (0o7777, 0, 0),
            (0o10000, 0, 0),
            (0o7777, u32max, u32max),
            (0, u32max + 1, 0),
            (0, 0, u32max + 1),
        ] {
            let want = mode <= 0o7777 && uid <= u32max && gid <= u32max;
            let (code, cddl) = run(&|a| a.push((0, Value::Array(vec![u(mode), u(uid), u(gid)]))));
            assert_eq!(
                (code, cddl),
                (want, want),
                "{schema}: posix {mode:o} {uid} {gid}"
            );
            cases += 1;
        }

        // Windows attributes: exactly the sixteen values of mask 0x27.
        let promised = u64::from(WINDOWS_PROMISED_MASK);
        for v in (0..=0x80u64).chain([0x100, 0x1_0000, u32max, u32max + 1]) {
            let want = v <= u32max && v & !promised == 0;
            let (code, cddl) = run(&|a| a.push((1, u(v))));
            assert_eq!((code, cddl), (want, want), "{schema}: windows {v:#x}");
            cases += 1;
        }

        // mtime: nanoseconds below 10^9. Seconds fit a signed 64-bit integer: the
        // schema says `int` and the code adds the range (R3 section 7.1).
        let secs = [
            (u(0), true),
            (u(i64::MAX as u64), true),
            (Value::Nint((1u64 << 63) - 1), true), // i64::MIN
            (u(1 << 63), false),
            (Value::Nint(i64_min_below.0), false),
        ];
        for (s, in_range) in secs {
            for (nanos, nanos_ok) in [(0, true), (999_999_999, true), (1_000_000_000, false)] {
                let want = in_range && nanos_ok;
                let (code, cddl) = run(&|a| a.push((2, Value::Array(vec![s.clone(), u(nanos)]))));
                assert_eq!(code, want, "{schema}: mtime {s:?} {nanos}");
                // Only the seconds' 64-bit range is the code's alone.
                assert_eq!(
                    cddl, nanos_ok,
                    "{schema}: mtime {s:?} {nanos} in the schema"
                );
                cases += 1;
            }
        }
    }

    // The commit's informational time (commit schema 1, key 8): the same types.
    let commit = all
        .iter()
        .find(|r| {
            r.kind == Rec::Commit
                && schema_version(&r.payload) == Some(1)
                && matches!(
                    cbor::decode(&r.payload, &CborLimits::default()),
                    Ok(Value::Map(m)) if m.iter().any(|(k, _)| *k == 8)
                )
        })
        .expect("a v1 commit with a time");
    let root = cbor::decode(&commit.payload, &CborLimits::default()).unwrap();
    for (s, nanos, want) in [
        (Value::Uint(0), 0u64, true),
        (Value::Uint(i64::MAX as u64), 999_999_999, true),
        (Value::Nint((1 << 63) - 1), 0, true),
        (Value::Uint(1 << 63), 0, false),
        (Value::Nint(1 << 63), 0, false),
        (Value::Uint(0), 1_000_000_000, false),
    ] {
        let mut m = root.clone();
        let Value::Map(entries) = &mut m else {
            panic!()
        };
        for (k, v) in entries.iter_mut() {
            if *k == 8 {
                *v = Value::Array(vec![s.clone(), Value::Uint(nanos)]);
            }
        }
        refresh_commit_id(&mut m);
        let bytes = cbor::encode(&m).unwrap();
        let (code, cddl) = (
            Rec::Commit.code_accepts(&bytes),
            validate("commit-record-v1", &bytes).is_ok(),
        );
        assert_eq!(code, want, "commit time {s:?} {nanos}");
        assert_eq!(
            cddl,
            nanos < 1_000_000_000,
            "commit time {s:?} {nanos} in the schema"
        );
        cases += 1;
    }
    println!("{cases} bounded-value cases");
}
