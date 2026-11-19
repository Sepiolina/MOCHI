"""TAR-compatible profile checks (spec B.2.9 D19).

A strict pax/ustar reader written from B.2.9 item 4 and item 8.  The stdlib
``tarfile`` module is used as an independent second reader: whatever this
module accepts, ``tarfile`` must read to the same members.
"""
from __future__ import annotations

import io
import tarfile
from dataclasses import dataclass

from .errors import MochiError

BLOCK = 512
KNOWN_PAX = {b"path", b"size", b"uid", b"gid", b"mtime", b"hdrcharset"}


def fail(msg: str):
    raise MochiError("PROFILE_VIOLATION", msg)


@dataclass
class Member:
    path: bytes
    kind: str                 # 'file' | 'dir'
    size: int
    mode: int
    uid: int
    gid: int
    mtime: tuple              # (seconds, nanoseconds); (0, 0) when absent
    content: bytes


def _octal(field: bytes, what: str) -> int:
    s = bytes(field).rstrip(b"\x00 ").lstrip(b" ")
    if not s:
        return 0
    if any(c not in b"01234567" for c in s):
        fail(f"{what} is not octal")
    return int(s, 8)


def _checksum_ok(h: bytes) -> bool:
    want = _octal(h[148:156], "checksum")
    got = sum(h[:148]) + 8 * 32 + sum(h[156:])
    return want == got


def _cstr(field: bytes) -> bytes:
    i = field.find(b"\x00")
    return bytes(field if i < 0 else field[:i])


def _pax_records(content: bytes) -> dict:
    out: dict[bytes, bytes] = {}
    i = 0
    while i < len(content):
        sp = content.find(b" ", i)
        if sp < 0:
            fail("pax record without a length")
        try:
            n = int(content[i:sp])
        except ValueError:
            fail("pax record length not a number")
        if n < sp - i + 4 or i + n > len(content) or content[i + n - 1:i + n] != b"\n":
            fail("pax record length inconsistent")
        rec = content[sp + 1:i + n - 1]
        eq = rec.find(b"=")
        if eq <= 0:
            fail("pax record without a key")
        key, val = rec[:eq], rec[eq + 1:]
        if key not in KNOWN_PAX:
            fail("unknown pax keyword %r" % key)
        if key in out:
            fail("duplicate pax keyword")
        out[key] = val
        i += n
    return out


def _pax_time(val: bytes) -> tuple:
    s = val.decode("ascii", "strict")
    neg = s.startswith("-")
    if neg:
        s = s[1:]
    whole, _, frac = s.partition(".")
    if not whole.isdigit() or (frac and (not frac.isdigit() or len(frac) > 9 or frac.endswith("0"))):
        fail("pax mtime malformed")
    secs = int(whole)
    nanos = int(frac.ljust(9, "0")) if frac else 0
    if neg:
        # -a.b  is  -(a + 0.b):  floor seconds, positive nanoseconds
        if nanos:
            return (-secs - 1, 1_000_000_000 - nanos)
        return (-secs, 0)
    return (secs, nanos)


def parse_stream(data: bytes) -> list[Member]:
    if len(data) % BLOCK:
        fail("stream length is not a multiple of 512")
    members: list[Member] = []
    i = 0
    pax: dict | None = None
    zero = bytes(BLOCK)
    while True:
        if i + BLOCK > len(data):
            fail("stream ends without two zero end blocks")
        h = data[i:i + BLOCK]
        if h == zero:
            if i + 2 * BLOCK != len(data) or data[i + BLOCK:i + 2 * BLOCK] != zero:
                fail("end of stream is not exactly two zero blocks")
            break
        if not _checksum_ok(h):
            fail("header checksum mismatch")
        if h[257:263] != b"ustar\x00" or h[263:265] != b"00":
            fail("not a ustar/pax (POSIX) header")
        if _cstr(h[265:297]) or _cstr(h[297:329]) or h[345:500].strip(b"\x00"):
            fail("user/group names or prefix are not empty")
        if _octal(h[329:337], "devmajor") or _octal(h[337:345], "devminor"):
            fail("device fields set")
        typeflag = h[156:157]
        if typeflag not in (b"0", b"5", b"x"):
            fail("typeflag %r is not file, directory or pax header" % typeflag)
        if _cstr(h[157:257]):
            fail("link name set")
        size = _octal(h[124:136], "size")
        body_len = size
        body = data[i + BLOCK:i + BLOCK + body_len]
        padded = (body_len + BLOCK - 1) // BLOCK * BLOCK
        if i + BLOCK + padded > len(data):
            fail("member content runs past the stream")
        if data[i + BLOCK + body_len:i + BLOCK + padded].strip(b"\x00"):
            fail("non-zero padding")
        if typeflag == b"x":
            if pax is not None:
                fail("two extended headers in a row")
            if _cstr(h[0:100]) != b"PaxHeader":
                fail("extended header not named PaxHeader")
            pax = _pax_records(bytes(body))
            i += BLOCK + padded
            continue
        # a regular member
        rec = pax or {}
        pax = None
        if b"hdrcharset" in rec and rec[b"hdrcharset"] != b"BINARY":
            fail("unknown hdrcharset")
        name = _cstr(h[0:100])
        path = rec.get(b"path", name)
        kind = "dir" if typeflag == b"5" else "file"
        if kind == "dir":
            if size != 0 and b"size" not in rec:
                fail("directory with a size")
            path = path[:-1] if path.endswith(b"/") and len(path) > 1 else path
        if b"size" in rec:
            if not rec[b"size"].isdigit():
                fail("pax size not a number")
            psize = int(rec[b"size"])
            if size != 0:
                fail("ustar size set although a pax size is present")
            body_len = psize
            body = data[i + BLOCK:i + BLOCK + body_len]
            padded = (body_len + BLOCK - 1) // BLOCK * BLOCK
            if i + BLOCK + padded > len(data) or data[i + BLOCK + body_len:i + BLOCK + padded].strip(b"\x00"):
                fail("pax-sized member content malformed")
            size = psize
        mode = _octal(h[100:108], "mode")
        uid = int(rec[b"uid"]) if b"uid" in rec and rec[b"uid"].isdigit() else _octal(h[108:116], "uid")
        gid = int(rec[b"gid"]) if b"gid" in rec and rec[b"gid"].isdigit() else _octal(h[116:124], "gid")
        mt = _pax_time(rec[b"mtime"]) if b"mtime" in rec else (_octal(h[136:148], "mtime"), 0)
        members.append(Member(bytes(path), kind, size, mode, uid, gid, mt, bytes(body)))
        i += BLOCK + padded
    if pax is not None:
        fail("extended header without a member")
    # the stdlib reader must see the same members
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:", errorlevel=2) as tf:
            seen = [(ti.size, ti.isdir()) for ti in tf]
    except (tarfile.TarError, ValueError, EOFError) as e:
        fail("tarfile cannot read the stream: %s" % e)
    if seen != [(m.size, m.kind == "dir") for m in members]:
        fail("tarfile and the strict reader disagree on the members")
    return members


def expected_matches(m: Member, path: bytes, version) -> None:
    """B.2.9 item 8: path bytes, type, size, mode, uid, gid, mtime equal the put's."""
    if m.path != path:
        fail("member path differs from the put's")
    is_dir = version.kind == 1
    if (m.kind == "dir") != is_dir:
        fail("member type differs from the put's")
    if m.size != version.length:
        fail("member size differs from the file version's length")
    mode, uid, gid = version.posix if version.posix else ((0o755 if is_dir else 0o644), 0, 0)
    if (m.mode & 0o7777, m.uid, m.gid) != (mode, uid, gid):
        fail("member mode/uid/gid differ from the promised attributes")
    want_t = version.mtime if version.mtime else (0, 0)
    if m.mtime != want_t:
        fail("member mtime differs from the promised attributes")
