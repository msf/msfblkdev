# Goal: implement ADR-08 autonomously in isolation

Implement ADR-08 completely in Rust, including format 4, raw reclamation, the cleaner thread,
recovery, bounded memory, discard, backing growth, statistics, tests and basic performance validation.
Work autonomously. Do not ask the user questions or request approval between implementation slices.
Resolve ordinary design and implementation choices from the specification, code, experiments and your judgment.
Do not stop after a plan, a format skeleton, the serialized cleaner, or the first green commit.

The saved specification is commit `57d56ccbf57ce243a06fcdba49cdd5c73b846a58` in
`/home/miguel/play/msfblkdev`. That commit contains both ADRs, the technical review, its review prompt
and the proposed workbench. Treat it as the starting point, regardless of later commits in the source repo.

Another agent owns ongoing benchmark/workbench work in the source checkout. Do not interfere with it.
This task does not implement ADR-09 or the general workbench.

## Isolation and authorization

Use an independent clone with its own Git database, not a worktree sharing the source repo's index or refs.
The source repository is read-only for this task. Do not copy its uncommitted files or use its build outputs.

For a fresh run, create a private checkout as follows. These commands work in Bash and zsh:

```sh
mkdir -p /home/miguel/play/_worktrees
git clone --no-hardlinks --no-checkout /home/miguel/play/msfblkdev \
  /home/miguel/play/_worktrees/msfblkdev-adr08
git -C /home/miguel/play/_worktrees/msfblkdev-adr08 switch \
  -c implement/adr08 57d56ccbf57ce243a06fcdba49cdd5c73b846a58
git -C /home/miguel/play/_worktrees/msfblkdev-adr08 remote remove origin
cd /home/miguel/play/_worktrees/msfblkdev-adr08
export CARGO_TARGET_DIR="$PWD/rust/target"
```

If the destination exists, inspect it. Resume only if it is this task's independent clone and branch.
Otherwise choose a fresh sibling path. Never delete, overwrite or reset an existing checkout.
Do not fetch, merge or cherry-pick the other agent's evolving workbench work.

You may edit the private clone, add focused tests and small benchmark adapters, update ADR-08 to match
proved implementation decisions, and create local commits on `implement/adr08`.
Do not push, merge into the source repository, amend its history, or create a PR.
No co-author trailers. Stage explicit paths only. Preserve unrelated work everywhere.

Use task-owned build directories, evidence paths, regular-file images and child processes.
No shared workbench files, benchmark output directories, VMs, mounts, devices or global cleanup commands.
Do not kill another process or run heavy benchmarks alongside the other agent's measurements.
If resources are occupied, continue code review or other light work and defer performance runs.

For live acceptance, use an available task-owned test VM or an otherwise exclusive test environment.
Within it, this task authorizes the existing lab's disposable regular-file-backed ublk fixtures and
the already installed mount helper for those fixtures only. Do not change host configuration, install
privileged helpers, load host kernel modules, create LVM volumes, or format existing block devices.
Do not bypass sandbox approvals or unavailable permissions to satisfy the no-questions instruction.
If an environment is unavailable, complete all independent work and report the exact unverified gate.
Never turn an infrastructure limitation into a claim that the test passed.

## Read and establish the contract

Read the private clone's `AGENTS.md`, `README.md`, `RALPH.md`, ADR-08 and
`docs/REVIEW-ADR-08.md` fully. Read ADR-09 only to establish the excluded compression boundary.
Read the relevant ADR-01/03/04/07 contracts and `ADR-07-PERF-RESULTS.md`.
Read `TMD-WORKBENCH.md` as background; do not implement its general framework.
Read `ADR-10-WORKBENCH-TESTS-AND-REPORTS.md` as a candidate system or tool suite to use or adapt for testing/benchmarking.
At this pinned revision, `RALPH.md` and `SQLITE-EXT4.md` are at the repository root.
Do not follow their uncommitted moves in the source checkout.

Load these skills before implementation or document edits:

- `/home/miguel/.agents/skills/coding/SKILL.md`
- `/home/miguel/.agents/skills/rust-development/SKILL.md`
- `/home/miguel/.agents/skills/tech-writing/SKILL.md`
- `/home/miguel/.agents/skills/simplified-technical-english/SKILL.md`

This is the active authorization for all ADR-08 deliveries. It supersedes the historical Ralph rules
that exclude format changes, make ADR text immutable, or require fresh user direction after each delivery.
Keep the useful Ralph discipline: bounded slices, focused tests, review, green commits and durable handoffs.
You act as coordinator; choose the next slice yourself. A one-hour boundary is a handoff, not task completion.
If a goal tool is available, create a goal for this objective without inventing a token or time budget.
Follow the harness's status and continuation rules. Mark complete only when the required work is verified.

The fixed design is:

- Two fixed heads, User and Relocation; one writer owns mutable engine state and all log appends.
  The cleaner prepares raw records. The writer validates, filters stale members, finalizes addresses,
  headers and checksums, writes through the relocation head, then publishes mappings.
  It does not sort, merge proposals, fill rejected slots or copy payload bytes into another arena.
- Format 4 uses the direct/indirect mapping index, two checkpoint roots and the two-publication freeing
  barrier. Use `checkpoint_lsn: u64` and `seal_lsn: u64`; no separate checkpoint-generation counter.
  Every new publication must advance the log. An unchanged checkpoint can be skipped.
- Repeated metadata uses parallel field arrays in memory and on disk. Segment tables hold 448 entries:
  `u64` seal LSNs and one flags byte each. State occupies bits 0–2, head occupies bit 7, and bits 3–6
  must be zero. Reject unknown states; the head bit is zero outside Open. Preserve the specified offsets.
- The complete heap budget is proportional to logical volume size and divided among write buffers,
  eviction/cleaner buffers, the mapping cache and supporting state. Preallocate the operating storage.
  Raw reclamation must work without compression. Do not add codecs, compressed records, a decoded cache,
  logical volume growth, dynamic heads or an automatic backing-growth controller.

The ADR remains a draft with known correctness gaps. Do not implement contradictory prose literally or
assume a review finding was solved because field names changed. Resolve applicable findings before enabling
the affected path. Give each a short disposition linked to the test and final protocol in ADR-08.
Keep historical review evidence intact; record current dispositions separately.

In particular, resolve source-address reuse/ABA; abandoned allocations with stale reissued LSNs;
bootstrap and segment-link framing; reservation including link/tail/metadata costs; both roots' replay
dependencies; fallback after reuse; dirty metadata relocation; buffered write/discard ordering;
candidate-index preallocation; and whole-operation memory bounds.
Checksumming old bytes is not proof that they are still the current source.

Choose the smallest correct protocol consistent with the fixed decisions. Write a failing example or
invariant argument before a consequential change, implement it, and test the crash boundaries it introduces.
You may resolve missing details and correct erroneous draft offsets or arithmetic, but must document why.
Do not silently add another logical counter or weaken durability, memory or progress requirements.

## Ralph execution loop

Create `GOAL-ADR08.md` in the private clone with a short checklist, decisions, evidence paths and next step.
Commit this implementation record with the work it describes. Use these four phases to organize the task;
split them into smaller commits when needed:

1. Establish a green baseline and resolve the format/recovery/reservation contracts. Add codecs, the
   segment allocator, mapping-index publication and a small accounting model. Close the relevant review gaps.
2. Implement serialized raw relocation and complete reuse. Prove repeated wraparound, recovery, discard,
   backing growth and bounded allocation before moving work to a background thread.
3. Add the cleaner thread, bounded proposal storage, buffer ownership and writer scheduling. Prove idle,
   sustained-load and space-pressure progress, plus shutdown and source lifetime safety.
4. Run the full regression/crash gates, live frontend acceptance and basic performance comparisons.
   Update the ADR and implementation record, review the complete branch, and leave a clean local commit stack.

Within each approximately one-hour slice: orient, add a focused failing test, implement, run the focused
test and required regression gate, inspect the diff, then make one cohesive green commit.
Aim for reviewable commits rather than one massive implementation commit. Do not split only for line count.
If the slice is not green, preserve its work and a precise handoff; do not commit a broken state or reset it.
Resume the earliest incomplete item in the next slice without asking the user.

Keep `GOAL-ADR08.md` current across restarts and context compaction. Record the last green commit, dirty
work, exact commands, results, known failures and next action. Do not repeatedly rerun passed suites unless
new changes or unresolved failures justify it. Do not use a time box as permission to skip an acceptance gate.

## Correctness and progress tests

First inspect the pinned Makefile and lab implementation. Run `make lint test` before changes and save
the output. Run `make test-acceptance` to establish the inherited crash baseline.
Use focused Cargo tests while editing; run `make lint test` before each implementation commit.
Run the full acceptance matrix at protocol milestones and against the final candidate.
Retain existing repetition counts, including the inherited 20 fresh repetitions for process-crash cases.
Keep tests bounded; add separate acceptance groups for slow new matrices rather than removing old coverage.

The following coverage is required:

| Area | Required evidence |
|---|---|
| Encoding | Exact sizes/offsets, little-endian round trips, zero padding, reserved flags, invalid states, bounds and checksums. Cover first/last entries and partial pages. Reject unsupported formats before writes. Preserve format-3 rejection/compatibility behavior as specified; do not invent migration. |
| Logical behavior | A deterministic reference map for seeded write/read/overwrite/discard/write-zeroes/flush/close/reopen sequences. Check boundary LBAs and record/segment transitions. Verify both pending-buffer and persisted visibility. Old-format tests remain meaningful; adapt format fixtures explicitly without deleting behavioral assertions. |
| Recovery | Fail at record submission/completion, mapping publication, each checkpoint metadata/barrier/descriptor step, links on both heads, growth and stale-tail sanitation. Test torn/short/failed I/O and corruption using the existing fault seams or a focused deterministic storage model. Test restart during recovery and fallback after reuse. Process SIGKILL alone does not simulate power loss. |
| Durability oracle | Every successfully flushed/FUA-covered value must survive. Unflushed outcomes must form an allowed record prefix. Reads never return stale rejected copies or unchecked corruption. Compare recovered values, allocation state, cursors and counters with the oracle. Failures that require latching must prevent further engine operations. |
| Reclamation | Write multiple times the backing capacity and prove actual victim reuse. Verify no reuse while either root, a pending proposal or an active reader still needs the bytes. Exercise an entirely dead victim, a nearly full victim, stale proposals, source reuse, and both heads crossing segments. Positive-gain cleaning must progress with its proved reserve; all-live exhaustion may correctly return ENOSPC. |
| Memory | Instrument allocations after initialization. Exercise cache pressure, checkpointing, recovery, full proposal queues, retained views and shutdown. Account for all heap allocations, including indexes and metadata. Test a small volume around 1 GiB with a deliberately small valid proportional budget, plus larger geometry. A 12 MiB total is a useful test candidate, not a required budget for every volume. Reject insufficient configurations before mutation. |
| Scheduling | Use a deterministic clock/event seam for policy tests and actual io_uring/ublk integration for wakeups. Idle wait and maximum maintenance interval are distinct. Continuous requests cannot postpone required maintenance indefinitely. A full payload queue stops copying, not control events or release progress. Empty proposal queues cannot prevent the two freeing checkpoints. Exercise no-cleaner-work, pressure, delayed I/O and cancellation without busy-spinning. |
| Discard and growth | Test write/discard/write races with buffered values and queued relocation. Physical discard requires durable release and exclusion from allocation while outstanding. Test unsupported discard and failure policy. Verify recorded backing growth, retries, unrecorded extensions, partial trailing segments and rejection beyond preallocated state capacity. |

For proposed io_uring integration, inspect pinned `libublk` rather than assuming a callback API exists.
The current engine blocks waiting for backing I/O completion. State and test the actual scheduling bound:
maintenance runs at a safe boundary after its deadline, not necessarily within a fixed wall-clock delay.
Keep SQEs, iovecs, headers and payload storage alive until the corresponding I/O completes.

In the exclusive live environment, run `make test-ublk-fio`, `make test-ext4` and `make test-xfs`.
Run the existing SQLite/ext4 and SQLite/XFS acceptance matrices after the final storage implementation
stabilizes, using the committed lab targets. Build pinned optional tools only inside this task's checkout.
Add a sustained overwrite case that actually wraps format 4; old short lifecycle tests alone cannot prove
reclamation. Check exact contents after close/reopen and owned-child crash/restart.
Run preflight first and preserve failed fixtures for diagnosis. Clean up only resources created by this run.
If any required live gate is unavailable, report that gate as unverified and do not claim full completion.

## Basic performance validation without the new workbench

Do not wait for or import the other agent's workbench. Use the committed direct-engine examples and
`tools/mapping-bench/` as references. Inspect their assumptions before reuse: some drivers select old tags
or expect the format-3 API. A small private adapter or ADR-08 smoke example is sufficient.
Do not build another general trace framework, workload catalogue, VM manager or benchmark suite.

Build release binaries for the pinned baseline and candidate in separate private directories.
Use fresh disposable regular-file backing, identical logical/backing sizes, seeds, request sequences,
user-buffer sizes and mapping-cache capacities. Report total memory separately because format 4 adds state.
Disable per-request debug logging. Keep checksums and durability enabled. Record backing filesystem,
O_DIRECT status, kernel, CPU, source/binary hashes and whether the environment was exclusive.

Run these four comparisons:

1. **Before cleaning is needed:** sequential 4 KiB reads/writes, random 4 KiB reads/overwrites, hot-cache
   and cache-miss reads, and writes with fixed periodic flushes. Use a workload that fits the format-3
   log. Compare baseline and candidate throughput, P50/P99 latency, bytes read/written and metadata/fsync counts.
2. **Aged raw reclamation:** candidate-only uniform and skewed overwrites, then mixed reads/writes.
   Start at a defined live fill factor such as 0.8. At 128 MiB and 4 GiB logical sizes, write at least
   three backing capacities and verify actual segment reuse. Continue within a declared deadline until
   two equal measurement windows agree within 5% on write amplification, or report that steady state
   was not reached. Compare fresh versus aged candidate results. Format 3 cannot supply an aged-reuse baseline.
3. **Progress and latency:** alternate request bursts and idle gaps, then run sustained traffic near
   the low watermark. Measure maintenance lateness, proposal queue occupancy, time waiting for buffers,
   relocation-to-reusable delay, free-segment minimum and foreground P99. Verify checkpoint/free progress,
   not only that the cleaner copied bytes. Explain observed synchronous-I/O stalls.
4. **Memory and restart:** measure total heap and its configured shares, peak retained proposal/view
   bytes, recovery time and bytes read, checkpoint/close time and zero unexpected heap growth after init.
   Report process RSS and kernel/ring resources separately. Include the small-volume configuration.

Use at least three interleaved baseline/candidate repetitions for comparable cases; report median and
range, not the best run. Declare workload sizes, warmup, sample counts and deadlines before measurement.
Inspect host load before and during a run. A contaminated run is not acceptance evidence.
Keep correctness checks outside timed loops where possible, and verify final contents after each run.

As initial investigation triggers, treat a reproducible throughput loss above 10% or P99 increase above
20% in comparable pre-cleaning workloads as a regression to diagnose. These are not measured device limits
or a license to waive a failure. Fix avoidable regressions; document required protocol costs with evidence.
Do not obtain a passing result by silently changing the workload, cache budget, flush policy or thresholds.
Require bounded space/memory and correctness regardless of performance results.

Keep a compact JSON/CSV result file and a readable report under task-owned
`evidence/adr08-implementation/`. Include exact reproduction commands and distinguish observations,
calculated values and unverified assumptions. Commit concise reports/manifests as appropriate; never images,
large generated traces, build trees or temporary benchmark data.

## Completion and handoff

Before completion, inspect the complete diff from the pinned base. Reconcile implementation with ADR-08,
remove stale field names and incomplete stubs, check the allocation inventory, and account for every
applicable review finding. Keep ADR-09 deferred. Preserve its raw-format rejection boundary.

The deliverable is a clean, isolated branch with reviewed local commits, executable tests, the updated
ADR-08, `GOAL-ADR08.md`, and reproducible correctness/performance evidence. No push or integration is requested.
Tag tip of branch with "v2.0-rc".

Report at most four items:

1. Clone path, branch and final commit range from `57d56cc`.
2. Implemented behavior and consequential decisions that resolved draft gaps.
3. Exact correctness gates and basic performance results, with evidence paths.
4. Remaining failures or unavailable gates, or verified completion.

Do not ask what to do next. Continue until the objective is verified or a real external blocker prevents
further useful work. Follow the harness's blocked-status rules if applicable. Never label partial work complete.
