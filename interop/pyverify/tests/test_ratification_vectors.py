"""R5 and R6 vectors (docs/ratification/R5-vectors, R6-vectors) reproduce."""
import unicodedata

import pytest

from mochi_verify import cbor, commit, crypto, descriptor, digest, envelope, frames, parity
from mochi_verify.errors import MochiError

B = bytes.fromhex


# ----------------------------------------------------------------------------- R5
def test_r5_xchacha20poly1305_cfrg_vector(r5):
    v = r5["primitives"]["xchacha20poly1305_draft_irtf_cfrg_xchacha_a31"]
    key, nonce, aad, pt = B(v["key"]), B(v["nonce"]), B(v["aad"]), B(v["plaintext"])
    ct = B(v["ciphertext_and_tag"])
    assert crypto.xchacha_seal(key, nonce, pt, aad) == ct
    assert crypto.xchacha_open(key, nonce, ct, aad) == pt
    flipped = bytes([ct[0] ^ 1]) + ct[1:]
    assert crypto.xchacha_open(key, nonce, flipped, aad) is None


def test_r5_hchacha20_and_the_construction(r5):
    h = r5["primitives"]["hchacha20_draft_irtf_cfrg_xchacha_221"]
    assert crypto.hchacha20(B(h["key"]), B(h["nonce"])).hex() == h["subkey"]
    # XChaCha20-Poly1305 = ChaCha20-Poly1305(HChaCha20(key, nonce[:16]), 00000000 || nonce[16:])
    from nacl import bindings
    v = r5["primitives"]["xchacha20poly1305_draft_irtf_cfrg_xchacha_a31"]
    sub = crypto.hchacha20(B(v["key"]), B(v["nonce"])[:16])
    ct = bindings.crypto_aead_chacha20poly1305_ietf_encrypt(B(v["plaintext"]), B(v["aad"]),
                                                            b"\0\0\0\0" + B(v["nonce"])[16:], sub)
    assert ct.hex() == v["ciphertext_and_tag"]


def test_r5_nfc_rule(r5):
    v = r5["primitives"]["kdf_nfc"]
    nfc, nfd = B(v["passphrase_nfc_utf8"]), B(v["passphrase_nfd_utf8"])
    assert crypto.passphrase_bytes(nfd.decode()) == nfc == crypto.passphrase_bytes(nfc.decode())
    assert nfd != nfc and unicodedata.normalize("NFC", nfd.decode()).encode() == nfc
    salt = B(v["salt"])
    assert crypto.argon2id(nfc, salt, v["m_kib"], v["t"], v["p"]).hex() == v["kek_from_normalized"]
    assert crypto.argon2id(nfd, salt, v["m_kib"], v["t"], v["p"]).hex() == v["kek_from_unnormalized_nfd"]


def test_r5_kdf_writer_defaults(r5):
    v = r5["primitives"]["kdf_writer_defaults"]
    assert crypto.argon2id(B(v["passphrase_utf8"]), B(v["salt"]), v["m_kib"], v["t"], v["p"]).hex() == v["kek"]


@pytest.mark.parametrize("name", ["key_envelope_small_params", "key_envelope_writer_defaults"])
def test_r5_key_envelope(r5, name):
    v = r5[name]
    i = v["inputs"]
    kek = crypto.argon2id(B(i["passphrase_utf8"]), B(i["salt"]), i["m_kib"], i["t"], i["p"])
    assert kek.hex() == v["kek"]
    payload = B(v["cbor_payload"])
    env = crypto.decode_envelope_payload(payload)
    assert env.kdf_bytes.hex() == v["canonical_kdf_parameter_bytes"]
    assert crypto.wrap_aad(env).hex() == v["wrap_aad"]
    assert crypto.xchacha_seal(kek, B(i["wrap_nonce"]), B(i["dek"]), crypto.wrap_aad(env)).hex() == v["wrapped_dek_and_tag"]
    assert crypto.unwrap(env, B(i["passphrase_utf8"])) == B(i["dek"])
    assert crypto.unwrap(env, b"wrong passphrase") is None
    frame = B(v["frame"])
    assert frame == frames.K_KEY_ENVELOPE.to_bytes(4, "little") + len(payload).to_bytes(4, "little") + payload
    assert digest.stored_object(frame).hex() == v["stored_object_hash"]
    assert cbor.encode(cbor.decode(payload)) == payload


@pytest.mark.parametrize("name", ["sealed_data_chunk", "sealed_manifest"])
def test_r5_sealed_objects(r5, name):
    v = r5[name]
    frame = B(v["frame"])
    assert digest.stored_object(frame).hex() == v["stored_object_hash"]
    fr = frames.walk_frame(frame, 0)
    assert fr.length == len(frame) and fr.magic == frames.K_SEALED
    sh = envelope.parse_sealed(frame[8:])
    assert sh.kind == v["kind"] and sh.key_id.hex() == v["key_id"] and sh.nonce.hex() == v["nonce"]
    aid = B(v["archive_id"])
    binding = B(v["binding"])
    aad = crypto.object_aad(aid, sh.header, binding)
    assert aad.hex() == v["aad"]
    pt = crypto.open_sealed(B(v["dek"]), aid, sh, binding)
    assert pt is not None and pt.hex() == v["plaintext"]
    if name == "sealed_data_chunk":
        assert digest.chunk_content(__import__("zstandard").ZstdDecompressor().decompress(pt)).hex() == v["decoded_content_hash"]
        bad = crypto.open_sealed(B(v["dek"]), aid, sh, B(v["reject_wrong_object_id"]["object_id_used_for_aad"]))
        assert bad is None
        assert crypto.binding_for(0, B(v["object_id"]), None, None) == binding
    else:
        assert crypto.binding_for(2, None, v["sequence"], B(v["transaction_id"])) == binding


def test_r5_commit_layout(r5):
    c = r5["commit_layout"]
    objs = {o["name"]: o for o in c["objects"]}
    for o in c["objects"]:
        assert B(o["frame"]).__len__() == o["length"]
    prefix = B(c["bytes_before_footer"])
    for o in c["objects"]:
        assert prefix[o["offset"]:o["offset"] + o["length"]] == B(o["frame"])
    region = c["data_region"]
    assert digest.stored_object(prefix[region["offset"]:region["offset"] + region["length"]]).hex() == region["hash"]
    payload = B(c["commit_payload"])
    cm = commit.decode_payload(payload)
    assert cm.commit_id.hex() == c["commit_id"]
    assert cm.data_region.offset == region["offset"] and cm.data_region.length == region["length"]
    assert cm.data_region.hash.hex() == region["hash"]
    assert cm.schema == 2 and cm.features == [1] and len(cm.key_envelopes) == 1
    body = cbor.decode(payload)
    del body[9]
    assert cbor.encode(body).hex() == c["commit_body_canonical_cbor"]
    assert digest.h(digest.COMMIT_ID, B(c["commit_body_canonical_cbor"])).hex() == c["commit_id"]
    # every reference lands on the object it names, with the stored-object hash
    for ref, name in ((cm.delta, "delta_manifest"), (cm.descriptor, "descriptor"), (cm.image, "catalog_image_placeholder"),
                      (cm.snapshot, "snapshot_manifest_placeholder"), (cm.key_envelopes[0], "key_envelope")):
        o = objs[name]
        assert (ref.offset, ref.length) == (o["offset"], o["length"])
        assert ref.hash == digest.stored_object(B(o["frame"]))
    # The layout vector's descriptor still says draft identifier 1 (the vectors were generated before
    # the draft-2 freeze, see READING-LOG.md): a 1.0 reader (ratified archives only) must refuse it as a draft, and the
    # record is otherwise a valid descriptor listing the Encrypted feature.
    with pytest.raises(MochiError) as ei:
        descriptor.decode_object(B(objs["descriptor"]["frame"]))
    assert ei.value.code == "UNSUPPORTED_FEATURE" and "draft identifier 1" in ei.value.message
    dv = cbor.decode(B(objs["descriptor"]["frame"])[8:])
    assert dv[3] == 1 and dv[4] == [1] and dv[1] == cm.archive_id
    ke = crypto.decode_envelope_payload(B(objs["key_envelope"]["frame"])[8:])
    assert ke.archive_id == cm.archive_id
    sh = envelope.parse_sealed(B(objs["delta_manifest"]["frame"])[8:])
    assert sh.kind == envelope.SEAL_DELTA


# ----------------------------------------------------------------------------- R6
def test_r6_field_and_coefficients(r6):
    assert r6["field_polynomial"] == 0x11D == parity.POLY
    for a, b in r6["field"]["inverses"]:
        assert parity.gf_inv(a) == b and parity.gf_mul(a, b) == 1
    for a, b, c in r6["field"]["products"]:
        assert parity.gf_mul(a, b) == c == parity._gf_mul_slow(a, b)
    for p, row in enumerate(r6["coefficients"]["values"]):
        assert [parity.coefficient(p, j) for j in range(len(row))] == row
    assert parity.coefficient(127, 127) == r6["coefficients"]["corner_127_127"]
    assert all(parity.coefficient(p, j) not in (0, 1) for p in range(128) for j in range(128))


def test_r6_layout_rule(r6):
    for l in r6["layouts"]:
        lay = parity.Layout.compute(l["range_len"], l["percent"])
        assert (lay.shard_size, lay.real_shards, lay.groups, lay.data_shards, lay.parity_shards) == (
            l["shard_size"], l["real_shards"], l["groups"], l["data_shards"], l["parity_shards"])


def test_r6_codes(r6):
    for c in r6["codes"]:
        data = [B(x) for x in c["data"]]
        par = [B(x) for x in c["parity"]]
        assert parity.encode_parity(data, c["m"]) == par
        k, m = c["k"], c["m"]
        # every erasure pattern of up to m shards rebuilds
        import itertools
        for r in range(1, m + 1):
            for lost in itertools.combinations(range(k + m), r):
                dd = [None if j in lost else data[j] for j in range(k)]
                pp = [None if k + p in lost else par[p] for p in range(m)]
                got = parity.reconstruct(k, m, dd, pp)
                for j, sb in got.items():
                    assert sb == data[j]
                assert set(got) == {j for j in lost if j < k}


def test_r6_shard_hashes(r6):
    for s in r6["shard_hashes"]:
        shard = B(s["stored"]).ljust(s["shard_size"], b"\0")
        assert parity.shard_hash(shard).hex() == s["hash"]


def test_r6_small_commit_frames_byte_exact(r6):
    sc = r6["small_commit"]
    rng = B(sc["range"])
    buf = bytes(sc["range_offset"]) + rng
    out = parity.build_commit_frames(buf, sc["range_offset"], len(rng), sc["percent"], sc["required_features"],
                                     B(sc["archive_id"]), sc["seq"], B(sc["transaction_id"]))
    assert [f.hex() for f in out] == sc["frames"]
    for f in out:
        pf = parity.decode_frame(f)
        assert pf.range_offset == sc["range_offset"] and pf.range_len == len(rng)


def test_r6_multi_shard_frame_length_and_layout(r6):
    m = r6["multi_shard_commit"]
    lay = parity.Layout.compute(m["range_len"], m["percent"])
    L = m["layout"]
    assert (lay.shard_size, lay.groups, lay.data_shards, lay.parity_shards) == (
        L["shard_size"], L["groups"], L["data_shards"], L["parity_shards"])
    n = len(m["required_features"])
    assert m["frame_len"] == 8 + 80 + 8 * n + 56 + 32 * lay.data_shards + lay.shard_size + 32
    assert len(m["frame_blake3"]) == lay.groups * lay.parity_shards


def test_r6_rejects(r6):
    expect = {"digest": "digest", "unsupported": "unsupported", "layout": "layout", "envelope": "envelope"}
    for r in r6["rejects"]:
        with pytest.raises(parity.FrameFault) as ei:
            parity.decode_frame(B(r["frame"]))
        assert ei.value.fault == expect[r["fault"]], r["name"]


def test_r6_layout_hostile_inputs():
    with pytest.raises(MochiError):
        parity.Layout.compute(0, 10)
    with pytest.raises(MochiError):
        parity.Layout.compute(100, 0)
    with pytest.raises(MochiError):
        parity.Layout.compute(100, 101)
