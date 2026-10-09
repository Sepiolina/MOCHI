//! C10: the profile's pax/ustar encoder against a real GNU tar (spec 7.2,
//! Annex B.2.9 D19). Local evidence only: it runs when `tar` on the path is
//! GNU tar and is skipped otherwise. The tested tools and versions of the
//! compatibility claim come from the CI job `tar interop`, not from here.

#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

use mochi_core::tar::{encode_header, padding, Member, MemberKind, END_BLOCKS};

fn gnu_tar() -> Option<String> {
    let out = Command::new("tar").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.contains("GNU tar")
        .then(|| text.lines().next().unwrap_or("").to_owned())
}

fn member(
    path: &[u8],
    kind: MemberKind,
    content: &[u8],
    mode: u32,
    mtime: (i64, u32),
) -> (Member, Vec<u8>) {
    (
        Member {
            path: path.to_vec(),
            kind,
            size: content.len() as u64,
            mode,
            uid: 1234,
            gid: 4321,
            mtime,
        },
        content.to_vec(),
    )
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

fn extract(tarball: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("in.tar");
    std::fs::write(&file, tarball).unwrap();
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).unwrap();
    // --no-same-owner: an unprivileged user cannot chown anyway.
    let out = Command::new("tar")
        .args(["-x", "--ignore-zeros", "--no-same-owner", "-f"])
        .arg(&file)
        .arg("-C")
        .arg(&dest)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, dest)
}

#[test]
fn gnu_tar_extracts_what_the_encoder_writes() {
    let Some(version) = gnu_tar() else {
        eprintln!("skipped: no GNU tar");
        return;
    };
    eprintln!("tested against: {version}");
    let long = format!("d/{}", "l".repeat(180)); // > 100 bytes: pax path
    let nested = format!("d/{}/{}", "n".repeat(120), "f.txt");
    let content_a = vec![7u8; 1500];
    let members = vec![
        member(b"d", MemberKind::Directory, b"", 0o755, (1_700_000_000, 0)),
        member(
            b"d/a.txt",
            MemberKind::File,
            &content_a,
            0o644,
            (1_700_000_001, 0),
        ),
        member(
            b"d/exec",
            MemberKind::File,
            b"#!/bin/sh\n",
            0o755,
            (1_700_000_002, 123_456_789),
        ),
        member(
            long.as_bytes(),
            MemberKind::File,
            b"long name",
            0o600,
            (1_700_000_003, 0),
        ),
        member(
            "d/héllo wörld".as_bytes(),
            MemberKind::File,
            b"utf8",
            0o644,
            (1_700_000_004, 0),
        ),
        member(
            b"d/bad\xff\xfename",
            MemberKind::File,
            b"binary name",
            0o644,
            (1_700_000_005, 500_000_000),
        ),
        member(b"d/empty", MemberKind::File, b"", 0o644, (1_700_000_006, 0)),
        member(
            nested.as_bytes(),
            MemberKind::File,
            b"nested",
            0o644,
            (0, 0),
        ),
    ];
    let (_dir, dest) = extract(&stream(&members));

    for (m, content) in &members {
        let rel = std::ffi::OsStr::from_bytes(&m.path);
        let on_disk = dest.join(rel);
        let meta = std::fs::symlink_metadata(&on_disk).unwrap_or_else(|e| {
            panic!(
                "{:?} was not extracted: {e}",
                String::from_utf8_lossy(&m.path)
            )
        });
        match m.kind {
            MemberKind::Directory => assert!(meta.is_dir()),
            MemberKind::File => {
                assert!(meta.is_file());
                assert_eq!(&std::fs::read(&on_disk).unwrap(), content);
            }
        }
        // Permission bits are subject to the umask only on creation; tar -x
        // applies the archived mode (-p is implied for the owner bits).
        assert_eq!(
            meta.mode() & 0o700,
            m.mode & 0o700,
            "{:?}",
            String::from_utf8_lossy(&m.path)
        );
        if m.mtime.0 > 0 && m.kind == MemberKind::File {
            assert_eq!(
                meta.mtime(),
                m.mtime.0,
                "{:?}",
                String::from_utf8_lossy(&m.path)
            );
            assert_eq!(
                meta.mtime_nsec() as u32,
                m.mtime.1,
                "{:?}",
                String::from_utf8_lossy(&m.path)
            );
        }
    }
}

/// Concatenated streams (one per commit) extract with `--ignore-zeros`, and
/// the later stream's member wins: the historical stream, not the latest
/// snapshot (spec 7.2).
#[test]
fn concatenated_streams_extract_in_order_with_ignore_zeros() {
    if gnu_tar().is_none() {
        eprintln!("skipped: no GNU tar");
        return;
    }
    let first = stream(&[
        member(b"a", MemberKind::File, b"first", 0o644, (1_700_000_000, 0)),
        member(
            b"gone",
            MemberKind::File,
            b"deleted later",
            0o644,
            (1_700_000_000, 0),
        ),
    ]);
    // The second commit rewrites `a`; a deletion of `gone` emits nothing.
    let second = stream(&[member(
        b"a",
        MemberKind::File,
        b"second",
        0o644,
        (1_700_000_100, 0),
    )]);
    let mut both = first.clone();
    both.extend_from_slice(&second);
    let (_dir, dest) = extract(&both);
    assert_eq!(std::fs::read(dest.join("a")).unwrap(), b"second");
    assert_eq!(
        std::fs::read(dest.join("gone")).unwrap(),
        b"deleted later",
        "deletions are not applied by generic extraction"
    );
}
