//! The G1 evidence audit (plan W16, R2 decision 4): a valid or reject vector
//! for every position and rule R3 section 7.2 marked as a gap, for the
//! CBOR encoding (the binary envelope's obligations already have a vector
//! each in `b2/`). Vectors are edits of the valid golden records, written to
//! `fixtures/golden/r8/g1/`; each carries the verdict the production decoder
//! must give, and each schema-level one must also fail the CDDL schema.

use std::path::PathBuf;

use mochi_format::cbor::{self, CborLimits, Value};
use mochi_format::digest::commit_id;
use mochi_format::frame::encode_skippable_frame;
use mochi_format::registry::FrameKind;
use mochi_format::repr::CanonicalCommitBody;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum K {
    Descriptor,
    Commit,
    Manifest,
    KeyEnvelope,
}

impl K {
    fn frame(self) -> FrameKind {
        match self {
            K::Descriptor => FrameKind::ArchiveDescriptor,
            K::Commit => FrameKind::CommitRecord,
            K::Manifest => FrameKind::RecoveryManifest,
            K::KeyEnvelope => FrameKind::KeyEnvelope,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Valid,
    /// The CDDL schema rejects it.
    Schema,
    /// The schema accepts it; a rule CDDL cannot state refuses it.
    Semantic,
}

pub struct V {
    pub name: &'static str,
    pub kind: K,
    /// The schema file the CDDL check uses.
    pub schema: &'static str,
    pub class: Class,
    /// The error code the production decoder gives (rejects only).
    pub code: &'static str,
    pub why: &'static str,
    pub bytes: Vec<u8>,
}

fn golden(p: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden")
        .join(p)
}

fn payload(file: &str) -> Vec<u8> {
    let b = std::fs::read(golden(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    b[8..].to_vec()
}

fn tree(p: &[u8]) -> Value {
    cbor::decode(p, &CborLimits::default()).expect("a valid golden record")
}

/// The value at `path`: a map key or an array index, whichever the node is.
fn at<'a>(v: &'a mut Value, path: &[u64]) -> &'a mut Value {
    let mut cur = v;
    for p in path {
        cur = match cur {
            Value::Map(m) => &mut m.iter_mut().find(|(k, _)| k == p).expect("key").1,
            Value::Array(a) => &mut a[*p as usize],
            _ => panic!("not a container"),
        };
    }
    cur
}

fn set(v: &mut Value, key: u64, x: Value) {
    let Value::Map(m) = v else { panic!("map") };
    match m.iter_mut().find(|(k, _)| *k == key) {
        Some(e) => e.1 = x,
        None => {
            m.push((key, x));
            m.sort_by_key(|(k, _)| *k);
        }
    }
}

fn refresh_commit_id(v: &mut Value) {
    let Value::Map(m) = v else { return };
    let body: Vec<(u64, Value)> = m.iter().filter(|(k, _)| *k != 9).cloned().collect();
    let bytes = cbor::encode(&Value::Map(body)).unwrap();
    let id = commit_id(CanonicalCommitBody::assume_canonical(&bytes));
    set(v, 9, Value::Bytes(id.as_bytes().to_vec()));
}

fn frame(kind: K, v: &Value) -> Vec<u8> {
    encode_skippable_frame(kind.frame(), &cbor::encode(v).unwrap()).unwrap()
}

fn b(n: usize, fill: u8) -> Value {
    Value::Bytes(vec![fill; n])
}

fn u(n: u64) -> Value {
    Value::Uint(n)
}

fn arr(v: Vec<Value>) -> Value {
    Value::Array(v)
}

fn map(v: Vec<(u64, Value)>) -> Value {
    Value::Map(v)
}

struct Spec {
    name: &'static str,
    kind: K,
    schema: &'static str,
    class: Class,
    code: &'static str,
    why: &'static str,
}

fn make(s: Spec, base: &Value, reid: bool, edit: impl FnOnce(&mut Value)) -> V {
    let mut t = base.clone();
    edit(&mut t);
    if reid {
        refresh_commit_id(&mut t);
    }
    V {
        name: s.name,
        kind: s.kind,
        schema: s.schema,
        class: s.class,
        code: s.code,
        why: s.why,
        bytes: frame(s.kind, &t),
    }
}

use Class::{Schema, Semantic, Valid};

macro_rules! v {
    ($name:expr, $kind:expr, $schema:expr, $class:expr, $code:expr, $why:expr) => {
        Spec {
            name: $name,
            kind: $kind,
            schema: $schema,
            class: $class,
            code: $code,
            why: $why,
        }
    };
}

/// The head commit record of the golden Encrypted archive.
fn encrypted_head_commit() -> Value {
    let bytes = std::fs::read(golden("c11/valid-archive-encrypted.mochi")).unwrap();
    let mut off = 0usize;
    let mut last = None;
    while off + 8 <= bytes.len() {
        let magic = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        if magic == mochi_format::registry::COMMIT_RECORD {
            last = Some(bytes[off + 8..off + 8 + len].to_vec());
        }
        off += 8 + len;
    }
    tree(&last.expect("a commit"))
}

pub fn vectors(v3_valid: Option<Vec<u8>>) -> Vec<V> {
    let mut out = Vec::new();

    // ---- descriptor ----------------------------------------------------------------
    let d = tree(&payload("b2/valid-descriptor-minimal.bin"));
    let ds = "archive-descriptor-v0";
    let dk = K::Descriptor;
    let red = |suite, pct| {
        map(vec![
            (0, Value::Bool(false)),
            (1, map(vec![(0, u(suite)), (1, u(pct))])),
        ])
    };
    out.push(make(
        v!(
            "reject-descriptor-feature-not-uint",
            dk,
            ds,
            Schema,
            "DESCRIPTOR_INVALID",
            "a required-feature entry that is not an unsigned integer"
        ),
        &d,
        false,
        |t| set(t, 4, arr(vec![Value::Text("x".into())])),
    ));
    out.push(make(
        v!(
            "reject-descriptor-redundancy-percent-0",
            dk,
            ds,
            Schema,
            "DESCRIPTOR_INVALID",
            "a recovery percentage of 0"
        ),
        &d,
        false,
        |t| set(t, 5, red(1, 0)),
    ));
    out.push(make(
        v!(
            "reject-descriptor-redundancy-percent-101",
            dk,
            ds,
            Schema,
            "DESCRIPTOR_INVALID",
            "a recovery percentage of 101"
        ),
        &d,
        false,
        |t| set(t, 5, red(1, 101)),
    ));
    out.push(make(
        v!(
            "reject-descriptor-redundancy-unknown-suite",
            dk,
            ds,
            Semantic,
            "UNSUPPORTED_FEATURE",
            "a redundancy suite no build defines"
        ),
        &d,
        false,
        |t| set(t, 5, red(2, 50)),
    ));
    out.push(make(
        v!(
            "reject-descriptor-encrypted-with-tar",
            dk,
            ds,
            Semantic,
            "DESCRIPTOR_INVALID",
            "the Encrypted feature combined with the TAR-compatible constraint"
        ),
        &d,
        false,
        |t| {
            set(t, 4, arr(vec![u(1)]));
            set(t, 5, map(vec![(0, Value::Bool(true))]));
        },
    ));
    out.push(make(
        v!(
            "valid-descriptor-redundancy",
            dk,
            ds,
            Valid,
            "",
            "a 50 % recovery record"
        ),
        &d,
        false,
        |t| set(t, 5, red(1, 50)),
    ));
    out.push(make(
        v!(
            "valid-descriptor-encrypted",
            dk,
            ds,
            Valid,
            "",
            "the Encrypted feature"
        ),
        &d,
        false,
        |t| set(t, 4, arr(vec![u(1)])),
    ));

    // ---- commit schema 1 ---------------------------------------------------------------
    let root = tree(&payload("c5/valid-commit-root.bin"));
    let child = tree(&payload("c5/valid-commit-child.bin"));
    let (cs, ck) = ("commit-record-v1", K::Commit);
    out.push(make(
        v!(
            "reject-commit-short-archive-id",
            ck,
            cs,
            Schema,
            "RECORD_INVALID",
            "a 31-byte archive ID"
        ),
        &root,
        true,
        |t| set(t, 1, b(31, 0x5a)),
    ));
    out.push(make(
        v!(
            "reject-commit-parent-short-commit-id",
            ck,
            cs,
            Schema,
            "RECORD_INVALID",
            "a parent whose commit ID is 31 bytes"
        ),
        &child,
        true,
        |t| *at(t, &[4, 0]) = b(31, 0x44),
    ));
    out.push(make(
        v!(
            "reject-commit-time-nanos",
            ck,
            cs,
            Schema,
            "RECORD_INVALID",
            "informational time with 10^9 nanoseconds"
        ),
        &root,
        true,
        |t| set(t, 8, arr(vec![u(0), u(1_000_000_000)])),
    ));
    out.push(make(
        v!(
            "reject-commit-object-ref-short-length",
            ck,
            cs,
            Schema,
            "RECORD_INVALID",
            "an object reference with stored length 7"
        ),
        &root,
        true,
        |t| *at(t, &[6, 1]) = u(7),
    ));
    out.push(make(
        v!(
            "reject-commit-object-ref-short-hash",
            ck,
            cs,
            Schema,
            "RECORD_INVALID",
            "an object reference with a 31-byte hash"
        ),
        &root,
        true,
        |t| *at(t, &[6, 2]) = b(31, 0xb4),
    ));
    out.push(make(
        v!(
            "valid-commit-time",
            ck,
            cs,
            Valid,
            "",
            "an informational time of 5 ns after 1700000000 s"
        ),
        &root,
        true,
        |t| set(t, 8, arr(vec![u(1_700_000_000), u(5)])),
    ));
    out.push(make(
        v!(
            "reject-commit-schema-1-lists-encrypted",
            ck,
            cs,
            Semantic,
            "RECORD_INVALID",
            "schema 1 listing the Encrypted feature"
        ),
        &root,
        true,
        |t| set(t, 7, arr(vec![u(1)])),
    ));

    // ---- commit schema 2 -----------------------------------------------------------------
    let e = encrypted_head_commit();
    let (es, ek) = ("commit-record-v2", K::Commit);
    out.push(make(
        v!(
            "valid-commit-v2-head",
            ek,
            es,
            Valid,
            "",
            "the head of the golden Encrypted archive"
        ),
        &e,
        true,
        |_| {},
    ));
    out.push(make(
        v!(
            "reject-commit-v2-no-envelopes",
            ek,
            es,
            Schema,
            "RECORD_INVALID",
            "an empty key-envelope list"
        ),
        &e,
        true,
        |t| set(t, 11, arr(vec![])),
    ));
    out.push(make(
        v!(
            "reject-commit-v2-too-many-envelopes",
            ek,
            es,
            Schema,
            "LIMIT_EXCEEDED",
            "17 key envelopes: over the reader limit of 16"
        ),
        &e,
        true,
        |t| {
            let first = at(t, &[11, 0]).clone();
            set(t, 11, arr(vec![first; 17]));
        },
    ));
    out.push(make(
        v!(
            "reject-commit-v2-data-region-short-length",
            ek,
            es,
            Schema,
            "RECORD_INVALID",
            "a data region with stored length 7"
        ),
        &e,
        true,
        |t| set(t, 12, map(vec![(0, u(100)), (1, u(7)), (2, b(32, 0x33))])),
    ));
    out.push(make(
        v!(
            "reject-commit-v2-without-encrypted-feature",
            ek,
            es,
            Schema,
            "RECORD_INVALID",
            "schema 2 without the Encrypted feature"
        ),
        &e,
        true,
        |t| set(t, 7, arr(vec![])),
    ));

    // ---- manifest schema 1 ---------------------------------------------------------------
    let md = tree(&payload("c4/valid-manifest-root-delta.bin"));
    let ms = tree(&payload("c4/valid-manifest-snapshot.bin"));
    let (m1, mk) = ("recovery-manifest-v1", K::Manifest);
    out.push(make(
        v!(
            "reject-manifest-short-archive-id",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a 31-byte archive ID"
        ),
        &md,
        false,
        |t| set(t, 1, b(31, 0x1a)),
    ));
    out.push(make(
        v!(
            "reject-manifest-chunk-encoding",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a chunk encoding other than 0"
        ),
        &md,
        false,
        |t| *at(t, &[5, 0, 1]) = u(1),
    ));
    out.push(make(
        v!(
            "reject-manifest-chunk-protection-aead",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "protection 1 in a schema-1 manifest"
        ),
        &md,
        false,
        |t| *at(t, &[5, 0, 2]) = u(1),
    ));
    out.push(make(
        v!(
            "reject-manifest-chunk-short-hash",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a chunk with a 31-byte stored hash"
        ),
        &md,
        false,
        |t| *at(t, &[5, 0, 4]) = b(31, 0x51),
    ));
    out.push(make(
        v!(
            "reject-manifest-directory-content-hash",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a directory version with a content hash"
        ),
        &md,
        false,
        |t| *at(t, &[6, 0, 3]) = b(32, 0x53),
    ));
    out.push(make(
        v!(
            "reject-manifest-directory-extents",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a directory version with an extent"
        ),
        &md,
        false,
        |t| *at(t, &[6, 0, 4]) = arr(vec![arr(vec![u(0), u(4096), b(32, 0x4c), u(0)])]),
    ));
    out.push(make(
        v!(
            "reject-manifest-hole-chunk-offset",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a hole with a non-zero chunk offset"
        ),
        &md,
        false,
        |t| *at(t, &[6, 1, 4, 1]) = arr(vec![u(4096), u(5904), Value::Null, u(5)]),
    ));
    out.push(make(
        v!(
            "reject-manifest-mtime-nanos",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "an mtime with 10^9 nanoseconds"
        ),
        &md,
        false,
        |t| *at(t, &[6, 1, 5, 2, 1]) = u(1_000_000_000),
    ));
    out.push(make(
        v!(
            "reject-manifest-unknown-operation-kind",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "an operation of kind 2"
        ),
        &md,
        false,
        |t| *at(t, &[7, 0]) = arr(vec![u(2), Value::Bytes(b"docs/s".to_vec())]),
    ));
    out.push(make(
        v!(
            "reject-manifest-snapshot-entry-shape",
            mk,
            m1,
            Schema,
            "RECORD_INVALID",
            "a snapshot entry that is a bare byte string"
        ),
        &ms,
        false,
        |t| *at(t, &[8, 0]) = Value::Bytes(b"docs".to_vec()),
    ));
    out.push(make(
        v!(
            "reject-manifest-chunks-unsorted",
            mk,
            m1,
            Semantic,
            "RECORD_INVALID",
            "chunks not in object-ID order"
        ),
        &md,
        false,
        |t| {
            let a = at(t, &[5, 0]).clone();
            let mut hi = a.clone();
            *at(&mut hi, &[0]) = b(32, 0xff);
            set(t, 5, arr(vec![hi, a]));
        },
    ));
    out.push(make(
        v!(
            "reject-manifest-entries-unsorted",
            mk,
            m1,
            Semantic,
            "RECORD_INVALID",
            "snapshot entries not in path order"
        ),
        &ms,
        false,
        |t| {
            let Value::Array(a) = at(t, &[8]) else {
                panic!()
            };
            a.swap(0, 1);
        },
    ));
    out.push(make(
        v!(
            "reject-manifest-mtime-seconds-overflow",
            mk,
            m1,
            Semantic,
            "RECORD_INVALID",
            "an mtime of -2^63 - 1 seconds"
        ),
        &md,
        false,
        |t| *at(t, &[6, 1, 5, 2, 0]) = Value::Nint(1u64 << 63),
    ));
    out.push(make(v!("valid-manifest-chunk-dependency", mk, m1, Valid, "", "a chunk that depends on a dictionary (no 1.0 writer emits one; readers refuse the chunk when they meet it)"), &md, false, |t| {
        *at(t, &[5, 0, 7]) = arr(vec![arr(vec![u(0), b(32, 0x77)])])
    }));
    out.push(make(
        v!(
            "reject-manifest-dependencies-unsorted",
            mk,
            m1,
            Semantic,
            "RECORD_INVALID",
            "dependencies not in order"
        ),
        &md,
        false,
        |t| {
            *at(t, &[5, 0, 7]) = arr(vec![
                arr(vec![u(1), b(32, 0x88)]),
                arr(vec![u(0), b(32, 0x77)]),
            ])
        },
    ));

    // ---- manifest schema 2 ---------------------------------------------------------------
    let mr = tree(&payload("c4/valid-manifest-retention-delta.bin"));
    out.push(make(
        v!(
            "valid-manifest-retention-release",
            mk,
            "recovery-manifest-v2",
            Valid,
            "",
            "a release of an active hold in a later commit"
        ),
        &mr,
        false,
        |t| {
            set(
                t,
                11,
                arr(vec![arr(vec![u(2), Value::Bytes(b"legal".to_vec())])]),
            )
        },
    ));

    // ---- manifest schema 3 (plaintext of a sealed record) --------------------------------------
    if let Some(p) = v3_valid {
        let v3 = tree(&p);
        let m3 = "recovery-manifest-v3";
        out.push(make(
            v!(
                "valid-manifest-v3-reencryption",
                mk,
                m3,
                Valid,
                "",
                "delta(0) of a re-encrypted archive: provenance reason 1"
            ),
            &v3,
            false,
            |_| {},
        ));
        out.push(make(
            v!(
                "reject-manifest-v3-key-operation-short-id",
                mk,
                m3,
                Schema,
                "RECORD_INVALID",
                "an add-key operation with a 15-byte envelope ID"
            ),
            &v3,
            false,
            |t| *at(t, &[13, 0, 1]) = b(15, 0x21),
        ));
        out.push(make(
            v!(
                "reject-manifest-v3-too-many-key-operations",
                mk,
                m3,
                Schema,
                "RECORD_INVALID",
                "33 key operations"
            ),
            &v3,
            false,
            |t| {
                let first = at(t, &[13, 0]).clone();
                set(t, 13, arr(vec![first; 33]));
            },
        ));
        out.push(make(
            v!(
                "reject-manifest-v3-provenance-reason",
                mk,
                m3,
                Schema,
                "RECORD_INVALID",
                "provenance reason 2"
            ),
            &v3,
            false,
            |t| *at(t, &[12, 3]) = u(2),
        ));
        out.push(make(
            v!(
                "reject-manifest-v3-explicit-default-reason",
                mk,
                m3,
                Schema,
                "RECORD_INVALID",
                "provenance reason 0 written explicitly"
            ),
            &v3,
            false,
            |t| *at(t, &[12, 3]) = u(0),
        ));
    }

    // ---- key envelope ---------------------------------------------------------------------
    let ke = tree(&payload("c11/valid-key-envelope.bin"));
    let (ks, kk) = ("key-envelope-v0", K::KeyEnvelope);
    out.push(make(
        v!(
            "reject-keyenv-nonce-size",
            kk,
            ks,
            Schema,
            "RECORD_INVALID",
            "a 23-byte wrap nonce"
        ),
        &ke,
        false,
        |t| set(t, 9, b(23, 0x91)),
    ));
    out.push(make(
        v!(
            "reject-keyenv-wrapped-key-size",
            kk,
            ks,
            Schema,
            "RECORD_INVALID",
            "a 47-byte wrapped key"
        ),
        &ke,
        false,
        |t| set(t, 10, b(47, 0x92)),
    ));
    out.push(make(
        v!(
            "reject-keyenv-salt-size",
            kk,
            ks,
            Schema,
            "RECORD_INVALID",
            "a 15-byte salt"
        ),
        &ke,
        false,
        |t| *at(t, &[8, 5]) = b(15, 0x93),
    ));
    out.push(make(
        v!(
            "reject-keyenv-envelope-id-size",
            kk,
            ks,
            Schema,
            "RECORD_INVALID",
            "a 15-byte envelope ID"
        ),
        &ke,
        false,
        |t| set(t, 5, b(15, 0x94)),
    ));
    out.push(make(
        v!(
            "reject-keyenv-key-id-size",
            kk,
            ks,
            Schema,
            "RECORD_INVALID",
            "a 15-byte key ID"
        ),
        &ke,
        false,
        |t| set(t, 6, b(15, 0x95)),
    ));
    out.push(make(
        v!(
            "reject-keyenv-suite",
            kk,
            ks,
            Schema,
            "UNSUPPORTED_FEATURE",
            "suite 2"
        ),
        &ke,
        false,
        |t| set(t, 7, u(2)),
    ));
    out.push(make(
        v!(
            "reject-keyenv-kdf-algorithm",
            kk,
            ks,
            Schema,
            "UNSUPPORTED_FEATURE",
            "KDF algorithm 2"
        ),
        &ke,
        false,
        |t| *at(t, &[8, 0]) = u(2),
    ));
    out.push(make(
        v!(
            "reject-keyenv-features",
            kk,
            ks,
            Schema,
            "UNSUPPORTED_FEATURE",
            "required features [2]"
        ),
        &ke,
        false,
        |t| set(t, 4, arr(vec![u(2)])),
    ));
    out
}
