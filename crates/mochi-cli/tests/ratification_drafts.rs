//! Drift test for the draft ratification artifacts R1, R2, R4, R6, and R7
//! (`docs/ratification/`, plan §6).
//!
//! The drafts are written from the code. Every table they mark *(checked)* is
//! parsed here and compared with the code it describes, so a change to the
//! registry, an envelope layout, the DDL, or the error-code registry fails
//! this test until the draft says the same thing. Two lists are compared with
//! the spec instead (R2's D11 obligations, R4's missing §10.1 tables), because
//! the draft restates them from there.
//!
//! The test never spells a frame magic or an 8-byte MOCHI magic: those are
//! read from the documents and compared with `mochi_format::registry`
//! (AGENTS.md, `ci/check-invariants.sh` rule 2).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use mochi_core::report::{Finding, Report, SeqRange, Severity, SkippedItem};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{classify, ErrorClass};
use mochi_core::ErrorCode;
use mochi_format::envelope::{self, BinaryEnvelope, PayloadEncoding, RecordIdentity};
use mochi_format::registry::{self, FrameKind};

// ---- reading the documents ---------------------------------------------------------

fn repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The lines after the first heading that contains `needle`, up to the next
/// heading of the same or a higher level.
fn section<'a>(doc: &'a str, needle: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = doc.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with('#') && l.contains(needle))
        .unwrap_or_else(|| panic!("no heading containing {needle:?}"));
    let level = lines[start].chars().take_while(|c| *c == '#').count();
    let end = lines[start + 1..]
        .iter()
        .position(|l| {
            let n = l.chars().take_while(|c| *c == '#').count();
            n > 0 && n <= level && l[n..].starts_with(' ')
        })
        .map_or(lines.len(), |i| start + 1 + i);
    lines[start + 1..end].to_vec()
}

/// Strip backticks and surrounding space from a table cell.
fn clean(cell: &str) -> String {
    cell.trim().replace('`', "")
}

/// The `n`th markdown table (0-based) in `lines`: data rows only, cells
/// cleaned.
fn table(lines: &[&str], n: usize) -> Vec<Vec<String>> {
    let mut tables: Vec<Vec<Vec<String>>> = Vec::new();
    let mut in_table = false;
    for line in lines {
        let t = line.trim();
        if t.starts_with('|') {
            if !in_table {
                tables.push(Vec::new());
                in_table = true;
            }
            let cells: Vec<String> = t.trim_matches('|').split('|').map(clean).collect();
            tables.last_mut().unwrap().push(cells);
        } else {
            in_table = false;
        }
    }
    let rows = tables
        .into_iter()
        .nth(n)
        .unwrap_or_else(|| panic!("table {n} missing"));
    // Header row, then the `|---|` separator, then data.
    assert!(rows.len() >= 2 && rows[1].iter().all(|c| c.starts_with('-')));
    rows.into_iter().skip(2).collect()
}

/// Every backticked word in `lines`, in order.
fn backticked(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for line in lines {
        let mut parts = line.split('`');
        parts.next();
        while let Some(inner) = parts.next() {
            out.push(inner.to_owned());
            parts.next();
        }
    }
    out
}

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(s.len().is_multiple_of(2), "odd hex {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or_else(|_| panic!("hex {s:?}")))
        .collect()
}

/// A byte expression as the drafts write them: parts joined by `‖`, each
/// either hex digits (with spaces) or ASCII text.
fn bytes_expr(cell: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for part in cell.split('‖') {
        let p = part.trim();
        let is_hex = p.len() >= 2
            && p.chars().all(|c| c.is_ascii_hexdigit() || c == ' ')
            && p.chars().filter(|c| *c != ' ').count() % 2 == 0
            && p.chars().any(|c| c.is_ascii_digit());
        if is_hex && !p.chars().any(|c| c.is_ascii_uppercase()) {
            out.extend(hex(p));
        } else {
            out.extend_from_slice(p.as_bytes());
        }
    }
    out
}

fn magic(cell: &str) -> u32 {
    let h = cell
        .trim()
        .strip_prefix("0x")
        .unwrap_or_else(|| panic!("{cell:?}"));
    u32::from_str_radix(h, 16).unwrap()
}

fn kind_of(cell: &str) -> FrameKind {
    let m = magic(cell);
    FrameKind::from_magic(m).unwrap_or_else(|| panic!("{m:#x} not registered"))
}

/// The JSON string form of a serializable enum value.
macro_rules! json_str {
    () => {
        |v| {
            serde_json::to_value(v)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        }
    };
}

// ---- R1: frame registry and version mapping ----------------------------------------

const R1: &str = "docs/ratification/R1-frame-registry-draft.md";

#[test]
fn r1_version_identifiers_match_the_code() {
    let doc = repo_file(R1);
    let rows: BTreeMap<String, String> = table(&section(&doc, "1. Version identifiers"), 0)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    assert_eq!(
        rows["Specification revision"],
        mochi_format::version::SPEC_REVISION
    );
    assert_eq!(
        rows["Wire generation"],
        mochi_format::version::WIRE_GENERATION.to_string()
    );
    assert_eq!(bytes_expr(&rows["Footer magic"]), registry::FOOTER_MAGIC);
    assert_eq!(
        rows["Draft identifier"],
        mochi_core::descriptor::DRAFT_ID.to_string()
    );
    assert_eq!(rows["Format status"], mochi_format::version::FORMAT_STATUS);
    assert_eq!(rows.len(), 5);
}

#[test]
fn r1_frame_registry_matches_the_code() {
    let doc = repo_file(R1);
    let rows = table(&section(&doc, "2. Frame registry"), 0);
    let mut seen = BTreeSet::new();
    for r in &rows {
        let m = magic(&r[0]);
        let kind = kind_of(&r[0]);
        assert_eq!(format!("{kind:?}"), r[1], "{m:#x}: kind");
        assert_eq!(
            kind.is_skippable(),
            match r[2].as_str() {
                "yes" => true,
                "no" => false,
                other => panic!("{m:#x}: skippable {other:?}"),
            },
            "{m:#x}: skippable"
        );
        let offset = match r[3].as_str() {
            "—" => None,
            n => Some(n.parse::<u64>().unwrap()),
        };
        assert_eq!(kind.required_offset(), offset, "{m:#x}: offset");
        assert!(seen.insert(m), "{m:#x} listed twice");
    }
    let registered: BTreeSet<u32> = std::iter::once(registry::ZSTD_DATA_FRAME)
        .chain(registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX)
        .collect();
    assert_eq!(seen, registered, "R1 lists exactly the registered magics");
}

#[test]
fn r1_footer_layout_matches_an_encoded_footer() {
    let doc = repo_file(R1);
    let rows = table(&section(&doc, "3. Commit footer"), 0);
    let commit_frame = [0x5Au8; 37];
    let (offset, seq) = (0x0102_0304_0506_0708u64, 0x1112_1314_1516_1718u64);
    let frame = mochi_format::footer::encode_footer_frame(offset, seq, &commit_frame);
    let payload = &frame[registry::SKIPPABLE_HEADER_LEN..];
    assert_eq!(payload.len(), registry::FOOTER_PAYLOAD_LEN);
    let mut at = 0;
    for r in &rows {
        let (o, n): (usize, usize) = (r[0].parse().unwrap(), r[1].parse().unwrap());
        assert_eq!(o, at, "{}: footer fields are contiguous", r[2]);
        let field = &payload[o..o + n];
        let expected: Vec<u8> = if r[2].starts_with("Magic") {
            registry::FOOTER_MAGIC.to_vec()
        } else if r[2].starts_with("Commit-frame offset") {
            offset.to_le_bytes().to_vec()
        } else if r[2].starts_with("Commit-frame stored length") {
            (commit_frame.len() as u64).to_le_bytes().to_vec()
        } else if r[2].starts_with("Commit sequence") {
            seq.to_le_bytes().to_vec()
        } else if r[2].starts_with("Footer digest") {
            let mut prefix = [0u8; 32];
            prefix.copy_from_slice(&payload[..32]);
            mochi_format::digest::footer_digest(
                &prefix,
                mochi_format::digest::StoredObjectBytes::new(&commit_frame),
            )
            .to_vec()
        } else {
            panic!("unexpected footer field {:?}", r[2]);
        };
        assert_eq!(field, expected, "{}", r[2]);
        at = o + n;
    }
    assert_eq!(at, registry::FOOTER_PAYLOAD_LEN);
}

#[test]
fn r1_registered_byte_strings_match_the_code() {
    let doc = repo_file(R1);
    let rows = table(&section(&doc, "4. Other registered byte strings"), 0);
    let code: BTreeMap<&str, &[u8]> = [
        ("FOOTER_MAGIC", &registry::FOOTER_MAGIC[..]),
        (
            "TAIL_QUARANTINE_MAGIC",
            &registry::TAIL_QUARANTINE_MAGIC[..],
        ),
        (
            "SQLITE_IMAGE_SIGNATURE",
            &registry::SQLITE_IMAGE_SIGNATURE[..],
        ),
        ("ZSTD_IMAGE_SIGNATURE", &registry::ZSTD_IMAGE_SIGNATURE[..]),
        ("PARITY_SIGNATURE", &registry::PARITY_SIGNATURE[..]),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows.len(), code.len());
    for r in &rows {
        assert_eq!(hex(&r[1]), code[r[0].as_str()], "{}", r[0]);
    }
}

/// What Annex B added to the wire format after ratification, never by editing
/// `docs/ratification/` (AGENTS.md: a change is a new identifier with an Annex B
/// entry). The R1 and R3 checks below compare the frozen documents with the
/// code *minus* exactly these additions, so an addition that has no entry here
/// still fails them.
mod since_ratification {
    use mochi_core::commit;
    use mochi_format::registry;

    /// B.2.19 (D29, the archive comment): commit record schema 4.
    pub const COMMIT_SCHEMAS: &[u64] = &[commit::COMMENT_SCHEMA_VERSION];
    /// B.2.19: required feature 3, known in commit records only.
    pub const COMMIT_FEATURES: &[u64] = &[registry::FEATURE_COMMENT];
    /// B.2.19: its schema file.
    pub const CDDL_FILES: &[&str] = &["commit-record-v4.cddl"];

    /// `all` without the additions.
    pub fn frozen(all: &[u64], added: &[u64]) -> Vec<u64> {
        all.iter().copied().filter(|v| !added.contains(v)).collect()
    }
}

#[test]
fn r1_record_schema_versions_match_the_code() {
    use mochi_core::{commit, descriptor, image, keys, manifest};
    let doc = repo_file(R1);
    let rows = table(&section(&doc, "5. Record schema versions"), 0);
    let mut kinds = BTreeSet::new();
    for r in &rows {
        let kind = kind_of(&r[0]);
        let listed: Vec<u64> = r[2].split(',').map(|v| v.trim().parse().unwrap()).collect();
        let code: Vec<u64> = match kind {
            FrameKind::ArchiveDescriptor => vec![descriptor::SCHEMA_VERSION],
            FrameKind::CommitRecord => {
                // B.2.19 (D29) adds schema 4 after ratification.
                assert_eq!(since_ratification::COMMIT_SCHEMAS, [4]);
                vec![commit::SCHEMA_VERSION, commit::ENCRYPTED_SCHEMA_VERSION]
            }
            FrameKind::RecoveryManifest => vec![
                manifest::SCHEMA_VERSION,
                manifest::RETENTION_SCHEMA_VERSION,
                manifest::ENCRYPTED_SCHEMA_VERSION,
            ],
            FrameKind::KeyEnvelope => vec![keys::ENVELOPE_SCHEMA_VERSION],
            FrameKind::MetadataDelta => image::IMAGE_SCHEMA_VERSIONS
                .iter()
                .map(|v| u64::from(*v))
                .collect(),
            FrameKind::EncryptedObject => vec![u64::from(mochi_format::seal::SEAL_VERSION)],
            FrameKind::ParityObject => {
                vec![u64::from(mochi_format::parity::RECORD_SCHEMA_VERSION)]
            }
            other => panic!("R1 §5 lists {other:?}, which has no record schema"),
        };
        assert_eq!(listed, code, "{kind:?}");
        assert!(kinds.insert(format!("{kind:?}")), "{kind:?} listed twice");
    }
    assert_eq!(kinds.len(), 7);
    // "Required-feature identifiers registered: **1**".
    assert_eq!(mochi_format::seal::FEATURE_ENCRYPTED, 1);
    for known in [
        descriptor::KNOWN_REQUIRED_FEATURES.to_vec(),
        since_ratification::frozen(
            commit::KNOWN_REQUIRED_FEATURES,
            since_ratification::COMMIT_FEATURES,
        ),
        manifest::KNOWN_REQUIRED_FEATURES.to_vec(),
    ] {
        assert_eq!(known, [mochi_format::seal::FEATURE_ENCRYPTED]);
    }
    // B.2.19: the comment is identifier 3, in commit records only; 2 stays
    // unassigned (the frozen vectors use it as the unknown feature).
    assert_eq!(since_ratification::COMMIT_FEATURES, [3]);
    assert!(!commit::KNOWN_REQUIRED_FEATURES.contains(&2));
}

// ---- R2: record envelopes -----------------------------------------------------------

const R2: &str = "docs/ratification/R2-record-envelopes-draft.md";

#[test]
fn r2_obligations_list_every_d11_field() {
    let spec = repo_file("docs/spec.md");
    let d11: Vec<&str> = spec
        .lines()
        .skip_while(|l| !l.starts_with("**D11 — Record envelopes"))
        .collect();
    let spec_fields: Vec<String> = table(&d11, 0).into_iter().map(|r| r[0].clone()).collect();
    let doc = repo_file(R2);
    let draft_fields: Vec<String> = table(&section(&doc, "1. Two encodings"), 0)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(draft_fields, spec_fields);
}

#[test]
fn r2_binary_envelope_layout_matches_an_encoded_record() {
    let doc = repo_file(R2);
    let rows = table(&section(&doc, "2. Binary envelope v0"), 0);

    // A minimal SQLite-signed payload whose user_version (bytes 60..64) is 0.
    let mut payload = vec![0u8; 100];
    payload[..16].copy_from_slice(&registry::SQLITE_IMAGE_SIGNATURE);
    let feature = mochi_format::seal::FEATURE_ENCRYPTED;
    let env = BinaryEnvelope {
        record_schema_version: 0,
        encoding: PayloadEncoding::SqliteImage,
        identity: RecordIdentity {
            archive_id: [0xA1; 32],
            commit_sequence: 0x0102_0304_0506_0708,
            transaction_id: [0xB2; 16],
        },
        required_features: vec![feature],
    };
    let frame = envelope::encode_binary_record(
        FrameKind::MetadataDelta,
        &env,
        &payload,
        &[feature],
        &mochi_format::Limits::default(),
    )
    .unwrap();
    let body = &frame[registry::SKIPPABLE_HEADER_LEN..];
    let n = env.required_features.len();
    let header_len = envelope::BINARY_ENVELOPE_FIXED_LEN + 8 * n;

    let mut at = 0;
    for r in &rows {
        let o: usize = r[0].parse().unwrap();
        let size = if r[1] == "8·n" {
            8 * n
        } else {
            r[1].parse().unwrap()
        };
        assert_eq!(o, at, "{}: fields are contiguous", r[2]);
        let expected: Vec<u8> = match r[2].as_str() {
            "Header length" => (header_len as u32).to_le_bytes().to_vec(),
            "Envelope version" => envelope::BINARY_ENVELOPE_VERSION.to_le_bytes().to_vec(),
            "Record schema version" => env.record_schema_version.to_le_bytes().to_vec(),
            "Payload encoding" => env.encoding.code().to_le_bytes().to_vec(),
            "n = required-feature count" => (n as u32).to_le_bytes().to_vec(),
            "Payload length" => (payload.len() as u64).to_le_bytes().to_vec(),
            "Archive ID" => env.identity.archive_id.to_vec(),
            "Commit sequence" => env.identity.commit_sequence.to_le_bytes().to_vec(),
            "Transaction ID" => env.identity.transaction_id.to_vec(),
            "Required features" => feature.to_le_bytes().to_vec(),
            other => panic!("unexpected envelope field {other:?}"),
        };
        assert_eq!(&body[o..o + size], expected, "{}", r[2]);
        at = o + size;
    }
    assert_eq!(at, header_len, "the header ends where the table ends");
    assert_eq!(
        &body[header_len..],
        payload,
        "the payload follows the header"
    );
}

#[test]
fn r2_payload_encodings_and_enveloped_kinds_match_the_code() {
    let doc = repo_file(R2);
    let lines = section(&doc, "2. Binary envelope v0");

    let encodings = table(&lines, 1);
    for r in &encodings {
        let code: u32 = r[0].parse().unwrap();
        let enc = PayloadEncoding::from_code(code).unwrap_or_else(|| panic!("code {code}"));
        assert_eq!(
            bytes_expr(&r[2].replace(" (16 bytes)", "").replace(" (4 bytes)", "")),
            enc.signature()
        );
    }
    let registered = (0..=1024u32)
        .filter(|c| PayloadEncoding::from_code(*c).is_some())
        .count();
    assert_eq!(
        registered,
        encodings.len(),
        "R2 lists every registered encoding"
    );

    let kinds: BTreeSet<String> = table(&lines, 2)
        .iter()
        .map(|r| format!("{:?}", kind_of(&r[0])))
        .collect();
    let code: BTreeSet<String> = std::iter::once(registry::ZSTD_DATA_FRAME)
        .chain(registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX)
        .filter_map(FrameKind::from_magic)
        .filter(|k| envelope::carries_binary_envelope(*k))
        .map(|k| format!("{k:?}"))
        .collect();
    assert_eq!(kinds, code);
}

// ---- R4: catalog DDL ----------------------------------------------------------------

const R4: &str = "docs/ratification/R4-catalog-ddl-draft.md";

#[test]
fn r4_ddl_is_the_code_ddl_byte_for_byte() {
    let doc = repo_file(R4);
    let block: String = section(&doc, "2. DDL")
        .into_iter()
        .skip_while(|l| *l != "```sql")
        .skip(1)
        .take_while(|l| *l != "```")
        .map(|l| format!("{l}\n"))
        .collect();
    assert_eq!(block.trim(), mochi_core::catalog::schema::DDL.trim());
}

#[test]
fn r4_header_values_and_meta_keys_match_the_code() {
    use mochi_core::catalog::schema;
    let doc = repo_file(R4);
    let lines = section(&doc, "1. The image");
    let header: BTreeMap<String, String> = table(&lines, 0)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    let first_word = |s: &str| s.split_whitespace().next().unwrap().to_owned();
    assert_eq!(
        magic(&first_word(&header["application_id"])) as i32,
        schema::APPLICATION_ID
    );
    assert_eq!(
        first_word(&header["user_version"]),
        schema::SCHEMA_VERSION.to_string()
    );
    assert_eq!(header["Page size"], schema::PAGE_SIZE.to_string());

    let keys: BTreeSet<String> = table(&lines, 1).into_iter().map(|r| r[0].clone()).collect();
    let code: BTreeSet<String> = [
        mochi_core::catalog::META_ARCHIVE_ID,
        mochi_core::publish::META_WRITER_PARAMS,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, code);
}

#[test]
fn r4_lists_exactly_the_spec_tables_the_ddl_lacks() {
    let ddl_tables: BTreeSet<String> = mochi_core::catalog::schema::DDL
        .lines()
        .filter_map(|l| l.strip_prefix("CREATE TABLE "))
        .map(|l| l.split_whitespace().next().unwrap().to_owned())
        .collect();
    let spec = repo_file("docs/spec.md");
    let spec_tables: BTreeSet<String> = table(&section(&spec, "10.1 Immutable records"), 0)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    let doc = repo_file(R4);
    let lines = section(&doc, "4. §10.1 tables");
    let firsts =
        |n| -> BTreeSet<String> { table(&lines, n).into_iter().map(|r| r[0].clone()).collect() };

    let missing: BTreeSet<String> = spec_tables.difference(&ddl_tables).cloned().collect();
    assert_eq!(firsts(0), missing, "§10.1 tables the DDL lacks");
    let extra: BTreeSet<String> = ddl_tables.difference(&spec_tables).cloned().collect();
    assert_eq!(firsts(1), extra, "DDL tables §10.1 does not name");
}

// ---- R7: reports and error codes ----------------------------------------------------

const R7: &str = "docs/ratification/R7-reports-and-error-codes-draft.md";

/// Every `Status`. The match makes a new variant a compile error here.
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

/// Every `VerificationLevel`, exhaustively as above.
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

#[test]
fn r7_enumerations_match_the_code() {
    let doc = repo_file(R7);

    let statuses: BTreeSet<String> = table(&section(&doc, "1. Status values"), 0)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    let code: BTreeSet<String> = all_statuses().iter().map(json_str!()).collect();
    assert_eq!(statuses, code);

    let dims: Vec<String> = backticked(&section(&doc, "2. Dimensions"));
    let code: Vec<String> = Dimension::ALL.iter().map(json_str!()).collect();
    assert_eq!(dims, code);

    let levels: Vec<String> = table(&section(&doc, "3. Verification levels"), 0)
        .into_iter()
        .map(|r| r[0].clone())
        .collect();
    let code: Vec<String> = all_levels().iter().map(json_str!()).collect();
    assert_eq!(levels, code);

    let other: BTreeMap<String, Vec<String>> = table(&section(&doc, "4. Other enumerations"), 0)
        .into_iter()
        .map(|r| (r[0].clone(), r[1].split(", ").map(str::to_owned).collect()))
        .collect();
    let severities: Vec<String> = [Severity::Info, Severity::Warning, Severity::Error]
        .iter()
        .map(json_str!())
        .collect();
    assert_eq!(other["findings[].severity"], severities);
    use mochi_core::report::FreshnessAnchorKind as A;
    let anchors: Vec<String> = [A::None, A::User, A::LocalHistory]
        .iter()
        .map(json_str!())
        .collect();
    assert_eq!(other["freshness_anchor"], anchors);
}

#[test]
fn r7_exit_codes_match_the_code() {
    use mochi_cli::exit;
    let doc = repo_file(R7);
    let rows = table(&section(&doc, "5. Exit codes"), 0);
    let code = [
        ("OK", exit::OK),
        ("FAILED", exit::FAILED),
        ("DEGRADED", exit::DEGRADED),
        ("ERROR", exit::ERROR),
        ("UNSUPPORTED", exit::UNSUPPORTED),
    ];
    assert_eq!(rows.len(), code.len());
    for (r, (name, value)) in rows.iter().zip(code) {
        assert_eq!(r[1], name);
        assert_eq!(r[0], value.to_string(), "{name}");
    }
}

#[test]
fn r7_error_registry_matches_both_exhaustive_matches() {
    let doc = repo_file(R7);
    let rows = table(&section(&doc, "6. Error codes"), 0);
    let listed: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    let code: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(listed, code, "R7 lists the registry in registry order");
    for (r, c) in rows.iter().zip(ErrorCode::ALL) {
        assert_eq!(
            r[1],
            mochi_cli::exit_code_for(*c).to_string(),
            "{c}: exit as error"
        );
        let class = match classify(*c) {
            ErrorClass::Violation => "violation",
            ErrorClass::Unsupported => "unsupported",
            ErrorClass::Operational => "operational",
        };
        assert_eq!(r[2], class, "{c}: verification class");
        assert!(!r[4].is_empty(), "{c}: meaning");
    }
}

#[test]
fn r7_report_fields_match_the_serialized_report() {
    let mut report = Report::new(VerificationLevel::Structural);
    report.skipped.push(SkippedItem {
        item: "i".into(),
        reason: "r".into(),
    });
    report.findings.push(Finding {
        code: ErrorCode::IoError,
        severity: Severity::Info,
        message: Some("m".into()),
        expected: Some("e".into()),
        observed: Some("o".into()),
        affected: Some(SeqRange { first: 0, last: 1 }),
    });
    let text = serde_json::to_string(&report).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys = |v: &serde_json::Value| -> BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    };

    let doc = repo_file(R7);
    let lines = section(&doc, "7. Report fields");
    let top_end = lines
        .iter()
        .position(|l| l.starts_with("Nested objects"))
        .unwrap();
    let top = backticked(&lines[..top_end])
        .into_iter()
        .filter(|w| !w.contains('.') && !w.contains('/') && !w.contains("::"))
        .collect::<Vec<_>>();
    assert_eq!(top.iter().cloned().collect::<BTreeSet<_>>(), keys(&value));
    // In serialization order: each key's first `"key":` comes after the last.
    let mut pos = 0;
    for k in &top {
        let at = text
            .find(&format!("\"{k}\":"))
            .unwrap_or_else(|| panic!("{k} not serialized"));
        assert!(at >= pos, "{k} is out of order");
        pos = at;
    }

    let nested: BTreeMap<String, BTreeSet<String>> = table(&lines[top_end..], 0)
        .into_iter()
        .map(|r| {
            let fields = r[1]
                .split(" (")
                .next()
                .unwrap()
                .split(", ")
                .map(str::to_owned)
                .collect();
            (r[0].clone(), fields)
        })
        .collect();
    let at = |path: &str| -> serde_json::Value {
        let mut v = value.clone();
        for part in path.split('.') {
            v = match part.strip_suffix("[]") {
                Some(name) => v[name][0].clone(),
                None => v[part].clone(),
            };
        }
        v
    };
    assert_eq!(nested.len(), 7);
    for (path, fields) in &nested {
        assert_eq!(*fields, keys(&at(path)), "{path}");
    }

    // The user-facing field reference names the same top-level fields.
    let schema_doc = repo_file("docs/report-schema-v1.md");
    let listed: BTreeSet<String> = table(&section(&schema_doc, "Fields"), 0)
        .into_iter()
        .flat_map(|r| r[0].split(", ").map(str::to_owned).collect::<Vec<_>>())
        .collect();
    assert_eq!(listed, keys(&value), "docs/report-schema-v1.md");
}

// ---- R6: erasure coding -------------------------------------------------------------

const R6: &str = "docs/ratification/R6-erasure-coding-draft.md";

#[test]
fn r6_suite_and_layout_constants_match_the_code() {
    use mochi_format::parity;
    let doc = repo_file(R6);
    let suite = table(&section(&doc, "1. Suite 1"), 0);
    let value = |name: &str| -> String {
        suite
            .iter()
            .find(|r| r[0] == name)
            .unwrap_or_else(|| panic!("R6 §1 lacks {name}"))[1]
            .clone()
    };
    assert_eq!(
        value("Suite identifier"),
        parity::SUITE_CAUCHY_GF256.to_string()
    );
    assert!(
        value("Field").contains(&format!("{:#X}", parity::FIELD_POLYNOMIAL).replace("0X", "0x"))
    );
    assert_eq!(
        value("Data shards per group, k"),
        format!("1 to {}", parity::MAX_DATA_SHARDS)
    );
    // The coefficient formula, at its corners.
    assert!(value("Coefficient").contains("1 / (p ⊕ (128 + j))"));
    for (p, j) in [(0u64, 0u64), (127, 127), (5, 77)] {
        let x = (p as u8) ^ ((128 + j) as u8);
        assert_eq!(
            parity::gf_mul(parity::coefficient(p, j).unwrap(), x),
            1,
            "C[{p}][{j}] is the inverse of p ⊕ (128 + j)"
        );
    }

    let rows = table(&section(&doc, "2. Layout rule"), 0);
    let code: BTreeMap<&str, u64> = [
        ("TARGET_SHARDS", parity::TARGET_SHARDS),
        ("SHARD_ALIGN", parity::SHARD_ALIGN),
        ("MIN_SHARD", parity::MIN_SHARD),
        ("MAX_SHARD", parity::MAX_SHARD),
        ("MAX_DATA_SHARDS", parity::MAX_DATA_SHARDS),
        ("MIN_PERCENT", u64::from(parity::MIN_PERCENT)),
        ("MAX_PERCENT", u64::from(parity::MAX_PERCENT)),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows.len(), code.len());
    for r in &rows {
        assert_eq!(r[1], code[r[0].as_str()].to_string(), "{}", r[0]);
    }
}

#[test]
fn r6_frame_layout_matches_an_encoded_frame() {
    use mochi_format::parity::{self, Layout, ParityFrameInfo};
    let doc = repo_file(R6);
    let rows = table(&section(&doc, "4. Parity frame"), 0);
    let layout = Layout::compute(0x0102, 1000, 50).unwrap();
    let info = ParityFrameInfo {
        layout,
        archive_id: [0xA1; 32],
        seq: 3,
        transaction_id: [0xB2; 16],
        required_features: Vec::new(),
    };
    let k = layout.data_shards as usize;
    let s = layout.shard_size as usize;
    let hashes = vec![[0xC3; 32]; k];
    let shard = vec![0xD4; s];
    let frame = parity::encode_frame(
        &info,
        &[],
        0,
        0,
        &hashes,
        &shard,
        &mochi_format::Limits::default(),
    )
    .unwrap();
    let body = &frame[registry::SKIPPABLE_HEADER_LEN + envelope::BINARY_ENVELOPE_FIXED_LEN..];
    let mut at = 0;
    for r in &rows {
        let size = match r[1].as_str() {
            "32·k" => 32 * k,
            "s" => s,
            n => n.parse().unwrap(),
        };
        let offset = match r[0].as_str() {
            "56 + 32·k" => 56 + 32 * k,
            "56 + 32·k + s" => 56 + 32 * k + s,
            n => n.parse().unwrap(),
        };
        assert_eq!(offset, at, "{}: fields are contiguous", r[2]);
        let expected: Vec<u8> = match r[2].as_str() {
            "Signature" => registry::PARITY_SIGNATURE.to_vec(),
            "Suite" => parity::SUITE_CAUCHY_GF256.to_le_bytes().to_vec(),
            "Parity percentage" => 50u16.to_le_bytes().to_vec(),
            "Shard size" => (s as u32).to_le_bytes().to_vec(),
            "Range offset" => 0x0102u64.to_le_bytes().to_vec(),
            "Range length" => 1000u64.to_le_bytes().to_vec(),
            "Group count" => (layout.groups as u32).to_le_bytes().to_vec(),
            "Data shards per group" => (k as u16).to_le_bytes().to_vec(),
            "Parity shards per group" => (layout.parity_shards as u16).to_le_bytes().to_vec(),
            "Group index" | "Parity index" => 0u32.to_le_bytes().to_vec(),
            "Data-shard hashes" => hashes.concat(),
            "Parity shard" => shard.clone(),
            "Frame digest" => {
                let end = frame.len() - 32;
                mochi_format::digest::parity_frame_digest(
                    mochi_format::digest::StoredObjectBytes::new(&frame[..end]),
                )
                .as_bytes()
                .to_vec()
            }
            other => panic!("unknown field {other}"),
        };
        assert_eq!(body[at..at + size], expected[..], "{}", r[2]);
        at += size;
    }
    assert_eq!(at, body.len(), "the table covers the whole payload");
    assert_eq!(frame.len() as u64, layout.frame_len(0));
}

#[test]
fn r6_byte_strings_match_the_code() {
    let doc = repo_file(R6);
    let rows = table(&section(&doc, "4. Parity frame"), 1);
    let code: BTreeMap<&str, &[u8]> = [
        ("PARITY_SIGNATURE", &registry::PARITY_SIGNATURE[..]),
        (
            "PARITY_SHARD_DOMAIN",
            mochi_format::digest::PARITY_SHARD_DOMAIN,
        ),
        (
            "PARITY_FRAME_DOMAIN",
            mochi_format::digest::PARITY_FRAME_DOMAIN,
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows.len(), code.len());
    for r in &rows {
        assert_eq!(hex(&r[1]), code[r[0].as_str()], "{}", r[0]);
    }
}

// ---- R3: canonical serialization and domain separators ---------------------------------

const R3: &str = "docs/ratification/R3-canonical-serialization-draft.md";

/// Hex bytes as R3 writes them: space-separated, `XX×N` repeating a byte.
fn hex_rep(cell: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for tok in cell.split_whitespace() {
        match tok.split_once('×') {
            Some((b, n)) => {
                let byte = hex(b);
                assert_eq!(byte.len(), 1, "{tok:?}");
                out.extend(std::iter::repeat_n(byte[0], n.parse::<usize>().unwrap()));
            }
            None => out.extend(hex(tok)),
        }
    }
    out
}

/// Every `CborFault`. The match makes a new fault a compile error here.
fn all_cbor_faults() -> Vec<mochi_format::cbor::CborFault> {
    use mochi_format::cbor::CborFault::*;
    let all = [
        Truncated,
        NotShortest,
        ReservedAdditionalInfo,
        Indefinite,
        Tag,
        Float,
        SimpleValue,
        NonIntegerKey,
        KeyOrder,
        InvalidUtf8,
        LengthExceedsInput,
        TooDeep,
        TooManyItems,
        TrailingBytes,
        NotCanonical,
    ];
    for f in all {
        match f {
            Truncated
            | NotShortest
            | ReservedAdditionalInfo
            | Indefinite
            | Tag
            | Float
            | SimpleValue
            | NonIntegerKey
            | KeyOrder
            | InvalidUtf8
            | LengthExceedsInput
            | TooDeep
            | TooManyItems
            | TrailingBytes
            | NotCanonical => {}
        }
    }
    all.to_vec()
}

#[test]
fn r3_cbor_subset_table_matches_the_codec() {
    use mochi_format::cbor::{self, CborLimits};
    use mochi_format::FormatError;
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "1.1 Constructs"), 0);
    let mut faults = BTreeSet::new();
    for r in &rows {
        let input = hex_rep(&r[1]);
        let mut limits = CborLimits::default();
        if let Some(n) = r[2].strip_prefix("items=") {
            limits.max_items = n.parse().unwrap();
        } else {
            assert_eq!(r[2], "default", "{}", r[0]);
        }
        let got = cbor::decode(&input, &limits);
        match r[3].as_str() {
            "accepted" => {
                assert!(got.is_ok(), "{}: {got:?}", r[0]);
                assert_eq!(r[4], "—", "{}", r[0]);
                // Accepted input is canonical: it re-encodes to itself.
                assert_eq!(cbor::encode(&got.unwrap()).unwrap(), input, "{}", r[0]);
            }
            "refused" => {
                let Err(FormatError::Cbor(f)) = got else {
                    panic!("{}: {got:?}", r[0])
                };
                assert_eq!(format!("{f:?}"), r[4], "{}", r[0]);
                faults.insert(r[4].clone());
            }
            other => panic!("{}: verdict {other:?}", r[0]),
        }
    }
    // Every fault but the last-resort guard has an example.
    let expected: BTreeSet<String> = all_cbor_faults()
        .iter()
        .map(|f| format!("{f:?}"))
        .filter(|f| f != "NotCanonical")
        .collect();
    assert_eq!(faults, expected);
}

#[test]
fn r3_codec_fault_codes_match_the_code() {
    use mochi_format::FormatError;
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "1.2 Codec faults"), 0);
    let faults = all_cbor_faults();
    assert_eq!(rows.len(), faults.len());
    for (r, f) in rows.iter().zip(&faults) {
        assert_eq!(r[0], format!("{f:?}"), "R3 lists the faults in code order");
        let code = mochi_core::MochiError::from(FormatError::Cbor(*f)).code;
        assert_eq!(r[1], code.as_str(), "{f:?}");
        assert!(!r[2].is_empty());
    }
}

#[test]
fn r3_writer_refusals_match_the_encoder() {
    use mochi_format::cbor::{self, CborLimits, Value};
    let nested = |depth: usize| {
        let mut v = Value::Uint(0);
        for _ in 0..depth {
            v = Value::Array(vec![v]);
        }
        v
    };
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "1.3 Writer refusals"), 0);
    assert_eq!(rows.len(), 5);
    let two = CborLimits {
        max_items: 2,
        ..CborLimits::default()
    };
    for r in &rows {
        let (name, result): (&str, Result<Vec<u8>, mochi_format::FormatError>) = match r[0].as_str()
        {
            "a map with keys 1 then 0" => (
                "encode",
                cbor::encode(&Value::Map(vec![(1, Value::Null), (0, Value::Null)])),
            ),
            "65 nested arrays around the integer 0" => (
                "encode_within",
                cbor::encode_within(&nested(65), &CborLimits::default()),
            ),
            "64 nested arrays around the integer 0" => (
                "encode_within",
                cbor::encode_within(&nested(64), &CborLimits::default()),
            ),
            "an array of 2 under items=2" => (
                "encode_within",
                cbor::encode_within(&Value::Array(vec![Value::Null, Value::Null]), &two),
            ),
            "an array of 1 under items=2" => (
                "encode_within",
                cbor::encode_within(&Value::Array(vec![Value::Null]), &two),
            ),
            other => panic!("unknown writer case {other:?}"),
        };
        match result {
            Ok(_) => assert!(r[1].starts_with(&format!("{name} succeeds")), "{}", r[0]),
            Err(e) => {
                let code = mochi_core::MochiError::from(e).code;
                assert!(
                    r[1].starts_with(&format!("{name} fails with {code}")),
                    "{}: {}",
                    r[0],
                    r[1]
                );
            }
        }
    }
}

#[test]
fn r3_cbor_limits_match_the_code() {
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "1.4 Limits"), 0);
    let l = mochi_format::cbor::CborLimits::default();
    let first_number = |cell: &str| -> usize {
        cell.split_whitespace()
            .next()
            .unwrap()
            .replace(',', "")
            .parse()
            .unwrap()
    };
    let by: BTreeMap<&str, usize> = rows
        .iter()
        .map(|r| (r[0].as_str(), first_number(&r[1])))
        .collect();
    assert_eq!(by["Depth"], l.max_depth);
    assert_eq!(by["Items"], l.max_items);
    assert_eq!(by.len(), 2);
}

/// `name` → `CODE` for every `rejects` line of the vector lists in `dir`.
fn vector_codes(dir: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for file in ["vectors.txt", "descriptor-vectors.txt"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/golden")
            .join(dir)
            .join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 && f[1] == "rejects" {
                out.insert(f[0].to_owned(), f[2].to_owned());
            }
        }
    }
    out
}

fn golden_exists(rel: &str) -> bool {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(rel)
        .is_file()
}

#[test]
fn r3_record_level_causes_match_the_vector_lists() {
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "2.2 Record-level causes"), 0);
    assert!(rows.len() >= 25);
    for r in &rows {
        let v = r[2].as_str();
        let (dir, file) = v.split_once('/').unwrap();
        let name = file.strip_suffix(".bin").unwrap();
        assert!(golden_exists(v), "{v} does not exist");
        let codes = vector_codes(dir);
        assert_eq!(
            codes.get(name).map(String::as_str),
            Some(r[1].as_str()),
            "{}: {v}",
            r[0]
        );
    }
}

#[test]
fn r3_schema_registry_matches_the_code() {
    use mochi_core::{commit, descriptor, keys, manifest, quarantine};
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "3. Schema registry"), 0);
    assert_eq!(rows.len(), 10);

    // Versions read per record, from the code.
    let current: BTreeMap<&str, Vec<u64>> = BTreeMap::from([
        ("Archive descriptor", vec![descriptor::SCHEMA_VERSION]),
        (
            "Commit record",
            vec![commit::SCHEMA_VERSION, commit::ENCRYPTED_SCHEMA_VERSION],
        ),
        (
            "Recovery manifest",
            vec![
                manifest::SCHEMA_VERSION,
                manifest::RETENTION_SCHEMA_VERSION,
                manifest::ENCRYPTED_SCHEMA_VERSION,
            ],
        ),
        ("Key envelope", vec![keys::ENVELOPE_SCHEMA_VERSION]),
        (
            "Tail-quarantine sidecar metadata",
            vec![quarantine::SIDECAR_SCHEMA_VERSION],
        ),
    ]);
    let legacy: BTreeMap<&str, u64> = BTreeMap::from([
        ("Commit record", commit::LEGACY_SCHEMA_VERSION),
        ("Recovery manifest", manifest::LEGACY_SCHEMA_VERSION),
    ]);

    let mut seen_current: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut files = BTreeSet::new();
    for r in &rows {
        let (record, version) = (r[0].as_str(), r[1].parse::<u64>().unwrap());
        let file = r[3].as_str();
        assert!(files.insert(file.to_owned()), "{file} listed twice");
        let schema = repo_file(&format!("docs/schemas/{file}"));
        assert!(
            schema
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("0: {version},"))),
            "{file} never draws `0: {version}`"
        );
        assert!(
            file.ends_with(&format!("-v{version}.cddl")),
            "{file} is not version {version}"
        );
        if r[2] != "none (a tool file)" {
            let kind = kind_of(&r[2]);
            let expected = match record {
                "Archive descriptor" => FrameKind::ArchiveDescriptor,
                "Commit record" => FrameKind::CommitRecord,
                "Recovery manifest" => FrameKind::RecoveryManifest,
                "Key envelope" => FrameKind::KeyEnvelope,
                other => panic!("{other}"),
            };
            assert_eq!(kind, expected, "{record}");
        }
        if r[5] == "legacy" {
            assert_eq!(legacy[record], version, "{record}: legacy version");
            assert!(
                r[4].starts_with("never"),
                "{record}: a legacy schema is never written"
            );
            assert!(schema.contains("LEGACY (refused)"), "{file}");
        } else {
            assert!(
                r[5].starts_with("draft") || r[5].starts_with("frozen"),
                "{record}: status {:?}",
                r[5]
            );
            seen_current
                .entry(record.to_owned())
                .or_default()
                .push(version);
            assert!(!schema.contains("LEGACY (refused)"), "{file}");
        }
    }
    for (record, versions) in &current {
        assert_eq!(&seen_current[*record], versions, "{record}: versions read");
    }
    assert_eq!(seen_current.len(), current.len());

    // The table lists every CDDL schema file, and nothing else (R7's JSON
    // Schemas in `docs/schemas/json/` are checked by r7_json_schemas.rs),
    // except the files Annex B added after ratification.
    let on_disk: BTreeSet<String> =
        std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".cddl"))
            .filter(|n| !since_ratification::CDDL_FILES.contains(&n.as_str()))
            .collect();
    assert_eq!(files, on_disk);
}

/// The first valid payload of a golden `.bin` vector (one skippable frame).
fn golden_payload(rel: &str) -> Vec<u8> {
    let bytes = std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/golden")
            .join(rel),
    )
    .unwrap();
    let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    bytes[8..8 + len].to_vec()
}

/// The domain separator R3 §5 gives for a constant name.
fn r3_domain(constant: &str) -> Vec<u8> {
    let doc = repo_file(R3);
    let lines = section(&doc, "5. Domain separators");
    for n in 0..3 {
        for r in table(&lines, n) {
            if r[0] == constant {
                return bytes_expr(&r[1]);
            }
        }
    }
    panic!("R3 §5 lacks {constant}");
}

#[test]
fn r3_commit_id_construction_reproduces_the_vectors() {
    use mochi_format::cbor::{self, CborLimits, Value};
    let doc = repo_file(R3);
    let rows = table(&section(&doc, "4. Commit ID"), 0);
    assert_eq!(rows.len(), 3);
    let domain = r3_domain("COMMIT_ID_DOMAIN");

    // The construction, as the document states it, over a commit payload.
    let recompute = |payload: &[u8]| -> (Vec<u8>, Vec<u8>) {
        let Value::Map(entries) = cbor::decode(payload, &CborLimits::default()).unwrap() else {
            panic!("a commit is a map")
        };
        let stored = match entries.iter().find(|(k, _)| *k == 9) {
            Some((_, Value::Bytes(b))) => b.clone(),
            other => panic!("key 9: {other:?}"),
        };
        let body: Vec<(u64, Value)> = entries.into_iter().filter(|(k, _)| *k != 9).collect();
        let body = cbor::encode(&Value::Map(body)).unwrap();
        let mut h = blake3::Hasher::new();
        h.update(&domain);
        h.update(&body);
        (h.finalize().as_bytes().to_vec(), stored)
    };

    let listed = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c5/vectors.txt"),
    )
    .unwrap();
    for r in &rows {
        let rel = r[0].as_str();
        let (id, stored) = recompute(&golden_payload(rel));
        assert_eq!(hex_string(&id), r[1], "{rel}: recomputed");
        assert_eq!(id, stored, "{rel}: key 9");
        let name = rel
            .strip_prefix("c5/")
            .unwrap()
            .strip_suffix(".bin")
            .unwrap();
        assert!(
            listed
                .lines()
                .any(|l| l == format!("{name} valid {}", r[1])),
            "{name}: c5/vectors.txt disagrees"
        );
    }

    // Every commit in the golden archives, schema 1 and schema 2 alike.
    let mut checked = 0;
    for (dir, file) in [
        ("c5", "valid-archive-3-commits.mochi"),
        ("c11", "valid-archive-encrypted.mochi"),
        ("c12", "valid-archive-recovery-record.mochi"),
    ] {
        let bytes = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/golden")
                .join(dir)
                .join(file),
        )
        .unwrap();
        let mut off = 0u64;
        while off < bytes.len() as u64 {
            let span = mochi_format::frame::walk_frame(
                bytes.as_slice(),
                off,
                &mochi_format::Limits::default(),
            )
            .unwrap();
            if span.magic == registry::COMMIT_RECORD {
                let at = off as usize + registry::SKIPPABLE_HEADER_LEN;
                let (id, stored) = recompute(&bytes[at..off as usize + span.len as usize]);
                assert_eq!(id, stored, "{dir}/{file} at {off}");
                checked += 1;
            }
            off = span.end();
        }
    }
    assert!(checked >= 9, "{checked} commits");
}

fn hex_string(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn r3_domain_separators_match_the_code() {
    use mochi_format::digest::{self, Digest};
    let doc = repo_file(R3);
    let lines = section(&doc, "5. Domain separators");

    // BLAKE3 scopes.
    let code: BTreeMap<&str, (&[u8], &str)> = BTreeMap::from([
        (
            "FOOTER_DOMAIN",
            (
                digest::FOOTER_DOMAIN,
                Digest::<digest::Footer>::scope_name(),
            ),
        ),
        (
            "CHUNK_CONTENT_DOMAIN",
            (
                digest::CHUNK_CONTENT_DOMAIN,
                Digest::<digest::ChunkContent>::scope_name(),
            ),
        ),
        (
            "STORED_OBJECT_DOMAIN",
            (
                digest::STORED_OBJECT_DOMAIN,
                Digest::<digest::StoredObjectScope>::scope_name(),
            ),
        ),
        (
            "DICTIONARY_DOMAIN",
            (
                digest::DICTIONARY_DOMAIN,
                Digest::<digest::Dictionary>::scope_name(),
            ),
        ),
        (
            "COMMIT_ID_DOMAIN",
            (
                digest::COMMIT_ID_DOMAIN,
                Digest::<digest::CommitIdScope>::scope_name(),
            ),
        ),
        (
            "PARITY_SHARD_DOMAIN",
            (
                digest::PARITY_SHARD_DOMAIN,
                Digest::<digest::ParityShard>::scope_name(),
            ),
        ),
        (
            "PARITY_FRAME_DOMAIN",
            (
                digest::PARITY_FRAME_DOMAIN,
                Digest::<digest::ParityFrame>::scope_name(),
            ),
        ),
    ]);
    let rows = table(&lines, 0);
    assert_eq!(rows.len(), code.len());
    let mut listed: BTreeSet<Vec<u8>> = BTreeSet::new();
    for r in &rows {
        let (bytes, scope) = code[r[0].as_str()];
        assert_eq!(bytes_expr(&r[1]), bytes, "{}", r[0]);
        assert_eq!(
            r[2].parse::<usize>().unwrap(),
            bytes.len(),
            "{}: length",
            r[0]
        );
        assert_eq!(r[3], scope, "{}: scope name", r[0]);
        listed.insert(bytes.to_vec());
    }
    let all: BTreeSet<Vec<u8>> = digest::ALL_DOMAINS.iter().map(|d| d.to_vec()).collect();
    assert_eq!(listed, all, "R3 lists exactly ALL_DOMAINS");

    // AEAD prefixes.
    let aead: BTreeMap<&str, &[u8]> = BTreeMap::from([
        ("KEY_WRAP_DOMAIN", digest::KEY_WRAP_DOMAIN),
        ("OBJECT_SEAL_DOMAIN", digest::OBJECT_SEAL_DOMAIN),
    ]);
    let rows = table(&lines, 1);
    assert_eq!(rows.len(), aead.len());
    for r in &rows {
        let bytes = aead[r[0].as_str()];
        assert_eq!(bytes_expr(&r[1]), bytes, "{}", r[0]);
        assert_eq!(
            r[2].parse::<usize>().unwrap(),
            bytes.len(),
            "{}: length",
            r[0]
        );
    }
    assert_eq!(digest::AEAD_DOMAINS.len(), aead.len());

    // The two unseparated scopes.
    let rows = table(&lines, 2);
    assert_eq!(rows.len(), 2);
    for r in &rows {
        let (bytes, scope) = match r[0].as_str() {
            "FILE_CONTENT_DOMAIN" => (
                digest::FILE_CONTENT_DOMAIN,
                Digest::<digest::FileContent>::scope_name(),
            ),
            "TAIL_QUARANTINE_DOMAIN" => (
                digest::TAIL_QUARANTINE_DOMAIN,
                Digest::<digest::TailQuarantine>::scope_name(),
            ),
            other => panic!("{other}"),
        };
        assert!(bytes.is_empty(), "{}: unseparated", r[0]);
        assert!(bytes_expr(&r[1]).is_empty(), "{}", r[0]);
        assert_eq!(r[2], scope);
    }

    // Prefix-freeness: one zero byte, and it is the last.
    let every: Vec<&[u8]> = digest::ALL_DOMAINS
        .iter()
        .chain(digest::AEAD_DOMAINS)
        .copied()
        .collect();
    for d in &every {
        assert_eq!(d.iter().filter(|b| **b == 0).count(), 1, "{d:?}");
        assert_eq!(d.last(), Some(&0), "{d:?}");
    }
    for a in &every {
        for b in &every {
            assert!(a == b || !b.starts_with(a), "{a:?} is a prefix of {b:?}");
        }
    }
}

#[test]
fn r3_separators_reproduce_the_c2_digest_vectors() {
    let doc = repo_file(R3);
    let lines = section(&doc, "5. Domain separators");
    // scope name → separator, from the document alone.
    let mut by_scope: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for n in [0, 2] {
        for r in table(&lines, n) {
            by_scope.insert(r[3 - usize::from(n == 2)].clone(), bytes_expr(&r[1]));
        }
    }
    assert!(by_scope["file-content"].is_empty());
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c2/vectors.txt"),
    )
    .unwrap();
    let mut checked = BTreeSet::new();
    for line in text.lines().filter(|l| l.starts_with("digest ")) {
        let f: Vec<&str> = line.split_whitespace().collect();
        let input = if f[2] == "-" { Vec::new() } else { hex(f[2]) };
        let mut h = blake3::Hasher::new();
        h.update(&by_scope[f[1]]);
        h.update(&input);
        assert_eq!(hex_string(h.finalize().as_bytes()), f[3], "{line}");
        checked.insert(f[1].to_owned());
    }
    let expected: BTreeSet<String> = [
        "file-content",
        "chunk-content",
        "stored-object",
        "dictionary",
        "commit-id",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    assert_eq!(checked, expected);
}

#[test]
fn r3_schema_selection_matches_the_code() {
    use mochi_core::commit::CommitRecord;
    use mochi_core::manifest::{Manifest, Provenance, RewriteReason};
    use mochi_core::object::ArchiveId;
    use mochi_core::retention::RetentionOp;
    use mochi_format::cbor::CborLimits;
    use mochi_format::Limits;

    let (l, c) = (Limits::default(), CborLimits::default());
    let commit = || {
        CommitRecord::decode(&golden_payload("c5/valid-commit-root.bin"), &l, &c)
            .unwrap()
            .0
    };
    let delta =
        || Manifest::decode(&golden_payload("c4/valid-manifest-root-delta.bin"), &l, &c).unwrap();
    let snapshot =
        || Manifest::decode(&golden_payload("c4/valid-manifest-snapshot.bin"), &l, &c).unwrap();
    let provenance = || {
        Some(Provenance {
            source_archive_id: ArchiveId::from_bytes([1; 32]),
            commits: vec![(0, mochi_format::digest::CommitId::from_bytes([2; 32]))],
            collected: vec![],
            reason: RewriteReason::Collection,
        })
    };

    let doc = repo_file(R3);
    let rows = table(&section(&doc, "6. One logical structure"), 0);
    assert_eq!(rows.len(), 9);
    for r in &rows {
        let want: u64 = r[2].parse().unwrap();
        let got = match (r[0].as_str(), r[1].as_str()) {
            ("Commit record", "the commit lists no required feature") => commit().schema_version(),
            ("Commit record", "the commit lists required feature 1 (Encrypted)") => {
                let mut x = commit();
                x.required_features = vec![1];
                x.schema_version()
            }
            (
                "Recovery manifest",
                "no retention operation, an empty retention state, no provenance, not Encrypted",
            ) => {
                assert_eq!(snapshot().schema_version(), delta().schema_version());
                delta().schema_version()
            }
            ("Recovery manifest", "a delta with at least one retention operation") => {
                let mut x = delta();
                x.retention_ops.push(RetentionOp::Expire { seq: 0 });
                x.schema_version()
            }
            ("Recovery manifest", "a snapshot with a non-empty retention state") => {
                let mut x = snapshot();
                x.retention.expired.insert(0);
                x.schema_version()
            }
            ("Recovery manifest", "provenance, whether or not retention data") => {
                let mut x = delta();
                x.provenance = provenance();
                let mut y = x.clone();
                y.retention_ops.push(RetentionOp::Expire { seq: 0 });
                assert_eq!(x.schema_version(), y.schema_version());
                x.schema_version()
            }
            (
                "Recovery manifest",
                "lists required feature 1 (Encrypted), whatever else it holds",
            ) => {
                let mut x = delta();
                x.required_features = vec![1];
                let mut y = x.clone();
                y.retention_ops.push(RetentionOp::Expire { seq: 0 });
                y.provenance = provenance();
                assert_eq!(x.schema_version(), y.schema_version());
                x.schema_version()
            }
            ("Archive descriptor", "always") => mochi_core::descriptor::SCHEMA_VERSION,
            ("Key envelope", "always") => mochi_core::keys::ENVELOPE_SCHEMA_VERSION,
            other => panic!("unknown condition {other:?}"),
        };
        assert_eq!(got, want, "{} / {}", r[0], r[1]);
        if r[3] != "—" {
            assert!(r[3].contains("/reject-"), "{}", r[3]);
            assert!(golden_exists(&r[3]), "{} does not exist", r[3]);
        }
    }
}

#[test]
fn r3_vector_index_names_only_files_that_exist() {
    let doc = repo_file(R3);
    let mut named = BTreeSet::new();
    for needle in ["7.1 Rules the code checks", "7.2 Vector index"] {
        for line in section(&doc, needle) {
            for token in backticked(&[line]) {
                if token.contains('/') && (token.ends_with(".bin") || token.ends_with(".mochi")) {
                    named.insert(token);
                }
            }
        }
    }
    assert!(named.len() >= 80, "{} files named", named.len());
    for rel in &named {
        assert!(golden_exists(rel), "R3 names {rel}, which does not exist");
    }
    // The schemas the index covers are the schema files.
    let rows = table(&section(&doc, "7.2 Vector index"), 0);
    let mut schemas = BTreeSet::new();
    for r in &rows {
        schemas.insert(r[0].clone());
        let rejects_are_rejects = backticked(&[r[3].as_str()])
            .into_iter()
            .filter(|t| t.contains('/'))
            .all(|t| t.contains("/reject-"));
        assert!(
            rejects_are_rejects,
            "{}: a Reject cell names a valid file",
            r[1]
        );
        let valids_are_valid = backticked(&[r[2].as_str()])
            .into_iter()
            .all(|t| t.contains("/valid-"));
        assert!(
            valids_are_valid,
            "{}: a Valid cell names a reject file",
            r[1]
        );
    }
    for s in &schemas {
        assert!(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../docs/schemas/{s}.cddl"))
                .is_file(),
            "{s}"
        );
    }
    assert_eq!(schemas.len(), 8, "{schemas:?}");
}

#[test]
fn r3_sidecar_layout_and_keys_match_the_code() {
    use mochi_core::quarantine::{sidecar_name, SidecarMetadata, MAX_METADATA_LEN};
    use mochi_format::cbor::{self, CborLimits, Value};
    let doc = repo_file(R3);
    let lines = section(&doc, "Appendix A");

    let hash = mochi_format::digest::Digest::from_bytes([0x7A; 32]);
    let meta = SidecarMetadata {
        archive_id: mochi_core::object::ArchiveId::from_bytes([0xA1; 32]),
        head_commit_id: mochi_format::digest::CommitId::from_bytes([0xC2; 32]),
        head_seq: 7,
        tail_offset: 4096,
        tail_len: 100,
        tail_hash: hash,
        frame_magics: vec![registry::COMMIT_RECORD],
        incomplete_final_frame: false,
        tool: "mochi 0.1.0".into(),
        time: "2026-10-12T01:02:03.123456789Z".into(),
    };
    let header = meta.header().unwrap();
    let rows = table(&lines, 0);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0][0], "0");
    assert_eq!(
        bytes_expr(&rows[0][3].replace("(R1 §4)", "")),
        registry::TAIL_QUARANTINE_MAGIC
    );
    assert_eq!(&header[..8], &registry::TAIL_QUARANTINE_MAGIC);
    assert_eq!((rows[1][0].as_str(), rows[1][1].as_str()), ("8", "4"));
    let l = u32::from_le_bytes(header[8..12].try_into().unwrap());
    assert_eq!(header.len(), 12 + l as usize, "metadata follows the length");
    assert_eq!(rows[2][0], "12");
    assert!(rows[1][3].contains(&format!("{MAX_METADATA_LEN}")));
    assert!((1..=MAX_METADATA_LEN).contains(&l));

    // The name.
    let example = "a.mochi.tail-4096-7a7a7a7a7a7a7a7a.mochiq";
    assert!(doc.contains(&format!("`{example}`")));
    assert_eq!(sidecar_name("a.mochi", 4096, &hash), example);

    // The key set.
    let keys: BTreeSet<u64> = table(&lines, 1)
        .iter()
        .map(|r| r[0].parse().unwrap())
        .collect();
    let Value::Map(entries) = cbor::decode(&header[12..], &CborLimits::default()).unwrap() else {
        panic!("a map")
    };
    assert_eq!(
        keys,
        entries.iter().map(|(k, _)| *k).collect::<BTreeSet<_>>()
    );
}
