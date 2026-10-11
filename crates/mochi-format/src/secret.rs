//! Secret material and randomness for the Encrypted profile (spec Annex B.2.10,
//! D20; `docs/ratification/R5-crypto-draft.md`).
//!
//! Every secret (passphrase, key-encryption key, data key) lives in a type
//! that zeroizes on drop and offers **no** way to read its bytes back out of
//! the crate: no `Clone`, no `AsRef<[u8]>`, no serialization, and a `Debug`
//! that prints a fixed placeholder. A secret is therefore never logged,
//! printed, or put in an error message by accident (AGENTS.md: secrets use
//! `zeroize`). The only readers are this crate's own AEAD and KDF code and
//! [`Secret::matches`], which compares against expected bytes without
//! returning them (test vectors use it).
//!
//! Randomness comes from the operating system's CSPRNG through [`Random`].
//! Nothing here derives a nonce, key, or identifier from content, position,
//! or a counter (spec §14.2).
//!
//! ```compile_fail,E0599
//! // A data key cannot be cloned: there is no second copy to forget to wipe.
//! use mochi_format::secret::DataKey;
//! let k = DataKey::from_bytes([1; 32]);
//! let _k2 = k.clone();
//! ```
//!
//! ```compile_fail,E0624
//! // And its bytes cannot be read back out.
//! use mochi_format::secret::DataKey;
//! let k = DataKey::from_bytes([1; 32]);
//! let _b: &[u8; 32] = k.as_bytes();
//! ```

use std::fmt;

use unicode_normalization::UnicodeNormalization;
use zeroize::{Zeroize, Zeroizing};

use crate::error::{FormatError, Result, SealFault};

/// Longest passphrase accepted, in bytes after NFC normalization (D20 item 2).
pub const MAX_PASSPHRASE_LEN: usize = 4096;

/// A source of cryptographically secure random bytes. The production source
/// is [`OsRandom`]; tests inject deterministic ones through the writer's
/// `test-controls`, never through a shipped path.
pub trait Random {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()>;
}

/// The operating system's CSPRNG. A failure is an error, never a fallback to
/// a weaker source.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl Random for OsRandom {
    fn fill(&mut self, buf: &mut [u8]) -> Result<()> {
        getrandom::fill(buf).map_err(|_| FormatError::Seal(SealFault::RandomFailed))
    }
}

/// Fixed-size secret bytes that wipe themselves. Private to this crate's
/// users except through the named wrappers below.
struct Secret<const N: usize>(Zeroizing<[u8; N]>);

impl<const N: usize> Secret<N> {
    fn new(bytes: [u8; N]) -> Self {
        Secret(Zeroizing::new(bytes))
    }
    fn bytes(&self) -> &[u8; N] {
        &self.0
    }
    fn matches(&self, expected: &[u8]) -> bool {
        // Constant time in the length of the secret; the length itself is
        // public.
        if expected.len() != N {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(expected) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

macro_rules! secret_type {
    ($(#[$doc:meta])* $name:ident, $label:literal) => {
        $(#[$doc])*
        pub struct $name(Secret<32>);

        impl $name {
            /// Wrap 32 bytes the caller has just drawn or derived. The
            /// caller's copy is its own to wipe.
            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                $name(Secret::new(bytes))
            }

            /// Draw a fresh random value.
            pub fn generate(rng: &mut dyn Random) -> Result<Self> {
                let mut b = Zeroizing::new([0u8; 32]);
                rng.fill(&mut b[..])?;
                Ok($name(Secret::new(*b)))
            }

            pub(crate) fn as_bytes(&self) -> &[u8; 32] {
                self.0.bytes()
            }

            /// Whether this secret equals `expected`, without revealing it.
            pub fn matches(&self, expected: &[u8]) -> bool {
                self.0.matches(expected)
            }

            /// Whether two secrets are equal, without revealing either.
            pub fn ct_eq(&self, other: &Self) -> bool {
                self.0.matches(other.0.bytes())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!($label, "(redacted)"))
            }
        }
    };
}

secret_type!(
    /// The archive's data key (DEK): the one key every sealed object is
    /// encrypted under (D20 item 3).
    DataKey, "DataKey"
);
secret_type!(
    /// A key-encryption key derived from one passphrase; it wraps the DEK in
    /// one key envelope and is dropped as soon as it has.
    Kek, "Kek"
);

/// A passphrase after Unicode NFC normalization (D20 item 2): the bytes the
/// KDF consumes. The same passphrase typed through a Windows or an Ubuntu
/// input method normalizes to the same bytes.
pub struct Passphrase(Zeroizing<Vec<u8>>);

impl Passphrase {
    /// Normalize `text`. Empty, or longer than [`MAX_PASSPHRASE_LEN`] bytes
    /// after normalization, is refused.
    pub fn new(text: &str) -> Result<Self> {
        let mut normalized: String = text.nfc().collect();
        let bytes = Zeroizing::new(normalized.as_bytes().to_vec());
        normalized.zeroize();
        if bytes.is_empty() {
            return Err(FormatError::InvalidArgument("the passphrase is empty"));
        }
        if bytes.len() > MAX_PASSPHRASE_LEN {
            return Err(FormatError::InvalidArgument(
                "the passphrase is longer than 4096 bytes",
            ));
        }
        Ok(Passphrase(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Whether the normalized bytes equal `expected`, without revealing them.
    pub fn matches(&self, expected: &[u8]) -> bool {
        if expected.len() != self.0.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.0.iter().zip(expected) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Passphrase(redacted)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_bytes() {
        let k = DataKey::from_bytes([0xAB; 32]);
        let p = Passphrase::new("hunter2-very-secret").unwrap();
        for s in [format!("{k:?}"), format!("{p:?}"), format!("{k:#?}")] {
            assert!(!s.contains("ab"), "{s}");
            assert!(!s.contains("hunter2"), "{s}");
            assert!(s.contains("redacted"), "{s}");
        }
    }

    #[test]
    fn nfc_makes_composed_and_decomposed_equal() {
        let composed = "\u{00C5}ngstr\u{00F6}m";
        let decomposed = "A\u{030A}ngstro\u{0308}m";
        assert_ne!(composed.as_bytes(), decomposed.as_bytes());
        let a = Passphrase::new(composed).unwrap();
        let b = Passphrase::new(decomposed).unwrap();
        assert!(a.matches(composed.as_bytes()));
        assert!(b.matches(composed.as_bytes()));
        assert!(!b.matches(decomposed.as_bytes()));
    }

    #[test]
    fn empty_and_overlong_passphrases_are_refused() {
        assert!(Passphrase::new("").is_err());
        assert!(Passphrase::new(&"x".repeat(MAX_PASSPHRASE_LEN)).is_ok());
        assert!(Passphrase::new(&"x".repeat(MAX_PASSPHRASE_LEN + 1)).is_err());
        // The limit is on the normalized form: a multi-byte letter counts as
        // its UTF-8 length.
        assert!(Passphrase::new(&"\u{00E9}".repeat(MAX_PASSPHRASE_LEN / 2)).is_ok());
        assert!(Passphrase::new(&"\u{00E9}".repeat(MAX_PASSPHRASE_LEN / 2 + 1)).is_err());
    }

    #[test]
    fn matches_is_exact() {
        let k = DataKey::from_bytes([7; 32]);
        assert!(k.matches(&[7; 32]));
        assert!(!k.matches(&[7; 31]));
        let mut other = [7u8; 32];
        other[31] = 8;
        assert!(!k.matches(&other));
    }

    #[test]
    fn os_random_fills_and_differs() {
        let mut r = OsRandom;
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        r.fill(&mut a).unwrap();
        r.fill(&mut b).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }
}
