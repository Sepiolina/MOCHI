//! C11 golden vectors (spec Annex B.2.10 D20; plan C11).
//!
//! * `c11/valid-key-envelope.bin`, `valid-sealed-chunk.bin`,
//!   `valid-sealed-delta-manifest.bin`: written with a **fixed random stream**,
//!   so they are byte-exact against the builders here, decode, and (with the
//!   fixed key) open. The key envelope's Argon2id cost is tiny (64 KiB, one
//!   pass, one lane); the production defaults are exercised by the format
//!   tests, not by a vector.
//! * `c11/reject-*.bin`: one deviation each; every one is refused with the
//!   code listed in [`rejects`].
//! * `c11/valid-archive-encrypted.mochi`: a whole Encrypted archive that
//!   **must keep verifying** with its passphrase at every level, and without
//!   it at the stored-integrity level. Like the C5 and C10 archives it is not
//!   compared with a fresh build (it holds zstd and SQLite output and random
//!   nonces); losing the ability to verify it would be a format change.
//!
//! Frozen when first written (`write_c11_golden_files` refuses to overwrite a
//! file that differs).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::keys::KeyEnvelope;
use mochi_core::object::ArchiveId;
use mochi_core::publish::{open_head, ArchiveWriter, Transaction};
use mochi_core::read::read_file;
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_format::cbor::CborLimits;
use mochi_format::kdf::KdfParams;
use mochi_format::registry::{ENCRYPTED_OBJECT, KEY_ENVELOPE};
use mochi_format::repr::StoredObject;
use mochi_format::seal::{
    open_payload, parse_sealed, seal_frame, sealed_frame_payload, KeyId, SealContext, SealTarget,
};
use mochi_format::secret::{DataKey, Passphrase, Random};
use mochi_format::Limits;
use mochi_testkit::archive::{encrypted_options, keyed_read, path, Job};
use mochi_testkit::fuzz::{
    exercise_all, exercise_archive_open, exercise_key_envelope, exercise_sealed_object,
};
use mochi_testkit::replay::attrs;
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const ARCHIVE: &str = "valid-archive-encrypted.mochi";
const PASSPHRASE: &str = "golden passphrase";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c11")
}

/// A replayable byte stream (xorshift), so the vectors are byte-exact.
struct Stream(u64);
impl Random for Stream {
    fn fill(&mut self, buf: &mut [u8]) -> mochi_format::error::Result<()> {
        for b in buf {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            *b = self.0 as u8;
        }
        Ok(())
    }
}

const KDF: KdfParams = KdfParams {
    memory_kib: 64,
    iterations: 1,
    lanes: 1,
};
const ARCHIVE_ID: [u8; 32] = [0xA1; 32];

fn dek() -> DataKey {
    DataKey::from_bytes([0x11; 32])
}

fn key_id() -> KeyId {
    KeyId::from_bytes([0xC3; 16])
}

fn ctx(key: &DataKey) -> SealContext<'_> {
    SealContext {
        key,
        key_id: key_id(),
        archive_id: ARCHIVE_ID,
    }
}

fn envelope() -> KeyEnvelope {
    KeyEnvelope::create(
        ArchiveId::from_bytes(ARCHIVE_ID),
        0,
        [0xB2; 16],
        key_id(),
        &dek(),
        &Passphrase::new(PASSPHRASE).unwrap(),
        KDF,
        &mut Stream(0x1234_5678_9ABC_DEF1),
        &Limits::default(),
    )
    .unwrap()
}

fn chunk_target() -> SealTarget {
    SealTarget::Chunk {
        object_id: [0xD4; 32],
    }
}

fn manifest_target() -> SealTarget {
    SealTarget::DeltaManifest {
        sequence: 3,
        transaction_id: [0xE5; 16],
    }
}

const PLAINTEXT: &[u8] = b"golden plaintext: a zstd frame would sit here in a real chunk";

fn sealed(target: &SealTarget, seed: u64) -> Vec<u8> {
    let key = dek();
    seal_frame(
        &ctx(&key),
        target,
        PLAINTEXT,
        &mut Stream(seed),
        &Limits::default(),
    )
    .unwrap()
    .as_bytes()
    .to_vec()
}

fn valid() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "valid-key-envelope.bin",
            envelope().to_stored().unwrap().as_bytes().to_vec(),
        ),
        ("valid-sealed-chunk.bin", sealed(&chunk_target(), 0xC0FFEE)),
        (
            "valid-sealed-delta-manifest.bin",
            sealed(&manifest_target(), 0xBEEF),
        ),
    ]
}

/// A key envelope with one cost parameter set to `f`'s value.
fn envelope_with(f: impl FnOnce(&mut KeyEnvelope)) -> Vec<u8> {
    let mut e = envelope();
    f(&mut e);
    e.to_stored().unwrap().as_bytes().to_vec()
}

/// A sealed frame with `edit` applied to the payload bytes (the frame's own
/// length is unchanged unless `edit` truncates).
fn sealed_edit(edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = sealed(&chunk_target(), 0xC0FFEE);
    edit(&mut b);
    if b.len() >= 8 {
        let payload = (b.len() - 8) as u32;
        b[4..8].copy_from_slice(&payload.to_le_bytes());
    }
    b
}

/// The sealed header starts after the 8-byte skippable header: version (2),
/// suite (2), kind (4), key ID (16), nonce (24).
const H: usize = 8;

/// `(file, what the reader does with it)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// The envelope decoder (or its cost check at unwrap) refuses it.
    Envelope(ErrorCode),
    /// The sealed-frame or header layer refuses it.
    SealedFraming(ErrorCode),
    /// The header is well formed; the AEAD refuses it under the golden key.
    SealedTag(ErrorCode),
}

fn rejects() -> Vec<(&'static str, Vec<u8>, Expect)> {
    let env = envelope().to_stored().unwrap().as_bytes().to_vec();
    let mut wrong_kind = env.clone();
    wrong_kind[..4].copy_from_slice(&ENCRYPTED_OBJECT.to_le_bytes());
    let mut flipped_wrap = env.clone();
    let last = flipped_wrap.len() - 3;
    flipped_wrap[last] ^= 1;
    vec![
        (
            "reject-envelope-truncated.bin",
            env[..env.len() / 2].to_vec(),
            Expect::Envelope(ErrorCode::Truncated),
        ),
        (
            "reject-envelope-wrong-frame-kind.bin",
            wrong_kind,
            Expect::Envelope(ErrorCode::MalformedFrame),
        ),
        (
            "reject-envelope-kdf-memory.bin",
            envelope_with(|e| e.kdf.memory_kib = 1 << 40),
            Expect::Envelope(ErrorCode::LimitExceeded),
        ),
        (
            "reject-envelope-kdf-iterations.bin",
            envelope_with(|e| e.kdf.iterations = 17),
            Expect::Envelope(ErrorCode::LimitExceeded),
        ),
        (
            "reject-envelope-kdf-lanes.bin",
            envelope_with(|e| e.kdf.lanes = 17),
            Expect::Envelope(ErrorCode::LimitExceeded),
        ),
        (
            "reject-envelope-kdf-zero-lanes.bin",
            envelope_with(|e| e.kdf.lanes = 0),
            Expect::Envelope(ErrorCode::RecordInvalid),
        ),
        (
            "reject-envelope-kdf-memory-below-8-per-lane.bin",
            envelope_with(|e| {
                e.kdf.memory_kib = 7;
                e.kdf.lanes = 1;
            }),
            Expect::Envelope(ErrorCode::RecordInvalid),
        ),
        (
            "reject-envelope-wrapped-key-flipped.bin",
            flipped_wrap,
            // A flipped wrapped key is well formed; the unwrap finds no key.
            Expect::Envelope(ErrorCode::KeyUnavailable),
        ),
        (
            "reject-sealed-version.bin",
            sealed_edit(|b| b[H] = 1),
            Expect::SealedFraming(ErrorCode::UnsupportedFeature),
        ),
        (
            "reject-sealed-suite.bin",
            sealed_edit(|b| b[H + 2] = 2),
            Expect::SealedFraming(ErrorCode::UnsupportedFeature),
        ),
        (
            "reject-sealed-kind.bin",
            sealed_edit(|b| b[H + 4] = 9),
            Expect::SealedFraming(ErrorCode::UnsupportedFeature),
        ),
        (
            "reject-sealed-too-short.bin",
            sealed_edit(|b| b.truncate(H + 40)),
            Expect::SealedFraming(ErrorCode::MalformedFrame),
        ),
        (
            "reject-sealed-key-id.bin",
            sealed_edit(|b| b[H + 8] ^= 1),
            Expect::SealedTag(ErrorCode::RecordInvalid),
        ),
        (
            "reject-sealed-nonce.bin",
            sealed_edit(|b| b[H + 30] ^= 1),
            Expect::SealedTag(ErrorCode::ContentIntegrityFailed),
        ),
        (
            "reject-sealed-ciphertext.bin",
            sealed_edit(|b| b[H + 50] ^= 1),
            Expect::SealedTag(ErrorCode::ContentIntegrityFailed),
        ),
        (
            "reject-sealed-tag.bin",
            sealed_edit(|b| {
                let n = b.len();
                b[n - 1] ^= 1;
            }),
            Expect::SealedTag(ErrorCode::ContentIntegrityFailed),
        ),
    ]
}

fn read_all() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin" || x == "mochi"))
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

#[test]
fn the_valid_vectors_are_byte_exact_and_open() {
    let stored: std::collections::BTreeMap<_, _> = read_all().into_iter().collect();
    for (name, bytes) in valid() {
        assert_eq!(
            stored.get(name),
            Some(&bytes),
            "{name} differs from its builder"
        );
    }
    let key = dek();
    let limits = Limits::default();

    let env = KeyEnvelope::from_stored(
        &StoredObject::from_loaded(stored["valid-key-envelope.bin"].clone()),
        &limits,
        &CborLimits::default(),
    )
    .unwrap();
    assert_eq!(
        &stored["valid-key-envelope.bin"][..4],
        &KEY_ENVELOPE.to_le_bytes()
    );
    let opened = env
        .unwrap(&Passphrase::new(PASSPHRASE).unwrap(), &limits)
        .unwrap()
        .expect("the golden passphrase opens the golden envelope");
    assert!(opened.ct_eq(&key));
    assert!(env
        .unwrap(&Passphrase::new("another passphrase").unwrap(), &limits)
        .unwrap()
        .is_none());

    for (name, target) in [
        ("valid-sealed-chunk.bin", chunk_target()),
        ("valid-sealed-delta-manifest.bin", manifest_target()),
    ] {
        let stored = StoredObject::from_loaded(stored[name].clone());
        let payload = sealed_frame_payload(&stored, &limits).unwrap();
        assert_eq!(&stored.as_bytes()[..4], &ENCRYPTED_OBJECT.to_le_bytes());
        assert_eq!(
            open_payload(&ctx(&key), &target, payload).unwrap(),
            PLAINTEXT,
            "{name}"
        );
        // Bound to its target: the other target does not open it.
        let other = if name.contains("chunk") {
            manifest_target()
        } else {
            chunk_target()
        };
        assert!(open_payload(&ctx(&key), &other, payload).is_err(), "{name}");
    }
}

#[test]
fn every_reject_vector_is_refused_with_its_code() {
    let stored: std::collections::BTreeMap<_, _> = read_all().into_iter().collect();
    let key = dek();
    let limits = Limits::default();
    for (name, bytes, expect) in rejects() {
        assert_eq!(
            stored.get(name),
            Some(&bytes),
            "{name} differs from its builder"
        );
        let object = StoredObject::from_loaded(bytes.clone());
        let got = match expect {
            Expect::Envelope(_) => {
                KeyEnvelope::from_stored(&object, &limits, &CborLimits::default())
                    .and_then(|e| {
                        e.unwrap(&Passphrase::new(PASSPHRASE).unwrap(), &limits)
                            .and_then(|k| {
                                k.map(|_| ()).ok_or_else(|| {
                                    mochi_core::MochiError::new(
                                        ErrorCode::KeyUnavailable,
                                        "no envelope opened",
                                    )
                                })
                            })
                    })
                    .map_err(|e| e.code)
            }
            Expect::SealedFraming(_) => sealed_frame_payload(&object, &limits)
                .and_then(|p| parse_sealed(p).map(|_| ()))
                .map_err(|e| mochi_core::MochiError::from(e).code),
            Expect::SealedTag(_) => sealed_frame_payload(&object, &limits)
                .and_then(|p| open_payload(&ctx(&key), &chunk_target(), p).map(|_| ()))
                .map_err(|e| mochi_core::MochiError::from(e).code),
        };
        let want = match expect {
            Expect::Envelope(c) | Expect::SealedFraming(c) | Expect::SealedTag(c) => c,
        };
        assert_eq!(got, Err(want), "{name}");
        exercise_key_envelope(&bytes);
        exercise_sealed_object(&bytes);
    }
}

/// The archive: three commits (a checkpoint, then deltas), two files across
/// several chunks, a rename, and a passphrase added by a rewrap.
fn archive() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(0xC11)),
        encrypted_options(&[PASSPHRASE]),
    )
    .unwrap();
    let job = Job::new();
    let mut tx = Transaction::new();
    tx.put_dir(path("docs"), attrs(0o755, 0));
    tx.put_file(path("docs/a"), deterministic_bytes(1, 200), attrs(0o644, 0));
    tx.put_file(path("b"), deterministic_bytes(2, 130), attrs(0o600, 0));
    w.commit(tx, &job.ctx()).unwrap();
    let mut tx = Transaction::new();
    tx.rename(path("b"), path("docs/b"));
    tx.put_file(path("docs/a"), deterministic_bytes(3, 90), attrs(0o644, 0));
    w.commit(tx, &job.ctx()).unwrap();
    mochi_core::rekey::rewrap(
        &mut w,
        vec![Passphrase::new("second golden passphrase").unwrap()],
        vec![],
        &job.ctx(),
    )
    .unwrap();
    w.close().unwrap();
    s
}

#[test]
fn the_archive_vector_keeps_verifying_keyed_and_keyless() {
    let stored: std::collections::BTreeMap<_, _> = read_all().into_iter().collect();
    let bytes = stored[ARCHIVE].clone();
    let s = SimStorage::from_bytes(bytes.clone());

    for pass in [PASSPHRASE, "second golden passphrase"] {
        for level in [
            VerificationLevel::Structural,
            VerificationLevel::Referential,
            VerificationLevel::StoredIntegrity,
            VerificationLevel::ContentIntegrity,
            VerificationLevel::Restoration,
        ] {
            let r = verify(
                &s,
                &VerifyOptions {
                    level,
                    read: keyed_read(&[pass]),
                    deep: true,
                    ..VerifyOptions::default()
                },
                &Job::new().ctx(),
            )
            .report;
            // The vector's two envelopes use the test KDF, below the writer
            // default: one informational finding each (D20 item 14), and
            // nothing else.
            assert_eq!(r.findings.len(), 2, "{pass} {level:?}: {:?}", r.findings);
            assert!(
                r.findings
                    .iter()
                    .all(|f| f.code == mochi_core::ErrorCode::KdfCostBelowDefault),
                "{pass} {level:?}: {:?}",
                r.findings
            );
            assert_eq!(r.dimensions[&Dimension::KeyAvailability], Status::Pass);
        }
    }
    // Without a key: stored integrity passes; the rest is UNKNOWN.
    let r = verify(
        &s,
        &VerifyOptions {
            level: VerificationLevel::StoredIntegrity,
            ..VerifyOptions::default()
        },
        &Job::new().ctx(),
    )
    .report;
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Pass);
    assert_eq!(r.dimensions[&Dimension::KeyAvailability], Status::Unknown);

    let o = keyed_read(&[PASSPHRASE]);
    let head = open_head(&s, &o).unwrap();
    assert_eq!(head.commit.seq, 2);
    let mut got = Vec::new();
    read_file(&s, &head, &path("docs/b"), &mut got, &o, &Job::new().ctx()).unwrap();
    assert_eq!(got, deterministic_bytes(2, 130));
    exercise_all(&bytes);
}

/// Deterministic mutation smoke over every vector (the fuzz targets'
/// properties): no panic, and nothing authenticates.
#[test]
fn fuzz_smoke_over_mutated_vectors() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for (name, bytes) in read_all() {
        // Whole archives go through the archive_open exerciser, which runs the
        // keyless verification, damage assessment, and repair plan on them.
        let archive = name.ends_with(".mochi");
        for _ in 0..if archive { 60 } else { 300 } {
            let mut b = bytes.clone();
            for _ in 0..=(next() % 3) {
                let i = (next() % b.len() as u64) as usize;
                match next() % 3 {
                    0 => b[i] ^= 1 << (next() % 8),
                    1 => b[i] = next() as u8,
                    _ => b.truncate(i.max(1)),
                }
            }
            if archive {
                exercise_archive_open(&b);
            } else {
                exercise_key_envelope(&b);
                exercise_sealed_object(&b);
            }
        }
    }
}

#[test]
#[ignore = "writes new fixtures; run by hand and review the diff"]
fn write_c11_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    let mut all: Vec<(String, Vec<u8>)> = valid()
        .into_iter()
        .map(|(n, b)| (n.to_string(), b))
        .collect();
    all.extend(rejects().into_iter().map(|(n, b, _)| (n.to_string(), b)));
    all.push((ARCHIVE.into(), archive().contents()));
    for (name, bytes) in all {
        let path = dir().join(&name);
        if let Ok(old) = std::fs::read(&path) {
            if name == ARCHIVE || old == bytes {
                continue; // frozen
            }
            panic!("{name} exists and differs: fixtures are frozen");
        }
        std::fs::write(&path, bytes).unwrap();
    }
}
