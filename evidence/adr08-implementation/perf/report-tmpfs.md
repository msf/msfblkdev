# ADR-08 performance comparisons

Baseline `57d56cc` (format 3) against candidate `2303843` (format 4). Kernel `7.0.0-31-generic`, CPU 0, 3 interleaved repetitions, `rustc 1.97.1 (8bab26f4f 2026-07-14)`, fixture filesystem `tmpfs tmpfs rw,nosuid,nodev,size=32025876k,nr_inodes=1048576,inode64,usrquota` (chattr +C unavailable: chattr: Operation not supported while setting flags on /tmp/claude-1000/-home-miguel-play-msfblkdev/92e83b6e-6920-4d1b-bd8e-3d3f914510b0/scratchpad/bench/.adr08-bench-m6o3ja_e). Values are medians over successful repetitions; `runs` counts successful/total.

## 1. Fresh volume, nothing to reclaim

| job | version | runs | ops/s | op p50 | op p99 | op p99.9 | flush p99 | RSS |
|---|---|---|---|---|---|---|---|---|
| rand-read | baseline-57d56cc-format3 | 3/3 | 460,933 | 2 µs | 3 µs | 7 µs | 0.00 ms | 7.5 MiB |
| rand-read | candidate-adr08-format4 | 3/3 | 477,143 | 2 µs | 3 µs | 7 µs | 0.00 ms | 10.9 MiB |
| rand-write | baseline-57d56cc-format3 | 3/3 | 1,008,158 | 0 µs | 0 µs | 1 µs | 0.31 ms | 7.5 MiB |
| rand-write | candidate-adr08-format4 | 3/3 | 1,024,332 | 0 µs | 0 µs | 1 µs | 0.42 ms | 10.8 MiB |
| seq-read | baseline-57d56cc-format3 | 3/3 | 475,595 | 2 µs | 3 µs | 7 µs | 0.00 ms | 7.5 MiB |
| seq-read | candidate-adr08-format4 | 3/3 | 488,584 | 2 µs | 3 µs | 7 µs | 0.00 ms | 10.9 MiB |
| seq-write | baseline-57d56cc-format3 | 3/3 | 1,052,865 | 0 µs | 0 µs | 1 µs | 0.34 ms | 7.6 MiB |
| seq-write | candidate-adr08-format4 | 3/3 | 1,057,653 | 0 µs | 0 µs | 1 µs | 0.28 ms | 10.8 MiB |

## 2. Aged overwrite for twice the log capacity

| job | version | runs | completed | ENOSPC | ops/s | op p99 | op p99.9 | op max | copies | freed | pressure steps | reopen |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct | baseline-57d56cc-format3 | 3/3 | 49,005 | 16 | 1,100,339 | 0 µs | 1 µs | 0.01 ms | — | — | — | 0.73 ms |
| 128mib-50pct | candidate-adr08-format4 | 3/3 | 131,072 | 0 | 543,237 | 0 µs | 1 µs | 32.66 ms | 13,370 | 59 | 1,071 | 3.22 ms |
| 128mib-80pct | baseline-57d56cc-format3 | 3/3 | 24,383 | 16 | 1,041,689 | 0 µs | 1 µs | 0.01 ms | — | — | — | 0.92 ms |
| 128mib-80pct | candidate-adr08-format4 | 3/3 | 81,920 | 0 | 356,107 | 0 µs | 1 µs | 20.30 ms | 42,447 | 55 | 989 | 2.83 ms |
| 4gib-50pct | baseline-57d56cc-format3 | 3/3 | 246,903 | 16 | 720,641 | 1 µs | 1 µs | 0.02 ms | — | — | — | 3.58 ms |
| 4gib-50pct | candidate-adr08-format4 | 3/3 | 1,048,576 | 0 | 184,288 | 1 µs | 2 µs | 40.23 ms | 557,888 | 892 | 15,449 | 7.99 ms |

## 3. Progress and latency: scheduled ticks against inline pressure only

| job | version | runs | completed | ops/s | op p99 | op p99.9 | op max | ticks | tick p99 | pressure steps | pressure waits | copies |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct-inline | candidate-adr08-format4 | 3/3 | 131,072 | 562,259 | 0 µs | 1 µs | 25.67 ms | 0 | 0 µs | 979 | 438 | 10,379 |
| 128mib-50pct-ticks | candidate-adr08-format4 | 3/3 | 131,072 | 542,447 | 0 µs | 1 µs | 33.03 ms | 2 | 306 µs | 1,068 | 484 | 13,366 |

## 4. Memory and restart

| job | version | runs | open | RSS after open | RSS after writes | reopen | RSS after reopen | heap total | heap allocated | cache | index |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-default | baseline-57d56cc-format3 | 3/3 | 1.88 ms | 7.2 MiB | 7.4 MiB | 2.05 ms | 7.4 MiB | — | — | 389 KiB | 0 KiB |
| 128mib-default | candidate-adr08-format4 | 3/3 | 4.19 ms | 10.5 MiB | 10.7 MiB | 3.92 ms | 10.9 MiB | 16.0 MiB | 7.3 MiB | 389 KiB | 0 KiB |
| 1gib-12mib | baseline-57d56cc-format3 | 3/3 | 2.20 ms | 9.8 MiB | 10.5 MiB | 2.73 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-12mib | candidate-adr08-format4 | 3/3 | 5.35 ms | 13.2 MiB | 13.8 MiB | 4.90 ms | 13.9 MiB | 12.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 1gib-default | baseline-57d56cc-format3 | 3/3 | 2.32 ms | 9.8 MiB | 10.5 MiB | 3.13 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-default | candidate-adr08-format4 | 3/3 | 4.84 ms | 13.2 MiB | 13.8 MiB | 4.99 ms | 13.9 MiB | 16.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 4gib-default | baseline-57d56cc-format3 | 3/3 | 2.30 ms | 10.8 MiB | 11.5 MiB | 3.09 ms | 11.4 MiB | — | — | 4.0 MiB | 12 KiB |
| 4gib-default | candidate-adr08-format4 | 3/3 | 6.50 ms | 22.3 MiB | 22.9 MiB | 6.49 ms | 23.0 MiB | 41.0 MiB | 19.1 MiB | 12.1 MiB | 12 KiB |
