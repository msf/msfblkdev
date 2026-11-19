# F01: distinguish capacity from a failed maintenance operation

Authored by LLM. `tag-red.txt` re-runs the capacity control and failing checkpoint-B test from `adr05-triage-v1` (`292040d`). `repair.txt` runs the same test body after the repair. Both runs used regular files on the same native Linux arm64 guest; neither used a real full disk or ublk mount.

| Case | Before | After |
|---|---|---|
| B's final backing fsync reports `ENOSPC` | One hit. `maintain` returned `ENOSPC`, `is_failed` was false, next flush succeeded. Exit 101. | One hit. `maintain` still returned `ENOSPC`; `is_failed` was true, next flush failed without writing. |
| Client writes into a full logical layout | Zero backing write hits; `ENOSPC`, unchanged image, next flush passed. | Same result. |

Only a *publication preflight* that says there isn't enough room gets the nonfatal exemption during maintenance. Failures while doing the work, including `ENOSPC`, now use the existing volume failure latch. Foreground pressure that tries to release pinned metadata uses the same classification. The ublk adapter preserves the errno, marks a latched request fatal and no longer suppresses a latched maintenance error in the scheduler.

All six seam cases, sixteen reclamation tests and 37 ublk binary tests passed, as did `cargo fmt --check` and Clippy with `--all-targets --features lab -- -D warnings`. The ublk test for fatal `ENOSPC` uses an accounting budget to surface the latched errno at queue dispatch, not an actual kernel client. This change doesn't define how to continue after an uncertain write and adds no physical power-loss guarantee.
