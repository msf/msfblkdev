# Ralph goal: V1.0 XFS correctness

Status: complete on 2026-09-13; protocol 3 installed with operator-approved XFS formatter privilege
Baseline: `v0.9` (`81a8aa8`), completed ext4 acceptance
Specification: [ADR-06 Delivery 2](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md#delivery-2-v10-xfs-correctness)

## Objective and exit

Implement and verify ADR-06 Delivery 2: V1.0 XFS correctness. Reuse the existing owned, bounded Rust filesystem lab and deterministic file workload. Use default `mkfs.xfs` and XFS storage behavior; retain the helper's `nosuid,nodev` security restrictions. Do not disable filesystem features or durability barriers to bypass a failure.

Require three complete fresh-image repetitions. Each must pass `mkfs.xfs`, four unmounted `xfs_repair -n` checks, synced file operations, exact byte/hash/directory verification, a clean block-daemon restart, remount and verification without repopulation. The check boundaries are before first mount, after first unmount, after daemon restart before remount, and after final unmount. Record every observed ublk operation/flag combination, including device discovery before readiness; bounded counters per combination are sufficient. `fsck.xfs` is not a filesystem check.

Add explicit `make check-xfs` and `make test-xfs` targets. The ordinary `make test` target must remain non-root and must not require XFS tools or mounts. Completion requires passing `make lint test`, the three-repetition XFS acceptance gate, and the existing three-repetition `make test-ext4` gate on the final code. Record commit, dirty state, binary/tool versions, effective geometry, commands, timings, results and request/flag evidence. Verify that successful runs leave no owned mount, device, child or temporary directory. Preserve failed images and raw evidence. Mark only proved ADR-06 Delivery 2 items complete; leave the database deliveries untouched.

## First iteration

Load `coding`, `rust-development`, `tech-writing` and `simplified-technical-english`. Read README.md, RALPH.md and Delivery 2 in full. Inspect current git and host state before changing anything. Existing user changes include database ADR drafts and manual test notes; preserve them and never stage their unrelated hunks.

First prove one fresh XFS lifecycle with the existing file workload, then finish operation/flag evidence and repeatability. Start from the V0.9 daemon; do not anticipate missing protocol features. Change additional ublk behavior only after capturing a request that demonstrates the need, reviewing its semantics and adding regression tests. Keep FUA rejected unless an observed workload requires it and its durability semantics are actually implemented and tested. Do not add batching, change the persistent format, implement SQLite/RocksDB/PostgreSQL, add engine concurrency or introduce crash/fault testing.

Use one bounded lab run at a time. The XFS fixture is 512 MiB logical with a 4 GiB regular-file backing ceiling. Default child and suite deadlines are 60 and 600 seconds, with the existing single bounded cleanup reserve. Check free space and memory before creating images; do not run broad benchmarks or parallel write workloads overnight. Never truncate, reuse or remove another run's image to recover space.

## Unattended privilege boundary

The operator authorizes live ext4 and XFS mount/unmount operations ONLY through `/usr/local/libexec/block-storage-mount` on test-created, caller-owned unprivileged ublk devices and their recorded owned directories. On 2026-09-13, the operator also approved the helper's scoped `format-xfs` action: default `mkfs.xfs` needs privilege for Linux's `BLKBSZSET` ioctl. Build and run the daemon, ext4 formatter, filesystem checkers, workload and lab as the normal user. Only XFS formatting and mount/unmount use the installed helper. Use `sudo -n` for the installed helper; never request an interactive password or grant sudo to a writable build binary.

The approved helper protocol is:

- `check`: reports `protocol=3`, caller UID, ext4 geometry `134217728/1073741824` and XFS geometry `536870912/4294967296` (logical/backing bytes).
- `mount <id> <owned-directory-name>` and `unmount <id> <owned-directory-name>`: unchanged ext4 behavior.
- `mount-xfs <id> <owned-directory-name>` and `unmount-xfs <id> <owned-directory-name>`: XFS with the fixed XFS geometry.
- `format-xfs <id> <owned-directory-name>`: default system `mkfs.xfs` on the validated, unmounted XFS device. The helper preserves the pinned device, lock and deadline across exec and clears the formatter environment.

Before implementation, verify passwordless `check`, the installed/current helper binary match, `/dev/ublk-control` access, the private `/run/ublksrvd` directory, XFS kernel support, and `mkfs.xfs`/`xfs_repair`. The operator must install protocol 3 before going offline. The approved formatter addition is the only exception to the prior helper freeze. Treat `rust/src/bin/block-storage-mount.rs` and `rust/src/bin/fs_support/mod.rs` as frozen after installation: an overnight edit can invalidate the installed helper and require another operator action. Reuse their `Filesystem` enum and geometry/mount checks from unprivileged code instead.

Keep working in `/home/miguel/play/my-block-storage` so rebuilding the frozen helper retains the validated build path. Do not switch branches or relocate the build while other user changes remain. The operator must leave the host awake and the Pi session running. Do not install packages, change sudo/udev/module policy, use arbitrary block devices, force/lazy-unmount, reboot or suspend the host. If a privileged prerequisite fails, preserve state and report `AWAITING OPERATOR VALIDATION`; do not claim completion or weaken the helper to continue.

## Worker loop and handoff

Follow RALPH.md in worker-sized deliveries: orient, implement the smallest failing-test/fix pair, run focused tests and `make lint test`, then record evidence and hand off. Local commits for this goal are authorized; stage only owned files or owned hunks, including the prepared helper changes after their tests pass. Do not commit the user's database ADR draft as a side effect of checking XFS acceptance boxes. Do not push, amend, rebase, reset, or create/move tags. Keep `v0.9` unchanged.

Continue through the complete XFS exit, not merely helper support or a successful first mount. Finish with commit IDs, exact gate commands/results, evidence paths, cleanup verification and any remaining limitation. If incomplete, name the blocker and last proved criterion instead of reporting V1.0 complete.

Verified completion: implementation `0a5af77` passed `make lint test check-xfs test-xfs test-ext4`. Evidence: `evidence/xfs-1789263094765541439.log`, `evidence/ext4-1789263101551586783.log`, `evidence/xfs-release-development-2026-09-13.log` and `evidence/xfs-release-audit-2026-09-13.log`. Each filesystem passed three fresh cycles, all 12 checks, nine workload checks and six clean daemon stops. Independent checks confirmed removal of fixtures, PIDs, mounts and devices. Request reports were complete and contained no real I/O errors. No crash or power-loss durability is claimed.

The later approved goal selects ADR-06 Delivery 3 after these XFS commits. This file remains the completed XFS contract, not a restriction on that separately approved database delivery.
