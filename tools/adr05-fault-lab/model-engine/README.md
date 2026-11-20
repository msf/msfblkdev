# What if only half a data record survives?

Authored by LLM. Run this gate inside the [disposable Linux guest](../README.md), using its separate ext4 scratch disk. Set `SCRATCH` to a writable directory on that filesystem and `DEVICE` to its block device. From the repository root:

```sh
(cd rust && cargo build --locked --example adr05-model-image)
timeout -k 5s 90s python3 -B -u tools/adr05-fault-lab/model-engine/record-cuts.py \
  "${SCRATCH:?}" "${DEVICE:?}" rust/target/debug/examples/adr05-model-image
```

Require exit zero and `record_cuts=PASS`. The runner prints and retains its private fixture directory. Copy evidence before deleting the VM.

The caller writes and flushes two known values, captures the entire format-4 image, then overwrites LBA 1 once without a flush. The engine ledger must report one completed 8 KiB write and no further sync. The runner refuses a capture unless only two adjacent 4 KiB blocks changed, the payload matches the write, and the footer links to the previous record.

To construct a recovery case, the runner gives the Python `Medium` the bytes from the captured write. It starts with the pre-write image and overlays only the fragments selected for that case. Production recovery opens a new file for each case; the input snapshots stay unchanged.

| Surviving part of the overwrite | Expected LBA 1 on production reopen/read |
|---|---|
| Neither block or payload only | Read the old flushed value, `0x22`. |
| Footer only | Open and replay succeed. Reading LBA 1 reports `InvalidData` (`payload checksum mismatch`) without copying data into the caller's buffer. |
| Both blocks | Read the new value, `0x33`. |

In every case LBA 0 retains its flushed value and all six unwritten LBAs read back as zero. These reads test **which mapping replay used**, not which descriptor slot won. The checksum error is a Rust `Volume` error, not a measurement of the proposed ublk errno. No production failpoint, release-policy change, physical power cut, schedule control or real backing `EIO` is involved. This covers four whole-block outcomes of one raw record, not all sector tears. See the [guest trace and captured images](../../../evidence/adr05-model-engine-20261119/).
