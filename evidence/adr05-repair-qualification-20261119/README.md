# ADR-05 repair qualification, 2026-11-19

Authored by LLM. Tested commit `77fc16ef00b24b3affdb0521522713746211ffa1` (`adr05-repairs-v1`) on native Debian arm64, kernel `6.12.117+deb13-arm64`, Lima/VZ on macOS. The guest clone was clean before the first gate. No tracked file changed during the run. `make test-fault` created an untracked guest `probe-io` executable. The test user ran without sudo.

| Gate | Result |
|---|---|
| `make lint test` | PASS, 351 checks; 51 seconds including lint/build |
| `make test-dst` | PASS, medium/checker controls and both whole-descriptor outcomes |
| `make test-fault` | PASS, FUSE syscall probe and ordinary close-time descriptor EIO |
| Live checkpoint B | PASS, control plus descriptor-write EIO and final-fsync EIO through a real ublk client; all eight LBAs recovered in each case |
| `make test-acceptance` | PASS, 351 checks in crash-acceptance mode; 179 seconds including build |
| `make test-ublk-fio` | PASS, 24 scenario/repetition pairs; 99 seconds including build |

All commands exited zero on the first final-candidate attempt. The [driver](qualify.sh), [exit statuses and wall times](results.tsv) and per-gate text logs record the commands and outcomes. The repository's inner deadlines were unchanged. The shell used outer timeouts to bound build plus execution. The crash lab recorded its own 1800-second per-test and 3600-second suite limits. These are qualification gates on one host/guest row, not a test of every medium, architecture or thread schedule.

The repaired engine's live fault cases each recorded one failed client write, no post-fault victim write/free/discard, and intact victim byte and extent snapshots. [Full live traces](live-traces.tar.gz) include the QEMU and daemon logs. `new-images/` contains compressed, pre-recovery originals for those two cases and the ordinary close error. The [offline checker](image-audit.txt) accepted both roots in both live images. It [also accepted](pre-repair-image-audit.txt) the two hash-verified pre-repair live images. This checks the reachable metadata and payloads within its traversal budget. It does not establish persistence across physical power loss or prove that no historical reuse occurred.

Copied evidence matched [the guest's SHA-256 manifest](SHA256SUMS); decompressed fixture bytes matched their guest hashes. [Postflight](postflight.txt) found no ublk device or lingering lab process. After verification, [teardown](teardown.txt) stopped and removed `liveb` and its separate scratch disk from their private Lima home. No privileged medium or filesystem tier ran here.
