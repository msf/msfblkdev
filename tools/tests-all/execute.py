"""Collect individual test verdicts. A successful suite is not a per-test result."""
import json
import os
import re
import selectors
import shlex
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

LIMIT = 4 * 1024 * 1024


def command(cwd, argv, log, seconds=600, env=None):
    """Drain the pipe while enforcing both time and output limits, including children."""
    argv = [str(x) for x in argv]
    data = bytearray()
    deadline = time.monotonic() + seconds
    note = ''
    proc = None
    rc = None
    try:
        # Never let a parent-process test enter one of its child-only modes or
        # open a caller-provided crash image. Preserve toolchain paths and TMPDIR.
        inherited = {k: v for k, v in os.environ.items() if not k.startswith('BLOCK_STORAGE_')
                     and k not in ('SEED', 'PLAN', 'OUT', 'DST_ROOT')}
        proc = subprocess.Popen(argv, cwd=cwd, env={**inherited,
                                'PYTHONDONTWRITEBYTECODE': '1', 'NO_COLOR': '1', **(env or {})},
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                stdin=subprocess.DEVNULL, start_new_session=True)
        with proc.stdout, selectors.DefaultSelector() as poll:
            poll.register(proc.stdout, selectors.EVENT_READ)
            while True:
                left = deadline - time.monotonic()
                if left <= 0 or not poll.select(left):
                    raise TimeoutError
                part = os.read(proc.stdout.fileno(), 65536)
                if not part:
                    break
                room = LIMIT - len(data)
                data.extend(part[:room])
                if len(part) > room:
                    raise OverflowError
        rc = proc.wait(timeout=max(0, deadline-time.monotonic()))
    except (subprocess.TimeoutExpired, TimeoutError):
        note = f'Deadline expired after {seconds}s.'
    except OverflowError:
        note = 'Command exceeded the log size limit.'
    except OSError as exc:
        note = str(exc)
    finally:
        if proc is not None and rc is None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            proc.wait()
        text = data.decode('utf-8', errors='replace')
        log.write_text(f'$ {shlex.join(argv)}\n{text}\nexit={rc} {note}\n', encoding='utf-8')
    reason = note or f'Command exited {rc}.'
    return rc, text, f'{reason} Log: {log.name}'


def result(status, detail):
    return {'status': status, 'detail': detail}


def python_worker(directory, path):
    """Write verdicts independently of verbose output, docstrings and subtest messages."""
    class Recorder(unittest.TextTestResult):
        def save(self, test, state, reason):
            with path.open('a', encoding='utf-8') as stream:
                stream.write(json.dumps([test.id(), result(state, reason)]) + '\n')

        def addSuccess(self, test):
            super().addSuccess(test)
            self.save(test, 'passed', 'All test assertions passed.')

        def addError(self, test, err):
            super().addError(test, err)
            self.save(test, 'blocked' if 'libxxhash unavailable' in str(err[1]) else 'failed', str(err[1])[:300])

        def addFailure(self, test, err):
            super().addFailure(test, err)
            self.save(test, 'failed', str(err[1])[:300])

        def addSubTest(self, test, subtest, err):
            super().addSubTest(test, subtest, err)
            if err is not None:
                self.save(test, 'failed', str(err[1])[:300])

        def addSkip(self, test, reason):
            super().addSkip(test, reason)
            self.save(test, 'not run', reason[:300])

        def addExpectedFailure(self, test, err):
            super().addExpectedFailure(test, err)
            self.save(test, 'passed', 'The expected failure occurred.')

        def addUnexpectedSuccess(self, test):
            super().addUnexpectedSuccess(test)
            self.save(test, 'failed', 'Unexpected success.')

    tests = unittest.defaultTestLoader.discover(directory)
    return unittest.TextTestRunner(verbosity=2, resultclass=Recorder).run(tests).wasSuccessful()


def parse_rust(text):
    return {re.sub(r'(?<=\)) - (?:compile fail|compile|should panic)$', '', m[1]): result({'ok': 'passed', 'FAILED': 'failed', 'ignored': 'not run'}[m[2]], m[0])
            for m in re.finditer(r'^test (.+?) \.\.\. (ok|FAILED|ignored)(?:, [^\n]*)?$', text.split('\nfailures:\n', 1)[0], re.MULTILINE)}


def assign(cases, verdicts, detail):
    found = {}
    for case in cases:
        key = case['selector']
        if key in verdicts:
            v = verdicts[key]
            found[case['id']] = result(v['status'], v['detail'] + '. ' + detail)
        else:
            found[case['id']] = result('blocked', 'No unique individual verdict. ' + detail)
    return found


def outcome(rc, text, detail):
    if rc == 0:
        return result('passed', detail)
    prereq = re.search(r'(?im)^.*(?:No space left on device|StorageFull|AWAITING OPERATOR|command not found|could not compile|could not find|no matching package named|requires Linux|/dev/ublk-control is missing|missing tool:).{0,180}', text)
    if prereq or rc is None:
        return result('blocked', (prereq[0].strip()[:220] + '. ' if prereq else '') + detail)
    return result('failed', (failure_summary(text) + ' ' + detail).strip())


def failure_summary(text):
    for pattern in (r'^Error: .+', r'^assertion .+', r'^.*panicked at .+', r'^(?:FAIL|FAILED|TIMEOUT|RuntimeError|ValueError|AssertionError|block-storage-lab):? .+'):
        match = re.search(pattern, text, re.MULTILINE)
        if match:
            return match[0][:250]
    return ''


def evidence_file(root, console, before):
    # Use only a newly created path announced by this invocation; never a stale passing log.
    paths = re.findall(r'^evidence: (.+)$', console, re.MULTILINE)
    if not paths:
        return None
    path = Path(paths[-1]).resolve()
    if path.parent != (root / 'evidence').resolve() or path.name in before or not path.exists():
        return None
    return path


def repetitions(done, failed, keys, detail):
    counts = [k in done for k in keys]
    state = 'failed' if any(k in failed for k in keys) else 'passed' if all(counts) else 'blocked'
    return result(state, f'{sum(counts)}/{len(keys)} repetitions passed. ' + detail)


class Runner:
    def __init__(self, root, out):
        self.root = root.resolve()
        self.out = out.resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        self.count = 0

    def run(self, argv, cwd=None, seconds=600, env=None):
        self.count += 1
        log = self.out / f'{self.count:03d}-{Path(argv[0]).name}.log'
        return command(cwd or self.root, argv, log, seconds, env)

    def seeds(self, key):
        runs = range(32) if key == 'sweep' else (42, 42, None)
        folders = []
        for i, seed in enumerate(runs):
            dest = self.out / f'{key}-{i}'
            folders.append(dest)
            env = {'DST_ROOT': str(self.root), 'OUT': str(dest)}
            if seed is None:
                env['PLAN'] = str(folders[0] / 'plan.txt')
            else:
                env['SEED'] = str(seed)
            rc, text, detail = self.run(['cargo','test','--locked','--lib','tests::seeded_dst::replay_from_environment','--','--exact','--ignored'], self.root/'rust', env=env)
            if rc != 0:
                return outcome(rc,text,detail)
            if not (dest/'trace.txt').is_file() or 'PASS epochs=4' not in (dest/'trace.txt').read_text().splitlines():
                return result('failed', 'Missing four-round success marker. ' + detail)
        if key != 'sweep' and not all(len({(d/name).read_bytes() for d in folders}) == 1 for name in ('plan.txt','trace.txt')):
            return result('failed','Generated plans or recorded traces differed.')
        return result('passed', ('32 seeds, four verified rounds per seed.' if key == 'sweep' else 'Two generated runs and a saved-plan replay matched byte for byte.') + ' ' + detail)

    def lab(self, group, cases):
        key = group.split(':', 1)[1]
        sqlite = group.startswith('sqlite:')
        fs = key if sqlite else {'test-ext4':'ext4','test-xfs':'xfs','test-sqlite':'ext4'}.get(key)
        if fs:
            preflight = f'check-sqlite-{fs}' if sqlite or key == 'test-sqlite' else f'check-{fs}'
            rc, _, detail = self.run(['make', preflight])
            if rc != 0:
                return self.all(cases,'blocked', 'Filesystem/tool preflight did not pass. '+detail)
        target = f'test-sqlite-{fs}' if sqlite else key
        before = {p.name for p in (self.root/'evidence').iterdir()}
        rc, text, detail = self.run(['make',target], seconds=4000)
        raw = evidence_file(self.root, text, before)
        if not raw:
            return self.all(cases,'blocked','No fresh lab evidence (preflight or build failed). '+detail)
        detail += f' Raw evidence: evidence/{raw.name}.'
        if sqlite:
            manifest = raw / 'manifest.json'
            if not manifest.is_file():
                return self.all(cases,'blocked','No SQLite case manifest. '+detail)
            m = json.loads(manifest.read_text())
            if m.get('filesystem','').lower() != fs or m.get('schema') != 'block-storage-sqlite-suite-index':
                raise ValueError('Unrecognized SQLite manifest.')
            return {c['id']:repetitions(m['completed_selections'],m['failed_selections'],
                    [c['selector'].replace('/','--')+f'--repetition-{i}' for i in range(1,4)],detail) for c in cases}
        if key == 'test-ublk-fio':
            passed, failed = set(), set()
            active = None
            with raw.open(errors='replace') as f:
                for line in f:
                    match = re.fullmatch(r'PASS scenario=([\w-]+) repetition=(\d+)\n',line)
                    start = re.fullmatch(r'=== scenario=([\w-]+) repetition=(\d+)/3 ===\n', line)
                    if start:
                        active = start.groups()
                    if match:
                        passed.add(match.groups())
                    if line.startswith('result: FAIL ') and active and active not in passed:
                        failed.add(active)
                        detail += ' ' + line.strip()[:250]
            return {c['id']:repetitions(passed,failed,[(c['selector'],str(i)) for i in range(1,4)],detail) for c in cases}
        return {c['id']:outcome(rc,text,detail) for c in cases}

    @staticmethod
    def all(cases,status,detail):
        return {c['id']:result(status,detail) for c in cases}

    def group(self, group, cases):
        if group.startswith('python:'):
            directory = group.split(':',1)[1]
            events = self.out / f'python-{self.count+1}.jsonl'
            rc,text,detail = self.run([sys.executable,'-B',Path(__file__).resolve(),directory,events])
            observed = dict(json.loads(line) for line in events.read_text().splitlines()) if events.is_file() else {}
            return assign(cases,observed,detail)
        if group == 'manual':
            return self.all(cases,'not run','Requires a separate operator-controlled procedure.')
        if sys.platform != 'linux' and not group.startswith('rust:tools/blockcheck:'):
            return self.all(cases,'blocked','Requires a Linux test host.')
        if group.startswith('rust:'):
            _,crate,target = group.split(':',2)
            args = ['cargo','test','--locked',target]
            if target == '--bin=block-storage-lab':
                args += ['--features','lab']
            rc,text,detail = self.run(args+['--','--test-threads=1'],self.root/crate,seconds=1800)
            observed = parse_rust(text)
            for name,v in observed.items():
                # Only attribute fixture errors from this test's own failure output.
                section = re.search(r'^---- '+re.escape(name)+r' stdout ----\n(.*?)(?=^---- |^failures:|\Z)',text,re.MULTILINE|re.DOTALL)
                if v['status'] == 'failed' and section:
                    summary = failure_summary(section[1])
                    if summary:
                        v['detail'] = summary
                    if 'No space left on device' in section[1]:
                        v.update(status='blocked',detail='The test could not complete: no space left on device.')
            return assign(cases,observed,detail)
        if group in ('make:test-acceptance','make:test-compaction-small'):
            key = group.split(':')[1]
            rc,text,detail = self.run(['make',key],seconds=3700 if key=='test-acceptance' else 900)
            v = outcome(rc,text,detail)
            if key=='test-compaction-small' and rc==0 and not re.search(r'test result: ok\. 1 passed;', text):
                v=result('blocked','Ignored test has no execution verdict. '+detail)
            return {c['id']:v for c in cases}
        if group in ('seed','seeds'):
            return {c['id']:self.seeds(c['selector']) for c in cases}
        if group == 'traces':
            rc,text,detail = self.build(['--example','adr05-traces'])
            if rc != 0:
                return self.all(cases,'blocked','Cannot build trace driver. '+detail)
            rc,text,detail=self.run([sys.executable,'-B','tools/adr05-fault-lab/traces/run.py',self.bin('examples/adr05-traces'),self.out/'traces'],seconds=600)
            observations = {}
            for case in cases:
                slug = case['selector']
                line = re.search(r'^'+re.escape(slug)+r' ([^\n]+)$',text,re.MULTILINE)
                if not line:
                    observations[case['id']] = result('blocked','No individual completion marker. '+detail)
                    continue
                success = (line[1].endswith(' PASS') or (slug in ('lose-discard','misdirect-write') and
                           re.fullmatch(r"detected=\['logical', 'kill', 336\] reduced_epochs=\d+",line[1])))
                observations[case['id']] = result('passed' if success else 'failed',line[0][:220]+'. '+detail)
            return observations
        if group == 'script':
            build = self.root/'tools/build/sqlite-3.51.3/build'
            if not all(os.access(build/name,os.X_OK) for name in ('sqlite3','kvtest','speedtest1')):
                return self.all(cases,'blocked','The pinned SQLite tools have not been built.')
            rc,text,detail=self.run(['bash','tools/sqlite/test-standalone.sh',self.out/'sqlite-standalone.log'])
            return {c['id']:outcome(rc,text,detail) for c in cases}
        if group in ('make:test-ublk-fio','make:test-sqlite','make:test-ext4','make:test-xfs','sqlite:xfs','sqlite:ext4','guest'):
            if os.geteuid()==0 or os.environ.get('TESTS_ALL_LAB')!='1':
                return self.all(cases,'blocked','Requires TESTS_ALL_LAB=1 as a normal user on a prepared disposable Linux guest.')
            if group=='guest':
                return self.guest(cases)
            return self.lab(group,cases)
        raise ValueError(f'No runner for inventory group {group!r}')

    def build(self,args):
        return self.run(['cargo','build','--locked',*args],self.root/'rust')

    def bin(self,name):
        path = self.root/'rust'/os.environ.get('CARGO_TARGET_DIR','target')/'debug'/name
        return path.resolve()

    def guest(self, cases):
        scratch, device = os.environ.get('SCRATCH'), os.environ.get('DEVICE')
        if not scratch or not device:
            return self.all(cases,'blocked','Set SCRATCH and DEVICE to an approved guest scratch directory and its separate ext4 device.')
        lab = self.root/'tools/adr05-fault-lab'
        rc,_,detail = self.run([sys.executable,'-B','-c',
            'from pathlib import Path; import sys; from qsd import check_scratch; check_scratch(Path(sys.argv[1]),Path(sys.argv[2]),64<<20)',scratch,device],lab)
        if rc!=0:
            return self.all(cases,'blocked','Scratch safety preflight failed. '+detail)
        rc,_,detail=self.build(['--features','test-failpoints','--bin','block-storage-ublk','--example','adr05-engine','--example','adr05-model-image'])
        if rc!=0:
            return self.all(cases,'blocked','Cannot build the fault drivers. '+detail)
        observed = {}
        def run_cases(script, args, names, pattern):
            rc,text,detail = self.run([sys.executable,'-B','-u',script,*args],seconds=150)
            for key,marker in names.items():
                if re.search(pattern.format(marker=marker),text,re.MULTILINE):
                    observed[key]=result('passed', detail)
                else:
                    observed[key]=result('blocked','Missing individual completion marker. '+detail)
            return rc,text,detail
        exe=self.bin('examples/adr05-engine')
        probe=self.out/'probe-io'
        rc,_,detail=self.run(['cc','-std=c11','-Wall','-Wextra','-Werror','-O2','-o',probe,lab/'probe-io.c','-luring'])
        if rc==0:
            # The original driver uses a binary beside its source. Override only that
            # path: keep the worktree clean and run the same preflight and assertions.
            probe_code="import runpy,sys; from pathlib import Path; m=runpy.run_path('guest-probe.py'); m['main'].__globals__['CALLER']=Path(sys.argv.pop(1)); m['main']()"
            rc,text,detail=self.run([sys.executable,'-B','-c',probe_code,probe,scratch,device],lab,seconds=150)
            for i,name in enumerate(('normal','write','fsync'),1):
                observed[f'PROBE-0{i}']=result('passed' if re.search(f'^case={name} base_match=1 .*result=PASS$',text,re.MULTILINE) else 'blocked',detail)
        else:
            for i in range(1,4):
                observed[f'PROBE-0{i}']=result('blocked','Unable to build the syscall probe. '+detail)
        run_cases(lab/'engine-checkpoint.py',[scratch,device,exe],{'CLOSE-01':'control','CLOSE-02':'descriptor-eio'},r'^case={marker} result=PASS ')
        miss_code="import runpy; m=runpy.run_path('engine-checkpoint.py'); m['main'].__globals__['DESCRIPTOR_SECTOR']=3000; m['main']()"
        rc,text,detail=self.run([sys.executable,'-B','-c',miss_code,scratch,device,exe],lab,seconds=150)
        observed['CLOSE-03']=result('passed' if rc not in (0,None) and 'checkpoint close missed EIO' in text else 'failed','Expected the checker to report the missed error. '+detail)
        run_cases(lab/'model-engine/record-cuts.py',[scratch,device,self.bin('examples/adr05-model-image')],dict(zip(('REOPEN-03','REOPEN-04','REOPEN-05','REOPEN-06'),('neither','payload','footer','both'))),r'^case={marker} lba=7 byte=0x00 verified_bytes=4096$')
        dest=self.out/'live-images'
        rc,text,detail=run_cases(lab/'live/checkpoint_b.py',[scratch,device,exe.parents[1],dest],{'UBLK-01':'control','UBLK-02':'write','UBLK-03':'fsync'},r'^case {marker} PASS$')
        audits=[]
        for name in ('write','fsync'):
            m=re.search(f'^case {name} fixture (.+?) evidence ',text,re.MULTILINE)
            if not m or f'case {name} PASS' not in text:
                break
            path=Path(m[1]).resolve()/'image'
            if path.parent.parent!=Path(scratch).resolve() or path.is_symlink():
                break
            arc,_,adetail=self.run([sys.executable,'-B',lab/'checker/check.py',path],seconds=15)
            audits.append(outcome(arc,'',adetail))
        observed['CHECK-05']=(result('blocked','Both live fault images are required. '+detail) if len(audits)!=2 else result('passed' if all(a['status']=='passed' for a in audits) else 'failed',' '.join(a['detail'] for a in audits)))
        dest=self.out/'miss-image'
        rc,text,detail=self.run([sys.executable,'-B',lab/'live/checkpoint_b.py',scratch,device,exe.parents[1],dest,'--case','miss'],seconds=150)
        log=dest/'miss/client.log'
        detected=log.is_file() and 'ValueError: expected EIO from the in-flight ublk write' in log.read_text(errors='replace')
        observed['UBLK-04']=result('passed' if detected and rc not in (0,None) else 'blocked' if not log.is_file() else 'failed','Expected the client to detect a missing injection. '+detail)
        return {c['id']:observed[c['selector']] for c in cases}


def execute(root, cases, out):
    runner = Runner(root, out)
    groups = {}
    for c in cases:
        groups.setdefault(c['group'], []).append(c)
    results = {}
    for group, entries in groups.items():
        print(f'Running {group} ({len(entries)} criteria)',flush=True)
        try:
            results.update(runner.group(group,entries))
        except (OSError, ValueError, KeyError) as exc:
            results.update(runner.all(entries,'blocked',f'Runner could not collect results: {exc}'))
    return results


if __name__ == '__main__':
    sys.exit(0 if python_worker(sys.argv[1], Path(sys.argv[2])) else 1)
