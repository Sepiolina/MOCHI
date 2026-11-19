"""Tests for latest_json.py (run by CI: python3 -I -m unittest discover -s packaging -p 'test_*.py')."""
import base64
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
TOOL = HERE / "latest_json.py"


def sig(version: str) -> str:
    """A .sig as the Tauri CLI writes it: base64 of a minisig whose trusted comment carries the version."""
    minisig = (
        "untrusted comment: signature from the test key\n"
        "RUTest\n"
        f"trusted comment: timestamp:1\tfile:x\tversion:{version}\n"
        "globalsig\n"
    )
    return base64.b64encode(minisig.encode()).decode()


class LatestJson(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = pathlib.Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)

    def installer(self, name, version="0.2.0"):
        f = self.dir / name
        f.write_bytes(b"installer")
        (self.dir / (name + ".sig")).write_text(sig(version))
        return f

    def run_tool(self, *args):
        return subprocess.run([sys.executable, "-I", str(TOOL), *args], capture_output=True, text=True)

    def make(self, nsis, deb, version="0.2.0", tag="v0.2.0"):
        (self.dir / "notes.txt").write_text("- a fix\n<b>text</b>\n")
        return self.run_tool(
            "make", "--version", version, "--tag", tag, "--repo", "immerh8/MOCHI",
            "--notes", str(self.dir / "notes.txt"), "--out", str(self.dir / "latest.json"),
            "--nsis", str(nsis), "--deb", str(deb), "--pub-date", "2026-10-14T00:00:00Z",
        )

    def test_manifest_has_every_key_the_plugin_looks_up(self):
        r = self.make(self.installer("MOCHI_0.2.0_x64-setup.exe"), self.installer("MOCHI_0.2.0_amd64.deb"))
        self.assertEqual(r.returncode, 0, r.stderr)
        m = json.loads((self.dir / "latest.json").read_text())
        self.assertEqual(m["version"], "0.2.0")
        self.assertEqual(
            sorted(m["platforms"]),
            ["linux-x86_64", "linux-x86_64-deb", "windows-x86_64", "windows-x86_64-nsis"],
        )
        nsis = m["platforms"]["windows-x86_64-nsis"]
        self.assertEqual(
            nsis["url"],
            "https://github.com/immerh8/MOCHI/releases/download/v0.2.0/MOCHI_0.2.0_x64-setup.exe",
        )
        self.assertEqual(nsis["signature"], sig("0.2.0"))
        self.assertIn("<b>text</b>", m["notes"])  # kept as literal text; the app shows it as text

    def test_a_signature_for_another_version_is_refused(self):
        r = self.make(self.installer("a-setup.exe", "0.1.0"), self.installer("a.deb"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("signed for version", r.stderr)
        self.assertFalse((self.dir / "latest.json").exists())

    def test_a_missing_sig_is_refused(self):
        a = self.installer("a-setup.exe")
        (self.dir / "a-setup.exe.sig").unlink()
        r = self.make(a, self.installer("a.deb"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn(".sig", r.stderr)

    def test_a_tag_that_is_not_the_version_is_refused(self):
        r = self.make(self.installer("a-setup.exe"), self.installer("a.deb"), tag="v0.3.0")
        self.assertNotEqual(r.returncode, 0)

    def test_check_names_the_bad_file(self):
        good = self.installer("g.deb")
        bad = self.installer("b.deb", "0.1.0")
        self.assertEqual(self.run_tool("check", "--version", "0.2.0", str(good)).returncode, 0)
        r = self.run_tool("check", "--version", "0.2.0", str(good), str(bad))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("b.deb.sig", r.stderr)


if __name__ == "__main__":
    unittest.main()
