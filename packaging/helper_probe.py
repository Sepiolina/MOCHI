#!/usr/bin/env python3
"""Ask an installed mochi-foreign-helper to list one archive (plan D8, D6, T16).

Speaks protocol v2 (crates/mochi-foreign/README.md) as the app's client does:
read HELLO, send one REQUEST (op `list`), read frames to DONE. Prints the entry
names, exits 0 only if HELLO carried version 2, FORMAT came back, DONE said
complete and (with --expect) every expected name was listed. Standard library
only; the helper never writes anything.

Usage: helper_probe.py HELPER ARCHIVE [--expect NAME]...
"""
import struct
import subprocess
import sys

PROTOCOL = 2
HELLO, FORMAT, ENTRY, DONE, ERROR = 0x80, 0x81, 0x82, 0x87, 0x8F


def frame(kind, body):
    return struct.pack("<IB", len(body) + 1, kind) + body


def read_exact(stream, n):
    buf = stream.read(n)
    if buf is None or len(buf) != n:
        sys.exit("helper_probe: helper closed its output early")
    return buf


def read_frame(stream):
    (length,) = struct.unpack("<I", read_exact(stream, 4))
    if not 1 <= length <= 1 << 20:
        sys.exit(f"helper_probe: bad frame length {length}")
    data = read_exact(stream, length)
    return data[0], data[1:]


def archive_path_bytes(path):
    # Raw bytes on Unix, UTF-16LE code units on Windows (protocol README).
    if sys.platform == "win32":
        return path.encode("utf-16-le")
    import os
    return os.fsencode(path)


def request(archive):
    body = struct.pack("<HBQI", PROTOCOL, 1, 0, 0)  # version, op list, read index, no indices
    # Limits: total, entry, entries, ratio, ratio-min-output, then u32 name length.
    body += struct.pack("<QQQQQI", 1 << 30, 1 << 30, 100_000, 200, 1 << 20, 4096)
    body += b"\x00\x00"  # no memory limit, no CPU limit
    path = archive_path_bytes(archive)
    body += struct.pack("<I", len(path)) + path
    body += b"\x00"  # no password
    return frame(0x01, body)


def main():
    args = sys.argv[1:]
    if len(args) < 2:
        sys.exit(__doc__)
    helper, archive, rest = args[0], args[1], args[2:]
    expect = [rest[i + 1] for i in range(0, len(rest) - 1, 2) if rest[i] == "--expect"]
    proc = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        kind, body = read_frame(proc.stdout)
        if kind != HELLO:
            sys.exit(f"helper_probe: first frame is 0x{kind:02x}, not HELLO")
        (version,) = struct.unpack("<H", body[:2])
        if version != PROTOCOL:
            sys.exit(f"helper_probe: helper speaks protocol {version}, expected {PROTOCOL}")
        print(f"HELLO protocol {version}")
        proc.stdin.write(request(archive))
        proc.stdin.flush()
        names, format_seen = [], False
        while True:
            kind, body = read_frame(proc.stdout)
            if kind == FORMAT:
                format_seen = True
            elif kind == ENTRY:
                (nlen,) = struct.unpack("<I", body[8:12])  # u64 index, then the name
                names.append(body[12:12 + nlen].decode("utf-8", "replace"))
            elif kind == DONE:
                complete = body[0] == 1
                break
            elif kind == ERROR:
                sys.exit(f"helper_probe: helper reported ERROR code {body[0]}")
        for n in names:
            print(f"entry {n}")
        if not format_seen or not complete:
            sys.exit("helper_probe: listing had no FORMAT or was not complete")
        missing = [n for n in expect if n not in names]
        if missing:
            sys.exit(f"helper_probe: entries not listed: {missing}")
    finally:
        proc.stdin.close()
        proc.stdout.close()
        proc.wait(timeout=30)
    print("helper list OK")


if __name__ == "__main__":
    main()
