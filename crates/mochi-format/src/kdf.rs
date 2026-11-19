//! Key derivation for the Encrypted profile (spec Annex B.2.10 item 1; R5).
//!
//! Argon2id (RFC 9106), version 0x13, a 16-byte salt, a 32-byte output, no
//! secret and no associated data. The cost parameters are recorded in each
//! key envelope and are **archive-derived**, so [`KdfParams::check`] compares
//! them with the reader's limits *before* any memory is allocated: a hostile
//! archive cannot make a reader reserve a terabyte or spin for an hour.
//!
//! The derivation consumes the NFC-normalized passphrase bytes
//! ([`Passphrase`]) so the same text derives the same key on every platform.

use argon2::{Algorithm, Argon2, Params, Version};

use crate::error::{FormatError, LimitKind, Result, SealFault};
use crate::limits::Limits;
use crate::secret::{Kek, Passphrase};

/// KDF identifier for Argon2id in a key envelope's KDF map (key 0).
pub const KDF_ARGON2ID: u64 = 1;
/// Argon2 version 0x13 (decimal 19) in a key envelope's KDF map (key 1).
pub const ARGON2_VERSION_0X13: u64 = 0x13;
/// Output length: a 256-bit key-encryption key.
pub const KDF_OUTPUT_LEN: usize = 32;
/// Salt length.
pub const SALT_LEN: usize = 16;

/// The cost parameters a key envelope records, as unsigned integers exactly
/// as stored (they may be hostile until [`KdfParams::check`] passes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    /// Memory in KiB (`m`).
    pub memory_kib: u64,
    /// Passes (`t`).
    pub iterations: u64,
    /// Lanes (`p`); the thread count is the same.
    pub lanes: u64,
}

impl KdfParams {
    /// The writer defaults (D20 item 1): RFC 9106 §4's second recommended
    /// option, m = 64 MiB, t = 3, p = 4.
    pub const WRITER_DEFAULT: KdfParams = KdfParams {
        memory_kib: 65_536,
        iterations: 3,
        lanes: 4,
    };

    /// Reader-side validation, **before any allocation**: first the reader's
    /// limits (`LIMIT_EXCEEDED`, naming the declared value), then Argon2's own
    /// minimums (`m ≥ 8·p`, `t ≥ 1`, `p ≥ 1`).
    pub fn check(&self, limits: &Limits) -> Result<()> {
        let over = |kind, limit: u64, actual: u64| FormatError::LimitExceeded {
            kind,
            limit,
            actual,
        };
        if self.memory_kib > limits.max_kdf_memory_kib {
            return Err(over(
                LimitKind::KdfMemory,
                limits.max_kdf_memory_kib,
                self.memory_kib,
            ));
        }
        if self.iterations > limits.max_kdf_iterations {
            return Err(over(
                LimitKind::KdfIterations,
                limits.max_kdf_iterations,
                self.iterations,
            ));
        }
        if self.lanes > limits.max_kdf_lanes {
            return Err(over(LimitKind::KdfLanes, limits.max_kdf_lanes, self.lanes));
        }
        let min_memory = self.lanes.checked_mul(8);
        if self.iterations < 1
            || self.lanes < 1
            || min_memory.is_none_or(|min| self.memory_kib < min)
        {
            return Err(FormatError::Seal(SealFault::BadKdfParameters));
        }
        Ok(())
    }
}

/// Derive the key-encryption key for one envelope: Argon2id over the
/// normalized passphrase and the envelope's salt. `params` is checked against
/// `limits` first.
pub fn derive_kek(
    passphrase: &Passphrase,
    salt: &[u8; SALT_LEN],
    params: &KdfParams,
    limits: &Limits,
) -> Result<Kek> {
    params.check(limits)?;
    let conv = |v: u64| {
        u32::try_from(v).map_err(|_| {
            // Unreachable under any limit below 2^32; refuse rather than wrap.
            FormatError::LimitExceeded {
                kind: LimitKind::KdfMemory,
                limit: u64::from(u32::MAX),
                actual: v,
            }
        })
    };
    let argon_params = Params::new(
        conv(params.memory_kib)?,
        conv(params.iterations)?,
        conv(params.lanes)?,
        Some(KDF_OUTPUT_LEN),
    )
    .map_err(|_| FormatError::Seal(SealFault::BadKdfParameters))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params);
    let mut out = zeroize::Zeroizing::new([0u8; KDF_OUTPUT_LEN]);
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut out[..])
        .map_err(|_| FormatError::Seal(SealFault::PrimitiveFailed))?;
    Ok(Kek::from_bytes(*out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> KdfParams {
        KdfParams {
            memory_kib: 64,
            iterations: 2,
            lanes: 1,
        }
    }

    #[test]
    fn limits_are_checked_before_structure_and_before_allocation() {
        let l = Limits::default();
        let mut p = KdfParams::WRITER_DEFAULT;
        p.memory_kib = l.max_kdf_memory_kib + 1;
        match p.check(&l) {
            Err(FormatError::LimitExceeded {
                kind: LimitKind::KdfMemory,
                actual,
                ..
            }) => assert_eq!(actual, l.max_kdf_memory_kib + 1),
            other => panic!("{other:?}"),
        }
        let mut p = KdfParams::WRITER_DEFAULT;
        p.iterations = 17;
        assert!(matches!(
            p.check(&l),
            Err(FormatError::LimitExceeded {
                kind: LimitKind::KdfIterations,
                ..
            })
        ));
        let mut p = KdfParams::WRITER_DEFAULT;
        p.lanes = 17;
        assert!(matches!(
            p.check(&l),
            Err(FormatError::LimitExceeded {
                kind: LimitKind::KdfLanes,
                ..
            })
        ));
        // A hostile 2^40 KiB is a limit error, never an attempted allocation.
        let p = KdfParams {
            memory_kib: 1 << 40,
            iterations: 3,
            lanes: 4,
        };
        let pass = Passphrase::new("x").unwrap();
        assert!(matches!(
            derive_kek(&pass, &[0; 16], &p, &l),
            Err(FormatError::LimitExceeded { .. })
        ));
    }

    #[test]
    fn structure_is_checked_at_the_boundary() {
        let l = Limits::default();
        let ok = |m, t, p| {
            KdfParams {
                memory_kib: m,
                iterations: t,
                lanes: p,
            }
            .check(&l)
        };
        assert!(ok(8, 1, 1).is_ok());
        assert!(ok(32, 1, 4).is_ok());
        assert!(matches!(
            ok(7, 1, 1),
            Err(FormatError::Seal(SealFault::BadKdfParameters))
        ));
        assert!(matches!(
            ok(31, 1, 4),
            Err(FormatError::Seal(SealFault::BadKdfParameters))
        ));
        assert!(matches!(
            ok(64, 0, 1),
            Err(FormatError::Seal(SealFault::BadKdfParameters))
        ));
        assert!(matches!(
            ok(64, 1, 0),
            Err(FormatError::Seal(SealFault::BadKdfParameters))
        ));
        // The limits themselves are accepted.
        assert!(ok(l.max_kdf_memory_kib, 16, 16).is_ok());
    }

    #[test]
    fn derivation_is_deterministic_and_depends_on_every_input() {
        let l = Limits::default();
        let p = Passphrase::new("correct horse").unwrap();
        let a = derive_kek(&p, &[1; 16], &small(), &l).unwrap();
        let b = derive_kek(&p, &[1; 16], &small(), &l).unwrap();
        assert!(a.ct_eq(&b));
        let other_salt = derive_kek(&p, &[2; 16], &small(), &l).unwrap();
        assert!(!a.ct_eq(&other_salt));
        let other_pass = derive_kek(
            &Passphrase::new("correct horsf").unwrap(),
            &[1; 16],
            &small(),
            &l,
        )
        .unwrap();
        assert!(!a.ct_eq(&other_pass));
        let mut costlier = small();
        costlier.iterations = 3;
        assert!(!a.ct_eq(&derive_kek(&p, &[1; 16], &costlier, &l).unwrap()));
    }
}
