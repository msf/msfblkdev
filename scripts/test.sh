#!/usr/bin/env bash
set -uo pipefail

slow_seconds=${TEST_SLOW_SECONDS:-1}
per_test_seconds=${TEST_PER_TEST_SECONDS:-15}
suite_seconds=${TEST_SUITE_SECONDS:-55}

for setting in slow_seconds per_test_seconds suite_seconds; do
    value=${!setting}
    if [[ ! $value =~ ^[0-9]+$ ]] || [[ $setting != slow_seconds && $value -eq 0 ]]; then
        printf 'invalid %s: %s (expected %s integer)\n' \
            "$setting" "$value" "$([[ $setting == slow_seconds ]] && echo non-negative || echo positive)" >&2
        exit 2
    fi
done

if [[ ${BLOCK_STORAGE_TEST_SUITE_GUARD:-} != 1 ]]; then
    set +e
    timeout --signal=TERM --kill-after=2s "${suite_seconds}s" \
        env BLOCK_STORAGE_TEST_SUITE_GUARD=1 "$0"
    status=$?
    set -e
    if [[ $status -eq 124 ]]; then
        printf 'FAILED: test suite exceeded %ss deadline\n' "$suite_seconds" >&2
    fi
    exit "$status"
fi

cd "$(dirname "$0")/../rust"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

format_duration() {
    local nanoseconds=$1
    printf '%d.%03d' "$((nanoseconds / 1000000000))" "$(((nanoseconds / 1000000) % 1000))"
}

run_group() {
    local label=$1
    local cargo_target=$2
    local list_output
    local test_name
    local -a tests=()

    if ! list_output=$(cargo test --quiet "$cargo_target" -- --list 2>&1); then
        printf '%s\n' "$list_output" >&2
        return 1
    fi
    while IFS= read -r test_name; do
        [[ $test_name == *': test' ]] || continue
        tests+=("${test_name%: test}")
    done <<< "$list_output"

    if [[ ${#tests[@]} -eq 0 ]]; then
        printf '[%s] no tests\n' "$label"
        return 0
    fi

    for test_name in "${tests[@]}"; do
        local start_ns end_ns elapsed_ns status duration slow_marker
        start_ns=$(date +%s%N)
        timeout --signal=TERM --kill-after=2s "${per_test_seconds}s" \
            cargo test --quiet "$cargo_target" "$test_name" -- --exact >"$tmp" 2>&1
        status=$?
        end_ns=$(date +%s%N)
        elapsed_ns=$((end_ns - start_ns))
        duration=$(format_duration "$elapsed_ns")
        slow_marker=''
        if ((elapsed_ns > slow_seconds * 1000000000)); then
            slow_marker=" SLOW(>${slow_seconds}s)"
        fi

        if [[ $status -eq 0 ]]; then
            printf 'PASS [%s] %s %ss%s\n' "$label" "$test_name" "$duration" "$slow_marker"
            continue
        fi
        cat "$tmp" >&2
        if [[ $status -eq 124 ]]; then
            printf 'TIMEOUT [%s] %s %ss (deadline %ss)\n' \
                "$label" "$test_name" "$duration" "$per_test_seconds" >&2
        else
            printf 'FAIL [%s] %s %ss (exit %s)\n' \
                "$label" "$test_name" "$duration" "$status" >&2
        fi
        return 1
    done
}

run_group library --lib || exit 1
run_group binary --bin=block-storage-ublk || exit 1
run_group doc --doc || exit 1
