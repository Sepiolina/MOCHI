"""Self-test for orchestrate.evaluate(): the verdict logic, on synthetic guest results.

    python3 -I ci/g6-windows/test_evaluate.py

Covers a good run and the ways a run must fail: a vacuous run (no complete
archive ever seen), a judge violation, broken controls (synced data missing,
unsynced data not droppable), a missing result, and a volume that does not mount.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import orchestrate as o  # noqa: E402


def judge(verdicts, hard=(), soft=()):
    return json.dumps({"result": "fail" if hard else "pass", "hard": list(hard), "soft": list(soft),
                       "archives": [{"verdict": v} for v in verdicts]})


def pt(i, kind, anchor, **kw):
    return dict(id=i, kind=kind, anchor=anchor, limit=anchor + 1, **kw)


def ok(i, **kw):
    return dict(id=i, mount="ok", judge_rc=0, verify={"a0.mochi": {"rc": 0}}, chkdsk_rc=0, **kw)


def ns_points(first_id, plain, wt, d):
    """Namespace control points with the given survival facts (True = the rename reached the device)."""
    pts, res, i = [], {}, first_id
    for variant, outcomes in (("plain", plain), ("wt", wt), ("dir", d)):
        for n, survived in enumerate(outcomes, 1):
            src, dst = f"ctl\\ns_{variant}_{n}.src", f"ctl\\ns_{variant}_{n}.dst"
            pts.append(pt(i, f"ctl-ns-{variant}", 5, probe=[src, dst]))
            res[i] = dict(id=i, mount="ok", facts={src: {"exists": not survived}, dst: {"exists": survived}})
            i += 1
    return pts, res


def run(points, results):
    s = {}
    return o.evaluate(points, results, s), s


def main():
    points = [
        pt(0, "ctl-synced", 1, probe=["ctl\\synced_1.bin"], expect={"ctl\\synced_1.bin": ("aa", 4)}),
        pt(1, "ctl-unsynced", 2, probe=["ctl\\unsynced_1.bin"], expect={"ctl\\unsynced_1.bin": ("aa", 4)}),
        pt(2, "flush", 10),
        pt(3, "edge", 20),
    ]
    good = {
        0: dict(id=0, mount="ok", facts={"ctl\\synced_1.bin": {"exists": True, "size": 4, "sha256": "aa"}}),
        1: dict(id=1, mount="ok", facts={"ctl\\unsynced_1.bin": {"exists": False}}),
        2: ok(2, judge=judge(["absent", "seq-0"])),
        3: ok(3, judge=judge(["seq-2"] * 4)),
    }
    nsp, nsr = ns_points(10, [False, True, False], [False] * 3, [True, True, True])
    points, good = points + nsp, {**good, **nsr}
    passed, s = run(points, good)
    assert passed and s["verdict"] == "PASS", s
    assert s["controls"]["rename_without_flush_survived"].startswith("1/3"), s["controls"]

    # Namespace controls decide validity: an unflushed rename that is never lost
    # (harness blind to metadata loss), or a flushed rename that is lost.
    for plain, d in (([True] * 3, [True] * 3), ([False] * 3, [True, True, False])):
        nsp2, nsr2 = ns_points(10, plain, [True] * 3, d)
        passed, s = run(points[:4] + nsp2, {**good, **nsr2})
        assert not passed and any("controls failed" in h for h in s["hard_failures"]), s

    # A write-through rename that is lost is a finding, not an invalid harness.
    nsp3, nsr3 = ns_points(10, [False] * 3, [False] * 3, [True] * 3)
    passed, s = run(points[:4] + nsp3, {**good, **nsr3})
    assert passed and s["controls"]["rename_move_write_through_survived"].startswith("0/3"), s

    vac = {**good, 2: ok(2, judge=judge(["absent"] * 4)), 3: ok(3, judge=judge(["absent"] * 4))}
    passed, s = run(points, vac)
    assert not passed and any("vacuous" in h for h in s["hard_failures"]), s

    bad2 = ok(2, judge=judge(["unopenable"], hard=["a0.mochi exists but does not open"]))
    bad2["judge_rc"] = 1
    passed, s = run(points, {**good, 2: bad2})
    assert not passed and any("does not open" in h for h in s["hard_failures"]), s

    broken = dict(id=0, mount="ok", facts={"ctl\\synced_1.bin": {"exists": False}})
    passed, s = run(points, {**good, 0: broken})
    assert not passed and any("controls failed" in h for h in s["hard_failures"]), s

    full = dict(id=1, mount="ok", facts={"ctl\\unsynced_1.bin": {"exists": True, "size": 4, "sha256": "aa"}})
    passed, s = run(points, {**good, 1: full})
    assert not passed, s

    missing = {k: v for k, v in good.items() if k != 2}
    passed, s = run(points, missing)
    assert not passed and any("no result" in h for h in s["hard_failures"]), s

    nomount = {**good, 2: {"id": 2, "mount": "failed", "error": "volume did not mount as NTFS"}}
    passed, s = run(points, nomount)
    assert not passed and any("NTFS recovery" in h for h in s["hard_failures"]), s

    # ack-cut points are judged like any other.
    cut = points + [pt(4, "ack-cut", 15, ack_limit=16)]
    cut_bad = {**good, 4: ok(4, judge=judge(["absent"], hard=["a0.mochi: durable creation acknowledged, name is gone"]))}
    cut_bad[4]["judge_rc"] = 1
    passed, s = run(cut, cut_bad)
    assert not passed and s["judged_points"] == 3, s

    # ack_line: --strict-creation turns a creation's U into D, and only that.
    class M:
        def __init__(self, label):
            self.label = label
    assert o.ack_line(M("ack:a0.mochi:0:U"), False) == "a0.mochi 0 U"
    assert o.ack_line(M("ack:a0.mochi:0:U"), True) == "a0.mochi 0 D"
    assert o.ack_line(M("ack:a0.mochi:1:U"), True) == "a0.mochi 1 U"

    # An Unconfirmed creation whose name is absent is allowed, and counted.
    soft = {**good, 2: ok(2, judge=judge(["absent", "seq-0"], soft=["a0.mochi: absent after acknowledged seq 1; creation was DirectoryUnconfirmed"]))}
    passed, s = run(points, soft)
    assert passed and s["absent_after_unconfirmed_ack"] == 1, s
    # Install stall detection: a frozen Setup screen (spinner glyphs change under
    # OCR, the words do not) is told apart from real progress.
    frame = "Installing Windows 11 | Your PC will restart several times. | {} {} complete | ves No."
    frozen = [o.screen_words(frame.format(g, "52%")) for g in "*»~‘CY"]
    moving = [o.screen_words(frame.format("*", p)) for p in ("52%", "57%", "63%", "71%")]

    def is_frozen(screens):
        return len(screens) >= o.STALL_TICKS and len(set(screens[-o.STALL_TICKS:])) == 1

    assert is_frozen(frozen) and not is_frozen(moving) and not is_frozen(frozen[:o.STALL_TICKS - 1])
    print("evaluate self-test ok")


main()
