# Prompt: review ADR-08 on two dimensions

Review `ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md` and its deferred compression extension,
`ADR-09-RELOCATION-COMPRESSION.md`. Write your review to `docs/REVIEW-ADR-08.md`. Do not edit the
ADRs or implement anything.

You are reviewing a design for a Rust log-structured ublk block device. ADR-08 proposes format 4: a
segmented log, raw relocation, a cleaner thread, discard, the mapping index in the log and backing
growth. ADR-09 defers compression to a separate delivery. Review each document's clarity and the
boundary between them.

## Read first, then review

Read these completely before you write anything:

1. `README.md`, `AGENTS.md`.
2. `ADR-01-LOG-STRUCTURED-BLOCK-DEVICE.md`, `ADR-03-GOAL-MINIMUM-CREDIBLE-DEVICE.md`,
   `ADR-04-GROWABLE-THIN-PROVISIONED-FORMAT.md`, `ADR-07-PAGED-MAPPING-AND-CACHE.md`. These define
   format 3, the vocabulary and the ADR form this project uses.
3. `ADR-08-SEGMENTED-LOG-AND-SPACE-RECLAMATION.md` and `ADR-09-RELOCATION-COMPRESSION.md`, the
   documents under review.
4. `TMD-WORKBENCH.md`, the measurement plan the ADR depends on.
5. `papers/README.md`, then the PDFs named below. Read the sections you cite. Do not cite a section
   you did not read. If you rely on recollection of a paper, say so.
6. `rust/src/mapping_cache.rs`, `rust/src/format_v3/checkpoint.rs`,
   `rust/src/format_v3/checkpoint_publish.rs`, `rust/src/format_v3/recovery.rs`, to know what exists
   today and what the code calls things.

Load the `tech-writing` and `simplified-technical-english` skills before you write. The review
itself must follow them: short sentences, one idea each, no em-dashes, active voice, findings with
line numbers.

## Dimension 1: structure, style and density of the prose

Compare the ADR against how the storage papers of the 1990s and 2000s in `papers/` present a design
of similar or larger scope:

- `1992-tocs-log-structured-file-system.pdf` (Rosenblum and Ousterhout). A whole filesystem, its
  cleaner, its cleaning policy and its measurements.
- `1993-logical-disk-technical-report.pdf` (de Jonge, Kaashoek, Hsieh).
- `1995-usenix-file-system-logging-versus-clustering.pdf` (Seltzer et al.).
- `2010-bitcask.pdf` (Sheehy and Smith). Six pages for a complete storage engine.

Answer these questions with evidence from both the ADR and the papers:

1. **Density.** Words per design decision. Count the ADR's distinct decisions and its words, then do
   the same for LFS §3 (the design and cleaning sections). Where does the ADR spend words that LFS
   does not? Where does LFS spend words that the ADR skips (for example, measured evidence, worked
   examples, figures)?
2. **Structure.** LFS and Logical Disk go problem, design, policy, evaluation. Map the ADR's
   sections onto that order. Name the sections that are out of order, that repeat another section,
   or that belong in a different document (the workbench memo, a delivery plan, ADR-04's revision).
3. **Register.** Those papers state a mechanism once, in one place, and refer back to it. Find every
   place where the ADR states the same rule twice. Quote both.
4. **Evidence versus argument.** The papers show a figure or a table where the ADR writes a
   paragraph. List paragraphs that should be a table, a diagram or a formula, and paragraphs of
   argument that a measurement would replace.
5. **Prior-art citations.** The ADR cites a paper section in most paragraphs. Judge whether that
   density helps or hides the design. Say which citations carry a decision and which are decoration.
6. **Vocabulary.** Check that each term in the ADR's Terms table is used with one meaning and one
   spelling throughout, and that every struct field name is a noun with its object attached (not
   `indirect` or `Provision`). List violations with line numbers.
7. **Verdict.** State whether the ADR is too long, and by how much. Give a target word count and
   name the sections you would cut, merge or move, in a table: section, action, destination, words
   saved.

Do not rewrite sections in the review. Quote at most two sentences per finding.

## Dimension 2: technical review and decomposition

Review the design for correctness first, then propose how to split it.

### Correctness and gaps

Check every one of these and report a finding, or state that you checked it and found it sound:

1. The five invariants in "What must reclamation preserve?" against every mechanism in the Design
   section. Name any mechanism that can violate one.
2. The two-checkpoint freeing barrier. Walk crashes between publications A and B, and after reuse
   with fallback to the older root. Check torn links, segment tables and mapping index pages. Check
   durable backing growth followed by an incomplete checkpoint. Verify root ordering by checkpoint
   LSN, including publications without user writes.
3. Two-head replay. Can the merged LSN stream have a gap that recovery misreads as the end of the
   log? Consider a segment link on one head while the other head's next record is still in flight.
4. The "no segment header or reuse generation" claim. Find a sequence of writes, truncations and LSN
   reissues after which an earlier-use record carries an LSN that replay would accept. If none
   exists, say why.
5. The cleaner's validate-at-apply rule with compressed chunks. A chunk of 16 payloads where 3 fail
   validation: what does the writer append, and what does the footer say?
6. Live-byte accounting when a payload sits inside a live chunk, when a chunk is re-emitted
   unchanged, and when a discard hits a compressed payload.
7. The capacity math: the progress floor formula, the 128 MiB table, the 82.7 GiB and 10.2 TiB index
   figures, the 416,640-segment table limit. Recompute each. Report any number that does not follow
   from the stated constants.
8. The read path for a compressed payload: number of backing reads, checksum verifications and their
   order. Can a corrupted footer send the reader to decompress arbitrary bytes before any checksum
   fails?
9. Admission reservation `N + R + D + P` under segments: does it account for a record that does not
   fit and forces a segment link plus a new segment?
10. The apply-step wake-up when idle, with libublk. State what you know and what is unverified.
11. Anything the ADR claims a paper says that the paper does not say. Quote the ADR and the paper.

Rank findings by severity: data loss or corruption, then availability (`ENOSPC`, stalls), then
performance, then clarity.

### Decomposition

The current split is raw reclamation in ADR-08 and deferred compression in ADR-09. Evaluate that
boundary and the delivery phases.

1. Draw the dependency graph of the changes as a Mermaid `flowchart`. Nodes: segments and segment
   links, segment table pages, mapping index in the log, two heads and replay, `BackingGrowth`,
   `LbaDiscard`, `CompressedData` record and read path, live-byte accounting, serialized cleaner and
   freeing barrier, cleaner thread and proposals, compression at relocation, selection policy,
   growth policy, `SpaceUsage` metrics. Edges: "cannot be built or tested before".
2. Evaluate the current two-ADR split. For each, state its decision, format compatibility, required
   content and excluded content. Compare it with splitting format layout from reclamation policy.
   Recommend a different boundary only if the dependency graph supports it.
3. Check the ADR's six deliveries against your graph. Name any delivery that depends on a later one,
   any delivery too large to have one crash matrix, and any change with no delivery.
4. Propose the phases as a numbered list. Each phase names: what ships, what a user can observe, the
   crash matrix it adds, the measurement that gates the next phase, and what can be rolled back.
   Keep a phase small enough that its diff can be reviewed in one sitting.
5. Name what you would defer past both ADRs, and why.

## Output

Write `docs/REVIEW-ADR-08.md` with this shape:

```
# Review of ADR-08

Date, reviewer, ADR revision reviewed (git hash or file mtime)

## Summary            three to six sentences: the verdict on length, the top technical risk, the recommended split
## Prose findings     numbered, each: line range, finding, evidence, recommendation
## Prose verdict      the cut/merge/move table and target word count
## Technical findings numbered, ranked by severity, each: line range, finding, failure scenario, recommendation
## Checked and sound  the items from the correctness list that passed, one line each
## Dependency graph   the Mermaid flowchart
## Proposed split     the two-ADR table
## Proposed phases    the numbered phase list
## Deferred           what waits, and why
## Not reviewed       anything you skipped, and why
```

Rules for the review:

- Every finding cites a line range in the ADR. Every paper claim cites a section you read.
- Mark each claim as measured, computed, read in a paper, or your judgment.
- Preserve decided choices: two fixed heads, mixed record kinds, raw reclamation before compression,
  and the direct/indirect mapping index. No codec is selected. Identify correctness defects
  directly; label other proposed changes as opinion.
- Do not pad. A section with nothing to say gets one line saying so.
