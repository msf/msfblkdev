#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly DEST="${ROOT}/tools/build/vm"
readonly KERNEL=7.0.0-31-generic
readonly PACKAGE=linux-image-7.0.0-31-generic_7.0.0-31.31_amd64.deb
readonly SHA256=f144b113c6957c186c404f31646ca749bf30107b12d78ad30d04ff1f3beb08ef
readonly URL="https://archive.ubuntu.com/ubuntu/pool/main/l/linux-signed/${PACKAGE}"
[[ "$(uname -m)" == x86_64 && "$(uname -r)" == "${KERNEL}" ]] || {
  echo 'This probe recipe requires the recorded x86_64 host kernel and its modules.' >&2
  exit 1
}
file /usr/bin/busybox | grep -q 'statically linked' || {
  echo 'The minimal initramfs requires the installed static BusyBox.' >&2; exit 1;
}
mkdir -p "${DEST}"
readonly STAGING="$(mktemp -d "${DEST}/staging.XXXXXX")"
trap 'rm -rf -- "${STAGING}"' EXIT
if [[ ! -f "${DEST}/${PACKAGE}" ]]; then
  curl --fail --silent --show-error --location --connect-timeout 10 --max-time 60 \
    "${URL}" -o "${STAGING}/${PACKAGE}"
  printf '%s  %s\n' "${SHA256}" "${STAGING}/${PACKAGE}" | sha256sum --check --status
  mv "${STAGING}/${PACKAGE}" "${DEST}/${PACKAGE}"
fi
printf '%s  %s\n' "${SHA256}" "${DEST}/${PACKAGE}" | sha256sum --check --status
dpkg-deb --extract "${DEST}/${PACKAGE}" "${STAGING}/package"
install -m 644 "${STAGING}/package/boot/vmlinuz-${KERNEL}" "${DEST}/vmlinuz"
mkdir -p "${STAGING}/initramfs"/{bin,dev,proc,sys,modules}
install -m 755 /usr/bin/busybox "${STAGING}/initramfs/bin/busybox"
ln -s busybox "${STAGING}/initramfs/bin/sh"
install -m 755 "${ROOT}/tools/vm/init" "${STAGING}/initramfs/init"
for module in drivers/block/ublk_drv fs/xfs/xfs; do
  zstd --decompress --stdout --quiet \
    "/lib/modules/${KERNEL}/kernel/${module}.ko.zst" \
    > "${STAGING}/initramfs/modules/${module##*/}.ko"
done
(
  cd "${STAGING}/initramfs"
  find . -print0 | sort -z | cpio --null --create --format=newc --owner=0:0 --quiet \
    | gzip -n > "${DEST}/initramfs.gz"
)
{
  printf 'scope=KVM boot prerequisite probe; not a benchmark image\n'
  printf 'kernel=%s\npackage_url=%s\n' "${KERNEL}" "${URL}"
  qemu-system-x86_64 --version
  sha256sum "${DEST}/${PACKAGE}" "${DEST}/vmlinuz" "${DEST}/initramfs.gz" \
    /usr/bin/busybox "${ROOT}/tools/vm/init" "${ROOT}/tools/vm/setup.sh" \
    "/lib/modules/${KERNEL}/kernel/drivers/block/ublk_drv.ko.zst" \
    "/lib/modules/${KERNEL}/kernel/fs/xfs/xfs.ko.zst"
} > "${DEST}/build-manifest.txt"
printf 'Built %s\n' "${DEST}"
