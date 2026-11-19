#!/usr/bin/env python3
"""Read-only, independent format-4 reachability audit for small offline images."""
import argparse
import ctypes
import ctypes.util
import hashlib
import json
import os
import stat
import struct
import sys

B = 4096
# Limits on a single offline experiment, not persistent-format limits.
MAX_BYTES = 64 << 20
MAX_VISITS = 262144
MAP, FANOUT, TABLE = 336, 1008, 448
FLAGS = 40 + TABLE * 8


def require(condition, message):
    if not condition:
        raise ValueError(message)


def ceil(n, divisor):
    return (n + divisor - 1) // divisor


def u32(b, off):
    return struct.unpack_from('<I', b, off)[0]


def u64(b, off):
    return struct.unpack_from('<Q', b, off)[0]


def checksum_function():
    name = os.environ.get('XXHASH_LIB') or ctypes.util.find_library('xxhash')
    if not name:
        raise RuntimeError('libxxhash unavailable: set XXHASH_LIB to the library path')
    fn = ctypes.CDLL(name).XXH3_64bits_withSeed
    fn.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_uint64]
    fn.restype = ctypes.c_uint64
    return lambda b, seed=0: fn(b, len(b), seed)


def pointers(b, off, total, count):
    for i in range(total):
        ptr = u32(b, off + 4 * i)
        if ptr:
            require(i < count, 'metadata: pointer outside page shape')
            yield i, ptr


class Root:
    def __init__(self, image, slot, hash64):
        self.image, self.slot, self.hash64 = image, slot, hash64
        self.visits = 0
        self.meta, self.replay = set(), set()
        self.mapping, self.heads = {}, {}
        d = self.d = image[slot*B:(slot+1)*B]
        self.sealed(d, slot)
        require(d[:8] == b'VBLC\x04' + bytes([slot, 0, 0]) and u32(d, 16) == B,
                'metadata: descriptor identity')
        self.vol, self.lbas, self.blocks = u64(d, 8), u32(d, 20), u64(d, 24)
        self.lsn, self.segsize = u64(d, 32), u32(d, 48)
        require(0 < self.lsn < 2**64-1 and 0 < self.lbas <= 2**31 and self.segsize >= 4
                and self.blocks * B <= len(image), 'metadata: descriptor geometry')
        require(not any(d[40:48] + d[4072:4088]), 'metadata: descriptor padding')
        self.segs = (self.blocks-2) // self.segsize
        require(3 <= self.segs <= 930*TABLE and u32(d, 348) == ceil(self.segs, TABLE),
                'metadata: segment count')
        self.states = [0] * self.segs

    def sealed(self, b, ptr):
        require(u64(b, 4088) == self.hash64(b[:4088] + bytes(8)), f'metadata checksum: block={ptr}')

    def seg(self, ptr):
        require(2 <= ptr < 2 + self.segsize*self.segs, f'metadata: out-of-range block={ptr}')
        return (ptr-2) // self.segsize

    def block(self, ptr):
        self.seg(ptr)
        self.visits += 1
        if self.visits > MAX_VISITS:
            raise RuntimeError('work budget exhausted')
        return self.image[ptr*B:(ptr+1)*B]

    def record(self, ptr, b, kind):
        self.sealed(b, ptr)
        require(b[:8] == b'VBLF\x04' + bytes([kind, 0, 0]) and u64(b, 8) == self.vol
                and u32(b, 28) == ptr and u64(b, 16) > 0, f'metadata: identity at block={ptr}')
        prev = u32(b, 24)
        start = prev+1 if prev and self.seg(prev) == self.seg(ptr) else 2+self.seg(ptr)*self.segsize
        if kind == 1:
            require(1 <= ptr-start <= 338, 'metadata: invalid footer span')
        else:
            require(ptr == start and self.hash64(b[:40]) == u64(b, 4072) and not any(b[4080:4088]),
                    f'metadata: record framing at block={ptr}')

    def page(self, ptr, kind, page_id):
        require(ptr not in self.meta, f'metadata: shared page at block={ptr}')
        self.meta.add(ptr)
        b = self.block(ptr)
        self.record(ptr, b, kind)
        require(u32(b, 32) == page_id and u64(b, 16) <= self.lsn, 'metadata: page ID/LSN')
        return b

    def tables(self):
        for pid, ptr in pointers(self.d, 352, 930, ceil(self.segs, TABLE)):
            b = self.page(ptr, 3, pid)
            require(not any(b[36:40]), 'metadata: table padding')
            for i in range(TABLE):
                seg, flags, seal = pid*TABLE+i, b[FLAGS+i], u64(b, 40+i*8)
                require(flags in (0, 1, 129, 2, 3) and bool(seal) == (flags in (2, 3))
                        and seal <= self.lsn, 'metadata: segment flags/seal')
                require(seg < self.segs or flags == 0, 'metadata: segment outside table')
                if seg < self.segs:
                    self.states[seg] = flags
                if flags in (1, 129):
                    head = flags >> 7
                    require(head not in self.heads, 'metadata: duplicate head')
                    prev = u32(self.d, 52+head*4)
                    require(head or prev, 'metadata: missing first boundary')
                    cursor = prev+1 if prev and self.seg(prev) == seg else 2+self.segsize*seg
                    require(self.seg(cursor) == seg, 'metadata: head at end of segment')
                    self.heads[head] = (cursor, prev)
        require(len(self.heads) == 2, 'metadata: missing open head')

    def indexes(self):
        pages = ceil(self.lbas, MAP)
        dirs = ceil(pages, FANOUT)
        direct = dict(pointers(self.d, 60, 64, dirs))
        for i, ptr in pointers(self.d, 316, 8, ceil(max(0, dirs-64), FANOUT)):
            b = self.page(ptr, 8, i)
            require(u32(b, 36) == 2, 'metadata: indirect level')
            first = 64 + i*FANOUT
            for j, child in pointers(b, 40, FANOUT, dirs-first):
                direct[first+j] = child
        for i, ptr in direct.items():
            b = self.page(ptr, 8, i)
            require(u32(b, 36) == 1, 'metadata: direct level')
            for j, child in pointers(b, 40, FANOUT, pages-i*FANOUT):
                pid = i*FANOUT+j
                m = self.page(child, 2, pid)
                require(not any(m[36:40]), 'metadata: mapping page padding')
                for k in range(MAP):
                    lba, phys, check = pid*MAP+k, u32(m, 40+k*4), u64(m, 1384+k*8)
                    if phys:
                        self.seg(phys)
                        require(lba < self.lbas, 'metadata: LBA outside volume')
                        require(len(self.mapping) < len(self.image)//B, 'metadata: more mappings than physical blocks')
                        self.mapping[lba] = (phys, check)
                    else:
                        require(not check, 'metadata: checksum with no mapping')

    def allocated(self, ptr):
        require(self.states[self.seg(ptr)] != 0, f'unsafe reuse: block={ptr} in free segment={self.seg(ptr)}')

    def probe(self, head, expected):
        start, prev = self.heads[head]
        end = 2 + (1+self.seg(start))*self.segsize
        for pos in range(start, min(start+339, end)):
            b = self.block(pos)
            if b[:4] != b'VBLF' or u64(b, 8) != self.vol or u64(b, 16) < expected:
                continue
            if u32(b, 24) != prev or u32(b, 28) != pos:
                continue
            kind = b[5]
            if (pos != start) != (kind == 1):
                continue
            require(kind in (1, 2, 3, 4, 5, 7, 8), 'metadata: reserved kind in chain')
            self.record(pos, b, kind)
            return pos, b
        return None

    def resize_or_discard(self, b):
        if b[5] == 5:
            before, after = u64(b, 32), u64(b, 40)
            require(before == self.blocks and before < after <= len(self.image)//B
                    and not any(b[48:4072]), 'metadata: invalid growth')
            self.blocks, self.segs = after, (after-2)//self.segsize
            self.states.extend([0] * (self.segs-len(self.states)))
            return
        count = u32(b, 32)
        require(1 <= count <= 504 and not any(b[36:40]), 'metadata: discard range count')
        for i in range(504):
            start, n = u32(b, 40+i*4), u32(b, 2056+i*4)
            if i >= count:
                require(start == n == 0, 'metadata: unused discard range')
                continue
            require(0 < n <= self.lbas-start, 'metadata: invalid discard range')
            self.visits += len(self.mapping)
            if self.visits > MAX_VISITS:
                raise RuntimeError('discard work budget exhausted')
            for lba in tuple(self.mapping):
                if start <= lba < start+n:
                    del self.mapping[lba]

    def apply(self, head, pos, b):
        start, _ = self.heads[head]
        self.replay.add(pos)
        kind = b[5]
        if kind == 1:
            count = pos-start
            require((pos-2) % self.segsize != self.segsize-1, 'metadata: footer in link slot')
            seen = set()
            for i in range(count):
                lba = u32(b, 32+4*i)
                require(lba < self.lbas and lba not in seen, 'metadata: invalid footer LBA')
                seen.add(lba)
                require(lba in self.mapping or len(self.mapping) < len(self.image)//B,
                        'metadata: more mappings than physical blocks')
                self.mapping[lba] = (start+i, u64(b, 1384+i*8))
            require(not any(b[32+count*4:1384] + b[1384+count*8:4088]), 'metadata: footer padding')
        if kind == 4:
            target = u32(b, 32)
            require(b[36] == head and target < self.segs and self.states[target] in (0, 3)
                    and not any(b[37:4072]), 'metadata: invalid segment link')
            self.states[self.seg(pos)] = 2
            self.states[target] = 1 + head*128
            self.heads[head] = (2+target*self.segsize, pos)
        else:
            if kind in (5, 7):
                self.resize_or_discard(b)
            require(self.seg(pos+1) == self.seg(pos), 'metadata: missing segment link')
            self.heads[head] = (pos+1, pos)

    def walk(self):
        self.tables()
        self.indexes()
        for ptr in self.meta:
            self.allocated(ptr)
        for ptr, _ in self.mapping.values():
            self.allocated(ptr)
        self.last = self.lsn
        for _ in range(len(self.image)//B):
            candidates = [(h, p) for h in (0, 1) if (p := self.probe(h, self.last+1))]
            if not candidates:
                break
            nexts = [(h, pos, b) for h, (pos, b) in candidates if u64(b, 16) == self.last+1]
            require(len(nexts) == 1, 'metadata: ambiguous or missing global LSN')
            self.apply(*nexts[0])
            self.last += 1
        else:
            raise RuntimeError('replay exceeds image block count')
        self.required = self.meta | self.replay
        for lba, (ptr, check) in self.mapping.items():
            self.allocated(ptr)
            require(ptr not in self.required, f'unsafe alias: LBA={lba} block={ptr}')
            data = struct.pack('<II', lba, ptr) + self.block(ptr)
            require(self.hash64(data, self.vol) == check, f'payload checksum: LBA={lba} block={ptr}')
            self.required.add(ptr)
        return {'slot': self.slot, 'checkpoint_lsn': self.lsn, 'last_lsn': self.last,
                'checkpoint_pages': len(self.meta), 'replayed': len(self.replay),
                'mapped_lbas': len(self.mapping), 'visits': self.visits}


def audit(image):
    require(2*B <= len(image) <= MAX_BYTES and len(image) % B == 0, 'image size/alignment')
    hash64 = checksum_function()
    roots, reports = [], []
    for slot in (0, 1):
        try:
            root = Root(image, slot, hash64)
            reports.append(root.walk())
            roots.append(root)
        except ValueError as exc:
            reports.append({'slot': slot, 'error': str(exc)})
    if len(roots) == 2:
        old, new = sorted(roots, key=lambda r: r.lsn)
        require((old.vol, old.lbas, old.segsize) == (new.vol, new.lbas, new.segsize),
                'metadata: roots disagree on volume')
        if old.lsn == new.lsn:
            require(old.d[8:4088] == new.d[8:4088], 'metadata: equal-LSN roots disagree')
        for ptr in sorted(old.required):
            new.allocated(ptr)
    return reports


def read_image(path):
    # Nonblocking open prevents a FIFO argument from blocking before fstat.
    entry = os.lstat(path)
    require(stat.S_ISREG(entry.st_mode), 'need a regular image')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as f:
        st = os.fstat(f.fileno())
        require(os.path.samestat(st, entry) and 2*B <= st.st_size <= MAX_BYTES, 'need a bounded regular image')
        image = f.read(st.st_size+1)
        require(len(image) == st.st_size, 'image changed during read')
        return image


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image')
    try:
        image = read_image(parser.parse_args().image)
        reports = audit(image)
        print(json.dumps({'sha256': hashlib.sha256(image).hexdigest(), 'roots': reports}))
        sys.exit(int(any('error' in report for report in reports)))
    except ValueError as exc:
        print(f'FAIL: {exc}', file=sys.stderr)
        sys.exit(1)
    except (OSError, RuntimeError, AttributeError) as exc:
        print(f'INCOMPLETE: {exc}', file=sys.stderr)
        sys.exit(2)
