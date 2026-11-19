"""The GNOME Files extension (packaging/linux/mochi-find-nautilus.py) without Nautilus:
imported with no `gi` (as when python3-nautilus is absent), and its pure parts."""
import importlib.util
import pathlib
import subprocess
import sys
import types
import unittest
from unittest import mock

PATH = pathlib.Path(__file__).parent / "linux" / "mochi-find-nautilus.py"


def load(name="mochi_find_ext"):
    spec = importlib.util.spec_from_file_location(name, PATH)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


class Item:
    def __init__(self, path, directory=True):
        self.path, self.directory = path, directory

    def is_directory(self):
        return self.directory

    def get_location(self):
        return types.SimpleNamespace(get_path=lambda: self.path)


class WithoutNautilus(unittest.TestCase):
    def test_imports_cleanly_and_defines_no_provider_when_gi_is_missing(self):
        with mock.patch.dict(sys.modules, {"gi": None, "gi.repository": None}):
            mod = load()
        self.assertFalse(hasattr(mod, "MochiFindProvider"))

    def test_only_folders_are_passed_in_order(self):
        mod = load()
        items = [Item("/a", True), Item("/f.txt", False), Item("/b c", True), Item(None, True)]
        self.assertEqual(mod.folders_of(items), ["/a", "/b c"])
        self.assertEqual(mod.folders_of(None), [])

    def test_a_broken_item_is_skipped_not_raised(self):
        class Broken:
            def is_directory(self):
                raise RuntimeError("boom")

        self.assertEqual(load().folders_of([Broken(), Item("/ok")]), ["/ok"])

    def test_the_label_follows_the_desktop_language(self):
        mod = load()
        self.assertEqual(mod.label({"LANG": "en_US.UTF-8"}), "Find in MOCHI archives")
        self.assertEqual(mod.label({}), "Find in MOCHI archives")
        self.assertIn("MOCHI", mod.label({"LANG": "th_TH.UTF-8"}))
        self.assertNotEqual(mod.label({"LANG": "th_TH.UTF-8"}), mod.DEFAULT_LABEL)
        self.assertNotEqual(mod.label({"LC_ALL": "zh_CN.UTF-8", "LANG": "en_US"}), mod.DEFAULT_LABEL)

    def test_start_passes_a_list_of_arguments_and_survives_a_missing_program(self):
        mod = load()
        with mock.patch.object(subprocess, "Popen") as popen:
            mod.start(["/x/-rf", "/y z"])
        popen.assert_called_once_with(["mochi-desktop", "--find", "--", "/x/-rf", "/y z"])
        with mock.patch.object(subprocess, "Popen", side_effect=OSError("no such file")):
            mod.start(["/x"])


class WithNautilus(unittest.TestCase):
    def provider(self):
        gi = types.ModuleType("gi")
        repo = types.ModuleType("gi.repository")

        class GObjectBase:
            pass

        class MenuProvider:
            pass

        class MenuItem:
            def __init__(self, name, label):
                self.name, self.label, self.handlers = name, label, []

            def connect(self, _signal, fn):
                self.handlers.append(fn)

        repo.GObject = types.SimpleNamespace(GObject=GObjectBase)
        repo.Nautilus = types.SimpleNamespace(MenuProvider=MenuProvider, MenuItem=MenuItem)
        gi.repository = repo
        with mock.patch.dict(sys.modules, {"gi": gi, "gi.repository": repo}):
            return load("mochi_find_ext_gi").MochiFindProvider()

    def test_entries_for_folders_in_both_nautilus_apis_and_none_for_files(self):
        p = self.provider()
        items = [Item("/a"), Item("/f", False)]
        for args in ((items,), ("window", items)):
            got = p.get_file_items(*args)
            self.assertEqual([i.name for i in got], ["MochiFind::Folders"])
        self.assertEqual(p.get_file_items([Item("/f", False)]), [])
        self.assertEqual(p.get_file_items(None), [])
        bg = p.get_background_items(Item("/open"))
        self.assertEqual([i.name for i in bg], ["MochiFind::Background"])
        self.assertEqual(p.get_background_items("window", Item("/open")) [0].name, "MochiFind::Background")

    def test_activating_runs_the_app_with_the_folders(self):
        p = self.provider()
        item = p.get_file_items([Item("/a"), Item("/b")])[0]
        with mock.patch.object(subprocess, "Popen") as popen:
            item.handlers[0](item)
        popen.assert_called_once_with(["mochi-desktop", "--find", "--", "/a", "/b"])


if __name__ == "__main__":
    unittest.main()
