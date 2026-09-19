"""Run the predeclared combined packing/prefetch experiment on disposable files."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import tempfile
from run import checked, prepare


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repetitions', type=int, choices=range(1, 6), default=3)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    build = args.build_dir.resolve()
    build.mkdir(exist_ok=False)
    binaries = {
        version: prepare(repo, build, version, 'prefetch.rs', f'prefetch-{adapter}.rs')
        for version, adapter in [('v1.2.2', 'baseline'), ('working', 'candidate')]
    }
    cpu = min(os.sched_getaffinity(0))
    workloads = ['page-cold', 'page-steady', 'random', 'seq-blocks']
    budgets = [str(512 << 10), str(4 << 20)]
    source = build / 'working/rust'
    digest = hashlib.sha256()
    for path in sorted(p for p in source.rglob('*') if p.is_file()):
        digest.update(str(path.relative_to(source)).encode() + b'\0' + path.read_bytes())
    metadata = dict(kernel=os.uname().release, cpu=cpu, workloads=workloads,
                    configurations=[(v, b) for v in binaries for b in budgets],
                    repetitions=args.repetitions, working_source_sha256=digest.hexdigest(),
                    logical_bytes=4 << 30, backing_bytes=1 << 30, write_buffer_bytes=256 << 10,
                    tags={v: checked(['git', 'rev-parse', 'HEAD' if v == 'working' else f'{v}^{{commit}}'],
                                     cwd=repo, capture_output=True).stdout.strip() for v in binaries},
                    driver_sha256=hashlib.sha256((repo/'tools/mapping-bench/prefetch.rs').read_bytes()).hexdigest(),
                    binaries={v: hashlib.sha256(p.read_bytes()).hexdigest() for v, p in binaries.items()})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open('x') as output, tempfile.TemporaryDirectory(prefix='.mapping-prefetch-', dir=repo) as fixture:
        checked(['chattr', '+C', fixture])
        output.write(json.dumps({'metadata': metadata}) + '\n')
        rng = random.Random(81919)
        for repetition in range(1, args.repetitions + 1):
            jobs = [(v, b, w) for v in binaries for b in budgets for w in workloads]
            rng.shuffle(jobs)
            for version, budget, workload in jobs:
                result = checked(['taskset', '-c', str(cpu), str(binaries[version]), fixture,
                                  workload, budget], capture_output=True, timeout=120)
                row = dict(version=version, budget=budget, repetition=repetition)
                for field in result.stdout.strip().split():
                    key, value = field.split('=', 1)
                    row[key] = value if key == 'workload' else int(value)
                assert row['allocated_bytes'] < 256 << 20
                assert row['operations'] > 0 and row['elapsed_ns'] > 0
                output.write(json.dumps(row) + '\n')
                output.flush()
                print(f'PASS {version} {budget} {workload} repetition={repetition}', flush=True)


if __name__ == '__main__':
    main()
