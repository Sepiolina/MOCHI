"""Command line: ``python3 -I -m mochi_verify ARCHIVE [--passphrase-file F] [--expected-head HEX] --summary``."""
from __future__ import annotations

import argparse
import os
import sys

from . import archive, crypto, summary
from .errors import MochiError


def _passphrase(path: str) -> bytes:
    with open(path, "rb") as fh:
        line = fh.readline()
    line = line.rstrip(b"\r\n")
    return crypto.passphrase_bytes(line.decode("utf-8"))


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="mochi_verify", description="Independent read-only MOCHI verifier")
    ap.add_argument("archive")
    ap.add_argument("--passphrase-file")
    ap.add_argument("--expected-head", help="expected head commit id (hex)")
    ap.add_argument("--summary", action="store_true", help="print the mochi-interop-summary-v1 JSON document")
    ap.add_argument("--case", help="case name for the summary (default: the archive's file name without extension)")
    args = ap.parse_args(argv)
    try:
        with open(args.archive, "rb") as fh:       # read-only: the archive is never opened for writing
            raw = fh.read()
        pw = _passphrase(args.passphrase_file) if args.passphrase_file else None
        eh = bytes.fromhex(args.expected_head) if args.expected_head else None
        if eh is not None and len(eh) != 32:
            raise MochiError("INVALID_ARGUMENT", "--expected-head must be 64 hex digits")
        res = archive.verify(raw, pw, eh)
        if res.head_status == "refused" and res.head_code == "KEY_UNAVAILABLE" and not args.summary:
            raise MochiError("KEY_UNAVAILABLE", res.head_message)
    except (OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 3
    except MochiError as e:
        print(f"error: {e}", file=sys.stderr)
        return 3 if e.code in ("KEY_UNAVAILABLE", "INVALID_ARGUMENT", "IO_ERROR") else 4 if e.code == "UNSUPPORTED_FEATURE" else 1
    case = args.case or os.path.splitext(os.path.basename(args.archive))[0]
    doc = summary.summarize(case, res)
    if args.summary:
        sys.stdout.write(summary.dumps(doc))
        return 0
    print(f"head: {doc['head']['status']} {doc['head']['code'] or ''}".rstrip())
    for f in doc["findings"]:
        print(f"finding: {f['code']} {f['object'] or ''}".rstrip())
    return 0 if res.head_status in ("opened", "keyless") and not doc["findings"] else 1
