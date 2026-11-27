#!/usr/bin/env bash
# BUG-1 field test of block-device backing, without root.
#
# An outer daemon serves a tmpfs file as /dev/ublkbN, which the normal user owns. That device
# is the backing under test:
#   1. The daemon formats it in place with --block-device.
#   2. An inner daemon serves it, and a second format and blkctl check must be refused.
#   3. After a write burst and a blkdiscard of the whole volume, the idle inner daemon must
#      free segments and release them with BLKDISCARD (BUG-4 once stopped its maintenance).
#   4. blkctl check must pass on the device, and the outer volume must have unmapped the
#      discarded ranges.
#
# Usage: tools/device-backing/nested-ublk.sh <directory with block-storage-ublk and blkctl>
set -euo pipefail

bin=${1:?usage: nested-ublk.sh <cargo target directory, such as rust/target/release>}
daemon=$bin/block-storage-ublk
blkctl=$bin/blkctl
dir=$(mktemp -d -p /tmp nested-ublk.XXXXXX)
pids=()
keep=0

cleanup() {
  for pid in "${pids[@]}"; do
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in "${pids[@]}"; do
    wait "$pid" 2>/dev/null || true
  done
  if [ "$keep" -eq 1 ]; then
    echo "evidence kept in $dir" >&2
  else
    rm -rf -- "$dir"
  fi
}
trap cleanup EXIT

fail() {
  keep=1
  echo "FAIL: $*" >&2
  exit 1
}

# Starts a daemon on $1 and sets $device to its path once it is ready. It runs in this shell,
# not in a $(...) subshell, so that cleanup sees the daemon's PID.
serve() {
  local backing=$1 name=$2
  "$daemon" serve "$backing" -1 >"$dir/$name.ready" 2>"$dir/$name.err" &
  pids+=("$!")
  for _ in $(seq 100); do
    if [ -s "$dir/$name.ready" ]; then
      device=$(cat "$dir/$name.ready")
      return
    fi
    sleep 0.1
  done
  fail "$name daemon not ready: $(cat "$dir/$name.err")"
}

# Stops the most recently started daemon cleanly.
stop_last() {
  local pid=${pids[-1]}
  kill -TERM "$pid"
  wait "$pid" || fail "daemon $pid exited with $?"
  unset 'pids[-1]'
}

# Mapped 4 KiB blocks of an offline backing. On a live backing info lags by one checkpoint.
mapped_blocks() {
  "$blkctl" info --blocks --color=never "$1" | awk '$1 == "mapped" {print $2}'
}

"$daemon" format "$dir/outer.img" --size-mib 1024 --prealloc-mib 1536 2>/dev/null
serve "$dir/outer.img" outer
outer=$device
echo "outer device: $outer"

"$daemon" format "$outer" --size-mib 640 --block-device 2>"$dir/format.err" \
  || fail "device format: $(cat "$dir/format.err")"
"$blkctl" info --color=never "$outer" | grep -q "block device" || fail "info: not a block device"


serve "$outer" inner
inner=$device
echo "inner device: $inner"
"$daemon" format "$outer" --size-mib 640 --block-device 2>"$dir/busy.err" \
  && fail "formatted a device that the inner daemon owns"
echo "second format refused: $(cat "$dir/busy.err")"
for _pass in 1 2; do
  dd if=/dev/urandom of="$inner" bs=1M count=640 oflag=direct status=none
done
blkdiscard "$inner"
sleep 1
set +e
"$blkctl" check "$outer" >/dev/null 2>"$dir/check-live.err"
code=$?
set -e
[ "$code" -eq 8 ] || fail "check on the served device exited $code"
echo "check while served: exit 8 ($(cat "$dir/check-live.err"))"

stop_last
stats=$(grep "event=reclaim_stats" "$dir/inner.err" | tail -1 | tr ' ' '\n')
grep -E "reclaim_(barrier_publications|segments_freed|discards_issued)" <<<"$stats"
publications=$(grep -o "reclaim_barrier_publications=[0-9]*" <<<"$stats" | cut -d= -f2)
issued=$(grep -o "reclaim_discards_issued=[0-9]*" <<<"$stats" | cut -d= -f2)
[ "${publications:-0}" -gt 0 ] || fail "the idle inner daemon ran no barrier publication"
[ "${issued:-0}" -gt 0 ] || fail "the idle inner daemon discarded nothing"
grep -q "event=backing_discard_disabled" "$dir/inner.err" \
  && fail "inner daemon disabled discard: $(grep backing_discard_disabled "$dir/inner.err")"
grep "event=space phase=shutdown" "$dir/inner.err" | tr ' ' '\n' \
  | grep -E "backing_declared_mib|backing_allocated_mib"
"$blkctl" check "$outer" || fail "check of the device after a clean stop"

stop_last
"$blkctl" check "$dir/outer.img" >/dev/null || fail "check of the outer image"
mapped=$(mapped_blocks "$dir/outer.img")
echo "outer volume mapped blocks after the inner discards: $mapped of 262144"
# The inner volume wrote 1280 MiB through a 1 GiB device; only BLKDISCARD unmaps any of it.
[ "$mapped" -lt 196608 ] || fail "BLKDISCARD never reached the outer volume"
echo "PASS"
