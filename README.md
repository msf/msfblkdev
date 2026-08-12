# my-block-storage

A Linux-only prototype of a 4 KiB log-structured block-storage engine. Separate Zig and Rust implementations complete V0.4; neither exposes a block device yet.

## Prerequisites

Linux x86_64, a kernel with `io_uring`, a backing filesystem or device that supports `O_DIRECT`, a Rust toolchain with Cargo, and `make`. Zig setup also needs `curl`, `sha256sum`, `tar`, and xz support.

`make setup` downloads Zig 0.16.0 from ziglang.org into `.tools/` and verifies its pinned SHA-256 checksum. No system Zig installation is used.

## Implementations

The Zig implementation is in `src/root.zig` and exposes `format`, `open`, `write_block`, `read_block`, `flush`, and `close`. The separate Rust crate is in `rust/` and exposes `format`, `open`, and `Volume::{write_block, read_block, flush, close}`.

## Zig and combined edit loop

```sh
make build
make lint
make test
make run
```

- `make build`, `make lint`, and `make test` check both implementations; append `-zig` or `-rust` to target one.
- `make run` runs the Zig V0.4 public-API scenario: format, open, zero-read, write/read, flush/close, reopen, and read again.

## Rust edit/test loop

```sh
cd rust
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The Rust acceptance test exercises the same V0.4 sequence through its public API.

There is no default container setup because the tests intentionally exercise the host kernel and direct-I/O path.

## Boundaries

The prototype fails closed on invalid metadata, checksum mismatch, short or failed I/O, log exhaustion, and an unreplayed crash tail; an I/O failure poisons the open volume. V0.5+ defers overwrite workloads, crash-tail replay, A/B checkpoint fallback, broader fault injection, compaction/wraparound, concurrency, discard, replication, and the ublk frontend.

See the [design and version roadmap](coding-project-tigerbeetle-railway.md). `ublksrv/` is optional, ignored reference material for the later V0.10 ublk milestone, not a current dependency.
