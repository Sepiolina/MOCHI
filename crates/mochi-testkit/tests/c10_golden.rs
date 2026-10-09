//! C10 golden vectors (spec Annex B.2.9 D19; plan C10).
//!
//! * `c10/valid-*.tar`: complete streams the profile's encoder writes,
//!   **byte-exact** against the builders here (the encoder involves no
//!   compression library, so exactness is meaningful) and accepted by the
//!   parser, which re-encodes them to the same bytes.
//! * `c10/reject-*.tar`: one deviation each; the parser refuses every one with
//!   `PROFILE_VIOLATION`.
//! * `c10/valid-archive-tar-4-commits.mochi`: a whole TAR-compatible archive
//!   that **must keep verifying** at every level. Like the C5 archives it is
//!   not compared with a fresh build (it contains zstd and SQLite output);
//!   losing the ability to verify it would be a format change.
//!
//! Frozen when first written (`write_c10_golden_files` refuses to overwrite a
//! file that differs).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::publish::{open_head, ArchiveWriter, ReadOptions, WriterOptions};
use mochi_core::status::{Dimension, Status, VerificationLevel};
use mochi_core::tar::{encode_header, padding, Event, Member, MemberKind, Parser, END_BLOCKS};
use mochi_core::verify::{verify, VerifyOptions};
use mochi_core::ErrorCode;
use mochi_testkit::archive::{read_state, test_options, Job};
use mochi_testkit::fuzz::{exercise_all, exercise_tar_stream};
use mochi_testkit::replay::{attrs, Model};
use mochi_testkit::{deterministic_bytes, SeqIds, SimStorage};

const ARCHIVE: &str = "valid-archive-tar-4-commits.mochi";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/c10")
}

fn member(path: &[u8], kind: MemberKind, size: u64, mtime: (i64, u32), uid: u32) -> Member {
    Member {
        path: path.to_vec(),
        kind,
        size,
        mode: if kind == MemberKind::File {
            0o644
        } else {
            0o755
        },
        uid,
        gid: 20,
        mtime,
    }
}

fn stream(members: &[(Member, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (m, c) in members {
        out.extend(encode_header(m).unwrap());
        out.extend_from_slice(c);
        out.resize(out.len() + padding(m.size), 0);
    }
    out.extend_from_slice(&END_BLOCKS);
    out
}

/// A ustar header for the reject vectors, with a correct checksum.
fn raw_header(name: &[u8], flag: u8, size: usize) -> Vec<u8> {
    let mut h = vec![0u8; 512];
    h[..name.len()].copy_from_slice(name);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = flag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    refix(&mut h);
    h
}

fn refix(h: &mut [u8]) {
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h[..512].iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
}

fn padded(content: &[u8]) -> Vec<u8> {
    let mut v = content.to_vec();
    v.resize(v.len() + padding(content.len() as u64), 0);
    v
}

fn files() -> Vec<(&'static str, Vec<u8>)> {
    let long = format!("{}/{}", "d".repeat(90), "f".repeat(60)).into_bytes();
    let basic = stream(&[
        (
            member(b"d", MemberKind::Directory, 0, (1_700_000_000, 0), 1000),
            vec![],
        ),
        (
            member(b"d/f", MemberKind::File, 5, (1_700_000_001, 0), 1000),
            b"hello".to_vec(),
        ),
        (member(b"empty", MemberKind::File, 0, (0, 0), 0), vec![]),
    ]);
    let mut out = vec![
        ("valid-basic.tar", basic.clone()),
        (
            "valid-pax.tar",
            stream(&[
                (
                    member(
                        &long,
                        MemberKind::File,
                        3,
                        (1_700_000_000, 123_000_000),
                        5_000_000,
                    ),
                    b"abc".to_vec(),
                ),
                (
                    member(b"caf\xe9", MemberKind::File, 1, (-5, 0), 7),
                    b"x".to_vec(),
                ),
            ]),
        ),
    ];

    // Rejects: one deviation each.
    let mut v = basic.clone();
    v[0] ^= 1; // the name changes, the checksum no longer matches
    out.push(("reject-bad-checksum.tar", v));
    let mut v = basic.clone();
    v[1024 + 5 + 3] = 1; // padding after `hello`, in the third block
    out.push(("reject-nonzero-padding.tar", v));
    out.push((
        "reject-no-end-blocks.tar",
        basic[..basic.len() - 1024].to_vec(),
    ));
    out.push((
        "reject-one-end-block.tar",
        basic[..basic.len() - 512].to_vec(),
    ));
    let mut v = basic.clone();
    v.extend_from_slice(&[0; 512]);
    out.push(("reject-bytes-after-end.tar", v));
    let mut v = raw_header(b"link", b'2', 0);
    v.extend_from_slice(&END_BLOCKS);
    out.push(("reject-symlink.tar", v));
    let body = b"20 path=elsewhere\n\0";
    let mut v = raw_header(b"PaxHeader", b'g', body.len() - 1);
    v.extend(padded(&body[..body.len() - 1]));
    v.extend(raw_header(b"f", b'0', 0));
    v.extend_from_slice(&END_BLOCKS);
    out.push(("reject-global-pax.tar", v));
    let body = b"12 foo=bar\n";
    let mut v = raw_header(b"PaxHeader", b'x', body.len());
    v.extend(padded(body));
    v.extend(raw_header(b"f", b'0', 0));
    v.extend_from_slice(&END_BLOCKS);
    out.push(("reject-unknown-pax-key.tar", v));
    let mut h = raw_header(b"f", b'0', 0);
    h[257..265].copy_from_slice(b"ustar  \0");
    refix(&mut h);
    let mut v = h;
    v.extend_from_slice(&END_BLOCKS);
    out.push(("reject-gnu-magic.tar", v));
    out
}

/// Why the parser refused: the code and the message.
fn parse(bytes: &[u8]) -> Result<(), (ErrorCode, String)> {
    let mut p = Parser::new(1 << 10);
    p.feed(bytes, &mut |_: Event<'_>| {})
        .and_then(|()| p.finish())
        .map_err(|e| (e.code, e.message))
}

/// What each reject vector must be refused for (a part of the message), so
/// that no vector is refused for an incidental reason.
fn reason(name: &str) -> &'static str {
    match name {
        "reject-bad-checksum.tar" => "checksum",
        "reject-nonzero-padding.tar" => "padding",
        "reject-no-end-blocks.tar" => "end-of-archive",
        "reject-one-end-block.tar" => "end-of-archive",
        "reject-bytes-after-end.tar" => "follow",
        "reject-symlink.tar" => "typeflag",
        "reject-global-pax.tar" => "typeflag",
        "reject-unknown-pax-key.tar" => "pax",
        "reject-gnu-magic.tar" => "ustar",
        other => panic!("{other}: no reason listed"),
    }
}

fn archive() -> SimStorage {
    let s = SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(SeqIds::new(7)),
        WriterOptions {
            profile: Some(mochi_core::descriptor::Profile {
                tar_compatible: true,
                encrypted: false,
            }),
            ..test_options()
        },
    )
    .unwrap();
    let job = Job::new();
    for st in model().steps {
        w.commit(st.tx, &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s
}

fn model() -> Model {
    let mut m = Model::default();
    m.dir("d", attrs(0o750, 0));
    m.file("d/a", deterministic_bytes(1, 200), attrs(0o640, 1));
    m.file("b", deterministic_bytes(2, 70), attrs(0o600, 2));
    m.file("c", Vec::new(), attrs(0o444, 3));
    m.end(0);
    m.file("d/a", deterministic_bytes(3, 130), attrs(0o641, 11));
    m.end(10);
    m.rename("b", "d/b");
    m.delete("c");
    m.end(20);
    m.delete("d/a");
    m.end(30);
    m
}

#[test]
fn builders_agree_with_the_parser() {
    for (name, bytes) in files() {
        match (name.starts_with("valid-"), parse(&bytes)) {
            (true, Ok(())) => {}
            (false, Err((code, message))) => {
                assert_eq!(code, ErrorCode::ProfileViolation, "{name}");
                assert!(message.contains(reason(name)), "{name}: {message}");
            }
            (_, got) => panic!("{name}: {got:?}"),
        }
        exercise_tar_stream(&bytes);
    }
}

#[test]
fn checked_in_files_match_the_builders() {
    let mut known: Vec<String> = vec![ARCHIVE.into()];
    for (name, bytes) in files() {
        let on_disk = std::fs::read(dir().join(name))
            .unwrap_or_else(|e| panic!("{name}: {e} (run write_c10_golden_files deliberately)"));
        assert_eq!(on_disk, bytes, "{name} differs from its builder");
        known.push(name.into());
    }
    let mut present: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    present.sort();
    known.sort();
    assert_eq!(present, known, "unlisted or missing fixture files");
}

/// The checked-in archive keeps verifying at every level, `fsck` depth
/// included, and keeps reading as the scripted history.
#[test]
fn the_checked_in_archive_keeps_verifying() {
    let bytes = std::fs::read(dir().join(ARCHIVE)).unwrap();
    let s = SimStorage::from_bytes(bytes.clone());
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
                deep: true,
                ..VerifyOptions::default()
            },
            &Job::new().ctx(),
        )
        .report;
        assert!(r.findings.is_empty(), "{level:?}: {:?}", r.findings);
        assert!(r.scope.as_deref().unwrap().contains("TAR stream"));
    }
    let head = open_head(&s, &ReadOptions::default()).unwrap();
    assert!(head.descriptor.tar_compatible);
    assert_eq!(
        read_state(&s, &head).unwrap(),
        model().steps.last().unwrap().after
    );
    let r = verify(&s, &VerifyOptions::default(), &Job::new().ctx()).report;
    assert_eq!(r.dimensions[&Dimension::Integrity], Status::Pass);
    exercise_all(&bytes);
}

/// Deterministic mutation smoke over every vector (the fuzz target's
/// properties): no panic, and the outcome does not depend on how the input
/// is cut.
#[test]
fn fuzz_smoke_over_mutated_vectors() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for (_, bytes) in files() {
        for _ in 0..400 {
            let mut b = bytes.clone();
            for _ in 0..=(next() % 3) {
                let i = (next() % b.len() as u64) as usize;
                match next() % 3 {
                    0 => b[i] ^= 1 << (next() % 8),
                    1 => b[i] = next() as u8,
                    _ => b.truncate(i.max(1)),
                }
            }
            exercise_tar_stream(&b);
        }
    }
}

#[test]
#[ignore = "writes new fixtures; run by hand and review the diff"]
fn write_c10_golden_files() {
    std::fs::create_dir_all(dir()).unwrap();
    let mut all: Vec<(String, Vec<u8>)> = files()
        .into_iter()
        .map(|(n, b)| (n.to_string(), b))
        .collect();
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
