"""Compare unchanged release tags with one common, bounded engine driver."""
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


def checked(args, **kwargs):
    try:
        return subprocess.run(args, check=True, text=True, **kwargs)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        raise


def prepare(repo, build, version):
    checkout = build / version
    checkout.mkdir(exist_ok=True)
    if version == "working":
        shutil.copytree(repo / "rust", checkout / "rust", dirs_exist_ok=True,
                        ignore=shutil.ignore_patterns("target"))
    else:
        archive = build / f"{version}.tar"
        checked(["git", "archive", f"--output={archive}", version, "rust"], cwd=repo)
        checked(["tar", "-xf", str(archive), "-C", str(checkout)])
    example = checkout / "rust/examples/mapping_compare"
    example.mkdir(parents=True, exist_ok=True)
    source = repo / "tools/mapping-bench"
    shutil.copyfile(source / "compare.rs", example / "main.rs")
    adapter = "v11" if version == "v1.1" else "v12"
    shutil.copyfile(source / f"{adapter}.rs", example / "adapter.rs")
    target = build / f"target-{version}"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    with (build / f"build-{version}.log").open("w") as log:
        checked(["cargo", "build", "--locked", "--release", "--example", "mapping_compare"],
                cwd=checkout / "rust", env=env, stdout=log, stderr=log, timeout=300)
        checked(["cargo", "clippy", "--locked", "--release", "--example", "mapping_compare",
                 "--", "-D", "warnings"], cwd=checkout / "rust", env=env,
                stdout=log, stderr=log, timeout=120)
    return target / "release/examples/mapping_compare"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repetitions", type=int, choices=range(1, 6), default=3)
    parser.add_argument("--pilot", action="store_true")
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--working-tree", action="store_true")
    parser.add_argument("--versions", nargs="+", choices=("v1.1", "v1.2", "v1.2.1", "v1.2.2"),
                        default=["v1.1", "v1.2"])
    parser.add_argument("--workloads", nargs="+", choices=(
        "seq-read", "hot-read", "wide-read", "seq-write", "wide-write"))
    parser.add_argument("--budgets", nargs="+", choices=(
        "default", "1048576", "4194304", "12582912", "16777216"))
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    build = args.build_dir or Path(tempfile.mkdtemp(prefix="msfblkdev-mapping-bench-", dir="/tmp"))
    build = build.resolve()
    build.mkdir(exist_ok=True)
    print(f"build={build}", flush=True)
    versions = args.versions + (["working"] if args.working_tree else [])
    binaries = {version: prepare(repo, build, version) for version in versions}
    configurations = [(version, "default") for version in versions]
    for version in versions:
        if version != "v1.1":
            # The packed writer's 4 MiB buffer requires 1,024 mapping slots.
            sizes = (1, 4, 12, 16) if version == "v1.2" else (4, 12, 16)
            configurations += [(version, str(mib << 20)) for mib in sizes]
    workloads = ["seq-read", "hot-read", "wide-read", "seq-write", "wide-write"]
    if args.pilot:
        configurations = [(version, budget) for version, budget in configurations
                          if budget in ("default", str(16 << 20))]
        workloads = ["wide-read", "wide-write"]
    if args.workloads:
        workloads = args.workloads
    if args.budgets:
        configurations = [(version, budget) for version, budget in configurations
                          if version == "v1.1" or budget in args.budgets]
    cpu = min(os.sched_getaffinity(0))
    metadata = {
        "kernel": os.uname().release,
        "cpu": cpu,
        "tags": {v: checked(["git", "rev-parse", f"{'HEAD' if v == 'working' else v}^{{commit}}"], cwd=repo,
                            capture_output=True).stdout.strip() for v in binaries},
        "rustc": checked(["rustc", "--version"], capture_output=True).stdout.strip(),
        "logical_bytes": 4 << 30, "backing_bytes": 1 << 30,
        "write_buffer_bytes": 4 << 20, "checkpoint_after_bytes": 64 << 20,
        "build": str(build), "repetitions": args.repetitions,
        "workloads": workloads, "configurations": configurations,
        "filesystem": checked(["findmnt", "-T", str(repo), "-n", "-o", "SOURCE,FSTYPE,OPTIONS"],
                              capture_output=True).stdout.strip(),
    }
    if args.working_tree:
        source = build / "working/rust"
        digest = hashlib.sha256()
        for path in sorted(p for p in source.rglob("*") if p.is_file()):
            digest.update(str(path.relative_to(source)).encode() + b"\0" + path.read_bytes())
        metadata["working_source_sha256"] = digest.hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output, tempfile.TemporaryDirectory(
            prefix=".mapping-bench-", dir=repo) as fixture_dir:
        checked(["chattr", "+C", fixture_dir])
        metadata["fixture_attributes"] = checked(["lsattr", "-d", fixture_dir],
                                                 capture_output=True).stdout.strip()
        output.write(json.dumps({"metadata": metadata}) + "\n")
        rng = random.Random(74703)
        for repetition in range(1, args.repetitions + 1):
            jobs = [(version, budget, workload) for version, budget in configurations
                    for workload in workloads]
            rng.shuffle(jobs)
            for version, budget, workload in jobs:
                started = time.monotonic()
                result = checked(["taskset", "-c", str(cpu), str(binaries[version]),
                                  fixture_dir, workload, budget], capture_output=True, timeout=120)
                record = {"version": version, "budget": budget, "repetition": repetition,
                          "total_run_seconds": time.monotonic() - started}
                for field in result.stdout.strip().split():
                    key, value = field.split("=", 1)
                    record[key] = value if key == "workload" else int(value)
                output.write(json.dumps(record) + "\n")
                output.flush()
                print(f"PASS repetition={repetition} {version} cache={budget} {workload} "
                      f"iops={record['operations'] * 1e9 / record['elapsed_ns']:.0f}", flush=True)


if __name__ == "__main__":
    main()
