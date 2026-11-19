# ADR-05 operator-tier approval checklist

Authored by LLM. Status: **NOT RUN**. This checklist maps [campaign E8](../../../docs/ADR05-CAMPAIGN.md) to [ADR-05 delivery 4](../../../ADR-05-GOAL-3-BACKING-MEDIUM-FAULT-RESILIENCE.md#delivery-4-operator-run-medium-faults) and the [ADR-06 operator boundary](../../../ADR-06-FILESYSTEM-AND-POSTGRESQL-CORRECTNESS.md#operator-and-test-boundary). It is not an operator procedure or permission to run privileged tests.

## Before approval

- [ ] **NOT RUN:** Record the operator, approval, date, source revision and local changes, binary hashes, format, guest image, kernel and tool versions.
- [ ] **NOT RUN:** Define a disposable Linux VM and owned loop-backed files. Record their identities, geometry, backing layers and mount state. Never target a system device or an existing user filesystem.
- [ ] **NOT RUN:** Review a separate procedure for identity checks, deadlines, child ownership, evidence retention and cleanup. Preserve resources if identity or cleanup state is ambiguous. Do not run privileged tests unattended.
- [ ] **NOT RUN:** Record the independent checker and negative-control results required by E8. Record the fault plan, expected effects, write budget and external client or transaction history before each run.

## Evidence to collect

| E8 case | Status | Criteria and data to record |
|---|---|---|
| Dropped writes and corruption | NOT RUN | Record the trigger and actual fault hits for payloads, record metadata, checkpoint descriptors and bodies. Separate violated durability assumptions from recovery bugs; retain the original failure image and observed result. |
| Backing write and flush capture | NOT RUN | Capture engine backing writes and flush/force-unit-access boundaries. Record the exact operation trace, fault, geometry, tool versions and log identity. Report backing-filesystem effects separately from raw or loop backing. |
| Replay and independent checking | NOT RUN | Replay selected outcomes into fresh owned images. Record selected boundaries, image hashes, recovery results and exact-content checks. Include the early-reuse negative control and its expected detection. |
| Filesystem workflows | NOT RUN | Select accepted ADR-06 filesystem workflows. Record filesystem, mount options, workload, sync points, offline checks and exact file contents against external history. Define safe device re-exposure and stale-mount handling before running. |
| Database workflows | NOT RUN | Select accepted ADR-06 database workflows. Record database versions and durability settings, transaction history outside the test device, committed results and logical and structural checks. Distinguish SQLite WAL/NORMAL from WAL/FULL. |

Save the image before recovery because recovery may write. Keep failed runs and their raw output even if a later repair succeeds. Replay covers selected crash states; it does not establish physical power-loss behavior. Existing ADR-06 clean-restart acceptance is not evidence for these fault cases.
