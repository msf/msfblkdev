#!/bin/bash
set -euo pipefail

# Per-boot host step for blockcheck: load ublk, then verify the one-time setup.
# It installs nothing. The README "Normal-user ublk setup" section owns the install.
if [[ "$EUID" != 0 || -z "${SUDO_USER:-}" || "$SUDO_USER" == root ]]; then
    printf 'Run this through sudo as the intended non-root developer.\n' >&2
    exit 1
fi

# The blockcheck plan's one-time setup creates this mount.
readonly backing_mount=/mnt/blockcheck
readonly control=/dev/ublk-control
readonly runtime=/run/ublksrvd
missing=0

report_missing() {
    printf 'missing: %s\n' "$1" >&2
    missing=1
}

modprobe ublk_drv
udevadm settle --timeout=10

[[ -c "$control" ]] || report_missing "$control after modprobe ublk_drv"
if [[ -c "$control" && $(stat -c '%G %a' "$control") != 'plugdev 660' ]]; then
    report_missing "$control as plugdev mode 0660 (/etc/udev/rules.d/90-ublk.rules)"
fi
id -nG "$SUDO_USER" | tr ' ' '\n' | grep -qx plugdev || report_missing "$SUDO_USER in group plugdev"
[[ -x /usr/local/sbin/ublk_user_id ]] || report_missing /usr/local/sbin/ublk_user_id
[[ -x /usr/local/sbin/ublk_chown.sh ]] || report_missing /usr/local/sbin/ublk_chown.sh
if [[ ! -d "$runtime" || -L "$runtime" || $(stat -c %U "$runtime") != "$SUDO_USER" ]]; then
    report_missing "$runtime owned by $SUDO_USER (/etc/tmpfiles.d/block-storage-ublk.conf)"
fi
[[ $(findmnt -n -o FSTYPE "$backing_mount" || true) == xfs ]] || report_missing "XFS mounted at $backing_mount"
[[ $(stat -c %U "$backing_mount") == "$SUDO_USER" ]] || report_missing "$backing_mount owned by $SUDO_USER"

if [[ "$missing" != 0 ]]; then
    printf 'Complete the README "Normal-user ublk setup" and the blockcheck plan one-time setup.\n' >&2
    exit 1
fi
printf 'blockcheck host ready: ublk_drv loaded, %s and %s usable by %s.\n' "$control" "$backing_mount" "$SUDO_USER"
