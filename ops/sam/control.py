#!/usr/bin/env python3
"""Remote durable launch/reconnect; never starts a duplicate live job."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1]).resolve()
lock = (root / '.launch.lock').open('w')
fcntl.flock(lock, fcntl.LOCK_EX)
launch = root / 'launch.json'
status_path = root / 'status.json'
status = json.loads(status_path.read_text()) if status_path.exists() else None
if status and status.get('status') in ('complete', 'failed'):
    print(json.dumps(status)); sys.exit(0)
pid = json.loads(launch.read_text()).get('pid') if launch.exists() else None
live = False
if pid:
    try:
        cmd = Path(f'/proc/{int(pid)}/cmdline').read_bytes()
        live = b'/plate-review-worker/infer.py' in cmd and str(root).encode() in cmd
    except (FileNotFoundError, ProcessLookupError):
        pass
if live:
    print(json.dumps(status or {'id':root.name,'status':'running','progress':0,'phase':'starting','logs':[],'error':None})); sys.exit(0)
if launch.exists():
    failed = {'id':root.name,'status':'failed','progress':status.get('progress',0) if status else 0,'phase':'failed','logs':(status or {}).get('logs',[]),'error':'Remote inference exited before committing results. Retry this clip.'}
    status_path.write_text(json.dumps(failed)); print(json.dumps(failed)); sys.exit(0)
python = Path.home() / 'faris/plate-sam-camera92/venv/bin/python'
script = Path.home() / 'faris/plate-review-worker/infer.py'
with (root / 'engine.log').open('ab') as log:
    process = subprocess.Popen([str(python), str(script), str(root)], stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
launch.write_text(json.dumps({'pid':process.pid}))
print(json.dumps({'id':root.name,'status':'running','progress':0,'phase':'starting','logs':['Remote SAM process launched.'],'error':None}))
