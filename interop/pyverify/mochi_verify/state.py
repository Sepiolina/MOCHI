"""Manifest replay without SQLite (spec D10, D18; R3 section 7; R4 section 3.1 rules)."""
from __future__ import annotations

from dataclasses import dataclass, field

from .catalog import check_namespace
from .errors import MochiError
from .manifest import KIND_DELTA, KIND_SNAPSHOT, Manifest


@dataclass
class State:
    chunks: dict = field(default_factory=dict)      # id -> Chunk
    versions: dict = field(default_factory=dict)    # id -> Version
    ns: dict = field(default_factory=dict)          # path -> file version id
    expired: set = field(default_factory=set)
    holds: dict = field(default_factory=dict)       # label -> sequence
    keys: list | None = None                        # envelope ids (Encrypted)

    def copy(self) -> "State":
        return State(dict(self.chunks), dict(self.versions), dict(self.ns), set(self.expired),
                     dict(self.holds), None if self.keys is None else list(self.keys))


def check_version_extents(v, chunks: dict) -> None:
    """Spec 10.3 / R4 section 3.1: ordinals are positions (implicit), lengths > 0, no gap or overlap,
    total = logical length, chunk reads inside the chunk, holes at chunk offset 0."""
    pos = 0
    for lo, ln, cid, co in v.extents:
        if ln <= 0 or lo != pos:
            raise MochiError("EXTENT_INVALID", "extent gap, overlap or empty extent")
        if cid is not None:
            ch = chunks.get(cid)
            if ch is None or co + ln > ch.decoded_len:
                raise MochiError("EXTENT_INVALID", "extent reads outside its chunk (or the chunk is absent)")
        pos += ln
    if pos != v.length:
        raise MochiError("EXTENT_INVALID", "extents do not cover the logical length")


def from_snapshot(m: Manifest) -> State:
    if m.kind != KIND_SNAPSHOT:
        raise MochiError("RECORD_INVALID", "not a snapshot manifest")
    st = State()
    st.chunks = {c.id: c for c in m.chunks}
    st.versions = {v.id: v for v in m.versions}
    for v in m.versions:
        check_version_extents(v, st.chunks)
    for path, fv in m.entries:
        st.ns[path] = fv
        if fv not in st.versions:
            raise MochiError("NAMESPACE_INVALID", "entry names an unknown file version")
    check_namespace(st.ns, lambda f: st.versions[f].kind == 1)
    if m.retention_state is not None:
        st.expired = set(m.retention_state[0])
        st.holds = dict(m.retention_state[1])
    if m.key_state is not None:
        st.keys = list(m.key_state)
    return st


def apply_delta(prev: State | None, m: Manifest) -> State:
    """Apply one delta manifest atomically; ``prev`` is None for commit 0."""
    if m.kind != KIND_DELTA:
        raise MochiError("RECORD_INVALID", "not a delta manifest")
    st = State() if prev is None else prev.copy()
    if prev is None and m.sequence != 0:
        raise MochiError("RECORD_INVALID", "no previous state")
    # introduction: an introduced id must not already exist (D10.4)
    for c in m.chunks:
        if c.id in st.chunks:
            raise MochiError("RECORD_INVALID", "chunk introduced twice")
        st.chunks[c.id] = c
    for v in m.versions:
        if v.id in st.versions:
            raise MochiError("RECORD_INVALID", "file version introduced twice")
        st.versions[v.id] = v
    for v in m.versions:
        check_version_extents(v, st.chunks)
    for op in m.ops:
        if op[0] == "put":
            if op[2] not in st.versions:
                raise MochiError("NAMESPACE_INVALID", "put of an unknown file version")
            st.ns[op[1]] = op[2]
        else:
            if op[1] not in st.ns:
                raise MochiError("NAMESPACE_INVALID", "delete of an absent path")
            del st.ns[op[1]]
    check_namespace(st.ns, lambda f: st.versions[f].kind == 1)
    if m.retention_ops:
        for op in m.retention_ops:
            if op[0] == "expire":
                if op[1] in st.expired:
                    raise MochiError("RECORD_INVALID", "snapshot already expired")
                st.expired.add(op[1])
            elif op[0] == "hold":
                if op[1] in st.holds:
                    raise MochiError("RECORD_INVALID", "label already active")
                st.holds[op[1]] = op[2]
            else:
                if op[1] not in st.holds:
                    raise MochiError("RECORD_INVALID", "release of a label that is not active")
                del st.holds[op[1]]
    if m.schema == 3:
        keys = set(st.keys or [])
        for op, eid in (m.key_ops or []):
            if op == "add":
                if eid in keys:
                    raise MochiError("RECORD_INVALID", "key envelope added twice")
                keys.add(eid)
            else:
                if eid not in keys:
                    raise MochiError("RECORD_INVALID", "removal of an envelope that is not in the set")
                keys.discard(eid)
        if not keys:
            raise MochiError("RECORD_INVALID", "the key envelope set would become empty")
        st.keys = sorted(keys)
    return st


def states_equal(a: State, b: State) -> bool:
    return (a.chunks == b.chunks and a.versions == b.versions and a.ns == b.ns and a.expired == b.expired
            and a.holds == b.holds and (a.keys or None) == (b.keys or None))


def retained(state: State, head_seq: int, seq: int) -> bool:
    """Roots at head h: h, every commit not expired, every held commit (D18)."""
    return seq == head_seq or seq not in state.expired or seq in set(state.holds.values())


def live_closure(st: State):
    """File versions the namespace names, and the chunks their extents read."""
    vids = set(st.ns.values())
    cids = set()
    for vid in vids:
        for e in st.versions[vid].extents:
            if e[2] is not None:
                cids.add(e[2])
    return vids, cids


def snapshot_consistent(snap: State, full: State) -> bool:
    """D10.7 adoption check.  A snapshot manifest is self-contained: it holds the
    namespace, the file versions the namespace names, and the chunks those read
    (and whatever else it chooses to carry, if identical to the replayed record).
    Retention and key state are complete."""
    if snap.ns != full.ns or snap.expired != full.expired or snap.holds != full.holds:
        return False
    if (snap.keys or None) != (full.keys or None):
        return False
    vids, cids = live_closure(full)
    for vid in vids:
        if snap.versions.get(vid) != full.versions.get(vid):
            return False
    for cid in cids:
        if snap.chunks.get(cid) != full.chunks.get(cid):
            return False
    if any(full.versions.get(k) != v for k, v in snap.versions.items()):
        return False
    return all(full.chunks.get(k) == v for k, v in snap.chunks.items())
