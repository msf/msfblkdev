# TMD: Benchmark and test workbench

Date: 2026-09-20 Author: Miguel Filipe Status: proposal Related:
[ADR-06](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md), [ADR-07 performance
results](ADR-07-PERF-RESULTS.md), [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md), [ADR-09
compression](ADR-09-RELOCATION-COMPRESSION.md), [SQLite suite](SQLITE-EXT4.md)

## Problem

ADR-07's experiments used separate `fio` sequences on a 128 MiB device backed by a 1 GiB file on
tmpfs. They compared against the previous tag. The [performance report](ADR-07-PERF-RESULTS.md)
records those results, but does not define reusable workload inputs. Three gaps remain:

1. **No representative workloads.** The engine benchmarks use synthetic sequential and seeded random
   sequences. The one realistic suite, [SQLite on ext4](SQLITE-EXT4.md), is a correctness gate with
   small datasets: 32 BLOBs, 4,096 reads, a 512 MiB write ledger. It says nothing about steady
   state.
2. **No fast inner loop.** Every realistic run goes through ublk, a mount helper and a live
   filesystem. That is minutes per run and needs operator approval. Design questions in ADR-08
   (segment size and selection policy) and ADR-09 (codec and chunk size) need thousands of runs.
3. **Extra layers under test.** File backing on tmpfs or btrfs puts a second allocator and a page
   cache between the engine and the device. The results measure that stack, not ours.

ADR-08 adds a requirement the current gates cannot meet at all: measuring write amplification after
the log has wrapped, on a volume aged into steady state.

The target workloads are stateful systems: databases, key-value stores and indexing engines. General
filesystem use is a blind-spot check.

## Goals

1. Fast: a policy question answers in seconds, an engine change in minutes, a live acceptance in
   under an hour.
2. Reproducible: fresh fixtures, pinned tool versions with hash manifests, fixed seeds, recorded
   environment, and the same trace replayed against every candidate.
3. Few dependencies: raw LVM logical volumes for backing, no filesystem between the engine and the
   device, tools we already build.
4. Reusable: the same layers serve ADR-08 and every later ADR.

## Non-goals

Power-loss testing, multi-node work, and exhaustive filesystem coverage. The existing correctness
gates (`make test`, `make test-acceptance`, the ext4, XFS and SQLite lifecycles) stay as they are;
this workbench sits beside them.

## Three layers

| Layer | What runs | Backing | Time per run | Answers |
|---|---|---|---|---|
| L0 simulator | Mapping and segment model only, no payload bytes | none | seconds | Raw-cleaning policy: selection, watermarks, segment size, `N` victims and sort key. ADR-09 can later add compression ratio as a parameter. |
| L1 engine replay | A captured block trace replayed straight into the Rust engine | raw LVM volume, or a file for unprivileged runs | minutes | Engine behaviour: amplification, latency, checkpoint and cleaner cost, memory, recovery time |
| L2 live | ublk device, filesystem, real workload | raw LVM volume | tens of minutes | End-to-end numbers, trace capture, blind spots |

Iteration happens in L0 and L1. L2 validates and produces traces for L1.

### L0: the simulator

Lomet and Luo built exactly this (§6.1.1): a simulator that writes page IDs, not page contents,
because "we only care about the cleaning cost". Ours models LBAs, segments, live bytes per segment,
the two fixed append heads and raw cleaning. It reads either a synthetic generator (uniform, 80/20
and 90/10 Zipf, moving hotspot) or a captured L2 trace. Output is amplification, free segments,
cleaned-segment emptiness and fill factor over time, as CSV. Compression assumptions do not enter
ADR-08 calibration or acceptance.

The baseline uses two fixed heads: User for new writes and generated metadata, Relocation for
accepted copies. Keep one shared head as a comparison in experiments. Measure aged amplification,
latency and reserve cost to test the lifetime-separation heuristic.

Calibration is mandatory before it answers anything: under uniform writes with age-based cleaning it
must reproduce Lomet and Luo's Table 1 to two digits, as their own simulator did (§8.1).

It lives in the Rust crate as an example or a small binary with no I/O dependencies, so `cargo run
--release --example` is the whole invocation.

### L1: trace replay into the engine

The engine already has the API a replayer needs: `write_block`, `read_block`, `flush`, and the
options for buffer and cache sizes. The replayer reads a trace file and drives the engine at maximum
speed or at recorded timing, on a backing path given by the caller. It records per-operation
latency, engine statistics (`SpaceUsage`, mapping stats and, with ADR-08, cleaner stats) at fixed
intervals, and a final verification pass of every LBA the trace wrote.

The trace source needs no `blktrace`, but it cannot be the current `debug` log. That log formats
text and issues one locked, unbuffered `write(2)` to stderr per request, on the queue thread
(`rust/src/logging.rs`). Its own doc comment says it "perturbs timing and must never be enabled
during measurement". At NVMe rates, hundreds of thousands of requests per second, a syscall per
request is the measurement.

Tracing therefore gets its own path, separate from logging:

- **Fixed-size binary records**, 32 bytes each: monotonic timestamp, operation, flags, start sector,
  sector count, and the completion result or latency. No formatting on the queue thread.
- **A preallocated ring buffer** written by the queue thread with a single atomic index update per
  record. Never blocks the I/O path. When the ring is full, the record is dropped and a drop counter
  increments; the trace header carries the final drop count so a replay knows whether it is
  complete. This is the `blktrace` relay design and the LTTng design, per-CPU buffers drained
  asynchronously.
- **A drain thread** that writes filled ring pages to the trace file with large sequential writes,
  or maps the file and lets the kernel write it back.
- **A converter** from the binary file to the same `key=value` lines the `debug` log uses today, so
  the human-readable form is derived, not produced on the hot path. The existing log stays as it is
  for lifecycle events.

The trace file has a header naming the workload, the tool versions, the device geometry, the record
count, the drop count and a content hash. Capturing is a flag on the daemon. `blktrace` remains
available as an independent cross-check that the daemon saw what the kernel sent, and `fio
--read_iolog` can replay the converted text form through ublk as a second independent path.

These trace records contain no payload bytes. L1 generates deterministic payloads to replay
addresses and timing, then checks those generated contents. Compression experiments require a
separately captured payload corpus. Address traces alone cannot reproduce the source workload's
compression ratio.

Traces are stored under `traces/` with their manifest. They are inputs, not evidence; evidence stays
under `evidence/`.

### L2: live workloads

The existing lab binary already creates a device, mounts, runs a workload, restarts and verifies. L2
adds trace capture, raw-volume backing and the workload catalogue below. It remains the only layer
that needs the mount helper and operator approval.

## What stays fixed

**Scales.** Two logical sizes, 128 MiB and 4 GiB. Backing is sized from ADR-08's floor and a
fill-factor target of 0.8: about 192 MiB for the small volume with 16 MiB segments, about 5.3 GiB
for the large one. A third scale, 32 GiB, is optional and only for the large-volume regressions
ADR-07 measured by calculation.

**Backing.** A raw LVM logical volume named `my-block-storage-test-*`, validated by the ADR-03
operator procedure before every write. No filesystem between engine and device. The device model,
firmware and preconditioning state are recorded, because the SSD's own log sits under ours (INFLOW
§3.3) and its GC state changes results by 2x (FAST '26, §7.2, Fig. 13). Unprivileged and CI runs use
a regular file and are labelled as such; they are for correctness and relative comparisons, never
for absolute latency.

**Aging.** No steady-state number is reported from a fresh volume. The procedure: fill to the target
fill factor, then run the workload's overwrite phase until amplification over the last two windows
agrees within 5%. Lomet and Luo wrote 100 times the system size (§6.2); the FAST '26 paper
preconditioned with two whole-disk sequential writes and 8 hours of 4 KiB random writes (§7.2).
Seltzer's point (§5) is the same: benchmark after use, not on an empty system. The aging run is
itself a measurement and is kept.

**Repetition and reporting.** Run three interleaved baseline and candidate repetitions. Report
median and range, avoid concurrent tests, and record CPU pinning. Use the same report fields for
every candidate. Engine metrics come from the engine's own counters; the workbench adds no external
observability dependency.

## Workloads

One per class the device targets, plus one blind-spot check. Each is built from pinned sources with
a hash manifest, as `make setup-sqlite` already does for SQLite 3.51.3.

| Class | Workload | Phases | Why this one |
|---|---|---|---|
| Database and key-value store, same definition | `go-ycsb` core workloads A to F against SQLite and RocksDB | load to the target record count, then run each workload to steady state | Compare engines with one workload definition. Measure the resulting hot-set fractions before comparing Zipf parameters with 80/20 or 90/10 models. Rate control changes offered load; record the actual idle gaps. |
| Database, engine-native | SQLite `speedtest1` and `kvtest`, already pinned and adapted | existing catalogue, then a sustained `kvtest` overwrite phase sized to wrap the log | Already integrated; WAL and rollback-journal profiles exercise two very different write patterns |
| Indexing engine | SQLite FTS5 indexing a fixed text corpus, then incremental updates | bulk index, then update and re-index | Same toolchain as above; an inverted index rewrites posting pages, which is the cleaner's hard case |
| Key-value store, engine-native | RocksDB `db_bench`: `fillrandom`, `overwrite`, `readrandomwriterandom`, with `sync=1` variants | fill to target, overwrite to steady state, mixed | The reference LSM workload with its own knobs for compaction; its compaction is a second log on top of ours, exactly INFLOW's scenario |
| Database, later phase | PostgreSQL through `go-ycsb` or `pgbench` | init at a scale factor that fits, then mixed | ADR-06 names PostgreSQL; a WAL plus heap-page write pattern unlike SQLite's. Not in this phase |
| Blind spot | Filebench `varmail` and `fileserver` personalities on ext4 | run to steady state, then delete and rewrite | Standard file-level workloads with fsync-heavy small files and deletes, which is where discard shows up. Replaces an ad hoc untar |

`go-ycsb` is the backbone. It is the one tool that runs the same logical workload against different
engines, so the difference between two traces is the engine's write pattern and nothing else. It is
a single Go binary, smaller and simpler than the Java YCSB, with no JVM competing with the daemon
for CPU. Its SQLite and RocksDB drivers are from recollection and must be verified before pinning.
Record size should be set to a multiple of the database page, not YCSB's 1 KiB default.

Filebench is dormant upstream, its last release is years old, and it needs flex and bison to build.
It is still the standard for file-level personalities, and the blind-spot class needs exactly one
such tool. Pin a commit and accept the build dependency; if it fails to build on the target kernel,
fall back to `fio` with a small-file job and an untar.

Every workload runs at both scales and produces a trace. ADR-09 compression experiments use the
pages these workloads write, not synthetic data. They compare against raw cleaning and do not gate
ADR-08 delivery.

## Metrics

From the engine, sampled at intervals and at the end:

- bytes written by the client, by the log for payloads, for metadata, and by the cleaner; write
  amplification as their ratios;
- fill factor, garbage ratio, free segments, cleaned-segment emptiness averaged over the window and
  best-victim score;
- checkpoint count and duration, mapping cache hits and misses, recovery time after a clean close
  and after SIGKILL.

From the driver: throughput, IOPS, and latency at the median, 99th and 99.9th percentiles, with
sample counts. Latency is reported separately for the phases before and after aging.

For compression experiments, use [ADR-09's measurement
definitions](ADR-09-RELOCATION-COMPRESSION.md#what-must-we-measure): byte-weighted codec ratios,
effective storage savings after framing and padding, current physical occupancy, raw fallback and
stale rejection, CPU and read amplification. Sample the underlying byte counters as well as ratios.
Compare the same aged traces with compression enabled and disabled; a codec ratio alone is not an
acceptance result.

## Make targets

Names are indicative. Each target writes evidence under `evidence/` and fails rather than continuing
past its deadline, as the existing lab does.

```sh
make bench-sim                      # L0: run the policy matrix, emit CSV
make bench-replay TRACE=... BACKING=...   # L1: replay one trace into the engine
make bench-capture WORKLOAD=... SCALE=... # L2: run a live workload, capture its trace
make bench-live WORKLOAD=... SCALE=...    # L2: run a live workload with aging, report
make bench-report                   # fill the template from evidence directories
```

## Deliveries

Each is small enough to finish in one session and useful on its own.

1. **Trace ring, converter and L1 replayer.** Build the binary records, bounded ring, drain thread,
   converter and replayer. Compare existing `fio` scenarios with tracing off and on. Set acceptable
   throughput and P99 differences before the comparison. Check replay operation counts and generated
   per-LBA contents. Cross-check one trace with `blktrace` and one with `fio --read_iolog`.
2. **L0 simulator.** Build it, calibrate against Lomet and Luo's Table 1, and run ADR-08's delivery
   0 matrix. This can start in parallel with delivery 1.
3. **Workload fixtures and captures.** Pin and build `go-ycsb` with the SQLite and RocksDB drivers,
   RocksDB `db_bench`, and Filebench; add the FTS5 case to the SQLite adapter. Capture each workload
   at both scales on the current engine. These traces are the baseline for every later comparison.
4. **Raw-volume backing and aging.** The LVM validation procedure as code in the lab, the aging
   loop, and the report generator. First use: rerun the ADR-07 v1.2.3 comparison on a raw volume
   after aging, to learn how much the tmpfs numbers misled us.

## Open questions

1. Verify `go-ycsb`'s SQLite and RocksDB drivers and its request-distribution parameters before
   pinning a commit.
2. Is a disposable LVM volume available on the development host, or do we need a second machine for
   L2?
3. How large a text corpus for FTS5, and which one, so the fixture stays under a few hundred
   megabytes and is redistributable?
4. Should L1 replay at recorded timing as well as at maximum speed? Recorded timing reproduces idle
   gaps, which matter for the cleaner's opportunity trigger.
5. Ring size and drop policy for the trace: how many records must the ring hold to survive a
   drain-thread stall at the device's peak rate?
