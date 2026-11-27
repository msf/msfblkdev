"""The workloads the aged harness can drive. Each client loads data, runs a window of work units and
checks its data; run.py owns the device, filesystem, windows and stop rule around it.

| client       | unit              | needs     |
|--------------|-------------------|-----------|
| ycsb-sqlite  | YCSB operation    | filesystem |
| ycsb-rocksdb | YCSB operation    | filesystem |
| fio          | 4 KiB write       | block device only |
"""

from __future__ import annotations

import json
import re
import shutil
import time
from dataclasses import dataclass
from pathlib import Path

import metrics

ROOT = Path(__file__).resolve().parents[2]
WORKLOADS = ROOT / "tools/go-ycsb/workloads"
GO_YCSB = ROOT / "tools/build/go-ycsb-8633137/bin/go-ycsb"
SQLITE3 = ROOT / "tools/build/sqlite-3.51.3/build/sqlite3"
LDB = ROOT / "tools/build/rocksdb-9.11.2/build/tools/ldb"
# go-ycsb and the sqlite3 shell both link the lab build of this version (tools/sqlite/setup.sh).
SQLITE_VERSION = "3.51.3"
FIO_BLOCK = 4096


@dataclass
class WindowResult:
    units: int
    summary: metrics.LatencySummary
    client_cpu_seconds: float


class Client:
    """Defaults for a filesystem client; subclasses override what differs."""

    name = ""
    needs_filesystem = True
    write_operation = ""
    default_fill = 0.5
    # The fewest device bytes one unit can write when its writes reach the device.
    min_device_bytes_per_unit = 2048
    calibration_units = 20_000

    def __init__(self, workload: str):
        self.workload = workload

    def executables(self) -> list[Path]:
        return []

    def prepare(self, bench) -> None:
        """Runs after the filesystem is mounted and before the load."""

    def stop(self, bench) -> None:
        """Runs before the filesystem is unmounted."""

    def start(self, bench) -> None:
        """Runs after a restart and remount."""

    def verify(self, bench, label: str) -> None:
        """Checks the client's data and records gates."""


# ------------------------------------------------------------------ YCSB


class Ycsb(Client):
    write_operation_by_workload = {"a": "UPDATE", "f": "READ_MODIFY_WRITE"}
    load_batch = 1
    driver = ""

    def __init__(self, workload: str):
        super().__init__(workload)
        self.write_operation = self.write_operation_by_workload[workload]

    def executables(self) -> list[Path]:
        return [GO_YCSB]

    def records(self, bench) -> int:
        fill = bench.config.client_fill or self.default_fill
        return metrics.record_count(bench.logical_bytes, fill, self.load_batch)

    def driver_properties(self, bench) -> dict[str, object]:
        return {}

    def check_output(self, output: str) -> str | None:
        return "go-ycsb reported operation errors" if "operation err" in output else None

    def ycsb(self, bench, label: str, command: str, properties: dict[str, object]) -> tuple:
        latency_csv = bench.config.evidence / f"{label}-latency.csv"
        base = {
            # dataintegrity stays off: it makes every update write the value the record already
            # holds, and SQLite commits an unchanged row without writing anything.
            "fieldlength": 4000, "dataintegrity": "false", "silence": "false", "measurementtype": "csv",
            "measurement.output_file": latency_csv, **self.driver_properties(bench), **properties,
        }
        argv = [*bench.pinned_client(), GO_YCSB, command, self.driver, "-P", WORKLOADS / "common.properties",
                "-P", WORKLOADS / f"{self.workload}.properties"]
        for name, value in base.items():
            argv += ["-p", f"{name}={value}"]
        for extra in bench.config.ycsb_properties:
            argv += ["-p", extra]
        output, cpu_seconds = bench.run_with_usage(label, argv)
        problem = self.check_output(output)
        if problem:
            raise bench.failure(f"{label}: {problem}; see {label}.out")
        summary = metrics.summarize_latency(latency_csv, bench.config.evidence / f"{label}-series.csv")
        if not bench.config.keep_latency_csv:
            latency_csv.unlink()
        return summary, cpu_seconds

    def load(self, bench) -> dict:
        records = self.records(bench)
        bench.facts["records"] = records
        summary, _ = self.ycsb(bench, "load", "load", {"recordcount": records, "batch.size": self.load_batch,
                                                       "workload.seed": bench.config.seed})
        return {"records": records, "seconds": round(summary.seconds, 3)}

    def window(self, bench, name: str, units: int, seed: int) -> WindowResult:
        summary, cpu = self.ycsb(bench, name, "run", {"recordcount": bench.facts["records"],
                                                      "operationcount": units, "workload.seed": seed})
        return WindowResult(units, summary, cpu)


class YcsbSqlite(Ycsb):
    name, driver = "ycsb-sqlite", "sqlite"
    default_fill = 0.75
    # go-ycsb loads in batches of this many records and always finishes the last batch.
    load_batch = 100

    def executables(self) -> list[Path]:
        return [GO_YCSB, SQLITE3]

    def database(self, bench) -> Path:
        return bench.workdir / "ycsb.db"

    def driver_properties(self, bench) -> dict[str, object]:
        return {"sqlite.db": self.database(bench), "sqlite.journalmode": "WAL"}

    def check_output(self, output: str) -> str | None:
        return metrics.client_output_problem(output, SQLITE_VERSION)

    def verify(self, bench, label: str) -> None:
        result = bench.run(f"sqlite-{label}", [SQLITE3, self.database(bench),
                                               "PRAGMA integrity_check; SELECT count(*) FROM usertable;"])
        lines = result.stdout.split()
        bench.gate(f"integrity_check_{label}", lines[:1] == ["ok"], result.stdout[:300])
        bench.gate(f"record_count_{label}", lines[-1:] == [str(bench.facts["records"])], result.stdout[-100:])


class YcsbRocksdb(Ycsb):
    name, driver = "ycsb-rocksdb", "rocksdb"
    # A leveled LSM needs free space for compaction output next to its live files.
    default_fill = 0.5

    def executables(self) -> list[Path]:
        return [GO_YCSB, LDB]

    def driver_properties(self, bench) -> dict[str, object]:
        return {"rocksdb.dir": bench.workdir / "rocksdb"}

    def verify(self, bench, label: str) -> None:
        directory = bench.workdir / "rocksdb"
        check = bench.run(f"ldb-check-{label}", [LDB, f"--db={directory}", "checkconsistency"], check=False)
        bench.gate(f"consistency_{label}", check.returncode == 0 and "OK" in check.stdout, check.stdout[-300:])
        count = bench.run(f"ldb-count-{label}", [LDB, f"--db={directory}", "dump", "--count_only"])
        keys = re.search(r"Keys in range: (\d+)", count.stdout)
        bench.gate(f"record_count_{label}", bool(keys) and int(keys.group(1)) == bench.facts["records"],
                   count.stdout[-200:])


# ------------------------------------------------------------------ fio


class Fio(Client):
    """The v2.0 aged-fio profile on the block device itself: fill every LBA, then 4 KiB random
    writes at queue depth 32 with an fsync every 256 writes."""

    name = "fio"
    needs_filesystem = False
    write_operation = "WRITE"
    min_device_bytes_per_unit = FIO_BLOCK
    calibration_units = 262_144  # 1 GiB

    def executables(self) -> list[Path]:
        return [Path(shutil.which("fio") or "fio")]

    def fio(self, bench, label: str, options: list[str]) -> tuple[dict, float]:
        output = bench.config.evidence / f"{label}.json"
        _, cpu = bench.run_with_usage(label, [
            *bench.pinned_client(), "fio", "--name=aged", f"--filename={bench.client_device}", "--direct=1",
            "--ioengine=io_uring", "--iodepth=32", f"--output={output}", "--output-format=json", *options])
        report = json.loads(output.read_text())
        job = report["jobs"][0]
        if job["error"]:
            raise bench.failure(f"{label}: fio error {job['error']}")
        return job, cpu

    def load(self, bench) -> dict:
        started = time.monotonic()
        self.fio(bench, "load", ["--rw=write", "--bs=1M"])
        bench.facts["records"] = bench.logical_bytes // FIO_BLOCK
        return {"bytes": bench.logical_bytes, "seconds": round(time.monotonic() - started, 3)}

    def window(self, bench, name: str, units: int, seed: int) -> WindowResult:
        distribution = {"uniform": "random", "zipf": "zipf:1.2"}[self.workload]
        job, cpu = self.fio(bench, name, [
            "--rw=randwrite", f"--bs={FIO_BLOCK}", "--fsync=256", f"--io_size={units * FIO_BLOCK}",
            f"--random_distribution={distribution}", "--randrepeat=0", f"--randseed={seed}",
            "--random_generator=tausworthe64", "--norandommap"])
        return WindowResult(job["write"]["total_ios"], metrics.summarize_fio(job), cpu)


CLIENTS = {"ycsb-sqlite": YcsbSqlite, "ycsb-rocksdb": YcsbRocksdb, "fio": Fio}
WORKLOADS_BY_CLIENT = {"ycsb-sqlite": ("a", "f"), "ycsb-rocksdb": ("a", "f"), "fio": ("uniform", "zipf")}


def make(name: str, workload: str) -> Client:
    if workload not in WORKLOADS_BY_CLIENT[name]:
        raise ValueError(f"{name} has no workload {workload}; choose from {WORKLOADS_BY_CLIENT[name]}")
    return CLIENTS[name](workload)

