#!/usr/bin/env bash
# Backticks in single quotes are Markdown for the summary page, not expansions.
# shellcheck disable=SC2016

# Runs every repository test step in order of required access. It continues
# after a failed or skipped step and writes one summary page for the run.
# Exit status: 0 = every step passed, 1 = a step failed, 2 = no failure but
# at least one step was skipped (the run is incomplete).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || exit 2
RUN_ID="test-all-$(date +%Y%m%dT%H%M%S)"
readonly ROOT RUN_ID
readonly RUN_DIR="${ROOT}/evidence/${RUN_ID}"
readonly SUMMARY="${RUN_DIR}/summary.md"
readonly SQLITE_BUILD="${ROOT}/tools/build/sqlite-3.51.3/build"

if [[ "${EUID}" == 0 ]]; then
  echo 'Run as the normal developer user, not root: Cargo and the lab refuse root-owned state.' >&2
  exit 2
fi
# After `scripts/setup.sh` adds the user to plugdev, sessions started earlier lack the group
# until the next login. Re-run once under sg so that /dev/ublk-control is accessible.
if [[ -z "${TEST_ALL_REEXEC:-}" ]] &&
  id -nG "${USER:-$(id -un)}" | tr ' ' '\n' | grep -qx plugdev &&
  ! id -nG | tr ' ' '\n' | grep -qx plugdev; then
  export TEST_ALL_REEXEC=1
  exec sg plugdev -c "$(printf '%q ' bash "${ROOT}/scripts/test-all.sh")"
fi
mkdir -p "${RUN_DIR}"
cd "${ROOT}" || exit 2

declare -a ROWS=() SKIPS=() FAILS=()
passed=0 failed=0 skipped=0

seconds_text() { printf '%dm%02ds' $(($1 / 60)) $(($1 % 60)); }

# Prints the number of tests a step reported, or "-" if its output has no count.
# The lab prints one "PASS" line per test; Cargo and unittest print totals.
count_tests() {
  local kind="$1" log="$2" n=""
  case "${kind}" in
    lab) n="$(grep -c '^PASS ' "${log}")" ;;
    totals) n="$(awk '/^test result:/ {s += $4} /^Ran [0-9]+ test/ {s += $2} END {print s + 0}' "${log}")" ;;
  esac
  [[ -n "${n}" && "${n}" != 0 ]] && printf '%s' "${n}" || printf -- '-'
}

# Prints the evidence paths a step announced, relative to the repository.
# Lab targets print "evidence: PATH"; the VM probe prints "PASS: PATH".
evidence_paths() {
  sed -n -e 's/^evidence: //p' -e 's|^PASS: \(/.*\)|\1|p' "$1" |
    sed -e "s|^${ROOT}/||" -e 's/.*/`&`/' | paste -sd ' ' -
}

skip_step() {
  local name="$1" rerun="$2" reason="$3"
  printf '%-18s SKIP  %s\n' "${name}" "${reason}"
  ROWS+=("| ${name} | SKIP | - | - | \`${rerun}\` | - |")
  SKIPS+=("- **${name}**: ${reason}")
  skipped=$((skipped + 1))
}

# run_step NAME TIMEOUT COUNT_KIND RERUN -- COMMAND...
# TIMEOUT is a backstop above the lab's own deadlines; it includes compilation.
run_step() {
  local name="$1" limit="$2" kind="$3" rerun="$4"
  shift 5
  local log="${RUN_DIR}/${name}.log" started status result elapsed
  printf '%-18s RUN   %s\n' "${name}" "${rerun}"
  started="$(date +%s)"
  timeout --kill-after=60s "${limit}" "$@" < /dev/null > "${log}" 2>&1
  status=$?
  elapsed=$(($(date +%s) - started))
  result=PASS
  if [[ "${status}" != 0 ]]; then
    result=FAIL
    [[ "${status}" == 124 ]] && result="FAIL (timeout ${limit})"
  fi
  local tests evidence
  tests="$(count_tests "${kind}" "${log}")"
  evidence="$(evidence_paths "${log}")"
  printf '%-18s %-5s %s, tests: %s\n' "${name}" "${result%% *}" "$(seconds_text "${elapsed}")" "${tests}"
  ROWS+=("| ${name} | ${result} | ${tests} | $(seconds_text "${elapsed}") | \`${rerun}\` | [log](${name}.log) ${evidence} |")
  if [[ "${result}" == PASS ]]; then
    passed=$((passed + 1))
    return 0
  fi
  failed=$((failed + 1))
  FAILS+=("### ${name}" "" "Log: \`evidence/${RUN_ID}/${name}.log\`" "" '```text' "$(tail -n 25 "${log}")" '```' "")
  return 1
}

# Runs a make preflight target; prints nothing on success, else its last lines.
preflight() {
  local target="$1" log="${RUN_DIR}/${1}.log"
  if timeout --kill-after=10s 600s make --no-print-directory "${target}" < /dev/null > "${log}" 2>&1; then
    return 0
  fi
  printf '`make %s` failed: %s (see %s.log)' "${target}" \
    "$(grep -v '^\s*$' "${log}" | tail -n 2 | paste -sd ' ' -)" "${target}"
  return 1
}

ublk_reason() {
  [[ -c /dev/ublk-control ]] || { printf '/dev/ublk-control missing. Fix: `sudo modprobe ublk_drv`'; return; }
  [[ -r /dev/ublk-control && -w /dev/ublk-control ]] || {
    printf 'no read/write access to /dev/ublk-control. Fix: README "Normal-user ublk setup"'; return; }
  command -v fio > /dev/null || printf 'fio not installed'
}

echo "Writing results to evidence/${RUN_ID}/"

# 1. Unprivileged: no device, no mount, no sudo.
run_step lint 30m none 'make lint' -- make --no-print-directory lint
# make test keeps a 55-second suite deadline for the edit loop; slower hosts and VMs need more.
run_step unit 10m lab 'TEST_SUITE_SECONDS=300 make test' -- env TEST_SUITE_SECONDS=300 make --no-print-directory test
run_step blockcheck 10m totals 'make test-blockcheck-checker' -- make --no-print-directory test-blockcheck-checker
run_step python-tools 5m totals 'make test-python' -- make --no-print-directory test-python
run_step compaction-small 20m totals 'make test-compaction-small' -- make --no-print-directory test-compaction-small
# ADR-05 backing faults on regular files: model controls, descriptor survival,
# lost writeback (FSYNC-01), and a seeded fault plan that must replay to the same trace.
run_step fault-dst 10m totals 'make test-dst' -- make --no-print-directory test-dst
run_step fsync01 10m totals 'make test-fsync01' -- make --no-print-directory test-fsync01
readonly SEED_DIR="${RUN_DIR}/fault-seed"
run_step fault-seed 10m totals 'make test-fault-seed SEED=42' -- bash -c '
  mkdir -p "$1" &&
  make --no-print-directory test-fault-seed SEED=42 OUT="$1/generated" &&
  make --no-print-directory test-fault-seed PLAN="$1/generated/plan.txt" OUT="$1/replayed" &&
  cmp "$1/generated/trace.txt" "$1/replayed/trace.txt" && echo "seed 42 replay: identical trace"' _ "${SEED_DIR}"
run_step acceptance 75m lab 'make test-acceptance' -- make --no-print-directory test-acceptance

# 2. Live ublk device.
reason="$(ublk_reason)"
if [[ -n "${reason}" ]]; then
  skip_step ublk-fio 'make test-ublk-fio' "${reason}"
else
  run_step ublk-fio 15m none 'make test-ublk-fio' -- make --no-print-directory test-ublk-fio
fi

# 3. Live mounts through the installed sudo helper. Each preflight creates nothing.
ext4_reason="${reason}"; xfs_reason="${reason}"
[[ -z "${reason}" ]] && { ext4_reason="$(preflight check-ext4)"; xfs_reason="$(preflight check-xfs)"; }
if [[ -n "${ext4_reason}" ]]; then skip_step ext4 'make test-ext4' "${ext4_reason}"
else run_step ext4 15m none 'make test-ext4' -- make --no-print-directory test-ext4; fi
if [[ -n "${xfs_reason}" ]]; then skip_step xfs 'make test-xfs' "${xfs_reason}"
else run_step xfs 15m none 'make test-xfs' -- make --no-print-directory test-xfs; fi

# 4. SQLite: standalone adapter probes, then the live filesystem matrices.
sqlite_reason=""
[[ -x "${SQLITE_BUILD}/sqlite3" ]] || sqlite_reason='SQLite tools not built. Fix: `make setup-sqlite`'
if [[ -n "${sqlite_reason}" ]]; then
  skip_step sqlite-standalone 'bash tools/sqlite/test-standalone.sh' "${sqlite_reason}"
else
  run_step sqlite-standalone 10m none 'bash tools/sqlite/test-standalone.sh' -- \
    bash tools/sqlite/test-standalone.sh "${RUN_DIR}/sqlite-standalone-detail.log"
fi
[[ -z "${sqlite_reason}" ]] && sqlite_reason="$(preflight check-sqlite)"
for step in sqlite-probe:test-sqlite:ext4 sqlite-ext4:test-sqlite-ext4:ext4 sqlite-xfs:test-sqlite-xfs:xfs; do
  IFS=: read -r name target filesystem <<< "${step}"
  reason="${sqlite_reason}"
  [[ -z "${reason}" && "${filesystem}" == ext4 ]] && reason="${ext4_reason}"
  [[ -z "${reason}" && "${filesystem}" == xfs ]] && reason="${xfs_reason}"
  if [[ -n "${reason}" ]]; then skip_step "${name}" "make ${target}" "${reason}"
  else run_step "${name}" 30m none "make ${target}" -- make --no-print-directory "${target}"; fi
done

# 5. KVM guest prerequisite probe. Its recipe is pinned to one host kernel.
if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
  skip_step vm-boot-probe 'bash tools/vm/boot-probe.sh' 'no read/write access to /dev/kvm'
elif [[ ! -r "${ROOT}/tools/build/vm/initramfs.gz" ]]; then
  skip_step vm-boot-probe 'bash tools/vm/boot-probe.sh' \
    'guest image not built. Fix: `bash tools/vm/setup.sh` (requires host kernel 7.0.0-31-generic)'
else
  run_step vm-boot-probe 5m none 'bash tools/vm/boot-probe.sh' -- bash tools/vm/boot-probe.sh
fi

verdict=PASS
[[ "${skipped}" != 0 ]] && verdict=INCOMPLETE
[[ "${failed}" != 0 ]] && verdict=FAIL
dirty=no
[[ -n "$(git status --porcelain --untracked-files=no 2> /dev/null)" ]] && dirty=yes
{
  printf '# Test run %s\n\n' "${RUN_ID#test-all-}"
  printf -- '- Result: **%s**: %d passed, %d failed, %d skipped\n' "${verdict}" "${passed}" "${failed}" "${skipped}"
  printf -- '- Commit: `%s` (uncommitted changes: %s)\n' "$(git rev-parse --short HEAD 2> /dev/null)" "${dirty}"
  printf -- '- Host: %s, kernel %s, %s\n\n' "$(hostname)" "$(uname -r)" "$(uname -m)"
  printf '| Step | Result | Tests | Time | Rerun | Output |\n|---|---|---|---|---|---|\n'
  printf '%s\n' "${ROWS[@]}"
  if [[ "${#SKIPS[@]}" != 0 ]]; then printf '\n## Skipped\n\n'; printf '%s\n' "${SKIPS[@]}"; fi
  if [[ "${#FAILS[@]}" != 0 ]]; then printf '\n## Failed\n\n'; printf '%s\n' "${FAILS[@]}"; fi
} > "${SUMMARY}"

ln -sfn "${RUN_ID}" "${ROOT}/evidence/test-all-latest"
printf '\n%s: %d passed, %d failed, %d skipped\nSummary: %s\n' \
  "${verdict}" "${passed}" "${failed}" "${skipped}" "${SUMMARY#"${ROOT}"/}"
[[ "${verdict}" == PASS ]] && exit 0
[[ "${verdict}" == FAIL ]] && exit 1
exit 2
