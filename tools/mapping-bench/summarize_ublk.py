"""Validate the fio matrix and print median throughput, tails, and daemon RSS."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median

from ublk import trace


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    metadata = json.loads((args.directory / "metadata.json").read_text())
    rows = [json.loads(line) for line in (args.directory / "results.jsonl").read_text().splitlines()]
    configurations = metadata.get("configurations", [("v1.1", "default"), ("v1.2", "default"), ("v1.2", str(16 << 20))])
    workloads = metadata.get("workloads", ["seq-read", "wide-read", "seq-write", "wide-write"])
    expected = {(v, b, w, r) for v, b in configurations for w in workloads
                for r in range(1, metadata["repetitions"] + 1)}
    actual = {(r["version"], r["budget"], r["workload"], r["repetition"]) for r in rows}
    assert expected == actual and len(rows) == len(expected), "incomplete or duplicate matrix"
    groups = defaultdict(list)
    for row in rows:
        addresses, _, flush_every = trace(row["workload"])
        action = "write" if flush_every else "read"
        job = row["job"]
        assert job["error"] == 0 and job[action]["io_bytes"] == len(addresses) * 4096
        assert job[action]["iops"] > 0 and row["allocated_bytes"] < 512 << 20
        if flush_every:
            assert job["sync"]["total_ios"] == len(addresses) // flush_every
        groups[row["workload"], row["version"], row["budget"]].append(row)
    print("workload version budget IOPS[min..max] completion_p99_us flush_p99_ms RSS_MiB")
    for (workload, version, budget), records in sorted(groups.items()):
        action = "write" if workload.endswith("write") else "read"
        rates = [row["job"][action]["iops"] for row in records]
        tails = [row["job"][action]["clat_ns"]["percentile"]["99.000000"] for row in records]
        flushes = [row["job"]["sync"]["lat_ns"].get("percentile", {}).get("99.000000", 0)
                   for row in records]
        rss = median(row["rss_bytes"] for row in records)
        print(f"{workload} {version} {budget} {median(rates):.0f}[{min(rates):.0f}..{max(rates):.0f}] "
              f"{median(tails)/1000:.2f} {median(flushes)/1e6:.2f} {rss/2**20:.2f}")


if __name__ == "__main__":
    main()
