# ADR-02 Goal 1: basic read and write

Date: 2026-08-29
Author: Miguel Filipe
Status: completed
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md)

## Context

Goal 1 built the local storage engine from an empty crate through V0.4. The implementation proves aligned direct I/O, persistent format roots, append-only writes, clean checkpoints and checksummed reads.

Zig was the first implementation experiment. Rust independently reached the same behavioral milestone and is now the implementation we will evolve. The persistent encodings have diverged, so cross-language image compatibility is no longer a requirement.

The Rust engine exposes:

```text
format
open
Volume::write_block
Volume::read_block
Volume::flush
Volume::close
```

It does not expose a Linux block device.

## Decision

Goal 1 consists of these cumulative deliveries:

- **V0.0, I/O gate:** perform aligned `io_uring` write, fsync, close, reopen and read against an `O_DIRECT` file. Validate completion errors and exact byte counts.
- **V0.1, format:** encode and checksum two empty checkpoint descriptors. Calculate non-overlapping checkpoint bodies and reject invalid geometry.
- **V0.2, open:** validate both checkpoint roots, reconstruct an empty mapping and reject inconsistent persistent state.
- **V0.3, append and shutdown:** append one payload and footer, publish the mapping only after exact completion, flush, and publish a clean checkpoint on close.
- **V0.4, read:** return zeroes for unwritten blocks and verify payload checksums before returning mapped data.

The public acceptance test must prove:

```text
format a temporary backing image
→ open an empty volume
→ read an unwritten LBA as 4096 zero bytes
→ write a known 4096-byte block
→ read and compare it
→ flush and close cleanly
→ reopen
→ read and compare the same block again
```

The implementation remains deliberately bounded:

- one serialized writer;
- one 4 KiB payload per record;
- finite append-only log;
- no crash-tail replay;
- no compaction, wraparound, discard or concurrency;
- no ublk frontend.

## What is implemented?

The Rust crate implements every V0.4 operation and has 15 tests covering:

- aligned direct I/O and fsync;
- exact `io_uring` completion handling;
- format geometry and checkpoint roots;
- raw payload and footer encoding;
- failed-write publication rules;
- flush and clean checkpoint publication;
- zero reads, mapped reads and payload corruption detection;
- clean reopen and checkpoint consistency;
- rejection of an unreplayed durable tail.

Before closure work started on 2026-08-29:

- `cargo clippy --all-targets -- -D warnings` passed;
- all 15 tests passed with one test thread;
- repeated default parallel runs failed intermittently in the resource-release test;
- `cargo fmt --check` failed on two ambiguous FIXME comments.

The parallel failure is a test defect. It checks a numeric file descriptor after ownership has been released, but another parallel test can reuse that number. Rust ownership already guarantees that `File` and `IoUring` are dropped when `Volume::close` consumes the volume.

## Closure work

Goal 1 required one cleanup delivery:

- [x] Replace the footer record-kind literal with a named format constant.
- [x] Replace the ambiguous tail-footer validation FIXME with a precise name.
- [x] Remove the invalid numeric file-descriptor reuse assertions.
- [x] Rename Zig-specific checksum-vector terminology to persistent-format terminology.
- [x] Make Rust the default build, lint and test path while retaining explicit optional Zig targets.
- [x] Pass the complete closure gate.

Closure gate:

```sh
cd rust
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test

for run in {1..100}; do
  cargo test --quiet || exit 1
done
```

No test may be skipped, ignored or weakened to close this goal.

Closure evidence from 2026-08-29:

- `cargo fmt --check` passed;
- `cargo clippy --all-targets -- -D warnings` passed;
- all 15 Rust tests passed;
- 100 consecutive default parallel test runs passed;
- `make build lint test` passed with Rust as the default;
- `make build-zig lint-zig test-zig` still passed as an optional historical gate.

## Consequences

The Zig source remains in the repository as historical evidence. Future milestones do not update it, and combined Zig/Rust image compatibility is not tested.

Goal 1 fails closed on an unreplayed tail. A process crash after a durable write can therefore prevent reopen. [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md) fixes this before we call the system a credible block device.
