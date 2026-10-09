#!/usr/bin/env bash
# C10 interop (spec 7.2, Annex B.2.9 D19 rule 10): a TAR-compatible archive,
# decoded by the reference Zstandard CLI and extracted by independent tar
# implementations. The tools and versions this runs are the only ones the
# compatibility claim in docs/c10-tar-compat.md names.
#
#   ci/tar-interop.sh MOCHI ZSTD GNU_TAR BSD_TAR
#
# MOCHI    the `mochi` binary
# ZSTD     the zstd command line tool
# GNU_TAR  GNU tar (use `-` to skip)
# BSD_TAR  bsdtar / libarchive tar (use `-` to skip)
#
# Scenario: commit 0 creates a tree with a 3 MB incompressible file (several
# chunks), an empty file, a nested directory; commit 1 changes one file and
# adds another; commit 2 deletes the added file (no stream). Extracting the
# concatenated streams therefore yields the *historical* tree: the last write
# of every path wins, and the deleted file is still there. The script asserts
# exactly that, and that `mochi get` (the latest snapshot) differs from it.
set -euo pipefail

MOCHI=$1 ZSTD=$2 GNU_TAR=$3 BSD_TAR=$4
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

mkdir -p src/dir expected/src/dir
printf 'one\n' > src/f1
head -c 3000000 /dev/urandom > src/big.bin
: > src/empty
printf 'three\n' > src/dir/f3

m() { "$MOCHI" --no-local-history "$@"; }

# `create` exits 2 on Windows until the directory entry's durability is
# confirmable (plan O12, gate G6); anything the commit got wrong is caught by
# `verify` below.
rc=0
m create a.mochi src --tar-compatible || rc=$?
case $rc in 0 | 2) ;; *) echo "create failed with exit $rc"; exit 1 ;; esac
printf 'two, changed\n' > src/f1
printf 'new\n' > src/f4
m append a.mochi src
m append a.mochi --delete src/f4

# What generic tools must extract: every path's last put, deleted or not.
cp src/big.bin expected/src/big.bin
: > expected/src/empty
printf 'three\n' > expected/src/dir/f3
printf 'two, changed\n' > expected/src/f1
printf 'new\n' > expected/src/f4

echo "== versions"
"$ZSTD" --version
[ "$GNU_TAR" = - ] || "$GNU_TAR" --version | head -n 1
[ "$BSD_TAR" = - ] || "$BSD_TAR" --version | head -n 1

echo "== mochi verify (streams parsed against the puts)"
report=$(m verify a.mochi --json)
grep -q '"integrity": *"PASS"' <<< "$report" || { echo "$report"; exit 1; }
m fsck a.mochi > /dev/null

echo "== zstd: every frame decodes, skippable frames skipped"
"$ZSTD" -t a.mochi
"$ZSTD" -dc a.mochi > streams.tar
test -s streams.tar

if [ "$GNU_TAR" != - ]; then
  echo "== GNU tar"
  n=$("$ZSTD" -dc a.mochi | "$GNU_TAR" -t --ignore-zeros -f - | grep -c .)
  test "$n" -eq 13 || { echo "GNU tar listed $n members, expected 13"; exit 1; }
  mkdir out-gnu
  "$ZSTD" -dc a.mochi | "$GNU_TAR" -x --ignore-zeros -f - -C out-gnu
  diff -r expected out-gnu
  # The same from a file rather than a pipe.
  mkdir out-gnu-file
  "$GNU_TAR" -x --ignore-zeros -f streams.tar -C out-gnu-file
  diff -r expected out-gnu-file
fi

if [ "$BSD_TAR" != - ]; then
  echo "== bsdtar"
  # libarchive stops at the first end-of-archive marker unless told to read
  # concatenated archives (docs/c10-tar-compat.md).
  n=$("$ZSTD" -dc a.mochi | "$BSD_TAR" -t --options read_concatenated_archives -f - | grep -c .)
  test "$n" -eq 13 || { echo "bsdtar listed $n members, expected 13"; exit 1; }
  mkdir out-bsd
  "$ZSTD" -dc a.mochi | "$BSD_TAR" -x --options read_concatenated_archives -f - -C out-bsd
  diff -r expected out-bsd
fi

echo "== the latest snapshot is not the historical stream"
mkdir snap
m get a.mochi -C snap > /dev/null
test ! -e snap/src/f4         # deleted in commit 2 ...
test -e out-gnu/src/f4 || test -e out-bsd/src/f4   # ... still in the stream
echo "tar interop: ok"
