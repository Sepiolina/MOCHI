//! Representation pipeline types (spec §9.1, plan §2.2).
//!
//! ```text
//! logical content        DecodedBytes / DecodedSlice
//!     -> compression     (codec::encode)
//! encoded plaintext      EncodedPlaintext
//!     -> AEAD, if any    (codec::protect)
//! stored payload         StoredPayload
//!     -> record framing  (codec::frame_object)
//! stored object bytes    StoredObject / StoredObjectBytes
//! ```
//!
//! "These representations MUST NOT be described interchangeably" (§9.1). Each
//! is its own type, and each digest in [`crate::digest`] accepts exactly one of
//! them, so hashing the wrong representation does not compile.
//!
//! Two deliberate asymmetries:
//!
//! * [`EncodedPlaintext`] and [`StoredPayload`] have **no public constructor
//!   and no public byte accessor.** They exist only inside the codec. No §9.2
//!   digest covers the compressed-plaintext representation, and the simplest
//!   way to guarantee nobody hashes it is to make its bytes unreachable.
//! * [`DecodedBytes`] and [`StoredObject`] *do* have public constructors,
//!   because bytes legitimately enter the system in those forms: logical
//!   content from the user, stored bytes read back from storage. The type then
//!   records which one the caller claimed; the verifier checks the claim.
//!
//! None of these types implements `From` or `AsRef` into another. Moving
//! between representations happens only through the named pipeline stages.
//!
//! # Compile-time guarantees (plan §8.1 "Digests" row)
//!
//! Each block below must fail to compile with the stated error code. Stable
//! rustdoc only checks *that* a block fails; the pinned code is enforced by
//! the `doc-nightly` CI job, so a typo cannot make a block pass for an
//! unrelated reason.
//!
//! The correct uses compile:
//!
//! ```
//! use mochi_format::digest::{chunk_content_hash, stored_object_hash, ChunkContentHash};
//! use mochi_format::repr::{DecodedBytes, StoredObject};
//! let h: ChunkContentHash = chunk_content_hash(&DecodedBytes::new(vec![1]));
//! let _ = stored_object_hash(StoredObject::from_loaded(vec![1]).view());
//! # let _ = h;
//! ```
//!
//! A chunk content hash is not a stored-object hash:
//!
//! ```compile_fail,E0308
//! use mochi_format::digest::{chunk_content_hash, StoredObjectHash};
//! use mochi_format::repr::DecodedBytes;
//! let _h: StoredObjectHash = chunk_content_hash(&DecodedBytes::new(vec![1]));
//! ```
//!
//! The chunk content hash does not accept stored bytes:
//!
//! ```compile_fail,E0308
//! use mochi_format::digest::chunk_content_hash;
//! use mochi_format::repr::StoredObject;
//! let _ = chunk_content_hash(&StoredObject::from_loaded(vec![1]));
//! ```
//!
//! The stored-object hash does not accept decoded bytes:
//!
//! ```compile_fail,E0308
//! use mochi_format::digest::stored_object_hash;
//! use mochi_format::repr::DecodedBytes;
//! let d = DecodedBytes::new(vec![1]);
//! let _ = stored_object_hash(d.as_slice());
//! ```
//!
//! Encoded-plaintext bytes are unreachable outside the codec, so no caller can
//! hash the compressed-plaintext representation:
//!
//! ```compile_fail,E0624
//! use mochi_format::codec::{encode, EncodeParams};
//! use mochi_format::repr::DecodedBytes;
//! let p = encode(&DecodedBytes::new(vec![1]), &EncodeParams::default()).unwrap();
//! let _bytes: Vec<u8> = p.into_inner();
//! ```
//!
//! Nor can a caller forge one:
//!
//! ```compile_fail,E0624
//! use mochi_format::repr::EncodedPlaintext;
//! let _ = EncodedPlaintext::from_codec(vec![1]);
//! ```

use std::fmt;
use std::ops::Range;

/// Exact decoded content: a chunk after decryption and decompression, or
/// logical file content before encoding. The chunk content hash covers this.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedBytes(Vec<u8>);

impl DecodedBytes {
    /// Wrap logical content (for example, bytes read from a user's file).
    pub fn new(bytes: Vec<u8>) -> Self {
        DecodedBytes(bytes)
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slice(&self) -> DecodedSlice<'_> {
        DecodedSlice(&self.0)
    }

    /// A sub-range, e.g. one file extent's bytes within a decoded chunk
    /// (spec §10.3). `None` if the range is out of bounds.
    pub fn slice(&self, range: Range<u64>) -> Option<DecodedSlice<'_>> {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        self.0.get(start..end).map(DecodedSlice)
    }

    /// Read access for restoration and comparison. Returning `&[u8]` is safe:
    /// every digest takes a typed wrapper, not a slice.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl fmt::Debug for DecodedBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Content may be sensitive; never print it.
        write!(f, "DecodedBytes({} bytes)", self.0.len())
    }
}

/// Borrowed view of decoded / logical bytes.
#[derive(Clone, Copy)]
pub struct DecodedSlice<'a>(&'a [u8]);

impl<'a> DecodedSlice<'a> {
    /// Borrow logical content that is not held in a [`DecodedBytes`].
    pub fn from_logical(bytes: &'a [u8]) -> Self {
        DecodedSlice(bytes)
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.0
    }
}

impl fmt::Debug for DecodedSlice<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DecodedSlice({} bytes)", self.0.len())
    }
}

/// Content after compression, before encryption. Crate-private bytes: no
/// digest scope covers this representation (§9.2 correction note).
pub struct EncodedPlaintext(Vec<u8>);

impl EncodedPlaintext {
    pub(crate) fn from_codec(bytes: Vec<u8>) -> Self {
        EncodedPlaintext(bytes)
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for EncodedPlaintext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EncodedPlaintext({} bytes)", self.0.len())
    }
}

/// Content after authenticated encryption (or identical to the encoded
/// plaintext when the object is unprotected), before record framing.
/// Crate-private bytes, as for [`EncodedPlaintext`].
pub struct StoredPayload(Vec<u8>);

impl StoredPayload {
    pub(crate) fn from_codec(bytes: Vec<u8>) -> Self {
        StoredPayload(bytes)
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.0
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for StoredPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StoredPayload({} bytes)", self.0.len())
    }
}

/// Exact stored object bytes: what is on disk, including record framing.
/// The stored-object hash covers this (spec §9.2).
#[derive(Clone, PartialEq, Eq)]
pub struct StoredObject(Vec<u8>);

impl StoredObject {
    /// Bytes read back from storage. Untrusted until verified against an
    /// object record.
    pub fn from_loaded(bytes: Vec<u8>) -> Self {
        StoredObject(bytes)
    }

    pub(crate) fn from_codec(bytes: Vec<u8>) -> Self {
        StoredObject(bytes)
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn view(&self) -> StoredObjectBytes<'_> {
        StoredObjectBytes(&self.0)
    }

    /// For writing to storage.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for StoredObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StoredObject({} bytes)", self.0.len())
    }
}

/// Borrowed exact stored-object bytes (spec §9.1). Not decoded bytes, not
/// compressed plaintext. Introduced in C1 for the footer digest.
#[derive(Debug, Clone, Copy)]
pub struct StoredObjectBytes<'a>(&'a [u8]);

impl<'a> StoredObjectBytes<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        StoredObjectBytes(bytes)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.0
    }
}

/// Exact compression-dictionary bytes. The dictionary hash covers this.
#[derive(Clone, PartialEq, Eq)]
pub struct DictionaryBytes(Vec<u8>);

impl DictionaryBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        DictionaryBytes(bytes)
    }

    pub fn len(&self) -> u64 {
        self.0.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for DictionaryBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DictionaryBytes({} bytes)", self.0.len())
    }
}

/// A commit body in canonical serialization, excluding the commit-ID field
/// (spec §9.2, §12.1).
///
/// **Canonicalization is not enforced yet.** The canonical serialization is
/// ratification item R3 and arrives with commits in C5; until then this type
/// only marks intent, and its constructor says so in its name.
#[derive(Debug, Clone, Copy)]
pub struct CanonicalCommitBody<'a>(&'a [u8]);

impl<'a> CanonicalCommitBody<'a> {
    /// The caller asserts `bytes` are canonical. Replaced by a serializer in C5.
    pub fn assume_canonical(bytes: &'a [u8]) -> Self {
        CanonicalCommitBody(bytes)
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_content() {
        let d = DecodedBytes::new(b"secret".to_vec());
        assert_eq!(format!("{d:?}"), "DecodedBytes(6 bytes)");
        let s = StoredObject::from_loaded(b"secret".to_vec());
        assert!(!format!("{s:?}").contains("secret"));
    }

    #[test]
    fn slice_is_bounds_checked() {
        let d = DecodedBytes::new((0..10).collect());
        assert_eq!(
            d.slice(2..5).map(|s| s.bytes().to_vec()),
            Some(vec![2, 3, 4])
        );
        assert!(d.slice(5..11).is_none());
        assert!(d.slice(u64::MAX - 1..u64::MAX).is_none());
        #[allow(clippy::reversed_empty_ranges)]
        let reversed = d.slice(5..2);
        assert!(reversed.is_none());
    }
}
