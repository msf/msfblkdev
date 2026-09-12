# Ralph worker contract

The coordinator owns design decisions and reviews every commit. One fresh worker performs one bounded Rust delivery and stops.

Active specifications:

1. [ADR-01](ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md)
2. [ADR-02](ADR-02-GOAL-1-BASIC-READ-WRITE.md)
3. [ADR-03](ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md), completed regression baseline
4. [ADR-06 Delivery 1: V0.9 ext4 correctness](ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md#delivery-1-v09-ext4-correctness), completed delivery

## One-hour loop

1. **Orient, by minute 10.** Read the active acceptance section fully. Inspect `git status`, recent commits, existing code and relevant tests.
2. **Implement, until minute 40.** Choose the earliest worker-sized unchecked item. Add the smallest test that proves it and the minimum implementation required to pass it.
3. **Verify, until minute 55.** Run the focused test, then the complete Rust gate. Do not weaken, skip or ignore an earlier test.
4. **Handoff, by minute 60.** Make one cohesive green commit, check only the item proved by that commit, record evidence, report the next item and stop.

If the criterion cannot be completed safely in the time box, make no commit. Report the blocker, evidence and smallest next step. Do not start a second criterion.

## Rules

- ADR-02 and ADR-03 are closed. Work on the earliest worker-sized unchecked ADR-06 Delivery 1 item. SQLite, XFS, PostgreSQL, batching, format changes and crash testing are outside this loop.
- Evolve only the Rust implementation. Do not update Zig or restore image compatibility.
- Do not redesign the persistent format from a worker loop.
- ADR acceptance text is immutable to workers. A worker may only change `[ ]` to `[x]` for behavior proved by the same commit and add concise evidence.
- Prefer a failing test followed by its fix in the same local iteration. The committed state must be green.
- Stage explicit paths only. Never use `git add .` or `git add -A`.
- Do not push, amend, rebase or reset existing commits.
- Do not use `sudo`, load kernel modules, create or format LVM logical volumes, manipulate arbitrary block devices, mount filesystems, reboot the host or trigger a real out-of-memory condition.
- `SIGKILL` tests must target only child processes created by the test.
- Privileged ublk acceptance belongs to an explicitly approved operator run. Dedicated-LV validation is optional and never blocks Ralph.

## Required gate

Run from the repository root as the normal user:

```sh
make lint test
```

Use focused Cargo tests for the inner loop. `make check-ext4` checks operator prerequisites without creating a device or mount. `make test-ext4` is a separate, explicitly approved operator run; installation of the helper alone does not authorize a worker to run it.

Run any delivery-specific repetition count required by its ADR before checking that acceptance item.

## Handoff

Finish with:

```text
commit: <sha or none>
criterion: <exact ADR checkbox>
focused tests: <commands and result>
full gate: <commands and result>
next: <earliest unchecked criterion>
blocker: <none or exact blocker>
```

If code gates pass but live acceptance remains unverified, stop and report `AWAITING OPERATOR VALIDATION`. Do not attempt privileged tests without approval. When every Delivery 1 criterion is verified, report completion and stop. Starting a later delivery requires a new approved goal.
