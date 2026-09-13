#!/usr/bin/env bash
set -euo pipefail

readonly ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly BUILD="${ROOT}/.tools/sqlite-3.51.3/build"
readonly LOG="${1:-/tmp/sqlite-standalone-$(date +%s).log}"
readonly CASE_ROOT="$(mktemp -d /tmp/sqlite-standalone.XXXXXX)"
unset BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION BLOCK_STORAGE_LAB_EXECUTABLE
suite_succeeded=0

cleanup() {
  if ((suite_succeeded)); then
    rm -rf -- "${CASE_ROOT}"
  else
    printf 'standalone fixtures preserved: %s\n' "${CASE_ROOT}" >&2
  fi
}
trap cleanup EXIT
for tool in "${BUILD}/speedtest1" "${BUILD}/kvtest" "${BUILD}/sqlite3"; do
  [[ -x "${tool}" ]] || { printf 'missing tool: %s\n' "${tool}" >&2; exit 1; }
done
command -v python3 >/dev/null 2>&1 || { echo 'missing required tool: python3' >&2; exit 1; }
[[ ! -e "${LOG}" ]] || { printf 'refusing to overwrite log: %s\n' "${LOG}" >&2; exit 1; }
printf 'standalone log=%s\n' "${LOG}"
(set -o noclobber; : > "${LOG}")
exec >> "${LOG}" 2>&1
printf 'standalone functional probes; no benchmark performance claim\n'
printf 'case_root=%s\nmanifest=%s\n' "${CASE_ROOT}" \
  "${ROOT}/.tools/sqlite-3.51.3/build-manifest.json"

if env -u BLOCK_STORAGE_SQLITE_PROFILE timeout 20 "${BUILD}/kvtest" init \
    "${CASE_ROOT}/missing.db" --count 1 --size 1 > "${CASE_ROOT}/missing-profile.out" 2>&1; then
  echo 'missing profile unexpectedly accepted'
  exit 1
fi
if BLOCK_STORAGE_SQLITE_PROFILE=invalid timeout 20 "${BUILD}/kvtest" init \
    "${CASE_ROOT}/invalid.db" --count 1 --size 1 > "${CASE_ROOT}/invalid-profile.out" 2>&1; then
  echo 'invalid profile unexpectedly accepted'
  exit 1
fi
if BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 20 "${BUILD}/kvtest" run \
    "${CASE_ROOT}/unused.db" --blob-api --cache-size 256 --mmap 0 \
    > "${CASE_ROOT}/blob-api.out" 2>&1; then
  echo 'forbidden blob API unexpectedly accepted'
  exit 1
fi
printf 'missing_profile=PASS\ninvalid_profile=PASS\nblob_api_rejected=PASS\n'

readonly BAD_DB="${CASE_ROOT}/absent/child/open.db"
if BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 20 "${BUILD}/kvtest" init \
    "${BAD_DB}" --count 1 --size 1 > "${CASE_ROOT}/open-error.out" 2>&1; then
  echo 'invalid-path open unexpectedly succeeded'
  exit 1
fi
grep -Eq '"record":"api_error","api":"sqlite3_open"' \
  "${CASE_ROOT}/open-error.out"
if BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 20 "${BUILD}/speedtest1" \
    "${CASE_ROOT}/absent/speedtest.db" --testset main --size 1 --verify \
    > "${CASE_ROOT}/open-v2-error.out" 2>&1; then
  echo 'invalid-path open_v2 unexpectedly succeeded'
  exit 1
fi
grep -Eq '"record":"api_error","api":"sqlite3_open_v2"' \
  "${CASE_ROOT}/open-v2-error.out"
printf 'open_failures_latched=PASS\n'

readonly SNAPSHOT_STUB="${CASE_ROOT}/snapshot-stub"
readonly SNAPSHOT_CALLS="${CASE_ROOT}/snapshot-calls"
cat > "${SNAPSHOT_STUB}" <<'SH'
#!/bin/sh
set -eu
[ "$#" -eq 3 ]
[ "$1" = database-snapshot ]
case "$2" in drain-start|drain-end) ;; *) exit 64 ;; esac
[ "$3" = 2 ]
printf '%s\n' "$2" >> "${BLOCK_STORAGE_SNAPSHOT_STUB_CALLS}"
printf 'sqlite-lab {"record":"device_snapshot","phase":"%s","connection":2,"monotonic_ns":1,"block":{"reads":1,"writes":2,"flushes":3},"ledger":{"write_limit_bytes":4,"write_reserved_bytes":0,"write_completed_bytes":0,"read_completed_bytes":0,"read_completed_operations":0,"write_completed_operations":0,"flush_completed_operations":0,"pending_operations":0,"errors":0,"completed_totals_incomplete":false,"budget_exhausted":false}}\n' "$(printf '%s' "$2" | tr - _)"
SH
# Cargo executables can be group-writable under the developer's normal umask.
chmod 775 "${SNAPSHOT_STUB}"
mkdir "${CASE_ROOT}/snapshot"
BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 30 "${BUILD}/kvtest" init \
  "${CASE_ROOT}/snapshot/main.db" --count 4 --size 128 --pagesize 4096 \
  > "${CASE_ROOT}/snapshot-init.out" 2>&1
printf 'CASE snapshot-boundary BEGIN\n'
BLOCK_STORAGE_SQLITE_PROFILE=wal-full \
BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION=2 \
BLOCK_STORAGE_LAB_EXECUTABLE="${SNAPSHOT_STUB}" \
BLOCK_STORAGE_SNAPSHOT_STUB_CALLS="${SNAPSHOT_CALLS}" \
timeout 30 "${BUILD}/kvtest" run "${CASE_ROOT}/snapshot/main.db" \
  --count 8 --max-id 4 --update --cache-size 256 --mmap 0
printf 'CASE snapshot-boundary END\n'
printf 'drain-start\ndrain-end\n' > "${CASE_ROOT}/expected-snapshot-calls"
cmp "${CASE_ROOT}/expected-snapshot-calls" "${SNAPSHOT_CALLS}"
readonly FAILING_SNAPSHOT_STUB="${CASE_ROOT}/failing-snapshot-stub"
printf '#!/bin/sh\nexit 1\n' > "${FAILING_SNAPSHOT_STUB}"
chmod 700 "${FAILING_SNAPSHOT_STUB}"
if BLOCK_STORAGE_SQLITE_PROFILE=wal-full \
  BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION=2 \
  BLOCK_STORAGE_LAB_EXECUTABLE="${FAILING_SNAPSHOT_STUB}" \
  timeout 20 "${BUILD}/kvtest" run "${CASE_ROOT}/snapshot/main.db" \
  --count 1 --max-id 4 --update --cache-size 256 --mmap 0 \
  > "${CASE_ROOT}/failed-snapshot.out" 2>&1; then
  echo 'failed snapshot child unexpectedly accepted'
  exit 1
fi
grep -Eq '"record":"api_error","api":"database_snapshot_(start|end)"' \
  "${CASE_ROOT}/failed-snapshot.out"
if BLOCK_STORAGE_SQLITE_PROFILE=wal-full \
  BLOCK_STORAGE_SQLITE_SNAPSHOT_CONNECTION=2 \
  timeout 20 "${BUILD}/kvtest" run "${CASE_ROOT}/snapshot/main.db" \
  --count 1 --max-id 4 --update --cache-size 256 --mmap 0 \
  > "${CASE_ROOT}/missing-snapshot-executable.out" 2>&1; then
  echo 'missing snapshot executable unexpectedly accepted'
  exit 1
fi
printf 'snapshot_adapter_validation=PASS\n'

for profile in delete-extra wal-full wal-normal; do
  directory="${CASE_ROOT}/profile-${profile}"
  mkdir "${directory}"
  printf 'CASE profile-%s BEGIN\n' "${profile}"
  BLOCK_STORAGE_SQLITE_PROFILE="${profile}" timeout 30 "${BUILD}/kvtest" \
    init "${directory}/profile.db" --count 2 --size 32 --pagesize 4096
  printf 'CASE profile-%s END\n' "${profile}"
done

for repetition in 1 2; do
  directory="${CASE_ROOT}/update-${repetition}"
  mkdir "${directory}"
  printf 'CASE update-%s BEGIN\n' "${repetition}"
  BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 30 "${BUILD}/kvtest" \
    init "${directory}/main.db" --count 32 --size 8192 --pagesize 4096
  BLOCK_STORAGE_SQLITE_PROFILE=wal-full timeout 60 "${BUILD}/kvtest" \
    run "${directory}/main.db" --count 2048 --max-id 32 --random --update \
    --multitrans --cache-size 256 --mmap 0 --integrity-check
  printf 'CASE update-%s END\n' "${repetition}"
  timeout 20 "${BUILD}/sqlite3" "${directory}/main.db" '.sha3sum --schema' \
    > "${directory}/sha3.txt"
done
cmp "${CASE_ROOT}/update-1/sha3.txt" "${CASE_ROOT}/update-2/sha3.txt"
printf 'update_sha3='; cat "${CASE_ROOT}/update-1/sha3.txt"
printf 'seeded_random_update_repeatability=PASS\n'

for repetition in 1 2; do
  directory="${CASE_ROOT}/speedtest-${repetition}"
  mkdir "${directory}"
  printf 'CASE speedtest-%s BEGIN\n' "${repetition}"
  BLOCK_STORAGE_SQLITE_PROFILE=delete-extra timeout 60 "${BUILD}/speedtest1" \
    "${directory}/main.db" --testset main --size 1 --verify --stats \
    > "${directory}/output.txt" 2>&1
  cat "${directory}/output.txt"
  printf 'CASE speedtest-%s END\n' "${repetition}"
  grep '^Verification Hash:' "${directory}/output.txt" > "${directory}/verifyhash.txt"
  timeout 20 "${BUILD}/sqlite3" "${directory}/main.db" '.sha3sum --schema' \
    > "${directory}/sha3.txt"
done
cmp "${CASE_ROOT}/speedtest-1/verifyhash.txt" \
  "${CASE_ROOT}/speedtest-2/verifyhash.txt"
cmp "${CASE_ROOT}/speedtest-1/sha3.txt" "${CASE_ROOT}/speedtest-2/sha3.txt"
printf 'speedtest_verifyhash='; cat "${CASE_ROOT}/speedtest-1/verifyhash.txt"
printf 'speedtest_sha3='; cat "${CASE_ROOT}/speedtest-1/sha3.txt"
printf 'speedtest_repeatability=PASS\n'

timeout 20 python3 - "${LOG}" <<'PY'
import json
import sys

cases = {}
current = None
with open(sys.argv[1], encoding="utf-8") as stream:
    for line_number, line in enumerate(stream, 1):
        if line.startswith("CASE "):
            _, name, boundary = line.split()
            if boundary == "BEGIN":
                if current is not None or name in cases:
                    raise SystemExit(f"bad case begin at line {line_number}")
                current = name
                cases[name] = []
            elif boundary == "END":
                if current != name:
                    raise SystemExit(f"bad case end at line {line_number}")
                current = None
            continue
        if not line.startswith("sqlite-lab "):
            continue
        record = json.loads(line[len("sqlite-lab "):])
        if record.get("record") in {"api_error", "configuration_error"}:
            raise SystemExit(f"unexpected adapter error at line {line_number}: {record}")
        if current is None:
            raise SystemExit(f"unscoped adapter record at line {line_number}")
        cases[current].append(record)
if current is not None:
    raise SystemExit(f"unterminated case: {current}")

required_profiles = {"delete-extra", "wal-full", "wal-normal"}
observed_profiles = set()
for profile in required_profiles:
    records = cases.get(f"profile-{profile}", [])
    settings = [r for r in records if r.get("record") == "settings"]
    if len(settings) != 2 or {r.get("phase") for r in settings} != {"open", "close"}:
        raise SystemExit(f"unexpected settings count for {profile}: {settings!r}")
    if any(r.get("rc") != 0 or r.get("wal_autocheckpoint") != 1000 for r in settings):
        raise SystemExit(f"invalid effective settings for {profile}: {settings!r}")
    observed_profiles.add(profile)

selected = cases.get("update-1", [])
auto_positions = [i for i, r in enumerate(selected)
                  if r.get("record") == "auto_checkpoint" and r.get("rc") == 0]
if len(auto_positions) < 2:
    raise SystemExit(f"repeated workload auto-checkpoints missing: {auto_positions!r}")
if any(selected[i].get("trigger_frames", 0) < 1000 for i in auto_positions):
    raise SystemExit("automatic checkpoint below configured threshold")
final_checkpoints = [i for i, r in enumerate(selected)
                     if r.get("record") == "checkpoint" and
                     r.get("status") == "completed"]
final_drains = [i for i, r in enumerate(selected)
                if r.get("record") == "drain_close" and
                r.get("status") == "completed"]
if not final_checkpoints or not final_drains:
    raise SystemExit("selected workload lacks a complete final drain")
if max(final_checkpoints) < max(auto_positions) or max(final_drains) < max(final_checkpoints):
    raise SystemExit("only a pre-workload checkpoint/drain was observed")
print(f"effective_profiles={','.join(sorted(observed_profiles))}")
print(f"workload_auto_checkpoints={len(auto_positions)}")
print("workload_final_drain=PASS")

snapshot = cases.get("snapshot-boundary", [])
starts = [i for i, r in enumerate(snapshot)
          if r.get("record") == "device_snapshot" and r.get("phase") == "drain_start"]
ends = [i for i, r in enumerate(snapshot)
        if r.get("record") == "device_snapshot" and r.get("phase") == "drain_end"]
close_settings = [i for i, r in enumerate(snapshot)
                  if r.get("record") == "settings" and r.get("phase") == "close" and
                  r.get("connection") == 2]
checkpoints = [i for i, r in enumerate(snapshot)
               if r.get("record") == "checkpoint" and r.get("connection") == 2]
drains = [i for i, r in enumerate(snapshot)
          if r.get("record") == "drain_close" and r.get("connection") == 2]
processes = [r for r in snapshot if r.get("record") == "process"]
if any(len(items) != 1 for items in
       [starts, ends, close_settings, checkpoints, drains, processes]):
    raise SystemExit(f"unexpected snapshot telemetry: {snapshot!r}")
if not close_settings[0] < starts[0] < checkpoints[0] < ends[0] < drains[0]:
    raise SystemExit("snapshot callbacks do not bracket only the drain clock")
drain = snapshot[drains[0]]
process = processes[0]
for record, fields in [(drain, ("duration_ns", "observer_overhead_ns")),
                       (process, ("duration_ns", "total_snapshot_overhead_ns"))]:
    if record.get("status") != "completed":
        raise SystemExit(f"timing record failed: {record!r}")
    if any(not isinstance(record.get(field), int) or record[field] < 0 for field in fields):
        raise SystemExit(f"invalid monotonic timing: {record!r}")
if drain["observer_overhead_ns"] <= 0 or process["total_snapshot_overhead_ns"] <= 0:
    raise SystemExit("snapshot overhead was not measured")
print("snapshot_boundary_telemetry=PASS")
PY
printf 'standalone_suite=PASS\nlog=%s\n' "${LOG}"
suite_succeeded=1
