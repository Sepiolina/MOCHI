"""Small helpers for reading CBOR values through a closed schema."""
from __future__ import annotations

from .errors import MochiError


class Schema:
    """Raises ``code`` (default RECORD_INVALID) for every value-level violation."""

    def __init__(self, code: str = "RECORD_INVALID", what: str = "record"):
        self.code = code
        self.what = what

    def fail(self, msg: str):
        raise MochiError(self.code, f"{self.what}: {msg}")

    def is_uint(self, v) -> bool:
        return isinstance(v, int) and not isinstance(v, bool) and v >= 0

    def uint(self, v, name: str, maximum: int | None = None, minimum: int = 0) -> int:
        if not self.is_uint(v) or v < minimum or (maximum is not None and v > maximum):
            self.fail(f"{name} is not an unsigned integer in range")
        return v

    def sint(self, v, name: str) -> int:
        if not isinstance(v, int) or isinstance(v, bool):
            self.fail(f"{name} is not an integer")
        return v

    def bstr(self, v, name: str, size: int | None = None) -> bytes:
        if not isinstance(v, bytes) or (size is not None and len(v) != size):
            self.fail(f"{name} is not a byte string" + (f" of {size} bytes" if size is not None else ""))
        return v

    def tstr(self, v, name: str) -> str:
        if not isinstance(v, str):
            self.fail(f"{name} is not text")
        return v

    def array(self, v, name: str, lo: int = 0, hi: int | None = None) -> list:
        if not isinstance(v, list) or len(v) < lo or (hi is not None and len(v) > hi):
            self.fail(f"{name} is not an array of the right length")
        return v

    def map(self, v, name: str, required, optional=()) -> dict:
        if not isinstance(v, dict):
            self.fail(f"{name} is not a map")
        keys = set(v)
        missing = set(required) - keys
        if missing:
            self.fail(f"{name} lacks keys {sorted(missing)}")
        unknown = keys - set(required) - set(optional)
        if unknown:
            self.fail(f"{name} has unknown keys {sorted(unknown)}")
        return v

    def exactly(self, v, expected, name: str):
        if type(v) is not type(expected) or v != expected:
            self.fail(f"{name} must be {expected!r}")
        return v

    def bool_(self, v, name: str) -> bool:
        if not isinstance(v, bool):
            self.fail(f"{name} is not a boolean")
        return v


def check_features(feats, known, schema: Schema, what: str = "required features") -> list:
    """D11: a list of unsigned integers, strictly increasing (ENVELOPE_INVALID),
    then every entry known (UNSUPPORTED_FEATURE).  ``schema.code`` is used for
    the increasing failure when the record layer re-maps it (descriptor)."""
    feats = schema.array(feats, what)
    for f in feats:
        schema.uint(f, what + " entry")
    inc_code = "DESCRIPTOR_INVALID" if schema.code == "DESCRIPTOR_INVALID" else "ENVELOPE_INVALID"
    for a, b in zip(feats, feats[1:]):
        if not a < b:
            raise MochiError(inc_code, f"{what} not strictly increasing")
    for f in feats:
        if f not in known:
            raise MochiError("UNSUPPORTED_FEATURE", f"unknown required feature {f}")
    return feats
