#!/usr/bin/env python3
"""Build the bundled user guide: docs/user/*.md -> static, offline HTML (plan D7, audit F8).

Usage: guide.py OUTDIR      (writes OUTDIR/index.html and one .html per page)

Standard library only, so the build needs no new dependency. The pages are written for the
`mochi` command line; every page gets a banner saying so, with pointers to the desktop
equivalents. The output is static: no script, no remote content, no external link (the only
links are between the bundled pages), and every character from the source is escaped.
"""
import html
import pathlib
import re
import sys

SRC = pathlib.Path(__file__).resolve().parent.parent / "docs" / "user"

# Pages in reading order. README.md becomes index.html. A page that exists in docs/user but
# is not listed here makes the build fail, so a new page cannot be left out by accident.
PAGES = [
    "README.md",
    "not-a-backup.md",
    "getting-started.md",
    "encrypted.md",
    "verification.md",
    "tar-compatible.md",
    "recovery-record.md",
    "experimental.md",
]

BANNER_TITLE = "This guide is written for the mochi command line."
BANNER_BODY = (
    "The desktop app does the same work through its own screens, and not every command here "
    "has one. Commands shown below are typed in a terminal, not in the app. "
    "Where the desktop app has an equivalent:"
)
# (command line, desktop app). Names are the app's own (ui/src/i18n/en/start.ts, write.ts, extract.ts, app.ts).
EQUIVALENTS = [
    ("mochi create", "“Create archive” on the start screen"),
    ("mochi append", "“Add to archive…” and “Remove from this version…” once an archive is open"),
    ("mochi get", "“Extract archive” on the start screen, or “Extract all” and “Extract selected” once an archive is open"),
    ("mochi verify, mochi fsck, mochi health", "“Test, health, and repair”, under “Advanced” (“Test archive” is verify)"),
    ("mochi repair plan, mochi repair apply", "“Test, health, and repair”, under “Advanced” (a reviewed plan, written to a new file)"),
    ("passphrases and key envelopes", "the “Passphrases” section, under “Advanced”"),
    ("mochi search", "the “Search names” section, under “Advanced”"),
    ("mochi find", "“Find in archives…” under “More” (Ctrl+Shift+F), or “Find in MOCHI archives” on folders in the file manager"),
    ("ZIP, 7z, RAR and tar archives", "“Other archive formats”, under “Advanced” (read-only)"),
]
NO_EQUIVALENT = (
    "The desktop app has no head history, evidence log, or restore-test, so its freshness is "
    "always UNKNOWN. A check there that passes is still not a backup."
)

CSS = """
:root { color-scheme: light dark; --bg: #fff; --fg: #1a1a1a; --muted: #555; --rule: #c8c8c8; --accent: #0b5cad; --note: #eef3fa; }
@media (prefers-color-scheme: dark) { :root { --bg: #16181c; --fg: #e8e8e8; --muted: #a8a8a8; --rule: #444; --accent: #7db7f0; --note: #1f2630; } }
body { background: var(--bg); color: var(--fg); font: 16px/1.55 system-ui, sans-serif; margin: 0 auto; max-width: 52rem; padding: 1rem 1rem 3rem; }
a { color: var(--accent); }
pre, code { font-family: ui-monospace, Consolas, monospace; font-size: .92em; }
pre { overflow-x: auto; padding: .75rem; border: 1px solid var(--rule); }
table { border-collapse: collapse; display: block; overflow-x: auto; }
th, td { border: 1px solid var(--rule); padding: .3rem .6rem; text-align: left; vertical-align: top; }
blockquote { border-left: 4px solid var(--rule); margin-left: 0; padding-left: 1rem; color: var(--muted); }
.banner { background: var(--note); border: 1px solid var(--rule); padding: .75rem 1rem; margin-bottom: 1.5rem; }
.banner p { margin: .3rem 0; }
.banner ul { margin: .3rem 0; padding-left: 1.2rem; }
nav.pages { margin-bottom: 1rem; color: var(--muted); }
"""


def inline(text):
    """Markdown inline spans -> HTML. All source text is escaped; nothing passes through raw."""
    out = []
    pos = 0
    for m in re.finditer(r"(`+)(.+?)\1", text):
        out.append(spans(text[pos : m.start()]))
        out.append("<code>" + html.escape(m.group(2).strip(), quote=False) + "</code>")
        pos = m.end()
    out.append(spans(text[pos:]))
    return "".join(out)


def link_target(href):
    """A bundled page (`name.md`, optionally `#anchor`) maps to its .html; anything else has no link."""
    m = re.fullmatch(r"([A-Za-z0-9_-]+)\.md(#[A-Za-z0-9_-]+)?", href)
    if not m or (m.group(1) + ".md") not in PAGES:
        return None
    name = "index" if m.group(1) == "README" else m.group(1)
    return name + ".html" + (m.group(2) or "")


def spans(text):
    """Escape, then links, bold, italic. Code spans were already split out by `inline`."""
    t = html.escape(text, quote=False)

    def a(m):
        target = link_target(html.unescape(m.group(2)))
        # A link to something that is not a bundled page (a repository path, a URL) is shown as
        # text: the guide is offline and makes no request.
        return f'<a href="{target}">{m.group(1)}</a>' if target else m.group(1)

    t = re.sub(r"\[([^\]]+)\]\(([^)\s]+)\)", a, t)
    t = re.sub(r"\*\*(.+?)\*\*", r"<strong>\1</strong>", t)
    t = re.sub(r"(?<![\w*])\*([^*\s][^*]*?)\*(?![\w*])", r"<em>\1</em>", t)
    t = re.sub(r"(?<![\w_])_([^_\s][^_]*?)_(?![\w_])", r"<em>\1</em>", t)
    return t


def slug(text):
    s = re.sub(r"[^a-z0-9 -]", "", re.sub(r"`", "", text.lower())).strip().replace(" ", "-")
    return s


def table_cells(line):
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|"):
        line = line[:-1]
    # Split on unescaped pipes outside code spans.
    cells, cur, in_code = [], "", False
    i = 0
    while i < len(line):
        c = line[i]
        if c == "`":
            in_code = not in_code
        if c == "\\" and i + 1 < len(line) and line[i + 1] == "|":
            cur += "|"
            i += 2
            continue
        if c == "|" and not in_code:
            cells.append(cur.strip())
            cur = ""
        else:
            cur += c
        i += 1
    cells.append(cur.strip())
    return cells


def convert(md):
    """The Markdown subset docs/user uses -> (title, body HTML)."""
    lines = md.splitlines()
    out, title, i = [], None, 0
    seen = {}

    def flush_para(buf):
        if buf:
            out.append("<p>" + inline(" ".join(s.strip() for s in buf)) + "</p>")
            buf.clear()

    para = []
    while i < len(lines):
        line = lines[i]
        if line.startswith("```"):
            flush_para(para)
            i += 1
            code = []
            while i < len(lines) and not lines[i].startswith("```"):
                code.append(lines[i])
                i += 1
            i += 1
            out.append("<pre><code>" + html.escape("\n".join(code), quote=False) + "</code></pre>")
            continue
        h = re.match(r"(#{1,6}) +(.*)", line)
        if h:
            flush_para(para)
            level, text = len(h.group(1)), h.group(2).strip()
            if title is None and level == 1:
                title = re.sub(r"`", "", text)
            base = slug(text) or "section"
            n = seen.get(base, 0)
            seen[base] = n + 1
            ident = base if n == 0 else f"{base}-{n}"
            out.append(f'<h{level} id="{ident}">{inline(text)}</h{level}>')
            i += 1
            continue
        if line.lstrip().startswith("|") and i + 1 < len(lines) and re.match(r"\s*\|[\s:|-]+\|\s*$", lines[i + 1]):
            flush_para(para)
            head = table_cells(line)
            i += 2
            rows = []
            while i < len(lines) and lines[i].lstrip().startswith("|"):
                rows.append(table_cells(lines[i]))
                i += 1
            out.append("<table><thead><tr>" + "".join(f"<th>{inline(c)}</th>" for c in head) + "</tr></thead><tbody>")
            for r in rows:
                out.append("<tr>" + "".join(f"<td>{inline(c)}</td>" for c in r) + "</tr>")
            out.append("</tbody></table>")
            continue
        if line.startswith(">"):
            flush_para(para)
            quote = []
            while i < len(lines) and lines[i].startswith(">"):
                quote.append(lines[i][1:].strip())
                i += 1
            out.append("<blockquote><p>" + inline(" ".join(quote)) + "</p></blockquote>")
            continue
        if re.match(r"\s*([*-]|\d+\.) +", line):
            flush_para(para)
            ordered = bool(re.match(r"\s*\d+\.", line))
            items = []
            while i < len(lines):
                m = re.match(r"\s*([*-]|\d+\.) +(.*)", lines[i])
                if m and not lines[i].startswith("  "):
                    items.append([m.group(2)])
                    i += 1
                elif lines[i].startswith("  ") and lines[i].strip() and items:
                    items[-1].append(lines[i].strip())
                    i += 1
                else:
                    break
            tag = "ol" if ordered else "ul"
            out.append(f"<{tag}>" + "".join("<li>" + inline(" ".join(it)) + "</li>" for it in items) + f"</{tag}>")
            continue
        if not line.strip():
            flush_para(para)
            i += 1
            continue
        para.append(line)
        i += 1
    flush_para(para)
    return title or "MOCHI user guide", "\n".join(out)


def banner():
    items = "".join(f"<li><code>{html.escape(a, quote=False)}</code>: {html.escape(b, quote=False)}</li>" for a, b in EQUIVALENTS)
    return (
        '<aside class="banner" role="note" aria-label="Scope of this guide">'
        f"<p><strong>{html.escape(BANNER_TITLE, quote=False)}</strong></p>"
        f"<p>{html.escape(BANNER_BODY, quote=False)}</p><ul>{items}</ul>"
        f"<p>{html.escape(NO_EQUIVALENT, quote=False)}</p></aside>"
    )


def page(title, body):
    nav = " · ".join('<a href="%s">%s</a>' % ("index.html" if p == "README.md" else p[:-3] + ".html", html.escape(p[:-3] if p != "README.md" else "contents", quote=False)) for p in PAGES)
    return (
        "<!doctype html>\n"
        '<html lang="en"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width, initial-scale=1">'
        # No script, no remote content of any kind.
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">"
        f"<title>{html.escape(title, quote=False)} – MOCHI user guide (command line)</title>"
        f"<style>{CSS}</style></head><body>"
        f"{banner()}<nav class=\"pages\" aria-label=\"Guide pages\">{nav}</nav><main>{body}</main></body></html>\n"
    )


def build(outdir, src=SRC):
    src, outdir = pathlib.Path(src), pathlib.Path(outdir)
    present = sorted(p.name for p in src.glob("*.md"))
    if present != sorted(PAGES):
        raise SystemExit(f"guide.py: PAGES {sorted(PAGES)} does not match docs/user {present}")
    outdir.mkdir(parents=True, exist_ok=True)
    for name in PAGES:
        title, body = convert((src / name).read_text(encoding="utf-8"))
        target = "index.html" if name == "README.md" else name[:-3] + ".html"
        (outdir / target).write_text(page(title, body), encoding="utf-8", newline="\n")
    return outdir / "index.html"


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    print(build(sys.argv[1]))
