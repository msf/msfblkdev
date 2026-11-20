# Read both format-4 checkpoint graphs

Authored by LLM. `check.py` audits an **offline, immutable copy** of a small format-4 backing image. It doesn't load the engine or open a device. It reads each descriptor, follows its segment table and mapping indexes, and walks records from both append heads in global LSN order. It checks the metadata framing and checksums, then verifies the final mapped payloads using the volume ID, LBA and physical address. A newer root must not expose a segment as free if the older graph still needs it.

Use Python 3.8+ and the system XXH3 library (`libxxhash0` on Debian). If Python can't locate the library, set `XXHASH_LIB` to its full path. Run on the actual image, not its gzip archive:

```sh
python3 tools/adr05-fault-lab/checker/check.py /path/to/offline-copy.img
(cd tools/adr05-fault-lab/checker && python3 -B -m unittest -v)
```

Use a suite deadline and interpret the exit status before reporting a result:

| Exit | Meaning |
|---|---|
| 0 | Both roots passed the structural checks. |
| 1 | The checker found invalid input, a corrupt root or an allocation conflict. A damaged inactive root fails the audit even if the engine can reopen from the other root. |
| 2 | The checker could not finish. For example, the library was missing, the file was unreadable or the traversal exceeded its budget. This result does not establish that the image is corrupt. |

The checker accepts regular files up to 64 MiB and limits block visits and discard work to 262,144 units per root. It never allocates an array sized by logical volume capacity.

This is a structural check, not a complete validator of every format field. A record needs enough recognizable framing to join the replay chain. Destroying all evidence of a tail can leave a shorter, internally consistent image. A coherent stale image or a missing client write needs an external history to detect. A current allocation bit doesn't prove that an earlier incarnation was never reused. This audit doesn't establish durability, application consistency, safe future allocation, exact engine fallback behavior, or absence of damage to superseded payloads.
