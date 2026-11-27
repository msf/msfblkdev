"""Active benchmarking: observes the host while each measured phase runs.

A throughput number alone doesn't say what limits it. For every phase, the observer answers:

- Is a daemon thread CPU-bound? Per-thread on-CPU, run-queue and off-CPU fractions come from
  /proc/<pid>/task/<tid>/schedstat. Off-CPU is the rest of the wall time: blocked on I/O, a lock,
  a futex or an io_uring completion.
- Is the time in our device or below it? Per-device write and flush latency, utilization and
  queue depth come from /sys/block/<dev>/stat for the client device and the medium under it.
- Is the host disturbing the run? Pressure stall (PSI) fractions for CPU and memory. I/O PSI is
  recorded in summary.json but left out of the table: the idle host reads 80-95% I/O pressure with
  no disk I/O, apparently because io_uring waiters (the terminal, our daemon) count as iowait.

It also records 1-second series with mpstat, pidstat and iostat, and with `perf` a per-thread
`perf stat`. Everything reads /proc and sysfs or runs on the sampler CPUs, away from the measured
cores, and needs no root except `perf stat` (kernel.perf_event_paranoid <= 1).
"""

from __future__ import annotations

import json
import signal
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path

SERIES_INTERVAL_SECONDS = "1"
TOOL_STOP_SECONDS = 30
PRESSURE_RESOURCES = ("cpu", "io", "memory")
# Only the IPC pair: in the first smoke run, perf stat --per-thread reported 2.3 times the task-clock
# and context switches that schedstat and pidstat agreed on. Unexplained, so not recorded.
PERF_STAT_EVENTS = "cycles,instructions"
SECTOR_BYTES = 512


@dataclass(frozen=True)
class ThreadTimes:
    comm: str
    on_cpu_ns: int
    runqueue_ns: int
    voluntary_switches: int
    involuntary_switches: int


@dataclass(frozen=True)
class DeviceTimes:
    """Fields of /sys/block/<dev>/stat (Documentation/block/stat.rst)."""

    writes: int
    write_sectors: int
    write_ms: int
    io_ms: int
    queue_ms: int
    flushes: int
    flush_ms: int


@dataclass(frozen=True)
class Snapshot:
    monotonic: float
    threads: dict[int, ThreadTimes]
    devices: dict[str, DeviceTimes]
    pressure_us: dict[str, int]


def read_threads(pid: int | None) -> dict[int, ThreadTimes]:
    if pid is None:
        return {}
    threads = {}
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            on_cpu, runqueue, _slices = (int(v) for v in (task / "schedstat").read_text().split())
            status = (task / "status").read_text()
            comm = (task / "comm").read_text().strip()
        except (FileNotFoundError, ProcessLookupError):  # the thread exited between listing and reading
            continue
        switches = dict(line.split(":\t") for line in status.splitlines()
                        if line.startswith(("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")))
        threads[int(task.name)] = ThreadTimes(comm, on_cpu, runqueue, int(switches["voluntary_ctxt_switches"]),
                                              int(switches["nonvoluntary_ctxt_switches"]))
    return threads


def parse_device_times(text: str) -> DeviceTimes:
    f = [int(v) for v in text.split()]
    flushes, flush_ms = (f[15], f[16]) if len(f) >= 17 else (0, 0)
    return DeviceTimes(writes=f[4], write_sectors=f[6], write_ms=f[7], io_ms=f[9], queue_ms=f[10],
                       flushes=flushes, flush_ms=flush_ms)


def read_devices(names: list[str]) -> dict[str, DeviceTimes]:
    return {name: parse_device_times(Path(f"/sys/class/block/{name}/stat").read_text()) for name in names}


def parse_pressure(resource: str, text: str) -> dict[str, int]:
    totals = {}
    for line in text.splitlines():
        kind, *fields = line.split()
        totals[f"{resource}_{kind}"] = int(dict(f.split("=") for f in fields)["total"])
    return totals


def read_pressure() -> dict[str, int]:
    totals = {}
    for resource in PRESSURE_RESOURCES:
        totals |= parse_pressure(resource, Path(f"/proc/pressure/{resource}").read_text())
    return totals


def snapshot(pid: int | None, devices: list[str]) -> Snapshot:
    return Snapshot(time.monotonic(), read_threads(pid), read_devices(devices), read_pressure())


def summarize(before: Snapshot, after: Snapshot) -> dict[str, object]:
    """Fractions of the phase's wall time per thread, device rates and latencies, and PSI."""
    wall = after.monotonic - before.monotonic
    if wall <= 0:
        raise ValueError("the phase has no wall time")
    wall_ns = wall * 1e9
    threads = []
    for tid, end in after.threads.items():
        start = before.threads.get(tid, ThreadTimes(end.comm, 0, 0, 0, 0))
        on_cpu = (end.on_cpu_ns - start.on_cpu_ns) / wall_ns
        runqueue = (end.runqueue_ns - start.runqueue_ns) / wall_ns
        threads.append({"tid": tid, "comm": end.comm, "on_cpu": round(on_cpu, 4), "runqueue": round(runqueue, 4),
                        "off_cpu": round(max(0.0, 1 - on_cpu - runqueue), 4),
                        "voluntary_switches_per_second": round(
                            (end.voluntary_switches - start.voluntary_switches) / wall, 1),
                        "involuntary_switches_per_second": round(
                            (end.involuntary_switches - start.involuntary_switches) / wall, 1)})
    threads.sort(key=lambda thread: -thread["on_cpu"])
    devices = {}
    for name, end in after.devices.items():
        start = before.devices[name]
        writes, flushes = end.writes - start.writes, end.flushes - start.flushes
        devices[name] = {
            "writes_per_second": round(writes / wall, 1),
            "write_mib_per_second": round((end.write_sectors - start.write_sectors) * SECTOR_BYTES / 2**20 / wall, 1),
            "write_await_ms": round((end.write_ms - start.write_ms) / writes, 3) if writes else None,
            "flushes_per_second": round(flushes / wall, 1),
            "flush_await_ms": round((end.flush_ms - start.flush_ms) / flushes, 3) if flushes else None,
            "utilization": round((end.io_ms - start.io_ms) / (wall * 1000), 4),
            "average_queue": round((end.queue_ms - start.queue_ms) / (wall * 1000), 2),
        }
    pressure = {key: round((after.pressure_us[key] - before.pressure_us[key]) / (wall * 1e6), 4)
                for key in after.pressure_us}
    return {"seconds": round(wall, 3), "threads": threads, "devices": devices, "pressure": pressure}


class PhaseObserver:
    """Context manager around one measured phase. Writes observe/<phase>/ and keeps the summary."""

    def __init__(self, evidence: Path, phase: str, *, sampler: list[str], pid: int | None,
                 devices: list[str], perf: bool):
        self.directory = evidence / "observe" / phase
        self.sampler, self.pid, self.devices, self.perf = sampler, pid, devices, perf
        self.processes: list[subprocess.Popen] = []
        self.logs: list[object] = []
        self.before: Snapshot | None = None
        self.summary: dict[str, object] | None = None

    def tools(self) -> dict[str, list[str]]:
        interval = SERIES_INTERVAL_SECONDS
        tools = {"mpstat.txt": ["mpstat", "-P", "ALL", interval],
                 "iostat.txt": ["iostat", "-dxy", "-t", *self.devices, interval]}
        if self.pid is not None:
            tools["pidstat.txt"] = ["pidstat", "-t", "-u", "-w", "-p", str(self.pid), interval]
            if self.perf:
                tools["perf-stat.txt"] = ["perf", "stat", "--per-thread", "-e", PERF_STAT_EVENTS,
                                          "-p", str(self.pid)]
        return tools

    def __enter__(self) -> PhaseObserver:
        self.directory.mkdir(parents=True, exist_ok=True)
        env = {"LC_ALL": "C", "S_TIME_FORMAT": "ISO", "PATH": "/usr/local/bin:/usr/bin:/bin"}
        for name, argv in self.tools().items():
            log = open(self.directory / name, "w")
            self.logs.append(log)
            self.processes.append(subprocess.Popen([*self.sampler, *argv], stdout=log, stderr=subprocess.STDOUT,
                                                   env=env))
        self.before = snapshot(self.pid, self.devices)
        return self

    def __exit__(self, *_exc: object) -> None:
        try:
            self.summary = summarize(self.before, snapshot(self.pid, self.devices))
            (self.directory / "summary.json").write_text(json.dumps(self.summary, indent=2) + "\n")
        finally:
            self.stop_tools()

    def stop_tools(self) -> None:
        for process in self.processes:
            process.send_signal(signal.SIGINT)
        for process in self.processes:
            try:
                process.wait(timeout=TOOL_STOP_SECONDS)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for log in self.logs:
            log.close()


# Threads below this on-CPU fraction are left out of the markdown table; summary.json keeps them.
TABLE_MINIMUM_ON_CPU = 0.01


def percent(fraction: float | None) -> str:
    return "" if fraction is None else f"{100 * fraction:.1f}%"


def markdown(observations: list[tuple[str, dict[str, object]]]) -> list[str]:
    """Tables for summary.md: daemon threads, devices and pressure per phase."""
    lines = ["## Where the time went", "",
             "Daemon threads, as fractions of the phase's wall time. Off-CPU is blocked or sleeping.", "",
             "| phase | thread | tid | on-CPU | run queue | off-CPU | voluntary cs/s | involuntary cs/s |",
             "|---|---|---:|---:|---:|---:|---:|---:|"]
    for phase, summary in observations:
        for t in summary["threads"]:
            if t["on_cpu"] >= TABLE_MINIMUM_ON_CPU:
                lines.append(f"| {phase} | {t['comm']} | {t['tid']} | {percent(t['on_cpu'])} | "
                             f"{percent(t['runqueue'])} | {percent(t['off_cpu'])} | "
                             f"{t['voluntary_switches_per_second']} | {t['involuntary_switches_per_second']} |")
    lines += ["", "| phase | device | writes/s | MiB/s | write await ms | flushes/s | flush await ms | "
              "utilization | queue |", "|---|---|---:|---:|---:|---:|---:|---:|---:|"]
    for phase, summary in observations:
        for name, d in summary["devices"].items():
            lines.append(f"| {phase} | {name} | {d['writes_per_second']} | {d['write_mib_per_second']} | "
                         f"{d['write_await_ms']} | {d['flushes_per_second']} | {d['flush_await_ms']} | "
                         f"{percent(d['utilization'])} | {d['average_queue']} |")
    lines += ["", "| phase | CPU pressure | memory pressure |", "|---|---:|---:|"]
    for phase, summary in observations:
        p = summary["pressure"]
        lines.append(f"| {phase} | {percent(p.get('cpu_some'))} | {percent(p.get('memory_some'))} |")
    return lines + ["", "Per-second series: observe/<phase>/{mpstat,pidstat,iostat}.txt."]
