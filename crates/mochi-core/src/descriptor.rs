//! Archive descriptor (spec Annex B.2 D12; schema
//! `docs/schemas/archive-descriptor-v0.cddl`). **DRAFT** (R1, R2).
//!
//! One frame of kind `ARCHIVE_DESCRIPTOR` at offset 0, written once at
//! creation and never rewritten. It holds creation-time facts only and never
//! locates key envelopes. Every v1 commit references it by stored-object hash
//! (commit key 10), so it is bound to a head exactly as manifests and images
//! are: relative to a trusted head (D8), as integrity, not authentication.
//!
//! # Errors
//!
//! D12: a missing, damaged, or mismatched descriptor is `DESCRIPTOR_INVALID`
//! (exit 1). Every decode failure maps there **except**:
//! * refusals (`UNSUPPORTED_FEATURE`, exit 4): an unknown schema version, an
//!   unknown required feature, a wire generation other than 2, or a draft
//!   identifier other than this batch's. These are not damage: a newer or
//!   older writer produced something this build will not interpret;
//! * reader limits (`LIMIT_EXCEEDED`) and I/O errors, which say nothing
//!   about the descriptor itself.
//!
//! The wire-generation and draft-identifier classifications are this
//! build's reading of D12 and §26, which do not say; recorded as open in
//! `docs/b2-implementation-checklist.md`.
//!
//! # Failure behaviour (plan T18)
//!
//! Creation writes the descriptor first and every commit binds it (key 10,
//! plan T8). When it is missing, damaged, or mismatched, head discovery,
//! commit validation, and diagnostics still work (`publish::locate_head`,
//! `publish::commit_history`); interpretation and append are refused
//! (`publish::open_at`, `ArchiveWriter::open_append`, before any byte is
//! written); and damage assessment reports it as a failed object
//! (`damage::assess_damage`, `FAIL`).
//!
//! A [`Profile`] is fixed at creation. Asking an append for a different one
//! is `PROFILE_CHANGE_UNSUPPORTED` (exit 4): there is no in-place conversion,
//! in particular none to the Encrypted profile (D12).

use mochi_format::cbor::{self, CborLimits, Fields, Value};
use mochi_format::envelope::{check_required_features, encode_cbor_record};
use mochi_format::frame::{walk_frame, FrameDetail};
use mochi_format::registry::{FrameKind, DESCRIPTOR_OFFSET, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::seal::FEATURE_ENCRYPTED;
use mochi_format::version::WIRE_GENERATION;
use mochi_format::Limits;

use crate::error::{ErrorCode, MochiError, Result};
use crate::object::ArchiveId;

/// Descriptor schema version (key 0).
pub const SCHEMA_VERSION: u64 = 0;

/// Draft identifier this build writes and reads (key 3): 1 = the Annex B.2
/// wire batch. A ratified archive carries null.
pub const DRAFT_ID: u64 = 1;

/// Required features this build understands: the Encrypted profile's
/// identifier (Annex B.2.10 D20 item 4, suite 1). Anything else fails closed.
pub const KNOWN_REQUIRED_FEATURES: &[u64] = &[FEATURE_ENCRYPTED];

/// Limits a writer declared on opting out of the defaults (key 6; B.2.3).
/// Advisory only: readers never raise their own limits from these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredLimits {
    pub max_skippable_payload: u64,
    pub max_cbor_items: u64,
    pub max_image_len: u64,
}

/// The creation-time profile choices a writer can be asked for (spec §7,
/// D4, D12). Both are fixed for the life of an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Profile {
    /// The TAR-compatible profile (descriptor constraint 0, D4).
    pub tar_compatible: bool,
    /// The Encrypted profile (§7.3, D3).
    pub encrypted: bool,
}

/// A decoded descriptor. Generation and draft identifier are not fields:
/// this build accepts exactly one value of each and refuses the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub archive_id: ArchiveId,
    pub required_features: Vec<u64>,
    /// Constraint 0: TAR-compatible profile, fixed at creation (D4).
    pub tar_compatible: bool,
    /// Present only when the writer opted into larger limits (key 6).
    pub declared_limits: Option<DeclaredLimits>,
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::DescriptorInvalid, msg)
}

fn refuse(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::UnsupportedFeature, msg)
}

/// D12 classification: damage becomes `DESCRIPTOR_INVALID`; refusals,
/// reader limits, and I/O keep their codes.
fn classify(e: MochiError) -> MochiError {
    match e.code {
        ErrorCode::UnsupportedFeature | ErrorCode::LimitExceeded | ErrorCode::IoError => e,
        _ => invalid(format!("archive descriptor: {}", e.message)),
    }
}

impl Descriptor {
    /// A descriptor for a new archive with default limits and no features.
    pub fn new(archive_id: ArchiveId, tar_compatible: bool) -> Self {
        Descriptor {
            archive_id,
            required_features: Vec::new(),
            tar_compatible,
            declared_limits: None,
        }
    }

    /// A descriptor for a new Encrypted-profile archive (D20): required
    /// feature 1, not TAR-compatible. It never locates key envelopes (D12);
    /// commits do.
    pub fn new_encrypted(archive_id: ArchiveId) -> Self {
        Descriptor {
            archive_id,
            required_features: vec![FEATURE_ENCRYPTED],
            tar_compatible: false,
            declared_limits: None,
        }
    }

    /// Whether the archive was created in the Encrypted profile: the
    /// required-feature identifier of D20 is listed.
    pub fn encrypted(&self) -> bool {
        self.required_features.contains(&FEATURE_ENCRYPTED)
    }

    /// The profile this archive was created with.
    pub fn profile(&self) -> Profile {
        Profile {
            tar_compatible: self.tar_compatible,
            encrypted: self.encrypted(),
        }
    }

    fn to_value(&self) -> Value {
        let mut m = vec![
            (0, Value::Uint(SCHEMA_VERSION)),
            (1, Value::Bytes(self.archive_id.as_bytes().to_vec())),
            (2, Value::Uint(u64::from(WIRE_GENERATION))),
            (3, Value::Uint(DRAFT_ID)),
            (
                4,
                Value::Array(
                    self.required_features
                        .iter()
                        .map(|f| Value::Uint(*f))
                        .collect(),
                ),
            ),
            (5, Value::Map(vec![(0, Value::Bool(self.tar_compatible))])),
        ];
        if let Some(d) = self.declared_limits {
            m.push((
                6,
                Value::Map(vec![
                    (0, Value::Uint(d.max_skippable_payload)),
                    (1, Value::Uint(d.max_cbor_items)),
                    (2, Value::Uint(d.max_image_len)),
                ]),
            ));
        }
        Value::Map(m)
    }

    /// The stored form: one `ARCHIVE_DESCRIPTOR` frame, to be written at
    /// offset 0. Refuses unknown or unsorted features, and anything over
    /// the reader defaults (B.2.3).
    pub fn to_stored(&self) -> Result<StoredObject> {
        check_required_features(
            &self.required_features,
            KNOWN_REQUIRED_FEATURES,
            &Limits::WRITER_DEFAULT,
        )?;
        let frame = encode_cbor_record(
            FrameKind::ArchiveDescriptor,
            &self.to_value(),
            &CborLimits::default(),
            &Limits::WRITER_DEFAULT,
        )?;
        Ok(StoredObject::from_loaded(frame))
    }

    /// Decode a descriptor payload (canonical CBOR, closed schema).
    pub fn decode(payload: &[u8], limits: &Limits, cbor_limits: &CborLimits) -> Result<Self> {
        Self::decode_inner(payload, limits, cbor_limits).map_err(classify)
    }

    fn decode_inner(payload: &[u8], limits: &Limits, cbor_limits: &CborLimits) -> Result<Self> {
        let v = cbor::decode(payload, cbor_limits)?;
        let mut f = Fields::of(&v, "archive descriptor")?;

        let version = f.req(0)?.uint("descriptor schema version")?;
        if version != SCHEMA_VERSION {
            return Err(refuse(format!(
                "archive descriptor schema version {version} is not supported by this build"
            )));
        }
        let archive_id = ArchiveId::from_bytes(f.req(1)?.bytes32("archive id")?);
        let generation = f.req(2)?.uint("wire generation")?;
        if generation != u64::from(WIRE_GENERATION) {
            return Err(refuse(format!(
                "archive declares wire generation {generation}; this build reads only \
                 generation {WIRE_GENERATION} (MOCHI2)"
            )));
        }
        match f.req(3)? {
            Value::Uint(DRAFT_ID) => {}
            Value::Uint(other) => {
                return Err(refuse(format!(
                    "archive was written under draft {other}; this build reads draft {DRAFT_ID}"
                )))
            }
            Value::Null => {
                return Err(refuse(
                    "archive declares a ratified format; this is a pre-1.0 draft build",
                ))
            }
            _ => {
                return Err(MochiError::from(mochi_format::FormatError::Schema(
                    "draft identifier: expected an unsigned integer or null".into(),
                )))
            }
        }
        let required_features = f
            .req(4)?
            .array("required features")?
            .iter()
            .map(|v| v.uint("required feature"))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        check_required_features(&required_features, KNOWN_REQUIRED_FEATURES, limits)?;

        let tar_compatible = {
            let c = f.req(5)?;
            let mut cf = Fields::of(c, "constraints")?;
            let tar = match cf.req(0)? {
                Value::Bool(b) => *b,
                _ => {
                    return Err(MochiError::from(mochi_format::FormatError::Schema(
                        "constraints: TAR-compatible must be a bool".into(),
                    )))
                }
            };
            cf.finish()?;
            tar
        };
        let declared_limits = match f.opt(6) {
            None => None,
            Some(d) => {
                let mut df = Fields::of(d, "declared limits")?;
                let out = DeclaredLimits {
                    max_skippable_payload: df.req(0)?.uint("declared skippable payload")?,
                    max_cbor_items: df.req(1)?.uint("declared CBOR items")?,
                    max_image_len: df.req(2)?.uint("declared image length")?,
                };
                df.finish()?;
                Some(out)
            }
        };
        f.finish()?;
        // D20 item 4: an Encrypted archive cannot be TAR-compatible, whatever
        // the two fields were each written as.
        if required_features.contains(&FEATURE_ENCRYPTED) && tar_compatible {
            return Err(invalid(
                "the descriptor declares both the Encrypted profile and the TAR-compatible                  constraint, which cannot be combined (Annex B.2.10 D20 item 4)",
            ));
        }
        Ok(Descriptor {
            archive_id,
            required_features,
            tar_compatible,
            declared_limits,
        })
    }

    /// Parse a stored descriptor found at absolute `offset`: exactly one
    /// `ARCHIVE_DESCRIPTOR` frame, at offset 0 (D12). The offset is passed
    /// because `bytes` is a copy, and the copy's own position says nothing
    /// about where it came from (`walk_frame` documents this).
    pub fn from_stored(
        bytes: &[u8],
        offset: u64,
        limits: &Limits,
        cbor_limits: &CborLimits,
    ) -> Result<Self> {
        if offset != DESCRIPTOR_OFFSET {
            return Err(invalid(format!(
                "archive descriptor referenced at offset {offset}; only offset 0 is valid"
            )));
        }
        let span = walk_frame(bytes, 0, limits).map_err(|e| classify(e.into()))?;
        if span.kind != FrameKind::ArchiveDescriptor || span.len != bytes.len() as u64 {
            return Err(invalid("not exactly one archive-descriptor frame"));
        }
        let FrameDetail::Skippable { .. } = span.detail else {
            return Err(invalid("archive descriptor is not a skippable frame"));
        };
        let payload = bytes
            .get(SKIPPABLE_HEADER_LEN..)
            .ok_or_else(|| invalid("descriptor payload out of range"))?;
        Self::decode(payload, limits, cbor_limits)
    }

    /// D12 "mismatched": the descriptor must name the archive of the commit
    /// that references it.
    pub fn check_archive_id(&self, expected: &ArchiveId) -> Result<()> {
        if &self.archive_id != expected {
            return Err(invalid(
                "archive descriptor names a different archive than the commit that references it",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> Descriptor {
        Descriptor::new(ArchiveId::from_bytes([7; 32]), false)
    }

    fn rt(x: &Descriptor) -> Result<Descriptor> {
        let s = x.to_stored()?;
        Descriptor::from_stored(s.as_bytes(), 0, &Limits::default(), &CborLimits::default())
    }

    #[test]
    fn round_trips_with_and_without_declared_limits() {
        assert_eq!(rt(&d()).unwrap(), d());
        let mut x = d();
        x.tar_compatible = true;
        x.declared_limits = Some(DeclaredLimits {
            max_skippable_payload: 1 << 30,
            max_cbor_items: 1 << 26,
            max_image_len: 1 << 30,
        });
        assert_eq!(rt(&x).unwrap(), x);
    }

    #[test]
    fn exact_encoding_of_the_minimal_descriptor() {
        let s = d().to_stored().unwrap();
        let mut want = vec![
            0xA6, // map(6)
            0x00, 0x00, // 0: 0
            0x01, 0x58, 0x20, // 1: bytes(32)
        ];
        want.extend([7u8; 32]);
        want.extend([
            0x02, 0x02, // 2: 2
            0x03, 0x01, // 3: 1
            0x04, 0x80, // 4: []
            0x05, 0xA1, 0x00, 0xF4, // 5: {0: false}
        ]);
        assert_eq!(&s.as_bytes()[8..], &want[..]);
    }

    #[test]
    fn only_offset_zero_is_accepted() {
        let s = d().to_stored().unwrap();
        let e =
            Descriptor::from_stored(s.as_bytes(), 1, &Limits::default(), &CborLimits::default())
                .unwrap_err();
        assert_eq!(e.code, ErrorCode::DescriptorInvalid);
    }

    #[test]
    fn mismatched_archive_id_is_descriptor_invalid() {
        let e = d()
            .check_archive_id(&ArchiveId::from_bytes([8; 32]))
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::DescriptorInvalid);
        assert!(d()
            .check_archive_id(&ArchiveId::from_bytes([7; 32]))
            .is_ok());
    }

    #[test]
    fn writer_refuses_unknown_or_unsorted_features() {
        // Feature 1 is the Encrypted profile (D20); 2 is not assigned.
        let mut x = d();
        x.required_features = vec![2];
        assert_eq!(
            x.to_stored().unwrap_err().code,
            ErrorCode::UnsupportedFeature
        );
        x.required_features = vec![2, 1];
        assert_eq!(x.to_stored().unwrap_err().code, ErrorCode::EnvelopeInvalid);
    }

    /// D20 item 4: feature 1 makes an Encrypted descriptor, with no schema
    /// change; combined with the TAR constraint it is `DESCRIPTOR_INVALID`.
    #[test]
    fn the_encrypted_feature_makes_an_encrypted_descriptor() {
        let e = Descriptor::new_encrypted(ArchiveId::from_bytes([7; 32]));
        assert!(e.encrypted() && !e.tar_compatible);
        assert_eq!(
            e.profile(),
            Profile {
                tar_compatible: false,
                encrypted: true
            }
        );
        assert_eq!(rt(&e).unwrap(), e);
        assert!(!d().encrypted());
        // Same wire shape as Core except key 4 lists 1.
        let want = [0x04, 0x81, 0x01, 0x05, 0xA1, 0x00, 0xF4];
        let s = e.to_stored().unwrap();
        assert!(s.as_bytes().windows(want.len()).any(|w| w == want));

        let mut both = e.clone();
        both.tar_compatible = true;
        let stored = both.to_stored().unwrap();
        let err = Descriptor::from_stored(
            stored.as_bytes(),
            0,
            &Limits::default(),
            &CborLimits::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::DescriptorInvalid);
    }
}
