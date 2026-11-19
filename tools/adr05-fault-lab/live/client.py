"""Caller for the live checkpoint B gate. Open only the new daemon's validated ublk node."""
import errno
import mmap
import os
from pathlib import Path
import re
import stat
import sys

BLOCK = 4096


def exercise(fd, armed, fault):
    with mmap.mmap(-1, BLOCK) as buf:
        for lba, byte in ((1, 0x17), (5, 0x53), (1, 0xab)):
            buf[:] = bytes([byte]) * BLOCK
            if os.pwrite(fd, buf, lba * BLOCK) != BLOCK:
                raise ValueError("short setup write")
            print(f"pwrite lba={lba} byte={byte:#x} ok")
        os.fsync(fd)
        print("client fsync ok; history: LBA1=ab LBA5=53 others=00", flush=True)
        # Refill the log until *foreground admission* has to release a victim.
        # Repeating the last flushed value keeps the oracle independent of the cut index.
        buf[:] = b'\x53' * BLOCK
        for index in range(512):
            try:
                count = os.pwrite(fd, buf, 5 * BLOCK)
            except OSError as exc:
                print(f"pressure pwrite index={index} errno={exc.errno}", flush=True)
                if not fault or exc.errno != errno.EIO or not armed.exists():
                    raise
                return
            if count != BLOCK:
                raise ValueError("short pressure write")
            if armed.exists():
                if fault:
                    raise ValueError("expected EIO from the in-flight ublk write")
                print(f"pressure pwrite index={index} ok after B", flush=True)
                return
        raise ValueError("no checkpoint B in write budget")


def main(node, armed, case):
    if os.geteuid() == 0 or not re.fullmatch(r'/dev/ublkb\d+', str(node)):
        raise ValueError("normal user's ublk device required")
    fd = os.open(node, os.O_RDWR | os.O_DIRECT | os.O_NOFOLLOW | os.O_EXCL)
    try:
        st = os.fstat(fd)
        sysnode = Path('/sys/class/block') / node.name
        if (not stat.S_ISBLK(st.st_mode) or st.st_uid != os.getuid()
                or (sysnode / 'dev').read_text().strip() != f'{os.major(st.st_rdev)}:{os.minor(st.st_rdev)}'
                or int((sysnode / 'size').read_text()) * 512 != 8 * BLOCK):
            raise ValueError('unexpected ublk identity or size')
        exercise(fd, armed, case != 'control')
    finally:
        os.close(fd)


if __name__ == '__main__':
    main(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3])
