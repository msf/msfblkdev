#!/usr/bin/env python3
"""Runs aged-workload variants × benchmarks × repeats one at a time and writes report.html.

Runs never overlap: two runs would compete for CPU, memory bandwidth and the RAM disk. The order is
shuffled with a fixed seed, so slow drift across a long matrix doesn't land on one variant. Before
each run the runner waits for the host to go idle. A matrix directory can be resumed: runs that
already passed are kept. Set up the RAM-backed LVM layout first with `sudo tools/aged-sqlite/ram-lvm.sh setup`.

A benchmark is a client and a workload, such as `sqlite-a` or `fio-uniform`. Filesystem benchmarks
run on filesystem variants and fio on block variants; other pairs are skipped.
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import sys
import time
from pathlib import Path

import report

ROOT = Path(__file__).resolve().parents[2]
RUN = Path(__file__).resolve().parent / "run.py"

# benchmark -> (client, workload, title)
BENCHMARKS = {
    "sqlite-a": ("ycsb-sqlite", "a", "YCSB A on SQLite"),
    "sqlite-f": ("ycsb-sqlite", "f", "YCSB F on SQLite"),
    "rocksdb-a": ("ycsb-rocksdb", "a", "YCSB A on RocksDB"),
    "rocksdb-f": ("ycsb-rocksdb", "f", "YCSB F on RocksDB"),
    "fio-uniform": ("fio", "uniform", "fio 4 KiB random writes, uniform"),
    "fio-zipf": ("fio", "zipf", "fio 4 KiB random writes, Zipf 1.2"),
}
# Shorter names from before the client dimension existed.
ALIASES = {"a": "sqlite-a", "f": "sqlite-f"}

# variant -> (label, kind, run.py arguments). Every variant has the same raw space: one RAW_MIB
# logical volume (or thin pool) from ram-lvm.sh. "filesystem" variants hold a filesystem for the
# client; "block" variants give the client a block device.
VARIANTS = {
    "raw-lv": ("ours: raw LV", "both", ["--backing", "/dev/bench/raw", "--erase-backing-device"]),
    "xfs-lv": ("ours: file on XFS LV", "both", ["--backing", "/mnt/bench/xfs/aged.img", "--backing-mib", "max"]),
    "ext4-lv": ("ours: file on ext4 LV", "both", ["--backing", "/mnt/bench/ext4/aged.img", "--backing-mib", "max"]),
    "thin-xfs": ("baseline: XFS on LVM thin", "filesystem",
                 ["--baseline-mount", "/mnt/bench/thin", "--baseline-device", "/dev/bench/thin",
                  "--baseline-medium", "/dev/mapper/bench-pool_tdata"]),
    "plain-xfs": ("baseline: XFS on a plain LV", "filesystem",
                  ["--baseline-mount", "/mnt/bench/xfs", "--baseline-device", "/dev/bench/xfs",
                   "--baseline-medium", "/dev/bench/xfs"]),
    "thin-raw": ("baseline: LVM thin LV", "block",
                 ["--baseline-device", "/dev/bench/thinfio", "--baseline-medium", "/dev/mapper/bench-pool_tdata"]),
    # The medium is the LV under dm-integrity: it sees the journal, in-place and checksum writes.
    "thin-integ-xfs": ("baseline: XFS on LVM thin on dm-integrity", "filesystem",
                       ["--baseline-mount", "/mnt/bench/thin-integ", "--baseline-device", "/dev/benchi/thin",
                        "--baseline-medium", "/dev/bench/integ"]),
    "thin-integ-raw": ("baseline: LVM thin LV on dm-integrity", "block",
                       ["--baseline-device", "/dev/benchi/thinfio", "--baseline-medium", "/dev/bench/integ"]),
    "plain-raw": ("baseline: plain LV", "block",
                  ["--baseline-device", "/dev/bench/raw", "--baseline-medium", "/dev/bench/raw"]),
}

# Zen 5 cores 0-3 (threads n and n+12 share a core) run what is measured; Zen 5c cores 4-11, on
# their own L3, run the harness and its helpers (mkfs, sqlite3, blkctl samples).
HARNESS_CPUS = "4-11"
DAEMON_CPUS = "2,3"
CLIENT_CPUS = "0,1"            # go-ycsb with SQLite or RocksDB inside, fio
CLIENT_FILESYSTEM = ["--fs", "xfs"]
# The load average is no idle signal: a task in uninterruptible sleep counts as load without using
# a CPU. Idle means the measured cores are below this busy fraction over one sample.
MEASURED_CPUS = (0, 1, 2, 3, 12, 13, 14, 15)
IDLE_BUSY_FRACTION = 0.05
IDLE_SAMPLE_SECONDS = 5
IDLE_WAIT_SECONDS = 900
COOLDOWN_SECONDS = 30
# A run's teardown unmounts, stops the daemon and checks nothing more; it finishes in seconds.
TEARDOWN_SECONDS = 600


def applies(variant: str, benchmark: str) -> bool:
    kind = VARIANTS[variant][1]
    needs = "block" if BENCHMARKS[benchmark][0] == "fio" else "filesystem"
    return kind in ("both", needs)


def pinning() -> list[str]:
    return ["--daemon-cpus", DAEMON_CPUS, "--client-cpus", CLIENT_CPUS, "--sampler-cpus", HARNESS_CPUS]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variants", nargs="+", choices=sorted(VARIANTS), default=["raw-lv", "thin-xfs"])
    parser.add_argument("--benchmarks", "--workloads", dest="benchmarks", nargs="+",
                        choices=sorted([*BENCHMARKS, *ALIASES]), default=["sqlite-a"])
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--order-seed", type=int, default=20261127)
    parser.add_argument("--out", type=Path, default=ROOT / "evidence" / f"aged-sqlite-matrix-{time.time_ns()}")
    parser.add_argument("--daemon", type=Path, help="daemon binary passed to every run")
    parser.add_argument("--run-arg", dest="run_args", action="append", default=[],
                        help="extra run.py argument for every run, repeatable")
    args = parser.parse_args(argv)
    benchmarks = [ALIASES.get(name, name) for name in args.benchmarks]
    runs = [(variant, benchmark, repeat) for variant in args.variants for benchmark in benchmarks
            for repeat in range(1, args.repeats + 1) if applies(variant, benchmark)]
    random.Random(args.order_seed).shuffle(runs)
    (args.out / "runs").mkdir(parents=True, exist_ok=True)
    plan = {"variants": {key: VARIANTS[key][0] for key in args.variants},
            "benchmarks": {key: BENCHMARKS[key][2] for key in benchmarks},
            "repeats": args.repeats, "order_seed": args.order_seed,
            "order": [run_name(*run) for run in runs]}
    (args.out / "matrix.json").write_text(json.dumps(plan, indent=2) + "\n")
    extra = [*(["--daemon", str(args.daemon)] if args.daemon else []), *args.run_args]
    for index, run in enumerate(runs, 1):
        run_one(args.out, index, len(runs), *run, extra)
    report.write(args.out)
    print(f"report: {args.out / 'report.html'}")
    return 0


def run_name(variant: str, benchmark: str, repeat: int) -> str:
    return f"{variant}__{benchmark}__r{repeat}"


def run_one(out: Path, index: int, total: int, variant: str, benchmark: str, repeat: int,
            extra: list[str]) -> None:
    name = run_name(variant, benchmark, repeat)
    evidence = out / "runs" / name
    if passed(evidence):
        print(f"[{index}/{total}] {name}: kept the earlier pass")
        return
    if evidence.exists():
        evidence.rename(evidence.with_name(f"{name}.failed-{time.time_ns()}"))
    wait_until_idle()
    client, workload, _ = BENCHMARKS[benchmark]
    # The harness and its helpers inherit the Zen 5c cores; the daemon and the client set their
    # own affinity to the Zen 5 cores.
    argv = ["taskset", "-c", HARNESS_CPUS, sys.executable, "-B", str(RUN), "--client", client,
            "--workload", workload, *CLIENT_FILESYSTEM, *pinning(), *VARIANTS[variant][2],
            "--evidence", str(evidence), *extra]
    print(f"[{index}/{total}] {name}: {' '.join(argv[6:])}", flush=True)
    started = time.monotonic()
    with open(out / "runs" / f"{name}.log", "w") as log:
        code = wait_for_run(subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT))
    print(f"[{index}/{total}] {name}: {'PASS' if code == 0 else 'FAIL'} in {time.monotonic() - started:.0f} s",
          flush=True)


def wait_for_run(process: subprocess.Popen) -> int:
    """Waits for run.py. On Ctrl-C the terminal sends SIGINT to run.py too, and run.py tears down:
    wait for that instead of killing it, as subprocess.run does after 0.25 s, which leaves the
    daemon running and the filesystem mounted."""
    try:
        return process.wait()
    except KeyboardInterrupt:
        print(f"interrupted: waiting up to {TEARDOWN_SECONDS} s for the run's teardown", flush=True)
        process.wait(timeout=TEARDOWN_SECONDS)
        raise


def passed(evidence: Path) -> bool:
    summary = evidence / "summary.json"
    return summary.exists() and json.loads(summary.read_text()).get("result") == "PASS"


def cpu_times() -> dict[int, tuple[int, int]]:
    """Busy and total jiffies per CPU from /proc/stat; iowait counts as idle."""
    times = {}
    for line in Path("/proc/stat").read_text().splitlines():
        name, *fields = line.split()
        if name.startswith("cpu") and name != "cpu":
            user, nice, system, idle, iowait, irq, softirq, steal = (int(v) for v in fields[:8])
            busy = user + nice + system + irq + softirq + steal
            times[int(name[3:])] = (busy, busy + idle + iowait)
    return times


def busy_fraction(seconds: float) -> float:
    before = cpu_times()
    time.sleep(seconds)
    after = cpu_times()
    busy = sum(after[c][0] - before[c][0] for c in MEASURED_CPUS)
    total = sum(after[c][1] - before[c][1] for c in MEASURED_CPUS)
    return busy / total if total else 0.0


def wait_until_idle() -> None:
    """Waits until the measured cores are idle, then a fixed cooldown."""
    deadline = time.monotonic() + IDLE_WAIT_SECONDS
    busy = busy_fraction(IDLE_SAMPLE_SECONDS)
    while busy > IDLE_BUSY_FRACTION and time.monotonic() < deadline:  # bounded by the deadline
        busy = busy_fraction(IDLE_SAMPLE_SECONDS)
    if busy > IDLE_BUSY_FRACTION:
        print(f"warning: measured CPUs {busy:.0%} busy after {IDLE_WAIT_SECONDS} s; running anyway", flush=True)
    time.sleep(COOLDOWN_SECONDS)


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        print("interrupted; runs that passed are kept: rerun with the same --out to resume", flush=True)
        sys.exit(130)
