# ADR-05: first pre-fix triage

Authored by LLM, 2026-11-19. Tag `adr05-triage-v1` preserves the framework, two red regressions and original failure images. This checkpoint is not an ADR closure or a claim that these are the only defects.

| ID and impact | Trigger / expected | Evidence and disposition |
|---|---|---|
| F01. Failed operation leaves writer online (safety contract) | Reject the second backing `fsync` during the checkpoint that frees a segment, after the descriptor write. The caller gets `ENOSPC`; the released volume must latch failed. A true pre-I/O capacity refusal must leave it usable. | [Failing assertion](../evidence/adr05-seam-20261119/before-fix.txt): one hit, `failed=false`, next flush succeeded. The separate [capacity control](../evidence/adr05-seam-20261119/pass.txt) measured zero hits, no backing change. `maintain` exempts by errno instead of its origin. Confirmed contract violation; unsafe continuation is possible, but loss of flushed bytes was **not** observed. Fix first. |
| F02. Valid closed volume refuses reopen (availability) | Issue ordinary writes, discards, flushes and growth across process restarts. After successful close the volume must reopen and match all 680 expected LBAs. | [Three seeds × two cache profiles](../evidence/adr05-traces-20261119/run-02.txt): two failures with one cache slot; four profiles passed. Both reduced failures returned `invalid metadata framing in replay tail`. The leading explanation is an intact older metadata record left by segment reuse. Confirm the classification in code before changing it. No injected error, no evidence of corrupted client data. |

## Repeat against the tag

On Linux with the normal Rust build prerequisites and disposable regular-file storage:

```sh
(cd rust && cargo test --locked --lib backing_enospc_at_checkpoint_b_final_fsync_must_latch -- --ignored --nocapture)
(cd rust && cargo build --locked --example adr05-traces)
python3 tools/adr05-fault-lab/traces/run.py rust/target/debug/examples/adr05-traces \
  "${TMPDIR:-/tmp}/adr05-replay-$$" --replay evidence/adr05-traces-20261119/run-02/seed-19-small/reduced/trace.json
```

## Other fault classes at this checkpoint

- **Passed in the indicated scope:** one-shot write/flush injector, real engine close-time descriptor write `EIO`, checkpoint fallback and two atomic descriptor-survival images, read isolation and overwrite after payload damage, no-side-effect format/open/short-write controls. The independent checker walked two roots of the retained engine image. Its intentionally broken fixtures and five logical-history violations validate the tests; they aren't engine defects.
- **Ongoing evaluation:** real ublk checkpoint-B descriptor-write/final-sync error gates, applying exact modeled payload/metadata cuts to production recovery, qualification after repairs. Do not infer a result for one fault (e.g. backing `ENOSPC`) from another (`EIO`).
- **Coverage gaps:** positive-length partial completions at each publication boundary, real hung/disappearing storage and cancellation, exhaustive prepared reuse/growth and cleaner schedules, every semantic decoder case, multi-ISA/native provider matrix, capture/replay/filesystem/database runs under operator control. The model can construct misdirected/stale/reordered bytes but does not by itself qualify the production engine for those cases.
- **Policy decisions:** keep the released failure latch. Online continuation, a new record chain, and accepting any durability assumption stronger than successful honest sync remain design decisions. See the [campaign](ADR05-CAMPAIGN.md) for the remaining work and closure criteria.
