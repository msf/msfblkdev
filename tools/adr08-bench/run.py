#!/usr/bin/env python3
"""ADR-08 performance comparisons: pre-ADR-08 baseline (57d56cc, format 3) against the
candidate working tree (format 4), one common driver, interleaved repetitions.

Each job runs the release build of `main.rs` + an adapter in its own checkout, pinned to one
CPU, and appends one JSON record per run to the output file. `summarize.py` turns the JSONL
into CSV and the report."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import shutil
import subprocess
import sys
import tempfile
import time

BASELINE = "57d56ccbf57ce243a06fcdba49cdd5c73b846a58"
MIB = 1 << 20
GIB = 1 << 30

# comparison -> list of (label, workload, parameters). `versions` names which builds run it.
COMPARISONS = {
    # 1. Pre-cleaning behaviour on a fresh volume: both engines, nothing to reclaim.
    "fresh": {
        "versions": ("baseline", "candidate"),
        "jobs": [
            ("seq-write", "fresh-seq-write", {"logical": 128 * MIB, "backing": 512 * MIB, "operations": 16384}),
            ("rand-write", "fresh-rand-write", {"logical": 128 * MIB, "backing": 512 * MIB, "operations": 16384}),
            ("seq-read", "fresh-seq-read", {"logical": 128 * MIB, "backing": 512 * MIB, "operations": 16384}),
            ("rand-read", "fresh-rand-read", {"logical": 128 * MIB, "backing": 512 * MIB, "operations": 16384}),
        ],
    },
    # 2. Aged raw reclamation: overwrite a live working set for twice the log capacity. The
    #    baseline has no reclamation and stops at ENOSPC; the candidate must sustain it.
    "aged": {
        "versions": ("baseline", "candidate"),
        "jobs": [
            ("128mib-50pct", "aged-overwrite", {"logical": 128 * MIB, "backing": 256 * MIB,
                                                "working_set": 16384, "capacity_multiple": 2}),
            ("128mib-80pct", "aged-overwrite", {"logical": 128 * MIB, "backing": 160 * MIB,
                                                "working_set": 16384, "capacity_multiple": 2}),
            ("4gib-50pct", "aged-overwrite", {"logical": 4 * GIB, "backing": 2 * GIB,
                                              "working_set": 262144, "capacity_multiple": 2}),
        ],
    },
    # 3. Progress and latency: the same aged overwrite with the daemon's scheduled ticks and
    #    with inline pressure reclamation only (candidate only; the baseline cannot age).
    "progress": {
        "versions": ("candidate",),
        "jobs": [
            ("128mib-50pct-ticks", "aged-overwrite", {"logical": 128 * MIB, "backing": 256 * MIB,
                                                      "working_set": 16384, "capacity_multiple": 2, "ticks": 1}),
            ("128mib-50pct-inline", "aged-overwrite", {"logical": 128 * MIB, "backing": 256 * MIB,
                                                       "working_set": 16384, "capacity_multiple": 2, "ticks": 0}),
        ],
    },
    # 4. Memory and restart: resident memory and open time on fresh and written volumes,
    #    including a 1 GiB volume under a deliberately small 12 MiB total budget.
    "memory": {
        "versions": ("baseline", "candidate"),
        "jobs": [
            ("128mib-default", "memory-restart", {"logical": 128 * MIB, "backing": 512 * MIB, "working_set": 16384}),
            ("1gib-default", "memory-restart", {"logical": GIB, "backing": 2 * GIB, "working_set": 65536}),
            ("1gib-12mib", "memory-restart", {"logical": GIB, "backing": 2 * GIB, "working_set": 65536,
                                              "heap_total": 12 * MIB}),
            ("4gib-default", "memory-restart", {"logical": 4 * GIB, "backing": 2 * GIB, "working_set": 65536}),
        ],
    },
}


def checked(args, **kwargs):
    try:
        return subprocess.run(args, check=True, text=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise


def prepare(repo, build, version):
    checkout = build / version
    checkout.mkdir(parents=True, exist_ok=True)
    if version == "candidate":
        shutil.copytree(repo / "rust", checkout / "rust", dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("target"))
    else:
        archive = build / f"{version}.tar"
        checked(["git", "archive", f"--output={archive}", BASELINE, "rust"], cwd=repo)
        checked(["tar", "-xf", str(archive), "-C", str(checkout)])
    example = checkout / "rust/examples/adr08_bench"
    example.mkdir(parents=True, exist_ok=True)
    source = repo / "tools/adr08-bench"
    shutil.copyfile(source / "main.rs", example / "main.rs")
    shutil.copyfile(source / f"adapter-{version}.rs", example / "adapter.rs")
    target = build / f"target-{version}"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    with (build / f"build-{version}.log").open("w") as log:
        checked(["cargo", "build", "--locked", "--release", "--example", "adr08_bench"],
                cwd=checkout / "rust", env=env, stdout=log, stderr=log, timeout=600)
    return target / "release/examples/adr08_bench"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="JSONL file to create")
    parser.add_argument("--repetitions", type=int, choices=range(1, 6), default=3)
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--comparisons", nargs="+", choices=sorted(COMPARISONS), default=sorted(COMPARISONS))
    parser.add_argument("--fixture-dir", type=Path, help="directory for backing files (default: inside the repo)")
    parser.add_argument("--timeout", type=int, default=1800, help="seconds per run")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    build = (args.build_dir or repo / "rust/target/adr08-bench").resolve()
    build.mkdir(parents=True, exist_ok=True)
    print(f"build={build}", flush=True)
    versions = sorted({v for name in args.comparisons for v in COMPARISONS[name]["versions"]})
    binaries = {version: prepare(repo, build, version) for version in versions}
    cpu = min(os.sched_getaffinity(0))
    source = repo / "tools/adr08-bench"
    digest = hashlib.sha256()
    for path in sorted(p for p in (build / "candidate/rust").rglob("*") if p.is_file()):
        digest.update(str(path.relative_to(build / "candidate/rust")).encode() + b"\0" + path.read_bytes())
    metadata = {
        "kernel": os.uname().release,
        "cpu": cpu,
        "baseline_commit": BASELINE,
        "candidate_commit": checked(["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True).stdout.strip(),
        "candidate_source_sha256": digest.hexdigest(),
        "driver_sha256": hashlib.sha256((source / "main.rs").read_bytes()).hexdigest(),
        "rustc": checked(["rustc", "--version"], capture_output=True).stdout.strip(),
        "repetitions": args.repetitions,
        "comparisons": {name: COMPARISONS[name] for name in args.comparisons},
        "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }
    jobs = []
    for name in args.comparisons:
        comparison = COMPARISONS[name]
        for label, workload, parameters in comparison["jobs"]:
            for version in comparison["versions"]:
                jobs.append((name, label, version, workload, parameters))
    fixture_parent = args.fixture_dir or repo
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output, tempfile.TemporaryDirectory(
            prefix=".adr08-bench-", dir=fixture_parent) as fixture_dir:
        try:
            checked(["chattr", "+C", fixture_dir], capture_output=True)
        except subprocess.CalledProcessError:
            pass
        metadata["filesystem"] = checked(["findmnt", "-T", fixture_dir, "-n", "-o", "SOURCE,FSTYPE,OPTIONS"],
                                         capture_output=True).stdout.strip()
        output.write(json.dumps({"metadata": metadata}) + "\n")
        rng = random.Random(74703)
        for repetition in range(1, args.repetitions + 1):
            order = list(jobs)
            rng.shuffle(order)
            for name, label, version, workload, parameters in order:
                command = [str(binaries[version]), fixture_dir, workload] + [
                    f"{key}={value}" for key, value in parameters.items()]
                started = time.monotonic()
                result = subprocess.run(["taskset", "-c", str(cpu)] + command, capture_output=True,
                                        text=True, timeout=args.timeout)
                record = {"comparison": name, "label": label, "version": version, "repetition": repetition,
                          "total_run_seconds": time.monotonic() - started, "exit_code": result.returncode,
                          "stderr": result.stderr.strip()[-2000:]}
                if result.returncode == 0:
                    for field in result.stdout.strip().split():
                        key, value = field.split("=", 1)
                        try:
                            record[key] = int(value)
                        except ValueError:
                            try:
                                record[key] = float(value)
                            except ValueError:
                                record[key] = value
                output.write(json.dumps(record) + "\n")
                output.flush()
                status = "PASS" if result.returncode == 0 else "FAIL"
                print(f"{status} repetition={repetition} {name}/{label} {version} "
                      f"seconds={record['total_run_seconds']:.1f}", flush=True)


if __name__ == "__main__":
    main()
