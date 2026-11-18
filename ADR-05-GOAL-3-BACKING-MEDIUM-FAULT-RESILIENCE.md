# ADR-05 Fault model, failure handling and fault testing

Date: 2026-08-30
Revised: 2026-10-06
Author: Miguel Filipe
Status: proposed; design under review
Goal status: implementation not started
On-disk format: released baseline is ADR-07 format 3; ADR-08 format 4 is under separate evaluation
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md), [ADR-04](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md), [ADR-06](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md), [ADR-07](ADR-07-PAGED-MAPPING-AND-CACHE.md), [ADR-08](ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md), [ADR-10](ADR-10-WORKBENCH-TESTS-AND-REPORTS.md)
Consolidates: the 2026-09-12 fault-policy discussion and `DETERMINISTIC_FAULT_TESTING_AND_SIMULATION.md`
Would update: ADR-01's V0 fault model and recovery rules, and affected ADR-03 acceptance criteria, when accepted and implemented

## Context

ADR-03 has accepted process-crash and live ublk evidence. It does not establish behavior under power loss, torn medium writes or the full range of device errors. The existing engine reports backing errors, but we have not implemented the fault-injection system proposed here.

This ADR owns the proposed fault model, failure policy, test mechanisms and simulation direction. ADR-01 and ADR-03 remain the accepted baseline. Their historical tests and evidence do not prove this proposal. Format ADRs define encodings and refer here for proposed fault behavior.

v1.2.3 already tests format-3 corruption, checkpoint fallback, repeated recovery and process crashes.
We will extend those tests. Compaction and space reclamation are implemented on a separate branch
and remain under operator evaluation. This revision changes the testing plan, not that implementation.

Existing acceptance gates remain required. The [open decisions](#open-decisions-before-implementation)
must be resolved before implementation of this ADR starts.

## Proposed decision

Define expected failure behavior before building its tests. Put backing I/O behind a narrow engine-owned interface. Start with exact fault plans and a reference model, then compare the same cases against real I/O. Add seeded exploration only after the exact cases are stable.

The scope is local storage. Replication, automated repair, network simulation and Byzantine tolerance are not initial deliveries. Their design implications are recorded here, not in separate competing fault policies.

## Fault model and failure policy

### Assumptions and durability

The engine has one serialized writer and trusts process memory and execution. Persistent bytes are untrusted input. Invalid structures must produce an error, not a panic.

A successful WRITE completion alone does not make the write durable. A successful FLUSH covers all earlier completed writes, assuming the backing store honors successful fsync. Crash recovery preserves flush-covered history and may retain an additional unflushed prefix. A reference model must not discard every unflushed write automatically.

Later medium corruption can destroy durable bytes. A device that loses writes covered by successful fsync violates the durability assumption. Tests must distinguish these cases from failures of crash recovery under that assumption.

The model covers process loss and non-adversarial backing faults: reported errors, short operations, timeouts, disappearance, torn writes, bit flips, stale reads, misdirected or lost writes, and reordering. XXH3-64 detects accidental corruption; it does not authenticate data. Memory corruption and Byzantine storage are excluded. `SIGKILL` tests do not prove power-loss or medium-fault behavior.

### Metadata and payload validation

Checkpoint loading validates descriptors and complete mapping bodies, not referenced payloads. Replay validates record metadata and applies mappings in LSN order. It does not perform a dedicated payload checksum scan. Online reads validate the current mapped payload before returning bytes.

Payloads are opaque blocks, not commands required to compute state. Valid metadata suffices to reconstruct their mappings. A valid header or footer proves neither payload integrity nor durability. An eager payload scan could detect damage earlier, even without replication, but is optional validation work rather than a recovery-correctness requirement.

A recognizable format-1 footer matches the expected magic, supported version and kind, volume ID, next LSN, previous-footer link and self-position. Its derived payload count and physical range fit the format and backing bounds. Matching fields with a bad checksum identify recognizable metadata corruption; they do not make the metadata safe to apply.

A valid footer also passes its checksum, LBA bounds and uniqueness checks, and zero-padding checks. Format 2 uses its record header for this classification. Its exact recognition and chain rules must agree with its final encoding.

### Proposed responses

| Condition | Proposed response |
|---|---|
| Valid log metadata with a damaged payload | Apply the metadata's mappings in order. Detect payload damage if the mapped version is read. |
| Payload checksum mismatch on an online read | Return no payload bytes and complete the read with `EIO`. Keep the volume online and leave mapping and cursors unchanged. |
| Recognizable log metadata with a bad checksum | Refuse startup and require operator action. Preserve the backing image unchanged. |
| Unusable checkpoint descriptor or body | Try another valid checkpoint candidate. Refuse startup if no candidate is usable. |
| Reported backing I/O error or short operation during recovery | Refuse startup and report the operation that failed. |
| Reported backing I/O error or short operation online | Fail the operation without publishing an incomplete operation; do not fail the volume. Safe continuation after partial writes and failed fsync remains an open implementation design. |
| Invalid request or finite-log exhaustion detected before I/O | Reject the operation without poisoning the volume or changing its mapping. |

Never skip a damaged mapping and expose an older value or zeroes. A later valid overwrite replaces the damaged version. The LBA is not permanently unusable. Filesystems and databases may nevertheless become unavailable after a failed read.

Recognizable metadata corruption prohibits automatic truncation, stale-tail clearing, checkpoint publication or fallback that conceals the error. Normal checkpoint-candidate fallback remains supported. Classify failures by cause, not errno alone: backing `ENOSPC` is not pre-I/O finite-log exhaustion.

The proposed online backing-error response differs from the current engine failure latch and the older ADR-05 test expectations. Keeping the volume online is a target, not a verified safe retry protocol. Partial record writes, incomplete checkpoint publication, failed fsync and unusable I/O resources need explicit continuation rules before implementation.

### Detection limits and repair

Severe corruption can erase every identifying field. A format-1 bounded scan that finds no valid or recognizable-corrupt next footer may stop without detecting a past durable write. Absence of a valid next footer does not prove that the tail was never written. A checksum chain also cannot prove that an entire suffix was not lost.

No local repair exists. A future offline salvage tool may remove a log tail after operator inspection. The operator must choose the cut point and accept possible durable-data loss. Startup never invokes salvage automatically. Salvage is not successful recovery.

Future replication may report corruption out of band without delaying a failed read. [Protocol-Aware Recovery for Consensus-Based Storage](Protocol-Aware-Recovery-for-Consensus-Based-Storage.pdf), Sections 3.3–3.5, requires the correct entry or snapshot version, not an arbitrary peer's value at the same LBA. Repair must not overwrite newer writes. Physical addresses, their bound checksums and local LSNs are not replicated version identities. The report and repair interfaces remain undefined.

This ADR remains the single proposed policy. A substantial later revision should contain the full replacement model and explicitly identify the superseded sections. Acceptance and implementation must remain separate from historical evidence.

## Proposal: checksum chaining instead of recovery-time zeroing

The historical format-1 recovery code zeroes and fsyncs up to 339 blocks at the recovered append position. That number comes from its maximum record size and footer-search window. Format 3 retains stale-tail clearing and its crash tests. Those tests do not establish a general protocol for segment reuse.

This checksum-chain proposal predates ADR-08. Each format ADR defines its own recovery and reuse
rules. We will test those rules first; this proposal does not replace ADR-08's protocol.

The proposed direction is to remove recovery-time zeroing and link records through checksums of preceding record metadata. The record checksum would bind the previous checksum as well as the current metadata, including stored payload checksums. Recovery would validate record linkage without rereading payloads solely to compute the chain.

This is not yet an encoding or a proof that zeroing can be deleted. The design must specify the checkpoint chain anchor, first-record rule, chain-mismatch classification and behavior when append positions are reused. Rewriting byte-identical records can reproduce the same checksum chain. Tests must determine whether a recovery-generation identifier is needed to prevent an abandoned suffix from attaching again.

Removing zero writes does not automatically remove the need for a recovery fsync. The design must specify when replayed history becomes durable and when `durable_lsn` may advance. Existing recovery also restores the selected checkpoint, opposite checkpoint slot, last footer and physical bytes since checkpoint. These counters must remain correct without the old clearing step.

The format version and field layout remain undecided. ADR-04 must use the resolved protocol rather than copy the format-1 clearing window. Until this proposal is accepted and tested, it does not change the existing code or its acceptance record.

## Fault plans and reference model

Each plan identifies the fault scope, trigger and action. Exact plans use an operation index. Seeded probability is a later trigger, not a substitute for a fault definition.

The storage vocabulary includes `Error`, `Delay`, `ShortOperation`, `Tear`, `Corrupt`, `Misdirect`, `LosePending` and `FailDevice`. Plans also need explicit stale-read and reordering cases. This is a conceptual vocabulary, not an implemented API.

`Tear { durable_blocks }` can describe only a surviving prefix. Exact tear plans must identify surviving block positions and bytes. For an 8 KiB format-1 payload/footer pair, the four cases are neither block, payload only, footer only and both blocks. Footer-only survival with valid metadata is not automatically a startup error: the payload check belongs to the read path.

The reference model records what the caller observed and which bytes reached storage. It must
distinguish four outcomes:

- A write or discard completed successfully.
- A flush completed successfully and made earlier completed operations durable.
- The target rejected an operation without changing storage.
- An operation may have changed storage, but its completion is uncertain.

A failed flush makes no new durability promise. Some pending writes may still survive.
Likewise, a write may reach storage before the caller receives its completion. The model must
retain unresolved operations until it can decide which recovered values are allowed.

Version numbers alone aren't enough. Consider three writes to one LBA:

1. Write v1 and flush successfully.
2. Reject v2 before issuing any I/O.
3. Write v3 successfully, then crash before the next flush.

Recovery may return v1 or v3, but never v2. A backing `ENOSPC` error does not by itself prove that
storage stayed unchanged. Treat the operation as uncertain unless the target contract guarantees no effect.

ADR-10's blockcheck compares the recovered value of each LBA with the values allowed by its history.
It does not check ordering across LBAs. A separate engine checker verifies record order, checkpoint
selection and replay. It must detect recovery that skips a required record but applies a later one.

The checker must use the selected format's atomicity rules: a write-buffer group that spans segments
need not survive as one unit.

### Independent logical and structural checks

We need two independent checks:

- The logical checker compares reads and errors with the reference model. It must not reuse the
  generator's expectations or the engine's mapping and recovery code.
- The structural checker reads the backing image and follows references from every checkpoint that
  recovery could select. It must not call engine recovery or trust the cleaner's counters.

For format 3, the structural checker follows descriptors, indexes, mapping pages and payloads.
For format 4, it also follows segment-table pages, direct and indirect index pages, mapping snapshots
and replay links from both append heads. Every segment needed by either checkpoint must remain
unavailable for allocation.

Run the checks after checkpoint publication and segment reuse, and before and after recovery.
Include images prepared by earlier writes, checkpoints and reuse. Save the image before open,
because recovery may write. If startup must reject an image without changing it, compare the
before and after images byte-for-byte.

Report damaged payloads separately from invalid metadata. This test-only scan does not add a payload
scan to production recovery. When a fault destroys data, the expected result may be a read or startup
error. Turning faults off cannot restore the lost bytes.

### How do we test the checker?

First run short, fixed operation sequences against correct targets and targets with deliberate bugs.
Each bug test must name the trigger, affected LBA and expected violation. It must also state whether
the checker should detect the bug before or after recovery.

The tests must detect lost durable data, data returned from the wrong LBA, discarded data returning
and a rejected write becoming visible. Engine tests must also detect skipped sync barriers and segment
reuse before it is safe.

Record how many times each injected fault occurred. A fault scheduled after the last operation must
have a count of zero and cause no violation. If a test permits repair, save the failure evidence first
and check the repaired state separately.

### Which crash states do we explore?

Start with short sequences of writes, overwrites, discards, flushes and reopens. Test a crash at each
backing write or sync boundary. Use both fresh images and images prepared by earlier writes,
checkpoints and reuse.

Each run must state its limits: sequence length, LBA count, backing geometry, surviving byte ranges
and thread schedules. A completed sweep covers only the states within those limits.

For power-loss tests, preserve every write covered by a successful sync. Vary which unsynced bytes
survive and their write order. State the assumed atomic write size; a 4 KiB logical block does not
imply an atomic physical write. Test later corruption and false sync acknowledgements separately,
with their own expected errors.

The ADR-08 evaluation also needs these fixed cases:

| Sequence | Required check |
|---|---|
| Relocate data, publish checkpoint A, then crash before checkpoint B | The older checkpoint still protects the source payload and metadata. |
| Publish both checkpoints, reuse the victim, then damage the newest descriptor | Any older checkpoint that recovery accepts must not need overwritten bytes. |
| Prepare a relocation proposal, overwrite or discard its LBA, then apply that stale proposal | Preserve the later client write or discard. A buffered write from before the discard must not restore data. |
| Reclaim a segment containing only metadata; switch segments during checkpoint publication | Preserve parent pointers, segment tables and replay links needed by either checkpoint. |
| Reuse a physical location with byte-identical replacement content | Recovery must not attach abandoned records merely because their checksums match. |
| Delay backing discard while allocation requests that segment | Keep the segment unavailable until discard completes. |
| Crash during backing growth and repeatedly during reopen | References into the new range require a durable growth record. Repeated recovery must remain correct. |

### Does the cleaner make progress?

Test progress separately from data safety. Turn faults off and provide a victim whose reclamation
would free space. Cover idle cleaning, a full proposal queue, reserve pressure and shutdown.
Control the order of cleaner/writer handoffs and bound the schedules tested. A storage seed alone
does not reproduce thread scheduling.

Record the scheduling assumptions and deadline for progress. Do not require progress when all space
is live or a required backing operation never completes.

### Generated sequences and regression tests

After the fixed cases pass, generate operation sequences. Reduce a failure by removing commands
and simplifying LBAs, values, geometry and fault positions. Save the smallest reproducer found as
a normal regression test, including its operations and faults.

Compare final logical contents across cache budgets, batch sizes and checkpoint frequency.
Account for extra flushes: they can change which writes must survive a crash.

### Persistent decoders

Extend the existing decoder tests with arbitrary bytes and modified valid encodings. Change semantic
fields and recompute checksums so tests reach validation beyond the checksum check. Cover record
kinds, reserved bits, pointer ranges, geometry and arithmetic limits.

Raw-only format 4 must reject reserved kind 6 and high mapping-address bits. Decoders must return
typed errors and finish within the test limits. This plan selects no fuzzing or formal-proof dependency.

## Test mechanisms

### Injectable backing I/O

The August V0.8 review found 11 `submit_exact` call sites in the format-1 engine. The released
v1.2.3 uses `rust/src/accounted_io.rs` and includes packed records and batched checkpoint writes.
Build the injectable I/O interface around the version being tested. Preserve its completion checks
and I/O accounting.

The historical format-1 sketch is:

```rust
#[repr(C, align(4096))]
pub struct Block(pub [u8; BLOCK_SIZE]);

pub trait BlockDevice {
    fn block_count(&self) -> u64;
    fn read(&mut self, first_block: u64, blocks: &mut [Block]) -> io::Result<()>;
    fn write(&mut self, first_block: u64, blocks: &[Block]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}
```

`UringDevice` would own the ring, native descriptor, exact-completion checks and buffer-lifetime rules. `Volume` would hold `Box<dyn BlockDevice>`. Dispatch overhead is unmeasured; this choice avoids propagating a device type parameter through the frontend.

Existing `open`, `open_with_options` and `format` would keep their path-based entry points. Injectable `open_on` and `format_on` would accept a device; `format_on` would also accept an explicit volume ID. No random-source trait is needed for one integer.

This interface is a draft. The format-1 WRITEV used two contiguous halves of one 8 KiB buffer.
The final interface must preserve format-3 batching and the evaluated format-4 discard and growth
operations. Test native submission and completion errors separately. Simulated block I/O cannot
verify ring cleanup or buffer lifetime.

### Four test tiers

These are proposed integrations. The tool capabilities measured during the spike are recorded in the [appendix](#appendix-tool-spike-recorded-2026-08-30).

| Tier | Mechanism | Boundary and limit |
|---|---|---|
| 0 | `MemoryDevice`, pending and durable state, exact fault plans | Fast model checks under `make test`; does not prove kernel behavior. |
| 1 | `CrashDevice` wrapping `UringDevice` | Real `io_uring`, `O_DIRECT` and child `SIGKILL`; does not force loss of kernel or device caches. |
| 2 | `qemu-storage-daemon`, `file` → `blkdebug` → `raw`, FUSE export | Unprivileged backing-error injection; ublk access still needs operator setup. Proposed opt-in `make test-fault`. |
| 3 | `dm-flakey` and `dm-log-writes` over a disposable loop-backed file | Privileged medium tests through ublk and `fio`; explicit operator procedure. |

Tier 0 is needed for exact short operations, tears and misdirected writes. Tier 2 provides errors and delay, not exact tears. Tier 3 adds dropped writes, corruption and flush-order capture. Process-crash testing alone is not a `LosePending` injector.

`CrashDevice` would terminate its test-created process after a selected completed operation. An operation sweep replaces named failpoints after equivalent coverage is proved. The existing engine publishes mappings between `record-write-complete` and `mapping-published` without intervening I/O. Those two failpoints have identical persistent state in the current serialized engine. Their tests may be consolidated with new evidence, not silently removed from the accepted baseline.

Native completion tests, direct-I/O alignment, real filesystems, ublk decoding, `fio` verification and process restart remain necessary. Simulation supplements them. Future power-cycle evidence is separate again.

## Proposed deliveries

These deliveries are not an active Ralph queue. Acceptance of this ADR and resolution of its open decisions come first. ADR-06 proposes filesystem and database correctness before format and medium-fault work. Reconcile that order and the selected format before assigning new milestone numbers.

### Delivery 1: injectable backing I/O and process crashes

Extract `Block`, `BlockDevice` and `UringDevice` without changing failure semantics. Add `open_on`, `format_on` and `CrashDevice`. Replace named failpoints only after the operation sweep covers the same persistence boundaries. Keep this extraction separate from the policy changes.

- [ ] Preserve native short/unexpected/extra completion and completion-flag checks, buffer lifetime and I/O-resource cleanup.
- [ ] Pass every existing test affected by extraction, including the required process-crash repetitions and live ublk acceptance. Verify child termination is `SIGKILL`.
- [ ] Produce byte-identical formatted images for a fixed volume ID. Remove failpoint code and environment plumbing after replacement coverage passes.
- [ ] Pass `make lint test` and record fresh evidence for the affected acceptance gates.

### Delivery 2: exact fault plans and failure handling

Add `MemoryDevice`, the reference model and the read-only structural checker. Pass the fixed
checker tests before generating operation sequences. Keep any failure-policy or tail-protocol change
separate from the test harness. The following cases must exercise engine behavior, not just error
classification helpers.

| Test group | Required evidence |
|---|---|
| Payload damage | Recovery from valid metadata succeeds. Damaged latest versions return no bytes; healthy LBAs work. Later valid overwrites replace damage. Check checkpoint-only and replay-tail mappings. |
| Recovery accounting | Preserve exact mappings, LSNs, append state, checkpoint selection and physical-byte budget despite payload damage. Retain format-1 coverage for all 1–338 payload counts where format 1 remains supported. |
| Metadata damage | Distinguish ordinary candidate bytes from recognizable corrupt metadata. Prove byte-for-byte image preservation on the latter, including a valid-looking later record. Preserve checkpoint fallback. |
| Tear outcomes | Exercise all four payload/footer survival cases, reported short I/O and corruption of identifying fields. Label cases beyond the detection or durability assumptions. |
| Tail reuse | Recover, append, flush, crash and recover again. Try different and byte-identical replacement records, abandoned suffixes beyond the old clearing window and interrupted checkpoint publication. Validate the selected chain protocol. |
| Online backing errors | Inject errors and short operations at reads, writes, checkpoints and fsync. Verify failed-operation reporting, no incomplete publication and the accepted safe-continuation rules. Distinguish backing `ENOSPC` from log exhaustion. |
| Bounds and progress | Reject out-of-geometry I/O. Bound waits and child lifetimes. Complete fetched ublk requests with a result rather than hanging. |

- [ ] Match model outcomes for every exact plan, including expected read and startup errors. Preserve flush-covered history when the plan honors the durability assumption.
- [ ] Run the bounded operation sequences for each selected format and its supported operations.
  For the ADR-08 candidate, also run the fixed reclamation cases. Record which state transitions,
  faults and outcomes were checked, and which faults occurred.
- [ ] Prove the accepted tail protocol before removing recovery-time zeroing. Update only the acceptance criteria explicitly superseded by that change.
- [ ] Prove the online continuation protocol before removing the current volume failure latch for backing errors.
- [ ] Pass `make lint test` within its bounded developer budget and rerun affected acceptance gates.

### Delivery 3: real-kernel backing errors

Use Tier 2 through ublk and `fio`. The spike found that daemon `format` rejects an existing FUSE path with `File exists`. Format the owned regular file first, then expose it through the fault node.

- [ ] Prove that each fault rule actually fires through `file` → `blkdebug` → `raw`. Use `once` or explicit state rules, not unconditional rules.
- [ ] Exercise `EIO` and backing `ENOSPC` during normal I/O, checkpoint publication and recovery. Check the accepted response policy, including failed fsync and subsequent requests.
- [ ] Bound child and suite lifetimes. Clean only owned resources, and preserve evidence when identity or cleanup state is ambiguous.
- [ ] Make the opt-in gate report missing prerequisites without creating resources. Record source identity, kernel, tool versions, geometry, commands, timings and outcomes.

### Delivery 4: operator-run medium faults

Use Tier 3 under ADR-03's operator-safety rules. Do not run privileged tests from an unattended worker. Never target a system device or an existing user filesystem.

- [ ] Validate owned device identities before destructive operations and clean the disposable loop/device-mapper stack after success.
- [ ] Test dropped writes and corruption of payloads, record metadata, checkpoint descriptors and bodies. Distinguish durability-assumption violations from recovery bugs.
- [ ] Capture backing writes and flush/FUA boundaries with `dm-log-writes`. Replay selected outcomes
  into owned fresh images and check recovery and exact contents with the independent checker.
- [ ] Run selected faults through the accepted ADR-06 filesystem and database workflows.
  Define safe device re-exposure, offline checks and stale-mount handling before those runs.

Capture I/O at the engine's backing layer. Client ublk writes don't show the order of internal
checkpoint writes. Report backing-file filesystem effects separately from disposable raw or loop
backing. Replay tests selected crash states; it is not a real power-loss test.

For filesystem and database runs, save the image before recovery. Keep expected transaction results
outside the tested device. Verify exact durable contents as well as filesystem and database structure.
The database settings determine which commits may be lost. SQLite WAL/NORMAL and WAL/FULL have
different power-loss guarantees. File-replacement cases must sync both the file and the required directories.

## Evidence and consequences

The September policy and chain proposals have no implementation acceptance evidence. August tool measurements show available mechanisms, not a working fault-resilient device.

Changing the failpoint mechanism requires fresh evidence for affected ADR-03 gates. Preserve existing PASS logs with their original source identity. The recorded engine-crash run includes a 608.100-second all-boundary stale-tail case. That measures the old test, not a reason to retain its recovery mechanism indefinitely.

The simulator can cover faults that the real-kernel tools cannot reproduce exactly. Those results prove the model and engine interaction, not the behavior of every storage device. Where tiers overlap, compare the same scenario and expected outcome. Keep claimed coverage narrower than the fault vocabulary until tests exist.

Each result must record:

- the commit, uncommitted changes, binary identity, format, kernel and backing layers;
- the model limits and exact operation sequence;
- the faults that occurred and the expected outcomes;
- the original failure image, observations before repair and reduced reproducer, if the run failed.

Repair or later convergence cannot turn a failed run into a pass. Keep small fixed regressions in
`make test`. Run larger sweeps with explicit acceptance limits.

The injected interface adds dispatch and maintenance cost. Its performance impact remains unmeasured. Its justification is the concrete need for a controllable backing device, not the future cluster simulator.

## Later deterministic simulation

This section incorporates the former simulation TMD. It remains an aspiration beyond the exact-plan deliveries.

The immediate sequence is storage-only: injectable I/O, pending/durable state, exact plans and independent checks. Add seeded generation only when those scenarios and invariants are stable. Do not add a virtual scheduler or network model to the current single-node engine.

A later multi-node simulator would run production state machines with virtual time, a seeded scheduler, storage, directed network links and node lifecycle controls. Order events by simulated time, priority and a monotonic tie-break sequence. Record the root seed, derived subsystem seeds, complete configuration, operation history, injected faults and result.

Network rules would identify source, destination, optional message kind, trigger and action. Actions include drop, delay, duplication and partition. Per-rule random streams avoid changing every unrelated fault when one rule is added. Add network faults only after a replicated protocol and stable message interfaces exist.

A simulation run has a safety phase with continuous checks and a liveness phase with faults disabled and a healthy topology. Future replicated checks include agreement, quorum durability, monotonic commit indexes and convergence. The local storage checker still follows this ADR's policy; a healthy topology cannot restore destroyed bytes without repair.

Seed replay and trace reduction are separate. A seed reproduces a run only while code, configuration and random-draw order remain compatible. A future reducer can minimize an explicit failing trace. Neither requires a generic chaos language or a new runtime dependency now.

### Design references

These are research references, not selected dependencies. The GEFS source was read for this revision; the other references are retained from earlier research. None establishes compatibility with this engine.

| System | Useful direction and sources |
|---|---|
| GEFS | [fuzz.c](https://github.com/9front/9front/blob/f6162bbc98668e12d131233b0169018bf623d923/sys/src/cmd/gefs/fuzz.c) compares lookups and scans with a shadow AVL tree and writes a seed/trace on failure. [check.c](https://github.com/9front/9front/blob/f6162bbc98668e12d131233b0169018bf623d923/sys/src/cmd/gefs/check.c) separately checks tree, free-list and dead-list relationships. Borrow the separation of logical and structural checks; these sources do not prove our crash or reuse protocol. Read on 2026-10-06. |
| TigerBeetle VOPR | Explicit simulation loop, subsystem seeds, continuous checks and a separate liveness phase. [Documentation](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md), [VOPR](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/vopr.zig), [packets](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/packet_simulator.zig), [storage](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/storage.zig), [cluster](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/cluster.zig). |
| FoundationDB | Deterministic priorities, virtual time, process hierarchy and valid fault combinations. [Overview](https://apple.github.io/foundationdb/testing.html), [simulator](https://github.com/apple/foundationdb/blob/main/fdbrpc/sim2.cpp), [task queue](https://github.com/apple/foundationdb/blob/main/flow/include/flow/TaskQueue.h), [non-durable file](https://github.com/apple/foundationdb/blob/main/fdbrpc/include/fdbrpc/AsyncFileNonDurable.h). |
| etcd, TiKV and CockroachDB | Directed links, composable message filters, explicit histories and an independent checker. [etcd network](https://github.com/etcd-io/raft/blob/main/rafttest/network.go), [TiKV transport](https://github.com/tikv/tikv/blob/master/components/test_raftstore/src/transport_simulate.rs), [kvnemesis](https://github.com/cockroachdb/cockroach/tree/master/pkg/kv/kvnemesis). |
| Turmoil and MadSim | Seeded host scheduling and replaceable runtime services. [Turmoil](https://github.com/tokio-rs/turmoil), [MadSim](https://github.com/madsim-rs/madsim), [RisingWave tests](https://github.com/risingwavelabs/risingwave/tree/main/src/tests/simulation). |

The earlier research did not establish a drop-in simulator for this synchronous native-descriptor engine. It recorded missing durable-crash modeling in MadSim and completion-model differences in Turmoil. Recheck those capabilities before dependency selection. WRITEV itself is not a fundamental obstacle: the current two segments are contiguous.

## Appendix: tool spike recorded 2026-08-30

These are historical measurements from Linux `7.0.0-29-generic` x86_64, `qemu-img` 8.2.2 and `fio` 3.36, run as unprivileged user `miguel`. They were not repeated during the September document consolidation. Recheck capabilities and permissions before implementation.

### Available tools at the spike

| Tool | Recorded state |
|---|---|
| `dm-flakey`, `dm-log-writes`, `dm-delay` | Modules present; privileged setup. Userspace `replay-log` absent. |
| `dm-dust` | Not built: `# CONFIG_DM_DUST is not set` in all 24 inspected kernel configs. |
| `dm-error` | Device-mapper core enabled by `CONFIG_BLK_DEV_DM=y`; no separate module needed (mechanism, not an injected test). |
| `scsi_debug` | Module present; `every_nth`, `medium_error_start`, `opts` timeout and DIF/DIX options. |
| `null_blk` | Module present; this build's `modinfo` exposed no bad-block parameter. |
| Block-layer fault injection | `CONFIG_FAULT_INJECTION` absent, so `fail_make_request` unavailable. |
| `qemu-storage-daemon` | 8.2.2, with `blkdebug`, `blklogwrites`, `blkverify` and FUSE export. |
| `fio` | 3.36; no fault-injection engine in its engine list. |
| `nbdkit`, `blktrace`, `replay-log` | Not installed. |

### Permissions at the spike

| Control node | Owner and mode | Unprivileged open |
|---|---|---|
| `/dev/mapper/control` | `root:root 0600` | `EACCES` |
| `/dev/ublk-control` | `root:root 0600` | `EACCES` |
| `/dev/loop-control` | `root:disk 0660` | `EACCES`; user was not in `disk`. |
| `/dev/fuse` | `root:root 0666` | Succeeded. |

Device-mapper still needs `CAP_SYS_ADMIN`, regardless of node permissions. At the spike, `kernel.unprivileged_userns_clone` was 1, but AppArmor's `apparmor_restrict_unprivileged_userns` was also 1. Writing `/proc/self/uid_map` after `unshare` failed with `EPERM`.

These observations preceded the normal-user ublk acceptance recorded in ADR-03. The operator-installed owner helper and udev setup changed ublk access. The table is not a statement of current host permissions.

### Measured QEMU behavior

The working stack was `file`, then `blkdebug`, then `raw`, with `raw` exported through FUSE. Exporting `blkdebug` directly produced no injected errors because no format driver generated the required events.

| Experiment | Recorded result |
|---|---|
| Unconditional write `EIO` | `pwrite`, `fsync` and `close` failed; 0 bytes reached the base image. |
| Fail the third write once | `ok, ok, EIO, ok, ok`; fsync and a second pass succeeded. |
| `EIO` on `flush_to_disk` only | Writes succeeded; fsync failed. |
| `errno = 28` | Caller received `ENOSPC`. |
| `io_uring` with `direct=1` | 1024 writes and 1024 reads completed on a clean export. A write-error rule produced `io_u error ... write offset=0`. |
| Engine-shaped access | `O_DIRECT | O_CLOEXEC`, exact `lseek(SEEK_END)` size and `flock(LOCK_EX | LOCK_NB)` worked. A second lock holder was refused. |

`blkdebug` provides errors and latency, not short completions or exact tears. Unconditional rules also affected later operations of the other type: reads failed after write rules and writes failed after read rules. Single-shot `once = "on"` rules did not show that behavior. The cause remains unexplained; use single-shot or explicit state rules.

### Why an in-memory device is still needed

The inspected tools did not provide exact short operations, arbitrary surviving blocks of a torn record or misdirected writes. `fio` verifies data rather than injecting these faults. `MemoryDevice` provides those exact plans; real-kernel tiers check overlapping assumptions independently.

`dm-flakey` uses timed up/down intervals rather than an exact operation index. Its `drop_writes` models acknowledged writes that disappear. `corrupt_bio_byte`, `random_read_corrupt` and `random_write_corrupt` alter bytes below the engine checksum.

`dm-log-writes` captures writes and flush ordering for later replay. Capture/replay is a model of selected crash outcomes, not proof of every possible power-loss image. QEMU `blklogwrites` may supply a compatible log without root, but format compatibility was not verified. `dm-error` adds little over the selected `dm-flakey` error modes; `dm-dust` was unavailable without a different kernel.

## Open decisions before implementation

- Select the tested format and its existing recovery and reuse rules. Define the atomic-write size
  and limits for crash exploration. Revisit the older checksum-chain proposal only if a protocol
  change is needed; it must not block ADR-08 evaluation.
- Define safe online continuation after a partial write, checkpoint error, failed fsync, timeout or device disappearance. Keep the desired operation-level response distinct from an unproved retry mechanism.
- Reconcile ADR-06's proposed delivery order, the selected persistent format and the injectable interface. Approve a new implementation prompt only after that scope is settled.
- Choose shared versus tier-specific fault plans. Verify `blklogwrites` compatibility and explain the unconditional `blkdebug` behavior before relying on either. Seeded and multi-node exploration remain later work.
