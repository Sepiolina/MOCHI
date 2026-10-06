#!/usr/bin/env bash
# Gate G7 power-loss run (spec Annex B.2.6 G7; D13 creation).
#
# Runs as root. An ext4 filesystem sits on a dm-flakey device over a loop
# file. For every cut point n of D13 creation (see
# crates/mochi-testkit/examples/g7_powercut.rs), the helper creates the
# archive and, immediately before mutation n, switches the device to
# drop_writes and aborts. Every write after that point, including the
# kernel's later writeback of unsynced pages, is lost, as in a power cut.
# The filesystem is then unmounted, the device restored, the filesystem
# remounted (journal replay), and the helper judges what survived:
# nothing or the complete first commit at the final name; never a lost
# acknowledged-durable creation; leftover temporary files removable.
#
# This is the xfstests dm-flakey method (suspend --nolockfs, load, resume),
# separate from the process-crash tests in t22_creation.rs.
#
# Usage: sudo bash ci/g7-powercut.sh <path to g7_powercut binary>
set -euo pipefail

BIN=$(realpath "${1:?usage: g7-powercut.sh <g7_powercut binary>}")
DM=mochi-g7
WORK=$(mktemp -d)
MNT=$WORK/mnt
LOG=$WORK/ack.log
mkdir -p "$MNT"

truncate -s 64M "$WORK/disk.img"
LOOP=$(losetup --find --show "$WORK/disk.img")
SECTORS=$(blockdev --getsz "$LOOP")
ALLOW="0 $SECTORS flakey $LOOP 0 180 0"
DROP="0 $SECTORS flakey $LOOP 0 0 180 1 drop_writes"

cleanup() {
    umount "$MNT" 2>/dev/null || true
    dmsetup remove "$DM" 2>/dev/null || true
    losetup -d "$LOOP" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

load() {
    dmsetup suspend --nolockfs "$DM"
    dmsetup load "$DM" --table "$1"
    dmsetup resume "$DM"
}
# The command the helper runs to cut the power.
export POWERCUT_CMD="dmsetup suspend --nolockfs $DM && dmsetup load $DM --table '$DROP' && dmsetup resume $DM"

fresh() {
    load "$ALLOW"
    mkfs.ext4 -q -F "/dev/mapper/$DM"
    mount "/dev/mapper/$DM" "$MNT"
}
# Power returns: unmount (its writes are dropped too), restore, remount.
power_on() {
    umount "$MNT"
    load "$ALLOW"
    mount "/dev/mapper/$DM" "$MNT"
}

dmsetup create "$DM" --table "$ALLOW"
echo "kernel $(uname -r); device $LOOP ($SECTORS sectors) via dm-flakey; ext4 $(mkfs.ext4 -V 2>&1 | head -1)"

# Controls: the cut must really drop unsynced writes and keep synced ones,
# or the run below proves nothing.
fresh
printf 'synced' > "$MNT/synced"
sync -f "$MNT/synced"
sync "$MNT"
printf 'unsynced' > "$MNT/unsynced"
sh -c "$POWERCUT_CMD"
power_on
[ "$(cat "$MNT/synced")" = synced ] || { echo "control: a synced file was lost"; exit 1; }
if [ -s "$MNT/unsynced" ]; then
    echo "control: an unsynced write survived the cut; drop_writes is not in effect"
    exit 1
fi
echo "controls: synced data survived, unsynced data was dropped"
umount "$MNT"

fresh
OPS=$("$BIN" count "$MNT" | sed -n 's/^ops //p')
umount "$MNT"
[ -n "$OPS" ] || { echo "could not count cut points"; exit 1; }
echo "cut points: 0..$OPS"

absent=0
complete=0
for n in $(seq 0 "$OPS"); do
    fresh
    set +e
    "$BIN" create "$MNT" "$n" >"$LOG" 2>"$WORK/err"
    rc=$?
    set -e
    if [ "$rc" -ne 134 ]; then
        echo "cut $n: expected the helper to abort (134), got $rc"
        cat "$WORK/err"
        exit 1
    fi
    power_on
    verdict=$("$BIN" check "$MNT" "$LOG")
    echo "cut $n: $verdict"
    case "$verdict" in
        *" complete;"*) complete=$((complete + 1)) ;;
        *" absent;"*) absent=$((absent + 1)) ;;
    esac
    umount "$MNT"
done

# The last cut comes after an acknowledged durable creation.
case "$verdict" in
    *"complete; acknowledged durable: true;"*) ;;
    *) echo "the post-acknowledgement cut did not keep the archive"; exit 1 ;;
esac
echo "power-loss run passed: $((OPS + 1)) cuts, $absent absent, $complete complete"
