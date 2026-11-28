#!/usr/bin/env python3
"""Aged workload benchmark on a block-storage-ublk device or on a baseline stack.

Formats a backing, serves it as /dev/ublkbN and, for filesystem clients, puts ext4 or XFS on it.
A client (clients.py: YCSB on SQLite or RocksDB, or fio) loads data to a
fixed fill and then runs windows of work until the endurance stop rule ends the run. The harness
records throughput, latency, write amplification and `blkctl info` samples, and optionally a perf
profile of the daemon. Baseline mode runs the same client on a stack without our daemon.
docs/BENCHMARKING.md describes the procedure and gates.
"""

from __future__ import annotations

import argparse
import hashlib
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

import clients
import metrics
import observe
from metrics import GIB, MIB, StopRule, Window

ROOT = Path(__file__).resolve().parents[2]
MOUNT_HELPER = "/usr/local/libexec/block-storage-mount"
HELPER_PROTOCOL = "protocol=4"
READINESS = re.compile(r"^/dev/ublkb(\d+)\n$")
# The helper's bench allowlist (rust/src/bin/fs_support/mod.rs) fixes the device sizes it mounts.
BENCH_SIZES_GIB = (4, 8, 16, 32, 64)
COMMAND_SECONDS = 3600
DAEMON_READY_SECONDS = 60
DAEMON_STOP_SECONDS = 300
POLL_SECONDS = 0.05
# Windows beyond the maximum multiple allow for a calibration that overestimates bytes per unit.
MAX_WINDOWS_PER_MULTIPLE = 3
# A file that fills its filesystem leaves this much for the filesystem's own allocations, such as
# extent-tree blocks, as the engine writes into the sparse file.
FILESYSTEM_SLACK_BYTES = 32 * MIB
NOTE_LOCK = threading.Lock()


class Failure(Exception):
    """A gate or a step failed. The message names the step."""


@dataclass
class Config:
    client: str
    workload: str
    filesystem: str
    size_gib: int
    backing: Path | None
    backing_mib: str | None
    erase_backing_device: bool
    min_logical_fill: int
    client_fill: float | None
    calibration_units: int | None
    stop: StopRule
    sample_seconds: float
    max_hours: float
    seed: int
    daemon: Path
    blkctl: Path
    daemon_args: list[str]
    ycsb_properties: list[str]
    perf: bool
    perf_frequency: int
    kallsyms: Path | None
    keep_latency_csv: bool
    daemon_cpus: str | None
    client_cpus: str | None
    sampler_cpus: str | None
    baseline_mount: Path | None
    baseline_device: Path | None
    baseline_medium: Path | None
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
    daemon_cpu_seconds: float | None
    client_cpu_seconds: float
    observation: dict[str, object] | None = None


@dataclass
class Daemon:
    process: subprocess.Popen
    device: Path
    stat_path: Path
    stderr_path: Path


@dataclass
class Bench:
    config: Config
    client: clients.Client
    owned: Path
    backing: Path
    backing_kind: str
    ledger: Path
    mount: Path
    workdir: Path
    log: object
    created_backing: bool = False
    daemon: Daemon | None = None
    mounted: bool = False
    client_running: bool = False
    phases: list[Phase] = field(default_factory=list)
    gates: dict[str, bool] = field(default_factory=dict)
    facts: dict[str, object] = field(default_factory=dict)
    deadline: float = 0.0

    # The methods below are the interface clients.py uses.
    @property
    def baseline(self) -> bool:
        return self.config.baseline_device is not None

    @property
    def logical_bytes(self) -> int:
        return self.config.size_gib * GIB

    @property
    def client_device(self) -> Path:
        return self.config.baseline_device if self.baseline else self.daemon.device

    def run(self, label: str, argv: list, *, check: bool = True, env: dict | None = None):
        return run(self, label, argv, check=check, env=env)

    def run_with_usage(self, label: str, argv: list) -> tuple[str, float]:
        return run_with_usage(self, label, argv)

    def failure(self, message: str) -> Failure:
        return Failure(message)

    def gate(self, name: str, passed: bool, detail: str) -> None:
        self.gates[name] = passed
        if not passed:
            raise Failure(f"gate {name} failed: {detail.strip()[:500]}")

    def pinned_client(self) -> list[str]:
        return pinned(self.config.client_cpus)


# ---------------------------------------------------------------- arguments


def parse_args(argv: list[str]) -> Config:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--client", choices=sorted(clients.CLIENTS), default="ycsb-sqlite")
    parser.add_argument("--workload", default="a", help="a or f for YCSB, uniform or zipf for fio")
    parser.add_argument("--fs", dest="filesystem", choices=("ext4", "xfs"), default="ext4",
                        help="filesystem on our device for filesystem clients")
    parser.add_argument("--size-gib", type=int, choices=BENCH_SIZES_GIB, default=8,
                        help="logical size of the ublk device")
    parser.add_argument("--backing", type=Path,
                        help="backing path: a new regular file (default: in the owned /tmp directory) "
                             "or an existing block device")
    parser.add_argument("--backing-mib",
                        help="regular-file backing size in MiB, or 'max' to fill the free space of the "
                             "filesystem that holds it; default: the logical size")
    parser.add_argument("--erase-backing-device", action="store_true",
                        help="required with a block-device backing: it is formatted in place")
    parser.add_argument("--min-logical-fill", type=int, default=90,
                        help="block-storage-ublk format --min-logical-fill-factor")
    parser.add_argument("--client-fill", type=float,
                        help="fraction of the logical volume the client's data fills; default per client")
    parser.add_argument("--calibration-units", type=int, help="units of the calibration phase; default per client")
    parser.add_argument("--min-multiple", type=float, default=3.0)
    parser.add_argument("--max-multiple", type=float, default=10.0)
    parser.add_argument("--steady-tolerance", type=float, default=0.05)
    parser.add_argument("--sample-seconds", type=float, default=30.0)
    parser.add_argument("--max-hours", type=float, default=6.0)
    parser.add_argument("--seed", type=int, default=123)
    parser.add_argument("--daemon", type=Path, default=ROOT / "rust/target/release/block-storage-ublk")
    parser.add_argument("--blkctl", type=Path, default=ROOT / "rust/target/release/blkctl")
    parser.add_argument("--daemon-arg", dest="daemon_args", action="append", default=[],
                        help="extra `serve` argument, repeatable, e.g. --daemon-arg=--cleaner-bytes=...")
    parser.add_argument("--ycsb", dest="ycsb_properties", action="append", default=[],
                        help="extra go-ycsb property NAME=VALUE, repeatable")
    parser.add_argument("--perf", action="store_true", help="perf record the daemon during the run phases")
    parser.add_argument("--perf-frequency", type=int, default=499)
    parser.add_argument("--kallsyms", type=Path, help="copy of /proc/kallsyms from this boot, made with sudo, so "
                        "perf report names kernel symbols; this kernel hides them from unprivileged users")
    parser.add_argument("--daemon-cpus", help="taskset CPU list for the daemon, e.g. 2,3")
    parser.add_argument("--client-cpus", help="taskset CPU list for the client, e.g. 0,1")
    parser.add_argument("--sampler-cpus", help="taskset CPU list for blkctl samples, away from the measured CPUs")
    parser.add_argument("--baseline-mount", type=Path,
                        help="baseline mode, filesystem clients: an already mounted filesystem to run on")
    parser.add_argument("--baseline-device", type=Path,
                        help="baseline mode: the block device under test, e.g. an LVM thin LV")
    parser.add_argument("--baseline-medium", type=Path,
                        help="baseline mode: the device that stores its data, e.g. the thin pool's tdata, "
                             "for write amplification")
    parser.add_argument("--keep-latency-csv", action="store_true",
                        help="keep go-ycsb's per-operation csv next to the per-second series")
    parser.add_argument("--evidence", type=Path, default=ROOT / "evidence" / f"aged-sqlite-{time.time_ns()}")
    args = parser.parse_args(argv)
    try:
        clients.make(args.client, args.workload)
    except ValueError as error:
        parser.error(str(error))
    daemon_args = [part for value in args.daemon_args for part in value.split("=", 1)]
    return Config(
        client=args.client, workload=args.workload, filesystem=args.filesystem, size_gib=args.size_gib,
        backing=args.backing, backing_mib=args.backing_mib, erase_backing_device=args.erase_backing_device,
        min_logical_fill=args.min_logical_fill, client_fill=args.client_fill,
        calibration_units=args.calibration_units,
        stop=StopRule(args.min_multiple, args.max_multiple, args.steady_tolerance),
        sample_seconds=args.sample_seconds, max_hours=args.max_hours, seed=args.seed,
        daemon=args.daemon, blkctl=args.blkctl, daemon_args=daemon_args, ycsb_properties=args.ycsb_properties,
        perf=args.perf, perf_frequency=args.perf_frequency, kallsyms=args.kallsyms,
        keep_latency_csv=args.keep_latency_csv,
        daemon_cpus=args.daemon_cpus, client_cpus=args.client_cpus,
        sampler_cpus=args.sampler_cpus, baseline_mount=args.baseline_mount,
        baseline_device=args.baseline_device, baseline_medium=args.baseline_medium, evidence=args.evidence,
    )


# ---------------------------------------------------------------- commands and evidence


def note(bench: Bench, text: str) -> None:
    line = f"{time.strftime('%H:%M:%S')} {text}"
    with NOTE_LOCK:  # the sampler thread notes failures too
        print(line, flush=True)
        bench.log.write(line + "\n")
        bench.log.flush()


def run(bench: Bench, label: str, argv: list, *, check: bool = True,
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


def run_with_usage(bench: Bench, label: str, argv: list) -> tuple[str, float]:
    """Runs one bounded command like `run` and also returns its own CPU seconds from wait4."""
    note(bench, f"run {label}: {shlex.join(str(part) for part in argv)}")
    output = bench.config.evidence / f"{label}.out"
    deadline = time.monotonic() + min(COMMAND_SECONDS, remaining(bench))
    with open(output, "w") as sink:
        process = subprocess.Popen([str(part) for part in argv], stdout=sink, stderr=subprocess.STDOUT)
        try:
            while True:  # bounded by the deadline
                pid, status, usage = os.wait4(process.pid, os.WNOHANG)
                if pid:
                    break
                if time.monotonic() > deadline:
                    raise Failure(f"{label} exceeded its deadline")
                time.sleep(POLL_SECONDS)
        except BaseException:
            # A deadline or an interrupt must not leave the client writing to a mount that the
            # teardown then can't unmount.
            process.kill()
            process.wait()
            raise
    process.returncode = os.waitstatus_to_exitcode(status)
    text = output.read_text(errors="replace")
    if process.returncode != 0:
        tail = "\n".join(text.splitlines()[-20:])
        raise Failure(f"{label} exited with {process.returncode}:\n{tail}")
    return text, usage.ru_utime + usage.ru_stime


def remaining(bench: Bench) -> float:
    left = bench.deadline - time.monotonic()
    if left <= 0:
        raise Failure("the run exceeded --max-hours")
    return left


def helper(bench: Bench, action: str) -> None:
    device_id = bench.daemon.device.name.removeprefix("ublkb")
    run(bench, f"helper-{action}", ["sudo", "-n", "--", MOUNT_HELPER, action, device_id, bench.owned.name])


def pinned(cpus: str | None) -> list[str]:
    return ["taskset", "-c", cpus] if cpus else []


# ---------------------------------------------------------------- host state


def environment(bench: Bench) -> dict[str, object]:
    """The host and build state that changes results, recorded with every run."""
    cpufreq = Path("/sys/devices/system/cpu")
    governors = sorted({path.read_text().strip() for path in cpufreq.glob("cpu*/cpufreq/scaling_governor")})
    boost = cpufreq / "cpufreq/boost"
    meminfo = Path("/proc/meminfo").read_text()
    available = re.search(r"^MemAvailable:\s+(\d+) kB", meminfo, re.MULTILINE)
    commit = subprocess.run(["git", "-C", str(ROOT), "describe", "--tags", "--always", "--dirty", "--long"],
                            capture_output=True, text=True, check=False).stdout.strip()
    config = bench.config
    tools = [path for path in bench.client.executables() if path.exists()]
    if not bench.baseline:
        tools += [config.daemon, config.blkctl]
    return {"commit": commit, "kernel": os.uname().release, "cpu": cpu_model(), "governors": governors,
            "boost": boost.read_text().strip() if boost.exists() else "unknown",
            "loadavg": Path("/proc/loadavg").read_text().split()[:3],
            "mem_available_kib": int(available.group(1)) if available else None,
            "daemon_cpus": config.daemon_cpus, "client_cpus": config.client_cpus,
            "sha256": {str(path): sha256(path) for path in tools}}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def cpu_model() -> str:
    match = re.search(r"^model name\s*: (.*)$", Path("/proc/cpuinfo").read_text(), re.MULTILINE)
    return match.group(1) if match else "unknown"


def warn_unstable_host(bench: Bench, host: dict[str, object]) -> None:
    """Warns about host settings that add run-to-run variance; the run still proceeds."""
    if host["governors"] != ["performance"]:
        note(bench, f"warning: CPU governor {host['governors']}, not performance")
    if host["boost"] == "1":
        note(bench, "warning: CPU boost is on; clock speed follows temperature")
    if float(host["loadavg"][0]) > 1.0:
        note(bench, f"warning: load average {host['loadavg'][0]} at start; other work competes for CPU")
    if not bench.config.client_cpus or not (bench.config.daemon_cpus or bench.baseline):
        note(bench, "warning: daemon or client is not pinned (--daemon-cpus, --client-cpus)")


# ---------------------------------------------------------------- setup


def preflight(bench: Bench) -> None:
    config = bench.config
    executables = list(bench.client.executables())
    if not bench.baseline:
        executables += [config.daemon, config.blkctl]
    for tool in executables:
        if not os.access(tool, os.X_OK):
            raise Failure(f"missing executable {tool}")
    if bench.baseline:
        preflight_baseline(bench)
    else:
        preflight_device(bench)
    bench.facts["environment_start"] = environment(bench)
    warn_unstable_host(bench, bench.facts["environment_start"])


def preflight_device(bench: Bench) -> None:
    config = bench.config
    if not os.access("/dev/ublk-control", os.R_OK | os.W_OK):
        raise Failure("no read/write access to /dev/ublk-control; see README normal-user ublk setup")
    if bench.client.needs_filesystem:
        check = run(bench, "helper-check", ["sudo", "-n", "--", MOUNT_HELPER, "check"], check=False)
        if check.returncode != 0 or HELPER_PROTOCOL not in check.stdout:
            raise Failure(f"mount helper {HELPER_PROTOCOL} is not installed; run "
                          "sudo bash scripts/install-ext4-helper.sh after make build-ext4")
        if config.filesystem == "xfs" and not shutil.which("/usr/sbin/xfs_repair"):
            raise Failure("xfs_repair is missing")
    if config.perf and int(Path("/proc/sys/kernel/perf_event_paranoid").read_text()) > 1:
        raise Failure("perf needs kernel.perf_event_paranoid <= 1 to record the daemon with kernel stacks")
    if config.perf and Path("/proc/sys/kernel/kptr_restrict").read_text().strip() != "0":
        note(bench, "warning: kernel.kptr_restrict != 0, so perf can't name kernel symbols")
    check_backing(bench)


def preflight_baseline(bench: Bench) -> None:
    config = bench.config
    if not config.baseline_medium:
        raise Failure("baseline mode needs --baseline-medium")
    for device in (config.baseline_device, config.baseline_medium):
        if not block_stat_path(device).exists():
            raise Failure(f"{device} has no block stat counters")
    if bench.client.needs_filesystem:
        if not config.baseline_mount:
            raise Failure(f"{config.client} needs --baseline-mount")
        if "discard" not in mount_options(config.baseline_mount).split(","):
            raise Failure(f"{config.baseline_mount} is not mounted with discard")
        if not os.access(config.baseline_mount, os.W_OK):
            raise Failure(f"{config.baseline_mount} is not writable")
    elif not os.access(config.baseline_device, os.R_OK | os.W_OK):
        raise Failure(f"no read/write access to {config.baseline_device}")
    bench.facts["backing_bytes"] = size_of_block_device(config.baseline_medium)


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
    bench.facts["requested_backing_bytes"] = requested_backing_bytes(bench)
    free = shutil.disk_usage(bench.backing.parent).free
    if free < bench.facts["requested_backing_bytes"]:
        raise Failure(f"{bench.backing.parent} has {free // MIB} MiB free, "
                      f"needs {bench.facts['requested_backing_bytes'] // MIB} MiB")


def requested_backing_bytes(bench: Bench) -> int:
    value = bench.config.backing_mib
    if value is None:
        return bench.logical_bytes
    if value != "max":
        return int(value) * MIB
    usage = os.statvfs(bench.backing.parent)
    available = usage.f_bavail * usage.f_frsize - FILESYSTEM_SLACK_BYTES
    return max(0, available) // MIB * MIB


def size_of_block_device(device: Path) -> int:
    with open(device, "rb") as source:
        return source.seek(0, os.SEEK_END)


def format_backing(bench: Bench) -> None:
    config = bench.config
    argv = [config.daemon, "format", bench.backing, "--size-mib", str(bench.logical_bytes // MIB),
            "--min-logical-fill-factor", str(config.min_logical_fill)]
    if bench.backing_kind == "block-device":
        argv.append("--block-device")
    else:
        argv += ["--prealloc-mib", str(bench.facts["requested_backing_bytes"] // MIB)]
    metrics.create_ledger(bench.ledger)
    # check_backing proved the file didn't exist, so whatever format leaves there is ours.
    bench.created_backing = bench.backing_kind == "regular-file"
    result = run(bench, "format", argv, env=daemon_env(bench))
    bench.facts["format"] = metrics.parse_daemon_events(result.stdout).get("format", {})
    bench.facts["backing_bytes"] = size_of_block_device(bench.backing)


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
    argv = [*pinned(config.daemon_cpus), str(config.daemon), "serve", str(bench.backing), "-1",
            *config.daemon_args]
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
    if size_bytes != bench.logical_bytes or block != 4096:
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
    """Mount and superblock options of the mount that holds `target`, from /proc/self/mountinfo."""
    best, options = "", None
    resolved = str(target.resolve())
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = line.split()
        point = fields[4]
        if (resolved == point or resolved.startswith(point.rstrip("/") + "/")) and len(point) >= len(best):
            separator = fields.index("-")
            best, options = point, f"{fields[5]},{fields[separator + 3]}"
    if options is None:
        raise Failure(f"{target} is not mounted")
    return options


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


def counters(bench: Bench) -> tuple[metrics.DeviceStat, int]:
    """The client device's counters and the bytes written to the medium below it."""
    config = bench.config
    if bench.baseline:
        device = metrics.parse_device_stat(block_stat_path(config.baseline_device).read_text())
        medium = metrics.parse_device_stat(block_stat_path(config.baseline_medium).read_text())
        return device, medium.write_bytes
    return (metrics.parse_device_stat(bench.daemon.stat_path.read_text()),
            metrics.read_ledger(bench.ledger).write_completed_bytes)


def observer(bench: Bench, phase: str) -> observe.PhaseObserver:
    return observe.PhaseObserver(bench.config.evidence, phase, sampler=pinned(bench.config.sampler_cpus),
                                 pid=bench.daemon.process.pid if bench.daemon else None,
                                 devices=observed_devices(bench), perf=bench.config.perf)


def observed_devices(bench: Bench) -> list[str]:
    """Kernel names of the client device and the device that holds the medium below it."""
    if bench.baseline:
        media = [bench.config.baseline_medium]
    elif bench.backing_kind == "block-device":
        media = [bench.backing]
    else:
        media = []
    names = [bench.client_device.resolve().name, *(path.resolve().name for path in media)]
    if not bench.baseline and bench.backing_kind != "block-device":
        dev = bench.backing.stat().st_dev
        names.append(Path(f"/sys/dev/block/{os.major(dev)}:{os.minor(dev)}").resolve().name)
    return list(dict.fromkeys(name for name in names if (Path("/sys/class/block") / name).exists()))


def block_stat_path(device: Path) -> Path:
    return Path("/sys/class/block") / device.resolve().name / "stat"


def daemon_cpu_seconds(bench: Bench) -> float | None:
    if not bench.daemon:
        return None
    return process_usage(bench.daemon.process.pid)["cpu_ticks"] / os.sysconf("SC_CLK_TCK")


def measured_phase(bench: Bench, name: str, units: int, seed: int) -> Phase:
    if bench.daemon and bench.daemon.process.poll() is not None:
        raise Failure(f"daemon exited with {bench.daemon.process.returncode} before {name}")
    device_before, medium_before = counters(bench)
    daemon_before = daemon_cpu_seconds(bench)
    with observer(bench, name) as watched:
        result = bench.client.window(bench, name, units, seed)
    device_after, medium_after = counters(bench)
    daemon_after = daemon_cpu_seconds(bench)
    device_bytes = device_after.write_bytes - device_before.write_bytes
    backing = medium_after - medium_before
    seconds = result.summary.seconds
    phase = Phase(
        name=name, operations=result.units, seconds=round(seconds, 3),
        operations_per_second=round(result.units / seconds, 1) if seconds else 0.0,
        device_write_bytes=device_bytes, backing_write_bytes=backing,
        write_amplification=round(backing / device_bytes, 4) if device_bytes else 0.0,
        latency=result.summary.per_operation,
        discards=device_after.discard_requests - device_before.discard_requests,
        daemon_cpu_seconds=round(daemon_after - daemon_before, 3) if daemon_before is not None else None,
        client_cpu_seconds=round(result.client_cpu_seconds, 3),
        observation=watched.summary,
    )
    bench.phases.append(phase)
    snapshot_backing(bench, name)
    note(bench, f"{name}: {phase.operations} units {phase.operations_per_second}/s "
                f"device={device_bytes // MIB} MiB medium={backing // MIB} MiB "
                f"WA={phase.write_amplification} discards={phase.discards}")
    return phase


def load(bench: Bench) -> None:
    device_before, medium_before = counters(bench)
    with observer(bench, "load") as watched:
        facts = bench.client.load(bench)
    bench.facts["load_observation"] = watched.summary
    device_after, medium_after = counters(bench)
    facts |= {"device_write_bytes": device_after.write_bytes - device_before.write_bytes,
              "backing_write_bytes": medium_after - medium_before}
    if bench.client.needs_filesystem:
        facts["filesystem_used_bytes"] = used_bytes(bench.mount)
    bench.facts["load"] = facts
    snapshot_backing(bench, "load")
    note(bench, f"load: {facts}")


def used_bytes(path: Path) -> int:
    usage = shutil.disk_usage(path)
    return usage.total - usage.free


def next_window_units(bench: Bench, previous: Phase) -> int:
    try:
        per_unit = previous.device_write_bytes / previous.operations
        return metrics.window_operations(bench.facts["backing_bytes"], per_unit, bench.client.min_device_bytes_per_unit)
    except ValueError as error:
        raise Failure(f"{previous.name}: {error}") from error


def age(bench: Bench) -> str:
    """Runs workload windows until the stop rule decides. Returns the reason."""
    config = bench.config
    calibration_units = config.calibration_units or bench.client.calibration_units
    calibration = measured_phase(bench, "calibration", calibration_units, config.seed + 1)
    units = next_window_units(bench, calibration)
    device_bytes, windows = calibration.device_write_bytes, []
    # Bounded: every window writes about one backing size, so the maximum multiple ends the loop first.
    for number in range(1, MAX_WINDOWS_PER_MULTIPLE * math.ceil(config.stop.maximum_multiple) + 1):
        phase = measured_phase(bench, f"window-{number:02d}", units, config.seed + 1 + number)
        device_bytes += phase.device_write_bytes
        windows.append(Window(phase.operations_per_second, phase.write_amplification))
        # Each window sizes the next: the short calibration over- or underestimates bytes per unit.
        units = next_window_units(bench, phase)
        reason = config.stop.decide(device_bytes, bench.facts["backing_bytes"], windows)
        if reason:
            return reason
    raise Failure("the window limit ended the run before the stop rule decided")


# ---------------------------------------------------------------- sampling and profiling


class Sampler:
    """Samples device, medium and daemon counters every `interval` seconds into samples.jsonl.

    It reads only sysfs, /proc and the ledger, which costs almost nothing. `blkctl info` reads the
    backing for seconds, so it runs between windows instead (snapshot_backing)."""

    def __init__(self, bench: Bench):
        self.bench = bench
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.output = open(bench.config.evidence / "samples.jsonl", "a")
        self.sequence = 0

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
                note(self.bench, f"sample failed: {error}")

    def sample(self) -> None:
        bench, self.sequence = self.bench, self.sequence + 1
        device, medium_write_bytes = counters(bench)
        record = {"time": time.time(), "device": asdict(device), "medium_write_bytes": medium_write_bytes}
        if bench.client.needs_filesystem:
            record["filesystem_used"] = used_bytes(bench.mount)
        if bench.daemon:
            record["daemon"] = process_usage(bench.daemon.process.pid)
        self.output.write(json.dumps(record) + "\n")
        self.output.flush()


def snapshot_backing(bench: Bench, phase: str) -> None:
    """Records `blkctl info` of the live backing between windows, outside any measured time."""
    if not bench.daemon:
        return
    started = time.monotonic()
    info = subprocess.run([*pinned(bench.config.sampler_cpus), str(bench.config.blkctl), "info", "--blocks",
                           "--color=never", str(bench.backing)], capture_output=True, text=True, timeout=300)
    (bench.config.evidence / "blkctl" / f"{phase}.txt").write_text(info.stdout + info.stderr)
    record = {"phase": phase, "seconds": round(time.monotonic() - started, 3), "exit": info.returncode,
              "blkctl": metrics.parse_blkctl_info(info.stdout)}
    with open(bench.config.evidence / "blkctl.jsonl", "a") as output:
        output.write(json.dumps(record) + "\n")


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
        if self.bench.config.perf and self.bench.daemon:
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
        kallsyms = [f"--kallsyms={self.bench.config.kallsyms}"] if self.bench.config.kallsyms else []
        run(self.bench, "perf-report", ["perf", "report", "-i", self.data, *kallsyms, "--stdio", "--no-children",
                                        "-g", "none", "--no-inline", "--percent-limit", "0.5",
                                        "--sort", "dso,symbol"], check=False)


# ---------------------------------------------------------------- checks


def check_writable(bench: Bench) -> None:
    options = mount_options(bench.mount).split(",")
    bench.gate("no_read_only_remount", "ro" not in options, "the filesystem was remounted read-only")


def check_filesystem(bench: Bench) -> None:
    device = bench.daemon.device
    if bench.config.filesystem == "ext4":
        result = run(bench, "e2fsck", ["e2fsck", "-f", "-n", device], check=False)
    else:
        result = run(bench, "xfs_repair", ["/usr/sbin/xfs_repair", "-n", device], check=False)
    bench.gate("filesystem_check", result.returncode == 0, f"exit {result.returncode}")


def check_backing_offline(bench: Bench) -> None:
    run(bench, "blkctl-info-final", [bench.config.blkctl, "info", "--segments", "--color=never", bench.backing])
    result = run(bench, "blkctl-check", [bench.config.blkctl, "check", "--color=never", bench.backing],
                 check=False)
    # Exit 1 means no errors and a tail that the next open replays (ADR-11).
    bench.gate("blkctl_check", result.returncode in (0, 1), f"exit {result.returncode}")


# ---------------------------------------------------------------- lifecycle


def start_client(bench: Bench, first: bool) -> None:
    if first:
        bench.client.prepare(bench)
    else:
        bench.client.start(bench)
    bench.client_running = True


def stop_client(bench: Bench) -> None:
    if bench.client_running:
        bench.client.stop(bench)
        bench.client_running = False


def age_and_verify(bench: Bench) -> None:
    with Sampler(bench):
        load(bench)
        with Profiler(bench):
            bench.facts["stop_reason"] = age(bench)
    if bench.client.needs_filesystem:
        check_writable(bench)
        bench.gates["device_discards"] = sum(phase.discards for phase in bench.phases) > 0
    bench.client.verify(bench, "aged")


def execute(bench: Bench) -> None:
    preflight(bench)
    if bench.baseline:
        execute_baseline(bench)
        return
    format_backing(bench)
    start_daemon(bench, "aging")
    filesystem = bench.client.needs_filesystem
    if filesystem:
        make_filesystem(bench)
        mount(bench)
        bench.workdir.mkdir(exist_ok=True)
    start_client(bench, first=True)
    age_and_verify(bench)
    stop_client(bench)
    if filesystem:
        unmount(bench)
        check_filesystem(bench)
    stop_daemon(bench, "aging")
    check_backing_offline(bench)
    start_daemon(bench, "restart")
    if filesystem:
        mount(bench)
    start_client(bench, first=False)
    bench.client.verify(bench, "restart")
    stop_client(bench)
    if filesystem:
        unmount(bench)
    stop_daemon(bench, "restart")
    if filesystem and not bench.gates["device_discards"]:
        raise Failure("the device received no discards")


def execute_baseline(bench: Bench) -> None:
    """Runs the same load, windows and checks on a stack without our daemon."""
    if bench.client.needs_filesystem:
        bench.workdir.mkdir(mode=0o700)
    start_client(bench, first=True)
    age_and_verify(bench)
    stop_client(bench)


def teardown(bench: Bench, succeeded: bool) -> None:
    try:
        stop_client(bench)
    except Failure as error:
        note(bench, f"teardown: client stop failed: {error}")
    if bench.baseline:
        if bench.client.needs_filesystem and bench.workdir.exists():
            # The work directory is ours; removing it returns its space to the thin pool.
            shutil.rmtree(bench.workdir)
        elif not bench.client.needs_filesystem:
            run(bench, "blkdiscard", ["blkdiscard", bench.config.baseline_device], check=False)
        shutil.rmtree(bench.owned)
        return
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
    # A backing on a matrix LV is the next run's fixture too, and an existing file stops that run. A
    # failed run keeps the backing's state as evidence instead of the backing itself. Only a file
    # this run created is removed; a block device is formatted again by the next run anyway.
    shared = bench.backing_kind == "block-device" or (bench.created_backing and bench.backing.parent != bench.owned)
    if not succeeded and not shared:
        note(bench, f"preserved {bench.owned} (backing {bench.backing}); remove it with: rm -rf {bench.owned}")
        return
    if shared:
        if not succeeded:
            record_failed_backing(bench)
        release_shared_backing(bench)
    shutil.rmtree(bench.owned)


def record_failed_backing(bench: Bench) -> None:
    if bench.backing_kind == "block-device" or bench.backing.exists():
        run(bench, "blkctl-info-failed", [bench.config.blkctl, "info", "--segments", "--color=never", bench.backing],
            check=False)
        run(bench, "blkctl-check-failed", [bench.config.blkctl, "check", "--color=never", bench.backing], check=False)


def release_shared_backing(bench: Bench) -> None:
    if bench.backing_kind == "block-device":
        # Return a RAM disk's pages.
        run(bench, "blkdiscard", ["blkdiscard", bench.backing], check=False)
    else:
        bench.backing.unlink(missing_ok=True)


def write_summary(bench: Bench, result: str) -> None:
    config = bench.config
    bench.facts["write_operation"] = bench.client.write_operation
    if "environment_start" in bench.facts:
        bench.facts["loadavg_end"] = Path("/proc/loadavg").read_text().split()[:3]
    report = {"result": result, "config": {k: str(v) for k, v in asdict(config).items()},
              "facts": bench.facts, "gates": bench.gates, "phases": [asdict(p) for p in bench.phases]}
    (config.evidence / "summary.json").write_text(json.dumps(report, indent=2, default=str) + "\n")
    where = "baseline" if bench.baseline else f"our device, {config.filesystem}"
    lines = [f"# {config.client} {config.workload} on {where}", "", f"Result: **{result}**", "",
             f"- device {config.size_gib} GiB, backing {bench.backing_kind} `{bench.backing}`, "
             f"{bench.facts.get('backing_bytes', 0) // MIB} MiB",
             f"- records {bench.facts.get('records')}, stop {bench.facts.get('stop_reason')}",
             f"- load {bench.facts.get('load')}",
             f"- environment {bench.facts.get('environment_start')}, load average at end "
             f"{bench.facts.get('loadavg_end')}", "",
             "| phase | units | units/s | device MiB | medium MiB | WA | discards | p50 us | p99 us | p99.9 us |",
             "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    write_operation = bench.client.write_operation
    for phase in bench.phases:
        update = phase.latency.get(write_operation, {})
        lines.append(f"| {phase.name} | {phase.operations} | {phase.operations_per_second} | "
                     f"{phase.device_write_bytes // MIB} | {phase.backing_write_bytes // MIB} | "
                     f"{phase.write_amplification} | {phase.discards} | {update.get('p50', '')} | "
                     f"{update.get('p99', '')} | {update.get('p999', '')} |")
    lines += ["", f"Latency columns are {write_operation} in microseconds. "
              "Per-second series are in <phase>-series.csv.", "", "| gate | pass |", "|---|---|"]
    lines += [f"| {name} | {'yes' if ok else 'NO'} |" for name, ok in bench.gates.items()]
    observations = [("load", bench.facts["load_observation"])] if bench.facts.get("load_observation") else []
    observations += [(phase.name, phase.observation) for phase in bench.phases if phase.observation]
    if observations:
        lines += ["", *observe.markdown(observations)]
    (config.evidence / "summary.md").write_text("\n".join(lines) + "\n")


def new_bench(config: Config) -> Bench:
    owned = Path("/tmp") / f"my-block-storage-ublk.{os.getpid()}.{secrets.randbelow(10**9)}"
    owned.mkdir(mode=0o700)
    (owned / "mount").mkdir(mode=0o700)
    baseline = config.baseline_device is not None
    backing = config.baseline_device if baseline else (config.backing or owned / "backing.img")
    kind = "block-device" if backing.exists() and stat.S_ISBLK(backing.stat().st_mode) else "regular-file"
    if baseline:
        kind = "baseline"
    config.evidence.mkdir(parents=True)
    (config.evidence / "blkctl").mkdir()
    mount = config.baseline_mount or owned / "mount"
    workdir = mount / f"aged-{os.getpid()}-{time.time_ns()}" if config.baseline_mount else mount / "work"
    bench = Bench(config=config, client=clients.make(config.client, config.workload), owned=owned,
                  backing=backing, backing_kind=kind, ledger=owned / "io-ledger", mount=mount, workdir=workdir,
                  log=open(config.evidence / "run.log", "a"))
    bench.deadline = time.monotonic() + config.max_hours * 3600
    return bench


def interrupt(signum: int, _frame: object) -> None:
    raise KeyboardInterrupt(signal.Signals(signum).name)


def main(argv: list[str]) -> int:
    # SIGTERM and SIGHUP (a closed terminal) take the same teardown path as Ctrl-C. Without it the
    # daemon, which runs in its own session, outlives the harness with the filesystem mounted.
    signal.signal(signal.SIGTERM, interrupt)
    signal.signal(signal.SIGHUP, interrupt)
    config = parse_args(argv)
    bench = new_bench(config)
    note(bench, f"evidence: {config.evidence}")
    result = "FAIL"
    try:
        execute(bench)
        result = "PASS"
    except (Failure, OSError, subprocess.SubprocessError, ValueError) as error:
        note(bench, f"FAIL: {error}")
    except KeyboardInterrupt:
        note(bench, "FAIL: interrupted")
    finally:
        teardown(bench, result == "PASS")
        write_summary(bench, result)
        note(bench, f"result: {result}; summary: {config.evidence / 'summary.md'}")
    return 0 if result == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
