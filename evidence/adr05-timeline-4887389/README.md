# Fault matrix at `4887389`

Authored by LLM. On 20 November 2026, 35 of 35 focused tests passed on arm64 Linux with Rust 1.97.1: 18 Rust regressions, 15 Python model tests, one 32-seed sweep and one seed-42 replay comparison. The [transcript](focused.txt) records per-test results and the two sweep verdicts. Build progress is omitted. We did not run the eight guest-only cases in the [timeline](../../docs/adr05-report.html#timeline).

The separate `make test` attempt stopped in an ext4 lab fixture with `ENOSPC`. The [failure excerpt](developer-failure.txt) shows the failing test. This was not a scheduled fault. The focused rerun passed; that does not turn the full-suite result into a pass.

To reproduce the focused column, start at the repository root on Linux with direct I/O and `io_uring` on disposable regular files. Use a shell that stops on error:

```sh
set -eu
export RUSTUP_TOOLCHAIN=1.97.1 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
(cd rust
 for group in adr05_seam replay_cursor_skips_intact fsync01 records_retry seeded_dst descriptor_survival; do
     cargo test --locked --lib "$group" -- --test-threads=1
 done)
make test-fault-model
work=$(mktemp -d)
for seed in $(seq 0 31) 42; do
    make test-fault-seed SEED="$seed" OUT="$work/s$seed"
    grep -qx 'PASS epochs=4' "$work/s$seed/trace.txt"
done
make test-fault-seed SEED=42 OUT="$work/repeat"
make test-fault-seed PLAN="$work/s42/plan.txt" OUT="$work/replay"
for file in plan.txt trace.txt; do
    cmp "$work/s42/$file" "$work/repeat/$file"
    cmp "$work/s42/$file" "$work/replay/$file"
done
```

The `seeded_dst` unit-test filter skips the environment-driven runner; the two seed checks invoke it explicitly. These results do not qualify live ublk, checkpoint retry, power loss or daemon enablement.
