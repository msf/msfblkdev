"""Shared Linux scratch checks and owned QEMU/FUSE export lifetime."""

from contextlib import contextmanager
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import time


def disk_for(dev):
    node = (Path("/sys/dev/block") / f"{os.major(dev)}:{os.minor(dev)}").resolve(strict=True)
    disk = node.parent if (node / "partition").exists() else node
    if not str(disk).endswith(f"/block/{disk.name}") or "/virtual/" in str(disk):
        raise ValueError(f"need a plain VM disk or partition, found {node}")
    return disk


def check_scratch(scratch, device, required_bytes):
    if sys.platform != "linux" or os.geteuid() == 0:
        raise ValueError("run as a normal user inside the guest")
    st = device.stat()
    if not stat.S_ISBLK(st.st_mode) or scratch.stat().st_dev != st.st_rdev:
        raise ValueError(f"scratch filesystem is not on {device}")
    if disk_for(st.st_rdev) == disk_for(Path("/").stat().st_dev):
        raise ValueError("scratch and / share a disk")
    fstype = subprocess.check_output(["findmnt", "-nro", "FSTYPE", "-T", scratch],
                                     text=True, timeout=5).strip()
    if fstype != "ext4" or not os.access(scratch, os.W_OK | os.X_OK):
        raise ValueError("need a writable ext4 scratch directory")
    fs = os.statvfs(scratch)
    if fs.f_bavail * fs.f_frsize < required_bytes:
        raise ValueError("insufficient scratch space for probe write budget")
    return fstype


def command(image, export, rule, trace):
    debug = {"driver": "blkdebug", "node-name": "debug", "image": "base"}
    if rule is not None:
        debug["inject-error"] = [rule]
    args = ["qemu-storage-daemon"]
    if trace:
        args.extend(["--trace", "enable=blk_co_pwritev"])
    for node in ({"driver": "file", "node-name": "base", "filename": str(image),
                  "cache": {"direct": True, "no-flush": False}},
                 debug, {"driver": "raw", "node-name": "raw", "file": "debug"}):
        args.extend(["--blockdev", json.dumps(node)])
    return [*args, "--export", json.dumps({"type": "fuse", "id": "probe", "node-name": "raw",
                                          "writable": True, "allow-other": "off",
                                          "mountpoint": str(export)})]


@contextmanager
def mounted(image, export, rule=None, *, trace=False):
    mount_dev = export.stat().st_dev
    args = command(image, export, rule, trace)
    print(shlex.join(args), flush=True)
    daemon = subprocess.Popen(args)
    try:
        deadline = time.monotonic() + 5
        while export.stat().st_dev == mount_dev:
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(f"FUSE export did not mount; daemon status={daemon.poll()}")
            time.sleep(0.05)
        yield
        if daemon.poll() is not None:
            raise RuntimeError(f"daemon exited during caller I/O: {daemon.returncode}")
    finally:
        daemon.terminate()
        try:
            status = daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
            raise
    if status != 0 or export.stat().st_dev != mount_dev:
        raise RuntimeError(f"unclean daemon shutdown: {status}; retaining {export.parent}")
