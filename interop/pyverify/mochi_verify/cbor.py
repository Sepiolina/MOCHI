"""Strict deterministic CBOR (spec Annex B.1 D2, R3 section 1).

Written from R3 section 1 only.  Subset: unsigned/negative integers, byte
strings, UTF-8 text strings, arrays, maps with unsigned-integer keys in strictly
increasing order, ``false``/``true``/``null``.  Definite shortest-form lengths
only.  After decoding the value is re-encoded and must equal the input.

Python mapping: int, bytes, str, list, dict (int keys), True/False/None.
"""
from __future__ import annotations

from .errors import MochiError

DEFAULT_MAX_DEPTH = 64
DEFAULT_MAX_ITEMS = 16 * 1024 * 1024  # 16 Mi (spec B.2.3)

# fault -> error code (R3 section 1.2)
_FAULT_CODES = {
    "Truncated": "TRUNCATED",
    "NotShortest": "RECORD_INVALID",
    "ReservedAdditionalInfo": "RECORD_INVALID",
    "Indefinite": "RECORD_INVALID",
    "Tag": "RECORD_INVALID",
    "Float": "RECORD_INVALID",
    "SimpleValue": "RECORD_INVALID",
    "NonIntegerKey": "RECORD_INVALID",
    "KeyOrder": "RECORD_INVALID",
    "InvalidUtf8": "RECORD_INVALID",
    "LengthExceedsInput": "TRUNCATED",
    "TooDeep": "LIMIT_EXCEEDED",
    "TooManyItems": "LIMIT_EXCEEDED",
    "TrailingBytes": "RECORD_INVALID",
    "NotCanonical": "RECORD_INVALID",
}


class CborError(MochiError):
    def __init__(self, fault: str, detail: str = ""):
        super().__init__(_FAULT_CODES[fault], f"CBOR {fault}" + (f": {detail}" if detail else ""))
        self.fault = fault


class _Reader:
    __slots__ = ("buf", "pos", "items", "max_depth", "max_items")

    def __init__(self, buf: bytes, max_depth: int, max_items: int):
        self.buf = buf
        self.pos = 0
        self.items = 0
        self.max_depth = max_depth
        self.max_items = max_items

    def head(self):
        """Read and validate one head -> (major, argument, additional_info)."""
        buf = self.buf
        if self.pos >= len(buf):
            raise CborError("Truncated")
        ib = buf[self.pos]
        major, ai = ib >> 5, ib & 31
        self.pos += 1
        if ai < 24:
            return major, ai, ai
        if ai in (28, 29, 30):
            raise CborError("ReservedAdditionalInfo")
        if ai == 31:
            if major in (2, 3, 4, 5, 7):
                raise CborError("Indefinite")
            raise CborError("ReservedAdditionalInfo", "ai 31 on major %d" % major)
        n = 1 << (ai - 24)  # 1,2,4,8
        if self.pos + n > len(buf):
            raise CborError("Truncated")
        arg = int.from_bytes(buf[self.pos:self.pos + n], "big")
        self.pos += n
        return major, arg, ai

    def value(self, depth: int):
        if depth > self.max_depth:
            raise CborError("TooDeep")
        self.items += 1
        if self.items > self.max_items:
            raise CborError("TooManyItems")
        major, arg, ai = self.head()
        if major == 7:
            # simple values / floats
            if ai in (25, 26, 27):
                raise CborError("Float")
            if ai == 20:
                return False
            if ai == 21:
                return True
            if ai == 22:
                return None
            raise CborError("SimpleValue")
        if major == 6:
            raise CborError("Tag")
        if ai >= 24:
            _check_shortest(arg, ai)
        if major == 0:
            return arg
        if major == 1:
            return -1 - arg
        if major in (2, 3):
            if arg > len(self.buf) - self.pos:
                raise CborError("LengthExceedsInput")
            raw = self.buf[self.pos:self.pos + arg]
            self.pos += arg
            if major == 2:
                return bytes(raw)
            try:
                return bytes(raw).decode("utf-8", "strict")
            except UnicodeDecodeError:
                raise CborError("InvalidUtf8") from None
        if major == 4:
            if arg > len(self.buf) - self.pos:
                raise CborError("LengthExceedsInput")
            return [self.value(depth + 1) for _ in range(arg)]
        # major 5: map
        if arg * 2 > len(self.buf) - self.pos:
            raise CborError("LengthExceedsInput")
        out = {}
        prev = None
        for _ in range(arg):
            self.items += 1
            if self.items > self.max_items:
                raise CborError("TooManyItems")
            kmajor, karg, kai = self.head()
            if kmajor != 0:
                if kmajor == 6:
                    raise CborError("Tag")
                if kmajor == 7:
                    raise CborError("Float" if kai in (25, 26, 27) else "SimpleValue")
                raise CborError("NonIntegerKey")
            if kai >= 24:
                _check_shortest(karg, kai)
            if prev is not None and karg <= prev:
                raise CborError("KeyOrder")
            prev = karg
            out[karg] = self.value(depth + 1)
        return out


def _check_shortest(arg: int, ai: int) -> None:
    # ai 24 -> 1 byte (arg must be >= 24), 25 -> 2 (>= 256), 26 -> 4 (>= 65536), 27 -> 8 (>= 2^32)
    minimum = {24: 24, 25: 256, 26: 65536, 27: 1 << 32}[ai]
    if arg < minimum:
        raise CborError("NotShortest")


def decode(data: bytes, max_depth: int = DEFAULT_MAX_DEPTH, max_items: int = DEFAULT_MAX_ITEMS):
    """Decode exactly one value; refuse anything non-canonical."""
    data = bytes(data)
    r = _Reader(data, max_depth, max_items)
    value = r.value(0)
    if r.pos != len(data):
        raise CborError("TrailingBytes")
    if encode(value) != data:  # the last guard (R3 2.1)
        raise CborError("NotCanonical")
    return value


def _head(major: int, arg: int) -> bytes:
    if arg < 24:
        return bytes([(major << 5) | arg])
    if arg < 1 << 8:
        return bytes([(major << 5) | 24, arg])
    if arg < 1 << 16:
        return bytes([(major << 5) | 25]) + arg.to_bytes(2, "big")
    if arg < 1 << 32:
        return bytes([(major << 5) | 26]) + arg.to_bytes(4, "big")
    return bytes([(major << 5) | 27]) + arg.to_bytes(8, "big")


def encode(value) -> bytes:
    """Deterministic encoder for the same subset (used for the identity check
    and the commit ID).  Map keys are emitted in increasing order."""
    if value is None:
        return b"\xf6"
    if value is True:
        return b"\xf5"
    if value is False:
        return b"\xf4"
    if isinstance(value, int):
        if value >= 0:
            if value >= 1 << 64:
                raise ValueError("integer out of range")
            return _head(0, value)
        if value < -(1 << 64):
            raise ValueError("integer out of range")
        return _head(1, -1 - value)
    if isinstance(value, (bytes, bytearray)):
        return _head(2, len(value)) + bytes(value)
    if isinstance(value, str):
        raw = value.encode("utf-8")
        return _head(3, len(raw)) + raw
    if isinstance(value, (list, tuple)):
        return _head(4, len(value)) + b"".join(encode(v) for v in value)
    if isinstance(value, dict):
        out = [_head(5, len(value))]
        for k in sorted(value):
            if not isinstance(k, int) or isinstance(k, bool) or k < 0:
                raise ValueError("map keys must be unsigned integers")
            out.append(_head(0, k))
            out.append(encode(value[k]))
        return b"".join(out)
    raise ValueError("unsupported type %r" % type(value))
