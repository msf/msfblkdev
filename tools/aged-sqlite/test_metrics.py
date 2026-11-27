import csv
import os
import struct
import tempfile
import unittest
from pathlib import Path

import metrics
from metrics import GIB, StopRule, Window


class LedgerTest(unittest.TestCase):
    def test_created_ledger_has_the_rust_header_and_zero_counters(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ledger"
            metrics.create_ledger(path)
            self.assertEqual(path.stat().st_size, 112)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            image = path.read_bytes()
            self.assertEqual(image[:8], b"MBSIOAC1")
            self.assertEqual(struct.unpack_from("=III", image, 8), (1, 112, os.geteuid()))
            self.assertEqual(struct.unpack_from("=Q", image, 24)[0], metrics.LEDGER_WRITE_LIMIT_BYTES)
            snapshot = metrics.read_ledger(path)
            self.assertEqual(snapshot.write_completed_bytes, 0)
            with self.assertRaises(FileExistsError):
                metrics.create_ledger(path)

    def test_counters_come_from_their_rust_offsets(self):
        image = bytearray(metrics.ledger_image(1000))
        # write_completed_bytes is at byte 48 and flush_completed_operations at byte 80.
        struct.pack_into("=Q", image, 48, 4096 * 3)
        struct.pack_into("=Q", image, 80, 7)
        epoch, snapshot = metrics.parse_ledger(bytes(image))
        self.assertEqual((epoch, snapshot.write_completed_bytes, snapshot.flush_operations), (0, 12288, 7))

    def test_wrong_header_is_rejected(self):
        image = bytearray(metrics.ledger_image(1000))
        image[8] = 2
        with self.assertRaises(ValueError):
            metrics.parse_ledger(bytes(image))


class DeviceStatTest(unittest.TestCase):
    def test_fields_follow_the_block_stat_layout(self):
        text = "10 0 80 5 20 1 160 9 0 12 30 3 0 2048 1 4 2\n"
        stat = metrics.parse_device_stat(text)
        self.assertEqual((stat.write_requests, stat.write_bytes), (20, 160 * 512))
        self.assertEqual((stat.discard_requests, stat.discard_sectors, stat.flush_requests), (3, 2048, 4))

    def test_short_stat_is_rejected(self):
        with self.assertRaises(ValueError):
            metrics.parse_device_stat("1 2 3")


class SizingTest(unittest.TestCase):
    def test_record_count_fills_one_page_per_record(self):
        self.assertEqual(metrics.record_count(8 * GIB, 0.75, 1), 1_572_864)
        self.assertEqual(metrics.record_count(8 * GIB, 0.75, 100), 1_572_800)

    def test_record_count_rejects_fill_outside_zero_and_one(self):
        for fill in (0, 1, 1.5, -0.1):
            with self.assertRaises(ValueError):
                metrics.record_count(GIB, fill, 100)

    def test_window_writes_one_backing(self):
        self.assertEqual(metrics.window_operations(8 * GIB, 8192.0), GIB // 1024)
        for too_little in (0, 1, metrics.MIN_DEVICE_BYTES_PER_OPERATION - 1):
            with self.assertRaises(ValueError):
                metrics.window_operations(GIB, too_little)


class StopRuleTest(unittest.TestCase):
    rule = StopRule(minimum_multiple=3, maximum_multiple=10, tolerance=0.05)

    def test_steady_windows_before_the_minimum_continue(self):
        windows = [Window(1000, 2.0), Window(1000, 2.0)]
        self.assertIsNone(self.rule.decide(2 * GIB, GIB, windows))

    def test_steady_windows_after_the_minimum_stop(self):
        windows = [Window(1000, 2.0), Window(1040, 2.08)]
        self.assertEqual(self.rule.decide(3 * GIB, GIB, windows), "steady")

    def test_unsteady_windows_continue_until_the_maximum(self):
        windows = [Window(1000, 2.0), Window(800, 2.0)]
        self.assertIsNone(self.rule.decide(5 * GIB, GIB, windows))
        self.assertEqual(self.rule.decide(10 * GIB, GIB, windows), "maximum")

    def test_one_window_is_never_steady(self):
        self.assertFalse(self.rule.steady([Window(1, 1)]))


class LatencyTest(unittest.TestCase):
    def test_summary_and_series(self):
        with tempfile.TemporaryDirectory() as directory:
            source, series = Path(directory) / "latency.csv", Path(directory) / "series.csv"
            with open(source, "w", newline="") as target:
                writer = csv.writer(target)
                writer.writerow(["operation", "timestamp_us", "latency_us"])
                for index in range(100):
                    writer.writerow(["UPDATE", 1_000_000 + index * 10_000, index + 1])
                writer.writerow(["READ", 2_500_000, 5])
            summary = metrics.summarize_latency(source, series)
            self.assertEqual(sum(op["count"] for op in summary.per_operation.values()), 101)
            self.assertEqual(summary.per_operation["UPDATE"]["p50"], 50)
            self.assertEqual(summary.per_operation["UPDATE"]["p99"], 99)
            self.assertEqual(summary.per_operation["UPDATE"]["max"], 100)
            self.assertAlmostEqual(summary.seconds, 1.500005)
            with open(series, newline="") as written:
                rows = list(csv.DictReader(written))
            self.assertEqual([(r["second"], r["operation"], r["count"]) for r in rows],
                             [("1", "UPDATE", "100"), ("2", "READ", "1")])


class ParserTest(unittest.TestCase):
    def test_blkctl_block_counts(self):
        text = ("log        31 segments × 2048 blocks = 63488 blocks\n"
                "  used                     1 block    0 sealed or pending-free, 2 open\n"
                "  live                    0 blocks    0.0% of used\n"
                "  dead                   12 blocks    0.0% of used\n"
                "  free                63487 blocks    29 segments + 4095 blocks in open segments\n"
                "segments   sealed 4   pending-free 1   open 2   free 29\n")
        self.assertEqual(metrics.parse_blkctl_info(text),
                         {"used": 1, "live": 0, "dead": 12, "free": 63487, "sealed": 4,
                          "pending_free": 1, "open": 2, "free_segments": 29})

    def test_daemon_events_keep_the_last_record_per_event_and_phase(self):
        text = ("info event=space phase=open used=1\n"
                "debug event=request tag=3\n"
                "info event=log_writes data_bytes=10 metadata_bytes=2\n"
                "info event=space phase=shutdown used=9\n")
        events = metrics.parse_daemon_events(text)
        self.assertEqual(events["space.shutdown"], {"used": "9"})
        self.assertEqual(events["space.open"], {"used": "1"})
        self.assertEqual(events["log_writes"], {"data_bytes": "10", "metadata_bytes": "2"})
        self.assertNotIn("request", events)


class ClientOutputTest(unittest.TestCase):
    good = ("lab-setting sqlite_version=3.51.3\nlab-setting journal_mode=wal\n"
            'sqlite-lab {"record":"auto_checkpoint","database":"main","trigger_frames":1000,"rc":0,'
            '"wal_frames":1000,"checkpointed_frames":1000,"policy":"passive"}\n')

    def test_confirmed_settings_and_successful_checkpoints_pass(self):
        self.assertIsNone(metrics.client_output_problem(self.good, "3.51.3"))

    def test_each_failure_is_named(self):
        cases = {
            "operation errors": self.good + "operation err: disk I/O error\n",
            "sqlite_version=3.51.3": self.good.replace("3.51.3", "3.46.1"),
            "journal_mode=wal": self.good.replace("journal_mode=wal", "journal_mode=delete"),
            "checkpoint failed": self.good.replace('"rc":0,', '"rc":10,'),
        }
        for expected, output in cases.items():
            with self.subTest(expected):
                self.assertIn(expected, metrics.client_output_problem(output, "3.51.3"))


class ClientSummaryTest(unittest.TestCase):
    def test_fio_job_gives_write_percentiles_in_microseconds(self):
        job = {"write": {"total_ios": 4096, "runtime": 2500,
                         "clat_ns": {"max": 9_000_000, "percentile": {"50.000000": 20_000, "99.000000": 80_000,
                                                                      "99.900000": 500_000}}}}
        summary = metrics.summarize_fio(job)
        self.assertEqual(summary.seconds, 2.5)
        self.assertEqual(summary.per_operation["WRITE"],
                         {"count": 4096, "max": 9000, "p50": 20, "p99": 80, "p999": 500, "p9999": 0})

    def test_window_minimum_is_per_client(self):
        self.assertEqual(metrics.window_operations(GIB, 512, minimum=256), GIB // 512)
        with self.assertRaises(ValueError):
            metrics.window_operations(GIB, 512, minimum=4096)


if __name__ == "__main__":
    unittest.main()
