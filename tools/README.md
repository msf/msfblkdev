# Native lab tools

To inspect the planned test matrix or generate readable KPI reports from
existing evidence, use the [workbench reporting CLI](workbench/README.md).
See [ADR-10](../ADR-10-WORKBENCH-TESTS-AND-REPORTS.md) for test goals,
execution commands, KPI definitions and the remaining VM integration.

Keep recipes, patches and source checksums in this directory. Generated sources,
dependency prefixes, manifests and executables live in ignored `tools/build/`.
Build as the normal user. These recipes do not install host packages, register
system services, or require containers.

| Command | Output | Pins |
|---|---|---|
| `make setup-sqlite` | `tools/build/sqlite-3.51.3/build/` | SQLite archives, adapter and observer hashes, compiler flags and binary hashes |
| `make setup-native` | `tools/build/workbench-native/root/` | Ubuntu resolute amd64 packages by version and SHA-256; extraction only |
| `make setup-go-ycsb` | `tools/build/go-ycsb-427715a/bin/go-ycsb` | Local source commit, Go module versions/checksums, native packages and build tags |
| `make setup-rocksdb` | `tools/build/rocksdb-9.11.2/build/` | RocksDB 9.11.2 and gflags 2.2.2 source hashes; GCC 13; static build without compression |

The go-ycsb recipe reads `~/play/go-ycsb` by default. Set `GO_YCSB_SOURCE` to use
another checkout containing commit `427715abeef2186b123cc93ced4acb385ff59ce1`.
It exports that commit into its own build directory and leaves the source checkout
unchanged. Its private adaptation replaces the obsolete RocksDB binding with
`github.com/linxGnu/grocksdb` v1.9.9 and adjusts four option types. Both database
drivers are enabled explicitly.

`go-ycsb/lab.patch` adds `workload.seed`, SQLite WAL/FULL settings with readback,
a 16 MiB SQLite cache with mmap disabled, synchronous RocksDB writes, and a
16 MiB RocksDB cache with compression disabled. It also orders SQLite scans,
frees RocksDB scan values and removes empty rows from short scan results.

`go-ycsb/workloads/common.properties` sets the project's 4 KiB value default,
8,192 initial records and 131,072 requests (the original three-run request budget
consolidated into one run, then rounded up to 128 Ki requests). Combine it with `a.properties`, `f.properties`,
or `e75.properties`. The E75 profile adds ordered 16-record scans
and inserts alongside the existing A and F workloads. These are workload
inputs, not a complete benchmark runner: restart, full-content verification,
space accounting and stricter I/O calibration still need the integration in the
[v1.3 benchmark plan](../docs/V1.3-BENCHMARK-PLAN.md).

`go-ycsb/time-ext4.py` measures the six cases in a temporary directory on the
existing `/var/tmp` ext4 mount. Run it in a user cgroup with `MemoryMax=256M` and
`MemorySwapMax=0`; it checks those limits and the filesystem, probes sync calls,
and records load/run times plus row-count validation. This estimates duration;
it does not format a device or establish the final storage comparison.

The go-ycsb package prefix currently supplies SQLite 3.46.1 and RocksDB 9.11.2.
The existing SQLite acceptance suite still uses its separately built SQLite 3.51.3.
Treat these as different tool identities; their results are not interchangeable.

`setup-native` targets this host's Ubuntu resolute amd64 ABI. The source builds
use the host compiler and standard runtime; their manifests record build identity.
This is a reproducible local dependency prefix, not a complete hermetic toolchain.
Go modules retain their upstream `go.sum` verification. RocksDB's source build
needs GCC/G++ 13, CMake, Ninja and the host development prerequisites detected by
CMake. Generated `CMakeCache.txt` records those detections.

Run build recipes serially with respect to benchmarks. Recreating a native prefix
while another process uses it can invalidate measurements or interrupt a build.
Successful tool builds do not constitute filesystem/database acceptance.

`bash tools/vm/setup.sh` builds a minimal initramfs and extracts a checksum-pinned
kernel package for the guest prerequisite probe. It records the installed
static BusyBox and matching host ublk/XFS module hashes. Nothing is installed on
the host. `bash tools/vm/boot-probe.sh [RAM_MiB]` boots it using KVM, one vCPU
and 320 MiB of RAM by default, checks ublk/ext4/XFS and disabled swap, then powers off. It saves
the command, build identity and serial console in a unique evidence directory.
This probe is not yet a database benchmark image. The selected comparison will
run its database, filesystem and storage backend inside the guest budget, with
direct host I/O for virtual disks. Start calibration at 320 MiB and increase in
64 MiB steps until every selected stack fits. Use one final RAM size for both
logical device capacities (128 MiB and 4 GiB) and all paired storage comparisons.
