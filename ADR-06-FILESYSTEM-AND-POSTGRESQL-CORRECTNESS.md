# ADR-06 Filesystem and PostgreSQL correctness

Date: 2026-08-30
Author: Miguel Filipe
Status: proposed
Goal status: V0.9 complete; V1.0 and V1.2 not started
On-disk format: 1 (unchanged)
Related: [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md), [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md), [ADR-04](ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md), [ADR-05](ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md)
Updates: the ADR-01 delivery order when accepted

## Context

ADR-03 proves direct 4 KiB I/O through ublk. It does not prove that a filesystem can mount or use the device.

The first ext4 trial found this difference. `mkfs.ext4` and `e2fsck` succeed because they issue flag-free I/O. The ext4 mount path marks metadata reads with `UBLK_IO_F_META`. At that commit, the daemon rejected the flag with `EOPNOTSUPP`, so ext4 could not read its superblock. The existing `fio` tests do not issue metadata-tagged requests and did not detect this problem.

ADR-01 currently places backing-medium fault injection before filesystem workloads. ADR-04 also says that it does not change that order. This sequence is no longer useful. We need to learn the block operations and flags that ext4, XFS and PostgreSQL use before we change the persistent format or build a broad fault matrix.

## Decision

We will complete this ADR before implementation work starts on ADR-04 or ADR-05.

The implementation order is:

```text
ADR-03 complete
→ ADR-06 ext4, XFS and PostgreSQL correctness
→ ADR-04 growable thin-provisioned format
→ ADR-05 backing-medium fault resilience
```

This order supersedes the numbered goal order in ADR-01 and the ordering statement in ADR-04. Supporting ext4 and XFS is a hard prerequisite for both ADR-04 and ADR-05.

This ADR keeps on-disk format version 1. It does not add thin provisioning, compaction, medium-fault injection or performance concurrency. It adds only the ublk semantics required by the accepted functional workloads.

Optional block operations remain unadvertised until the engine implements their semantics. The daemon must reject an unsupported operation instead of returning false success. An implementation may accept a request hint only after proving that the hint does not change data or durability semantics.

## Operator and test boundary

Build and run the filesystem lab as the normal user. Only mount and unmount use a separately installed, root-owned helper through a user-specific sudo rule. The helper accepts only caller-owned unprivileged ublk devices and owned lab directories. Do not grant sudo access to a user-writable lab binary.

Use default ext4 formatting and storage behavior. The helper adds only `nosuid,nodev` to restrict the privilege grant. It does not disable journaling or durability barriers. Live mount operations require explicit operator approval.

The lab must:

- create its own regular-file backing store and mount directory;
- format and use only the ublk device identity returned by its child daemon;
- verify the device identity and geometry before every destructive operation;
- verify the mount source before unmounting;
- stop and reap all workload children before unmounting; keep the block daemon running until the filesystem is unmounted;
- use a fresh image for each filesystem run;
- remove only its recorded device, mount and temporary directory;
- preserve the image and evidence if identity or cleanup state becomes ambiguous;
- reject caller-supplied backing-device and mount paths.

The normal `make test` target must not mount filesystems or require root. Filesystem acceptance remains an explicit operator run with bounded child deadlines and a suite deadline.

## Delivery 1: V0.9 ext4 correctness

The daemon accepts `UBLK_IO_F_META` on READ and WRITE as a request hint. The flag does not change the read or write result. The daemon continues to reject `UBLK_IO_F_FUA` and unknown flags with `EOPNOTSUPP`.

Live validation also observed READ requests with `op_flags=0x700` during device discovery. These are `UBLK_IO_F_FAILFAST_DEV`, `UBLK_IO_F_FAILFAST_TRANSPORT` and `UBLK_IO_F_FAILFAST_DRIVER`. Linux defines them as requests not to retry device, transport and driver errors. The adapter already issues each read once and returns failures. It accepts these hints on READ, including combinations with META; WRITE and FLUSH support is unchanged. Tests cover all hint combinations, invalid geometry, FUA and unknown flags.

The ext4 scenario uses this lifecycle:

1. Format and expose a fresh volume.
2. Create an ext4 filesystem on the recorded ublk device.
3. Keep the filesystem unmounted and run `e2fsck -f -n` against the ublk device.
4. Mount the filesystem on the owned mount directory.
5. Create directories and files, overwrite and rename files, and truncate one file.
6. Call fsync for file data and containing directories.
7. Read the files and verify recorded hashes.
8. Unmount the filesystem and run `e2fsck -f -n` again.
9. Stop and restart the daemon cleanly, then expose the same backing image.
10. Run `e2fsck -f -n`, mount the filesystem, and verify the same hashes.
11. Unmount the filesystem and run the final `e2fsck -f -n`.

Every filesystem check runs while the filesystem is unmounted. Every `e2fsck -f -n` command must report a clean filesystem and exit successfully. The lab must preserve its output when any check fails.

Acceptance requires three complete repetitions, each with a fresh backing image. The initial fixture exposes 128 MiB backed by a 1 GiB regular file. Each child command has a 60-second deadline; the three-run suite has a 600-second deadline. Failure cleanup has one additional 60-second reserve plus bounded process reaping. These are test limits, not performance claims.

The workload compares exact bytes, lengths, directory entries and required absences against an independent expected result. It also records content hashes outside the tested filesystem. It checks partial-block overwrite, cross-directory rename, truncate, zero-filled extension and deletion. Failures preserve the backing image and command output. An ambiguous mount preserves the daemon and records its PID instead of forcing an unmount.

`make check-ext4` checks prerequisites without creating a device or mount. `make test-ext4` runs the explicit operator-approved acceptance gate. Neither is part of `make test`.

Acceptance tests:

- [x] A metadata-tagged READ decodes to the same engine request as a flag-free READ.
- [x] A metadata-tagged WRITE decodes to the same engine request as a flag-free WRITE.
- [x] FUA and unknown request flags still return `EOPNOTSUPP`.
- [x] `mkfs.ext4` and all four `e2fsck -f -n` checks succeed against the recorded ublk device.
- [x] The mounted file operations complete without an unsupported request, panic or hang.
- [x] File hashes match before unmount, after remount and after the clean daemon restart.
- [x] The lab removes every owned mount, ublk device and temporary file on success.
- [x] `make lint test` passes without weakening ADR-03 coverage.

Initial acceptance evidence (2026-09-12, before implementation commits, based on `b5a294a`):

- `evidence/ext4-1789253771549431348.log`: three fresh-image repetitions passed in 3.989 seconds on Linux 7.0.0-31-generic with e2fsprogs 1.47.2. All 12 filesystem checks, exact content/hash checks and six clean daemon exits passed. A post-run audit confirmed that all three temporary directories, six daemon PIDs and owned mounts/devices were gone.
- `evidence/ext4-development-2026-09-12.log`: the final `make lint test` passed after live acceptance. META and FAILFAST regression tests each failed before their corresponding fix and passed afterward.
- Failed runs remain preserved: `ext4-1789252944846851909.log` records the missing libublk runtime-directory prerequisite; `ext4-1789253603159228766.log` records FAILFAST READ rejection; `ext4-1789253701121336829.log` records the udev permission race. The lab now waits for read/write access before declaring device readiness.

Release verification (2026-09-13): committed implementation `e635c5e` passed another three fresh-image repetitions in 3.861 seconds; see `evidence/ext4-1789255520952406853.log`. The release developer gate is recorded in `evidence/ext4-release-development-2026-09-13.log`.

The known ext4 mount issue in the appendix is closed. This proves the clean filesystem lifecycle, not crash or power-loss durability.

## Delivery 2: V1.0 XFS correctness

The XFS scenario uses the same file operations, sync points, daemon restart and hash checks as V0.9.

XFS validation uses `xfs_repair -n` against the unmounted ublk device. `fsck.xfs` is not an acceptance command because it does not check the filesystem.

Acceptance tests:

- [ ] `mkfs.xfs` succeeds on a fresh recorded ublk device with a supported volume size.
- [ ] `xfs_repair -n` succeeds before the first mount, after the first unmount, after the daemon restart and after the final unmount.
- [ ] The mounted file operations complete without an unsupported request, panic or hang.
- [ ] File hashes match before unmount, after remount and after the clean daemon restart.
- [ ] The evidence records every ublk operation and flag that the XFS lifecycle uses.
- [ ] The lab removes every owned mount, ublk device and temporary file on success.
- [ ] `make lint test` passes without weakening V0.9 or ADR-03 coverage.

V0.9 and V1.0 are complete before any ADR-04 or ADR-05 implementation delivery starts.

## Delivery 3: V1.2 PostgreSQL correctness

V1.2 runs PostgreSQL on each filesystem accepted by V0.9 and V1.0. It is a correctness test, not a performance result.

The PostgreSQL scenario uses this lifecycle:

1. Mount a fresh accepted filesystem.
2. Initialize PostgreSQL with data checksums enabled, then start PostgreSQL.
3. Initialize a bounded `pgbench` database.
4. Run a bounded `pgbench` workload and require every client to finish successfully.
5. Run `pg_amcheck` against the running database.
6. Stop PostgreSQL cleanly and run `pg_checksums --check` against the data directory.
7. Unmount the filesystem and run its read-only filesystem check.
8. Restart the block daemon cleanly and expose the same backing image.
9. Run the filesystem check again, mount the filesystem and start PostgreSQL.
10. Run `pg_amcheck` and application-level row checks.
11. Stop PostgreSQL, unmount the filesystem and run the final filesystem check.

The implementation must set explicit limits for database scale, clients, runtime and total physical writes. These limits must keep the test below the finite format-version-1 log capacity. Evidence must record the selected values and actual write budget.

Acceptance tests:

- [ ] PostgreSQL initializes with data checksums on ext4 and XFS.
- [ ] The bounded `pgbench` initialization and workload complete on ext4 and XFS.
- [ ] `pg_amcheck` reports no corruption before and after the clean block-daemon restart.
- [ ] `pg_checksums --check` reports no checksum failure while PostgreSQL is stopped.
- [ ] `e2fsck -f -n` reports clean ext4 state at every required check.
- [ ] `xfs_repair -n` reports clean XFS state at every required check.
- [ ] PostgreSQL starts and returns the expected application-level rows after remount.
- [ ] The complete scenario stays within its process, suite and physical-write limits.
- [ ] `make lint test` passes without weakening V0.9, V1.0 or ADR-03 coverage.

This ADR does not define V1.1. A milestone number does not need a delivery created only to fill the sequence.

## Later fault testing

ADR-05 defines the later fault mechanisms and detailed fault matrix. That work must consider both ADR-05 and this ADR.

Fault acceptance must include end-to-end user behavior, not only engine recovery. Selected faults must run through the accepted ext4, XFS and PostgreSQL workflows. After a daemon or backing failure, the lab must prove that it can safely re-expose the volume, check the unmounted filesystem, remount it when valid, and verify durable user data. PostgreSQL cases must also run database checks.

The exact ublk recovery mode, stale-mount handling, forced-unmount policy and repair policy belong to ADR-05. This ADR does not choose them.

## Goal exit

This ADR is complete only when:

- [ ] V0.9, V1.0 and V1.2 acceptance items are complete.
- [ ] ext4 and XFS pass every required read-only filesystem check.
- [ ] PostgreSQL passes the bounded correctness workload on ext4 and XFS.
- [ ] Evidence records the commit, kernel, tool versions, backing geometry, commands, timings and results.
- [ ] The operator-run lab leaves no owned mount, ublk device, child process or temporary directory after success.
- [ ] The top-level Rust lint and developer test gates pass.

ADR-04 and ADR-05 implementation work remains blocked until this goal exit is complete.

## Consequences

We accept that ADR-04 and ADR-05 start later. The current engine remains finite, serialized and slow during this phase.

We gain filesystem and database workloads that expose real block-protocol requirements. ADR-04 can then change the format against measured workload behavior. ADR-05 can inject faults through user-visible operations instead of testing only the engine and direct `fio` path.

## Appendix: known issue from the first ext4 mount attempt

### Report

```text
Status:       closed by V0.9 acceptance on 2026-09-12
Observed at:  commit 1e2899d
System:       Linux 7.0.0-29-generic, ublk_drv, mke2fs 1.47.0
Works:        mkfs.ext4, e2fsck, direct flag-free fio
Fails:        mount of the ext4 filesystem
Mount error:  mount reports a bad or unreadable superblock
Kernel error: operation not supported on metadata READ requests
Ext4 result:  unable to read superblock
```

### Reproduction result

```text
mkfs.ext4 -F /dev/ublkbN       succeeds
e2fsck -n -f /dev/ublkbN      succeeds and reports clean
mount /dev/ublkbN <mountpoint> fails with EOPNOTSUPP
```

Representative kernel output:

```text
operation not supported error, dev ublkbN, sector 0 op 0x0:(READ)
EXT4-fs (ublkbN): unable to read superblock
```

### Known cause

```text
Caller:       ext4 sb_bread metadata path
Kernel flag:  REQ_META
ublk flag:    UBLK_IO_F_META, bit 11, 0x800
Daemon path:  decode_request
Daemon result: any nonzero flag bits after the opcode return EOPNOTSUPP
Test state:   the existing unit test requires META-tagged READ to fail
```

At the observed commit, `decode_request` applied this rule:

```rust
let operation = descriptor.op_flags & 0xff;
let flags = descriptor.op_flags & !0xff;
if flags & UBLK_IO_F_FUA != 0 {
    return Err(-libc::EOPNOTSUPP);
}
if flags != 0 {
    return Err(-libc::EOPNOTSUPP);
}
```

### Why earlier acceptance passed

```text
mkfs.ext4: unix_io issues raw requests without UBLK_IO_F_META
e2fsck:    unix_io issues raw requests without UBLK_IO_F_META
fio:       ADR-03 uses direct 4 KiB requests without metadata flags
mount:     the buffer-cache path marks superblock and metadata reads as META
```

Result: ADR-03 proved the direct I/O path, not filesystem compatibility.

### V0.9 fix direction

```text
Allow:   UBLK_IO_F_META as a semantics-free request hint
Reject:  UBLK_IO_F_FUA until the engine implements FUA durability
Reject:  unknown flags until each flag has reviewed semantics and tests
Verify:  unit decoding, ext4 mount lifecycle, hashes and e2fsck checks
```

The claim is limited to the observed ext4 failure. XFS behavior remains unverified until V1.0.
