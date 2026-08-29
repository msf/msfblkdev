# TMD: Deterministic fault testing and simulation

Date: 2026-08-29  
Status: aspiration, not an active implementation specification  
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md)

## Why do we want this?

ADR-03 tests exact process-crash boundaries against real files, `io_uring`, ublk and `fio`. These tests prove important Linux behavior. They don't explore combinations of operations, timing and faults under one reproducible execution model.

The longer-term goal is a deterministic simulator that owns the environment in which the system runs. It constructs every node with injected implementations of storage, networking, time, randomness and process lifecycle. A seed and a materialized configuration must reproduce the same event order and result.

This is an aspiration for later storage and distributed-system goals. It does not expand ADR-03. For now, we need a smaller Rust lab application that runs existing crash and ublk tests safely and records evidence.

## What does the final shape look like?

The simulator runs production state-machine logic in one process. It replaces nondeterministic substrates with deterministic implementations:

- a virtual clock;
- a seeded event scheduler;
- a seeded random source;
- storage with separate pending and durable state;
- a network with explicit directed links;
- node start, pause, crash and restart controls;
- invariant and model checkers.

The scheduler orders events by simulated time, priority and a monotonic sequence number. The sequence number gives equal-time events a stable order. The simulator records the root seed, derived subsystem seeds, complete configuration, operations, injected faults and final result.

A run has two phases:

1. The safety phase generates operations and faults while checking invariants after each relevant transition.
2. The liveness phase stops injecting faults, restores a known healthy topology and requires the system to converge.

A temporary partition can prevent progress without violating safety. Keeping these phases separate avoids treating every expected loss of liveness as data corruption.

## How should faults be described?

Faults need explicit scope, trigger and action. Random incidence is one trigger, not the fault model itself.

A future directed network rule can have this conceptual shape:

```rust
struct Link {
    source: NodeId,
    destination: NodeId,
}

struct NetworkFaultRule {
    link: Link,
    message: Option<MessageKind>,
    action: NetworkFault,
    incidence: Probability,
}

enum NetworkFault {
    Drop,
    Delay(SimDuration),
    Duplicate,
    Partition,
}
```

A delay can reorder messages when later messages receive shorter delays. Rules can also target one message type or one direction of a link. The simulator derives a stable random stream for each rule so that unrelated rule additions don't silently change every existing draw.

Storage needs a separate vocabulary:

```rust
enum StorageFault {
    Error,
    Delay(SimDuration),
    ShortOperation,
    Tear { durable_blocks: u32 },
    Corrupt,
    Misdirect,
    LosePending,
    FailDevice,
}
```

The storage model distinguishes three facts:

- an operation completed from the caller's perspective;
- bytes exist in volatile or pending state;
- bytes are durable and survive a crash.

This distinction lets the simulator model acknowledged writes, `fsync`, torn records, interrupted checkpoints and stale-tail recovery without relying on wall-clock races.

## What do we check?

Seeded execution is useful only when the simulator checks a model or invariant. Initial storage invariants include:

- an acknowledged durable prefix survives a crash;
- recovery never exposes a footer without its complete record;
- local sequence numbers and previous-footer links remain contiguous;
- a checkpoint maps each logical block to the expected payload and checksum;
- an invalid tail cannot resurrect a valid-looking later record;
- recovery never reads or writes beyond the backing geometry;
- a healed run reaches the same logical block contents as the reference model.

Future replicated-system checks can add agreement, quorum durability, monotonic commit indexes and replica convergence.

## What do we build now?

ADR-03 needs a much smaller slice:

1. `block-storage-lab` remains a typed Rust application for process ownership, deadlines, evidence and scenario selection.
2. The ublk and `fio` workflow moves from Bash into that application.
3. Existing named failpoints and child `SIGKILL` tests remain the deterministic crash mechanism.
4. The real-file and real-kernel tests remain authoritative for Linux integration.

We do not build a virtual scheduler, a network model or an abstract cluster for ADR-03. There is only one storage node and no network protocol to simulate.

The first actual simulator slice belongs with backing-medium fault resilience after ADR-03. It should be storage-only:

1. Put backing I/O behind the narrowest interface that the engine needs.
2. Add an in-memory implementation with pending and durable block state.
3. Run exact fault plans before adding random exploration.
4. Compare every operation and recovered state with a small reference model.
5. Add seeded generation only after deterministic scenarios and invariants are stable.

This sequence keeps the current implementation small while preserving the correct architectural direction.

## What remains real?

The deterministic simulator does not replace:

- native `io_uring` completion tests;
- `O_DIRECT` alignment and filesystem behavior;
- process-level `SIGKILL` and restart tests;
- ublk request handling;
- direct 4 KiB `fio` verification;
- future dedicated-device or power-cycle evidence.

The simulator explores state space quickly and reproducibly. The integration lab proves that the adapters and operating system preserve the assumptions used by the simulator.

## What did we learn from existing systems?

### TigerBeetle VOPR

TigerBeetle's Viewstamped Operation Replicator (VOPR) simulator owns time, packet delivery, storage and replica lifecycle. Its cluster tick drains network and storage work before advancing time. It checks state and storage invariants continuously, then runs a healthy-core liveness phase.

Useful sources:

- [VOPR documentation](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md)
- [`src/vopr.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/vopr.zig)
- [`src/testing/packet_simulator.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/packet_simulator.zig)
- [`src/testing/storage.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/storage.zig)
- [`src/testing/cluster.zig`](https://github.com/tigerbeetle/tigerbeetle/blob/main/src/testing/cluster.zig)

We should borrow its explicit simulation loop, subsystem seeds, storage fault atlas, continuous checkers and separate liveness phase.

### FoundationDB

FoundationDB constructs simulated machines and processes inside its Flow runtime. A deterministic priority queue controls task and timer order. The simulator injects process, machine, network and disk failures while consistency workloads and storage audits run.

Useful sources:

- [Simulation testing overview](https://apple.github.io/foundationdb/testing.html)
- [`fdbrpc/sim2.cpp`](https://github.com/apple/foundationdb/blob/main/fdbrpc/sim2.cpp)
- [`flow/include/flow/TaskQueue.h`](https://github.com/apple/foundationdb/blob/main/flow/include/flow/TaskQueue.h)
- [`fdbrpc/include/fdbrpc/AsyncFileNonDurable.h`](https://github.com/apple/foundationdb/blob/main/fdbrpc/include/fdbrpc/AsyncFileNonDurable.h)

We should borrow deterministic event priorities, explicit virtual time, injected process hierarchy and policy checks that reject invalid fault combinations.

### etcd, TiKV and CockroachDB

etcd's raft test network keys loss and delay by the directed `(source, destination)` connection. TiKV wraps its in-process Raft transport with message filters for drops, delays and partitions. CockroachDB's `kvnemesis` separates generated operations and faults from its history validator, and records the seed used by each run.

Useful sources:

- [etcd `rafttest/network.go`](https://github.com/etcd-io/raft/blob/main/rafttest/network.go)
- [TiKV `transport_simulate.rs`](https://github.com/tikv/tikv/blob/master/components/test_raftstore/src/transport_simulate.rs)
- [CockroachDB `kvnemesis`](https://github.com/cockroachdb/cockroach/tree/master/pkg/kv/kvnemesis)

We should borrow directed link keys, composable transport rules, explicit operation histories and a checker that is independent from the generator.

### Turmoil and MadSim

Turmoil provides seeded host scheduling, virtual time, node lifecycle and per-link controls. Its filesystem separates pending and durable operations and models crash loss, torn writes and `O_DIRECT` constraints. MadSim demonstrates how a large Rust system can replace selected Tokio services and start many nodes in one deterministic runtime.

Useful sources:

- [Turmoil](https://github.com/tokio-rs/turmoil)
- [MadSim](https://github.com/madsim-rs/madsim)
- [RisingWave simulation tests](https://github.com/risingwavelabs/risingwave/tree/main/src/tests/simulation)

Neither is a drop-in backend for this engine. The current engine is synchronous, uses native file descriptors and depends on `Writev`. MadSim's filesystem does not model durable crash behavior, while Turmoil's simulated `io_uring` has a different completion model and does not support this `Writev` path. We should use both as design references before considering a dependency.

## What are we deliberately not doing?

For now, we will not:

- add a generic chaos domain-specific language;
- implement network faults before a replicated protocol exists;
- replace native acceptance tests with simulation;
- add MadSim or Turmoil as a dependency;
- claim that replay by seed is a general test-case shrinker;
- refactor production I/O solely to satisfy a speculative simulator API.

Replay and reduction are separate. A seed reproduces a run while code, configuration and random-draw order remain stable. A future reducer can minimize an explicit failing trace by removing operations, faults, nodes or time ranges.

## When do we revisit this?

Revisit the storage simulator boundary when ADR-03 is complete and ADR-01 starts backing-medium fault resilience. Revisit the network and multi-node model only when the project has a real replicated protocol and stable message interfaces.
