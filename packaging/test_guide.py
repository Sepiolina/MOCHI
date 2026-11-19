"""Tests for guide.py (run by CI: python3 -I -m unittest discover -s packaging -p 'test_*.py')."""
import pathlib
import re
import tempfile
import unittest

import guide


class Convert(unittest.TestCase):
    def test_source_text_is_escaped_never_passed_through(self):
        _, body = guide.convert("# T\n\n<script>alert(1)</script> & `<b>x</b>` [a](https://example.com)\n")
        self.assertNotIn("<script", body)
        self.assertNotIn("<b>", body)
        self.assertIn("&lt;script&gt;", body)
        # An external link is shown as text and has no href: the guide makes no request.
        self.assertNotIn("href", body)
        self.assertTrue(body.endswith("&amp; <code>&lt;b&gt;x&lt;/b&gt;</code> a</p>"), body)

    def test_links_between_bundled_pages_only(self):
        _, body = guide.convert("[v](verification.md#x) [r](README.md) [c](../cli.md) [n](nope.md)\n")
        self.assertIn('href="verification.html#x"', body)
        self.assertIn('href="index.html"', body)
        self.assertEqual(len(re.findall("href=", body)), 2)

    def test_structures(self):
        title, body = guide.convert("# Ti`t`le\n\n* a\n  more\n* b\n\n|H|I|\n|---|---|\n|`x|y`|2|\n\n```\n<raw>\n```\n\n> q **b**\n")
        self.assertEqual(title, "Title")
        self.assertIn("<li>a more</li>", body)
        self.assertIn("<td><code>x|y</code></td>", body)
        self.assertIn("<pre><code>&lt;raw&gt;</code></pre>", body)
        self.assertIn("<blockquote><p>q <strong>b</strong></p></blockquote>", body)


class Build(unittest.TestCase):
    def test_every_page_is_built_offline_with_the_scope_banner(self):
        with tempfile.TemporaryDirectory() as d:
            index = guide.build(d)
            self.assertEqual(index.name, "index.html")
            for name in guide.PAGES:
                target = "index.html" if name == "README.md" else name[:-3] + ".html"
                text = (pathlib.Path(d) / target).read_text(encoding="utf-8")
                self.assertIn("written for the mochi command line", text, target)
                self.assertIn("mochi verify", text.split("</aside>")[0], target)
                self.assertNotRegex(text, r"<script|src=|https?://|<link|@import|url\(", target)
                # Every link that is left points at a page that was built.
                for href in re.findall(r'href="([^"#]+)', text):
                    self.assertTrue((pathlib.Path(d) / href).is_file(), (target, href))

    def test_a_page_missing_from_the_list_fails_the_build(self):
        with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as src:
            for p in guide.SRC.glob("*.md"):
                (pathlib.Path(src) / p.name).write_text(p.read_text(encoding="utf-8"), encoding="utf-8")
            (pathlib.Path(src) / "extra.md").write_text("# x\n", encoding="utf-8")
            with self.assertRaises(SystemExit):
                guide.build(d, src)


if __name__ == "__main__":
    unittest.main()
