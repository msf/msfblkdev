# Distributed reliable block storage

Date: 2026-08-27
Last reviewed: 2026-08-29
Author: Miguel Filipe
Status: exploratory
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-04](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md)

A storage node can hold tens of thousands of volume replicas. Losing one dense node therefore creates a fleet-wide repair event, not a local disk-replacement problem.

This note decomposes a distributed block-storage system for Railway into state-owning domains. It gives more detail to distributed coordination than to the local storage peer described by ADR-01 and ADR-04. It is a non-authoritative future design note, not an implementation plan and not a change to the local-device roadmap.

Use **domain boundaries for ownership** and **workflows for overlap**.

## What scale must the design handle?

These are design targets, not measured limits:

- Hundreds of thousands to millions of volumes.
- 100 to 3,000 storage nodes.
- Approximately 5 to 50 NVMe devices per storage node.
- Approximately 1,000 to 100,000 NVMe devices across the fleet as it grows by up to 100 times.
- Approximately 1,000 volume replicas per NVMe device.
- Expected logical volume sizes between roughly 1 GB and 8 TB.
- Parallel repair of the replicas lost with one NVMe device or storage node.
- Thin provisioning with a configured upper overcommit ratio such as 1:1, 2:1, or 3:1. A 1:1 policy does not overcommit capacity.

The principal failure modes are:

- **Repair storms:** one dense node failure can remove many replicas and require a large transfer volume.
- **Consensus cardinality:** millions of replication groups make one process per group impractical. Each storage process must multiplex many Raft groups.
- **Capacity exhaustion:** overcommitment can produce `ENOSPC` unless admission, reservation, and repair capacity remain consistent.
- **Independent growth:** logical volume size, committed capacity, allocated extents, and used bytes change at different times.

Mean time to recovery (MTTR) depends on bytes and available repair bandwidth, not only on replica count.

## Who owns which state?

### Service and user resources

This domain owns:

- accounts, projects, volume identity, names, quotas, and desired lifecycle state;
- user operations such as create, grow, attach, detach, wipe, and delete;
- the user-visible durability and capacity contract.

It does not know about logical volumes, write-ahead logs, or replica placement.

### Compute-node presentation

This domain:

- creates and removes local block devices through `ublk`;
- routes block I/O to the volume's replication group;
- holds the runtime attachment configuration and renews the attachment lease;
- makes the device available to the target container.

A volume is attached only while the replication group recognizes the current fencing epoch and the compute node exposes the corresponding `ublk` device. The compute frontend does not select replicas or own durable volume state. It can reconstruct its state from the replication group and control plane.

### Storage-node resources

This domain owns actual local resource state:

- NVMe discovery, identity, health, endpoints, and fault-domain labels;
- volume group (VG) registration;
- logical volume (LV) reservation, creation, extension, and deletion;
- startup and supervision of the per-NVMe Multi-Raft and storage process;
- local capacity and health reports.

It does not make fleet-wide placement decisions.

### Local replica storage engine

This domain owns one replica's physical representation:

- the logical block address (LBA) to payload mapping;
- the write-ahead log (WAL), indexes, roots, checksums, and crash recovery;
- segment cleaning, compaction, and physical-use watermarks;
- snapshot production and installation;
- requests for LV growth through the storage-node API;
- application of replicated grow and wipe operations.

It does not authorize logical growth, choose group membership, or define distributed write order.

### Replication group

Each volume has one Raft group. A Multi-Raft process multiplexes many of these groups.

The replication group owns:

- authoritative membership, roles, and configuration epoch;
- elections, write order, quorum durability, and committed local sequence number (LSN);
- bootstrap, learner promotion, membership changes, and removal;
- the committed logical size and per-replica capacity commitment;
- attachment fencing epochs and client leases;
- deterministic snapshot boundaries and WAL catch-up state;
- replication health and peer reachability.

Logical growth, capacity commitment, wipe, and membership changes require quorum agreement. The group records which NVMe device hosts each current member, but it does not choose that hardware. It consumes physical-health reports from the storage-node domain.

### Fleet inventory and reservation ledger

This domain contains two different kinds of state:

- **Authoritative temporary reservations:** short-lived claims issued for create, grow, migration, and repair workflows.
- **Derived inventory:** nodes, NVMe devices, topology, endpoints, accepted commitments, observed allocations, physical use, heat, and health.

Replication groups and storage nodes publish anti-entropy updates to the derived inventory. The inventory aggregates by NVMe device, node, rack, data center, and fleet. It does not replace group membership or local resource authority, and it does not choose placement.

### Admission and placement

This domain:

- enforces the configured overcommit policy;
- evaluates current commitments, reservations, repair reserve, topology, heat, and membership constraints;
- selects candidate NVMe devices for new and replacement replicas;
- decides whether create, grow, or repair has a feasible placement;
- returns a placement plan.

It does not create LVs, change Raft membership, or transfer data.

### Control plane

The control plane executes cross-domain workflows:

- create, grow, attach, detach, wipe, delete, migration, and repair;
- desired-versus-observed reconciliation;
- retries, idempotency, leases, and partial-completion recovery;
- node admission and removal;
- deep NVMe health checks and the repairs they trigger;
- repair prioritization and transfer-concurrency control;
- fleet-wide heat management, compaction scheduling, and resource collection.

It calls the other domains without taking ownership of their authoritative state.

Observability, security, upgrades, and service-level objectives (SLOs) are cross-cutting concerns, not additional state owners.

## What objects cross domain boundaries?

Keep volume, replication-group, and replica state separate:

```text
Volume
  volume_id, account_id, project_id, name
  desired_logical_size, observed_logical_size
  generation, lifecycle_state, replication_group_id

ReplicationGroup
  group_id, volume_id, replication_factor
  logical_size, peer_commitment_bytes
  membership, config_epoch, group_state
  committed_lsn, last_snapshot_id
  attachment_epoch, attachment_lease

Replica
  group_id, replica_id, nvme_id
  role, applied_lsn, replication_health
  allocated_bytes, used_bytes, observed_at

StorageNode
  node_id, endpoint, fault_domain_labels, health

NVMe
  nvme_id, serial_number, node_id, endpoint
  total_bytes, allocatable_bytes
  committed_bytes, commitments_observed_at
  allocated_bytes_observed, used_bytes_observed
  health, report_epoch, observed_at

CapacityReservation
  reservation_id, operation_id, group_id, nvme_id
  bytes, expires_at, state
```

Use one capacity term for each concept:

```text
logical_size              User-visible addressable bytes committed by the replication group.
peer_commitment_bytes      logical_size plus bounded per-replica overhead.
allocated_bytes           Physical LV extents assigned to one replica.
used_bytes                Assigned bytes occupied by replica data and metadata.
reservation               Short-lived claim for an imminent allocation or transfer.
repair_reserve            Capacity withheld from normal admission for migration and failures.
allocatable_bytes         Total bytes minus system overhead and repair reserve.
committed_bytes           Sum of accepted peer_commitment_bytes at the observation time.
overcommit_ratio          committed_bytes divided by allocatable_bytes.
```

For a 3:1 policy:

```text
nvme_committed_bytes  <= 3 × nvme_allocatable_bytes
fleet_committed_bytes <= 3 × fleet_allocatable_bytes
```

The commitment totals sum per-replica commitments, so the replication factor is already represented in the numerator.

State authority is explicit:

- The service database owns identity, user intent, desired lifecycle state, and workflow intent.
- The reservation ledger owns unexpired temporary reservations.
- The Raft group owns accepted membership, `logical_size`, `peer_commitment_bytes`, attachment epoch, and committed LSN.
- The storage node owns actual LV state and local hardware health.
- Replica reports own observed physical use at their stated observation time.
- Fleet totals, heat, overcommit ratios, and repair queues are derived views.

Physical use belongs to each replica because replicas can temporarily differ. Group-level logical live data is a separate value.

## How do workflows cross domains?

### Create

```text
Create volume intent
→ admit peer commitments and select replication_factor NVMe devices
→ acquire temporary capacity reservations
→ create LVs and local replicas
→ bootstrap the replication group
→ commit initial logical metadata
→ mark the volume active
→ release temporary reservations after commitments appear in derived inventory
```

### Attach

```text
Request attachment
→ replication group commits a new fencing epoch and lease
→ control plane sends routing configuration to the compute node
→ compute node creates the ublk device and renews the lease
→ replication group accepts I/O only with the current epoch
```

### User-requested growth

```text
Validate the requested logical size
→ admit the additional peer commitment
→ replication group commits the new logical size and peer commitment
→ compute node resizes or recreates the presented device
→ physical replica allocation continues on its independent watermark policy
```

### Local capacity pressure

Admission first tries to extend the existing replica:

```text
Replica crosses its pressure watermark
→ replica reports projected exhaustion
→ admission reserves capacity on the current NVMe device
→ storage node extends the LV
→ local replica adopts the larger provisioned bound
```

If the current device cannot support the extension, migration is the fallback:

```text
Placement selects a replacement destination
→ control plane reserves capacity and creates a learner
→ learner installs a snapshot and replays the WAL tail
→ replication group promotes the learner and removes the old peer
→ storage node releases the old LV
```

### NVMe device or node failure

```text
Failure detector identifies affected replicas
→ repair controller builds a risk-prioritized queue
→ placement finds feasible destinations
→ scheduler admits transfers under resource budgets
→ each group runs the learner, catch-up, and promotion workflow
```

Do not conflate these three scheduling decisions:

- **Repair priority:** quorum health, time to exhaustion, write rate, and customer tier.
- **Packing order:** commonly decreasing required capacity to preserve placeability.
- **Transfer concurrency:** source-read, target-write, host-network, and rack-network budgets.

## What must we measure?

Treat capacity and repair analysis as a workbook of models and experiments, not as component design.

For this example fleet, the inputs are assumptions:

```text
storage nodes                  = 200
NVMe devices per node          = 20
replicas per NVMe device       = 1,000
replication factor             = 3
```

The derived cardinalities are:

```text
total NVMe devices             = 4,000
total replicas                 = 4,000,000
replication groups             ≈ 1,333,333
replicas affected by node loss = 20,000
remaining NVMe devices         = 3,980
mean replacements per device   ≈ 20,000 / 3,980 ≈ 5.0
```

Five replacements per remaining device is only the unconstrained average. Topology, existing membership, capacity, heat, and repair budgets reduce the eligible destination set.

Model repair time in bytes:

```text
bytes_to_seed = sum(live snapshot bytes for affected replicas)

minimum_seed_time = max(
  bytes_to_seed / aggregate source-read budget,
  bytes_to_seed / aggregate target-write budget,
  bytes_to_seed / repair-network budget
)

minimum_repair_time = minimum_seed_time + WAL catch-up time
```

This is a lower bound. It excludes detection, scheduling, elections, placement retries, and learner promotion.

Benchmark these areas:

- **Cardinality:** LVs per NVMe device, Raft groups per process, metadata memory per group, startup time, and enumeration time.
- **Repair:** snapshot throughput, WAL catch-up rate, source and target contention, and placement success.
- **Capacity:** overcommit distribution, largest placeable peer, repair reserve, and time to exhaustion.
- **Failure scenarios:** NVMe device, storage process, node, rack, network partition, and control-plane outage.

## How do implementation tools map to the domains?

Implementation choices must follow the domain boundaries rather than define them:

- `ublk` implements compute-node block-device presentation.
- Logical Volume Manager (LVM) implements storage-node allocation and extension of local replica backing.
- `io_uring` can implement local replica I/O.
- Raft provides replication-group ordering and membership; Multi-Raft multiplexes groups per storage process.
- gRPC can carry control, placement, health, and replication traffic across explicit domain APIs.
- Kubernetes can deploy and supervise control-plane, compute-node, and storage-node processes.
- Go or Rust is an implementation choice for each component, not a domain boundary.
