"""Run the repository inventory and publish a single-file report."""

import collections
import datetime as dt
import fcntl
import json
import platform
import re
import subprocess
import sys
from pathlib import Path

from execute import execute
from inventory import inventory
from render import render

ROOT = Path(__file__).resolve().parents[2]
STATUSES = {'passed', 'failed', 'blocked', 'not run'}


def write_atomic(path, data):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(data, encoding='utf-8')
    temporary.replace(path)


def describe_cases(cases, directory):
    descriptions = {}
    for file in sorted(directory.glob('*.json')):
        entries = json.loads(file.read_text())
        if descriptions.keys() & entries.keys():
            raise ValueError(f'Duplicate test description in {file.name}')
        descriptions.update(entries)
    for case in cases:
        key = case.get('source_key')
        if key in descriptions:
            case['setup'], case['action'], case['expected'] = descriptions[key]


def attach_explanations(cases, report):
    # The authored ADR-05 appendix uses one non-nested section per explanation.
    sections = re.findall(r'<section id="ex-.*?</section>', report, re.S)
    by_anchor = {}
    for section in sections:
        prose = re.sub(r'<a\b[^>]*>.*?</a>', '', section, flags=re.S)
        prose = re.sub(r' id="[^"]*"', '', prose)
        for anchor in re.findall(r' id="(ex-[^"]+)"', section):
            by_anchor[anchor] = prose
    for case in cases:
        anchor = case.get('adr05_anchor')
        if anchor:
            case['explanation_html'] = by_anchor[anchor]


def checked_results(cases, results):
    expected = {case['id'] for case in cases}
    if not expected or len(expected) != len(cases):
        raise ValueError('Empty inventory or duplicate case IDs')
    if set(results) != expected:
        raise ValueError('Runner and inventory case IDs do not match')
    for result in results.values():
        if result['status'] not in STATUSES or not isinstance(result['detail'], str):
            raise ValueError('Invalid runner verdict')
    return collections.Counter(result['status'] for result in results.values())


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT).decode().strip()


def run(output):
    date = dt.datetime.now(dt.timezone.utc)
    revision = git('rev-parse', 'HEAD')
    dirty = bool(git('status', '--porcelain', '--untracked-files=normal'))
    label = revision + (' (modified working tree)' if dirty else '')
    evidence = output / 'runs' / date.strftime('%Y%m%dT%H%M%S%fZ')
    evidence.mkdir(parents=True)
    cases = inventory(ROOT)
    if not cases or len({case['id'] for case in cases}) != len(cases):
        raise ValueError('Empty inventory or duplicate case IDs; no tests started')
    describe_cases(cases, Path(__file__).with_name('descriptions'))
    attach_explanations(cases, (ROOT / 'docs/adr05-report.html').read_text())
    print(f'Running {len(cases)} cases at {label}; evidence: {evidence}', flush=True)
    try:
        results = execute(ROOT, cases, evidence)
        counts = checked_results(cases, results)
    except Exception as error:
        # Report a runner failure, not a failure of every product test.
        print(f'Test runner error: {error}', file=sys.stderr)
        results = {case['id']: {'status': 'blocked', 'detail':
                   f'Runner did not return a usable result set: {error}'} for case in cases}
        counts = checked_results(cases, results)
    record = dict(revision=label, date=date.isoformat(timespec='seconds'),
                  environment=f'{platform.system()} {platform.release()} {platform.machine()}',
                  results=results)
    history = [json.loads(file.read_text()) for file in sorted((output / 'runs').glob('*/result.json'))]
    html = render(cases, [*history, record])
    write_atomic(evidence / 'result.json', json.dumps(record, indent=2) + '\n')
    report = output / 'report.html'
    write_atomic(report, html)
    print(f'{report}\n' + ', '.join(f'{counts[s]} {s}' for s in sorted(STATUSES)), flush=True)
    return int(counts['passed'] != len(cases))


def main():
    output = ROOT / 'build/tests-all'
    output.mkdir(parents=True, exist_ok=True)
    with (output / 'run.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            sys.exit('Another tests-all run holds the output lock')
        return run(output)


if __name__ == '__main__':
    sys.exit(main())
