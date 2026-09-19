"""Validate complete repetitions and print compact benchmark summaries."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path)
    args = parser.parse_args()
    lines = [json.loads(line) for line in args.input.read_text().splitlines()]
    metadata = lines[0]["metadata"]
    rows = lines[1:]
    configurations = metadata.get("configurations", {(r["version"], r["budget"]) for r in rows})
    workloads = metadata.get("workloads", {r["workload"] for r in rows})
    repetitions = range(1, metadata["repetitions"] + 1)
    expected = {(v, b, w, r) for v, b in configurations for w in workloads for r in repetitions}
    actual = {(r["version"], r["budget"], r["workload"], r["repetition"]) for r in rows}
    assert actual == expected and len(rows) == len(expected), "missing or repeated benchmark case"
    groups = defaultdict(list)
    for row in rows:
        assert row["operations"] > 0 and row["elapsed_ns"] > 0
        assert row["allocated_bytes"] < 512 << 20
        groups[row["workload"], row["version"], row["budget"]].append(row)
    print("workload version budget iops[min..max] MiB/s p99_us flush_p99_ms RSS_MiB hit_pct pressure records close_ms including_close_iops")
    for (workload, version, budget), records in sorted(groups.items()):
        rates = [r["operations"] * 1e9 / r["elapsed_ns"] for r in records]
        with_close = [r["operations"] * 1e9 / (r["elapsed_ns"] + r["close_ns"]) for r in records]
        lookups = sum(r["hits"] + r["misses"] for r in records)
        hit_rate = f"{sum(r['hits'] for r in records) * 100 / lookups:.2f}" if lookups else "n/a"
        metrics = [median(r[field] for r in records) for field in
                   ("op_p99_ns", "flush_p99_ns", "rss_bytes", "pressure", "records", "close_ns")]
        p99, flush, rss, pressure, count, close = metrics
        print(f"{workload} {version} {budget} {median(rates):.0f}[{min(rates):.0f}..{max(rates):.0f}] "
              f"{median(rates)/256:.1f} {p99/1e3:.2f} {flush/1e6:.2f} {rss/2**20:.2f} "
              f"{hit_rate} {pressure:g} {count:g} {close/1e6:.2f} {median(with_close):.0f}")


if __name__ == "__main__":
    main()
