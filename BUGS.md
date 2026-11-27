# Known bugs and TODOs

Authored by LLM from a design session on 2026-11-25; findings reviewed by human in that session.
GitHub issues are disabled on `msf/msfblkdev`, so we track them here.

Each entry names the code sites, what is verified and what is inferred, and a proposed fix.

## Not yet a bug: segments sit 8 KiB off device alignment

Segments start at block 2, after the two descriptors, so they sit 8 KiB off natural device
alignment (ADR-08 [Persistent layout](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md#persistent-layout)).
A device with a `discard_granularity` above 4 KiB may ignore the unaligned head and tail of each
segment discard. Measure on a real device before changing the layout.

## TODO-1 Make the engine deterministic for a given seed

Found 2026-11-25. Status: open.

Goal: given a fixed volume ID, request sequence and clock trace, the engine produces a
byte-identical backing image, in live runs and in tests. ADR-05 Delivery 1 already requires
byte-identical formatted images for a fixed volume ID.

Candidate sources, found by grep and not yet analysed for effect:

| Source | Sites |
|---|---|
| `std::hash::RandomState`, seeded per process. It matters only where iteration order reaches a decision or the disk. | mapping cache lookup (`rust/src/mapping_cache/sizing.rs:5`, `89`); write buffer `slot_of_lba` (`rust/src/lib.rs:272`, `302-303`) |
| Time. Idle and interval deadlines decide when cleaning and checkpoints run. The engine and the daemon now read time and sleep only through the `Clock` trait (`rust/src/clock.rs`), and tests can inject `ManualClock`. The daemon's maintenance timers are still io_uring timeouts on the kernel's monotonic clock. | `rust/src/bin/block-storage-ublk.rs` (`Maintenance`); `Volume::maintain_for` |
| The cleaner thread. Proposals reach the writer at times set by thread scheduling. | `rust/src/format_v4/reclaim/cleaner.rs` |
| The volume ID comes from `/dev/urandom`. It is the intended seed, but format has no way to set it. | `rust/src/lib.rs:410-414`, `rust/src/format_v4/storage.rs:75` |

Steps: list every source; seed hashers and any random choice from the volume ID; drive the
daemon's timers from the injected clock in a test mode; make the points where cleaner results
reach the writer deterministic in a test mode; let format take an explicit volume ID, through a
random source that tests can replace as they replace the clock.

## Checked, not a bug: on-disk format version

Checked 2026-11-25. Every metadata block already carries the format version, and the code rejects
any other version before it trusts a field:

- The checkpoint descriptor stores magic `VBLC` and version byte 4 at offset 4
  (`rust/src/format_v4/descriptor.rs:116-120`).
- Every record header stores magic `VBLF` and version byte 4 at offset 4
  (`rust/src/format_v4.rs:184-192`).
- `validate_block_checksum` rejects a magic or version mismatch (`rust/src/format_v4.rs:224-238`).
  `decode_fields` and the tail probe reject unknown versions and kinds
  (`rust/src/format_v4.rs:195-206`, `220-222`).
- Payload blocks have no header, by design. Their data-record footer carries the version.

The remaining gap is that the version is one byte with no compatible or incompatible feature flags.
ADR-09 leaves that decision open until compressed records need it.
