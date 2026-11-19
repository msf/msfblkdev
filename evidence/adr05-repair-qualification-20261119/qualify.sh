#!/bin/bash
# Run inside the disposable guest, with the clean candidate as the working directory.
set -eu
umask 077
out=$1
ref=77fc16ef00b24b3affdb0521522713746211ffa1
. "$HOME/.cargo/env"
export RUSTUP_TOOLCHAIN=1.97.1 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 PYTHONDONTWRITEBYTECODE=1
export SCRATCH=/mnt/lima-liveb-scratch/probe DEVICE=/dev/vdb1
unset TEST_PER_TEST_SECONDS TEST_SUITE_SECONDS BLOCK_STORAGE_TEST_FAILPOINT
mkdir "$out"
exec >"$out/session.txt" 2>&1
set -x
test "$(git rev-parse HEAD)" = "$ref"
test -z "$(git status --porcelain=v1)"
test "$(findmnt -nro SOURCE,UUID -T "$SCRATCH")" = '/dev/vdb1 8490e987-f9af-45ed-b770-b793a94270cd'
date -u; uname -srmo; id; rustc -vV; cargo -V; lsblk -bo NAME,MAJ:MIN,SIZE,FSTYPE,UUID,MOUNTPOINT; git status --short --branch; df -h / "$SCRATCH" /tmp
# The outer timeout only caps build+test for the gate; lab defaults remain unchanged.
run() {
    local name=$1 limit=$2 start end rc
    shift 2
    start=$(date +%s)
    if timeout -k 12s "$limit" "$@" >"$out/$name.txt" 2>&1; then rc=0; else rc=$?; fi
    end=$(date +%s)
    printf '%s\t%d\t%d\t%d\t%ds\n' "$name" "$rc" "$start" "$end" "$((end-start))" >>"$out/results.tsv"
}
run lint-test 330s make lint test
run dst 240s make test-dst
run fault 360s make test-fault
run live-build 240s cargo build --locked --manifest-path rust/Cargo.toml --features test-failpoints --example adr05-engine --bin block-storage-ublk
if grep -q '^live-build[[:space:]]0[[:space:]]' "$out/results.tsv"; then
    run live-b 90s python3 -B -u tools/adr05-fault-lab/live/checkpoint_b.py "$SCRATCH" "$DEVICE" rust/target/debug "$out/live"
fi
run acceptance 3900s make test-acceptance
run ublk-fio 700s make test-ublk-fio
# Record live resources after the gates; a leftover owned resource requires investigation.
pgrep -af 'block-storage|qemu-storage-daemon' || :
ls -l /sys/class/ublk-char /dev/ublkb* || :
df -h / "$SCRATCH" /tmp
git status --porcelain=v1
cat "$out/results.tsv"
touch "$out/done"
