//! Archive descriptor (spec Annex B.2 D12; schema
//! `docs/schemas/archive-descriptor-v0.cddl`). Ratified at 1.0 (R1, R2; D24).
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
//! build's reading of D12 and §26, which do not say.
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
use mochi_format::parity::{MAX_PERCENT, MIN_PERCENT, SUITE_CAUCHY_GF256};
use mochi_format::registry::{FrameKind, DESCRIPTOR_OFFSET, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::seal::FEATURE_ENCRYPTED;
use mochi_format::version::WIRE_GENERATION;
use mochi_format::Limits;

use crate::error::{ErrorCode, MochiError, Result};
use crate::object::ArchiveId;

/// Descriptor schema version (key 0).
pub const SCHEMA_VERSION: u64 = 0;

/// The last pre-1.0 draft identifier (key 3): 2 = the layout frozen at the
/// format freeze (gate G12; R1 to R4; plan K13). This build never writes it:
/// a 1.0 writer writes null (a ratified archive, spec B.2.14 item 2). Only
/// `mochi migrate` reads it ([`DraftPolicy::AllowLastDraft`]); every other
/// command refuses it with `UNSUPPORTED_FEATURE`. Draft 1 was the Annex B.2
/// wire batch before the freeze (R4 `user_version` 0) and is refused without
/// the migrate hint (K25).
pub const DRAFT_ID: u64 = 2;

#[cfg(any(test, feature = "test-controls"))]
thread_local! {
    static LEGACY_DRAFT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Test control: archives **created on this thread** carry `draft` in
/// descriptor key 3 instead of null, until it is set back to `None`. It
/// builds the draft-2 sources `mochi migrate` is tested on; the one other
/// difference between draft 2 and a ratified archive is the catalog image,
/// which draft 2 stores uncompressed (payload encoding 0; B.2.15). Returns the previous value. Production code cannot call this.
#[cfg(any(test, feature = "test-controls"))]
pub fn set_legacy_draft_for_tests(draft: Option<u64>) -> Option<u64> {
    LEGACY_DRAFT.with(|c| c.replace(draft))
}

/// The draft a new descriptor is stamped with: always null in production.
pub(crate) fn new_archive_draft() -> Option<u64> {
    #[cfg(any(test, feature = "test-controls"))]
    {
        LEGACY_DRAFT.with(std::cell::Cell::get)
    }
    #[cfg(not(any(test, feature = "test-controls")))]
    {
        None
    }
}

/// Whether a read accepts the last draft (spec B.2.14). Ratified archives
/// (key 3 = null) are always accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DraftPolicy {
    /// Every command but `migrate`: a draft archive is refused, naming it.
    #[default]
    RatifiedOnly,
    /// `migrate`: draft [`DRAFT_ID`] is read too. Other drafts stay refused.
    AllowLastDraft,
}

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
/// D4, D12). All are fixed for the life of an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Profile {
    /// The TAR-compatible profile (descriptor constraint 0, D4).
    pub tar_compatible: bool,
    /// The Encrypted profile (§7.3, D3).
    pub encrypted: bool,
    /// The Redundancy profile (§7.4, Annex B.2.11 D21): the parity
    /// percentage, 1–100, of suite 1. Descriptor constraint 1.
    pub redundancy: Option<u16>,
}

/// Descriptor constraint 1: the Redundancy profile's suite and parity
/// percentage (D21). Every commit carries parity frames laid out from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Redundancy {
    pub suite: u16,
    pub percent: u16,
}

/// A decoded descriptor. The wire generation is not a field: this build
/// accepts exactly one value and refuses the rest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub archive_id: ArchiveId,
    pub required_features: Vec<u64>,
    /// Constraint 0: TAR-compatible profile, fixed at creation (D4).
    pub tar_compatible: bool,
    /// Constraint 1: the Redundancy profile (D21), fixed at creation.
    pub redundancy: Option<Redundancy>,
    /// Present only when the writer opted into larger limits (key 6).
    pub declared_limits: Option<DeclaredLimits>,
    /// Key 3: `None` is a ratified archive (null); `Some(DRAFT_ID)` is the
    /// last draft, readable only under [`DraftPolicy::AllowLastDraft`].
    /// Always `None` on a descriptor this build writes.
    pub draft: Option<u64>,
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
            redundancy: None,
            declared_limits: None,
            draft: None,
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
            redundancy: None,
            declared_limits: None,
            draft: None,
        }
    }

    /// A descriptor for `profile`: Encrypted or not, TAR-compatible or not,
    /// and with or without parity (D21). The combinations the profiles
    /// forbid are refused by the writer, not here.
    pub fn for_profile(archive_id: ArchiveId, profile: Profile) -> Self {
        let mut d = if profile.encrypted {
            Descriptor::new_encrypted(archive_id)
        } else {
            Descriptor::new(archive_id, profile.tar_compatible)
        };
        d.redundancy = profile.redundancy.map(|percent| Redundancy {
            suite: SUITE_CAUCHY_GF256,
            percent,
        });
        d
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
            redundancy: self.redundancy.map(|r| r.percent),
        }
    }

    fn to_value(&self) -> Value {
        let mut m = vec![
            (0, Value::Uint(SCHEMA_VERSION)),
            (1, Value::Bytes(self.archive_id.as_bytes().to_vec())),
            (2, Value::Uint(u64::from(WIRE_GENERATION))),
            (3, self.draft.map_or(Value::Null, Value::Uint)),
            (
                4,
                Value::Array(
                    self.required_features
                        .iter()
                        .map(|f| Value::Uint(*f))
                        .collect(),
                ),
            ),
            (5, {
                let mut c = vec![(0, Value::Bool(self.tar_compatible))];
                if let Some(r) = self.redundancy {
                    c.push((
                        1,
                        Value::Map(vec![
                            (0, Value::Uint(u64::from(r.suite))),
                            (1, Value::Uint(u64::from(r.percent))),
                        ]),
                    ));
                }
                Value::Map(c)
            }),
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
        Self::decode_with(payload, limits, cbor_limits, DraftPolicy::RatifiedOnly)
    }

    /// [`Descriptor::decode`] under an explicit [`DraftPolicy`].
    pub fn decode_with(
        payload: &[u8],
        limits: &Limits,
        cbor_limits: &CborLimits,
        drafts: DraftPolicy,
    ) -> Result<Self> {
        Self::decode_inner(payload, limits, cbor_limits, drafts).map_err(classify)
    }

    fn decode_inner(
        payload: &[u8],
        limits: &Limits,
        cbor_limits: &CborLimits,
        drafts: DraftPolicy,
    ) -> Result<Self> {
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
        let draft = match f.req(3)? {
            Value::Null => None,
            Value::Uint(DRAFT_ID) if drafts == DraftPolicy::AllowLastDraft => Some(DRAFT_ID),
            Value::Uint(DRAFT_ID) => {
                return Err(refuse(format!(
                    "archive was written under draft {DRAFT_ID}, a pre-1.0 layout; this build \
                     reads ratified archives only. Convert it with `mochi migrate SRC --output \
                     DST`"
                )))
            }
            Value::Uint(other) => {
                return Err(refuse(match drafts {
                    DraftPolicy::RatifiedOnly => format!(
                        "archive was written under draft {other}, a pre-1.0 layout; this build \
                         reads ratified archives only"
                    ),
                    DraftPolicy::AllowLastDraft => format!(
                        "archive was written under draft {other}; migration reads only draft \
                         {DRAFT_ID}"
                    ),
                }))
            }
            _ => {
                return Err(MochiError::from(mochi_format::FormatError::Schema(
                    "draft identifier: expected an unsigned integer or null".into(),
                )))
            }
        };
        let required_features = f
            .req(4)?
            .array("required features")?
            .iter()
            .map(|v| v.uint("required feature"))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        check_required_features(&required_features, KNOWN_REQUIRED_FEATURES, limits)?;

        let (tar_compatible, redundancy) = {
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
            let redundancy = match cf.opt(1) {
                None => None,
                Some(r) => {
                    let mut rf = Fields::of(r, "redundancy")?;
                    let suite = rf.req(0)?.uint("parity suite")?;
                    let percent = rf.req(1)?.uint("parity percentage")?;
                    rf.finish()?;
                    if suite != u64::from(SUITE_CAUCHY_GF256) {
                        return Err(refuse(format!(
                            "the archive's parity uses suite {suite}; this build knows only \
                             suite {SUITE_CAUCHY_GF256}"
                        )));
                    }
                    let percent = u16::try_from(percent)
                        .ok()
                        .filter(|p| (MIN_PERCENT..=MAX_PERCENT).contains(p))
                        .ok_or_else(|| {
                            invalid(format!(
                                "parity percentage {percent} is outside \
                                 {MIN_PERCENT}–{MAX_PERCENT}"
                            ))
                        })?;
                    Some(Redundancy {
                        suite: SUITE_CAUCHY_GF256,
                        percent,
                    })
                }
            };
            cf.finish()?;
            (tar, redundancy)
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
            redundancy,
            declared_limits,
            draft,
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
        Self::from_stored_with(
            bytes,
            offset,
            limits,
            cbor_limits,
            DraftPolicy::RatifiedOnly,
        )
    }

    /// [`Descriptor::from_stored`] under an explicit [`DraftPolicy`].
    pub fn from_stored_with(
        bytes: &[u8],
        offset: u64,
        limits: &Limits,
        cbor_limits: &CborLimits,
        drafts: DraftPolicy,
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
        Self::decode_with(payload, limits, cbor_limits, drafts)
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
            0x03, 0xF6, // 3: null (a ratified archive, B.2.14)
            0x04, 0x80, // 4: []
            0x05, 0xA1, 0x00, 0xF4, // 5: {0: false}
        ]);
        assert_eq!(&s.as_bytes()[8..], &want[..]);
    }

    /// B.2.14: key 3 = 2 (the last draft) is refused with the migrate hint
    /// unless the read is `migrate`'s; any other draft is refused without it.
    #[test]
    fn drafts_are_refused_and_only_the_last_one_has_the_migrate_hint() {
        let l = Limits::default();
        let c = CborLimits::default();
        let with = |draft: Value, policy| {
            let mut v = d().to_value();
            if let Value::Map(m) = &mut v {
                m[3].1 = draft;
            }
            let payload = cbor::encode(&v).unwrap();
            Descriptor::decode_with(&payload, &l, &c, policy)
        };
        let e = with(Value::Uint(2), DraftPolicy::RatifiedOnly).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnsupportedFeature);
        assert!(e.message.contains("draft 2") && e.message.contains("mochi migrate"));
        let ok = with(Value::Uint(2), DraftPolicy::AllowLastDraft).unwrap();
        assert_eq!(ok.draft, Some(2));
        for n in [1u64, 3] {
            for p in [DraftPolicy::RatifiedOnly, DraftPolicy::AllowLastDraft] {
                let e = with(Value::Uint(n), p).unwrap_err();
                assert_eq!(e.code, ErrorCode::UnsupportedFeature);
                assert!(e.message.contains(&format!("draft {n}")));
            }
            let e = with(Value::Uint(n), DraftPolicy::RatifiedOnly).unwrap_err();
            assert!(!e.message.contains("migrate"), "{}", e.message);
        }
        assert_eq!(
            with(Value::Null, DraftPolicy::RatifiedOnly).unwrap().draft,
            None
        );
        assert_eq!(
            with(Value::Bool(true), DraftPolicy::AllowLastDraft)
                .unwrap_err()
                .code,
            ErrorCode::DescriptorInvalid
        );
    }

    /// D21: constraint 1 carries the suite and percentage; an unknown suite
    /// is refused and a percentage outside 1–100 is damage.
    #[test]
    fn redundancy_constraint_round_trips_and_is_checked() {
        let mut x = d();
        x.redundancy = Some(Redundancy {
            suite: 1,
            percent: 10,
        });
        assert_eq!(rt(&x).unwrap(), x);
        assert_eq!(x.profile().redundancy, Some(10));
        let want = [0x05, 0xA2, 0x00, 0xF4, 0x01, 0xA2, 0x00, 0x01, 0x01, 0x0A];
        let s = x.to_stored().unwrap();
        assert!(s.as_bytes().windows(want.len()).any(|w| w == want));
        for (suite, percent, code) in [
            (2, 10, ErrorCode::UnsupportedFeature),
            (1, 0, ErrorCode::DescriptorInvalid),
            (1, 101, ErrorCode::DescriptorInvalid),
        ] {
            x.redundancy = Some(Redundancy { suite, percent });
            assert_eq!(rt(&x).unwrap_err().code, code, "{suite} {percent}");
        }
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
                encrypted: true,
                redundancy: None,
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
