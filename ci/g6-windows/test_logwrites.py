"""Self-test for logwrites.py against the real replay-log (needs REPLAY_LOG).

    REPLAY_LOG=/path/to/replay-log python3 -I ci/g6-windows/test_logwrites.py
"""

import os
import struct
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import logwrites as lw  # noqa: E402

SS = 512


def build_log(path, ops):
    """ops: ('w', sector, bytes) | ('f',) bare flush | ('pf', sector, bytes) | ('fua', sector, bytes) | ('m', label)"""
    body = bytearray()
    n = 0
    for op in ops:
        if op[0] == "m":
            lab = op[1].encode()
            hdr = struct.pack("<QQQQ", 0, 0, lw.MARK, len(lab)) + lab
            body += hdr.ljust(SS, b"\0")
        elif op[0] == "f":
            body += struct.pack("<QQQQ", 0, 0, lw.FLUSH, 0).ljust(SS, b"\0")
        else:
            kind, sector, data = op
            assert len(data) % SS == 0
            flags = {"w": 0, "pf": lw.FLUSH, "fua": lw.FUA}[kind]
            body += struct.pack("<QQQQ", sector, len(data) // SS, flags, 0).ljust(SS, b"\0")
            body += data
        n += 1
    sup = struct.pack("<QQQI", lw.MAGIC, 1, n, SS).ljust(SS, b"\0")
    with open(path, "wb") as f:
        f.write(sup + body)


def replay(replay_log, log, dev, limit):
    subprocess.run([replay_log, "--log", log, "--replay", dev, "--limit", str(limit)],
                   check=True, capture_output=True)


def main():
    replay_log = os.environ["REPLAY_LOG"]
    blk = lambda c: bytes([c]) * SS
    ops = [
        ("m", "run:begin"), ("w", 0, blk(1)), ("w", 1, blk(2)), ("f",),
        ("m", "ack:a0.mochi:0:D"), ("w", 2, blk(3)), ("pf", 3, blk(4)), ("w", 4, blk(5)),
        ("fua", 5, blk(6)), ("m", "workload:end"),
    ]
    with tempfile.TemporaryDirectory() as t:
        log = os.path.join(t, "log")
        build_log(log, ops)
        es = lw.parse(log)
        assert len(es) == len(ops), es
        assert [e.label for e in es if e.is_mark] == ["run:begin", "ack:a0.mochi:0:D", "workload:end"]
        assert lw.find_mark(es, "workload:end").idx == 9
        # durable_limit against what replay-log actually writes.
        for e in (es[3], es[6], es[8]):
            dev = os.path.join(t, f"dev{e.idx}")
            open(dev, "wb").truncate(8 * SS)
            replay(replay_log, log, dev, lw.durable_limit(e))
            data = open(dev, "rb").read()
            # (sector 3 is the pre-flush's own data; stop before it.)
            present = {s for s in range(8) if data[s * SS : (s + 1) * SS] != b"\0" * SS}
            want = {0, 1} if e.idx == 3 else ({0, 1, 2} if e.idx == 6 else {0, 1, 2, 3, 4, 5})
            assert present == want, (e, present, want)
        pts = lw.select_points(es, 0, 9, [es[4]], max_flush=10, n_between=2, seed=1)
        kinds = [p["kind"] for p in pts]
        assert kinds.count("flush") == 3 and kinds.count("between") == 2, pts
        assert lw.select_points(es, 0, 9, [es[4]], 10, 2, 1) == pts, "selection must be deterministic"
    print("logwrites self-test ok")


main()
