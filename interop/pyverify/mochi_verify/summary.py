"""The ``mochi-interop-summary-v1`` document (the summary format of the R9 interoperability run)."""
from __future__ import annotations

import json

from .archive import Result

FORMAT = "mochi-interop-summary-v1"


def _entry(path: bytes, ver) -> dict:
    mtime = None
    if ver.mtime is not None:
        mtime = ver.mtime[0] * 1_000_000_000 + ver.mtime[1]
    posix = ver.posix
    return {
        "path": path.hex(),
        "kind": "dir" if ver.kind == 1 else "file",
        "size": ver.length,
        "content_hash": ver.content_hash.hex() if ver.content_hash is not None else None,
        "target": None,
        "attributes": {
            "posix_mode": posix[0] if posix else None,
            "uid": posix[1] if posix else None,
            "gid": posix[2] if posix else None,
            "mtime_ns": mtime,
            "windows": ver.windows,
        },
    }


def _parity(res: Result):
    d = res.descriptor
    if res.parity is not None:      # a scan that reached commits is reported even when the head is then refused
        return res.parity
    if res.head_status not in ("opened", "keyless"):
        return None
    if d is not None and d.recovery is not None:
        return {"commits": 0, "damaged_shards": 0, "rebuilt": 0, "beyond_capacity": 0}
    return None


def summarize(case: str, res: Result) -> dict:
    opened = res.head_status in ("opened", "keyless")
    # A refused head verified nothing: the document then carries the refusal and nothing else.
    d = res.descriptor if opened else None
    profile = {
        "encrypted": bool(d.encrypted) if d else False,
        "tar_compatible": bool(d.tar_compatible) if d else False,
        "recovery_percent": d.recovery[1] if d and d.recovery else None,
    }
    commits = []
    snapshots = []
    if opened:
        for nd in res.nodes:
            commits.append({"seq": nd.seq, "commit_id": nd.commit.commit_id.hex(),
                            "footer_offset": nd.footer.offset, "checkpoint": nd.commit.checkpoint})
        if res.keyless:
            snapshots = None                    # content-level fields are null without the key
        else:
            for seq in sorted(res.snapshots):
                st = res.snapshots[seq]
                entries = [_entry(p, st.versions[st.ns[p]]) for p in sorted(st.ns)]
                snapshots.append({"seq": seq, "entries": entries})
    # One row per damaged data chunk; a code with no such chunk appears once, with a null object
    # (fixtures/golden/README.md, "R8 summaries").  The head's refusal is a finding too.
    rows = set(res.findings)
    if res.head_status == "refused" and res.head_code:
        rows.add((res.head_code, None))
    with_object = {c for c, o in rows if o is not None}
    findings = sorted(({"code": c, "object": o} for c, o in rows if o is not None or c not in with_object),
                      key=lambda f: (f["code"], f["object"] or ""))
    return {
        "format": FORMAT,
        "case": case,
        "head": {"status": res.head_status, "code": res.head_code},
        "archive_id": res.archive_id.hex() if (opened and res.archive_id) else None,
        "profile": profile,
        "commits": commits,
        "snapshots": snapshots,
        "findings": findings,
        "parity": _parity(res),
    }


def dumps(doc: dict) -> str:
    return json.dumps(doc, sort_keys=True, indent=2) + "\n"
