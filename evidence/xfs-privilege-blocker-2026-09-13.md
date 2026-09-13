# XFS formatter privilege blocker

Date: 2026-09-13
State: resolved by the operator-approved protocol-3 formatter action and subsequent V1.0 acceptance

Default `mkfs.xfs` 6.18.0-3 cannot format the caller-owned ublk device as a normal user. This is a Linux ioctl privilege check, not an unsupported ublk request or a device-node permission race.

## Measured failure

- `xfs-1789258053832864833.log`: first default lifecycle attempt failed at `mkfs.xfs /dev/ublkb0`. The exposed volume was 536870912 bytes, with 4096-byte logical and physical blocks. Kernel counters recorded zero writes to the exposed device. The daemon exited cleanly; stderr contained no rejected requests or panic.
- `xfs-1789258389760150553.log`: a second fresh fixture ran the same formatter under `strace -f -e trace=ioctl -s 256`. It recorded `ioctl(3, BLKSSZGET, [4096]) = 0`, `ioctl(3, BLKGETSIZE64, [536870912]) = 0`, then `ioctl(3, BLKBSZSET, [4096]) = -1 EACCES (Permission denied)`. The formatter exited 1. This diagnostic did not change filesystem options.
- Both failed images and raw outputs remain at `/tmp/my-block-storage-ublk.127408.0` and `/tmp/my-block-storage-ublk.131553.0`. The lab stopped both daemons cleanly. Post-run checks found no ublk device or owned mount.
- `ext4-1789258515152971194.log`: all three ext4 regression repetitions passed in 4435 ms on the generalized lab, including 12 filesystem checks and cleanup. `xfs-lifecycle-development-2026-09-13.log` records passing `make lint test`. No XFS acceptance checkbox was complete at that point.

At diagnosis, the installed helper matched the built protocol-2 binary and the frozen helper files were unchanged. The operator subsequently approved the formatter exception and installed protocol 3. The completed implementation is `0a5af77`; final evidence is recorded in ADR-06.

## Source verification

The installed formatter and checker are ordinary root-owned executable files without file capabilities. SHA-256:

```text
6345cbe029f163cd7998ea9b6c6dbea731b0fb2f23c264954cf62e463bdcdb8a  /usr/sbin/mkfs.xfs
cfd3197815e5acb3992ae80d40987607b8902d3164dcba509010e4d4d48db3b4  /usr/sbin/xfs_repair
```

Source: `https://archive.ubuntu.com/ubuntu/pool/main/x/xfsprogs/xfsprogs_6.18.0.orig.tar.xz` (SHA-256 `3a6dc7b1245ce9bccd197bab00691f1b190bd3694d3ccc301d21b83afc133199`).

- `mkfs/xfs_mkfs.c:3710`: sets `xi->setblksize = cfg->sectorsize` before `libxfs_init`.
- `libxfs/init.c:147-155`: writable block-device initialization calls `platform_set_blocksize`; failure is fatal for direct I/O.
- `libfrog/linux.c:130-145`: calls `ioctl(fd, BLKBSZSET, &blocksize)` and exits on fatal failure. It does not skip the ioctl when the size is already correct.
- `repair/init.c:75-77`: sets `setblksize = 0` and selects read-only mode for `-n`. The checker's source does not require this ioctl; live XFS checks remain unverified.

Upstream Linux v7.0 `block/ioctl.c:625-650`, `blkdev_bszset`, checks `capable(CAP_SYS_ADMIN)` and returns `-EACCES` before reading or comparing the requested size:
`https://raw.githubusercontent.com/torvalds/linux/v7.0/block/ioctl.c`.

## Operator-approved fix

The operator approved an exception to the normal-user formatter rule and helper freeze: a scoped `format-xfs` action in the installed helper. The operator installed protocol 3. It must run only the system XFS formatter with default options on the validated, unmounted, caller-owned XFS fixture. Keep the daemon, lab, file workload and read-only filesystem checks unprivileged. Do not grant sudo to a writable binary or general-purpose formatter, and do not grant general `CAP_SYS_ADMIN` to the developer.

Changing device-node modes cannot solve the capability check. Pre-setting the block size would not avoid the formatter's unconditional ioctl. Buffered-I/O overrides, ioctl suppression, formatting a regular file instead of the exposed device, and filesystem-feature changes would bypass the required default workflow; none were applied.

The subsequent default XFS lifecycle, bounded request evidence, final XFS/ext4/developer gates and cleanup audit passed. See `xfs-1789263094765541439.log` and `xfs-release-audit-2026-09-13.log`. No crash or power-loss durability is inferred.
