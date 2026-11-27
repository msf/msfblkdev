#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Every lab change to go-ycsb is a commit on this fork's master branch.
readonly REPOSITORY=https://github.com/msf/go-ycsb.git
readonly REVISION=39ccca7c0f406d997683ed6f490f0352630f223b
readonly CHECKOUT="${GO_YCSB_SOURCE:-${HOME}/play/go-ycsb}"
readonly DEST="${ROOT}/tools/build/go-ycsb-${REVISION:0:7}"
readonly NATIVE="${ROOT}/tools/build/workbench-native/root/usr"
readonly SOURCE="${DEST}/source"
if [[ ! -d "${CHECKOUT}" ]]; then
  git clone --quiet "${REPOSITORY}" "${CHECKOUT}"
fi
if ! git -C "${CHECKOUT}" cat-file -e "${REVISION}^{commit}" 2>/dev/null; then
  git -C "${CHECKOUT}" fetch --quiet "${REPOSITORY}" master
fi
git -C "${CHECKOUT}" cat-file -e "${REVISION}^{commit}"
mkdir -p "${DEST}/bin"
if [[ ! -d "${SOURCE}" ]]; then
  mkdir "${SOURCE}"
  git -C "${CHECKOUT}" archive "${REVISION}" | tar -x -C "${SOURCE}"
fi
CGO_CFLAGS="-I${NATIVE}/include" \
CGO_LDFLAGS="-L${NATIVE}/lib/x86_64-linux-gnu -Wl,-rpath,${NATIVE}/lib/x86_64-linux-gnu -lm" \
  go -C "${SOURCE}" build -mod=readonly -trimpath \
    -tags 'libsqlite3 rocksdb grocksdb_clean_link' \
    -o "${DEST}/bin/go-ycsb" ./cmd/go-ycsb
{
  printf 'go_ycsb_repository=%s\ngo_ycsb_revision=%s\n' "${REPOSITORY}" "${REVISION}"
  go version -m "${DEST}/bin/go-ycsb"
  sha256sum "${DEST}/bin/go-ycsb" "${SOURCE}/go.mod" "${SOURCE}/go.sum" \
    "${ROOT}/tools/go-ycsb/setup.sh" \
    "${ROOT}/tools/native/packages.sha256"
} > "${DEST}/build-manifest.txt"
printf 'go-ycsb: %s/bin/go-ycsb\n' "${DEST}"
