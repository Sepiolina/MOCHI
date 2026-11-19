"""Structural frame walk (spec 8.5, 8.6; R1 section 2)."""
from __future__ import annotations

from dataclasses import dataclass

from .errors import MochiError

ZSTD_MAGIC = 0xFD2FB528
SKIP_FIRST = 0x184D2A50
SKIP_LAST = 0x184D2A5F

K_IMAGE = 0x184D2A50          # catalog image (binary envelope, encoding 2; 0 in draft 2)
K_COMMIT = 0x184D2A51
K_PREPARED = 0x184D2A52
K_FOOTER = 0x184D2A56
K_DESCRIPTOR = 0x184D2A57
K_MANIFEST = 0x184D2A58
K_SEALED = 0x184D2A59
K_KEY_ENVELOPE = 0x184D2A5C
K_PARITY = 0x184D2A5F

SKIPPABLE_PAYLOAD_LIMIT = 256 * 1024 * 1024            # S (B.2.3)
FRAME_LIMIT = 269_484_032                              # any frame (B.2.3)
COMMIT_FRAME_LIMIT = 8 + 64 * 1024                     # B.2.3
IMAGE_PAYLOAD_LIMIT = SKIPPABLE_PAYLOAD_LIMIT - 592    # S - 592
MAX_WINDOW_SIZE = 134_217_728                          # reader limit max-window-size, 128 MiB (B.2.15 item 2)


def is_skippable(magic: int) -> bool:
    return SKIP_FIRST <= magic <= SKIP_LAST


@dataclass(frozen=True)
class Frame:
    offset: int
    length: int      # total stored length, header included
    magic: int

    @property
    def end(self) -> int:
        return self.offset + self.length


def u32(buf, off: int) -> int:
    return int.from_bytes(buf[off:off + 4], "little")


def u64(buf, off: int) -> int:
    return int.from_bytes(buf[off:off + 8], "little")


def walk_frame(buf, off: int, limit: int | None = None, max_window: int | None = None) -> Frame:
    """Walk one frame starting at ``off``.  Raises MALFORMED_FRAME/TRUNCATED.

    The physical length comes from the structure only (8.6), never from
    Frame_Content_Size.  ``limit`` is the exclusive upper bound of readable bytes.
    """
    end_limit = len(buf) if limit is None else min(limit, len(buf))
    if off < 0 or off + 4 > end_limit:
        raise MochiError("TRUNCATED", "frame magic cut short at %d" % off)
    magic = u32(buf, off)
    if is_skippable(magic):
        if off + 8 > end_limit:
            raise MochiError("TRUNCATED", "skippable header cut short at %d" % off)
        size = u32(buf, off + 4)
        if size > SKIPPABLE_PAYLOAD_LIMIT:
            raise MochiError("LIMIT_EXCEEDED", "skippable payload %d" % size)
        if off + 8 + size > end_limit:
            raise MochiError("TRUNCATED", "skippable payload cut short at %d" % off)
        if magic == K_DESCRIPTOR and off != 0:
            raise MochiError("MALFORMED_FRAME", "descriptor frame away from offset 0 (%d)" % off)
        return Frame(off, 8 + size, magic)
    if magic != ZSTD_MAGIC:
        raise MochiError("MALFORMED_FRAME", "not a frame magic at %d" % off)
    return _walk_data_frame(buf, off, end_limit, max_window)


def _walk_data_frame(buf, off: int, end_limit: int, max_window: int | None = None) -> Frame:
    p = off + 4
    if p + 1 > end_limit:
        raise MochiError("TRUNCATED", "frame header cut short")
    fhd = buf[p]
    p += 1
    fcs_flag = fhd >> 6
    single = (fhd >> 5) & 1
    if (fhd >> 3) & 1:
        raise MochiError("MALFORMED_FRAME", "reserved header bit set")
    checksum = (fhd >> 2) & 1
    did_flag = fhd & 3
    window = None
    if not single:
        if p + 1 > end_limit:
            raise MochiError("TRUNCATED", "window descriptor cut short")
        wd = buf[p]
        p += 1
        exp, mant = wd >> 3, wd & 7
        base = 1 << (10 + exp)
        window = base + (base >> 3) * mant
    did_len = (0, 1, 2, 4)[did_flag]
    if p + did_len > end_limit:
        raise MochiError("TRUNCATED", "dictionary id cut short")
    p += did_len
    fcs_len = {0: 1 if single else 0, 1: 2, 2: 4, 3: 8}[fcs_flag]
    if p + fcs_len > end_limit:
        raise MochiError("TRUNCATED", "content size cut short")
    fcs = None
    if fcs_len:
        fcs = int.from_bytes(buf[p:p + fcs_len], "little")
        if fcs_flag == 1:
            fcs += 256
    p += fcs_len
    if single:
        window = fcs
    if max_window is not None and window is not None and window > max_window:
        raise MochiError("LIMIT_EXCEEDED", "window size %d above the limit %d" % (window, max_window))
    block_max = min(window if window is not None else 128 * 1024, 128 * 1024)
    while True:
        if p + 3 > end_limit:
            raise MochiError("TRUNCATED", "block header cut short")
        bh = buf[p] | (buf[p + 1] << 8) | (buf[p + 2] << 16)
        p += 3
        last = bh & 1
        btype = (bh >> 1) & 3
        bsize = bh >> 3
        if btype == 3:
            raise MochiError("MALFORMED_FRAME", "reserved block type")
        if bsize > block_max:
            raise MochiError("MALFORMED_FRAME", "block size %d above maximum %d" % (bsize, block_max))
        stored = 1 if btype == 1 else bsize   # an RLE block occupies one byte
        if p + stored > end_limit:
            raise MochiError("TRUNCATED", "block body cut short")
        p += stored
        if last:
            break
    if checksum:
        if p + 4 > end_limit:
            raise MochiError("TRUNCATED", "checksum cut short")
        p += 4
    if p - off > FRAME_LIMIT:
        raise MochiError("LIMIT_EXCEEDED", "frame length")
    return Frame(off, p - off, ZSTD_MAGIC)


def data_frame_header(buf) -> dict:
    """Parsed header fields of a standalone data frame (for O21 checks)."""
    fhd = buf[4]
    fcs_flag = fhd >> 6
    single = (fhd >> 5) & 1
    checksum = (fhd >> 2) & 1
    did_flag = fhd & 3
    dl = (0, 1, 2, 4)[did_flag]
    p = 5 + (0 if single else 1) + dl
    did = int.from_bytes(buf[p - dl:p], "little")
    fcs_len = {0: 1 if single else 0, 1: 2, 2: 4, 3: 8}[fcs_flag]
    fcs = None
    if fcs_len:
        fcs = int.from_bytes(buf[p:p + fcs_len], "little")
        if fcs_flag == 1:
            fcs += 256
    return {"checksum": bool(checksum), "content_size": fcs, "dictionary_id_flag": did_flag, "dictionary_id": did}


def walk_all(buf, start: int = 0, end: int | None = None):
    """Walk consecutive frames from ``start``.  Returns (frames, stop_offset,
    error_or_None).  The walk stops at the first byte that does not begin a
    valid frame; ``stop_offset`` is where it stopped."""
    end = len(buf) if end is None else end
    frames = []
    pos = start
    while pos < end:
        try:
            f = walk_frame(buf, pos, end)
        except MochiError as e:
            return frames, pos, e
        frames.append(f)
        pos = f.end
    return frames, pos, None


def skippable_payload(buf, frame: Frame):
    return buf[frame.offset + 8:frame.end]
