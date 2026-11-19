# F02: intact metadata after segment reuse

Authored by LLM. Tested on native Debian arm64 under OrbStack, from `adr05-triage-v1` plus `rust/src/format_v4/tail.rs` and `rust/src/tests/format_v4_protocol.rs`. No format or online policy change.

The retained seed-19 [original image](../adr05-traces-20261119/seed-19-original.img.gz) gave a direct check of the prior incarnation. The headers below all matched both the independent XXH3 header checksum and the full block checksum. Block numbering is physical, starting at zero. The User link at block 33 closed segment 0 at LSN 25. The link at block 129 returned to segment 0 at LSN 80.

| Physical block | Kind | LSN | Previous boundary | Link target |
|---|---|---:|---:|---|
| 33 | SegmentLink | 25 | 32 | User → segment 2 |
| 129 | SegmentLink | 80 | 128 | User → segment 0 |
| 2 | MappingPage | 81 | 129 | |
| 3 | MappingIndexPage | 82 | 2 | |
| 4 | SegmentTablePage | 83 | 3 | |
| 5 | MappingPage | 3 | 4 | |

The current User cursor follows block 4 at LSN 83. The complete mapping page at block 5 also names block 4 as its predecessor, but predates even the old closing link. A matching physical predecessor alone doesn't make this record a successor. The new probe requires a supported metadata record at its own physical address, valid framing, full block checksum and zero trailer padding, and an LSN strictly older than the head to recognize this case. It still looks for a valid data footer after these bytes. The normal path still checks the LSN and framing of current metadata, and still allows traversal of snapshot bodies as specified by ADR-08.

[Before](red.txt), production open on a copy of the retained image refused with `invalid metadata framing in replay tail`. The new focused protocol test failed in the same way on the intact older record. [After](traces.txt), the copy opened; both reduced workload files passed all four epochs. The complete three-seed, two-profile matrix passed all six runs, checking 680 logical blocks per epoch. Both planted fault controls still failed their caller-byte checks. The regression includes negative controls for a damaged header, damaged full-record checksum and intact but misplaced old record. [Rust checks](checks.txt) include those controls, existing torn-metadata handling, checkpoint recovery, stale-tail and reclamation tests. The last negative control was added after the trace run; the engine hash is unchanged between runs.

This establishes an availability repair for these clean-close workloads under the released checksum model. It does not establish safety after arbitrary torn writes, media misdirection or false flush acknowledgements.
