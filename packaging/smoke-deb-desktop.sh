#!/usr/bin/env bash
# Desktop-level smoke test of the .deb (docs/dev/release-runbook.md "Automated", Ubuntu part).
# What packaging/smoke-deb.sh does not do: with the package installed from a path
# (`apt install ./mochi_*.deb`, as the release checklist says) it checks the type and
# the "Open with" registration the way the desktop sees it (xdg-mime, gio), starts the
# app through its .desktop launcher under Xvfb on a session bus, opens a path with
# spaces, non-ASCII letters and an ampersand, checks that a second open goes to the
# running window, then removes the package and checks that no MOCHI type, file or
# package record is left.
#
# Usage: smoke-deb-desktop.sh path/to/mochi.deb [results.json]
# Needs: sudo, apt, xvfb, xdotool, x11-utils, dbus, desktop-file-utils, libglib2.0-bin,
#        shared-mime-info, xdg-utils (the workflow installs them). Not a GUI test: what
# the owner sees in GNOME Files (the type name, the "Open with" list) stays manual.
#
# Every check prints "RESULT <id> <PASS|FAIL|INFO> <detail>"; the JSON file gets the same.
# Exit status 1 if any check FAILed.
set -uo pipefail
deb=$(realpath "${1:?usage: smoke-deb-desktop.sh file.deb [results.json]}")
out=${2:-}
results=()
failed=0
logs=$(mktemp -d)

rec() { # id status detail
  local id=$1 st=$2 detail=${3:-}
  echo "RESULT $id $st $detail"
  [ "$st" = FAIL ] && failed=1
  results+=("$(python3 -I -c 'import json,sys; print(json.dumps({"id": sys.argv[1], "status": sys.argv[2], "detail": sys.argv[3]}))' "$id" "$st" "$detail")")
}
finish() {
  if [ -n "$out" ]; then
    { echo '['; local i; for i in "${!results[@]}"; do printf '%s%s\n' "${results[$i]}" "$([ "$i" -lt $((${#results[@]} - 1)) ] && echo ,)"; done; echo ']'; } > "$out"
  fi
  [ "$failed" = 0 ] && echo "deb desktop smoke check passed" || echo "deb desktop smoke check FAILED"
  exit "$failed"
}

# shellcheck source=/dev/null
osr=$(. /etc/os-release && echo "$PRETTY_NAME")
rec U-ENV INFO "$osr; $(dpkg-deb --field "$deb" Package) $(dpkg-deb --field "$deb" Version) $(dpkg-deb --field "$deb" Architecture)"

# ---- install ---------------------------------------------------------------------------------
sudo apt-get update -qq
if sudo apt-get install -y --no-install-recommends "$deb" 2>&1 | tee "$logs/install.log" > /dev/null; then
  rec U-INSTALL PASS "apt install ./$(basename "$deb") succeeded"
else
  tail -20 "$logs/install.log"
  rec U-INSTALL FAIL "apt install ./$(basename "$deb") failed (log above)"
  finish
fi
# The app: not the foreign-format helper, not the `mochi` command line (/usr/bin/mochi).
exe=$(dpkg -L mochi | grep -E '^/usr/bin/' | grep -v mochi-foreign-helper | grep -vx /usr/bin/mochi | head -1)
desktop=$(dpkg -L mochi | grep -E '^/usr/share/applications/.*\.desktop$' | grep -v '/mochi-find\.desktop$' | head -1)
desktop_id=$(basename "$desktop")
find_desktop=/usr/share/applications/mochi-find.desktop
find_kde=/usr/share/kio/servicemenus/mochi-find.desktop
find_nemo=/usr/share/nemo/actions/mochi-find.nemo_action
find_nautilus=/usr/share/nautilus-python/extensions/mochi_find.py
if [ -x "$exe" ] && [ -x /usr/bin/mochi-foreign-helper ] && [ -f "$desktop" ] && [ -f /usr/share/mime/packages/mochi.xml ] \
   && [ -f /usr/share/doc/mochi/THIRD-PARTY-LICENSES.txt ]; then
  rec U-FILES PASS "$exe, /usr/bin/mochi-foreign-helper, $desktop, the mime package and the license list are installed"
else
  rec U-FILES FAIL "an expected file is missing: exe=$exe desktop=$desktop"
fi

# MOCHI Find: the launcher for folders, the Dolphin service menu and the Nemo action are installed.
if [ -f "$find_desktop" ] && [ -f "$find_kde" ] && [ -f "$find_nemo" ]; then
  rec U-FIND-FILES PASS "$find_desktop, $find_kde and $find_nemo are installed"
else
  rec U-FIND-FILES FAIL "missing: $(for f in "$find_desktop" "$find_kde" "$find_nemo"; do [ -e "$f" ] || echo "$f"; done | tr '\n' ' ')"
fi

# ---- the desktop's view of the type and "Open with" ----------------------------------------------
work=$(mktemp -d); trap 'rm -rf "$work" "$logs"' EXIT
tricky="$work/T$(printf '\303\251')st & spaces $(printf '\303\234')n$(printf '\303\257')"
mkdir -p "$tricky"
here=$(cd "$(dirname "$0")/.." && pwd)
archive="$tricky/$(printf '\303\204')rchive & $(printf '\303\247')o.mochi"
archive2="$tricky/Second $(printf '\303\251') & copy.mochi"
cp "$here/fixtures/golden/c5/valid-archive-3-commits.mochi" "$archive"
cp "$here/fixtures/golden/c5/valid-archive-3-commits.mochi" "$archive2"

# GIO is what GNOME Files uses (name and content together). xdg-mime without a desktop
# session falls back to `file`, which sees the zstd frame every .mochi file starts with.
ctype() { gio info -a standard::content-type "$1" 2>/dev/null | sed -n 's/^ *standard::content-type: //p'; }
mt=$(ctype "$archive")
if [ "$mt" = application/x-mochi ]; then rec U-MIME-TYPE PASS "GIO: a .mochi file is application/x-mochi"
else rec U-MIME-TYPE FAIL "GIO says '$mt'"; fi
if grep -q '<comment>MOCHI archive</comment>' /usr/share/mime/application/x-mochi.xml 2>/dev/null; then
  rec U-MIME-NAME PASS "the type's name is 'MOCHI archive' (what GNOME Files shows)"
else rec U-MIME-NAME FAIL "no 'MOCHI archive' comment in /usr/share/mime/application/x-mochi.xml"; fi
if grep -q "^application/x-mochi=.*$desktop_id" /usr/share/applications/mimeinfo.cache 2>/dev/null; then
  rec U-MIME-APP PASS "mimeinfo.cache offers $desktop_id for application/x-mochi"
else rec U-MIME-APP FAIL "$desktop_id is not registered for application/x-mochi in mimeinfo.cache"; fi

# "Open with" must not turn MOCHI into the default program for ZIP, 7z, RAR or tar. What the
# package controls: it ships no defaults list (smoke-deb.sh). What decides the default: the
# desktop's own defaults list when it names one, otherwise the first handler in mimeinfo.cache
# (sorted by file name; "MOCHI.desktop" sorts before "org.gnome.FileRoller.desktop").
explicit_default() { # type -> the program an explicit defaults list names, if any
  grep -h "^$1=" /usr/share/applications/*mimeapps.list /usr/local/share/applications/*mimeapps.list \
    /etc/xdg/*mimeapps.list "$HOME/.config/mimeapps.list" 2>/dev/null | head -1 | cut -d= -f2 | cut -d';' -f1
}
bad=""; listed=""; only=""; byorder=""
for t in application/zip application/x-7z-compressed application/vnd.rar application/x-tar application/x-compressed-tar; do
  def=$(xdg-mime query default "$t" 2>/dev/null)
  line=$(grep "^$t=" /usr/share/applications/mimeinfo.cache 2>/dev/null)
  case "$line" in *"$desktop_id"*) listed="$listed $t" ;; esac
  others=$(echo "${line#*=}" | tr ';' '\n' | grep -v '^$' | grep -vx "$desktop_id" | head -1)
  expl=$(explicit_default "$t")
  if [ "$def" = "$desktop_id" ]; then
    if [ -z "$others" ]; then only="$only $t"
    elif [ -z "$expl" ]; then byorder="$byorder $t"
    else bad="$bad $t"; fi
  fi
done
if [ -n "$bad" ]; then rec U-FOREIGN-NO-DEFAULT FAIL "MOCHI is the default although a defaults list names another program for:$bad"
else rec U-FOREIGN-NO-DEFAULT PASS "MOCHI is offered under Open with for:$listed; the package sets no default, and MOCHI is not the default for any type whose defaults list names another program"; fi
if [ -n "$byorder" ]; then
  rec U-DEFAULT-BY-ORDER INFO "no desktop defaults list names a program for:$byorder, so this machine falls back to the first handler in mimeinfo.cache, and that is MOCHI.desktop (update-desktop-database orders handlers by file name, so a lower-case name would sort first as well). On a real Ubuntu desktop its own defaults list decides: check it in Files (runbook U-04)."
fi
[ -n "$only" ] && rec U-DEFAULT-ONLY INFO "MOCHI is the only registered handler here for:$only"

# MOCHI Find must never be a registered handler of anything: no MimeType in its launcher, so
# mimeinfo.cache never names it, and it is nobody's default (folders keep opening in the file manager).
if grep -q "mochi-find.desktop" /usr/share/applications/mimeinfo.cache 2>/dev/null; then
  rec U-FIND-NO-HANDLER FAIL "mimeinfo.cache names mochi-find.desktop: $(grep 'mochi-find.desktop' /usr/share/applications/mimeinfo.cache | cut -c1-200)"
else
  fdef=$(xdg-mime query default inode/directory 2>/dev/null)
  if [ "$fdef" = mochi-find.desktop ]; then
    rec U-FIND-NO-HANDLER FAIL "mochi-find.desktop is the default for folders"
  else
    rec U-FIND-NO-HANDLER PASS "mochi-find.desktop is not registered for any type (folders open with '${fdef:-nothing registered}')"
  fi
fi
if [ -e /usr/share/nautilus-python/extensions/mochi_find.py ] && python3 -I -m py_compile /usr/share/nautilus-python/extensions/mochi_find.py; then
  rec U-FIND-NAUTILUS PASS "the GNOME Files extension is installed and compiles (it loads only if python3-nautilus is installed)"
else
  rec U-FIND-NAUTILUS FAIL "/usr/share/nautilus-python/extensions/mochi_find.py is missing or does not compile"
fi

# ---- start through the launcher, under Xvfb and a session bus --------------------------------------
cat > "$work/session.sh" <<'EOS'
#!/usr/bin/env bash
# Runs inside dbus-run-session + xvfb-run. Prints SESSION <id> <status> <detail> lines.
desktop=$1; archive=$2; archive2=$3
s() { echo "SESSION $1 $2 $3"; }
export WEBKIT_DISABLE_DMABUF_RENDERER=1 LIBGL_ALWAYS_SOFTWARE=1
count() { pgrep -x -c mochi-desktop 2>/dev/null || true; }
window() { xdotool search --onlyvisible --name 'MOCHI' 2>/dev/null | head -1; }
gio launch "$desktop" "$archive" > /tmp/mochi-launch.log 2>&1
w=""
for _ in $(seq 1 60); do w=$(window); [ -n "$w" ] && break; sleep 1; done
if [ -z "$w" ]; then
  s U-LAUNCH FAIL "no visible window named MOCHI within 60 s of 'gio launch'; processes: $(pgrep -a mochi | head -3 | tr '\n' ';'); log: $(tail -c 400 /tmp/mochi-launch.log)"
  exit 0
fi
sleep 12
w=$(window)
n=$(count)
if [ -n "$w" ] && [ "${n:-0}" -ge 1 ]; then
  s U-LAUNCH PASS "the launcher started MOCHI with '$(basename "$archive")': window '$(xdotool getwindowname "$w" 2>/dev/null)', $n process, still up after 12 s"
else
  s U-LAUNCH FAIL "the window or the process is gone 12 s after the start (window='$w', processes=$n)"
  exit 0
fi
wm=$(pgrep -f webkit -c 2>/dev/null || true)
s U-WEBVIEW INFO "$wm WebKit process(es) running"
gio launch "$desktop" "$archive2" >> /tmp/mochi-launch.log 2>&1
sleep 8
n2=$(count); nw=$(xdotool search --onlyvisible --name 'MOCHI' 2>/dev/null | wc -l)
if [ "${n2:-0}" = 1 ] && [ "$nw" = 1 ]; then
  s U-SINGLE-INSTANCE PASS "a second open went to the running window: 1 process, 1 window"
else
  s U-SINGLE-INSTANCE FAIL "after a second open: $n2 mochi-desktop process(es), $nw window(s)"
fi
# MOCHI Find: the folder launcher goes to the running window (single instance) and starts nothing new.
find_desktop=$4; folder=$5
gio launch "$find_desktop" "$folder" >> /tmp/mochi-launch.log 2>&1
sleep 6
n3=$(count); nw3=$(xdotool search --onlyvisible --name 'MOCHI' 2>/dev/null | wc -l)
if [ "${n3:-0}" = 1 ] && [ "$nw3" = 1 ]; then
  s U-FIND-LAUNCH PASS "the Find launcher with a folder went to the running window: 1 process, 1 window"
else
  s U-FIND-LAUNCH FAIL "after the Find launcher: $n3 mochi-desktop process(es), $nw3 window(s)"
fi
pkill -x mochi-desktop 2>/dev/null; sleep 3
EOS
chmod +x "$work/session.sh"
# shellcheck disable=SC2016
sess=$(dbus-run-session -- xvfb-run -a -s '-screen 0 1280x800x24' "$work/session.sh" "$desktop" "$archive" "$archive2" "$find_desktop" "$tricky" 2>&1)
echo "$sess"
saw=0
while IFS= read -r l; do
  case "$l" in
    "SESSION "*) saw=1; rest=${l#SESSION }; id=${rest%% *}; rest=${rest#* }; st=${rest%% *}; rec "$id" "$st" "${rest#* }" ;;
  esac
done <<< "$sess"
[ "$saw" = 1 ] || rec U-LAUNCH FAIL "the Xvfb session reported nothing"

# What the running app left in the user's folders (observation: removing the package keeps it).
ud=$(find "$HOME/.local/share" "$HOME/.config" "$HOME/.cache" -maxdepth 1 -name 'io.github.immerh8.mochi' 2>/dev/null | tr '\n' ' ')
rec U-USERDATA INFO "user data created by running the app: ${ud:-none}"

# ---- remove ------------------------------------------------------------------------------------------
sudo apt-get remove -y mochi 2>&1 | tee "$logs/remove.log" > /dev/null || { tail -5 "$logs/remove.log"; rec U-REMOVE FAIL "apt remove failed"; }
left=$(for f in "$exe" /usr/bin/mochi-foreign-helper "$desktop" "$find_desktop" "$find_kde" "$find_nemo" "$find_nautilus" /usr/share/mime/packages/mochi.xml /usr/share/doc/mochi; do [ -e "$f" ] && echo "$f"; done)
if [ -z "$left" ]; then rec U-REMOVE PASS "after apt remove the app, helper, .desktop file, mime package and documentation are gone"
else rec U-REMOVE FAIL "still present after apt remove: $left"; fi
mt2=$(ctype "$archive")
cache=$(grep -l "$desktop_id" /usr/share/applications/mimeinfo.cache 2>/dev/null)
glob=$(grep -c 'application/x-mochi' /usr/share/mime/globs2 2>/dev/null || true)
if [ "$mt2" != application/x-mochi ] && [ -z "$cache" ] && [ "${glob:-0}" = 0 ]; then
  rec U-REMOVE-TYPE PASS "no MOCHI archive type or launcher registration remains (GIO says '$mt2')"
else rec U-REMOVE-TYPE FAIL "GIO says '$mt2'; mimeinfo.cache names the launcher: '${cache:-no}'; globs2 lines: ${glob:-0}"; fi
sudo apt-get purge -y mochi 2>&1 | tee "$logs/purge.log" > /dev/null
rest=$(sudo find /usr/bin /usr/share /etc /opt /var/lib/dpkg/info -iname '*mochi*' 2>/dev/null | head -20)
st=$(dpkg-query -W -f '${db:Status-Abbrev}' mochi 2>/dev/null || echo none)
if [ -z "$rest" ] && { [ "$st" = none ] || [ "$st" = "un " ] || [ "$st" = "" ]; }; then
  rec U-PURGE-LEFTOVERS PASS "after apt purge nothing named *mochi* is left under /usr/bin, /usr/share, /etc, /opt or the dpkg file lists (dpkg status: ${st:-none})"
else rec U-PURGE-LEFTOVERS FAIL "left after purge (dpkg status '$st'): $rest"; fi
finish
