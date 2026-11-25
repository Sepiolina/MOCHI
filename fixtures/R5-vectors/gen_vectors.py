#!/usr/bin/env python3
"""Generate the R5 test vectors (spec Annex B.2.10, D20) into vectors.json.

Every value is deterministic: fixed keys, nonces, and salts, so the output is
byte-for-byte reproducible. The implementation PR's tests read vectors.json;
nothing here is the implementation's own code (plan section 2: independent
oracles).

Oracles, each independent of the Rust crates the implementation will use:
  * XChaCha20-Poly1305: libsodium (PyNaCl), cross-checked against OpenSSL's
    ChaCha20-Poly1305 (`cryptography`) fed with this file's own HChaCha20.
  * Argon2id: the Argon2 reference C library (argon2-cffi bindings).
  * BLAKE3: the `blake3` Python package (the reference implementation's port).
  * Deterministic CBOR (spec B.1 D2): the small encoder below, cross-checked
    against cbor2's canonical mode.
  * zstd: python-zstandard (libzstd).

Run:  pip install pynacl cryptography argon2-cffi blake3 zstandard cbor2
      python3 gen_vectors.py > vectors.json
"""
import json
import struct
import sys
import unicodedata

import blake3
import cbor2
import zstandard
from _argon2_cffi_bindings import ffi, lib
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from nacl.bindings import (
    crypto_aead_xchacha20poly1305_ietf_decrypt as xc_open,
    crypto_aead_xchacha20poly1305_ietf_encrypt as xc_seal,
)

# --- constants fixed by the spec (mochi-format registry.rs / digest.rs) -------
ENCRYPTED_OBJECT = 0x184D2A59
KEY_ENVELOPE = 0x184D2A5C
COMMIT_RECORD = 0x184D2A51
ARCHIVE_DESCRIPTOR = 0x184D2A57
STORED_OBJECT_DOMAIN = b"MOCHI2-STORED-OBJECT\0"
CHUNK_CONTENT_DOMAIN = b"MOCHI2-CHUNK-CONTENT\0"
COMMIT_ID_DOMAIN = b"MOCHI2-COMMIT-ID\0"
# New in D20 (defined in mochi-format/src/digest.rs by the implementation).
KEY_WRAP_DOMAIN = b"MOCHI2-KEY-WRAP\0"
OBJECT_SEAL_DOMAIN = b"MOCHI2-OBJECT-SEAL\0"

FEATURE_ENCRYPTED_XCHACHA = 1  # first required-feature identifier (D20)
SUITE_XCHACHA20POLY1305 = 1
KDF_ARGON2ID = 1
ARGON2_VERSION = 0x13

KIND_DATA, KIND_IMAGE, KIND_DELTA_MANIFEST, KIND_SNAPSHOT_MANIFEST = 0, 1, 2, 3


def h(b):
    return b.hex()


# --- deterministic CBOR (spec B.1 D2 subset) ----------------------------------
def head(major, n):
    if n < 24:
        return bytes([major << 5 | n])
    for ai, width in ((24, 1), (25, 2), (26, 4), (27, 8)):
        if n < 1 << (8 * width):
            return bytes([major << 5 | ai]) + n.to_bytes(width, "big")
    raise ValueError(n)


def cbor(v):
    if v is None:
        return b"\xf6"
    if v is True:
        return b"\xf5"
    if v is False:
        return b"\xf4"
    if isinstance(v, int):
        return head(0, v) if v >= 0 else head(1, -1 - v)
    if isinstance(v, bytes):
        return head(2, len(v)) + v
    if isinstance(v, str):
        e = v.encode()
        return head(3, len(e)) + e
    if isinstance(v, list):
        return head(4, len(v)) + b"".join(cbor(x) for x in v)
    if isinstance(v, dict):
        # unsigned-integer keys in canonical (numeric) order
        assert all(isinstance(k, int) and k >= 0 for k in v)
        return head(5, len(v)) + b"".join(cbor(k) + cbor(v[k]) for k in sorted(v))
    raise TypeError(type(v))


def cbor_checked(v):
    mine = cbor(v)
    assert mine == cbor2.dumps(v, canonical=True), "encoder disagrees with cbor2"
    return mine


# --- hashes ---------------------------------------------------------------------
def scoped(domain, data):
    k = blake3.blake3()
    k.update(domain)
    k.update(data)
    return k.digest()


def stored_hash(frame):
    return scoped(STORED_OBJECT_DOMAIN, frame)


def skippable(magic, payload):
    return struct.pack("<II", magic, len(payload)) + payload


# --- primitives -----------------------------------------------------------------
def argon2id(password, salt, m_kib, t, p, out=32, secret=b"", ad=b""):
    pw = ffi.new("uint8_t[]", password or b"\0")
    sa = ffi.new("uint8_t[]", salt)
    se = ffi.new("uint8_t[]", secret or b"\0")
    a = ffi.new("uint8_t[]", ad or b"\0")
    o = ffi.new("uint8_t[]", out)
    c = ffi.new("argon2_context *")
    c.out, c.outlen = o, out
    c.pwd, c.pwdlen = pw, len(password)
    c.salt, c.saltlen = sa, len(salt)
    c.secret, c.secretlen = se, len(secret)
    c.ad, c.adlen = a, len(ad)
    c.t_cost, c.m_cost, c.lanes, c.threads = t, m_kib, p, p
    c.version = ARGON2_VERSION
    c.allocate_cbk = ffi.NULL
    c.free_cbk = ffi.NULL
    c.flags = 0
    rc = lib.argon2_ctx(c, lib.Argon2_id)
    assert rc == 0, rc
    return bytes(ffi.buffer(o, out))


def rotl(x, n):
    return ((x << n) & 0xFFFFFFFF) | (x >> (32 - n))


def hchacha20(key, nonce16):
    s = list(struct.unpack("<4I", b"expand 32-byte k"))
    s += list(struct.unpack("<8I", key)) + list(struct.unpack("<4I", nonce16))

    def qr(a, b, c, d):
        s[a] = (s[a] + s[b]) & 0xFFFFFFFF; s[d] = rotl(s[d] ^ s[a], 16)
        s[c] = (s[c] + s[d]) & 0xFFFFFFFF; s[b] = rotl(s[b] ^ s[c], 12)
        s[a] = (s[a] + s[b]) & 0xFFFFFFFF; s[d] = rotl(s[d] ^ s[a], 8)
        s[c] = (s[c] + s[d]) & 0xFFFFFFFF; s[b] = rotl(s[b] ^ s[c], 7)

    for _ in range(10):
        qr(0, 4, 8, 12); qr(1, 5, 9, 13); qr(2, 6, 10, 14); qr(3, 7, 11, 15)
        qr(0, 5, 10, 15); qr(1, 6, 11, 12); qr(2, 7, 8, 13); qr(3, 4, 9, 14)
    return struct.pack("<8I", *(s[0:4] + s[12:16]))


def xchacha_via_openssl(key, nonce24, pt, aad):
    """XChaCha20-Poly1305 built from HChaCha20 + OpenSSL's ChaCha20-Poly1305."""
    sub = hchacha20(key, nonce24[:16])
    return ChaCha20Poly1305(sub).encrypt(b"\0\0\0\0" + nonce24[16:], pt, aad)


def seal_raw(key, nonce, pt, aad):
    ct = xc_seal(pt, aad, nonce, key)
    assert ct == xchacha_via_openssl(key, nonce, pt, aad), "oracles disagree"
    assert xc_open(ct, aad, nonce, key) == pt
    return ct


# --- fixed inputs -----------------------------------------------------------------
ARCHIVE_ID = bytes(range(0x00, 0x20))
TXID = bytes.fromhex("00112233445566774899aabbccddeeff")  # RFC 9562 v4 bits
KEY_ID = bytes(range(0xC0, 0xD0))
ENVELOPE_ID = bytes(range(0xD0, 0xE0))
DEK = bytes(range(0x80, 0xA0))
SALT = bytes(range(0x50, 0x60))
WRAP_NONCE = bytes(range(0x60, 0x78))
PASSPHRASE = "correct horse battery staple"
SMALL = dict(m_kib=64, t=2, p=1)            # test-only parameters
DEFAULT = dict(m_kib=65536, t=3, p=4)       # writer defaults (D20 item 1)


def nonce_n(i):
    return bytes((0xA0 + i + j) & 0xFF for j in range(24))


# --- 1-4: published and derived primitive vectors ---------------------------------
def primitive_vectors():
    v = {}
    # draft-irtf-cfrg-xchacha-03, A.3.1
    key = bytes(range(0x80, 0xA0))
    nonce = bytes(range(0x40, 0x58))
    aad = bytes.fromhex("50515253c0c1c2c3c4c5c6c7")
    pt = (b"Ladies and Gentlemen of the class of '99: If I could offer you only "
          b"one tip for the future, sunscreen would be it.")
    ct = seal_raw(key, nonce, pt, aad)
    assert ct[:16].hex() == "bd6d179d3e83d43b9576579493c0e939"
    assert ct[-16:].hex() == "c0875924c1c7987947deafd8780acf49"
    v["xchacha20poly1305_draft_irtf_cfrg_xchacha_a31"] = {
        "source": "draft-irtf-cfrg-xchacha, Appendix A.3.1",
        "key": h(key), "nonce": h(nonce), "aad": h(aad), "plaintext": h(pt),
        "ciphertext_and_tag": h(ct),
    }
    # draft-irtf-cfrg-xchacha-03, 2.2.1 HChaCha20 test vector
    hk = bytes(range(0x00, 0x20))
    hn = bytes.fromhex("000000090000004a0000000031415927")
    sub = hchacha20(hk, hn)
    assert sub.hex() == "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc"
    v["hchacha20_draft_irtf_cfrg_xchacha_221"] = {
        "source": "draft-irtf-cfrg-xchacha, Section 2.2.1",
        "key": h(hk), "nonce": h(hn), "subkey": h(sub),
    }
    # RFC 9106, Section 5.3 (Argon2id, with secret and associated data)
    tag = argon2id(b"\x01" * 32, b"\x02" * 16, 32, 3, 4, secret=b"\x03" * 8, ad=b"\x04" * 12)
    assert tag.hex() == "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
    v["argon2id_rfc9106_53"] = {
        "source": "RFC 9106, Section 5.3 (Argon2id)",
        "password": h(b"\x01" * 32), "salt": h(b"\x02" * 16), "secret": h(b"\x03" * 8),
        "associated_data": h(b"\x04" * 12), "memory_kib": 32, "iterations": 3,
        "lanes": 4, "version": ARGON2_VERSION, "tag_length": 32, "tag": h(tag),
        "note": "MOCHI uses neither a secret nor associated data; the vector checks the Argon2id core.",
    }
    # MOCHI KDF: NFC normalization, then Argon2id (D20 items 1 and 2)
    nfc = unicodedata.normalize("NFC", "Ångström")
    nfd = unicodedata.normalize("NFD", nfc)
    assert nfc != nfd
    kek_nfc = argon2id(nfc.encode(), SALT, **SMALL)
    kek_nfd_raw = argon2id(nfd.encode(), SALT, **SMALL)
    assert kek_nfc != kek_nfd_raw
    v["kdf_nfc"] = {
        "salt": h(SALT), **{k: x for k, x in SMALL.items()},
        "passphrase_nfc_utf8": h(nfc.encode()), "passphrase_nfd_utf8": h(nfd.encode()),
        "kek_from_normalized": h(kek_nfc),
        "kek_from_unnormalized_nfd": h(kek_nfd_raw),
        "rule": "A reader and a writer MUST derive from the NFC bytes, so both inputs give kek_from_normalized.",
    }
    v["kdf_writer_defaults"] = {
        "passphrase_utf8": h(PASSPHRASE.encode()), "salt": h(SALT), **DEFAULT,
        "kek": h(argon2id(PASSPHRASE.encode(), SALT, **DEFAULT)),
    }
    return v


# --- 5: key envelope --------------------------------------------------------------
def kdf_map(params):
    return {0: KDF_ARGON2ID, 1: ARGON2_VERSION, 2: params["m_kib"], 3: params["t"],
            4: params["p"], 5: SALT}


def key_envelope(params, seq=0, tx=TXID, env_id=ENVELOPE_ID, passphrase=PASSPHRASE):
    kdf = kdf_map(params)
    kdf_bytes = cbor_checked(kdf)
    kek = argon2id(unicodedata.normalize("NFC", passphrase).encode(), SALT, **params)
    aad = (KEY_WRAP_DOMAIN + ARCHIVE_ID + env_id + KEY_ID
           + struct.pack("<H", SUITE_XCHACHA20POLY1305) + kdf_bytes)
    wrapped = seal_raw(kek, WRAP_NONCE, DEK, aad)
    assert len(wrapped) == 48
    body = {0: 0, 1: ARCHIVE_ID, 2: seq, 3: tx, 4: [FEATURE_ENCRYPTED_XCHACHA],
            5: env_id, 6: KEY_ID, 7: SUITE_XCHACHA20POLY1305, 8: kdf, 9: WRAP_NONCE,
            10: wrapped}
    payload = cbor_checked(body)
    frame = skippable(KEY_ENVELOPE, payload)
    return dict(kek=kek, kdf_bytes=kdf_bytes, aad=aad, wrapped=wrapped, payload=payload,
                frame=frame)


def key_envelope_vector(params, label):
    e = key_envelope(params)
    return {
        "label": label,
        "inputs": {
            "archive_id": h(ARCHIVE_ID), "envelope_id": h(ENVELOPE_ID), "key_id": h(KEY_ID),
            "dek": h(DEK), "passphrase_utf8": h(PASSPHRASE.encode()), "salt": h(SALT),
            "wrap_nonce": h(WRAP_NONCE), "introducing_sequence": 0,
            "introducing_transaction_id": h(TXID), **params,
        },
        "kek": h(e["kek"]),
        "canonical_kdf_parameter_bytes": h(e["kdf_bytes"]),
        "wrap_aad": h(e["aad"]),
        "wrapped_dek_and_tag": h(e["wrapped"]),
        "cbor_payload": h(e["payload"]),
        "frame": h(e["frame"]),
        "stored_object_hash": h(stored_hash(e["frame"])),
    }


# --- 6: sealed objects ------------------------------------------------------------
def seal_object(kind, plaintext, nonce, binding):
    """Sealed object v0: 48-byte header, ciphertext, 16-byte tag, in one 0x184D2A59 frame."""
    header_no_nonce = struct.pack("<HHI", 0, SUITE_XCHACHA20POLY1305, kind) + KEY_ID
    assert len(header_no_nonce) == 24
    aad = OBJECT_SEAL_DOMAIN + ARCHIVE_ID + header_no_nonce + binding
    ct = seal_raw(DEK, nonce, plaintext, aad)
    payload = header_no_nonce + nonce + ct
    assert len(payload) == 48 + len(plaintext) + 16
    frame = skippable(ENCRYPTED_OBJECT, payload)
    return dict(aad=aad, payload=payload, frame=frame)


def binding_meta(seq, tx):
    return struct.pack("<Q", seq) + tx


def sealed_vector(label, kind, plaintext, nonce, binding, extra=None):
    s = seal_object(kind, plaintext, nonce, binding)
    d = {
        "label": label, "kind": kind, "dek": h(DEK), "key_id": h(KEY_ID),
        "archive_id": h(ARCHIVE_ID), "nonce": h(nonce), "binding": h(binding),
        "plaintext": h(plaintext), "aad": h(s["aad"]), "payload": h(s["payload"]),
        "frame": h(s["frame"]), "stored_object_hash": h(stored_hash(s["frame"])),
    }
    d.update(extra or {})
    return d, s


def zstd_frame(data):
    c = zstandard.ZstdCompressor(level=3, write_content_size=True, write_checksum=True)
    return c.compress(data)


# --- 7: a whole commit, layout only -----------------------------------------------
def ref(offset, length, digest):
    return {0: offset, 1: length, 2: digest}


def layout_vector():
    objs = []  # (name, frame)
    off = 0

    def put(name, frame):
        nonlocal off
        objs.append((name, off, frame))
        off += len(frame)
        return ref(off - len(frame), len(frame), stored_hash(frame))

    descriptor_payload = cbor_checked(
        {0: 0, 1: ARCHIVE_ID, 2: 2, 3: 1, 4: [FEATURE_ENCRYPTED_XCHACHA], 5: {0: False}})
    r_desc = put("descriptor", skippable(ARCHIVE_DESCRIPTOR, descriptor_payload))

    env = key_envelope(DEFAULT)
    r_env = put("key_envelope", env["frame"])

    chunk_a = b"mochi encrypted chunk A\n" * 40
    chunk_b = b"mochi encrypted chunk B\n" * 25
    oid_a = scoped(b"MOCHI2-TEST-OBJECT-ID\0", b"A")   # synthetic IDs for the vector only
    oid_b = scoped(b"MOCHI2-TEST-OBJECT-ID\0", b"B")
    sa = seal_object(KIND_DATA, zstd_frame(chunk_a), nonce_n(1), oid_a)
    sb = seal_object(KIND_DATA, zstd_frame(chunk_b), nonce_n(2), oid_b)
    region_start = off
    put("data_chunk_a", sa["frame"])
    put("data_chunk_b", sb["frame"])
    region_len = off - region_start
    region_bytes = sa["frame"] + sb["frame"]
    region = ref(region_start, region_len, stored_hash(region_bytes))

    manifest_plain = cbor_checked({
        0: 3, 1: ARCHIVE_ID, 2: 0, 3: None, 4: 0, 5: [], 6: [], 7: [], 8: [],
        9: [FEATURE_ENCRYPTED_XCHACHA], 10: TXID, 11: [], 13: [[0, ENVELOPE_ID]]})
    sm = seal_object(KIND_DELTA_MANIFEST, manifest_plain, nonce_n(3), binding_meta(0, TXID))
    r_man = put("delta_manifest", sm["frame"])
    si = seal_object(KIND_IMAGE, b"PLACEHOLDER: not a real catalog image", nonce_n(4),
                     binding_meta(0, TXID))
    r_img = put("catalog_image_placeholder", si["frame"])
    ss = seal_object(KIND_SNAPSHOT_MANIFEST, b"PLACEHOLDER: not a real snapshot", nonce_n(5),
                     binding_meta(0, TXID))
    r_snap = put("snapshot_manifest_placeholder", ss["frame"])

    body = {
        0: 2, 1: ARCHIVE_ID, 2: 0, 3: TXID, 4: None,
        5: {0: 0, 1: r_img, 2: r_snap}, 6: r_man, 7: [FEATURE_ENCRYPTED_XCHACHA],
        10: r_desc, 11: [r_env], 12: region,
    }
    body_bytes = cbor_checked(body)
    cid = scoped(COMMIT_ID_DOMAIN, body_bytes)
    full = dict(body)
    full[9] = cid
    commit_payload = cbor_checked(full)
    commit_frame = skippable(COMMIT_RECORD, commit_payload)
    put("commit_record_v2", commit_frame)
    archive = b"".join(f for _, _, f in objs)
    return {
        "note": ("Layout vector. Descriptor, envelope, sealed chunks, delta manifest and commit "
                 "record are real encodings; the catalog image and snapshot manifest plaintexts "
                 "are placeholders, and the object IDs are synthetic. There is no footer."),
        "objects": [{"name": n, "offset": o, "length": len(f), "frame": h(f)} for n, o, f in objs],
        "data_region": {"offset": region_start, "length": region_len,
                        "hash": h(region[2])},
        "commit_body_canonical_cbor": h(body_bytes),
        "commit_id": h(cid),
        "commit_payload": h(commit_payload),
        "commit_frame": h(commit_frame),
        "bytes_before_footer": h(archive),
        "delta_manifest_plaintext": h(manifest_plain),
        "synthetic_object_ids": {"a": h(oid_a), "b": h(oid_b)},
        "chunk_a_zstd_frame": h(zstd_frame(chunk_a)),
        "chunk_a_content_hash": h(scoped(CHUNK_CONTENT_DOMAIN, chunk_a)),
    }


def main():
    out = {
        "about": ("R5 draft vectors for spec Annex B.2.10 (D20). Generated by gen_vectors.py; "
                  "see docs/ratification/R5-crypto-draft.md. DRAFT, not frozen."),
        "constants": {
            "frame_encrypted_object": ENCRYPTED_OBJECT, "frame_key_envelope": KEY_ENVELOPE,
            "key_wrap_domain": h(KEY_WRAP_DOMAIN), "object_seal_domain": h(OBJECT_SEAL_DOMAIN),
            "feature_encrypted_xchacha20poly1305_argon2id": FEATURE_ENCRYPTED_XCHACHA,
            "suite_xchacha20poly1305": SUITE_XCHACHA20POLY1305,
        },
        "primitives": primitive_vectors(),
        "key_envelope_small_params": key_envelope_vector(SMALL, "test-only Argon2id parameters"),
        "key_envelope_writer_defaults": key_envelope_vector(DEFAULT, "writer default parameters"),
    }
    chunk = zstd_frame(b"hello, sealed chunk\n" * 20)
    oid = scoped(b"MOCHI2-TEST-OBJECT-ID\0", b"vector-object")
    d, _ = sealed_vector("data chunk (kind 0): plaintext is one zstd frame", KIND_DATA, chunk,
                         nonce_n(0), oid,
                         {"object_id": h(oid), "decoded_content_hash": h(scoped(
                             CHUNK_CONTENT_DOMAIN, b"hello, sealed chunk\n" * 20))})
    out["sealed_data_chunk"] = d
    d, _ = sealed_vector("delta manifest (kind 2): bound to sequence and transaction ID",
                         KIND_DELTA_MANIFEST, b"synthetic manifest plaintext", nonce_n(9),
                         binding_meta(7, TXID), {"sequence": 7, "transaction_id": h(TXID)})
    out["sealed_manifest"] = d
    # A reject vector: the same ciphertext opened with a different object ID must fail.
    good, s = sealed_vector("(internal)", KIND_DATA, chunk, nonce_n(0), oid)
    wrong_oid = bytes(32)
    wrong_aad = OBJECT_SEAL_DOMAIN + ARCHIVE_ID + s["payload"][:24] + wrong_oid
    try:
        xc_open(s["payload"][48:], wrong_aad, s["payload"][24:48], DEK)
        raise SystemExit("wrong binding opened")
    except Exception as ex:  # nacl raises CryptoError
        assert "decrypt" in str(ex).lower() or "Decryption" in str(ex)
    out["sealed_data_chunk"]["reject_wrong_object_id"] = {
        "object_id_used_for_aad": h(wrong_oid),
        "expected": "authentication failure (CONTENT_INTEGRITY_FAILED if the stored hash verified)",
    }
    out["commit_layout"] = layout_vector()
    json.dump(out, sys.stdout, indent=2, sort_keys=False)
    print()


if __name__ == "__main__":
    main()
