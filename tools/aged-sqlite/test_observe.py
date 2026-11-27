import os
import unittest

import observe
from observe import DeviceTimes, Snapshot, ThreadTimes


class ParseTest(unittest.TestCase):
    def test_device_times_with_flush_fields(self):
        text = "1 0 8 2 100 0 800 50 0 40 60 0 0 0 0 10 5"
        self.assertEqual(observe.parse_device_times(text),
                         DeviceTimes(writes=100, write_sectors=800, write_ms=50, io_ms=40, queue_ms=60,
                                     flushes=10, flush_ms=5))

    def test_device_times_without_flush_fields(self):
        times = observe.parse_device_times("1 0 8 2 100 0 800 50 0 40 60")
        self.assertEqual((times.flushes, times.flush_ms), (0, 0))

    def test_pressure(self):
        text = "some avg10=1.00 avg60=0.00 avg300=0.00 total=1500\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=700\n"
        self.assertEqual(observe.parse_pressure("io", text), {"io_some": 1500, "io_full": 700})

    def test_own_threads_are_readable(self):
        threads = observe.read_threads(os.getpid())
        self.assertIn(os.getpid(), threads)
        self.assertGreater(threads[os.getpid()].on_cpu_ns, 0)


class SummarizeTest(unittest.TestCase):
    def test_fractions_rates_and_latency(self):
        before = Snapshot(10.0, {7: ThreadTimes("worker", 0, 0, 0, 0)},
                          {"ublkb0": DeviceTimes(0, 0, 0, 0, 0, 0, 0)}, {"io_some": 0})
        after = Snapshot(12.0, {7: ThreadTimes("worker", 1_500_000_000, 100_000_000, 20, 4),
                                9: ThreadTimes("new", 200_000_000, 0, 0, 0)},
                         {"ublkb0": DeviceTimes(1000, 8 * 2048, 500, 1000, 3000, 10, 20)}, {"io_some": 500_000})
        summary = observe.summarize(before, after)
        worker = summary["threads"][0]
        self.assertEqual((worker["tid"], worker["on_cpu"], worker["runqueue"], worker["off_cpu"]),
                         (7, 0.75, 0.05, 0.2))
        self.assertEqual(worker["voluntary_switches_per_second"], 10.0)
        self.assertEqual(summary["threads"][1]["on_cpu"], 0.1)  # a thread born during the phase
        device = summary["devices"]["ublkb0"]
        self.assertEqual((device["writes_per_second"], device["write_mib_per_second"], device["write_await_ms"]),
                         (500.0, 4.0, 0.5))
        self.assertEqual((device["flush_await_ms"], device["utilization"], device["average_queue"]), (2.0, 0.5, 1.5))
        self.assertEqual(summary["pressure"]["io_some"], 0.25)
        table = observe.markdown([("window", summary)])
        self.assertTrue(any("| window | worker | 7 | 75.0%" in line for line in table))

    def test_no_writes_means_no_await(self):
        zero = DeviceTimes(0, 0, 0, 0, 0, 0, 0)
        summary = observe.summarize(Snapshot(0.0, {}, {"d": zero}, {}), Snapshot(1.0, {}, {"d": zero}, {}))
        self.assertIsNone(summary["devices"]["d"]["write_await_ms"])

    def test_zero_wall_time_is_rejected(self):
        empty = Snapshot(1.0, {}, {}, {})
        with self.assertRaises(ValueError):
            observe.summarize(empty, empty)


if __name__ == "__main__":
    unittest.main()
