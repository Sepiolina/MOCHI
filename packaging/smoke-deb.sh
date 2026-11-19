#!/usr/bin/env bash
# Smoke-check a built .deb (plan D7, D6, O11): metadata, dependency, contents, the
# .desktop MimeType and the shared-mime-info type for .mochi; with
# --install, also install it with apt, check that every shared library resolves, that the
# installed foreign-format helper lists a fixture (D8), that the mochi CLI runs,
# then remove it. Usage: smoke-deb.sh [--install] path/to/mochi.deb
set -euo pipefail
install=0
if [ "${1:-}" = "--install" ]; then install=1; shift; fi
deb="${1:?usage: smoke-deb.sh [--install] file.deb}"
fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

dpkg-deb --info "$deb"
pkg=$(dpkg-deb --field "$deb" Package)
[ "$pkg" = "mochi" ] || fail "package name is '$pkg', expected 'mochi'"
[ "$(dpkg-deb --field "$deb" Architecture)" = "amd64" ] || fail "not amd64 (O11: x86-64 only)"
depends=$(dpkg-deb --field "$deb" Depends)
echo "Depends: $depends"
case ", $depends," in *", libwebkit2gtk-4.1-0"*) ;; *) fail "Depends lacks libwebkit2gtk-4.1-0" ;; esac
case "$depends" in *libwebkit2gtk-4.0*) fail "Depends names the 4.0 series, not 4.1" ;; esac

# Member paths may or may not carry a leading "./" (dpkg-deb --build adds it,
# Tauri's builder does not): compare them without.
contents=$(dpkg-deb --contents "$deb" | sed -E 's# \./# #')
echo "$contents"
# The app and, next to it, the foreign-format helper (D8, T16; the app looks for it in
# its own directory, mochi_foreign::default_helper_path).
helper=usr/bin/mochi-foreign-helper
# The `mochi` command-line program (packaging/build-installer.sh, externalBin), on PATH.
cli=usr/bin/mochi
exes=$(echo "$contents" | awk '$1 ~ /^-..x/ && $6 ~ /^usr\/bin\// {print $6}')
echo "$exes" | grep -qx "$helper" || fail "$helper missing or not executable in the package"
echo "$exes" | grep -qx "$cli" || fail "$cli missing or not executable in the package"
exe=$(echo "$exes" | grep -vx -e "$helper" -e "$cli" || true)
[ "$(echo "$exe" | grep -c .)" = 1 ] || fail "expected the app plus $helper and $cli in /usr/bin, got: $exes"
echo "$contents" | grep -Eq ' usr/share/applications/[^ ]*\.desktop$' || fail "no .desktop entry"
lic=usr/share/doc/mochi/THIRD-PARTY-LICENSES.txt
echo "$contents" | grep -q " $lic\$" || fail "third-party license list ($lic) missing"
# F8: the user guide, at the resource path the app resolves (help.rs GUIDE_RESOURCE,
# "guide/index.html", under Tauri's resource directory /usr/lib/<productName>/).
guide=usr/lib/MOCHI/guide/index.html
echo "$contents" | grep -q " $guide\$" || fail "user guide ($guide) missing from the package"
dir=$(mktemp -d); trap 'rm -rf "$dir"' EXIT
dpkg-deb --extract "$deb" "$dir"
grep -q "Rust crates:" "$dir/$lic" && grep -q "npm packages (UI bundle):" "$dir/$lic" \
  && grep -q "MIT License" "$dir/$lic" || fail "license list lacks an expected section"
for page in index not-a-backup getting-started encrypted verification tar-compatible recovery-record experimental; do
  [ -s "$dir/usr/lib/MOCHI/guide/$page.html" ] || fail "user guide page $page.html missing or empty"
done
grep -q "This guide is written for the mochi command line." "$dir/$guide" || fail "user guide lacks its command-line scope banner"
if grep -Eq '<script|src=|https?://' "$dir/$guide"; then fail "user guide must be offline and static"; fi
# The helper's own graph (T16): the RARLAB UnRAR paragraph and the crates only it links.
grep -q "UnRAR source code may be used in any software" "$dir/$lic" || fail "license list lacks the RARLAB UnRAR notice"
for c in unrar_sys sevenz-rust2 zip tar flate2; do
  grep -Eq "Used by:.*(^|[ ,])$c [0-9]" "$dir/$lic" || fail "license list does not cover the helper's dependency $c"
done
# The CLI's own graph (clap is only in it).
grep -Eq "Used by:.*(^|[ ,])clap [0-9]" "$dir/$lic" || fail "license list does not cover the CLI's dependency clap"

# D6 (O10): .mochi association and "Open with". The .desktop passes the file
# (%f) and lists application/x-mochi; the foreign formats are only listed; the
# package sets no default for any type.
# MOCHI's own launcher is the one .desktop that is not Find's (mochi-find.desktop, checked below).
desktops=$(echo "$contents" | awk '$6 ~ /^usr\/share\/applications\/.*\.desktop$/ {print $6}')
desktop=$(echo "$desktops" | grep -vx 'usr/share/applications/mochi-find.desktop' || true)
[ "$(echo "$desktop" | grep -c .)" = 1 ] || fail "expected the app's .desktop file and mochi-find.desktop, got: $desktops"
grep -q '^Exec=.* %f$' "$dir/$desktop" || fail "$desktop: Exec must end in ' %f' so the file manager passes the file"
mimetype=$(grep '^MimeType=' "$dir/$desktop" || true)
[ -n "$mimetype" ] || fail "$desktop has no MimeType"
for t in application/x-mochi application/zip application/x-7z-compressed application/vnd.rar application/x-tar application/x-compressed-tar application/gzip; do
  case ";${mimetype#MimeType=}" in *";$t;"*) ;; *) fail "$desktop: MimeType lacks $t" ;; esac
done
# Only the first entry is MOCHI's own type; everything after it is an "Open with" offer.
case "${mimetype#MimeType=}" in application/x-mochi\;*) ;; *) fail "$desktop: application/x-mochi must come first" ;; esac
if echo "$contents" | grep -Eq ' usr/(share|local/share)/(applications/)?(mimeapps|defaults)\.list$'; then
  fail "the package must not set default applications (it would take over ZIP/RAR/...)"
fi
# MOCHI Find: a launcher (app menu), a Dolphin service menu, a Nemo action and a GNOME Files
# (nautilus-python) extension. They run `mochi-desktop --find -- FOLDER...`; none may touch the
# .mochi association or any defaults list, and the launcher registers NO mime type at all
# (inode/directory would let MOCHI be offered, and by file-name order chosen, to open folders).
find=usr/share/applications/mochi-find.desktop
fsm=usr/share/kio/servicemenus/mochi-find.desktop
fnemo=usr/share/nemo/actions/mochi-find.nemo_action
fnaut=usr/share/nautilus-python/extensions/mochi_find.py
for f in "$find" "$fsm" "$fnemo" "$fnaut"; do
  echo "$contents" | grep -q " $f\$" || fail "MOCHI Find: $f missing from the package"
done
exe_name=$(basename "$exe")
grep -qx "Exec=$exe_name --find -- %F" "$dir/$find" || fail "$find: Exec must be '$exe_name --find -- %F' (folders arrive as %F)"
if grep -Eqi '^(MimeType|X-Ubuntu-.*)=' "$dir/$find"; then fail "$find must not have a MimeType: MOCHI must never be a registered folder handler"; fi
grep -qx 'Terminal=false' "$dir/$find" || fail "$find: Terminal=false missing"
grep -q '^Name=Find in MOCHI archives$' "$dir/$find" || fail "$find: Name missing"
grep -q '^Name\[th\]=' "$dir/$find" && grep -q '^Name\[zh_CN\]=' "$dir/$find" || fail "$find: Thai and Chinese names missing"
grep -qx "Exec=$exe_name --find -- %F" "$dir/$fsm" || fail "$fsm: Exec must be '$exe_name --find -- %F'"
grep -q '^MimeType=inode/directory;$' "$dir/$fsm" || fail "$fsm: MimeType must be inode/directory;"
grep -qx 'Actions=mochiFind;' "$dir/$fsm" || fail "$fsm: Actions missing"
grep -qx "Exec=$exe_name --find -- %F" "$dir/$fnemo" || fail "$fnemo: Exec must be '$exe_name --find -- %F'"
grep -qx 'Extensions=dir;' "$dir/$fnemo" || fail "$fnemo: Extensions=dir; missing (folders only)"
grep -qx 'Quote=double' "$dir/$fnemo" || fail "$fnemo: Quote=double missing (paths with spaces)"
python3 -I -m py_compile "$dir/$fnaut" || fail "$fnaut does not compile"
grep -q 'PROGRAM = "mochi-desktop"' "$dir/$fnaut" && grep -q '"--find", "--"' "$dir/$fnaut" || fail "$fnaut must run '$exe_name --find -- FOLDER...'"
# Optional: no package depends on python3-nautilus (the file is unused without it).
if dpkg-deb --field "$deb" Depends | grep -q nautilus; then fail "Depends must not name python3-nautilus"; fi

mime=usr/share/mime/packages/mochi.xml
echo "$contents" | grep -q " $mime\$" || fail "shared-mime-info type ($mime) missing"
python3 - "$dir/$mime" <<'PY' || fail "$mime is not the expected shared-mime-info type"
import sys, xml.etree.ElementTree as ET
ns = "{http://www.freedesktop.org/standards/shared-mime-info}"
root = ET.parse(sys.argv[1]).getroot()
assert root.tag == ns + "mime-info", root.tag
types = {t.get("type"): t for t in root.findall(ns + "mime-type")}
assert list(types) == ["application/x-mochi"], list(types)
globs = [g.get("pattern") for g in types["application/x-mochi"].findall(ns + "glob")]
assert globs == ["*.mochi"], globs
PY
if command -v desktop-file-validate > /dev/null; then
  desktop-file-validate "$dir/$desktop" || fail "$desktop does not validate"
  desktop-file-validate "$dir/$find" || fail "$find does not validate"
fi

if [ "$install" = 1 ]; then
  # Runner images ship a package index that can lag the archive (404s on dependencies).
  sudo apt-get update
  sudo apt-get install -y --no-install-recommends "./$deb"
  bin="/$exe"
  [ -x "$bin" ] || fail "$bin not installed"
  missing=$(ldd "$bin" | grep 'not found' || true)
  [ -z "$missing" ] || fail "unresolved libraries: $missing"
  # D8 (T16): the helper is installed, executable, links, and answers a protocol-v2 list of a
  # real fixture. Run from the installed path, as the app does.
  hbin="/$helper"
  [ -x "$hbin" ] || fail "$hbin not installed or not executable"
  missing=$(ldd "$hbin" | grep 'not found' || true)
  [ -z "$missing" ] || fail "helper: unresolved libraries: $missing"
  here=$(cd "$(dirname "$0")/.." && pwd)
  python3 -I "$here/packaging/helper_probe.py" "$hbin" "$here/fixtures/foreign/basic_infozip.zip" \
    --expect a.txt --expect sub/b.txt || fail "$hbin did not list the fixture"
  # The CLI is installed, links, runs, and is found on PATH.
  missing=$(ldd "/$cli" | grep 'not found' || true)
  [ -z "$missing" ] || fail "mochi: unresolved libraries: $missing"
  [ "$(command -v mochi)" = "/$cli" ] || fail "mochi on PATH is '$(command -v mochi)', expected /$cli"
  mochi --version | head -n 1 | grep -q '^mochi ' || fail "mochi --version did not answer"
  # F8: the installed guide is where the app looks for it.
  [ -s "/$guide" ] || fail "/$guide not installed"
  # dpkg triggers (shared-mime-info, desktop-file-utils) registered the type.
  if [ -d /usr/share/mime ] && command -v update-mime-database > /dev/null; then
    grep -q 'application/x-mochi:\*\.mochi' /usr/share/mime/globs2 || fail "update-mime-database did not register *.mochi"
  fi
  sudo apt-get remove -y mochi
  [ ! -e "$bin" ] || fail "$bin still present after removal"
  [ ! -e "$hbin" ] || fail "$hbin still present after removal"
  [ ! -e "/$cli" ] || fail "/$cli still present after removal"
  [ ! -e "/$guide" ] || fail "/$guide still present after removal"
  [ ! -e "/$mime" ] || fail "/$mime still present after removal"
  for f in "$find" "$fsm" "$fnemo" "$fnaut"; do [ ! -e "/$f" ] || fail "/$f still present after removal"; done
  if [ -f /usr/share/mime/globs2 ]; then
    ! grep -q 'application/x-mochi' /usr/share/mime/globs2 || fail "*.mochi still registered after removal"
  fi
fi
echo "deb smoke check passed"
