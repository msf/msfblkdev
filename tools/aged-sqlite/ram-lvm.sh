#!/usr/bin/env bash
# RAM-backed LVM layout for the aged SQLite matrix. Run with sudo: setup | status | teardown.
#
# One brd RAM disk holds volume group "bench". Every variant gets exactly RAW_MIB of raw space;
# a filesystem or a thin pool pays its own overhead from it:
#
#   bench/raw   RAW_MIB                     raw block-device backing for our daemon
#   bench/xfs   RAW_MIB, XFS, discard       holds our daemon's backing file
#   bench/ext4  RAW_MIB, ext4, discard      holds our daemon's backing file
#   bench/pool  RAW_MIB thin pool: data RAW_MIB - META_MIB, metadata META_MIB, no spare
#   bench/thin  THIN_MIB virtual, XFS, discard: the baseline without our daemon
#
# brd allocates RAM only for written pages. The NVMe sees none of the benchmark's writes.
set -euo pipefail

readonly RAW_MIB="${RAW_MIB:-9216}"
readonly META_MIB="${META_MIB:-64}"
readonly THIN_MIB="${THIN_MIB:-8192}"
readonly VG=bench
readonly MOUNTS=/mnt/bench
readonly DISK=/dev/ram0
# Four LVs plus LVM metadata and extent rounding.
readonly BRD_MIB=$((4 * RAW_MIB + 256))

owner() {
  [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != root ]] || { echo "run with sudo as the benchmark user" >&2; exit 2; }
  echo "${SUDO_USER}"
}

make_filesystem_lv() {
  local name="$1" mkfs=("${@:2}")
  lvcreate --yes --wipesignatures y -L "${RAW_MIB}m" -n "${name}" "${VG}" >/dev/null
  "${mkfs[@]}" "/dev/${VG}/${name}" >/dev/null
  mkdir -p "${MOUNTS}/${name}"
  mount -o discard "/dev/${VG}/${name}" "${MOUNTS}/${name}"
  chown "$(owner):" "${MOUNTS}/${name}"
}

setup() {
  local user; user="$(owner)"
  [[ ! -e "${DISK}" ]] || { echo "${DISK} exists: brd is already loaded; run teardown first" >&2; exit 1; }
  modprobe brd rd_nr=1 rd_size=$((BRD_MIB * 1024))
  pvcreate --yes "${DISK}" >/dev/null
  vgcreate "${VG}" "${DISK}" >/dev/null
  lvcreate --yes --wipesignatures y -L "${RAW_MIB}m" -n raw "${VG}" >/dev/null
  chown "${user}:" "$(readlink -f "/dev/${VG}/raw")"
  make_filesystem_lv xfs mkfs.xfs -q -f
  make_filesystem_lv ext4 mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0
  lvcreate --yes --type thin-pool -L "$((RAW_MIB - META_MIB))m" --poolmetadatasize "${META_MIB}m" \
    --poolmetadataspare n -n pool "${VG}" >/dev/null
  lvcreate --yes --thin -V "${THIN_MIB}m" -n thin "${VG}/pool" >/dev/null
  mkfs.xfs -q -f "/dev/${VG}/thin"
  mkdir -p "${MOUNTS}/thin"
  mount -o discard "/dev/${VG}/thin" "${MOUNTS}/thin"
  chown "${user}:" "${MOUNTS}/thin"
  status
}

status() {
  lvs -a -o lv_name,lv_size,data_percent,metadata_percent,lv_attr "${VG}"
  df -B1M --output=target,size,avail "${MOUNTS}"/* 2>/dev/null || true
  printf 'brd discard_max_bytes: %s (0: discards do not return RAM)\n' \
    "$(cat /sys/block/ram0/queue/discard_max_bytes)"
  printf 'raw backing: /dev/%s/raw -> %s\n' "${VG}" "$(readlink -f "/dev/${VG}/raw")"
  printf 'thin pool data device: %s\n' "$(readlink -f "/dev/mapper/${VG}-pool_tdata")"
}

teardown() {
  for name in xfs ext4 thin; do
    mountpoint -q "${MOUNTS}/${name}" && umount "${MOUNTS}/${name}"
  done
  vgremove --yes --force "${VG}" >/dev/null 2>&1 || true
  pvremove --yes "${DISK}" >/dev/null 2>&1 || true
  rmmod brd
}

case "${1:-}" in
  setup) setup ;;
  status) status ;;
  teardown) teardown ;;
  *) echo "usage: sudo $0 setup|status|teardown" >&2; exit 2 ;;
esac
