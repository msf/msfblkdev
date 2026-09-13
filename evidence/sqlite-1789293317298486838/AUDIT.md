# First full ext4 matrix: correctness passed, baseline not selected

All 87 cases passed the live correctness gate. Raw artifacts remain unchanged.

The baseline selector rejected a summary/result discrepancy for `kv-sequential-update-small-single-transaction--wal-full`. The result's throughput sample was `119551.02984339283`; the summary reread and emitted `119551.02984339284`.

The default serde_json float parser changed this value by one ULP. Integer durations and correctness results were unaffected. The lab now enables `float_roundtrip`, with this value as a regression test. A fresh full run is required before baseline selection. This run is not the selected performance baseline.
