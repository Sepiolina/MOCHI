"""Data objects: one Zstandard frame per chunk (spec 8.5, 8.6, 9.2; O21 via R1/R4)."""
from __future__ import annotations

import zstandard

from . import digest, frames
from .errors import MochiError
from .frames import ZSTD_MAGIC


def decode_chunk_frame(frame: bytes, decoded_len: int, content_hash: bytes | None = None,
                       dictionary_declared: bool = False) -> bytes:
    """Decode one chunk object.  MALFORMED_FRAME for shape faults (not exactly one data
    frame, no Frame_Content_Size, no checksum); CONTENT_INTEGRITY_FAILED when the bytes
    do not decode to the recorded length and content hash."""
    try:
        fr = frames.walk_frame(frame, 0)
    except MochiError as e:
        raise MochiError("MALFORMED_FRAME" if e.code != "LIMIT_EXCEEDED" else e.code, e.message) from None
    if fr.magic != ZSTD_MAGIC or fr.length != len(frame):
        raise MochiError("MALFORMED_FRAME", "a chunk is exactly one Zstandard data frame")
    hdr = frames.data_frame_header(frame)
    if hdr["content_size"] is None or not hdr["checksum"]:
        raise MochiError("MALFORMED_FRAME", "a chunk frame carries Frame_Content_Size and the checksum (O21)")
    if hdr["dictionary_id_flag"] and not dictionary_declared:
        raise MochiError("CONTENT_INTEGRITY_FAILED", "frame names a dictionary the catalog does not declare")
    if hdr["dictionary_id_flag"]:
        raise MochiError("UNSUPPORTED_FEATURE", "chunk depends on a dictionary")
    try:
        out = zstandard.ZstdDecompressor().decompress(frame, max_output_size=hdr["content_size"] + 1)
    except zstandard.ZstdError as e:
        raise MochiError("CONTENT_INTEGRITY_FAILED", "Zstandard: %s" % e) from None
    if len(out) != decoded_len or len(out) != hdr["content_size"]:
        raise MochiError("CONTENT_INTEGRITY_FAILED", "decoded length differs from the record or the frame header")
    if content_hash is not None and digest.chunk_content(out) != content_hash:
        raise MochiError("CONTENT_INTEGRITY_FAILED", "chunk content hash mismatch")
    return out
