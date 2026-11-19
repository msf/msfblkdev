#!/usr/bin/env python3
"""Exercise format-4 close through a real FUSE/blkdebug export. Guest only."""

import argparse
import errno
import hashlib
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

from qsd import check_scratch, mounted


BLOCK = 4096
# At format, both descriptor LSNs are equal. Green wins the tie on open;
# the first publish replaces blue (physical block 1). blkdebug uses 512-byte sectors.
DESCRIPTOR_SECTOR = BLOCK // 512


def client(binary, mode, path):
    cmd = [str(binary), mode, str(path)]
    print(shlex.join(cmd), flush=True)
    subprocess.run(cmd, check=True, timeout=15)


def roots(image):
    with image.open("rb") as f:
        green, blue = f.read(BLOCK), f.read(BLOCK)
    if len(blue) != BLOCK:
        raise ValueError("missing root blocks")
    return [hashlib.sha256(block).hexdigest() for block in (green, blue)]


def run_case(work, binary, case):
    start = time.monotonic()
    work.mkdir(mode=0o700)
    image, export, copy = (work / name for name in ("image", "export", "recovery-copy"))
    client(binary, "format-new", image)
    before = roots(image)
    fault = case == "descriptor-eio"
    rule = {"event": "write_aio", "iotype": "write", "sector": DESCRIPTOR_SECTOR,
            "once": True, "errno": errno.EIO} if fault else None
    export.touch(exist_ok=False)
    with mounted(image, export, rule, trace=True):
        client(binary, case, export)
    after = roots(image)
    print(f"case={case} roots_before={before} roots_after={after}", flush=True)
    if (before[0] != after[0]) or ((before[1] == after[1]) != fault):
        raise ValueError("wrong descriptor changed, or rule did not suppress blue write")
    # Reopen can write. Retain the unmodified failed image for offline inspection.
    shutil.copyfile(image, copy)
    client(binary, "verify-copy", copy)
    print(f"case={case} result=PASS elapsed_s={time.monotonic() - start:.3f}", flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("scratch", type=Path)
    p.add_argument("device", type=Path)
    p.add_argument("client", type=Path, help="built adr05-engine example")
    args = p.parse_args()
    scratch = args.scratch.resolve(strict=True)
    binary = args.client.resolve(strict=True)
    # Two cases, an image and a copy each, currently <2 MiB per image.
    check_scratch(scratch, args.device, 8 * 1024**2)
    work = Path(tempfile.mkdtemp(prefix="adr05-engine-", dir=scratch))
    print(f"fixtures={work} device={args.device} client={binary}", flush=True)
    for case in ("control", "descriptor-eio"):
        run_case(work / case, binary, case)
    print("engine_checkpoint=PASS")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        sys.exit(f"engine_checkpoint=INCOMPLETE: {exc}")
