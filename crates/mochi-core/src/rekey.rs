//! `rekey`: changing who can open an Encrypted archive (spec Annex B.2.10 D20
//! item 10; plan C11).
//!
//! * [`list`] shows the head's key envelopes. It needs no key.
//! * [`rewrap`] adds a passphrase and/or removes envelopes by ID. It writes
//!   **one ordinary commit**: no namespace operations, the new envelope frames,
//!   the new complete envelope set in commit key 11, and the key operations
//!   (manifest key 13) as the audit record. The data key and the key ID are
//!   unchanged and no data is rewritten. Any passphrase of the head's set
//!   authenticates it. The set never becomes empty.
//! * [`reencrypt`] writes a **new archive** through the compaction path: new
//!   archive ID, new data key, new key ID, one envelope per new passphrase,
//!   every snapshot kept. Its audit record is delta(0)'s provenance with
//!   reason *re-encryption*. Never in place.
//!
//! **Removal is not revocation.** The removed envelope's frame stays in the
//! file's history, and any earlier copy of the archive still opens with it.
//! Re-encryption does not recall copies already made. Nothing here, and no
//! caller's wording, may call either "revoked" or "secure".

use std::sync::Arc;

use mochi_format::kdf::KdfParams;
use mochi_format::secret::Passphrase;

use crate::compact::{compact, CompactOptions, CompactReport, Keep};
use crate::error::{ErrorCode, MochiError, Result};
use crate::job::JobContext;
use crate::keys::{read_envelopes, KeySession};
use crate::manifest::RewriteReason;
use crate::object::IdSource;
use crate::publish::{commit_history, ArchiveWriter, CommitOutcome, ReadOptions, Transaction};
use crate::storage::{ReadStorage, Storage, StorageDir};

/// One key envelope of the head, as `rekey --list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeInfo {
    /// Hex, the ID `--remove-passphrase` takes.
    pub envelope_id: String,
    /// The commit that wrote the envelope.
    pub created_at_seq: u64,
    pub kdf: KdfParams,
}

/// The head's key envelopes (commit key 11), in increasing ID order. Needs no
/// key: the envelopes are plaintext records, hash-verified before they are
/// parsed. `INVALID_ARGUMENT` for an archive that is not Encrypted.
pub fn list(src: &dyn ReadStorage, opts: &ReadOptions) -> Result<Vec<EnvelopeInfo>> {
    let head = commit_history(src, opts)?
        .pop()
        .ok_or_else(|| MochiError::new(ErrorCode::NoValidHead, "the archive has no commits"))?;
    if !head.commit.encrypted() {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "this archive is not Encrypted: it has no key envelopes",
        ));
    }
    Ok(read_envelopes(src, &head.commit, head.commit_offset, opts)?
        .into_iter()
        .map(|(_, e)| EnvelopeInfo {
            envelope_id: hex(&e.envelope_id),
            created_at_seq: e.sequence,
            kdf: e.kdf,
        })
        .collect())
}

/// Parse the hex envelope ID `--remove-passphrase` takes.
pub fn parse_envelope_id(s: &str) -> Result<[u8; 16]> {
    let bad = || {
        MochiError::new(
            ErrorCode::InvalidArgument,
            "an envelope ID is 32 hexadecimal digits (see `rekey --list`)",
        )
    };
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The transaction of a rewrap: `add` as new key envelopes, `remove` taken out
/// of the set. `INVALID_ARGUMENT` if it would change nothing.
pub fn rewrap_transaction(add: Vec<Passphrase>, remove: Vec<[u8; 16]>) -> Result<Transaction> {
    if add.is_empty() && remove.is_empty() {
        return Err(MochiError::new(
            ErrorCode::InvalidArgument,
            "nothing to do: give a passphrase to add or an envelope ID to remove",
        ));
    }
    let mut tx = Transaction::new();
    for p in add {
        tx.add_passphrase(Arc::new(p));
    }
    for id in remove {
        tx.remove_envelope(id);
    }
    Ok(tx)
}

/// Add `add` as new key envelopes and take `remove` out of the set, in one
/// commit on the open writer (a rewrap). Authentication was the writer's
/// opening with a passphrase of the head's set. `INVALID_ARGUMENT` if the
/// request changes nothing, names an envelope that is not valid at the head,
/// or would leave the set empty.
pub fn rewrap<S: Storage>(
    w: &mut ArchiveWriter<S>,
    add: Vec<Passphrase>,
    remove: Vec<[u8; 16]>,
    ctx: &JobContext<'_>,
) -> Result<CommitOutcome> {
    w.commit(rewrap_transaction(add, remove)?, ctx)
}

/// Write a new archive `name` in `dir` from the open source writer: every
/// snapshot kept, a new data key, one envelope for each of `new_passphrases`.
/// `source_read` opens the source (its passphrases); the source is never
/// written.
#[allow(clippy::too_many_arguments)]
pub fn reencrypt<S, D>(
    source: &ArchiveWriter<S>,
    dir: &mut D,
    name: &str,
    ids: Box<dyn IdSource>,
    source_read: ReadOptions,
    new_passphrases: Vec<Passphrase>,
    kdf: Option<KdfParams>,
    ctx: &JobContext<'_>,
) -> Result<CompactReport>
where
    S: Storage,
    D: StorageDir,
    D::File: Storage,
{
    let new_keys = KeySession::new(new_passphrases)?;
    compact(
        source,
        dir,
        name,
        ids,
        &Keep::Every,
        &CompactOptions {
            read: source_read,
            new_keys: Some(new_keys),
            kdf,
            reason: RewriteReason::Reencryption,
            ..CompactOptions::default()
        },
        ctx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_ids_parse_from_32_hex_digits_only() {
        let id = parse_envelope_id("000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(id[0], 0);
        assert_eq!(id[15], 15);
        assert_eq!(hex(&id), "000102030405060708090a0b0c0d0e0f");
        for bad in [
            "",
            "00",
            "zz0102030405060708090a0b0c0d0e0f",
            "+0102030405060708090a0b0c0d0e0f1",
        ] {
            assert_eq!(
                parse_envelope_id(bad).unwrap_err().code,
                ErrorCode::InvalidArgument,
                "{bad}"
            );
        }
        assert!(parse_envelope_id("é0102030405060708090a0b0c0d0e0f").is_err());
    }
}
