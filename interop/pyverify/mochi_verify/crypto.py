"""Encrypted profile (spec B.2.10 D20, R5).

XChaCha20-Poly1305 is taken from PyNaCl (libsodium); Argon2id from argon2-cffi.
Everything else (associated data, key envelope, sealed-object layout) is from R5.
"""
from __future__ import annotations

import unicodedata

from . import cbor, digest
from .errors import MochiError
from .schema import Schema

MAX_KDF_MEMORY_KIB = 1_048_576
MAX_KDF_ITERATIONS = 16
MAX_KDF_LANES = 16
MAX_PASSPHRASE = 4096


def passphrase_bytes(text: str) -> bytes:
    """UTF-8 of the NFC-normalized text; 1..4096 bytes (R5 section 1)."""
    b = unicodedata.normalize("NFC", text).encode("utf-8")
    if not 1 <= len(b) <= MAX_PASSPHRASE:
        raise MochiError("INVALID_ARGUMENT", "passphrase must be 1 to 4096 bytes after normalization")
    return b


def xchacha_open(key: bytes, nonce: bytes, ciphertext_and_tag: bytes, aad: bytes) -> bytes | None:
    """Return the plaintext, or None when authentication fails."""
    from nacl import bindings, exceptions
    try:
        return bindings.crypto_aead_xchacha20poly1305_ietf_decrypt(ciphertext_and_tag, aad, nonce, key)
    except exceptions.CryptoError:
        return None


def xchacha_seal(key: bytes, nonce: bytes, plaintext: bytes, aad: bytes) -> bytes:
    from nacl import bindings
    return bindings.crypto_aead_xchacha20poly1305_ietf_encrypt(plaintext, aad, nonce, key)


def argon2id(passphrase: bytes, salt: bytes, m_kib: int, t: int, p: int) -> bytes:
    from argon2.low_level import Type, hash_secret_raw
    return hash_secret_raw(passphrase, salt, time_cost=t, memory_cost=m_kib, parallelism=p,
                           hash_len=32, type=Type.ID, version=19)


class KeyEnvelope:
    def __init__(self, v: dict):
        self.archive_id = v[1]
        self.sequence = v[2]
        self.transaction_id = v[3]
        self.envelope_id = v[5]
        self.key_id = v[6]
        self.suite = v[7]
        self.kdf = v[8]
        self.kdf_bytes = cbor.encode(v[8])
        self.nonce = v[9]
        self.wrapped = v[10]


def decode_envelope_payload(payload: bytes) -> KeyEnvelope:
    S = Schema("RECORD_INVALID", "key envelope")
    v = cbor.decode(payload)
    if not isinstance(v, dict) or 0 not in v:
        S.fail("not a map with a schema version")
    if v[0] != 0 or isinstance(v[0], bool):
        raise MochiError("UNSUPPORTED_FEATURE", f"key envelope schema version {v[0]!r}")
    S.map(v, "key envelope", tuple(range(11)))
    S.bstr(v[1], "archive id", 32)
    S.uint(v[2], "sequence")
    S.bstr(v[3], "transaction id", 16)
    S.exactly(v[4], [1], "required features")
    S.bstr(v[5], "envelope id", 16)
    S.bstr(v[6], "key id", 16)
    if S.uint(v[7], "suite") != 1:
        raise MochiError("UNSUPPORTED_FEATURE", f"suite {v[7]}")
    k = S.map(v[8], "kdf", tuple(range(6)))
    if S.uint(k[0], "kdf id") != 1:
        raise MochiError("UNSUPPORTED_FEATURE", f"kdf {k[0]}")
    if S.uint(k[1], "argon2 version") != 19:
        raise MochiError("UNSUPPORTED_FEATURE", f"argon2 version {k[1]}")
    S.uint(k[2], "memory")
    S.uint(k[3], "iterations")
    S.uint(k[4], "lanes")
    S.bstr(k[5], "salt", 16)
    S.bstr(v[9], "wrap nonce", 24)
    S.bstr(v[10], "wrapped key", 48)
    return KeyEnvelope(v)


def check_kdf_limits(kdf: dict) -> None:
    """Before any Argon2 memory is allocated (R5 section 3)."""
    m, t, p = kdf[2], kdf[3], kdf[4]
    if m > MAX_KDF_MEMORY_KIB or t > MAX_KDF_ITERATIONS or p > MAX_KDF_LANES:
        raise MochiError("LIMIT_EXCEEDED", f"KDF parameters m={m} t={t} p={p} above the reader limits")
    if p < 1 or t < 1 or m < 8 * p:
        raise MochiError("RECORD_INVALID", "KDF cost fields below their lower bounds")


def wrap_aad(env: KeyEnvelope) -> bytes:
    return (digest.KEY_WRAP + env.archive_id + env.envelope_id + env.key_id
            + (1).to_bytes(2, "little") + env.kdf_bytes)


def unwrap(env: KeyEnvelope, passphrase: bytes) -> bytes | None:
    """Return the data key, or None when the passphrase does not open it."""
    check_kdf_limits(env.kdf)
    kek = argon2id(passphrase, env.kdf[5], env.kdf[2], env.kdf[3], env.kdf[4])
    return xchacha_open(kek, env.nonce, env.wrapped, wrap_aad(env))


def object_aad(archive_id: bytes, header24: bytes, binding: bytes) -> bytes:
    return digest.OBJECT_SEAL + archive_id + header24 + binding


def binding_for(kind: int, object_id: bytes | None, sequence: int | None, txid: bytes | None) -> bytes:
    if kind == 0:
        return object_id
    return sequence.to_bytes(8, "little") + txid


def open_sealed(dek: bytes, archive_id: bytes, sh, binding: bytes) -> bytes | None:
    aad = object_aad(archive_id, sh.header, binding)
    return xchacha_open(dek, sh.nonce, sh.ciphertext + sh.tag, aad)


def hchacha20(key: bytes, nonce16: bytes) -> bytes:
    """HChaCha20 (draft-irtf-cfrg-xchacha section 2.2), used only to cross-check the AEAD."""
    def rotl(v, c):
        return ((v << c) & 0xFFFFFFFF) | (v >> (32 - c))

    st = [0x61707865, 0x3320646E, 0x79622D32, 0x6B206574]
    st += [int.from_bytes(key[i:i + 4], "little") for i in range(0, 32, 4)]
    st += [int.from_bytes(nonce16[i:i + 4], "little") for i in range(0, 16, 4)]

    def qr(a, b, c, d):
        st[a] = (st[a] + st[b]) & 0xFFFFFFFF; st[d] = rotl(st[d] ^ st[a], 16)
        st[c] = (st[c] + st[d]) & 0xFFFFFFFF; st[b] = rotl(st[b] ^ st[c], 12)
        st[a] = (st[a] + st[b]) & 0xFFFFFFFF; st[d] = rotl(st[d] ^ st[a], 8)
        st[c] = (st[c] + st[d]) & 0xFFFFFFFF; st[b] = rotl(st[b] ^ st[c], 7)

    for _ in range(10):
        qr(0, 4, 8, 12); qr(1, 5, 9, 13); qr(2, 6, 10, 14); qr(3, 7, 11, 15)
        qr(0, 5, 10, 15); qr(1, 6, 11, 12); qr(2, 7, 8, 13); qr(3, 4, 9, 14)
    out = st[0:4] + st[12:16]
    return b"".join(w.to_bytes(4, "little") for w in out)
