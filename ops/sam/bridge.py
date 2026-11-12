#!/usr/bin/env python3
"""Host daemon bridging an authenticated app spool to the L40 over SSH.
Run as the existing faris host user, with its existing L40 SSH identity. No SSH
private key or GPU model is exposed to the web container.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess
import time


def atomic(path, value):
    tmp = path.with_suffix('.tmp')
    with tmp.open('w') as f:
        json.dump(value, f)
        f.flush(); os.fsync(f.fileno())
    tmp.replace(path)


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=kw.pop('timeout', 60), **kw).stdout


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--queue', type=Path, required=True)
    ap.add_argument('--exports', type=Path, required=True)
    ap.add_argument('--remote', default='pityudhistira28@gpu-labkc.ub.ac.id')
    ap.add_argument('--port', default='1004')
    args = ap.parse_args()
    args.queue.mkdir(parents=True, exist_ok=True)
    lock = (args.queue / '.bridge.lock').open('w')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    scripts = Path(__file__).resolve().parent
    ssh = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15', '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3', '-p', args.port, args.remote]
    scp = ['scp', '-q', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=15', '-P', args.port]
    remote_root = 'faris/plate-review-worker'
    while True:
        if (args.queue / 'PAUSED').exists():
            time.sleep(5); continue
        requests = sorted(args.queue.glob('*/request.json'), key=lambda p:p.stat().st_mtime)
        for request_path in requests:
            directory = request_path.parent
            if not (directory / 'READY').exists():
                continue
            status_path = directory / 'status.json'
            try:
                previous = json.loads(status_path.read_text()) if status_path.exists() else {}
                if previous.get('status') in ('complete', 'failed'):
                    continue
                req = json.loads(request_path.read_text())
                job = req['job_id']
                if directory.name != job or any(c not in '0123456789abcdef-' for c in job):
                    raise ValueError('Invalid job identifier')
                source = (args.exports / req['source_relative']).resolve()
                if not source.is_relative_to(args.exports.resolve()) or not source.is_file():
                    raise ValueError('Source file is missing or outside the export root')
                remote_dir = f'{remote_root}/jobs/{job}'
                def status(phase, progress, message, state='running', error=None):
                    log = previous.get('logs', []) + [time.strftime('%Y-%m-%d %H:%M:%S ') + message]
                    atomic(status_path, {'id':job,'status':state,'progress':progress,'phase':phase,'logs':log[-200:],'error':error})
                if not (directory / 'uploaded.json').exists():
                    status('transferring', .005, 'Transferring source clip and request to the L40 over SSH.')
                    with source.open('rb') as f:
                        h = hashlib.file_digest(f, 'sha256').hexdigest()
                    if h != req['source_sha256']:
                        raise ValueError('Source changed after it was queued; rescan the clip')
                    run(ssh + [f'mkdir -p {shlex.quote(remote_dir)}'])
                    for name in ('infer.py','control.py'):
                        run(scp + [str(scripts/name), f'{args.remote}:{remote_root}/{name}'])
                    run(scp + [str(source), f'{args.remote}:{remote_dir}/source.mp4'], timeout=1800)
                    run(scp + [str(request_path), f'{args.remote}:{remote_dir}/request.json'])
                    atomic(directory / 'uploaded.json', {'remote_dir':remote_dir})
                while True:
                    raw = run(ssh + [f'python3 {shlex.quote(remote_root)}/control.py {shlex.quote(remote_dir)}'])
                    remote_status = json.loads(raw)
                    if remote_status.get('status') == 'complete':
                        temp = directory / 'proposals.part'
                        run(scp + [f'{args.remote}:{remote_dir}/proposals.json', str(temp)], timeout=600)
                        result = json.loads(temp.read_text())
                        if result['source_sha256'] != req['source_sha256'] or result['processed_frames'] != req['meta']['frame_count']:
                            raise ValueError('Invalid result checksum or incomplete frame coverage')
                        temp.replace(directory / 'proposals.json')
                        run(scp + [f'{args.remote}:{remote_dir}/engine.log', str(directory/'engine.log')], timeout=120)
                        # Complete is committed only after the result is locally durable.
                        atomic(status_path, remote_status)
                        break
                    atomic(status_path, remote_status)
                    if remote_status.get('status') == 'failed':
                        try:
                            run(scp + [f'{args.remote}:{remote_dir}/engine.log', str(directory/'engine.log')], timeout=120)
                        except Exception:
                            pass
                        break
                    time.sleep(3)
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
                # A dropped connection never launches a second inference process.
                previous = json.loads(status_path.read_text()) if status_path.exists() else {}
                previous.update({'id':directory.name,'status':'running','phase':'connection_wait','error':None})
                previous.setdefault('progress',0)
                previous['logs']=(previous.get('logs',[])+[f'Connection interrupted; retrying and reconnecting to the same job: {type(exc).__name__}'])[-200:]
                atomic(status_path,previous)
                time.sleep(15)
                break
            except Exception as exc:
                atomic(status_path, {'id':directory.name,'status':'failed','progress':0,'phase':'failed','logs':[str(exc)],'error':str(exc)})
        time.sleep(3)

if __name__ == '__main__':
    main()
