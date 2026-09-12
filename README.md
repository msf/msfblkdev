# my-block-storage

A Linux-only log-structured block device experiment. Rust is authoritative and includes the storage engine, a serialized ublk frontend, and a bounded lab harness.

## Current status

Goals 1 and 2 are complete. ADR-03's engine, serialized ublk frontend and live regular-file `fio` acceptance gates pass. V0.8 acceptance is complete.

[ADR-05](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) consolidates the proposed fault model, failure handling, fault testing and simulation direction. Its design remains under review; implementation has not started. ADR-01 and ADR-03 remain the accepted baseline.

The completed minimum credible device sequence is:

```text
V0.4 closure
→ V0.5 overwrite semantics
→ V0.6 crash recovery
→ V0.7 serialized ublk frontend
→ V0.8 fio validation and restart recovery
```

Rust is authoritative. The Zig implementation is a completed initial experiment and may diverge.

## Design and delivery specifications

- [ADR-01: log-structured block device](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) defines the architecture, persistent format and high-level roadmap.
- [ADR-02: basic read and write](ADR-02-GOAL-1-BASIC-READ-WRITE.md) records V0.0 through V0.4 and its closure gate.
- [ADR-03: minimum credible device](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) specifies V0.5 through V0.8.
- [RALPH.md](RALPH.md) defines the bounded worker loop for one-hour implementation sessions.
- [ADR-05: fault model, failure handling and fault testing](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md) owns the proposed policy and test plan, including the former simulation TMD. It does not expand ADR-03's completed scope.
- [Distributed reliable block storage](DISTRIBUTED_RELIABLE_BLOCK_STORAGE.md) is a non-authoritative future design note. It is not an implementation plan.

## Rust API

The crate exposes:

```text
format
open
Volume::write_block
Volume::read_block
Volume::flush
Volume::close
```

Tests require Linux, `io_uring`, and a temporary filesystem supporting `O_DIRECT`.

## Testing

These descriptions record the test behavior at V0.8. Update them when a target changes.

### Development loop

Use Cargo for a focused inner loop while editing Rust. Use the top-level Makefile targets for repository gates and before each commit.

```sh
cd rust
cargo test test_name_fragment
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cd ..
make lint test
```

`make test` is the bounded developer suite. It runs the normal Rust matrix with a 15-second limit per test and a 55-second suite limit. It reports each test's duration and does not run live ublk/`fio` or the exhaustive crash matrix.

### Exhaustive engine crash acceptance

`make test-acceptance` runs the full Rust matrix in acceptance crash mode. Each process-crash scenario uses 20 fresh repetitions. Coverage includes record, mapping, log-fsync, checkpoint-body, descriptor, and all 339 stale-tail clearing boundaries. It also covers uncaught-panic recovery and corrupted-checkpoint fallback.

The lab starts only test-created children. It waits for named persistence handshakes, kills only the recorded process group, reaps descendants, and verifies recovery from regular-file backing. It does not require ublk or root access.

The default limits are 30 minutes per test and 60 minutes for the suite. The V0.8 run took about 10 minutes, dominated by the exhaustive stale-tail test. Evidence is written to `evidence/engine-crash-*.log` with the commit, kernel, backing type, commands, timings, and result.

### Live ublk and fio acceptance

`make test-ublk-fio` builds the daemon and lab with `test-failpoints`, then tests the live kernel-to-engine path. It requires Linux, `fio`, loaded ublk kernel support, and read/write access to `/dev/ublk-control`.

The target runs seven scenarios three times. Every repetition uses a newly formatted 32-record regular-file image:

1. Sequential write, flush, read, and verify.
2. Seeded random write, flush, read, and verify.
3. Eight overwrites of one logical block address.
4. Graceful `SIGTERM`, restart, and verify.
5. `SIGKILL` after a successful `fio` flush, restart, and verify.
6. `SIGKILL` at the descriptor-write failpoint, restart, and verify.
7. A 33-write exhaustion test that requires `ENOSPC`, restarts, and verifies the 32 successful writes.

Each child operation has a 30-second limit. The harness validates device identity and geometry before I/O or deletion. It proves backing-file lock contention and cleans only its recorded device and owned temporary directory. If identity becomes ambiguous, it refuses cleanup and preserves evidence. The V0.8 run took about 21 seconds. Evidence is written to `evidence/ublk-fio-*.log`.

A normal-user run can print a harmless `fio` warning that only root may invalidate a block-device cache. This is separate from the ublk FLUSH requests used for durability validation.

Run all repository gates with:

```sh
make lint test test-acceptance test-ublk-fio
```

### Normal-user ublk setup

Load the driver if `/dev/ublk-control` is absent:

```sh
sudo modprobe ublk_drv
```

The preferred setup gives the existing `plugdev` group access to the global control node. Each created device uses `UBLK_F_UNPRIVILEGED_DEV`. The upstream owner helper then assigns `/dev/ublkcN` and `/dev/ublkbN` to the kernel-recorded owner.

Build the vendored helper as the normal user. Install the helper, library, and restrictive udev rule as an administrator:

```sh
make -C ublksrv lib/libublksrv.la ublk_user_id
sudo ublksrv/libtool --mode=install install -m 0755 ublksrv/lib/libublksrv.la /usr/local/lib
sudo ublksrv/libtool --mode=install install -m 0755 ublksrv/ublk_user_id /usr/local/sbin
sudo install -m 0755 ublksrv/utils/ublk_chown.sh /usr/local/sbin/ublk_chown.sh
sudo ldconfig
sudo install -m 0644 ublksrv/utils/ublk_dev.rules /etc/udev/rules.d/90-ublk.rules
sudo sed -i \
  -e 's/KERNEL=="ublk-control", MODE="0666"/KERNEL=="ublk-control", GROUP="plugdev", MODE="0660"/' \
  -e 's/",KERNEL/", KERNEL/g' \
  -e 's/",RUN/", RUN/g' \
  /etc/udev/rules.d/90-ublk.rules
sudo udevadm verify /etc/udev/rules.d/90-ublk.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --settle --action=add --name-match=ublk-control
```

The project does not install or reload these host files automatically. Do not install the vendored rule unchanged because it grants mode `0666` on `/dev/ublk-control`. Do not run Make or Cargo as root because they create root-owned build artifacts.

Override test limits with `TEST_PER_TEST_SECONDS` and `TEST_SUITE_SECONDS`.

## Historical Zig experiment

The Zig source remains in `src/root.zig`. Its explicit targets remain available:

```sh
make build-zig
make lint-zig
make test-zig
```

Future milestones do not require Zig changes or Zig/Rust image compatibility.

## Safety

Automated engine and ublk lab tests use disposable regular files. Any future LVM test must identify and validate an explicitly disposable target before writing. Never use the laptop's system NVMe, a mounted filesystem or an arbitrary block device.

## Current limits

The engine has one serialized writer, one 4 KiB payload per log record, a finite log, and no compaction or wraparound. V0.6 recovers complete flushed records after process loss. V0.8 has passed live ublk and `fio` acceptance; medium-write faults, compaction and wraparound remain outside ADR-03.
