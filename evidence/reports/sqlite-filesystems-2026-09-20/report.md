# Workbench evidence report

Descriptive comparison of selected evidence. Correctness and calibration are separate result classes.

N/A means unavailable, never zero. CSV blanks and JSON nulls have the same meaning.
Backing I/O covers the workload plus drain, excludes initialization and validation, and is not occupied physical space.
YCSB operation rates use logical requests; F's API-call latency counts include its additional updates.
The CSV retains each repetition, load/drain times, request/flush counts and peak RSS where measured.
The JSON retains source paths, configuration, all API latency summaries and failures. No raw evidence is changed.

| Column | Run | FS | Kind | Status | Selected samples |
| --- | --- | --- | --- | --- | --- |
| Run 1 | sqlite-1789917416605635188 | Ext4 | sqlite-correctness | validated-full-matrix | 87 |
| Run 2 | sqlite-1789917155929943435 | Xfs | sqlite-correctness | validated-full-matrix | 87 |

## Workload seconds by case

Columns follow the input-run order above. Medians only; configuration differences remain visible in the per-run sections and JSON.
N/A can also mean that this case was not selected in that input. No percentage-change or regression verdict is inferred.

| DB / case / profile | Run 1 | Run 2 |
| --- | --- | --- |
| sqlite / kv-long-wal-checkpoint-stress / wal-full | 1.039 | 0.803 |
| sqlite / kv-long-wal-checkpoint-stress / wal-normal | 0.452 | 0.432 |
| sqlite / kv-random-read-large / delete-extra | 0.005 | 0.006 |
| sqlite / kv-random-read-large / wal-full | 0.006 | 0.007 |
| sqlite / kv-random-read-large / wal-normal | 0.006 | 0.006 |
| sqlite / kv-random-read-small / delete-extra | 0.002 | 0.002 |
| sqlite / kv-random-read-small / wal-full | 0.002 | 0.002 |
| sqlite / kv-random-read-small / wal-normal | 0.002 | 0.002 |
| sqlite / kv-random-update-large-per-op-commit / delete-extra | 0.408 | 0.183 |
| sqlite / kv-random-update-large-per-op-commit / wal-full | 0.113 | 0.069 |
| sqlite / kv-random-update-large-per-op-commit / wal-normal | 0.008 | 0.005 |
| sqlite / kv-random-update-large-single-transaction / delete-extra | 0.010 | 0.009 |
| sqlite / kv-random-update-large-single-transaction / wal-full | 0.007 | 0.007 |
| sqlite / kv-random-update-large-single-transaction / wal-normal | 0.005 | 0.005 |
| sqlite / kv-sequential-read-large / delete-extra | 0.006 | 0.006 |
| sqlite / kv-sequential-read-large / wal-full | 0.006 | 0.007 |
| sqlite / kv-sequential-read-large / wal-normal | 0.006 | 0.007 |
| sqlite / kv-sequential-read-small / delete-extra | 0.002 | 0.002 |
| sqlite / kv-sequential-read-small / wal-full | 0.002 | 0.002 |
| sqlite / kv-sequential-read-small / wal-normal | 0.002 | 0.002 |
| sqlite / kv-sequential-update-small-per-op-commit / delete-extra | 0.271 | 0.144 |
| sqlite / kv-sequential-update-small-per-op-commit / wal-full | 0.078 | 0.066 |
| sqlite / kv-sequential-update-small-per-op-commit / wal-normal | 0.003 | 0.003 |
| sqlite / kv-sequential-update-small-single-transaction / delete-extra | 0.002 | 0.002 |
| sqlite / kv-sequential-update-small-single-transaction / wal-full | 0.002 | 0.002 |
| sqlite / kv-sequential-update-small-single-transaction / wal-normal | 0.001 | 0.001 |
| sqlite / speedtest-main / delete-extra | 0.048 | 0.044 |
| sqlite / speedtest-main / wal-full | 0.029 | 0.030 |
| sqlite / speedtest-main / wal-normal | 0.022 | 0.022 |

## sqlite-1789917416605635188

sqlite-correctness: **validated-full-matrix**. Source: `/home/miguel/play/msfblkdev/evidence/sqlite-1789917416605635188`.

- Warm-cache correctness timings; backing counters exclude host-device amplification. Physical occupancy and latency tails unavailable.

Values are per-case medians across the listed repetitions; single samples remain single samples.

| DB | FS | Case | Profile | Samples OK | Workload s | Ops/s | Backing read MiB | Backing written MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| sqlite | Ext4 | speedtest-main | delete-extra | 3/3 | 0.048 | N/A | 0.000 | 4.184 |
| sqlite | Ext4 | kv-sequential-read-small | delete-extra | 3/3 | 0.002 | 2,482,522.039 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-small | delete-extra | 3/3 | 0.002 | 2,546,141.036 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-read-large | delete-extra | 3/3 | 0.006 | 716,177.946 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-large | delete-extra | 3/3 | 0.005 | 849,743.874 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-update-small-single-transaction | delete-extra | 3/3 | 0.002 | 117,812.245 | 0.000 | 0.145 |
| sqlite | Ext4 | kv-sequential-update-small-per-op-commit | delete-extra | 3/3 | 0.271 | 945.801 | 0.000 | 28.016 |
| sqlite | Ext4 | kv-random-update-large-single-transaction | delete-extra | 3/3 | 0.010 | 26,285.316 | 0.000 | 0.660 |
| sqlite | Ext4 | kv-random-update-large-per-op-commit | delete-extra | 3/3 | 0.408 | 626.851 | 0.000 | 35.535 |
| sqlite | Ext4 | speedtest-main | wal-full | 3/3 | 0.029 | N/A | 0.000 | 2.180 |
| sqlite | Ext4 | kv-sequential-read-small | wal-full | 3/3 | 0.002 | 2,203,460.768 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-small | wal-full | 3/3 | 0.002 | 2,177,768.893 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-read-large | wal-full | 3/3 | 0.006 | 668,669.410 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-large | wal-full | 3/3 | 0.006 | 634,820.067 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-update-small-single-transaction | wal-full | 3/3 | 0.002 | 129,199.749 | 0.000 | 0.117 |
| sqlite | Ext4 | kv-sequential-update-small-per-op-commit | wal-full | 3/3 | 0.078 | 3,284.492 | 0.000 | 7.957 |
| sqlite | Ext4 | kv-random-update-large-single-transaction | wal-full | 3/3 | 0.007 | 34,996.448 | 0.000 | 0.633 |
| sqlite | Ext4 | kv-random-update-large-per-op-commit | wal-full | 3/3 | 0.113 | 2,264.025 | 0.000 | 12.219 |
| sqlite | Ext4 | speedtest-main | wal-normal | 3/3 | 0.022 | N/A | 0.000 | 1.523 |
| sqlite | Ext4 | kv-sequential-read-small | wal-normal | 3/3 | 0.002 | 2,037,304.153 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-small | wal-normal | 3/3 | 0.002 | 2,168,322.377 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-read-large | wal-normal | 3/3 | 0.006 | 653,731.320 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-random-read-large | wal-normal | 3/3 | 0.006 | 662,341.610 | 0.000 | 0.000 |
| sqlite | Ext4 | kv-sequential-update-small-single-transaction | wal-normal | 3/3 | 0.001 | 177,213.157 | 0.000 | 0.117 |
| sqlite | Ext4 | kv-sequential-update-small-per-op-commit | wal-normal | 3/3 | 0.003 | 86,833.144 | 0.000 | 0.988 |
| sqlite | Ext4 | kv-random-update-large-single-transaction | wal-normal | 3/3 | 0.005 | 52,818.812 | 0.000 | 0.633 |
| sqlite | Ext4 | kv-random-update-large-per-op-commit | wal-normal | 3/3 | 0.008 | 32,711.826 | 0.000 | 3.379 |
| sqlite | Ext4 | kv-long-wal-checkpoint-stress | wal-full | 3/3 | 1.039 | 3,942.445 | 0.000 | 102.949 |
| sqlite | Ext4 | kv-long-wal-checkpoint-stress | wal-normal | 3/3 | 0.452 | 9,052.250 | 0.000 | 49.613 |


## sqlite-1789917155929943435

sqlite-correctness: **validated-full-matrix**. Source: `/home/miguel/play/msfblkdev/evidence/sqlite-1789917155929943435`.

- Warm-cache correctness timings; backing counters exclude host-device amplification. Physical occupancy and latency tails unavailable.

Values are per-case medians across the listed repetitions; single samples remain single samples.

| DB | FS | Case | Profile | Samples OK | Workload s | Ops/s | Backing read MiB | Backing written MiB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| sqlite | Xfs | speedtest-main | delete-extra | 3/3 | 0.044 | N/A | 0.016 | 2.762 |
| sqlite | Xfs | kv-sequential-read-small | delete-extra | 3/3 | 0.002 | 2,558,471.313 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-small | delete-extra | 3/3 | 0.002 | 2,517,589.629 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-read-large | delete-extra | 3/3 | 0.006 | 674,830.646 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-large | delete-extra | 3/3 | 0.006 | 658,399.488 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-update-small-single-transaction | delete-extra | 3/3 | 0.002 | 144,128.157 | 0.000 | 0.066 |
| sqlite | Xfs | kv-sequential-update-small-per-op-commit | delete-extra | 3/3 | 0.144 | 1,775.775 | 0.031 | 13.250 |
| sqlite | Xfs | kv-random-update-large-single-transaction | delete-extra | 3/3 | 0.009 | 28,170.945 | 0.000 | 0.590 |
| sqlite | Xfs | kv-random-update-large-per-op-commit | delete-extra | 3/3 | 0.183 | 1,395.922 | 0.031 | 19.059 |
| sqlite | Xfs | speedtest-main | wal-full | 3/3 | 0.030 | N/A | 0.016 | 1.754 |
| sqlite | Xfs | kv-sequential-read-small | wal-full | 3/3 | 0.002 | 2,195,884.004 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-small | wal-full | 3/3 | 0.002 | 2,219,536.910 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-read-large | wal-full | 3/3 | 0.007 | 608,315.687 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-large | wal-full | 3/3 | 0.007 | 619,036.802 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-update-small-single-transaction | wal-full | 3/3 | 0.002 | 149,568.967 | 0.000 | 0.059 |
| sqlite | Xfs | kv-sequential-update-small-per-op-commit | wal-full | 3/3 | 0.066 | 3,905.287 | 0.000 | 4.414 |
| sqlite | Xfs | kv-random-update-large-single-transaction | wal-full | 3/3 | 0.007 | 37,599.346 | 0.000 | 0.574 |
| sqlite | Xfs | kv-random-update-large-per-op-commit | wal-full | 3/3 | 0.069 | 3,722.001 | 0.000 | 7.281 |
| sqlite | Xfs | speedtest-main | wal-normal | 3/3 | 0.022 | N/A | 0.016 | 1.426 |
| sqlite | Xfs | kv-sequential-read-small | wal-normal | 3/3 | 0.002 | 2,178,580.869 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-small | wal-normal | 3/3 | 0.002 | 2,175,497.188 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-read-large | wal-normal | 3/3 | 0.007 | 613,869.034 | 0.000 | 0.000 |
| sqlite | Xfs | kv-random-read-large | wal-normal | 3/3 | 0.006 | 636,753.156 | 0.000 | 0.000 |
| sqlite | Xfs | kv-sequential-update-small-single-transaction | wal-normal | 3/3 | 0.001 | 191,319.048 | 0.000 | 0.059 |
| sqlite | Xfs | kv-sequential-update-small-per-op-commit | wal-normal | 3/3 | 0.003 | 94,735.658 | 0.000 | 0.930 |
| sqlite | Xfs | kv-random-update-large-single-transaction | wal-normal | 3/3 | 0.005 | 48,388.530 | 0.000 | 0.574 |
| sqlite | Xfs | kv-random-update-large-per-op-commit | wal-normal | 3/3 | 0.005 | 49,897.223 | 0.000 | 3.320 |
| sqlite | Xfs | kv-long-wal-checkpoint-stress | wal-full | 3/3 | 0.803 | 5,098.133 | 0.000 | 88.941 |
| sqlite | Xfs | kv-long-wal-checkpoint-stress | wal-normal | 3/3 | 0.432 | 9,474.175 | 0.000 | 49.133 |

