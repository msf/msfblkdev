"""Bounded fio replay through unchanged release daemons; private regular-file backings."""
import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import signal
import stat
import subprocess
import tarfile
import tempfile
import time

from run import checked, prepare


def trace(workload):
    sequential = workload.startswith("seq-")
    writing = workload.endswith("write")
    count = (16384 if sequential else 4096) if writing else 32768
    seed = 0x6A09E667
    addresses = []
    for i in range(count):
        seed ^= (seed << 13) & 0xFFFFFFFF
        seed ^= seed >> 17
        seed ^= (seed << 5) & 0xFFFFFFFF
        addresses.append(i % 16384 if sequential else (seed % 3277) * 320)
    warm = list(range(16384)) if sequential else [page * 320 for page in range(3277)]
    return addresses, warm, (256 if sequential else 32) if writing else 0


def device_identity(device):
    match = re.fullmatch(r"/dev/ublkb(0|[1-9][0-9]*)", str(device))
    if not match:
        raise RuntimeError(f"invalid readiness path: {device}")
    info = device.stat()
    node = Path("/sys/block") / device.name
    if not stat.S_ISBLK(info.st_mode):
        raise RuntimeError("readiness path is not a block device")
    number = f"{os.major(info.st_rdev)}:{os.minor(info.st_rdev)}"
    if (node / "dev").read_text().strip() != number:
        raise RuntimeError("device and sysfs numbers differ")
    if int((node / "size").read_text()) != (4 << 30) // 512:
        raise RuntimeError("unexpected device geometry")
    if int((node / "queue/logical_block_size").read_text()) != 4096:
        raise RuntimeError("unexpected logical block size")
    if any(line.split()[2] == number for line in Path("/proc/self/mountinfo").read_text().splitlines()):
        raise RuntimeError("benchmark device is mounted")
    control = Path(f"/sys/class/ublk-char/ublkc{match[1]}").stat()
    return info.st_rdev, control.st_dev, control.st_ino


@contextmanager
def daemon(binary, backing, budget, logs, phase, cpu):
    before = set(Path("/sys/block").glob("ublkb*"))
    command = ["taskset", "-c", str(cpu), str(binary), "serve", str(backing), "-1",
               "--write-buffer-bytes", str(4 << 20)]
    if budget != "default":
        command += ["--mapping-cache-bytes", budget]
    stdout_path = logs / f"{phase}.stdout"
    device = None
    with stdout_path.open("x") as stdout, (logs / f"{phase}.stderr").open("x") as stderr:
        child = subprocess.Popen(command, stdout=stdout, stderr=stderr,
                                 env=dict(os.environ, BLOCK_STORAGE_LOG="info"))
        (logs / f"{phase}.process.json").write_text(
            json.dumps(dict(pid=child.pid, command=command)) + "\n")
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if child.poll() is not None:
                    raise RuntimeError(f"daemon exited early: {child.returncode}; see {logs}")
                ready = stdout_path.read_text()
                if ready.endswith("\n"):
                    candidate = Path(ready.strip())
                    if Path("/sys/block") / candidate.name in before:
                        raise RuntimeError("daemon readiness names a pre-existing device")
                    try:
                        identity = device_identity(candidate)
                        with candidate.open("r+b", buffering=0):
                            pass
                        device = candidate
                        break
                    except (FileNotFoundError, PermissionError):
                        pass
                time.sleep(0.05)
            if device is None:
                raise TimeoutError(f"daemon readiness timed out; see {logs}")
            yield device, child.pid, identity
        finally:
            if child.poll() is None:
                child.send_signal(signal.SIGTERM)
            code = child.wait(timeout=30)
            if code != 0:
                raise RuntimeError(f"daemon shutdown failed: {code}; preserved {backing}")
            if device and (Path("/sys/block") / device.name).exists():
                raise RuntimeError(f"device still present after shutdown: {device}")


def fio(device, identity, addresses, writing, flush_every, fixture, logs, phase, cpu):
    if device_identity(device) != identity:
        raise RuntimeError("device identity changed before fio")
    path = fixture / f"{phase}.iolog"
    action = "write" if writing else "read"
    with path.open("x") as output:
        output.write(f"fio version 2 iolog\n{device} add\n{device} open\n")
        for i, lba in enumerate(addresses):
            output.write(f"{device} {action} {lba * 4096} 4096\n")
            if flush_every and (i + 1) % flush_every == 0:
                output.write(f"{device} sync 0 0\n")
        if writing and len(addresses) % flush_every:
            output.write(f"{device} sync 0 0\n")
        output.write(f"{device} close\n")
    command = ["taskset", "-c", str(cpu), "fio", f"--name={phase}", "--ioengine=psync",
               "--iodepth=1", "--direct=1", "--invalidate=0", "--allow_file_create=0",
               "--bs=4096", "--replay_no_stall=1", f"--read_iolog={path}",
               "--verify=crc32c", "--verify_fatal=1", "--verify_header_seed=0",
               "--verify_write_sequence=0", "--verify_state_save=0", "--do_verify=0", "--output-format=json",
               f"--output={logs / (phase + '.json')}"]
    if not writing:
        command.append("--readonly")
    checked(command, capture_output=True, timeout=120)
    result = json.loads((logs / f"{phase}.json").read_text())
    job, = result["jobs"]
    if job["error"] or job[action]["io_bytes"] != len(addresses) * 4096:
        raise RuntimeError(f"fio error or incomplete replay: {logs / phase}")
    if writing and job["sync"]["total_ios"] != (len(addresses) + flush_every - 1) // flush_every:
        raise RuntimeError("fio did not execute the requested flush count")
    return job


def run_case(binary, version, budget, workload, repetition, repo, output, cpus):
    logs = output / f"{version}-{budget}-{workload}-{repetition}"
    logs.mkdir()
    fixture = Path(tempfile.mkdtemp(prefix=".mapping-ublk-bench-", dir=repo))
    checked(["chattr", "+C", str(fixture)])
    backing = fixture / "backing.img"
    geometry = [str(1 << 30), str(4 << 30)] if version == "v1.1" else [
        "--size-mib", "4096", "--prealloc-mib", "1024"]
    checked([str(binary), "format", str(backing), *geometry],
            capture_output=True, timeout=30)
    addresses, warm, flush_every = trace(workload)
    if not flush_every:
        with daemon(binary, backing, budget, logs, "setup-daemon", cpus[0]) as (device, _, identity):
            fio(device, identity, warm, True, 256, fixture, logs, "setup", cpus[1])
    with daemon(binary, backing, budget, logs, "measured-daemon", cpus[0]) as (device, pid, identity):
        if not flush_every:
            fio(device, identity, warm, False, 0, fixture, logs, "warm", cpus[1])
        rss = next(line.split()[1] for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()
                   if line.startswith("Rss:"))
        job = fio(device, identity, addresses, bool(flush_every), flush_every,
                  fixture, logs, "measured", cpus[1])
    if flush_every:
        with daemon(binary, backing, budget, logs, "verify-daemon", cpus[0]) as (device, _, identity):
            fio(device, identity, sorted(set(addresses)), False, 0, fixture, logs, "verify", cpus[1])
    allocated = backing.stat().st_blocks * 512
    if allocated >= 512 << 20:
        raise RuntimeError("backing allocation exceeds the bounded benchmark ceiling")
    record = dict(version=version, budget=budget, workload=workload, repetition=repetition,
                  rss_bytes=int(rss) * 1024, allocated_bytes=allocated, job=job)
    (logs / "result.json").write_text(json.dumps(record) + "\n")
    shutil.rmtree(fixture)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pilot", action="store_true")
    parser.add_argument("--working-tree", action="store_true")
    parser.add_argument("--versions", nargs="+", choices=("v1.1", "v1.2", "v1.2.1", "v1.2.2"),
                        default=["v1.1", "v1.2"])
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    args.output.mkdir()
    build = args.build_dir.resolve()
    build.mkdir(exist_ok=True)
    versions = args.versions + (["working"] if args.working_tree else [])
    for version in versions:
        prepare(repo, build, version)
        with (build / f"daemon-{version}.log").open("w") as log:
            checked(["cargo", "build", "--offline", "--locked", "--release", "--bin", "block-storage-ublk"],
                    cwd=build / version / "rust", timeout=120, stdout=log, stderr=log,
                    env=dict(os.environ, CARGO_TARGET_DIR=str(build / f"target-{version}")))
    cpus = sorted(os.sched_getaffinity(0))[:2]
    if len(cpus) != 2:
        raise RuntimeError("benchmark needs two allowed CPUs")
    binaries = {v: build / f"target-{v}/release/block-storage-ublk"
                for v in versions}
    metadata = dict(kernel=os.uname().release, cpus=cpus, logical_bytes=4 << 30,
                    backing_bytes=1 << 30, repetitions=1 if args.pilot else 3,
                    workloads=["wide-write"] if args.pilot else ["seq-read", "wide-read", "seq-write", "wide-write"],
                    fio=checked(["fio", "--version"], capture_output=True).stdout.strip(),
                    binaries={v: dict(path=str(p), sha256=hashlib.sha256(p.read_bytes()).hexdigest())
                              for v, p in binaries.items()},
                    tags={v: checked(["git", "rev-parse", f"{'HEAD' if v == 'working' else v}^{{commit}}"], cwd=repo,
                                     capture_output=True).stdout.strip() for v in binaries})
    configurations = [(version, "default") for version in versions]
    configurations += [(version, str(16 << 20)) for version in versions if version != "v1.1"]
    if args.working_tree:
        digest = hashlib.sha256()
        source = build / "working/rust"
        for path in sorted(p for p in source.rglob("*") if p.is_file()):
            digest.update(str(path.relative_to(source)).encode() + b"\0" + path.read_bytes())
        metadata["working_source_sha256"] = digest.hexdigest()
    metadata["configurations"] = configurations
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    workloads = ["wide-write"] if args.pilot else ["seq-read", "wide-read", "seq-write", "wide-write"]
    rng = random.Random(74703)
    with (args.output / "results.jsonl").open("x") as output:
        for repetition in range(1, metadata["repetitions"] + 1):
            jobs = [(v, b, w) for v, b in configurations for w in workloads]
            rng.shuffle(jobs)
            for version, budget, workload in jobs:
                record = run_case(binaries[version], version, budget, workload, repetition,
                                  repo, args.output, cpus)
                output.write(json.dumps(record) + "\n")
                output.flush()
                action = "write" if workload.endswith("write") else "read"
                print(f"PASS {version} {budget} {workload} repetition={repetition} "
                      f"iops={record['job'][action]['iops']:.0f}", flush=True)
    directories = sorted(path for path in args.output.iterdir() if path.is_dir())
    with tarfile.open(args.output / "raw-logs.tar.gz", "x:gz") as archive:
        for path in directories:
            archive.add(path, arcname=path.name)
    for path in directories:
        shutil.rmtree(path)


if __name__ == "__main__":
    main()
