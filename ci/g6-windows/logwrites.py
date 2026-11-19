"""Reader for dm-log-writes logs, and replay-point selection (gate G6).

The log format is the kernel's (drivers/md/dm-log-writes.c) as read by
xfstests' replay-log (src/log-writes/log-writes.c): sector 0 holds the
superblock; each entry is one log sector holding the header (and, for a mark,
its label) followed by `nr_sectors` sectors of data. Entry numbers are the
ones replay-log's `--limit` counts: replaying with `--limit N` applies
entries 0..N-1.
"""

import random
import struct

MAGIC = 0x6A736677736872
FLUSH, FUA, DISCARD, MARK, METADATA = 1, 2, 4, 8, 16


class Entry:
    __slots__ = ("idx", "sector", "nr_sectors", "flags", "label")

    def __init__(self, idx, sector, nr_sectors, flags, label):
        self.idx = idx
        self.sector = sector
        self.nr_sectors = nr_sectors
        self.flags = flags
        self.label = label

    @property
    def is_mark(self):
        return bool(self.flags & MARK)

    @property
    def is_barrier(self):
        return bool(self.flags & (FLUSH | FUA))

    def __repr__(self):
        return f"Entry({self.idx}, sector={self.sector}, n={self.nr_sectors}, flags={self.flags:#x}, label={self.label!r})"


def parse(path):
    """All entries of the log at `path`, in order."""
    out = []
    with open(path, "rb") as f:
        head = f.read(28)
        magic, version, nr_entries, sectorsize = struct.unpack("<QQQI", head)
        if magic != MAGIC or version != 1:
            raise ValueError(f"{path}: not a dm-log-writes log (magic {magic:#x}, version {version})")
        pos = sectorsize
        for idx in range(nr_entries):
            f.seek(pos)
            raw = f.read(sectorsize)
            if len(raw) != sectorsize:
                raise ValueError(f"{path}: truncated at entry {idx}")
            sector, nr_sectors, flags, data_len = struct.unpack("<QQQQ", raw[:32])
            label = None
            if flags & MARK:
                label = raw[32 : 32 + data_len].decode("utf-8", "replace")
            out.append(Entry(idx, sector, nr_sectors, flags, label))
            pos += sectorsize + nr_sectors * sectorsize
    return out


def durable_limit(e):
    """`--limit` that applies everything the entry `e` makes durable.

    A bare FLUSH makes every earlier write durable, so replaying it or
    stopping before it is the same prefix. A FLUSH that carries data is a
    pre-flush: its own data is not durable, so stop before it. A FUA write
    is durable itself.
    """
    if e.flags & FUA:
        return e.idx + 1
    if e.flags & FLUSH:
        return e.idx if e.nr_sectors else e.idx + 1
    return e.idx + 1


def find_mark(entries, label, start=0):
    for e in entries[start:]:
        if e.is_mark and e.label == label:
            return e
    return None


def last_barrier_before(entries, idx):
    best = None
    for e in entries[:idx]:
        if e.is_barrier:
            best = e
    return best


def select_points(entries, begin, end, ack_marks, max_flush, n_between, seed):
    """Replay points inside the workload (entries begin..end).

    Returns dicts with `kind` ("flush", "between", "edge"), `limit`, and the
    entry index the point is anchored on. Every barrier adjacent to an
    acknowledgement is always kept; the rest are thinned evenly to
    `max_flush`. `n_between` points land after ordinary writes, seeded.
    """
    region = entries[begin : end + 1]
    barriers = [e for e in region if e.is_barrier]
    keep = {}
    for m in ack_marks:
        before = last_barrier_before(entries, m.idx)
        if before is not None and before.idx >= begin:
            keep[before.idx] = before
        after = next((e for e in region if e.is_barrier and e.idx > m.idx), None)
        if after is not None:
            keep[after.idx] = after
    rest = [e for e in barriers if e.idx not in keep]
    room = max(0, max_flush - len(keep))
    if len(rest) > room:
        step = len(rest) / room if room else 0
        rest = [rest[int(i * step)] for i in range(room)] if room else []
    for e in rest:
        keep[e.idx] = e
    points = [
        {"kind": "flush", "anchor": e.idx, "limit": durable_limit(e)}
        for e in sorted(keep.values(), key=lambda e: e.idx)
    ]

    writes = [e for e in region if not e.is_mark and not e.is_barrier and e.nr_sectors]
    rng = random.Random(seed)
    picks = sorted(rng.sample(writes, min(n_between, len(writes))), key=lambda e: e.idx)
    points += [{"kind": "between", "anchor": e.idx, "limit": e.idx + 1} for e in picks]
    return points
