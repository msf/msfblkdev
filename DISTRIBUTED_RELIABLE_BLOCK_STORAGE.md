# Block Storage Design Domains

This focusses on design from first principles and very simple building blocks a full implementation of block devices for Railway.
It minimizes the focus on the "storage-peer" that implements a local-only thinly provisioned blockdevice, it maximizes the focus on the distributed systems problem.

Use **domain boundaries for ownership** and **workflows for overlap**.


## 0. Requirements imposed:

Cardinality of volumes: hundreds of thousands to millions

Work for +100 to +3000 Storage Nodes, with ~5-50 NVMe nodes per node.
Total NVMe counts: +1000 to 100_000 (growth for 100x)
Store ~1k volumes per NVMe, expected volume size  1GB < N < 8TB
Minimize MTTR on NVME loss, parallel repair of all volumes present on single NVME upon NVME loss.
Must support storage overcommitment: sum of logical size of all volumes > (sum of physical size of all volumes/replication_factor)
Design for defined upper commitment: 1, 2, 3,... (1 means no overcommitment)
Volumes are "thinly privisioned"

Risks and key problems:
- with storage dense nodes, a single node death creates a massive replication storm, and the amount of data to repair is extremely large.

 - aka, high ratio of volume-replica per storage node is a key problem to solve
 - at this scale of so many volumes, multi-raft must be implemented to reduce replication overheads of so many replication groups
- E_NO_SPACE due to overcommitment on storage (running out of capacity)
- Thin provisioning complicates volume management (support for demand-based and usage based growth)

## 1. Functional domains

### 1.1 Service and user-resource domain

- Accounts, projects, volumes, names, logical sizes, and quotas.
- User operations: create, grow, attach, detach, wipe, and delete.
- User-visible lifecycle and durability/capacity contract.
- Does not know about LVs, WALs, or replica placement.

### 1.2 Compute-node presentation domain

- Creates and removes local block devices through `ublk`.
- Routes I/O to the replication group.
- Behaves like the Frontend Router/Gateway to a rep-vol-id + control-plane
- Attachment state, fencing epochs, and reconnect behavior.
-  maintains a metadata lease for "attached" state, attached == ublk exists and available to userland + container
- Makes devices available to container userland.
- Does not select replicas or own durable volume state. - stateless, it always issues commands to the rep-vol-group 

### 1.3 Storage-node resource domain

- Node coordinator.
- NVMe discovery, identity, health, and VG registration.
- LV reservation, creation, extension, and deletion.
- Starts and supervises the per-NVMe Multi-Raft/storage process.
- Reports local capacity and health; does not make fleet placement decisions.

### 1.4 Local replica storage-engine domain

- Persistent LBA-to-data mapping.
- WAL, indexes, superblocks, checksums, and crash recovery.
- Segment cleaning/compaction and physical watermarks on usage, reservation
- Snapshot production and installation.
- Operation for reservation size growth (uses storage-node api for LV growth, but controls this operation)
- Operation for committed size growth
- Operation for WIPE/clear
- Owns one replica's physical representation, not distributed ordering.

### 1.5 Replication-group domain

- Multi-Raft group lifecycle, elections, and write ordering.
- all rep-vol-ids, all rep-vol-group-membership and metadata information
- the leases for the attached client endpoint (which means ublk exists and is available to userland)
- Quorum durability and committed LSN.
- Bootstrap, learners, membership changes, and removal.
- Group Snapshot boundary plus WAL catch-up, deterministic, requires quorum success
- Group metadata operations such as: reservation or committed size growth, requires quorum success
- Owns authoritative group membership and logical replicated metadata; does not choose hardware.
- Owns authoritative information on all committments and reservations, all NVMEs in effective use
- Owns heartbeats and health-checks on the rep-vol-group + nvme

### 1.6 Fleet inventory and capacity-ledger domain

- NVMes, (and nodes) topology/fault labels, endpoints, and health, commitments and reservations
- Authoritative commitments and temporary reservations.
- Eventually consistent physical-use observations.
- Aggregates by NVMe, node, rack, DC, and fleet.
- Provides facts; it does not choose placement, it isn't authoritative, it is advisory because the authoritative source is the sum of all rep-vol-groups and their metadata.
  - rep-vol-ids (or multi-raft groups) run anti-entropy to update this information with the authoritative information they hold

### 1.7 Admission and placement domain

- Enforces oversubscription policy.
- Selects candidate NVMes under topology, capacity, heat, and membership constraints.
- Bin-packs new and replacement peers.
- Decides whether create, grow, or repair has a feasible placement.
- Returns a placement plan; it does not create LVs or transfer data.

### 1.8 Control-plane

- Executes create, grow, wipe, delete, peer movement on rep-vol-groups.
- Reconciles desired versus observed state.
- Handles retries, idempotency, leases, and partial completion.
- handles adding and removing nodes and deep node-nvme health checks
- Prioritizes rep-vol-group repairs and controls concurrency.
- handles also heat management and fleet-wide compactions, defrags, etc..
- handle fleet wide metric or resource collection
- Calls the placement, node-resource, and replication domains without owning their truths.

Observability, security, upgrades, and SLOs are cross-cutting concerns, not additional owners of state.

## 2. Shared object and capacity model

Use separate entities rather than combining volume, group, and replica state:

```text
Volume
  volume_id, account_id, project_id, name
  logical_size, generation, lifecycle_state
  replication_group_id

ReplicationGroup
  group_id, volume_id, replication_factor
  committed_capacity_per_replica,
  allocated_capacity_per_replica,
  membership, last_snapshot_id, LSNs 
  config_epoch, group_state
  attachment_state

Replica
  group_id, replica_id, nvme_id
  role, applied_lsn, health,
  peer_commitment,
  allocated_bytes, physical_used_bytes, observed_at

StorageNode
  node_id, endpoint, fault-domain labels, health

NVMe
  nvme_id, serial_number, node_id, endpoint
  total_bytes, committed_bytes
  allocated_bytes_observed, used_bytes_observed (observed because authoritative is the sum of ReplicationGroups on that nvme)
  health, report_epoch, observed_at
  reservations

CapacityReservation
  reservation_id, operation_id, group_id, nvme_id
  bytes, expires_at, state

```

Use unambiguous capacity terms:

```text
logical_size       User-visible addressable bytes.
peer_commitment    logical_size + bounded per-replica overhead.
allocated_bytes    Physical LV extents currently assigned.
used_bytes         Bytes currently occupied by data and metadata.
reservation        Short-lived claim for an imminent allocation.
repair_reserve     Capacity withheld for migration and failures.
overcommit_ratio   committed_bytes / allocatable_bytes.
```

For a 3:1 policy:

```text
device_committed_bytes <= 3 × device_allocatable_bytes
fleet_committed_bytes  <= 3 × fleet_allocatable_bytes
```

Keep state ownership explicit:

- Control-plane DB: identity, ownership, commitments, reservations, and workflow intent.
    rep-vol-group creation, repairs (membership adds, deletes), migrations
    nvme-deep healthcheck and repairs (drives requests for rep-vol-group changes)
- Raft group: membership, configuration epoch, logical metadata, and committed LSN.
- Storage node: actual LV state, physical use, local health, and multiplexer to nvme-centric rep-vol-peers and local volume state
- Derived views: fleet totals, heat, overcommit ratios, and repair queue.

Physical use belongs to each **replica**, because replicas can temporarily differ. Group-level logical live data is a separate value.

## 3. Cross-domain workflows


### 3.1 Create

```text
Create volume
→ capacity admission
→ select three NVMes
→ acquire capacity reservations
→ create LVs/local replicas
→ bootstrap replication group
→ initialize logical volume
→ mark volume active
→ convert/release temporary reservations
```

### 3.2 Attach

```text
Create attachment and fencing epoch
→ send routing/configuration to compute node
→ create ublk device
→ storage group accepts I/O carrying current epoch, registers compute-node-id and attachment lease
```

### 3.3 User-requested grow

```text
Validate new logical size
→ increase fleet commitment (validate fleet_allocatable_bytes)
→ rep-vol-group command: update replicated logical metadata
→ resize compute-side device
→ physical replica capacity continues growing independently
```

### 3.4 Local capacity pressure

```text
Replica crosses pressure watermark
→ report projected exhaustion
→ placement selects replacement destination
→ reserve capacity and create learner
→ transfer snapshot
→ replay WAL tail
→ promote learner
→ remove old peer
→ release old LV
```

### 3.5 Device or node failure

```text
Failure detector identifies affected replicas
→ repair controller builds risk-prioritized queue
→ placement processes required sizes
→ scheduler admits transfers under resource budgets
→ normal learner/catch-up/promotion workflow
```

Separate three decisions that are easy to conflate:

- **Repair priority:** quorum health, time to exhaustion, write rate, and customer tier.
- **Packing order:** commonly decreasing required capacity to preserve placeability.
- **Transfer concurrency:** source-read, target-write, host-network, and rack-network budgets.

## 4. Fermi and validation domain

Treat this as a workbook of models and experiments, not component design.

For the example fleet:

```text
nodes                          = 200
NVMes per node                 = 20
peers per NVMe                 = 1,000
total NVMes                    = 4,000
total replica peers            = 4,000,000
groups at replication factor 3 ≈ 1,333,333

replicas affected by node loss = 20,000
remaining NVMes                = 3,980
mean replacements per NVMe     ≈ 20,000 / 3,980 ≈ 5.0
```

Five is only the unconstrained average; topology, existing membership, capacity, and heat reduce eligible destinations.

Repair time should be modeled in bytes, not replica count:

```text
bytes_to_seed = sum(live snapshot bytes for affected replicas)

minimum repair time = max(
  bytes_to_seed / aggregate source-read budget,
  bytes_to_seed / aggregate target-write budget,
  bytes_to_seed / repair-network budget
) + WAL catch-up time
```

Benchmark buckets:

- **Cardinality:** LVs per NVMe, Raft groups per process, metadata memory per group, startup time, and enumeration time.
- **Repair:** snapshot throughput, WAL catch-up rate, source/target contention, and placement success.
- **Capacity:** overcommit distribution, largest-placeable peer, repair reserve, and time to full.
- **Failure scenarios:** NVMe, storage process, node, rack, network partition, and control-plane outage.

Keep Go/Rust, gRPC, LVM, `io_uring`, `ublk`, and Kubernetes in a final **implementation mapping** section. They implement these domains; they should not define the problem decomposition.
