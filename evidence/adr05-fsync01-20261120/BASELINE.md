# FSYNC-01: baseline and controls (2026-11-20)

Authored by LLM. Source `c2ceb37` has the test seam and two tests; the production engine is unchanged from `8805e6c`. A normal user ran the following on Debian arm64 with a disposable build directory and temporary regular files. No ublk device, mounted test filesystem, sudo or external disk was needed.

```sh
export RUSTUP_TOOLCHAIN=1.97.1 CARGO_NET_OFFLINE=true
make lint
make test-fsync01
```

[Result excerpt](baseline.txt): lint passed; the controls passed; the engine case failed its client FLUSH availability assertion (`make` exit 2, Cargo exit 101). Both 8192-byte writes completed before the chosen barrier. The test restored the previously flushed image and returned `EIO` instead of executing that barrier. The engine reported `EIO` and latched the volume. It did not acknowledge lost data. This is a failure against the accepted retry target, not a defect against the old fail-closed contract.

The clean control recovered the two new blocks. The planted sync-only policy observed one failed barrier, issued a real successful backing fsync without any rewrites and returned client FLUSH success. Production recovery on the image cut at completion returned **both old blocks**. The recovered blocks did not match the new values recorded outside the image, despite the successful client FLUSH.

To pass the engine case, recovery must return the new values. The trace must also show that the engine rewrote both complete physical records in order at their original offsets before the successful backing barrier. The test compares all bytes, not the printed digests. Compare digests only within one fixture: the formatter chooses a volume ID for each image.

The loss is deterministic at the engine boundary. This is not a live ext4 writeback failure, a PostgreSQL workload, a test of descriptor-specific kernel error reporting or a power-cut result.
