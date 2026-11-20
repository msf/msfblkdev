# FSYNC-01: baseline and controls (2026-11-20)

Authored by LLM. Source `c2ceb37` has the test seam and two tests; the production engine is unchanged from `8805e6c`. A normal user ran the following on Debian arm64 with a disposable build directory and temporary regular files. No ublk device, mounted test filesystem, sudo or external disk was needed.

```sh
export RUSTUP_TOOLCHAIN=1.97.1 CARGO_NET_OFFLINE=true
make lint
make test-fsync01
```

[Result excerpt](baseline.txt): lint passed; the controls passed; the engine case failed its client FLUSH availability assertion (`make` exit 2, Cargo exit 101). Both 8192-byte writes completed before the chosen barrier. The test restored the previously flushed image and returned `EIO` instead of executing that barrier. The engine reported `EIO` and latched the volume. It did not acknowledge lost data. This is a failure against the accepted retry target, not a defect against the old fail-closed contract.

The clean control recovered the two new blocks. The planted sync-only policy observed one failed barrier, issued a real successful backing fsync without any rewrites and returned client FLUSH success. Production recovery on the image cut at completion returned **both old blocks**. The same external expected bytes rejected that apparent success. In the engine case, recovery of new values is necessary but insufficient: the trace must also contain the same two complete physical records at the original offsets, in order, before the successful backing barrier. Equality uses full bytes, not the printed digests. Each format chooses its own volume ID; compare digests within, not between, fixtures.

The loss is deterministic at the engine boundary. This is not a live ext4 writeback failure, a PostgreSQL workload, a test of descriptor-specific kernel error reporting or a power-cut result.
