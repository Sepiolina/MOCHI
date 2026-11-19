"""R3 section 1.1: every row of the codec table."""
import pytest

from mochi_verify import cbor
from mochi_verify.errors import MochiError


def h(s: str) -> bytes:
    out = []
    for tok in s.split():
        if "×" in tok:
            b, n = tok.split("×")
            out.append(bytes.fromhex(b) * int(n))
        else:
            out.append(bytes.fromhex(tok))
    return b"".join(out)


ACCEPT = [
    "00", "17", "18 18", "18 ff", "19 01 00", "1a 00 01 00 00", "1b 00 00 00 01 00 00 00 00", "1b ff×8",
    "20", "3b ff×8", "f4", "f5", "f6", "40", "44 01 02 03 04", "60", "62 c3 a9", "80", "82 01 02", "a0",
    "a2 00 01 01 02", "a1 18 18 00", "81×64 00",
]
REFUSE = [
    ("18 00", "NotShortest"), ("18 17", "NotShortest"), ("19 00 ff", "NotShortest"),
    ("1a 00 00 ff ff", "NotShortest"), ("1b 00 00 00 00 ff ff ff ff", "NotShortest"), ("38 00", "NotShortest"),
    ("58 00", "NotShortest"), ("78 00", "NotShortest"), ("98 00", "NotShortest"), ("b8 00", "NotShortest"),
    ("a1 18 00 00", "NotShortest"), ("c0 60", "Tag"), ("d8 18 40", "Tag"), ("f9 3c 00", "Float"),
    ("fa 3f 80 00 00", "Float"), ("fb 3f f0 00 00 00 00 00 00", "Float"), ("f7", "SimpleValue"),
    ("e0", "SimpleValue"), ("f8 20", "SimpleValue"), ("f8 14", "SimpleValue"), ("5f ff", "Indefinite"),
    ("7f ff", "Indefinite"), ("9f ff", "Indefinite"), ("bf ff", "Indefinite"), ("ff", "Indefinite"),
    ("1c", "ReservedAdditionalInfo"), ("3d", "ReservedAdditionalInfo"), ("5e", "ReservedAdditionalInfo"),
    ("a1 61 61 00", "NonIntegerKey"), ("a1 41 00 00", "NonIntegerKey"), ("a1 20 00", "NonIntegerKey"),
    ("a2 01 00 00 00", "KeyOrder"), ("a2 00 01 00 02", "KeyOrder"), ("61 ff", "InvalidUtf8"),
    ("62 c0 80", "InvalidUtf8"), ("63 ed a0 80", "InvalidUtf8"), ("", "Truncated"), ("18", "Truncated"),
    ("19 01", "Truncated"), ("81 18", "Truncated"), ("44 01", "LengthExceedsInput"),
    ("83 00", "LengthExceedsInput"), ("a2 00", "LengthExceedsInput"), ("5b ff×8", "LengthExceedsInput"),
    ("00 00", "TrailingBytes"), ("81×65 00", "TooDeep"),
]
CODES = {"Truncated": "TRUNCATED", "LengthExceedsInput": "TRUNCATED", "TooDeep": "LIMIT_EXCEEDED",
         "TooManyItems": "LIMIT_EXCEEDED"}


@pytest.mark.parametrize("hexs", ACCEPT)
def test_accepts(hexs):
    data = h(hexs)
    assert cbor.encode(cbor.decode(data)) == data


@pytest.mark.parametrize("hexs,fault", REFUSE)
def test_refuses(hexs, fault):
    with pytest.raises(cbor.CborError) as ei:
        cbor.decode(h(hexs))
    assert ei.value.fault == fault
    assert ei.value.code == CODES.get(fault, "RECORD_INVALID")


@pytest.mark.parametrize("hexs,items,ok", [("a1 00 00", 3, True), ("a1 00 00", 2, False),
                                           ("83 00 00 00", 4, True), ("84 00 00 00 00", 4, False)])
def test_item_limit(hexs, items, ok):
    if ok:
        cbor.decode(h(hexs), max_items=items)
    else:
        with pytest.raises(cbor.CborError) as ei:
            cbor.decode(h(hexs), max_items=items)
        assert ei.value.fault == "TooManyItems"


def test_decode_is_identity_with_encode_for_nested_records():
    v = {0: 1, 1: [b"\x00" * 3, "é", None, True, False, -5, {2: [1, 2, 3]}], 24: 2 ** 40}
    assert cbor.decode(cbor.encode(v)) == v


def test_errors_are_mochi_errors():
    with pytest.raises(MochiError):
        cbor.decode(b"\x1c")
