# Live checkpoint B, Debian arm64 on Lima/VZ

Authored by LLM. On 2026-11-19 the live ublk client flushed known bytes, then issued a 4 KiB write at LBA 5 under space pressure. In each case pressure write index 114 triggered B (descriptor slot 0, LSN 129), which could release segments 2 through 7.

| Case | Descriptor write | Final fsync | Live pwrite | Freed / discards issued | Recovery from copy |
|---|---|---|---|---|---|
| Control | OK | OK | 4096 bytes | 6 / 4 | 8 of 8 matched |
| One-shot `write_aio`, sector 0 | EIO | Not reached | EIO | 0 / 0 | 8 of 8 matched |
| One-shot `flush_to_disk` | OK | EIO | EIO | 0 / 0 | 8 of 8 matched |

Both failure cases had one post-arm QEMU write attempt, the descriptor at byte 0. Each client received one `-EIO` write completion. All six victim digests and allocated-block maps remained unchanged after shutdown. The control changed victim contents; backlog can limit the number of discards issued before shutdown. Moving the rule outside the image made the expected-EIO check fail ([negative control](negative.txt)). No product defect demonstrated in these two fault cases.

- [Runner transcript](run.txt) and [full daemon/client/QEMU logs](traces.tar.gz), including the missed rule.
- [Environment and source fingerprints](environment.txt), [build checks](build.txt), [reproduction steps](../../tools/adr05-fault-lab/live/README.md).
- [Descriptor-write image](write.img.gz) and [final-fsync image](fsync.img.gz) are the originals before recovery, not the reopened copies. [SHA-256](originals.sha256) applies to the decompressed bytes.

This is reported backing `EIO`, not a forced crash image. Shutdown can flush pending data. The armed window covers foreground reclamation in a single serialized writer; the test doesn't cover every scheduling order, other errnos or ambiguous media writes. The caller history checks previously flushed data; the repeated pressure writes do not distinguish versions with identical bytes.
