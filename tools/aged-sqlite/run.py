#!/usr/bin/env python3
"""Aged SQLite A/F benchmark on a block-storage-ublk device.

Formats a backing, serves it as /dev/ublkbN, puts ext4 or XFS on it, loads a SQLite database to a
fixed fill and then runs YCSB workload A or F in windows until the endurance stop rule ends the
run. It records throughput, latency, write amplification and `blkctl info` samples, and
optionally a perf profile of the daemon. docs/BENCHMARKING.md describes the procedure and gates.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import secrets
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path

import metrics
from metrics import GIB, MIB, StopRule, Window

ROOT = Path(__file__).resolve().parents[2]
MOUNT_HELPER = "/usr/local/libexec/block-storage-mount"
HELPER_PROTOCOL = "protocol=4"
GO_YCSB_DEFAULT = ROOT / "tools/build/go-ycsb-8633137/bin/go-ycsb"
SQLITE_DEFAULT = ROOT / "tools/build/sqlite-3.51.3/build/sqlite3"
WORKLOADS = ROOT / "tools/go-ycsb/workloads"
# go-ycsb and the sqlite3 shell both link the lab build of this version (tools/sqlite/setup.sh).
SQLITE_VERSION = "3.51.3"
READINESS = re.compile(r"^/dev/ublkb(\d+)\n$")
# The helper's bench allowlist (rust/src/bin/fs_support/mod.rs) fixes the device sizes it mounts.
BENCH_SIZES_GIB = (4, 8, 16, 32, 64)
COMMAND_SECONDS = 3600
DAEMON_READY_SECONDS = 60
DAEMON_STOP_SECONDS = 300
POLL_SECONDS = 0.05
# Windows beyond the maximum multiple allow for a calibration that overestimates bytes per operation.
MAX_WINDOWS_PER_MULTIPLE = 3
NOTE_LOCK = threading.Lock()
# The client-visible write of each workload. In F, UPDATE is the inner write of READ_MODIFY_WRITE.
WRITE_OPERATION = {"a": "UPDATE", "f": "READ_MODIFY_WRITE"}


class Failure(Exception):
    """A gate or a step failed. The message names the step."""


@dataclass
class Config:
    filesystem: str
    workload: str
    size_gib: int
    backing: Path | None
    backing_gib: int | None
    erase_backing_device: bool
    min_logical_fill: int
    client_fill: float
    records: int | None
    calibration_operations: int
    stop: StopRule
    sample_seconds: float
    max_hours: float
    seed: int
    daemon: Path
    blkctl: Path
    go_ycsb: Path
    sqlite3: Path
    daemon_args: list[str]
    ycsb_properties: list[str]
    perf: bool
    perf_frequency: int
    keep_latency_csv: bool
    evidence: Path


@dataclass
class Phase:
    name: str
    operations: int
    seconds: float
    operations_per_second: float
    device_write_bytes: int
    backing_write_bytes: int
    write_amplification: float
    latency: dict[str, dict[str, int]]
    discards: int


@dataclass
class Daemon:
    process: subprocess.Popen
    device: Path
    stat_path: Path
    stderr_path: Path


@dataclass
class Bench:
    config: Config
    owned: Path
    backing: Path
    backing_kind: str
    created_backing: bool
    ledger: Path
    mount: Path
    database: Path
    log: object
    daemon: Daemon | None = None
    mounted: bool = False
    phases: list[Phase] = field(default_factory=list)
    gates: dict[str, bool] = field(default_factory=dict)
    facts: dict[str, object] = field(default_factory=dict)
    deadline: float = 0.0


# ---------------------------------------------------------------- arguments


def parse_args(argv: list[str]) -> Config:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fs", dest="filesystem", choices=("ext4", "xfs"), default="ext4")
    parser.add_argument("--workload", choices=("a", "f"), default="a")
    parser.add_argument("--size-gib", type=int, choices=BENCH_SIZES_GIB, default=8,
                        help="logical size of the ublk device")
    parser.add_argument("--backing", type=Path,
                        help="backing path: a new regular file (default: in the owned /tmp directory) "
                             "or an existing block device")
    parser.add_argument("--backing-gib", type=int,
                        help="regular-file backing size; default: the logical size")
    parser.add_argument("--erase-backing-device", action="store_true",
                        help="required with a block-device backing: it is formatted in place")
    parser.add_argument("--min-logical-fill", type=int, default=90,
                        help="block-storage-ublk format --min-logical-fill-factor")
    parser.add_argument("--client-fill", type=float, default=0.75,
                        help="fraction of the logical volume the database fills")
    parser.add_argument("--records", type=int, help="override the computed record count")
    parser.add_argument("--calibration-operations", type=int, default=20_000)
    parser.add_argument("--min-multiple", type=float, default=3.0)
    parser.add_argument("--max-multiple", type=float, default=10.0)
    parser.add_argument("--steady-tolerance", type=float, default=0.05)
    parser.add_argument("--sample-seconds", type=float, default=30.0)
    parser.add_argument("--max-hours", type=float, default=6.0)
    parser.add_argument("--seed", type=int, default=123)
    parser.add_argument("--daemon", type=Path, default=ROOT / "rust/target/release/block-storage-ublk")
    parser.add_argument("--blkctl", type=Path, default=ROOT / "rust/target/release/blkctl")
    parser.add_argument("--go-ycsb", type=Path, default=GO_YCSB_DEFAULT)
    parser.add_argument("--sqlite3", type=Path, default=SQLITE_DEFAULT)
    parser.add_argument("--daemon-arg", dest="daemon_args", action="append", default=[],
                        help="extra `serve` argument, repeatable, e.g. --daemon-arg=--cleaner-bytes=...")
    parser.add_argument("--ycsb", dest="ycsb_properties", action="append", default=[],
                        help="extra go-ycsb property NAME=VALUE, repeatable")
    parser.add_argument("--perf", action="store_true", help="perf record the daemon during the run phases")
    parser.add_argument("--perf-frequency", type=int, default=499)
    parser.add_argument("--keep-latency-csv", action="store_true",
                        help="keep go-ycsb's per-operation csv next to the per-second series")
    parser.add_argument("--evidence", type=Path,
                        default=ROOT / "evidence" / f"aged-sqlite-{time.time_ns()}")
    args = parser.parse_args(argv)
    daemon_args = [part for value in args.daemon_args for part in value.split("=", 1)]
    return Config(
        filesystem=args.filesystem, workload=args.workload, size_gib=args.size_gib,
        backing=args.backing, backing_gib=args.backing_gib,
        erase_backing_device=args.erase_backing_device, min_logical_fill=args.min_logical_fill,
        client_fill=args.client_fill, records=args.records,
        calibration_operations=args.calibration_operations,
        stop=StopRule(args.min_multiple, args.max_multiple, args.steady_tolerance),
        sample_seconds=args.sample_seconds, max_hours=args.max_hours, seed=args.seed,
        daemon=args.daemon, blkctl=args.blkctl, go_ycsb=args.go_ycsb, sqlite3=args.sqlite3,
        daemon_args=daemon_args, ycsb_properties=args.ycsb_properties, perf=args.perf,
        perf_frequency=args.perf_frequency, keep_latency_csv=args.keep_latency_csv,
        evidence=args.evidence,
    )


# ---------------------------------------------------------------- commands and evidence


def note(bench: Bench, text: str) -> None:
    line = f"{time.strftime('%H:%M:%S')} {text}"
    with NOTE_LOCK:  # the sampler thread notes failures too
        print(line, flush=True)
        bench.log.write(line + "\n")
        bench.log.flush()


def run(bench: Bench, label: str, argv: list[str], *, check: bool = True,
        env: dict[str, str] | None = None, timeout: float = COMMAND_SECONDS) -> subprocess.CompletedProcess:
    """Runs one bounded command and keeps its output as evidence."""
    note(bench, f"run {label}: {shlex.join(str(part) for part in argv)}")
    output = bench.config.evidence / f"{label}.out"
    with open(output, "w") as sink:
        try:
            result = subprocess.run([str(part) for part in argv], stdout=sink, stderr=subprocess.STDOUT,
                                    env=env, timeout=min(timeout, remaining(bench)), check=False)
        except subprocess.TimeoutExpired as error:
            raise Failure(f"{label} exceeded its deadline") from error
    result.stdout = output.read_text(errors="replace")
    if check and result.returncode != 0:
        tail = "\n".join(result.stdout.splitlines()[-20:])
        raise Failure(f"{label} exited with {result.returncode}:\n{tail}")
    return result


def remaining(bench: Bench) -> float:
    left = bench.deadline - time.monotonic()
    if left <= 0:
        raise Failure("the run exceeded --max-hours")
    return left


def helper(bench: Bench, action: str) -> None:
    device_id = bench.daemon.device.name.removeprefix("ublkb")
    run(bench, f"helper-{action}", ["sudo", "-n", "--", MOUNT_HELPER, action, device_id, bench.owned.name])


# ---------------------------------------------------------------- setup


def preflight(bench: Bench) -> None:
    config = bench.config
    for tool in (config.daemon, config.blkctl, config.go_ycsb, config.sqlite3):
        if not os.access(tool, os.X_OK):
            raise Failure(f"missing executable {tool}; run make bench-aged-sqlite-setup")
    if not os.access("/dev/ublk-control", os.R_OK | os.W_OK):
        raise Failure("no read/write access to /dev/ublk-control; see README normal-user ublk setup")
    check = run(bench, "helper-check", ["sudo", "-n", "--", MOUNT_HELPER, "check"], check=False)
    if check.returncode != 0 or HELPER_PROTOCOL not in check.stdout:
        raise Failure(f"mount helper {HELPER_PROTOCOL} is not installed; run "
                      "sudo bash scripts/install-ext4-helper.sh after make build-ext4")
    if config.filesystem == "xfs" and not shutil.which("/usr/sbin/xfs_repair"):
        raise Failure("xfs_repair is missing")
    if config.perf and not shutil.which("perf"):
        raise Failure("perf is missing")
    if config.perf and int(Path("/proc/sys/kernel/perf_event_paranoid").read_text()) > 1:
        raise Failure("perf needs kernel.perf_event_paranoid <= 1 to record the daemon with kernel stacks")
    if config.perf and Path("/proc/sys/kernel/kptr_restrict").read_text().strip() != "0":
        note(bench, "warning: kernel.kptr_restrict != 0, so perf can't name kernel symbols")
    check_backing(bench)


def check_backing(bench: Bench) -> None:
    config = bench.config
    if bench.backing_kind == "block-device":
        if not config.erase_backing_device:
            raise Failure(f"{bench.backing} is a block device; pass --erase-backing-device to format it")
        if not os.access(bench.backing, os.R_OK | os.W_OK):
            raise Failure(f"no read/write access to {bench.backing}")
        return
    if bench.backing.exists():
        raise Failure(f"{bench.backing} exists; the benchmark formats only a new file or a block device")
    need = backing_bytes(config) + GIB
    free = shutil.disk_usage(bench.backing.parent).free
    if free < need:
        raise Failure(f"{bench.backing.parent} has {free // MIB} MiB free, needs {need // MIB} MiB")


def backing_bytes(config: Config) -> int:
    return (config.backing_gib or config.size_gib) * GIB


def logical_bytes(config: Config) -> int:
    return config.size_gib * GIB


def format_backing(bench: Bench) -> None:
    config = bench.config
    argv = [config.daemon, "format", bench.backing, "--size-mib", str(logical_bytes(config) // MIB),
            "--min-logical-fill-factor", str(config.min_logical_fill)]
    if bench.backing_kind == "block-device":
        argv.append("--block-device")
    else:
        argv += ["--prealloc-mib", str(backing_bytes(config) // MIB)]
    metrics.create_ledger(bench.ledger)
    result = run(bench, "format", argv, env=daemon_env(bench))
    bench.created_backing = bench.backing_kind == "regular-file"
    bench.facts["format"] = metrics.parse_daemon_events(result.stdout).get("format", {})
    bench.facts["backing_bytes"] = backing_size(bench)


def backing_size(bench: Bench) -> int:
    with open(bench.backing, "rb") as backing:
        return backing.seek(0, os.SEEK_END)


def daemon_env(bench: Bench) -> dict[str, str]:
    env = dict(os.environ)
    env["BLOCK_STORAGE_IO_LEDGER"] = str(bench.ledger)
    env["BLOCK_STORAGE_LOG"] = "info"
    env.pop("BLOCK_STORAGE_TEST_FAILPOINT", None)
    return env


def start_daemon(bench: Bench, label: str) -> None:
    config = bench.config
    stdout_path = config.evidence / f"daemon-{label}.stdout"
    stderr_path = config.evidence / f"daemon-{label}.stderr"
    argv = [str(config.daemon), "serve", str(bench.backing), "-1", *config.daemon_args]
    note(bench, f"start daemon {label}: {shlex.join(argv)}")
    with open(stdout_path, "w") as stdout, open(stderr_path, "w") as stderr:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                   env=daemon_env(bench), start_new_session=True)
    device = wait_ready(process, stdout_path)
    bench.daemon = Daemon(process, device, Path("/sys/block") / device.name / "stat", stderr_path)
    note(bench, f"daemon pid={process.pid} device={device}")
    check_geometry(bench)


def wait_ready(process: subprocess.Popen, stdout_path: Path) -> Path:
    deadline = time.monotonic() + DAEMON_READY_SECONDS
    while time.monotonic() < deadline:  # bounded by the readiness deadline
        if process.poll() is not None:
            raise Failure(f"daemon exited with {process.returncode} before readiness")
        match = READINESS.match(stdout_path.read_text())
        if match and os.access(match.group(0).strip(), os.R_OK | os.W_OK):
            return Path(match.group(0).strip())
        time.sleep(POLL_SECONDS)
    raise Failure("daemon readiness timed out")


def check_geometry(bench: Bench) -> None:
    queue = Path("/sys/block") / bench.daemon.device.name
    size_bytes = int((queue / "size").read_text()) * metrics.SECTOR_BYTES
    block = int((queue / "queue/logical_block_size").read_text())
    if size_bytes != logical_bytes(bench.config) or block != 4096:
        raise Failure(f"device geometry {size_bytes} bytes / {block} B blocks does not match the request")


def make_filesystem(bench: Bench) -> None:
    if bench.config.filesystem == "xfs":
        helper(bench, "bench-format-xfs")
        return
    # Initialize inode tables and the journal now: ext4's lazy init would write them in the
    # background during the load and blur its numbers.
    run(bench, "mkfs-ext4", ["mkfs.ext4", "-q", "-E", "lazy_itable_init=0,lazy_journal_init=0",
                             bench.daemon.device])


def mount(bench: Bench) -> None:
    helper(bench, "bench-mount" if bench.config.filesystem == "ext4" else "bench-mount-xfs")
    bench.mounted = True
    options = mount_options(bench.mount)
    note(bench, f"mounted {bench.mount}: {options}")
    if "discard" not in options.split(","):
        raise Failure(f"mount lacks the discard option: {options}")


def unmount(bench: Bench) -> None:
    helper(bench, "bench-unmount" if bench.config.filesystem == "ext4" else "bench-unmount-xfs")
    bench.mounted = False


def mount_options(target: Path) -> str:
    """Mount and superblock options of `target` from /proc/self/mountinfo."""
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split()
        if fields[4] == str(target):
            separator = fields.index("-")
            return f"{fields[5]},{fields[separator + 3]}"
    raise Failure(f"{target} is not mounted")


def stop_daemon(bench: Bench, label: str) -> dict[str, dict[str, str]]:
    daemon = bench.daemon
    note(bench, f"stop daemon pid={daemon.process.pid}")
    daemon.process.send_signal(signal.SIGTERM)
    try:
        code = daemon.process.wait(timeout=DAEMON_STOP_SECONDS)
    except subprocess.TimeoutExpired as error:
        raise Failure("daemon did not stop after SIGTERM") from error
    bench.daemon = None
    events = metrics.parse_daemon_events(daemon.stderr_path.read_text(errors="replace"))
    bench.facts[f"daemon_{label}"] = events
    if code != 0:
        raise Failure(f"daemon exited with {code}; see {daemon.stderr_path}")
    return events


# ---------------------------------------------------------------- workload


def ycsb(bench: Bench, label: str, command: str, properties: dict[str, object]) -> metrics.LatencySummary:
    config = bench.config
    latency_csv = config.evidence / f"{label}-latency.csv"
    base = {
        "sqlite.db": bench.database, "sqlite.journalmode": "WAL", "fieldlength": 4000,
        "dataintegrity": "true", "silence": "false", "measurementtype": "csv",
        "measurement.output_file": latency_csv, **properties,
    }
    argv = [config.go_ycsb, command, "sqlite", "-P", WORKLOADS / "common.properties",
            "-P", WORKLOADS / f"{config.workload}.properties"]
    for name, value in base.items():
        argv += ["-p", f"{name}={value}"]
    for extra in config.ycsb_properties:
        argv += ["-p", extra]
    result = run(bench, label, argv)
    check_client_output(label, result.stdout)
    summary = metrics.summarize_latency(latency_csv, config.evidence / f"{label}-series.csv")
    if not config.keep_latency_csv:
        latency_csv.unlink()
    return summary


def check_client_output(label: str, output: str) -> None:
    problem = metrics.client_output_problem(output, SQLITE_VERSION)
    if problem:
        raise Failure(f"{label}: {problem}; see {label}.out")


def counters(bench: Bench) -> tuple[metrics.DeviceStat, metrics.LedgerSnapshot]:
    return (metrics.parse_device_stat(bench.daemon.stat_path.read_text()),
            metrics.read_ledger(bench.ledger))


def measured_phase(bench: Bench, name: str, operations: int, seed: int) -> Phase:
    if bench.daemon.process.poll() is not None:
        raise Failure(f"daemon exited with {bench.daemon.process.returncode} before {name}")
    device_before, ledger_before = counters(bench)
    summary = ycsb(bench, name, "run", {"recordcount": bench.facts["records"],
                                        "operationcount": operations, "workload.seed": seed})
    device_after, ledger_after = counters(bench)
    device_bytes = device_after.write_bytes - device_before.write_bytes
    backing = ledger_after.write_completed_bytes - ledger_before.write_completed_bytes
    phase = Phase(
        name=name, operations=operations, seconds=round(summary.seconds, 3),
        operations_per_second=round(operations / summary.seconds, 1) if summary.seconds else 0.0,
        device_write_bytes=device_bytes, backing_write_bytes=backing,
        write_amplification=round(backing / device_bytes, 4) if device_bytes else 0.0,
        latency=summary.per_operation,
        discards=device_after.discard_requests - device_before.discard_requests,
    )
    bench.phases.append(phase)
    note(bench, f"{name}: {phase.operations} ops {phase.operations_per_second} ops/s "
                f"device={device_bytes // MIB} MiB backing={backing // MIB} MiB "
                f"WA={phase.write_amplification} discards={phase.discards}")
    return phase


def load(bench: Bench) -> None:
    config = bench.config
    records = config.records or metrics.record_count(logical_bytes(config), config.client_fill)
    bench.facts["records"] = records
    device_before, ledger_before = counters(bench)
    summary = ycsb(bench, "load", "load", {"recordcount": records, "batch.size": 100,
                                           "workload.seed": config.seed})
    device_after, ledger_after = counters(bench)
    bench.facts["load"] = {
        "records": records, "seconds": round(summary.seconds, 3),
        "device_write_bytes": device_after.write_bytes - device_before.write_bytes,
        "backing_write_bytes": ledger_after.write_completed_bytes - ledger_before.write_completed_bytes,
        "filesystem_used_bytes": used_bytes(bench.mount),
    }
    note(bench, f"load: {bench.facts['load']}")


def used_bytes(path: Path) -> int:
    usage = shutil.disk_usage(path)
    return usage.total - usage.free


def age(bench: Bench) -> str:
    """Runs workload windows until the stop rule decides. Returns the reason."""
    config = bench.config
    backing = bench.facts["backing_bytes"]
    calibration = measured_phase(bench, "calibration", config.calibration_operations, config.seed + 1)
    window_ops = metrics.window_operations(backing, calibration.device_write_bytes / calibration.operations)
    bench.facts["window_operations"] = window_ops
    device_bytes, windows = calibration.device_write_bytes, []
    # Bounded: every window writes about one backing size, so the maximum multiple ends the loop first.
    for number in range(1, MAX_WINDOWS_PER_MULTIPLE * math.ceil(config.stop.maximum_multiple) + 1):
        phase = measured_phase(bench, f"window-{number:02d}", window_ops, config.seed + 1 + number)
        device_bytes += phase.device_write_bytes
        windows.append(Window(phase.operations_per_second, phase.write_amplification))
        reason = config.stop.decide(device_bytes, backing, windows)
        if reason:
            return reason
    raise Failure("the window limit ended the run before the stop rule decided")


# ---------------------------------------------------------------- sampling and profiling


class Sampler:
    """Samples device, ledger, daemon and blkctl state every `interval` seconds into samples.jsonl."""

    def __init__(self, bench: Bench):
        self.bench = bench
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.output = open(bench.config.evidence / "samples.jsonl", "a")
        self.sequence = 0
        self.failures = 0

    def __enter__(self) -> Sampler:
        self.thread.start()
        return self

    def __exit__(self, *_exc: object) -> None:
        self.stop_event.set()
        self.thread.join(timeout=120)
        self.output.close()

    def loop(self) -> None:
        while not self.stop_event.wait(self.bench.config.sample_seconds):  # ends when the run ends
            try:
                self.sample()
            except Exception as error:  # a sample is best effort; the phases measure the gates
                self.failures += 1
                note(self.bench, f"sample failed: {error}")

    def sample(self) -> None:
        bench, self.sequence = self.bench, self.sequence + 1
        device, ledger = counters(bench)
        info = subprocess.run([str(bench.config.blkctl), "info", "--blocks", "--color=never",
                               str(bench.backing)], capture_output=True, text=True, timeout=120)
        info_path = bench.config.evidence / "blkctl" / f"info-{self.sequence:05d}.txt"
        info_path.write_text(info.stdout + info.stderr)
        record = {"time": time.time(), "device": asdict(device), "ledger": asdict(ledger),
                  "daemon": process_usage(bench.daemon.process.pid), "filesystem_used": used_bytes(bench.mount),
                  "blkctl_exit": info.returncode, "blkctl": metrics.parse_blkctl_info(info.stdout)}
        self.output.write(json.dumps(record) + "\n")
        self.output.flush()


def process_usage(pid: int) -> dict[str, int]:
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    status = Path(f"/proc/{pid}/status").read_text()
    rss = re.search(r"^VmRSS:\s+(\d+) kB", status, re.MULTILINE)
    threads = re.search(r"^Threads:\s+(\d+)", status, re.MULTILINE)
    return {"cpu_ticks": int(fields[11]) + int(fields[12]), "rss_kib": int(rss.group(1)) if rss else 0,
            "threads": int(threads.group(1)) if threads else 0}


class Profiler:
    """perf record of the daemon with frame-pointer call graphs and kernel stacks."""

    def __init__(self, bench: Bench):
        self.bench = bench
        self.process: subprocess.Popen | None = None
        self.data = bench.config.evidence / "perf.data"

    def __enter__(self) -> Profiler:
        if self.bench.config.perf:
            argv = ["perf", "record", "-F", str(self.bench.config.perf_frequency), "--call-graph", "fp",
                    "-p", str(self.bench.daemon.process.pid), "-o", str(self.data)]
            note(self.bench, f"perf: {shlex.join(argv)}")
            log = open(self.bench.config.evidence / "perf-record.out", "w")
            self.process = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
        return self

    def __exit__(self, *_exc: object) -> None:
        if not self.process:
            return
        self.process.send_signal(signal.SIGINT)
        self.process.wait(timeout=300)
        # A flat profile; open perf.data with perf report or a flame graph tool for call graphs.
        run(self.bench, "perf-report", ["perf", "report", "-i", self.data, "--stdio", "--no-children",
                                        "-g", "none", "--no-inline", "--percent-limit", "0.5",
                                        "--sort", "dso,symbol"], check=False)


# ---------------------------------------------------------------- checks


def verify_database(bench: Bench, label: str) -> None:
    result = run(bench, f"sqlite-{label}", [bench.config.sqlite3, bench.database,
                                            "PRAGMA integrity_check; SELECT count(*) FROM usertable;"])
    lines = result.stdout.split()
    integrity_ok = lines[:1] == ["ok"]
    count_ok = lines[-1:] == [str(bench.facts["records"])]
    bench.gates[f"integrity_check_{label}"] = integrity_ok
    bench.gates[f"record_count_{label}"] = count_ok
    if not (integrity_ok and count_ok):
        raise Failure(f"database check {label}: {result.stdout.strip()[:500]}")


def check_writable(bench: Bench) -> None:
    options = mount_options(bench.mount).split(",")
    bench.gates["no_read_only_remount"] = "ro" not in options
    if "ro" in options:
        raise Failure("the filesystem was remounted read-only")


def check_filesystem(bench: Bench) -> None:
    device = bench.daemon.device
    if bench.config.filesystem == "ext4":
        result = run(bench, "e2fsck", ["e2fsck", "-f", "-n", device], check=False)
    else:
        result = run(bench, "xfs_repair", ["/usr/sbin/xfs_repair", "-n", device], check=False)
    bench.gates["filesystem_check"] = result.returncode == 0
    if result.returncode != 0:
        raise Failure(f"filesystem check exited with {result.returncode}")


def check_backing_offline(bench: Bench) -> None:
    run(bench, "blkctl-info-final", [bench.config.blkctl, "info", "--segments", "--color=never", bench.backing])
    result = run(bench, "blkctl-check", [bench.config.blkctl, "check", "--color=never", bench.backing],
                 check=False)
    # Exit 1 means no errors and a tail that the next open replays (ADR-11).
    bench.gates["blkctl_check"] = result.returncode in (0, 1)
    if not bench.gates["blkctl_check"]:
        raise Failure(f"blkctl check exited with {result.returncode}")


# ---------------------------------------------------------------- lifecycle


def execute(bench: Bench) -> None:
    preflight(bench)
    format_backing(bench)
    start_daemon(bench, "aging")
    make_filesystem(bench)
    mount(bench)
    with Sampler(bench):
        load(bench)
        with Profiler(bench):
            bench.facts["stop_reason"] = age(bench)
    check_writable(bench)
    verify_database(bench, "aged")
    total_discards = sum(phase.discards for phase in bench.phases)
    bench.gates["device_discards"] = total_discards > 0
    unmount(bench)
    check_filesystem(bench)
    stop_daemon(bench, "aging")
    check_backing_offline(bench)
    start_daemon(bench, "restart")
    mount(bench)
    verify_database(bench, "restart")
    unmount(bench)
    stop_daemon(bench, "restart")
    if not bench.gates["device_discards"]:
        raise Failure("the device received no discards")


def teardown(bench: Bench, succeeded: bool) -> None:
    if bench.mounted:
        try:
            unmount(bench)
        except Failure as error:
            note(bench, f"unmount failed; preserving daemon pid={bench.daemon.process.pid}: {error}")
            return
    if bench.daemon:
        try:
            stop_daemon(bench, "teardown")
        except Failure as error:
            note(bench, f"teardown: {error}")
            return
    if succeeded:
        if bench.created_backing and bench.backing.parent != bench.owned:
            bench.backing.unlink()
        shutil.rmtree(bench.owned)
    else:
        note(bench, f"preserved {bench.owned} (backing {bench.backing}); remove it with: rm -rf {bench.owned}")


def write_summary(bench: Bench, result: str) -> None:
    config = bench.config
    report = {"result": result, "config": {k: str(v) for k, v in asdict(config).items()},
              "facts": bench.facts, "gates": bench.gates, "phases": [asdict(p) for p in bench.phases]}
    (config.evidence / "summary.json").write_text(json.dumps(report, indent=2, default=str) + "\n")
    lines = [f"# Aged SQLite {config.workload.upper()} on {config.filesystem}", "",
             f"Result: **{result}**", "",
             f"- device {config.size_gib} GiB, backing {bench.backing_kind} `{bench.backing}`, "
             f"{bench.facts.get('backing_bytes', 0) // MIB} MiB",
             f"- records {bench.facts.get('records')}, stop {bench.facts.get('stop_reason')}",
             f"- load {bench.facts.get('load')}", "",
             "| phase | ops | ops/s | device MiB | backing MiB | WA | discards | p50 us | p99 us | p99.9 us |",
             "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    write_operation = WRITE_OPERATION[config.workload]
    for phase in bench.phases:
        update = phase.latency.get(write_operation, {})
        lines.append(f"| {phase.name} | {phase.operations} | {phase.operations_per_second} | "
                     f"{phase.device_write_bytes // MIB} | {phase.backing_write_bytes // MIB} | "
                     f"{phase.write_amplification} | {phase.discards} | {update.get('p50', '')} | "
                     f"{update.get('p99', '')} | {update.get('p999', '')} |")
    lines += ["", f"Latency columns are {write_operation} in microseconds. "
              "Per-second series are in <phase>-series.csv.", "",
              "| gate | pass |", "|---|---|"]
    lines += [f"| {name} | {'yes' if ok else 'NO'} |" for name, ok in bench.gates.items()]
    (config.evidence / "summary.md").write_text("\n".join(lines) + "\n")


def new_bench(config: Config) -> Bench:
    owned = Path("/tmp") / f"my-block-storage-ublk.{os.getpid()}.{secrets.randbelow(10**9)}"
    owned.mkdir(mode=0o700)
    (owned / "mount").mkdir(mode=0o700)
    backing = config.backing or owned / "backing.img"
    kind = "block-device" if backing.exists() and stat.S_ISBLK(backing.stat().st_mode) else "regular-file"
    config.evidence.mkdir(parents=True)
    (config.evidence / "blkctl").mkdir()
    bench = Bench(config=config, owned=owned, backing=backing, backing_kind=kind, created_backing=False,
                  ledger=owned / "io-ledger", mount=owned / "mount", database=owned / "mount/work/ycsb.db",
                  log=open(config.evidence / "run.log", "a"))
    bench.deadline = time.monotonic() + config.max_hours * 3600
    return bench


def main(argv: list[str]) -> int:
    config = parse_args(argv)
    bench = new_bench(config)
    note(bench, f"evidence: {config.evidence}")
    result = "FAIL"
    try:
        execute(bench)
        result = "PASS"
    except (Failure, OSError, subprocess.SubprocessError) as error:
        note(bench, f"FAIL: {error}")
    finally:
        teardown(bench, result == "PASS")
        write_summary(bench, result)
        note(bench, f"result: {result}; summary: {config.evidence / 'summary.md'}")
    return 0 if result == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
