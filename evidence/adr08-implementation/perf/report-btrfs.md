# ADR-08 performance comparisons

Baseline `57d56cc` (format 3) against candidate `5517587` (format 4). Kernel `7.0.0-31-generic`, CPU 0, 3 interleaved repetitions, `rustc 1.97.1 (8bab26f4f 2026-07-14)`, fixture filesystem `/dev/mapper/ubuntu--vg-home btrfs rw,relatime,compress=zstd:1,ssd,discard=async,space_cache=v2,subvolid=5,subvol=/` (attributes not recorded). Values are medians over successful repetitions; `runs` counts successful/total.

## 1. Fresh volume, nothing to reclaim

| job | version | runs | ops/s | op p50 | op p99 | op p99.9 | flush p99 | RSS |
|---|---|---|---|---|---|---|---|---|
| rand-read | baseline-57d56cc-format3 | 3/3 | 17,100 | 55 µs | 91 µs | 153 µs | 0.00 ms | 7.5 MiB |
| rand-read | candidate-adr08-format4 | 3/3 | 23,928 | 49 µs | 75 µs | 141 µs | 0.00 ms | 10.9 MiB |
| rand-write | baseline-57d56cc-format3 | 3/3 | 320,710 | 0 µs | 0 µs | 1 µs | 1.63 ms | 7.5 MiB |
| rand-write | candidate-adr08-format4 | 3/3 | 105,736 | 0 µs | 0 µs | 1 µs | 6.63 ms | 10.8 MiB |
| seq-read | baseline-57d56cc-format3 | 3/3 | 53,371 | 18 µs | 38 µs | 93 µs | 0.00 ms | 7.5 MiB |
| seq-read | candidate-adr08-format4 | 3/3 | 53,381 | 18 µs | 48 µs | 94 µs | 0.00 ms | 10.9 MiB |
| seq-write | baseline-57d56cc-format3 | 3/3 | 147,198 | 0 µs | 0 µs | 0 µs | 3.20 ms | 7.6 MiB |
| seq-write | candidate-adr08-format4 | 3/3 | 113,395 | 0 µs | 0 µs | 1 µs | 5.22 ms | 10.8 MiB |

## 2. Aged overwrite for twice the log capacity

| job | version | runs | completed | ENOSPC | ops/s | op p99 | op p99.9 | op max | copies | freed | pressure steps | reopen |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct | baseline-57d56cc-format3 | 3/3 | 49,005 | 16 | 322,171 | 0 µs | 0 µs | 0.01 ms | — | — | — | 2.30 ms |
| 128mib-50pct | candidate-adr08-format4 | 3/3 | 131,072 | 0 | 67,337 | 0 µs | 1 µs | 277.39 ms | 15,452 | 62 | 1,095 | 9.77 ms |
| 128mib-80pct | baseline-57d56cc-format3 | 3/3 | 24,383 | 16 | 166,850 | 0 µs | 1 µs | 0.00 ms | — | — | — | 3.62 ms |
| 128mib-80pct | candidate-adr08-format4 | 3/3 | 81,920 | 0 | 53,647 | 1 µs | 1 µs | 155.17 ms | 45,993 | 58 | 1,036 | 8.08 ms |
| 4gib-50pct | baseline-57d56cc-format3 | 3/3 | 246,903 | 16 | 218,413 | 1 µs | 1 µs | 0.02 ms | — | — | — | 22.49 ms |
| 4gib-50pct | candidate-adr08-format4 | 3/3 | 1,048,576 | 0 | 16,672 | 1 µs | 3 µs | 610.67 ms | 550,476 | 877 | 15,402 | 33.18 ms |

## 3. Progress and latency: scheduled ticks against inline pressure only

| job | version | runs | completed | ops/s | op p99 | op p99.9 | op max | ticks | tick p99 | pressure steps | pressure waits | copies |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-50pct-inline | candidate-adr08-format4 | 3/3 | 131,072 | 68,680 | 0 µs | 1 µs | 178.60 ms | 0 | 0 µs | 978 | 437 | 10,379 |
| 128mib-50pct-ticks | candidate-adr08-format4 | 3/3 | 131,072 | 60,437 | 0 µs | 1 µs | 300.58 ms | 15 | 1285 µs | 1,111 | 505 | 15,598 |

## 4. Memory and restart

| job | version | runs | open | RSS after open | RSS after writes | reopen | RSS after reopen | heap total | heap allocated | cache | index |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 128mib-default | baseline-57d56cc-format3 | 3/3 | 6.91 ms | 7.2 MiB | 7.4 MiB | 8.48 ms | 7.4 MiB | — | — | 389 KiB | 0 KiB |
| 128mib-default | candidate-adr08-format4 | 3/3 | 10.58 ms | 10.5 MiB | 10.7 MiB | 19.19 ms | 10.9 MiB | 16.0 MiB | 7.3 MiB | 389 KiB | 0 KiB |
| 1gib-12mib | baseline-57d56cc-format3 | 3/3 | 7.37 ms | 9.8 MiB | 10.5 MiB | 12.69 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-12mib | candidate-adr08-format4 | 3/3 | 11.15 ms | 13.2 MiB | 13.8 MiB | 23.24 ms | 13.9 MiB | 12.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 1gib-default | baseline-57d56cc-format3 | 3/3 | 7.86 ms | 9.8 MiB | 10.5 MiB | 13.42 ms | 10.5 MiB | — | — | 3.0 MiB | 3 KiB |
| 1gib-default | candidate-adr08-format4 | 3/3 | 12.76 ms | 13.2 MiB | 13.8 MiB | 22.99 ms | 13.9 MiB | 16.0 MiB | 10.0 MiB | 3.0 MiB | 3 KiB |
| 4gib-default | baseline-57d56cc-format3 | 3/3 | 8.16 ms | 10.8 MiB | 11.5 MiB | 14.30 ms | 11.4 MiB | — | — | 4.0 MiB | 12 KiB |
| 4gib-default | candidate-adr08-format4 | 3/3 | 15.37 ms | 22.2 MiB | 22.8 MiB | 23.82 ms | 23.0 MiB | 41.0 MiB | 19.1 MiB | 12.1 MiB | 12 KiB |
