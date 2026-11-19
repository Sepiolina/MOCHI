"""Tail handling after the head footer (spec 12.2, Annex B.2 D14): eligibility is a conservative screen."""
import pytest

from conftest import GOLDEN, read
from mochi_verify import archive, frames

pytestmark = pytest.mark.skipif(not GOLDEN.is_dir(), reason="fixtures/golden is absent")

NAME = "c5/valid-archive-3-commits.mochi"


@pytest.fixture(scope="module")
def raw():
    return read(GOLDEN / NAME)


def first_data_frame(raw):
    desc_len = frames.walk_frame(raw, 0).length
    fr = frames.walk_frame(raw, desc_len)
    assert fr.magic == frames.ZSTD_MAGIC
    return raw[fr.offset:fr.end]


def test_clean_archive_has_no_tail(raw):
    res = archive.verify(raw)
    assert res.tail == "none" and not res.findings


def test_complete_frames_after_the_head_are_an_uncommitted_tail(raw):
    res = archive.verify(raw + first_data_frame(raw) * 2)
    assert res.head_status == "opened" and len(res.nodes) == 3
    # the file does not end in the head's footer (FOOTER_INVALID); the tail is eligible, so no TAIL_UNRESOLVED
    assert res.tail == "uncommitted" and res.findings == {("FOOTER_INVALID", None)}


def test_a_frame_cut_short_by_the_end_of_the_file_is_still_eligible(raw):
    res = archive.verify(raw + first_data_frame(raw)[:9])
    assert res.tail == "uncommitted" and res.findings == {("FOOTER_INVALID", None)}


def test_unrecognised_bytes_are_not_eligible(raw):
    for junk in (b"\x00", b"\x00" * 100, b"junk" * 50):
        res = archive.verify(raw + junk)
        assert res.head_status == "opened" and res.tail == "unresolved"
        assert ("TAIL_UNRESOLVED", None) in res.findings


def test_a_flipped_footer_header_makes_the_tail_ineligible_and_the_head_the_previous_commit(raw):
    full = archive.verify(raw)
    foot = full.nodes[-1].footer.offset
    m = bytearray(raw)
    m[foot] = 0x57                                      # 0x184D2A56 -> 0x184D2A57 (G8): looks like a descriptor frame
    res = archive.verify(bytes(m))
    assert len(res.nodes) == 2 and res.nodes[-1].commit.commit_id == full.nodes[1].commit.commit_id
    assert res.tail == "unresolved" and ("TAIL_UNRESOLVED", None) in res.findings


def test_a_destroyed_footer_digest_keeps_the_previous_commit_and_reports_the_tail(raw):
    full = archive.verify(raw)
    foot = full.nodes[-1].footer.offset
    m = bytearray(raw)
    m[foot + 40] ^= 0xFF
    res = archive.verify(bytes(m))
    assert len(res.nodes) == 2 and ("TAIL_UNRESOLVED", None) in res.findings


def test_a_cut_commit_without_its_footer_is_an_uncommitted_tail(raw):
    full = archive.verify(raw)
    foot = full.nodes[-1].footer.offset
    res = archive.verify(raw[:foot])                    # the last commit frame is complete, its footer is missing
    assert len(res.nodes) == 2 and res.tail == "uncommitted" and res.findings == {("FOOTER_INVALID", None)}
