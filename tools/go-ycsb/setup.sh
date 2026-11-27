#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Every lab change to go-ycsb is a commit on this fork's master branch.
readonly REPOSITORY=https://github.com/msf/go-ycsb.git
readonly REVISION=86331371b032ee0265e40a2a209d5d4acfcaad09
readonly CHECKOUT="${GO_YCSB_SOURCE:-${HOME}/play/go-ycsb}"
readonly DEST="${ROOT}/tools/build/go-ycsb-${REVISION:0:7}"
readonly NATIVE="${ROOT}/tools/build/workbench-native/root/usr"
readonly SOURCE="${DEST}/source"
# The SQLite driver links the pinned lab SQLite from make setup-sqlite, not the native package, so
# the client and the lab's sqlite3 shell run one version with the same compile options.
readonly SQLITE="${ROOT}/tools/build/sqlite-3.51.3/build"
if [[ ! -f "${SQLITE}/lib/libsqlite3.a" || ! -f "${SQLITE}/include/sqlite3.h" ]]; then
  printf 'missing %s/lib/libsqlite3.a; run make setup-sqlite first\n' "${SQLITE}" >&2
  exit 1
fi
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
# The pinned SQLite omits extension loading, so the driver must omit it too.
CGO_CFLAGS="-I${SQLITE}/include -I${NATIVE}/include" \
CGO_LDFLAGS="-L${SQLITE}/lib -L${NATIVE}/lib/x86_64-linux-gnu -Wl,-rpath,${NATIVE}/lib/x86_64-linux-gnu -lm" \
  go -C "${SOURCE}" build -mod=readonly -trimpath \
    -tags 'libsqlite3 sqlite_omit_load_extension rocksdb grocksdb_clean_link' \
    -o "${DEST}/bin/go-ycsb" ./cmd/go-ycsb
{
  printf 'go_ycsb_repository=%s\ngo_ycsb_revision=%s\n' "${REPOSITORY}" "${REVISION}"
  go version -m "${DEST}/bin/go-ycsb"
  sha256sum "${DEST}/bin/go-ycsb" "${SOURCE}/go.mod" "${SOURCE}/go.sum" \
    "${ROOT}/tools/go-ycsb/setup.sh" "${SQLITE}/lib/libsqlite3.a" \
    "${ROOT}/tools/native/packages.sha256"
} > "${DEST}/build-manifest.txt"
printf 'go-ycsb: %s/bin/go-ycsb\n' "${DEST}"
