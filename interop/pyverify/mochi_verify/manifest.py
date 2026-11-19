"""Recovery manifests v1/v2/v3 (spec D10, D18, D20; R3; recovery-manifest-v{1,2,3}.cddl)."""
from __future__ import annotations

from dataclasses import dataclass, field

from . import cbor, frames
from .errors import MochiError
from .paths import check_path
from .schema import Schema, check_features

KIND_DELTA, KIND_SNAPSHOT = 0, 1
WINDOWS_VALUES = set(range(0, 8)) | set(range(0x20, 0x28))


@dataclass
class Chunk:
    id: bytes
    protection: int
    stored_len: int
    stored_hash: bytes
    decoded_len: int
    content_hash: bytes
    deps: list
    offset: int | None


@dataclass
class Version:
    id: bytes
    kind: int                 # 0 file, 1 directory
    length: int
    content_hash: bytes | None
    extents: list             # (logical_offset, length, chunk_id|None, chunk_offset)
    posix: tuple | None       # (mode, uid, gid)
    windows: int | None
    mtime: tuple | None       # (secs, nanos)


@dataclass
class Manifest:
    schema: int
    archive_id: bytes
    sequence: int
    parent: tuple | None      # (sequence, delta_manifest_hash)
    kind: int
    chunks: list
    versions: list
    ops: list                 # ('put', path, fv) | ('delete', path)
    entries: list             # (path, fv)
    features: list
    transaction_id: bytes
    retention_ops: list | None = None          # delta, schema >= 2
    retention_state: tuple | None = None       # snapshot: (expired list, holds list[(label, seq)])
    provenance: dict | None = None
    key_ops: list | None = None                # delta, schema 3
    key_state: list | None = None              # snapshot, schema 3


def decode_object(data: bytes, expected_magic: int = frames.K_MANIFEST) -> Manifest:
    """``data`` is a complete stored 0x184D2A58 frame."""
    try:
        fr = frames.walk_frame(data, 0)
    except MochiError as e:
        raise MochiError(e.code if e.code in ("TRUNCATED", "LIMIT_EXCEEDED") else "MALFORMED_FRAME", e.message) from None
    if fr.length != len(data):
        raise MochiError("MALFORMED_FRAME", "bytes after the manifest frame")
    if fr.magic != expected_magic:
        raise MochiError("MALFORMED_FRAME", "not a recovery-manifest frame")
    return decode_payload(bytes(data[8:]))


def _sorted_unique(S: Schema, items, keyfn, name: str):
    prev = None
    for it in items:
        k = keyfn(it)
        if prev is not None and not prev < k:
            S.fail(f"{name} not sorted without duplicates")
        prev = k


def decode_payload(payload: bytes) -> Manifest:
    S = Schema("RECORD_INVALID", "manifest")
    v = cbor.decode(payload)
    if not isinstance(v, dict) or 0 not in v:
        S.fail("not a map with a schema version")
    ver = v[0]
    if not S.is_uint(ver):
        S.fail("schema version")
    if ver not in (1, 2, 3):
        raise MochiError("UNSUPPORTED_FEATURE", f"manifest schema version {ver}" + (" (legacy)" if ver == 0 else ""))
    base_keys = (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
    kind = v.get(4)
    if kind not in (0, 1) or isinstance(kind, bool):
        S.fail("manifest kind")
    if ver == 1:
        S.map(v, "manifest", base_keys)
    elif ver == 2:
        S.map(v, "manifest", base_keys + (11,), (12,))
    else:
        S.map(v, "manifest", base_keys + (11, 13), (12,))
    aid = S.bstr(v[1], "archive id", 32)
    seq = S.uint(v[2], "sequence")
    parent = None
    if v[3] is not None:
        p = S.map(v[3], "parent", (0, 1))
        parent = (S.uint(p[0], "parent sequence"), S.bstr(p[1], "parent hash", 32))
    feats = check_features(v[9], {1}, S)
    txid = S.bstr(v[10], "transaction id", 16)
    if ver == 3:
        if feats != [1]:
            S.fail("schema 3 requires exactly feature 1")
    elif 1 in feats:
        S.fail("feature 1 requires schema 3")
    encrypted = ver == 3
    chunks = [_chunk(S, c, encrypted) for c in S.array(v[5], "chunks")]
    versions = [_version(S, x) for x in S.array(v[6], "file versions")]
    ops = [_op(S, o) for o in S.array(v[7], "operations")]
    entries = []
    for e in S.array(v[8], "entries"):
        e = S.array(e, "entry", 2, 2)
        pth = S.bstr(e[0], "entry path")
        check_path(pth)
        entries.append((pth, S.bstr(e[1], "entry file version", 32)))
    _sorted_unique(S, chunks, lambda c: c.id, "chunks")
    _sorted_unique(S, versions, lambda x: x.id, "file versions")
    _sorted_unique(S, entries, lambda e: e[0], "entries")
    m = Manifest(ver, aid, seq, parent, kind, chunks, versions, ops, entries, feats, txid)
    # structure
    if kind == KIND_DELTA:
        if (parent is None) != (seq == 0):
            S.fail("parent is null exactly when sequence is 0")
        if parent is not None and parent[0] != seq - 1:
            S.fail("parent sequence is not sequence - 1")
        if entries:
            S.fail("a delta carries entries")
    else:
        if parent is not None:
            S.fail("a snapshot has a parent")
        if ops:
            S.fail("a snapshot carries operations")
        _snapshot_self_contained(S, m)
    if ver >= 2:
        _retention(S, m, v)
    if ver == 3:
        _keys(S, m, v)
    return m


def _chunk(S, c, encrypted: bool) -> Chunk:
    S.map(c, "chunk", tuple(range(9)))
    S.exactly(c[1], 0, "chunk encoding")
    S.exactly(c[2], 1 if encrypted else 0, "chunk protection")
    deps = S.array(c[7], "chunk dependencies")
    if encrypted and deps:
        S.fail("an Encrypted chunk lists dependencies")
    dd = []
    for d in deps:
        d = S.array(d, "dependency", 2, 2)
        if d[0] not in (0, 1) or isinstance(d[0], bool):
            S.fail("dependency kind")
        dd.append((d[0], S.bstr(d[1], "dependency id", 32)))
    _sorted_unique(S, dd, lambda x: (x[0], x[1]), "dependencies")
    off = c[8]
    if off is not None:
        S.uint(off, "stored offset")
    return Chunk(S.bstr(c[0], "chunk id", 32), c[2], S.uint(c[3], "stored length", minimum=1),
                 S.bstr(c[4], "stored hash", 32), S.uint(c[5], "decoded length"),
                 S.bstr(c[6], "content hash", 32), dd, off)


def _version(S, x) -> Version:
    if not isinstance(x, dict) or 1 not in x:
        S.fail("file version is not a map")
    k = x[1]
    if isinstance(k, bool) or not S.is_uint(k):
        S.fail("file version kind")
    if k == 2:
        raise MochiError("UNSUPPORTED_FEATURE", "symbolic-link file version (reserved)")
    if k not in (0, 1):
        S.fail("unknown file version kind")
    S.map(x, "file version", tuple(range(6)))
    vid = S.bstr(x[0], "file version id", 32)
    length = S.uint(x[2], "logical length")
    if k == 0:
        chash = S.bstr(x[3], "content hash", 32)
    else:
        S.exactly(x[2], 0, "directory length")
        if x[3] is not None:
            S.fail("a directory has a content hash")
        chash = None
        if S.array(x[4], "extents"):
            S.fail("a directory has extents")
    extents = []
    for e in S.array(x[4], "extents"):
        e = S.array(e, "extent", 4, 4)
        lo = S.uint(e[0], "extent offset")
        ln = S.uint(e[1], "extent length")
        if e[2] is None:
            if e[3] != 0 or isinstance(e[3], bool):
                S.fail("a hole has a non-zero chunk offset")
            extents.append((lo, ln, None, 0))
        else:
            extents.append((lo, ln, S.bstr(e[2], "extent chunk", 32), S.uint(e[3], "chunk offset")))
    at = x[5]
    A = S.map(at, "attributes", (), (0, 1, 2))
    posix = windows = mtime = None
    if 0 in A:
        a = S.array(A[0], "posix attributes", 3, 3)
        posix = (S.uint(a[0], "mode", maximum=0o7777), S.uint(a[1], "uid", maximum=0xFFFFFFFF),
                 S.uint(a[2], "gid", maximum=0xFFFFFFFF))
    if 1 in A:
        w = A[1]
        if not S.is_uint(w) or w not in WINDOWS_VALUES:
            S.fail("windows attributes")
        windows = w
    if 2 in A:
        a = S.array(A[2], "mtime", 2, 2)
        sec = S.sint(a[0], "mtime seconds")
        if not -(1 << 63) <= sec < (1 << 63):
            S.fail("mtime seconds outside signed 64 bits")
        mtime = (sec, S.uint(a[1], "mtime nanoseconds", maximum=999_999_999))
    return Version(vid, k, length, chash, extents, posix, windows, mtime)


def _op(S, o):
    o = S.array(o, "operation", 2, 3)
    if o[0] == 0 and not isinstance(o[0], bool) and len(o) == 3:
        pth = S.bstr(o[1], "put path")
        check_path(pth)
        return ("put", pth, S.bstr(o[2], "put file version", 32))
    if o[0] == 1 and not isinstance(o[0], bool) and len(o) == 2:
        pth = S.bstr(o[1], "delete path")
        check_path(pth)
        return ("delete", pth)
    S.fail("unknown operation")


def _snapshot_self_contained(S, m: Manifest) -> None:
    cids = {c.id for c in m.chunks}
    vids = {x.id for x in m.versions}
    for x in m.versions:
        for e in x.extents:
            if e[2] is not None and e[2] not in cids:
                S.fail("snapshot references a chunk it does not hold")
    for _, fv in m.entries:
        if fv not in vids:
            S.fail("snapshot references a file version it does not hold")


def _label(S, v) -> bytes:
    if not isinstance(v, bytes) or not 1 <= len(v) <= 255:
        S.fail("hold label must be 1..255 bytes")
    return v


def _retention(S, m: Manifest, v: dict) -> None:
    seq = m.sequence
    r = v[11]
    prov = v.get(12)
    if m.kind == KIND_DELTA:
        ops = []
        for o in S.array(r, "retention operations"):
            o = S.array(o, "retention operation", 2, 3)
            t = o[0]
            if isinstance(t, bool) or t not in (0, 1, 2):
                S.fail("unknown retention operation")
            if t == 0:
                if len(o) != 2:
                    S.fail("expire shape")
                s = S.uint(o[1], "expire sequence")
                if s >= seq:
                    S.fail("expire of this commit or a later one")
                ops.append(("expire", s))
            elif t == 1:
                if len(o) != 3:
                    S.fail("hold shape")
                lab = _label(S, o[1])
                s = S.uint(o[2], "hold sequence")
                if s > seq:
                    S.fail("hold of a future sequence")
                ops.append(("hold", lab, s))
            else:
                if len(o) != 2:
                    S.fail("release shape")
                ops.append(("release", _label(S, o[1])))
        # local, in-manifest consistency (state-dependent checks happen in replay)
        expired, held = set(), set()
        for o in ops:
            if o[0] == "expire":
                if o[1] in expired:
                    S.fail("snapshot expired twice")
                expired.add(o[1])
            elif o[0] == "hold":
                if o[1] in held:
                    S.fail("label already active")
                held.add(o[1])
            else:
                held.discard(o[1])
        m.retention_ops = ops
        if m.schema == 2 and not ops and prov is None:
            S.fail("schema 2 delta without retention data or provenance")
    else:
        if not isinstance(r, dict):
            S.fail("snapshot retention state must be a map")
        S.map(r, "retention state", (0, 1))
        exp = [S.uint(s, "expired sequence") for s in S.array(r[0], "expired")]
        for a, b in zip(exp, exp[1:]):
            if not a < b:
                S.fail("expired sequences not strictly increasing")
        if any(s >= seq for s in exp):
            S.fail("expired sequence not before this commit")
        holds = []
        for h in S.array(r[1], "holds"):
            h = S.array(h, "hold entry", 2, 2)
            lab = _label(S, h[0])
            s = S.uint(h[1], "hold sequence")
            if s > seq:
                S.fail("held sequence after this commit")
            holds.append((lab, s))
        for a, b in zip(holds, holds[1:]):
            if not a[0] < b[0]:
                S.fail("holds not strictly increasing by label")
        if m.schema == 2 and not exp and not holds:
            S.fail("schema 2 snapshot with an empty retention state")
        m.retention_state = (exp, holds)
    if prov is not None:
        if not (m.kind == KIND_DELTA and seq == 0):
            S.fail("provenance outside the delta of commit 0")
        P = S.map(prov, "provenance", (0, 1, 2), (3,) if m.schema == 3 else ())
        S.bstr(P[0], "source archive id", 32)
        kept = []
        for it in S.array(P[1], "kept commits", 1):
            it = S.array(it, "kept commit", 2, 2)
            kept.append((S.uint(it[0], "source sequence"), S.bstr(it[1], "source commit id", 32)))
        for a, b in zip(kept, kept[1:]):
            if not a[0] < b[0]:
                S.fail("source sequences not strictly increasing")
        coll = [S.uint(s, "collected sequence") for s in S.array(P[2], "collected")]
        for a, b in zip(coll, coll[1:]):
            if not a < b:
                S.fail("collected sequences not strictly increasing")
        kept_seqs = {k[0] for k in kept}
        for s in coll:
            if s >= kept[-1][0] or s in kept_seqs:
                S.fail("collected sequence is the head or also kept")
        if 3 in P:
            S.exactly(P[3], 1, "provenance reason")
        m.provenance = {"source": P[0], "kept": kept, "collected": coll, "reason": P.get(3, 0)}


def _keys(S, m: Manifest, v: dict) -> None:
    k = v[13]
    if m.kind == KIND_DELTA:
        ops = []
        arr = S.array(k, "key operations", 0, 32)
        for o in arr:
            o = S.array(o, "key operation", 2, 2)
            if o[0] not in (0, 1) or isinstance(o[0], bool):
                S.fail("key operation kind")
            ops.append(("add" if o[0] == 0 else "remove", S.bstr(o[1], "envelope id", 16)))
        if m.sequence == 0:
            if not ops or any(o[0] != "add" for o in ops):
                S.fail("commit 0 adds its initial envelopes and removes none")
        m.key_ops = ops
    else:
        ids = [S.bstr(e, "envelope id", 16) for e in S.array(k, "key state", 1, 16)]
        for a, b in zip(ids, ids[1:]):
            if not a < b:
                S.fail("key state not strictly increasing")
        m.key_state = ids
