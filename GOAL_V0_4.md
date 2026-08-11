# Goal: continuous implementation through V0.4

Use `coding-project-tigerbeetle-railway.md` as the format and behavior specification. Work continuously from the empty repository through V0.4, preserving a reviewable sequence of small green commits.

## Toolchain

Use only the pinned project-local Zig compiler:

```sh
.tools/zig-x86_64-linux-0.16.0/zig
```

Do not switch language or Zig version. If the low-level `std.os.linux.IoUring` path cannot satisfy V0.0 cleanly, stop and report the concrete blocker for the coordinator to decide.

`ublksrv/` is a read-only local checkout for later reference. Do not modify, build, commit or integrate it before V0.10.

## Cumulative acceptance

Every implementation commit from the V0.0 build scaffold onward must pass (the preceding planning-only commits contain no build yet):

```sh
.tools/zig-x86_64-linux-0.16.0/zig build test
```

V0.4 is complete only when one automated functional test proves this complete sequence without inspecting state manually:

```text
format temporary backing image
→ open empty volume
→ read an unwritten LBA as 4096 zero bytes
→ write a known 4096-byte block
→ read and compare it
→ flush and cleanly close
→ reopen
→ read and compare the same block again
```

The test must exercise the public `format`, `open`, `write_block`, `read_block`, `flush` and `close` operations.

## Ordered milestones

- **V0.0 — build and I/O gate:** establish the Zig build/test loop; prove an aligned low-level `io_uring` write, fsync, close/reopen and read/compare on a real file opened for direct I/O. Validate every CQE result, including exact byte counts.
- **V0.1 — format:** explicitly encode and checksum the two valid empty 4 KiB checkpoint descriptors; calculate non-overlapping checkpoint bodies and log start; reject invalid volume size, alignment and backing capacity.
- **V0.2 — open:** decode and validate checkpoint descriptors, select the highest valid generation, allocate the configured SoA mapping, and reconstruct an empty `Volume`.
- **V0.3 — bootstrap, write and shutdown:** append one payload plus its footer with one exact-length `WRITEV`, publish the mapping only after completion, implement flush, and persist a clean checkpoint on close.
- **V0.4 — read:** return zeros for unmapped LBAs; read mapped payload blocks and validate their XXH3-64 checksums before and after reopen.

Implement the simplest bounded V0 behavior only. No ublk frontend, concurrency, compaction, wraparound, discard, performance framework, simulator or speculative abstraction.
