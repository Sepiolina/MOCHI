"""The command line and the mochi-interop-summary-v1 document."""
import json
import subprocess
import sys

import pytest

from conftest import GOLDEN, GOLDEN_PASSPHRASE, read

pytestmark = pytest.mark.skipif(not GOLDEN.is_dir(), reason="fixtures/golden is absent")


def run(*args, check=True):
    p = subprocess.run([sys.executable, "-I", "-m", "mochi_verify", *map(str, args)], capture_output=True, text=True)
    if check:
        assert p.returncode == 0, p.stderr
    return p


def summary(path, *extra):
    return json.loads(run(path, *extra, "--summary").stdout)


def test_summary_shape_and_hex_encoding():
    s = summary(GOLDEN / "c5" / "valid-archive-delta-segment.mochi")
    assert s["format"] == "mochi-interop-summary-v1" and s["case"] == "valid-archive-delta-segment"
    assert s["head"] == {"status": "opened", "code": None}
    assert set(s) == {"format", "case", "head", "archive_id", "profile", "commits", "snapshots", "findings", "parity"}
    assert len(s["archive_id"]) == 64
    assert [c["seq"] for c in s["commits"]] == list(range(6))
    assert all(isinstance(c["checkpoint"], bool) and len(c["commit_id"]) == 64 for c in s["commits"])
    assert [c["footer_offset"] for c in s["commits"]] == sorted(c["footer_offset"] for c in s["commits"])
    assert s["parity"] is None and s["findings"] == []
    for snap in s["snapshots"]:
        paths = [bytes.fromhex(e["path"]) for e in snap["entries"]]
        assert paths == sorted(paths)                       # sorted by path bytes
        for e in snap["entries"]:
            assert e["kind"] in ("file", "dir") and set(e["attributes"]) == {"posix_mode", "uid", "gid", "mtime_ns", "windows"}
            assert (e["content_hash"] is None) == (e["kind"] == "dir")


def test_output_keys_are_sorted():
    out = run(GOLDEN / "c5" / "valid-archive-3-commits.mochi", "--summary").stdout
    def check(o):
        if isinstance(o, dict):
            assert list(o) == sorted(o)
            for v in o.values():
                check(v)
        elif isinstance(o, list):
            for v in o:
                check(v)
    check(json.loads(out))


def test_recovery_record_profile_and_parity_block():
    s = summary(GOLDEN / "c12" / "damaged-within-capacity.mochi")
    assert s["profile"]["recovery_percent"] == 50
    assert s["parity"] == {"commits": 3, "damaged_shards": 4, "rebuilt": 4, "beyond_capacity": 0}
    assert s["findings"] == [{"code": "STORED_INTEGRITY_FAILED", "object": None}]     # raw bytes are reported


def test_beyond_capacity_findings_name_a_hash_only_for_a_data_chunk():
    s = summary(GOLDEN / "c12" / "damaged-beyond-capacity.mochi")
    codes = {(f["code"], f["object"] is None) for f in s["findings"]}
    # the damage here is in a catalog image, not a data chunk: its object is null
    assert codes == {("PARITY_CAPACITY_EXCEEDED", True), ("STORED_INTEGRITY_FAILED", True)}
    assert s["findings"] == sorted(s["findings"], key=lambda f: (f["code"], f["object"] or ""))


def test_encrypted_with_and_without_a_passphrase(tmp_path):
    pw = tmp_path / "pw.txt"
    pw.write_text(GOLDEN_PASSPHRASE + "\n")
    keyed = summary(GOLDEN / "c11" / "valid-archive-encrypted.mochi", "--passphrase-file", pw)
    keyless = summary(GOLDEN / "c11" / "valid-archive-encrypted.mochi")
    assert keyed["profile"]["encrypted"] and keyless["profile"]["encrypted"]
    assert keyed["commits"] == keyless["commits"] and keyed["archive_id"] == keyless["archive_id"]
    assert keyed["snapshots"] and keyless["snapshots"] is None
    # sealed paths are readable only with the key; the plaintext names are hex of the stored path bytes
    assert all(e["path"] for snap in keyed["snapshots"] for e in snap["entries"])


def test_wrong_passphrase_is_a_refused_summary_and_operational_exit_3_without_it(tmp_path):
    pw = tmp_path / "pw.txt"
    pw.write_text("not the passphrase\n")
    arch = GOLDEN / "c11" / "valid-archive-encrypted.mochi"
    p = run(arch, "--passphrase-file", pw, "--summary", check=False)
    s = json.loads(p.stdout)
    assert p.returncode == 0 and s["head"] == {"status": "refused", "code": "KEY_UNAVAILABLE"}
    assert s["findings"] == [{"code": "KEY_UNAVAILABLE", "object": None}] and s["commits"] == []
    p = run(arch, "--passphrase-file", pw, check=False)
    assert p.returncode == 3 and "KEY_UNAVAILABLE" in p.stderr


def test_refused_summary():
    s = summary(GOLDEN / "c5" / "reject-archive-duplicate-version.mochi")
    assert s["head"] == {"status": "refused", "code": "RECORD_INVALID"}
    assert s["commits"] == [] and s["snapshots"] == [] and s["archive_id"] is None
    assert s["findings"] == [{"code": "RECORD_INVALID", "object": None}]       # the refusal is a finding too


def test_no_valid_head_summary(tmp_path):
    f = tmp_path / "junk.mochi"
    f.write_bytes(b"not an archive")
    s = summary(f)
    assert s["head"] == {"status": "refused", "code": "NO_VALID_HEAD"} and s["archive_id"] is None


def test_expected_head_freshness(tmp_path):
    full = summary(GOLDEN / "c5" / "valid-archive-3-commits.mochi")
    first, last = full["commits"][0]["commit_id"], full["commits"][-1]["commit_id"]
    assert summary(GOLDEN / "c5" / "valid-archive-3-commits.mochi", "--expected-head", last)["findings"] == []
    assert summary(GOLDEN / "c5" / "valid-archive-3-commits.mochi", "--expected-head", first)["findings"] == []  # in history
    other = summary(GOLDEN / "c5" / "valid-archive-3-commits.mochi", "--expected-head", "ab" * 32)
    assert other["findings"] == [{"code": "FRESHNESS_FAILED", "object": None}]


def test_the_archive_is_never_modified(tmp_path):
    src = read(GOLDEN / "c12" / "damaged-within-capacity.mochi")
    f = tmp_path / "a.mochi"
    f.write_bytes(src)
    before = f.stat().st_mtime_ns
    run(f, "--summary")
    assert f.read_bytes() == src and f.stat().st_mtime_ns == before
