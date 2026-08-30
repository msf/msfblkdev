# ADR-05 Goal 3: backing-medium fault resilience

Date: 2026-08-30
Author: Miguel Filipe
Status: proposed
Goal status: not started
On-disk format: 1 (unchanged)
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md), [TMD: Deterministic fault testing and simulation](DETERMINISTIC_FAULT_TESTING_AND_SIMULATION.md)
Would update: ADR-03 crash-boundary list and its failpoint-based evidence when accepted and implemented

## Context

[Goal 3](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md#goal-3-backing-medium-fault-resilience) requires deterministic backing-medium faults and leaves one question open: *"We will evaluate `dm-flakey`, `dm-log-writes`, `dm-error` and existing `fio` facilities before writing a custom faulting backend."* This ADR answers that question with measurements taken on 2026-08-30, and records the mechanism that follows from the answer.

Two properties of the current code decide the shape of the answer.

First, the engine calls `io_uring` directly. Every backing operation in `rust/src/lib.rs` and `rust/src/checkpoint.rs` goes through one function, `submit_exact` (`rust/src/lib.rs:510`), and every operation is synchronous, 4 KiB aligned, a 4 KiB multiple, and exact length. There are 11 call sites. The engine therefore has one narrow seam, not a scattered I/O surface.

Second, crash placement uses named failpoints compiled into the engine. `pause_at_test_failpoint` (`rust/src/lib.rs:51`) prints a name and parks the thread forever so a parent process can `SIGKILL` it. Ten failpoint families exist behind `#[cfg(any(test, feature = "test-failpoints"))]`, selected at runtime by the `BLOCK_STORAGE_TEST_FAILPOINT` environment variable. The mechanism reaches into the lab too: `rust/src/bin/block-storage-lab/ublk_fio/scenario.rs:4` pins `descriptor-write-complete`, `scenario_run.rs:215` sets the variable, and `Makefile:49` builds the daemon with `--features test-failpoints`.

That mechanism cannot express a medium fault at all. It only pauses a process. Making a write return `EIO`, land short, tear across a record, or silently disappear requires control of the device *below* the engine, which the engine does not currently have.

I also think the failpoints are the wrong abstraction independently of Goal 3. A source-level hook can name a boundary that has no durable consequence, and one of ours does: see [Which crash boundary does not exist?](#which-crash-boundary-does-not-exist).

## Decision

Put backing I/O behind one engine-owned trait, construct a `Volume` from an injected implementation, and delete the named-failpoint mechanism. Express every fault, including process death, as an implementation of that trait.

```rust
/// One 4 KiB block, aligned for O_DIRECT. A `&[Block]` is contiguous, so a
/// slice of N blocks is one 4096 * N region.
#[repr(C, align(4096))]
pub struct Block(pub [u8; BLOCK_SIZE]);

/// Backing-store operations the engine needs today.
///
/// This is NOT a general block-device abstraction, and it is not a stable
/// public API. It is the exact set of operations the current call sites use,
/// and it is expected to change with the engine and with the on-disk format.
/// ADR-04 already names two required changes: a write that takes a segment
/// list, so a 1 to 338 payload-block record is not staged into one 1.3 MiB
/// buffer, and a `block_count` that advances at runtime after a durable
/// `PROVISION_BACKING` record. Grow the trait when a call site needs it, not
/// before.
pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read(&mut self, first_block: u64, blocks: &mut [Block]) -> io::Result<()>;
    fn write(&mut self, first_block: u64, blocks: &[Block]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}
```

The interface is block indexed, not byte indexed, because every existing call site already computes `u64::from(block) * BLOCK_SIZE as u64`. Block indexing removes that arithmetic from 11 places, makes an out-of-geometry access a typed concern, and makes an in-memory implementation a map from block index to block. `Block` replaces `AlignedBlock`, `AlignedRecord` and `AlignedDescriptors` with one type.

The engine loses the `io_uring` protocol entirely. `submit_exact`, `wait_exact`, `wait_exact_with`, the `user_data` and exact-result checks, the completion-flag checks, and the `Option<IoUring>` poisoning dance (`rust/src/lib.rs:510-580`) all move inside `UringDevice`. `Volume::failed` stays, because that is engine state.

`Volume` holds `Box<dyn BlockDevice>`, not a type parameter. One vtable dispatch per operation is noise next to a submit-and-wait syscall, and a generic `Volume<D>` would propagate through the 1378-line ublk daemon for no measured benefit.

The public entry points keep their current shape and gain injectable siblings:

```rust
pub fn open(path: impl AsRef<Path>) -> io::Result<Volume>;
pub fn open_with_options(path: impl AsRef<Path>, options: VolumeOpenOptions) -> io::Result<Volume>;
pub fn open_on(device: Box<dyn BlockDevice>, options: VolumeOpenOptions) -> io::Result<Volume>;

pub fn format(path: impl AsRef<Path>, volume_bytes: u64) -> io::Result<()>;
pub fn format_on(device: &mut dyn BlockDevice, volume_bytes: u64, volume_id: u64) -> io::Result<()>;
```

`format_on` takes the volume ID as a parameter. `random_volume_id` (`rust/src/lib.rs:285`) reads `/dev/urandom` inside `format` today, and it is the only nondeterministic input in the engine besides the device. One `u64` parameter is the whole fix. We will not add a random-source trait for it.

Beyond the device and that `u64`, the engine has nothing else to inject. Every `Instant` and `SystemTime` in the tree is in `rust/src/bin/`, none in `lib.rs` or `checkpoint.rs`, and the engine has no threads and no network. Deterministic execution of this engine reduces to controlling the device and one integer.

## What did the spike measure?

All results below are from one machine on 2026-08-30: Linux `7.0.0-29-generic` x86_64, `qemu-img` 8.2.2, `fio` 3.36, unprivileged user `miguel`. They are measurements, not estimates. Where I state a mechanism rather than an observation, I say so.

### Which fault tools exist locally?

| Tool | State | Note |
|---|---|---|
| `dm-flakey` | module present | needs root |
| `dm-log-writes` | module present | needs root; userspace `replay-log` not installed |
| `dm-delay` | module present | needs root |
| `dm-dust` | **not built** | `# CONFIG_DM_DUST is not set` on all 24 installed kernel configs |
| `dm-error` | in device-mapper core | core is built in (`CONFIG_BLK_DEV_DM=y`); the `error` target needs no module (mechanism, not measured here) |
| `scsi_debug` | module present | needs root; has `every_nth`, `medium_error_start`, `opts` timeout and DIF/DIX |
| `null_blk` | module present | needs root; this build's `modinfo` exposes no bad-block parameter |
| block-layer fault injection | **unavailable** | `CONFIG_FAULT_INJECTION` is not set, so `fail_make_request` does not exist |
| `qemu-storage-daemon` | installed, 8.2.2 | supports `blkdebug`, `blklogwrites`, `blkverify`, FUSE export |
| `fio` | installed, 3.36 | no fault-injection engine in its engine list |
| `nbdkit`, `blktrace`, `replay-log` | not installed | |

### Can any of this run without root?

No, for everything in device-mapper and ublk. This is a permission question the user asked directly, so the measurement is recorded in full:

| Control node | Owner and mode | Result of an unprivileged open |
|---|---|---|
| `/dev/mapper/control` | `root:root 0600` | `EACCES` |
| `/dev/ublk-control` | `root:root 0600` | `EACCES` |
| `/dev/loop-control` | `root:disk 0660` | `EACCES` (user is not in `disk`) |
| `/dev/fuse` | `root:root 0666` | opens |

No udev rule on this system grants device-mapper or ublk to a non-root group. Adding the user to `disk` would grant loop-device creation and nothing else, and `dm-flakey` still needs `CAP_SYS_ADMIN` for the device-mapper ioctl regardless of node permissions. Unprivileged user namespaces do not provide an escape either: `kernel.unprivileged_userns_clone` is 1, but AppArmor's `apparmor_restrict_unprivileged_userns` is 1 and writing `/proc/self/uid_map` after `unshare` fails with `EPERM`.

So the privileged tier stays privileged, exactly like `make test-ublk-fio` today (`Makefile:47`, and the operator instruction at `ublk_fio/daemon.rs:58`).

### What can we inject without root?

`qemu-storage-daemon` with a `blkdebug` node and a FUSE export gives real kernel-level fault injection to an unprivileged user. Verified end to end against `O_DIRECT` and against `io_uring`:

- **Unconditional `EIO` on every write:** all `pwrite`, `fsync` and `close` calls fail, and 0 bytes reach the base image.
- **Fail exactly the third write, once:** `ok, ok, EIO, ok, ok`, `fsync` clean. The rule is consumed, so a second pass is clean.
- **`EIO` on `flush_to_disk` only:** writes succeed, `fsync` fails.
- **`errno = 28`:** the caller sees `ENOSPC`.
- **`io_uring` with `direct=1`:** `fio` completed 1024 write and 1024 read operations through the export on a clean node, and reported `io_u error ... write offset=0` when the `EIO` rule was active.
- **Engine-shaped access:** opening the export with `O_DIRECT | O_CLOEXEC` works, `lseek(SEEK_END)` returns the exact size, `flock(LOCK_EX | LOCK_NB)` succeeds and a second holder is correctly refused. The engine performs no file-type check (`rust/src/lib.rs:607`, `:703`), so it can run against this node unmodified.

One configuration detail is load bearing and cost me several attempts: **`blkdebug` emits no events unless a format driver sits above it.** Exporting the `blkdebug` node directly produced zero injected errors. The working stack is `file` then `blkdebug` then `raw`, with the `raw` node exported.

Two limits, both measured:

1. **No short or partial operation.** `blkdebug`'s action vocabulary has error injection and latency, and nothing that completes an operation with fewer bytes than requested. It cannot express `ShortOperation`.
2. **Unconditional rules leak across operation types.** With an unconditional write rule, reads issued afterwards also failed. With an unconditional read rule, writes issued after a read event also failed. Single-shot rules (`once = "on"`) did *not* leak: the surrounding operations, `fsync` and later reads were all clean. I have not identified the mechanism. The practical rule that follows is to use `once` or state-machine rules and never an unconditional one, and I will treat any unconditional-rule result as untrustworthy until that is explained.

### What is `dm-flakey` and `dm-log-writes` uniquely good for?

`dm-flakey` is time based, not operation based: its table takes an up interval and a down interval in seconds. That makes it a poor instrument for landing a fault on operation `N`. It has two capabilities nothing else here has:

- `drop_writes` silently ignores writes while reads keep working, which models an acknowledged write that never became durable.
- `corrupt_bio_byte`, `random_read_corrupt` and `random_write_corrupt` replace bytes in flight, which models medium corruption below the checksum.

`dm-log-writes` records completed writes ordered by flush, which is exactly the disk content a power failure would leave. Its replay tool is unpackaged and absent here. QEMU's `blklogwrites` driver writes the same log format and needs no root, but I did **not** verify format compatibility, and a small replayer would have to be written either way.

`dm-error` is redundant: `dm-flakey`'s `error_writes` and `error_reads` cover it with more control. `dm-dust` is not compilable on this kernel without a custom build, so precise bad-sector emulation is not available from it.

### Do we still need a custom faulting backend?

Yes. No available tool can produce a short operation, a torn record with an exact durable-block count, or a misdirected write, and none is operation indexed without root. `fio` verifies data, it does not inject faults. That answers the ADR-01 question: build the in-process backend, and keep `dm-flakey` and `dm-log-writes` as a separate privileged tier, because they cover corruption and write ordering that the in-process backend can only assert about itself.

## How do we test faults?

Four tiers. Each names what it proves and what it cannot prove.

| Tier | Mechanism | Privilege | Runs in |
|---|---|---|---|
| 0 | `MemoryDevice`, in process, separate pending and durable state | none | `make test` |
| 1 | `CrashDevice` wrapping `UringDevice`, real `io_uring` and `O_DIRECT` | none | `make test`, `make test-acceptance` |
| 2 | `qemu-storage-daemon` `blkdebug` under a `raw` node, FUSE exported | none | `make test-fault` (new, opt-in) |
| 3 | `dm-flakey` and `dm-log-writes` over a loop device, under ublk and `fio` | root | operator procedure, not a `make` target |

Fault coverage, using the `StorageFault` vocabulary from the TMD (`DETERMINISTIC_FAULT_TESTING_AND_SIMULATION.md:68`):

| Fault | Tier 0 | Tier 1 | Tier 2 | Tier 3 |
|---|---|---|---|---|
| `Error` | yes | no | yes, any errno, at a chosen operation index | yes, `error_reads` / `error_writes` |
| `Delay` | yes | no | yes, `latency-ns` | yes, `dm-delay` |
| `ShortOperation` | yes | no | no | no |
| `Tear { durable_blocks }` | yes | no | no | no |
| `Corrupt` | yes | no | no | yes, `corrupt_bio_byte`, `random_*_corrupt` |
| `Misdirect` | yes | no | no | no |
| `LosePending` | yes | real, not injected | no | yes, `drop_writes` |
| `FailDevice` | yes | no | yes, but see the leak finding above | yes, down interval |
| process death at operation `N` | no | yes | no | no |

Tier 1 replaces the failpoint mechanism. `CrashDevice` counts completed operations and calls `kill(getpid(), SIGKILL)` after the Nth:

```rust
let device = CrashDevice::after(7, UringDevice::open(path)?);
let mut volume = open_on(Box::new(device), options)?;
```

This keeps every property the current tests rely on. The I/O is real `io_uring` against a real `O_DIRECT` file, so the durable bytes are still the kernel's verdict rather than our model of it. `SIGKILL` is uncatchable and unblockable whether it is raised inside or outside the process, and kernel teardown is identical (POSIX semantics: knowledge, not measured here). What disappears is the handshake protocol: no printed boundary name, no parked thread, no parent polling stdout, no race window between the report and the signal.

Operation indexing is also strictly more expressive than named sites. A sweep over `crash_after ∈ 0..N` covers every boundary in ADR-03's list, including the 339 stale-tail clear writes and each checkpoint-body block, plus boundaries nobody enumerated. It has exactly one blind spot, described next.

### Which crash boundary does not exist?

ADR-03 requires nine crash boundaries (`ADR-03:139-149`). Eight are device operations. One is not: *"mapping publication, before flush submission"*, the `mapping-published` failpoint at `rust/src/lib.rs:264`.

Between `record-write-complete` (`rust/src/lib.rs:256`) and `mapping-published` (`:264`) the engine issues no I/O. It updates `physical_blocks`, `checksums`, `last_lsn`, `last_footer_block` and `log_bytes_since_checkpoint`, all in memory, all lost to the `SIGKILL`. The two boundaries therefore have identical durable state.

The tests agree. `recover_after_kill_at_record_write_boundary` and `recover_after_kill_at_mapping_publication_boundary` (`rust/src/tests/process_crash.rs:169-195`) have identical setup, identical arguments (`flush_write: false`, `require_new_state: false`) and the identical assertion `actual == old || actual == new`. The same scenario runs twice.

This is the concrete argument against source-level failpoints. A hook can name a boundary with no observable durable state. A device-level mechanism can only express boundaries that exist.

This ADR proposes that ADR-03 drop the mapping-publication boundary from its required list and merge its acceptance item into the record-write item. It does not apply that amendment: ADR-03 is accepted, and the change belongs with the implementation.

## Preconditions

- ADR-03 goal exit is complete, including every V0.7 and V0.8 acceptance item.
- The Git worktree is clean.
- `make lint` and `make test` pass.

No delivery work starts before these checks pass. Goal 3 refactors the I/O layer of the engine that ADR-03 is currently validating, and the V0.6 evidence is recorded against the present `lib.rs` and `checkpoint.rs`.

## Delivery 1: V0.9 injectable backing device

Extraction only. No behavior change, no new test coverage, no format change.

- Add `Block`, `BlockDevice` and `UringDevice`. Move `submit_exact`, `wait_exact`, `wait_exact_with` and the ring poisoning into `UringDevice`.
- Replace `Volume.backing: File` and `Volume.ring: Option<IoUring>` with one boxed device. Collapse the helpers that take `(ring, backing)` to one parameter.
- Add `open_on` and `format_on`. Keep `open`, `open_with_options` and `format` constructing a `UringDevice` internally, so the ublk daemon is unchanged.
- Add `CrashDevice`. Convert every failpoint-driven test to an operation index.
- Delete `pause_at_test_failpoint`, all ten failpoint name constants, every `#[cfg(any(test, feature = "test-failpoints"))]` in `lib.rs` and `checkpoint.rs`, the `test-failpoints` Cargo feature, and the `BLOCK_STORAGE_TEST_FAILPOINT` plumbing in `ublk_fio/scenario.rs`, `ublk_fio/scenario_run.rs`, `ublk_fio/daemon.rs` and `Makefile:49`.
- Correct the `Writev` claim in the TMD (`:187`). The two iovecs at `rust/src/lib.rs:244` point at contiguous halves of one buffer, so the operation is byte-identical to a single 8 KiB write and is not a barrier to any alternative backend.

Acceptance tests:

- [ ] Every test that passed before the extraction passes after it, with no test weakened, skipped or ignored.
- [ ] `rg 'test-failpoints|BLOCK_STORAGE_TEST_FAILPOINT|pause_at_test_failpoint'` returns nothing.
- [ ] `UringDevice` rejects a short completion, an unexpected completion, an unexpected extra completion and a non-zero completion flag, with the same errors the engine produced before.
- [ ] `CrashDevice` dies at the requested operation index, verified by the parent observing `SIGKILL` rather than an exit code.
- [ ] A sweep over every operation index of a single write plus flush recovers a valid prefix at each index.
- [ ] `format_on` with a fixed volume ID produces byte-identical images across runs.
- [ ] `make lint` and `make test` pass.

## Delivery 2: V1.0 in-memory device and exact fault plans

- Add `MemoryDevice` with separate pending and durable block state, and a scripted plan of `(operation_index, StorageFault)` pairs. Exact plans only. No seeded generation.
- Add a reference model: an ordinary map from LBA to block, updated on each acknowledged write and truncated to the last flush on crash. Compare recovered state against it after every plan.
- Implement the invariants listed in the TMD (`:90-98`).

The highest-value case that is impossible to write today is the payload and footer pair. One record is one 8 KiB write; a `Tear { durable_blocks }` plan enumerates all four durable outcomes (neither, payload only, footer only, both) and asserts that `decode_write_tail_footer` (`rust/src/lib.rs:395`) rejects the footer-only case. Reaching that today needs an exact interruption inside one operation, which no tier except this one can produce.

Acceptance tests:

- [ ] An acknowledged durable prefix survives a crash under every exact plan.
- [ ] Recovery never exposes a footer without its complete record, proven by the four-outcome tear table for one record.
- [ ] A short operation on any engine write is reported as an error and publishes no mapping.
- [ ] An `EIO` at each checkpoint-publication step leaves the previous checkpoint selectable.
- [ ] Recovery never reads or writes outside the backing geometry, enforced by `MemoryDevice` returning an error for an out-of-range block index.
- [ ] Recovered logical contents equal the reference model for every plan.
- [ ] The suite runs under `make test` in the existing time budget.

## Delivery 3: V1.1 unprivileged real-kernel faults

Tier 2, through the ublk daemon and `fio`, so the adapter and the operating system are in the path.

The daemon's `format` refuses an existing path (`create_new`, `rust/src/bin/block-storage-ublk.rs:733`), verified: formatting the FUSE export fails with `File exists`. The harness therefore formats a regular file first and serves against the fault node afterwards.

- [ ] The harness starts and stops `qemu-storage-daemon`, and cleans up its FUSE mount and process on every exit path including timeout.
- [ ] The stack is `file`, then `blkdebug`, then `raw`, with the `raw` node exported. A test asserts the injected error actually fires, so a silently ineffective stack fails loudly.
- [ ] `EIO` at a chosen operation index during normal write, during checkpoint publication, and during recovery: the daemon stays alive, the volume fails closed, and ublk completes requests with an error instead of hanging.
- [ ] `ENOSPC` from the device is distinguished from the engine's own log-full `ENOSPC`.
- [ ] `EIO` on `fsync` alone fails the volume without losing an earlier durable prefix.
- [ ] Every plan uses `once` or state-machine rules. No unconditional rule appears in the suite.
- [ ] `make test-fault` skips with a clear message, and creates no resources, when `qemu-storage-daemon` is absent.

## Delivery 4: V1.2 privileged medium faults

Tier 3. Operator run, under the ADR-03 operator-safety rules (`ADR-03:296-307`). Never run by Ralph, never targeting a system device.

- [ ] A documented procedure builds a `dm-flakey` device over a loop device over a disposable file, and tears it down on every exit path.
- [ ] `drop_writes` during a `fio` run: after restart, the recovered state is a valid prefix and every flush-covered write is present.
- [ ] `corrupt_bio_byte` on a payload block: `read_block` returns a checksum error rather than corrupt data.
- [ ] `corrupt_bio_byte` on a checkpoint descriptor and on a checkpoint body: startup falls back to the older root.
- [ ] A `dm-log-writes` or `blklogwrites` run produces a write log whose flush-ordered prefix, replayed into a fresh image, opens and contains every acknowledged durable write.
- [ ] Evidence records the commit, kernel, backing type, exact commands and results, matching the ADR-03 evidence format.

## What must be re-run?

Deleting the failpoints invalidates the *evidence* for nine V0.6 acceptance items (`ADR-03:159-176`), part of one more (`:181`), and one goal-exit item (`:317`). The acceptance items themselves are mechanism neutral, so the durability contract they state survives unchanged. Each needs a fresh run and a fresh date against the operation-indexed mechanism.

This is wall-clock cost, not design cost. `Mode::EngineCrash` already allots 1800 and 3600 seconds (`rust/src/bin/block-storage-lab/config.rs:49`), and the last recorded acceptance log took 608 seconds for the all-boundary stale-tail case alone (`ADR-03:318`). The operation sweep replaces several separately named runs with one loop.

The handshake-based tests that do not use a failpoint (`recover_one_flushed_write_after_sigkill_without_close`, `recover_multiple_flushed_writes_and_overwrites_after_sigkill`, `recover_after_uncaught_unwinding_panic_without_close`) print their own handshake from test code and are unaffected.

## What stays real?

The simulator tiers do not replace, and Delivery 1 must not weaken:

- native `io_uring` completion tests (`rust/src/tests/io_uring.rs`);
- `O_DIRECT` alignment and real filesystem behavior;
- process-level `SIGKILL` and restart tests, which is why Tier 1 wraps `UringDevice` instead of replacing it;
- ublk request handling;
- direct 4 KiB `fio` verification.

A `MemoryDevice` that never short-writes will hide a real kernel that does. Where the fault vocabulary overlaps between tiers, the same scenario table runs against both implementations.

## Consequences

We accept:

- one vtable dispatch per backing operation, unmeasured and expected to be irrelevant next to a syscall;
- a re-run of ten ADR-03 evidence lines;
- a new opt-in dependency on `qemu-storage-daemon` for Tier 2, skipped when absent;
- `ShortOperation`, `Tear` and `Misdirect` provable only in Tier 0, so their real-kernel behavior stays inferred;
- a privileged tier that CI cannot run, exactly like `make test-ublk-fio` today.

We gain:

- an engine with no test-only code in it, and no `test-failpoints` feature;
- crash placement at any operation index instead of ten named sites;
- the first faults we can express at all: error, short, torn, corrupt, misdirected, lost-pending;
- a recorded answer to the ADR-01 tooling question, with measurements;
- the interface ADR-04 needs for vectored writes and runtime-growable capacity, introduced before that format work rather than during it.

The trait is the load-bearing commitment, and it is deliberately provisional. Revisit its shape when ADR-04 implementation starts, because `PROVISION_BACKING` makes `block_count` mutable and 338-block records make a single-slice write wasteful. Revisit the tier split if `CONFIG_FAULT_INJECTION` or `dm-dust` become available on our kernels, which would move some Tier 0 cases into a real-kernel tier.

## Open questions

- Should Tier 0 plans live in a shared table that Tier 2 also consumes, or should each tier own its plans? A shared table is more honest about divergence between model and kernel, and costs an abstraction.
- Does QEMU's `blklogwrites` produce a log that a `dm-log-writes` replayer can read? Unverified. If not, we write a replayer for one of the two formats, not both.
- What explains the `blkdebug` cross-operation leak under unconditional rules? Until it is explained, unconditional rules stay out of the suite.
- Where does seeded exploration enter? The TMD sequences it after exact plans and invariants are stable (`:119`). This ADR does not schedule it.
