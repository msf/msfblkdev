# Exact submission faults in the format-4 engine

Authored by LLM. Run on native Debian arm64 under OrbStack, 2026-11-19. The source snapshot is the commit in `provenance.txt` plus the five files identified there. No ublk device, FUSE export or raw disk was used. `TMPDIR=/var/tmp` held the private regular files, and compilation ran in a guest tmpfs.

The fault plan belongs to one `BackingIo` in a unit-test build. It matches the Nth instance of an io_uring opcode after installation. A rule can reject before submission, report an error after the real completion, or model a zero-byte transfer. For the last case the dispatcher submits a pointer-free NOP in place of the read or write and checks its zero-result CQE against the original byte count. It never shortens an in-flight buffer or alters the completion queue. The native path retains the existing exact-completion checks, ring ownership, buffers and accounting. The formatter and opener accept crate-private I/O factories, so the same dispatcher can run before a `Volume` exists. Release builds contain no fault plan.

In `pass.txt`, a no-hit format matched a clean format at a fixed volume ID, byte for byte. Five seam cases, ten existing accounting tests and three native ring tests passed. The short writev completed no bytes; accounting recorded an error and no completed write. The caller's prior flush was readable after reopen. Rustfmt and focused Clippy passed. The suite doesn't implement an in-memory device, simulate a partial-sector transfer, arm the cleaner's separate ring, measure dispatch performance or test physical power loss.

## Confirmed defect before repair

At the tested revision, `Volume::maintain` left the volume usable after any raw `ENOSPC`. Under ADR-08, the errno alone does not decide whether the engine can continue. The tests distinguish a rejection before I/O from a backing error during publication.

1. On a five-segment file a second client write cannot fit. It returns `ENOSPC` before the data write (rule hits: 0). The bytes don't change; a subsequent flush succeeds. This is correct.
2. On a twelve-segment file checkpoint A has a victim pending release. A pressure maintenance step begins B and writes its descriptor. The seam rejects the second fsync during this step with `ENOSPC`. The caller receives errno 28, the rule fired exactly once, and the active in-memory slot didn't advance. **The volume isn't latched and the next flush returns success.** This violates the released failure policy. It does not by itself demonstrate data loss or premature hole punching.

`before-fix.txt` records the failing assertion and status 101. The compressed fixture contains the image after the error, **before** the subsequent flush. Run the pre-fix repro explicitly from `rust/`:

```sh
cargo test --locked --lib backing_enospc_at_checkpoint_b_final_fsync_must_latch -- --ignored --nocapture
```

The repair must preserve the public errno and the nonfatal pre-I/O capacity refusal, but latch an uncertain backing write or sync. The [F01 repair report](../adr05-f01-repair-20261119/README.md) records the subsequent run of the same reproducer and its five-segment control.
