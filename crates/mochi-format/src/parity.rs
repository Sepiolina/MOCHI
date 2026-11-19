//! Redundancy profile: Reed–Solomon parity over stored bytes (spec §15,
//! Annex B.2.11 D21; ratification artifact R6, which states everything here
//! as normative text and vectors).
//!
//! # Suite 1: Cauchy Reed–Solomon over GF(2^8)
//!
//! * **Field.** GF(2^8) with the primitive polynomial x^8 + x^4 + x^3 + x^2 + 1
//!   (`0x11D`); addition is XOR.
//! * **Matrix.** A group has *k* data shards and *m* parity shards,
//!   1 ≤ m ≤ k ≤ 128. Parity shard *p* is Σⱼ C\[p\]\[j\] · Dⱼ, byte by byte,
//!   with the Cauchy coefficient C\[p\]\[j\] = 1 / (xₚ ⊕ yⱼ), xₚ = p and
//!   yⱼ = 128 + j. Every square submatrix of a Cauchy matrix is invertible,
//!   so the systematic code is MDS: **any k of the k + m shards rebuild the
//!   group.** Because xₚ < 128 ≤ yⱼ, no coefficient is 1 and no parity shard
//!   is a byte copy of a data shard.
//!
//! # Layout of one commit's group set ([`Layout::compute`])
//!
//! A commit protects the stored byte range *R* = \[*a*, *b*): *a* is its
//! parent's footer offset (0 for commit 0, so the descriptor is covered),
//! *b* the end of its own commit frame. With *L* = *b* − *a* and the
//! archive's parity percentage *r* (1–100):
//!
//! * shard size *s* = min(1 MiB, max(⌈L / 1024⌉↑64, min(16 KiB, L↑64))),
//!   where x↑64 rounds up to a multiple of 64;
//! * real data shards *N* = ⌈L / s⌉; shard *i* covers \[*a* + *i*·*s*, …) and is
//!   zero-padded to *s*;
//! * groups *G* = ⌈N / 128⌉; shard *i* belongs to group *i* mod *G* at index
//!   *i* div *G* (interleaved, so a contiguous burst spreads across groups);
//! * *k* = ⌈N / G⌉ data shards per group; indices that reach past *N* are
//!   virtual all-zero shards, never stored;
//! * *m* = ⌈k · r / 100⌉ parity shards per group.
//!
//! # Parity frame (kind `ParityObject`, `registry::PARITY_OBJECT`)
//!
//! One frame per parity shard, *G*·*m* of them, all the same length, written
//! after the commit frame and before its footer, group by group (frame
//! *j* is shard *j* mod *m* of group *j* div *m*). Each frame is a skippable
//! frame whose payload is a binary envelope v0 (payload encoding 1, record
//! schema version 0, the commit's identity and required features) followed
//! by this payload (integers unsigned little-endian):
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 16 | Signature `MOCHI2-PARITY\0\0\0` |
//! | 16 | 2 | Suite = 1 |
//! | 18 | 2 | Parity percentage *r* |
//! | 20 | 4 | Shard size *s* |
//! | 24 | 8 | Range offset *a* |
//! | 32 | 8 | Range length *L* |
//! | 40 | 4 | Group count *G* |
//! | 44 | 2 | Data shards per group *k* |
//! | 46 | 2 | Parity shards per group *m* |
//! | 48 | 4 | Group index *g* |
//! | 52 | 4 | Parity index *p* |
//! | 56 | 32·k | The group's data-shard hashes, index order |
//! | 56 + 32·k | s | Parity shard |
//! | 56 + 32·k + s | 32 | Frame digest |
//!
//! The data-shard hash is BLAKE3 over `MOCHI2-PARITY-SHARD\0` and the padded
//! shard; the frame digest is BLAKE3 over `MOCHI2-PARITY-FRAME\0` and every
//! frame byte before it, skippable header included. Every frame of a group
//! repeats the group's hash table, so any one intact frame locates damage.
//!
//! Parity is a repair aid, not an integrity anchor: no hash in the commit
//! chain covers a parity frame, and a rebuilt byte is used only after the
//! commit chain's own hashes accept it (`mochi-core`).

use crate::digest::{parity_frame_digest, parity_shard_hash, StoredObjectBytes, DIGEST_LEN};
use crate::envelope::{
    decode_binary_record, encode_binary_record, BinaryEnvelope, EnvelopeRules, PayloadEncoding,
    RecordIdentity,
};
use crate::error::{FormatError, ParityFault, Result};
use crate::frame::{walk_frame, FrameDetail};
use crate::limits::Limits;
use crate::registry::{self, FrameKind, SKIPPABLE_HEADER_LEN};

/// Suite 1: Cauchy Reed–Solomon over GF(2^8), polynomial `0x11D`, with the
/// layout rule above. The only suite this build reads or writes.
pub const SUITE_CAUCHY_GF256: u16 = 1;
/// Record schema version of a parity frame's binary envelope.
pub const RECORD_SCHEMA_VERSION: u16 = 0;
/// The field's reduction polynomial, x^8 + x^4 + x^3 + x^2 + 1.
pub const FIELD_POLYNOMIAL: u16 = 0x11D;
/// Most data shards in one group; also the first *y* value of the matrix.
pub const MAX_DATA_SHARDS: u64 = 128;
/// Shard size bounds and alignment of the layout rule.
pub const MIN_SHARD: u64 = 16 << 10;
pub const MAX_SHARD: u64 = 1 << 20;
pub const SHARD_ALIGN: u64 = 64;
/// The number of shards the layout aims for before *s* reaches its cap.
pub const TARGET_SHARDS: u64 = 1024;
/// The parity percentage's range.
pub const MIN_PERCENT: u16 = 1;
pub const MAX_PERCENT: u16 = 100;
/// Fixed part of the parity payload, before the hash table.
pub const PAYLOAD_FIXED_LEN: usize = 56;

// ---- GF(2^8) ---------------------------------------------------------------

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

const fn build_tables() -> Tables {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= FIELD_POLYNOMIAL;
        }
        i += 1;
    }
    while i < 512 {
        exp[i] = exp[i - 255];
        i += 1;
    }
    Tables { exp, log }
}

static TABLES: Tables = build_tables();

/// Product in GF(2^8).
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    TABLES.exp[usize::from(TABLES.log[usize::from(a)]) + usize::from(TABLES.log[usize::from(b)])]
}

/// Multiplicative inverse in GF(2^8); `None` for 0.
pub fn gf_inv(a: u8) -> Option<u8> {
    if a == 0 {
        return None;
    }
    Some(TABLES.exp[255 - usize::from(TABLES.log[usize::from(a)])])
}

/// The Cauchy coefficient C\[p\]\[j\] = 1 / (p ⊕ (128 + j)). `None` outside
/// p < 128, j < 128.
pub fn coefficient(p: u64, j: u64) -> Option<u8> {
    if p >= MAX_DATA_SHARDS || j >= MAX_DATA_SHARDS {
        return None;
    }
    // p < 128 ≤ 128 + j < 256: the XOR has bit 7 set, so it is never 0.
    gf_inv((p as u8) ^ ((MAX_DATA_SHARDS + j) as u8))
}

/// `out ^= c · data`, byte by byte.
fn mul_add(out: &mut [u8], c: u8, data: &[u8]) {
    if c == 0 {
        return;
    }
    let mut row = [0u8; 256];
    for (x, r) in row.iter_mut().enumerate() {
        *r = gf_mul(c, x as u8);
    }
    for (o, d) in out.iter_mut().zip(data) {
        *o ^= row[usize::from(*d)];
    }
}

/// Add data shard `j` (padded to the parity length by the caller, or shorter:
/// missing bytes are zero) into the `m` parity accumulators of its group.
pub fn accumulate(parity: &mut [Vec<u8>], j: u64, data: &[u8]) -> Result<()> {
    for (p, out) in parity.iter_mut().enumerate() {
        let c = coefficient(p as u64, j).ok_or(FormatError::Parity(ParityFault::Layout))?;
        mul_add(out, c, data);
    }
    Ok(())
}

/// Rebuild missing data shards of one group in place. `shards` holds the
/// k data shards then the m parity shards, each `Some` (intact, exactly
/// `shard_len` bytes) or `None` (missing or damaged). On success every data
/// shard is `Some`; parity shards are left as they were. Fewer than k
/// intact shards is [`ParityFault::BeyondCapacity`].
pub fn reconstruct(
    k: usize,
    m: usize,
    shards: &mut [Option<Vec<u8>>],
    shard_len: usize,
) -> Result<()> {
    if k == 0 || m == 0 || k > MAX_DATA_SHARDS as usize || m > k || shards.len() != k + m {
        return Err(FormatError::Parity(ParityFault::Layout));
    }
    if shards.iter().flatten().any(|s| s.len() != shard_len) {
        return Err(FormatError::Parity(ParityFault::Layout));
    }
    if shards[..k].iter().all(Option::is_some) {
        return Ok(());
    }
    // The first k intact shards, data before parity.
    let rows: Vec<usize> = (0..k + m)
        .filter(|i| shards[*i].is_some())
        .take(k)
        .collect();
    if rows.len() < k {
        return Err(FormatError::Parity(ParityFault::BeyondCapacity));
    }
    // Generator rows: identity for data shard i, Cauchy row for parity p.
    let mut a = vec![vec![0u8; k]; k];
    for (r, &row) in rows.iter().enumerate() {
        if row < k {
            a[r][row] = 1;
        } else {
            for (j, cell) in a[r].iter_mut().enumerate() {
                *cell = coefficient((row - k) as u64, j as u64)
                    .ok_or(FormatError::Parity(ParityFault::Layout))?;
            }
        }
    }
    let inv = invert(a)?;
    let missing: Vec<usize> = (0..k).filter(|i| shards[*i].is_none()).collect();
    let mut rebuilt = Vec::with_capacity(missing.len());
    for &d in &missing {
        let mut out = vec![0u8; shard_len];
        for (r, &row) in rows.iter().enumerate() {
            if let Some(src) = &shards[row] {
                mul_add(&mut out, inv[d][r], src);
            }
        }
        rebuilt.push(out);
    }
    for (d, bytes) in missing.into_iter().zip(rebuilt) {
        shards[d] = Some(bytes);
    }
    Ok(())
}

/// Gauss–Jordan inversion over GF(2^8).
fn invert(mut a: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>> {
    let n = a.len();
    let mut inv: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut r = vec![0u8; n];
            r[i] = 1;
            r
        })
        .collect();
    for col in 0..n {
        let pivot = (col..n)
            .find(|r| a[*r][col] != 0)
            .ok_or(FormatError::Parity(ParityFault::Layout))?;
        a.swap(col, pivot);
        inv.swap(col, pivot);
        let f = gf_inv(a[col][col]).ok_or(FormatError::Parity(ParityFault::Layout))?;
        for x in 0..n {
            a[col][x] = gf_mul(a[col][x], f);
            inv[col][x] = gf_mul(inv[col][x], f);
        }
        for r in 0..n {
            let c = a[r][col];
            if r != col && c != 0 {
                for x in 0..n {
                    let (ac, ic) = (a[col][x], inv[col][x]);
                    a[r][x] ^= gf_mul(c, ac);
                    inv[r][x] ^= gf_mul(c, ic);
                }
            }
        }
    }
    Ok(inv)
}

// ---- layout ----------------------------------------------------------------

/// One commit's group set, as the layout rule derives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub range_offset: u64,
    pub range_len: u64,
    pub percent: u16,
    pub shard_size: u64,
    pub groups: u64,
    pub data_shards: u64,
    pub parity_shards: u64,
}

fn round_up(x: u64, to: u64) -> Option<u64> {
    x.checked_add(to - 1).map(|v| v / to * to)
}

fn div_ceil(a: u64, b: u64) -> u64 {
    a / b + u64::from(!a.is_multiple_of(b))
}

impl Layout {
    /// The layout rule (module docs). Refuses an empty range, a percentage
    /// outside 1–100, overflow, and a group count that does not fit the
    /// frame's 32-bit field.
    pub fn compute(range_offset: u64, range_len: u64, percent: u16) -> Result<Layout> {
        let bad = || FormatError::Parity(ParityFault::Layout);
        if range_len == 0 || !(MIN_PERCENT..=MAX_PERCENT).contains(&percent) {
            return Err(bad());
        }
        range_offset.checked_add(range_len).ok_or_else(bad)?;
        let target = round_up(div_ceil(range_len, TARGET_SHARDS), SHARD_ALIGN).ok_or_else(bad)?;
        let small = round_up(range_len, SHARD_ALIGN)
            .ok_or_else(bad)?
            .min(MIN_SHARD);
        let shard_size = target.max(small).min(MAX_SHARD);
        let n = div_ceil(range_len, shard_size);
        let groups = div_ceil(n, MAX_DATA_SHARDS);
        if groups > u64::from(u32::MAX) {
            return Err(bad());
        }
        let data_shards = div_ceil(n, groups);
        let parity_shards = div_ceil(data_shards * u64::from(percent), 100);
        Ok(Layout {
            range_offset,
            range_len,
            percent,
            shard_size,
            groups,
            data_shards,
            parity_shards,
        })
    }

    /// Real (stored) data shards, *N*.
    pub fn real_shards(&self) -> u64 {
        div_ceil(self.range_len, self.shard_size)
    }

    /// Parity frames of the commit, *G*·*m*.
    pub fn frame_count(&self) -> u64 {
        self.groups * self.parity_shards
    }

    /// The archive-wide index of data shard `j` of group `g`.
    pub fn shard_index(&self, g: u64, j: u64) -> u64 {
        j * self.groups + g
    }

    /// The stored bytes of shard `i`: (absolute offset, length). Length is
    /// below the shard size only for the last real shard, and 0 for a
    /// virtual one.
    pub fn shard_extent(&self, i: u64) -> (u64, u64) {
        let start = i.saturating_mul(self.shard_size);
        if start >= self.range_len {
            return (self.range_offset + self.range_len, 0);
        }
        let len = (self.range_len - start).min(self.shard_size);
        (self.range_offset + start, len)
    }

    /// Parity payload length after the binary envelope.
    pub fn payload_len(&self) -> u64 {
        PAYLOAD_FIXED_LEN as u64 + DIGEST_LEN as u64 * self.data_shards + self.shard_size + 32
    }

    /// Length of every parity frame of the commit, for an envelope listing
    /// `features` required features.
    pub fn frame_len(&self, features: usize) -> u64 {
        SKIPPABLE_HEADER_LEN as u64 + 80 + 8 * features as u64 + self.payload_len()
    }
}

// ---- frames ----------------------------------------------------------------

/// Encode parity frame (`group`, `parity_index`) of the commit `info`
/// describes. `hashes` is the group's data-shard hash table (k entries).
pub fn encode_frame(
    info: &ParityFrameInfo,
    known_features: &[u64],
    group: u64,
    parity_index: u64,
    hashes: &[[u8; DIGEST_LEN]],
    shard: &[u8],
    writer_limits: &Limits,
) -> Result<Vec<u8>> {
    let bad = || FormatError::Parity(ParityFault::Layout);
    let layout = &info.layout;
    let identity = RecordIdentity {
        archive_id: info.archive_id,
        commit_sequence: info.seq,
        transaction_id: info.transaction_id,
    };
    let required_features = &info.required_features;
    if Layout::compute(layout.range_offset, layout.range_len, layout.percent)? != *layout
        || group >= layout.groups
        || parity_index >= layout.parity_shards
        || hashes.len() as u64 != layout.data_shards
        || shard.len() as u64 != layout.shard_size
    {
        return Err(bad());
    }
    let mut payload = Vec::with_capacity(usize::try_from(layout.payload_len()).map_err(|_| bad())?);
    payload.extend_from_slice(&registry::PARITY_SIGNATURE);
    payload.extend_from_slice(&SUITE_CAUCHY_GF256.to_le_bytes());
    payload.extend_from_slice(&layout.percent.to_le_bytes());
    payload.extend_from_slice(&(layout.shard_size as u32).to_le_bytes());
    payload.extend_from_slice(&layout.range_offset.to_le_bytes());
    payload.extend_from_slice(&layout.range_len.to_le_bytes());
    payload.extend_from_slice(&(layout.groups as u32).to_le_bytes());
    payload.extend_from_slice(&(layout.data_shards as u16).to_le_bytes());
    payload.extend_from_slice(&(layout.parity_shards as u16).to_le_bytes());
    payload.extend_from_slice(&(group as u32).to_le_bytes());
    payload.extend_from_slice(&(parity_index as u32).to_le_bytes());
    for h in hashes {
        payload.extend_from_slice(h);
    }
    payload.extend_from_slice(shard);
    payload.extend_from_slice(&[0u8; DIGEST_LEN]);
    let envelope = BinaryEnvelope {
        record_schema_version: RECORD_SCHEMA_VERSION,
        encoding: PayloadEncoding::ParityShard,
        identity,
        required_features: required_features.to_vec(),
    };
    let mut frame = encode_binary_record(
        FrameKind::ParityObject,
        &envelope,
        &payload,
        known_features,
        writer_limits,
    )?;
    let at = frame.len() - DIGEST_LEN;
    let digest = parity_frame_digest(StoredObjectBytes::new(&frame[..at]));
    frame[at..].copy_from_slice(digest.as_bytes());
    Ok(frame)
}

/// A decoded parity frame. Its identity is unchecked (bind it to the commit
/// whose footer follows it), and its layout is checked only against itself.
#[derive(Debug)]
pub struct ParityFrame<'a> {
    pub envelope: BinaryEnvelope,
    pub layout: Layout,
    pub group: u64,
    pub parity_index: u64,
    hashes: &'a [u8],
    pub shard: &'a [u8],
}

impl ParityFrame<'_> {
    /// Hash of data shard `j` of this frame's group.
    pub fn shard_hash(&self, j: u64) -> Option<[u8; DIGEST_LEN]> {
        let at = usize::try_from(j).ok()?.checked_mul(DIGEST_LEN)?;
        let mut h = [0u8; DIGEST_LEN];
        h.copy_from_slice(self.hashes.get(at..at + DIGEST_LEN)?);
        Some(h)
    }

    /// The whole hash table, for comparing frames of one group.
    pub fn hash_table(&self) -> &[u8] {
        self.hashes
    }
}

/// Decode one complete parity frame (`frame` is exactly its stored bytes).
///
/// Order: the frame walks as one skippable frame of kind `ParityObject` filling
/// `frame`; the **frame digest** (before any field is trusted); the binary
/// envelope (D11 obligations, `known_features`); the signature; the suite
/// (unknown: `Unsupported`); the layout fields equal [`Layout::compute`] of
/// the stated range and percentage; group and parity index in range; the
/// payload length.
pub fn decode_frame<'a>(
    frame: &'a [u8],
    known_features: &[u64],
    limits: &Limits,
) -> Result<ParityFrame<'a>> {
    let span = walk_frame(frame, 0, limits)?;
    let payload_len = match span.detail {
        FrameDetail::Skippable { payload_len }
            if span.kind == FrameKind::ParityObject && span.len == frame.len() as u64 =>
        {
            payload_len
        }
        _ => return Err(FormatError::Parity(ParityFault::NotAParityFrame)),
    };
    let at = frame
        .len()
        .checked_sub(DIGEST_LEN)
        .filter(|at| *at >= SKIPPABLE_HEADER_LEN)
        .ok_or(FormatError::Parity(ParityFault::TooShort))?;
    let digest = parity_frame_digest(StoredObjectBytes::new(&frame[..at]));
    if digest.as_bytes()[..] != frame[at..] {
        return Err(FormatError::Parity(ParityFault::DigestMismatch));
    }
    let candidate = decode_binary_record(
        registry::PARITY_OBJECT,
        &frame[SKIPPABLE_HEADER_LEN..SKIPPABLE_HEADER_LEN + payload_len as usize],
        &EnvelopeRules {
            kind: FrameKind::ParityObject,
            schema_versions: &[RECORD_SCHEMA_VERSION],
            known_features,
        },
        limits,
    )?;
    let envelope = candidate.unbound_envelope().clone();
    // The candidate's payload is the parity payload; its identity is the
    // caller's to bind, so the payload is located by length from the end.
    let body_len = frame.len() - SKIPPABLE_HEADER_LEN - (80 + 8 * envelope.required_features.len());
    let body = &frame[frame.len() - body_len..];
    let fixed = body
        .get(..PAYLOAD_FIXED_LEN)
        .ok_or(FormatError::Parity(ParityFault::TooShort))?;
    let u16_at = |i: usize| u16::from_le_bytes([fixed[i], fixed[i + 1]]);
    let u32_at =
        |i: usize| u32::from_le_bytes([fixed[i], fixed[i + 1], fixed[i + 2], fixed[i + 3]]);
    let u64_at = |i: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&fixed[i..i + 8]);
        u64::from_le_bytes(b)
    };
    if fixed[..16] != registry::PARITY_SIGNATURE {
        return Err(FormatError::Envelope(
            crate::error::EnvelopeFault::EncodingSignatureMismatch,
        ));
    }
    let suite = u16_at(16);
    if suite != SUITE_CAUCHY_GF256 {
        return Err(FormatError::Parity(ParityFault::UnknownSuite { suite }));
    }
    let layout = Layout::compute(u64_at(24), u64_at(32), u16_at(18))?;
    let stated = (
        u64::from(u32_at(20)),
        u64::from(u32_at(40)),
        u64::from(u16_at(44)),
        u64::from(u16_at(46)),
    );
    if stated
        != (
            layout.shard_size,
            layout.groups,
            layout.data_shards,
            layout.parity_shards,
        )
    {
        return Err(FormatError::Parity(ParityFault::Layout));
    }
    let group = u64::from(u32_at(48));
    let parity_index = u64::from(u32_at(52));
    if group >= layout.groups || parity_index >= layout.parity_shards {
        return Err(FormatError::Parity(ParityFault::Layout));
    }
    if body.len() as u64 != layout.payload_len() {
        return Err(FormatError::Parity(ParityFault::Layout));
    }
    let table_end = PAYLOAD_FIXED_LEN + DIGEST_LEN * layout.data_shards as usize;
    let shard_end = table_end + layout.shard_size as usize;
    Ok(ParityFrame {
        envelope,
        layout,
        group,
        parity_index,
        hashes: &body[PAYLOAD_FIXED_LEN..table_end],
        shard: &body[table_end..shard_end],
    })
}

/// Everything every frame of one commit must share: the layout and the
/// envelope (identity and required features).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityFrameInfo {
    pub layout: Layout,
    pub archive_id: [u8; 32],
    pub seq: u64,
    pub transaction_id: [u8; 16],
    pub required_features: Vec<u64>,
}

impl ParityFrameInfo {
    pub fn of(f: &ParityFrame<'_>) -> Self {
        ParityFrameInfo {
            layout: f.layout,
            archive_id: f.envelope.identity.archive_id,
            seq: f.envelope.identity.commit_sequence,
            transaction_id: f.envelope.identity.transaction_id,
            required_features: f.envelope.required_features.clone(),
        }
    }
}

/// The hash of data shard bytes `stored` (unpadded) under `layout`.
pub fn data_shard_hash(stored: &[u8], layout: &Layout) -> [u8; DIGEST_LEN] {
    *parity_shard_hash(StoredObjectBytes::new(stored), layout.shard_size).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_tables_are_a_field() {
        // The generator 2 has order 255 under 0x11D.
        let mut seen = [false; 256];
        for i in 0..255 {
            let v = TABLES.exp[i];
            assert!(!seen[usize::from(v)], "repeat at {i}");
            seen[usize::from(v)] = true;
        }
        assert!(!seen[0]);
        for a in 1..=255u8 {
            let inv = gf_inv(a).unwrap();
            assert_eq!(gf_mul(a, inv), 1, "{a}");
        }
        assert_eq!(gf_inv(0), None);
        // Distributivity spot check.
        for (a, b, c) in [(3u8, 7u8, 200u8), (0x53, 0xCA, 0x01), (255, 254, 253)] {
            assert_eq!(gf_mul(a, b ^ c), gf_mul(a, b) ^ gf_mul(a, c));
        }
        // 0x11D: x^8 = x^4 + x^3 + x^2 + 1.
        assert_eq!(gf_mul(0x80, 2), 0x1D);
    }

    #[test]
    fn no_coefficient_is_zero_or_one() {
        for p in 0..MAX_DATA_SHARDS {
            for j in 0..MAX_DATA_SHARDS {
                let c = coefficient(p, j).unwrap();
                assert!(c > 1, "C[{p}][{j}] = {c}");
            }
        }
        assert_eq!(coefficient(128, 0), None);
        assert_eq!(coefficient(0, 128), None);
    }

    fn encode(k: usize, m: usize, data: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let len = data[0].len();
        let mut parity = vec![vec![0u8; len]; m];
        for (j, d) in data.iter().enumerate().take(k) {
            accumulate(&mut parity, j as u64, d).unwrap();
        }
        parity
    }

    fn pseudo(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Every erasure pattern of up to m shards, for every small (k, m).
    #[test]
    fn any_k_of_k_plus_m_rebuild_the_group() {
        for k in 1..=6usize {
            for m in 1..=k {
                let data: Vec<Vec<u8>> = (0..k).map(|i| pseudo((k * 31 + i) as u64, 17)).collect();
                let parity = encode(k, m, &data);
                let all: Vec<Vec<u8>> = data.iter().chain(&parity).cloned().collect();
                for mask in 0u32..(1 << (k + m)) {
                    let lost = mask.count_ones() as usize;
                    let mut shards: Vec<Option<Vec<u8>>> = all
                        .iter()
                        .enumerate()
                        .map(|(i, s)| (mask & (1 << i) == 0).then(|| s.clone()))
                        .collect();
                    let r = reconstruct(k, m, &mut shards, 17);
                    if lost <= m {
                        r.unwrap();
                        for i in 0..k {
                            assert_eq!(shards[i].as_ref(), Some(&data[i]), "k={k} m={m} {mask:b}");
                        }
                    } else if (0..k).any(|i| mask & (1 << i) != 0) {
                        assert_eq!(
                            r,
                            Err(FormatError::Parity(ParityFault::BeyondCapacity)),
                            "k={k} m={m} {mask:b}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn largest_group_rebuilds_from_parity_alone() {
        let (k, m) = (128usize, 128usize);
        let data: Vec<Vec<u8>> = (0..k).map(|i| pseudo(i as u64 + 1, 8)).collect();
        let parity = encode(k, m, &data);
        let mut shards: Vec<Option<Vec<u8>>> = std::iter::repeat_n(None, k)
            .chain(parity.into_iter().map(Some))
            .collect();
        reconstruct(k, m, &mut shards, 8).unwrap();
        for i in 0..k {
            assert_eq!(shards[i].as_ref(), Some(&data[i]));
        }
    }

    #[test]
    fn layout_rule_examples() {
        // (L, r) -> (s, G, k, m)
        let cases = [
            (1u64, 10u16, (64u64, 1u64, 1u64, 1u64)),
            (300, 10, (320, 1, 1, 1)),
            (16 << 10, 10, (16 << 10, 1, 1, 1)),
            ((16 << 10) + 1, 10, (16 << 10, 1, 2, 1)),
            (100_000, 10, (16 << 10, 1, 7, 1)),
            (1 << 20, 10, (16 << 10, 1, 64, 7)),
            (16 << 20, 10, (16 << 10, 8, 128, 13)),
            (1 << 30, 10, (1 << 20, 8, 128, 13)),
            (1 << 30, 100, (1 << 20, 8, 128, 128)),
            (1 << 30, 1, (1 << 20, 8, 128, 2)),
            ((1 << 30) + 1, 5, (1 << 20, 9, 114, 6)),
        ];
        for (len, r, (s, g, k, m)) in cases {
            let l = Layout::compute(7, len, r).unwrap();
            assert_eq!(
                (l.shard_size, l.groups, l.data_shards, l.parity_shards),
                (s, g, k, m),
                "L={len} r={r}"
            );
            assert!(l.groups * l.data_shards >= l.real_shards());
            assert!(l.parity_shards <= l.data_shards);
        }
        assert!(Layout::compute(0, 0, 10).is_err());
        assert!(Layout::compute(0, 1, 0).is_err());
        assert!(Layout::compute(0, 1, 101).is_err());
        assert!(Layout::compute(u64::MAX, 1, 10).is_err());
    }

    #[test]
    fn shard_extents_tile_the_range() {
        let l = Layout::compute(1000, 100_000, 10).unwrap();
        let mut at = 1000;
        for i in 0..l.groups * l.data_shards {
            let (off, len) = l.shard_extent(i);
            if i < l.real_shards() {
                assert_eq!(off, at);
                at += len;
            } else {
                assert_eq!(len, 0);
            }
        }
        assert_eq!(at, 1000 + 100_000);
    }

    fn identity() -> RecordIdentity {
        RecordIdentity {
            archive_id: [7; 32],
            commit_sequence: 3,
            transaction_id: [9; 16],
        }
    }

    fn info(l: &Layout) -> ParityFrameInfo {
        ParityFrameInfo {
            layout: *l,
            archive_id: [7; 32],
            seq: 3,
            transaction_id: [9; 16],
            required_features: Vec::new(),
        }
    }

    #[test]
    fn frame_round_trip_and_digest() {
        let l = Layout::compute(0, 1000, 50).unwrap();
        let hashes = vec![[1u8; 32]; l.data_shards as usize];
        let shard = pseudo(5, l.shard_size as usize);
        let f = encode_frame(
            &info(&l),
            &[],
            0,
            0,
            &hashes,
            &shard,
            &Limits::WRITER_DEFAULT,
        )
        .unwrap();
        assert_eq!(f.len() as u64, l.frame_len(0));
        let d = decode_frame(&f, &[], &Limits::default()).unwrap();
        assert_eq!(d.layout, l);
        assert_eq!(d.shard, &shard[..]);
        assert_eq!(d.shard_hash(0), Some([1u8; 32]));
        assert_eq!(d.envelope.identity, identity());
        // Any flipped byte fails the digest (or the walk, for the header).
        for i in 0..f.len() {
            let mut g = f.clone();
            g[i] ^= 0x40;
            assert!(decode_frame(&g, &[], &Limits::default()).is_err(), "{i}");
        }
    }

    #[test]
    fn frame_with_inconsistent_layout_is_refused_even_with_a_good_digest() {
        let l = Layout::compute(0, 1000, 50).unwrap();
        let hashes = vec![[1u8; 32]; l.data_shards as usize];
        let shard = vec![0u8; l.shard_size as usize];
        let mut f = encode_frame(
            &info(&l),
            &[],
            0,
            0,
            &hashes,
            &shard,
            &Limits::WRITER_DEFAULT,
        )
        .unwrap();
        // Shard size field (payload offset 20) changed, digest recomputed.
        let at = SKIPPABLE_HEADER_LEN + 80 + 20;
        f[at] ^= 0x40;
        let end = f.len() - 32;
        let d = parity_frame_digest(StoredObjectBytes::new(&f[..end]));
        f[end..].copy_from_slice(d.as_bytes());
        assert_eq!(
            decode_frame(&f, &[], &Limits::default()).unwrap_err(),
            FormatError::Parity(ParityFault::Layout)
        );
    }
}
