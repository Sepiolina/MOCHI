"""Whole-archive verification: head, chain, replay, objects, profiles.

Everything here is derived from the specification and the ratification
artifacts; see READING-LOG.md for the readings chosen where they were silent.
"""
from __future__ import annotations

import zstandard

from dataclasses import dataclass, field

from . import catalog, commit as commit_mod, crypto, descriptor as desc_mod, digest, footer as footer_mod
from . import frames, manifest as manifest_mod, objects, parity, state as state_mod, tar as tar_mod
from .envelope import SEAL_DATA, SEAL_DELTA, SEAL_IMAGE, SEAL_SNAPSHOT, decode_binary, decode_image, parse_sealed
from .errors import MochiError, is_violation
from .frames import K_COMMIT, K_DESCRIPTOR, K_FOOTER, K_IMAGE, K_KEY_ENVELOPE, K_MANIFEST, K_SEALED, ZSTD_MAGIC


@dataclass
class Node:
    footer: footer_mod.Footer
    commit: commit_mod.Commit
    delta: manifest_mod.Manifest | None = None
    snap: manifest_mod.Manifest | None = None
    delta_err: MochiError | None = None
    snap_err: MochiError | None = None
    state: state_mod.State | None = None
    state_err: MochiError | None = None
    snap_state: state_mod.State | None = None
    state_minimal: bool = False        # state taken from a snapshot manifest alone (no history)

    @property
    def seq(self) -> int:
        return self.commit.sequence


@dataclass
class Result:
    head_status: str = "refused"
    head_code: str | None = None
    head_message: str = ""
    archive_id: bytes | None = None
    descriptor: desc_mod.Descriptor | None = None
    nodes: list = field(default_factory=list)
    snapshots: dict = field(default_factory=dict)      # seq -> State (retained, ascending)
    findings: set = field(default_factory=set)         # (code, object hex | None)
    notes: list = field(default_factory=list)
    parity: dict | None = None
    keyless: bool = False
    tail: str = "none"                                 # none | uncommitted | unresolved
    healed: bytes | None = None
    stored_notes: list = field(default_factory=list)


class Verifier:
    def __init__(self, raw: bytes, passphrase: bytes | None = None, expected_head: bytes | None = None,
                 use_parity: bool = True):
        self.use_parity = use_parity
        self.raw = bytes(raw)
        self.view = bytearray(self.raw)
        self.passphrase = passphrase
        self.expected_head = expected_head
        self.res = Result()
        self.dek: bytes | None = None
        self.desc: desc_mod.Descriptor | None = None
        self.nodes: list[Node] = []
        self.bad_offsets: set[int] = set()   # stored data frames already reported as failing their hash

    # ------------------------------------------------------------ helpers
    def find(self, code: str, msg: str = "", obj: bytes | str | None = None, chunk: bool = False) -> None:
        """Record a finding.  The summary names the stored-object hash only for a damaged
        data chunk (fixtures/golden/README.md, "R8 summaries"); any other object is a note."""
        if not is_violation(code):
            self.res.notes.append(f"{code}: {msg}")
            return
        if isinstance(obj, bytes):
            obj = obj.hex()
        if not chunk:
            if obj is not None:
                msg = f"{msg} [object {obj}]"
            obj = None
        self.res.findings.add((code, obj))
        if msg:
            self.res.notes.append(f"{code}: {msg}")

    def refuse(self, e: MochiError) -> Result:
        r = self.res
        r.head_status = "refused"
        r.head_code = e.code
        r.head_message = e.message
        return r

    # ------------------------------------------------------------ run
    def run(self) -> Result:
        try:
            self._parity_pre()
            head = self._find_head()
            self._tail(head)
            self._chain(head)
            self._descriptor()
            self._segments()
            self._open_head()
        except MochiError as e:
            if e.code == "INVALID_ARGUMENT":
                raise
            return self.refuse(e)
        self.res.head_status = "keyless" if self.res.keyless else "opened"
        self.res.head_code = None
        self._verify_all()
        self._finish()
        return self.res

    # ------------------------------------------------------------ parity
    def _parity_pre(self) -> None:
        """Use parity in memory only (ladder step 6).  Applies when the descriptor
        declares a recovery record (R8 summary rules: an undecodable descriptor means no scan)."""
        if not self.use_parity:
            return
        v = self.view
        want = None
        try:
            fr = frames.walk_frame(v, 0)
            if fr.magic == K_DESCRIPTOR:
                try:
                    want = desc_mod.decode_object(bytes(v[:fr.length])).recovery
                except MochiError:
                    want = None
        except MochiError:
            pass
        head_off = self._head_footer_offset_parse()
        if head_off is None:
            return
        try:
            f = footer_mod.parse_footer(v, head_off)
        except MochiError:
            return
        # fixtures/golden/README.md (R8 summaries): the scan runs only when the descriptor at offset 0 decodes
        # and declares a percentage; a descriptor that does not decode gives no parity, even if parity frames follow
        if want is None:
            return
        self._scan_parity(f, want[1])

    def _footer_candidates(self):
        """Footers that may be the head, best first: the footer at end of file and the
        footer frames the forward walk meets, latest first.  A magic match is a
        candidate only at a frame boundary of the structural walk (8.5); where the
        walk itself broke, a footer beyond the break is a candidate too."""
        v = self.view
        n = len(v)
        fr, stop, err = frames.walk_all(v)
        boundaries = {f.offset for f in fr}
        out = []
        p = n - footer_mod.FOOTER_LEN
        if p >= 0 and (p in boundaries or (err is not None and p >= stop)):
            out.append(p)
        for f in reversed(fr):
            if f.magic == K_FOOTER and f.length == footer_mod.FOOTER_LEN and f.offset not in out:
                out.append(f.offset)
        return out

    def _head_footer_offset_parse(self):
        """A footer to start the parity walk from: parsed, not validated (its digest
        covers a commit frame that may itself need rebuilding)."""
        v = self.view
        cands = self._footer_candidates()
        if cands and cands[0] == len(v) - footer_mod.FOOTER_LEN:
            try:
                footer_mod.parse_footer(v, cands[0])
                return cands[0]
            except MochiError:
                pass
        for off in cands:
            try:
                footer_mod.validate_footer(v, off)
                return off
            except MochiError:
                continue
        return None

    def _scan_parity(self, head_footer: footer_mod.Footer, percent: int | None) -> None:
        res = parity.ScanResult()
        healed = self.view
        f = head_footer
        seen = set()
        while True:
            if f.offset in seen:
                break
            seen.add(f.offset)
            c0, clen = f.commit_offset, f.commit_length
            if clen < 8 or c0 + clen > f.offset or clen > frames.COMMIT_FRAME_LIMIT:
                break
            try:
                cm = commit_mod.decode_object(bytes(healed[c0:c0 + clen]))
            except MochiError:
                # the commit may need rebuilding: heal this commit's range first, then retry
                cm = None
            if cm is None:
                # parity of the *next* commit... not available: try once with a provisional rebuild
                cm = self._provisional_commit(healed, f)
                if cm is None:
                    break
            a = cm.parent[2] if cm.parent else 0
            parity.heal_commit(healed, cm, f, a, percent, res)
            if cm.parent is None:
                break
            try:
                f = footer_mod.validate_footer(healed, cm.parent[2])
            except MochiError:
                break
        if res.commits or res.findings:
            self.res.parity = {"commits": res.commits, "damaged_shards": res.damaged_shards,
                               "rebuilt": res.rebuilt, "beyond_capacity": res.beyond_capacity}
            for code, obj in res.findings:
                self.find(code, "", obj)
            self.res.notes.extend(res.notes)
        self.view = healed

    def _provisional_commit(self, healed, f):
        """The commit frame is damaged: its own parity can rebuild it.  Rebuild with a
        throw-away commit stand-in carrying only what the cross-checks need."""
        c0, clen = f.commit_offset, f.commit_length
        c_end = c0 + clen
        tmpl, _ = parity.find_template(healed, c_end, f.offset)
        if tmpl is None:
            return None
        a = tmpl.range_offset
        stub = commit_mod.Commit(2 if tmpl.features else 1, tmpl.archive_id, tmpl.sequence, tmpl.transaction_id,
                                 (b"", tmpl.sequence - 1, a) if tmpl.sequence else None, True, None, None, None,
                                 None, tmpl.features, None, b"", None)
        res = parity.ScanResult()
        parity.heal_commit(healed, stub, f, a, None, res)
        try:
            return commit_mod.decode_object(bytes(healed[c0:c0 + clen]))
        except MochiError:
            return None

    # ------------------------------------------------------------ head
    def _find_head(self) -> footer_mod.Footer:
        v = self.view
        for off in self._footer_candidates():
            try:
                f = footer_mod.validate_footer(v, off)
                footer_mod.require_commit_kind(v, f)
                return f
            except MochiError:
                continue
        raise MochiError("NO_VALID_HEAD", "no valid committed footer")

    def _tail(self, head: footer_mod.Footer) -> None:
        """D14 eligibility is a conservative screen; a tail that is not eligible may hold a commit."""
        v = self.view
        start = head.offset + footer_mod.FOOTER_LEN
        if start >= len(v):
            return
        self.find("FOOTER_INVALID", "the end of the file is not the head's footer; the head was found by scanning")
        tail = bytes(v[start:])
        eligible = True
        fr, stop, err = frames.walk_all(v, start)
        if err is not None:
            # allowed: exactly one frame cut short by the end of the file, with a recognised magic
            remaining = len(v) - stop
            ok = (err.code == "TRUNCATED" and remaining >= 8
                  and (frames.is_skippable(frames.u32(v, stop)) or frames.u32(v, stop) == ZSTD_MAGIC))
            if not ok:
                eligible = False
        for f in fr:
            if f.magic in (K_FOOTER, K_DESCRIPTOR):
                eligible = False
        if len(tail) >= footer_mod.FOOTER_LEN:
            fm = K_FOOTER.to_bytes(4, "little")
            mm = footer_mod.FOOTER_MAGIC
            for p in range(0, len(tail) - footer_mod.FOOTER_LEN + 1):
                if tail[p:p + 4] == fm or tail[p + 8:p + 16] == mm:
                    eligible = False
                    break
        if eligible:
            self.res.tail = "uncommitted"
            self.res.notes.append("UNCOMMITTED_TAIL: bytes after the head are eligible for truncation")
        else:
            self.res.tail = "unresolved"
            self.find("TAIL_UNRESOLVED", "bytes after the head are not eligible for truncation")

    # ------------------------------------------------------------ chain
    def _chain(self, head: footer_mod.Footer) -> None:
        v = self.view
        nodes: list[Node] = []
        f = head
        last_off = None
        while True:
            c0, clen = f.commit_offset, f.commit_length
            cm = commit_mod.decode_object(bytes(v[c0:c0 + clen]))
            if cm.sequence != f.sequence:
                raise MochiError("FOOTER_INVALID", "footer sequence differs from the commit's")
            commit_mod.check_refs_before(cm, c0)
            nodes.append(Node(f, cm))
            if cm.parent is None:
                break
            pid, pseq, poff = cm.parent
            if last_off is not None and poff >= last_off:
                raise MochiError("RECORD_INVALID", "parent traversal offset does not go backwards")
            pf = footer_mod.validate_footer(v, poff)        # FOOTER_INVALID if it does not resolve
            footer_mod.require_commit_kind(v, pf)
            if poff + footer_mod.FOOTER_LEN > c0:
                raise MochiError("RECORD_INVALID", "parent footer does not precede the commit frame")
            if pf.sequence != pseq:
                raise MochiError("RECORD_INVALID", "parent footer's commit has another sequence")
            pc = commit_mod.decode_object(bytes(v[pf.commit_offset:pf.commit_offset + pf.commit_length]))
            if pc.commit_id != pid:
                raise MochiError("RECORD_INVALID", "parent footer's commit is not the declared parent")
            last_off = f.offset
            f = pf
        nodes.reverse()
        for i, nd in enumerate(nodes):
            if nd.seq != i:
                raise MochiError("RECORD_INVALID", "commit sequences are not 0..n")
            if nd.commit.archive_id != nodes[0].commit.archive_id:
                raise MochiError("RECORD_INVALID", "commits of more than one archive")
        # base rule (D10.6): the parent if it is a checkpoint, else the parent's base
        for i, nd in enumerate(nodes):
            c = nd.commit
            if c.checkpoint:
                continue
            par = nodes[i - 1].commit
            want = (par.commit_id, par.sequence) if par.checkpoint else (par.base[0], par.base[1])
            if (c.base[0], c.base[1]) != want:
                raise MochiError("RECORD_INVALID", "recorded base differs from the base rule")
        self.nodes = nodes
        self.res.nodes = nodes
        self.res.archive_id = nodes[0].commit.archive_id

    # ------------------------------------------------------------ descriptor
    def _descriptor(self) -> None:
        v = self.view
        head = self.nodes[-1].commit
        seg_base = self._segment_base(len(self.nodes) - 1)
        first = None
        for nd in self.nodes:
            r = nd.commit.descriptor
            ref_key = (r.offset, r.length, r.hash)
            if first is None:
                first = ref_key
            if ref_key != first:
                if nd.seq >= seg_base:
                    raise MochiError("DESCRIPTOR_INVALID", "commits of one segment reference different descriptors")
                self.find("DESCRIPTOR_INVALID", f"commit {nd.seq} references another descriptor")
        r = head.descriptor
        data = bytes(v[0:r.length])
        if len(data) != r.length or digest.stored_object(data) != r.hash:
            raise MochiError("DESCRIPTOR_INVALID", "descriptor bytes do not hash to the commit's reference")
        d = desc_mod.decode_object(data)
        for nd in self.nodes:
            if d.archive_id != nd.commit.archive_id:
                raise MochiError("DESCRIPTOR_INVALID", "descriptor archive id differs from a commit's")
            if d.encrypted != nd.commit.encrypted:
                raise MochiError("DESCRIPTOR_INVALID", "descriptor and commit disagree about the Encrypted feature")
        self.desc = d
        self.res.descriptor = d

    def _segment_base(self, idx: int) -> int:
        c = self.nodes[idx].commit
        return c.sequence if c.checkpoint else c.base[1]

    # ------------------------------------------------------------ keys
    def _segments(self) -> None:
        if not self.desc.encrypted:
            return
        head = self.nodes[-1].commit
        v = self.view
        envs = []
        for ref in head.key_envelopes:
            data = bytes(v[ref.offset:ref.end])
            if digest.stored_object(data) != ref.hash:
                self.find("STORED_INTEGRITY_FAILED", "key envelope", ref.hash)
                continue
            try:
                envs.append(self._decode_envelope(data, head))
            except MochiError as e:
                self.find(e.code, "key envelope: " + e.message)
        # R3 7.1 (commit v2): the head's envelopes carry one key ID and are listed in strictly
        # increasing envelope-ID order; a record that breaks this is refused, whatever its CDDL says
        if len(envs) == len(head.key_envelopes):
            if len({e.key_id for e in envs}) > 1:
                raise MochiError("RECORD_INVALID", "the head's key envelopes name different data keys")
            ids = [e.envelope_id for e in envs]
            if ids != sorted(set(ids)):
                raise MochiError("RECORD_INVALID", "the head's key envelopes are not distinct and in increasing ID order")
        self.res.keyless = self.passphrase is None
        if self.passphrase is None:
            return
        for env in envs:
            dek = crypto.unwrap(env, self.passphrase)
            if dek is not None:
                self.dek = dek
                return
        raise MochiError("KEY_UNAVAILABLE", "no key envelope opens with the supplied passphrase")

    def _decode_envelope(self, data: bytes, c: commit_mod.Commit) -> crypto.KeyEnvelope:
        fr = frames.walk_frame(data, 0)
        if fr.magic != K_KEY_ENVELOPE or fr.length != len(data):
            raise MochiError("MALFORMED_FRAME", "not one key-envelope frame")
        env = crypto.decode_envelope_payload(data[8:])
        if env.archive_id != c.archive_id:
            raise MochiError("ENVELOPE_INVALID", "key envelope archive id differs from the commit's")
        if env.sequence > c.sequence:
            raise MochiError("ENVELOPE_INVALID", "key envelope written after the commit that lists it")
        return env

    # ------------------------------------------------------------ objects
    def _object(self, ref: commit_mod.ObjectRef, what: str) -> bytes:
        data = bytes(self.view[ref.offset:ref.end])
        if len(data) != ref.length or digest.stored_object(data) != ref.hash:
            raise MochiError("STORED_INTEGRITY_FAILED", f"{what}: stored length or hash mismatch", ref.hash.hex())
        return data

    def _unseal(self, data: bytes, c: commit_mod.Commit, kind: int, object_id: bytes | None = None) -> bytes:
        fr = frames.walk_frame(data, 0)
        if fr.magic != K_SEALED or fr.length != len(data):
            raise MochiError("ENVELOPE_INVALID", "reference names a frame that is not a sealed object")
        sh = parse_sealed(data[8:])
        if sh.kind != kind:
            raise MochiError("ENVELOPE_INVALID", "sealed object is not of the kind the reference names")
        if sh.key_id != self._key_id(c):
            raise MochiError("RECORD_INVALID", "sealed object key id differs from the envelopes'")
        binding = crypto.binding_for(kind, object_id, c.sequence, c.transaction_id)
        pt = crypto.open_sealed(self.dek, c.archive_id, sh, binding)
        if pt is None:
            raise MochiError("CONTENT_INTEGRITY_FAILED", "sealed object fails authentication")
        return pt

    def _key_id(self, c: commit_mod.Commit) -> bytes:
        # all envelopes of one archive carry the same key id; read it from the first listed
        if not hasattr(self, "_kid"):
            ref = c.key_envelopes[0]
            data = bytes(self.view[ref.offset:ref.end])
            self._kid = crypto.decode_envelope_payload(data[8:]).key_id
        return self._kid

    def _load_manifest(self, nd: Node, ref: commit_mod.ObjectRef, kind: int, what: str) -> manifest_mod.Manifest:
        c = nd.commit
        data = self._object(ref, what)
        if c.encrypted:
            if self.dek is None:
                raise MochiError("KEY_UNAVAILABLE", "manifest is sealed")
            pt = self._unseal(data, c, SEAL_DELTA if kind == 0 else SEAL_SNAPSHOT)
            m = manifest_mod.decode_payload(pt)
        else:
            m = manifest_mod.decode_object(data)
        if m.archive_id != c.archive_id or m.sequence != c.sequence or m.transaction_id != c.transaction_id:
            raise MochiError("ENVELOPE_INVALID", f"{what} identity differs from the commit's")
        if m.features != c.features or (m.schema == 3) != c.encrypted:
            raise MochiError("RECORD_INVALID", f"{what} and its commit disagree about required features")
        if m.kind != kind:
            raise MochiError("RECORD_INVALID", f"{what} is of the wrong kind")
        return m

    def _load_delta(self, nd: Node) -> None:
        if nd.delta is not None or nd.delta_err is not None:
            return
        try:
            m = self._load_manifest(nd, nd.commit.delta, 0, "delta manifest")
            if nd.seq > 0:
                par = self.nodes[nd.seq - 1].commit
                if m.parent != (nd.seq - 1, par.delta.hash):
                    raise MochiError("RECORD_INVALID", "delta manifest's parent link differs from the parent commit's")
            nd.delta = m
        except MochiError as e:
            nd.delta_err = e

    def _load_snap(self, nd: Node) -> None:
        if nd.snap is not None or nd.snap_err is not None or not nd.commit.checkpoint:
            return
        try:
            nd.snap = self._load_manifest(nd, nd.commit.snapshot, 1, "snapshot manifest")
        except MochiError as e:
            nd.snap_err = e

    # ------------------------------------------------------------ open / states
    def _compute_states(self, upto: int | None = None) -> None:
        if self.res.keyless:
            return
        for nd in self.nodes:
            if upto is not None and nd.seq > upto:
                break
            self._state_of(nd)

    def _state_of(self, nd: Node) -> None:
        if nd.state is not None or nd.state_err is not None:
            return
        self._load_delta(nd)
        self._load_snap(nd)
        prev = self.nodes[nd.seq - 1].state if nd.seq > 0 else None
        replay = None
        replay_err = nd.delta_err
        if nd.delta is not None and (nd.seq == 0 or prev is not None):
            try:
                replay = state_mod.apply_delta(prev, nd.delta)
            except MochiError as e:
                replay_err = e
        elif nd.delta is not None:
            replay_err = MochiError("RECORD_INVALID", "no earlier state to apply the delta to")
            replay_err.cascade = True          # a consequence of an earlier failure, reported there
        snap = None
        snap_err = nd.snap_err
        if nd.snap is not None:
            try:
                snap = state_mod.from_snapshot(nd.snap)
            except MochiError as e:
                snap_err = e
        if nd.commit.checkpoint:
            if snap is not None and replay is not None:
                if not state_mod.snapshot_consistent(snap, replay):
                    self.find("CHECKPOINT_MISMATCH", f"commit {nd.seq}: snapshot manifest differs from the replayed delta chain")
                nd.state = replay          # the full replay is what an image-based open sees
                nd.snap_state = snap
            elif snap is not None:
                nd.state = snap
                nd.snap_state = snap
                nd.state_minimal = True
            elif replay is not None:
                nd.state = replay
            else:
                nd.state_err = snap_err or replay_err
        else:
            if replay is not None:
                nd.state = replay
            else:
                nd.state_err = replay_err
        if nd.state is None and nd.state_err is None:
            nd.state_err = MochiError("RECORD_INVALID", "state unavailable")

    def _open_head(self) -> None:
        if self.res.keyless:
            return
        h = len(self.nodes) - 1
        hn = self.nodes[h]
        base = self._segment_base(h)
        # the head's segment must open: deltas base+1..h (and the head's own delta) verify and replay
        self._compute_states()
        for nd in self.nodes[base + 1:h + 1]:
            if nd.delta_err is not None:
                raise nd.delta_err
        if hn.delta_err is not None:
            raise hn.delta_err
        if hn.state is None:
            for nd in self.nodes[base:h + 1]:
                if nd.state_err is not None:
                    raise nd.state_err
            raise MochiError("RECORD_INVALID", "head state unavailable")
        # the base image (or, failing that, the snapshot manifest) establishes the segment's start
        self._check_image(self.nodes[base], strict=False, refuse=True)

    # ------------------------------------------------------------ verification pass
    def _check_image(self, nd: Node, strict: bool, refuse: bool = False) -> None:
        """An image that fails its stored hash (or its AEAD tag) is damage: reads rebuild the catalog from
        the snapshot manifest (D10 item 9) and the failure is a finding.  An image whose bytes are
        authentic but which then fails the envelope, schema or relationship checks (R4 3) is not
        damage; when it is the head's base image the head is refused (spec D10 item 9, clarified)."""
        c = nd.commit
        if not c.checkpoint or getattr(nd, "image_done", False):
            return
        nd.image_done = True
        try:
            data = self._object(c.image, "catalog image")
            if c.encrypted:
                if self.dek is None:
                    return
                pt = self._unseal(data, c, SEAL_IMAGE)
                data = frames.K_IMAGE.to_bytes(4, "little") + len(pt).to_bytes(4, "little") + pt
            env = decode_binary(data, K_IMAGE, (1,))
            env.bind(c.archive_id, c.sequence, c.transaction_id)
            if env.features != ([1] if c.encrypted else []):
                raise MochiError("RECORD_INVALID", "image features differ from the archive's profile")
            st = catalog.check_image(decode_image(env))
            if st.archive_id != c.archive_id or st.head_seq != c.sequence:
                raise MochiError("RECORD_INVALID", "image's archive id or head sequence differs from the commit's")
            self._compare_image(nd, st)
        except MochiError as e:
            if refuse and e.code not in ("STORED_INTEGRITY_FAILED", "CONTENT_INTEGRITY_FAILED", "KEY_UNAVAILABLE"):
                raise
            self.find(e.code, f"commit {nd.seq} catalog image: {e.message}", e.obj)

    def _compare_image(self, nd: Node, im: catalog.ImageState) -> None:
        s = nd.state
        if s is None:
            return
        ok = im.namespace == s.ns
        if nd.state_minimal:
            # the state came from a snapshot manifest alone: the image may hold more history
            ok = ok and set(s.chunks) <= set(im.chunks) and set(s.versions) <= set(im.versions)
        else:
            ok = ok and set(im.chunks) == set(s.chunks) and set(im.versions) == set(s.versions)
        if ok:
            for cid, ch in s.chunks.items():
                prot, dl, chash = im.chunks[cid]
                if (prot == "aead") != (ch.protection == 1) or dl != ch.decoded_len or chash != ch.content_hash:
                    ok = False
                if im.objects.get(cid) != (ch.stored_len, ch.stored_hash):
                    ok = False
                if ch.offset is not None and im.locations.get(cid) != ch.offset:
                    ok = False
            for fid, v in s.versions.items():
                kind, ln, chash = im.versions[fid]
                if (kind == "directory") != (v.kind == 1) or ln != v.length or chash != v.content_hash:
                    ok = False
                exts = [(e[1], e[2], e[3], e[4]) for e in im.extents.get(fid, [])]
                if exts != list(v.extents):
                    ok = False
        if not ok:
            self.find("CHECKPOINT_MISMATCH", f"commit {nd.seq}: catalog image differs from the snapshot manifest")

    def _verify_all(self) -> None:
        v = self.view
        # every commit's referenced objects
        for nd in self.nodes:
            c = nd.commit
            ids = []
            for ref in c.key_envelopes:
                try:
                    data = self._object(ref, "key envelope")
                    env = self._decode_envelope(data, c)
                    ids.append(env.envelope_id)
                    kid = getattr(self, "_seen_key_id", None)
                    if kid is None:
                        self._seen_key_id = env.key_id
                    elif kid != env.key_id:
                        self.find("RECORD_INVALID", f"commit {nd.seq}: envelopes carry different key ids")
                except MochiError as e:
                    self.find(e.code, f"commit {nd.seq} key envelope: {e.message}",
                              e.obj or (ref.hash if e.code == "STORED_INTEGRITY_FAILED" else None))
            nd.env_ids = ids
            if len(ids) == len(c.key_envelopes) and (ids != sorted(set(ids))):
                self.find("RECORD_INVALID", f"commit {nd.seq}: key envelopes not distinct and in increasing id order")
            if c.data_region is not None:
                self._check_region(nd)
            for ref, what in ((c.delta, "delta manifest"),) + (((c.snapshot, "snapshot manifest"),) if c.checkpoint else ()):
                try:
                    self._object(ref, what)
                except MochiError as e:
                    self.find(e.code, f"commit {nd.seq}: {e.message}", e.obj)
        if not self.res.keyless:
            self._compute_states()
            for nd in self.nodes:
                for err in (nd.delta_err, nd.snap_err):
                    if err is not None:
                        self.find(err.code, f"commit {nd.seq}: {err.message}", err.obj)
                if nd.state is None and nd.state_err is not None and not getattr(nd.state_err, "cascade", False):
                    self.find(nd.state_err.code, f"commit {nd.seq} state: {nd.state_err.message}", nd.state_err.obj)
                if nd.commit.checkpoint:
                    self._check_image(nd, strict=True)
                ids = getattr(nd, "env_ids", None)
                if nd.state is not None and nd.state.keys is not None and ids is not None \
                        and len(ids) == len(nd.commit.key_envelopes) and sorted(ids) != list(nd.state.keys):
                    self.find("RECORD_INVALID", f"commit {nd.seq}: key envelope set differs from the replayed key state")
            self._check_baselines()
            self._check_chunks_and_files()
            if self.desc.tar_compatible:
                self._check_tar()
        else:
            for nd in self.nodes:
                c = nd.commit
                if c.checkpoint:
                    for ref, what in ((c.image, "catalog image"),):
                        try:
                            self._object(ref, what)
                        except MochiError as e:
                            self.find(e.code, e.message, e.obj)
        self._freshness()

    def _check_baselines(self) -> None:
        """B.2.8 reference scope: recovery from a checkpoint's snapshot manifest alone must
        reach every later state of its segment.  A delta that reads something the baseline
        does not hold opens normally but fails baseline recovery: REFERENCE_INVALID."""
        for nd in self.nodes:
            if not nd.commit.checkpoint or nd.snap is None:
                continue
            try:
                st = state_mod.from_snapshot(nd.snap)
            except MochiError:
                continue
            for later in self.nodes[nd.seq + 1:]:
                if later.commit.checkpoint:
                    break
                if later.delta is None or later.state is None:
                    break                      # already reported (or the full replay failed too)
                try:
                    st = state_mod.apply_delta(st, later.delta)
                except MochiError as e:
                    self.find("REFERENCE_INVALID",
                              f"commit {later.seq} cannot be recovered from the baseline at {nd.seq}: {e.message}")
                    break

    def _check_region(self, nd: Node) -> None:
        c = nd.commit
        r = c.data_region
        try:
            data = bytes(self.view[r.offset:r.end])
            if len(data) != r.length or digest.stored_object(data) != r.hash:
                raise MochiError("STORED_INTEGRITY_FAILED", "data region hash mismatch", r.hash.hex())
        except MochiError as e:
            self.find(e.code, f"commit {nd.seq}: {e.message}", e.obj)
            return
        if r.end > c.delta.offset:
            self.find("RECORD_INVALID", f"commit {nd.seq}: data region does not end before the delta manifest")
            return
        pos = r.offset
        while pos < r.end:
            try:
                fr = frames.walk_frame(self.view, pos, r.end)
            except MochiError:
                self.find("RECORD_INVALID", f"commit {nd.seq}: data region does not walk as complete frames")
                return
            if fr.magic != K_SEALED:
                self.find("RECORD_INVALID", f"commit {nd.seq}: data region holds a frame that is not a sealed object")
                return
            try:
                sh = parse_sealed(bytes(self.view[fr.offset + 8:fr.end]))
                if sh.kind != SEAL_DATA:
                    raise MochiError("RECORD_INVALID", "not a data-chunk sealed object")
            except MochiError as e:
                self.find(e.code, f"commit {nd.seq}: data region: {e.message}")
                return
            pos = fr.end

    # ------------------------------------------------------------ chunks and files
    def _check_chunks_and_files(self) -> None:
        # every chunk record any verified manifest introduced
        chunks: dict[bytes, tuple[manifest_mod.Chunk, int]] = {}
        for nd in self.nodes:
            for m in (nd.delta, nd.snap):
                if m is None:
                    continue
                for ch in m.chunks:
                    chunks.setdefault(ch.id, (ch, nd.seq))
        index = None
        bad: set[bytes] = set()
        decoded: dict[bytes, bytes] = {}
        cache_bytes = 0
        for cid, (ch, seq) in chunks.items():
            if ch.deps:
                # spec 24.2 (missing shared dictionary): a dependency that resolves to a catalog
                # object is UNSUPPORTED_FEATURE; one naming an object the catalog lacks is dangling
                if any(dep_id not in chunks for _, dep_id in ch.deps):
                    self.find("REFERENCE_INVALID", "chunk depends on an object the catalog does not hold")
                else:
                    self.res.notes.append("UNSUPPORTED_FEATURE: chunk depends on a dictionary or key envelope")
                bad.add(cid)
                continue
            off = ch.offset
            if off is None:
                if index is None:
                    index = self._hash_index()
                off = index.get(ch.stored_hash)
                if off is None:
                    self.find("STORED_INTEGRITY_FAILED", "chunk cannot be rediscovered by its stored hash", ch.stored_hash, chunk=True)
                    bad.add(cid)
                    continue
            data = bytes(self.view[off:off + ch.stored_len])
            if len(data) != ch.stored_len or digest.stored_object(data) != ch.stored_hash:
                self.find("STORED_INTEGRITY_FAILED", "chunk stored length or hash mismatch", ch.stored_hash, chunk=True)
                self.bad_offsets.add(off)
                bad.add(cid)
                continue
            plain = self._chunk_plain(data, ch, cid, seq)
            if plain is None:
                bad.add(cid)
                continue
            if plain is False:
                continue                      # keyless: stored bytes only
            if cache_bytes < 128 * 1024 * 1024:
                decoded[cid] = plain
                cache_bytes += len(plain)
        if self.res.keyless:
            return
        # files: whole-file length and plain BLAKE3
        seen: set[bytes] = set()
        for nd in self.nodes:
            st = nd.state
            if st is None:
                continue
            for fid, ver in st.versions.items():
                if fid in seen or ver.kind != 0:
                    continue
                seen.add(fid)
                if any(e[2] is not None and e[2] in bad for e in ver.extents):
                    continue
                buf = bytearray()
                complete = True
                for lo, ln, cid, co in ver.extents:
                    if cid is None:
                        buf += bytes(ln)
                        continue
                    pl = decoded.get(cid)
                    if pl is None:
                        complete = False
                        break
                    buf += pl[co:co + ln]
                if not complete:
                    continue
                if len(buf) != ver.length or digest.file_content(bytes(buf)) != ver.content_hash:
                    self.find("CONTENT_INTEGRITY_FAILED", "file content differs from its recorded length or hash")

    def _hash_index(self) -> dict:
        idx = {}
        fr, _, _ = frames.walk_all(self.view)
        for f in fr:
            idx.setdefault(digest.stored_object(bytes(self.view[f.offset:f.end])), f.offset)
        return idx

    def _chunk_plain(self, data: bytes, ch: manifest_mod.Chunk, cid: bytes, seq: int):
        """Decoded bytes of a chunk, None on a finding, False when no key is held."""
        nd = self.nodes[seq]
        try:
            if nd.commit.encrypted:
                if self.dek is None:
                    return False
                pt = self._unseal(data, nd.commit, SEAL_DATA, cid)
            else:
                pt = data
            return self._decode_zstd(pt, ch)
        except MochiError as e:
            self.find(e.code, f"chunk: {e.message}", ch.stored_hash, chunk=True)
            return None

    def _decode_zstd(self, frame: bytes, ch: manifest_mod.Chunk) -> bytes:
        return objects.decode_chunk_frame(frame, ch.decoded_len, ch.content_hash)

    # ------------------------------------------------------------ TAR-compatible profile
    def _check_tar(self) -> None:
        for nd in self.nodes:
            m = nd.delta
            if m is None or nd.state is None:
                continue
            c = nd.commit
            a = self.nodes[nd.seq - 1].footer.offset if nd.seq else 0
            data_frames = []
            pos = a
            end = nd.footer.commit_offset
            while pos < end:
                try:
                    fr = frames.walk_frame(self.view, pos, end)
                except MochiError:
                    break
                if fr.magic == ZSTD_MAGIC:
                    data_frames.append(fr)
                pos = fr.end
            if any(fr.offset in self.bad_offsets for fr in data_frames):
                self.res.notes.append(f"commit {nd.seq}: framing chunk already reported as failing its stored hash")
                continue
            puts = [op for op in m.ops if op[0] == "put"]
            if not puts:
                if data_frames:
                    self.find("PROFILE_VIOLATION", f"commit {nd.seq}: data frames without puts")
                continue
            try:
                stream = b"".join(self._plain_frame(fr) for fr in data_frames)
                members = tar_mod.parse_stream(stream)
                if len(members) != len(puts):
                    raise MochiError("PROFILE_VIOLATION", "stream members differ in number from the commit's puts")
                for mem, put in zip(members, puts):
                    ver = nd.state.versions[put[2]]
                    tar_mod.expected_matches(mem, put[1], ver)
                    if ver.kind == 0 and digest.file_content(mem.content) != ver.content_hash:
                        raise MochiError("PROFILE_VIOLATION", "re-emitted content differs from the file version")
            except MochiError as e:
                if e.code in ("PROFILE_VIOLATION",):
                    self.find("PROFILE_VIOLATION", f"commit {nd.seq}: {e.message}")
                else:
                    self.find(e.code, f"commit {nd.seq} tar framing chunk: {e.message}", e.obj)

    def _plain_frame(self, fr: frames.Frame) -> bytes:
        data = bytes(self.view[fr.offset:fr.end])
        hdr = frames.data_frame_header(data)
        if hdr["content_size"] is None:
            raise MochiError("MALFORMED_FRAME", "framing chunk without Frame_Content_Size")
        try:
            return zstandard.ZstdDecompressor().decompress(data, max_output_size=hdr["content_size"] + 1)
        except zstandard.ZstdError as e:
            raise MochiError("CONTENT_INTEGRITY_FAILED", "framing chunk: %s" % e) from None

    # ------------------------------------------------------------ freshness
    def _freshness(self) -> None:
        if self.expected_head is None:
            return
        if not any(nd.commit.commit_id == self.expected_head for nd in self.nodes):
            self.find("FRESHNESS_FAILED", "the verified history does not contain the expected head")

    # ------------------------------------------------------------ finish
    def _finish(self) -> None:
        r = self.res
        r.healed = bytes(self.view)
        if self.res.keyless or not self.nodes:
            return
        for nd in self.nodes:
            if nd.state is not None:
                r.snapshots[nd.seq] = nd.state


def verify(raw: bytes, passphrase: bytes | None = None, expected_head: bytes | None = None) -> Result:
    """Two passes, as the reference does (spec B.2.11 item 12): the bytes as stored, whose findings are
    all reported, then, when a recovery record rebuilds anything, the in-memory rebuilt view, which
    gives the chain, the snapshots and the damaged data chunks the rebuild could not heal."""
    stored = Verifier(raw, passphrase, expected_head, use_parity=False).run()
    healed = Verifier(raw, passphrase, expected_head).run()
    if healed.parity is None:
        return healed
    codes = {c for c, _ in stored.findings}
    if stored.head_status == "refused" and stored.head_code:
        codes.add(stored.head_code)
    out_findings = {(c, o) for c, o in healed.findings if o is not None}
    out_findings |= {(c, None) for c in codes}
    out_findings |= {(c, o) for c, o in healed.findings if c in PARITY_CODES}
    healed.findings = out_findings
    healed.stored_notes = stored.notes
    return healed


PARITY_CODES = ("PARITY_DAMAGED", "PARITY_CAPACITY_EXCEEDED", "PROFILE_VIOLATION")
