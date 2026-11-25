#!/usr/bin/env python3
"""Write the updater manifest `latest.json` for a release (plan D7, T13), or check signatures.

    latest_json.py check --version 0.2.0 FILE...      # each FILE.sig exists and names 0.2.0
    latest_json.py make --version 0.2.0 --tag v0.2.0 --repo immerh8/MOCHI \
        --notes notes.txt --out latest.json \
        --nsis MOCHI_0.2.0_x64-setup.exe --deb MOCHI_0.2.0_amd64.deb

Each installer must have its `.sig` beside it (`<file>.sig`, written by `cargo tauri build`
with createUpdaterArtifacts). The manifest names the installers by their GitHub Release
download URLs; the signature is the `.sig` file's text, which the app checks against the
public key compiled into it before it installs anything. The manifest itself is not signed:
the app also requires the signature to name the announced version (`requireSignedVersion`).

Keys follow the plugin's lookup: `{os}-{arch}-{installer}` first, then `{os}-{arch}`.
"""
import argparse
import base64
import datetime
import json
import pathlib
import re
import sys
import urllib.parse

VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$")


def trusted_comment(sig_text: str) -> str:
    """The trusted comment of a minisign signature, as the Tauri CLI writes the .sig (base64 of the minisig)."""
    decoded = base64.b64decode(sig_text.strip(), validate=True).decode("utf-8")
    for line in decoded.splitlines():
        if line.startswith("trusted comment:"):
            return line[len("trusted comment:"):].strip()
    raise ValueError("the signature has no trusted comment")


def signed_version(sig_text: str):
    for field in trusted_comment(sig_text).split("\t"):
        if field.startswith("version:"):
            return field[len("version:"):]
    return None


def entry(path: pathlib.Path, version: str, repo: str, tag: str) -> dict:
    sig_path = path.with_name(path.name + ".sig")
    if not path.is_file():
        raise SystemExit(f"error: {path} does not exist")
    if not sig_path.is_file():
        raise SystemExit(f"error: {sig_path} does not exist (was the build made with updater artifacts?)")
    if re.search(r"\s", path.name):
        raise SystemExit(f"error: {path.name} has whitespace in its name; GitHub would rename the asset")
    sig = sig_path.read_text(encoding="utf-8").strip()
    got = signed_version(sig)
    if got != version:
        raise SystemExit(
            f"error: {sig_path.name} is signed for version {got!r}, not {version!r}; the app would refuse it"
        )
    url = f"https://github.com/{repo}/releases/download/{urllib.parse.quote(tag)}/{urllib.parse.quote(path.name)}"
    return {"signature": sig, "url": url}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    chk = sub.add_parser("check")
    chk.add_argument("--version", required=True)
    chk.add_argument("files", nargs="+", type=pathlib.Path)
    mk = sub.add_parser("make")
    mk.add_argument("--version", required=True)
    mk.add_argument("--tag", required=True)
    mk.add_argument("--repo", required=True)
    mk.add_argument("--notes", required=True, help="file with the plain-text release notes")
    mk.add_argument("--out", required=True)
    mk.add_argument("--nsis", required=True, type=pathlib.Path)
    mk.add_argument("--deb", required=True, type=pathlib.Path)
    mk.add_argument("--pub-date", help="RFC 3339; default now (UTC)")
    a = ap.parse_args()
    if not VERSION.match(a.version):
        raise SystemExit(f"error: {a.version!r} is not a semantic version")
    if a.cmd == "check":
        for f in a.files:
            entry(f, a.version, "x/y", f"v{a.version}")
            print(f"ok: {f.name}.sig names version {a.version}")
        return 0
    if a.tag != f"v{a.version}":
        raise SystemExit(f"error: tag {a.tag!r} is not v{a.version}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", a.repo):
        raise SystemExit(f"error: bad repository {a.repo!r}")
    pub = a.pub_date or datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    notes = pathlib.Path(a.notes).read_text(encoding="utf-8").strip()
    nsis = entry(a.nsis, a.version, a.repo, a.tag)
    deb = entry(a.deb, a.version, a.repo, a.tag)
    manifest = {
        "version": a.version,
        "notes": notes,
        "pub_date": pub,
        "platforms": {
            "windows-x86_64": nsis,
            "windows-x86_64-nsis": nsis,
            "linux-x86_64": deb,
            "linux-x86_64-deb": deb,
        },
    }
    pathlib.Path(a.out).write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {a.out} for {a.version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
