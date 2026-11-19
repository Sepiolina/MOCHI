"""Catalog image check against R4 (spec 10.5; R4 sections 1-3).

SQLite is used only through a temporary file (the verifier never trusts the
image's bytes beyond what these checks establish).
"""
from __future__ import annotations

import os
import sqlite3
import tempfile
from dataclasses import dataclass, field

from .errors import MochiError
from .paths import check_path

APPLICATION_ID = 0x4D4F4348
USER_VERSION = 1
PAGE_SIZE = 4096
IMAGE_LIMIT = 268_434_864

DDL = """
CREATE TABLE archive_meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
) STRICT;

CREATE TABLE commits (
    seq        INTEGER PRIMARY KEY CHECK (seq >= 0),
    parent_seq INTEGER REFERENCES commits(seq),
    CHECK (parent_seq IS NULL OR parent_seq < seq)
) STRICT;

CREATE TABLE objects (
    object_id   BLOB PRIMARY KEY CHECK (length(object_id) = 32),
    stored_len  INTEGER NOT NULL CHECK (stored_len > 0),
    stored_hash BLOB NOT NULL CHECK (length(stored_hash) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE object_locations (
    object_id     BLOB PRIMARY KEY REFERENCES objects(object_id),
    stored_offset INTEGER NOT NULL CHECK (stored_offset >= 0)
) STRICT, WITHOUT ROWID;

CREATE TABLE chunks (
    object_id    BLOB PRIMARY KEY REFERENCES objects(object_id),
    encoding     TEXT NOT NULL CHECK (encoding IN ('zstd-frame')),
    protection   TEXT NOT NULL CHECK (protection IN ('none', 'aead')),
    decoded_len  INTEGER NOT NULL CHECK (decoded_len >= 0),
    content_hash BLOB NOT NULL CHECK (length(content_hash) = 32)
) STRICT, WITHOUT ROWID;

CREATE TABLE chunk_dependencies (
    object_id       BLOB NOT NULL REFERENCES chunks(object_id),
    dependency_kind TEXT NOT NULL CHECK (dependency_kind IN ('dictionary', 'key-envelope')),
    dependency_id   BLOB NOT NULL CHECK (length(dependency_id) = 32),
    PRIMARY KEY (object_id, dependency_kind, dependency_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE file_versions (
    file_version_id BLOB PRIMARY KEY CHECK (length(file_version_id) = 32),
    kind            TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
    logical_len     INTEGER NOT NULL CHECK (logical_len >= 0),
    content_hash    BLOB,
    CHECK ((kind = 'file' AND length(content_hash) = 32)
        OR (kind = 'directory' AND content_hash IS NULL AND logical_len = 0))
) STRICT, WITHOUT ROWID;

CREATE TABLE file_extents (
    file_version_id BLOB NOT NULL REFERENCES file_versions(file_version_id),
    ordinal         INTEGER NOT NULL CHECK (ordinal >= 0),
    logical_offset  INTEGER NOT NULL CHECK (logical_offset >= 0),
    length          INTEGER NOT NULL CHECK (length > 0),
    chunk_id        BLOB REFERENCES chunks(object_id),
    chunk_offset    INTEGER NOT NULL CHECK (chunk_offset >= 0),
    CHECK (chunk_id IS NOT NULL OR chunk_offset = 0),
    PRIMARY KEY (file_version_id, ordinal)
) STRICT, WITHOUT ROWID;

CREATE TABLE namespace_ops (
    commit_seq      INTEGER NOT NULL REFERENCES commits(seq),
    op_seq          INTEGER NOT NULL CHECK (op_seq >= 0),
    op              TEXT NOT NULL CHECK (op IN ('put', 'delete')),
    path            BLOB NOT NULL CHECK (length(path) > 0),
    file_version_id BLOB REFERENCES file_versions(file_version_id),
    CHECK ((op = 'put') = (file_version_id IS NOT NULL)),
    PRIMARY KEY (commit_seq, op_seq)
) STRICT, WITHOUT ROWID;
"""

_SCHEMA_SQL = "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name"


def _reference_schema():
    con = sqlite3.connect(":memory:")
    try:
        with con:
            for stmt in DDL.split(";"):
                if stmt.strip():
                    con.execute(stmt)
        return [tuple(r) for r in con.execute(_SCHEMA_SQL)]
    finally:
        con.close()


@dataclass
class ImageState:
    archive_id: bytes | None
    head_seq: int | None
    objects: dict = field(default_factory=dict)        # id -> (stored_len, stored_hash)
    locations: dict = field(default_factory=dict)      # id -> offset
    chunks: dict = field(default_factory=dict)         # id -> (protection, decoded_len, content_hash)
    versions: dict = field(default_factory=dict)       # id -> (kind, length, content_hash)
    extents: dict = field(default_factory=dict)        # id -> [(ordinal, logical_off, len, chunk, chunk_off)]
    namespace: dict = field(default_factory=dict)      # path -> file version id (final state)
    deps: list = field(default_factory=list)


def check_image(image: bytes) -> ImageState:
    """Run R4 section 3 steps 1, 3, 4 on the SQLite bytes (the part after the
    binary envelope).  Raises MochiError with the R4 codes."""
    # 1. before SQLite parses anything
    if len(image) > IMAGE_LIMIT:
        raise MochiError("LIMIT_EXCEEDED", "image above the payload budget")
    if len(image) < 100 or len(image) % PAGE_SIZE:
        raise MochiError("CATALOG_INVALID", "not a whole number of 4096-byte pages")
    if image[:16] != b"SQLite format 3\x00":
        raise MochiError("CATALOG_INVALID", "wrong SQLite signature")
    if int.from_bytes(image[16:18], "big") != PAGE_SIZE:
        raise MochiError("CATALOG_INVALID", "page size is not 4096")
    if image[18] != 1 or image[19] != 1:
        raise MochiError("CATALOG_INVALID", "file format versions are not 1/1 (rollback journal)")
    if int.from_bytes(image[68:72], "big") != APPLICATION_ID:
        raise MochiError("CATALOG_INVALID", "application_id is not MOCH")
    if int.from_bytes(image[60:64], "big", signed=True) != USER_VERSION:
        raise MochiError("UNSUPPORTED_FEATURE", "catalog user_version is not 1")
    fd, path = tempfile.mkstemp(prefix="mochi-verify-", suffix=".sqlite")
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(image)
        con = sqlite3.connect("file:%s?mode=ro&immutable=1" % path, uri=True)
        try:
            con.execute("PRAGMA query_only = ON")
            return _check_open(con)
        except sqlite3.DatabaseError as e:
            raise MochiError("CATALOG_INVALID", "SQLite: %s" % e) from None
        finally:
            con.close()
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass


def _check_open(con: sqlite3.Connection) -> ImageState:
    # 3. exact schema
    rows = [tuple(r) for r in con.execute(_SCHEMA_SQL)]
    if rows != _reference_schema():
        raise MochiError("CATALOG_INVALID", "stored schema differs from R4's DDL")
    # 4. integrity
    res = [r[0] for r in con.execute("PRAGMA integrity_check")]
    if res != ["ok"]:
        raise MochiError("CATALOG_INVALID", "PRAGMA integrity_check: %s" % res[:3])
    if list(con.execute("PRAGMA foreign_key_check")):
        raise MochiError("CATALOG_INVALID", "PRAGMA foreign_key_check reported violations")
    st = ImageState(None, None)
    meta = dict(con.execute("SELECT key, value FROM archive_meta"))
    aid = meta.get("archive_id")
    if aid is not None and (not isinstance(aid, bytes) or len(aid) != 32):
        raise MochiError("CATALOG_INVALID", "archive_meta.archive_id is not 32 bytes")
    st.archive_id = aid        # the caller requires it to equal the referencing commit's
    for oid, ln, h in con.execute("SELECT object_id, stored_len, stored_hash FROM objects"):
        st.objects[oid] = (ln, h)
    for oid, off in con.execute("SELECT object_id, stored_offset FROM object_locations"):
        st.locations[oid] = off
    for oid, enc, prot, dl, ch in con.execute("SELECT object_id, encoding, protection, decoded_len, content_hash FROM chunks"):
        st.chunks[oid] = (prot, dl, ch)
    st.deps = list(con.execute("SELECT object_id, dependency_kind, dependency_id FROM chunk_dependencies"))
    for fid, kind, ln, ch in con.execute("SELECT file_version_id, kind, logical_len, content_hash FROM file_versions"):
        st.versions[fid] = (kind, ln, ch)
    for fid, ordn, lo, ln, cid, co in con.execute(
            "SELECT file_version_id, ordinal, logical_offset, length, chunk_id, chunk_offset FROM file_extents "
            "ORDER BY file_version_id, ordinal"):
        st.extents.setdefault(fid, []).append((ordn, lo, ln, cid, co))
    _check_extents(st)
    commits = list(con.execute("SELECT seq, parent_seq FROM commits ORDER BY seq"))
    if not commits:
        raise MochiError("CATALOG_INVALID", "no commits in the image")
    for i, (seq, parent) in enumerate(commits):
        if i == 0:
            if parent is not None:
                raise MochiError("CATALOG_INVALID", "first commit has a parent")
        elif parent != commits[i - 1][0]:
            raise MochiError("CATALOG_INVALID", "commit chain is not linear")
    st.head_seq = commits[-1][0]
    ops = list(con.execute(
        "SELECT commit_seq, op_seq, op, path, file_version_id FROM namespace_ops ORDER BY commit_seq, op_seq"))
    _replay_namespace(st, [c[0] for c in commits], ops)
    return st


def _check_extents(st: ImageState) -> None:
    for fid, (kind, length, _) in st.versions.items():
        exts = st.extents.get(fid, [])
        pos = 0
        for i, (ordn, lo, ln, cid, co) in enumerate(exts):
            if ordn != i or ln <= 0 or lo != pos:
                raise MochiError("EXTENT_INVALID", "ordinal, length or gap/overlap")
            if cid is None:
                if co != 0:
                    raise MochiError("EXTENT_INVALID", "hole with a chunk offset")
            else:
                ch = st.chunks.get(cid)
                if ch is None or co + ln > ch[1]:
                    raise MochiError("EXTENT_INVALID", "extent reads outside its chunk")
            pos += ln
        if pos != length:
            raise MochiError("EXTENT_INVALID", "extents do not cover the logical length")
        if kind == "directory" and exts:
            raise MochiError("EXTENT_INVALID", "a directory has extents")


def _replay_namespace(st: ImageState, seqs: list, ops: list) -> None:
    ns: dict[bytes, bytes] = {}
    by_commit: dict[int, list] = {s: [] for s in seqs}
    for seq, opseq, op, path, fid in ops:
        by_commit[seq].append((op, bytes(path), fid))
    for seq in seqs:
        for op, path, fid in by_commit[seq]:
            check_path(path)
            if op == "put":
                if fid not in st.versions:
                    raise MochiError("NAMESPACE_INVALID", "put of an unknown file version")
                ns[path] = fid
            else:
                if path not in ns:
                    raise MochiError("NAMESPACE_INVALID", "delete of an absent path")
                del ns[path]
        check_namespace(ns, lambda f: st.versions[f][0] == "directory")
    st.namespace = ns


def check_namespace(ns: dict, is_dir) -> None:
    """Every entry's parent exists as a directory at the end of the commit."""
    for path in ns:
        parts = path.split(b"/")
        for i in range(1, len(parts)):
            par = b"/".join(parts[:i])
            f = ns.get(par)
            if f is None or not is_dir(f):
                raise MochiError("NAMESPACE_INVALID", "an entry's parent is not a directory")
