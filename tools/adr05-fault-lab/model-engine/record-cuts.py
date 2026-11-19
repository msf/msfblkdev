#!/usr/bin/env python3
"""Model four survival subsets of a single format-4 append, then reopen independent copies."""

import argparse
import hashlib
from pathlib import Path
import struct
import subprocess
import sys
import tempfile

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))
sys.path.insert(0, str(LAB / "model"))
from medium import Medium
from qsd import check_scratch

BLOCK = 4096
SIZE = (2 + 12 * 32) * BLOCK


def append_offset(before, after):
    if len(before) != SIZE or len(after) != SIZE:
        raise ValueError("unexpected capture size")
    changed = [i for i in range(0, SIZE, BLOCK) if before[i:i + BLOCK] != after[i:i + BLOCK]]
    if len(changed) != 2 or changed[0] < 2 * BLOCK or changed[0] + BLOCK != changed[1]:
        raise ValueError(f"expected only two adjacent non-root blocks, got {changed}")
    payload, footer = changed
    if after[payload:footer] != b"\x33" * BLOCK or after[footer:footer + 8] != b"VBLF\x04\x01\0\0":
        raise ValueError("not the expected overwrite and format-4 data footer")
    vid, lsn, prev, self_block, lba = struct.unpack_from("<QQIII", after, footer + 8)
    old_footer = payload - BLOCK
    if (prev * BLOCK != old_footer or self_block * BLOCK != footer or lba != 1 or
            before[old_footer:old_footer + 6] != b"VBLF\x04\x01" or
            struct.unpack_from("<QQ", before, old_footer + 8) != (vid, lsn - 1)):
        raise ValueError("not a one-payload record linked to the prior durable footer")
    return payload


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("scratch", type=Path)
    p.add_argument("device", type=Path)
    p.add_argument("binary", type=Path)
    args = p.parse_args()
    scratch = args.scratch.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    check_scratch(scratch, args.device, 12 * SIZE)
    work = Path(tempfile.mkdtemp(prefix="adr05-record-", dir=scratch))
    print(f"fixtures={work}", flush=True)
    capture = work / "capture"
    subprocess.run([binary, "capture", capture], check=True, timeout=15)
    before, after = ((capture / name).read_bytes() for name in ("before", "after"))
    offset = append_offset(before, after)
    print(f"append offset={offset} bytes={2 * BLOCK} before={hashlib.sha256(before).hexdigest()} "
          f"after={hashlib.sha256(after).hexdigest()}", flush=True)
    medium = Medium(before)
    if medium.write(offset, after[offset:offset + 2 * BLOCK]) != 2 * BLOCK:
        raise ValueError("model did not accept the captured write")
    for mask, case in enumerate(("neither", "payload", "footer", "both")):
        pieces = [(1, i * BLOCK, BLOCK) for i in (0, 1) if mask & (1 << i)]
        image = medium.crash_image(pieces)
        if image[:offset] != before[:offset] or image[offset + 2 * BLOCK:] != before[offset + 2 * BLOCK:]:
            raise ValueError("unrelated bytes changed")
        path = work / case
        with path.open("xb") as f:
            f.write(image)
        print(f"case={case} surviving={pieces} copy_sha256={hashlib.sha256(image).hexdigest()}", flush=True)
        subprocess.run([binary, case, path], check=True, timeout=15)
    print("record_cuts=PASS")


if __name__ == "__main__":
    main()
