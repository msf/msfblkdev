# Initial SQLite probe audit

This run passed correctness, not database acceptance or performance-baseline qualification.

The ext4 WAL/FULL case completed 4096 updates, four filesystem checks and logical-content verification before and after restart. The final ledger recorded 214749184 bytes written, with no errors or pending operations, below its 536870912-byte limit.

The kernel write-operation measurements are invalid: `/sys/block/ublkb0/stat` reported 28389 write completions, but only 204856 data sectors. The daemon reported 25607 data writes (`204856 / 8`). Kernel write completions also count zero-data flush I/O. The corrected parser uses data sectors divided by eight for this validated 4096-byte request geometry. Final daemon reports account for requests that complete after the last sysfs sample.

The original JSON and raw output remain unchanged. Do not select this run as a baseline.
