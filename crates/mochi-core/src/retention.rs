//! Retention state (spec §16.3, §18.3; Annex B D18; plan C9).
//!
//! Every commit is a retained root by default. Retention changes are manifest
//! operations (D10.3): a delta lists the operations its commit performs, and
//! a snapshot manifest carries the complete state after its commit. The
//! catalog image does not hold retention state, exactly as it does not hold
//! promised attributes: both are rebuilt from the segment base's snapshot
//! manifest S(*b*) plus the deltas after it (D10.10), so baseline recovery
//! needs no SQLite for them either.
//!
//! - **Expire** `s`: snapshot `s` stops being retained by default. Only
//!   earlier commits can be expired (`s` < the commit doing it); the head is
//!   always a root (§18.3 "current heads").
//! - **Hold** `label` on `s`: a legal hold. A held snapshot is a root whether
//!   or not it is expired (§16.3: holds override expiration and collection).
//!   `s` may be the commit placing the hold. Labels are 1–255 bytes, unique
//!   among active holds.
//! - **Release** `label`: ends that hold.
//!
//! Roots at head *h* are therefore *h*, every commit not expired, and every
//! held commit. Collection (C9 GC) keeps the dependency closure of exactly
//! those snapshots.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{ErrorCode, MochiError, Result};

/// Longest hold label, in bytes.
pub const MAX_HOLD_LABEL: usize = 255;

/// One retention operation (recovery-manifest schema 2, delta key 11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionOp {
    Expire { seq: u64 },
    Hold { label: Vec<u8>, seq: u64 },
    Release { label: Vec<u8> },
}

/// Complete retention state after a commit (snapshot key 11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionState {
    /// Expired commits.
    pub expired: BTreeSet<u64>,
    /// Active holds: label → held commit.
    pub holds: BTreeMap<Vec<u8>, u64>,
}

fn invalid(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::RecordInvalid, msg)
}

/// Rules an operation must meet on its own, for the commit `seq` that
/// carries it: checked on encode and decode alike.
pub(crate) fn check_op(op: &RetentionOp, seq: u64) -> Result<()> {
    match op {
        RetentionOp::Expire { seq: s } => {
            if *s >= seq {
                return Err(invalid(format!(
                    "commit {seq} expires snapshot {s}: only earlier snapshots can expire"
                )));
            }
        }
        RetentionOp::Hold { label, seq: s } => {
            check_label(label)?;
            if *s > seq {
                return Err(invalid(format!(
                    "commit {seq} holds snapshot {s}, which does not exist yet"
                )));
            }
        }
        RetentionOp::Release { label } => check_label(label)?,
    }
    Ok(())
}

pub(crate) fn check_label(label: &[u8]) -> Result<()> {
    if label.is_empty() || label.len() > MAX_HOLD_LABEL {
        return Err(invalid(format!(
            "a hold label is 1 to {MAX_HOLD_LABEL} bytes, not {}",
            label.len()
        )));
    }
    Ok(())
}

impl RetentionState {
    pub fn is_empty(&self) -> bool {
        self.expired.is_empty() && self.holds.is_empty()
    }

    /// Rules the complete state after commit `seq` must meet.
    pub(crate) fn check(&self, seq: u64) -> Result<()> {
        if let Some(s) = self.expired.iter().next_back() {
            if *s >= seq {
                return Err(invalid(format!(
                    "the state after commit {seq} has snapshot {s} expired"
                )));
            }
        }
        for (label, s) in &self.holds {
            check_label(label)?;
            if *s > seq {
                return Err(invalid(format!(
                    "the state after commit {seq} holds snapshot {s}"
                )));
            }
        }
        Ok(())
    }

    /// Apply commit `seq`'s operations in order, atomically: on any failure
    /// the state is unchanged (D10.4). Validity is judged against the state
    /// as each operation finds it.
    pub fn apply(&mut self, ops: &[RetentionOp], seq: u64) -> Result<()> {
        let mut next = self.clone();
        for op in ops {
            check_op(op, seq)?;
            match op {
                RetentionOp::Expire { seq: s } => {
                    if !next.expired.insert(*s) {
                        return Err(invalid(format!("snapshot {s} is already expired")));
                    }
                }
                RetentionOp::Hold { label, seq: s } => {
                    if next.holds.insert(label.clone(), *s).is_some() {
                        return Err(invalid(format!(
                            "hold {} is already active",
                            String::from_utf8_lossy(label)
                        )));
                    }
                }
                RetentionOp::Release { label } => {
                    if next.holds.remove(label).is_none() {
                        return Err(invalid(format!(
                            "hold {} is not active",
                            String::from_utf8_lossy(label)
                        )));
                    }
                }
            }
        }
        *self = next;
        Ok(())
    }

    /// The retained roots at head `head`: the head, every commit not
    /// expired, and every held commit.
    pub fn roots(&self, head: u64) -> BTreeSet<u64> {
        (0..=head)
            .filter(|s| *s == head || !self.expired.contains(s) || self.is_held(*s))
            .collect()
    }

    pub fn is_held(&self, seq: u64) -> bool {
        self.holds.values().any(|s| *s == seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(l: &str, seq: u64) -> RetentionOp {
        RetentionOp::Hold {
            label: l.as_bytes().to_vec(),
            seq,
        }
    }

    #[test]
    fn operations_apply_atomically_and_in_order() {
        let mut s = RetentionState::default();
        s.apply(&[RetentionOp::Expire { seq: 0 }, hold("legal", 1)], 2)
            .unwrap();
        assert_eq!(s.roots(2), BTreeSet::from([1, 2]));
        // The second operation fails, so the first does not apply either.
        let before = s.clone();
        let e = s
            .apply(&[RetentionOp::Expire { seq: 1 }, hold("legal", 2)], 3)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::RecordInvalid);
        assert_eq!(s, before);
        // A held snapshot stays a root after it expires; releasing frees it.
        s.apply(&[RetentionOp::Expire { seq: 1 }], 3).unwrap();
        assert_eq!(s.roots(3), BTreeSet::from([1, 2, 3]));
        s.apply(
            &[RetentionOp::Release {
                label: b"legal".to_vec(),
            }],
            4,
        )
        .unwrap();
        assert_eq!(s.roots(4), BTreeSet::from([2, 3, 4]));
    }

    #[test]
    fn invalid_operations_are_refused() {
        let mut s = RetentionState::default();
        for (ops, seq) in [
            (vec![RetentionOp::Expire { seq: 3 }], 3),
            (vec![RetentionOp::Expire { seq: 4 }], 3),
            (
                vec![
                    RetentionOp::Expire { seq: 1 },
                    RetentionOp::Expire { seq: 1 },
                ],
                3,
            ),
            (vec![hold("x", 4)], 3),
            (vec![hold("", 1)], 3),
            (vec![hold(&"x".repeat(256), 1)], 3),
            (vec![hold("x", 1), hold("x", 2)], 3),
            (
                vec![RetentionOp::Release {
                    label: b"none".to_vec(),
                }],
                3,
            ),
        ] {
            let e = s.apply(&ops, seq).unwrap_err();
            assert_eq!(e.code, ErrorCode::RecordInvalid, "{ops:?}");
            assert!(s.is_empty());
        }
        // The boundaries that are allowed.
        s.apply(
            &[hold(&"x".repeat(255), 3), RetentionOp::Expire { seq: 2 }],
            3,
        )
        .unwrap();
    }
}
