#!/usr/bin/env python3
"""One SAM job per process: load, process all source frames, release model, exit.
No OCR. Only this process is terminated if it exceeds its GPU budget.
"""
import argparse
import fcntl
import gc
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import traceback


def atomic(path, data):
    tmp = path.with_suffix('.tmp')
    with tmp.open('w') as f:
        json.dump(data, f, separators=(',', ':'))
        f.flush()
        os.fsync(f.fileno())
    tmp.replace(path)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for b in iter(lambda: f.read(1024 * 1024), b''):
            h.update(b)
    return h.hexdigest()


def ram_available():
    return int(next(l.split()[1] for l in Path('/proc/meminfo').read_text().splitlines() if l.startswith('MemAvailable:'))) * 1024


def choose_gpu():
    out = subprocess.check_output(['nvidia-smi', '--query-gpu=index,memory.free,memory.total,utilization.gpu', '--format=csv,noheader,nounits'], text=True)
    choices = []
    for line in out.splitlines():
        index, free, total, utilization = map(int, line.split(','))
        free *= 1024 ** 2
        total *= 1024 ** 2
        effective = min(30_000_000_000, free - 4_000_000_000)
        if effective >= 18_000_000_000:
            choices.append((utilization, -free, index, (effective - 2_000_000_000) / total))
    return min(choices)[2:] if choices else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('job_dir', type=Path)
    args = ap.parse_args()
    root = args.job_dir.resolve()
    request = json.loads((root / 'request.json').read_text())
    job_id = request['job_id']
    status = {'id': job_id, 'status': 'running', 'progress': 0., 'phase': 'starting', 'logs': [], 'error': None, 'pid': os.getpid()}
    def update(phase, progress=None, message=None, state=None, error=None):
        status['phase'] = phase
        if progress is not None:
            status['progress'] = progress
        if state:
            status['status'] = state
        if error:
            status['error'] = error
        if message:
            status['logs'].append(time.strftime('%Y-%m-%d %H:%M:%S ') + message)
            status['logs'] = status['logs'][-200:]
        atomic(root / 'status.json', status)
    try:
        lock = (root.parent.parent / 'sam.lock').open('w')
        fcntl.flock(lock, fcntl.LOCK_EX)
        update('verifying', .01, 'Verifying transferred raw clip; OCR is disabled.')
        source = root / 'source.mp4'
        if digest(source) != request['source_sha256']:
            raise RuntimeError('Transferred source checksum does not match queued clip')
        checkpoint = Path(os.environ.get('PLATE_SAM_CHECKPOINT', str(Path.home() / 'faris/plate-sam-camera92/models/sam3.1_multiplex.pt')))
        if not checkpoint.is_file():
            raise RuntimeError(f'SAM checkpoint missing: {checkpoint}')
        meta = request['meta']
        required_ram = max(10_000_000_000, int(meta['frame_count'] * 1008 * 1008 * 3 * 4 * 1.15 + 4_000_000_000))
        while True:
            paused = (root.parent.parent / 'PAUSED').exists()
            gpu = choose_gpu()
            available = ram_available()
            if not paused and gpu is not None and available >= required_ram:
                break
            reason = 'Paused' if paused else f'Waiting for resources: RAM {available / 1e9:.1f}/{required_ram / 1e9:.1f} GB; GPU requires 18 GB plus 4 GB headroom'
            update('resource_wait', .02, reason)
            time.sleep(30)
        index, fraction = gpu
        os.environ['CUDA_VISIBLE_DEVICES'] = str(index)
        update('loading_model', .03, f'Loading SAM 3.1 on L40 GPU {index}; process budget <=30 decimal GB. Model unloads after this job.')
        import cv2
        import numpy as np
        import torch
        from sam3.model_builder import build_sam3_multiplex_video_predictor
        torch.set_num_threads(4)
        torch.cuda.set_per_process_memory_fraction(fraction)
        stop_watchdog = threading.Event()
        def watchdog():
            while not stop_watchdog.wait(1):
                try:
                    usage = subprocess.check_output(['nvidia-smi', '--query-compute-apps=pid,used_memory', '--format=csv,noheader,nounits'], text=True)
                    memory = sum(int(line.split(',')[1]) * 1024 ** 2 for line in usage.splitlines() if line.split(',')[0].strip() == str(os.getpid()))
                    if memory > 30_000_000_000 or ram_available() < 2_000_000_000:
                        update('failed', state='failed', error='SAM resource guard stopped this job', message='Only our SAM process exceeded its resource guard; the clip can be retried.')
                        os._exit(137)
                except Exception:
                    pass
        threading.Thread(target=watchdog, daemon=True).start()
        probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v:0', '-show_entries', 'frame=best_effort_timestamp_time', '-of', 'json', str(source)]))
        timestamps = [float(f['best_effort_timestamp_time']) for f in probe['frames']]
        if not timestamps:
            raise RuntimeError('No source frame timestamps were found')
        first = timestamps[0]
        timestamps = [max(0, round((v - first) * 1000)) for v in timestamps]
        if len(timestamps) != meta['frame_count']:
            raise RuntimeError(f'Source frame-count mismatch: probe {len(timestamps)}, expected {meta["frame_count"]}')
        model = build_sam3_multiplex_video_predictor(checkpoint_path=str(checkpoint), max_num_objects=256, multiplex_count=16, use_fa3=False, use_rope_real=True, compile=False, warm_up=False, async_loading_frames=True, default_output_prob_thresh=.25)
        session = None
        tracks = {}
        coverage = set()
        try:
            update('decoding', .08, f'Loading all {len(timestamps)} original-rate source frames.')
            session = model.handle_request({'type': 'start_session', 'resource_path': str(source), 'offload_video_to_cpu': True, 'offload_state_to_cpu': True})['session_id']
            update('segmenting', .12, 'Searching for license plates and propagating plate masks throughout the clip.')
            model.handle_request({'type': 'add_prompt', 'session_id': session, 'frame_index': 0, 'text': 'license plate', 'output_prob_thresh': .25})
            def numpy(value):
                return value.detach().cpu().numpy() if hasattr(value, 'detach') else np.asarray(value)
            for result in model.handle_stream_request({'type': 'propagate_in_video', 'session_id': session, 'start_frame_index': 0, 'propagation_direction': 'forward', 'output_prob_thresh': .25, 'evict_cached_frame_outputs': True}):
                frame = int(result['frame_index'])
                if frame < 0 or frame >= len(timestamps):
                    raise RuntimeError('SAM returned an invalid source frame index')
                coverage.add(frame)
                output = result.get('outputs', {})
                ids = numpy(output.get('out_obj_ids', [])).reshape(-1)
                masks = output.get('out_binary_masks', [])
                scores = numpy(output.get('out_probs', np.ones(len(ids)))).reshape(-1)
                for i, object_id in enumerate(ids):
                    if i >= len(masks):
                        continue
                    score = float(scores[i]) if i < len(scores) else 1.
                    if not np.isfinite(score) or not 0 <= score <= 1:
                        continue  # SAM's -10000 no-object sentinel is not a detection.
                    mask = numpy(masks[i]).squeeze().astype(np.uint8)
                    if mask.ndim != 2 or not mask.any():
                        continue
                    if mask.shape != (meta['height'], meta['width']):
                        mask = cv2.resize(mask, (meta['width'], meta['height']), interpolation=cv2.INTER_NEAREST)
                    x, y, w, h = cv2.boundingRect(mask)
                    if not w or not h:
                        continue
                    tracks.setdefault(str(int(object_id)), []).append({'frame': frame, 'timestamp_ms': timestamps[frame], 'x_min': x, 'y_min': y, 'x_max': x+w, 'y_max': y+h, 'confidence': score})
                if len(coverage) % 10 == 0 or len(coverage) == len(timestamps):
                    update('segmenting', .12 + .82 * len(coverage) / len(timestamps), f'Processed {len(coverage)}/{len(timestamps)} frames; {len(tracks)} candidate tracks.')
        finally:
            update('unloading_model', .95, 'Closing SAM session and releasing CUDA memory.')
            if session:
                model.handle_request({'type': 'close_session', 'session_id': session})
            del model
            gc.collect()
            torch.cuda.empty_cache()
            stop_watchdog.set()
        if len(coverage) != len(timestamps):
            raise RuntimeError(f'Incomplete timeline: {len(coverage)}/{len(timestamps)} frames')
        proposals = {'schema': 'plate-box-proposals/v1', 'source_sha256': request['source_sha256'], 'processed_frames': len(coverage), 'meta': meta,
                     'tracks': [{'id': key, 'selected': False, 'plate_text': '', 'detections': sorted(ds, key=lambda d:d['frame'])} for key, ds in sorted(tracks.items(), key=lambda kv:int(kv[0]))]}
        atomic(root / 'proposals.json', proposals)
        update('complete', 1., f'Complete: {len(tracks)} tracks, {sum(len(v) for v in tracks.values())} box frames. SAM unloaded. No OCR was run.', state='complete')
    except BaseException as exc:
        update('failed', state='failed', error=str(exc), message=str(exc))
        traceback.print_exc()
        raise

if __name__ == '__main__':
    main()
