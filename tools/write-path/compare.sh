#!/usr/bin/env bash
# Compares the engine write path of git revisions under hyperfine.
#
# Usage: tools/write-path/compare.sh <rev> [<rev>...]
#
# Each revision's engine is built with the current rust/examples/write_path_bench.rs, so a
# revision that predates the example still builds it. Every run starts from a freshly formatted
# tmpfs backing (hyperfine --prepare, not timed) and is pinned to $CPUS. The script records the
# host state that changes results: CPU model, frequency governor, boost and load.
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly CPUS="${CPUS:-2,3}"
readonly RUNS="${RUNS:-15}"
readonly WARMUP="${WARMUP:-3}"
readonly LOGICAL_MIB="${LOGICAL_MIB:-4096}"
readonly RAW_MIB="${RAW_MIB:-6144}"
readonly WRITES="${WRITES:-262144}"
readonly FLUSH_EVERY="${FLUSH_EVERY:-64}"
readonly SEED="${SEED:-1}"
readonly OUT="${ROOT}/evidence/write-path-$(date +%s%N)"
readonly SCRATCH="$(mktemp -d /tmp/write-path.XXXXXX)"
trap 'rm -rf -- "${SCRATCH}"' EXIT

(($# > 0)) || { echo "usage: $0 <rev> [<rev>...]" >&2; exit 2; }
command -v hyperfine >/dev/null || { echo "hyperfine is missing" >&2; exit 1; }
mkdir -p "${OUT}"

commands=()
names=()
for rev in "$@"; do
  commit="$(git -C "${ROOT}" rev-parse --verify "${rev}^{commit}")"
  short="${commit:0:7}"
  build="${ROOT}/tools/build/write-path/${short}"
  if [[ ! -d "${build}/src" ]]; then
    mkdir -p "${build}/src"
    git -C "${ROOT}" archive "${commit}" rust | tar -x -C "${build}/src"
  fi
  cp -- "${ROOT}/rust/examples/write_path_bench.rs" "${build}/src/rust/examples/"
  cargo build --quiet --locked --release --example write_path_bench \
    --manifest-path "${build}/src/rust/Cargo.toml" --target-dir "${build}/target"
  binary="${build}/target/release/examples/write_path_bench"
  backing="${SCRATCH}/${short}.img"
  commands+=(--prepare "rm -f ${backing} && ${binary} format ${backing} ${LOGICAL_MIB} ${RAW_MIB}"
             "taskset -c ${CPUS} ${binary} write ${backing} ${WRITES} ${FLUSH_EVERY} ${SEED}")
  names+=(--command-name "${rev} (${short})")
  printf '%s %s %s\n' "${rev}" "${commit}" "$(sha256sum "${binary}" | cut -d' ' -f1)" >> "${OUT}/revisions.txt"
done

{
  echo "date: $(date --iso-8601=seconds)"
  echo "kernel: $(uname -r)"
  echo "cpu: $(lscpu | sed -n 's/^Model name: *//p')"
  echo "governor: $(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | sort | uniq -c | xargs)"
  echo "boost: $(cat /sys/devices/system/cpu/cpufreq/boost 2>/dev/null || echo unknown)"
  echo "pinned cpus: ${CPUS}"
  echo "loadavg: $(cat /proc/loadavg)"
  echo "backing: tmpfs $(findmnt -no SOURCE,FSTYPE --target "${SCRATCH}")"
  echo "workload: logical ${LOGICAL_MIB} MiB, raw ${RAW_MIB} MiB, ${WRITES} random 4 KiB writes, flush every ${FLUSH_EVERY}, seed ${SEED}"
} > "${OUT}/environment.txt"
cat "${OUT}/environment.txt"

hyperfine --warmup "${WARMUP}" --runs "${RUNS}" --style basic \
  --export-json "${OUT}/hyperfine.json" --export-markdown "${OUT}/hyperfine.md" \
  "${names[@]}" "${commands[@]}"
echo "evidence: ${OUT}"
