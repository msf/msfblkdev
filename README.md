# my-block-storage

A Linux-only log-structured block device experiment. Rust is authoritative and includes the storage engine, a serialized ublk frontend, and a bounded lab harness.

## Current status

Goal 1 is complete. ADR-03 remains active. Its Rust implementation and lab scenarios are present, but the operator-run ublk/`fio` acceptance criteria are not claimed here.

The active goal is a minimum credible device:

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
- [Deterministic fault testing and simulation](DETERMINISTIC_FAULT_TESTING_AND_SIMULATION.md) is a TMD for future work. Simulation is an aspiration, not part of ADR-03's scope.
- [Distributed reliable block storage](DISTRIBUTED_RELIABLE_BLOCK_STORAGE.md) is a non-authoritative future design note. It is not an implementation plan.

## Rust edit loop

```sh
cd rust
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

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

## Lab test targets

- `make test` runs the normal Rust matrix with a 15-second per-test limit and a 55-second suite limit.
- `make test-acceptance` runs the long engine matrix, including 20 fresh repetitions of each crash scenario; defaults are 30 minutes per test and 60 minutes for the suite.
- `make test-ublk-fio` is explicit opt-in. It requires Linux, `fio`, ublk kernel support, read/write access to `/dev/ublk-control`, and the feature-enabled sibling binaries built by the target. It creates only fresh regular files in owned temporary directories, validates each recorded `/dev/ublkbN` identity, and never accepts a backing or device path. Do not run it while ublk access or cleanup safety is uncertain.

Timing limits can be overridden with `TEST_PER_TEST_SECONDS` and `TEST_SUITE_SECONDS`.

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

The engine has one serialized writer, one 4 KiB payload per log record, a finite log, and no compaction or wraparound. V0.4 rejects an uncheckpointed crash tail. ADR-03 closes that recovery gap before the project claims a credible block device.
