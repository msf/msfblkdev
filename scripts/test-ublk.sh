#!/usr/bin/env bash
set -Eeuo pipefail

readonly BLOCK_BYTES=4096
readonly VOLUME_BLOCKS=64
readonly RECORD_CAPACITY=32
readonly VOLUME_BYTES=$((BLOCK_BYTES * VOLUME_BLOCKS))
readonly LOG_START_BLOCKS=6
readonly BACKING_BLOCKS=$((LOG_START_BLOCKS + RECORD_CAPACITY * 2))
readonly BACKING_BYTES=$((BLOCK_BYTES * BACKING_BLOCKS))
readonly EXPECTED_SECTORS=$((VOLUME_BYTES / 512))
readonly REPETITIONS=3
readonly PROCESS_SECONDS=20
readonly DELETE_SECONDS=5
readonly BUILD_SECONDS=300
readonly FAILPOINT=descriptor-write-complete

BINARY=
EVIDENCE=
OWNED_DIR=
OWNED_DIR_ID=
BACKING=
DAEMON_PID=
DAEMON_ID=
DAEMON_PATH=
DAEMON_STDOUT=
DAEMON_STDERR=
DAEMON_OUTPUT_RECORDED=1
DEVICE_VALIDATED=0
DEVICE_SYS_ID=
STARTUP_PENDING=0
SECOND_PID=

usage() {
    printf 'usage: %s run | self-test\n' "${0##*/}" >&2
    exit 2
}

log() {
    printf '%s\n' "$*"
    if [[ -n ${EVIDENCE:-} ]]; then
        printf '%s\n' "$*" >>"$EVIDENCE"
    fi
}

log_command() {
    local quoted
    printf -v quoted '%q ' "$@"
    log "+ ${quoted% }"
}

run_logged() {
    local status
    log_command "$@"
    set +e
    "$@" > >(tee -a "$EVIDENCE") 2> >(tee -a "$EVIDENCE" >&2)
    status=$?
    set -e
    log "result: exit=$status"
    return "$status"
}

parse_device_path() {
    local path=$1
    [[ $path =~ ^/dev/ublkb(0|[1-9][0-9]*)$ ]] || return 1
    printf '%s\n' "${BASH_REMATCH[1]}"
}

geometry_values() {
    local volume_blocks=$1
    local physical checksum log_start backing_blocks
    [[ $volume_blocks =~ ^[1-9][0-9]*$ ]] || return 1
    physical=$(((volume_blocks + 1023) / 1024))
    checksum=$(((volume_blocks + 511) / 512))
    log_start=$((2 + 2 * physical + 2 * checksum))
    backing_blocks=$((log_start + 2 * RECORD_CAPACITY))
    printf '%s %s\n' "$log_start" "$backing_blocks"
}

validate_sys_geometry() {
    local sys_root=$1 id=$2 logical sectors
    [[ $id =~ ^(0|[1-9][0-9]*)$ ]] || return 1
    [[ -d $sys_root/block/ublkb$id ]] || return 1
    [[ -r $sys_root/block/ublkb$id/queue/logical_block_size ]] || return 1
    [[ -r $sys_root/block/ublkb$id/size ]] || return 1
    read -r logical <"$sys_root/block/ublkb$id/queue/logical_block_size"
    read -r sectors <"$sys_root/block/ublkb$id/size"
    [[ $logical == "$BLOCK_BYTES" && $sectors == "$EXPECTED_SECTORS" ]]
}

validate_exact_block_node() {
    local path=$1 id=$2 sys_dev node_hex_major node_hex_minor sys_major sys_minor
    [[ $path == "/dev/ublkb$id" && -b $path ]] || return 1
    [[ -r /sys/block/ublkb$id/dev ]] || return 1
    read -r sys_dev </sys/block/ublkb"$id"/dev
    [[ $sys_dev =~ ^([0-9]+):([0-9]+)$ ]] || return 1
    sys_major=${BASH_REMATCH[1]}
    sys_minor=${BASH_REMATCH[2]}
    node_hex_major=$(stat -Lc '%t' -- "$path")
    node_hex_minor=$(stat -Lc '%T' -- "$path")
    ((16#$node_hex_major == sys_major && 16#$node_hex_minor == sys_minor))
}

validate_live_device() {
    local path=$1 id=$2
    validate_exact_block_node "$path" "$id" && validate_sys_geometry /sys "$id"
}

new_owned_dir() {
    local temp_root marker
    temp_root=${TMPDIR:-/tmp}
    OWNED_DIR=$(mktemp -d "$temp_root/my-block-storage-ublk.XXXXXX")
    OWNED_DIR=$(realpath -e -- "$OWNED_DIR")
    [[ -d $OWNED_DIR && ! -L $OWNED_DIR && -O $OWNED_DIR ]] || return 1
    [[ ${OWNED_DIR##*/} == my-block-storage-ublk.* ]] || return 1
    OWNED_DIR_ID=$(stat -Lc '%d:%i' -- "$OWNED_DIR")
    marker=$OWNED_DIR/.owned-by-test-ublk
    (set -o noclobber; printf '%s\n' "$OWNED_DIR_ID" >"$marker")
    [[ -f $marker && ! -L $marker && -O $marker ]]
}

validate_owned_dir() {
    local dir=$1 expected_id=$2 marker id
    [[ -n $dir && -n $expected_id && -d $dir && ! -L $dir && -O $dir ]] || return 1
    [[ ${dir##*/} == my-block-storage-ublk.* ]] || return 1
    [[ $(realpath -e -- "$dir") == "$dir" ]] || return 1
    id=$(stat -Lc '%d:%i' -- "$dir")
    [[ $id == "$expected_id" ]] || return 1
    marker=$dir/.owned-by-test-ublk
    [[ -f $marker && ! -L $marker && -O $marker ]] || return 1
    [[ $(<"$marker") == "$expected_id" ]]
}

validate_owned_backing() {
    local path=$1 dir=$2
    [[ -f $path && ! -L $path && -O $path ]] || return 1
    [[ $(dirname -- "$(realpath -e -- "$path")") == "$dir" ]]
}

remove_owned_dir() {
    local dir=$1 expected_id=$2
    validate_owned_dir "$dir" "$expected_id" || return 1
    rm -rf --one-file-system -- "$dir"
}

process_state() {
    local pid=$1
    [[ -r /proc/$pid/stat ]] || return 1
    awk '{ print $3 }' "/proc/$pid/stat"
}

wait_pid_bounded() {
    local pid=$1 seconds=$2 deadline state
    deadline=$((SECONDS + seconds))
    while kill -0 "$pid" 2>/dev/null; do
        state=$(process_state "$pid" 2>/dev/null || true)
        [[ $state == Z ]] && break
        ((SECONDS < deadline)) || return 124
        sleep 0.05
    done
    set +e
    wait "$pid"
    local status=$?
    set -e
    return "$status"
}

record_daemon_output() {
    ((DAEMON_OUTPUT_RECORDED == 0)) || return 0
    {
        printf '%s\n' '--- daemon stdout ---'
        cat -- "$DAEMON_STDOUT" 2>/dev/null || true
        printf '%s\n' '--- daemon stderr ---'
        cat -- "$DAEMON_STDERR" 2>/dev/null || true
        printf '%s\n' '--- end daemon output ---'
    } >>"$EVIDENCE"
    DAEMON_OUTPUT_RECORDED=1
}

validate_recorded_device_identity() {
    [[ $DEVICE_VALIDATED == 1 && -n $DEVICE_SYS_ID ]] || return 1
    [[ -d /sys/class/ublk-char/ublkc$DAEMON_ID ]] || return 1
    [[ $(stat -Lc '%d:%i' -- "/sys/class/ublk-char/ublkc$DAEMON_ID") == "$DEVICE_SYS_ID" ]]
}

delete_recorded_device() {
    [[ $DEVICE_VALIDATED == 1 && $DAEMON_ID =~ ^(0|[1-9][0-9]*)$ ]] || return 0
    if device_exists "$DAEMON_ID"; then
        validate_recorded_device_identity || {
            log "refusing delete: recorded device identity changed: $DAEMON_ID"
            return 1
        }
        if [[ -e /sys/block/ublkb$DAEMON_ID || -e /dev/ublkb$DAEMON_ID ]]; then
            validate_live_device "/dev/ublkb$DAEMON_ID" "$DAEMON_ID" || {
                log "refusing delete: recorded block device no longer validates: $DAEMON_ID"
                return 1
            }
        fi
        run_logged timeout --signal=TERM --kill-after=2s "${DELETE_SECONDS}s" \
            "$BINARY" delete "$DAEMON_ID"
    fi
    DEVICE_VALIDATED=0
    DEVICE_SYS_ID=
}

stop_daemon() {
    local signal=${1:-TERM} expected=${2:-zero} status
    [[ $DAEMON_PID =~ ^[1-9][0-9]*$ ]] || return 1
    log_command kill "-$signal" "$DAEMON_PID"
    kill "-$signal" "$DAEMON_PID"
    set +e
    wait_pid_bounded "$DAEMON_PID" "$PROCESS_SECONDS"
    status=$?
    set -e
    log "daemon wait result: exit=$status signal=$signal"
    if [[ $status -eq 124 ]]; then
        kill -KILL "$DAEMON_PID" 2>/dev/null || true
        wait_pid_bounded "$DAEMON_PID" 2 >/dev/null 2>&1 || true
        record_daemon_output
        return 1
    fi
    record_daemon_output
    if [[ $expected == zero && $status -ne 0 ]]; then
        return 1
    fi
    if [[ $expected == killed && $status -ne 137 ]]; then
        return 1
    fi
    DAEMON_PID=
}

device_exists() {
    local id=$1
    [[ -e /sys/block/ublkb$id || -e /dev/ublkb$id || -e /sys/class/ublk-char/ublkc$id ]]
}

wait_for_device_absent() {
    local id=$1 deadline=$((SECONDS + PROCESS_SECONDS))
    while device_exists "$id"; do
        ((SECONDS < deadline)) || return 124
        sleep 0.05
    done
}

remove_after_kill() {
    local id=$DAEMON_ID deadline=$((SECONDS + 2))
    while device_exists "$id"; do
        ((SECONDS < deadline)) || break
        sleep 0.05
    done
    delete_recorded_device
    wait_for_device_absent "$id"
}

start_daemon() {
    local failpoint=${1:-} deadline line id
    DAEMON_STDOUT=$OWNED_DIR/daemon.$RANDOM.stdout
    DAEMON_STDERR=$OWNED_DIR/daemon.$RANDOM.stderr
    : >"$DAEMON_STDOUT"
    : >"$DAEMON_STDERR"
    STARTUP_PENDING=1
    if [[ -n $failpoint ]]; then
        log_command env "BLOCK_STORAGE_TEST_FAILPOINT=$failpoint" "$BINARY" serve "$BACKING" -1
        env BLOCK_STORAGE_TEST_FAILPOINT="$failpoint" "$BINARY" serve "$BACKING" -1 \
            >"$DAEMON_STDOUT" 2>"$DAEMON_STDERR" &
    else
        log_command "$BINARY" serve "$BACKING" -1
        "$BINARY" serve "$BACKING" -1 >"$DAEMON_STDOUT" 2>"$DAEMON_STDERR" &
    fi
    DAEMON_PID=$!
    DAEMON_OUTPUT_RECORDED=0
    deadline=$((SECONDS + PROCESS_SECONDS))
    while [[ ! -s $DAEMON_STDOUT ]]; do
        if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
            wait "$DAEMON_PID" || true
            record_daemon_output
            return 1
        fi
        ((SECONDS < deadline)) || return 124
        sleep 0.05
    done
    IFS= read -r line <"$DAEMON_STDOUT"
    id=$(parse_device_path "$line") || return 1
    DAEMON_PATH=$line
    DAEMON_ID=$id
    DEVICE_VALIDATED=0
    validate_live_device "$line" "$id" || return 1
    [[ -d /sys/class/ublk-char/ublkc$id ]] || return 1
    DEVICE_SYS_ID=$(stat -Lc '%d:%i' -- "/sys/class/ublk-char/ublkc$id")
    DEVICE_VALIDATED=1
    STARTUP_PENDING=0
    log "ready: pid=$DAEMON_PID id=$DAEMON_ID path=$DAEMON_PATH logical_block_size=$BLOCK_BYTES sectors=$EXPECTED_SECTORS sys_identity=$DEVICE_SYS_ID"
}

wait_for_failpoint() {
    local name=$1 deadline=$((SECONDS + PROCESS_SECONDS))
    while ! grep -Fxq -- "$name" "$DAEMON_STDOUT"; do
        kill -0 "$DAEMON_PID" 2>/dev/null || return 1
        ((SECONDS < deadline)) || return 124
        sleep 0.05
    done
    log "failpoint handshake: $name"
}

fio_base() {
    local name=$1 pattern=$2 rw=$3 size=$4 offset=${5:-0}
    printf '%s\0' fio --name="$name" --filename="$DAEMON_PATH" --direct=1 --ioengine=sync \
        --bs=4096 --iodepth=1 --numjobs=1 --rw="$rw" --offset="$offset" --size="$size" \
        --verify=md5 --verify_pattern="$pattern" --verify_fatal=1 --verify_dump=0 \
        --randrepeat=1 --randseed=74703 --end_fsync=1
}

run_fio() {
    validate_recorded_device_identity
    validate_live_device "$DAEMON_PATH" "$DAEMON_ID"
    local -a command=(timeout --signal=TERM --kill-after=2s "${PROCESS_SECONDS}s")
    while IFS= read -r -d '' word; do command+=("$word"); done < <(fio_base "$@")
    shift 5 || true
    command+=("$@")
    run_logged "${command[@]}"
}

write_for_restart() {
    local name=$1 pattern=$2 blocks=$3
    run_fio "$name" "$pattern" write "$((blocks * BLOCK_BYTES))" 0 --do_verify=0 --fsync="$blocks"
}

verify_after_restart() {
    local name=$1 pattern=$2 blocks=$3
    run_fio "$name" "$pattern" write "$((blocks * BLOCK_BYTES))" 0 --verify_only=1
}

format_fresh() {
    [[ ! -e $BACKING ]]
    run_logged "$BINARY" format "$BACKING" "$BACKING_BYTES" "$VOLUME_BYTES"
    validate_owned_backing "$BACKING" "$OWNED_DIR"
    [[ $(stat -Lc '%s' -- "$BACKING") -eq BACKING_BYTES ]]
}

finish_scenario() {
    if [[ -n ${DAEMON_PID:-} ]]; then
        stop_daemon TERM zero
    fi
    wait_for_device_absent "$DAEMON_ID"
    DEVICE_VALIDATED=0
    validate_owned_backing "$BACKING" "$OWNED_DIR"
    rm -f -- "$BACKING"
}

prove_lock_contention() {
    local out=$OWNED_DIR/second.out err=$OWNED_DIR/second.err status
    log_command "$BINARY" serve "$BACKING" -1
    "$BINARY" serve "$BACKING" -1 >"$out" 2>"$err" &
    SECOND_PID=$!
    set +e
    wait_pid_bounded "$SECOND_PID" "$PROCESS_SECONDS"
    status=$?
    set -e
    if [[ $status -eq 124 ]]; then
        kill -TERM "$SECOND_PID" 2>/dev/null || true
        set +e
        wait_pid_bounded "$SECOND_PID" 2 >/dev/null 2>&1
        local term_status=$?
        set -e
        if [[ $term_status -eq 124 ]]; then
            kill -KILL "$SECOND_PID" 2>/dev/null || true
            set +e
            wait_pid_bounded "$SECOND_PID" 2 >/dev/null 2>&1
            term_status=$?
            set -e
        fi
        [[ $term_status -ne 124 ]] && SECOND_PID=
    else
        SECOND_PID=
    fi
    { cat "$out"; cat "$err"; } >>"$EVIDENCE"
    log "second daemon result: exit=$status"
    [[ $status -ne 0 && $status -ne 124 ]]
    grep -Fq 'backing file is already locked' "$err"
}

scenario_sequential() {
    start_daemon
    run_fio sequential 0x13579bdf write $((16 * BLOCK_BYTES)) 0 --do_verify=1 --fsync=16
}

scenario_random() {
    start_daemon
    run_fio random 0x2468ace0 randwrite $((16 * BLOCK_BYTES)) 0 --do_verify=1 --fsync=16
}

scenario_overwrite() {
    local pattern
    start_daemon
    for pattern in 0x01010101 0x02020202 0x03030303 0x04040404 \
        0x05050505 0x06060606 0x07070707 0x08080808; do
        run_fio overwrite "$pattern" write "$BLOCK_BYTES" 0 --do_verify=1 --fsync=1
    done
}

scenario_graceful_restart() {
    start_daemon
    write_for_restart graceful 0x11223344 8
    stop_daemon TERM zero
    wait_for_device_absent "$DAEMON_ID"
    DEVICE_VALIDATED=0
    start_daemon
    verify_after_restart graceful 0x11223344 8
}

scenario_sigkill_restart() {
    start_daemon
    write_for_restart sigkill 0x55667788 8
    stop_daemon KILL killed
    remove_after_kill
    start_daemon
    verify_after_restart sigkill 0x55667788 8
}

scenario_close_failpoint() {
    start_daemon "$FAILPOINT"
    write_for_restart close-failpoint 0x99aabbcc 8
    log_command kill -TERM "$DAEMON_PID"
    kill -TERM "$DAEMON_PID"
    wait_for_failpoint "$FAILPOINT"
    stop_daemon KILL killed
    remove_after_kill
    start_daemon
    verify_after_restart close-failpoint 0x99aabbcc 8
}

scenario_exhaustion() {
    local output=$OWNED_DIR/fio-exhaustion.out status
    start_daemon
    validate_recorded_device_identity
    validate_live_device "$DAEMON_PATH" "$DAEMON_ID"
    local -a command=(timeout --signal=TERM --kill-after=2s "${PROCESS_SECONDS}s")
    while IFS= read -r -d '' word; do command+=("$word"); done \
        < <(fio_base exhaustion 0xdeadbeef write $((33 * BLOCK_BYTES)) 0)
    command+=(--do_verify=0 --fsync=32 --output-format=json)
    log_command "${command[@]}"
    set +e
    "${command[@]}" >"$output" 2>&1
    status=$?
    set -e
    cat "$output" | tee -a "$EVIDENCE"
    log "result: exit=$status (expected fio errno 28)"
    [[ $status -eq 1 ]]
    grep -Eq '"error"[[:space:]]*:[[:space:]]*28' "$output"
    grep -Eq '"io_bytes"[[:space:]]*:[[:space:]]*131072' "$output"
    stop_daemon TERM zero
    wait_for_device_absent "$DAEMON_ID"
    DEVICE_VALIDATED=0
    start_daemon
    verify_after_restart exhaustion 0xdeadbeef 32
}

cleanup() {
    local status=$? safe_to_remove=1
    trap - EXIT INT TERM HUP
    if [[ ${SECOND_PID:-} =~ ^[1-9][0-9]*$ ]]; then
        kill -TERM "$SECOND_PID" 2>/dev/null || true
        set +e
        wait_pid_bounded "$SECOND_PID" 2 >/dev/null 2>&1
        local child_status=$?
        set -e
        if [[ $child_status -eq 124 ]]; then
            kill -KILL "$SECOND_PID" 2>/dev/null || true
            set +e
            wait_pid_bounded "$SECOND_PID" 2 >/dev/null 2>&1
            child_status=$?
            set -e
        fi
        [[ $child_status -eq 124 ]] && safe_to_remove=0
    fi
    if [[ ${DAEMON_PID:-} =~ ^[1-9][0-9]*$ ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null || true
        set +e
        wait_pid_bounded "$DAEMON_PID" 2 >/dev/null 2>&1
        child_status=$?
        set -e
        if [[ $child_status -eq 124 ]]; then
            kill -KILL "$DAEMON_PID" 2>/dev/null || true
            set +e
            wait_pid_bounded "$DAEMON_PID" 2 >/dev/null 2>&1
            child_status=$?
            set -e
        fi
        [[ $child_status -eq 124 ]] && safe_to_remove=0
    fi
    record_daemon_output 2>/dev/null || true
    delete_recorded_device 2>/dev/null || safe_to_remove=0
    [[ $STARTUP_PENDING -eq 1 ]] && safe_to_remove=0
    if [[ ${DAEMON_ID:-} =~ ^(0|[1-9][0-9]*)$ ]] && device_exists "$DAEMON_ID"; then
        safe_to_remove=0
    fi
    if [[ -n ${OWNED_DIR:-} && $safe_to_remove -eq 1 ]]; then
        remove_owned_dir "$OWNED_DIR" "$OWNED_DIR_ID" || {
            printf 'refusing to remove unvalidated temporary directory: %s\n' "$OWNED_DIR" >&2
            status=1
        }
    elif [[ -n ${OWNED_DIR:-} ]]; then
        printf 'preserving backing because daemon/device cleanup was not proven: %s\n' "$OWNED_DIR" >&2
        status=1
    fi
    if [[ -n ${EVIDENCE:-} ]]; then
        printf 'final result: exit=%s\n' "$status" >>"$EVIDENCE"
        printf 'evidence: %s\n' "$EVIDENCE"
    fi
    exit "$status"
}

preflight_live() {
    local timeout_version
    [[ $(uname -s) == Linux ]] || { printf 'run requires Linux\n' >&2; return 1; }
    command -v timeout >/dev/null || { printf 'GNU timeout is required\n' >&2; return 1; }
    timeout_version=$(timeout --version 2>/dev/null) || true
    [[ $timeout_version == *'GNU coreutils'* ]] || {
        printf 'GNU timeout is required\n' >&2; return 1;
    }
    command -v fio >/dev/null || { printf 'fio is required\n' >&2; return 1; }
    command -v cargo >/dev/null || { printf 'cargo is required\n' >&2; return 1; }
    [[ -c /dev/ublk-control && -r /dev/ublk-control && -w /dev/ublk-control ]] || {
        printf '/dev/ublk-control must exist and be readable/writable; no resources created\n' >&2
        return 1
    }
}

run_live() {
    local repo commit timestamp scenario repetition
    preflight_live
    repo=$(cd "$(dirname "$0")/.." && pwd -P)
    cd "$repo"
    mkdir -p evidence
    timestamp=$(date -u +%Y%m%dT%H%M%SZ)
    commit=$(git rev-parse HEAD)
    EVIDENCE=$repo/evidence/test-ublk-$timestamp-$commit.log
    : >"$EVIDENCE"
    log "commit: $commit"
    log "kernel: $(uname -srvm)"
    log 'backing type: fresh create-new regular file; 64 logical 4-KiB blocks; 32 two-block records'
    BINARY=$repo/rust/target/debug/block-storage-ublk
    run_logged timeout --signal=TERM --kill-after=5s "${BUILD_SECONDS}s" \
        cargo build --manifest-path "$repo/rust/Cargo.toml" --features test-failpoints --bin block-storage-ublk
    trap cleanup EXIT INT TERM HUP
    new_owned_dir
    BACKING=$OWNED_DIR/backing.img

    local -a scenarios=(sequential random overwrite graceful_restart sigkill_restart close_failpoint exhaustion)
    for scenario in "${scenarios[@]}"; do
        for ((repetition = 1; repetition <= REPETITIONS; repetition++)); do
            log "=== scenario=$scenario repetition=$repetition/$REPETITIONS ==="
            format_fresh
            "scenario_$scenario"
            if [[ $scenario == sequential && $repetition -eq 1 ]]; then
                prove_lock_contention
            fi
            finish_scenario
            log "PASS scenario=$scenario repetition=$repetition"
        done
    done
    log 'PASS all regular-file ublk/fio scenarios'
}

expect_usage_error() {
    local status
    set +e
    "$@" >/dev/null 2>&1
    status=$?
    set -e
    [[ $status -eq 2 ]]
}

self_test() {
    local root dir id backing
    [[ $(geometry_values 64) == '6 70' ]]
    [[ $BACKING_BYTES -eq 286720 && $EXPECTED_SECTORS -eq 512 ]]
    [[ $(parse_device_path /dev/ublkb0) == 0 ]]
    [[ $(parse_device_path /dev/ublkb42) == 42 ]]
    ! parse_device_path /dev/ublkb01 >/dev/null
    ! parse_device_path /dev/sda >/dev/null
    ! parse_device_path '/dev/ublkb1 extra' >/dev/null

    root=$(mktemp -d)
    trap 'rm -rf -- "$root"' RETURN
    mkdir -p "$root/sys/block/ublkb7/queue"
    printf '4096\n' >"$root/sys/block/ublkb7/queue/logical_block_size"
    printf '512\n' >"$root/sys/block/ublkb7/size"
    validate_sys_geometry "$root/sys" 7
    printf '4096\n' >"$root/sys/block/ublkb7/size"
    ! validate_sys_geometry "$root/sys" 7

    dir=$(mktemp -d "$root/my-block-storage-ublk.XXXXXX")
    dir=$(realpath -e -- "$dir")
    id=$(stat -Lc '%d:%i' -- "$dir")
    printf '%s\n' "$id" >"$dir/.owned-by-test-ublk"
    backing=$dir/backing.img
    (set -o noclobber; : >"$backing")
    validate_owned_dir "$dir" "$id"
    validate_owned_backing "$backing" "$dir"
    ! remove_owned_dir "$root" "$(stat -Lc '%d:%i' -- "$root")"
    printf 'wrong\n' >"$dir/.owned-by-test-ublk"
    ! remove_owned_dir "$dir" "$id"
    printf '%s\n' "$id" >"$dir/.owned-by-test-ublk"
    remove_owned_dir "$dir" "$id"
    [[ ! -e $dir ]]
    rm -rf -- "$root"
    trap - RETURN

    expect_usage_error "$0"
    expect_usage_error "$0" run extra
    expect_usage_error "$0" --backing /tmp/nope
    printf 'PASS test-ublk self-test\n'
}

[[ $# -eq 1 ]] || usage
case $1 in
    run) run_live ;;
    self-test) self_test ;;
    *) usage ;;
esac
