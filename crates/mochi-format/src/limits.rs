//! Configurable parsing limits (spec §8.5). Archives are untrusted; every
//! walk is bounded by these before any seek or allocation.
//!
//! Defaults are spec Annex B.2.3. Each is chosen so it can actually be
//! reached through the frame that carries it; the derivations are written
//! out as constants below so the arithmetic is checked, not copied.
//!
//! **Writer default rule (B.2.3).** A default writer enforces every
//! *reader-default* limit on everything it emits, whatever limits it was
//! configured to read with. Writers therefore use [`Limits::WRITER_DEFAULT`],
//! never a caller's raised reader limits. Opting out is a creation-time
//! choice (`create --exceed-default-limits`, plan T29), deferred by owner
//! decision Q10 (2026-10-06; spec Annex B, D16): every writer keeps these.

/// Default skippable payload *S* (B.2.3): 256 MiB. The wire maximum is
/// 4 GiB − 1.
pub const DEFAULT_SKIPPABLE_PAYLOAD: u64 = 256 << 20;

/// Default decoded data-object size *D* (B.2.3): 256 MiB.
pub const DEFAULT_DECODED_OBJECT: u64 = 256 << 20;

/// libzstd's `ZSTD_COMPRESSBOUND(D)` for *D* ≥ 128 KiB: `D + (D >> 8)`.
/// The small-input term `((128 KiB − D) >> 11)` is zero in that range.
pub const fn zstd_compress_bound_large(d: u64) -> u64 {
    d + (d >> 8)
}

/// Default limit on any single frame (B.2.3): max(8 + *S*,
/// `ZSTD_COMPRESSBOUND`(*D*)) = 269,484,032 bytes. Was 4 GiB.
pub const DEFAULT_FRAME_LEN: u64 = {
    let skippable = 8 + DEFAULT_SKIPPABLE_PAYLOAD;
    let data = zstd_compress_bound_large(DEFAULT_DECODED_OBJECT);
    if skippable > data {
        skippable
    } else {
        data
    }
};

/// Default limit on a stored commit frame (B.2.3): 8 + 64 KiB. Was 256 MiB.
pub const DEFAULT_COMMIT_FRAME_LEN: u64 = 8 + (64 << 10);

/// Default cap on required features in one record (B.2.3). Also the binary
/// envelope v0 wire rule n ≤ 64 (B.2.2).
pub const DEFAULT_REQUIRED_FEATURES: u64 = 64;

/// Largest binary envelope v0 header: 80 + 8·64 = 592 bytes.
pub const MAX_BINARY_ENVELOPE_HEADER: u64 = 80 + 8 * DEFAULT_REQUIRED_FEATURES;

/// Default catalog-image payload budget (B.2.3): *S* − 592 = 268,434,864
/// bytes, so the largest header plus the image fits one skippable frame.
pub const DEFAULT_IMAGE_PAYLOAD: u64 = DEFAULT_SKIPPABLE_PAYLOAD - MAX_BINARY_ENVELOPE_HEADER;

/// Bytes sealing adds to a payload (spec Annex B.2.10 item 5): the 48-byte
/// sealed header and the 16-byte tag.
pub const SEAL_OVERHEAD: u64 = 64;

/// Catalog-image payload budget of an Encrypted archive (B.2.10 item 5): the
/// binary envelope header (at most 592 bytes) and the 64 bytes of sealing
/// both come out of *S*, so *S* − 656 = 268,434,800.
pub const DEFAULT_SEALED_IMAGE_PAYLOAD: u64 = DEFAULT_IMAGE_PAYLOAD - SEAL_OVERHEAD;

/// Reader defaults for the Encrypted profile's key derivation (B.2.10 item 1):
/// at most 1 GiB of Argon2id memory, 16 passes, 16 lanes, and 16 key
/// envelopes per commit.
pub const DEFAULT_KDF_MEMORY_KIB: u64 = 1 << 20;
pub const DEFAULT_KDF_ITERATIONS: u64 = 16;
pub const DEFAULT_KDF_LANES: u64 = 16;
pub const DEFAULT_KEY_ENVELOPES: u64 = 16;

// The figures B.2.3 states, pinned at compile time.
const _: () = assert!(DEFAULT_SEALED_IMAGE_PAYLOAD == 268_434_800);
const _: () = assert!(DEFAULT_FRAME_LEN == 269_484_032);
const _: () = assert!(DEFAULT_IMAGE_PAYLOAD == 268_434_864);
const _: () = assert!(DEFAULT_COMMIT_FRAME_LEN == 65_544);

/// Resource limits applied while walking frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest skippable-frame payload accepted (the wire field allows 4 GiB − 1).
    pub max_skippable_payload: u64,
    /// Largest physical length of any single frame.
    pub max_frame_len: u64,
    /// Most blocks a single data frame may contain. Bounds work on inputs made
    /// of millions of empty blocks.
    pub max_blocks_per_frame: u64,
    /// Largest Zstandard window a data frame may declare.
    pub max_window_size: u64,
    /// Largest stored commit frame the footer check will read and hash.
    pub max_commit_frame_len: u64,
    /// Largest decoded length of one object (C2). Bounds decompression output
    /// and the allocation made for it.
    pub max_decoded_object_len: u64,
    /// Most required features one record may list (B.2.3). Applies to both
    /// envelope encodings (D11).
    pub max_required_features: u64,
    /// Most Argon2id memory (KiB) a key envelope may declare (B.2.10 item 1).
    /// Checked before any memory is allocated.
    pub max_kdf_memory_kib: u64,
    /// Most Argon2id passes a key envelope may declare.
    pub max_kdf_iterations: u64,
    /// Most Argon2id lanes a key envelope may declare.
    pub max_kdf_lanes: u64,
    /// Most key envelopes one commit may list.
    pub max_key_envelopes: u64,
}

impl Limits {
    /// The reader defaults of spec Annex B.2.3, which are also what a default
    /// writer must stay within.
    pub const WRITER_DEFAULT: Limits = Limits {
        max_skippable_payload: DEFAULT_SKIPPABLE_PAYLOAD,
        max_frame_len: DEFAULT_FRAME_LEN,
        max_blocks_per_frame: 1 << 22,
        max_window_size: 128 << 20,
        max_commit_frame_len: DEFAULT_COMMIT_FRAME_LEN,
        max_decoded_object_len: DEFAULT_DECODED_OBJECT,
        max_required_features: DEFAULT_REQUIRED_FEATURES,
        max_kdf_memory_kib: DEFAULT_KDF_MEMORY_KIB,
        max_kdf_iterations: DEFAULT_KDF_ITERATIONS,
        max_kdf_lanes: DEFAULT_KDF_LANES,
        max_key_envelopes: DEFAULT_KEY_ENVELOPES,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::WRITER_DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_b_2_3_values() {
        let l = Limits::default();
        assert_eq!(l.max_skippable_payload, 268_435_456);
        assert_eq!(l.max_frame_len, 269_484_032);
        assert_eq!(l.max_commit_frame_len, 8 + 65_536);
        assert_eq!(l.max_decoded_object_len, 268_435_456);
        assert_eq!(l.max_required_features, 64);
        assert_eq!(l.max_kdf_memory_kib, 1_048_576);
        assert_eq!(l.max_kdf_iterations, 16);
        assert_eq!(l.max_kdf_lanes, 16);
        assert_eq!(l.max_key_envelopes, 16);
        assert_eq!(DEFAULT_SEALED_IMAGE_PAYLOAD, 268_434_800);
        assert_eq!(DEFAULT_IMAGE_PAYLOAD, 268_434_864);
    }

    /// B.2.3: "Each default below is chosen so that it can actually be
    /// reached through the frame that carries it."
    #[test]
    fn defaults_are_mutually_reachable() {
        let l = Limits::default();
        // A maximal skippable frame fits the frame limit.
        assert!(8 + l.max_skippable_payload <= l.max_frame_len);
        // A maximal image plus the largest envelope header is exactly S.
        assert_eq!(
            DEFAULT_IMAGE_PAYLOAD + MAX_BINARY_ENVELOPE_HEADER,
            l.max_skippable_payload
        );
        // A maximal commit frame is a skippable frame within S.
        assert!(l.max_commit_frame_len - 8 <= l.max_skippable_payload);
        // The 8 MiB maximum chunk (spec §13) fits the decoded-object limit.
        assert!(l.max_decoded_object_len >= 8 << 20);
    }
}
