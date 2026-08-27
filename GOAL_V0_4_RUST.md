# Goal: native Rust implementation through V0.4

Use `coding-project-tigerbeetle-railway.md` as the format and behavior specification. Build a separate Rust implementation continuously from an empty crate through V0.4, preserving a reviewable sequence of small green commits.

The existing Zig implementation in `src/root.zig` (git branch ralph/v0.4, was created from a similar goal to this one by another agent) is a working reference for persistent layout, invariants, failure handling and tests. Read it when useful, but do not translate it mechanically. Design idiomatic Rust ownership, errors, modules and tests. Similar data structures and control flow are expected where they follow from the same storage format.

## Isolation

Keep the Rust implementation under `rust/` and in a separate git branch. Do not modify the Zig implementation, its build files, or its tests. Both implementations must remain independently buildable. Update Makefile to have targets to both implementations.

Keep dependencies minimal: the low-level `io-uring` crate and an XXH3 implementation are justified; prefer the standard library otherwise. Do not introduce an async runtime.

`ublksrv/` is read-only local reference material for later work. Do not modify, build, commit or integrate it before V0.10.

## Cumulative acceptance

Every Rust implementation commit from the initial crate scaffold onward must pass:

```sh
cd rust
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
Wrapping these commands in makefile targets is recommended.

## Functional test

V0.4 is complete only when one automated functional test proves this complete sequence without manual inspection:

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

The test must exercise the public Rust `format`, `open`, `write_block`, `read_block`, `flush` and `close` operations.

## Compatibility

Preserve the persistent format specified by the design document: explicit little-endian encoding, fixed offsets, XXH3-64 inputs, footer ordering and checkpoint publication rules. Add pinned cross-language checksum vectors. Full Zig↔Rust image interoperability is desirable evidence, but it must not replace the Rust public-API acceptance test or turn the task into a line-by-line port.

## Ordered milestones

- **V0.0 — crate and I/O gate:** establish the Rust format/clippy/test loop; prove an aligned low-level `io_uring` write, fsync, close/reopen and read/compare on a real file opened with `O_DIRECT`. Validate every CQE result, including exact byte counts and buffer lifetimes.
- **V0.1 — format:** explicitly encode and checksum the two valid empty 4 KiB checkpoint descriptors; calculate non-overlapping checkpoint bodies and log start; reject invalid volume size, alignment and backing capacity.
- **V0.2 — open:** decode and validate both checkpoint descriptors, cross-check their immutable volume identity and geometry, select the highest valid generation, allocate the configured SoA mapping, and reconstruct an empty `Volume`.
- **V0.3 — bootstrap, write and shutdown:** append one payload plus its footer with one exact-length vectored `io_uring` write, publish the mapping only after completion, implement flush, and persist a clean checkpoint on close.
- **V0.4 — read:** return zeros for unmapped LBAs; read mapped payload blocks and validate their XXH3-64 checksums before and after reopen.

Implement the simplest bounded V0 behavior only. Fail closed on ambiguous persistent state or unreplayed tails. No ublk frontend, async runtime, concurrency, compaction, wraparound, discard, performance framework, simulator or speculative abstraction.
