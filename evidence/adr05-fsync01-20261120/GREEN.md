# FSYNC-01: retained replay (2026-11-20)

Authored by LLM. In the [baseline run](BASELINE.md), the default engine did not falsely report success, but it stopped the volume after one transient error. Commit `519e850` implements an opt-in records-only retry. `067fd12` adds one line to the frozen test opener to enable it before the first successful client flush. The medium, fault, controls, recovery cut and expected bytes are unchanged.

From a clean Git checkout of `067fd12` on arm64 Linux, as a normal user:

```sh
export RUSTUP_TOOLCHAIN=1.97.1 CARGO_NET_OFFLINE=true
make test-fsync01
make lint
make test
```

All three commands exited zero. The [trace excerpt](green.txt) shows the two complete 8192-byte records at offsets 28672 and 36864. The test erased both, reported one barrier `EIO`, then observed both records rewritten in original order followed by a successful backing barrier. Production recovery on a separate copy at client completion returned both new blocks. The control's bare backing sync also succeeded, but both old blocks came back. The oracle checks full physical bytes and recovered logical blocks, not just the printed hashes. `make test-fsync01` explicitly runs the opt-in engine case; a default Rust run skips it.

On the same revision, `cd rust && cargo test --locked --lib records_retry -- --test-threads=1 --nocapture` passed six controls. They cover failure after a second barrier error, metadata invalidation, incomplete retention when enabled mid-epoch, heap and lock requirements, a failed barrier at the retention limit, and service after replay. Earlier `make test` runs on guest copies of linked worktrees stopped at a lab fixture that invokes Git (exit 128; the checkout lacked Git metadata). The complete developer run above used the same source revision in a scratch clone with Git metadata.

This result covers one terminal records-barrier error with deterministic byte loss. `Volume::enable_records_flush_retry()` does not enable retry in the daemon or in checkpoint publication. It requires an exclusively owned backing and an unaccounted ring. Relocation, metadata writes and unretained epochs still fail closed on a barrier error. Neither delay deadlines, caller cancellation, crashes during replay, real filesystem writeback nor a PostgreSQL workload were exercised here.
