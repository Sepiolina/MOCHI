#!/usr/bin/env python3
"""Run the verifier over every case of the R8 golden set and compare with the reference summaries.

``cases.json`` is the index (fixtures/golden/README.md, "R8"): for each case the archive is
``<case>.mochi``, the passphrase is the case's ``passphrase`` (null: read keylessly) and the freshness anchor
is its ``expected_head``.  Every case must have a summary and every summary a case.  The two JSON documents are compared as
parsed values, so whitespace and key order are irrelevant.  The exit status is 1 if any case differs, and
the differing cases are listed.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
import tempfile


def diff(a, b, path="$", out=None, limit=12):
    out = [] if out is None else out
    if len(out) >= limit:
        return out
    if type(a) is not type(b):
        out.append(f"{path}: expected {json.dumps(a)[:80]} but got {json.dumps(b)[:80]}")
    elif isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a:
                out.append(f"{path}.{k}: unexpected key")
            elif k not in b:
                out.append(f"{path}.{k}: missing key")
            else:
                diff(a[k], b[k], f"{path}.{k}", out, limit)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: expected {len(a)} items but got {len(b)}")
        for i, (x, y) in enumerate(zip(a, b)):
            diff(x, y, f"{path}[{i}]", out, limit)
    elif a != b:
        out.append(f"{path}: expected {json.dumps(a)[:80]} but got {json.dumps(b)[:80]}")
    return out[:limit]


def find_archive(base: pathlib.Path, stem: str):
    for cand in (base / f"{stem}.mochi", base / stem):
        if cand.is_file():
            return cand
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("r8", nargs="?", default=str(pathlib.Path(__file__).resolve().parents[3] / "fixtures" / "golden" / "r8"))
    args = ap.parse_args()
    root = pathlib.Path(args.r8)
    if not root.is_dir():
        print(f"{root} does not exist: the comparison cannot run")
        return 2
    cases = {c["name"]: c for c in json.loads((root / "cases.json").read_text())}
    expected = sorted(root.rglob("*.summary.json"))
    on_disk = {p.name[: -len(".summary.json")] for p in expected if p.parent == root}
    bad: dict[str, list[str]] = {}
    for stem in sorted(set(cases) ^ on_disk):
        bad[stem] = ["in cases.json but no summary" if stem in cases else "a summary but not in cases.json"]
    for exp_path in expected:
        stem = exp_path.name[: -len(".summary.json")]
        if exp_path.parent != root or stem not in cases:
            continue
        case = cases[stem]
        archive = find_archive(exp_path.parent, stem)
        if archive is None:
            bad[stem] = ["no archive file next to the summary"]
            continue
        cmd = [sys.executable, "-I", "-m", "mochi_verify", str(archive), "--summary", "--case", stem]
        with tempfile.TemporaryDirectory() as tmp:
            if case["passphrase"] is not None:
                pw = pathlib.Path(tmp) / "passphrase"
                pw.write_text(case["passphrase"] + "\n")
                cmd += ["--passphrase-file", str(pw)]
            if case["expected_head"] is not None:
                cmd += ["--expected-head", case["expected_head"]]
            p = subprocess.run(cmd, capture_output=True, text=True)
        if p.returncode != 0:
            bad[stem] = [f"verifier exited {p.returncode}: {p.stderr.strip()[:200]}"]
            continue
        want = json.loads(exp_path.read_text())
        got = json.loads(p.stdout)
        d = diff(want, got)
        if d:
            bad[stem] = d
    print(f"{len(cases)} cases compared, {len(bad)} differ")
    for stem, d in bad.items():
        print(f"MISMATCH {stem}")
        for line in d:
            print(f"    {line}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
