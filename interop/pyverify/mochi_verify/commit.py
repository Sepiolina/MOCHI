"""Commit record v1/v2 (spec B.2.2, R3 sections 2-4, 6, 7; commit-record-v{1,2}.cddl)."""
from __future__ import annotations

from dataclasses import dataclass, field

from . import cbor, digest, frames
from .errors import MochiError
from .schema import Schema, check_features

MAX_KEY_ENVELOPES = 16


@dataclass(frozen=True)
class ObjectRef:
    offset: int
    length: int
    hash: bytes

    @property
    def end(self) -> int:
        return self.offset + self.length

    @property
    def hash_hex(self) -> str:
        return self.hash.hex()


@dataclass
class Commit:
    schema: int
    archive_id: bytes
    sequence: int
    transaction_id: bytes
    parent: tuple | None              # (commit_id, sequence, footer_offset)
    checkpoint: bool
    image: ObjectRef | None
    snapshot: ObjectRef | None
    base: tuple | None                # (commit_id, sequence, footer_hint)
    delta: ObjectRef
    features: list
    time: tuple | None
    commit_id: bytes
    descriptor: ObjectRef
    key_envelopes: list = field(default_factory=list)
    data_region: ObjectRef | None = None
    raw: dict = field(default_factory=dict)

    @property
    def encrypted(self) -> bool:
        return self.schema == 2


def _ref(S: Schema, v, name: str) -> ObjectRef:
    S.map(v, name, (0, 1, 2))
    return ObjectRef(S.uint(v[0], name + ".offset"), S.uint(v[1], name + ".length", minimum=8),
                     S.bstr(v[2], name + ".hash", 32))


def compute_commit_id(body: dict) -> bytes:
    b = {k: v for k, v in body.items() if k != 9}
    return digest.h(digest.COMMIT_ID, cbor.encode(b))


def decode_object(data: bytes) -> Commit:
    """``data`` is one complete stored commit frame."""
    try:
        fr = frames.walk_frame(data, 0)
    except MochiError as e:
        raise MochiError(e.code if e.code in ("TRUNCATED", "LIMIT_EXCEEDED") else "MALFORMED_FRAME", e.message) from None
    if fr.length != len(data):
        raise MochiError("MALFORMED_FRAME", "bytes after the commit frame")
    if fr.magic != frames.K_COMMIT:
        raise MochiError("MALFORMED_FRAME", "not a commit frame")
    return decode_payload(bytes(data[8:]))


def decode_payload(payload: bytes) -> Commit:
    S = Schema("RECORD_INVALID", "commit")
    v = cbor.decode(payload)
    if not isinstance(v, dict) or 0 not in v:
        S.fail("not a map with a schema version")
    ver = v[0]
    if not S.is_uint(ver):
        S.fail("schema version")
    if ver not in (1, 2):
        raise MochiError("UNSUPPORTED_FEATURE", f"commit schema version {ver}" + (" (legacy)" if ver == 0 else ""))
    if ver == 1:
        S.map(v, "commit", (0, 1, 2, 3, 4, 5, 6, 7, 9, 10), (8,))
    else:
        S.map(v, "commit", (0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 12))
    aid = S.bstr(v[1], "archive id", 32)
    seq = S.uint(v[2], "sequence")
    txid = S.bstr(v[3], "transaction id", 16)
    parent = None
    if v[4] is not None:
        p = S.map(v[4], "parent", (0, 1, 2))
        parent = (S.bstr(p[0], "parent id", 32), S.uint(p[1], "parent sequence"), S.uint(p[2], "parent footer offset"))
    meta = v[5]
    if not isinstance(meta, dict) or 0 not in meta or meta[0] not in (0, 1) or isinstance(meta[0], bool):
        S.fail("metadata form")
    image = snapshot = base = None
    if meta[0] == 0:
        S.map(meta, "checkpoint", (0, 1, 2))
        image = _ref(S, meta[1], "image")
        snapshot = _ref(S, meta[2], "snapshot manifest")
    else:
        S.map(meta, "delta form", (0, 1))
        b = S.map(meta[1], "base", (0, 1, 2))
        base = (S.bstr(b[0], "base id", 32), S.uint(b[1], "base sequence"), S.uint(b[2], "base footer hint"))
    delta = _ref(S, v[6], "delta manifest")
    # features: D11 order, then agreement with the schema
    feats = check_features(v[7], {1}, S)
    t = None
    if 8 in v:
        tt = S.array(v[8], "time", 2, 2)
        t = (S.sint(tt[0], "seconds"), S.uint(tt[1], "nanoseconds", maximum=999_999_999))
        if not -(1 << 63) <= t[0] < (1 << 63):
            S.fail("seconds outside signed 64 bits")
    cid = S.bstr(v[9], "commit id", 32)
    desc = _ref(S, v[10], "descriptor")
    envs = []
    region = None
    if ver == 2:
        arr = S.array(v[11], "key envelopes", 1)
        if len(arr) > MAX_KEY_ENVELOPES:
            raise MochiError("LIMIT_EXCEEDED", "more key envelopes than the reader limit")
        envs = [_ref(S, e, "key envelope") for e in arr]
        if v[12] is not None:
            r = S.map(v[12], "data region", (0, 1, 2))
            region = ObjectRef(S.uint(r[0], "region offset"), S.uint(r[1], "region length", minimum=73),
                               S.bstr(r[2], "region hash", 32))
        if feats != [1]:
            S.fail("schema 2 requires exactly required feature 1")
    else:
        if 1 in feats:
            S.fail("a schema-1 commit lists the Encrypted feature")
    # structure (R3 7.1)
    if seq == 0:
        if parent is not None:
            S.fail("sequence 0 has a parent")
        if meta[0] != 0:
            S.fail("sequence 0 must be a checkpoint")
    else:
        if parent is None:
            S.fail("orphan commit")
        if parent[1] != seq - 1:
            S.fail("parent sequence is not sequence - 1")
        if base is not None and base[1] >= seq:
            S.fail("base sequence not before the commit")
    if desc.offset != 0:
        S.fail("descriptor reference not at offset 0")
    if compute_commit_id(v) != cid:
        S.fail("commit id does not match the recomputed value")
    c = Commit(ver, aid, seq, txid, parent, meta[0] == 0, image, snapshot, base, delta, feats, t, cid, desc,
               envs, region, v)
    return c


def check_refs_before(c: Commit, commit_offset: int) -> None:
    """Every reference ends at or before the commit frame (5.1)."""
    refs = [c.delta, c.descriptor] + ([c.image, c.snapshot] if c.checkpoint else []) + c.key_envelopes
    if c.data_region is not None:
        refs.append(c.data_region)
    for r in refs:
        if r.end > commit_offset:
            raise MochiError("RECORD_INVALID", "a reference reaches the commit frame or beyond")
