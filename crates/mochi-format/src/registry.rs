//! Frame registry constants (spec §8.2, §8.4).
//!
//! **This is the only module allowed to contain frame magic literals.**
//! `ci/check-invariants.sh` enforces that. The values are those of spec
//! revision 2.0, frozen with ratification artifact R1 (docs/ratification/) at
//! the format freeze and ratified at 1.0.

/// Standard Zstandard data frame magic.
pub const ZSTD_DATA_FRAME: u32 = 0xFD2F_B528;

/// First and last magic of the Zstandard skippable-frame range (RFC 8878).
pub const SKIPPABLE_MIN: u32 = 0x184D_2A50;
pub const SKIPPABLE_MAX: u32 = 0x184D_2A5F;

pub const METADATA_DELTA: u32 = 0x184D_2A50;
pub const COMMIT_RECORD: u32 = 0x184D_2A51;
pub const PREPARED_TRANSACTION_MANIFEST: u32 = 0x184D_2A52;
/// Reserved; no Core-profile interpretation (spec §8.2).
pub const RESERVED_NO_CORE_INTERPRETATION: u32 = 0x184D_2A53;
pub const SIGNATURE_EXTENSION: u32 = 0x184D_2A54;
pub const SEARCH_INDEX: u32 = 0x184D_2A55;
pub const COMMIT_FOOTER: u32 = 0x184D_2A56;
pub const ARCHIVE_DESCRIPTOR: u32 = 0x184D_2A57;
pub const RECOVERY_MANIFEST: u32 = 0x184D_2A58;
pub const ENCRYPTED_OBJECT: u32 = 0x184D_2A59;
/// `0x184D2A5A`–`0x184D2A5B` are reserved.
pub const RESERVED_5A: u32 = 0x184D_2A5A;
pub const RESERVED_5B: u32 = 0x184D_2A5B;
pub const KEY_ENVELOPE: u32 = 0x184D_2A5C;
pub const COMPRESSION_DICTIONARY: u32 = 0x184D_2A5D;
/// Optional accelerator, never a recovery authority (spec §22).
pub const FOOTER_HISTORY: u32 = 0x184D_2A5E;
pub const PARITY_OBJECT: u32 = 0x184D_2A5F;

/// Required-feature identifier 3: the archive comment (spec Annex B.2.19,
/// D29; `docs/schemas/commit-record-v4.cddl`). Listed by exactly the commit
/// records that carry a comment (schema 4, key 13); a reader that does not
/// know it refuses the commit (`UNSUPPORTED_FEATURE`, exit 4). Identifier 1,
/// the Encrypted profile, is `seal::FEATURE_ENCRYPTED`. Identifier 2 stays
/// unassigned: the frozen golden vectors use it as the feature no build
/// defines.
pub const FEATURE_COMMENT: u64 = 3;

/// Longest archive comment, in UTF-8 bytes (Annex B.2.19 item 3). It is
/// repeated in every later commit, so it is kept well inside the 64 KiB
/// commit-frame limit (B.2.3).
pub const MAX_COMMENT_BYTES: usize = 16 * 1024;

/// Footer geometry (spec §8.4): 8-byte skippable header + 64-byte payload.
pub const SKIPPABLE_HEADER_LEN: usize = 8;
pub const FOOTER_PAYLOAD_LEN: usize = 64;
/// ASCII `MOCHI2` followed by two zero bytes.
pub const FOOTER_MAGIC: [u8; 8] = *b"MOCHI2\0\0";

/// The only offset at which an archive-descriptor frame is valid (spec
/// Annex B.2 D12: "A descriptor frame at any offset other than 0 is
/// invalid"). Enforced by [`crate::frame::walk_frame`] on the magic alone.
pub const DESCRIPTOR_OFFSET: u64 = 0;

/// Tail-quarantine sidecar magic (spec Annex B.2.2): ASCII `MOCHITQ` and one
/// zero byte, at offset 0 of a `.mochiq` file. Never appears inside an
/// archive; it is registered here so that no other module spells it.
pub const TAIL_QUARANTINE_MAGIC: [u8; 8] = *b"MOCHITQ\0";

/// Signature of binary-envelope payload encoding 0, a SQLite 3 database
/// image (spec Annex B.2.2): the 16-byte SQLite header string.
pub const SQLITE_IMAGE_SIGNATURE: [u8; 16] = *b"SQLite format 3\0";

/// Signature of binary-envelope payload encoding 2, a Zstandard-compressed
/// SQLite 3 image (spec Annex B.2.2, owner decision 2026-10-16): the
/// Zstandard frame magic number, little-endian `0xFD2FB528`. The SQLite
/// signature is checked on the *decoded* bytes.
pub const ZSTD_IMAGE_SIGNATURE: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Signature of binary-envelope payload encoding 1, a parity shard of the
/// Redundancy profile (spec Annex B.2.11 D21; R6): ASCII `MOCHI2-PARITY` and
/// three zero bytes.
pub const PARITY_SIGNATURE: [u8; 16] = *b"MOCHI2-PARITY\0\0\0";

/// What a registered magic number denotes. A magic match is only ever a
/// *candidate* (spec §8.5); this classifies candidates, it does not validate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrameKind {
    ZstdData,
    MetadataDelta,
    CommitRecord,
    PreparedTransactionManifest,
    SignatureExtension,
    SearchIndex,
    CommitFooter,
    ArchiveDescriptor,
    RecoveryManifest,
    EncryptedObject,
    KeyEnvelope,
    CompressionDictionary,
    FooterHistory,
    ParityObject,
    /// Registered but reserved (`…2A53`, `…2A5A`, `…2A5B`): readers skip it and
    /// give it no Core interpretation.
    Reserved,
}

impl FrameKind {
    /// Classify a 4-byte magic. `None` means the value is not in the registry.
    pub const fn from_magic(magic: u32) -> Option<FrameKind> {
        Some(match magic {
            ZSTD_DATA_FRAME => FrameKind::ZstdData,
            METADATA_DELTA => FrameKind::MetadataDelta,
            COMMIT_RECORD => FrameKind::CommitRecord,
            PREPARED_TRANSACTION_MANIFEST => FrameKind::PreparedTransactionManifest,
            RESERVED_NO_CORE_INTERPRETATION | RESERVED_5A | RESERVED_5B => FrameKind::Reserved,
            SIGNATURE_EXTENSION => FrameKind::SignatureExtension,
            SEARCH_INDEX => FrameKind::SearchIndex,
            COMMIT_FOOTER => FrameKind::CommitFooter,
            ARCHIVE_DESCRIPTOR => FrameKind::ArchiveDescriptor,
            RECOVERY_MANIFEST => FrameKind::RecoveryManifest,
            ENCRYPTED_OBJECT => FrameKind::EncryptedObject,
            KEY_ENVELOPE => FrameKind::KeyEnvelope,
            COMPRESSION_DICTIONARY => FrameKind::CompressionDictionary,
            FOOTER_HISTORY => FrameKind::FooterHistory,
            PARITY_OBJECT => FrameKind::ParityObject,
            _ => return None,
        })
    }

    /// The magic for this kind, or `None` for [`FrameKind::Reserved`]
    /// (which has three magics and no writer may emit any of them).
    pub const fn magic(self) -> Option<u32> {
        Some(match self {
            FrameKind::ZstdData => ZSTD_DATA_FRAME,
            FrameKind::MetadataDelta => METADATA_DELTA,
            FrameKind::CommitRecord => COMMIT_RECORD,
            FrameKind::PreparedTransactionManifest => PREPARED_TRANSACTION_MANIFEST,
            FrameKind::SignatureExtension => SIGNATURE_EXTENSION,
            FrameKind::SearchIndex => SEARCH_INDEX,
            FrameKind::CommitFooter => COMMIT_FOOTER,
            FrameKind::ArchiveDescriptor => ARCHIVE_DESCRIPTOR,
            FrameKind::RecoveryManifest => RECOVERY_MANIFEST,
            FrameKind::EncryptedObject => ENCRYPTED_OBJECT,
            FrameKind::KeyEnvelope => KEY_ENVELOPE,
            FrameKind::CompressionDictionary => COMPRESSION_DICTIONARY,
            FrameKind::FooterHistory => FOOTER_HISTORY,
            FrameKind::ParityObject => PARITY_OBJECT,
            FrameKind::Reserved => return None,
        })
    }

    /// Every MOCHI control record is a skippable frame; only Zstandard data
    /// frames are not (spec §8.1).
    pub const fn is_skippable(self) -> bool {
        !matches!(self, FrameKind::ZstdData)
    }

    /// The single absolute offset at which this kind may appear, if it is
    /// restricted. Only the archive descriptor is (D12).
    pub const fn required_offset(self) -> Option<u64> {
        match self {
            FrameKind::ArchiveDescriptor => Some(DESCRIPTOR_OFFSET),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_skippable_magic_is_registered_and_skippable() {
        for magic in SKIPPABLE_MIN..=SKIPPABLE_MAX {
            let kind = FrameKind::from_magic(magic)
                .unwrap_or_else(|| panic!("{magic:#010x} missing from registry"));
            assert!(kind.is_skippable(), "{magic:#010x}");
        }
    }

    #[test]
    fn values_outside_the_registry_are_not_classified() {
        assert_eq!(FrameKind::from_magic(SKIPPABLE_MIN - 1), None);
        assert_eq!(FrameKind::from_magic(SKIPPABLE_MAX + 1), None);
        assert_eq!(FrameKind::from_magic(0), None);
    }

    #[test]
    fn magic_round_trips_for_non_reserved_kinds() {
        for magic in SKIPPABLE_MIN..=SKIPPABLE_MAX {
            let kind = FrameKind::from_magic(magic).unwrap();
            match kind.magic() {
                Some(m) => assert_eq!(m, magic),
                None => assert_eq!(kind, FrameKind::Reserved),
            }
        }
        assert_eq!(FrameKind::ZstdData.magic(), Some(ZSTD_DATA_FRAME));
    }

    #[test]
    fn exactly_three_reserved_magics() {
        let reserved = (SKIPPABLE_MIN..=SKIPPABLE_MAX)
            .filter(|m| FrameKind::from_magic(*m) == Some(FrameKind::Reserved))
            .count();
        assert_eq!(reserved, 3);
    }

    #[test]
    fn only_the_descriptor_has_a_required_offset() {
        assert_eq!(
            FrameKind::ArchiveDescriptor.required_offset(),
            Some(0),
            "D12: offset 0"
        );
        assert_eq!(FrameKind::ArchiveDescriptor.magic(), Some(0x184D_2A57));
        for magic in SKIPPABLE_MIN..=SKIPPABLE_MAX {
            let kind = FrameKind::from_magic(magic).unwrap();
            if kind != FrameKind::ArchiveDescriptor {
                assert_eq!(kind.required_offset(), None, "{magic:#010x}");
            }
        }
        assert_eq!(FrameKind::ZstdData.required_offset(), None);
    }

    #[test]
    fn sidecar_magic_matches_b_2_2() {
        assert_eq!(&TAIL_QUARANTINE_MAGIC, b"MOCHITQ\0");
        // Not mistakable for a frame: its first four bytes are no frame magic.
        let head = u32::from_le_bytes([
            TAIL_QUARANTINE_MAGIC[0],
            TAIL_QUARANTINE_MAGIC[1],
            TAIL_QUARANTINE_MAGIC[2],
            TAIL_QUARANTINE_MAGIC[3],
        ]);
        assert_eq!(FrameKind::from_magic(head), None);
    }

    #[test]
    fn footer_geometry_matches_spec_8_4() {
        assert_eq!(SKIPPABLE_HEADER_LEN + FOOTER_PAYLOAD_LEN, 72);
        assert_eq!(&FOOTER_MAGIC[..6], b"MOCHI2");
        assert_eq!(&FOOTER_MAGIC[6..], &[0, 0]);
    }
}
