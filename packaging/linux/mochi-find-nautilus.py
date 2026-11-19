"""GNOME Files (Nautilus): "Find in MOCHI archives" on folders.

Installed to /usr/share/nautilus-python/extensions/mochi_find.py. It is loaded only
when the python3-nautilus package (nautilus-python) is installed; without it this file
is never read and nothing else in MOCHI depends on it. It only starts
`mochi-desktop --find -- FOLDER...` (a list of arguments, never a shell string); it reads
no archive and writes nothing. Any error is swallowed: a broken menu entry must never
break the file manager.

Works with Nautilus 3 (get_file_items(window, files)) and Nautilus 4
(get_file_items(files)): the last argument is always the list.
"""

import os
import subprocess

try:
    from gi.repository import GObject, Nautilus
except Exception:  # not running inside nautilus-python (tests import it with a stand-in)
    GObject = None
    Nautilus = None

PROGRAM = "mochi-desktop"

LABELS = {
    "th": "ค้นหาในไฟล์บีบอัด MOCHI",
    "zh": "在 MOCHI 压缩文件中查找",
}
DEFAULT_LABEL = "Find in MOCHI archives"


def label(environ=None):
    """The menu text in the desktop language (Thai, Chinese, else English)."""
    env = os.environ if environ is None else environ
    for name in ("LC_ALL", "LC_MESSAGES", "LANGUAGE", "LANG"):
        value = env.get(name, "")
        if value:
            return LABELS.get(value.lower()[:2], DEFAULT_LABEL)
    return DEFAULT_LABEL


def folders_of(items):
    """Local folder paths among Nautilus file objects, in order."""
    out = []
    for item in items or []:
        try:
            if item.is_directory():
                location = item.get_location()
                path = location.get_path() if location is not None else None
                if path:
                    out.append(path)
        except Exception:
            continue
    return out


def start(paths):
    try:
        subprocess.Popen([PROGRAM, "--find", "--", *paths])
    except Exception:
        pass


if GObject is not None and Nautilus is not None:

    class MochiFindProvider(GObject.GObject, Nautilus.MenuProvider):
        def _item(self, name, paths):
            item = Nautilus.MenuItem(name=name, label=label())
            item.connect("activate", lambda _menu: start(paths))
            return [item]

        def get_file_items(self, *args):
            try:
                paths = folders_of(args[-1])
                return self._item("MochiFind::Folders", paths) if paths else []
            except Exception:
                return []

        def get_background_items(self, *args):
            try:
                paths = folders_of([args[-1]])
                return self._item("MochiFind::Background", paths) if paths else []
            except Exception:
                return []
