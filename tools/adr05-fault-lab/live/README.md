# Fail the checkpoint that releases a segment

Authored by LLM. Run this opt-in test as the normal user inside the [disposable fault-lab guest](../README.md). It needs a real ublk device, a build with `test-failpoints`, and QEMU's FUSE export and QMP `blockdev-reopen`. Set `SCRATCH` to a writable directory on the guest's separate ext4 test disk. Set `DEVICE` to the device that backs that filesystem. Keep the evidence directory on the guest OS filesystem. From the repository root:

```sh
(cd rust && cargo build --locked --features test-failpoints --bin block-storage-ublk --example adr05-engine)
out=$(mktemp -d /var/tmp/live-b-results.XXXXXX)
timeout -k 5s 90s python3 -u tools/adr05-fault-lab/live/checkpoint_b.py \
  "${SCRATCH:?}" "${DEVICE:?}" rust/target/debug "$out/evidence" >"$out/run.txt" 2>&1
```

The runner formats a fresh 8-LBA image for each case. The kernel allocates a new ublk device for the daemon. Through that device the client writes and flushes two known values, including an overwrite. Repeated writes of the same value then fill the small log and force checkpoint B **inside a client write**. The 30-second idle interval keeps background maintenance out of this bounded sequence. In each fault case the in-flight write must report the injected error. In the no-fault control it must succeed.

The test build pauses once when a publication will change one or more segments from `PendingFree` to `Free`. It reports the affected segments, descriptor slot and LSN after syncing the checkpoint records and **before writing the descriptor**. While the writer waits, QMP inserts a new `blkdebug` node under `raw`. The control uses the same graph swap but no rule. The error runs target the reported descriptor sector or the first `flush_to_disk` after the swap. Both arm before the descriptor write: swapping graphs between its write and final sync could introduce a flush there.

Require exit status zero and a PASS for `control`, `write` and `fsync`. The runner checks the QEMU write offsets, exact engine boundary results, live caller response, discarded/freed counts and victim bytes plus their data/hole maps. It stops the writers before reopening a separate copy of each image and checking all eight blocks. Each case allows 512 pressure writes at most. The [arm64 run](../../../evidence/adr05-live-b-20261119/run.txt) covers one workload producing six eligible segments; it isn't a power-loss or cross-platform test.

For the missed-rule control, repeat with a fresh evidence path and `--case miss`. Check the client trace: the write must have succeeded when the client expected `EIO`. A timeout or unrelated child failure does not pass this control. This case tests the harness, not the engine's response to `EIO`. Preserve the output directory and original scratch images before destroying the VM. The runner never deletes a ublk device by guessed ID.
