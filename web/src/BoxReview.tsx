import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, json } from "./api";
import type { BoxClip, BoxClipDetail, BoxSettings, BoxTrack, Detection } from "./box-contracts";
import "./BoxReview.css";

function clock(ms: number) {
  const seconds = Math.max(0, Math.floor(ms / 1000));
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}
function size(bytes: number) { return bytes < 1024 * 1024 ? `${(bytes / 1024).toFixed(0)} KB` : `${(bytes / 1024 / 1024).toFixed(1)} MB`; }
function median(values: number[]) { return [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)]; }

function filteredDetections(track: BoxTrack, settings: BoxSettings, width: number, height: number): Detection[] {
  const raw = track.detections.filter((d) => d.confidence >= settings.min_confidence &&
    (d.x_max - d.x_min) * (d.y_max - d.y_min) <= width * height * settings.max_area_fraction)
    .slice().sort((a, b) => a.frame - b.frame);
  if (raw.length < settings.minimum_frames) return [];
  let left = 0, right = 0;
  const filtered = raw.flatMap((d) => {
    while (left < raw.length && raw[left].frame < d.frame - settings.smoothing_radius) left++;
    while (right < raw.length && raw[right].frame <= d.frame + settings.smoothing_radius) right++;
    const near = raw.slice(left, right);
    const box = [d.x_min, d.y_min, d.x_max - d.x_min, d.y_max - d.y_min];
    const med = [
      median(near.map((other) => other.x_min)), median(near.map((other) => other.y_min)),
      median(near.map((other) => other.x_max - other.x_min)), median(near.map((other) => other.y_max - other.y_min)),
    ];
    const tooLargeOrSmall = near.length >= 3 && (
      box[2] > med[2] * settings.max_size_ratio || box[3] > med[3] * settings.max_size_ratio ||
      box[2] * settings.max_size_ratio < med[2] || box[3] * settings.max_size_ratio < med[3]);
    if (tooLargeOrSmall) return [];
    if (settings.smoothing_radius > 0 && settings.jitter_px > 0 && box.every((v, i) => Math.abs(v - med[i]) <= settings.jitter_px)) {
      return [{ ...d, x_min: med[0], y_min: med[1], x_max: med[0] + med[2], y_max: med[1] + med[3] }];
    }
    return [d];
  });
  return filtered.length >= settings.minimum_frames ? filtered : [];
}

export default function BoxReview({ onBack }: { onBack: () => void }) {
  const [clips, setClips] = useState<BoxClip[]>([]);
  const [detail, setDetail] = useState<BoxClipDetail | null>(null);
  const [selectedTrack, setSelectedTrack] = useState<string | null>(null);
  const [settings, setSettings] = useState<BoxSettings>({ min_confidence: 0.45, jitter_px: 3, smoothing_radius: 2, minimum_frames: 2, max_area_fraction: 0.08, max_size_ratio: 3 });
  const [tracks, setTracks] = useState<BoxTrack[]>([]);
  const [currentMs, setCurrentMs] = useState(0);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [videoFailed, setVideoFailed] = useState(false);
  const [framePreview, setFramePreview] = useState(false);
  const [playing, setPlaying] = useState(false);
  const framePreviewTime = useRef<number | null>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const requestSeq = useRef(0);

  const refreshClips = useCallback(async () => {
    try { setClips(await api<BoxClip[]>("/api/boxes/clips")); } catch (reason) { setError(reason instanceof Error ? reason.message : "Could not load clips"); }
  }, []);

  const loadDetail = useCallback(async (id: string, quiet = false) => {
    const seq = ++requestSeq.current;
    if (!quiet) { setLoading(true); setError(""); }
    try {
      const loaded = await api<BoxClipDetail>(`/api/boxes/clips/${encodeURIComponent(id)}`);
      if (seq !== requestSeq.current) return;
      setDetail(loaded);
      setTracks(loaded.tracks.map((track) => ({ ...track, detections: [...track.detections] })));
      setSettings(loaded.settings);
      setSelectedTrack((current) => current && loaded.tracks.some((track) => track.id === current) ? current : loaded.tracks[0]?.id ?? null);
      setVideoFailed(false);
    } catch (reason) { if (!quiet) setError(reason instanceof Error ? reason.message : "Could not open this clip"); }
    finally { if (!quiet && seq === requestSeq.current) setLoading(false); }
  }, []);

  useEffect(() => { void refreshClips(); const timer = window.setInterval(() => void refreshClips(), 5000); return () => clearInterval(timer); }, [refreshClips]);
  useEffect(() => {
    if (!detail || (detail.clip.status !== "queued" && detail.clip.status !== "running")) return;
    const timer = window.setInterval(() => { void refreshClips(); void loadDetail(detail.clip.id, true); }, 1200);
    return () => clearInterval(timer);
  }, [detail, loadDetail, refreshClips]);

  const activeDetection = (track: BoxTrack): Detection | undefined => {
    if (!detail) return undefined;
    const usable = track.detections;
    if (!usable.length) return undefined;
    const nearest = usable.reduce((best, item) => Math.abs(item.timestamp_ms - currentMs) < Math.abs(best.timestamp_ms - currentMs) ? item : best);
    return Math.abs(nearest.timestamp_ms - currentMs) <= 500 / detail.meta.fps + 2 ? nearest : undefined;
  };

  const visibleTracks = useMemo(() => detail ? tracks.map((track) => ({ ...track, detections: filteredDetections(track, settings, detail.meta.width, detail.meta.height) })) : [], [tracks, settings, detail?.meta.width, detail?.meta.height]);
  const jobIsActive = detail?.clip.status === "queued" || detail?.clip.status === "running";
  const updateTrack = (id: string, change: Partial<BoxTrack>) => setTracks((current) => current.map((track) => track.id === id ? { ...track, ...change } : track));
  const updateSetting = (key: keyof BoxSettings, value: number) => setSettings((current) => ({ ...current, [key]: value }));

  useEffect(() => {
    if ((!framePreview && !videoFailed) || !playing || !detail) { framePreviewTime.current = null; return; }
    let timer: number;
    const tick = () => {
      const now = performance.now();
      if (framePreviewTime.current === null) framePreviewTime.current = now;
      const elapsed = now - framePreviewTime.current;
      if (elapsed >= 200) {
        const delta = Math.max(200, Math.floor(elapsed / 200) * 200);
        framePreviewTime.current = now;
        setCurrentMs((ms) => {
          const next = Math.min(detail.clip.duration_ms, ms + delta);
          if (next >= detail.clip.duration_ms) setPlaying(false);
          return next;
        });
      }
      timer = window.setTimeout(tick, 50);
    };
    timer = window.setTimeout(tick, 50);
    return () => clearTimeout(timer);
  }, [detail, framePreview, videoFailed, playing]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video?.requestVideoFrameCallback) return;
    let handle = 0;
    const tick = (_time: number, metadata: VideoFrameCallbackMetadata) => {
      if (!video.paused) setCurrentMs(metadata.mediaTime * 1000);
      handle = video.requestVideoFrameCallback(tick);
    };
    handle = video.requestVideoFrameCallback(tick);
    return () => video.cancelVideoFrameCallback(handle);
  }, [detail?.clip.id]);

  const save = async () => {
    if (!detail) return false;
    setSaving(true); setError("");
    try {
      const saved = await api<BoxClipDetail>(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/review`, json("PUT", {
        expected_revision: detail.revision,
        tracks: tracks.map(({ id, selected, plate_text }) => ({ id, selected, plate_text })),
        settings,
      }));
      setDetail(saved); setTracks(saved.tracks); setSettings(saved.settings); return true;
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Review could not be saved"); return false; }
    finally { setSaving(false); }
  };

  const segment = async () => {
    if (!detail) return;
    if (!(await save())) return;
    setError("");
    try { await api(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/segment`, json("POST", {})); await loadDetail(detail.clip.id); await refreshClips(); }
    catch (reason) { setError(reason instanceof Error ? reason.message : "Segmentation could not start"); }
  };

  const exportDataset = async () => {
    if (!detail || !(await save())) return;
    window.location.assign(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/export`);
  };

  const selectClip = (clip: BoxClip) => { setDetail(null); setCurrentMs(0); setPlaying(false); setVideoFailed(false); setFramePreview(false); void loadDetail(clip.id); };
  const activeJob = detail?.job;
  const duration = detail?.clip.duration_ms ?? 0;
  const frameUrl = detail ? `/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/frame?timestamp_ms=${Math.round(currentMs)}` : "";

  return <main className="box-review">
    <header className="box-header"><div className="box-brand"><span className="box-mark">BX</span><div><p className="eyebrow">PLATE REVIEW / DATASET</p><h1>Plate boxes</h1></div></div><div className="box-header-actions"><button className="secondary-button" onClick={onBack}>← Clip review</button><button className="secondary-button" aria-label="Refresh clips" onClick={() => void refreshClips()}>↻ Refresh</button></div></header>
    {error && <div className="box-error" role="status">{error}</div>}
    {!detail ? <section className="box-library"><div className="box-library-heading"><div><p className="eyebrow">COMPLETED EXPORTS</p><h2>Choose a trimmed clip</h2><p>New clips appear automatically after export finishes.</p></div><span className="box-count">{clips.length} clips</span></div>
      {loading && <p className="box-loading">Loading clip…</p>}
      {clips.length ? <div className="clip-grid">{clips.map((clip) => <button className="clip-card" key={clip.id} onClick={() => selectClip(clip)}><span className={`clip-status ${clip.status}`}>{clip.status}</span><strong>{clip.name}</strong><span>{clock(clip.duration_ms)} <i>·</i> {size(clip.bytes)}</span>{clip.status === "running" || clip.status === "queued" ? <span className="clip-progress"><i style={{ width: `${Math.round(clip.progress * 100)}%` }} /></span> : null}{clip.error && <span className="clip-error">{clip.error}</span>}</button>)}</div> : !loading && <div className="box-empty"><span>▧</span><h3>No trimmed clips yet</h3><p>Mark and export intervals in Clip review. Completed clips will show up here automatically.</p></div>}
    </section> : <>
      <div className="box-detail-heading"><button className="back-button" onClick={() => { requestSeq.current++; setDetail(null); }}>←</button><div className="box-detail-title"><p className="eyebrow">TRIMMED CLIP</p><h2>{detail.clip.name}</h2><span>{clock(duration)} · {detail.meta.width} × {detail.meta.height} · {detail.meta.fps.toFixed(2)} fps · {detail.meta.frame_count.toLocaleString()} frames</span></div><span className={`clip-status ${detail.clip.status}`}>{detail.clip.status}</span></div>
      {activeJob && <section className="box-job" aria-live="polite"><div className="job-title"><strong>{activeJob.phase || activeJob.status}</strong><span>{Math.round(activeJob.progress * 100)}%</span></div><div className="box-progress"><i style={{ width: `${Math.round(activeJob.progress * 100)}%` }} /></div>{activeJob.error && <p className="clip-error">{activeJob.error}</p>}<details open={jobIsActive}><summary>Segmentation log <span>{activeJob.logs.length} lines</span></summary><pre>{activeJob.logs.length ? activeJob.logs.join("\n") : jobIsActive ? "Waiting for worker output…" : "No worker output recorded."}</pre></details></section>}
      <div className="box-layout"><section className="box-viewer"><div className="box-viewer-toolbar"><div><span className="live-dot" /> RAW SOURCE <span>·</span> {detail.clip.name}</div><span>{clock(currentMs)} / {clock(duration)}</span></div><div className="box-stage">
        <video ref={videoRef} className={framePreview || videoFailed ? "box-video-hidden" : ""} src={`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/stream`} preload="metadata" playsInline onTimeUpdate={(event) => setCurrentMs(event.currentTarget.currentTime * 1000)} onPlay={() => { if (!framePreview) setPlaying(true); }} onPause={() => { if (!framePreview) setPlaying(false); }} onError={() => setVideoFailed(true)} />
        {(videoFailed || framePreview || !playing) && <div className="box-decoded-frame"><img src={frameUrl} alt={`Decoded source frame at ${clock(currentMs)}`} />{(videoFailed || framePreview) && <span>{videoFailed ? "Video playback unavailable · showing decoded frame" : "Decoded frame preview · 5 frames per second"}</span>}</div>}
        <svg className="box-overlay" viewBox={`0 0 ${detail.meta.width} ${detail.meta.height}`} preserveAspectRatio="xMidYMid meet" role="img" aria-label="Detected plate boxes">{visibleTracks.map((track) => { const detection = activeDetection(track); if (!detection) return null; const active = selectedTrack === track.id; return <g key={track.id} onClick={() => setSelectedTrack(track.id)} className={active ? "box-active" : ""}><rect x={detection.x_min} y={detection.y_min} width={detection.x_max - detection.x_min} height={detection.y_max - detection.y_min} /><text x={detection.x_min} y={Math.max(16, detection.y_min - 7)}>{track.id} · {(detection.confidence * 100).toFixed(0)}%</text></g>; })}</svg>
      </div><div className="box-transport"><button aria-label={playing ? "Pause clip" : "Play clip"} onClick={() => { if (framePreview || videoFailed) { setPlaying((value) => !value); return; } const video = videoRef.current; if (!video) return; if (video.paused) void video.play().catch(() => setVideoFailed(true)); else video.pause(); }}>{playing ? "Ⅱ" : "▶"}</button><input aria-label="Clip timeline" type="range" min="0" max={duration || 1} step="1" value={Math.min(currentMs, duration)} onChange={(event) => { const value = Number(event.target.value); setCurrentMs(value); if (videoRef.current) videoRef.current.currentTime = value / 1000; }} /><span>{clock(currentMs)}</span><button className="preview-toggle" onClick={() => { const next = !framePreview; if (next) { videoRef.current?.pause(); setPlaying(false); } else if (videoRef.current) videoRef.current.currentTime = currentMs / 1000; setFramePreview(next); }}>{framePreview ? "Video" : "Frame preview"}</button></div></section>
      <aside className="box-sidebar"><section className="box-panel"><div className="box-panel-title"><div><p className="eyebrow">TRACK REVIEW</p><h3>Plate candidates <span>{tracks.length}</span></h3></div><span className="box-small-note">No OCR</span></div>{tracks.length ? <div className="track-list">{tracks.map((track) => <div className={`track-row ${selectedTrack === track.id ? "focused" : ""}`} key={track.id} onClick={() => setSelectedTrack(track.id)}><label className="track-toggle" onClick={(event) => event.stopPropagation()}><input type="checkbox" disabled={jobIsActive} checked={track.selected} onChange={(event) => updateTrack(track.id, { selected: event.target.checked })} aria-label={`Include box ${track.id}`} /><span>{track.id}</span></label><input className="plate-entry" disabled={jobIsActive} aria-label={`Plate text for box ${track.id}`} placeholder="Enter plate text" value={track.plate_text} onChange={(event) => updateTrack(track.id, { plate_text: event.target.value })} /><small>{visibleTracks.find((item) => item.id === track.id)?.detections.length ?? 0} frames after filtering</small></div>)}</div> : <p className="box-hint">No tracks yet. Run segmentation to find plate candidates.</p>}</section>
        <section className="box-panel box-settings"><div className="box-panel-title"><div><p className="eyebrow">SEGMENTATION</p><h3>Run settings</h3></div></div><label>Minimum confidence <output>{settings.min_confidence.toFixed(2)}</output><input disabled={jobIsActive} aria-label="Minimum confidence" type="range" min="0" max="1" step="0.01" value={settings.min_confidence} onChange={(event) => updateSetting("min_confidence", Number(event.target.value))} /></label><label>Median jitter <span>px</span><input disabled={jobIsActive} aria-label="Median jitter" type="number" min="0" max="100" step="1" value={settings.jitter_px} onChange={(event) => updateSetting("jitter_px", Number(event.target.value))} /></label><label>Smoothing window <span>frames</span><input disabled={jobIsActive} aria-label="Smoothing window" type="number" min="0" max="30" step="1" value={settings.smoothing_radius} onChange={(event) => updateSetting("smoothing_radius", Number(event.target.value))} /></label><label>Minimum track length <span>frames</span><input disabled={jobIsActive} aria-label="Minimum frames" type="number" min="1" max="10000" step="1" value={settings.minimum_frames} onChange={(event) => updateSetting("minimum_frames", Number(event.target.value))} /></label><label>Maximum box area <output>{(settings.max_area_fraction * 100).toFixed(1)}%</output><input disabled={jobIsActive} aria-label="Maximum area fraction" type="range" min="0.001" max="0.25" step="0.001" value={settings.max_area_fraction} onChange={(event) => updateSetting("max_area_fraction", Number(event.target.value))} /></label><label>Maximum size ratio <output>{settings.max_size_ratio.toFixed(1)}×</output><input disabled={jobIsActive} aria-label="Maximum size ratio" type="range" min="1" max="10" step="0.1" value={settings.max_size_ratio} onChange={(event) => updateSetting("max_size_ratio", Number(event.target.value))} /></label><p className="box-hint">Boxes stay unchecked until you include a track. Plate text is entered manually.{jobIsActive && " Editing is paused while segmentation runs."}</p></section>
        <div className="box-actions"><button className="secondary-button" onClick={() => void save()} disabled={saving || jobIsActive}>{saving ? "Saving…" : "Save review"}</button><button className="primary-button" onClick={() => void segment()} disabled={jobIsActive || detail.clip.status === "complete"}>{jobIsActive ? "Segmentation running" : detail.clip.status === "complete" ? "Segmentation complete" : "Segment plates"}</button><button className="export-dataset" onClick={() => void exportDataset()} disabled={saving || jobIsActive || detail.clip.status !== "complete"}>Export dataset ↓</button></div>
      </aside></div>
    </>}
  </main>;
}
