#!/usr/bin/env bash
# Runs the setup and the full test suite inside a disposable Lima VM (Debian 13).
# Works on macOS (VZ) and Linux (QEMU with KVM). Usage: scripts/vm.sh setup|test|shell|delete
# VM_NAME selects the instance (default msfblkdev).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" || exit 2
readonly ROOT
readonly NAME="${VM_NAME:-msfblkdev}"
readonly TEMPLATE="${ROOT}/tools/vm/lima-debian13.yaml"
# Repository path inside the guest, on the guest's own disk. The guest shell expands the tilde.
# shellcheck disable=SC2088
readonly GUEST_REPO='~/msfblkdev'

fail() { printf 'vm: %s\n' "$*" >&2; exit 1; }

command -v limactl > /dev/null ||
  fail 'limactl not found. Install Lima: https://lima-vm.io/docs/installation/ (macOS: brew install lima)'

guest() { limactl shell --workdir / "${NAME}" -- bash -lc "$1"; }

vm_status() { limactl list --format '{{.Status}}' "${NAME}" 2> /dev/null || true; }

ensure_running() {
  case "$(vm_status)" in
    Running) ;;
    Stopped) limactl start --tty=false "${NAME}" ;;
    '') limactl start --tty=false --name="${NAME}" "${TEMPLATE}" ;;
    *) fail "instance ${NAME} is $(vm_status); inspect it with limactl list" ;;
  esac
}

# Sends the history as a bundle (the lab records the commit in its evidence), then copies
# tracked and untracked, non-ignored files over it, so uncommitted changes are tested too.
# Ignored build output (rust/target, tools/build, ublksrv) stays on the host.
sync_repo() {
  echo "Copying the repository to ${NAME}:${GUEST_REPO}"
  git -C "${ROOT}" bundle create - HEAD 2> /dev/null |
    guest "cat > /tmp/repo.bundle && mkdir -p ${GUEST_REPO} && cd ${GUEST_REPO} &&
      { [ -d .git ] || git init --quiet; } && git fetch --quiet /tmp/repo.bundle HEAD &&
      git reset --quiet --hard FETCH_HEAD && rm /tmp/repo.bundle"
  (
    cd "${ROOT}"
    git ls-files -z --cached --others --exclude-standard |
      while IFS= read -r -d '' path; do [[ -e "${path}" ]] && printf '%s\0' "${path}"; done |
      tar --null -T - -czf -
  ) | guest "mkdir -p ${GUEST_REPO} && tar -xzf - -C ${GUEST_REPO}"
}

vm_setup() {
  ensure_running
  sync_repo
  guest "cd ${GUEST_REPO} && YES=1 bash scripts/setup.sh"
}

# Copies every evidence file the run created into evidence/vm-test-<name>-<time>/ on the host.
vm_test() {
  ensure_running
  sync_repo
  local stamp status=0 results
  stamp="$(date +%Y%m%dT%H%M%S)"
  results="${ROOT}/evidence/vm-test-${NAME}-${stamp}"
  guest "touch /tmp/vm-test-start" > /dev/null
  guest "cd ${GUEST_REPO} && make --no-print-directory test-all" || status=$?
  mkdir -p "${results}"
  guest "cd ${GUEST_REPO}/evidence && find . -newer /tmp/vm-test-start \\( -type f -o -type l \\) -print0 |
    tar --null -T - -czf -" | tar -xzf - -C "${results}"
  ln -sfn "vm-test-${NAME}-${stamp}" "${ROOT}/evidence/vm-latest"
  printf '\nVM results: %s\nSummary:    evidence/vm-latest/test-all-latest/summary.md\n' "${results#"${ROOT}"/}"
  return "${status}"
}

case "${1:-}" in
  setup) vm_setup ;;
  test) vm_test ;;
  shell) ensure_running && limactl shell "${NAME}" ;;
  delete) limactl delete --force "${NAME}" ;;
  *) fail 'usage: scripts/vm.sh setup|test|shell|delete' ;;
esac
