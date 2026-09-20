#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly VERSION=9.11.2
readonly SHA256=0466a3c220464410687c45930f3fa944052229c894274fddb7d821397f2b8fba
readonly GFLAGS_SHA256=34af2f15cf7367513b352bdcd2493ab14ce43692d2dcd9dfc499492966c64dcf
readonly DEST="${ROOT}/tools/build/rocksdb-${VERSION}"
readonly SOURCE="${DEST}/rocksdb-${VERSION}"
mkdir -p "${DEST}"
if [[ ! -f "${DEST}/source.tar.gz" ]]; then
  curl --fail --silent --show-error --location --max-time 60 \
    "https://codeload.github.com/facebook/rocksdb/tar.gz/refs/tags/v${VERSION}" \
    -o "${DEST}/source.tar.gz"
fi
printf '%s  %s\n' "${SHA256}" "${DEST}/source.tar.gz" | sha256sum --check --status
if [[ ! -d "${SOURCE}" ]]; then
  tar -xzf "${DEST}/source.tar.gz" -C "${DEST}"
fi
if [[ ! -f "${DEST}/gflags.tar.gz" ]]; then
  curl --fail --silent --show-error --location --max-time 60 \
    https://codeload.github.com/gflags/gflags/tar.gz/refs/tags/v2.2.2 \
    -o "${DEST}/gflags.tar.gz"
fi
printf '%s  %s\n' "${GFLAGS_SHA256}" "${DEST}/gflags.tar.gz" | sha256sum --check --status
if [[ ! -d "${DEST}/gflags-2.2.2" ]]; then
  tar -xzf "${DEST}/gflags.tar.gz" -C "${DEST}"
fi
cmake -S "${DEST}/gflags-2.2.2" -B "${DEST}/gflags-build" -G Ninja \
  -DCMAKE_C_COMPILER=gcc-13 -DCMAKE_CXX_COMPILER=g++-13 \
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="${DEST}/gflags-install" -DBUILD_SHARED_LIBS=OFF \
  -DBUILD_STATIC_LIBS=ON -DBUILD_TESTING=OFF \
  -DREGISTER_BUILD_DIR=OFF -DREGISTER_INSTALL_PREFIX=OFF
CCACHE_DISABLE=1 cmake --build "${DEST}/gflags-build" --parallel 4
cmake --install "${DEST}/gflags-build"
cmake --fresh -S "${SOURCE}" -B "${DEST}/build" -G Ninja \
  -DCMAKE_C_COMPILER=gcc-13 -DCMAKE_CXX_COMPILER=g++-13 \
  -DCMAKE_BUILD_TYPE=Release -DROCKSDB_BUILD_SHARED=OFF \
  -DWITH_TESTS=OFF -DWITH_BENCHMARK_TOOLS=ON -DWITH_TOOLS=ON \
  -DWITH_GFLAGS=ON -DGFLAGS_SHARED=OFF \
  -Dgflags_DIR="${DEST}/gflags-install/lib/cmake/gflags" \
  -DWITH_SNAPPY=OFF -DWITH_LZ4=OFF -DWITH_ZLIB=OFF -DWITH_BZ2=OFF \
  -DWITH_ZSTD=OFF -DPORTABLE=ON -DFAIL_ON_WARNINGS=OFF
CCACHE_DISABLE=1 timeout --kill-after=10s 900s \
  cmake --build "${DEST}/build" --target db_stress db_bench ldb sst_dump --parallel 4
(
  cd "${DEST}"
  sha256sum source.tar.gz gflags.tar.gz build/db_bench build/tools/ldb build/tools/sst_dump build/db_stress_tool/db_stress
) > "${DEST}/artifacts.sha256"
{
  printf 'rocksdb_version=%s\nsource_sha256=%s\n' "${VERSION}" "${SHA256}"
  g++-13 --version | head -1
  cmake --version | head -1
  sha256sum "${ROOT}/tools/rocksdb/setup.sh"
} > "${DEST}/build-manifest.txt"
printf 'RocksDB tools: %s/build\n' "${DEST}"
