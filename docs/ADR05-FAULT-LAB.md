# ADR-05: portable backing-fault lab

Authored by LLM. Status: the [guest injector probe](../tools/adr05-fault-lab/README.md#run-the-injector-probe) passed on macOS arm64 with Lima/VZ and a native Debian 13 guest. The [live ublk checkpoint-B gate](../tools/adr05-fault-lab/live/README.md) also passed in this configuration. See the [campaign plan](ADR05-CAMPAIGN.md) and [pre-fix triage](ADR05-TRIAGE-01.md) for outstanding work.

Can format 4 recover flushed data if the backing store returns `EIO` while the engine publishes a checkpoint? Use the same Linux guest test on every host. Put the fault **below the engine**: failing a client write above ublk does not exercise checkpoint publication. Start with exact, one-shot failures. Keep the released failed-volume behavior for this test; changing how the engine continues after errors is a separate [ADR-05 decision](../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#open-decisions-before-implementation).

## Which machine owns what?

| Role | Required capabilities | What crosses the boundary |
|---|---|---|
| Host: Linux or macOS, arm64 or amd64 | Boot a disposable VM, attach two file-backed virtual disks, copy files, run a guest command with a deadline, collect evidence and stop the VM. No host ublk, FUSE, Rust, raw disk access or kernel changes. | Source or build bundle in; evidence out. Do not expose writable host directories or pass through physical devices. |
| Build OS: Linux | Simplest path: build as a normal user inside the guest before the test. An external Linux builder or build container is optional. Use the repository lockfile, a recorded Rust toolchain and native build dependencies. | Supply Linux binaries for the **guest** ISA and userspace ABI, with matching runtime libraries. Record source revision, local changes, target triple, features and binary hashes. A Darwin binary or an amd64 binary translated on arm64 is not a native arm64 test. |
| Test-lab OS: one disposable Linux VM | Pin a Debian 13 image by digest per ISA, kernel release/config and QEMU storage tools. Require `io_uring`, ublk with normal-user device access, FUSE and a guest filesystem supporting aligned `O_DIRECT`. Provision modules and permissions once as guest administrator; run the daemon, injector and tests as a normal user. | The guest owns the fault plan, device identities, expected contents and verdict. Test code must not call a host-specific VM tool. |

Use a native guest by default. The [dependency inventory and setup instructions](../tools/adr05-fault-lab/README.md) use public tools and do not require `vmlab`. The following are launcher choices, not a tested compatibility matrix:

| Host | Guest | Example VM provider |
|---|---|---|
| Linux arm64 | Linux arm64 (`aarch64`) | QEMU/KVM |
| Linux amd64 | Linux amd64 (`x86_64`) | QEMU/KVM |
| macOS arm64 | Linux arm64 | Lima with Virtualization.framework; an existing `vmlab` clone can supply this role |
| macOS amd64 (Intel) | Linux amd64 | QEMU with Hypervisor.framework (HVF) |

To qualify the other ISA, use a native builder and VM of that ISA, local or remote. QEMU software emulation is an optional functional check: label it as emulated and don't compare its timings to native runs. A build container can package dependencies; it cannot select a different test kernel. No nested VM is required. Add one only for a separate experiment that must kill the guest kernel while an outer **Linux** injector continues recording. Killing a VM does not establish physical power-loss behavior.

## Where does the failure occur?

```text
Guest OS disk: runner, expected client history, fault plan and trace
Guest test disk: disposable filesystem containing one private format-4 image

raw client → /dev/ublkbN → format-4 daemon → io_uring → FUSE export
           → qemu-storage-daemon: raw → blkdebug → file → format-4 image
```

The arrows show request direction. Configure the QEMU graph from the file up and export the **raw** node. `qemu-storage-daemon` is a guest process, not another hypervisor. Format the image before exporting it; the daemon formatter expects to create its own path. Keep flushes enabled through every layer. Use the OS disk for observations so a failure on the test disk cannot erase the expected results. The [ADR-05 tool spike](../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#measured-qemu-behavior) demonstrated single-shot write and flush errors on Linux. The [guest trace](../evidence/adr05-probe-arm64-20261119/run.txt) verifies the path from the FUSE caller to a regular file on the scratch disk. Neither run exercised checkpoint publication.

## First engine results (Debian arm64, 2026-11-19)

- **Reported write error:** After a successful client flush, `blkdebug` rejected the blue checkpoint descriptor write during `Volume::close`. The engine returned `EIO`. The clean control closed successfully. Reopening a copy of each image recovered the expected bytes at all eight LBAs. This [engine trace](../evidence/adr05-engine-arm64-20261119/run.txt) covers an ordinary close, not the checkpoint that releases a segment.
- **Modeled final sync failure:** At the checkpoint that can release segment 0, keep or discard only the pending 4 KiB descriptor. With the old descriptor, recovery selected A and kept segment 0 `PendingFree`. With the new descriptor, it selected B and marked segment 0 `Free`. All eight LBAs matched in both cases. A separate control overwrote a still-referenced payload and the read failed its checksum. The [test transcript](../evidence/adr05-dst-arm64-20261119/run.txt) covers two whole-descriptor outcomes; it doesn't inject an `fsync` error or simulate torn sectors.

## What tooling do we build first?

1. **Guest preflight and injector probe.** Keep provider-specific code limited to VM creation, transfer, execution and teardown. Have one guest probe validate the kernel interfaces, normal-user permissions, scratch disk identity and a backing-write budget before creating a fixture. Through the FUSE export, run an aligned write, flush and read-back with faults off. On fresh fixtures, return `EIO` once on a write and once on a flush. Record which operation fired and what the caller observed. An absent capability or a rule that never fires is incomplete coverage, not a pass. Qualify this same probe on each host/guest row before claiming support.
2. **Two checkpoint cases through live ublk.** Reuse the [small reclamation fixture](../rust/src/tests/reclaim.rs): checkpoint A marks a segment pending; checkpoint B can release it. Flush known client contents first. Add a test-only handshake immediately before B's descriptor write or its final `fsync`. Pause there, arm the one-shot backing rule, then resume. Correlate the backing event with that operation; a flush count from daemon startup isn't sufficient. Test the two faults separately. Require one hit, a bounded error response and no allocation or hole punch of the victim on the failed volume. Keep the existing [process-kill cases](../rust/src/tests/reclaim_crash.rs) as a separate gate.
3. **Independent recovery checks.** Stop the writers and copy the image **before** reopen, because recovery can write. Verify recovered contents against the external client history. Independently check that neither selectable checkpoint, including its replay path, needs bytes already reused. Include a known-bad early-reuse fixture to show the check detects that error. A failed `fsync` makes no new durability promise: use a small deterministic storage model to exercise both a surviving descriptor and an absent descriptor, while preserving all writes covered by successful syncs. `blkdebug` tests error propagation; it does not choose exactly which unsynced bytes survive.
4. **Repeat and retain evidence.** Export the host/provider, guest ISA, image/kernel/tool identities, disk geometry, exact operation trace, activated fault, client result and recovery verdict per case. Bound execution and copy evidence before teardown; retain failed images. Only after the fixed cases catch their negative controls, add seeded operation sequences with saved traces and reduction. Random chaos, a whole-system simulator and device-mapper tests are not prerequisites for this milestone.

The [live engine trace](../evidence/adr05-live-b-20261119/run.txt) records three checkpoint-B runs. The same foreground ublk write triggered B each time. The control released six segments. On either descriptor-write or final-sync `EIO`, the client received an error, no victim was freed or discarded, and an independent copy recovered all eight LBAs. A separate [record-cut test](../tools/adr05-fault-lab/model-engine/README.md) covers surviving payload and footer blocks; this does not measure physical power loss. Linux-host and amd64 qualification are still open. `make test-acceptance` remains the process-crash baseline.
