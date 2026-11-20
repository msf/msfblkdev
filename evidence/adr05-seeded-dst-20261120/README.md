# Seeded backing faults: 32-seed smoke run

Authored by LLM. Tested clean revision `39165a67a91685e865d6c65332fccc9612dff57a` on arm64 Debian, Linux `7.2.12-orbstack-00394-gbecdd6130068`, Rust `1.97.1`. The runner operated on disposable regular files without root. `make lint` and `make test` exited zero. The library tests checked the v1 golden plan, parser bounds, the replay trace, a rejected misdirection and a missed fault. The seed/plan CLI test is ignored unless invoked explicitly.

The [seed 42 plan](seed42.plan) and [observed trace](seed42.trace) matched byte for byte across two fresh seed runs and one replay of the saved plan. SHA-256 was `b3cb1238e832f5f087b517c5d34ecd8eba416e1cb2115ef0998a2fc433378dc7` for each plan and `42f40d7488a1207999f968bf73d654fbbbc345b2fea2501ac001bf14643187d5` for each trace, including its verdict.

A separate sweep of seeds 0 through 31 passed all 128 epochs. To pass an epoch, the runner had to hit the scheduled fault exactly once and verify the logical blocks after production recovery opened a copy. The sweep recorded five fault effects:

| Effect | Epochs |
|---|---:|
| Write rejected with ENOSPC | 36 |
| Zero-byte write completion | 20 |
| Completed write followed by EIO | 20 |
| Rejected sync | 23 |
| Completed sync followed by EIO | 29 |

For example, seed 0 targeted the first Writev with ENOSPC, then the first Writev with post-completion EIO after reopen. Seed 1 targeted the second Writev with ENOSPC, then the fourth Writev with ENOSPC after reopen.

To reproduce from the repository root with a fresh scratch directory:

```sh
set -eu
export PATH="$HOME/.cargo/bin:$PATH" RUSTUP_TOOLCHAIN=1.97.1 CARGO_NET_OFFLINE=true LC_ALL=C
d=$(mktemp -d)
make lint
make test
make test-fault-seed SEED=42 OUT="$d/a"
make test-fault-seed SEED=42 OUT="$d/b"
make test-fault-seed PLAN="$d/a/plan.txt" OUT="$d/c"
for file in plan.txt trace.txt; do
    cmp "$d/a/$file" "$d/b/$file"
    cmp "$d/a/$file" "$d/c/$file"
done
for seed in $(seq 0 31); do
    make test-fault-seed SEED="$seed" OUT="$d/s$seed"
    grep -qx 'PASS epochs=4' "$d/s$seed/trace.txt"
done
```

This run checks a serialized fail-closed engine and a cut that retains completed writes. It does not measure filesystem writeback, power loss, concurrent scheduling or retry policy. See the [lab guide](../../tools/adr05-fault-lab/README.md#how-do-i-replay-a-seed-against-the-engine).
