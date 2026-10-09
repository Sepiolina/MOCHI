//! Golden vectors for the C1 framing layer (ratification artifact R8, plan C1).
//!
//! Vectors are built here, deterministically, and checked in under
//! `fixtures/golden/c1/`. A test compares the builders against the files, so a
//! format-affecting change shows up as a specific, reviewable diff. Files are
//! rewritten only by the explicit, ignored `write_c1_golden_files` test, one
//! deliberate run at a time (AGENTS.md: never blanket regeneration).

use mochi_format::footer::encode_footer_frame;
use mochi_format::registry;
use mochi_format::ErrorClass;

/// What a correct reader must conclude about a vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// Frames from offset 0 have exactly these stored lengths and end cleanly at EOF.
    Frames(Vec<u64>),
    /// As `Frames`, and the footer at EOF validates with these fields.
    Footer {
        lens: Vec<u64>,
        commit_offset: u64,
        sequence: u64,
    },
    /// Walking from offset 0 yields these frames, then an error of this class.
    FramesThenError(Vec<u64>, ErrorClass),
    /// Frames walk cleanly (these lengths), but the footer at EOF is rejected.
    FooterRejected { lens: Vec<u64>, class: ErrorClass },
}

#[derive(Debug, Clone)]
pub struct Vector {
    pub name: &'static str,
    pub description: &'static str,
    pub bytes: Vec<u8>,
    pub expect: Expect,
}

fn skippable(magic: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = magic.to_le_bytes().to_vec();
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

fn data_frame(descriptor: u8, window: Option<u8>, rest: &[u8]) -> Vec<u8> {
    let mut v = registry::ZSTD_DATA_FRAME.to_le_bytes().to_vec();
    v.push(descriptor);
    v.extend(window);
    v.extend_from_slice(rest);
    v
}

fn block_header(size: u32, block_type: u32, last: bool) -> [u8; 3] {
    let word = (size << 3) | (block_type << 1) | u32::from(last);
    let b = word.to_le_bytes();
    [b[0], b[1], b[2]]
}

fn empty() -> Vec<u8> {
    data_frame(0x20, None, &[0x00, 0x01, 0x00, 0x00])
}

fn hello() -> Vec<u8> {
    let mut rest = vec![5];
    rest.extend_from_slice(&block_header(5, 0, true));
    rest.extend_from_slice(b"hello");
    data_frame(0x20, None, &rest)
}

fn rle() -> Vec<u8> {
    let mut rest = vec![10];
    rest.extend_from_slice(&block_header(10, 1, true));
    rest.push(b'A');
    data_frame(0x20, None, &rest)
}

fn two_block() -> Vec<u8> {
    let mut rest = block_header(3, 0, false).to_vec();
    rest.extend_from_slice(b"abc");
    rest.extend_from_slice(&block_header(4, 1, true));
    rest.push(b'z');
    data_frame(0x00, Some(0x00), &rest)
}

fn checksummed() -> Vec<u8> {
    // XXH64 of empty input, low 32 bits: 0x51D8E999.
    let mut f = data_frame(0x24, None, &[0x00, 0x01, 0x00, 0x00]);
    f.extend_from_slice(&0x51D8_E999u32.to_le_bytes());
    f
}

/// data, metadata delta, commit record, footer.
fn session() -> (Vec<u8>, u64, u64) {
    let mut bytes = hello();
    bytes.extend(skippable(registry::METADATA_DELTA, b"delta"));
    let commit_offset = bytes.len() as u64;
    let commit = skippable(registry::COMMIT_RECORD, b"commit-body");
    bytes.extend_from_slice(&commit);
    let footer_offset = bytes.len() as u64;
    bytes.extend_from_slice(&encode_footer_frame(commit_offset, 7, &commit));
    (bytes, commit_offset, footer_offset)
}

/// Every C1 vector, in a stable order.
pub fn c1_vectors() -> Vec<Vector> {
    let mut out = Vec::new();

    // The descriptor goes first: offset 0 is its only valid position (spec
    // Annex B.2 D12). Changed in the B.2 batch from ascending magic order,
    // which put it at offset 84.
    let mut all = skippable(
        registry::ARCHIVE_DESCRIPTOR,
        &registry::ARCHIVE_DESCRIPTOR.to_le_bytes(),
    );
    for magic in registry::SKIPPABLE_MIN..=registry::SKIPPABLE_MAX {
        if magic != registry::ARCHIVE_DESCRIPTOR {
            all.extend(skippable(magic, &magic.to_le_bytes()));
        }
    }
    out.push(Vector {
        name: "valid-skippable-all-registered",
        description: "one skippable frame for each of the 16 magics, including the three \
                      reserved ones; the descriptor first, at offset 0",
        bytes: all,
        expect: Expect::Frames(vec![12; 16]),
    });

    let mut misplaced = skippable(registry::COMMIT_RECORD, b"c");
    misplaced.extend(skippable(registry::ARCHIVE_DESCRIPTOR, b"desc"));
    out.push(Vector {
        name: "reject-descriptor-not-at-offset-0",
        description: "a well-formed descriptor frame at offset 9 (D12: only offset 0 is valid)",
        bytes: misplaced,
        expect: Expect::FramesThenError(vec![9], ErrorClass::Malformed),
    });

    for (name, description, bytes) in [
        (
            "valid-data-empty",
            "empty content, single segment, raw last block of size 0",
            empty(),
        ),
        (
            "valid-data-hello",
            "raw block, single segment, FCS 5",
            hello(),
        ),
        (
            "valid-data-rle",
            "RLE block regenerating 10 bytes from 1 stored byte",
            rle(),
        ),
        (
            "valid-data-two-block",
            "raw then RLE block, window 1 KiB",
            two_block(),
        ),
        (
            "valid-data-checksum",
            "content checksum flag set",
            checksummed(),
        ),
    ] {
        let len = bytes.len() as u64;
        out.push(Vector {
            name,
            description,
            bytes,
            expect: Expect::Frames(vec![len]),
        });
    }

    let (bytes, commit_offset, footer_offset) = session();
    let lens = vec![hello().len() as u64, 13, 19, 72];
    debug_assert_eq!(lens.iter().sum::<u64>(), bytes.len() as u64);
    debug_assert_eq!(footer_offset, bytes.len() as u64 - 72);
    out.push(Vector {
        name: "valid-session-with-footer",
        description: "data frame, metadata delta, commit record, framed footer (sequence 7)",
        bytes: bytes.clone(),
        expect: Expect::Footer {
            lens: lens.clone(),
            commit_offset,
            sequence: 7,
        },
    });

    // Footer digest mismatch: flip one byte inside the commit payload.
    let mut flipped = bytes.clone();
    flipped[commit_offset as usize + 10] ^= 0x01;
    out.push(Vector {
        name: "reject-footer-digest-mismatch",
        description: "one bit flipped inside the stored commit frame",
        bytes: flipped,
        expect: Expect::FooterRejected {
            lens: lens.clone(),
            class: ErrorClass::Footer,
        },
    });

    // v1.2-style bare trailer: footer payload with no skippable header.
    let mut bare = bytes[..footer_offset as usize].to_vec();
    bare.extend_from_slice(&bytes[footer_offset as usize + 8..]);
    out.push(Vector {
        name: "reject-footer-bare-trailer",
        description: "64 bare trailer bytes at EOF with no skippable header (the v1.2 layout)",
        bytes: bare,
        expect: Expect::FramesThenError(vec![hello().len() as u64, 13, 19], ErrorClass::Malformed),
    });

    // False magic inside a data payload: a forged footer embedded in a raw block.
    let forged_commit = skippable(registry::COMMIT_RECORD, b"forged");
    let mut content = forged_commit.clone();
    content.extend_from_slice(&encode_footer_frame(
        0,
        99,
        &forged_commit[..forged_commit.len() - 1],
    ));
    let mut rest = block_header(content.len() as u32, 0, true).to_vec();
    rest.extend_from_slice(&content);
    let false_magic = data_frame(0x00, Some(0x00), &rest);
    let len = false_magic.len() as u64;
    out.push(Vector {
        name: "reject-false-magic-inside-payload",
        description: "a forged footer inside a raw block: one frame, and no footer at EOF",
        bytes: false_magic,
        expect: Expect::FooterRejected {
            lens: vec![len],
            class: ErrorClass::Footer,
        },
    });

    out.push(Vector {
        name: "reject-reserved-block-type",
        description: "Block_Type 3",
        bytes: data_frame(0x20, None, &[0x00, 0b0000_0111, 0x00, 0x00]),
        expect: Expect::FramesThenError(vec![], ErrorClass::Malformed),
    });
    out.push(Vector {
        name: "reject-reserved-header-bit",
        description: "reserved bit set in Frame_Header_Descriptor",
        bytes: data_frame(0x28, None, &[0x00, 0x01, 0x00, 0x00]),
        expect: Expect::FramesThenError(vec![], ErrorClass::Malformed),
    });
    let mut too_big = block_header(2000, 0, true).to_vec();
    too_big.extend(std::iter::repeat_n(0u8, 2000));
    out.push(Vector {
        name: "reject-block-exceeds-maximum",
        description: "raw block of 2000 bytes in a frame whose window is 1 KiB",
        bytes: data_frame(0x00, Some(0x00), &too_big),
        expect: Expect::FramesThenError(vec![], ErrorClass::Malformed),
    });
    let mut huge = registry::METADATA_DELTA.to_le_bytes().to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    out.push(Vector {
        name: "reject-skippable-oversized",
        description: "Frame_Size 0xFFFFFFFF; rejected by the default payload limit before any read",
        bytes: huge,
        expect: Expect::FramesThenError(vec![], ErrorClass::LimitExceeded),
    });
    let h = hello();
    out.push(Vector {
        name: "reject-truncated-data-frame",
        description: "a data frame cut one byte short",
        bytes: h[..h.len() - 1].to_vec(),
        expect: Expect::FramesThenError(vec![], ErrorClass::Truncated),
    });
    out.push(Vector {
        name: "reject-not-a-frame",
        description: "bytes with no frame magic",
        bytes: vec![0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0],
        expect: Expect::FramesThenError(vec![], ErrorClass::Malformed),
    });

    out
}

// ---- C2: object and digest vectors ------------------------------------------
//
// Object vectors are hand-built frames, never libzstd compressor output, so
// they do not change when the zstd library does. Digest vectors are
// known-answer tests for every §9.2 scope with a single-input construction,
// under the **draft** separators (R3); they are what an independent reader
// (R9) checks itself against.

/// What a correct decoder must conclude about a stored object, given the
/// record's expected decoded length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectExpect {
    Decodes(Vec<u8>),
    Rejected(ErrorClass),
}

#[derive(Debug, Clone)]
pub struct ObjectVector {
    pub name: &'static str,
    pub description: &'static str,
    pub bytes: Vec<u8>,
    /// The record's decoded length.
    pub expected_len: u64,
    pub expect: ObjectExpect,
}

/// C2 frames carry the frame checksum (O21). Checksums are XXH64 low 32 bits,
/// derived independently (python `xxhash`) and cross-checked by libzstd
/// accepting every valid vector.
fn with_checksum(mut frame: Vec<u8>, xxh64_low32: u32) -> Vec<u8> {
    frame.extend_from_slice(&xxh64_low32.to_le_bytes());
    frame
}

/// `hello`: single segment, FCS 5, raw block, checksum.
fn hello_c2() -> Vec<u8> {
    let mut rest = vec![5];
    rest.extend_from_slice(&block_header(5, 0, true));
    rest.extend_from_slice(b"hello");
    with_checksum(data_frame(0x24, None, &rest), 0x889F_6DA3)
}

/// Every C2 object vector, in a stable order.
pub fn c2_object_vectors() -> Vec<ObjectVector> {
    let rle_c2 = {
        let mut rest = vec![10];
        rest.extend_from_slice(&block_header(10, 1, true));
        rest.push(b'A');
        with_checksum(data_frame(0x24, None, &rest), 0x2ACA_5533)
    };
    let two_block_blocks = {
        let mut b = block_header(3, 0, false).to_vec();
        b.extend_from_slice(b"abc");
        b.extend_from_slice(&block_header(4, 1, true));
        b.push(b'z');
        b
    };
    let two_block_c2 = {
        let mut rest = vec![7];
        rest.extend_from_slice(&two_block_blocks);
        with_checksum(data_frame(0x24, None, &rest), 0xAF03_B1D1)
    };
    let no_content_size =
        with_checksum(data_frame(0x04, Some(0x00), &two_block_blocks), 0xAF03_B1D1);
    // Single segment: the window *is* the content size, so a 6-byte block
    // under FCS 5 is structurally invalid (walker, spec §8.6).
    let block_exceeds_content_size = {
        let mut rest = vec![5];
        rest.extend_from_slice(&block_header(6, 0, true));
        rest.extend_from_slice(b"hello!");
        with_checksum(data_frame(0x24, None, &rest), 0xFCDF_F27B)
    };
    // Explicit 1 KiB window, 4-byte FCS = 5, one 6-byte raw block: structurally
    // valid, so only decoding reveals the output is longer than declared.
    let overlong = {
        let mut rest = 5u32.to_le_bytes().to_vec();
        rest.extend_from_slice(&block_header(6, 0, true));
        rest.extend_from_slice(b"hello!");
        with_checksum(data_frame(0x84, Some(0x00), &rest), 0xFCDF_F27B)
    };
    let dictionary_id = {
        // Descriptor: single segment, checksum, 1-byte Dictionary_ID; then FCS.
        let mut rest = vec![0x07, 5];
        rest.extend_from_slice(&block_header(5, 0, true));
        rest.extend_from_slice(b"hello");
        with_checksum(data_frame(0x25, None, &rest), 0x889F_6DA3)
    };
    let mut bad_checksum = hello_c2();
    if let Some(last) = bad_checksum.last_mut() {
        *last ^= 0x01;
    }
    let mut trailing = hello_c2();
    trailing.extend(checksummed());
    vec![
        ObjectVector {
            name: "valid-object-hello",
            description: "raw block, Frame_Content_Size 5, checksum",
            bytes: hello_c2(),
            expected_len: 5,
            expect: ObjectExpect::Decodes(b"hello".to_vec()),
        },
        ObjectVector {
            name: "valid-object-rle",
            description: "RLE block: one stored byte, ten decoded",
            bytes: rle_c2,
            expected_len: 10,
            expect: ObjectExpect::Decodes(vec![b'A'; 10]),
        },
        ObjectVector {
            name: "valid-object-two-block",
            description: "raw block then RLE block",
            bytes: two_block_c2,
            expected_len: 7,
            expect: ObjectExpect::Decodes(b"abczzzz".to_vec()),
        },
        ObjectVector {
            name: "valid-object-empty-checksummed",
            description: "empty content with a correct frame checksum",
            bytes: checksummed(),
            expected_len: 0,
            expect: ObjectExpect::Decodes(Vec::new()),
        },
        ObjectVector {
            name: "reject-object-no-content-size",
            description: "valid Zstandard frame without Frame_Content_Size (O21)",
            bytes: no_content_size,
            expected_len: 7,
            expect: ObjectExpect::Rejected(ErrorClass::Malformed),
        },
        ObjectVector {
            name: "reject-object-no-checksum",
            description: "valid Zstandard frame without the frame checksum (O21)",
            bytes: hello(),
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::Malformed),
        },
        ObjectVector {
            name: "reject-object-declared-size-mismatch",
            description: "Frame_Content_Size 5, record says 4",
            bytes: hello_c2(),
            expected_len: 4,
            expect: ObjectExpect::Rejected(ErrorClass::ContentIntegrity),
        },
        ObjectVector {
            name: "reject-object-block-exceeds-content-size",
            description: "single segment, FCS 5, 6-byte block: structurally invalid",
            bytes: block_exceeds_content_size,
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::Malformed),
        },
        ObjectVector {
            name: "reject-object-overlong-output",
            description: "1 KiB window, FCS 5, 6-byte raw block: caught only by decoding",
            bytes: overlong,
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::ContentIntegrity),
        },
        ObjectVector {
            name: "reject-object-bad-checksum",
            description: "frame checksum does not match content",
            bytes: bad_checksum,
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::ContentIntegrity),
        },
        ObjectVector {
            name: "reject-object-dictionary-id",
            description: "frame declares Dictionary_ID 7; record has no dictionary (O21)",
            bytes: dictionary_id,
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::ContentIntegrity),
        },
        ObjectVector {
            name: "reject-object-trailing-frame",
            description: "a second frame after the object",
            bytes: trailing,
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::Malformed),
        },
        ObjectVector {
            name: "reject-object-skippable",
            description: "a skippable frame is not a data object",
            bytes: skippable(registry::METADATA_DELTA, b"hello"),
            expected_len: 5,
            expect: ObjectExpect::Rejected(ErrorClass::Malformed),
        },
    ]
}

/// One known-answer digest.
#[derive(Debug, Clone)]
pub struct DigestVector {
    /// `Digest::scope_name()` of the scope.
    pub scope: &'static str,
    pub input: Vec<u8>,
    pub digest: [u8; 32],
}

/// Known answers for every single-input digest scope (five, per O20) over three inputs: empty,
/// `abc`, and 1025 bytes (crossing BLAKE3's 1024-byte chunk boundary). The
/// footer digest is covered by the C1 footer vectors.
pub fn c2_digest_vectors() -> Vec<DigestVector> {
    use mochi_format::digest::*;
    use mochi_format::repr::{
        CanonicalCommitBody, DecodedBytes, DecodedSlice, DictionaryBytes, StoredObjectBytes,
    };

    let inputs = [
        Vec::new(),
        b"abc".to_vec(),
        crate::deterministic_bytes(0xC2, 1025),
    ];
    let mut out = Vec::new();
    for input in inputs {
        let i = &input[..];
        let rows: [(&'static str, [u8; 32]); 5] = [
            (
                FileContentHash::scope_name(),
                *file_content_hash(DecodedSlice::from_logical(i)).as_bytes(),
            ),
            (
                ChunkContentHash::scope_name(),
                *chunk_content_hash(&DecodedBytes::new(input.clone())).as_bytes(),
            ),
            (
                StoredObjectHash::scope_name(),
                *stored_object_hash(StoredObjectBytes::new(i)).as_bytes(),
            ),
            (
                DictionaryHash::scope_name(),
                *dictionary_hash(&DictionaryBytes::new(input.clone())).as_bytes(),
            ),
            (
                CommitId::scope_name(),
                *commit_id(CanonicalCommitBody::assume_canonical(i)).as_bytes(),
            ),
        ];
        for (scope, digest) in rows {
            out.push(DigestVector {
                scope,
                input: input.clone(),
                digest,
            });
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".to_string();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The checked-in `fixtures/golden/c2/vectors.txt`: digest known answers,
/// then each object vector's record fields and expected outcome.
pub fn render_c2_manifest() -> String {
    use mochi_format::digest::{chunk_content_hash, stored_object_hash};
    use mochi_format::repr::{DecodedBytes, StoredObjectBytes};

    let mut s = String::from(
        "# MOCHI C2 golden vectors. DRAFT: separators are ratification item R3.\n\
         # Built by mochi_testkit::golden; do not edit by hand.\n\
         #\n\
         # digest <scope> <input-hex|-> <blake3-256-hex>\n\
         #   file-content: BLAKE3-256(input), equal to b3sum (plan O20)\n\
         #   others: BLAKE3-256(\"MOCHI2-<SCOPE>\\0\" || input)\n\
         # object <name> <record-decoded-len> decodes <chunk-content-hex> <stored-object-hex>\n\
         # object <name> <record-decoded-len> rejects <error-class>\n",
    );
    for v in c2_digest_vectors() {
        s.push_str(&format!(
            "digest {} {} {}\n",
            v.scope,
            hex(&v.input),
            hex(&v.digest)
        ));
    }
    for v in c2_object_vectors() {
        match &v.expect {
            ObjectExpect::Decodes(content) => s.push_str(&format!(
                "object {} {} decodes {} {}\n",
                v.name,
                v.expected_len,
                hex(chunk_content_hash(&DecodedBytes::new(content.clone())).as_bytes()),
                hex(stored_object_hash(StoredObjectBytes::new(&v.bytes)).as_bytes()),
            )),
            ObjectExpect::Rejected(class) => s.push_str(&format!(
                "object {} {} rejects {class:?}\n",
                v.name, v.expected_len
            )),
        }
    }
    s
}

// ---- C4: recovery-manifest vectors (schema 1) -------------------------------------------
//
// Canonical encoding makes these byte-exact: each file must equal its
// builder. Records are hand-made (no libzstd output), so they do not change
// with the zstd library. Reject vectors are a valid manifest with exactly one
// targeted defect, made by editing the decoded CBOR tree.
//
// Schema 1 (docs/schemas/recovery-manifest-v1.cddl). The three valid vectors
// describe two commits: delta(0), then commit 1 as a checkpoint, which binds
// both delta(1) (linked to delta(0)) and the snapshot S(1) (no parent; same
// sequence and transaction ID as delta(1)).

use mochi_core::catalog::extent::{Extent, ExtentSource};
use mochi_core::catalog::namespace::{EntryKind, FileVersionId, NamespaceOp};
use mochi_core::catalog::path::ArchivePath;
use mochi_core::catalog::FileVersion;
use mochi_core::manifest::Provenance;
use mochi_core::manifest::{
    Attributes, ChunkEntry, FileVersionEntry, Manifest, ManifestKind, Mtime, ParentLink,
    PosixAttributes, WINDOWS_ARCHIVE, WINDOWS_READONLY,
};
use mochi_core::object::{ArchiveId, ObjectId, ObjectRecord};
use mochi_core::retention::RetentionOp;
use mochi_format::cbor::{self as cbor_codec, Value};
use mochi_format::codec::{Encoding, Protection};
use mochi_format::digest::CommitId;
use mochi_format::digest::{
    stored_object_hash, ChunkContentHash, FileContentHash, StoredObjectHash,
};
use mochi_format::frame::encode_skippable_frame;
use mochi_format::registry::FrameKind;
use mochi_format::repr::StoredObjectBytes as C4StoredBytes;

/// What a correct reader must conclude about a manifest vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestExpect {
    Valid,
    /// Rejected with this core error code name.
    Rejected(&'static str),
}

#[derive(Debug, Clone)]
pub struct ManifestVector {
    pub name: &'static str,
    pub description: &'static str,
    /// The complete stored object (one frame).
    pub bytes: Vec<u8>,
    pub expect: ManifestExpect,
}

fn c4_path(s: &str) -> ArchivePath {
    ArchivePath::from_stored(s.as_bytes()).expect("fixture path")
}

const C4_ARCHIVE: ArchiveId = ArchiveId::from_bytes([0xA1; 32]);
const C4_CHUNK: ObjectId = ObjectId::from_bytes([0xC4; 32]);
const C4_DIR: FileVersionId = FileVersionId::from_bytes([0xD4; 32]);
const C4_FILE: FileVersionId = FileVersionId::from_bytes([0xF4; 32]);

fn c4_txid(n: u8) -> [u8; 16] {
    mochi_core::commit::uuid_v4([n; 16])
}

fn c4_root() -> Manifest {
    let file_attrs = Attributes {
        posix: Some(PosixAttributes {
            mode: 0o644,
            uid: 1000,
            gid: 100,
        }),
        windows: Some(WINDOWS_READONLY | WINDOWS_ARCHIVE),
        // Before 1970: exercises a negative CBOR integer.
        mtime: Some(Mtime {
            secs: -86_400,
            nanos: 123_456_789,
        }),
    };
    Manifest {
        archive_id: C4_ARCHIVE,
        commit_seq: 0,
        transaction_id: c4_txid(0x70),
        parent: None,
        kind: ManifestKind::Delta,
        chunks: vec![ChunkEntry {
            record: ObjectRecord {
                id: C4_CHUNK,
                encoding: Encoding::ZstdFrame,
                protection: Protection::None,
                stored_len: 321,
                stored_hash: StoredObjectHash::from_bytes([0x51; 32]),
                decoded_len: 4096,
                content_hash: ChunkContentHash::from_bytes([0x52; 32]),
                dependencies: vec![],
            },
            location: Some(0),
        }],
        file_versions: vec![
            FileVersionEntry {
                version: FileVersion {
                    id: C4_DIR,
                    kind: EntryKind::Directory,
                    logical_len: 0,
                    content_hash: None,
                },
                extents: vec![],
                attributes: Attributes {
                    posix: Some(PosixAttributes {
                        mode: 0o755,
                        uid: 0,
                        gid: 0,
                    }),
                    windows: None,
                    mtime: None,
                },
            },
            FileVersionEntry {
                version: FileVersion {
                    id: C4_FILE,
                    kind: EntryKind::File,
                    logical_len: 10_000,
                    content_hash: Some(FileContentHash::from_bytes([0x53; 32])),
                },
                extents: vec![
                    Extent {
                        ordinal: 0,
                        logical_offset: 0,
                        length: 4096,
                        source: ExtentSource::Chunk {
                            chunk: C4_CHUNK,
                            chunk_offset: 0,
                        },
                    },
                    Extent {
                        ordinal: 1,
                        logical_offset: 4096,
                        length: 5904,
                        source: ExtentSource::Hole,
                    },
                ],
                attributes: file_attrs,
            },
        ],
        ops: vec![
            NamespaceOp::Put {
                path: c4_path("docs/sparse.bin"),
                version: C4_FILE,
            },
            NamespaceOp::Put {
                path: c4_path("docs"),
                version: C4_DIR,
            },
        ],
        entries: vec![],
        required_features: vec![],
        retention_ops: Vec::new(),
        retention: Default::default(),
        provenance: None,
        keys: Default::default(),
    }
}

fn c4_frame(m: &Manifest) -> Vec<u8> {
    m.to_stored().expect("fixture manifest").as_bytes().to_vec()
}

/// Delta of commit 1: a rename, linked to delta(0) by hash.
fn c4_child(parent_delta_hash: StoredObjectHash) -> Manifest {
    Manifest {
        archive_id: C4_ARCHIVE,
        commit_seq: 1,
        transaction_id: c4_txid(0x71),
        parent: Some(ParentLink {
            seq: 0,
            delta_manifest_hash: parent_delta_hash,
        }),
        kind: ManifestKind::Delta,
        chunks: vec![],
        file_versions: vec![],
        ops: vec![
            NamespaceOp::Delete {
                path: c4_path("docs/sparse.bin"),
            },
            NamespaceOp::Put {
                path: c4_path("docs/renamed.bin"),
                version: C4_FILE,
            },
        ],
        entries: vec![],
        required_features: vec![],
        retention_ops: Vec::new(),
        retention: Default::default(),
        provenance: None,
        keys: Default::default(),
    }
}

/// Snapshot S(1): the complete state after commit 1. No parent; identity
/// equals commit 1's (same sequence and transaction ID as `c4_child`).
fn c4_snapshot() -> Manifest {
    let root = c4_root();
    Manifest {
        archive_id: C4_ARCHIVE,
        commit_seq: 1,
        transaction_id: c4_txid(0x71),
        parent: None,
        kind: ManifestKind::Snapshot,
        chunks: root.chunks,
        file_versions: root.file_versions,
        ops: vec![],
        entries: vec![
            (c4_path("docs"), C4_DIR),
            (c4_path("docs/renamed.bin"), C4_FILE),
        ],
        required_features: vec![],
        retention_ops: Vec::new(),
        retention: Default::default(),
        provenance: None,
        keys: Default::default(),
    }
}

/// Edit the payload's CBOR tree and re-frame it.
fn c4_edit(m: &Manifest, edit: impl FnOnce(&mut Vec<(u64, Value)>)) -> Vec<u8> {
    let payload = m.encode().expect("fixture manifest");
    let Ok(Value::Map(mut root)) = cbor_codec::decode(&payload, &Default::default()) else {
        unreachable!("a manifest is a map")
    };
    edit(&mut root);
    root.sort_by_key(|(k, _)| *k);
    let bytes = cbor_codec::encode(&Value::Map(root)).expect("edited tree stays encodable");
    encode_skippable_frame(FrameKind::RecoveryManifest, &bytes).expect("fits a frame")
}

/// The second file version's map (the file, sorted after the directory).
fn c4_first_version(root: &mut [(u64, Value)]) -> &mut Vec<(u64, Value)> {
    match c4_field(root, 6) {
        Value::Array(vs) => match &mut vs[1] {
            Value::Map(m) => m,
            _ => unreachable!("file versions are maps"),
        },
        _ => unreachable!("key 6 is an array"),
    }
}

fn c4_field(root: &mut [(u64, Value)], key: u64) -> &mut Value {
    &mut root
        .iter_mut()
        .find(|(k, _)| *k == key)
        .expect("fixture key")
        .1
}

fn parent_value(seq: u64, hash: &StoredObjectHash) -> Value {
    Value::Map(vec![
        (0, Value::Uint(seq)),
        (1, Value::Bytes(hash.as_bytes().to_vec())),
    ])
}

/// The schema-0 encoding of `c4_root()`: the same tree without keys 9 and 10,
/// with schema version 0. Byte-identical to the schema-0 golden file
/// `valid-manifest-root-delta.bin` it replaces (checked when regenerated).
pub fn c4_legacy_v0_root() -> Vec<u8> {
    c4_edit(&c4_root(), |r| {
        r.retain(|(k, _)| *k != 9 && *k != 10);
        *c4_field(r, 0) = Value::Uint(0);
    })
}

/// Every C4 manifest vector, in a stable order.
pub fn c4_manifest_vectors() -> Vec<ManifestVector> {
    let root = c4_root();
    let root_bytes = c4_frame(&root);
    let root_hash = stored_object_hash(C4StoredBytes::new(&root_bytes));
    let child = c4_child(root_hash);
    let child_bytes = c4_frame(&child);
    let snapshot = c4_snapshot();
    let snapshot_bytes = c4_frame(&snapshot);
    let mut retention_delta = child.clone();
    retention_delta.retention_ops = vec![
        RetentionOp::Hold {
            label: b"legal".to_vec(),
            seq: 1,
        },
        RetentionOp::Expire { seq: 0 },
    ];
    let mut provenance_root = root.clone();
    provenance_root.provenance = Some(Provenance {
        source_archive_id: ArchiveId::from_bytes([0x5A; 32]),
        commits: vec![
            (2, CommitId::from_bytes([0xC2; 32])),
            (5, CommitId::from_bytes([0xC5; 32])),
        ],
        collected: vec![0, 1, 3, 4],
        reason: mochi_core::manifest::RewriteReason::Collection,
    });
    let mut retention_snapshot = snapshot.clone();
    retention_snapshot.retention.expired.insert(0);
    retention_snapshot
        .retention
        .holds
        .insert(b"legal".to_vec(), 1);

    let mut v = vec![
        ManifestVector {
            name: "valid-manifest-root-delta",
            description: "delta(0): a directory and a sparse file with attributes (negative mtime)",
            bytes: root_bytes.clone(),
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-manifest-child-delta",
            description: "delta(1): rename, linked to delta(0) by hash",
            bytes: child_bytes,
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-manifest-snapshot",
            description: "S(1): self-contained, no parent, identity of commit 1",
            bytes: snapshot_bytes,
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-manifest-retention-delta",
            description: "schema 2: delta(1) also holding snapshot 1 and expiring snapshot 0",
            bytes: c4_frame(&retention_delta),
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-manifest-provenance-root-delta",
            description: "schema 2: delta(0) of a compacted archive with its provenance",
            bytes: c4_frame(&provenance_root),
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-manifest-retention-snapshot",
            description: "schema 2: S(1) with snapshot 0 expired and hold \"legal\" on 1",
            bytes: c4_frame(&retention_snapshot),
            expect: ManifestExpect::Valid,
        },
    ];
    let reject = |name, description, bytes, code| ManifestVector {
        name,
        description,
        bytes,
        expect: ManifestExpect::Rejected(code),
    };

    // ---- retention (schema 2, C9)
    let ops = |r: &mut Vec<(u64, Value)>, ops: Vec<Value>| *c4_field(r, 11) = Value::Array(ops);
    let u = |n: u64| Value::Uint(n);
    let b = |x: &[u8]| Value::Bytes(x.to_vec());
    v.push(reject(
        "reject-manifest-retention-schema-2-empty",
        "schema 2 with no retention data (schema 2 is used exactly when there is some)",
        c4_edit(&child, |r| {
            *c4_field(r, 0) = u(2);
            r.push((11, Value::Array(vec![])));
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-schema-2-missing-key",
        "schema 2 without key 11",
        c4_edit(&child, |r| *c4_field(r, 0) = u(2)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-expire-self",
        "delta(1) expires snapshot 1: only earlier snapshots can expire",
        c4_edit(&retention_delta, |r| {
            ops(r, vec![Value::Array(vec![u(0), u(1)])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-hold-future",
        "delta(1) holds snapshot 2",
        c4_edit(&retention_delta, |r| {
            ops(r, vec![Value::Array(vec![u(1), b(b"legal"), u(2)])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-empty-label",
        "a hold with an empty label",
        c4_edit(&retention_delta, |r| {
            ops(r, vec![Value::Array(vec![u(1), b(b""), u(1)])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-unknown-op",
        "retention operation kind 3",
        c4_edit(&retention_delta, |r| {
            ops(r, vec![Value::Array(vec![u(3), u(0)])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-state-in-delta",
        "a delta whose key 11 is a retention state, not operations",
        c4_edit(&retention_delta, |r| {
            *c4_field(r, 11) = Value::Map(vec![
                (0, Value::Array(vec![u(0)])),
                (1, Value::Array(vec![])),
            ])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-ops-in-snapshot",
        "a snapshot whose key 11 is operations, not a state",
        c4_edit(&retention_snapshot, |r| {
            ops(r, vec![Value::Array(vec![u(0), u(0)])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-expired-duplicate",
        "S(1) lists expired snapshot 0 twice",
        c4_edit(&retention_snapshot, |r| {
            *c4_field(r, 11) = Value::Map(vec![
                (0, Value::Array(vec![u(0), u(0)])),
                (1, Value::Array(vec![])),
            ])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-holds-unsorted",
        "S(1) lists holds out of label order",
        c4_edit(&retention_snapshot, |r| {
            *c4_field(r, 11) = Value::Map(vec![
                (0, Value::Array(vec![])),
                (
                    1,
                    Value::Array(vec![
                        Value::Array(vec![b(b"b"), u(1)]),
                        Value::Array(vec![b(b"a"), u(1)]),
                    ]),
                ),
            ])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-retention-expired-not-earlier",
        "S(1) with snapshot 1 expired",
        c4_edit(&retention_snapshot, |r| {
            *c4_field(r, 11) = Value::Map(vec![
                (0, Value::Array(vec![u(1)])),
                (1, Value::Array(vec![])),
            ])
        }),
        "RECORD_INVALID",
    ));

    // ---- provenance (schema 2, C9)
    let prov = |r: &mut Vec<(u64, Value)>, commits: Vec<(u64, u8)>, collected: Vec<u64>| {
        let v = Value::Map(vec![
            (0, Value::Bytes(vec![0x5A; 32])),
            (
                1,
                Value::Array(
                    commits
                        .into_iter()
                        .map(|(s, b)| Value::Array(vec![u(s), Value::Bytes(vec![b; 32])]))
                        .collect(),
                ),
            ),
            (2, Value::Array(collected.into_iter().map(u).collect())),
        ]);
        *c4_field(r, 12) = v;
    };
    v.push(reject(
        "reject-manifest-provenance-not-root",
        "delta(1) carrying provenance",
        c4_edit(&retention_delta, |r| {
            r.push((12, Value::Null));
            prov(r, vec![(5, 0xC5)], vec![]);
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-provenance-empty",
        "provenance naming no source commit",
        c4_edit(&provenance_root, |r| prov(r, vec![], vec![])),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-provenance-unsorted",
        "source commits out of order",
        c4_edit(&provenance_root, |r| {
            prov(r, vec![(5, 0xC5), (2, 0xC2)], vec![])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-provenance-collected-kept",
        "snapshot 2 both kept and collected",
        c4_edit(&provenance_root, |r| {
            prov(r, vec![(2, 0xC2), (5, 0xC5)], vec![2])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-provenance-collected-head",
        "the source head listed as collected",
        c4_edit(&provenance_root, |r| {
            prov(r, vec![(2, 0xC2), (5, 0xC5)], vec![5])
        }),
        "RECORD_INVALID",
    ));

    // ---- closed schema and versions
    v.push(reject(
        "reject-manifest-unknown-key",
        "an extra top-level key 11",
        c4_edit(&root, |r| r.push((11, Value::Null))),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-missing-key",
        "top-level key 8 (entries) removed",
        c4_edit(&root, |r| r.retain(|(k, _)| *k != 8)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-missing-transaction-id",
        "top-level key 10 (transaction ID) removed",
        c4_edit(&root, |r| r.retain(|(k, _)| *k != 10)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-short-transaction-id",
        "a 15-byte transaction ID",
        c4_edit(&root, |r| *c4_field(r, 10) = Value::Bytes(vec![0x40; 15])),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-schema-version",
        "schema version 3 (schema 2 adds retention, C9)",
        c4_edit(&root, |r| *c4_field(r, 0) = Value::Uint(3)),
        "UNSUPPORTED_FEATURE",
    ));
    v.push(reject(
        "reject-manifest-legacy-v0",
        "the schema-0 draft encoding of delta(0) (pre-batch legacy, spec §26)",
        c4_legacy_v0_root(),
        "UNSUPPORTED_FEATURE",
    ));
    // ---- D11 required features
    v.push(reject(
        "reject-manifest-required-feature",
        "requires feature 1, which no build defines",
        c4_edit(&root, |r| {
            *c4_field(r, 9) = Value::Array(vec![Value::Uint(1)])
        }),
        "UNSUPPORTED_FEATURE",
    ));
    v.push(reject(
        "reject-manifest-features-not-increasing",
        "required features [2, 1]",
        c4_edit(&root, |r| {
            *c4_field(r, 9) = Value::Array(vec![Value::Uint(2), Value::Uint(1)])
        }),
        "ENVELOPE_INVALID",
    ));
    // ---- entries
    v.push(reject(
        "reject-manifest-symlink-kind",
        "entry kind 2 (symbolic link) before C6",
        c4_edit(&root, |r| {
            let fv = c4_first_version(r);
            fv.iter_mut().find(|(k, _)| *k == 1).expect("kind").1 = Value::Uint(2);
        }),
        "UNSUPPORTED_FEATURE",
    ));
    v.push(reject(
        "reject-manifest-windows-bits",
        "Windows attribute bit outside the promised set",
        c4_edit(&root, |r| {
            let fv = c4_first_version(r);
            if let Value::Map(a) = &mut fv.iter_mut().find(|(k, _)| *k == 5).expect("attrs").1 {
                a.iter_mut().find(|(k, _)| *k == 1).expect("windows").1 = Value::Uint(0x400);
            }
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-mode-bits",
        "POSIX mode beyond 0o7777",
        c4_edit(&root, |r| {
            let fv = c4_first_version(r);
            if let Value::Map(a) = &mut fv.iter_mut().find(|(k, _)| *k == 5).expect("attrs").1 {
                if let Value::Array(t) = &mut a.iter_mut().find(|(k, _)| *k == 0).expect("posix").1
                {
                    t[0] = Value::Uint(0o100644);
                }
            }
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-unsorted-versions",
        "file versions out of ID order",
        c4_edit(&root, |r| {
            if let Value::Array(vs) = c4_field(r, 6) {
                vs.swap(0, 1);
            }
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-traversal-path",
        "an operation path with '..'",
        c4_edit(&root, |r| {
            if let Value::Array(ops) = c4_field(r, 7) {
                if let Value::Array(op) = &mut ops[0] {
                    op[1] = Value::Bytes(b"docs/../x".to_vec());
                }
            }
        }),
        "PATH_INVALID",
    ));
    // ---- delta shape and parent rule
    v.push(reject(
        "reject-manifest-orphan-delta",
        "delta(1) with no parent",
        c4_edit(&root, |r| *c4_field(r, 2) = Value::Uint(1)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-root-delta-with-parent",
        "delta(0) with a parent link",
        c4_edit(&root, |r| *c4_field(r, 3) = parent_value(0, &root_hash)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-parent-seq-gap",
        "delta(2) whose parent link names sequence 0 (must be 1)",
        c4_edit(&child, |r| *c4_field(r, 2) = Value::Uint(2)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-delta-with-entries",
        "delta(1) with a snapshot entry in key 8",
        c4_edit(&child, |r| {
            *c4_field(r, 8) = Value::Array(vec![Value::Array(vec![
                Value::Bytes(b"docs".to_vec()),
                Value::Bytes(C4_DIR.as_bytes().to_vec()),
            ])])
        }),
        "RECORD_INVALID",
    ));
    // ---- snapshot shape
    v.push(reject(
        "reject-manifest-snapshot-with-parent",
        "S(1) with a parent link to delta(0)",
        c4_edit(&snapshot, |r| *c4_field(r, 3) = parent_value(0, &root_hash)),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-snapshot-with-ops",
        "S(1) with a namespace operation in key 7",
        c4_edit(&snapshot, |r| {
            *c4_field(r, 7) = Value::Array(vec![Value::Array(vec![
                Value::Uint(1),
                Value::Bytes(b"docs".to_vec()),
            ])])
        }),
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-snapshot-not-self-contained",
        "S(1) entry names a version it does not contain",
        c4_edit(&snapshot, |r| {
            if let Value::Array(vs) = c4_field(r, 6) {
                vs.pop();
            }
        }),
        "RECORD_INVALID",
    ));
    // ---- encoding and framing
    v.push(reject(
        "reject-manifest-noncanonical-cbor",
        "a key in non-shortest form",
        {
            // Payload starts with a map header; replace the small-integer key 0
            // (0x00) by its two-byte form (0x18 0x00).
            let payload = root.encode().expect("fixture");
            let mut p = vec![payload[0], 0x18];
            p.extend_from_slice(&payload[1..]);
            encode_skippable_frame(FrameKind::RecoveryManifest, &p).expect("frame")
        },
        "RECORD_INVALID",
    ));
    v.push(reject(
        "reject-manifest-wrong-frame-kind",
        "a valid payload in a METADATA_DELTA frame",
        {
            encode_skippable_frame(FrameKind::MetadataDelta, &root.encode().expect("fixture"))
                .expect("frame")
        },
        "MALFORMED_FRAME",
    ));
    v.push(reject(
        "reject-manifest-trailing-frame",
        "a second frame after the manifest",
        {
            let mut b = root_bytes.clone();
            b.extend_from_slice(&root_bytes);
            b
        },
        "MALFORMED_FRAME",
    ));
    // Added for G1 (T30): D11 "Payload length", CBOR: the whole payload is
    // consumed. Appended last so earlier vectors.txt lines stay identical.
    v.push(reject(
        "reject-manifest-payload-not-consumed",
        "one byte after the manifest's CBOR item, inside the frame",
        {
            let mut p = root.encode().expect("fixture");
            p.push(0x00);
            encode_skippable_frame(FrameKind::RecoveryManifest, &p).expect("frame")
        },
        "RECORD_INVALID",
    ));
    v
}

/// The checked-in `fixtures/golden/c4/vectors.txt`.
pub fn render_c4_manifest() -> String {
    let mut s = String::from(
        "# MOCHI C4 recovery-manifest vectors. DRAFT schema 1 (R3); docs/schemas/recovery-manifest-v1.cddl.\n\
         # Built by mochi_testkit::golden; do not edit by hand.\n\
         # <name> valid <stored-object-hash>   |   <name> rejects <core error code>\n",
    );
    for v in c4_manifest_vectors() {
        match v.expect {
            ManifestExpect::Valid => s.push_str(&format!(
                "{} valid {}\n",
                v.name,
                hex(stored_object_hash(C4StoredBytes::new(&v.bytes)).as_bytes())
            )),
            ManifestExpect::Rejected(code) => s.push_str(&format!("{} rejects {code}\n", v.name)),
        }
    }
    s
}

// ---- C5: commit records (schema 1) --------------------------------------------------------
//
// Hand-made commit records (spec §12.1, Annex B.2 D10–D12;
// docs/schemas/commit-record-v1.cddl). Canonical encoding makes byte
// comparison with the builders meaningful. Reject vectors carry exactly one
// defect each. Where the defect is in the body, the stored ID is
// *recomputed* so the vector exercises the targeted rule rather than the ID
// check, unless the ID check is the target.
//
// Valid: a root checkpoint, a child checkpoint, and a delta on the child
// (the base rule makes the child its base, since the parent is a checkpoint).

use mochi_core::commit::{uuid_v4, CommitLink, CommitRecord, Metadata, ObjectRef};
use mochi_format::digest::commit_id as c5_commit_id;
use mochi_format::repr::CanonicalCommitBody;

const C5_ARCHIVE: ArchiveId = ArchiveId::from_bytes([0xA5; 32]);

fn c5_ref(offset: u64, stored_len: u64, fill: u8) -> ObjectRef {
    ObjectRef {
        offset,
        stored_len,
        stored_hash: StoredObjectHash::from_bytes([fill; 32]),
    }
}

/// Plausible layout: descriptor 0..56, content, delta(0) 120..300,
/// S(0) 300..500, image 500..4604, commit at 4604.
fn c5_root() -> CommitRecord {
    CommitRecord {
        archive_id: C5_ARCHIVE,
        seq: 0,
        transaction_id: uuid_v4([0x11; 16]),
        parent: None,
        metadata: Metadata::Checkpoint {
            image: c5_ref(500, 4104, 0x3A),
            snapshot: c5_ref(300, 200, 0x3C),
        },
        delta_manifest: c5_ref(120, 180, 0x3B),
        required_features: vec![],
        time: Some(Mtime {
            secs: 1_700_000_000,
            nanos: 1,
        }),
        descriptor: c5_ref(0, 56, 0x3D),
        key_envelopes: Vec::new(),
        data_region: None,
    }
}

fn c5_child(parent: CommitId) -> CommitRecord {
    CommitRecord {
        seq: 1,
        transaction_id: uuid_v4([0x22; 16]),
        parent: Some(CommitLink {
            commit_id: parent,
            seq: 0,
            footer_offset: 5100,
        }),
        metadata: Metadata::Checkpoint {
            image: c5_ref(5800, 8200, 0x4A),
            snapshot: c5_ref(5500, 300, 0x4C),
        },
        delta_manifest: c5_ref(5172, 316, 0x4B),
        time: None,
        ..c5_root()
    }
}

fn c5_delta(parent: CommitId) -> CommitRecord {
    let link = CommitLink {
        commit_id: parent,
        seq: 1,
        footer_offset: 14_400,
    };
    CommitRecord {
        seq: 2,
        transaction_id: uuid_v4([0x33; 16]),
        parent: Some(link),
        metadata: Metadata::Delta { base: link },
        delta_manifest: c5_ref(14_500, 240, 0x5B),
        time: None,
        ..c5_root()
    }
}

fn c5_frame(r: &CommitRecord) -> Vec<u8> {
    r.to_stored().expect("fixture commit").0.as_bytes().to_vec()
}

/// Recompute key 9 from the tree without it.
fn c5_reid(root: &mut Vec<(u64, Value)>) {
    root.retain(|(k, _)| *k != 9);
    root.sort_by_key(|(k, _)| *k);
    let body = cbor_codec::encode(&Value::Map(root.clone())).expect("encodable");
    let id = c5_commit_id(CanonicalCommitBody::assume_canonical(&body));
    root.push((9, Value::Bytes(id.as_bytes().to_vec())));
}

/// Edit the record's CBOR tree. With `reid`, recompute key 9 from the
/// edited body so only the targeted rule is violated.
fn c5_edit(r: &CommitRecord, reid: bool, edit: impl FnOnce(&mut Vec<(u64, Value)>)) -> Vec<u8> {
    let (payload, _) = r.encode().expect("fixture commit");
    let Ok(Value::Map(mut root)) = cbor_codec::decode(&payload, &Default::default()) else {
        unreachable!("a commit is a map")
    };
    edit(&mut root);
    if reid {
        c5_reid(&mut root);
    }
    root.sort_by_key(|(k, _)| *k);
    let bytes = cbor_codec::encode(&Value::Map(root)).expect("encodable");
    encode_skippable_frame(FrameKind::CommitRecord, &bytes).expect("fits a frame")
}

fn c5_set(root: &mut [(u64, Value)], key: u64, v: Value) {
    *c4_field(root, key) = v;
}

/// The metadata map (key 5) of a record tree.
fn c5_metadata(root: &mut [(u64, Value)]) -> &mut Vec<(u64, Value)> {
    match c4_field(root, 5) {
        Value::Map(m) => m,
        _ => unreachable!("key 5 is a map"),
    }
}

fn c5_ref_value(r: &ObjectRef) -> Value {
    Value::Map(vec![
        (0, Value::Uint(r.offset)),
        (1, Value::Uint(r.stored_len)),
        (2, Value::Bytes(r.stored_hash.as_bytes().to_vec())),
    ])
}

/// The schema-0 draft root commit, rebuilt from its CDDL
/// (`commit-record-v0.cddl`) with the schema-0 fixture's values. Byte-identical
/// to the schema-0 golden file `valid-commit-root.bin` it replaces (checked
/// when regenerated). The ID uses the same separator and body rule.
pub fn c5_legacy_v0_root() -> Vec<u8> {
    let mut root = vec![
        (0, Value::Uint(0)),
        (1, Value::Bytes(C5_ARCHIVE.as_bytes().to_vec())),
        (2, Value::Uint(0)),
        (3, Value::Bytes(uuid_v4([0x11; 16]).to_vec())),
        (4, Value::Null),
        (
            5,
            Value::Map(vec![
                (0, Value::Uint(0)),
                (1, Value::Uint(300)),
                (2, Value::Uint(4104)),
                (3, Value::Bytes(vec![0x3A; 32])),
            ]),
        ),
        (6, c5_ref_value(&c5_ref(120, 180, 0x3B))),
        (7, Value::Array(vec![])),
        (
            8,
            Value::Array(vec![Value::Uint(1_700_000_000), Value::Uint(1)]),
        ),
    ];
    c5_reid(&mut root);
    let bytes = cbor_codec::encode(&Value::Map(root)).expect("encodable");
    encode_skippable_frame(FrameKind::CommitRecord, &bytes).expect("fits a frame")
}

/// Every C5 commit-record vector, in a stable order.
pub fn c5_commit_vectors() -> Vec<ManifestVector> {
    let root = c5_root();
    let root_id = root.commit_id().expect("fixture");
    let child = c5_child(root_id);
    let child_id = child.commit_id().expect("fixture");
    let delta = c5_delta(child_id);
    let reject = |name, description, bytes, code| ManifestVector {
        name,
        description,
        bytes,
        expect: ManifestExpect::Rejected(code),
    };
    let link = |seq: u64| {
        Value::Map(vec![
            (0, Value::Bytes(vec![1; 32])),
            (1, Value::Uint(seq)),
            (2, Value::Uint(0)),
        ])
    };
    vec![
        ManifestVector {
            name: "valid-commit-root",
            description: "commit 0: checkpoint, no parent, informational time",
            bytes: c5_frame(&root),
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-commit-child",
            description: "commit 1: checkpoint, parent link with footer hint, no time",
            bytes: c5_frame(&child),
            expect: ManifestExpect::Valid,
        },
        ManifestVector {
            name: "valid-commit-delta",
            description: "commit 2: delta on base = commit 1 (the parent, a checkpoint)",
            bytes: c5_frame(&delta),
            expect: ManifestExpect::Valid,
        },
        // ---- ID and closed schema
        reject(
            "reject-commit-id-mismatch",
            "stored commit ID differs from the canonical body's",
            c5_edit(&root, false, |r| {
                c5_set(r, 9, Value::Bytes(vec![0; 32]));
            }),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-unknown-key",
            "an extra top-level key 11",
            c5_edit(&root, true, |r| r.push((11, Value::Null))),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-missing-key",
            "no delta-manifest reference (key 6)",
            c5_edit(&root, true, |r| r.retain(|(k, _)| *k != 6)),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-missing-descriptor",
            "no descriptor reference (key 10)",
            c5_edit(&root, true, |r| r.retain(|(k, _)| *k != 10)),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-schema-version",
            "schema version 2",
            c5_edit(&root, true, |r| c5_set(r, 0, Value::Uint(2))),
            "UNSUPPORTED_FEATURE",
        ),
        reject(
            "reject-commit-legacy-v0",
            "the schema-0 draft root commit (pre-batch legacy, spec §26)",
            c5_legacy_v0_root(),
            "UNSUPPORTED_FEATURE",
        ),
        // ---- metadata (key 5)
        reject(
            "reject-commit-metadata-form",
            "metadata form 2",
            c5_edit(&root, true, |r| c5_metadata(r)[0].1 = Value::Uint(2)),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-checkpoint-missing-snapshot",
            "a checkpoint without its snapshot-manifest reference",
            c5_edit(&root, true, |r| c5_metadata(r).retain(|(k, _)| *k != 2)),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-delta-with-snapshot",
            "a delta form that also carries a snapshot reference (key 2)",
            c5_edit(&delta, true, |r| {
                c5_metadata(r).push((2, c5_ref_value(&c5_ref(1, 100, 0x77))))
            }),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-root-delta",
            "commit 0 in delta form (commit 0 is always a checkpoint)",
            c5_edit(&root, true, |r| {
                c5_set(r, 5, Value::Map(vec![(0, Value::Uint(1)), (1, link(0))]))
            }),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-base-not-before",
            "a delta whose base sequence equals its own",
            c5_edit(&delta, true, |r| {
                if let Value::Map(b) = &mut c5_metadata(r)[1].1 {
                    b[1].1 = Value::Uint(2);
                }
            }),
            "RECORD_INVALID",
        ),
        // ---- parent (key 4)
        reject(
            "reject-commit-root-with-parent",
            "commit 0 with a parent link",
            c5_edit(&root, true, |r| c5_set(r, 4, link(0))),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-orphan",
            "commit 1 without a parent link",
            c5_edit(&child, true, |r| c5_set(r, 4, Value::Null)),
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-parent-seq-gap",
            "commit 2 whose parent sequence is 0 (must be 1)",
            c5_edit(&delta, true, |r| {
                if let Value::Map(p) = c4_field(r, 4) {
                    p[1].1 = Value::Uint(0);
                }
            }),
            "RECORD_INVALID",
        ),
        // ---- descriptor (key 10)
        reject(
            "reject-commit-descriptor-offset",
            "the descriptor referenced at offset 8 (D12: only offset 0)",
            c5_edit(&root, true, |r| {
                c5_set(r, 10, c5_ref_value(&c5_ref(8, 56, 0x3D)))
            }),
            "RECORD_INVALID",
        ),
        // ---- D11 required features
        reject(
            "reject-commit-required-feature",
            "requires feature 1, which no build defines",
            c5_edit(&root, true, |r| {
                c5_set(r, 7, Value::Array(vec![Value::Uint(1)]))
            }),
            "UNSUPPORTED_FEATURE",
        ),
        reject(
            "reject-commit-features-not-increasing",
            "required features [2, 1]",
            c5_edit(&root, true, |r| {
                c5_set(r, 7, Value::Array(vec![Value::Uint(2), Value::Uint(1)]))
            }),
            "ENVELOPE_INVALID",
        ),
        // ---- other fields
        reject(
            "reject-commit-short-transaction-id",
            "a 15-byte transaction ID",
            c5_edit(&root, true, |r| c5_set(r, 3, Value::Bytes(vec![0x40; 15]))),
            "RECORD_INVALID",
        ),
        // ---- encoding and framing
        reject(
            "reject-commit-noncanonical-cbor",
            "a key in non-shortest form",
            {
                let (payload, _) = root.encode().expect("fixture");
                let mut p = vec![payload[0], 0x18];
                p.extend_from_slice(&payload[1..]);
                encode_skippable_frame(FrameKind::CommitRecord, &p).expect("frame")
            },
            "RECORD_INVALID",
        ),
        reject(
            "reject-commit-wrong-frame-kind",
            "a valid payload in a RECOVERY_MANIFEST frame",
            {
                let (payload, _) = root.encode().expect("fixture");
                encode_skippable_frame(FrameKind::RecoveryManifest, &payload).expect("frame")
            },
            "MALFORMED_FRAME",
        ),
        reject(
            "reject-commit-trailing-frame",
            "a second frame after the commit",
            {
                let mut b = c5_frame(&root);
                b.extend_from_slice(&c5_frame(&root));
                b
            },
            "MALFORMED_FRAME",
        ),
        // Added for G1 (T30): D11 "Payload length", CBOR: the whole payload
        // is consumed. Appended last so earlier lines stay identical.
        reject(
            "reject-commit-payload-not-consumed",
            "one byte after the commit's CBOR item, inside the frame",
            {
                let (mut p, _) = root.encode().expect("fixture");
                p.push(0x00);
                encode_skippable_frame(FrameKind::CommitRecord, &p).expect("frame")
            },
            "RECORD_INVALID",
        ),
    ]
}

/// The checked-in `fixtures/golden/c5/vectors.txt`.
pub fn render_c5_manifest() -> String {
    let mut s = String::from(
        "# MOCHI C5 commit-record vectors. DRAFT schema 1 (R3); docs/schemas/commit-record-v1.cddl.\n\
         # Built by mochi_testkit::golden; do not edit by hand.\n\
         # <name> valid <commit-id>   |   <name> rejects <core error code>\n",
    );
    for v in c5_commit_vectors() {
        match v.expect {
            ManifestExpect::Valid => {
                let (_, id) =
                    CommitRecord::from_stored(&v.bytes, &Default::default(), &Default::default())
                        .expect("valid vector");
                s.push_str(&format!("{} valid {}\n", v.name, hex(id.as_bytes())));
            }
            ManifestExpect::Rejected(code) => s.push_str(&format!("{} rejects {code}\n", v.name)),
        }
    }
    s.push_str(
        "# T11/T12 archive vectors (docs/t11-t12-acceptance.md). Frozen; checked by behaviour.\n\
         # <name>.mochi opens   |   <name>.mochi rejects <core error code> (open_head)\n",
    );
    for v in c5_archive_vectors() {
        match v.expect {
            ArchiveExpect::Opens => s.push_str(&format!("{}.mochi opens\n", v.name)),
            ArchiveExpect::Rejected(code) => {
                s.push_str(&format!("{}.mochi rejects {code}\n", v.name))
            }
        }
    }
    s
}

// ---- B.2: binary envelope v0 vectors (spec Annex B.2.2, D11; plan T30) -------
//
// Each vector is one complete skippable frame. A conforming reader, given the
// expectations below, must reach exactly the stated outcome:
//
// * referencing field expects a catalog image (`METADATA_DELTA`);
// * known record schema versions: `[0]`; known required features: none;
// * default limits;
// * after the intrinsic checks, bind to [`B2_IDENTITY`].
//
// The payload is a minimal 100-byte SQLite header carrying only the
// signature and `user_version`: the envelope checks nothing more, and opening
// the image is `mochi-core`'s job. The integrity-scope obligation (stored
// hash held by the referencing commit) has no vector here; it needs a
// referencing commit and is covered by the C5/v1 commit vectors.

use mochi_format::envelope::{
    decode_binary_record, BinaryEnvelope, EnvelopeRules, PayloadEncoding, RecordIdentity,
};
use mochi_format::error::EnvelopeFault;
use mochi_format::frame::walk_frame;
use mochi_format::{FormatError, Limits};

/// The identity every B.2 envelope vector is bound to.
pub const B2_IDENTITY: RecordIdentity = RecordIdentity {
    archive_id: [0xA1; 32],
    commit_sequence: 7,
    transaction_id: [0xB2; 16],
};

/// The rules a reader applies to every B.2 envelope vector.
pub fn b2_envelope_rules() -> EnvelopeRules<'static> {
    EnvelopeRules {
        kind: FrameKind::MetadataDelta,
        schema_versions: &[0],
        known_features: &[],
    }
}

#[derive(Debug, Clone)]
pub struct EnvelopeVector {
    pub name: &'static str,
    pub description: &'static str,
    pub bytes: Vec<u8>,
    /// `Ok` with the bound envelope, or the exact error.
    pub expect: std::result::Result<BinaryEnvelope, FormatError>,
}

/// Every header field written raw, so rejects can break exactly one rule.
#[derive(Clone)]
struct RawEnvelope {
    magic: u32,
    header_len: u32,
    version: u16,
    schema: u16,
    encoding: u32,
    n: u32,
    payload_len: u64,
    identity: RecordIdentity,
    features: Vec<u64>,
    payload: Vec<u8>,
}

impl RawEnvelope {
    fn valid() -> Self {
        let payload = b2_sqlite_header(0);
        RawEnvelope {
            magic: registry::METADATA_DELTA,
            header_len: 80,
            version: 0,
            schema: 0,
            encoding: 0,
            n: 0,
            payload_len: payload.len() as u64,
            identity: B2_IDENTITY,
            features: vec![],
            payload,
        }
    }

    /// Features with n and header length kept consistent.
    fn with_features(mut self, f: &[u64]) -> Self {
        self.features = f.to_vec();
        self.n = f.len() as u32;
        self.header_len = 80 + 8 * f.len() as u32;
        self
    }

    fn frame(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&self.header_len.to_le_bytes());
        p.extend_from_slice(&self.version.to_le_bytes());
        p.extend_from_slice(&self.schema.to_le_bytes());
        p.extend_from_slice(&self.encoding.to_le_bytes());
        p.extend_from_slice(&self.n.to_le_bytes());
        p.extend_from_slice(&self.payload_len.to_le_bytes());
        p.extend_from_slice(&self.identity.archive_id);
        p.extend_from_slice(&self.identity.commit_sequence.to_le_bytes());
        p.extend_from_slice(&self.identity.transaction_id);
        for f in &self.features {
            p.extend_from_slice(&f.to_le_bytes());
        }
        p.extend_from_slice(&self.payload);
        skippable(self.magic, &p)
    }
}

/// A 100-byte SQLite header: signature, `user_version` at 60 (big-endian).
pub fn b2_sqlite_header(user_version: i32) -> Vec<u8> {
    let mut h = vec![0u8; 100];
    h[..16].copy_from_slice(&registry::SQLITE_IMAGE_SIGNATURE);
    h[60..64].copy_from_slice(&user_version.to_be_bytes());
    h
}

/// Decode one B.2 envelope vector the way the expectations above describe.
pub fn b2_decode_envelope_vector(bytes: &[u8]) -> std::result::Result<BinaryEnvelope, FormatError> {
    let limits = Limits::default();
    let span = walk_frame(bytes, 0, &limits)?;
    if span.len != bytes.len() as u64 {
        return Err(FormatError::CannotWrite("not exactly one frame"));
    }
    let payload = &bytes[8..];
    let cand = decode_binary_record(span.magic, payload, &b2_envelope_rules(), &limits)?;
    cand.bind(&B2_IDENTITY).map(|(e, _)| e)
}

/// Every B.2 envelope vector, in a stable order.
pub fn b2_envelope_vectors() -> Vec<EnvelopeVector> {
    let ok = BinaryEnvelope {
        record_schema_version: 0,
        encoding: PayloadEncoding::SqliteImage,
        identity: B2_IDENTITY,
        required_features: vec![],
    };
    let f = |x| Err(FormatError::Envelope(x));
    let v = RawEnvelope::valid;
    let mut out = vec![EnvelopeVector {
        name: "valid-envelope-image",
        description: "catalog image, schema 0 = user_version 0, no features",
        bytes: v().frame(),
        expect: Ok(ok),
    }];
    let mut push = |name, description, raw: RawEnvelope, expect| {
        out.push(EnvelopeVector {
            name,
            description,
            bytes: raw.frame(),
            expect,
        })
    };
    push(
        "reject-envelope-wrong-frame-kind",
        "a valid header in a RECOVERY_MANIFEST frame (record type)",
        RawEnvelope {
            magic: registry::RECOVERY_MANIFEST,
            ..v()
        },
        f(EnvelopeFault::KindHasNoEnvelope),
    );
    push(
        "reject-envelope-version",
        "envelope version 1 (schema version: refuse)",
        RawEnvelope { version: 1, ..v() },
        Err(FormatError::UnsupportedEnvelopeVersion { version: 1 }),
    );
    push(
        "reject-envelope-schema-unknown",
        "record schema version 1 with user_version 1; this build knows only 0",
        RawEnvelope {
            schema: 1,
            payload: b2_sqlite_header(1),
            ..v()
        },
        Err(FormatError::UnsupportedSchema { version: 1 }),
    );
    push(
        "reject-envelope-schema-user-version",
        "record schema version 0, but the image's user_version is 1",
        RawEnvelope {
            payload: b2_sqlite_header(1),
            ..v()
        },
        f(EnvelopeFault::SchemaVersionMismatch),
    );
    push(
        "reject-envelope-unknown-feature",
        "required feature 1, which this build does not know",
        v().with_features(&[1]),
        Err(FormatError::UnsupportedRequiredFeature { feature: 1 }),
    );
    push(
        "reject-envelope-features-not-increasing",
        "required features [2, 1]",
        v().with_features(&[2, 1]),
        f(EnvelopeFault::FeaturesNotIncreasing),
    );
    push(
        "reject-envelope-feature-count",
        "n = 65 (> 64), header length consistent with it",
        v().with_features(&(1..=65).collect::<Vec<_>>()),
        f(EnvelopeFault::FeatureCountOutOfRange),
    );
    push(
        "reject-envelope-header-length",
        "header length 88 with n = 0",
        RawEnvelope {
            header_len: 88,
            ..v()
        },
        f(EnvelopeFault::HeaderLengthMismatch),
    );
    push(
        "reject-envelope-payload-encoding",
        "payload encoding 1 (unregistered)",
        RawEnvelope { encoding: 1, ..v() },
        f(EnvelopeFault::UnknownPayloadEncoding),
    );
    let mut bad_sig = b2_sqlite_header(0);
    bad_sig[0] = b's';
    push(
        "reject-envelope-signature",
        "payload starts \"sQLite format 3\\0\"",
        RawEnvelope {
            payload: bad_sig,
            ..v()
        },
        f(EnvelopeFault::EncodingSignatureMismatch),
    );
    push(
        "reject-envelope-payload-length",
        "header says 101 payload bytes; the frame holds 100",
        RawEnvelope {
            payload_len: 101,
            ..v()
        },
        f(EnvelopeFault::PayloadLengthMismatch),
    );
    let mut other = B2_IDENTITY;
    other.archive_id[0] ^= 1;
    push(
        "reject-envelope-archive-id",
        "archive ID differs from the referencing commit's",
        RawEnvelope {
            identity: other,
            ..v()
        },
        f(EnvelopeFault::ArchiveIdMismatch),
    );
    let mut other = B2_IDENTITY;
    other.commit_sequence = 8;
    push(
        "reject-envelope-sequence",
        "commit sequence 8; the referencing commit is 7",
        RawEnvelope {
            identity: other,
            ..v()
        },
        f(EnvelopeFault::SequenceMismatch),
    );
    let mut other = B2_IDENTITY;
    other.transaction_id[15] ^= 1;
    push(
        "reject-envelope-transaction-id",
        "transaction ID differs from the referencing commit's",
        RawEnvelope {
            identity: other,
            ..v()
        },
        f(EnvelopeFault::TransactionIdMismatch),
    );
    out
}

/// `vectors.txt` for `fixtures/golden/b2/`.
pub fn render_b2_manifest() -> String {
    let mut s = String::from(
        "# MOCHI B.2 binary envelope v0 vectors. DRAFT (R2); spec Annex B.2.2, D11.\n\
         # Built by mochi_testkit::golden; do not edit by hand.\n\
         # Reader expectations: field expects METADATA_DELTA; known schema versions [0];\n\
         # no known features; default limits; bind to archive ID a1*32, sequence 7,\n\
         # transaction ID b2*16.\n\
         # <name> valid   |   <name> rejects <error>\n",
    );
    for v in b2_envelope_vectors() {
        match &v.expect {
            Ok(_) => s.push_str(&format!("{} valid\n", v.name)),
            Err(e) => s.push_str(&format!("{} rejects {e:?}\n", v.name)),
        }
    }
    s
}

// ---- B.2: archive descriptor v0 vectors (spec Annex B.2 D12; plan T7, T30) ----
//
// Each is one complete frame, read as if found at offset 0 with default
// limits. Expected outcomes are core error codes (D12 classification in
// `mochi_core::descriptor`).

use mochi_core::descriptor::{DeclaredLimits, Descriptor};
use mochi_core::ErrorCode;

#[derive(Debug, Clone)]
pub struct DescriptorVector {
    pub name: &'static str,
    pub description: &'static str,
    pub bytes: Vec<u8>,
    pub expect: std::result::Result<Descriptor, ErrorCode>,
}

fn b2_descriptor() -> Descriptor {
    Descriptor::new(ArchiveId::from_bytes([0xA1; 32]), false)
}

/// The canonical map of [`b2_descriptor`], as editable entries.
fn b2_descriptor_entries() -> Vec<(u64, Value)> {
    vec![
        (0, Value::Uint(0)),
        (1, Value::Bytes(vec![0xA1; 32])),
        (2, Value::Uint(2)),
        (3, Value::Uint(1)),
        (4, Value::Array(vec![])),
        (5, Value::Map(vec![(0, Value::Bool(false))])),
    ]
}

fn descriptor_frame_of(entries: Vec<(u64, Value)>) -> Vec<u8> {
    skippable(
        registry::ARCHIVE_DESCRIPTOR,
        &mochi_format::cbor::encode(&Value::Map(entries)).expect("fixture"),
    )
}

fn edit(key: u64, v: Option<Value>) -> Vec<(u64, Value)> {
    let mut e = b2_descriptor_entries();
    e.retain(|(k, _)| *k != key);
    if let Some(v) = v {
        e.push((key, v));
        e.sort_by_key(|(k, _)| *k);
    }
    e
}

/// Decode one descriptor vector as the expectations above describe.
pub fn b2_decode_descriptor_vector(bytes: &[u8]) -> std::result::Result<Descriptor, ErrorCode> {
    Descriptor::from_stored(
        bytes,
        0,
        &Limits::default(),
        &mochi_format::cbor::CborLimits::default(),
    )
    .map_err(|e| e.code)
}

pub fn b2_descriptor_vectors() -> Vec<DescriptorVector> {
    let mut out = Vec::new();
    let mut push = |name, description, bytes, expect| {
        out.push(DescriptorVector {
            name,
            description,
            bytes,
            expect,
        })
    };
    let ok = b2_descriptor();
    push(
        "valid-descriptor-minimal",
        "schema 0, generation 2, draft 1, no features, not TAR-compatible",
        ok.to_stored().expect("fixture").as_bytes().to_vec(),
        Ok(ok.clone()),
    );
    let mut wide = ok.clone();
    wide.tar_compatible = true;
    wide.declared_limits = Some(DeclaredLimits {
        max_skippable_payload: 1 << 30,
        max_cbor_items: 1 << 26,
        max_image_len: (1 << 30) - 592,
    });
    push(
        "valid-descriptor-declared-limits",
        "TAR-compatible, with optional key 6 (declared limits; advisory only)",
        wide.to_stored().expect("fixture").as_bytes().to_vec(),
        Ok(wide),
    );
    let d = ErrorCode::DescriptorInvalid;
    let u = ErrorCode::UnsupportedFeature;
    push(
        "reject-descriptor-schema-version",
        "key 0 = 1",
        descriptor_frame_of(edit(0, Some(Value::Uint(1)))),
        Err(u),
    );
    push(
        "reject-descriptor-generation",
        "key 2 = 1: wire generation other than MOCHI2",
        descriptor_frame_of(edit(2, Some(Value::Uint(1)))),
        Err(u),
    );
    push(
        "reject-descriptor-draft-null",
        "key 3 = null: a ratified archive, read by a draft build",
        descriptor_frame_of(edit(3, Some(Value::Null))),
        Err(u),
    );
    push(
        "reject-descriptor-draft-2",
        "key 3 = 2: a later draft",
        descriptor_frame_of(edit(3, Some(Value::Uint(2)))),
        Err(u),
    );
    push(
        "reject-descriptor-draft-type",
        "key 3 = \"1\": neither uint nor null",
        descriptor_frame_of(edit(3, Some(Value::Text("1".into())))),
        Err(d),
    );
    push(
        "reject-descriptor-unknown-feature",
        "key 4 = [1]",
        descriptor_frame_of(edit(4, Some(Value::Array(vec![Value::Uint(1)])))),
        Err(u),
    );
    push(
        "reject-descriptor-features-not-increasing",
        "key 4 = [2, 1]",
        descriptor_frame_of(edit(
            4,
            Some(Value::Array(vec![Value::Uint(2), Value::Uint(1)])),
        )),
        Err(d),
    );
    push(
        "reject-descriptor-unknown-key",
        "key 7 present",
        descriptor_frame_of(edit(7, Some(Value::Uint(0)))),
        Err(d),
    );
    push(
        "reject-descriptor-missing-key",
        "key 5 (constraints) absent",
        descriptor_frame_of(edit(5, None)),
        Err(d),
    );
    push(
        "reject-descriptor-short-archive-id",
        "key 1 is 31 bytes",
        descriptor_frame_of(edit(1, Some(Value::Bytes(vec![0xA1; 31])))),
        Err(d),
    );
    push(
        "reject-descriptor-constraint-type",
        "constraint 0 is 0, not a bool",
        descriptor_frame_of(edit(5, Some(Value::Map(vec![(0, Value::Uint(0))])))),
        Err(d),
    );
    push(
        "reject-descriptor-constraint-unknown-key",
        "constraints carry key 1",
        descriptor_frame_of(edit(
            5,
            Some(Value::Map(vec![
                (0, Value::Bool(false)),
                (1, Value::Bool(true)),
            ])),
        )),
        Err(d),
    );
    push(
        "reject-descriptor-declared-limits-missing-key",
        "key 6 = {0, 1} without 2",
        descriptor_frame_of(edit(
            6,
            Some(Value::Map(vec![(0, Value::Uint(1)), (1, Value::Uint(1))])),
        )),
        Err(d),
    );
    // Non-canonical: key 0's value 0 written as a one-byte uint (0x18 0x00).
    let mut nc = ok.to_stored().expect("fixture").as_bytes().to_vec();
    nc.splice(10..11, [0x18, 0x00]);
    let len = (nc.len() - 8) as u32;
    nc[4..8].copy_from_slice(&len.to_le_bytes());
    push(
        "reject-descriptor-noncanonical-cbor",
        "schema version 0 encoded in two bytes",
        nc,
        Err(d),
    );
    push(
        "reject-descriptor-wrong-frame-kind",
        "a valid payload in a RECOVERY_MANIFEST frame",
        skippable(
            registry::RECOVERY_MANIFEST,
            &ok.to_stored().expect("fixture").as_bytes()[8..],
        ),
        Err(d),
    );
    let mut trailing = ok.to_stored().expect("fixture").as_bytes().to_vec();
    trailing.extend(skippable(registry::COMMIT_RECORD, b"x"));
    push(
        "reject-descriptor-trailing-frame",
        "a valid descriptor frame followed by another frame",
        trailing,
        Err(d),
    );
    // Added for G1 (T30): D11 "Payload length", CBOR: the whole payload is
    // consumed. Appended last so earlier lines stay identical.
    let mut p = ok.to_stored().expect("fixture").as_bytes()[8..].to_vec();
    p.push(0x00);
    push(
        "reject-descriptor-payload-not-consumed",
        "one byte after the descriptor's CBOR item, inside the frame",
        skippable(registry::ARCHIVE_DESCRIPTOR, &p),
        Err(d),
    );
    out
}

pub fn render_b2_descriptor_manifest() -> String {
    let mut s = String::from(
        "# MOCHI B.2 archive descriptor v0 vectors. DRAFT (R1, R2); spec Annex B.2 D12;\n\
         # docs/schemas/archive-descriptor-v0.cddl. Built by mochi_testkit::golden.\n\
         # Read as found at offset 0 with default limits.\n\
         # <name> valid   |   <name> rejects <core error code>\n",
    );
    for v in b2_descriptor_vectors() {
        match &v.expect {
            Ok(_) => s.push_str(&format!("{} valid\n", v.name)),
            Err(c) => s.push_str(&format!("{} rejects {}\n", v.name, c.as_str())),
        }
    }
    s
}

// ---- C5 archive vectors for T11/T12 (docs/t11-t12-acceptance.md, "Fixtures") -----
//
// Whole archives, written by the real writer under a test checkpoint policy
// (review decision 14), with forged commits appended for the reject cases
// (`crate::forge`). Each reject archive changes exactly one rule relative to
// a valid archive. Like `valid-archive-3-commits.mochi`, these contain zstd
// and SQLite output, so the checked-in files are frozen and checked by
// behaviour, never compared with a fresh build byte for byte; the fresh
// builds are checked against the same expectations.
//
// Not included, deliberately: the unknown-operation case (the acceptance
// list names parent-link, duplicate-ID, and unknown-feature for T12). The
// parent traversal offset that lands on no footer was excluded while Q23
// was provisional and is included since its decision (2026-10-04).

use crate::archive::Step as ArchiveStep;
use crate::forge::{self, empty_delta, link, rule_base, txid, Forge};
use mochi_core::publish::{ArchiveWriter, CheckpointPolicy};

/// What `open_head` must conclude about an archive vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveExpect {
    /// Opens; every commit of `commit_history` opens too.
    Opens,
    /// `open_head` fails with this core error code name.
    Rejected(&'static str),
}

pub struct ArchiveVector {
    /// File name without the `.mochi` extension.
    pub name: &'static str,
    pub description: &'static str,
    pub expect: ArchiveExpect,
    pub bytes: Vec<u8>,
}

/// Six commits for the segment vectors: `scripted_history` plus three that
/// add a directory, files across chunks, a replacement, a rename, and a
/// delete.
pub fn c5_segment_history() -> Vec<ArchiveStep> {
    use crate::archive::{path, Content};
    let mut steps = crate::archive::scripted_history();
    let mut state = steps.last().unwrap().after.clone();
    let a = |mode: u32| Attributes {
        posix: Some(PosixAttributes {
            mode,
            uid: 1000,
            gid: 1000,
        }),
        windows: None,
        mtime: Some(Mtime {
            secs: 1_700_000_500 + i64::from(mode),
            nanos: 0,
        }),
    };
    let mut push = |tx: mochi_core::publish::Transaction, state: &crate::archive::State| {
        steps.push(ArchiveStep {
            tx,
            after: state.clone(),
        })
    };

    let mut tx = mochi_core::publish::Transaction::new();
    tx.put_dir(path("seg"), a(0o750));
    state.insert(b"seg".to_vec(), Content::Dir);
    let f3 = crate::deterministic_bytes(31, 150);
    tx.put_file(path("seg/f3"), f3.clone(), a(0o640));
    state.insert(b"seg/f3".to_vec(), Content::File(f3));
    push(tx, &state);

    let mut tx = mochi_core::publish::Transaction::new();
    let f4 = crate::deterministic_bytes(41, 90);
    tx.put_file(path("seg/f4"), f4.clone(), a(0o604));
    state.insert(b"seg/f4".to_vec(), Content::File(f4));
    let f3b = crate::deterministic_bytes(32, 70);
    tx.put_file(path("seg/f3"), f3b.clone(), a(0o641));
    state.insert(b"seg/f3".to_vec(), Content::File(f3b));
    push(tx, &state);

    let mut tx = mochi_core::publish::Transaction::new();
    tx.rename(path("seg/f4"), path("f4-moved"));
    let moved = state.remove(b"seg/f4".as_slice()).unwrap();
    state.insert(b"f4-moved".to_vec(), moved);
    tx.delete(path("seg/f3"));
    state.remove(b"seg/f3".as_slice());
    push(tx, &state);
    steps
}

const SEGMENT_SEED: u64 = 0x7112;

fn write_under(policy: CheckpointPolicy, seed: u64, steps: &[ArchiveStep]) -> Vec<u8> {
    let s = crate::SimStorage::new();
    let mut w = ArchiveWriter::create(
        s.clone(),
        Box::new(crate::SeqIds::new(seed)),
        crate::archive::test_options(),
    )
    .unwrap();
    w.set_checkpoint_policy(policy).unwrap();
    let job = crate::archive::Job::new();
    for st in steps {
        w.commit(st.tx.clone(), &job.ctx()).unwrap();
    }
    w.close().unwrap();
    s.contents()
}

/// cp0, d1 (`Never`): the base of most reject vectors.
fn cp0_d1() -> Forge {
    Forge::new(write_under(
        CheckpointPolicy::Never,
        SEGMENT_SEED,
        &c5_segment_history()[..2],
    ))
}

fn other_id() -> CommitId {
    CommitId::from_bytes([0xAB; 32])
}

/// Forge commit 2 on cp0_d1 with `edit` applied to the empty delta manifest
/// (before encoding) and to the record.
fn forge_c2(
    edit_manifest: impl FnOnce(&mut Manifest, &[mochi_core::publish::HistoryEntry]),
    edit_record: impl FnOnce(&mut CommitRecord, &[mochi_core::publish::HistoryEntry]),
) -> Vec<u8> {
    let mut f = cp0_d1();
    let h = f.history();
    let mut m = empty_delta(&h[1], txid(0x62));
    edit_manifest(&mut m, &h);
    let r = f.append_manifest(&m);
    let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), r, txid(0x62));
    edit_record(&mut rec, &h);
    f.append_commit(&rec);
    f.bytes
}

/// Forge c2 with an edited manifest (canonical CBOR edited after encoding),
/// then a valid c3 on top: the fault sits mid-segment.
fn forge_c2_edited_then_c3(edit: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut f = cp0_d1();
    let h = f.history();
    let r2 = f.append_manifest_edited(&empty_delta(&h[1], txid(0x63)), edit);
    let c2 = f.append_delta(&h[1], rule_base(&h[1]), r2, txid(0x63));
    let r3 = f.append_manifest(&empty_delta(&c2, txid(0x64)));
    f.append_delta(&c2, rule_base(&h[1]), r3, txid(0x64));
    f.bytes
}

/// Every T11/T12 archive vector, in `vectors.txt` order.
pub fn c5_archive_vectors() -> Vec<ArchiveVector> {
    let steps = c5_segment_history();
    let mut out = Vec::new();
    let mut push = |name, description, expect, bytes| {
        out.push(ArchiveVector {
            name,
            description,
            expect,
            bytes,
        })
    };

    push(
        "valid-archive-delta-segment",
        "checkpoint, three deltas, checkpoint, one delta (Every(4), six commits)",
        ArchiveExpect::Opens,
        write_under(CheckpointPolicy::Every(4), SEGMENT_SEED, &steps),
    );
    push(
        "valid-archive-wrong-base-hint",
        "decision 17: a delta whose base footer hint is wrong opens through its ancestry",
        ArchiveExpect::Opens,
        forge_c2(
            |_, _| {},
            |r, _| {
                let Metadata::Delta { base } = &mut r.metadata else {
                    unreachable!()
                };
                base.footer_offset += 1;
            },
        ),
    );

    // T11 rows.
    push(
        "reject-archive-base-id-tampered",
        "T11: the base ID is tampered, the record otherwise self-consistent",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        forge_c2(
            |_, _| {},
            |r, _| {
                let Metadata::Delta { base } = &mut r.metadata else {
                    unreachable!()
                };
                base.commit_id = other_id();
            },
        ),
    );
    let other = write_under(CheckpointPolicy::Never, SEGMENT_SEED + 1, &steps[..1]);
    let other_cp0 = Forge::new(other).history().remove(0);
    push(
        "reject-archive-base-off-ancestry",
        "T11: the base is another archive's checkpoint at the same sequence",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        forge_c2(
            |_, _| {},
            |r, _| {
                r.metadata = Metadata::Delta {
                    base: link(&other_cp0),
                };
            },
        ),
    );
    push(
        "reject-archive-base-is-delta",
        "T11: the named base is a delta",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        forge_c2(
            |_, _| {},
            |r, h| {
                r.metadata = Metadata::Delta { base: link(&h[1]) };
            },
        ),
    );
    push(
        "reject-archive-base-skips-checkpoint",
        "T11: the base skips a later checkpoint",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = Forge::new(write_under(
                CheckpointPolicy::Every(2),
                SEGMENT_SEED,
                &steps[..3],
            ));
            let h = f.history();
            let r = f.append_manifest(&empty_delta(&h[2], txid(0x65)));
            f.append_delta(&h[2], link(&h[0]), r, txid(0x65));
            f.bytes
        },
    );
    push(
        "reject-archive-segment-inconsistent",
        "T11: an intermediate delta names another base; the head names the right one",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r2 = f.append_manifest(&empty_delta(&h[1], txid(0x66)));
            let mut wrong = rule_base(&h[1]);
            wrong.commit_id = other_id();
            let c2 = f.append_delta(&h[1], wrong, r2, txid(0x66));
            let r3 = f.append_manifest(&empty_delta(&c2, txid(0x67)));
            f.append_delta(&c2, rule_base(&h[1]), r3, txid(0x67));
            f.bytes
        },
    );
    push(
        "reject-archive-segment-descriptor-differs",
        // Frozen as first written. `forge::delta_record` copies c2's wrong
        // reference into c3, so the head fails on its own descriptor (D12)
        // and the segment rule is never reached; the mid-segment case is
        // `reject-archive-segment-descriptor-differs-head-valid`.
        "D12: the head and an intermediate commit reference another descriptor",
        ArchiveExpect::Rejected("DESCRIPTOR_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r2 = f.append_manifest(&empty_delta(&h[1], txid(0x68)));
            let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), r2, txid(0x68));
            rec.descriptor.stored_hash = StoredObjectHash::from_bytes([0x11; 32]);
            let c2 = f.append_commit(&rec);
            let r3 = f.append_manifest(&empty_delta(&c2, txid(0x69)));
            f.append_delta(&c2, rule_base(&h[1]), r3, txid(0x69));
            f.bytes
        },
    );
    push(
        "reject-archive-parent-hint-wrong-footer",
        "T11: a parent hint mid-segment names another valid footer",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r2 = f.append_manifest(&empty_delta(&h[1], txid(0x6A)));
            let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), r2, txid(0x6A));
            rec.parent = Some(CommitLink {
                footer_offset: h[0].footer_offset,
                ..rec.parent.unwrap()
            });
            let c2 = f.append_commit(&rec);
            let r3 = f.append_manifest(&empty_delta(&c2, txid(0x6B)));
            f.append_delta(&c2, rule_base(&h[1]), r3, txid(0x6B));
            f.bytes
        },
    );

    // T12: parent link (checklist Q22), duplicate ID, unknown feature.
    push(
        "reject-archive-link-to-snapshot",
        "Q22: the delta after a checkpoint links to its snapshot (key 5), not key 6",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = Forge::new(write_under(
                CheckpointPolicy::EveryCommit,
                SEGMENT_SEED,
                &steps[..1],
            ));
            let h = f.history();
            let Metadata::Checkpoint { snapshot, .. } = h[0].commit.metadata else {
                unreachable!()
            };
            let mut m = empty_delta(&h[0], txid(0x6C));
            m.parent.as_mut().unwrap().delta_manifest_hash = snapshot.stored_hash;
            let r = f.append_manifest(&m);
            f.append_delta(&h[0], rule_base(&h[0]), r, txid(0x6C));
            f.bytes
        },
    );
    push(
        "reject-archive-link-to-other-delta",
        "Q22: the link names the key-6 manifest of a commit other than j - 1",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        forge_c2(
            |m, h| {
                m.parent.as_mut().unwrap().delta_manifest_hash =
                    h[0].commit.delta_manifest.stored_hash
            },
            |_, _| {},
        ),
    );
    push(
        "reject-archive-link-wrong-seq",
        "Q22: the link's parent sequence is not j - 1",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r = f.append_manifest_edited(&empty_delta(&h[1], txid(0x6D)), |v| {
                *forge::field(forge::field(v, 3), 0) = Value::Uint(0);
            });
            f.append_delta(&h[1], rule_base(&h[1]), r, txid(0x6D));
            f.bytes
        },
    );
    push(
        "reject-archive-link-missing",
        "Q22: a delta after commit 0 has no parent link",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r = f.append_manifest_edited(&empty_delta(&h[1], txid(0x6E)), |v| {
                *forge::field(v, 3) = Value::Null;
            });
            f.append_delta(&h[1], rule_base(&h[1]), r, txid(0x6E));
            f.bytes
        },
    );
    push(
        "reject-archive-duplicate-version",
        "decision 18: a delta reintroduces a file version from the base (identical copy)",
        ArchiveExpect::Rejected("RECORD_INVALID"),
        {
            let f0 = cp0_d1();
            let s = f0.storage();
            let h = f0.history();
            let cp0 =
                mochi_core::publish::open_at_footer(&s, h[0].footer_offset, &Default::default())
                    .unwrap();
            let snap = mochi_core::publish::read_snapshot(&s, &cp0, &Default::default()).unwrap();
            forge_c2(
                move |m, _| m.file_versions.push(snap.file_versions[0].clone()),
                |_, _| {},
            )
        },
    );
    push(
        "reject-archive-unknown-feature",
        "an unknown required feature in a delta manifest mid-segment",
        ArchiveExpect::Rejected("UNSUPPORTED_FEATURE"),
        forge_c2_edited_then_c3(|v| *forge::field(v, 9) = Value::Array(vec![Value::Uint(1)])),
    );
    // Added 2026-10-04 (Q23 decided with clarification B; D10.6 amended).
    // Appended last so earlier vectors.txt lines stay byte-identical.
    push(
        "reject-archive-parent-offset-no-footer",
        "D10.6/Q23: a parent traversal offset mid-segment resolves to no valid footer",
        ArchiveExpect::Rejected("FOOTER_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r2 = f.append_manifest(&empty_delta(&h[1], txid(0x6F)));
            let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), r2, txid(0x6F));
            rec.parent = Some(CommitLink {
                footer_offset: h[1].footer_offset - 3,
                ..rec.parent.unwrap()
            });
            let c2 = f.append_commit(&rec);
            let r3 = f.append_manifest(&empty_delta(&c2, txid(0x70)));
            f.append_delta(&c2, rule_base(&h[1]), r3, txid(0x70));
            f.bytes
        },
    );
    // Added for G1 (T18 review, T30). Appended last so earlier vectors.txt
    // lines stay byte-identical.
    push(
        "reject-archive-segment-descriptor-differs-head-valid",
        "D10.6: an intermediate commit references another descriptor; the head references \
         the real one, so only the segment rule refuses it",
        ArchiveExpect::Rejected("DESCRIPTOR_INVALID"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r2 = f.append_manifest(&empty_delta(&h[1], txid(0x71)));
            let mut rec = forge::delta_record(&h[1], rule_base(&h[1]), r2, txid(0x71));
            rec.descriptor.stored_hash = StoredObjectHash::from_bytes([0x11; 32]);
            let c2 = f.append_commit(&rec);
            let r3 = f.append_manifest(&empty_delta(&c2, txid(0x72)));
            let mut head = forge::delta_record(&c2, rule_base(&h[1]), r3, txid(0x72));
            head.descriptor = h[1].commit.descriptor;
            f.append_commit(&head);
            f.bytes
        },
    );
    // D11 bindings of a CBOR record to the commit that references it.
    push(
        "reject-archive-manifest-archive-id",
        "D11 archive identity: the head's delta manifest names another archive",
        ArchiveExpect::Rejected("ENVELOPE_INVALID"),
        forge_c2(
            |m, _| m.archive_id = mochi_core::object::ArchiveId::from_bytes([0x5A; 32]),
            |_, _| {},
        ),
    );
    push(
        "reject-archive-manifest-transaction-id",
        "D11 identity: the head's delta manifest carries another transaction ID",
        ArchiveExpect::Rejected("ENVELOPE_INVALID"),
        forge_c2(|m, _| m.transaction_id = txid(0x73), |_, _| {}),
    );
    push(
        "reject-archive-manifest-sequence",
        "D11 identity: the head's delta manifest names another commit sequence",
        ArchiveExpect::Rejected("ENVELOPE_INVALID"),
        forge_c2(
            |m, _| {
                m.commit_seq += 1;
                if let Some(p) = m.parent.as_mut() {
                    p.seq += 1;
                }
            },
            |_, _| {},
        ),
    );
    push(
        "reject-archive-manifest-hash",
        "D11 integrity scope: the head's delta manifest fails its stored-object hash",
        ArchiveExpect::Rejected("STORED_INTEGRITY_FAILED"),
        {
            let mut f = cp0_d1();
            let h = f.history();
            let r = f.append_manifest(&empty_delta(&h[1], txid(0x74)));
            f.append_delta(&h[1], rule_base(&h[1]), r, txid(0x74));
            let at = (r.offset + r.stored_len / 2) as usize;
            f.bytes[at] ^= 0x01;
            f.bytes
        },
    );
    push(
        "reject-archive-descriptor-hash",
        "D11 integrity scope / D12: the descriptor fails its stored-object hash",
        ArchiveExpect::Rejected("DESCRIPTOR_INVALID"),
        {
            let mut b = cp0_d1().bytes;
            b[20] ^= 0x01; // inside the descriptor's archive ID
            b
        },
    );
    push(
        "reject-archive-descriptor-archive-id",
        "D11 archive identity / D12: the descriptor names another archive",
        ArchiveExpect::Rejected("DESCRIPTOR_INVALID"),
        {
            let f = cp0_d1();
            let e0 = f.history().remove(0);
            let other =
                Descriptor::new(mochi_core::object::ArchiveId::from_bytes([0x5A; 32]), false)
                    .to_stored()
                    .expect("fixture");
            let desc = other.as_bytes();
            let r = e0.commit.descriptor;
            assert_eq!(desc.len() as u64, r.stored_len, "same-length descriptor");
            let mut b = f.bytes;
            b[..desc.len()].copy_from_slice(desc);
            b.truncate(e0.commit_offset as usize);
            let mut g = Forge::new(b);
            let mut rec = e0.commit.clone();
            rec.descriptor.stored_hash = mochi_format::digest::stored_object_hash(other.view());
            g.append_commit(&rec);
            g.bytes
        },
    );
    out
}
