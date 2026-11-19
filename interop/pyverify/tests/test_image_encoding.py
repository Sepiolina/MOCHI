"""Catalog-image payload encodings 0 and 2 (spec B.2.2, B.2.15; R2 section 2).

Envelopes are built here from the spec's layout, not copied from the reference implementation."""
import pytest
import zstandard

from mochi_verify import envelope, frames
from mochi_verify.errors import MochiError

AID, SEQ, TXN = b"\xa1" * 32, 7, b"\xb2" * 16


def sqlite_image(user_version=1, pages=2):
    img = bytearray(4096 * pages)
    img[:16] = envelope.SQLITE_SIGNATURE
    img[60:64] = user_version.to_bytes(4, "big")
    return bytes(img)


def zstd_frame(data, checksum=True, content_size=True):
    return zstandard.ZstdCompressor(level=9, write_checksum=checksum, write_content_size=content_size).compress(data)


def envelope_frame(payload, enc, schema=1, kind=frames.K_IMAGE, features=()):
    hdr = (80 + 8 * len(features)).to_bytes(4, "little") + (0).to_bytes(2, "little") + schema.to_bytes(2, "little") \
        + enc.to_bytes(4, "little") + len(features).to_bytes(4, "little") + len(payload).to_bytes(8, "little") \
        + AID + SEQ.to_bytes(8, "little") + TXN + b"".join(f.to_bytes(8, "little") for f in features)
    body = hdr + payload
    return kind.to_bytes(4, "little") + len(body).to_bytes(4, "little") + body


def read(payload, enc):
    env = envelope.decode_binary(envelope_frame(payload, enc), frames.K_IMAGE, (1,))
    env.bind(AID, SEQ, TXN)
    return envelope.decode_image(env)


def test_encoding_2_round_trip():
    img = sqlite_image()
    assert read(zstd_frame(img), 2) == img


def test_encoding_0_still_read():
    img = sqlite_image()
    assert read(img, 0) == img


def test_encoding_3_unregistered():
    with pytest.raises(MochiError) as e:
        envelope.decode_binary(envelope_frame(zstd_frame(sqlite_image()), 3), frames.K_IMAGE, (1,))
    assert e.value.code == "ENVELOPE_INVALID"


def test_encoding_2_in_a_parity_frame_is_refused():
    with pytest.raises(MochiError) as e:
        envelope.decode_binary(envelope_frame(zstd_frame(sqlite_image()), 2, schema=0, kind=frames.K_PARITY),
                               frames.K_PARITY, (0,))
    assert e.value.code == "ENVELOPE_INVALID"


def test_envelope_signature_is_the_zstd_magic():
    with pytest.raises(MochiError) as e:
        envelope.decode_binary(envelope_frame(sqlite_image(), 2), frames.K_IMAGE, (1,))
    assert e.value.code == "ENVELOPE_INVALID"


def test_decoded_signature_is_checked():
    img = bytearray(sqlite_image())
    img[0] ^= 1
    with pytest.raises(MochiError) as e:
        read(zstd_frame(bytes(img)), 2)
    assert e.value.code == "ENVELOPE_INVALID"


def test_decoded_user_version_is_checked():
    with pytest.raises(MochiError) as e:
        read(zstd_frame(sqlite_image(user_version=2)), 2)
    assert e.value.code == "ENVELOPE_INVALID"


@pytest.mark.parametrize("payload", [
    lambda f: f + b"\x00",                                   # bytes after the frame
    lambda f: zstd_frame(sqlite_image(), content_size=False),
    lambda f: zstd_frame(sqlite_image(), checksum=False),
])
def test_frame_shape_is_checked(payload):
    with pytest.raises(MochiError) as e:
        read(payload(zstd_frame(sqlite_image())), 2)
    assert e.value.code == "MALFORMED_FRAME"


def test_two_frames_are_refused():
    f = zstd_frame(sqlite_image())
    with pytest.raises(MochiError):
        read(f + f, 2)


def raw_frame(fhd, header_rest, blocks=b"\x01\x00\x00", checksum=b"\x00\x00\x00\x00"):
    return b"\x28\xb5\x2f\xfd" + bytes([fhd]) + header_rest + blocks + checksum


def test_declared_size_above_the_bound_is_refused_before_decoding():
    # not single-segment (1 KiB window), 8-byte Frame_Content_Size of 2**30 > S - 592; body never decodes
    f = raw_frame(0xC0 | 0x04, b"\x00" + (1 << 30).to_bytes(8, "little"))
    with pytest.raises(MochiError) as e:
        read(f, 2)
    assert e.value.code == "LIMIT_EXCEEDED"
    assert "window" not in e.value.message


def test_bound_is_checked_before_the_checksum_flag():
    f = raw_frame(0xC0, b"\x00" + (1 << 30).to_bytes(8, "little"), checksum=b"")
    with pytest.raises(MochiError) as e:
        read(f, 2)
    assert e.value.code == "LIMIT_EXCEEDED"


def test_window_above_max_window_size_is_refused_on_the_window():
    # single segment: window = content size = 200,000,000 > 128 MiB but below the image bound
    f = raw_frame(0x80 | 0x20 | 0x04, (200_000_000).to_bytes(4, "little"))
    with pytest.raises(MochiError) as e:
        read(f, 2)
    assert e.value.code == "LIMIT_EXCEEDED" and "window" in e.value.message


def test_declared_size_differing_from_actual_is_integrity_failure():
    # declares 8192 bytes (4-byte FCS), carries one empty last raw block: it decodes to nothing
    f = raw_frame(0x80 | 0x20 | 0x04, (8192).to_bytes(4, "little"))
    with pytest.raises(MochiError) as e:
        read(f, 2)
    assert e.value.code == "CONTENT_INTEGRITY_FAILED"


def test_nonzero_dictionary_id_is_integrity_failure():
    f = raw_frame(0x20 | 0x04 | 0x01, b"\x05" + b"\x00")           # single segment, 1-byte dictionary ID 5, size 0
    with pytest.raises(MochiError) as e:
        read(f, 2)
    assert e.value.code == "CONTENT_INTEGRITY_FAILED"


def test_corrupt_block_or_checksum_is_integrity_failure():
    f = bytearray(zstd_frame(sqlite_image()))
    f[-1] ^= 1                                                  # the content checksum
    with pytest.raises(MochiError) as e:
        read(bytes(f), 2)
    assert e.value.code == "CONTENT_INTEGRITY_FAILED"


def test_payload_without_the_four_magic_bytes_is_envelope_invalid():
    f = zstd_frame(sqlite_image())
    for payload in (b"", f[:1], f[:2], f[:3]):          # empty, 28, 28 B5, 28 B5 2F
        with pytest.raises(MochiError) as e:
            envelope.decode_binary(envelope_frame(payload, 2), frames.K_IMAGE, (1,))
        assert e.value.code == "ENVELOPE_INVALID", len(payload)


def test_cut_short_inside_the_frame_is_truncated():
    f = zstd_frame(sqlite_image())
    for n in (4, 5, 10, len(f) - 1):
        with pytest.raises(MochiError) as e:
            read(f[:n], 2)
        assert e.value.code == "TRUNCATED", n


def test_second_frame_and_trailing_bytes_are_malformed():
    f = zstd_frame(sqlite_image())
    for payload in (f + f, f + b"\x00"):
        with pytest.raises(MochiError) as e:
            read(payload, 2)
        assert e.value.code == "MALFORMED_FRAME"


def test_missing_content_size_is_malformed():
    with pytest.raises(MochiError) as e:
        read(zstd_frame(sqlite_image(), content_size=False), 2)
    assert e.value.code == "MALFORMED_FRAME"


def test_missing_checksum_flag_is_malformed():
    with pytest.raises(MochiError) as e:
        read(zstd_frame(sqlite_image(), checksum=False), 2)
    assert e.value.code == "MALFORMED_FRAME"


def test_required_features_are_checked_before_any_decode():
    junk = zstd_frame(sqlite_image())[:-1]          # would be CONTENT_INTEGRITY_FAILED / TRUNCATED if decoded
    for feats, code in (((2,), "UNSUPPORTED_FEATURE"), ((1, 1), "ENVELOPE_INVALID"), ((2, 1), "ENVELOPE_INVALID")):
        with pytest.raises(MochiError) as e:
            envelope.decode_binary(envelope_frame(junk, 2, features=feats), frames.K_IMAGE, (1,))
        assert e.value.code == code, feats


def test_identity_is_bound_before_decoding():
    # decode_binary alone never decompresses; a wrong identity is refused by bind, with a payload that cannot decode
    env = envelope.decode_binary(envelope_frame(zstd_frame(sqlite_image())[:-1], 2), frames.K_IMAGE, (1,))
    with pytest.raises(MochiError) as e:
        env.bind(b"\x00" * 32, SEQ, TXN)
    assert e.value.code == "ENVELOPE_INVALID"
