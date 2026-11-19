"""Generate, replay and reduce serialized format-4 client histories on Linux."""

import argparse
import json
from pathlib import Path
import random
import re
import shutil
import signal
import subprocess
import time

LBAS = 680
INITIAL_BYTES = (2 + 32 * 32) * 4096
GROWN_BYTES = INITIAL_BYTES + 4 * 32 * 4096
MAX_EPOCHS = 8
MAX_OPS = 32
HOT = (0, 1, 335, 336, 337, 671, 672, 679)


def check_trace(trace):
    if trace['profile'] not in ('small', 'wide') or trace['plant'] not in ('none', 'lose-discard', 'misdirect-write'):
        raise ValueError('unknown profile or planted fault')
    if not 1 <= len(trace['epochs']) <= MAX_EPOCHS:
        raise ValueError('wrong number of epochs')
    for epoch in trace['epochs']:
        if not 1 <= len(epoch) <= MAX_OPS + 1 or epoch[-1] not in ('kill', 'close'):
            raise ValueError('invalid epoch length or cut')
        if epoch[-1] == 'kill' and epoch[-2:] != ['f', 'kill']:
            raise ValueError('a kill must immediately follow a flush')
        for op in epoch[:-1]:
            if op in ('f', 'g', 'x'):
                continue
            if match := re.fullmatch(r'd([0-9]{1,3})|w([0-9]{1,3}):([0-9]{1,3})', op):
                if int(match[1] or match[2]) < LBAS and (not match[3] or 1 <= int(match[3]) <= 255):
                    continue
            raise ValueError(f'invalid operation: {op!r}')


def generate(seed):
    rng = random.Random(seed)
    result = []
    for epoch in range(4):
        ops = ['x'] if epoch != 2 else ['g']
        for _ in range(14):
            lba = rng.choice(HOT)
            ops.append(rng.choice([f'w{lba}:{rng.randint(1,255)}', f'w{lba}:{rng.randint(1,255)}', f'd{lba}', 'f']))
        ops += ['f', 'kill'] if epoch % 2 == 0 else ['close']
        result.append(ops)
    return result


def save(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')


def execute(args, output):
    with output.open('x') as log:
        subprocess.run(args, check=True, timeout=12, stdout=log, stderr=subprocess.STDOUT)


def kill_after_flush(args, image, output):
    ready = image.with_suffix('.ready')
    with output.open('x') as log, subprocess.Popen(args, stdout=log, stderr=subprocess.STDOUT) as child:
        try:
            deadline = time.monotonic() + 10
            while not ready.exists():
                if child.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f'flush cut not reached; see {output}')
                time.sleep(.005)
        finally:
            child.kill()
            result = child.wait(timeout=2)
        if result != -signal.SIGKILL:
            raise RuntimeError(f'wrong cut status: {result}')
        ready.unlink()


def issued(ops, plant):
    """Only the target gets this transformation, never the logical oracle."""
    actual = []
    for op in ops:
        if plant == 'lose-discard' and op.startswith('d'):
            continue
        if plant == 'misdirect-write' and op.startswith('w'):
            lba, value = op[1:].split(':')
            op = f'w{(int(lba) + 1) % LBAS}:{value}'
        actual.append(op)
    return actual


def update(expected, ops):
    for op in ops:
        if op.startswith('w'):
            lba, value = op[1:].split(':')
            expected[int(lba)] = int(value)
        elif op.startswith('d'):
            expected[int(op[1:])] = 0


def replay(binary, trace, work):
    """Return a stable failure signature, not a boolean that also accepts a setup error."""
    check_trace(trace)
    work.mkdir(mode=0o700)
    save(work / 'trace.json', trace)
    image = work / 'backing.img'
    profile = trace['profile']
    execute([binary, 'format', profile, image], work / 'format.txt')
    expected = bytearray(LBAS)
    size = INITIAL_BYTES
    for n, epoch in enumerate(trace['epochs']):
        ops, cut = epoch[:-1], epoch[-1]
        actual = issued(ops, trace['plant'])
        log = work / f'{n}-{cut}.txt'
        args = [binary, cut, profile, image, *actual]
        if cut == 'kill':
            kill_after_flush(args, image, log)
        else:
            execute(args, log)
        acks = [line[4:] for line in log.read_text().splitlines() if line.startswith('ack=')]
        if acks != actual:
            raise RuntimeError(f'incomplete operation history: {log}')
        update(expected, ops)
        if 'g' in ops:
            size = GROWN_BYTES
        # Scan a copy: reading a dirty mapping page or opening an image may write.
        copy = work / 'recovery.img'
        shutil.copyfile(image, copy)
        scan = work / f'{n}-scan.txt'
        try:
            execute([binary, 'scan', profile, copy], scan)
        except subprocess.CalledProcessError as error:
            lines = scan.read_text().splitlines()
            if error.returncode != 1 or not lines or not lines[-1].startswith('Error: '):
                raise
            phase = 'recover-read' if lines[0].startswith('open ') else 'reopen'
            return [phase, cut, lines[-1]]
        header, state = scan.read_text().splitlines()
        if header.split()[-1] != f'provisioned={size}':
            return ['growth', cut]
        prefix, value = state.split('=', 1)
        recovered = bytes.fromhex(value)
        if prefix != 'state' or len(recovered) != LBAS:
            raise ValueError(f'incomplete scan: {scan}')
        for lba, (seen, wanted) in enumerate(zip(recovered, expected)):
            if seen != wanted:
                save(work / 'mismatch.json', {'epoch': n, 'cut': cut, 'lba': lba, 'expected': wanted, 'read': seen})
                return ['logical', cut, lba]
        copy.unlink()
    image.unlink()
    return None


def reduce_trace(binary, trace, signature, work):
    """Try one-pass command deletion; keep the epoch boundaries and failure class/LBA."""
    epochs = [epoch.copy() for epoch in trace['epochs']]
    for e, ops in enumerate(epochs):
        # Keep a kill's final successful flush and never delete a cut.
        for i in reversed(range(len(ops) - (2 if ops[-1] == 'kill' else 1))):
            candidate = ops[:i] + ops[i+1:]
            trial = dict(trace, epochs=epochs[:e] + [candidate] + epochs[e+1:])
            tmp = work / 'candidate'
            result = replay(binary, trial, tmp)
            shutil.rmtree(tmp)  # owned by replay; unexpected exceptions retain the fixture
            if result == signature:
                ops.pop(i)
    reduced = dict(trace, epochs=epochs)
    if replay(binary, reduced, work / 'reduced') != signature:
        raise RuntimeError('reduction did not replay')
    return reduced


def run(binary, root, name, trace, planted_gate=False):
    case = root / name
    signature = replay(binary, trace, case)
    if signature:
        reduced = reduce_trace(binary, trace, signature, case)
        print(f'{name} detected={signature} reduced_epochs={reduced["epochs"]}', flush=True)
    if planted_gate:
        if signature != ['logical', 'kill', 336]:
            raise RuntimeError(f'{name}: planted fault missed or detected the wrong violation')
    elif signature:
        return False
    else:
        print(f'{name} profile={trace["profile"]} epochs={len(trace["epochs"])} verified_lbas_per_epoch={LBAS} PASS', flush=True)
    return True


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('binary', type=Path)
    p.add_argument('new_results_dir', type=Path)
    p.add_argument('--replay', type=Path, help='saved trace.json; exits nonzero for observed mismatches')
    args = p.parse_args()
    binary, root = args.binary.resolve(strict=True), args.new_results_dir.resolve()
    root.mkdir(mode=0o700, exist_ok=False)
    if args.replay:
        with args.replay.open('rb') as f:
            data = f.read(16_385)
        if len(data) > 16_384:
            raise ValueError('trace exceeds 16 KiB')
        if not run(binary, root, 'replay', json.loads(data)):
            raise SystemExit(1)
        return
    for plant, ops in [('lose-discard', ['x','w336:17','d336','g']), ('misdirect-write', ['f','w336:17','x','g'])]:
        run(binary, root, plant, dict(profile='small', plant=plant, epochs=[ops+['f','kill']]), planted_gate=True)
    passed = True
    for seed in (19, 701, 20261119):
        for profile in ('small', 'wide'):
            passed &= run(binary, root, f'seed-{seed}-{profile}', dict(seed=seed, profile=profile, plant='none', epochs=generate(seed)))
    if not passed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
