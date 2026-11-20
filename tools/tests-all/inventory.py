"""Enumerate tests without building binaries, importing tests, or opening devices.

Rust discovery follows this repository's rustfmt-formatted module declarations and
literal #[test] attributes. Check every discovered name against libtest at execution
rather than treating a zero-exit, zero-test Cargo invocation as a pass. Cargo and
Make aliases are entry points, not additional cases. The two ignored Rust drivers
are represented by their explicitly configured workloads.
"""
import ast
from html import unescape
from pathlib import Path
import re
import subprocess


def case(group, selector, suite, title, setup, action, expected, **extra):
    return dict(id=f'{group}/{selector}', group=group, selector=selector, suite=suite,
                title=title, setup=setup, action=action, expected=expected, **extra)


def source_case(group, selector, suite, path, name):
    title = name.removeprefix('test_').replace('_', ' ').capitalize()
    return case(group, selector, suite, title, 'Plain-language explanation pending.',
                'Run this named test with its own fixtures and assertions.',
                'All assertions must hold; a skip is not a pass.', source_key=f'{path}::{name}')


def tracked(root):
    data = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others',
                                    '--exclude-standard'], cwd=root)
    return {Path(p.decode()) for p in data.split(b'\0') if p and (root / p.decode()).is_file()}


def python_cases(root, files):
    for path in sorted(p for p in files if p.suffix == '.py'):
        source = (root/path).read_text()
        if 'def test' not in source:
            continue
        tree = ast.parse(source, filename=str(path))
        for obj in tree.body:
            if isinstance(obj, ast.FunctionDef) and obj.name.startswith('test'):
                raise ValueError(f'{path}:{obj.name} needs a function-test runner, not unittest')
            if not isinstance(obj, ast.ClassDef):
                continue
            for fn in obj.body:
                if isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) and fn.name.startswith('test'):
                    if not path.name.startswith('test'):
                        raise ValueError(f'{path}:{fn.name} is outside unittest discovery; register its runner')
                    selector = '.'.join([path.stem, obj.name, fn.name])
                    yield source_case(f'python:{path.parent}', selector, f'Python · {path.parent}', path, fn.name)


MODULE = re.compile(r'^(?P<indent> *)(?:pub(?:\([^)]*\))? )?mod (\w+)\s*([;{])', re.M)
TEST = re.compile(r'^ *#\[test\]\s*(?P<attrs>(?:\s*#\[[^\n]*\](?: *//[^\n]*)?\s*)*)fn (\w+)\(', re.M)
DOC = re.compile(r'^ */// ```(?:rust|compile_fail|no_run|should_panic)?\s*\n(?:(?: *///[^\n]*)\n)*? */// ```\s*\n(?: *///[^\n]*\n)* *(?:pub(?:\([^)]*\))? )?(?:struct|enum|fn|trait) (\w+)', re.M)


def rust_cases(root, files):
    manifests = sorted(p for p in files if p.name == 'Cargo.toml')
    seen_functions = set()
    for manifest in manifests:
        crate = manifest.parent
        targets = [(crate/'src/lib.rs', '--lib')]
        for prefix, where in [('--bin=', 'src/bin'), ('--test=', 'tests'), ('--example=', 'examples')]:
            base = crate / where
            for path in sorted(p for p in files if p.suffix == '.rs'):
                if path.parent == base or (path.parent.parent == base and path.name == 'main.rs'):
                    targets.append((path, prefix + (path.parent.name if path.name == 'main.rs' else path.stem)))

        def walk(path, names, children, target):
            text = (root/path).read_text()
            mods = list(MODULE.finditer(text))
            # External module locations can be overridden, e.g. fs_support in two binaries.
            for m in mods:
                if m[3] != ';':
                    continue
                name = m[2]
                override = re.search(r'#\[path = "([^"]+)"\]\s*$', text[:m.start()])
                options = [path.parent/override[1]] if override else [children/f'{name}.rs', children/name/'mod.rs']
                resolved = [Path(p.resolve().relative_to(root)) for p in (root/o for o in options) if p.is_file()]
                if len(resolved) != 1:
                    raise ValueError(f'cannot resolve {path}: mod {name}')
                child = resolved[0]
                childdir = child.parent if child.name == 'mod.rs' else child.with_suffix('')
                yield from walk(child, names+[name], childdir, target)
            for t in TEST.finditer(text):
                name = t[2]
                seen_functions.add((path, name))
                parents = []
                for m in mods:
                    if m[3] == '{' and m.start() < t.start() and not re.search(r'^'+m[1]+r'}', text[m.end():t.start()], re.M):
                        parents.append(m[2])
                selector = '::'.join(names+parents+[name])
                group = f'rust:{crate}:{target}'
                if '#[ignore' in t[1]:
                    if str(path) == 'rust/tests/compaction_small.rs' and name == 'overwrites_at_full_logical_fill_survive_restart':
                        yield case('make:test-compaction-small', 'test-compaction-small', 'Space reclamation',
                                   'Reuse space on a full logical volume', 'Fill a small volume with known values.',
                                   'Overwrite its blocks, force the engine to reclaim space, then reopen the volume.',
                                   'Every block must contain its latest value after restart.', source_key=f'{path}::{name}')
                        continue
                    if str(path) == 'rust/src/tests/seeded_dst.rs' and name == 'replay_from_environment':
                        continue  # represented by sweep and replay below
                    # A new ignored test needs an explicit fixture; do not quietly discard it.
                    group = 'manual'
                    selector = f'{crate}:{target}:{selector}'
                yield source_case(group, selector, f'Rust · {crate} · {target.lstrip("-").replace("bin=", "").replace("test=", "")}', path, name)
            if target == '--lib':
                for d in DOC.finditer(text):
                    line = text.count('\n', 0, d.start()) + 1
                    item = '::'.join(names+[d[1]])
                    selector = f'{path.relative_to(crate)} - {item} (line {line})'
                    row = case(f'rust:{crate}:--doc', selector, 'Rust documentation examples',
                               f'Check the documented example for {d[1]}', 'Compile the example as documented.',
                               'Use the example with the public library interface.',
                               'The example must satisfy its documented compile or execution expectation.')
                    # Line numbers change; the report identity should not.
                    row['id'] = f'rust:{crate}:--doc/{item}'
                    if item == 'io_accounting::PendingOperation':
                        row.update(title='Reject a second operation while the first still owns the writer',
                                   action='Try to start a second flush before the first one finishes.',
                                   expected='The Rust compiler must reject the example. Two operations cannot use this writer at the same time.')
                    yield row
        for path, target in targets:
            if path in files:
                yield from walk(path, [], path.parent, target)
    # Never silently drop an attributed function in a file we did not traverse.
    for path in sorted(p for p in files if p.suffix == '.rs'):
        text = (root/path).read_text()
        funcs = list(TEST.finditer(text))
        if len(funcs) != len(re.findall(r'^ *#\[test\]', text, re.M)) or any((path,t[2]) not in seen_functions for t in funcs):
            raise ValueError(f'unrecognized Rust test layout in {path}; extend discovery')


def adr_descriptions(root):
    text = (root/'docs/adr05-report.html').read_text()
    descriptions = {}
    for m in re.finditer(r'<section id="(ex-[^"]+)">\s*<h4(?: id="(ex-[^"]+)")?>(.*?)</h4>(.*?)</section>', text, re.S):
        title = unescape(re.sub('<[^>]+>', '', m[3]))
        paragraphs = re.findall(r'<p><strong>[^<]+</strong>\s*(.*?)</p>', m[4], re.S)
        if len(paragraphs) < 3:
            raise ValueError('incomplete ADR-05 scenario: ' + m[1])
        content = [unescape(re.sub('<[^>]+>', '', p)) for p in paragraphs[:3]]
        for anchor in filter(None, (m[1], m[2])):
            descriptions[anchor[3:]] = dict(title=title, setup=content[0], action=content[1],
                                              expected=content[2], adr05_anchor=anchor)
    return descriptions


def sqlite_cases(root, lab):
    matrix = (root/lab/'ext4/sqlite/matrix.rs').read_text()
    config = (root/lab/'config.rs').read_text()
    names = dict(re.findall(r'Self::(\w+) => (?:\{\s*)?"([\w-]+)"', config))
    short_cases = re.findall(r'Case::(\w+)', matrix.split('const SHORT_CASES:', 1)[1].split('];',1)[0])
    profiles = re.findall(r'Profile::(\w+)', matrix.split('const PROFILES:',1)[1].split('];',1)[0])
    long = re.search(r'for profile in \[([^\]]+)\] \{\s*push_repetitions\(&mut selections, Case::(\w+), profile\)', matrix)
    if not long:
        raise ValueError('SQLite matrix changed: update discovery for the long runs')
    pairs = [(names[c], names[p]) for c in short_cases for p in profiles]
    pairs += [(names[long[2]], names[p]) for p in re.findall(r'Profile::(\w+)', long[1])]
    if len(set(pairs)) != int(re.search(r'MATRIX_CASES: usize = (\d+);', matrix)[1]):
        raise ValueError('SQLite case count does not match its runner')
    repetitions = int(re.search(r'REPETITIONS: u8 = (\d+);', matrix)[1])
    for fs in ('ext4', 'xfs'):
        for workload, profile in pairs:
            if workload == 'speedtest-main':
                action = 'Create a database and run the standard SQLite mix of queries and updates.'
            elif workload == 'kv-long-wal-checkpoint-stress':
                action = 'Update values repeatedly while SQLite moves pages from its write-ahead log into the database.'
            else:
                m = re.fullmatch(r'kv-(random|sequential)-(read|update)-(small|large)(?:-(single-transaction|per-op-commit))?', workload)
                if not m:
                    raise ValueError(f'new SQLite workload needs an explanation: {workload}')
                verb = {'read':'Read', 'update':'Update'}[m[2]]
                action = f'{verb} {"128-byte" if m[3]=="small" else "8192-byte"} values in {m[1]} order'
                action += {None:'.','single-transaction':' in one transaction.', 'per-op-commit':', committing after each update.'}[m[4]]
            journal = {'delete-extra':'a rollback journal and the EXTRA sync policy',
                       'wal-full':'a write-ahead log and the FULL sync policy',
                       'wal-normal':'a write-ahead log and the NORMAL sync policy'}[profile]
            yield case(f'sqlite:{fs}', f'{workload}/{profile}', f'SQLite on {fs}', f'{action.rstrip(".")} · {profile}',
                       f'Create a fresh {fs} filesystem on the test device. Configure SQLite to use {journal}.',
                       action + ' Compare the database, stop the device cleanly, restart it, and compare again without writing new data.',
                       f'All {repetitions} fresh runs must pass the filesystem and database integrity checks and match the independent reference database. This is not a power-loss test.')


FIO = {
    'sequential': ('Write blocks in address order', 'Write a known pattern in ascending block order.', 'Read the same pattern back.'),
    'random': ('Write blocks in random order', 'Write a known pattern at randomly selected addresses.', 'Every verified block must match the pattern.'),
    'overwrite': ('Replace a block repeatedly', 'Replace its contents eight times with different patterns.', 'Each read must return the replacement pattern.'),
    'graceful-restart': ('Read after a clean restart', 'Write known data, stop the service cleanly and restart it.', 'Read the original pattern from the restarted device.'),
    'sigkill-restart': ('Read after the process is killed', 'Write and flush the pattern, then kill the serving process and restart it.', 'Read the flushed pattern without relying on a clean shutdown.'),
    'descriptor-close-failpoint': ('Interrupt a checkpoint during shutdown', 'Pause shutdown at the checkpoint descriptor boundary, kill the process, and reopen the disk.', 'Read back the previously flushed pattern after restart.'),
    'exhaustion': ('Keep completed writes when space runs out', 'Write distinct blocks until the backing store cannot hold another write. Restart the device.', 'The client must receive a no-space error, and all completed blocks must read back correctly.'),
    'sustained-overwrite': ('Keep the last pattern as storage space is reused', 'Overwrite the same region until the log wraps. Verify after a clean restart and after killing and restarting the service.', 'The log must wrap and the last pattern must survive both restarts.'),
}


def scenario_cases(root, adr):
    lab = Path('rust/src/bin/block-storage-lab')
    yield from sqlite_cases(root, lab)
    fio = (root/lab/'ublk_fio/scenario.rs').read_text()
    actual = re.findall(r'Scenario::(\w+)', fio.split('const SCENARIOS:',1)[1].split('];',1)[0])
    labels = dict(re.findall(r'Self::(\w+) => "([\w-]+)"', fio))
    if {labels[x] for x in actual} != set(FIO):
        raise ValueError('fio scenario list changed: update its explanations')
    for slug,(title,action,expected) in FIO.items():
        yield case('make:test-ublk-fio', slug, 'Live Linux block device', title,
                   'Start the storage service with an isolated backing file and give fio its Linux block device.',
                   action, expected+' All three repetitions must pass.')
    for fs in ('ext4','xfs'):
        group = f'test-{fs}'
        yield case('make:'+group, group, 'Filesystem lifecycle', f'Reopen a populated {fs} filesystem',
                   f'Create and mount a disposable {fs} filesystem on the device.',
                   'Write known files, rename a file, unmount, stop the service cleanly, start it again and remount.',
                   'The directory entries and file contents must match. Offline filesystem checks and cleanup must succeed on all three fresh runs.')
    yield case('make:test-acceptance', 'test-acceptance', 'Process crash acceptance',
               'Run the crash scenarios in full acceptance mode', 'Use temporary backing files on Linux.',
               'Set the crash-test mode to acceptance and run the lab suite, including its expanded crash workloads.',
               'The full-mode gate must finish within its deadlines with no failed checks. This is a suite verdict; its ordinary Rust checks are not counted twice.')
    yield case('make:test-sqlite', 'test-sqlite', 'SQLite on ext4', 'Probe one SQLite clean restart',
               'Create a fresh ext4 database on the test device with a write-ahead log and FULL synchronization.',
               'Update values in seeded random order. Stop the device cleanly and reopen the database.',
               'The reopened database must pass integrity checks and match the reference. This single probe does not qualify a matrix.')
    yield case('script', 'tools/sqlite/test-standalone.sh', 'SQLite tools', 'Validate the standalone SQLite workload tools',
               'Use the pinned SQLite binaries and a private temporary directory. No test block device is involved.',
               'Exercise workload profiles and their error checks using ordinary database files.',
               'The probe must detect invalid settings and the functional checks must complete without error.')
    for slug in ['lose-discard','misdirect-write'] + [f'seed-{s}-{p}' for s in (19,701,20261119) for p in ('small','wide')]:
        bad = slug in ('lose-discard','misdirect-write')
        action = {'lose-discard':'Make previously discarded data reappear.', 'misdirect-write':'Send a write to the wrong logical block.'}.get(slug,
                  'Generate the repeatable client workload for this seed and cache profile. Reopen the volume and compare every block to the client history.')
        yield case('traces', slug, 'Client history replay', f'{"Detect a planted error" if bad else "Recover the recorded writes"} · {slug}',
                   'Use a fresh test volume and keep the expected block values outside it.',action,
                   'The checker must identify the planted mismatch in logical block 336 after the process is killed.' if bad else
                   'All 680 logical blocks must agree with the external history after each recovery.')
    yield case('manual', 'vm-boot-320m', 'Workbench VM prerequisites', 'Boot the 320 MiB test guest',
               'Use the prepared x86 Linux kernel, initial filesystem and KVM host from the workbench setup.',
               'Start the guest with one CPU and 320 MiB of RAM. Inside the guest, load the device modules and inspect its filesystems, memory and swap.',
               'The console must confirm the memory check, swap disabled, the block-device interface, ext4 and XFS. Booting does not run the database tests.')
    for mode in ('uniform','zipf'):
        yield case('manual', f'aged-fio-{mode}', 'Manual endurance qualification', f'Sustain repeated writes · {mode}',
                   'An operator must approve the physical write budget and provision a disposable, fully populated device.',
                   'Overwrite random blocks in measured windows. '+('Select addresses evenly.' if mode=='uniform' else 'Concentrate writes on popular addresses.'),
                   'Record whether throughput and write amplification stabilize within the specified window budget, without exhausting space or crashing. Do not run this test unattended.')
    for group, selector, anchor in [('seed', 'sweep','DST-01'),('seed','replay','DST-02')]+[
            ('guest',x,x) for prefix,nums in [('UBLK',range(1,5)),('CLOSE',range(1,4)),('PROBE',range(1,4)),('REOPEN',range(3,7)),('CHECK',[5])]
            for n in nums for x in [f'{prefix}-{n:02}']]:
        yield dict(id=f'{group}/{selector}', group=group, selector=selector, suite='Backing fault scenarios', **adr[anchor])


ANCHORS = {
    'fsync01_engine':'FSYNC-01C', 'fsync01_controls':'FSYNC-01B',
    'backing_enospc_at_checkpoint_b_final_fsync_must_latch':'SEAM-08',
    'pre_io_capacity_enospc_keeps_volume_usable':'SEAM-07',
    'completed_fsync_can_report_uncertain_completion_and_latch_volume':'SEAM-06',
    'writev_short_releases_no_bytes_and_charges_existing_failure_path':'SEAM-05',
    'replay_cursor_skips_intact_old_metadata_but_not_damaged_framing':'REOPEN-10',
    'falls_back_from_corrupted_newest_descriptor_and_replays_tail':'REOPEN-07',
    'falls_back_from_corrupted_newest_index_page_and_replays_tail':'REOPEN-08',
    'rejects_two_unusable_checkpoint_roots_without_panic_or_mutation':'REOPEN-09',
}


def inventory(root):
    root = Path(root).resolve()
    files = tracked(root)
    adr = adr_descriptions(root)
    rows = [*rust_cases(root, files), *python_cases(root, files), *scenario_cases(root, adr)]
    for row in rows:
        name = row.get('source_key','').split('::')[-1]
        if name and ANCHORS.get(name,name) in adr:
            row.update(adr[ANCHORS.get(name,name)])
    if len({r['id'] for r in rows}) != len(rows):
        raise ValueError('duplicate test ID: cannot attribute a verdict')
    return sorted(rows, key=lambda r:r['id'])
