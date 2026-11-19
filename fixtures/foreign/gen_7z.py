#!/usr/bin/env python3
"""Generates the 7z fixtures for mochi-foreign (plan D8, slice 2).

    python3 -I fixtures/foreign/gen_7z.py        # needs `7z` on PATH and, for
                                                 # the hostile-name archives, py7zr

Ordinary archives come from the real 7-Zip command line (the version is
recorded in 7z/PROVENANCE.txt). 7-Zip refuses to store names such as
`../evil.txt`, so the hostile-name archives come from py7zr, an independent
pure-Python writer. Fixtures are COMMITTED; regenerating them is a deliberate
act whose diff must be explained. Mtimes are pinned; 7-Zip's own output is
reproducible given them. Nothing here writes RAR (never, plan D8 rule 1): the
RAR fixtures in rar/ are real archives copied from the `unrar` crate's test
data (see rar/PROVENANCE.txt) and byte-level mutations of them.
"""
import os, random, shutil, subprocess, sys, tempfile

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "7z")
MTIME = 1577836800


def sz(name, args, files, password=None):
    """Runs 7z on a prepared tree; `files` maps relative path -> bytes (a key
    ending in / is a directory)."""
    with tempfile.TemporaryDirectory() as d:
        for rel, data in files.items():
            p = os.path.join(d, rel)
            if rel.endswith("/"):
                os.makedirs(p, exist_ok=True)
            else:
                os.makedirs(os.path.dirname(p) or d, exist_ok=True)
                with open(p, "wb") as f:
                    f.write(data)
            os.utime(p, (MTIME, MTIME))
        dest = os.path.join(OUT, name)
        for root, dirs, files_ in os.walk(d):
            for n in dirs + files_:
                os.utime(os.path.join(root, n), (MTIME, MTIME))
        os.utime(d, (MTIME, MTIME))
        for old in os.listdir(OUT):
            if old == name or old.startswith(name + "."):
                os.remove(os.path.join(OUT, old))
        cmd = ["7z", "a", "-bd", "-y", "-mtm=on", "-mtc=off", "-mta=off"] + args
        if password:
            cmd.append("-p" + password)
        cmd += [dest] + sorted(os.listdir(d))
        subprocess.run(cmd, cwd=d, check=True, stdout=subprocess.DEVNULL)
    return dest


def rd(n):
    return open(os.path.join(OUT, n), "rb").read()


def wr(n, data):
    with open(os.path.join(OUT, n), "wb") as f:
        f.write(data)


BASIC = {
    "a.txt": b"hello\n",
    "empty/": b"",
    "sub/b.txt": b"second file\n" * 50,
    "zero.txt": b"",
}

random.seed(7)
NOISE = bytes(random.randrange(256) for _ in range(900))

# --- ordinary archives (real 7-Zip) ---
sz("basic.7z", ["-mx=5"], BASIC)
sz("nonsolid.7z", ["-ms=off"], BASIC)
sz("copy.7z", ["-m0=Copy"], BASIC)
sz("ppmd.7z", ["-m0=PPMd"], BASIC)
sz("bzip2.7z", ["-m0=BZip2"], BASIC)
sz("lzma_d16m.7z", ["-m0=LZMA:d=16m"], BASIC)
sz("enc_data.7z", ["-ms=off"], BASIC, password="SECRET")
sz("enc_names.7z", ["-mhe=on"], BASIC, password="SECRET")
# Encrypted but stored (no compressor): a wrong password decrypts to garbage that
# no decoder rejects, so only the CRC32 at the end of the entry says so.
sz("enc_copy.7z", ["-m0=Copy", "-ms=off"], BASIC, password="SECRET")
# A split set: plain byte splits, read as one stream (plan D8 rule 8).
sz("split.7z", ["-m0=Copy", "-v400b"], {"noise.bin": NOISE, "a.txt": b"hello\n"})
vols = sorted(f for f in os.listdir(OUT) if f.startswith("split.7z."))
assert len(vols) >= 3, vols

# --- corruptions of real archives ---
copy = bytearray(rd("copy.7z"))
# Stored data starts at byte 32; flip a byte inside "second file" (entry b.txt).
at = bytes(copy).find(b"second file")
assert at > 0
copy[at + 3] ^= 0x01
wr("crc_corrupt.7z", bytes(copy))

basic = rd("basic.7z")
wr("truncated.7z", basic[: len(basic) * 6 // 10])
bad_start = bytearray(basic)
bad_start[8] ^= 0xFF  # start-header CRC
wr("bad_start_crc.7z", bytes(bad_start))
bad_hdr = bytearray(basic)
bad_hdr[-5] ^= 0xFF  # inside the (compressed) header
wr("bad_header.7z", bytes(bad_hdr))
# Split set with a volume missing from the middle: .001 and .003 only.
os.makedirs(os.path.join(OUT, "gap"), exist_ok=True)
shutil.copy(os.path.join(OUT, "split.7z.001"), os.path.join(OUT, "gap", "split.7z.001"))
shutil.copy(os.path.join(OUT, "split.7z.003"), os.path.join(OUT, "gap", "split.7z.003"))
# Split set that stops early (last volume absent).
os.makedirs(os.path.join(OUT, "short"), exist_ok=True)
for v in vols[:-1]:
    shutil.copy(os.path.join(OUT, v), os.path.join(OUT, "short", v))

# --- bombs (real 7-Zip) ---
sz("bomb_ratio.7z", ["-mx=9"], {"zeros.bin": bytes(8 << 20)})
sz("bomb_count.7z", ["-ms=off"], {f"f{i:03}.txt": b"" for i in range(300)})

# --- hostile names (py7zr: 7-Zip will not store these) ---
try:
    import py7zr
except ImportError:
    print("py7zr missing: traversal.7z not regenerated", file=sys.stderr)
else:
    dest = os.path.join(OUT, "traversal.7z")
    if os.path.exists(dest):
        os.remove(dest)
    names = [
        "../evil.txt", "/abs/path.txt", "a/../../b.txt", "C:\\win.txt",
        "dir\\back.txt", "CON", "nul.txt", "file.txt:stream", "A.txt", "a.txt",
        "trail.", "ok/fine.txt", "./dot/slash.txt",
    ]
    with py7zr.SevenZipFile(dest, "w") as z:
        for n in names:
            z._writestr(("data of " + n).encode(), n)  # skips py7zr's own path check, on purpose
    dest = os.path.join(OUT, "long_name.7z")
    with py7zr.SevenZipFile(dest, "w") as z:
        z._writestr(b"x", ("n" * 1004) + ".txt")
print("ok")

# --- a header that claims a 2 GiB LZMA dictionary (patched, CRCs repaired) ---
import struct, zlib
sz("lzma_raw_header.7z", ["-m0=LZMA:d=16m", "-mhc=off", "-ms=off"], BASIC)
raw = bytearray(rd("lzma_raw_header.7z"))
marker = bytes([0x03, 0x01, 0x01, 0x05])  # LZMA method id + 5 property bytes
at = bytes(raw).find(marker)
assert at > 32, "raw header should carry the LZMA coder"
while at > 0:
    raw[at + 5 : at + 9] = struct.pack("<I", 0x8000_0000)  # dictionary size
    at = bytes(raw).find(marker, at + 1)
off, size = struct.unpack_from("<QQ", raw, 12)
nh = bytes(raw[32 + off : 32 + off + size])
struct.pack_into("<I", raw, 28, zlib.crc32(nh))
struct.pack_into("<I", raw, 8, zlib.crc32(bytes(raw[12:32])))
wr("lzma_huge_dict.7z", bytes(raw))
os.remove(os.path.join(OUT, "lzma_raw_header.7z"))
print("ok2")

# --- an AES-256 ZIP written by 7-Zip (interop for the `zip` crate's reader) ---
# Slice 1 could only test AES-ZIP against the same library's writer; 7-Zip
# writes WinZip-style AES, so this is the interop evidence.
sz("zip_aes256_7zip.zip", ["-tzip", "-mem=AES256"], BASIC, password="SECRET")
print("ok3")
