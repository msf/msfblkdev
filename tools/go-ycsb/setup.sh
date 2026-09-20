#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly REVISION=427715abeef2186b123cc93ced4acb385ff59ce1
readonly CHECKOUT="${GO_YCSB_SOURCE:-${HOME}/play/go-ycsb}"
readonly DEST="${ROOT}/tools/build/go-ycsb-${REVISION:0:7}"
readonly NATIVE="${ROOT}/tools/build/workbench-native/root/usr"
readonly SOURCE="${DEST}/source"
mkdir -p "${DEST}/bin"
git -C "${CHECKOUT}" cat-file -e "${REVISION}^{commit}"
if [[ ! -d "${SOURCE}" ]]; then
  mkdir "${SOURCE}"
  git -C "${CHECKOUT}" archive "${REVISION}" | tar -x -C "${SOURCE}"
fi
# The maintained fork's v1.9.9 targets RocksDB 9.11.x.
sed -i \
  -e 's|"github.com/tecbot/gorocksdb"|gorocksdb "github.com/linxGnu/grocksdb"|' \
  -e 's/p.GetInt(rocksdbArenaBlockSize,/p.GetUint64(rocksdbArenaBlockSize,/' \
  -e 's/p.GetInt(rocksdbDBWriteBufferSize,/p.GetUint64(rocksdbDBWriteBufferSize,/' \
  -e 's/p.GetInt(rocksdbMemtableHugePageSize,/p.GetUint64(rocksdbMemtableHugePageSize,/' \
  -e 's/p.GetInt(rocksdbWriteBufferSize,/p.GetUint64(rocksdbWriteBufferSize,/' \
  "${SOURCE}/db/rocksdb/db.go"
if ! patch --batch --silent --dry-run --reverse -d "${SOURCE}" -p1 < "${ROOT}/tools/go-ycsb/lab.patch" >/dev/null 2>&1; then
  patch --batch --forward -d "${SOURCE}" -p1 < "${ROOT}/tools/go-ycsb/lab.patch"
fi
gofmt -w "${SOURCE}/db/sqlite/db.go" "${SOURCE}/db/rocksdb/db.go" "${SOURCE}/pkg/workload/core.go"
go -C "${SOURCE}" mod edit -droprequire=github.com/tecbot/gorocksdb \
  -require=github.com/linxGnu/grocksdb@v1.9.9
go -C "${SOURCE}" mod download github.com/linxGnu/grocksdb@v1.9.9
CGO_CFLAGS="-I${NATIVE}/include" \
CGO_LDFLAGS="-L${NATIVE}/lib/x86_64-linux-gnu -Wl,-rpath,${NATIVE}/lib/x86_64-linux-gnu -lm" \
  go -C "${SOURCE}" build -mod=readonly -trimpath \
    -tags 'libsqlite3 rocksdb grocksdb_clean_link' \
    -o "${DEST}/bin/go-ycsb" ./cmd/go-ycsb
{
  printf 'go_ycsb_revision=%s\nrocksdb_binding=github.com/linxGnu/grocksdb@v1.9.9\n' "${REVISION}"
  go version -m "${DEST}/bin/go-ycsb"
  sha256sum "${DEST}/bin/go-ycsb" "${SOURCE}/go.mod" "${SOURCE}/go.sum" \
    "${SOURCE}/db/rocksdb/db.go" "${SOURCE}/db/sqlite/db.go" \
    "${SOURCE}/pkg/workload/core.go" "${ROOT}/tools/go-ycsb/lab.patch" \
    "${ROOT}/tools/go-ycsb/setup.sh" \
    "${ROOT}/tools/native/packages.sha256"
} > "${DEST}/build-manifest.txt"
printf 'go-ycsb: %s/bin/go-ycsb\n' "${DEST}"
