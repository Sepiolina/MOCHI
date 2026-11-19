//! C3 golden vector (R8): a checked-in published catalog image that must keep
//! opening, with the same content, in every later build.
//!
//! Unlike the C1/C2 vectors, the file is **not** compared byte-for-byte with a
//! fresh build: SQLite may lay out pages differently in a later version, and
//! that is not a format change. What must never change is that this image
//! still opens, verifies, and replays to the same snapshot: a reader-side
//! compatibility promise (spec §26).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use mochi_core::catalog::extent::{Extent, ExtentSource};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::catalog::{Catalog, CatalogLimits, Commit, FileVersion};
use mochi_core::object::{ObjectId, ObjectRecord};
use mochi_format::codec::{Encoding, Protection};
use mochi_format::digest::{ChunkContentHash, FileContentHash, StoredObjectHash};
use mochi_testkit::fuzz::exercise_catalog_image;

fn path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden/c3/valid-catalog-v0.sqlite")
}

fn p(s: &str) -> ArchivePath {
    ArchivePath::from_stored(s.as_bytes()).unwrap()
}

const CHUNK: ObjectId = ObjectId::from_bytes([0xC0; 32]);
const DIR: FileVersionId = FileVersionId::from_bytes([0xD0; 32]);
const SPARSE: FileVersionId = FileVersionId::from_bytes([0xF0; 32]);
const RENAMED: FileVersionId = FileVersionId::from_bytes([0xF1; 32]);

/// Three commits: create a directory and a sparse file; add a file with a
/// Windows name containing an unpaired surrogate; rename the sparse file.
fn build() -> Catalog {
    let mut c = Catalog::new_working().unwrap();
    c.insert_object(
        &ObjectRecord {
            id: CHUNK,
            encoding: Encoding::ZstdFrame,
            protection: Protection::None,
            stored_len: 123,
            stored_hash: StoredObjectHash::from_bytes([0x11; 32]),
            decoded_len: 4096,
            content_hash: ChunkContentHash::from_bytes([0x22; 32]),
            dependencies: vec![],
        },
        Some(0),
    )
    .unwrap();
    c.insert_file_version(
        &FileVersion {
            id: DIR,
            kind: EntryKind::Directory,
            logical_len: 0,
            content_hash: None,
        },
        &[],
    )
    .unwrap();
    c.insert_file_version(
        &FileVersion {
            id: SPARSE,
            kind: EntryKind::File,
            logical_len: 10_000,
            content_hash: Some(FileContentHash::from_bytes([0x33; 32])),
        },
        &[
            Extent {
                ordinal: 0,
                logical_offset: 0,
                length: 1000,
                source: ExtentSource::Chunk {
                    chunk: CHUNK,
                    chunk_offset: 0,
                },
            },
            Extent {
                ordinal: 1,
                logical_offset: 1000,
                length: 8000,
                source: ExtentSource::Hole,
            },
            Extent {
                ordinal: 2,
                logical_offset: 9000,
                length: 1000,
                source: ExtentSource::Chunk {
                    chunk: CHUNK,
                    chunk_offset: 3000,
                },
            },
        ],
    )
    .unwrap();
    c.insert_file_version(
        &FileVersion {
            id: RENAMED,
            kind: EntryKind::File,
            logical_len: 5,
            content_hash: Some(FileContentHash::from_bytes([0x44; 32])),
        },
        &[Extent {
            ordinal: 0,
            logical_offset: 0,
            length: 5,
            source: ExtentSource::Chunk {
                chunk: CHUNK,
                chunk_offset: 4091,
            },
        }],
    )
    .unwrap();
    let windows_name = ArchivePath::from_utf16_components([
        "docs".encode_utf16().collect::<Vec<u16>>(),
        vec![u16::from(b'x'), 0xD800],
    ])
    .unwrap();
    c.append_commit(&Commit {
        seq: 0,
        parent: None,
        ops: vec![
            NamespaceOp::Put {
                path: p("docs/sparse.bin"),
                version: SPARSE,
            },
            NamespaceOp::Put {
                path: p("docs"),
                version: DIR,
            },
        ],
    })
    .unwrap();
    c.append_commit(&Commit {
        seq: 1,
        parent: Some(0),
        ops: vec![NamespaceOp::Put {
            path: windows_name,
            version: RENAMED,
        }],
    })
    .unwrap();
    c.append_commit(&Commit {
        seq: 2,
        parent: Some(1),
        ops: vec![
            NamespaceOp::Delete {
                path: p("docs/sparse.bin"),
            },
            NamespaceOp::Put {
                path: p("docs/renamed.bin"),
                version: SPARSE,
            },
        ],
    })
    .unwrap();
    c
}

#[test]
fn checked_in_image_opens_with_the_expected_content() {
    let bytes = std::fs::read(path())
        .expect("golden image missing (run write_c3_golden_files deliberately)");
    let opened = Catalog::open_image(&bytes, &CatalogLimits::default()).unwrap();
    let fresh = build();
    for seq in 0..=2 {
        assert_eq!(
            opened.replay(Some(seq)).unwrap(),
            fresh.replay(Some(seq)).unwrap(),
            "commit {seq}"
        );
    }
    assert_eq!(
        opened.object(&CHUNK).unwrap(),
        fresh.object(&CHUNK).unwrap()
    );
    for id in [DIR, SPARSE, RENAMED] {
        assert_eq!(
            opened.file_version(&id).unwrap(),
            fresh.file_version(&id).unwrap()
        );
    }
    // The unpaired surrogate survives the round trip exactly.
    let snap = opened.replay(None).unwrap();
    let win = snap
        .iter()
        .map(|(p, _)| p)
        .find(|p| p.as_stored().starts_with(b"docs/x"))
        .unwrap();
    assert_eq!(
        win.to_utf16_components().unwrap()[1],
        vec![u16::from(b'x'), 0xD800]
    );
}

#[test]
#[ignore = "rewrites fixtures; run by hand and review the diff"]
fn write_c3_golden_files() {
    std::fs::create_dir_all(path().parent().unwrap()).unwrap();
    std::fs::write(path(), build().publish().unwrap().as_bytes()).unwrap();
}

/// Stable-Rust stand-in for the `catalog_image` fuzz target.
#[test]
fn fuzz_smoke_over_mutated_images() {
    let good = std::fs::read(path()).unwrap();
    exercise_catalog_image(&good);
    let mut state = 0x5EED_0003u64;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for _ in 0..300 {
        let mut b = good.clone();
        for _ in 0..(1 + next() % 4) {
            let i = (next() as usize) % b.len();
            match next() % 3 {
                0 => b[i] ^= 1 << (next() % 8),
                1 => b[i] = next() as u8,
                _ => b.truncate(i.max(1)),
            }
        }
        exercise_catalog_image(&b);
    }
}
