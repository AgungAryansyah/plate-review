import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { PointerEvent as ReactPointerEvent } from "react";
import { api, json } from "./api";
import type { BoxClip, BoxClipDetail, BoxDoneResult, BoxSettings, BoxTrack, Detection } from "./box-contracts";
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

function editableTarget(target: EventTarget | null): boolean {
  return target instanceof HTMLElement && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
}

export default function BoxReview({
  onBack,
  libraryResetSeq = 0,
  onDetailChange,
}: {
  onBack: () => void;
  libraryResetSeq?: number;
  onDetailChange?: (hasDetail: boolean, clipName: string | null) => void;
}) {
  const [clips, setClips] = useState<BoxClip[]>([]);
  const [statusFilter, setStatusFilter] = useState<"all" | "complete" | "need_label" | "failed" | "unsegmented" | "processing">("all");
  const [selectedClipIds, setSelectedClipIds] = useState<Set<string>>(new Set());
  const [batchProgress, setBatchProgress] = useState<{ done: number; total: number } | null>(null);
  const [detail, setDetail] = useState<BoxClipDetail | null>(null);
  const [selectedTrack, setSelectedTrack] = useState<string | null>(null);
  const [settings, setSettings] = useState<BoxSettings>({ min_confidence: 0.45, jitter_px: 3, smoothing_radius: 2, minimum_frames: 2, max_area_fraction: 0.08, max_size_ratio: 3 });
  const [tracks, setTracks] = useState<BoxTrack[]>([]);
  const [currentMs, setCurrentMs] = useState(0);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [doneMessage, setDoneMessage] = useState("");
  const [error, setError] = useState("");
  const [videoFailed, setVideoFailed] = useState(false);
  const [framePreview, setFramePreview] = useState(false);
  const [decodedFrame, setDecodedFrame] = useState<{ src: string; clipId: string; timestampMs: number } | null>(null);
  const [frameLoading, setFrameLoading] = useState(false);
  const [showBoxes, setShowBoxes] = useState(true);
  const [playing, setPlaying] = useState(false);
  const [jobLogOpen, setJobLogOpen] = useState(false);
  const framePreviewTime = useRef<number | null>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const trackInputRefs = useRef<Record<string, HTMLInputElement | null>>({});
  const trackRowRefs = useRef<Record<string, HTMLDivElement | null>>({});
  const panOrigin = useRef<{ x: number; y: number; panX: number; panY: number; trackId: string | null } | null>(null);
  const didPan = useRef(false);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const requestSeq = useRef(0);

  const refreshClips = useCallback(async () => {
    try { setClips(await api<BoxClip[]>("/api/boxes/clips")); } catch (reason) { setError(reason instanceof Error ? reason.message : "Could not load clips"); }
  }, []);

  const queueableIds = clips.filter((clip) => clip.status === "unsegmented" || clip.status === "failed").map((clip) => clip.id);
  const reviewState = (clip: BoxClip) => clip.status === "complete" ? clip.review_state ?? "need_label" : clip.status === "queued" || clip.status === "running" ? "processing" : clip.status;
  const filteredClips = useMemo(
    () => clips.filter((clip) => statusFilter === "all" || reviewState(clip) === statusFilter),
    [clips, statusFilter],
  );
  const runBatch = async (ids: string[]) => {
    if (!ids.length || batchProgress) return;
    setBatchProgress({ done: 0, total: ids.length }); setError("");
    let done = 0;
    const failures: string[] = [];
    for (const id of ids) {
      try {
        await api(`/api/boxes/clips/${encodeURIComponent(id)}/segment`, json("POST", {}));
        setClips((current) => current.map((clip) => clip.id === id ? { ...clip, status: "queued", progress: 0, error: null } : clip));
      } catch (reason) {
        failures.push(`${id}: ${reason instanceof Error ? reason.message : "could not queue"}`);
      }
      done++;
      setBatchProgress({ done, total: ids.length });
    }
    setSelectedClipIds(new Set());
    await refreshClips();
    setBatchProgress(null);
    if (failures.length) setError(`Could not queue ${failures.length} clip(s): ${failures.join("; ")}`);
  };

  const toggleClipSelection = (id: string) => setSelectedClipIds((current) => {
    const next = new Set(current);
    if (next.has(id)) next.delete(id); else next.add(id);
    return next;
  });

  const setZoomAt = useCallback((nextZoom: number, x = 0, y = 0) => {
    const next = Math.max(1, Math.min(8, Number(nextZoom.toFixed(2))));
    if (next === 1) { setPan({ x: 0, y: 0 }); setZoom(next); return; }
    const ratio = next / zoom;
    setPan((current) => ({ x: x - ratio * (x - current.x), y: y - ratio * (y - current.y) }));
    setZoom(next);
  }, [zoom]);
  const focusTrackInput = useCallback((trackId: string) => {
    setSelectedTrack(trackId);
    trackRowRefs.current[trackId]?.scrollIntoView?.({ block: "center" });
    const input = trackInputRefs.current[trackId];
    if (input && !input.disabled) {
      input.focus();
      const len = input.value.length;
      input.setSelectionRange?.(len, len);
    }
  }, []);
  const onStagePointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    const pointerButton = (event as ReactPointerEvent<HTMLDivElement> & { button?: number }).button;
    if (zoom <= 1 || (pointerButton !== undefined && pointerButton !== 0)) return;
    const trackGroup = (event.target as Element).closest<SVGGElement>("[data-track-id]");
    panOrigin.current = { x: event.clientX, y: event.clientY, panX: pan.x, panY: pan.y, trackId: trackGroup?.dataset.trackId ?? null };
    didPan.current = false;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const onStagePointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    const origin = panOrigin.current;
    if (!origin) return;
    const dx = event.clientX - origin.x, dy = event.clientY - origin.y;
    if (Math.abs(dx) + Math.abs(dy) > 4) didPan.current = true;
    setPan({ x: origin.panX + dx, y: origin.panY + dy });
  };
  const onStagePointerUp = () => {
    const origin = panOrigin.current;
    panOrigin.current = null;
    if (origin && !didPan.current && origin.trackId) focusTrackInput(origin.trackId);
    if (didPan.current) window.setTimeout(() => { didPan.current = false; }, 120);
  };

  useEffect(() => {
    const stage = stageRef.current;
    if (!stage) return;
    const handleWheel = (event: WheelEvent) => {
      event.preventDefault();
      const rect = stage.getBoundingClientRect();
      setZoomAt(zoom * (event.deltaY < 0 ? 1.12 : 1 / 1.12), event.clientX - rect.left - rect.width / 2, event.clientY - rect.top - rect.height / 2);
    };
    stage.addEventListener("wheel", handleWheel, { passive: false });
    return () => stage.removeEventListener("wheel", handleWheel);
  }, [setZoomAt, zoom]);

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

  const overlayTimestampMs = framePreview || videoFailed
    ? decodedFrame && detail && decodedFrame.clipId === detail.clip.id ? decodedFrame.timestampMs : null
    : currentMs;
  const activeDetection = (track: BoxTrack): Detection | undefined => {
    if (!detail || overlayTimestampMs === null) return undefined;
    const usable = track.detections;
    if (!usable.length) return undefined;
    const nearest = usable.reduce((best, item) => Math.abs(item.timestamp_ms - overlayTimestampMs) < Math.abs(best.timestamp_ms - overlayTimestampMs) ? item : best);
    return Math.abs(nearest.timestamp_ms - overlayTimestampMs) <= 500 / detail.meta.fps + 2 ? nearest : undefined;
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

  const activeJob = detail?.job;
  const duration = detail?.clip.duration_ms ?? 0;

  const seekToMs = useCallback((targetMs: number) => {
    const clamped = Math.max(0, Math.min(duration, Math.round(targetMs)));
    setCurrentMs(clamped);
    if (videoRef.current) videoRef.current.currentTime = clamped / 1000;
  }, [duration]);

  useEffect(() => {
    if (libraryResetSeq > 0) {
      requestSeq.current++;
      setJobLogOpen(false);
      setDetail(null);
    }
  }, [libraryResetSeq]);
  useEffect(() => {
    onDetailChange?.(!!detail, detail?.clip.name ?? null);
  }, [detail, onDetailChange]);

  const seekToTrack = useCallback((trackId: string) => {
    const visible = visibleTracks.find((t) => t.id === trackId);
    const track = (visible && visible.detections.length > 0 ? visible : undefined) ?? tracks.find((t) => t.id === trackId);
    if (!track || track.detections.length === 0) return;
    const sorted = [...track.detections].sort((a, b) => a.timestamp_ms - b.timestamp_ms);
    const first = sorted[0];
    const last = sorted[sorted.length - 1];
    const target = currentMs >= first.timestamp_ms && currentMs <= last.timestamp_ms
      ? sorted.reduce((best, item) => Math.abs(item.timestamp_ms - currentMs) < Math.abs(best.timestamp_ms - currentMs) ? item : best)
      : first;
    seekToMs(target.timestamp_ms);
  }, [currentMs, seekToMs, tracks, visibleTracks]);

  const cycleTrack = useCallback((delta: number, focusInput = false) => {
    if (tracks.length === 0) return;
    const currentIndex = tracks.findIndex((t) => t.id === selectedTrack);
    const baseIndex = currentIndex === -1 ? (delta > 0 ? -1 : 0) : currentIndex;
    const nextIndex = (baseIndex + delta + tracks.length) % tracks.length;
    const nextId = tracks[nextIndex].id;
    setSelectedTrack(nextId);
    trackRowRefs.current[nextId]?.scrollIntoView?.({ block: "nearest" });
    seekToTrack(nextId);
    if (focusInput) {
      const input = trackInputRefs.current[nextId];
      if (input && !input.disabled) {
        input.focus();
        input.select();
      }
    }
  }, [seekToTrack, selectedTrack, tracks]);

  const save = useCallback(async () => {
    if (!detail) return false;
    setSaving(true); setError("");
    try {
      const saved = await api<BoxClipDetail>(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/review`, json("PUT", {
        expected_revision: detail.revision,
        tracks: tracks.map(({ id, selected, plate_text }) => ({ id, selected, plate_text })),
        settings,
      }));
      setDetail(saved); setTracks(saved.tracks); setSettings(saved.settings); setDoneMessage(""); await refreshClips(); return true;
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Review could not be saved"); return false; }
    finally { setSaving(false); }
  }, [detail, refreshClips, settings, tracks]);

  const segment = async () => {
    if (!detail) return;
    if (!(await save())) return;
    setError("");
    try { await api(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/segment`, json("POST", {})); await loadDetail(detail.clip.id); await refreshClips(); }
    catch (reason) { setError(reason instanceof Error ? reason.message : "Segmentation could not start"); }
  };

  const markDone = useCallback(async () => {
    if (!detail) return;
    setSaving(true); setError(""); setDoneMessage("");
    try {
      const result = await api<BoxDoneResult>(`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/done`, json("POST", {
        expected_revision: detail.revision,
        tracks: tracks.map(({ id, selected, plate_text }) => ({ id, selected, plate_text })),
        settings,
      }));
      setDetail(result.clip); setTracks(result.clip.tracks); setSettings(result.clip.settings);
      setDoneMessage(`Saved on this PC. Repeating Done replaces the prior files at ${result.saved_to}.`);
      await refreshClips();
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Could not finish this clip"); await loadDetail(detail.clip.id, true); await refreshClips(); }
    finally { setSaving(false); }
  }, [detail, loadDetail, refreshClips, settings, tracks]);

  const selectClip = useCallback((clip: BoxClip) => {
    setDetail(null); setJobLogOpen(false); setCurrentMs(0); setPlaying(false); setVideoFailed(false);
    setFramePreview(false); setDecodedFrame(null); setShowBoxes(true); setZoom(1); setPan({ x: 0, y: 0 });
    void loadDetail(clip.id);
  }, [loadDetail]);

  const openAdjacentClip = useCallback((delta: -1 | 1) => {
    if (!detail || filteredClips.length === 0) return;
    const currentIndex = filteredClips.findIndex((clip) => clip.id === detail.clip.id);
    const target = currentIndex === -1
      ? filteredClips[0]
      : filteredClips[(currentIndex + delta + filteredClips.length) % filteredClips.length];
    if (target && target.id !== detail.clip.id) {
      selectClip(target);
    }
  }, [detail, filteredClips, selectClip]);

  const togglePlay = useCallback(() => {
    if (framePreview || videoFailed) { setPlaying((value) => !value); return; }
    const video = videoRef.current;
    if (!video) return;
    if (video.paused) void video.play().catch(() => setVideoFailed(true)); else video.pause();
  }, [framePreview, videoFailed]);

  const toggleFramePreview = useCallback(() => {
    const next = !framePreview;
    if (next) { videoRef.current?.pause(); setPlaying(false); }
    else if (videoRef.current) videoRef.current.currentTime = currentMs / 1000;
    setFramePreview(next);
  }, [currentMs, framePreview]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.defaultPrevented) return;
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        if (detail && !jobIsActive && !saving) { event.preventDefault(); void save(); }
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key === "Enter") {
        if (detail && !jobIsActive && !saving && detail.clip.status === "complete") { event.preventDefault(); void markDone(); }
        return;
      }
      if (editableTarget(event.target) || !detail) return;
      const key = event.key.toLowerCase();
      if (event.code === "Space" || key === "k") { event.preventDefault(); togglePlay(); return; }
      const frameStepMs = Math.max(1, Math.round(1000 / (detail.meta.fps || 25)));
      if (event.key === "ArrowLeft" || event.key === ",") {
        event.preventDefault();
        seekToMs(currentMs - (event.shiftKey ? 10 : 1) * frameStepMs);
        return;
      }
      if (event.key === "ArrowRight" || event.key === ".") {
        event.preventDefault();
        seekToMs(currentMs + (event.shiftKey ? 10 : 1) * frameStepMs);
        return;
      }
      if (event.key === "ArrowDown") { event.preventDefault(); cycleTrack(1, false); return; }
      if (event.key === "ArrowUp") { event.preventDefault(); cycleTrack(-1, false); return; }
      if (/^[1-9]$/.test(event.key)) {
        const idx = Number(event.key) - 1;
        if (idx < tracks.length) {
          event.preventDefault();
          const targetId = tracks[idx].id;
          setSelectedTrack(targetId);
          trackRowRefs.current[targetId]?.scrollIntoView?.({ block: "nearest" });
          seekToTrack(targetId);
        }
        return;
      }
      if (key === "g") { if (selectedTrack) { event.preventDefault(); seekToTrack(selectedTrack); } return; }
      if (key === "x") {
        if (selectedTrack && !jobIsActive) {
          event.preventDefault();
          const target = tracks.find((t) => t.id === selectedTrack);
          if (target) updateTrack(target.id, { selected: !target.selected });
        }
        return;
      }
      if (event.key === "Enter" || key === "e") {
        const target = event.target as HTMLElement | null;
        if (event.key === "Enter" && (target instanceof HTMLButtonElement || target instanceof HTMLAnchorElement || target?.tagName === "SUMMARY")) return;
        if (selectedTrack && !jobIsActive) { event.preventDefault(); focusTrackInput(selectedTrack); }
        return;
      }
      if (key === "b") { event.preventDefault(); setShowBoxes((shown) => !shown); return; }
      if (key === "f") { event.preventDefault(); toggleFramePreview(); return; }
      if (event.key === "+" || event.key === "=") { event.preventDefault(); setZoomAt(zoom * 1.25); return; }
      if (event.key === "-") { event.preventDefault(); setZoomAt(zoom / 1.25); return; }
      if (event.key === "0") { event.preventDefault(); setZoomAt(1); return; }
      if (key === "d") {
        if (!saving && !jobIsActive && detail.clip.status === "complete") { event.preventDefault(); void markDone(); }
        return;
      }
      if (key === "n") { event.preventDefault(); openAdjacentClip(1); return; }
      if (key === "p") { event.preventDefault(); openAdjacentClip(-1); return; }
      if (event.key === "Escape") {
        event.preventDefault();
        if (jobLogOpen) { setJobLogOpen(false); return; }
        requestSeq.current++;
        setDetail(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [currentMs, cycleTrack, detail, focusTrackInput, jobIsActive, jobLogOpen, markDone, openAdjacentClip, save, saving, seekToMs, seekToTrack, selectedTrack, setZoomAt, toggleFramePreview, togglePlay, tracks, zoom]);

  const frameUrl = detail ? `/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/frame?timestamp_ms=${Math.round(currentMs)}` : "";
  const showDecodedFrame = !!detail && (framePreview || videoFailed) && decodedFrame?.clipId === detail.clip.id;

  useEffect(() => {
    if (!detail || (!framePreview && !videoFailed)) { setFrameLoading(false); return; }
    let cancelled = false;
    setFrameLoading(true);
    const image = new Image();
    const publishDecodedImage = () => {
      if (cancelled) return;
      setDecodedFrame({ src: frameUrl, clipId: detail.clip.id, timestampMs: Math.round(currentMs) });
      setFrameLoading(false);
    };
    image.onload = () => {
      if (typeof image.decode === "function") void image.decode().then(publishDecodedImage).catch(() => { if (!cancelled) setFrameLoading(false); });
      else publishDecodedImage();
    };
    image.onerror = () => { if (!cancelled) setFrameLoading(false); };
    image.src = frameUrl;
    if (image.complete && image.naturalWidth > 0) image.onload?.(new Event("load"));
    return () => { cancelled = true; image.onload = null; image.onerror = null; };
  }, [detail, framePreview, frameUrl, videoFailed]);

  return <main className={`box-review ${detail ? "has-detail" : ""}`}>
    {!detail && <header className="box-header"><div className="box-brand"><span className="box-mark">BX</span><div><p className="eyebrow">PLATE REVIEW / DATASET</p><h1>Plate boxes</h1></div></div><div className="box-header-actions"><button className="secondary-button" onClick={onBack}>← Clip review</button><button className="secondary-button" aria-label="Refresh clips" onClick={() => void refreshClips()}>↻ Refresh</button></div></header>}
    {error && <div className="box-error" role="status">{error}</div>}
    {!detail ? <section className="box-library"><div className="box-library-heading"><div><p className="eyebrow">PLATE DATASET</p><h2>Plate clips</h2><p>Filter by review and segmentation status. Complete means a reviewer clicked Done.</p></div><span className="box-count">{filteredClips.length} of {clips.length} clips</span></div>
      <div className="box-library-controls"><label className="clip-filter"><span>Show</span><select aria-label="Filter clips by status" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value as typeof statusFilter)}><option value="all">All clips</option><option value="complete">Complete</option><option value="need_label">Need label</option><option value="failed">Failed</option><option value="unsegmented">Unsegmented</option><option value="processing">Processing (queued / running)</option></select></label><div className="box-batch-actions"><button className="secondary-button" disabled={!queueableIds.length || !!batchProgress} onClick={() => setSelectedClipIds(new Set(queueableIds))}>Select pending / failed</button><button className="primary-button" disabled={!selectedClipIds.size || !!batchProgress} onClick={() => void runBatch([...selectedClipIds])}>{batchProgress ? `Queueing ${batchProgress.done}/${batchProgress.total}…` : `Segment ${selectedClipIds.size} selected`}</button><button className="secondary-button" disabled={!queueableIds.length || !!batchProgress} onClick={() => void runBatch(queueableIds)}>Segment all pending / retry failed</button></div></div>
      {batchProgress && <div className="batch-progress" role="status"><span>Adding clips to segmentation queue one at a time</span><strong>{batchProgress.done} / {batchProgress.total}</strong><i style={{ width: `${batchProgress.done / batchProgress.total * 100}%` }} /></div>}
      {loading && <p className="box-loading">Loading clip…</p>}
      {(() => {
        const labels = { complete: "Complete", need_label: "Need label", failed: "Failed", unsegmented: "Unsegmented", processing: "Processing" } as const;
        return filteredClips.length ? <div className="clip-grid">{filteredClips.map((clip) => { const displayState = reviewState(clip); const displayLabel = labels[displayState as keyof typeof labels] ?? displayState; return <article className="clip-card-wrap" key={clip.id}>{(clip.status === "unsegmented" || clip.status === "failed") && <input className="clip-select" type="checkbox" aria-label={`Select ${clip.name} for segmentation`} checked={selectedClipIds.has(clip.id)} disabled={!!batchProgress} onChange={() => toggleClipSelection(clip.id)} />}<button className="clip-card" onClick={() => selectClip(clip)}><span className={`clip-status ${displayState}`}>{displayLabel}</span><strong>{clip.name}</strong><span>{clock(clip.duration_ms)} <i>·</i> {size(clip.bytes)}</span>{clip.status === "running" || clip.status === "queued" ? <span className="clip-progress"><i style={{ width: `${Math.round(clip.progress * 100)}%` }} /></span> : null}{clip.error && <span className="clip-error">{clip.error}</span>}</button></article>;})}</div> : !loading && <div className="box-empty"><span>▧</span><h3>{statusFilter === "all" ? "No clips available" : `No ${statusFilter === "need_label" ? "clips need labeling" : statusFilter === "unsegmented" ? "unsegmented clips" : `${statusFilter} clips`}`}</h3><p>Choose another status filter to see clips in a different state.</p></div>;
      })()}
    </section> : <>
      <div className="box-detail-heading"><button className="back-button" title="Back to clips (Esc)" onClick={() => { requestSeq.current++; setJobLogOpen(false); setDetail(null); }}>←</button><div className="box-detail-title"><p className="eyebrow">TRIMMED CLIP</p><h2>{detail.clip.name}</h2><span>{clock(duration)} · {detail.meta.width} × {detail.meta.height} · {detail.meta.fps.toFixed(2)} fps · {detail.meta.frame_count.toLocaleString()} frames</span></div><div className="box-detail-nav"><button className="secondary-button" aria-label="Previous clip" title="Previous clip (P)" disabled={filteredClips.length <= 1} onClick={() => openAdjacentClip(-1)}>Prev <kbd>P</kbd></button><button className="secondary-button" aria-label="Next clip" title="Next clip (N)" disabled={filteredClips.length <= 1} onClick={() => openAdjacentClip(1)}>Next <kbd>N</kbd></button>{activeJob && <div className="box-job-popover"><button className={`secondary-button box-job-trigger ${jobLogOpen ? "active" : ""}`} aria-expanded={jobLogOpen} aria-label="Segmentation log" onClick={() => setJobLogOpen((open) => !open)}>Log <span>{Math.round(activeJob.progress * 100)}%</span></button>{jobLogOpen && <section className="box-job-popup" role="dialog" aria-label="Segmentation progress and log" aria-live="polite"><div className="box-job-popup-head"><div className="job-title"><strong>{activeJob.phase || activeJob.status}</strong><span>{Math.round(activeJob.progress * 100)}%</span></div><button className="box-job-close" aria-label="Close segmentation log" onClick={() => setJobLogOpen(false)}>×</button></div><div className="box-progress"><i style={{ width: `${Math.round(activeJob.progress * 100)}%` }} /></div>{activeJob.error && <p className="clip-error">{activeJob.error}</p>}<div className="box-job-log-meta"><span>Segmentation log</span><span>{activeJob.logs.length} lines</span></div><pre>{activeJob.logs.length ? activeJob.logs.join("\n") : jobIsActive ? "Waiting for worker output…" : "No worker output recorded."}</pre></section>}</div>}<button className="secondary-button" aria-label="Refresh clips" onClick={() => void refreshClips()}>↻</button><span className={`clip-status ${detail.clip.status === "complete" ? detail.clip.review_state : detail.clip.status}`}>{detail.clip.status === "complete" ? (detail.clip.review_state === "need_label" ? "Need label" : "Complete") : detail.clip.status}</span></div></div>
      <div className="box-layout"><section className="box-viewer"><div className="box-viewer-toolbar"><div><span className="live-dot" /> RAW SOURCE <span>·</span> {detail.clip.name}</div><div className="box-zoom-controls"><button aria-label="Zoom out" disabled={zoom <= 1} onClick={() => setZoomAt(zoom / 1.25)}>−</button><span>{Math.round(zoom * 100)}%</span><button aria-label="Zoom in" disabled={zoom >= 8} onClick={() => setZoomAt(zoom * 1.25)}>+</button><button onClick={() => setZoomAt(1)}>Reset</button></div><span>{clock(currentMs)} / {clock(duration)}</span></div><div className={`box-stage ${zoom > 1 ? "zoomed" : ""}`} ref={stageRef} onPointerDown={onStagePointerDown} onPointerMove={onStagePointerMove} onPointerUp={onStagePointerUp} onPointerCancel={onStagePointerUp}>
        <video ref={videoRef} className={showDecodedFrame ? "box-video-hidden" : ""} src={`/api/boxes/clips/${encodeURIComponent(detail.clip.id)}/stream`} preload="metadata" playsInline onTimeUpdate={(event) => setCurrentMs(event.currentTarget.currentTime * 1000)} onPlay={() => { if (!framePreview && !videoFailed) setPlaying(true); }} onPause={() => { if (!framePreview && !videoFailed) setPlaying(false); }} onError={() => setVideoFailed(true)} style={{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }} />
        {showDecodedFrame && <div className="box-decoded-frame"><img draggable={false} onDragStart={(event) => event.preventDefault()} src={decodedFrame.src} alt={`Decoded source frame at ${clock(decodedFrame.timestampMs)}`} style={{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }} />{(videoFailed || framePreview) && <span>{videoFailed ? "Video playback unavailable · showing decoded frame" : "Decoded frame preview · 5 frames per second"}{frameLoading ? " · loading next frame" : ""}</span>}</div>}
        {(framePreview || videoFailed) && !showDecodedFrame && <div className="box-frame-loading" role="status">{frameLoading ? "Loading decoded source frame…" : "Decoded source frame unavailable"}</div>}
        {showBoxes && <svg className="box-overlay" style={{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }} viewBox={`0 0 ${detail.meta.width} ${detail.meta.height}`} preserveAspectRatio="xMidYMid meet" role="img" aria-label="Detected plate boxes">{visibleTracks.map((track) => { const detection = activeDetection(track); if (!detection) return null; const active = selectedTrack === track.id; return <g key={track.id} data-track-id={track.id} onClick={() => { if (!didPan.current) focusTrackInput(track.id); }} className={active ? "box-active" : ""}><rect x={detection.x_min} y={detection.y_min} width={detection.x_max - detection.x_min} height={detection.y_max - detection.y_min} /><text x={detection.x_min} y={Math.max(16, detection.y_min - 7)}>{track.id} · {(detection.confidence * 100).toFixed(0)}%</text></g>; })}</svg>}
      </div><div className="box-shortcuts-hint"><kbd>Space</kbd> Play · <kbd>←/→</kbd> Frame · <kbd>↑/↓</kbd> Track · <kbd>Enter</kbd> Label · <kbd>X</kbd> Include · <kbd>G</kbd> Jump · <kbd>B</kbd> Boxes · <kbd>F</kbd> Frame · <kbd>Ctrl+S</kbd> Save · <kbd>Ctrl+Enter</kbd> Done · <kbd>N/P</kbd> Clip</div><div className="box-transport"><button aria-label={playing ? "Pause clip" : "Play clip"} onClick={togglePlay}>{playing ? "Ⅱ" : "▶"}</button><input aria-label="Clip timeline" type="range" min="0" max={duration || 1} step="1" value={Math.min(currentMs, duration)} onChange={(event) => seekToMs(Number(event.target.value))} /><span>{clock(currentMs)}</span><button className="preview-toggle" onClick={toggleFramePreview}>{framePreview ? "Video" : "Frame preview"}</button><button className="preview-toggle" aria-pressed={showBoxes} onClick={() => setShowBoxes((shown) => !shown)}>{showBoxes ? "Hide boxes" : "Show boxes"}</button></div></section>
      <aside className="box-sidebar"><section className="box-panel box-tracks-panel"><div className="box-panel-title"><div><p className="eyebrow">TRACK REVIEW</p><h3>Plate candidates <span>{tracks.length}</span></h3></div><span className="box-small-note">No OCR</span></div>{tracks.length ? <div className="track-list">{tracks.map((track) => <div ref={(el) => { trackRowRefs.current[track.id] = el; }} className={`track-row ${selectedTrack === track.id ? "focused" : ""}`} key={track.id} onClick={() => focusTrackInput(track.id)}><label className="track-toggle" onClick={(event) => event.stopPropagation()}><input type="checkbox" disabled={jobIsActive} checked={track.selected} onChange={(event) => updateTrack(track.id, { selected: event.target.checked })} aria-label={`Include box ${track.id}`} /><span>{track.id}</span></label><input ref={(el) => { trackInputRefs.current[track.id] = el; }} className="plate-entry" disabled={jobIsActive} aria-label={`Plate text for box ${track.id}`} placeholder="Enter plate text" value={track.plate_text} onFocus={() => setSelectedTrack(track.id)} onChange={(event) => { const value = event.target.value; const hasText = value.trim().length > 0; updateTrack(track.id, { plate_text: value, selected: hasText }); }} onBlur={() => { if (track.plate_text.trim().length > 0 && !track.selected) updateTrack(track.id, { selected: true }); }} onKeyDown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") { event.preventDefault(); if (!saving && !jobIsActive) void save(); return; } if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); if (!saving && !jobIsActive && detail.clip.status === "complete") void markDone(); return; } if (event.key === "Enter" || event.key === "Escape") { event.preventDefault(); event.currentTarget.blur(); return; } if (event.key === "ArrowDown") { event.preventDefault(); cycleTrack(1, true); return; } if (event.key === "ArrowUp") { event.preventDefault(); cycleTrack(-1, true); } }} /><div className="track-meta"><small>{visibleTracks.find((item) => item.id === track.id)?.detections.length ?? 0} frames after filtering</small><button type="button" className="track-jump" title="Jump to track frame (G)" onClick={(event) => { event.stopPropagation(); setSelectedTrack(track.id); seekToTrack(track.id); }}>Jump</button></div></div>)}</div> : <p className="box-hint">No tracks yet. Run segmentation to find plate candidates.</p>}</section>
        <section className="box-panel box-settings">
          <details className="box-settings-details" open>
            <summary className="box-panel-title"><div><p className="eyebrow">REVIEW FILTERS</p><h3>Box filtering</h3></div><span className="box-small-note">Toggle</span></summary>
            <div className="box-settings-body">
              <p className="box-filter-disclaimer">These filters affect boxes shown and exported. They do not rerun SAM.</p>
              <label>Minimum confidence <output>{settings.min_confidence.toFixed(2)}</output><input disabled={jobIsActive} aria-label="Minimum confidence" type="range" min="0" max="1" step="0.01" value={settings.min_confidence} onChange={(event) => updateSetting("min_confidence", Number(event.target.value))} /><small>Ignore detections with a lower confidence score.</small></label>
              <label>Maximum box area <output>{(settings.max_area_fraction * 100).toFixed(1)}%</output><input disabled={jobIsActive} aria-label="Maximum area fraction" type="range" min="0.001" max="0.25" step="0.001" value={settings.max_area_fraction} onChange={(event) => updateSetting("max_area_fraction", Number(event.target.value))} /><small>Drop boxes larger than this share of the full frame.</small></label>
              <label>Maximum size ratio <output>{settings.max_size_ratio.toFixed(1)}×</output><input disabled={jobIsActive} aria-label="Maximum size ratio" type="range" min="1" max="10" step="0.1" value={settings.max_size_ratio} onChange={(event) => updateSetting("max_size_ratio", Number(event.target.value))} /><small>Drop boxes much larger or smaller than nearby median width or height.</small></label>
              <label>Median jitter <span>px</span><input disabled={jobIsActive} aria-label="Median jitter" type="number" min="0" max="100" step="1" value={settings.jitter_px} onChange={(event) => updateSetting("jitter_px", Number(event.target.value))} /><small>Smooth small box-edge changes within this many pixels. Larger changes remain as detected.</small></label>
              <label>Smoothing window <span>frames</span><input disabled={jobIsActive} aria-label="Smoothing window" type="number" min="0" max="30" step="1" value={settings.smoothing_radius} onChange={(event) => updateSetting("smoothing_radius", Number(event.target.value))} /><small>Compare a detection with neighbors within this many frames.</small></label>
              <label>Minimum track length <span>frames</span><input disabled={jobIsActive} aria-label="Minimum frames" type="number" min="1" max="10000" step="1" value={settings.minimum_frames} onChange={(event) => updateSetting("minimum_frames", Number(event.target.value))} /><small>Keep tracks with at least this many frames after filtering.</small></label>
              <p className="box-hint">Boxes with plate text auto-include; toggle anytime. Plate text is entered manually.{jobIsActive && " Editing is paused while segmentation runs."}</p>
            </div>
          </details>
        </section>
        <div className="box-actions"><button className="secondary-button" onClick={() => void save()} disabled={saving || jobIsActive}>{saving ? "Saving…" : "Save review"}</button><button className="primary-button" onClick={() => void segment()} disabled={jobIsActive || detail.clip.status === "complete"}>{jobIsActive ? "Segmentation running" : detail.clip.status === "complete" ? "Segmentation complete" : "Segment plates"}</button><button className="export-dataset" onClick={() => void markDone()} disabled={saving || jobIsActive || detail.clip.status !== "complete"}>{saving ? "Saving…" : detail.clip.review_state === "complete" ? "Done · overwrite saved version" : "Done"}</button><button className="secondary-button next-clip-action" disabled={filteredClips.length <= 1} onClick={() => openAdjacentClip(1)}>Next clip <kbd>N</kbd></button>{doneMessage && <p className="done-message" role="status">{doneMessage}</p>}</div>
      </aside></div>
    </>}
  </main>;
}
