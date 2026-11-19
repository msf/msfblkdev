#!/usr/bin/env python3
"""Run only inside a disposable Linux VM, as the test user."""

import argparse
import errno
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

from qsd import check_scratch, mounted


HERE = Path(__file__).resolve().parent
CALLER = HERE / "probe-io"
BLOCK = 4096
CASES = ("normal", "write", "fsync")
# One normal pwrite and at most two per fault case, counting the rejected write.
WRITE_BUDGET = 5 * BLOCK


def preflight(scratch, device):
    fstype = check_scratch(scratch, device, WRITE_BUDGET)
    for node in ("/dev/fuse", "/dev/ublk-control"):
        os.close(os.open(node, os.O_RDWR | os.O_CLOEXEC))
    subprocess.run([CALLER, "preflight"], check=True, timeout=5)
    subprocess.run(["qemu-storage-daemon", "--version"], check=True, timeout=5)
    print(f"preflight=ok uid={os.getuid()} scratch={scratch} device={device} "
          f"fstype={fstype} image_bytes={BLOCK} max_pwrite_bytes={WRITE_BUDGET}", flush=True)


def rule_for(case):
    if case == "normal":
        return None
    event, op = ("write_aio", "write") if case == "write" else ("flush_to_disk", "flush")
    return {"event": event, "iotype": op, "errno": errno.EIO, "once": True}


def run_case(case, scratch):
    start = time.monotonic()
    work = Path(tempfile.mkdtemp(prefix=f"adr05-{case}-", dir=scratch))
    print(f"case={case} fixture={work}", flush=True)
    image, export = work / "image", work / "export"
    with image.open("xb") as backing:
        backing.truncate(BLOCK)
    export.touch(exist_ok=False)
    with mounted(image, export, rule_for(case)):
        subprocess.run([CALLER, case, export], check=True, timeout=10)
    pattern = b"\x5a" if case == "fsync" else b"\xa5"
    with image.open("rb") as backing:
        if backing.read(BLOCK + 1) != pattern * BLOCK:
            raise RuntimeError(f"base image mismatch; retaining {work}")
    export.unlink()
    image.unlink()
    work.rmdir()
    print(f"case={case} base_match=1 elapsed_s={time.monotonic() - start:.3f} result=PASS",
          flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scratch", type=Path, help="writable directory on the disposable disk")
    parser.add_argument("device", type=Path, help="expected ext4 device (identity check only)")
    args = parser.parse_args()
    scratch = args.scratch.resolve(strict=True)
    preflight(scratch, args.device)
    for case in CASES:
        run_case(case, scratch)
    print("injector=PASS")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        sys.exit(f"injector=INCOMPLETE: {exc}")
