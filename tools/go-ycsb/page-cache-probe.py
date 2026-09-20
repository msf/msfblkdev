import json
import os
from pathlib import Path
import tempfile
import time

def counters(path):
    return {key.rstrip(':'): int(value) for key, value in
            (line.split() for line in Path(path).read_text().splitlines())}

cgroup = Path('/sys/fs/cgroup') / Path('/proc/self/cgroup').read_text().strip().split('::', 1)[1].lstrip('/')
limit = int((cgroup / 'memory.max').read_text())
assert limit in (64 << 20, 256 << 20)
assert (cgroup / 'memory.swap.max').read_text().strip() == '0'
report = {'scope': 'kernel page-cache calibration, not a database benchmark',
          'memory_max': limit, 'file_bytes': 192 << 20, 'passes': []}
with tempfile.TemporaryDirectory(prefix='msfblkdev-page-cache-', dir='/var/tmp') as directory:
    path = Path(directory) / 'data'
    chunk = b'page-cache-proof' * 65536
    with path.open('wb', buffering=0) as output:
        for _ in range(192):
            assert output.write(chunk) == len(chunk)
        os.fsync(output.fileno())
    report['after_load_memory'] = counters(cgroup / 'memory.stat')
    for _ in range(2):
        before = counters('/proc/self/io')
        start = time.monotonic()
        count = 0
        with path.open('rb', buffering=0) as source:
            for _ in range(192):
                data = source.read(1 << 20)
                assert len(data) == 1 << 20
                count += len(data)
            assert source.read(1) == b''
        after = counters('/proc/self/io')
        report['passes'].append({'logical_bytes': count,
            'process_storage_read_bytes': after['read_bytes'] - before['read_bytes'],
            'elapsed_seconds': time.monotonic() - start,
            'memory': counters(cgroup / 'memory.stat')})
    report['memory_events'] = counters(cgroup / 'memory.events')
report['fixture_removed'] = not Path(directory).exists()
print(json.dumps(report, indent=2))
