# my-block-storage

A Linux-only log-structured block device experiment. The Rust storage engine implements basic 4 KiB read and write behavior through V0.4. It does not expose a Linux block device yet.

## Current status

V0.4 behavior is implemented, but Goal 1 remains open until every ADR-02 closure item and its complete Rust gate pass.

The active goal is a minimum credible device:

```text
V0.4 closure
→ V0.5 overwrite semantics
→ V0.6 crash recovery
→ serialized ublk frontend
→ fio validation and restart recovery
```

Rust is authoritative. The Zig implementation is a completed initial experiment and may diverge.

## Design and delivery specifications

- [ADR-01: log-structured block device](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md) defines the architecture, persistent format and high-level roadmap.
- [ADR-02: basic read and write](ADR-02-GOAL-1-BASIC-READ-WRITE.md) records V0.0 through V0.4 and its closure gate.
- [ADR-03: minimum credible device](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) specifies V0.5, V0.6, ublk and vertical acceptance.
- [RALPH.md](RALPH.md) defines the bounded worker loop for one-hour implementation sessions.
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

## Historical Zig experiment

The Zig source remains in `src/root.zig`. Its explicit targets remain available:

```sh
make build-zig
make lint-zig
make test-zig
```

Future milestones do not require Zig changes or Zig/Rust image compatibility.

## Safety

Automated engine tests use disposable regular files. Future ublk and LVM tests must identify and validate an explicitly disposable target before writing. Never use the laptop's system NVMe, a mounted filesystem or an arbitrary block device.

## Current limits

The engine has one serialized writer, one 4 KiB payload per log record, a finite log, and no compaction or wraparound. V0.4 rejects an uncheckpointed crash tail. ADR-03 closes that recovery gap before the project claims a credible block device.
