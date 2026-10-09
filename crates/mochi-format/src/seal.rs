//! Sealed objects and key wrapping (spec Annex B.2.10 items 3 and 5;
//! `docs/ratification/R5-crypto-draft.md` §3–§4).
//!
//! Two constructions, both XChaCha20-Poly1305 with a fresh random 24-byte
//! nonce for every encryption and no counter anywhere (spec §14.2):
//!
//! * **Key wrap.** A KEK derived from a passphrase ([`crate::kdf`]) wraps the
//!   archive's data key. The associated data names the archive, the envelope,
//!   the key, the suite, and the exact KDF-parameter bytes, so an envelope
//!   edited in the file fails to unwrap instead of weakening the key.
//! * **Sealed object.** A data chunk, catalog image, or manifest is sealed
//!   under the data key and stored as the payload of one `ENCRYPTED_OBJECT` frame:
//!   a 48-byte header (version, suite, kind, key ID, nonce), the ciphertext,
//!   and the 16-byte tag. The associated data binds the archive ID, the
//!   header, and what the object *is*: its object ID for a chunk, its commit's
//!   sequence and transaction ID for an image or manifest. A sealed frame
//!   therefore opens only in its own archive, at its own object or commit.
//!
//! Everything fixed-width is concatenated in a fixed order, with the one
//! variable-length item (the KDF-parameter bytes) last, so the associated data
//! is unambiguous without length prefixes.
//!
//! The stored-object hash covers the whole frame (header, ciphertext, tag), so
//! stored integrity needs no key (spec §20.1). Whether a failed open means a
//! wrong key or a damaged object is the caller's call: it knows whether the
//! stored hash already verified. This module only reports
//! [`SealFault::Authentication`].

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::digest::{KEY_WRAP_DOMAIN, OBJECT_SEAL_DOMAIN};
use crate::error::{FormatError, Result, SealFault};
use crate::frame::{walk_frame, FrameDetail};
use crate::limits::Limits;
use crate::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use crate::repr::StoredObject;
use crate::secret::{DataKey, Kek, Random};

/// Sealed-object version this build writes and reads.
pub const SEAL_VERSION: u16 = 0;
/// Suite 1: XChaCha20-Poly1305 with Argon2id (D3). Also the meaning of
/// required-feature identifier [`FEATURE_ENCRYPTED`].
pub const SUITE_XCHACHA20_POLY1305: u16 = 1;
/// The Encrypted profile's required-feature identifier (D20 item 4): the first
/// unassigned value, naming suite 1.
pub const FEATURE_ENCRYPTED: u64 = 1;

/// Length of the sealed-object header.
pub const SEAL_HEADER_LEN: usize = 48;
/// The shortest possible sealed frame: the 8-byte skippable header, the
/// sealed header, one byte of ciphertext (a sealed object is never empty), and
/// the tag.
pub const MIN_SEALED_FRAME_LEN: u64 = 8 + 48 + 1 + 16;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;
/// XChaCha20 nonce length.
pub const NONCE_LEN: usize = 24;
/// Key ID length.
pub const KEY_ID_LEN: usize = 16;
/// Length of the wrapped data key: 32 bytes of ciphertext and the tag.
pub const WRAPPED_DEK_LEN: usize = 32 + TAG_LEN;

/// The random 128-bit name of an archive's data key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId([u8; KEY_ID_LEN]);

impl KeyId {
    pub const fn from_bytes(bytes: [u8; KEY_ID_LEN]) -> Self {
        KeyId(bytes)
    }
    pub fn generate(rng: &mut dyn Random) -> Result<Self> {
        let mut b = [0u8; KEY_ID_LEN];
        rng.fill(&mut b)?;
        Ok(KeyId(b))
    }
    pub const fn as_bytes(&self) -> &[u8; KEY_ID_LEN] {
        &self.0
    }
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KeyId({})", self.to_hex())
    }
}

/// What kind of object a sealed frame holds (header bytes 4 to 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SealKind {
    Chunk = 0,
    Image = 1,
    DeltaManifest = 2,
    SnapshotManifest = 3,
}

impl SealKind {
    pub const fn code(self) -> u32 {
        self as u32
    }

    pub fn from_code(code: u32) -> Result<Self> {
        Ok(match code {
            0 => SealKind::Chunk,
            1 => SealKind::Image,
            2 => SealKind::DeltaManifest,
            3 => SealKind::SnapshotManifest,
            kind => return Err(FormatError::Seal(SealFault::UnknownKind { kind })),
        })
    }
}

/// What a sealed object *is*: the kind and the binding the associated data
/// carries (B.2.10 item 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealTarget {
    /// A data chunk, bound to its 32-byte object ID.
    Chunk { object_id: [u8; 32] },
    /// A catalog image of the commit with this sequence and transaction ID.
    Image {
        sequence: u64,
        transaction_id: [u8; 16],
    },
    /// A delta manifest of that commit.
    DeltaManifest {
        sequence: u64,
        transaction_id: [u8; 16],
    },
    /// A snapshot manifest of that commit.
    SnapshotManifest {
        sequence: u64,
        transaction_id: [u8; 16],
    },
}

impl SealTarget {
    pub fn kind(&self) -> SealKind {
        match self {
            SealTarget::Chunk { .. } => SealKind::Chunk,
            SealTarget::Image { .. } => SealKind::Image,
            SealTarget::DeltaManifest { .. } => SealKind::DeltaManifest,
            SealTarget::SnapshotManifest { .. } => SealKind::SnapshotManifest,
        }
    }

    fn binding(&self) -> Vec<u8> {
        match self {
            SealTarget::Chunk { object_id } => object_id.to_vec(),
            SealTarget::Image {
                sequence,
                transaction_id,
            }
            | SealTarget::DeltaManifest {
                sequence,
                transaction_id,
            }
            | SealTarget::SnapshotManifest {
                sequence,
                transaction_id,
            } => {
                let mut v = sequence.to_le_bytes().to_vec();
                v.extend_from_slice(transaction_id);
                v
            }
        }
    }
}

/// The key and the archive an object is sealed for or opened in.
#[derive(Debug, Clone, Copy)]
pub struct SealContext<'a> {
    pub key: &'a DataKey,
    pub key_id: KeyId,
    pub archive_id: [u8; 32],
}

/// The parsed, unauthenticated header of a sealed object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealHeader {
    pub kind: SealKind,
    pub key_id: KeyId,
    pub nonce: [u8; NONCE_LEN],
}

/// Parse a sealed payload's header and split off ciphertext-and-tag. Nothing
/// is authenticated yet: a caller that has not verified the stored-object hash
/// treats the result as a candidate (spec §8.5).
pub fn parse_sealed(payload: &[u8]) -> Result<(SealHeader, &[u8])> {
    if payload.len() < SEAL_HEADER_LEN + 1 + TAG_LEN {
        return Err(FormatError::Seal(SealFault::TooShort));
    }
    let u16_at = |at: usize| u16::from_le_bytes([payload[at], payload[at + 1]]);
    let version = u16_at(0);
    if version != SEAL_VERSION {
        return Err(FormatError::Seal(SealFault::UnknownVersion { version }));
    }
    let suite = u16_at(2);
    if suite != SUITE_XCHACHA20_POLY1305 {
        return Err(FormatError::Seal(SealFault::UnknownSuite { suite }));
    }
    let kind = SealKind::from_code(u32::from_le_bytes([
        payload[4], payload[5], payload[6], payload[7],
    ]))?;
    let mut key_id = [0u8; KEY_ID_LEN];
    key_id.copy_from_slice(&payload[8..24]);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&payload[24..48]);
    Ok((
        SealHeader {
            kind,
            key_id: KeyId(key_id),
            nonce,
        },
        &payload[SEAL_HEADER_LEN..],
    ))
}

/// The associated data of a sealed object (R5 §4): the domain prefix, the
/// archive ID, header bytes 0 to 23, and the binding.
fn object_aad(archive_id: &[u8; 32], header_prefix: &[u8; 24], target: &SealTarget) -> Vec<u8> {
    let mut aad = Vec::with_capacity(OBJECT_SEAL_DOMAIN.len() + 32 + 24 + 32);
    aad.extend_from_slice(OBJECT_SEAL_DOMAIN);
    aad.extend_from_slice(archive_id);
    aad.extend_from_slice(header_prefix);
    aad.extend_from_slice(&target.binding());
    aad
}

fn header_prefix(kind: SealKind, key_id: &KeyId) -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..2].copy_from_slice(&SEAL_VERSION.to_le_bytes());
    h[2..4].copy_from_slice(&SUITE_XCHACHA20_POLY1305.to_le_bytes());
    h[4..8].copy_from_slice(&kind.code().to_le_bytes());
    h[8..24].copy_from_slice(key_id.as_bytes());
    h
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

/// Seal `plaintext` for `target`: the payload of a sealed frame (header,
/// ciphertext, tag), with a nonce drawn from `rng` for this encryption alone.
pub fn seal_payload(
    ctx: &SealContext<'_>,
    target: &SealTarget,
    plaintext: &[u8],
    rng: &mut dyn Random,
) -> Result<Vec<u8>> {
    if plaintext.is_empty() {
        return Err(FormatError::InvalidArgument("a sealed object is not empty"));
    }
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce)?;
    let prefix = header_prefix(target.kind(), &ctx.key_id);
    let aad = object_aad(&ctx.archive_id, &prefix, target);
    let ct = cipher(ctx.key.as_bytes())
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| FormatError::Seal(SealFault::PrimitiveFailed))?;
    let mut out = Vec::with_capacity(SEAL_HEADER_LEN + ct.len());
    out.extend_from_slice(&prefix);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// [`seal_payload`], framed as one `ENCRYPTED_OBJECT` skippable frame within
/// `limits` (the writer default rule, B.2.3).
pub fn seal_frame(
    ctx: &SealContext<'_>,
    target: &SealTarget,
    plaintext: &[u8],
    rng: &mut dyn Random,
    limits: &Limits,
) -> Result<StoredObject> {
    let payload = seal_payload(ctx, target, plaintext, rng)?;
    let frame =
        crate::frame::encode_skippable_frame_within(FrameKind::EncryptedObject, &payload, limits)?;
    Ok(StoredObject::from_codec(frame))
}

/// Open a sealed payload for `target`. Checks, in this order: the header
/// parses; its kind is the target's; its key ID is the context's; then the
/// AEAD tag. A tag failure is [`SealFault::Authentication`].
pub fn open_payload(ctx: &SealContext<'_>, target: &SealTarget, payload: &[u8]) -> Result<Vec<u8>> {
    let (header, ciphertext) = parse_sealed(payload)?;
    if header.kind != target.kind() {
        return Err(FormatError::Seal(SealFault::KindMismatch));
    }
    if header.key_id != ctx.key_id {
        return Err(FormatError::Seal(SealFault::KeyIdMismatch));
    }
    let mut prefix = [0u8; 24];
    prefix.copy_from_slice(&payload[0..24]);
    let aad = object_aad(&ctx.archive_id, &prefix, target);
    cipher(ctx.key.as_bytes())
        .decrypt(
            &XNonce::from(header.nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| FormatError::Seal(SealFault::Authentication))
}

/// The payload of `stored`, which must be exactly one `ENCRYPTED_OBJECT` frame.
pub fn sealed_frame_payload<'a>(stored: &'a StoredObject, limits: &Limits) -> Result<&'a [u8]> {
    let bytes = stored.as_bytes();
    let span = walk_frame(bytes, 0, limits)?;
    match span.detail {
        FrameDetail::Skippable { .. }
            if span.kind == FrameKind::EncryptedObject && span.len == bytes.len() as u64 =>
        {
            bytes
                .get(SKIPPABLE_HEADER_LEN..)
                .ok_or(FormatError::Truncated {
                    offset: SKIPPABLE_HEADER_LEN as u64,
                })
        }
        _ => Err(FormatError::Codec(crate::error::CodecFault::NotSingleFrame)),
    }
}

// ---- key wrap ------------------------------------------------------------------

/// What a key wrap is bound to (R5 §3).
#[derive(Debug, Clone, Copy)]
pub struct KeyWrapContext<'a> {
    pub archive_id: &'a [u8; 32],
    pub envelope_id: &'a [u8; 16],
    pub key_id: &'a KeyId,
    pub suite: u16,
    /// The exact bytes of the envelope's KDF-parameter map (its canonical
    /// CBOR), as encoded in the envelope.
    pub kdf_bytes: &'a [u8],
}

/// The wrap's associated data: `KEY_WRAP_DOMAIN`, archive ID, envelope ID, key
/// ID, suite (u16 little-endian), then the KDF-parameter bytes to the end.
pub fn wrap_aad(ctx: &KeyWrapContext<'_>) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(KEY_WRAP_DOMAIN.len() + 32 + 16 + 16 + 2 + ctx.kdf_bytes.len());
    aad.extend_from_slice(KEY_WRAP_DOMAIN);
    aad.extend_from_slice(ctx.archive_id);
    aad.extend_from_slice(ctx.envelope_id);
    aad.extend_from_slice(ctx.key_id.as_bytes());
    aad.extend_from_slice(&ctx.suite.to_le_bytes());
    aad.extend_from_slice(ctx.kdf_bytes);
    aad
}

/// Wrap `dek` under `kek` with a fresh random nonce.
pub fn wrap_dek(
    kek: &Kek,
    dek: &DataKey,
    ctx: &KeyWrapContext<'_>,
    rng: &mut dyn Random,
) -> Result<([u8; NONCE_LEN], [u8; WRAPPED_DEK_LEN])> {
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce)?;
    let aad = wrap_aad(ctx);
    let ct = cipher(kek.as_bytes())
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: dek.as_bytes().as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| FormatError::Seal(SealFault::PrimitiveFailed))?;
    let wrapped: [u8; WRAPPED_DEK_LEN] = ct
        .try_into()
        .map_err(|_| FormatError::Seal(SealFault::PrimitiveFailed))?;
    Ok((nonce, wrapped))
}

/// Unwrap a data key. A failed open is [`SealFault::Authentication`]: with a
/// stored hash that already verified, it means the passphrase is wrong.
pub fn unwrap_dek(
    kek: &Kek,
    nonce: &[u8; NONCE_LEN],
    wrapped: &[u8; WRAPPED_DEK_LEN],
    ctx: &KeyWrapContext<'_>,
) -> Result<DataKey> {
    let aad = wrap_aad(ctx);
    let mut pt = cipher(kek.as_bytes())
        .decrypt(
            &XNonce::from(*nonce),
            Payload {
                msg: wrapped.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| FormatError::Seal(SealFault::Authentication))?;
    let result = <[u8; 32]>::try_from(pt.as_slice())
        .map(DataKey::from_bytes)
        .map_err(|_| FormatError::Seal(SealFault::PrimitiveFailed));
    zeroize::Zeroize::zeroize(&mut pt);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::OsRandom;

    /// Replays a fixed byte stream, so nonces are known in a test.
    struct Fixed(Vec<u8>);
    impl Random for Fixed {
        fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
            for b in buf.iter_mut() {
                *b = self.0.remove(0);
            }
            Ok(())
        }
    }

    fn ctx(key: &DataKey) -> SealContext<'_> {
        SealContext {
            key,
            key_id: KeyId::from_bytes([9; 16]),
            archive_id: [3; 32],
        }
    }

    fn chunk(id: u8) -> SealTarget {
        SealTarget::Chunk {
            object_id: [id; 32],
        }
    }

    #[test]
    fn a_sealed_object_round_trips() {
        let key = DataKey::from_bytes([5; 32]);
        let c = ctx(&key);
        let t = chunk(1);
        let payload = seal_payload(&c, &t, b"hello sealed", &mut OsRandom).unwrap();
        assert_eq!(payload.len(), SEAL_HEADER_LEN + 12 + TAG_LEN);
        assert_eq!(open_payload(&c, &t, &payload).unwrap(), b"hello sealed");
        // Header layout: version 0, suite 1, kind 0, key ID, nonce.
        assert_eq!(&payload[0..8], &[0, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(&payload[8..24], &[9; 16]);
    }

    #[test]
    fn every_binding_is_authenticated() {
        let key = DataKey::from_bytes([5; 32]);
        let c = ctx(&key);
        let t = chunk(1);
        let payload = seal_payload(&c, &t, b"data", &mut OsRandom).unwrap();

        // Another object ID, another archive, another key: all fail the tag.
        assert_eq!(
            open_payload(&c, &chunk(2), &payload),
            Err(FormatError::Seal(SealFault::Authentication))
        );
        let mut other_archive = c;
        other_archive.archive_id = [4; 32];
        assert_eq!(
            open_payload(&other_archive, &t, &payload),
            Err(FormatError::Seal(SealFault::Authentication))
        );
        let other_key = DataKey::from_bytes([6; 32]);
        assert_eq!(
            open_payload(&ctx(&other_key), &t, &payload),
            Err(FormatError::Seal(SealFault::Authentication))
        );

        // A manifest binds sequence and transaction ID, and the kind.
        let m = SealTarget::DeltaManifest {
            sequence: 7,
            transaction_id: [1; 16],
        };
        let sealed = seal_payload(&c, &m, b"manifest", &mut OsRandom).unwrap();
        assert!(open_payload(&c, &m, &sealed).is_ok());
        for wrong in [
            SealTarget::DeltaManifest {
                sequence: 8,
                transaction_id: [1; 16],
            },
            SealTarget::DeltaManifest {
                sequence: 7,
                transaction_id: [2; 16],
            },
        ] {
            assert_eq!(
                open_payload(&c, &wrong, &sealed),
                Err(FormatError::Seal(SealFault::Authentication))
            );
        }
        let snapshot_same_commit = SealTarget::SnapshotManifest {
            sequence: 7,
            transaction_id: [1; 16],
        };
        assert_eq!(
            open_payload(&c, &snapshot_same_commit, &sealed),
            Err(FormatError::Seal(SealFault::KindMismatch))
        );
    }

    #[test]
    fn any_flipped_bit_fails_and_headers_are_checked_in_order() {
        let key = DataKey::from_bytes([5; 32]);
        let c = ctx(&key);
        let t = chunk(1);
        let payload = seal_payload(&c, &t, b"0123456789", &mut OsRandom).unwrap();
        for i in 0..payload.len() {
            let mut bad = payload.clone();
            bad[i] ^= 0x01;
            assert!(open_payload(&c, &t, &bad).is_err(), "byte {i}");
        }
        // The specific faults.
        let mut v = payload.clone();
        v[0] = 1;
        assert_eq!(
            open_payload(&c, &t, &v),
            Err(FormatError::Seal(SealFault::UnknownVersion { version: 1 }))
        );
        let mut s = payload.clone();
        s[2] = 2;
        assert_eq!(
            open_payload(&c, &t, &s),
            Err(FormatError::Seal(SealFault::UnknownSuite { suite: 2 }))
        );
        let mut k = payload.clone();
        k[4] = 9;
        assert_eq!(
            open_payload(&c, &t, &k),
            Err(FormatError::Seal(SealFault::UnknownKind { kind: 9 }))
        );
        let mut id = payload.clone();
        id[8] ^= 1;
        assert_eq!(
            open_payload(&c, &t, &id),
            Err(FormatError::Seal(SealFault::KeyIdMismatch))
        );
        // Too short for a header, one byte, and a tag.
        assert_eq!(
            open_payload(&c, &t, &payload[..SEAL_HEADER_LEN + TAG_LEN]),
            Err(FormatError::Seal(SealFault::TooShort))
        );
    }

    #[test]
    fn nonces_come_from_the_random_source_and_are_not_reused() {
        let key = DataKey::from_bytes([5; 32]);
        let c = ctx(&key);
        let t = chunk(1);
        let mut rng = Fixed((0u8..48).collect());
        let a = seal_payload(&c, &t, b"same", &mut rng).unwrap();
        let b = seal_payload(&c, &t, b"same", &mut rng).unwrap();
        assert_eq!(&a[24..48], (0u8..24).collect::<Vec<_>>().as_slice());
        assert_eq!(&b[24..48], (24u8..48).collect::<Vec<_>>().as_slice());
        assert_ne!(a, b, "the same plaintext seals differently each time");
        // Sealing with the real source twice also differs (no determinism).
        let x = seal_payload(&c, &t, b"same", &mut OsRandom).unwrap();
        let y = seal_payload(&c, &t, b"same", &mut OsRandom).unwrap();
        assert_ne!(&x[24..48], &y[24..48]);
    }

    #[test]
    fn a_frame_round_trips_and_is_one_encrypted_object_frame() {
        let key = DataKey::from_bytes([5; 32]);
        let c = ctx(&key);
        let t = chunk(1);
        let limits = Limits::default();
        let frame = seal_frame(&c, &t, b"framed", &mut OsRandom, &limits).unwrap();
        assert_eq!(
            &frame.as_bytes()[..4],
            &crate::registry::ENCRYPTED_OBJECT.to_le_bytes()
        );
        let payload = sealed_frame_payload(&frame, &limits).unwrap();
        assert_eq!(open_payload(&c, &t, payload).unwrap(), b"framed");
        // A data frame or a longer buffer is not a sealed frame.
        let mut two = frame.as_bytes().to_vec();
        two.extend_from_slice(frame.as_bytes());
        assert!(sealed_frame_payload(&StoredObject::from_loaded(two), &limits).is_err());
        let not_sealed =
            crate::frame::encode_skippable_frame(FrameKind::RecoveryManifest, b"x").unwrap();
        assert!(sealed_frame_payload(&StoredObject::from_loaded(not_sealed), &limits).is_err());
    }

    #[test]
    fn empty_plaintext_is_refused() {
        let key = DataKey::from_bytes([5; 32]);
        assert!(seal_payload(&ctx(&key), &chunk(1), b"", &mut OsRandom).is_err());
    }

    fn wrap_ctx<'a>(kdf: &'a [u8], key_id: &'a KeyId) -> KeyWrapContext<'a> {
        KeyWrapContext {
            archive_id: &[3; 32],
            envelope_id: &[4; 16],
            key_id,
            suite: SUITE_XCHACHA20_POLY1305,
            kdf_bytes: kdf,
        }
    }

    #[test]
    fn a_wrapped_key_unwraps_only_with_the_same_kek_and_context() {
        let kek = Kek::from_bytes([8; 32]);
        let dek = DataKey::from_bytes([5; 32]);
        let key_id = KeyId::from_bytes([9; 16]);
        let kdf = b"kdf-bytes".to_vec();
        let (nonce, wrapped) =
            wrap_dek(&kek, &dek, &wrap_ctx(&kdf, &key_id), &mut OsRandom).unwrap();

        let back = unwrap_dek(&kek, &nonce, &wrapped, &wrap_ctx(&kdf, &key_id)).unwrap();
        assert!(back.ct_eq(&dek));

        // Wrong KEK, or any altered binding, is an authentication failure.
        let bad = Kek::from_bytes([7; 32]);
        assert_eq!(
            unwrap_dek(&bad, &nonce, &wrapped, &wrap_ctx(&kdf, &key_id)).unwrap_err(),
            FormatError::Seal(SealFault::Authentication)
        );
        let altered_kdf = b"kdf-bytes-weaker".to_vec();
        assert!(unwrap_dek(&kek, &nonce, &wrapped, &wrap_ctx(&altered_kdf, &key_id)).is_err());
        let other_key_id = KeyId::from_bytes([10; 16]);
        assert!(unwrap_dek(&kek, &nonce, &wrapped, &wrap_ctx(&kdf, &other_key_id)).is_err());
        let mut c = wrap_ctx(&kdf, &key_id);
        c.envelope_id = &[5; 16];
        assert!(unwrap_dek(&kek, &nonce, &wrapped, &c).is_err());
        let mut c = wrap_ctx(&kdf, &key_id);
        c.archive_id = &[4; 32];
        assert!(unwrap_dek(&kek, &nonce, &wrapped, &c).is_err());
        let mut c = wrap_ctx(&kdf, &key_id);
        c.suite = 2;
        assert!(unwrap_dek(&kek, &nonce, &wrapped, &c).is_err());
    }

    #[test]
    fn wrap_aad_layout_is_fixed_width_with_the_kdf_bytes_last() {
        let key_id = KeyId::from_bytes([9; 16]);
        let aad = wrap_aad(&wrap_ctx(b"KDF", &key_id));
        assert_eq!(&aad[..16], KEY_WRAP_DOMAIN);
        assert_eq!(&aad[16..48], &[3; 32]);
        assert_eq!(&aad[48..64], &[4; 16]);
        assert_eq!(&aad[64..80], &[9; 16]);
        assert_eq!(&aad[80..82], &[1, 0]);
        assert_eq!(&aad[82..], b"KDF");
    }

    #[test]
    fn the_object_aad_layout_matches_r5() {
        let t = SealTarget::Image {
            sequence: 0x0102,
            transaction_id: [0xEE; 16],
        };
        let prefix = header_prefix(SealKind::Image, &KeyId::from_bytes([9; 16]));
        let aad = object_aad(&[3; 32], &prefix, &t);
        assert_eq!(&aad[..19], OBJECT_SEAL_DOMAIN);
        assert_eq!(&aad[19..51], &[3; 32]);
        assert_eq!(&aad[51..75], &prefix);
        assert_eq!(&aad[75..83], &[2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&aad[83..], &[0xEE; 16]);
        let c = SealTarget::Chunk {
            object_id: [0xAA; 32],
        };
        let aad = object_aad(&[3; 32], &prefix, &c);
        assert_eq!(&aad[75..], &[0xAA; 32]);
    }
}
