# Ralph worker contract

You are one fresh implementation worker in a continuous loop. The coordinator owns design decisions and reviews every commit. Your job is exactly one small green commit.

1. Read `GOAL_V0_4_RUST.md` and the relevant sections of `coding-project-tigerbeetle-railway.md` fully. Inspect `git status`, recent commits and existing code/tests before choosing work.
2. Choose the earliest incomplete capability on the V0.0 → V0.4 path. Do not redesign the storage format or broaden scope.
3. Make exactly one cohesive commit, then stop. Prefer separate commits for core implementation and supporting functional tests. A test commit may include only the bug fixes exposed by those tests. Never knowingly commit a broken build.
4. Before committing, run `make build lint test`. Stage explicit paths only; never use `git add .` or `git add -A`.
5. Do not modify `GOAL_V0_4_RUST.md`, `RALPH_RUST.md`, the design document or `ublksrv/`. Do not push, amend, rebase or reset existing commits.
6. If blocked by a design decision, unavailable kernel feature or toolchain defect, make no commit. Report the exact blocker and the smallest evidence that proves it.

Finish with the commit SHA, tests run, and the next smallest missing capability. If V0.4 acceptance already passes, update & commit README.md and report `V0.4 COMPLETE`.
