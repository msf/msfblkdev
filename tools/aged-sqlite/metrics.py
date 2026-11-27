"""Pure helpers for the aged SQLite benchmark: counters, stop rule and latency summaries.

Nothing here starts a process or touches a device, so test_metrics.py covers all of it.
"""

from __future__ import annotations

import csv
import math
import os
import re
import struct
from dataclasses import dataclass
from pathlib import Path

SECTOR_BYTES = 512
GIB = 1 << 30
MIB = 1 << 20

# Layout of `Ledger` in rust/src/io_accounting.rs (repr(C, align(8)), native endian, version 1).
# The daemon validates magic, version, length and owner before it uses a ledger, so a layout
# change there fails the run at format time instead of producing wrong numbers here.
LEDGER_MAGIC = b"MBSIOAC1"
LEDGER_VERSION = 1
LEDGER_FORMAT = "=8sIIIIQ" + "Q" * 10
LEDGER_BYTES = struct.calcsize(LEDGER_FORMAT)
LEDGER_SNAPSHOT_ATTEMPTS = 100
# The ledger is also a write budget. The benchmark only wants the counters.
LEDGER_WRITE_LIMIT_BYTES = 1 << 62

assert LEDGER_BYTES == 112, "ledger layout must match rust/src/io_accounting.rs"


@dataclass(frozen=True)
class LedgerSnapshot:
    write_completed_bytes: int
    read_completed_bytes: int
    write_operations: int
    read_operations: int
    flush_operations: int
    errors: int
    flags: int


def ledger_image(owner_uid: int, write_limit_bytes: int = LEDGER_WRITE_LIMIT_BYTES) -> bytes:
    return struct.pack(
        LEDGER_FORMAT, LEDGER_MAGIC, LEDGER_VERSION, LEDGER_BYTES, owner_uid, 0,
        write_limit_bytes, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    )


def create_ledger(path: Path) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        os.write(descriptor, ledger_image(os.geteuid()))
    finally:
        os.close(descriptor)


def parse_ledger(image: bytes) -> tuple[int, LedgerSnapshot]:
    """Returns the epoch and the counters of one ledger image."""
    (magic, version, length, _owner, _reserved, _limit, epoch, _reserved_bytes, written, read,
     read_operations, write_operations, flush_operations, _pending, errors, flags) = struct.unpack(
        LEDGER_FORMAT, image)
    if magic != LEDGER_MAGIC or version != LEDGER_VERSION or length != LEDGER_BYTES:
        raise ValueError("ledger header mismatch")
    return epoch, LedgerSnapshot(written, read, write_operations, read_operations,
                                 flush_operations, errors, flags)


def read_ledger(path: Path) -> LedgerSnapshot:
    """Reads one coherent snapshot. Writers make the epoch odd while they change counters."""
    with open(path, "rb", buffering=0) as ledger:
        for _ in range(LEDGER_SNAPSHOT_ATTEMPTS):
            before, snapshot = parse_ledger(os.pread(ledger.fileno(), LEDGER_BYTES, 0))
            after, _ = parse_ledger(os.pread(ledger.fileno(), LEDGER_BYTES, 0))
            if before == after and before % 2 == 0:
                return snapshot
    raise RuntimeError("ledger changed throughout bounded snapshot retries")


@dataclass(frozen=True)
class DeviceStat:
    """Counters from /sys/block/<dev>/stat (Documentation/block/stat.rst)."""

    read_sectors: int
    write_requests: int
    write_sectors: int
    discard_requests: int
    discard_sectors: int
    flush_requests: int

    @property
    def write_bytes(self) -> int:
        return self.write_sectors * SECTOR_BYTES


def parse_device_stat(text: str) -> DeviceStat:
    fields = [int(value) for value in text.split()]
    if len(fields) < 17:
        raise ValueError(f"expected 17 block stat fields, got {len(fields)}")
    return DeviceStat(read_sectors=fields[2], write_requests=fields[4], write_sectors=fields[6],
                      discard_requests=fields[11], discard_sectors=fields[13], flush_requests=fields[15])


def record_count(logical_bytes: int, client_fill: float, page_bytes: int = 4096) -> int:
    """Records that make the database about `client_fill` of the volume.

    With fieldlength=4000 one record and its leaf-cell overhead fit in one 4 KiB SQLite page
    (the inline payload limit of a 4096-byte table leaf is 4061 bytes), so the database holds
    about one page per record. Calibrate against the filesystem's used space after the load.
    """
    if not 0 < client_fill < 1:
        raise ValueError("client fill must be between 0 and 1")
    return int(logical_bytes * client_fill) // page_bytes


def window_operations(backing_bytes: int, device_bytes_per_operation: float) -> int:
    """Operations that write about one backing size to the device."""
    if device_bytes_per_operation <= 0:
        raise ValueError("calibration wrote nothing to the device")
    return max(1, math.ceil(backing_bytes / device_bytes_per_operation))


@dataclass(frozen=True)
class Window:
    operations_per_second: float
    write_amplification: float


@dataclass(frozen=True)
class StopRule:
    """docs/BLOCKCHECK-PLAN.md endurance stop rule for the 4 GiB scale.

    Steady state: two consecutive windows agree within `tolerance` on throughput and write
    amplification.
    """

    minimum_multiple: float = 3.0
    maximum_multiple: float = 10.0
    tolerance: float = 0.05

    def decide(self, device_bytes: int, backing_bytes: int, windows: list[Window]) -> str | None:
        multiple = device_bytes / backing_bytes
        if multiple >= self.maximum_multiple:
            return "maximum"
        if multiple >= self.minimum_multiple and self.steady(windows):
            return "steady"
        return None

    def steady(self, windows: list[Window]) -> bool:
        if len(windows) < 2:
            return False
        previous, last = windows[-2], windows[-1]
        return (close(previous.operations_per_second, last.operations_per_second, self.tolerance)
                and close(previous.write_amplification, last.write_amplification, self.tolerance))


def close(a: float, b: float, tolerance: float) -> bool:
    largest = max(abs(a), abs(b))
    return largest == 0 or abs(a - b) / largest <= tolerance


def percentile(sorted_values: list[int], fraction: float) -> int:
    if not sorted_values:
        return 0
    index = min(len(sorted_values) - 1, max(0, math.ceil(fraction * len(sorted_values)) - 1))
    return sorted_values[index]


@dataclass
class LatencySummary:
    """Latency per go-ycsb measurement name.

    go-ycsb measures each DB call (READ, UPDATE, INSERT, ...), each workload-level
    READ_MODIFY_WRITE and every DB call again as TOTAL, so measurement counts don't add up to
    the requested operations. Throughput uses the requested operation count instead.
    """

    seconds: float
    per_operation: dict[str, dict[str, int]]


PERCENTILES = {"p50": 0.50, "p99": 0.99, "p999": 0.999, "p9999": 0.9999}


def summarize_latency(csv_path: Path, series_path: Path) -> LatencySummary:
    """Summarizes go-ycsb csv output and writes one row per operation type and second."""
    latencies: dict[str, list[int]] = {}
    per_second: dict[tuple[int, str], list[int]] = {}
    first_start, last_end = None, None
    with open(csv_path, newline="") as source:
        for row in csv.DictReader(source):
            operation, start, latency = row["operation"], int(row["timestamp_us"]), int(row["latency_us"])
            latencies.setdefault(operation, []).append(latency)
            per_second.setdefault((start // 1_000_000, operation), []).append(latency)
            first_start = start if first_start is None else min(first_start, start)
            last_end = start + latency if last_end is None else max(last_end, start + latency)
    write_series(series_path, per_second)
    summary: dict[str, dict[str, int]] = {}
    for operation, values in latencies.items():
        values.sort()
        summary[operation] = {"count": len(values), "max": values[-1],
                              **{name: percentile(values, f) for name, f in PERCENTILES.items()}}
    seconds = (last_end - first_start) / 1e6 if first_start is not None else 0.0
    return LatencySummary(seconds, summary)


def write_series(path: Path, per_second: dict[tuple[int, str], list[int]]) -> None:
    with open(path, "w", newline="") as target:
        writer = csv.writer(target)
        writer.writerow(["second", "operation", "count", "p50_us", "p99_us", "max_us"])
        for (second, operation), values in sorted(per_second.items()):
            values.sort()
            writer.writerow([second, operation, len(values), percentile(values, 0.5),
                             percentile(values, 0.99), values[-1]])


# blkctl info --blocks prints counts of 4 KiB blocks, for example "  live   24676   73.5% of used".
BLKCTL_FIELD = re.compile(r"^\s{2}(used|live|metadata|dead|free|mapped)\s+(\d+)\b", re.MULTILINE)
BLKCTL_SEGMENTS = re.compile(
    r"^segments\s+sealed (\d+)\s+pending-free (\d+)\s+open (\d+)\s+free (\d+)", re.MULTILINE)


def parse_blkctl_info(text: str) -> dict[str, int]:
    fields = {name: int(value) for name, value in BLKCTL_FIELD.findall(text)}
    segments = BLKCTL_SEGMENTS.search(text)
    if segments:
        for name, value in zip(("sealed", "pending_free", "open", "free_segments"), segments.groups(), strict=True):
            fields[name] = int(value)
    return fields


DAEMON_EVENT = re.compile(r"^info event=(\w+)(?: phase=(\w+))? (.*)$")


def parse_daemon_events(text: str) -> dict[str, dict[str, str]]:
    """Returns the last key=value record of each daemon info event, keyed by event[.phase]."""
    events: dict[str, dict[str, str]] = {}
    for line in text.splitlines():
        match = DAEMON_EVENT.match(line)
        if not match:
            continue
        name = match.group(1) + (f".{match.group(2)}" if match.group(2) else "")
        events[name] = dict(pair.split("=", 1) for pair in match.group(3).split() if "=" in pair)
    return events
