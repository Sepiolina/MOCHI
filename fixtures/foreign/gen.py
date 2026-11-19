#!/usr/bin/env python3
"""Generates the hostile-archive fixtures for mochi-foreign (plan D8).

Run from anywhere:  python3 -I fixtures/foreign/gen.py
Output goes next to this script. Everything is deterministic (fixed mtimes,
fixed seeds) except Info-ZIP's own output, which is reproducible given the
pinned mtimes below. The fixtures are COMMITTED; regenerating them is a
deliberate act whose diff must be explained (fixtures/foreign/README.md).

Writers are independent of the code under test: Python's zipfile/tarfile, Info-ZIP
`zip`, GNU `tar`, and `gzip`. The one exception is the AES-encrypted ZIP, which
no installed tool can write; tests build it in memory with the `zip` crate and
say so.
"""
import gzip, io, os, random, shutil, stat, subprocess, sys, tarfile, tempfile, time, zipfile

OUT = os.path.dirname(os.path.abspath(__file__))
MTIME = 1577836800  # 2020-01-01T00:00:00Z
ZT = (2020, 1, 1, 0, 0, 0)


def w(name, data):
    with open(os.path.join(OUT, name), "wb") as f:
        f.write(data)


def zinfo(name, method=zipfile.ZIP_DEFLATED, attr=0o100644, raw_flags=None):
    zi = zipfile.ZipInfo(name, ZT)
    zi.compress_type = method
    zi.external_attr = attr << 16
    zi.create_system = 3
    return zi


def mkzip(name, entries, comment=b""):
    """entries: list of (ZipInfo-or-name, bytes)."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for zi, data in entries:
            if isinstance(zi, str):
                zi = zinfo(zi)
            z.writestr(zi, data)
        z.comment = comment
    w(name, buf.getvalue())
    return buf.getvalue()


def infozip(name, files, extra=(), cwd=None):
    """Runs Info-ZIP zip -X on prepared files with pinned mtimes."""
    with tempfile.TemporaryDirectory() as d:
        for rel, data in files.items():
            p = os.path.join(d, rel)
            if rel.endswith("/"):
                os.makedirs(p, exist_ok=True)
                os.utime(p, (MTIME, MTIME))
                continue
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "wb") as f:
                f.write(data)
            os.utime(p, (MTIME, MTIME))
        out = os.path.join(d, "_out.zip")
        names = sorted(files)
        subprocess.run(["zip", "-X", "-q", out, *extra, *names], cwd=d, check=True,
                       env={**os.environ, "TZ": "UTC"})
        return out, d, open(out, "rb").read()


# ---- ZIP --------------------------------------------------------------------

BASIC = {"a.txt": b"hello\n", "sub/b.txt": b"second file\n" * 50, "empty/": b""}
_, _, data = infozip("basic_infozip.zip", BASIC)
w("basic_infozip.zip", data)

# ZipCrypto (the weak legacy cipher), written by Info-ZIP. Password: "secret".
_, _, data = infozip("zipcrypto.zip", {"secret.txt": b"classified bytes\n" * 20}, extra=("-P", "secret"))
w("zipcrypto.zip", data)

# Hostile names. Each is a name the C6 restore engine must reject or report;
# the helper must carry the bytes exactly and judge nothing.
mkzip("traversal.zip", [
    ("../evil.txt", b"x"),
    ("/abs/path.txt", b"x"),
    ("a/../../b.txt", b"x"),
    ("C:\\win.txt", b"x"),
    ("dir\\back.txt", b"x"),
    ("CON", b"x"),
    ("nul.txt", b"x"),
    ("file.txt:stream", b"x"),
    ("A.txt", b"upper"),
    ("a.txt", b"lower"),
    ("trailing.dot.", b"x"),
    ("日本語.txt", b"utf8 name"),
])
# Two entries with one name (and a third, distinct one). Readers disagree on
# which wins; the zip library keeps one, and the helper must say it did.
import warnings
with warnings.catch_warnings():
    warnings.simplefilter("ignore")
    mkzip("duplicate_names.zip", [("dup.txt", b"first"), ("dup.txt", b"second"), ("other.txt", b"o")])

# A name stored in CP437 without the UTF-8 flag: byte 0x82 is "e-acute" in
# CP437. Python would set the UTF-8 flag for a non-ASCII str, so the name is
# written as "caf?.txt" and the "?" is patched to 0x82 in both headers.
zi = zinfo("caf?.txt")
raw = mkzip("_tmp_cp437.zip", [(zi, b"legacy code page")])
os.remove(os.path.join(OUT, "_tmp_cp437.zip"))
assert raw.count(b"caf?.txt") == 2
w("cp437_name.zip", raw.replace(b"caf?.txt", b"caf\x82.txt"))

# Symlink entries (Unix mode S_IFLNK in the external attributes). The data is
# the link target; one escapes the destination.
mkzip("symlink.zip", [
    (zinfo("link_out", attr=stat.S_IFLNK | 0o777), b"../../etc/passwd"),
    (zinfo("link_in", attr=stat.S_IFLNK | 0o777), b"a.txt"),
    ("a.txt", b"hello"),
    (zinfo("d/", attr=stat.S_IFDIR | 0o755), b""),
])

# Bombs. 8 MiB of zeros deflate to ~8 KiB (ratio ~1000:1).
ZEROS8 = b"\0" * (8 << 20)
mkzip("bomb_ratio.zip", [("zeros.bin", ZEROS8)])
# 300 empty entries (entry-count limit tests use a smaller limit).
mkzip("bomb_count.zip", [(f"e{i:03}.txt", b"") for i in range(300)])
# Many small, individually fine entries whose sum exceeds a total limit.
mkzip("bomb_total.zip", [(f"chunk{i}.bin", os.urandom(0) + bytes([i]) * (600 * 1024)) for i in range(6)])
# A name over a 256-byte limit.
mkzip("long_name.zip", [("n" * 1000 + ".txt", b"x")])

# Integrity damage.
good = mkzip("_tmp_stored.zip", [(zinfo("data.bin", zipfile.ZIP_STORED), b"hello world, stored and checked\n")])
os.remove(os.path.join(OUT, "_tmp_stored.zip"))
i = good.index(b"hello world")
w("crc_corrupt.zip", good[:i] + b"J" + good[i + 1:])  # one flipped data byte, stored: CRC32 mismatch
dflt = mkzip("_tmp_deflate.zip", [("data.txt", b"the quick brown fox jumps over the lazy dog\n" * 200)])
os.remove(os.path.join(OUT, "_tmp_deflate.zip"))
j = dflt.index(b"data.txt") + len(b"data.txt") + 4
w("deflate_corrupt.zip", dflt[:j] + bytes([dflt[j] ^ 0xFF]) + dflt[j + 1:])  # damaged deflate stream
w("truncated.zip", data_basic := open(os.path.join(OUT, "basic_infozip.zip"), "rb").read()[:-40])  # EOCD cut off

# Other methods and variants.
mkzip("method_bzip2.zip", [(zinfo("b.txt", zipfile.ZIP_BZIP2), b"bzip2 compressed\n" * 30)])
mkzip("method_lzma.zip", [(zinfo("l.txt", zipfile.ZIP_LZMA), b"lzma compressed\n" * 30)])
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w") as z:
    with z.open(zinfo("big64.txt"), "w", force_zip64=True) as f:
        f.write(b"zip64 local header\n")
w("zip64_forced.zip", buf.getvalue())

# Split ZIP: Info-ZIP -s writes name.z01 .. and name.zip (last segment).
with tempfile.TemporaryDirectory() as d:
    rnd = random.Random(7)
    open(os.path.join(d, "big.bin"), "wb").write(bytes(rnd.getrandbits(8) for _ in range(100_000)))
    subprocess.run(["zip", "-X", "-q", "-s", "64k", "split.zip", "big.bin"], cwd=d, check=True)
    shutil.copy(os.path.join(d, "split.z01"), os.path.join(OUT, "split.z01"))
    shutil.copy(os.path.join(d, "split.zip"), os.path.join(OUT, "split_last.zip"))

# SFX: a stub that begins like a PE file ("MZ"), with a ZIP appended. The stub
# is synthetic (not executable); detection must not depend on it running.
w("sfx.exe", b"MZ" + b"\x90" * 62 + b"This program cannot be run in DOS mode.\r\n" + b"\0" * 400
  + open(os.path.join(OUT, "basic_infozip.zip"), "rb").read())

# ---- tar --------------------------------------------------------------------

def mktar(name, entries, fmt=tarfile.GNU_FORMAT, gz=False, mtime=MTIME):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=fmt) as t:
        for ti, data in entries:
            ti.mtime = mtime
            ti.uid = ti.gid = 0
            ti.uname = ti.gname = ""
            t.addfile(ti, io.BytesIO(data) if data is not None else None)
    raw = buf.getvalue()
    if gz:
        raw = gzip_bytes(raw)
    w(name, raw)
    return raw


def gzip_bytes(raw):
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as g:
        g.write(raw)
    return out.getvalue()


def reg(name, size, mode=0o644):
    ti = tarfile.TarInfo(name)
    ti.size = size
    ti.mode = mode
    return ti


def special(name, typ, link="", mode=0o777):
    ti = tarfile.TarInfo(name)
    ti.type = typ
    ti.linkname = link
    ti.mode = mode
    return ti


basic_entries = [
    (special("d/", tarfile.DIRTYPE, mode=0o755), None),
    (reg("a.txt", 6), b"hello\n"),
    (reg("d/b.txt", 600), b"second file\n" * 50),
]
raw = mktar("basic.tar", basic_entries)
w("basic.tar.gz", gzip_bytes(raw))
mktar("pax.tar", [(reg("p" * 150 + "/" + "q" * 150 + ".txt", 3), b"pax")], fmt=tarfile.PAX_FORMAT)
mktar("longname_gnu.tar", [(reg("g" * 300 + ".txt", 3), b"gnu")], fmt=tarfile.GNU_FORMAT)

hostile = [
    (reg("../evil.txt", 1), b"x"),
    (reg("/abs/path.txt", 1), b"x"),
    (reg("a/../../b.txt", 1), b"x"),
    (special("link_out", tarfile.SYMTYPE, "../../etc/passwd"), None),
    (special("hard", tarfile.LNKTYPE, "a.txt"), None),
    (special("dev_null", tarfile.CHRTYPE), None),
    (special("fifo", tarfile.FIFOTYPE), None),
    (reg("CON", 1), b"x"),
    (reg("Dup.txt", 1), b"1"),
    (reg("dup.txt", 1), b"2"),
]
mktar("traversal.tar", hostile)

zeros = b"\0" * (8 << 20)
raw_bomb = mktar("_tmp.tar", [(reg("zeros.bin", len(zeros)), zeros)])
os.remove(os.path.join(OUT, "_tmp.tar"))
w("bomb_ratio.tar.gz", gzip_bytes(raw_bomb))
raw_cnt = mktar("_tmp.tar", [(reg(f"e{i:03}.txt", 0), b"") for i in range(300)])
os.remove(os.path.join(OUT, "_tmp.tar"))
w("bomb_count.tar.gz", gzip_bytes(raw_cnt))
w("bomb_count.tar", raw_cnt)

# Damage.
gzraw = gzip_bytes(raw)
w("gz_crc_corrupt.tar.gz", gzraw[:-8] + bytes([gzraw[-8] ^ 0xFF]) + gzraw[-7:])  # trailer CRC32 wrong
# 20 KB of incompressible data so the gzip stream is long enough to cut after
# the first tar header (cutting inside the first 512 bytes would not even
# be recognised as a tar.gz).
rnd = random.Random(11)
rawr = mktar("_tmp.tar", [(reg("rand.bin", 20000), bytes(rnd.getrandbits(8) for _ in range(20000)))])
os.remove(os.path.join(OUT, "_tmp.tar"))
gzr = gzip_bytes(rawr)
w("truncated.tar.gz", gzr[: len(gzr) * 6 // 10])
big = mktar("_tmp.tar", [(reg("big.bin", 100_000), bytes(range(256)) * 391 + bytes(range(64)))])
os.remove(os.path.join(OUT, "_tmp.tar"))
w("truncated.tar", big[:60_000])  # data cut off mid-entry

# Not supported / not archives.
w("single.gz", gzip_bytes(b"just a gzipped text file\n" * 10))
w("fake.7z", b"7z\xBC\xAF\x27\x1C\x00\x04" + b"\0" * 64)
w("fake.rar", b"Rar!\x1A\x07\x01\x00" + b"\0" * 64)
w("garbage.bin", bytes(random.Random(3).getrandbits(8) for _ in range(2000)))
w("empty.bin", b"")
print("fixtures written to", OUT)
