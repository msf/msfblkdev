#!/usr/bin/env python3
"""Real ublk write -> reclaim checkpoint B -> one blkdebug fault. Disposable VM only."""
import argparse
from contextlib import contextmanager
import errno
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
from qsd import check_scratch

BLOCK = 4096
SEGMENT = 32 * BLOCK  # The geometry formatted by adr05-engine format-new.


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


@contextmanager
def child(argv, log, **kwargs):
    print('exec', argv, flush=True)
    with log.open('xb') as output:
        proc = subprocess.Popen(argv, stdout=output, stderr=subprocess.STDOUT, **kwargs)
        try:
            yield proc
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(4)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(3)
                    raise


def wait(proc, predicate, label):
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        require(proc.poll() is None, f'{label}: child exited {proc.returncode}')
        time.sleep(.02)
    raise TimeoutError(label)


def qmp_command(io, name, **args):
    io.write((json.dumps({'execute': name, 'arguments': args}) + '\n').encode())
    reply = json.loads(io.readline(65536))
    require('return' in reply, f'QMP {name}: {reply}')
    print('QMP', name, args, reply, flush=True)


def arm(qmp, slot, case):
    debug = {'driver': 'blkdebug', 'node-name': 'armed', 'image': 'base', 'discard': 'unmap'}
    if case != 'control':
        rule = {'once': True, 'errno': errno.EIO}
        if case in ('write', 'miss'):
            rule.update(event='write_aio', iotype='write', sector=2**31 if case == 'miss' else 8 * slot)
        else:
            rule.update(event='flush_to_disk', iotype='flush')
        debug['inject-error'] = [rule]
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(5)
        sock.connect(str(qmp))
        with sock.makefile('rwb', buffering=0) as io:
            require('QMP' in json.loads(io.readline(65536)), 'not a QMP greeting')
            qmp_command(io, 'qmp_capabilities')
            qmp_command(io, 'blockdev-add', **debug)
            qmp_command(io, 'blockdev-reopen', options=[{'driver': 'raw', 'node-name': 'raw',
                                                        'discard': 'unmap', 'file': 'armed'}])


def victim_digest(image, segment):
    # Count block allocation as well as bytes. A zero-filled extent is not a hole.
    start = 2 * BLOCK + segment * SEGMENT
    with image.open('rb') as f:
        f.seek(start)
        data = f.read(SEGMENT)
        require(len(data) == SEGMENT, 'short victim')
        allocated = []
        for i in range(32):
            offset = start + i * BLOCK
            try:
                present = os.lseek(f.fileno(), offset, os.SEEK_DATA) == offset
            except OSError as exc:
                if exc.errno != errno.ENXIO:
                    raise
                present = False
            allocated.append('1' if present else '0')
    return hashlib.sha256(data).hexdigest(), ''.join(allocated)


def pause(listener, qmp, image, case, armed):
    listener.settimeout(12)
    with listener.accept()[0] as gate:
        gate.settimeout(5)
        with gate.makefile('rb') as io:
            lines = io.read(1024).decode().splitlines()
        m = re.fullmatch(r'slot=([01]) lsn=(\d+)', lines[0])
        require(m is not None and len(lines) > 1, f'malformed B marker: {lines}')
        victims = {}
        for line in lines[1:]:
            v = re.fullmatch(r'victim=(\d+)', line)
            require(v is not None and int(v[1]) in range(12), 'invalid victim')
            segment = int(v[1])
            victims[segment] = victim_digest(image, segment)
        print('gate', lines, 'victims_before', victims, flush=True)
        arm(qmp, int(m[1]), case)
        trace_start = (armed.parent/'qemu.log').stat().st_size
        armed.touch(exist_ok=False)
        gate.sendall(b'G')
    return victims, int(m[1]), trace_start


def check_publication(save, case, server_log, victims, slot, start):
    # On the faulted volume there must be no later write, even of identical bytes.
    with (save/'qemu.log').open('rb') as f:
        f.seek(start)
        writes = re.findall(rb' offset (\d+) bytes (\d+) flags ', f.read())
    require(writes and tuple(map(int, writes[0])) == (slot * BLOCK, BLOCK),
            f'first armed QEMU write not the descriptor: {writes}')
    freed = re.findall(r' reclaim_segments_freed=(\d+)\b', server_log)
    issued = re.findall(r' reclaim_discards_issued=(\d+)\b', server_log)
    if case == 'control':
        require(freed == [str(len(victims))] and len(issued) == 1 and int(issued[0]) > 0,
                'control must actually free and discard')
    else:
        require(len(writes) == 1 and freed == ['0'] and issued == ['0'],
                'write, release or discard after B failed')
        errors = re.findall(r'op=write .*start_sector=40 nr_sectors=8 completion=-5 cancelled=false', server_log)
        require(len(errors) == 1, 'expected one failed live write completion')
    print(f'post_gate_write_count={len(writes)} first_write={slot*BLOCK} freed={freed} discards={issued}', flush=True)


def run_daemon(work, save, image, case, qmp, daemon, gate):
    argv = [daemon, 'serve', work/'export', '-1', '--write-buffer-bytes', '4096',
            '--idle-wait-us', '30000000', '--max-maintenance-interval-ms', '30000']
    env = dict(os.environ, BLOCK_STORAGE_TEST_FAILPOINT='live-checkpoint-b',
               ADR05_GATE_SOCKET=str(save/'gate'), BLOCK_STORAGE_LOG='debug')
    before = set(Path('/dev').glob('ublkb*'))
    log = save/'daemon.log'
    with child(argv, log, env=env) as server:
        match = wait(server, lambda: re.search(r'^(/dev/ublkb\d+)$', log.read_text(), re.M), 'ublk readiness')
        node = Path(match[1])
        require(node not in before, 'device existed before daemon startup')
        wait(server, lambda: node.exists() and node.stat().st_uid == os.getuid(), 'udev ownership')
        armed = save/'armed'
        args = [sys.executable, '-u', HERE/'client.py', node, armed, case]
        with child(args, save/'client.log') as client:
            victims, slot, start = pause(gate, qmp, image, case, armed)
            require(client.wait(8) == 0, 'unexpected live client result')
        if case != 'control':
            require(server.wait(8) != 0, 'failed daemon must report its error')
    require(server.returncode == 0 if case == 'control' else server.returncode != 0, 'unexpected daemon status')
    require(not node.exists(), 'daemon device still present')
    text = log.read_text()
    expected = [('descriptor-write', '5')] if case == 'write' else [
        ('descriptor-write', '0'), ('final-fsync', '5' if case == 'fsync' else '0')]
    actual = re.findall(r'^live-checkpoint-b op=(\S+) errno=Some\((\d+)\)$', text, re.M)
    require(actual == expected, f'wrong engine event {actual} expected {expected}')
    check_publication(save, case, text, victims, slot, start)
    return victims


def run_case(scratch, save, case, binaries):
    helper, daemon = binaries/'examples/adr05-engine', binaries/'block-storage-ublk'
    work = Path(tempfile.mkdtemp(prefix=f'live-b-{case}-', dir=scratch))
    save.mkdir()
    print('case', case, 'fixture', work, 'evidence', save, flush=True)
    image, export, qmp = work/'image', work/'export', save/'qmp'
    subprocess.run([helper, 'format-new', image], check=True, timeout=10)
    export.touch(exist_ok=False)
    dev = export.stat().st_dev
    args = ['qemu-storage-daemon', '--trace', 'enable=blk_co_pwritev', '--chardev',
            f'socket,id=c,path={qmp},server=on,wait=off', '--monitor', 'chardev=c',
            '--blockdev', json.dumps({'driver': 'file', 'node-name': 'base', 'filename': str(image),
                                      'cache': {'direct': True, 'no-flush': False}, 'discard': 'unmap'}),
            '--blockdev', 'driver=raw,node-name=raw,file=base,discard=unmap', '--export',
            json.dumps({'type': 'fuse', 'id': 'e', 'node-name': 'raw', 'writable': True,
                        'allow-other': 'off', 'mountpoint': str(export)})]
    with socket.socket(socket.AF_UNIX) as gate, child(args, save/'qemu.log') as qsd:
        gate.bind(str(save/'gate'))
        gate.listen(1)
        wait(qsd, lambda: export.stat().st_dev != dev, 'FUSE mount')
        victims = run_daemon(work, save, image, case, qmp, daemon, gate)
    require(qsd.returncode == 0 and export.stat().st_dev == dev, 'unclean QEMU shutdown')
    after = {v: victim_digest(image, v) for v in victims}
    print('victims_after', after, flush=True)
    require((victims == after) == (case != 'control'), 'unexpected victim byte/allocation change')
    copy = work/'recovery-copy'
    shutil.copyfile(image, copy)
    subprocess.run([helper, 'verify-copy', copy], check=True, timeout=10)
    print('case', case, 'PASS', flush=True)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('scratch', type=Path)
    p.add_argument('device', type=Path)
    p.add_argument('binaries', type=Path, help='target/debug built with test-failpoints')
    p.add_argument('evidence', type=Path, help='new private directory on guest OS disk')
    p.add_argument('--case', choices=('control', 'write', 'fsync', 'miss'))
    args = p.parse_args()
    scratch, save = args.scratch.resolve(strict=True), args.evidence.resolve()
    check_scratch(scratch, args.device, 16 * 1024**2)
    require(save.parent.stat().st_dev == Path('/').stat().st_dev, 'save evidence on guest OS filesystem')
    require(',' not in str(save), 'QMP socket path cannot contain a comma')
    save.mkdir(mode=0o700)
    for case in [args.case] if args.case else ['control', 'write', 'fsync']:
        run_case(scratch, save/case, case, args.binaries.resolve(strict=True))


if __name__ == '__main__':
    main()
