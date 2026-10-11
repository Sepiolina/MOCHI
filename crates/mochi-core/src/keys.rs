//! Key envelopes and key sessions (spec Annex B.2.10, D20 items 3 and 4;
//! `docs/schemas/key-envelope-v0.cddl`; `docs/ratification/R5-crypto-draft.md`
//! §3).
//!
//! An archive of the Encrypted profile has one random data key (DEK). Each
//! passphrase derives a key-encryption key (Argon2id) that wraps the DEK in a
//! **key envelope**, a plaintext deterministic-CBOR frame (`KEY_ENVELOPE`).
//! Every commit record lists the envelopes valid at that commit (commit schema
//! 2, key 11), so a reader finds them from the footer-verified head **without
//! decrypting anything** (spec §14.3); the descriptor never locates them (D12).
//!
//! # What a failed unwrap means
//!
//! An envelope is read only after its stored-object hash verified against the
//! commit that references it, so its bytes are exactly the bytes the writer
//! published. An AEAD failure on such an envelope therefore says one thing:
//! the passphrase is not one of the archive's. That is
//! [`ErrorCode::KeyUnavailable`] (exit 3), never a finding about the archive.
//!
//! # Sessions
//!
//! Deriving a key costs about a quarter of a second at the writer defaults, so
//! a [`KeySession`] remembers which envelope opened under which passphrase and
//! hands the same DEK to every later object of the archive. The cache is keyed
//! by **envelope ID**, not by archive: reading a commit whose key set does not
//! contain an envelope the session already opened derives again. A passphrase
//! removed by `rekey --remove-passphrase` therefore does not open the head, and
//! it still opens an older commit that listed its envelope (removal is not
//! revocation; B.2.10 item 10).

use std::sync::{Arc, Mutex};

use mochi_format::cbor::{self, CborLimits, Fields, Value};
use mochi_format::envelope::check_required_features;
use mochi_format::error::{FormatError, SealFault};
use mochi_format::frame::{encode_skippable_frame_within, walk_frame, FrameDetail};
use mochi_format::kdf::{derive_kek, KdfParams, ARGON2_VERSION_0X13, KDF_ARGON2ID, SALT_LEN};
use mochi_format::registry::{FrameKind, SKIPPABLE_HEADER_LEN};
use mochi_format::repr::StoredObject;
use mochi_format::seal::{
    unwrap_dek, wrap_dek, KeyId, KeyWrapContext, FEATURE_ENCRYPTED, NONCE_LEN,
    SUITE_XCHACHA20_POLY1305, WRAPPED_DEK_LEN,
};
use mochi_format::secret::{DataKey, Passphrase, Random};
use mochi_format::Limits;

use crate::commit::{CommitRecord, ObjectRef};
use crate::error::{ErrorCode, MochiError, Result};
use crate::object::ArchiveId;
use crate::publish::{load_verified, ReadOptions};
use crate::storage::ReadStorage;

/// Key-envelope schema version this build writes and reads.
pub const ENVELOPE_SCHEMA_VERSION: u64 = 0;

fn schema(msg: impl Into<String>) -> MochiError {
    MochiError::from(FormatError::Schema(msg.into()))
}

fn invalid_envelope(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

/// The keys a session has opened, by the envelope that opened each.
type OpenedKeys = Vec<([u8; 16], Arc<Unlocked>)>;

/// One decoded key envelope. Nothing secret: the wrapped key is ciphertext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEnvelope {
    pub archive_id: ArchiveId,
    /// D11 identity: the commit that wrote this envelope. Later commits
    /// reference it without rewriting it.
    pub sequence: u64,
    pub transaction_id: [u8; 16],
    pub envelope_id: [u8; 16],
    pub key_id: KeyId,
    pub kdf: KdfParams,
    pub salt: [u8; SALT_LEN],
    pub wrap_nonce: [u8; NONCE_LEN],
    pub wrapped: [u8; WRAPPED_DEK_LEN],
}

impl KeyEnvelope {
    /// The envelope's KDF-parameter map, key 8, exactly as encoded.
    fn kdf_value(&self) -> Value {
        Value::Map(vec![
            (0, Value::Uint(KDF_ARGON2ID)),
            (1, Value::Uint(ARGON2_VERSION_0X13)),
            (2, Value::Uint(self.kdf.memory_kib)),
            (3, Value::Uint(self.kdf.iterations)),
            (4, Value::Uint(self.kdf.lanes)),
            (5, Value::Bytes(self.salt.to_vec())),
        ])
    }

    /// The canonical bytes of key 8: the tail of the wrap's associated data.
    pub fn kdf_bytes(&self) -> Result<Vec<u8>> {
        Ok(cbor::encode(&self.kdf_value())?)
    }

    fn to_value(&self) -> Value {
        Value::Map(vec![
            (0, Value::Uint(ENVELOPE_SCHEMA_VERSION)),
            (1, Value::Bytes(self.archive_id.as_bytes().to_vec())),
            (2, Value::Uint(self.sequence)),
            (3, Value::Bytes(self.transaction_id.to_vec())),
            (4, Value::Array(vec![Value::Uint(FEATURE_ENCRYPTED)])),
            (5, Value::Bytes(self.envelope_id.to_vec())),
            (6, Value::Bytes(self.key_id.as_bytes().to_vec())),
            (7, Value::Uint(u64::from(SUITE_XCHACHA20_POLY1305))),
            (8, self.kdf_value()),
            (9, Value::Bytes(self.wrap_nonce.to_vec())),
            (10, Value::Bytes(self.wrapped.to_vec())),
        ])
    }

    /// The frame payload: canonical CBOR, bounded by the reader defaults.
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(cbor::encode_within(
            &self.to_value(),
            &CborLimits::default(),
        )?)
    }

    /// The stored form: one `KEY_ENVELOPE` frame within the writer defaults.
    pub fn to_stored(&self) -> Result<StoredObject> {
        let frame = encode_skippable_frame_within(
            FrameKind::KeyEnvelope,
            &self.encode()?,
            &Limits::WRITER_DEFAULT,
        )?;
        Ok(StoredObject::from_loaded(frame))
    }

    /// Decode a payload: canonical CBOR, closed schema. The KDF cost is **not**
    /// judged here (only at use, against the reader's limits), so a reader can
    /// list a hostile archive's envelopes without deriving anything.
    pub fn decode(payload: &[u8], limits: &Limits, cbor_limits: &CborLimits) -> Result<Self> {
        let root = cbor::decode(payload, cbor_limits)?;
        let mut f = Fields::of(&root, "key envelope")?;
        let version = f.req(0)?.uint("key envelope schema version")?;
        if version != ENVELOPE_SCHEMA_VERSION {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                format!("key-envelope schema version {version} is not supported by this build"),
            ));
        }
        let archive_id = ArchiveId::from_bytes(f.req(1)?.bytes32("archive id")?);
        let sequence = f.req(2)?.uint("introducing sequence")?;
        let transaction_id = fixed::<16>(f.req(3)?.bytes("transaction id")?, "transaction id")?;
        let features = f
            .req(4)?
            .array("required features")?
            .iter()
            .map(|v| v.uint("required feature"))
            .collect::<std::result::Result<Vec<u64>, _>>()?;
        check_required_features(&features, &[FEATURE_ENCRYPTED], limits)?;
        if features != [FEATURE_ENCRYPTED] {
            return Err(invalid_envelope(
                "a key envelope lists exactly the Encrypted required feature",
            ));
        }
        let envelope_id = fixed::<16>(f.req(5)?.bytes("envelope id")?, "envelope id")?;
        let key_id = KeyId::from_bytes(fixed::<16>(f.req(6)?.bytes("key id")?, "key id")?);
        let suite = f.req(7)?.uint("suite")?;
        if suite != u64::from(SUITE_XCHACHA20_POLY1305) {
            return Err(MochiError::from(FormatError::Seal(
                SealFault::UnknownSuite {
                    suite: u16::try_from(suite).unwrap_or(u16::MAX),
                },
            )));
        }
        let (kdf, salt) = {
            let mut k = Fields::of(f.req(8)?, "kdf parameters")?;
            if k.req(0)?.uint("kdf")? != KDF_ARGON2ID
                || k.req(1)?.uint("argon2 version")? != ARGON2_VERSION_0X13
            {
                return Err(MochiError::from(FormatError::Seal(SealFault::UnknownKdf)));
            }
            let params = KdfParams {
                memory_kib: k.req(2)?.uint("kdf memory")?,
                iterations: k.req(3)?.uint("kdf iterations")?,
                lanes: k.req(4)?.uint("kdf lanes")?,
            };
            let salt = fixed::<SALT_LEN>(k.req(5)?.bytes("kdf salt")?, "kdf salt")?;
            k.finish()?;
            (params, salt)
        };
        let wrap_nonce = fixed::<NONCE_LEN>(f.req(9)?.bytes("wrap nonce")?, "wrap nonce")?;
        let wrapped = fixed::<WRAPPED_DEK_LEN>(f.req(10)?.bytes("wrapped key")?, "wrapped key")?;
        f.finish()?;
        Ok(KeyEnvelope {
            archive_id,
            sequence,
            transaction_id,
            envelope_id,
            key_id,
            kdf,
            salt,
            wrap_nonce,
            wrapped,
        })
    }

    /// Parse a stored envelope: exactly one `KEY_ENVELOPE` frame whose payload
    /// decodes. The caller has verified the stored-object hash.
    pub fn from_stored(
        stored: &StoredObject,
        limits: &Limits,
        cbor_limits: &CborLimits,
    ) -> Result<Self> {
        let bytes = stored.as_bytes();
        let span = walk_frame(bytes, 0, limits)?;
        match span.detail {
            FrameDetail::Skippable { .. }
                if span.kind == FrameKind::KeyEnvelope && span.len == bytes.len() as u64 =>
            {
                let payload = bytes.get(SKIPPABLE_HEADER_LEN..).ok_or_else(|| {
                    MochiError::new(
                        ErrorCode::MalformedFrame,
                        "key envelope payload out of range",
                    )
                })?;
                Self::decode(payload, limits, cbor_limits)
            }
            _ => Err(MochiError::new(
                ErrorCode::MalformedFrame,
                "expected exactly one key-envelope frame",
            )),
        }
    }

    /// Wrap `dek` for `passphrase` into a new envelope introduced by the commit
    /// with this sequence and transaction ID. Every random value (envelope ID,
    /// salt, wrap nonce) comes from `rng`.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        archive_id: ArchiveId,
        sequence: u64,
        transaction_id: [u8; 16],
        key_id: KeyId,
        dek: &DataKey,
        passphrase: &Passphrase,
        kdf: KdfParams,
        rng: &mut dyn Random,
        limits: &Limits,
    ) -> Result<Self> {
        let mut envelope_id = [0u8; 16];
        rng.fill(&mut envelope_id)?;
        let mut salt = [0u8; SALT_LEN];
        rng.fill(&mut salt)?;
        let mut e = KeyEnvelope {
            archive_id,
            sequence,
            transaction_id,
            envelope_id,
            key_id,
            kdf,
            salt,
            wrap_nonce: [0; NONCE_LEN],
            wrapped: [0; WRAPPED_DEK_LEN],
        };
        let kek = derive_kek(passphrase, &e.salt, &e.kdf, limits)?;
        let kdf_bytes = e.kdf_bytes()?;
        let ctx = KeyWrapContext {
            archive_id: archive_id.as_bytes(),
            envelope_id: &e.envelope_id,
            key_id: &e.key_id,
            suite: SUITE_XCHACHA20_POLY1305,
            kdf_bytes: &kdf_bytes,
        };
        let (nonce, wrapped) = wrap_dek(&kek, dek, &ctx, rng)?;
        e.wrap_nonce = nonce;
        e.wrapped = wrapped;
        Ok(e)
    }

    /// Try to open this envelope with `passphrase`. The KDF cost is checked
    /// against `limits` **before** any memory is allocated. `Ok(None)` is a
    /// passphrase that does not open it (the AEAD failed); other errors are
    /// real (a limit, a malformed parameter).
    pub fn unwrap(&self, passphrase: &Passphrase, limits: &Limits) -> Result<Option<DataKey>> {
        let kek = derive_kek(passphrase, &self.salt, &self.kdf, limits)?;
        let kdf_bytes = self.kdf_bytes()?;
        let ctx = KeyWrapContext {
            archive_id: self.archive_id.as_bytes(),
            envelope_id: &self.envelope_id,
            key_id: &self.key_id,
            suite: SUITE_XCHACHA20_POLY1305,
            kdf_bytes: &kdf_bytes,
        };
        match unwrap_dek(&kek, &self.wrap_nonce, &self.wrapped, &ctx) {
            Ok(dek) => Ok(Some(dek)),
            Err(FormatError::Seal(SealFault::Authentication)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn fixed<const N: usize>(bytes: &[u8], what: &str) -> Result<[u8; N]> {
    <[u8; N]>::try_from(bytes).map_err(|_| schema(format!("{what}: expected exactly {N} bytes")))
}

// ---- sessions ---------------------------------------------------------------------

/// An archive's data key, unlocked by one of its envelopes. Not `Clone`; share
/// it through `Arc`.
pub struct Unlocked {
    pub archive_id: ArchiveId,
    pub key_id: KeyId,
    /// The envelope that opened it.
    pub envelope_id: [u8; 16],
    dek: DataKey,
}

impl Unlocked {
    pub fn new(archive_id: ArchiveId, key_id: KeyId, envelope_id: [u8; 16], dek: DataKey) -> Self {
        Unlocked {
            archive_id,
            key_id,
            envelope_id,
            dek,
        }
    }

    /// The context sealed objects are written and opened under.
    pub fn context(&self) -> mochi_format::seal::SealContext<'_> {
        mochi_format::seal::SealContext {
            key: &self.dek,
            key_id: self.key_id,
            archive_id: *self.archive_id.as_bytes(),
        }
    }

    /// The data key, for wrapping it in a new envelope (`rekey`).
    pub(crate) fn dek(&self) -> &DataKey {
        &self.dek
    }
}

impl std::fmt::Debug for Unlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unlocked")
            .field("archive_id", &self.archive_id)
            .field("key_id", &self.key_id)
            .field("dek", &"redacted")
            .finish()
    }
}

/// The passphrases a caller supplies, and the data keys they have opened so
/// far. Cheap to share (`Arc`); the passphrases never leave it.
pub struct KeySession {
    passphrases: Vec<Passphrase>,
    opened: Mutex<OpenedKeys>,
}

impl KeySession {
    /// A session over at least one passphrase.
    pub fn new(passphrases: Vec<Passphrase>) -> Result<Arc<Self>> {
        if passphrases.is_empty() {
            return Err(MochiError::new(
                ErrorCode::InvalidArgument,
                "a key session needs at least one passphrase",
            ));
        }
        Ok(Arc::new(KeySession {
            passphrases,
            opened: Mutex::new(Vec::new()),
        }))
    }

    /// A session over one passphrase.
    pub fn single(passphrase: Passphrase) -> Arc<Self> {
        Arc::new(KeySession {
            passphrases: vec![passphrase],
            opened: Mutex::new(Vec::new()),
        })
    }

    /// The supplied passphrases, for the envelopes of a rewritten archive
    /// (`compact`, `gc apply`, `repair apply`, `rekey --reencrypt`): a
    /// passphrase not given again does not carry over (B.2.10 item 10).
    pub(crate) fn passphrases(&self) -> &[Passphrase] {
        &self.passphrases
    }

    /// The data key of `archive`, if some envelope of it was opened (or its
    /// writer registered it). The head open decides **which commit's** key
    /// set a passphrase may open ([`KeySession::unlock`]); every later read of
    /// that archive reuses the key, because the data key is archive-wide.
    pub fn unlocked_for(&self, archive: &ArchiveId) -> Option<Arc<Unlocked>> {
        let guard = self.opened.lock().ok()?;
        guard
            .iter()
            .find(|(_, u)| &u.archive_id == archive)
            .map(|(_, u)| u.clone())
    }

    /// Remember the key of an archive this process just wrote, under each of
    /// the envelopes now valid, so that reading it back (verification after a
    /// rewrite, a later append) needs no second derivation.
    pub fn register_envelopes(&self, unlocked: Arc<Unlocked>, envelope_ids: &[[u8; 16]]) {
        if let Ok(mut guard) = self.opened.lock() {
            for id in envelope_ids {
                if !guard.iter().any(|(known, _)| known == id) {
                    guard.push((*id, unlocked.clone()));
                }
            }
        }
    }

    /// Open the data key through whichever of `envelopes` one of the session's
    /// passphrases opens. An envelope this session already opened is answered
    /// from the cache. `KEY_UNAVAILABLE` when none opens.
    pub fn unlock(&self, envelopes: &[KeyEnvelope], limits: &Limits) -> Result<Arc<Unlocked>> {
        let cached = {
            let guard = self.lock()?;
            envelopes.iter().find_map(|e| {
                guard
                    .iter()
                    .find(|(id, _)| *id == e.envelope_id)
                    .map(|(_, u)| u.clone())
            })
        };
        if let Some(u) = cached {
            return Ok(u);
        }
        for e in envelopes {
            for p in &self.passphrases {
                if let Some(dek) = e.unwrap(p, limits)? {
                    let unlocked =
                        Arc::new(Unlocked::new(e.archive_id, e.key_id, e.envelope_id, dek));
                    self.lock()?.push((e.envelope_id, unlocked.clone()));
                    return Ok(unlocked);
                }
            }
        }
        Err(MochiError::new(
            ErrorCode::KeyUnavailable,
            format!(
                "none of the {} supplied passphrase(s) opens any of the archive's {} key \
                 envelope(s)",
                self.passphrases.len(),
                envelopes.len()
            ),
        ))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, OpenedKeys>> {
        self.opened.lock().map_err(|_| {
            MochiError::new(
                ErrorCode::InvalidArgument,
                "internal: the key session is poisoned",
            )
        })
    }
}

impl std::fmt::Debug for KeySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySession")
            .field("passphrases", &self.passphrases.len())
            .finish()
    }
}

// ---- sealed records ------------------------------------------------------------------

/// The data key of the archive `cat` belongs to, if `opts` carries one that
/// opened it: how a reader that holds a catalog but no commit finds the key its
/// sealed chunks need. `None` for a Core archive and for a keyless reader; a
/// sealed chunk then fails with `KEY_UNAVAILABLE` when it is decoded.
pub(crate) fn catalog_key(
    opts: &ReadOptions,
    cat: &crate::catalog::Catalog,
) -> Option<Arc<Unlocked>> {
    let session = opts.keys.as_ref()?;
    let id = cat.meta(crate::catalog::META_ARCHIVE_ID).ok().flatten()?;
    let archive = ArchiveId::from_bytes(<[u8; 32]>::try_from(id.as_slice()).ok()?);
    session.unlocked_for(&archive)
}

/// Open a sealed record (a manifest or an image) that was read from `stored`
/// and whose stored-object hash has been verified: exactly one
/// `ENCRYPTED_OBJECT` frame, authenticated for `target`. A tag failure is
/// `CONTENT_INTEGRITY_FAILED` (the bytes are as written; they are not what
/// should be sealed there).
pub(crate) fn open_sealed_record(
    stored: &StoredObject,
    key: &Unlocked,
    target: &mochi_format::seal::SealTarget,
    limits: &Limits,
) -> Result<Vec<u8>> {
    let payload = mochi_format::seal::sealed_frame_payload(stored, limits)?;
    Ok(mochi_format::seal::open_payload(
        &key.context(),
        target,
        payload,
    )?)
}

/// Seal a record's plaintext for `target` as one sealed frame, within the
/// writer defaults (B.2.3): a record that does not fit is `CAPACITY_EXCEEDED`.
pub(crate) fn seal_record(
    key: &Unlocked,
    target: &mochi_format::seal::SealTarget,
    plaintext: &[u8],
    rng: &mut dyn Random,
) -> Result<StoredObject> {
    Ok(mochi_format::seal::seal_frame(
        &key.context(),
        target,
        plaintext,
        rng,
        &Limits::WRITER_DEFAULT,
    )?)
}

// ---- reading a commit's envelopes ---------------------------------------------------

/// Load, hash-verify (before parsing), and decode the envelopes `commit` lists
/// (commit key 11), and check them against the commit: the archive ID, a
/// sequence not after the commit's, one key ID throughout, and strictly
/// increasing, distinct envelope IDs. Needs no key (spec §14.3).
pub fn read_envelopes(
    src: &dyn ReadStorage,
    commit: &CommitRecord,
    limit: u64,
    opts: &ReadOptions,
) -> Result<Vec<(ObjectRef, KeyEnvelope)>> {
    let n = commit.key_envelopes.len() as u64;
    if n > opts.limits.max_key_envelopes {
        return Err(MochiError::from(FormatError::LimitExceeded {
            kind: mochi_format::error::LimitKind::KeyEnvelopes,
            limit: opts.limits.max_key_envelopes,
            actual: n,
        }));
    }
    let mut out: Vec<(ObjectRef, KeyEnvelope)> = Vec::new();
    for r in &commit.key_envelopes {
        let stored = load_verified(src, r, limit, opts.limits.max_frame_len, "key envelope")?;
        let e = KeyEnvelope::from_stored(&stored, &opts.limits, &opts.cbor)?;
        if e.archive_id != commit.archive_id {
            return Err(invalid_envelope(
                "a key envelope belongs to another archive than the commit that lists it",
            ));
        }
        if e.sequence > commit.seq {
            return Err(invalid_envelope(
                "a key envelope claims to be written by a commit after the one listing it",
            ));
        }
        if let Some((_, prev)) = out.last() {
            if prev.key_id != e.key_id {
                return Err(invalid_envelope(
                    "the key envelopes of one commit name different data keys",
                ));
            }
            if prev.envelope_id >= e.envelope_id {
                return Err(invalid_envelope(
                    "a commit lists its key envelopes in strictly increasing order of envelope ID",
                ));
            }
        }
        out.push((*r, e));
    }
    Ok(out)
}

/// Open the data key for `commit`. `KEY_UNAVAILABLE` when the caller supplied
/// no passphrase or none opens the key.
///
/// `strict` is for the **head**: the passphrase must open one of the head's
/// own key envelopes, so that a passphrase removed by `rekey
/// --remove-passphrase` does not open the head (B.2.10 item 10). A historical
/// commit (`strict` false) is opened with the archive's data key if the
/// session already holds it: the key is archive-wide, and whoever can open the
/// head can read the history it contains. Otherwise the commit's own envelope
/// set decides, as for the head.
pub fn unlock_commit(
    src: &dyn ReadStorage,
    commit: &CommitRecord,
    limit: u64,
    opts: &ReadOptions,
    strict: bool,
) -> Result<Arc<Unlocked>> {
    let Some(session) = &opts.keys else {
        return Err(MochiError::new(
            ErrorCode::KeyUnavailable,
            "this archive is encrypted: a passphrase is required",
        ));
    };
    if !strict {
        if let Some(u) = session.unlocked_for(&commit.archive_id) {
            return Ok(u);
        }
    }
    let envelopes = read_envelopes(src, commit, limit, opts)?;
    let list: Vec<KeyEnvelope> = envelopes.into_iter().map(|(_, e)| e).collect();
    session.unlock(&list, &opts.limits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mochi_format::secret::OsRandom;

    fn params() -> KdfParams {
        KdfParams {
            memory_kib: 64,
            iterations: 2,
            lanes: 1,
        }
    }

    fn make(pass: &str) -> (KeyEnvelope, DataKey) {
        let dek = DataKey::from_bytes([5; 32]);
        let e = KeyEnvelope::create(
            ArchiveId::from_bytes([1; 32]),
            0,
            [2; 16],
            KeyId::from_bytes([3; 16]),
            &dek,
            &Passphrase::new(pass).unwrap(),
            params(),
            &mut OsRandom,
            &Limits::default(),
        )
        .unwrap();
        (e, dek)
    }

    #[test]
    fn an_envelope_round_trips_and_opens_with_its_passphrase_only() {
        let (e, dek) = make("correct horse");
        let stored = e.to_stored().unwrap();
        assert_eq!(
            &stored.as_bytes()[..4],
            &mochi_format::registry::KEY_ENVELOPE.to_le_bytes()
        );
        let back =
            KeyEnvelope::from_stored(&stored, &Limits::default(), &CborLimits::default()).unwrap();
        assert_eq!(back, e);
        let opened = back
            .unwrap(
                &Passphrase::new("correct horse").unwrap(),
                &Limits::default(),
            )
            .unwrap()
            .unwrap();
        assert!(opened.ct_eq(&dek));
        assert!(back
            .unwrap(
                &Passphrase::new("correct horsf").unwrap(),
                &Limits::default()
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn nfc_normalization_makes_equivalent_passphrases_equal() {
        let (e, dek) = make("\u{00C5}ngstr\u{00F6}m");
        let decomposed = Passphrase::new("A\u{030A}ngstro\u{0308}m").unwrap();
        let opened = e.unwrap(&decomposed, &Limits::default()).unwrap().unwrap();
        assert!(opened.ct_eq(&dek));
    }

    #[test]
    fn every_altered_field_defeats_the_unwrap_or_the_decode() {
        let (e, _) = make("pw");
        let good = Passphrase::new("pw").unwrap();
        let opens = |x: &KeyEnvelope| matches!(x.unwrap(&good, &Limits::default()), Ok(Some(_)));
        assert!(opens(&e));
        // Bound into the wrap's associated data: any change breaks the unwrap.
        let mut x = e.clone();
        x.envelope_id[0] ^= 1;
        assert!(!opens(&x));
        let mut x = e.clone();
        x.key_id = KeyId::from_bytes([4; 16]);
        assert!(!opens(&x));
        let mut x = e.clone();
        x.archive_id = ArchiveId::from_bytes([9; 32]);
        assert!(!opens(&x));
        // A weakened KDF parameter changes the KEK and the associated data.
        let mut x = e.clone();
        x.kdf.iterations = 1;
        assert!(!opens(&x));
        let mut x = e.clone();
        x.salt[0] ^= 1;
        assert!(!opens(&x));
        // Identity fields the wrap does not bind are informational, so a
        // changed sequence still opens (the reader checks it against the commit).
        let mut x = e.clone();
        x.sequence = 99;
        assert!(opens(&x));
        let mut x = e.clone();
        x.wrapped[0] ^= 1;
        assert!(!opens(&x));
        let mut x = e.clone();
        x.wrap_nonce[0] ^= 1;
        assert!(!opens(&x));
    }

    #[test]
    fn hostile_cost_parameters_are_refused_before_any_allocation() {
        let (mut e, _) = make("pw");
        let p = Passphrase::new("pw").unwrap();
        e.kdf.memory_kib = 1 << 40;
        let err = e.unwrap(&p, &Limits::default()).unwrap_err();
        assert_eq!(err.code, ErrorCode::LimitExceeded);
        assert!(err.message.contains("KdfMemory"), "{}", err.message);
        let (mut e, _) = make("pw");
        e.kdf.iterations = 1_000;
        assert_eq!(
            e.unwrap(&p, &Limits::default()).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        let (mut e, _) = make("pw");
        e.kdf.lanes = 100;
        assert_eq!(
            e.unwrap(&p, &Limits::default()).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
        // Below Argon2's own minimum is a malformed record, not a limit.
        let (mut e, _) = make("pw");
        e.kdf.memory_kib = 4;
        assert_eq!(
            e.unwrap(&p, &Limits::default()).unwrap_err().code,
            ErrorCode::RecordInvalid
        );
        // A reader may raise the limit explicitly, and then it is honoured.
        let (mut e, dek) = make("pw");
        e.kdf.memory_kib = 64;
        let raised = Limits {
            max_kdf_memory_kib: 64,
            ..Limits::default()
        };
        assert!(e.unwrap(&p, &raised).unwrap().unwrap().ct_eq(&dek));
        let tight = Limits {
            max_kdf_memory_kib: 63,
            ..Limits::default()
        };
        assert_eq!(
            e.unwrap(&p, &tight).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }

    fn decode_with(mutate: impl FnOnce(&mut Vec<(u64, Value)>)) -> Result<KeyEnvelope> {
        let (e, _) = make("pw");
        let Value::Map(mut m) = e.to_value() else {
            unreachable!()
        };
        mutate(&mut m);
        let bytes = cbor::encode(&Value::Map(m)).unwrap();
        KeyEnvelope::decode(&bytes, &Limits::default(), &CborLimits::default())
    }

    #[test]
    fn the_schema_is_closed_and_every_rule_has_a_code() {
        assert!(decode_with(|_| {}).is_ok());
        // Schema version.
        assert_eq!(
            decode_with(|m| m[0].1 = Value::Uint(1)).unwrap_err().code,
            ErrorCode::UnsupportedFeature
        );
        // Features: exactly [1].
        assert_eq!(
            decode_with(|m| m[4].1 = Value::Array(vec![]))
                .unwrap_err()
                .code,
            ErrorCode::RecordInvalid
        );
        assert_eq!(
            decode_with(|m| m[4].1 = Value::Array(vec![Value::Uint(2)]))
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedFeature
        );
        // Suite, KDF, Argon2 version.
        assert_eq!(
            decode_with(|m| m[7].1 = Value::Uint(2)).unwrap_err().code,
            ErrorCode::UnsupportedFeature
        );
        let kdf_edit = |at: usize, v: Value| {
            decode_with(move |m| {
                let Value::Map(k) = &mut m[8].1 else {
                    unreachable!()
                };
                k[at].1 = v;
            })
        };
        assert_eq!(
            kdf_edit(0, Value::Uint(2)).unwrap_err().code,
            ErrorCode::UnsupportedFeature
        );
        assert_eq!(
            kdf_edit(1, Value::Uint(0x10)).unwrap_err().code,
            ErrorCode::UnsupportedFeature
        );
        // Lengths of the fixed fields.
        assert_eq!(
            kdf_edit(5, Value::Bytes(vec![0; 15])).unwrap_err().code,
            ErrorCode::RecordInvalid
        );
        assert_eq!(
            decode_with(|m| m[9].1 = Value::Bytes(vec![0; 23]))
                .unwrap_err()
                .code,
            ErrorCode::RecordInvalid
        );
        assert_eq!(
            decode_with(|m| m[10].1 = Value::Bytes(vec![0; 47]))
                .unwrap_err()
                .code,
            ErrorCode::RecordInvalid
        );
        assert_eq!(
            decode_with(|m| m[5].1 = Value::Bytes(vec![0; 17]))
                .unwrap_err()
                .code,
            ErrorCode::RecordInvalid
        );
        // Unknown and missing keys.
        assert_eq!(
            decode_with(|m| m.push((11, Value::Uint(0))))
                .unwrap_err()
                .code,
            ErrorCode::RecordInvalid
        );
        assert_eq!(
            decode_with(|m| {
                m.pop();
            })
            .unwrap_err()
            .code,
            ErrorCode::RecordInvalid
        );
    }

    #[test]
    fn a_non_canonical_encoding_is_refused() {
        let (e, _) = make("pw");
        let mut bytes = e.encode().unwrap();
        // Re-encode key 2 (a one-byte uint 0) as a two-byte uint: still CBOR,
        // not the shortest form.
        let at = bytes
            .windows(2)
            .position(|w| w == [0x02, 0x00])
            .expect("key 2");
        bytes.splice(at + 1..at + 2, [0x18, 0x00]);
        assert!(KeyEnvelope::decode(&bytes, &Limits::default(), &CborLimits::default()).is_err());
    }

    #[test]
    fn a_session_caches_by_envelope_and_names_the_failure() {
        let (e, dek) = make("pw");
        let session = KeySession::single(Passphrase::new("pw").unwrap());
        let a = session
            .unlock(std::slice::from_ref(&e), &Limits::default())
            .unwrap();
        assert!(a.dek().ct_eq(&dek));
        // A second unlock is answered from the cache even when the KDF limits
        // would now refuse a derivation: nothing is derived again.
        let none = Limits {
            max_kdf_memory_kib: 8,
            ..Limits::default()
        };
        let b = session.unlock(std::slice::from_ref(&e), &none).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // A commit whose key set does not contain the opened envelope derives
        // afresh, and a wrong passphrase there is KEY_UNAVAILABLE.
        let (other, _) = make("different");
        let err = session
            .unlock(std::slice::from_ref(&other), &Limits::default())
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::KeyUnavailable);
        assert!(!err.message.contains("pw") || !err.message.contains("different"));
        assert!(KeySession::new(vec![]).is_err());
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        let session = KeySession::single(Passphrase::new("super secret phrase").unwrap());
        assert!(!format!("{session:?}").contains("super"));
        let (e, dek) = make("pw");
        let u = Unlocked::new(e.archive_id, e.key_id, e.envelope_id, dek);
        let dbg = format!("{u:?}");
        assert!(dbg.contains("redacted"));
        assert!(!dbg.contains("[5, 5"));
    }
}
