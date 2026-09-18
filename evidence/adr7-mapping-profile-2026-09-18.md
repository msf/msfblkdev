# ADR-07 mapping memory and access measurements

Measured on 2026-09-18 against engine commit `41b01a1`, with the accompanying
`rust/examples/mapping_cache_profile.rs`. Linux 7.0.0-31-generic, x86_64,
Rust 1.97.1, release build, default allocator. No new dependencies.

## Reproduce

```sh
CARGO_TARGET_DIR=/tmp/msfblkdev-adr7-target cargo build --release \
  --manifest-path rust/Cargo.toml --example mapping_cache_profile
for profile_case in 4gib 400gib 4tib accesses; do
  /tmp/msfblkdev-adr7-target/release/examples/mapping_cache_profile "$profile_case"
done
```

Each invocation creates and removes its own 128 MiB sparse regular-file backing.
The logical geometry can exceed that backing. No device or filesystem mount is used.
Each memory case runs in a fresh process, formats the backing, and samples RSS before
and after open. It then writes one payload and closes, publishing a checkpoint, before
sampling RSS again. Formatting precedes the baseline and can leave allocator-retained memory.

## Allocated mapping storage

All sizes below are bytes. Cache sizes come from the public statistics API, including
the pinned hash table's actual bucket/control-byte allocation size. Slot bytes include
native padding. Index bytes are the unpadded in-memory pointer array.

| Logical size | Requested cache | Effective cache | Slots | Slot bytes | Lookup bytes | CLOCK bytes | Index bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 GiB | 429,496 | 429,464 | 111 | 427,128 | 2,320 | 16 | 13,108 |
| 400 GiB | 42,949,672 | 42,947,552 | 11,084 | 42,651,232 | 294,928 | 1,392 | 1,310,720 |
| 4 TiB | 439,804,651 | 439,802,624 | 113,677 | 437,429,096 | 2,359,312 | 14,216 | 13,421,776 |

| Logical size | Steady mapping heap | Checkpoint/recovery mapping heap bound |
| --- | ---: | ---: |
| 4 GiB | 442,572 | 455,680 |
| 400 GiB | 44,258,272 | 45,568,992 |
| 4 TiB | 453,224,400 | 466,646,176 |

Steady storage is one cache plus one index. The calculated peak bound adds one candidate
index. Publication clones the active index once. Bounded recovery retains active and
candidate indexes and drops each window's cache before constructing its replacement.
The recovery tests establish this lifecycle; these memory probes execute clean open and
publication, not large-tail recovery. The bound is calculated from measured allocation
sizes, not sampled at the instant of peak allocation.

These totals exclude allocator bookkeeping/rounding, inline objects, fixed stack and I/O
buffers, the payload write buffer, its lookup, and the I/O ring. `size_of::<Volume>()`
is 12,288 bytes in this build. The totals bound dynamic mapping storage, not total engine
memory or process RSS.

## Process RSS

RSS is sampled from `/proc/self/smaps_rollup` (`Rss`, converted from KiB to bytes).
These are three snapshots, not a measured process peak or a guaranteed RSS limit.
The kernel documents that the faster `/proc/self/status` RSS accounting can be imprecise;
see [the proc filesystem documentation](https://docs.kernel.org/filesystems/proc.html).

| Logical size | After format, before open | While open | After write and close |
| --- | ---: | ---: | ---: |
| 4 GiB | 2,723,840 | 3,280,896 | 2,850,816 |
| 400 GiB | 2,703,360 | 48,148,480 | 5,505,024 |
| 4 TiB | 2,707,456 | 467,513,344 | 30,089,216 |

RSS includes resident code, libraries, allocator-retained memory, stacks, and other
engine allocations. It need not equal live mapping allocations or return to baseline
after close. These values describe this run only.

## Exact cache access sequences

The fixture has 16 mapping pages and a 16,000-byte cache budget, which produces four
slots. Setup writes the first LBA of every page and closes before measurement. Each
workload opens with an empty mapping cache, performs 4,096 reads, and checks every payload.
Each miss loads an existing snapshot. Snapshot reads exclude checkpoint validation during
open. The probe asserts exact hits, misses, snapshot reads, and zero snapshot writes.

For access number `i` from 0 through 4095:

| Sequence | LBA | Hits | Misses / snapshot reads | Hit rate | Clean evictions |
| --- | --- | ---: | ---: | ---: | ---: |
| Sequential | `i` | 4,083 | 13 | 0.996826172 | 9 |
| Hot | `(i % 3) * 320` | 4,093 | 3 | 0.999267578 | 0 |
| Scattered | `((i * 5) % 16) * 320` | 0 | 4,096 | 0 | 4,092 |
| Moving hot | `((i / 1024) * 3 + i % 3) * 320` | 4,084 | 12 | 0.997070312 | 8 |

Division is integer division. Scattered accesses cycle through all 16 pages; they are
deterministic, not random. Moving hot accesses use four disjoint three-page working sets.
These sequences prove lookup accounting under known locality. They do not predict a real
workload's hit rate or sustainable device throughput.
