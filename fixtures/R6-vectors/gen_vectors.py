#!/usr/bin/env python3
"""Generate the R6 test vectors (spec Annex B.2.11, D21) into vectors.json.

An independent implementation of the Redundancy profile's suite 1, written
from the text of R6 and B.2.11, sharing no code with the Rust crates: the
field is multiplied bit by bit (no tables), inverses are a^254, and the
frame is assembled from the layout table. Every input is deterministic, so
the output is byte-for-byte reproducible. The Rust test
`crates/mochi-testkit/tests/r6_vectors.rs` reads vectors.json and checks the
implementation against every value; CI regenerates the file and fails if
it differs.

Run:  pip install blake3==1.0.8
      python3 gen_vectors.py > vectors.json
"""
import json
import struct

import blake3

PARITY_OBJECT = 0x184D2A5F
SIGNATURE = b"MOCHI2-PARITY\0\0\0"
SHARD_DOMAIN = b"MOCHI2-PARITY-SHARD\0"
FRAME_DOMAIN = b"MOCHI2-PARITY-FRAME\0"
SUITE = 1
PAYLOAD_ENCODING_PARITY = 1
POLY = 0x11D

MAX_DATA_SHARDS = 128
MIN_SHARD = 16 * 1024
MAX_SHARD = 1 << 20
ALIGN = 64
TARGET = 1024


def h(b):
    return b.hex()


# --- GF(2^8), polynomial 0x11D -------------------------------------------------
def gmul(a, b):
    r = 0
    while b:
        if b & 1:
            r ^= a
        b >>= 1
        a <<= 1
        if a & 0x100:
            a ^= POLY
    return r


def ginv(a):
    assert a != 0
    r = 1
    for _ in range(254):  # a^254 = a^-1 in GF(2^8)*
        r = gmul(r, a)
    return r


def coeff(p, j):
    return ginv(p ^ (128 + j))


def encode(data, m):
    n = len(data[0])
    out = []
    for p in range(m):
        s = bytearray(n)
        for j, d in enumerate(data):
            c = coeff(p, j)
            for i in range(n):
                s[i] ^= gmul(c, d[i])
        out.append(bytes(s))
    return out


# --- layout rule ---------------------------------------------------------------
def ceil_div(a, b):
    return -(-a // b)


def round_up(x, to):
    return ceil_div(x, to) * to


def layout(offset, length, percent):
    target = round_up(ceil_div(length, TARGET), ALIGN)
    small = min(round_up(length, ALIGN), MIN_SHARD)
    s = min(max(target, small), MAX_SHARD)
    n = ceil_div(length, s)
    g = ceil_div(n, MAX_DATA_SHARDS)
    k = ceil_div(n, g)
    m = ceil_div(k * percent, 100)
    return dict(range_offset=offset, range_len=length, percent=percent,
                shard_size=s, real_shards=n, groups=g, data_shards=k,
                parity_shards=m)


def shard_bytes(rng, lay, i):
    """Stored bytes of shard i of the protected range `rng` (unpadded)."""
    s = lay["shard_size"]
    start = i * s
    if start >= len(rng):
        return b""
    return rng[start:start + s]


def shard_hash(stored, s):
    return blake3.blake3(SHARD_DOMAIN + stored + bytes(s - len(stored))).digest()


# --- frames --------------------------------------------------------------------
def frame(identity, features, lay, g, p, hashes, shard):
    archive_id, seq, txid = identity
    payload = SIGNATURE + struct.pack(
        "<HHIQQIHHII", SUITE, lay["percent"], lay["shard_size"],
        lay["range_offset"], lay["range_len"], lay["groups"],
        lay["data_shards"], lay["parity_shards"], g, p)
    payload += b"".join(hashes) + shard + bytes(32)
    header_len = 80 + 8 * len(features)
    env = struct.pack("<IHHIIQ", header_len, 0, 0, PAYLOAD_ENCODING_PARITY,
                      len(features), len(payload))
    env += archive_id + struct.pack("<Q", seq) + txid
    env += b"".join(struct.pack("<Q", f) for f in features)
    body = env + payload
    f = struct.pack("<II", PARITY_OBJECT, len(body)) + body
    digest = blake3.blake3(FRAME_DOMAIN + f[:-32]).digest()
    return f[:-32] + digest


def parity_frames(rng, offset, percent, identity, features):
    lay = layout(offset, len(rng), percent)
    G, k, m, s = (lay["groups"], lay["data_shards"], lay["parity_shards"],
                  lay["shard_size"])
    frames = []
    for g in range(G):
        data = []
        hashes = []
        for j in range(k):
            stored = shard_bytes(rng, lay, j * G + g)
            hashes.append(shard_hash(stored, s))
            data.append(stored + bytes(s - len(stored)))
        for p, shard in enumerate(encode(data, m)):
            frames.append(frame(identity, features, lay, g, p, hashes, shard))
    return lay, frames


def pattern(n, seed):
    """Deterministic test bytes: x(i) = (i * 167 + seed * 13 + (i >> 8)) mod 256."""
    return bytes((i * 167 + seed * 13 + (i >> 8)) & 0xFF for i in range(n))


def main():
    out = {"suite": SUITE, "field_polynomial": POLY}

    out["field"] = {
        "products": [[a, b, gmul(a, b)] for a, b in
                     [(2, 0x80), (3, 7), (0x53, 0xCA), (0xFF, 0xFF), (1, 0x9D), (0, 5)]],
        "inverses": [[a, ginv(a)] for a in [1, 2, 3, 0x53, 0x80, 0xFF]],
    }
    out["coefficients"] = {
        "rows": 4, "cols": 8,
        "values": [[coeff(p, j) for j in range(8)] for p in range(4)],
        "corner_127_127": coeff(127, 127),
    }

    layouts = []
    for off, length, r in [(0, 1, 10), (0, 300, 10), (0, 16384, 10),
                           (0, 16385, 10), (0, 100000, 10), (5, 1 << 20, 10),
                           (0, 16 << 20, 10), (0, 1 << 30, 10), (0, 1 << 30, 100),
                           (0, 1 << 30, 1), (0, (1 << 30) + 1, 5),
                           (77, 3 * (1 << 30) + 12345, 7)]:
        layouts.append(layout(off, length, r))
    out["layouts"] = layouts

    codes = []
    for k, m, n, seed in [(1, 1, 8, 1), (3, 2, 8, 2), (5, 3, 16, 3), (4, 4, 5, 4)]:
        data = [pattern(n, seed * 10 + j) for j in range(k)]
        codes.append({"k": k, "m": m, "data": [h(d) for d in data],
                      "parity": [h(p) for p in encode(data, m)]})
    out["codes"] = codes

    out["shard_hashes"] = [
        {"stored": h(pattern(n, 9)), "shard_size": s, "hash": h(shard_hash(pattern(n, 9), s))}
        for n, s in [(0, 64), (10, 64), (64, 64), (100, 128)]
    ]

    identity = (bytes(range(32)), 3, bytes(range(100, 116)))
    small_range = pattern(100, 5)
    lay, frames = parity_frames(small_range, 4096, 50, identity, [])
    out["small_commit"] = {
        "archive_id": h(identity[0]), "seq": identity[1], "transaction_id": h(identity[2]),
        "required_features": [], "range_offset": 4096, "range": h(small_range),
        "percent": 50, "layout": lay, "frames": [h(f) for f in frames],
    }
    big_range = pattern(40000, 6)
    lay, frames = parity_frames(big_range, 0, 60, identity, [1])
    out["multi_shard_commit"] = {
        "archive_id": h(identity[0]), "seq": identity[1], "transaction_id": h(identity[2]),
        "required_features": [1], "range_offset": 0, "range_len": len(big_range),
        "range_seed": 6, "percent": 60, "layout": lay,
        "frame_len": len(frames[0]),
        "frame_blake3": [h(blake3.blake3(f).digest()) for f in frames],
    }

    # Rejects, from the small commit's frame: each must be refused.
    f = bytearray(bytes.fromhex(out["small_commit"]["frames"][0]))
    rejects = []
    bad = bytearray(f)
    bad[200] ^= 1
    rejects.append({"name": "flipped shard byte", "fault": "digest", "frame": h(bad)})

    def redigest(b):
        b[-32:] = blake3.blake3(FRAME_DOMAIN + bytes(b[:-32])).digest()
        return b

    pay = 8 + 80  # parity payload offset in the frame (no features)
    bad = bytearray(f)
    struct.pack_into("<H", bad, pay + 16, 2)
    rejects.append({"name": "unknown suite", "fault": "unsupported", "frame": h(redigest(bad))})
    bad = bytearray(f)
    struct.pack_into("<I", bad, pay + 20, 64)
    rejects.append({"name": "shard size not the layout rule's", "fault": "layout",
                    "frame": h(redigest(bad))})
    bad = bytearray(f)
    struct.pack_into("<I", bad, pay + 52, 1)
    rejects.append({"name": "parity index out of range", "fault": "layout",
                    "frame": h(redigest(bad))})
    bad = bytearray(f)
    struct.pack_into("<H", bad, pay + 18, 0)
    rejects.append({"name": "percentage 0", "fault": "layout", "frame": h(redigest(bad))})
    bad = bytearray(f)
    bad[pay] ^= 0x20
    rejects.append({"name": "signature", "fault": "envelope", "frame": h(redigest(bad))})
    bad = bytearray(f)
    struct.pack_into("<I", bad, 8 + 8, 0)
    rejects.append({"name": "payload encoding 0 in a parity frame", "fault": "envelope",
                    "frame": h(redigest(bad))})
    out["rejects"] = rejects

    print(json.dumps(out, indent=1, sort_keys=True))


if __name__ == "__main__":
    main()
