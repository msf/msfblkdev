#!/usr/bin/env python3
"""Runs aged SQLite variants × workloads × repeats one at a time and writes report.html.

Runs never overlap: two runs would compete for CPU, memory bandwidth and the RAM disk. The order is
shuffled with a fixed seed, so slow drift across a long matrix doesn't land on one variant. Before
each run the runner waits for the host to go idle. A matrix directory can be resumed: runs that
already passed are kept. Set up the RAM-backed LVM layout first with `sudo tools/aged-sqlite/ram-lvm.sh setup`.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import time
from pathlib import Path

import report

ROOT = Path(__file__).resolve().parents[2]
RUN = Path(__file__).resolve().parent / "run.py"
# Measured processes on the Zen 5 cores; blkctl samples on the Zen 5c cores, which have their own L3.
PIN = ["--daemon-cpus", "2,3", "--client-cpus", "0,1", "--sampler-cpus", "4-11"]
HARNESS_CPUS = "4-11"
CLIENT_FILESYSTEM = ["--fs", "xfs"]

# Every variant has the same raw space: one RAW_MIB logical volume from ram-lvm.sh.
VARIANTS = {
    "raw-lv": ("ours: raw LV", ["--backing", "/dev/bench/raw", "--erase-backing-device"]),
    "xfs-lv": ("ours: file on XFS LV", ["--backing", "/mnt/bench/xfs/aged.img", "--backing-mib", "max"]),
    "ext4-lv": ("ours: file on ext4 LV", ["--backing", "/mnt/bench/ext4/aged.img", "--backing-mib", "max"]),
    "thin-xfs": ("baseline: XFS on LVM thin", ["--baseline-mount", "/mnt/bench/thin",
                                               "--baseline-device", "/dev/bench/thin",
                                               "--baseline-medium", "/dev/mapper/bench-pool_tdata"]),
}
IDLE_LOAD = 0.5
IDLE_WAIT_SECONDS = 900
COOLDOWN_SECONDS = 30


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variants", nargs="+", choices=sorted(VARIANTS), default=sorted(VARIANTS))
    parser.add_argument("--workloads", nargs="+", choices=("a", "f"), default=["a"])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--order-seed", type=int, default=20261127)
    parser.add_argument("--out", type=Path, default=ROOT / "evidence" / f"aged-sqlite-matrix-{time.time_ns()}")
    parser.add_argument("--daemon", type=Path, help="daemon binary passed to every run")
    parser.add_argument("--run-arg", dest="run_args", action="append", default=[],
                        help="extra run.py argument for every run, repeatable")
    args = parser.parse_args(argv)
    runs = [(variant, workload, repeat) for variant in args.variants for workload in args.workloads
            for repeat in range(1, args.repeats + 1)]
    random.Random(args.order_seed).shuffle(runs)
    (args.out / "runs").mkdir(parents=True, exist_ok=True)
    plan = {"variants": {key: VARIANTS[key][0] for key in args.variants}, "workloads": args.workloads,
            "repeats": args.repeats, "order_seed": args.order_seed,
            "order": [f"{v}-{w}-r{r}" for v, w, r in runs]}
    (args.out / "matrix.json").write_text(json.dumps(plan, indent=2) + "\n")
    extra = [*(["--daemon", str(args.daemon)] if args.daemon else []), *args.run_args]
    for index, (variant, workload, repeat) in enumerate(runs, 1):
        run_one(args.out, index, len(runs), variant, workload, repeat, extra)
    report.write(args.out)
    print(f"report: {args.out / 'report.html'}")
    return 0


def run_one(out: Path, index: int, total: int, variant: str, workload: str, repeat: int,
            extra: list[str]) -> None:
    name = f"{variant}-{workload}-r{repeat}"
    evidence = out / "runs" / name
    if passed(evidence):
        print(f"[{index}/{total}] {name}: kept the earlier pass")
        return
    if evidence.exists():
        evidence.rename(evidence.with_name(f"{name}.failed-{time.time_ns()}"))
    wait_until_idle()
    # The harness and its helpers (mkfs, sqlite3, blkctl) inherit the Zen 5c cores; the daemon and
    # go-ycsb set their own affinity to the Zen 5 cores.
    argv = ["taskset", "-c", HARNESS_CPUS, sys.executable, "-B", str(RUN), *CLIENT_FILESYSTEM, *PIN,
            "--workload", workload,
            *VARIANTS[variant][1], "--evidence", str(evidence), *extra]
    print(f"[{index}/{total}] {name}: {' '.join(argv[6:])}", flush=True)
    started = time.monotonic()
    with open(out / "runs" / f"{name}.log", "w") as log:
        code = subprocess.run(argv, stdout=log, stderr=subprocess.STDOUT, check=False).returncode
    print(f"[{index}/{total}] {name}: {'PASS' if code == 0 else 'FAIL'} in {time.monotonic() - started:.0f} s",
          flush=True)


def passed(evidence: Path) -> bool:
    summary = evidence / "summary.json"
    return summary.exists() and json.loads(summary.read_text()).get("result") == "PASS"


def wait_until_idle() -> None:
    """Waits for the 1-minute load average to fall below IDLE_LOAD, then a fixed cooldown."""
    deadline = time.monotonic() + IDLE_WAIT_SECONDS
    while os.getloadavg()[0] > IDLE_LOAD and time.monotonic() < deadline:  # bounded by the deadline
        time.sleep(10)
    if os.getloadavg()[0] > IDLE_LOAD:
        print(f"warning: load average {os.getloadavg()[0]:.2f} after {IDLE_WAIT_SECONDS} s; running anyway",
              flush=True)
    time.sleep(COOLDOWN_SECONDS)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
