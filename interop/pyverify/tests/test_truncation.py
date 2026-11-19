"""Spec 24.2: truncation at every byte of a small valid archive never yields a commit outside
the original's committed prefix (and never an invalid one)."""
import os

import pytest

from conftest import GOLDEN, read
from mochi_verify import archive

pytestmark = pytest.mark.skipif(not GOLDEN.is_dir(), reason="fixtures/golden is absent")

SMALL = ["c5/valid-archive-wrong-base-hint.mochi", "c10/valid-archive-tar-4-commits.mochi",
         "c11/valid-archive-encrypted.mochi"]
LARGER = ["c5/valid-archive-3-commits.mochi", "c5/valid-archive-delta-segment.mochi",
          "c12/valid-archive-recovery-record.mochi"]


def check_all(raw: bytes, points):
    full = archive.verify(raw)                      # keyless for the Encrypted archive: commits are plaintext
    assert full.head_status in ("opened", "keyless")
    chain = [(n.commit.commit_id, n.footer.offset + 72) for n in full.nodes]
    for n in points:
        res = archive.verify(raw[:n])
        k = sum(1 for _, end in chain if end <= n)
        got = [nd.commit.commit_id for nd in res.nodes] if res.head_status in ("opened", "keyless") else []
        want = [cid for cid, _ in chain[:k]]
        assert got == want, f"truncated at {n}: {len(got)} commits, expected the first {k}"
        if k == 0:
            assert res.head_status == "refused", n


@pytest.mark.parametrize("name", SMALL)
def test_every_byte_of_a_small_archive(name):
    raw = read(GOLDEN / name)
    check_all(raw, range(len(raw) + 1))


@pytest.mark.parametrize("name", LARGER)
def test_boundaries_and_stride_of_larger_archives(name):
    raw = read(GOLDEN / name)
    full = archive.verify(raw)
    edges = set()
    for nd in full.nodes:
        for base in (nd.footer.offset, nd.footer.offset + 72, nd.footer.commit_offset):
            edges.update(range(max(0, base - 80), min(len(raw), base + 80) + 1))
    edges.update(range(0, len(raw) + 1, 251))
    edges.add(len(raw))
    check_all(raw, sorted(edges))
