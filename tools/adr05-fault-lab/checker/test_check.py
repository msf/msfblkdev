"""Construct exact, isolated on-disk reference graphs; don't call production codecs."""
import hashlib
import struct
import unittest

from check import B, FLAGS, audit, checksum_function

VOL = 17
LBA = 336*1008*64   # First LBA reached through a descriptor's indirect index.


def put(block, offset, fmt, *values):
    struct.pack_into('<'+fmt, block, offset, *values)


def two_roots(free_victim=False, damage=None):
    # 4 segments of 16 blocks. The old checkpoint still needs block 34 in segment 2.
    # The new checkpoint maps the same LBA to block 5 in segment 0.
    blocks = [bytearray(B) for _ in range(66)]
    h = checksum_function()
    for slot, first, payload in ((0, 6, 34), (1, 10, 5)):
        blocks[payload][:] = b'x'*B
        mapping, direct, indirect, table = blocks[first:first+4]
        put(mapping, 32, 'I', 64*1008)
        put(mapping, 40, 'I', payload)
        put(mapping, 1384, 'Q', h(struct.pack('<II', LBA, payload)+blocks[payload], VOL))
        for idx, page, level, ptr in [(direct, 64, 1, first), (indirect, 0, 2, first+1)]:
            put(idx, 32, 'III', page, level, ptr)
        put(table, FLAGS, 'BB', 1, 129)
        if not free_victim or slot == 0:
            put(table, FLAGS+2, 'B', 2)
            put(table, 40+2*8, 'Q', 3)
        for i, kind in enumerate([2, 8, 8, 3]):
            pos = first+i
            b = blocks[pos]
            put(b, 0, '4sBBHQ QII', b'VBLF', 4, kind, 0, VOL, pos, pos-1, pos)
            put(b, 4072, 'Q', h(bytes(b[:40])))
            put(b, 4088, 'Q', h(bytes(b)))
        root = blocks[slot]
        put(root, 0, '4sBBHQIIQQ', b'VBLC', 4, slot, 0, VOL, B, LBA+1, 66, first+3)
        put(root, 48, 'II', 16, first+3)
        put(root, 316, 'I', first+2)
        put(root, 348, 'II', 1, first+3)
        put(root, 4088, 'Q', h(bytes(root)))
    if damage is not None:
        blocks[damage][50] ^= 1
    return b''.join(blocks)


def two_head_image():
    # Append three one-block records in global LSN order, alternating the two heads.
    image = bytearray(two_roots()) + bytearray(16*B)
    h = checksum_function()
    for pos, prev, lsn, kind in [(14, 13, 14, 4), (18, 0, 15, 7), (50, 14, 16, 5)]:
        block = bytearray(B)
        put(block, 0, '4sBBHQQII', b'VBLF', 4, kind, 0, VOL, lsn, prev, pos)
        if kind == 4:
            put(block, 32, 'IB', 3, 0)  # User opens segment 3.
        elif kind == 7:
            put(block, 32, 'I', 1)
            put(block, 40, 'I', LBA)
            put(block, 2056, 'I', 1)
        else:
            put(block, 32, 'QQ', 66, 82)
        put(block, 4072, 'Q', h(bytes(block[:40])))
        put(block, 4088, 'Q', h(bytes(block)))
        image[pos*B:(pos+1)*B] = block
    return bytes(image)


class Checker(unittest.TestCase):
    def test_both_heads_link_discard_growth(self):
        reports = audit(two_head_image())
        self.assertEqual([r['last_lsn'] for r in reports], [16, 16])
        self.assertEqual([r['replayed'] for r in reports], [7, 3])
        self.assertEqual([r['mapped_lbas'] for r in reports], [0, 0])

    def test_two_distinct_roots_and_unreferenced_damage(self):
        clean = two_roots()
        no_hit = two_roots(damage=65)
        want = [(9, 4, 1), (13, 4, 1)]
        for image in [clean, no_hit]:
            reports = audit(image)
            self.assertEqual([(r['checkpoint_lsn'], r['checkpoint_pages'], r['mapped_lbas'])
                              for r in reports], want)
            self.assertEqual([r['replayed'] for r in reports], [4, 0])
        self.assertNotEqual(hashlib.sha256(clean).digest(), hashlib.sha256(no_hit).digest())

    def test_new_root_frees_segment_still_referenced_by_old_root(self):
        with self.assertRaisesRegex(ValueError, 'unsafe reuse: block=34 in free segment=2'):
            audit(two_roots(free_victim=True))

    def test_overwrite_old_payload(self):
        old, new = audit(two_roots(damage=34))
        self.assertEqual(old['error'], f'payload checksum: LBA={LBA} block=34')
        self.assertNotIn('error', new)

    def test_damage_metadata(self):
        old, new = audit(two_roots(damage=7))
        self.assertEqual(old['error'], 'metadata checksum: block=7')
        self.assertNotIn('error', new)

    def test_input_size_bound(self):
        with self.assertRaisesRegex(ValueError, 'image size'):
            audit(b'short')


if __name__ == '__main__':
    unittest.main()
