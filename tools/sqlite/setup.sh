#!/usr/bin/env bash
set -euo pipefail

readonly VERSION="3.51.3"
readonly RELEASE="3510300"
readonly YEAR="2026"
readonly SRC_ARCHIVE="sqlite-src-${RELEASE}.zip"
readonly AMALGAMATION_ARCHIVE="sqlite-amalgamation-${RELEASE}.zip"
readonly SRC_SHA256="f8a67a1f5b5cae7c6d42f0994ca7bf1a4a5858868c82adc9fc1340bed5eb8cd2"
readonly AMALGAMATION_SHA256="acb1e6f5d832484bf6d32b681e858c38add8b2acdfd42ac5df24b8afb46552b4"
readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly DEST="${ROOT}/tools/build/sqlite-${VERSION}"
readonly CACHE="${DEST}/cache"
readonly BUILD="${DEST}/build"
readonly SOURCE="${DEST}/source"
readonly PATCH="${ROOT}/tools/sqlite/sqlite-autocheckpoint-observer.patch"
readonly COMMON_FLAGS="-O2 -std=c11 -D_POSIX_C_SOURCE=200809L -DSQLITE_THREADSAFE=1 -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_DEFAULT_MMAP_SIZE=0 -DSQLITE_MAX_MMAP_SIZE=0"
# Pinned upstream sources intentionally contain unused test-harness parameters
# and variables, signed comparisons, and a const-discard in the legacy shell.
# Keep these compatibility exceptions off the lab-owned translation units.
readonly UPSTREAM_WARNINGS="-Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-variable -Wno-sign-compare -Wno-discarded-qualifiers"
readonly WRAPS="-Dmain=sqlite_benchmark_main -Dsqlite3_open=lab_open -Dsqlite3_open_v2=lab_open_v2 -Dsqlite3_close=lab_close -Dsqlite3_exec=lab_exec -Dsqlite3_step=lab_step"

require_tool() {
  command -v "$1" >/dev/null 2>&1 || {
    printf 'missing required tool: %s\n' "$1" >&2
    exit 1
  }
}

fetch_archive() {
  local name="$1"
  local expected="$2"
  local url="$3"
  local source_path=""
  local temporary="${CACHE}/${name}.partial"

  if [[ -f "${CACHE}/${name}" ]]; then
    source_path="${CACHE}/${name}"
  elif [[ -n "${SQLITE_DOWNLOAD_CACHE:-}" && -f "${SQLITE_DOWNLOAD_CACHE}/${name}" ]]; then
    cp -- "${SQLITE_DOWNLOAD_CACHE}/${name}" "${temporary}"
    mv -- "${temporary}" "${CACHE}/${name}"
    source_path="${CACHE}/${name}"
  else
    curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
      --retry 3 --connect-timeout 20 --max-time 60 \
      --output "${temporary}" "${url}"
    mv -- "${temporary}" "${CACHE}/${name}"
    source_path="${CACHE}/${name}"
  fi

  printf '%s  %s\n' "${expected}" "${source_path}" | sha256sum --check --status || {
    printf 'SHA256 mismatch for %s; removing untrusted cache entry\n' "${name}" >&2
    rm -f -- "${source_path}"
    exit 1
  }
}

for tool in cc curl patch sha256sum unzip; do
  require_tool "${tool}"
done
mkdir -p -- "${CACHE}" "${BUILD}"
fetch_archive "${SRC_ARCHIVE}" "${SRC_SHA256}" \
  "https://www.sqlite.org/${YEAR}/${SRC_ARCHIVE}"
fetch_archive "${AMALGAMATION_ARCHIVE}" "${AMALGAMATION_SHA256}" \
  "https://www.sqlite.org/${YEAR}/${AMALGAMATION_ARCHIVE}"

work="$(mktemp -d "${DEST}/source.partial.XXXXXX")"
trap 'rm -rf -- "${work}"' EXIT
unzip -q "${CACHE}/${SRC_ARCHIVE}" -d "${work}"
unzip -q "${CACHE}/${AMALGAMATION_ARCHIVE}" -d "${work}"
mkdir -p "${work}/pinned"
cp -- "${work}/sqlite-amalgamation-${RELEASE}/sqlite3.c" \
  "${work}/sqlite-amalgamation-${RELEASE}/sqlite3.h" \
  "${work}/sqlite-amalgamation-${RELEASE}/shell.c" \
  "${work}/pinned/"
cp -- "${work}/sqlite-src-${RELEASE}/test/speedtest1.c" \
  "${work}/sqlite-src-${RELEASE}/test/kvtest.c" "${work}/pinned/"
patch --batch --fuzz=0 -d "${work}/pinned" -p0 < "${PATCH}"
rm -rf -- "${SOURCE}"
mv -- "${work}/pinned" "${SOURCE}"
rm -rf -- "${work}"
trap - EXIT

rm -f -- "${BUILD}"/*.o "${BUILD}/speedtest1" "${BUILD}/kvtest" "${BUILD}/sqlite3"
# Build serially: the amalgamation is compiled once and shared by all three tools.
cc ${COMMON_FLAGS} ${UPSTREAM_WARNINGS} -I"${SOURCE}" \
  -c "${SOURCE}/sqlite3.c" -o "${BUILD}/sqlite3.o"
cc ${COMMON_FLAGS} -Wall -Wextra -Werror -I"${SOURCE}" \
  -c "${ROOT}/tools/sqlite/wal-observer.c" -o "${BUILD}/wal-observer.o"
cc ${COMMON_FLAGS} ${UPSTREAM_WARNINGS} -I"${SOURCE}" ${WRAPS} \
  -c "${SOURCE}/speedtest1.c" -o "${BUILD}/speedtest1-upstream.o"
cc ${COMMON_FLAGS} -Wall -Wextra -Werror -I"${SOURCE}" \
  -DLAB_UPSTREAM_MAIN=sqlite_benchmark_main \
  -c "${ROOT}/tools/sqlite/sqlite_lab.c" -o "${BUILD}/speedtest1-adapter.o"
cc "${BUILD}/speedtest1-upstream.o" "${BUILD}/speedtest1-adapter.o" \
  "${BUILD}/sqlite3.o" "${BUILD}/wal-observer.o" -lpthread -lm \
  -o "${BUILD}/speedtest1"
cc ${COMMON_FLAGS} ${UPSTREAM_WARNINGS} -I"${SOURCE}" ${WRAPS} \
  -c "${SOURCE}/kvtest.c" -o "${BUILD}/kvtest-upstream.o"
cc ${COMMON_FLAGS} -Wall -Wextra -Werror -I"${SOURCE}" \
  -DLAB_UPSTREAM_MAIN=sqlite_benchmark_main -DLAB_KVTEST=1 \
  -c "${ROOT}/tools/sqlite/sqlite_lab.c" -o "${BUILD}/kvtest-adapter.o"
cc "${BUILD}/kvtest-upstream.o" "${BUILD}/kvtest-adapter.o" \
  "${BUILD}/sqlite3.o" "${BUILD}/wal-observer.o" -lpthread -lm \
  -o "${BUILD}/kvtest"
cc ${COMMON_FLAGS} ${UPSTREAM_WARNINGS} -DHAVE_READLINE=0 -I"${SOURCE}" \
  -c "${SOURCE}/shell.c" -o "${BUILD}/shell.o"
cc "${BUILD}/shell.o" "${BUILD}/sqlite3.o" "${BUILD}/wal-observer.o" \
  -lpthread -lm -o "${BUILD}/sqlite3"

source_id="$(${BUILD}/sqlite3 ':memory:' 'SELECT sqlite_source_id();')"
compile_options="$(${BUILD}/sqlite3 -json ':memory:' 'PRAGMA compile_options;')"
patch_sha="$(sha256sum "${PATCH}" | awk '{print $1}')"
adapter_sha="$(sha256sum "${ROOT}/tools/sqlite/sqlite_lab.c" | awk '{print $1}')"
observer_sha="$(sha256sum "${ROOT}/tools/sqlite/wal-observer.c" | awk '{print $1}')"
adapted_sha="$(sha256sum "${SOURCE}/sqlite3.c" | awk '{print $1}')"
sqlite_object_sha="$(sha256sum "${BUILD}/sqlite3.o" | awk '{print $1}')"
speedtest_sha="$(sha256sum "${BUILD}/speedtest1" | awk '{print $1}')"
kvtest_sha="$(sha256sum "${BUILD}/kvtest" | awk '{print $1}')"
shell_sha="$(sha256sum "${BUILD}/sqlite3" | awk '{print $1}')"
cat > "${DEST}/build-manifest.json" <<EOF
{
  "sqlite_version": "${VERSION}",
  "sqlite_release": "${RELEASE}",
  "sqlite_source_id": "${source_id}",
  "archives": {
    "${SRC_ARCHIVE}": "${SRC_SHA256}",
    "${AMALGAMATION_ARCHIVE}": "${AMALGAMATION_SHA256}"
  },
  "adaptation": {
    "identity": "sqlite3WalDefaultHook-passive-counters-v1",
    "observability_only": true,
    "compiled_source_id_remains_upstream": true,
    "patch_sha256": "${patch_sha}",
    "adapter_source_sha256": "${adapter_sha}",
    "observer_source_sha256": "${observer_sha}",
    "adapted_sqlite3_c_sha256": "${adapted_sha}"
  },
  "compile": {
    "cc": "$(cc --version | head -n 1)",
    "common_flags": "${COMMON_FLAGS}",
    "upstream_warning_flags": "${UPSTREAM_WARNINGS}",
    "upstream_translation_unit_renames": "${WRAPS}",
    "amalgamation_and_adapter_use_renames": false,
    "amalgamation_compile_count": 1,
    "link_libraries": ["pthread", "m"],
    "pragma_compile_options": ${compile_options}
  },
  "features": {
    "threadsafe": 1,
    "mmap_max_bytes": 0,
    "dynamic_extension_loading": false,
    "kvtest_randomblob": {
      "identity": "splitmix64-byte-v1",
      "seed": "0x6a09e667f3bcc909",
      "max_bytes": 16777216,
      "upstream_access_order_prng_unchanged": true
    }
  },
  "artifacts_sha256": {
    "sqlite3.o": "${sqlite_object_sha}",
    "speedtest1": "${speedtest_sha}",
    "kvtest": "${kvtest_sha}",
    "sqlite3": "${shell_sha}"
  }
}
EOF
printf 'SQLite lab tools built in %s\nManifest: %s\n' "${BUILD}" "${DEST}/build-manifest.json"
