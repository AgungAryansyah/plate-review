#!/usr/bin/env python3
"""Durable launch/reconnect with bounded OOM retries in fresh inference processes."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def atomic(path, value):
    temp = path.with_suffix('.tmp')
    with temp.open('w') as output:
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())
    temp.replace(path)


def live_process(pid, root):
    if not pid:
        return False
    try:
        command = Path(f'/proc/{int(pid)}/cmdline').read_bytes()
        return b'/plate-review-worker/infer.py' in command and str(root).encode() in command
    except (FileNotFoundError, ProcessLookupError):
        return False


def retry_profile(status):
    """Only allocator OOM is automatically retried, at most 4 -> 2 -> 1."""
    if status.get('error_kind') != 'cuda_oom':
        return None
    batch = int(status.get('batch_size', 1))
    return batch // 2 if batch > 1 else None


def main():
    root = Path(sys.argv[1]).resolve()
    with (root / '.launch.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        launch_path = root / 'launch.json'
        status_path = root / 'status.json'
        status = json.loads(status_path.read_text()) if status_path.exists() else None
        launch = json.loads(launch_path.read_text()) if launch_path.exists() else {}
        live = live_process(launch.get('pid'), root)
        history = []
        ceiling = 4
        attempt = launch.get('attempt', 1)
        if status and status.get('status') == 'complete':
            print(json.dumps(status)); return
        if status and status.get('status') == 'failed':
            ceiling = retry_profile(status)
            if ceiling is None:
                print(json.dumps(status)); return
            if live:
                waiting = dict(status, status='running', phase='releasing_for_retry', error=None)
                print(json.dumps(waiting)); return
            history = status.get('logs', [])[-150:]
            history.append(time.strftime('%Y-%m-%d %H:%M:%S ') + f'GPU memory pressure: retrying the complete clip with batch ceiling {ceiling}. Source quality and frame rate are unchanged.')
            atomic(root / f'attempt-{attempt}-status.json', status)
            attempt += 1
        elif live:
            print(json.dumps(status or {'id': root.name, 'status': 'running', 'progress': 0, 'phase': 'starting', 'logs': [], 'error': None})); return
        elif launch:
            failed = {'id': root.name, 'status': 'failed', 'progress': (status or {}).get('progress', 0), 'phase': 'failed', 'logs': (status or {}).get('logs', []), 'error': 'Remote inference exited before committing results. Retry this clip.'}
            atomic(status_path, failed); print(json.dumps(failed)); return
        atomic(root / 'runtime.json', {'batch_ceiling': ceiling, 'attempt': attempt, 'logs': history})
        pending = {'id': root.name, 'status': 'running', 'progress': 0, 'phase': 'retrying' if attempt > 1 else 'starting', 'logs': history or ['Checking free VRAM before loading SAM.'], 'error': None, 'attempt': attempt}
        atomic(status_path, pending)
        python = Path.home() / 'faris/plate-sam-camera92/venv/bin/python'
        script = Path.home() / 'faris/plate-review-worker/infer.py'
        with (root / 'engine.log').open('ab') as log:
            process = subprocess.Popen([str(python), str(script), str(root)], stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
        atomic(launch_path, {'pid': process.pid, 'attempt': attempt})
        print(json.dumps(pending))


if __name__ == '__main__':
    main()
