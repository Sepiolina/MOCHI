//! Tail-quarantine sidecar v0 (spec Annex B.2 D14, B.2.2;
//! `docs/schemas/tail-quarantine-v0.cddl`; plan T24). **Not** part of the
//! `.mochi` wire format: a separate file written beside the archive before
//! an explicit tail truncation, so the removed bytes can be restored.
//!
//! Layout:
//!
//! | Offset | Size | Field |
//! |---:|---:|---|
//! | 0 | 8 | magic `MOCHITQ\0` |
//! | 8 | 4 | *L*, metadata length, u32 LE, 1 ≤ *L* ≤ 65,536 |
//! | 12 | *L* | metadata: deterministic CBOR (the schema's map) |
//! | 12 + *L* | key 5 | the removed tail bytes, exactly |
//!
//! The file length is exactly 12 + *L* + key 5. Name:
//! `<archive name>.tail-<decimal offset>-<16 hex of key 6>.mochiq`, created
//! exclusively in the archive's directory.

use mochi_format::cbor::{self, CborLimits, Fields, Value};
use mochi_format::digest::{CommitId, TailQuarantineHash, TailQuarantineHasher};
use mochi_format::registry::TAIL_QUARANTINE_MAGIC;

use crate::error::{ErrorCode, MochiError, Result};
use crate::object::ArchiveId;
use crate::storage::ReadStorage;

/// Sidecar schema version (key 0).
pub const SIDECAR_SCHEMA_VERSION: u64 = 0;
/// Bytes before the metadata: magic and length.
pub const SIDECAR_HEADER_LEN: u64 = 12;
/// Largest metadata length *L*.
pub const MAX_METADATA_LEN: u32 = 65_536;
/// Chunk size for streaming the tail.
const CHUNK: usize = 1 << 20;

/// The sidecar's metadata map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarMetadata {
    pub archive_id: ArchiveId,
    pub head_commit_id: CommitId,
    pub head_seq: u64,
    /// The tail's offset in the archive: the end of the head footer.
    pub tail_offset: u64,
    pub tail_len: u64,
    pub tail_hash: TailQuarantineHash,
    /// Magics of the complete frames in the tail, in order.
    pub frame_magics: Vec<u32>,
    pub incomplete_final_frame: bool,
    /// Tool name and version.
    pub tool: String,
    /// Time written (D15 timestamp).
    pub time: String,
}

fn failed(msg: impl Into<String>) -> MochiError {
    MochiError::new(ErrorCode::QuarantineFailed, msg)
}

impl SidecarMetadata {
    fn to_value(&self) -> Value {
        Value::Map(vec![
            (0, Value::Uint(SIDECAR_SCHEMA_VERSION)),
            (1, Value::Bytes(self.archive_id.as_bytes().to_vec())),
            (2, Value::Bytes(self.head_commit_id.as_bytes().to_vec())),
            (3, Value::Uint(self.head_seq)),
            (4, Value::Uint(self.tail_offset)),
            (5, Value::Uint(self.tail_len)),
            (6, Value::Bytes(self.tail_hash.as_bytes().to_vec())),
            (
                7,
                Value::Array(
                    self.frame_magics
                        .iter()
                        .map(|m| Value::Uint(u64::from(*m)))
                        .collect(),
                ),
            ),
            (8, Value::Bool(self.incomplete_final_frame)),
            (9, Value::Text(self.tool.clone())),
            (10, Value::Text(self.time.clone())),
        ])
    }

    /// Magic, length, and metadata: everything before the tail bytes.
    pub fn header(&self) -> Result<Vec<u8>> {
        if self.tail_len == 0 {
            return Err(failed("a quarantine sidecar records a non-empty tail"));
        }
        let meta = cbor::encode(&self.to_value())?;
        let len = u32::try_from(meta.len())
            .ok()
            .filter(|l| (1..=MAX_METADATA_LEN).contains(l))
            .ok_or_else(|| failed(format!("sidecar metadata of {} bytes", meta.len())))?;
        let mut out = Vec::with_capacity(meta.len() + SIDECAR_HEADER_LEN as usize);
        out.extend_from_slice(&TAIL_QUARANTINE_MAGIC);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&meta);
        Ok(out)
    }

    fn decode(payload: &[u8]) -> Result<Self> {
        let v = cbor::decode(payload, &CborLimits::default())?;
        let mut f = Fields::of(&v, "quarantine sidecar")?;
        if f.req(0)?.uint("sidecar schema version")? != SIDECAR_SCHEMA_VERSION {
            return Err(MochiError::new(
                ErrorCode::UnsupportedFeature,
                "unknown quarantine sidecar schema version",
            ));
        }
        let archive_id = ArchiveId::from_bytes(f.req(1)?.bytes32("archive id")?);
        let head_commit_id = CommitId::from_bytes(f.req(2)?.bytes32("head commit id")?);
        let head_seq = f.req(3)?.uint("head sequence")?;
        let tail_offset = f.req(4)?.uint("tail offset")?;
        let tail_len = f.req(5)?.uint("tail length")?;
        let tail_hash = TailQuarantineHash::from_bytes(f.req(6)?.bytes32("tail hash")?);
        let frame_magics = f
            .req(7)?
            .array("frame magics")?
            .iter()
            .map(|m| {
                m.uint("frame magic").and_then(|u| {
                    u32::try_from(u).map_err(|_| {
                        mochi_format::FormatError::Schema("frame magic exceeds 32 bits".into())
                    })
                })
            })
            .collect::<std::result::Result<Vec<u32>, _>>()?;
        let incomplete_final_frame = match f.req(8)? {
            Value::Bool(b) => *b,
            _ => {
                return Err(MochiError::from(mochi_format::FormatError::Schema(
                    "incomplete-final-frame must be a bool".into(),
                )))
            }
        };
        let tool = f.req(9)?.text("tool")?.to_string();
        let time = f.req(10)?.text("time")?.to_string();
        f.finish()?;
        crate::timestamp::Timestamp::parse(&time)?;
        if tail_len == 0 {
            return Err(failed("a quarantine sidecar records an empty tail"));
        }
        Ok(SidecarMetadata {
            archive_id,
            head_commit_id,
            head_seq,
            tail_offset,
            tail_len,
            tail_hash,
            frame_magics,
            incomplete_final_frame,
            tool,
            time,
        })
    }
}

/// The sidecar's file name for an archive named `archive_name`.
pub fn sidecar_name(archive_name: &str, tail_offset: u64, hash: &TailQuarantineHash) -> String {
    let hex = hash.to_hex();
    format!("{archive_name}.tail-{tail_offset}-{}.mochiq", &hex[..16])
}

/// Hash `len` bytes of `src` from `offset`, streaming.
pub fn hash_range(src: &dyn ReadStorage, offset: u64, len: u64) -> Result<TailQuarantineHash> {
    let mut h = TailQuarantineHasher::new();
    for_each_chunk(src, offset, len, |c| {
        h.update(c);
        Ok(())
    })?;
    Ok(h.finalize())
}

/// Call `f` on `len` bytes of `src` from `offset`, in chunks.
pub fn for_each_chunk(
    src: &dyn ReadStorage,
    offset: u64,
    len: u64,
    mut f: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut buf = vec![0u8; CHUNK.min(usize::try_from(len).unwrap_or(CHUNK))];
    let mut done = 0u64;
    while done < len {
        let n = usize::try_from((len - done).min(CHUNK as u64)).unwrap_or(CHUNK);
        let at = offset
            .checked_add(done)
            .ok_or_else(|| MochiError::new(ErrorCode::OutOfBounds, "offset overflow"))?;
        let chunk = buf
            .get_mut(..n)
            .ok_or_else(|| MochiError::new(ErrorCode::OutOfBounds, "chunk"))?;
        src.read_exact_at(at, chunk)?;
        f(chunk)?;
        done += n as u64;
    }
    Ok(())
}

/// Read and check a sidecar: magic, metadata length bounds, canonical
/// metadata, exact file length, and the tail bytes' hash. Returns the
/// metadata; the tail is at [`SIDECAR_HEADER_LEN`] + *L*.
pub fn read_sidecar(src: &dyn ReadStorage) -> Result<(SidecarMetadata, u64)> {
    let size = src.size()?;
    let mut head = [0u8; SIDECAR_HEADER_LEN as usize];
    src.read_exact_at(0, &mut head)
        .map_err(|e| failed(format!("sidecar header unreadable: {e}")))?;
    if head[..8] != TAIL_QUARANTINE_MAGIC {
        return Err(failed("not a quarantine sidecar (magic)"));
    }
    let len = u32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    if !(1..=MAX_METADATA_LEN).contains(&len) {
        return Err(failed(format!(
            "sidecar metadata length {len} is out of range"
        )));
    }
    let mut meta = vec![0u8; len as usize];
    src.read_exact_at(SIDECAR_HEADER_LEN, &mut meta)
        .map_err(|e| failed(format!("sidecar metadata unreadable: {e}")))?;
    let m = SidecarMetadata::decode(&meta).map_err(|e| match e.code {
        ErrorCode::QuarantineFailed | ErrorCode::UnsupportedFeature => e,
        _ => failed(format!("sidecar metadata: {}", e.message)),
    })?;
    let tail_at = SIDECAR_HEADER_LEN + u64::from(len);
    if tail_at.checked_add(m.tail_len) != Some(size) {
        return Err(failed(format!(
            "sidecar is {size} bytes; its header says {}",
            tail_at.saturating_add(m.tail_len)
        )));
    }
    if hash_range(src, tail_at, m.tail_len)? != m.tail_hash {
        return Err(failed(
            "the sidecar's tail bytes do not match its recorded hash",
        ));
    }
    Ok((m, tail_at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(tail: &[u8]) -> SidecarMetadata {
        SidecarMetadata {
            archive_id: ArchiveId::from_bytes([1; 32]),
            head_commit_id: CommitId::from_bytes([2; 32]),
            head_seq: 3,
            tail_offset: 4096,
            tail_len: tail.len() as u64,
            tail_hash: mochi_format::digest::tail_quarantine_hash(tail),
            frame_magics: vec![
                mochi_format::registry::RECOVERY_MANIFEST,
                mochi_format::registry::ZSTD_DATA_FRAME,
            ],
            incomplete_final_frame: true,
            tool: "mochi-core 0.1.0".into(),
            time: "2026-10-05T12:00:00.000000000Z".into(),
        }
    }

    struct Bytes(Vec<u8>);
    impl ReadStorage for Bytes {
        fn size(&self) -> std::result::Result<u64, crate::storage::StorageError> {
            Ok(self.0.len() as u64)
        }
        fn read_at(
            &self,
            offset: u64,
            buf: &mut [u8],
        ) -> std::result::Result<usize, crate::storage::StorageError> {
            let start = (offset as usize).min(self.0.len());
            let n = buf.len().min(self.0.len() - start);
            buf[..n].copy_from_slice(&self.0[start..start + n]);
            Ok(n)
        }
    }

    fn sidecar(tail: &[u8]) -> Vec<u8> {
        let mut out = meta(tail).header().unwrap();
        out.extend_from_slice(tail);
        out
    }

    #[test]
    fn round_trip_and_layout() {
        let tail = b"interrupted commit bytes";
        let bytes = sidecar(tail);
        assert_eq!(bytes[..8], TAIL_QUARANTINE_MAGIC);
        let l = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 12 + l + tail.len());
        let (m, at) = read_sidecar(&Bytes(bytes.clone())).unwrap();
        assert_eq!(m, meta(tail));
        assert_eq!(&bytes[at as usize..], tail);
    }

    #[test]
    fn name_follows_the_schema() {
        let m = meta(b"x");
        let n = sidecar_name("a.mochi", 4096, &m.tail_hash);
        assert_eq!(
            n,
            format!("a.mochi.tail-4096-{}.mochiq", &m.tail_hash.to_hex()[..16])
        );
    }

    #[test]
    fn damage_is_refused() {
        let tail = b"interrupted commit bytes";
        let good = sidecar(tail);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("magic", {
                let mut b = good.clone();
                b[0] ^= 1;
                b
            }),
            ("length zero", {
                let mut b = good.clone();
                b[8..12].copy_from_slice(&0u32.to_le_bytes());
                b
            }),
            ("length too large", {
                let mut b = good.clone();
                b[8..12].copy_from_slice(&(MAX_METADATA_LEN + 1).to_le_bytes());
                b
            }),
            ("tail byte", {
                let mut b = good.clone();
                let n = b.len();
                b[n - 1] ^= 1;
                b
            }),
            ("truncated", good[..good.len() - 1].to_vec()),
            ("extended", {
                let mut b = good.clone();
                b.push(0);
                b
            }),
            ("metadata byte", {
                let mut b = good.clone();
                b[14] ^= 0x40;
                b
            }),
        ];
        for (what, b) in cases {
            let e = read_sidecar(&Bytes(b)).unwrap_err();
            assert!(
                matches!(
                    e.code,
                    ErrorCode::QuarantineFailed | ErrorCode::UnsupportedFeature
                ),
                "{what}: {e}"
            );
        }
    }

    #[test]
    fn an_empty_tail_is_never_written() {
        assert_eq!(
            meta(b"").header().unwrap_err().code,
            ErrorCode::QuarantineFailed
        );
    }
}
