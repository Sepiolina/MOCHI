"""Archive descriptor v0 (spec B.2.2, D12, R1 sec. 5, R3 sec. 2.2, archive-descriptor-v0.cddl)."""
from __future__ import annotations

from dataclasses import dataclass, field

from . import cbor, frames
from .errors import MochiError
from .schema import Schema, check_features

KNOWN_FEATURES = {1}
RATIFIED = None  # descriptor key 3 of a ratified 1.0 archive is null (B.2.14, D24)


@dataclass
class Descriptor:
    archive_id: bytes
    features: list
    tar_compatible: bool
    recovery: tuple | None            # (suite, percent)
    declared_limits: dict | None
    draft: int | None = RATIFIED

    @property
    def encrypted(self) -> bool:
        return 1 in self.features


def decode_object(data: bytes) -> Descriptor:
    """``data`` is the exact stored object (one skippable frame)."""
    S = Schema("DESCRIPTOR_INVALID", "descriptor")
    try:
        fr = frames.walk_frame(data, 0)
    except MochiError as e:
        if e.code in ("LIMIT_EXCEEDED", "UNSUPPORTED_FEATURE"):
            raise
        raise MochiError("DESCRIPTOR_INVALID", "descriptor frame: " + e.message) from None
    if fr.magic != frames.K_DESCRIPTOR:
        S.fail("not a descriptor frame")
    if fr.length != len(data):
        S.fail("bytes after the descriptor frame")
    try:
        v = cbor.decode(data[8:])
    except MochiError as e:
        if e.code in ("LIMIT_EXCEEDED", "UNSUPPORTED_FEATURE"):
            raise
        raise MochiError("DESCRIPTOR_INVALID", e.message) from None
    if not isinstance(v, dict):
        S.fail("not a map")
    if 0 not in v:
        S.fail("missing schema version")
    if v[0] != 0 or isinstance(v[0], bool):
        if S.is_uint(v[0]):
            raise MochiError("UNSUPPORTED_FEATURE", f"descriptor schema version {v[0]}")
        S.fail("schema version not an integer")
    S.map(v, "descriptor", (0, 1, 2, 3, 4, 5), (6,))
    aid = S.bstr(v[1], "archive id", 32)
    gen = S.uint(v[2], "wire generation")
    draft = v[3]
    if draft is not None:
        S.uint(draft, "draft identifier")
    feats = list(S.array(v[4], "features"))
    for f in feats:
        S.uint(f, "feature")
    for a, b in zip(feats, feats[1:]):
        if not a < b:
            S.fail("features not strictly increasing")
    cons = S.map(v[5], "constraints", (0,), (1,))
    tar = S.bool_(cons[0], "tar-compatible")
    recovery = None
    if 1 in cons:
        r = S.map(cons[1], "redundancy", (0, 1))
        suite = S.uint(r[0], "suite")
        pct = S.uint(r[1], "percentage")
        recovery = (suite, pct)
    limits = None
    if 6 in v:
        limits = S.map(v[6], "declared limits", (0, 1, 2))
        for k in (0, 1, 2):
            S.uint(limits[k], "declared limit")
    # refusals and value rules, after the closed schema
    if gen != 2:
        raise MochiError("UNSUPPORTED_FEATURE", f"wire generation {gen}")
    if draft is not None:
        raise MochiError("UNSUPPORTED_FEATURE",
                         f"draft identifier {draft} (a pre-1.0 draft archive; this verifier reads only ratified 1.0 archives)")
    for f in feats:
        if f not in KNOWN_FEATURES:
            raise MochiError("UNSUPPORTED_FEATURE", f"unknown required feature {f}")
    if recovery is not None:
        if recovery[0] != 1:
            raise MochiError("UNSUPPORTED_FEATURE", f"redundancy suite {recovery[0]}")
        if not 1 <= recovery[1] <= 100:
            S.fail("parity percentage outside 1..100")
    if 1 in feats and tar:
        S.fail("Encrypted profile combined with TAR compatibility")
    return Descriptor(aid, feats, tar, recovery, limits, draft)
