"""Digest scopes (R3 section 5).  BLAKE3-256 with ``MOCHI2-<SCOPE>`` + 0x00."""
from __future__ import annotations

from blake3 import blake3

FOOTER = b"MOCHI2-FOOTER\x00"
CHUNK_CONTENT = b"MOCHI2-CHUNK-CONTENT\x00"
STORED_OBJECT = b"MOCHI2-STORED-OBJECT\x00"
DICTIONARY = b"MOCHI2-DICTIONARY\x00"
COMMIT_ID = b"MOCHI2-COMMIT-ID\x00"
PARITY_SHARD = b"MOCHI2-PARITY-SHARD\x00"
PARITY_FRAME = b"MOCHI2-PARITY-FRAME\x00"
KEY_WRAP = b"MOCHI2-KEY-WRAP\x00"
OBJECT_SEAL = b"MOCHI2-OBJECT-SEAL\x00"
# FILE_CONTENT is deliberately unseparated: plain BLAKE3-256 (O20 via R3 section 5)


def h(domain: bytes, *parts: bytes) -> bytes:
    hasher = blake3(domain)
    for p in parts:
        hasher.update(p)
    return hasher.digest()


def stored_object(data) -> bytes:
    return h(STORED_OBJECT, bytes(data))


def chunk_content(data) -> bytes:
    return h(CHUNK_CONTENT, bytes(data))


def file_content(data) -> bytes:
    return blake3(bytes(data)).digest()
