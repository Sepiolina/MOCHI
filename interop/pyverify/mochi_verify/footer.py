"""Commit footer (spec 8.4, R1 section 3)."""
from __future__ import annotations

from dataclasses import dataclass

from . import digest
from .errors import MochiError
from .frames import COMMIT_FRAME_LIMIT, K_COMMIT, K_FOOTER, u32, u64

FOOTER_MAGIC = b"MOCHI2\x00\x00"
FOOTER_LEN = 72


@dataclass(frozen=True)
class Footer:
    offset: int          # offset of the footer frame itself
    commit_offset: int
    commit_length: int
    sequence: int
    digest: bytes


def parse_footer(buf, offset: int) -> Footer:
    """Parse (not validate the digest of) a footer frame at ``offset``."""
    if offset < 0 or offset + FOOTER_LEN > len(buf):
        raise MochiError("FOOTER_INVALID", "footer does not fit at %d" % offset)
    if u32(buf, offset) != K_FOOTER or u32(buf, offset + 4) != 64:
        raise MochiError("FOOTER_INVALID", "footer skippable header invalid")
    p = offset + 8
    if bytes(buf[p:p + 8]) != FOOTER_MAGIC:
        raise MochiError("FOOTER_INVALID", "footer payload magic")
    return Footer(offset, u64(buf, p + 8), u64(buf, p + 16), u64(buf, p + 24), bytes(buf[p + 32:p + 64]))


def validate_footer(buf, offset: int) -> Footer:
    """Validate in R1 section 3 order.  Does not check the commit's own
    contents; the caller requires the frame kind and sequence."""
    f = parse_footer(buf, offset)
    c0, clen = f.commit_offset, f.commit_length
    if clen < 8 or c0 + clen > offset:     # at least a header; ends at or before the footer
        raise MochiError("FOOTER_INVALID", "commit range invalid")
    if clen > COMMIT_FRAME_LIMIT:
        raise MochiError("LIMIT_EXCEEDED", "commit frame length %d" % clen)
    # a skippable frame of exactly that stored length starts at the offset
    magic = u32(buf, c0)
    if not (0x184D2A50 <= magic <= 0x184D2A5F) or 8 + u32(buf, c0 + 4) != clen:
        raise MochiError("FOOTER_INVALID", "no skippable frame of that length at the commit offset")
    want = digest.h(digest.FOOTER, bytes(buf[offset + 8:offset + 8 + 32]), bytes(buf[c0:c0 + clen]))
    if want != f.digest:
        raise MochiError("FOOTER_INVALID", "footer digest mismatch")
    return f


def commit_frame_magic(buf, f: Footer) -> int:
    return u32(buf, f.commit_offset)


def require_commit_kind(buf, f: Footer) -> None:
    if commit_frame_magic(buf, f) != K_COMMIT:
        raise MochiError("FOOTER_INVALID", "footer names a frame that is not a commit record")
