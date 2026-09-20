#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly BUILD="${ROOT}/tools/build/vm"
readonly MEMORY_MIB="${1:-320}"
[[ $# -le 1 && "${MEMORY_MIB}" =~ ^[1-9][0-9]{2,3}$ ]] && \
  (( MEMORY_MIB >= 320 && MEMORY_MIB <= 4096 )) || {
  echo 'usage: bash tools/vm/boot-probe.sh [RAM MiB: 320..4096]' >&2; exit 2;
}
readonly EVIDENCE="${ROOT}/evidence/vm-${MEMORY_MIB}m-$(date +%s%N)"
[[ -r /dev/kvm && -w /dev/kvm ]] || { echo 'KVM access is required.' >&2; exit 1; }
[[ -r "${BUILD}/build-manifest.txt" && -r "${BUILD}/initramfs.gz" ]] || {
  echo 'Run bash tools/vm/setup.sh first.' >&2; exit 1;
}
mkdir "${EVIDENCE}"
cp "${BUILD}/build-manifest.txt" "${EVIDENCE}/build-manifest.txt"
readonly -a COMMAND=(qemu-system-x86_64
  -machine q35,accel=kvm -cpu host -m "${MEMORY_MIB}M" -smp 1
  -nodefaults -display none -monitor none -serial stdio -no-reboot
  -kernel "${BUILD}/vmlinuz" -initrd "${BUILD}/initramfs.gz"
  -append "console=ttyS0 rdinit=/init panic=1 lab.memory_mib=${MEMORY_MIB}")
printf '%q ' "${COMMAND[@]}" > "${EVIDENCE}/command.txt"
printf '\n' >> "${EVIDENCE}/command.txt"
timeout --kill-after=5s 45s "${COMMAND[@]}" > "${EVIDENCE}/console.log" 2>&1
grep -q '^vm-probe: PASS ' "${EVIDENCE}/console.log"
printf 'PASS: %s\n' "${EVIDENCE}"
