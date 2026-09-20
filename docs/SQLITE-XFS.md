# SQLite on XFS

Date: 2026-09-20
Status: SQLite/XFS accepted; ADR-06 RocksDB and PostgreSQL remain open

All 87 SQLite/XFS cases passed in 160.073 seconds on the dirty tree based on
`bc801ad7a39340609e672d2992f8cbc5275aa6ec`. The gate uses the same pinned SQLite
3.51.3 tools, cases, profiles, content oracle and measurements as
[SQLite/ext4](../SQLITE-EXT4.md).

Each case creates a fresh 512 MiB XFS device with a 4 GiB backing file on tmpfs.
The existing protocol-3 helper formats, mounts and unmounts the owned device.
The cumulative backing-write limit remains 512 MiB across formatting, both daemon
lifetimes and cleanup. Command and suite deadlines remain 60 and 600 seconds.

## Run and reproduce

After completing the [normal-user setup](../README.md#normal-user-ublk-setup), run:

```sh
make check-sqlite-xfs
make test-sqlite-xfs
```

To repeat a single matrix member:

```sh
make test-sqlite-xfs-one SQLITE_CASE=speedtest-main \
  SQLITE_PROFILE=wal-full SQLITE_REPETITION=1
```

`make test` remains unprivileged and creates no live device or mount. The XFS
matrix runs separately from ext4. Baseline validation requires matching
filesystems; it rejects a mixed matrix or an ext4/XFS performance comparison.

## Acceptance evidence

The [full run](../evidence/sqlite-1789917155929943435/summary.json) and
[independent audit](../evidence/sqlite-xfs-audit-2026-09-20.json) establish:

- 348 successful unmounted `xfs_repair -n` checks and 174 logical validations.
- 174 clean daemon stops, complete request reports and zero real I/O errors.
- 12 automatic PASSIVE checkpoints in each of six WAL stress runs; a maximum
  write reservation of 169,025,536 bytes against the 536,870,912-byte limit.
- No remaining owned fixture directories, daemon processes, devices or mounts.

The [selected baseline](../evidence/sqlite-xfs-baseline-2026-09-20.json) records
artifact hashes and every case's settings. It is an initial baseline, not evidence
of improvement. The [developer gate](../evidence/sqlite-xfs-development-2026-09-20.log)
records passing `make lint test`. The six baseline-parser tests also pass.

These tests establish the clean lifecycle and logical correctness. They do not
establish crash or power-loss durability, physical-media latency, or steady-state
performance. The LVM devices supplied for the proposed workbench were not used.

The [evidence archive](../evidence/sqlite-xfs-acceptance-2026-09-20.tar.gz) contains
the full XFS run, the ext4 regression, the post-migration probes on both
filesystems and the passing developer gates. Extract it from the repository root
to restore the ignored raw evidence paths used by the baseline pointer:

```sh
tar -xzf evidence/sqlite-xfs-acceptance-2026-09-20.tar.gz
```
