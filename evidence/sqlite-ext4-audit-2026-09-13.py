import importlib.util
import json
from pathlib import Path
import re
import subprocess

root = Path('evidence/sqlite-1789293925836503146')
spec = importlib.util.spec_from_file_location('baseline', 'tools/sqlite/baseline.py')
baseline = importlib.util.module_from_spec(spec)
spec.loader.exec_module(baseline)
baseline.validate_run(root)
summary = json.loads((root / 'summary.json').read_text())
checks = stops = validations = 0
writes = []
stress = {}
fixtures = []
pids = []
mounts = Path('/proc/self/mountinfo').read_text()
for identity in baseline.catalog():
    directory = root / identity
    row = json.loads((directory / 'result.jsonl').read_text())
    log = (directory / 'commands.log').read_text()
    for number in range(1, 5):
        marker = f'command Check({number}): outcome=Ok(Exit(ExitStatus(unix_wait_status(0))))'
        assert log.count(marker) == 1, (identity, marker)
        checks += 1
    assert log.count('daemon SIGTERM: Exit(ExitStatus(unix_wait_status(0)))') == 2
    assert log.count('daemon request/flag evidence: PASS') == 2
    reports = re.findall(r'request-count-end .*complete=(\w+)', log)
    assert reports == ['true', 'true'], identity
    assert all(value == '0' for value in re.findall(r'^request-count .* errors=(\d+)', log, re.M))
    assert 'result: PASS; SQLite ext4 matrix selection' in log
    assert re.search(r'command format[^\n]*BLOCK_STORAGE_IO_LEDGER[^\n]*Some', log)
    fixture = re.search(r'^fixture: (\S+) identity=', log, re.M).group(1)
    assert not Path(fixture).exists() and fixture not in mounts, fixture
    fixtures.append(fixture)
    daemon_pids = re.findall(r'^daemon pid=(\d+)$', log, re.M)
    assert len(daemon_pids) == 2
    for pid in daemon_pids:
        assert not Path('/proc', pid).exists(), pid
    pids.extend(daemon_pids)
    stops += 2
    validations += len(row['validations'])
    assert row['validations'][0]['sha3sum_schema'] == row['validations'][1]['sha3sum_schema']
    assert row['metadata']['tested_source']['commit'].startswith('632ff0a')
    telemetry = row['telemetry']
    assert telemetry['errors'] == 0
    assert telemetry['device_snapshots']['enabled'] is True
    start = telemetry['device_snapshots']['drain_start']
    end = telemetry['device_snapshots']['drain_end']
    assert end['monotonic_ns'] >= start['monotonic_ns'] > 0
    for point in (start, end):
        assert point['ledger']['errors'] == 0
        assert point['ledger']['budget_exhausted'] is False
        assert point['ledger']['completed_totals_incomplete'] is False
    for key in ('reads', 'writes', 'flushes'):
        assert end['block'][key] >= start['block'][key]
    timings = row['phases']['workload_plus_drain']['timings']
    assert timings['workload_ns'] > 0
    assert timings['workload_plus_drain_ns'] == (
        telemetry['process']['duration_ns'] - telemetry['process']['total_snapshot_overhead_ns'])
    assert timings['workload_ns'] == (
        timings['workload_plus_drain_ns'] - telemetry['selected_drain_close']['duration_ns'])
    writes.append(row['metadata']['final_ledger']['write_reserved_bytes'])
    if identity.startswith(baseline.LONG):
        count = telemetry['automatic_passive_checkpoints_completed']
        assert count >= 2
        stress[identity] = count
assert len(fixtures) == len(set(fixtures)) == 87
assert checks == 348 and stops == validations == 174 and len(stress) == 6
assert list(Path('/sys/class/ublk-char').iterdir()) == []
subprocess.run(['cmp', 'rust/target/debug/block-storage-mount',
                '/usr/local/libexec/block-storage-mount'], check=True)
result = {
    'status': 'pass', 'run': str(root), 'tested_commit': '632ff0a',
    'runs': 87, 'case_profile_pairs': 29, 'repetitions': 3,
    'elapsed_seconds': summary['elapsed_ns'] / 1e9,
    'unmounted_filesystem_checks': checks, 'clean_daemon_stops': stops,
    'logical_validations': validations, 'max_reserved_write_bytes': max(writes),
    'write_limit_bytes_per_run': 512 * 1024 * 1024,
    'automatic_passive_checkpoints_in_stress_runs': stress,
    'owned_fixture_directories_remaining': 0, 'recorded_daemon_pids_remaining': 0,
    'live_ublk_devices_observed': 0, 'built_installed_helper_identical': True,
    'raw_request_reports_complete': True, 'real_io_errors': 0,
    'baseline': 'sqlite-ext4-baseline.json',
    'performance_claim': 'initial baseline only; self-comparison is not an independent candidate',
}
with Path('evidence/sqlite-ext4-audit.json').open('x') as output:
    json.dump(result, output, indent=2)
    output.write('\n')
print(json.dumps(result, indent=2))
