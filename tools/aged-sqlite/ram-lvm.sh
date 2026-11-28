#!/usr/bin/env bash
# RAM-backed LVM layout for the aged workload matrix. Run with sudo: setup | status | teardown.
#
# One brd RAM disk holds volume group "bench". Every stack gets exactly RAW_MIB of raw space; a
# filesystem, a thin pool or dm-integrity pays its own overhead from it:
#
#   bench/raw     RAW_MIB                     raw block-device backing for our daemon
#   bench/xfs     RAW_MIB, XFS, discard       holds our daemon's backing file
#   bench/ext4    RAW_MIB, ext4 -m 0, discard holds our daemon's backing file
#   bench/pool    RAW_MIB thin pool: data RAW_MIB - META_MIB, metadata META_MIB, no spare
#   bench/thin    THIN_MIB virtual, XFS, discard: the thin baseline
#   bench/thinfio THIN_MIB virtual, no filesystem: fio's thin baseline, in the same pool
#   bench/integ   RAW_MIB under dm-integrity (journal mode, crc32c, 4 KiB sectors), opened as
#                 /dev/mapper/integ-bench and used as the only PV of volume group "benchi":
#     benchi/pool    all of benchi: data = free - META_MIB, metadata META_MIB, no spare
#     benchi/thin    THIN_MIB virtual, XFS, discard: the thin baseline with checksums
#     benchi/thinfio THIN_MIB virtual, no filesystem: fio's checksummed thin baseline
#
# ext4 gets -m 0: the 5% of blocks reserved for root is a policy, not filesystem overhead, and
# our backing file belongs to the benchmark user, who can't use them.
#
# Thin LVs in one pool share it. Runs are serial and each returns its pool space with discard,
# so they never hold data at the same time. dm-integrity is opened with --allow-discards so
# discards reach brd and return its RAM.
#
# brd allocates RAM only for written pages. The NVMe sees none of the benchmark's writes.
set -euo pipefail
# LVM prints sizes in the locale's number format: "9136,00" under en_DK, and bash arithmetic
# reads the comma as an operator.
export LC_ALL=C

readonly RAW_MIB="${RAW_MIB:-9216}"
readonly META_MIB="${META_MIB:-64}"
readonly THIN_MIB="${THIN_MIB:-8192}"
readonly VG=bench
readonly INTEGRITY_VG=benchi
# Not "bench-integ": that is the device-mapper name LVM gives LV bench/integ itself.
readonly INTEGRITY_NAME=integ-bench
readonly MOUNTS=/mnt/bench
readonly DISK=/dev/ram0
# Five LVs plus LVM metadata and extent rounding.
readonly BRD_MIB=$((5 * RAW_MIB + 256))

owner() {
  [[ -n "${SUDO_USER:-}" && "${SUDO_USER}" != root ]] || { echo "run with sudo as the benchmark user" >&2; exit 2; }
  echo "${SUDO_USER}"
}

mount_owned() {
  local device="$1" name="$2"
  mkdir -p "${MOUNTS}/${name}"
  mount -o discard "${device}" "${MOUNTS}/${name}"
  chown "$(owner):" "${MOUNTS}/${name}"
}

make_filesystem_lv() {
  local name="$1" mkfs=("${@:2}")
  lvcreate --yes --wipesignatures y -L "${RAW_MIB}m" -n "${name}" "${VG}" >/dev/null
  "${mkfs[@]}" "/dev/${VG}/${name}" >/dev/null
  mount_owned "/dev/${VG}/${name}" "${name}"
}

# A thin pool of data_mib + META_MIB in a volume group, with an XFS thin LV mounted at
# MOUNTS/<mount_name> and a bare thin LV for fio.
make_thin_stack() {
  local vg="$1" data_mib="$2" mount_name="$3" user; user="$(owner)"
  lvcreate --yes --type thin-pool -L "${data_mib}m" --poolmetadatasize "${META_MIB}m" \
    --poolmetadataspare n -n pool "${vg}" >/dev/null
  lvcreate --yes --thin -V "${THIN_MIB}m" -n thin "${vg}/pool" >/dev/null
  mkfs.xfs -q -f "/dev/${vg}/thin"
  mount_owned "/dev/${vg}/thin" "${mount_name}"
  lvcreate --yes --thin -V "${THIN_MIB}m" -n thinfio "${vg}/pool" >/dev/null
  chown "${user}:" "$(readlink -f "/dev/${vg}/thinfio")"
}

setup_integrity_stack() {
  lvcreate --yes --wipesignatures y -L "${RAW_MIB}m" -n integ "${VG}" >/dev/null
  # Journal mode (the default): crash-safe checksums, like our engine's. Format wipes the device
  # so every sector has a valid checksum.
  integritysetup format --batch-mode --sector-size 4096 --integrity crc32c "/dev/${VG}/integ"
  # The superblock doesn't record the hash; open must name the same one as format.
  integritysetup open --allow-discards --integrity crc32c "/dev/${VG}/integ" "${INTEGRITY_NAME}"
  pvcreate --yes "/dev/mapper/${INTEGRITY_NAME}" >/dev/null
  vgcreate "${INTEGRITY_VG}" "/dev/mapper/${INTEGRITY_NAME}" >/dev/null
  local free_mib
  free_mib="$(vgs --noheadings --nosuffix --units m -o vg_free "${INTEGRITY_VG}" | tr -d ' ' | cut -d. -f1)"
  [[ "${free_mib}" =~ ^[0-9]+$ ]] && ((free_mib > META_MIB)) ||
    { echo "unexpected free space in ${INTEGRITY_VG}: '${free_mib}' MiB" >&2; exit 1; }
  # The pool takes the whole volume group: data, metadata, and no spare.
  make_thin_stack "${INTEGRITY_VG}" "$((free_mib - META_MIB))" thin-integ
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
  make_filesystem_lv ext4 mkfs.ext4 -q -F -m 0 -E lazy_itable_init=0,lazy_journal_init=0
  make_thin_stack "${VG}" "$((RAW_MIB - META_MIB))" thin
  setup_integrity_stack
  status
}

status() {
  # Exit 5 before setup_integrity_stack has created INTEGRITY_VG; the bench rows still print.
  lvs -a -o vg_name,lv_name,lv_size,data_percent,metadata_percent,lv_attr "${VG}" "${INTEGRITY_VG}" || true
  df -B1M --output=target,size,avail "${MOUNTS}"/* 2>/dev/null || true
  printf 'brd discard_max_bytes: %s (0: discards do not return RAM)\n' \
    "$(cat /sys/block/ram0/queue/discard_max_bytes)"
  printf 'raw backing: /dev/%s/raw -> %s\n' "${VG}" "$(readlink -f "/dev/${VG}/raw")"
  printf 'thin pool data device: %s\n' "$(readlink -f "/dev/mapper/${VG}-pool_tdata")"
  if [[ -e "/dev/mapper/${INTEGRITY_NAME}" ]]; then
    printf 'dm-integrity: %s MiB raw -> %s MiB provided\n' \
      "$(( $(blockdev --getsize64 "/dev/${VG}/integ") >> 20 ))" \
      "$(( $(blockdev --getsize64 "/dev/mapper/${INTEGRITY_NAME}") >> 20 ))"
    integritysetup dump "/dev/${VG}/integ"
    # Mode J is journal mode; internal_hash names the checksum.
    dmsetup table "${INTEGRITY_NAME}"
  fi
}

teardown() {
  for name in xfs ext4 thin thin-integ; do
    if mountpoint -q "${MOUNTS}/${name}"; then umount "${MOUNTS}/${name}"; fi
  done
  vgremove --yes --force "${INTEGRITY_VG}" >/dev/null 2>&1 || true
  pvremove --yes "/dev/mapper/${INTEGRITY_NAME}" >/dev/null 2>&1 || true
  if [[ -e "/dev/mapper/${INTEGRITY_NAME}" ]]; then integritysetup close "${INTEGRITY_NAME}"; fi
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
