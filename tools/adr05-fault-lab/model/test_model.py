"""Run with `python3 -B -m unittest discover -v -s tools/adr05-fault-lab/model`."""

import errno
import gzip
import hashlib
from pathlib import Path
import re
import unittest

from history import History
from medium import Fault, MAX_IMAGE, MAX_OPS, Medium


class PhysicalCuts(unittest.TestCase):
    def test_all_four_payload_metadata_survival_cases(self):
        base = b"x" * 8192
        m = Medium(base, [Fault(2, "sync", 0, 0, "pre_error", errno.EIO)])
        m.write(0, b"P" * 4096 + b"M" * 4096)
        with self.assertRaises(OSError) as err:
            m.sync()
        self.assertEqual(err.exception.errno, errno.EIO)
        for mask in range(4):
            fragments = [(1, j * 4096, 4096) for j in range(2) if mask & (1 << j)]
            expected = (b"P" if mask & 1 else b"x") * 4096 + (b"M" if mask & 2 else b"x") * 4096
            self.assertEqual(m.crash_image(fragments), expected)
        self.assertEqual(m.hits, {2: 1})

    def test_sync_floor_tear_and_out_of_order_overlapping_writes(self):
        m = Medium(b"0000")
        m.write(0, b"aaaa")
        m.sync()
        m.write(1, b"BB")
        m.write(2, b"CC")
        self.assertEqual(m.crash_image(), b"aaaa")
        self.assertEqual(m.crash_image([(3, 0, 2), (4, 0, 2)]), b"aBCC")
        self.assertEqual(m.crash_image([(4, 0, 2), (3, 0, 2)]), b"aBBC")
        self.assertEqual(m.crash_image([(4, 1, 1)]), b"aaaC")
        with self.assertRaises(ValueError):
            m.crash_image([(1, 0, 1)])  # no pre-sync writes can be reordered after the barrier

    def test_exact_match_and_hit_counts(self):
        for index, op, offset, count in [(1, "write", 0, 1), (2, "write", 0, 1),
                                         (1, "read", 0, 1), (1, "write", 1, 1), (1, "write", 0, 2)]:
            m = Medium(b"00", [Fault(index, op, offset, count, "pre_error", errno.ENOSPC)])
            match = (index, op, offset, count) == (1, "write", 0, 1)
            if match:
                with self.assertRaises(OSError) as err:
                    m.write(0, b"X")
                self.assertEqual(err.exception.errno, errno.ENOSPC)
                self.assertEqual(m.write(0, b"Y"), 1)
            else:
                self.assertEqual(m.write(0, b"X"), 1)
            self.assertEqual(m.hits, {index: int(match)})

    def test_uncertain_completion_and_short_read_write(self):
        m = Medium(b"0000", [Fault(1, "write", 0, 2, "post_error", errno.ETIMEDOUT),
                             Fault(2, "write", 2, 2, "short", 1), Fault(3, "read", 0, 4, "short", 3),
                             Fault(4, "sync", 0, 0, "post_error", errno.EIO)])
        with self.assertRaises(OSError) as err:
            m.write(0, b"AB")
        self.assertEqual(err.exception.errno, errno.ETIMEDOUT)
        self.assertEqual(m.write(2, b"CD"), 1)
        self.assertEqual(m.read(0, 4), b"ABC")
        self.assertEqual(m.crash_image(), b"0000")
        with self.assertRaises(OSError) as err:
            m.sync()
        self.assertEqual(err.exception.errno, errno.EIO)
        self.assertEqual(m.crash_image(), b"ABC0")
        self.assertEqual(m.hits, dict.fromkeys(range(1, 5), 1))

    def test_misdirected_io_stale_read_and_disappearance(self):
        m = Medium(b"01", [Fault(1, "write", 0, 1, "misdirect", 1), Fault(2, "read", 1, 1, "stale"),
                           Fault(3, "read", 0, 1, "misdirect", 1), Fault(4, "sync", 0, 0, "disappear")])
        m.write(0, b"X")
        self.assertEqual(m.read(1, 1), b"1")
        self.assertEqual(m.read(0, 1), b"X")
        for call in (m.sync, lambda: m.discard(1, 1)):
            with self.assertRaises(OSError) as err:
                call()
            self.assertEqual(err.exception.errno, errno.ENODEV)
        self.assertEqual(m.hits, dict.fromkeys(range(1, 5), 1))

    def test_discard_obeys_barriers_and_preserves_other_bytes(self):
        m = Medium(b"AAAA")
        self.assertEqual(m.discard(1, 2), 2)
        self.assertEqual(m.read(0, 4), b"A\0\0A")
        self.assertEqual(m.crash_image(), b"AAAA")
        m.sync()
        self.assertEqual(m.crash_image(), b"A\0\0A")

    def test_limits_and_malformed_plans(self):
        with self.assertRaises(ValueError):
            Medium(b"")
        for args in [(1, "write", 0, 2, "short", 2), (1, "sync", 0, 0, "misdirect", 0),
                     (1, "write", 0, 2, "misdirect", 1), (0, "read", 0, 2, "stale", 0)]:
            with self.assertRaises(ValueError):
                Medium(b"00", [Fault(*args)])
        m = Medium(b"00")
        with self.assertRaises(ValueError):
            m.discard(0, 3)
        for _ in range(MAX_OPS):
            m.sync()
        with self.assertRaises(ValueError):
            m.sync()


class LogicalChecker(unittest.TestCase):
    def setUp(self):
        self.h = History([b"0", b"0"])
        self.h.observe("write", "ok", 0, b"A")
        self.h.observe("flush", "ok")

    def test_rejected_write_must_never_reappear_on_reopen_lba0(self):
        self.h.observe("write", "rejected", 0, b"B")
        self.h.observe("write", "ok", 0, b"C")
        for first in (b"A", b"C"):
            self.assertTrue(self.h.accepts([first, b"0"], after_crash=True))
        self.assertFalse(self.h.accepts([b"B", b"0"], after_crash=True))

    def test_discarded_value_resurrection_on_reopen_lba0(self):
        self.h.observe("discard", "ok", 0)
        self.h.observe("flush", "ok")
        self.assertTrue(self.h.accepts([b"\x00", b"0"], after_crash=True))
        self.assertFalse(self.h.accepts([b"A", b"0"], after_crash=True))

    def test_per_lba_legal_but_skipped_record_on_reopen(self):
        self.h.observe("write", "ok", 0, b"B")
        self.h.observe("write", "ok", 1, b"C")
        self.assertTrue(self.h.accepts([b"B", b"C"], after_crash=True))
        self.assertFalse(self.h.accepts([b"A", b"C"], after_crash=True))

    def test_uncertain_write_and_failed_flush_are_not_barriers(self):
        self.h.observe("write", "uncertain", 0, b"B")
        self.h.observe("write", "ok", 1, b"C")
        self.h.observe("flush", "uncertain")
        for last in (b"0", b"C"):
            for first in (b"A", b"B"):
                self.assertTrue(self.h.accepts([first, last], after_crash=True))
        self.h.observe("flush", "ok")
        self.assertFalse(self.h.accepts([b"B", b"0"], after_crash=True))
        self.assertTrue(self.h.accepts([b"B", b"C"], after_crash=True))
        self.assertTrue(self.h.accepts([b"A", b"C"], after_crash=True))

    def test_planted_lying_sync_loses_flushed_lba1(self):
        m = Medium(b"A0", [Fault(2, "sync", 0, 0, "false_ack")])
        self.assertEqual(m.write(1, b"C"), 1)
        self.h.observe("write", "ok", 1, b"C")
        m.sync()
        self.h.observe("flush", "ok")
        self.assertTrue(self.h.accepts([m.read(i, 1) for i in range(2)], after_crash=False))
        self.assertFalse(self.h.accepts([m.crash_image()[:1], m.crash_image()[1:]], after_crash=True))
        self.assertEqual(m.hits, {2: 1})

    def test_oracle_rejects_invalid_inputs_and_overlarge_uncertainty(self):
        for args in [("read", "ok", 0, b"A"), ("write", "whatever", 0, b"A"),
                     ("write", "ok", 2, b"A"), ("discard", "ok", 0, b"X"),
                     ("write", "ok", 0, b"XX"), ("flush", "ok", 0, None)]:
            with self.assertRaises(ValueError):
                self.h.observe(*args)
        h = History([b"0"] * 9)
        for i in range(8):
            h.observe("write", "uncertain", i, b"X")
        with self.assertRaisesRegex(ValueError, "too many candidate prefixes"):
            h.observe("write", "uncertain", 8, b"X")

    def test_planted_wrong_lba_before_reopen(self):
        m = Medium(b"A0", [Fault(1, "write", 1, 1, "misdirect", 0)])
        self.assertEqual(m.write(1, b"C"), 1)
        self.h.observe("write", "ok", 1, b"C")
        self.assertFalse(self.h.accepts([m.read(i, 1) for i in range(2)], after_crash=False))
        self.assertEqual(m.hits, {1: 1})


class CapturedEngineCase(unittest.TestCase):
    def test_retained_format4_image_and_recovered_history(self):
        root = Path(__file__).resolve().parents[3] / "evidence/adr05-engine-arm64-20261119"
        with gzip.open(root / "failed-descriptor.img.gz", "rb") as f:
            image = f.read(MAX_IMAGE + 1)
        digest = hashlib.sha256(image).hexdigest()
        self.assertIn(digest + "  descriptor-eio/image", (root / "artifacts.sha256").read_text().splitlines())
        # Exercise the medium on actual bytes, without inventing the missing descriptor contents.
        m = Medium(image, [Fault(1, "write", 4096, 4096, "pre_error", errno.EIO)])
        with self.assertRaises(OSError) as err:
            m.write(4096, b"?" * 4096)
        self.assertEqual(err.exception.errno, errno.EIO)
        self.assertEqual(m.hits, {1: 1})
        self.assertEqual(m.crash_image(), image)
        # Replay only the fault case, not the control. read_block lines were emitted by
        # verify-copy *after* checking all 4096 bytes, not by a raw-byte capture tool.
        log = (root / "run.txt").read_text().split("case=control result=PASS")[1]
        history = History([bytes(4096)] * 8)
        recovered = {}
        counts = [0, 0]
        for line in log.splitlines():
            if match := re.fullmatch(r"op=write_block lba=(\d+) byte=(0x\w+) result=ok", line):
                history.observe("write", "ok", int(match[1]), bytes([int(match[2], 16)]) * 4096)
                counts[0] += 1
            elif line == "op=flush result=ok":
                history.observe("flush", "ok")
                counts[1] += 1
            elif match := re.fullmatch(r"op=read_block lba=(\d+) byte=(0x\w+) bytes=4096 match=true", line):
                lba = int(match[1])
                self.assertNotIn(lba, recovered)
                recovered[lba] = bytes([int(match[2], 16)]) * 4096
        self.assertEqual(counts, [3, 1])
        self.assertIn("op=close errno=5 result=expected_eio", log.splitlines())
        self.assertEqual(set(recovered), set(range(8)))
        self.assertTrue(history.accepts([recovered[i] for i in range(8)], after_crash=True))
