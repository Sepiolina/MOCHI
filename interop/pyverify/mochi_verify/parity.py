"""Redundancy profile: Cauchy Reed-Solomon over GF(2^8) (spec B.2.11 D21, R6).

Written from R6's text and spec B.2.11 only.
"""
from __future__ import annotations

from dataclasses import dataclass, field

from . import digest, frames
from .envelope import PARITY_SIGNATURE, decode_binary
from .errors import MochiError

POLY = 0x11D
MAX_SHARD = 1 << 20
MIN_SHARD = 16384
SHARD_ALIGN = 64
TARGET_SHARDS = 1024
MAX_DATA_SHARDS = 128
MAX_DECODE_ATTEMPTS = 4096


# ---------------------------------------------------------------- field
def _gf_mul_slow(a: int, b: int) -> int:
    r = 0
    while b:
        if b & 1:
            r ^= a
        a <<= 1
        if a & 0x100:
            a ^= POLY
        b >>= 1
    return r


_EXP = [0] * 512
_LOG = [0] * 256
_x = 1
for _i in range(255):
    _EXP[_i] = _x
    _LOG[_x] = _i
    _x = _gf_mul_slow(_x, 2)
for _i in range(255, 512):
    _EXP[_i] = _EXP[_i - 255]
assert _x == 1  # 2 generates the multiplicative group


def gf_mul(a: int, b: int) -> int:
    if a == 0 or b == 0:
        return 0
    return _EXP[_LOG[a] + _LOG[b]]


def gf_inv(a: int) -> int:
    if a == 0:
        raise ZeroDivisionError
    return _EXP[255 - _LOG[a]]


def coefficient(p: int, j: int) -> int:
    """C[p][j] = 1 / (p xor (128 + j))."""
    return gf_inv(p ^ (128 + j))


_MUL_TABLES: dict[int, bytes] = {}


def _table(c: int) -> bytes:
    t = _MUL_TABLES.get(c)
    if t is None:
        t = bytes(gf_mul(c, x) for x in range(256))
        _MUL_TABLES[c] = t
    return t


def _scale(c: int, data: bytes) -> int:
    """c * data (bytewise), as a little-endian integer for fast XOR."""
    if c == 0:
        return 0
    return int.from_bytes(data.translate(_table(c)), "little")


# ---------------------------------------------------------------- layout
def _up64(x: int) -> int:
    return (x + 63) // 64 * 64


def _cdiv(a: int, b: int) -> int:
    return -(-a // b)


@dataclass(frozen=True)
class Layout:
    range_len: int
    percent: int
    shard_size: int
    real_shards: int
    groups: int
    data_shards: int       # k
    parity_shards: int     # m

    @staticmethod
    def compute(length: int, percent: int) -> "Layout":
        if length < 1 or not 1 <= percent <= 100:
            raise MochiError("PROFILE_VIOLATION", "layout inputs out of range")
        s = min(MAX_SHARD, max(_up64(_cdiv(length, TARGET_SHARDS)), min(MIN_SHARD, _up64(length))))
        n = _cdiv(length, s)
        g = _cdiv(n, MAX_DATA_SHARDS)
        if g >= 1 << 32:
            raise MochiError("PROFILE_VIOLATION", "group count does not fit 32 bits")
        k = _cdiv(n, g)
        m = _cdiv(k * percent, 100)
        return Layout(length, percent, s, n, g, k, m)

    def shard_of(self, group: int, index: int) -> int:
        """Global shard number of (group, index); >= real_shards means virtual."""
        return index * self.groups + group


def shard_bytes(buf, a: int, lay: Layout, i: int) -> bytes:
    """Stored bytes of data shard i, zero-padded to the shard size."""
    if i >= lay.real_shards:
        return bytes(lay.shard_size)
    lo = a + i * lay.shard_size
    hi = min(a + lay.range_len, lo + lay.shard_size)
    chunk = bytes(buf[lo:hi])
    return chunk + bytes(lay.shard_size - len(chunk))


def shard_hash(shard: bytes) -> bytes:
    return digest.h(digest.PARITY_SHARD, shard)


# ---------------------------------------------------------------- coding
def encode_parity(data: list[bytes], m: int) -> list[bytes]:
    """Parity shards p = sum_j C[p][j] * D_j."""
    size = len(data[0])
    out = []
    for p in range(m):
        acc = 0
        for j, d in enumerate(data):
            acc ^= _scale(coefficient(p, j), d)
        out.append(acc.to_bytes(size, "little"))
    return out


def _invert(matrix: list[list[int]]) -> list[list[int]]:
    n = len(matrix)
    a = [row[:] + [1 if i == j else 0 for j in range(n)] for i, row in enumerate(matrix)]
    for col in range(n):
        piv = next((r for r in range(col, n) if a[r][col]), None)
        if piv is None:
            raise MochiError("PARITY_CAPACITY_EXCEEDED", "singular system")
        a[col], a[piv] = a[piv], a[col]
        iv = gf_inv(a[col][col])
        a[col] = [gf_mul(iv, x) for x in a[col]]
        for r in range(n):
            if r != col and a[r][col]:
                f = a[r][col]
                a[r] = [x ^ gf_mul(f, y) for x, y in zip(a[r], a[col])]
    return [row[n:] for row in a]


def reconstruct(k: int, m: int, data: list[bytes | None], parity: list[bytes | None]) -> dict[int, bytes]:
    """Rebuild the missing data shards from any k intact shards (data before
    parity).  Returns {index: shard}.  Raises PARITY_CAPACITY_EXCEEDED."""
    missing = [j for j in range(k) if data[j] is None]
    if not missing:
        return {}
    chosen: list[tuple[str, int]] = [("d", j) for j in range(k) if data[j] is not None]
    chosen += [("p", p) for p in range(m) if parity[p] is not None]
    if len(chosen) < k:
        raise MochiError("PARITY_CAPACITY_EXCEEDED", "fewer than k intact shards")
    chosen = chosen[:k]
    rows = []
    for kind, idx in chosen:
        if kind == "d":
            rows.append([1 if c == idx else 0 for c in range(k)])
        else:
            rows.append([coefficient(idx, c) for c in range(k)])
    inv = _invert(rows)
    size = len((data[chosen[0][1]] if chosen[0][0] == "d" else parity[chosen[0][1]]))
    vecs = [data[i] if kind == "d" else parity[i] for kind, i in chosen]
    out = {}
    for j in missing:
        acc = 0
        for r, vec in enumerate(vecs):
            acc ^= _scale(inv[j][r], vec)
        out[j] = acc.to_bytes(size, "little")
    return out


# ---------------------------------------------------------------- frames
@dataclass
class ParityFrame:
    features: list
    archive_id: bytes
    sequence: int
    transaction_id: bytes
    suite: int
    percent: int
    shard_size: int
    range_offset: int
    range_len: int
    groups: int
    k: int
    m: int
    group: int
    index: int
    hashes: list
    shard: bytes
    length: int            # stored frame length

    @property
    def layout_key(self):
        return (self.suite, self.percent, self.shard_size, self.range_offset, self.range_len,
                self.groups, self.k, self.m)


class FrameFault(MochiError):
    """A parity frame that does not decode (``fault`` names the failed step)."""

    def __init__(self, fault: str, code: str, message: str = ""):
        super().__init__(code, message or fault)
        self.fault = fault


def decode_frame(data: bytes) -> ParityFrame:
    """R6 section 5 order: skippable parity frame filling the bytes; digest;
    binary envelope; suite; layout; indices; payload length."""
    try:
        fr = frames.walk_frame(data, 0)
    except MochiError as e:
        raise FrameFault("frame", e.code, e.message) from None
    if fr.length != len(data) or fr.magic != frames.K_PARITY:
        raise FrameFault("frame", "MALFORMED_FRAME", "not one parity frame filling the bytes")
    if len(data) < 40 or digest.h(digest.PARITY_FRAME, data[:-32]) != data[-32:]:
        raise FrameFault("digest", "STORED_INTEGRITY_FAILED", "frame digest mismatch")
    try:
        env = decode_binary(data, frames.K_PARITY, (0,))
    except MochiError as e:
        raise FrameFault("envelope", e.code, e.message) from None
    p = env.payload
    if len(p) < 56 + 32:
        raise FrameFault("payload", "ENVELOPE_INVALID", "parity payload too short")
    suite = int.from_bytes(p[16:18], "little")
    if suite != 1:
        raise FrameFault("unsupported", "UNSUPPORTED_FEATURE", f"suite {suite}")
    percent = int.from_bytes(p[18:20], "little")
    s = int.from_bytes(p[20:24], "little")
    a = int.from_bytes(p[24:32], "little")
    length = int.from_bytes(p[32:40], "little")
    groups = int.from_bytes(p[40:44], "little")
    k = int.from_bytes(p[44:46], "little")
    m = int.from_bytes(p[46:48], "little")
    gidx = int.from_bytes(p[48:52], "little")
    pidx = int.from_bytes(p[52:56], "little")
    try:
        lay = Layout.compute(length, percent)
    except MochiError as e:
        raise FrameFault("layout", e.code, e.message) from None
    if (lay.shard_size, lay.groups, lay.data_shards, lay.parity_shards) != (s, groups, k, m):
        raise FrameFault("layout", "PROFILE_VIOLATION", "stated layout differs from the layout rule")
    if gidx >= groups or pidx >= m:
        raise FrameFault("layout", "PROFILE_VIOLATION", "group or parity index out of range")
    if len(p) != 56 + 32 * k + s + 32:
        raise FrameFault("payload", "ENVELOPE_INVALID", "parity payload length")
    hashes = [bytes(p[56 + 32 * i:88 + 32 * i]) for i in range(k)]
    shard = bytes(p[56 + 32 * k:56 + 32 * k + s])
    return ParityFrame(env.features, env.archive_id, env.sequence, env.transaction_id, suite, percent, s, a,
                       length, groups, k, m, gidx, pidx, hashes, shard, len(data))


def encode_frame(features: list, archive_id: bytes, sequence: int, txid: bytes, lay: Layout, a: int,
                 group: int, pindex: int, hashes: list, shard: bytes) -> bytes:
    n = len(features)
    hlen = 80 + 8 * n
    body = (PARITY_SIGNATURE + (1).to_bytes(2, "little") + lay.percent.to_bytes(2, "little")
            + lay.shard_size.to_bytes(4, "little") + a.to_bytes(8, "little") + lay.range_len.to_bytes(8, "little")
            + lay.groups.to_bytes(4, "little") + lay.data_shards.to_bytes(2, "little")
            + lay.parity_shards.to_bytes(2, "little") + group.to_bytes(4, "little") + pindex.to_bytes(4, "little")
            + b"".join(hashes) + shard)
    plen = len(body) + 32
    header = (hlen.to_bytes(4, "little") + (0).to_bytes(2, "little") + (0).to_bytes(2, "little")
              + (1).to_bytes(4, "little") + n.to_bytes(4, "little") + plen.to_bytes(8, "little")
              + archive_id + sequence.to_bytes(8, "little") + txid
              + b"".join(f.to_bytes(8, "little") for f in features))
    payload = header + body + bytes(32)
    frame = frames.K_PARITY.to_bytes(4, "little") + len(payload).to_bytes(4, "little") + payload
    return frame[:-32] + digest.h(digest.PARITY_FRAME, frame[:-32])


def build_commit_frames(buf, a: int, length: int, percent: int, features: list, archive_id: bytes,
                        sequence: int, txid: bytes) -> list[bytes]:
    """The parity frames of a commit over buf[a:a+length] (group by group)."""
    lay = Layout.compute(length, percent)
    out = []
    for g in range(lay.groups):
        data = [shard_bytes(buf, a, lay, lay.shard_of(g, j)) for j in range(lay.data_shards)]
        hashes = [shard_hash(d) for d in data]
        for pi, par in enumerate(encode_parity(data, lay.parity_shards)):
            out.append(encode_frame(features, archive_id, sequence, txid, lay, a, g, pi, hashes, par))
    return out


# ---------------------------------------------------------------- scan
@dataclass
class ScanResult:
    commits: int = 0
    damaged_shards: int = 0
    rebuilt: int = 0
    beyond_capacity: int = 0
    findings: list = field(default_factory=list)       # (code, object-hex|None)
    notes: list = field(default_factory=list)
    patched: int = 0


def _try_decode(buf, pos: int, end: int):
    try:
        fr = frames.walk_frame(buf, pos, end)
    except MochiError:
        return None, None
    if fr.magic != frames.K_PARITY:
        return None, fr
    try:
        return decode_frame(bytes(buf[pos:fr.end])), fr
    except MochiError:
        return None, fr


def find_template(buf, start: int, end: int):
    """R6 section 6 step 2: a frame of the commit that decodes -> (frame, position)."""
    attempts = 0
    pos = start
    fr0 = None
    if pos + 8 <= end and frames.u32(buf, pos) == frames.K_PARITY:
        f, fr0 = _try_decode(buf, pos, end)
        attempts += 1
        if f:
            return f, pos
    if fr0 is not None:
        stride = fr0.length
        q = start + stride
        while q + 8 <= end and attempts < MAX_DECODE_ATTEMPTS:
            f, _ = _try_decode(buf, q, end)
            attempts += 1
            if f:
                return f, q
            q += stride
    magic = frames.K_PARITY.to_bytes(4, "little")
    q = bytes(buf[start:end]).find(magic)
    while q >= 0 and attempts < MAX_DECODE_ATTEMPTS:
        f, _ = _try_decode(buf, start + q, end)
        attempts += 1
        if f:
            return f, start + q
        q = bytes(buf[start:end]).find(magic, q + 1)
    return None, None


def heal_commit(healed: bytearray, commit, footer, parent_footer_offset: int, descriptor_percent: int | None,
                res: ScanResult) -> None:
    """One commit's parity region.  Mutates ``healed`` with rebuilt shards that
    hash to their table entry; reports into ``res``."""
    a = parent_footer_offset
    c_end = footer.commit_offset + footer.commit_length
    region_len = footer.offset - c_end
    res.commits += 1            # every commit the scan reaches counts, whatever its frames hold
    if region_len == 0:
        res.findings.append(("PROFILE_VIOLATION", None))
        res.notes.append(f"commit {commit.sequence}: Redundancy-profile commit without parity frames")
        return
    tmpl, tpos = find_template(healed, c_end, footer.offset)
    if tmpl is None:
        res.findings.append(("PARITY_DAMAGED", None))
        res.notes.append(f"commit {commit.sequence}: no parity frame decodes")
        return
    lay = Layout.compute(tmpl.range_len, tmpl.percent)
    flen = tmpl.length
    nframes = lay.groups * lay.parity_shards
    violation = False
    # the frames must fill exactly [commit frame end, footer)
    if nframes * flen != region_len or (tpos - c_end) % flen != 0:
        violation = True
    # identity / range cross-checks against the commit (D21 item 7)
    if (tmpl.archive_id != commit.archive_id or tmpl.sequence != commit.sequence
            or tmpl.transaction_id != commit.transaction_id or tmpl.features != commit.features
            or tmpl.range_offset != a or a + tmpl.range_len != c_end
            or (descriptor_percent is not None and tmpl.percent != descriptor_percent)):
        violation = True
    if violation:
        res.findings.append(("PROFILE_VIOLATION", None))
        res.notes.append(f"commit {commit.sequence}: parity frames verify but disagree with the commit")
        return
    frs: list[ParityFrame | None] = []
    for j in range(nframes):
        f, _ = _try_decode(healed, c_end + j * flen, c_end + (j + 1) * flen)
        frs.append(f)
    # frames of a commit agree with each other
    good = [f for f in frs if f is not None]
    for f in good:
        if f.layout_key != tmpl.layout_key or f.archive_id != tmpl.archive_id or f.sequence != tmpl.sequence \
                or f.transaction_id != tmpl.transaction_id or f.features != tmpl.features:
            res.findings.append(("PROFILE_VIOLATION", None))
            res.notes.append(f"commit {commit.sequence}: parity frames disagree with each other")
            return
    for j, f in enumerate(frs):
        if f is not None and (f.group, f.index) != (j // lay.parity_shards, j % lay.parity_shards):
            res.findings.append(("PROFILE_VIOLATION", None))
            res.notes.append(f"commit {commit.sequence}: parity frame out of its slot")
            return
    damaged_parity = sum(1 for f in frs if f is None)
    res.damaged_shards += damaged_parity
    if damaged_parity:
        res.findings.append(("PARITY_DAMAGED", None))
    patches = []
    rebuilt_here = 0
    for g in range(lay.groups):
        gframes = frs[g * lay.parity_shards:(g + 1) * lay.parity_shards]
        intact = [f for f in gframes if f is not None]
        if not intact:
            res.findings.append(("PARITY_DAMAGED", None))
            res.notes.append(f"commit {commit.sequence}: group {g} has no intact parity frame")
            continue
        table = intact[0].hashes
        if any(f.hashes != table for f in intact):
            res.findings.append(("PROFILE_VIOLATION", None))
            res.notes.append(f"commit {commit.sequence}: group {g} hash tables disagree")
            continue
        data: list[bytes | None] = []
        damaged_idx = []
        zero = bytes(lay.shard_size)
        for j in range(lay.data_shards):
            i = lay.shard_of(g, j)
            sb = shard_bytes(healed, a, lay, i)
            if i >= lay.real_shards:
                if table[j] != shard_hash(zero):
                    res.findings.append(("PROFILE_VIOLATION", None))
                    res.notes.append(f"commit {commit.sequence}: virtual shard hash is not the zero shard's")
                data.append(zero)
                continue
            if shard_hash(sb) == table[j]:
                data.append(sb)
            else:
                data.append(None)
                damaged_idx.append(j)
        res.damaged_shards += len(damaged_idx)
        if not damaged_idx:
            continue
        parity = [None] * lay.parity_shards
        for f in intact:
            parity[f.index] = f.shard
        if len(damaged_idx) > len(intact):
            res.beyond_capacity += 1
            res.findings.append(("PARITY_CAPACITY_EXCEEDED", None))
            res.notes.append(f"commit {commit.sequence}: group {g} damage exceeds its intact parity")
            continue
        try:
            rebuilt = reconstruct(lay.data_shards, lay.parity_shards, data, parity)
        except MochiError:
            res.beyond_capacity += 1
            res.findings.append(("PARITY_CAPACITY_EXCEEDED", None))
            continue
        for j, sb in rebuilt.items():
            if shard_hash(sb) != table[j]:
                res.beyond_capacity += 1
                res.findings.append(("PARITY_CAPACITY_EXCEEDED", None))
                res.notes.append(f"commit {commit.sequence}: rebuilt shard misses its table entry")
                continue
            i = lay.shard_of(g, j)
            lo = a + i * lay.shard_size
            hi = min(a + lay.range_len, lo + lay.shard_size)
            patches.append((lo, sb[:hi - lo]))
            rebuilt_here += 1
    for lo, b in patches:
        healed[lo:lo + len(b)] = b
    res.rebuilt += rebuilt_here
    res.patched += rebuilt_here
