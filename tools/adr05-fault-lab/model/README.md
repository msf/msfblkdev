# Which bytes can survive a crash?

Authored by LLM. Run the fixed medium and caller-history tests from the repository root:

```sh
python3 -B -m unittest discover -v -s tools/adr05-fault-lab/model
```

The tests need Python 3.8+ and the checked-in [format-4 fixture](../../../evidence/adr05-engine-arm64-20261119/failed-descriptor.img.gz). They don't open a device or start a VM.

## Physical writes

`Medium(initial, faults)` starts with exact durable bytes. A `Fault` matches a 1-based operation index, operation kind, byte offset and length. Inspect `hits` after a run; an unmatched rule stays at zero. A short completion returns its actual count. An error before a write has no effect; an error after a write leaves the data pending. An error after a sync leaves the data durable. Errno alone doesn't distinguish these outcomes. `stale` reads the last successful media sync (including a sync whose completion reported an error); `misdirect` redirects the transfer to an explicit byte offset. `disappear` takes the medium offline. `false_ack` deliberately violates the sync contract by reporting success without persisting data.

`crash_image(fragments)` returns a new image for each case. Each `(operation_index, start_in_write, length)` selects pending bytes; the list order determines the winner for overlaps. Passing `[]` loses all pending writes. A successful honest sync removes those writes from the list of candidates. For example, if operation 1 wrote 8 KiB, `[(1, 4096, 4096)]` lets only the second half survive. To test engine recovery, write the image to a fresh file and open a copy. The pure medium doesn't call `Volume::open`.

## Logical observations

`History` starts with the known client blocks. Record one serial, single-block write or discard at a time and each flush, using `ok`, `rejected` (guaranteed no effect), or `uncertain`. Before a crash, `accepts` compares the entire read vector with the possible live states. After a crash it accepts a complete prefix since the last successful flush, including any possible effects of uncertain operations. A failed flush imposes no new lower bound. The checker doesn't import the physical medium, inspect the pending fragments, or use an engine mapping table. Apply the ordered-prefix oracle only to traces for which that ordering is guaranteed. It isn't an atomicity rule for arbitrary packed engine writes.

The tests plant a wrong-LBA write and a lying sync in the medium. Additional checker controls present the forbidden read vectors directly: a rejected value, a resurrected discard, and a later record after skipping an earlier one across LBAs. One test uses the archived failed-descriptor image as initial bytes and compares the historical caller transcript with the oracle. Those reads were recorded by the earlier engine run, not produced by a new recovery in this test.

## Limits

One serial schedule. The medium accepts up to 32 operations on a 2 MiB image, 128 KiB per request, and 256 fragments per proposed crash image. The logical checker accepts 16 LBAs, up to 4096 bytes each, 32 events and 256 candidate vectors. Exceeding a bound raises an error rather than counting as a pass. Tests enumerate four 4 KiB metadata/payload cuts and selected finer tears and write orders, not all byte subsets. `ETIMEDOUT` models a completion result, not real time or a hung thread. Checking both selectable format-4 roots, handling damaged reads, and feeding generated images to production recovery need separate engine-facing tests.
