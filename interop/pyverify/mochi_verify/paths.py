"""Stored path validity (R4 section 3.1)."""
from __future__ import annotations

from .errors import MochiError

MAX_COMPONENT = 1024
MAX_PATH = 16384
MAX_COMPONENTS = 1024


def check_path(path: bytes) -> None:
    if not isinstance(path, bytes) or not path:
        raise MochiError("PATH_INVALID", "empty path")
    if len(path) > MAX_PATH:
        raise MochiError("PATH_INVALID", "path longer than 16384 bytes")
    comps = path.split(b"/")
    if len(comps) > MAX_COMPONENTS:
        raise MochiError("PATH_INVALID", "more than 1024 components")
    for c in comps:
        if not c or c in (b".", b"..") or b"\x00" in c:
            raise MochiError("PATH_INVALID", "invalid path component")
        if len(c) > MAX_COMPONENT:
            raise MochiError("PATH_INVALID", "component longer than 1024 bytes")


def parent_of(path: bytes):
    i = path.rfind(b"/")
    return None if i < 0 else path[:i]
