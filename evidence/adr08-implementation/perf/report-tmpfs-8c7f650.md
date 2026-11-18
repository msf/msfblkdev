# ADR-08 performance comparisons

Baseline `57d56cc` (format 3) against candidate `8c7f650` (format 4). Kernel `7.0.0-31-generic`, CPU 0, 3 interleaved repetitions, `rustc 1.97.1 (8bab26f4f 2026-07-14)`, fixture filesystem `tmpfs tmpfs rw,nosuid,nodev,size=32025876k,nr_inodes=1048576,inode64,usrquota` (chattr +C unavailable: chattr: Operation not supported while setting flags on /tmp/claude-1000/-home-miguel-play-msfblkdev/92e83b6e-6920-4d1b-bd8e-3d3f914510b0/scratchpad/bench/.adr08-bench-kbx1d5j5). Values are medians over successful repetitions; `runs` counts successful/total.

## 1. Fresh volume, nothing to reclaim

| job | version | runs | ops/s | op p50 | op p99 | op p99.9 | flush p99 | RSS |
|---|---|---|---|---|---|---|---|---|
| rand-read | baseline-57d56cc-format3 | 3/3 | 463,346 | 2 µs | 3 µs | 7 µs | 0.00 ms | 7.5 MiB |
| rand-read | candidate-adr08-format4 | 3/3 | 468,504 | 2 µs | 3 µs | 7 µs | 0.00 ms | 10.9 MiB |
| rand-write | baseline-57d56cc-format3 | 3/3 | 989,274 | 0 µs | 0 µs | 1 µs | 0.26 ms | 7.5 MiB |
| rand-write | candidate-adr08-format4 | 3/3 | 1,011,842 | 0 µs | 0 µs | 1 µs | 0.39 ms | 10.8 MiB |
| seq-read | baseline-57d56cc-format3 | 3/3 | 467,901 | 2 µs | 3 µs | 6 µs | 0.00 ms | 7.5 MiB |
| seq-read | candidate-adr08-format4 | 3/3 | 479,940 | 2 µs | 2 µs | 6 µs | 0.00 ms | 10.9 MiB |
| seq-write | baseline-57d56cc-format3 | 3/3 | 1,054,644 | 0 µs | 0 µs | 1 µs | 0.33 ms | 7.5 MiB |
| seq-write | candidate-adr08-format4 | 3/3 | 1,054,657 | 0 µs | 0 µs | 1 µs | 0.28 ms | 10.8 MiB |

## 2. Aged overwrite for twice the log capacity

| job | version | runs | completed | ENOSPC | ops/s | op p99 | op p99.9 | op max | copies | freed | pressure steps | reopen |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct | baseline-57d56cc-format3 | 3/3 | 49,005 | 16 | 1,101,248 | 0 µs | 1 µs | 0.00 ms | — | — | — | 0.74 ms |
| 128mib-50pct | candidate-adr08-format4 | 3/3 | 131,072 | 0 | 574,991 | 0 µs | 1 µs | 29.76 ms | 11,823 | 52 | 976 | 2.46 ms |
| 128mib-80pct | baseline-57d56cc-format3 | 3/3 | 24,383 | 16 | 1,041,127 | 0 µs | 0 µs | 0.00 ms | — | — | — | 0.92 ms |
| 128mib-80pct | candidate-adr08-format4 | 3/3 | 81,920 | 0 | 333,229 | 0 µs | 1 µs | 20.09 ms | 45,942 | 59 | 1,062 | 3.45 ms |
| 4gib-50pct | baseline-57d56cc-format3 | 3/3 | 246,903 | 16 | 722,413 | 1 µs | 1 µs | 0.02 ms | — | — | — | 3.86 ms |
| 4gib-50pct | candidate-adr08-format4 | 3/3 | 1,048,576 | 0 | 184,098 | 1 µs | 2 µs | 40.19 ms | 558,169 | 890 | 16,127 | 8.08 ms |

## 3. Progress and latency: scheduled ticks against inline pressure only

| job | version | runs | completed | ops/s | op p99 | op p99.9 | op max | ticks | tick p99 | pressure steps | pressure waits | copies |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct-inline | candidate-adr08-format4 | 3/3 | 131,072 | 576,962 | 0 µs | 1 µs | 29.80 ms | 0 | 0 µs | 976 | 451 | 11,823 |
| 128mib-50pct-ticks | candidate-adr08-format4 | 3/3 | 131,072 | 562,206 | 0 µs | 1 µs | 31.16 ms | 2 | 1 µs | 977 | 452 | 11,823 |

## 4. Memory and restart

| job | version | runs | open | RSS after open | RSS after writes | reopen | RSS after reopen | heap total | heap allocated | cache | index |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-default | baseline-57d56cc-format3 | 3/3 | 1.89 ms | 7.2 MiB | 7.4 MiB | 2.11 ms | 7.4 MiB | — | — | 389 KiB | 0 KiB |
| 128mib-default | candidate-adr08-format4 | 3/3 | 4.50 ms | 10.5 MiB | 10.7 MiB | 4.01 ms | 10.9 MiB | 16.0 MiB | 7.3 MiB | 389 KiB | 0 KiB |
| 1gib-12mib | baseline-57d56cc-format3 | 3/3 | 2.25 ms | 9.8 MiB | 10.5 MiB | 2.95 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-12mib | candidate-adr08-format4 | 3/3 | 4.73 ms | 13.2 MiB | 13.8 MiB | 4.64 ms | 13.9 MiB | 12.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 1gib-default | baseline-57d56cc-format3 | 3/3 | 2.34 ms | 9.8 MiB | 10.5 MiB | 3.04 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-default | candidate-adr08-format4 | 3/3 | 5.07 ms | 13.2 MiB | 13.8 MiB | 5.26 ms | 13.9 MiB | 16.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 4gib-default | baseline-57d56cc-format3 | 3/3 | 2.43 ms | 10.8 MiB | 11.5 MiB | 3.23 ms | 11.4 MiB | — | — | 4.0 MiB | 12 KiB |
| 4gib-default | candidate-adr08-format4 | 3/3 | 6.39 ms | 22.2 MiB | 22.8 MiB | 6.70 ms | 23.0 MiB | 41.0 MiB | 19.1 MiB | 12.1 MiB | 12 KiB |
