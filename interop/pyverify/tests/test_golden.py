"""Per-phase golden vectors (fixtures/golden/{c1,c2,c3,c4,c5,b2,c10,c11,c12}): each gives the
verdict its vectors.txt (or README) lists.  Verdicts are compared as stable error codes."""
import json
import pathlib

import pytest

from conftest import GOLDEN, GOLDEN_PASSPHRASE, read, vectors_lines
from mochi_verify import (archive, catalog, commit, crypto, descriptor, digest, envelope, footer, frames,
                          manifest, objects, parity, tar)
from mochi_verify.errors import MochiError

pytestmark = pytest.mark.skipif(not GOLDEN.is_dir(), reason="fixtures/golden is absent")


def code_of(fn, *a, **k):
    try:
        fn(*a, **k)
    except MochiError as e:
        return e.code
    return None


# ------------------------------------------------------------------------------ c1: frames
C1 = sorted(p.name for p in (GOLDEN / "c1").glob("*.bin"))


@pytest.mark.parametrize("name", C1)
def test_c1_frame_vectors(name):
    data = read(GOLDEN / "c1" / name)
    fr, stop, err = frames.walk_all(data)
    footers = [f for f in fr if f.magic == frames.K_FOOTER and f.length == footer.FOOTER_LEN]
    if name.startswith("valid-"):
        assert err is None and stop == len(data)
        for f in footers:
            footer.validate_footer(data, f.offset)
        return
    # a reject: the walk fails, or a footer fails, or no footer sits at a frame boundary
    broken = err is not None
    for f in footers:
        if code_of(footer.validate_footer, data, f.offset) is not None:
            broken = True
    if name == "reject-false-magic-inside-payload.bin":
        assert err is None and not footers
        with pytest.raises(MochiError) as ei:
            archive.Verifier(data)._find_head()
        assert ei.value.code == "NO_VALID_HEAD"
        return
    assert broken, name


def test_c1_reject_codes():
    exp = {"reject-block-exceeds-maximum.bin": "MALFORMED_FRAME", "reject-reserved-block-type.bin": "MALFORMED_FRAME",
           "reject-reserved-header-bit.bin": "MALFORMED_FRAME", "reject-not-a-frame.bin": "MALFORMED_FRAME",
           "reject-descriptor-not-at-offset-0.bin": "MALFORMED_FRAME", "reject-footer-bare-trailer.bin": "MALFORMED_FRAME",
           "reject-truncated-data-frame.bin": "TRUNCATED"}
    for name, code in exp.items():
        _, _, err = frames.walk_all(read(GOLDEN / "c1" / name))
        assert err is not None and err.code == code, name
    _, _, err = frames.walk_all(read(GOLDEN / "c1" / "reject-skippable-oversized.bin"))
    assert err.code in ("LIMIT_EXCEEDED", "TRUNCATED")


# ------------------------------------------------------------------------------ c2: objects, digests
def test_c2_vectors():
    from blake3 import blake3
    seps = {"chunk-content": digest.CHUNK_CONTENT, "stored-object": digest.STORED_OBJECT,
            "dictionary": digest.DICTIONARY, "commit-id": digest.COMMIT_ID}
    n = 0
    for name, t in vectors_lines(GOLDEN / "c2" / "vectors.txt"):
        if name == "digest":
            scope, inp, want = t
            data = b"" if inp == "-" else bytes.fromhex(inp)
            got = blake3(data).hexdigest() if scope == "file-content" else digest.h(seps[scope], data).hex()
            assert got == want, (scope, inp[:20])
            n += 1
        elif name == "object":
            oname, dlen, verdict = t[0], int(t[1]), t[2]
            frame = read(GOLDEN / "c2" / (oname + ".bin"))
            if verdict == "decodes":
                out = objects.decode_chunk_frame(frame, dlen)
                assert digest.chunk_content(out).hex() == t[3] and digest.stored_object(frame).hex() == t[4]
            else:
                want = {"Malformed": "MALFORMED_FRAME", "ContentIntegrity": "CONTENT_INTEGRITY_FAILED"}[t[3]]
                assert code_of(objects.decode_chunk_frame, frame, dlen) == want, oname
            n += 1
    assert n == 15 + 13


# ------------------------------------------------------------------------------ c3: a catalog image
def test_c3_catalog_image():
    st = catalog.check_image(read(GOLDEN / "c3" / "valid-catalog-v0.sqlite"))
    assert st.head_seq == 2 and len(st.versions) == 3
    assert b"docs/x\xed\xa0\x80" in st.namespace            # a WTF-8 name with an unpaired surrogate is kept as bytes


def test_c3_catalog_rejects_mutations():
    img = bytearray(read(GOLDEN / "c3" / "valid-catalog-v0.sqlite"))
    for off, code in ((16, "CATALOG_INVALID"), (60 + 3, "UNSUPPORTED_FEATURE"), (68 + 3, "CATALOG_INVALID"),
                      (18, "CATALOG_INVALID")):
        m = bytearray(img)
        m[off] ^= 0x01
        assert code_of(catalog.check_image, bytes(m)) == code, off
    assert code_of(catalog.check_image, bytes(img[:-1])) == "CATALOG_INVALID"
    assert code_of(catalog.check_image, b"\0" * 4096) == "CATALOG_INVALID"


# ------------------------------------------------------------------------------ c4 / c5: records
def _record_vectors(dirname, decoder):
    for name, t in vectors_lines(GOLDEN / dirname / "vectors.txt"):
        if name.endswith(".mochi") or t[0] == "opens" or (len(t) > 1 and t[0] == "rejects" and name.endswith(".mochi")):
            continue
        yield name, t


@pytest.mark.parametrize("name,t", list(_record_vectors("c4", None)))
def test_c4_manifest_vectors(name, t):
    data = read(GOLDEN / "c4" / (name + ".bin"))
    if t[0] == "valid":
        manifest.decode_object(data)
        assert digest.stored_object(data).hex() == t[1]
    else:
        assert code_of(manifest.decode_object, data) == t[1]


@pytest.mark.parametrize("name,t", list(_record_vectors("c5", None)))
def test_c5_commit_vectors(name, t):
    data = read(GOLDEN / "c5" / (name + ".bin"))
    if t[0] == "valid":
        assert commit.decode_object(data).commit_id.hex() == t[1]
    else:
        assert code_of(commit.decode_object, data) == t[1]


def test_r3_commit_ids():
    want = {"valid-commit-root": "b4592ef9ac9cc216937605d9a2f5067c827e6f033bda0391a6fb10f7162ca692",
            "valid-commit-child": "d899e30833e2ee8dd89f07ab6f25eeb035894ee18af657d8ca196b9f37b25c56",
            "valid-commit-delta": "a7cf5aed31f4aaa2061c4002bd7a618d43781cf628602f777e02166de7e7d2b8"}
    for name, h in want.items():
        assert commit.decode_object(read(GOLDEN / "c5" / (name + ".bin"))).commit_id.hex() == h


C5_ARCHIVES = [(n, t) for n, t in vectors_lines(GOLDEN / "c5" / "vectors.txt") if n.endswith(".mochi")]


@pytest.mark.parametrize("name,t", C5_ARCHIVES)
def test_c5_archive_vectors_open_head(name, t):
    res = archive.verify(read(GOLDEN / "c5" / name))
    if t[0] == "opens":
        assert res.head_status == "opened"
    else:
        assert res.head_status == "refused" and res.head_code == t[1], res.head_message


@pytest.mark.parametrize("path,code", [("c5/reject-archive-legacy-v0.mochi", "UNSUPPORTED_FEATURE"),
                                       ("c5/reject-archive-bare-image.mochi", "UNSUPPORTED_FEATURE"),
                                       ("b2/reject-archive-draft-1.mochi", "UNSUPPORTED_FEATURE")])
def test_older_drafts_are_refused(path, code):
    res = archive.verify(read(GOLDEN / path))
    assert res.head_status == "refused" and res.head_code == code


@pytest.mark.parametrize("name", ["valid-archive-3-commits.mochi", "valid-archive-delta-segment.mochi",
                                  "valid-archive-wrong-base-hint.mochi"])
def test_c5_valid_archives_verify_clean(name):
    res = archive.verify(read(GOLDEN / "c5" / name))
    assert res.head_status == "opened" and not res.findings, res.notes
    assert res.snapshots


# ------------------------------------------------------------------------------ b2: envelopes, descriptors
B2_ENV_CODES = {
    "valid-envelope-image": None, "valid-envelope-image-zstd": None,
    "reject-envelope-decoded-signature": "ENVELOPE_INVALID",
    "reject-envelope-wrong-frame-kind": "ENVELOPE_INVALID", "reject-envelope-version": "UNSUPPORTED_FEATURE",
    "reject-envelope-schema-unknown": "UNSUPPORTED_FEATURE", "reject-envelope-schema-user-version": "ENVELOPE_INVALID",
    "reject-envelope-unknown-feature": "UNSUPPORTED_FEATURE", "reject-envelope-features-not-increasing": "ENVELOPE_INVALID",
    "reject-envelope-feature-count": "ENVELOPE_INVALID", "reject-envelope-header-length": "ENVELOPE_INVALID",
    "reject-envelope-payload-encoding": "ENVELOPE_INVALID", "reject-envelope-payload-encoding-kind": "ENVELOPE_INVALID",
    "reject-envelope-signature": "ENVELOPE_INVALID", "reject-envelope-payload-length": "ENVELOPE_INVALID",
    "reject-envelope-archive-id": "ENVELOPE_INVALID", "reject-envelope-sequence": "ENVELOPE_INVALID",
    "reject-envelope-transaction-id": "ENVELOPE_INVALID",
}


def _b2_envelope(data):
    env = envelope.decode_binary(data, frames.K_IMAGE, (0,))
    env.bind(b"\xa1" * 32, 7, b"\xb2" * 16)
    envelope.decode_image(env)      # B.2.15: encoding 2 is decoded after bind, then its signature is checked
    return env


@pytest.mark.parametrize("name,code", sorted(B2_ENV_CODES.items()))
def test_b2_envelope_vectors(name, code):
    # the listing in vectors.txt names the same set
    assert code_of(_b2_envelope, read(GOLDEN / "b2" / (name + ".bin"))) == code


def test_b2_envelope_listing_is_complete():
    listed = {n for n, _ in vectors_lines(GOLDEN / "b2" / "vectors.txt")}
    assert listed == set(B2_ENV_CODES)


@pytest.mark.parametrize("name,t", list(vectors_lines(GOLDEN / "b2" / "descriptor-vectors.txt")))
def test_b2_descriptor_vectors(name, t):
    data = read(GOLDEN / "b2" / (name + ".bin"))
    if t[0] == "valid":
        d = descriptor.decode_object(data)
        assert len(d.archive_id) == 32
    else:
        assert code_of(descriptor.decode_object, data) == t[1]


# ------------------------------------------------------------------------------ c10: TAR-compatible
@pytest.mark.parametrize("name", sorted(p.name for p in (GOLDEN / "c10").glob("valid-*.tar")))
def test_c10_valid_streams(name):
    assert tar.parse_stream(read(GOLDEN / "c10" / name))


@pytest.mark.parametrize("name", sorted(p.name for p in (GOLDEN / "c10").glob("reject-*.tar")))
def test_c10_reject_streams(name):
    assert code_of(tar.parse_stream, read(GOLDEN / "c10" / name)) == "PROFILE_VIOLATION"


def test_c10_whole_archive():
    res = archive.verify(read(GOLDEN / "c10" / "valid-archive-tar-4-commits.mochi"))
    assert res.head_status == "opened" and not res.findings and res.descriptor.tar_compatible


# ------------------------------------------------------------------------------ c11: encrypted
def test_c11_archive_with_and_without_the_key():
    raw = read(GOLDEN / "c11" / "valid-archive-encrypted.mochi")
    keyed = archive.verify(raw, crypto.passphrase_bytes(GOLDEN_PASSPHRASE))
    assert keyed.head_status == "opened" and not keyed.findings and keyed.snapshots and not keyed.keyless
    keyless = archive.verify(raw)
    assert keyless.head_status == "keyless" and keyless.keyless and not keyless.findings and not keyless.snapshots
    assert [n.commit.commit_id for n in keyless.nodes] == [n.commit.commit_id for n in keyed.nodes]


def test_c11_wrong_passphrase_is_key_unavailable_not_a_verdict():
    raw = read(GOLDEN / "c11" / "valid-archive-encrypted.mochi")
    res = archive.verify(raw, b"definitely wrong")      # a refusal, never a verification result
    assert res.head_status == "refused" and res.head_code == "KEY_UNAVAILABLE" and not res.snapshots


def test_c11_damage_is_found_keyless_and_keyed():
    raw = bytearray(read(GOLDEN / "c11" / "valid-archive-encrypted.mochi"))
    base = archive.verify(bytes(raw))
    reg = base.nodes[0].commit.data_region
    raw[reg.offset + 60] ^= 0x40                       # a ciphertext byte of the first sealed chunk
    for pw in (None, crypto.passphrase_bytes(GOLDEN_PASSPHRASE)):
        res = archive.verify(bytes(raw), pw)
        assert res.head_status == ("opened" if pw else "keyless")
        assert any(c == "STORED_INTEGRITY_FAILED" for c, _ in res.findings)


C11_ENVELOPE_REJECTS = {
    "reject-envelope-kdf-iterations": "LIMIT_EXCEEDED", "reject-envelope-kdf-lanes": "LIMIT_EXCEEDED",
    "reject-envelope-kdf-memory": "LIMIT_EXCEEDED", "reject-envelope-kdf-memory-below-8-per-lane": "RECORD_INVALID",
    "reject-envelope-kdf-zero-lanes": "RECORD_INVALID", "reject-envelope-wrapped-key-flipped": "KEY_UNAVAILABLE",
}


def _unwrap(data, passphrase=GOLDEN_PASSPHRASE):
    fr = frames.walk_frame(data, 0)
    if fr.magic != frames.K_KEY_ENVELOPE:
        raise MochiError("ENVELOPE_INVALID", "not a key-envelope frame")
    env = crypto.decode_envelope_payload(data[8:])
    if crypto.unwrap(env, crypto.passphrase_bytes(passphrase)) is None:
        raise MochiError("KEY_UNAVAILABLE", "does not open")


def test_c11_valid_key_envelope():
    data = read(GOLDEN / "c11" / "valid-key-envelope.bin")
    _unwrap(data)


@pytest.mark.parametrize("name,code", sorted(C11_ENVELOPE_REJECTS.items()))
def test_c11_key_envelope_rejects(name, code):
    assert code_of(_unwrap, read(GOLDEN / "c11" / (name + ".bin"))) == code


def test_c11_envelope_shape_rejects():
    assert code_of(_unwrap, read(GOLDEN / "c11" / "reject-envelope-wrong-frame-kind.bin")) is not None
    assert code_of(_unwrap, read(GOLDEN / "c11" / "reject-envelope-truncated.bin")) is not None


C11_SEALED = {"reject-sealed-version": "UNSUPPORTED_FEATURE", "reject-sealed-suite": "UNSUPPORTED_FEATURE",
              "reject-sealed-kind": "UNSUPPORTED_FEATURE", "reject-sealed-too-short": "RECORD_INVALID"}


@pytest.mark.parametrize("name,code", sorted(C11_SEALED.items()))
def test_c11_sealed_header_rejects(name, code):
    data = read(GOLDEN / "c11" / (name + ".bin"))
    fr = frames.walk_frame(data, 0)
    assert code_of(envelope.parse_sealed, bytes(data[8:fr.end])) == code


# ------------------------------------------------------------------------------ c12: redundancy
C12_FRAME_FAULTS = {"reject-parity-flipped-shard-byte": "digest", "reject-parity-unknown-suite": "unsupported",
                    "reject-parity-shard-size-not-the-layout-rule-s": "layout", "reject-parity-percentage-0": "layout",
                    "reject-parity-parity-index-out-of-range": "layout", "reject-parity-signature": "envelope",
                    "reject-parity-payload-encoding-0-in-a-parity-frame": "envelope"}


def test_c12_valid_frame_decodes():
    parity.decode_frame(read(GOLDEN / "c12" / "valid-parity-frame.bin"))


@pytest.mark.parametrize("name,fault", sorted(C12_FRAME_FAULTS.items()))
def test_c12_frame_rejects(name, fault):
    with pytest.raises(parity.FrameFault) as ei:
        parity.decode_frame(read(GOLDEN / "c12" / (name + ".bin")))
    assert ei.value.fault == fault


def test_c12_valid_archive_verifies_and_its_parity_decodes():
    res = archive.verify(read(GOLDEN / "c12" / "valid-archive-recovery-record.mochi"))
    assert res.head_status == "opened" and not res.findings
    assert res.parity == {"commits": 3, "damaged_shards": 0, "rebuilt": 0, "beyond_capacity": 0}
    assert res.descriptor.recovery == (1, 50)


def test_c12_damage_within_capacity_rebuilds_bit_identically():
    valid = read(GOLDEN / "c12" / "valid-archive-recovery-record.mochi")
    res = archive.verify(read(GOLDEN / "c12" / "damaged-within-capacity.mochi"))
    assert res.healed == valid
    assert res.head_status == "opened" and res.parity["rebuilt"] == res.parity["damaged_shards"] == 4
    assert res.parity["beyond_capacity"] == 0
    # the bytes as stored are damaged and that is reported (B.2.11 item 12); the rebuilt view is what the rest reads
    assert res.findings == {("STORED_INTEGRITY_FAILED", None)}


def test_c12_damage_beyond_capacity_is_reported_and_left_alone():
    damaged = read(GOLDEN / "c12" / "damaged-beyond-capacity.mochi")
    res = archive.verify(damaged)
    assert ("PARITY_CAPACITY_EXCEEDED", None) in res.findings
    assert res.parity["beyond_capacity"] == 1 and res.parity["rebuilt"] == 0
    assert res.healed == damaged                          # nothing in the group is rewritten


# ------------------------------------------------------------------------------ r8 and the 1.0 boundary (B.2.14, D24)
R8 = GOLDEN / "r8"


def test_r8_golden_set_matches_every_reference_summary_exactly():
    import subprocess, sys
    tool = pathlib.Path(__file__).resolve().parents[1] / "tools" / "compare_r8.py"
    p = subprocess.run([sys.executable, str(tool), str(R8)], capture_output=True, text=True)
    cases = json.loads((R8 / "cases.json").read_text())
    assert p.returncode == 0, p.stdout + p.stderr
    assert f"{len(cases)} cases compared, 0 differ" in p.stdout


LEGACY = sorted((R8 / "legacy").glob("*.mochi"))


def test_legacy_directory_is_not_empty():
    assert len(LEGACY) == 3


@pytest.mark.parametrize("path", LEGACY, ids=lambda p: p.name)
def test_legacy_draft_2_archives_are_refused_naming_the_draft(path):
    res = archive.verify(read(path))
    assert res.head_status == "refused" and res.head_code == "UNSUPPORTED_FEATURE", res.head_message
    assert "draft identifier 2" in res.head_message


@pytest.mark.parametrize("name,draft", [("reject-descriptor-draft-1", 1), ("reject-descriptor-draft-2", 2),
                                        ("reject-descriptor-draft-3", 3)])
def test_descriptor_draft_is_refused_naming_it(name, draft):
    with pytest.raises(MochiError) as ei:
        descriptor.decode_object(read(GOLDEN / "b2" / f"{name}.bin"))
    assert ei.value.code == "UNSUPPORTED_FEATURE" and f"draft identifier {draft}" in ei.value.message


def test_ratified_descriptor_has_null_draft():
    d = descriptor.decode_object(read(GOLDEN / "b2" / "valid-descriptor-minimal.bin"))
    assert d.draft is None


# ---------------------------------------------------------------- R8 summary: parity without a decodable descriptor
def test_parity_is_null_when_the_descriptor_does_not_decode():
    raw = bytearray(read(GOLDEN / "c12" / "valid-archive-recovery-record.mochi"))
    assert archive.verify(bytes(raw)).parity is not None
    fr = frames.walk_frame(bytes(raw), 0)
    assert fr.magic == frames.K_DESCRIPTOR
    raw[fr.length - 5] ^= 0x01                      # damage inside the descriptor; parity frames still follow the commits
    assert archive.verify(bytes(raw)).parity is None
