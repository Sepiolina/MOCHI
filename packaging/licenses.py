#!/usr/bin/env python3
"""Generate THIRD-PARTY-LICENSES.txt for the desktop installers (plan D7, O23).

Rust: `cargo about generate --format json` over the desktop crate and the
foreign-format helper (T16); policy and targets in
packaging/about.toml; it fails on a license not accepted.
JS:   `pnpm licenses list --prod --json` in the UI (the packages that end up in
the bundle), checked against the same accepted list, with each package's
license file read from node_modules.

Output: one text file with MOCHI's own licenses, THIRD-PARTY-NOTICES.md, then
every dependency grouped by license text. Exit 1 on a disallowed or
unreadable license. Standard library only.
"""
import json
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ABOUT = ROOT / "packaging" / "about.toml"
DESKTOP_MANIFEST = ROOT / "apps" / "mochi-desktop" / "src-tauri" / "Cargo.toml"
# The installers also ship mochi-foreign-helper (T16), a separate binary whose graph
# (zip, tar, flate2, sevenz-rust2, unrar_sys, ...) is not the desktop crate's: the app
# names mochi-foreign with default-features = false. Built with the default `engine` feature.
HELPER_MANIFEST = ROOT / "crates" / "mochi-foreign" / "Cargo.toml"
# The Windows installer also ships mochi-shellext (the Explorer menu DLL; owner decision
# 2026-10-20): its `windows` crates are listed even where the app's graph has other versions.
SHELLEXT_MANIFEST = ROOT / "crates" / "mochi-shellext" / "Cargo.toml"
# Both installers also ship the `mochi` command-line program (crates/mochi-cli): its own
# graph (clap, ...) is listed too.
CLI_MANIFEST = ROOT / "crates" / "mochi-cli" / "Cargo.toml"
UI = ROOT / "apps" / "mochi-desktop" / "ui"
LICENSE_FILES = ("LICENSE", "LICENCE", "LICENSE.md", "LICENSE.txt", "LICENSE-MIT", "COPYING")


def accepted_list():
    return set(tomllib.loads(ABOUT.read_text(encoding="utf-8"))["accepted"])


def expr_allowed(expr, accepted):
    """SPDX-lite: `A OR B` needs one allowed alternative, `A AND B` all."""
    expr = expr.replace("(", " ").replace(")", " ")
    for alt in expr.split(" OR "):
        if all(t.strip() in accepted for t in alt.split(" AND ")):
            return True
    return False


def run(cmd, cwd=None):
    # Windows: corepack's pnpm is a pnpm.cmd shim, which CreateProcess only finds by full
    # path (no PATHEXT lookup for a bare name). No shell=True.
    cmd = [shutil.which(cmd[0]) or cmd[0], *cmd[1:]]
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        sys.exit(f"licenses.py: {' '.join(cmd)} failed ({r.returncode})")
    return r.stdout


def rust_entries():
    entries, seen = [], set()  # (name, version, spdx, text); a crate in both graphs once
    for manifest in (DESKTOP_MANIFEST, HELPER_MANIFEST, SHELLEXT_MANIFEST, CLI_MANIFEST):
        out = run(["cargo", "about", "generate", "--config", str(ABOUT),
                   "--manifest-path", str(manifest), "--format", "json", "--fail"])
        for lic in json.loads(out)["licenses"]:
            for used in lic["used_by"]:
                c = used["crate"]
                key = (c["name"], c["version"])
                if key not in seen:
                    seen.add(key)
                    entries.append((c["name"], c["version"], lic["id"], lic["text"]))
    return entries


def require_unrar_notice(rust):
    """unrar_sys is labelled MIT/Apache but compiles RARLAB's UnRAR (freeware, with a
    reproduce-this-paragraph condition). The notice comes from THIRD-PARTY-NOTICES.md,
    which render() always includes; fail if the helper's graph has the crate and the
    notice text is gone, so the condition cannot be dropped silently."""
    if any(name == "unrar_sys" for name, *_ in rust):
        notices = (ROOT / "THIRD-PARTY-NOTICES.md").read_text(encoding="utf-8")
        if "UnRAR source code may be used in any software" not in notices:
            sys.exit("licenses.py: unrar_sys is in the helper's graph but THIRD-PARTY-NOTICES.md "
                     "lacks the RARLAB UnRAR license paragraph")
    else:
        sys.exit("licenses.py: unrar_sys is missing from the helper's dependency graph; "
                 "the helper was not built with the `engine` feature?")


def js_entries(accepted):
    out = run(["pnpm", "licenses", "list", "--prod", "--json"], cwd=UI)
    entries, bad = [], []
    for spdx, pkgs in json.loads(out).items():
        ok = expr_allowed(spdx, accepted)
        for p in pkgs:
            if not ok:
                bad.append(f"{p['name']} ({spdx})")
                continue
            for ver, path in zip(p["versions"], p["paths"]):
                text = next((f.read_text(encoding="utf-8", errors="replace") for n in LICENSE_FILES
                             if (f := Path(path) / n).is_file()), None)
                if text is None:
                    bad.append(f"{p['name']}@{ver}: no license file in {path}")
                else:
                    entries.append((p["name"], ver, spdx, text))
    if bad:
        sys.exit("licenses.py: npm packages not allowed or without license text:\n  " + "\n  ".join(bad))
    return entries


def render(rust, js):
    parts = ["MOCHI - third-party licenses\n"
             "============================\n\n"
             "MOCHI is licensed under the MIT license. Generated at build time from the\n"
             "dependency graph of this installer's app and UI bundle (plan D7, O23).\n\n"]
    for f in ("LICENSE", "THIRD-PARTY-NOTICES.md"):
        body = (ROOT / f).read_text(encoding="utf-8")
        parts.append(f"\n{'#' * 78}\n# {f}\n{'#' * 78}\n\n{body}\n")
    for title, entries in (("Rust crates", rust), ("npm packages (UI bundle)", js)):
        groups = {}
        for name, ver, spdx, text in entries:
            groups.setdefault((spdx, text.strip()), []).append(f"{name} {ver}")
        parts.append(f"\n{'#' * 78}\n# {title}: {len(entries)} packages\n{'#' * 78}\n")
        for (spdx, text), users in sorted(groups.items()):
            parts.append(f"\n--- {spdx} ---\nUsed by: {', '.join(sorted(set(users)))}\n\n{text}\n")
    return "".join(parts)


def main():
    # Windows consoles default to a locale codec; never die printing a name.
    sys.stdout.reconfigure(errors="replace")
    sys.stderr.reconfigure(errors="replace")
    if len(sys.argv) != 2:
        sys.exit("usage: licenses.py OUTPUT_FILE")
    accepted = accepted_list()
    rust, js = rust_entries(), js_entries(accepted)
    if not rust or not js:
        sys.exit("licenses.py: empty dependency list; refusing to write a bogus file")
    require_unrar_notice(rust)
    out = Path(sys.argv[1])
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(render(rust, js), encoding="utf-8")
    print(f"wrote {out}: {len(rust)} Rust crate entries, {len(js)} npm entries")


if __name__ == "__main__":
    main()
