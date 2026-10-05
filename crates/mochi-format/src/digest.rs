//! Digest scopes (spec §9.2) and the footer digest (§8.4).
//!
//! **Domain separators are defined once, here** (AGENTS.md). Every separator
//! is `MOCHI2-<SCOPE>` followed by one zero byte, matching the footer's
//! spec-given construction (§8.4). None contains an interior zero byte, so the
//! separated scopes are prefix-free with respect to each other. Final strings
//! are ratification item R3; these are **draft**.
//!
//! Decisions (plan §9, O20):
//!
//! * **The file-content hash is plain BLAKE3-256 with no separator**, so it
//!   equals `b3sum` of the restored file. It is the one digest that describes
//!   content independently of MOCHI: users can check restorations with a
//!   stock tool, an independent reader (R9) needs nothing MOCHI-specific to
//!   compute it, and it survives migration to a later wire generation (§26)
//!   unchanged. Cost: a file whose bytes begin with a MOCHI separator has a
//!   file-content hash equal to that scope's hash of the remainder. That is
//!   harmless only because digests are never compared across scopes; the
//!   [`Digest<S>`] types enforce it in this crate, and the catalog (C3) and
//!   reports must keep scopes in distinct, labelled fields.
//! * **The §9.2 "metadata-object hash" and "recovery-manifest hash" are the
//!   stored-object hash of those objects**, not separate scopes. Their inputs
//!   are identical ("exact stored … bytes"), and stored bytes already begin
//!   with the frame magic that says which kind of object they are, so a
//!   separate separator would add no discrimination while giving every control
//!   object two different digests and every verifier a way to pick the wrong
//!   one. Their role is carried by the field that holds them (C4, C5).
//!
//! Every scope has its own output type ([`Digest<S>`]) and accepts exactly one
//! input representation from [`crate::repr`]:
//!
//! | Scope | Input type | Construction | Spec row (§9.2) |
//! |---|---|---|---|
//! | [`FileContent`] | [`DecodedSlice`] stream, plus holes | `BLAKE3(input)` | File content hash |
//! | [`ChunkContent`] | [`DecodedBytes`] | `BLAKE3(sep ‖ input)` | Chunk content hash |
//! | [`StoredObjectScope`] | [`StoredObjectBytes`] | `BLAKE3(sep ‖ input)` | Stored-object, metadata-object, and recovery-manifest hashes |
//! | [`Dictionary`] | [`DictionaryBytes`] | `BLAKE3(sep ‖ input)` | Dictionary hash |
//! | [`CommitIdScope`] | [`CanonicalCommitBody`] | `BLAKE3(sep ‖ input)` | Commit ID |
//! | [`Footer`] | payload bytes 0–31 + [`StoredObjectBytes`] | §8.4 | Footer digest |
//!
//! A `ChunkContentHash` cannot be compared with, stored as, or passed where a
//! `StoredObjectHash` is expected: they are different types.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

pub use crate::repr::StoredObjectBytes;
use crate::repr::{CanonicalCommitBody, DecodedBytes, DecodedSlice, DictionaryBytes};

/// Length of every MOCHI digest: BLAKE3-256 (spec §9.2 baseline).
pub const DIGEST_LEN: usize = 32;

/// `MOCHI2-FOOTER` followed by one zero byte (spec §8.4; spec-given, not draft).
pub const FOOTER_DOMAIN: &[u8] = b"MOCHI2-FOOTER\0";
/// The file-content hash has no separator (O20): plain BLAKE3, `b3sum`-equal.
pub const FILE_CONTENT_DOMAIN: &[u8] = b"";
/// The tail-quarantine sidecar's tail hash (Annex B.2.2, `tail-quarantine-v0`
/// key 6): plain BLAKE3 by the spec's own definition, so `b3sum` over the
/// removed bytes checks it. Not part of the archive format; a separate,
/// labelled scope, and never compared with any archive digest.
pub const TAIL_QUARANTINE_DOMAIN: &[u8] = b"";
/// Draft (R3).
pub const CHUNK_CONTENT_DOMAIN: &[u8] = b"MOCHI2-CHUNK-CONTENT\0";
/// Draft (R3).
pub const STORED_OBJECT_DOMAIN: &[u8] = b"MOCHI2-STORED-OBJECT\0";
/// Draft (R3).
pub const DICTIONARY_DOMAIN: &[u8] = b"MOCHI2-DICTIONARY\0";
/// Draft (R3). §9.2 requires this one to be domain-separated.
pub const COMMIT_ID_DOMAIN: &[u8] = b"MOCHI2-COMMIT-ID\0";

/// Every separator in use, for tests that check the set is prefix-free.
/// The file-content scope is unseparated and deliberately absent (O20).
pub const ALL_DOMAINS: &[&[u8]] = &[
    FOOTER_DOMAIN,
    CHUNK_CONTENT_DOMAIN,
    STORED_OBJECT_DOMAIN,
    DICTIONARY_DOMAIN,
    COMMIT_ID_DOMAIN,
];

mod sealed {
    pub trait Sealed {}
}

/// A §9.2 digest scope. Sealed: scopes are defined only in this module.
pub trait DigestScope: sealed::Sealed {
    const DOMAIN: &'static [u8];
    /// Short name for diagnostics and reports.
    const NAME: &'static str;
}

macro_rules! scopes {
    ($($(#[$doc:meta])* $ty:ident => $domain:ident, $name:literal;)*) => {$(
        $(#[$doc])*
        #[derive(Debug)]
        pub enum $ty {}
        impl sealed::Sealed for $ty {}
        impl DigestScope for $ty {
            const DOMAIN: &'static [u8] = $domain;
            const NAME: &'static str = $name;
        }
    )*};
}

scopes! {
    /// Complete logical file byte stream, excluding path and attributes.
    /// Unseparated (O20).
    FileContent => FILE_CONTENT_DOMAIN, "file-content";
    /// Exact decoded chunk bytes.
    ChunkContent => CHUNK_CONTENT_DOMAIN, "chunk-content";
    /// Exact stored object bytes, including framing. Also the §9.2
    /// metadata-object and recovery-manifest hashes (O20).
    StoredObjectScope => STORED_OBJECT_DOMAIN, "stored-object";
    /// Exact dictionary bytes.
    Dictionary => DICTIONARY_DOMAIN, "dictionary";
    /// Canonical commit body, excluding the ID field.
    CommitIdScope => COMMIT_ID_DOMAIN, "commit-id";
    /// The §8.4 footer construction.
    Footer => FOOTER_DOMAIN, "footer";
    /// Bytes removed by a tail truncation, as recorded in the quarantine
    /// sidecar (D14). Unseparated by the spec's definition.
    TailQuarantine => TAIL_QUARANTINE_DOMAIN, "tail-quarantine";
}

/// A BLAKE3-256 digest in scope `S`.
pub struct Digest<S: DigestScope> {
    bytes: [u8; DIGEST_LEN],
    _scope: PhantomData<fn() -> S>,
}

pub type FileContentHash = Digest<FileContent>;
pub type ChunkContentHash = Digest<ChunkContent>;
pub type StoredObjectHash = Digest<StoredObjectScope>;
pub type DictionaryHash = Digest<Dictionary>;
pub type CommitId = Digest<CommitIdScope>;

impl<S: DigestScope> Digest<S> {
    /// Rehydrate a digest read from a record. The caller asserts the scope;
    /// the value is only ever *compared* against a freshly computed digest.
    pub const fn from_bytes(bytes: [u8; DIGEST_LEN]) -> Self {
        Digest {
            bytes,
            _scope: PhantomData,
        }
    }

    pub const fn as_bytes(&self) -> &[u8; DIGEST_LEN] {
        &self.bytes
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(DIGEST_LEN * 2);
        for b in self.bytes {
            s.push(char::from(HEX[usize::from(b >> 4)]));
            s.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        s
    }

    /// Scope name, for reports.
    pub const fn scope_name() -> &'static str {
        S::NAME
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

// Manual impls: derives would demand `S: Clone`, etc., of an uninhabited type.
impl<S: DigestScope> Clone for Digest<S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S: DigestScope> Copy for Digest<S> {}
impl<S: DigestScope> PartialEq for Digest<S> {
    fn eq(&self, other: &Self) -> bool {
        // Digests are public values here, not MACs; constant time is not needed.
        self.bytes == other.bytes
    }
}
impl<S: DigestScope> Eq for Digest<S> {}
impl<S: DigestScope> Hash for Digest<S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}
impl<S: DigestScope> fmt::Debug for Digest<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", S::NAME, self.to_hex())
    }
}

/// Streaming hasher for scope `S`, pre-seeded with its separator. Private:
/// public entry points below fix the input type for each scope.
struct Scoped<S: DigestScope> {
    hasher: blake3::Hasher,
    _scope: PhantomData<fn() -> S>,
}

impl<S: DigestScope> Scoped<S> {
    fn new() -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(S::DOMAIN);
        Scoped {
            hasher,
            _scope: PhantomData,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    fn finalize(&self) -> Digest<S> {
        Digest::from_bytes(*self.hasher.finalize().as_bytes())
    }
}

// ---- chunk, dictionary, commit: single-shot --------------------------------

/// Chunk content hash: exact **decoded** chunk bytes (after decryption and
/// decompression). Never the compressed or stored form.
pub fn chunk_content_hash(decoded: &DecodedBytes) -> ChunkContentHash {
    let mut h = Scoped::<ChunkContent>::new();
    h.update(decoded.as_bytes());
    h.finalize()
}

pub fn dictionary_hash(dictionary: &DictionaryBytes) -> DictionaryHash {
    let mut h = Scoped::<Dictionary>::new();
    h.update(dictionary.bytes());
    h.finalize()
}

/// Commit ID over a canonical body. Canonicalization is R3/C5; see
/// [`CanonicalCommitBody`].
pub fn commit_id(body: CanonicalCommitBody<'_>) -> CommitId {
    let mut h = Scoped::<CommitIdScope>::new();
    h.update(body.bytes());
    h.finalize()
}

// ---- stored-bytes scopes: streaming ------------------------------------------

/// Streaming hasher over exact stored bytes, for objects too large to buffer.
pub struct StoredBytesHasher<S: DigestScope> {
    inner: Scoped<S>,
    len: u64,
}

impl<S: DigestScope> StoredBytesHasher<S> {
    pub fn update(&mut self, bytes: StoredObjectBytes<'_>) {
        self.inner.update(bytes.bytes());
        self.len = self.len.saturating_add(bytes.len() as u64);
    }

    /// Bytes fed so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finalize(&self) -> Digest<S> {
        self.inner.finalize()
    }
}

impl StoredBytesHasher<StoredObjectScope> {
    pub fn stored_object() -> Self {
        StoredBytesHasher {
            inner: Scoped::new(),
            len: 0,
        }
    }
}

pub fn stored_object_hash(stored: StoredObjectBytes<'_>) -> StoredObjectHash {
    let mut h = StoredBytesHasher::stored_object();
    h.update(stored);
    h.finalize()
}

// ---- file content: logical stream with holes -------------------------------

/// Zero block fed for holes. Holes cost O(hole length) to hash: spec §9.3
/// defines the file hash over the *logical* stream, zeros included.
static ZEROS: [u8; 64 * 1024] = [0u8; 64 * 1024];

/// File content hash over the complete logical stream (spec §9.2, §9.3):
/// extent bytes in order, with holes contributing their zero bytes.
pub struct FileContentHasher {
    inner: Scoped<FileContent>,
    len: u64,
}

impl Default for FileContentHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl FileContentHasher {
    pub fn new() -> Self {
        FileContentHasher {
            inner: Scoped::new(),
            len: 0,
        }
    }

    /// Feed the next extent's decoded bytes. Returns `None` if the logical
    /// length would overflow `u64` (archive-derived; never wraps).
    pub fn update(&mut self, bytes: DecodedSlice<'_>) -> Option<()> {
        self.len = self.len.checked_add(bytes.len())?;
        self.inner.update(bytes.bytes());
        Some(())
    }

    /// Feed a hole of `len` logical zero bytes (sparse files, spec §9.3).
    pub fn hole(&mut self, len: u64) -> Option<()> {
        self.len = self.len.checked_add(len)?;
        let mut left = len;
        while left > 0 {
            let n = usize::try_from(left.min(ZEROS.len() as u64)).unwrap_or(ZEROS.len());
            self.inner.update(&ZEROS[..n]);
            left -= n as u64;
        }
        Some(())
    }

    /// Logical length fed so far.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn finalize(&self) -> FileContentHash {
        self.inner.finalize()
    }
}

/// A truncated tail's hash for its quarantine sidecar (D14; plain BLAKE3).
pub type TailQuarantineHash = Digest<TailQuarantine>;

/// The hash of `tail`, as the quarantine sidecar records it (key 6).
pub fn tail_quarantine_hash(tail: &[u8]) -> TailQuarantineHash {
    let mut h = TailQuarantineHasher::new();
    h.update(tail);
    h.finalize()
}

/// Streaming [`tail_quarantine_hash`], for tails too large to buffer.
pub struct TailQuarantineHasher {
    inner: Scoped<TailQuarantine>,
}

impl Default for TailQuarantineHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl TailQuarantineHasher {
    pub fn new() -> Self {
        TailQuarantineHasher {
            inner: Scoped::new(),
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    pub fn finalize(&self) -> TailQuarantineHash {
        self.inner.finalize()
    }
}

/// File content hash of a file held entirely in memory with no holes.
pub fn file_content_hash(content: DecodedSlice<'_>) -> FileContentHash {
    let mut h = Scoped::<FileContent>::new();
    h.update(content.bytes());
    h.finalize()
}

// ---- footer (C1, spec §8.4) -------------------------------------------------

/// Incremental footer-digest builder so large stored commit frames can be
/// hashed without being held in memory.
pub struct FooterDigest {
    hasher: blake3::Hasher,
}

impl FooterDigest {
    /// Starts with the domain separator and footer payload bytes 0 through 31.
    pub fn new(payload_prefix: &[u8; 32]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(FOOTER_DOMAIN);
        hasher.update(payload_prefix);
        FooterDigest { hasher }
    }

    /// Feed stored commit-frame bytes, in order.
    pub fn update(&mut self, commit_frame: StoredObjectBytes<'_>) {
        self.hasher.update(commit_frame.bytes());
    }

    pub fn finalize(self) -> [u8; 32] {
        *self.hasher.finalize().as_bytes()
    }
}

/// Footer digest over a fully buffered stored commit frame.
pub fn footer_digest(payload_prefix: &[u8; 32], commit_frame: StoredObjectBytes<'_>) -> [u8; 32] {
    let mut d = FooterDigest::new(payload_prefix);
    d.update(commit_frame);
    d.finalize()
}

#[cfg(test)]
mod tests {
    //! One test per §9.2 scope proving it hashes exactly its declared input
    //! (plan C2 exit criterion), plus separator hygiene.

    use super::*;
    use crate::repr::StoredObject;

    /// BLAKE3 of `domain || input`: the declared construction, written out.
    fn expected(domain: &[u8], input: &[u8]) -> [u8; 32] {
        let mut v = domain.to_vec();
        v.extend_from_slice(input);
        *blake3::hash(&v).as_bytes()
    }

    const INPUT: &[u8] = b"the same bytes under every scope";

    // -- separators --

    #[test]
    fn separators_are_prefix_free_nul_terminated_and_distinct() {
        assert_eq!(
            FILE_CONTENT_DOMAIN, b"" as &[u8],
            "O20: file content is unseparated"
        );
        for (i, a) in ALL_DOMAINS.iter().enumerate() {
            assert!(a.starts_with(b"MOCHI2-"), "{a:?}");
            assert_eq!(a.last(), Some(&0), "{a:?} must end in NUL");
            assert_eq!(
                a.iter().filter(|b| **b == 0).count(),
                1,
                "{a:?} has an interior NUL"
            );
            for (j, b) in ALL_DOMAINS.iter().enumerate() {
                if i != j {
                    assert!(!b.starts_with(a), "{a:?} is a prefix of {b:?}");
                }
            }
        }
    }

    #[test]
    fn same_bytes_differ_across_every_scope() {
        let dec = DecodedBytes::new(INPUT.to_vec());
        let dict = DictionaryBytes::new(INPUT.to_vec());
        let stored = StoredObjectBytes::new(INPUT);
        let all = [
            *file_content_hash(dec.as_slice()).as_bytes(),
            *chunk_content_hash(&dec).as_bytes(),
            *stored_object_hash(stored).as_bytes(),
            *dictionary_hash(&dict).as_bytes(),
            *commit_id(CanonicalCommitBody::assume_canonical(INPUT)).as_bytes(),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "scopes {i} and {j} collide");
            }
        }
    }

    // -- one test per scope --

    #[test]
    fn file_content_hash_is_the_logical_stream_including_holes() {
        // Extents "abc", a 5-byte hole, "de": logical stream is abc\0\0\0\0\0de.
        let mut h = FileContentHasher::new();
        h.update(DecodedSlice::from_logical(b"abc")).unwrap();
        h.hole(5).unwrap();
        h.update(DecodedSlice::from_logical(b"de")).unwrap();
        assert_eq!(h.len(), 10);
        let logical = b"abc\0\0\0\0\0de";
        assert_eq!(h.finalize().as_bytes(), blake3::hash(logical).as_bytes());
        assert_eq!(
            h.finalize(),
            file_content_hash(DecodedSlice::from_logical(logical))
        );
        // Extent boundaries do not matter; only the stream does.
        let mut g = FileContentHasher::new();
        g.update(DecodedSlice::from_logical(b"ab")).unwrap();
        g.update(DecodedSlice::from_logical(b"c\0\0")).unwrap();
        g.hole(3).unwrap();
        g.update(DecodedSlice::from_logical(b"de")).unwrap();
        assert_eq!(g.finalize(), h.finalize());
    }

    #[test]
    fn large_hole_spans_multiple_zero_blocks() {
        let n = ZEROS.len() as u64 * 2 + 17;
        let mut h = FileContentHasher::new();
        h.hole(n).unwrap();
        let zeros = vec![0u8; n as usize];
        assert_eq!(h.finalize().as_bytes(), blake3::hash(&zeros).as_bytes());
    }

    #[test]
    fn file_length_overflow_is_refused_not_wrapped() {
        let mut h = FileContentHasher::new();
        h.update(DecodedSlice::from_logical(b"x")).unwrap();
        // Would overflow before hashing a single zero: refused up front.
        assert!(h.hole(u64::MAX).is_none());
    }

    #[test]
    fn chunk_content_hash_is_exact_decoded_bytes() {
        let dec = DecodedBytes::new(INPUT.to_vec());
        assert_eq!(
            chunk_content_hash(&dec).as_bytes(),
            &expected(CHUNK_CONTENT_DOMAIN, INPUT)
        );
    }

    #[test]
    fn stored_object_hash_is_exact_stored_bytes_and_streams() {
        let obj = StoredObject::from_loaded(INPUT.to_vec());
        let whole = stored_object_hash(obj.view());
        assert_eq!(whole.as_bytes(), &expected(STORED_OBJECT_DOMAIN, INPUT));
        let mut h = StoredBytesHasher::stored_object();
        for part in INPUT.chunks(7) {
            h.update(StoredObjectBytes::new(part));
        }
        assert_eq!(h.len(), INPUT.len() as u64);
        assert_eq!(h.finalize(), whole);
    }

    #[test]
    fn dictionary_hash_is_exact_dictionary_bytes() {
        let d = DictionaryBytes::new(INPUT.to_vec());
        assert_eq!(
            dictionary_hash(&d).as_bytes(),
            &expected(DICTIONARY_DOMAIN, INPUT)
        );
    }

    #[test]
    fn file_content_hash_is_b3sum_compatible() {
        assert_eq!(
            file_content_hash(DecodedSlice::from_logical(INPUT)).as_bytes(),
            blake3::hash(INPUT).as_bytes()
        );
        // Known answer: BLAKE3("abc"), as printed by `b3sum`.
        assert_eq!(
            file_content_hash(DecodedSlice::from_logical(b"abc")).to_hex(),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn the_unseparated_file_scope_meets_other_scopes_only_under_their_separator() {
        // Documents the O20 trade-off: a file that begins with a separator
        // shares a digest value with that scope. Types keep them apart.
        let mut crafted = CHUNK_CONTENT_DOMAIN.to_vec();
        crafted.extend_from_slice(INPUT);
        assert_eq!(
            file_content_hash(DecodedSlice::from_logical(&crafted)).as_bytes(),
            chunk_content_hash(&DecodedBytes::new(INPUT.to_vec())).as_bytes()
        );
    }

    #[test]
    fn commit_id_is_the_separated_canonical_body() {
        assert_eq!(
            commit_id(CanonicalCommitBody::assume_canonical(INPUT)).as_bytes(),
            &expected(COMMIT_ID_DOMAIN, INPUT)
        );
    }

    #[test]
    fn footer_digest_is_blake3_of_the_spec_concatenation() {
        let prefix = [7u8; 32];
        let commit = b"commit-frame-bytes";
        let mut want_input = prefix.to_vec();
        want_input.extend_from_slice(commit);
        assert_eq!(
            footer_digest(&prefix, StoredObjectBytes::new(commit)),
            expected(FOOTER_DOMAIN, &want_input)
        );
    }

    #[test]
    fn footer_streaming_equals_buffered() {
        let prefix = [1u8; 32];
        let commit: Vec<u8> = (0..=255u8).collect();
        let whole = footer_digest(&prefix, StoredObjectBytes::new(&commit));
        let mut d = FooterDigest::new(&prefix);
        for chunk in commit.chunks(13) {
            d.update(StoredObjectBytes::new(chunk));
        }
        assert_eq!(d.finalize(), whole);
    }

    #[test]
    fn footer_domain_is_the_spec_string() {
        assert_eq!(FOOTER_DOMAIN, b"MOCHI2-FOOTER\0");
    }

    #[test]
    fn debug_and_hex_name_the_scope() {
        let d = ChunkContentHash::from_bytes([0xab; 32]);
        assert_eq!(d.to_hex(), "ab".repeat(32));
        assert!(format!("{d:?}").starts_with("chunk-content:abab"));
        assert_eq!(ChunkContentHash::scope_name(), "chunk-content");
    }

    /// The sidecar's tail hash is plain BLAKE3 (`b3sum`-equal), as the
    /// tail-quarantine schema defines key 6.
    #[test]
    fn tail_quarantine_hash_is_plain_blake3() {
        let tail = b"bytes after the last valid commit";
        assert_eq!(
            tail_quarantine_hash(tail).as_bytes(),
            blake3::hash(tail).as_bytes()
        );
    }
}
