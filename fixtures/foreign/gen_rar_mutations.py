#!/usr/bin/env python3
"""Byte-level mutations of the real RAR archives in rar/ (plan D8, slice 2).

    python3 -I fixtures/foreign/gen_rar_mutations.py

MOCHI never writes RAR (plan D8 rule 1) and no RAR writer is available to this
repository's CI, so there is no generator for RAR: the base archives in rar/
are real files copied from the `unrar` crate's test data (rar/PROVENANCE.txt)
and the hostile ones are made here by flipping or cutting bytes of them,
nothing more. In particular there is no RAR fixture with a traversal name, a
bomb, or a BLAKE2sp hash: those would need a RAR writer. Format-independent
name rules are exercised through ZIP, tar and 7z fixtures (the restore engine,
not the format reader, judges names).
"""
import os

D = os.path.join(os.path.dirname(os.path.abspath(__file__)), "rar")


def rd(n):
    with open(os.path.join(D, n), "rb") as f:
        return f.read()


def wr(n, b):
    with open(os.path.join(D, n), "wb") as f:
        f.write(b)


v = rd("version.rar")  # RAR4, one stored 11-byte file "VERSION", CRC32
i = v.find(b"VERSION")
data_at = len(v) - 7 - 11  # file data sits before the end-of-archive block
bad = bytearray(v)
bad[data_at + 2] ^= 0x01
wr("crc_corrupt.rar", bytes(bad))
wr("truncated.rar", v[: len(v) - 20])
hdr = bytearray(v)
hdr[7 + 13] ^= 0xFF  # the file header's own CRC16 field
wr("bad_header.rar", bytes(hdr))
s = rd("solid.rar")  # RAR5
bad5 = bytearray(s)
bad5[-30] ^= 0x01
wr("rar5_corrupt.rar", bytes(bad5))
wr("rar5_truncated.rar", s[: len(s) - 25])
print("ok")
