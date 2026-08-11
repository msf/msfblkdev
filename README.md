# my-block-storage

A Linux-only V0.4 prototype of a 4 KiB log-structured block-storage engine. It exposes the Zig public API (`format`, `open`, `write_block`, `read_block`, `flush`, and `close`); it does not yet expose a block device.

## Prerequisites

Linux x86_64, a kernel with `io_uring`, and a backing filesystem or device that supports `O_DIRECT`. Setup also needs `make`, `curl`, `sha256sum`, `tar`, and xz support.

`make setup` downloads Zig 0.16.0 from ziglang.org into `.tools/` and verifies its pinned SHA-256 checksum. No system Zig installation is used.

## Edit loop

```sh
make build
make lint
make test
make run
```

- `make build` builds `zig-out/lib/libblock-storage.a`.
- `make lint` checks Zig formatting.
- `make test` runs the complete test suite.
- `make run` runs the V0.4 public-API black-box scenario: format, open, zero-read, write/read, flush/close, reopen, and read again.

There is no default container setup because these tests intentionally exercise the host kernel and direct-I/O path.

## Boundaries

The prototype fails closed on invalid metadata, checksum mismatch, short or failed I/O, log exhaustion, and an unreplayed crash tail; an I/O failure poisons the open volume. V0.5+ defers overwrite workloads, crash-tail replay, A/B checkpoint fallback, broader fault injection, compaction/wraparound, concurrency, discard, replication, and the ublk frontend.

See the [design and version roadmap](coding-project-tigerbeetle-railway.md). `ublksrv/` is optional, ignored reference material for the later V0.10 ublk milestone, not a current dependency.
