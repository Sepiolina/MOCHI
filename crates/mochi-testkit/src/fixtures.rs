//! Deterministic fixture helpers.

use mochi_core::object::IdSource;

/// `len` reproducible pseudo-random bytes for `seed` (SplitMix64). Used instead
/// of a randomness dependency so fixtures are byte-identical everywhere; golden
/// vectors must never depend on ambient entropy.
pub fn deterministic_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        for b in z.to_le_bytes() {
            if out.len() < len {
                out.push(b);
            }
        }
    }
    out
}

/// Deterministic [`IdSource`] for tests and fixtures: IDs are
/// `seed || counter` expanded through BLAKE3 so they look like real 32-byte
/// identities without ambient entropy. **Test-only**; production uses
/// `mochi_core::object::OsIds` (plan §9, O19).
pub struct SeqIds {
    seed: u64,
    next: u64,
}

impl SeqIds {
    pub fn new(seed: u64) -> Self {
        SeqIds { seed, next: 0 }
    }
}

impl IdSource for SeqIds {
    fn next_id(&mut self) -> mochi_core::Result<[u8; 32]> {
        let mut input = [0u8; 16];
        input[..8].copy_from_slice(&self.seed.to_le_bytes());
        input[8..].copy_from_slice(&self.next.to_le_bytes());
        self.next += 1;
        Ok(*blake3::hash(&input).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_exact_length() {
        assert_eq!(deterministic_bytes(1, 37), deterministic_bytes(1, 37));
        assert_ne!(deterministic_bytes(1, 37), deterministic_bytes(2, 37));
        assert_eq!(deterministic_bytes(9, 0).len(), 0);
        assert_eq!(deterministic_bytes(9, 1000).len(), 1000);
    }

    #[test]
    fn prefix_stable() {
        let long = deterministic_bytes(5, 64);
        let short = deterministic_bytes(5, 20);
        assert_eq!(&long[..20], &short[..]);
    }
}
