import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from "react";
import { ApiError, api, json } from "./api";
import type { ExportJob, Interval, ReviewDraft, User, Video, VideoDetail } from "./contracts";
import { draftKey, loadDraft, saveDraft } from "./storage";

type SyncState = "saved" | "saving" | "offline" | "conflict" | "local_error";

const shortSteps: Record<string, number> = { ArrowLeft: -0.1, ArrowRight: 0.1 };

function sessionClientId(): string {
  let id = sessionStorage.getItem("plate-review-client-id");
  if (!id) {
    id = crypto.randomUUID();
    sessionStorage.setItem("plate-review-client-id", id);
  }
  return id;
}

function fmtTime(milliseconds: number): string {
  const seconds = Math.max(0, Math.floor(milliseconds / 1000));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  return hours ? `${hours}:${String(minutes).padStart(2, "0")}:${String(rest).padStart(2, "0")}` : `${minutes}:${String(rest).padStart(2, "0")}`;
}

function dateLabel(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(date);
}

function humanBytes(value: number): string {
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(0)} KB`;
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}

function newDraft(user: User, detail: VideoDetail, clientId: string): ReviewDraft {
  return {
    key: draftKey(user.id, detail.video.id), userId: user.id, videoId: detail.video.id,
    clientId, intervals: detail.video.review.intervals, pendingStartMs: null,
    status: detail.video.review.status, baseRevision: detail.video.review.revision,
    pendingMutationId: null, positionMs: detail.position_ms, zoom: detail.zoom, updatedAtMs: Date.now(),
  };
}

function editableTarget(target: EventTarget | null): boolean {
  return target instanceof HTMLElement && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
}

export default function App() {
  const [user, setUser] = useState<User | null>(null);
  const [loginName, setLoginName] = useState("");
  const [loginPassword, setLoginPassword] = useState("");
  const [loginError, setLoginError] = useState("");
  const [videos, setVideos] = useState<Video[]>([]);
  const [jobs, setJobs] = useState<ExportJob[]>([]);
  const [jobsExpanded, setJobsExpanded] = useState(true);
  const [statusFilter, setStatusFilter] = useState("all");
  const [cameraFilter, setCameraFilter] = useState("all");
  const [search, setSearch] = useState("");
  const [activeId, setActiveId] = useState<string | null>(null);
  const [detail, setDetail] = useState<VideoDetail | null>(null);
  const [draft, setDraft] = useState<ReviewDraft | null>(null);
  const [syncState, setSyncState] = useState<SyncState>("saved");
  const [readOnlyReason, setReadOnlyReason] = useState("");
  const [error, setError] = useState("");
  const [claimHeld, setClaimHeld] = useState(false);
  const [currentTimeMs, setCurrentTimeMs] = useState(0);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState(1);
  const [zoom, setZoom] = useState(1);
  const [pan, setPan] = useState({ x: 0, y: 0 });
  const [helpOpen, setHelpOpen] = useState(false);
  const [selectedForExport, setSelectedForExport] = useState<Set<string>>(new Set());
  const [isOnline, setIsOnline] = useState(navigator.onLine);
  const [now, setNow] = useState(Date.now());
  const videoRef = useRef<HTMLVideoElement>(null);
  const stageRef = useRef<HTMLDivElement>(null);
  const draftRef = useRef<ReviewDraft | null>(null);
  const detailRef = useRef<VideoDetail | null>(null);
  const userRef = useRef<User | null>(null);
  const claimRef = useRef(false);
  const clientIdRef = useRef(sessionClientId());
  const flushRef = useRef<() => Promise<void>>(async () => undefined);
  const flushInFlight = useRef<Promise<void> | null>(null);
  const openingSequence = useRef(0);
  const undoStack = useRef<Interval[][]>([]);
  const redoStack = useRef<Interval[][]>([]);
  const dragOrigin = useRef<{ x: number; y: number; panX: number; panY: number } | null>(null);
  const replayEndMs = useRef<number | null>(null);

  useEffect(() => { userRef.current = user; }, [user]);
  useEffect(() => { draftRef.current = draft; }, [draft]);
  useEffect(() => { detailRef.current = detail; }, [detail]);
  useEffect(() => { claimRef.current = claimHeld; }, [claimHeld]);

  const refreshVideos = useCallback(async () => {
    if (!userRef.current) return;
    const params = new URLSearchParams();
    if (statusFilter !== "all") params.set("status", statusFilter);
    if (cameraFilter !== "all") params.set("camera", cameraFilter);
    if (search.trim()) params.set("q", search.trim());
    try { setVideos(await api<Video[]>(`/api/videos?${params}`)); } catch (reason) { if (reason instanceof ApiError && reason.status === 401) setUser(null); }
  }, [cameraFilter, search, statusFilter]);

  const refreshJobs = useCallback(async () => {
    if (!userRef.current) return;
    try { setJobs(await api<ExportJob[]>("/api/exports")); } catch { }
  }, []);

  useEffect(() => {
    api<{ user: User }>("/api/auth/me").then((result) => setUser(result.user)).catch(() => setUser(null));
  }, []);

  useEffect(() => {
    if (!user) return;
    void refreshVideos();
    void refreshJobs();
    const libraryTimer = window.setInterval(() => void refreshVideos(), 10_000);
    const jobsTimer = window.setInterval(() => void refreshJobs(), 1500);
    const clock = window.setInterval(() => setNow(Date.now()), 1000);
    return () => { clearInterval(libraryTimer); clearInterval(jobsTimer); clearInterval(clock); };
  }, [refreshJobs, refreshVideos, user]);

  useEffect(() => {
    const online = () => setIsOnline(true);
    const offline = () => setIsOnline(false);
    window.addEventListener("online", online);
    window.addEventListener("offline", offline);
    return () => { window.removeEventListener("online", online); window.removeEventListener("offline", offline); };
  }, []);

  const commitLocal = useCallback(async (next: ReviewDraft, sync = true) => {
    next.updatedAtMs = Date.now();
    try {
      await saveDraft(next);
      draftRef.current = next;
      setDraft(next);
      setSyncState(next.pendingMutationId ? (isOnline ? "saving" : "offline") : "saved");
      if (sync && next.pendingMutationId) void flushRef.current();
    } catch (reason) {
      setSyncState("local_error");
      setError(reason instanceof Error ? reason.message : "The browser could not save this draft");
    }
  }, [isOnline]);

  const flush = useCallback(async () => {
    if (flushInFlight.current) return flushInFlight.current;
    const task = (async () => {
      while (isOnline && claimRef.current) {
        const current = draftRef.current;
        if (!current?.pendingMutationId) break;
        setSyncState("saving");
        try {
          const saved = await api<Video["review"]>(`/api/videos/${current.videoId}/review`, json("PUT", {
            client_id: current.clientId,
            mutation_id: current.pendingMutationId,
            expected_revision: current.baseRevision,
            status: current.status,
            intervals: current.intervals,
          }));
          const latest = draftRef.current;
          if (!latest || latest.videoId !== current.videoId) break;
          const next = { ...latest, baseRevision: saved.revision, pendingMutationId: latest.pendingMutationId === current.pendingMutationId ? null : latest.pendingMutationId };
          await saveDraft(next);
          draftRef.current = next;
          setDraft(next);
          setSyncState(next.pendingMutationId ? "saving" : "saved");
          setDetail((previous) => previous?.video.id === next.videoId ? { ...previous, video: { ...previous.video, review: saved } } : previous);
          void refreshVideos();
        } catch (reason) {
          if (reason instanceof ApiError && reason.status === 409) {
            claimRef.current = false;
            setClaimHeld(false);
            setSyncState("conflict");
            setReadOnlyReason(reason.message);
          } else {
            setSyncState(navigator.onLine ? "saving" : "offline");
          }
          break;
        }
      }
    })().finally(() => { flushInFlight.current = null; });
    flushInFlight.current = task;
    return task;
  }, [isOnline, refreshVideos]);
  useEffect(() => { flushRef.current = flush; }, [flush]);
  useEffect(() => { if (isOnline && draft?.pendingMutationId && claimHeld) void flushRef.current(); }, [claimHeld, draft?.pendingMutationId, isOnline]);
  useEffect(() => {
    if (!draft?.pendingMutationId || !claimHeld || !isOnline) return;
    const timer = window.setInterval(() => void flushRef.current(), 5000);
    return () => clearInterval(timer);
  }, [claimHeld, draft?.pendingMutationId, isOnline]);

  const openVideo = useCallback(async (video: Video) => {
    const previousId = detailRef.current?.video.id;
    if (previousId && previousId !== video.id && claimRef.current) {
      void api(`/api/videos/${previousId}/claim`, json("DELETE", { client_id: clientIdRef.current })).catch(() => undefined);
    }
    const sequence = ++openingSequence.current;
    setError("");
    setActiveId(video.id);
    setDetail(null);
    setDraft(null);
    setClaimHeld(false);
    claimRef.current = false;
    setReadOnlyReason("");
    setSyncState(isOnline ? "saved" : "offline");
    setCurrentTimeMs(0);
    setZoom(1);
    setPan({ x: 0, y: 0 });
    undoStack.current = [];
    redoStack.current = [];
    try {
      const loaded = await api<VideoDetail>(`/api/videos/${video.id}`);
      if (sequence !== openingSequence.current) return;
      setDetail(loaded);
      let acquired = false;
      if (isOnline && video.source_status === "available") {
        try {
          await api<Video>(`/api/videos/${video.id}/claim`, json("POST", { client_id: clientIdRef.current }));
          acquired = true;
        } catch (reason) {
          setReadOnlyReason(reason instanceof Error ? reason.message : "Another reviewer is editing this video");
        }
      } else if (video.source_status !== "available") {
        setReadOnlyReason(`Source is ${video.source_status}`);
      }
      if (sequence !== openingSequence.current) {
        if (acquired) void api(`/api/videos/${video.id}/claim`, json("DELETE", { client_id: clientIdRef.current })).catch(() => undefined);
        return;
      }
      claimRef.current = acquired;
      setClaimHeld(acquired);
      const signedIn = userRef.current;
      if (!signedIn) return;
      const key = draftKey(signedIn.id, video.id);
      const local = await loadDraft(key);
      const base = newDraft(signedIn, loaded, clientIdRef.current);
      const restored = local?.pendingMutationId
        ? { ...base, ...local, key, userId: signedIn.id, videoId: video.id, clientId: clientIdRef.current }
        : { ...base, positionMs: local?.positionMs ?? loaded.position_ms, zoom: local?.zoom ?? loaded.zoom };
      setCurrentTimeMs(restored.positionMs);
      setZoom(restored.zoom);
      await commitLocal(restored, acquired);
      if (local?.pendingMutationId && !acquired) {
        setSyncState(isOnline ? "conflict" : "offline");
        setReadOnlyReason(isOnline ? "Claim this video to sync its saved draft." : "Reconnect to claim this video and sync its saved draft.");
      }
    } catch (reason) {
      if (sequence === openingSequence.current) setError(reason instanceof Error ? reason.message : "Cannot load video");
    }
  }, [commitLocal, isOnline]);

  useEffect(() => {
    if (!activeId || !claimHeld || !isOnline) return;
    const timer = window.setInterval(async () => {
      try { await api(`/api/videos/${activeId}/claim/heartbeat`, json("POST", { client_id: clientIdRef.current })); }
      catch (reason) {
        claimRef.current = false;
        setClaimHeld(false);
        setReadOnlyReason(reason instanceof Error ? reason.message : "The edit claim expired");
        setSyncState("conflict");
      }
    }, 20_000);
    return () => clearInterval(timer);
  }, [activeId, claimHeld, isOnline]);

  useEffect(() => {
    if (!activeId || !user) return;
    const videoId = activeId;
    const timer = window.setInterval(() => {
      const current = draftRef.current;
      const element = videoRef.current;
      if (!current || current.videoId !== videoId || !element) return;
      const next = { ...current, positionMs: Math.round(element.currentTime * 1000), zoom };
      void commitLocal(next, false);
      if (isOnline) void api(`/api/videos/${videoId}/position`, json("PUT", { position_ms: next.positionMs, zoom })).catch(() => undefined);
    }, 2000);
    return () => clearInterval(timer);
  }, [activeId, commitLocal, isOnline, user, zoom]);

  const changeVideoState = useCallback((update: (current: ReviewDraft) => ReviewDraft, addHistory = false) => {
    const current = draftRef.current;
    if (!current || !claimRef.current) return;
    if (addHistory) {
      undoStack.current.push(current.intervals);
      if (undoStack.current.length > 100) undoStack.current.shift();
      redoStack.current = [];
    }
    const changed = update(current);
    const next = { ...changed, pendingMutationId: crypto.randomUUID(), baseRevision: current.baseRevision, status: changed.status === "reviewed" ? "in_progress" : changed.status };
    void commitLocal(next);
  }, [commitLocal]);

  const markIn = useCallback(() => changeVideoState((current) => ({ ...current, pendingStartMs: currentTimeMs, status: "in_progress" })), [changeVideoState, currentTimeMs]);
  const markOut = useCallback(() => {
    const current = draftRef.current;
    if (!current?.pendingStartMs && current?.pendingStartMs !== 0) { setError("Set an interval start with I first."); return; }
    if (currentTimeMs <= current.pendingStartMs) { setError("The end must be after the start."); return; }
    setError("");
    const interval: Interval = { id: crypto.randomUUID(), start_ms: current.pendingStartMs, end_ms: currentTimeMs };
    changeVideoState((state) => ({ ...state, intervals: [...state.intervals, interval].sort((a,b) => a.start_ms-b.start_ms), pendingStartMs: null, status: "in_progress" }), true);
  }, [changeVideoState, currentTimeMs]);

  const markReviewed = useCallback(() => {
    const current = draftRef.current;
    if (!current || !claimRef.current) return;
    void commitLocal({ ...current, pendingStartMs: null, status: "reviewed", pendingMutationId: crypto.randomUUID() });
    void refreshVideos();
  }, [commitLocal, refreshVideos]);

  const undo = useCallback(() => {
    const previous = undoStack.current.pop();
    const current = draftRef.current;
    if (!previous || !current || !claimRef.current) return;
    redoStack.current.push(current.intervals);
    changeVideoState((state) => ({ ...state, intervals: previous, status: "in_progress" }));
  }, [changeVideoState]);

  const redo = useCallback(() => {
    const next = redoStack.current.pop();
    const current = draftRef.current;
    if (!next || !current || !claimRef.current) return;
    undoStack.current.push(current.intervals);
    changeVideoState((state) => ({ ...state, intervals: next, status: "in_progress" }));
  }, [changeVideoState]);

  const removeInterval = useCallback((id: string) => changeVideoState((current) => ({ ...current, intervals: current.intervals.filter((interval) => interval.id !== id), status: "in_progress" }), true), [changeVideoState]);

  const adjustInterval = useCallback((id: string, edge: "start_ms" | "end_ms") => {
    const interval = draftRef.current?.intervals.find((item) => item.id === id);
    if (!interval) return;
    const time = currentTimeMs;
    const next = edge === "start_ms" ? { ...interval, start_ms: time } : { ...interval, end_ms: time };
    if (next.start_ms >= next.end_ms) { setError("The adjusted boundary must leave the interval longer than zero."); return; }
    setError("");
    changeVideoState((current) => ({ ...current, intervals: current.intervals.map((item) => item.id === id ? next : item).sort((a,b) => a.start_ms-b.start_ms), status: "in_progress" }), true);
  }, [changeVideoState, currentTimeMs]);

  const replayInterval = useCallback((interval: Interval) => {
    const video = videoRef.current;
    if (!video) return;
    replayEndMs.current = interval.end_ms;
    video.currentTime = interval.start_ms / 1000;
    void video.play().catch(() => setError("Video playback failed"));
  }, []);

  const applyDraft = useCallback(async () => {
    if (!activeId || !user || !draftRef.current || !detailRef.current) return;
    try {
      const loaded = await api<VideoDetail>(`/api/videos/${activeId}`);
      await api<Video>(`/api/videos/${activeId}/claim`, json("POST", { client_id: clientIdRef.current }));
      claimRef.current = true;
      setClaimHeld(true);
      const current = draftRef.current;
      await commitLocal({ ...current, baseRevision: loaded.video.review.revision, pendingMutationId: crypto.randomUUID() });
      setReadOnlyReason("");
      setSyncState("saving");
    } catch (reason) {
      setReadOnlyReason(reason instanceof Error ? reason.message : "Could not reclaim the video");
    }
  }, [activeId, commitLocal, user]);

  const releaseAndClose = useCallback(async () => {
    const id = activeId;
    if (id && claimRef.current) await api(`/api/videos/${id}/claim`, json("DELETE", { client_id: clientIdRef.current })).catch(() => undefined);
    openingSequence.current++;
    claimRef.current = false;
    setClaimHeld(false);
    setActiveId(null);
    setDetail(null);
    setDraft(null);
    setError("");
  }, [activeId]);

  const togglePlay = useCallback(() => {
    const video = videoRef.current;
    if (!video) return;
    if (video.paused) void video.play().catch(() => setError("Video playback failed"));
    else video.pause();
  }, []);

  const seek = useCallback((deltaMs: number) => {
    const video = videoRef.current;
    if (!video) return;
    video.currentTime = Math.max(0, Math.min(video.duration || 0, video.currentTime + deltaMs / 1000));
  }, []);

  const changeZoom = useCallback((next: number) => {
    const value = Math.max(0.25, Math.min(8, Number(next.toFixed(2))));
    setZoom(value);
    setPan(value === 1 ? { x: 0, y: 0 } : (previous) => previous);
    const current = draftRef.current;
    if (current) void commitLocal({ ...current, zoom: value }, false);
  }, [commitLocal]);

  const nativeZoom = useCallback(() => {
    const video = videoRef.current;
    const stage = stageRef.current;
    if (!video?.videoWidth || !stage) return;
    const factor = Math.max(video.videoWidth / stage.clientWidth, video.videoHeight / stage.clientHeight);
    changeZoom(Math.min(8, Math.max(1, factor)));
  }, [changeZoom]);

  const jumpNext = useCallback(async () => {
    const candidate = videos.find((video) => video.id !== activeId && video.source_status === "available" && video.review.status !== "reviewed" && (!video.claim || video.claim.mine || video.claim.expires_at_ms <= Date.now()));
    if (candidate) await openVideo(candidate);
  }, [activeId, openVideo, videos]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (editableTarget(event.target)) return;
      const video = videoRef.current;
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "z") { event.preventDefault(); if (event.shiftKey) redo(); else undo(); return; }
      if (event.key === "?") { event.preventDefault(); setHelpOpen((value) => !value); return; }
      if (!activeId || !video) return;
      if (event.code === "Space" || event.key.toLowerCase() === "k") { event.preventDefault(); togglePlay(); return; }
      if (event.key.toLowerCase() === "i") { event.preventDefault(); markIn(); return; }
      if (event.key.toLowerCase() === "o") { event.preventDefault(); markOut(); return; }
      if (event.key.toLowerCase() === "r") { event.preventDefault(); markReviewed(); return; }
      if (event.key.toLowerCase() === "n") { event.preventDefault(); void jumpNext(); return; }
      if (event.key.toLowerCase() === "j" || event.key.toLowerCase() === "l") {
        event.preventDefault();
        const rates = [0.25,0.5,1,2,4];
        const current = rates.indexOf(speed);
        const next = Math.max(0, Math.min(rates.length-1, current + (event.key.toLowerCase()==="j" ? -1 : 1)));
        setSpeed(rates[next]);
        video.playbackRate = rates[next];
        return;
      }
      if (shortSteps[event.key] !== undefined) { event.preventDefault(); seek(shortSteps[event.key] * (event.shiftKey ? 10 : event.altKey ? 100 : 1) * 1000); return; }
      if (event.key === "+" || event.key === "=") { event.preventDefault(); changeZoom(zoom * 1.25); return; }
      if (event.key === "-") { event.preventDefault(); changeZoom(zoom / 1.25); return; }
      if (event.key === "0") { event.preventDefault(); changeZoom(1); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [activeId, changeZoom, jumpNext, markIn, markOut, markReviewed, redo, seek, speed, togglePlay, undo, zoom]);

  const handleStageWheel = (event: React.WheelEvent<HTMLDivElement>) => {
    event.preventDefault();
    const rectangle = event.currentTarget.getBoundingClientRect();
    const x = event.clientX - rectangle.left - rectangle.width / 2;
    const y = event.clientY - rectangle.top - rectangle.height / 2;
    const next = Math.max(0.25, Math.min(8, zoom * (event.deltaY < 0 ? 1.12 : 1 / 1.12)));
    const ratio = next / zoom;
    setPan((current) => ({ x: x - ratio * (x - current.x), y: y - ratio * (y - current.y) }));
    changeZoom(next);
  };

  const onPointerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (zoom <= 1 || event.button !== 0) return;
    dragOrigin.current = { x: event.clientX, y: event.clientY, panX: pan.x, panY: pan.y };
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const onPointerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!dragOrigin.current) return;
    setPan({ x: dragOrigin.current.panX + event.clientX - dragOrigin.current.x, y: dragOrigin.current.panY + event.clientY - dragOrigin.current.y });
  };
  const onPointerUp = () => { dragOrigin.current = null; };

  const onTimeUpdate = () => {
    const value = videoRef.current?.currentTime ?? 0;
    setCurrentTimeMs((previous) => Math.abs(previous - value * 1000) > 80 ? Math.round(value * 1000) : previous);
    if (replayEndMs.current !== null && value * 1000 >= replayEndMs.current) {
      replayEndMs.current = null;
      videoRef.current?.pause();
    }
  };

  const onTimelineClick = (event: React.MouseEvent<HTMLDivElement>) => {
    const duration = detail?.video.duration_ms ?? 0;
    const rectangle = event.currentTarget.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (event.clientX - rectangle.left) / rectangle.width));
    if (videoRef.current) videoRef.current.currentTime = ratio * duration / 1000;
  };

  const submitLogin = async (event: React.FormEvent) => {
    event.preventDefault();
    setLoginError("");
    try {
      const result = await api<{ user: User }>("/api/auth/login", json("POST", { username: loginName, password: loginPassword }));
      setLoginPassword("");
      setUser(result.user);
    } catch (reason) { setLoginError(reason instanceof Error ? reason.message : "Sign-in failed"); }
  };

  const logout = async () => {
    await releaseAndClose();
    await api("/api/auth/logout", json("POST", {})).catch(() => undefined);
    setUser(null);
    setVideos([]);
    setJobs([]);
  };

  const cameras = useMemo(() => [...new Map(videos.map((video) => [video.camera_id, video.camera_name])).entries()], [videos]);
  const activeVideo = detail?.video;
  const durationMs = activeVideo?.duration_ms ?? 0;
  const pendingStart = draft?.pendingStartMs;
  const isConflict = syncState === "conflict";

  const toggleExportSelection = (id: string) => {
    setSelectedForExport((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id); else next.add(id);
      return next;
    });
  };

  const exportSelected = async () => {
    try {
      const created = await api<ExportJob>("/api/exports", json("POST", { video_ids: [...selectedForExport], padding_ms: 500 }));
      setSelectedForExport(new Set());
      setJobs((current) => [created, ...current]);
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Export could not start"); }
  };

  const activeJobCount = jobs.filter((job) => job.status === "queued" || job.status === "running").length;

  const toggleFullscreen = () => {
    if (stageRef.current?.requestFullscreen) void stageRef.current.requestFullscreen();
  };

  if (!user) {
    return <main className="login-shell">
      <form className="login-card" onSubmit={submitLogin}>
        <div className="brand-mark">PR</div>
        <p className="eyebrow">FIELD REVIEW</p>
        <h1>Plate Review</h1>
        <p className="muted">Sign in with an account created by your administrator.</p>
        <label>Username<input autoComplete="username" value={loginName} onChange={(event) => setLoginName(event.target.value)} required /></label>
        <label>Password<input type="password" autoComplete="current-password" value={loginPassword} onChange={(event) => setLoginPassword(event.target.value)} required /></label>
        {loginError && <p className="error-text">{loginError}</p>}
        <button className="primary-button wide" type="submit">Sign in</button>
      </form>
    </main>;
  }

  return <div className="app-shell">
    <header className="topbar">
      <div className="brand"><span className="brand-mark small">PR</span><div><strong>Plate Review</strong><span>Manual clip selection</span></div></div>
      <div className="topbar-right"><span className={`network-indicator ${isOnline ? "online" : "offline"}`}>{isOnline ? "Connected" : "Offline"}</span><span className="user-label">{user.username}</span><button className="quiet-button" onClick={() => setHelpOpen(true)}>Shortcuts <kbd>?</kbd></button><button className="quiet-button" onClick={() => void logout()}>Sign out</button></div>
    </header>
    {error && !activeVideo && <div className="global-notice" role="status"><span>{error}</span><button aria-label="Dismiss message" onClick={() => setError("")}>×</button></div>}
    <main className={`workspace ${activeId ? "has-editor" : ""}`}>
      <aside className="library-panel">
        <div className="panel-heading"><div><p className="eyebrow">VIDEO LIBRARY</p><h2>Recordings</h2></div><div className="panel-heading-actions">{user.role === "admin" && <button className="icon-button" title="Refresh source index" onClick={() => void api("/api/admin/scan", json("POST", {})).then(refreshVideos).catch((reason) => setError(reason.message))}>↻</button>}<span className="count-pill">{videos.length}</span></div></div>
        <div className="filters">
          <input aria-label="Search videos" placeholder="Search camera or file" value={search} onChange={(event) => setSearch(event.target.value)} />
          <select aria-label="Review status" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value)}><option value="unreviewed">Unreviewed</option><option value="in_progress">In progress</option><option value="reviewed">Reviewed</option><option value="all">All videos</option></select>
          <select aria-label="Camera" value={cameraFilter} onChange={(event) => setCameraFilter(event.target.value)}><option value="all">All cameras</option>{cameras.map(([id,name]) => <option value={id} key={id}>{id} · {name}</option>)}</select>
        </div>
        <div className="library-list" aria-label="Videos">
          {videos.map((video) => <article className={`video-row ${video.id === activeId ? "active" : ""}`} key={video.id}>
            <button className="video-row-main" onClick={() => void openVideo(video)}>
              <span className="row-topline"><strong>{video.camera_id} · {video.camera_name.replaceAll("_", " ")}</strong><span className={`status-dot ${video.review.status}`} /></span>
              <span className="row-meta">{dateLabel(video.session_started_at)} · {fmtTime(video.duration_ms)}</span>
              <span className="row-bottom"><span>{video.session_id}</span><span>{video.review.intervals.length} clips</span></span>
              {video.interruption_count > 0 && <span className="row-warning">{video.interruption_count} interruption{video.interruption_count === 1 ? "" : "s"}</span>}
              {video.source_status !== "available" && <span className="row-warning">Source {video.source_status}</span>}
              {video.claim && !video.claim.mine && video.claim.expires_at_ms > now && <span className="row-claim">Editing · {video.claim.username}</span>}
            </button>
            {video.source_status === "available" && video.review.status === "reviewed" && video.review.intervals.length > 0 && <input className="export-check" type="checkbox" aria-label={`Select ${video.source_path} for export`} checked={selectedForExport.has(video.id)} onChange={() => toggleExportSelection(video.id)} />}
          </article>)}
          {videos.length === 0 && <div className="empty-state"><span className="empty-icon">⌕</span><strong>No videos match</strong><p>Change a filter or refresh the source index.</p>{user.role === "admin" && <button className="quiet-button" onClick={() => void api("/api/admin/scan", json("POST", {})).then(refreshVideos).catch((e) => setError(e.message))}>Refresh library</button>}</div>}
        </div>
        <div className="library-footer"><span>{videos.filter((video) => video.source_status === "available").length} available</span><span>{videos.filter((video) => video.review.status === "reviewed").length} reviewed</span></div>
      </aside>

      <section className="review-area">
        {activeVideo && detail && draft ? <>
          <div className="editor-heading">
            <div className="editor-title"><button className="back-button" aria-label="Close video" onClick={() => void releaseAndClose()}>←</button><div><h1>{activeVideo.camera_name.replaceAll("_", " ")}</h1><p>{activeVideo.session_id} <span>·</span> {activeVideo.source_path.split("/").at(-1)} <span>·</span> {activeVideo.width} × {activeVideo.height}</p></div></div>
            <div className="editor-actions"><span className={`save-state ${syncState}`}><i />{syncState === "saved" ? "Saved on server" : syncState === "saving" ? "Saving" : syncState === "offline" ? "Offline · changes pending" : syncState === "local_error" ? "Local save failed" : "Conflict requires attention"}</span><button className="primary-button" disabled={!claimHeld} onClick={markReviewed}>Mark reviewed <kbd>R</kbd></button></div>
          </div>
          {(error || readOnlyReason || isConflict) && <div className={`notice ${isConflict ? "notice-warning" : "notice-error"}`} role="status"><span>{error || readOnlyReason || "Resolve the local draft before editing."}</span>{isConflict && <button className="small-button" onClick={() => void applyDraft()}>Claim and apply local draft</button>}{activeVideo.source_status === "changed" && user.role === "admin" && <button className="small-button" onClick={() => void api(`/api/admin/videos/${activeVideo.id}/accept-source`, json("POST", {})).then(async () => { await refreshVideos(); const refreshed = await api<VideoDetail>(`/api/videos/${activeVideo.id}`); await openVideo(refreshed.video); }).catch((reason) => setError(reason.message))}>Accept changed source</button>}</div>}
          {!claimHeld && activeVideo.source_status === "available" && !isConflict && <div className="readonly-banner">Read only while another reviewer holds this video. Your personal playback position still saves.</div>}
          <div className="review-content">
            <div className="viewer-column">
              <div className="viewer-toolbar"><div className="viewer-mode"><span className="live-dot" />SOURCE VIDEO <span className="toolbar-separator">/</span>{speed}×</div><div className="zoom-controls"><button title="Zoom out" onClick={() => changeZoom(zoom / 1.25)}>−</button><span>{Math.round(zoom * 100)}%</span><button title="Zoom in" onClick={() => changeZoom(zoom * 1.25)}>+</button><button title="View at source pixels" onClick={nativeZoom}>1:1</button><button title="Reset zoom" onClick={() => changeZoom(1)}>Reset</button><button title="Full screen" onClick={toggleFullscreen}>⛶</button></div></div>
              <div className={`video-stage ${zoom > 1 ? "zoomed" : ""}`} ref={stageRef} onWheel={handleStageWheel} onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={onPointerUp} onPointerCancel={onPointerUp}>
                {activeVideo.source_status === "available" ? <video ref={videoRef} src={`/api/videos/${activeVideo.id}/stream`} preload="metadata" playsInline onTimeUpdate={onTimeUpdate} onPlay={() => setPlaying(true)} onPause={() => setPlaying(false)} onError={() => setError("Video could not be loaded. Check its source file and network connection.")} style={{ transform: `translate(${pan.x}px, ${pan.y}px) scale(${zoom})` }} /> : <div className="stage-unavailable">This source is {activeVideo.source_status}.</div>}
                {pendingStart != null && <div className="marker-chip pending">IN MARK · {fmtTime(pendingStart)}</div>}
              </div>
              <div className="timeline-block">
                <div className="timeline-labels"><span>{fmtTime(currentTimeMs)}</span><span>{fmtTime(durationMs)}</span></div>
                <div className="timeline" role="slider" aria-label="Video timeline" aria-valuemin={0} aria-valuemax={durationMs} aria-valuenow={currentTimeMs} tabIndex={0} onClick={onTimelineClick} onKeyDown={(event: ReactKeyboardEvent<HTMLDivElement>) => { if (event.key === "ArrowLeft") seek(-1000); if (event.key === "ArrowRight") seek(1000); }}>
                  {draft.intervals.map((interval) => <button key={interval.id} className="timeline-interval" title={`${fmtTime(interval.start_ms)}–${fmtTime(interval.end_ms)}`} style={{ left: `${interval.start_ms / durationMs * 100}%`, width: `${Math.max(0.2, (interval.end_ms-interval.start_ms) / durationMs * 100)}%` }} onClick={(event) => { event.stopPropagation(); if (videoRef.current) videoRef.current.currentTime = interval.start_ms / 1000; }} />)}
                  {pendingStart != null && <span className="timeline-pending" style={{ left: `${pendingStart / durationMs * 100}%` }} />}
                  <span className="timeline-playhead" style={{ left: `${currentTimeMs / durationMs * 100}%` }} />
                </div>
                <div className="timeline-marks"><span>0:00</span><span>{draft.intervals.length} marked interval{draft.intervals.length === 1 ? "" : "s"}</span><span>{fmtTime(durationMs)}</span></div>
              </div>
              <div className="transport-controls"><div className="transport-left"><button aria-label="Back one second" onClick={() => seek(-1000)}>↶ <small>1s</small></button><button className="play-button" onClick={togglePlay} aria-label={playing ? "Pause" : "Play"}>{playing ? "Ⅱ" : "▶"}</button><button aria-label="Forward one second" onClick={() => seek(1000)}>↷ <small>1s</small></button><button className="speed-button" onClick={() => { const values=[0.25,0.5,1,2,4]; const next=values[(values.indexOf(speed)+1)%values.length]; setSpeed(next); if(videoRef.current) videoRef.current.playbackRate=next; }}>{speed}× <small>Speed</small></button></div><div className="transport-right"><button className="mark-button" disabled={!claimHeld} onClick={markIn}><span>IN</span> Mark start <kbd>I</kbd></button><button className="mark-button accent" disabled={!claimHeld || pendingStart == null} onClick={markOut}><span>OUT</span> Mark end <kbd>O</kbd></button><button className="next-button" onClick={() => void jumpNext()}>Next video <kbd>N</kbd></button></div></div>
            </div>
            <aside className="interval-panel"><div className="interval-heading"><div><p className="eyebrow">SELECTED RANGES</p><h2>Intervals <span>{draft.intervals.length}</span></h2></div><button className="icon-button" title="Undo" disabled={!claimHeld || undoStack.current.length === 0} onClick={undo}>↶</button></div>
              <p className="interval-help">Mark only the moments where plates are visible. The export adds 0.5 seconds around each range.</p>
              <div className="interval-list">{draft.intervals.map((interval, index) => <div className="interval-card" key={interval.id}><button className="interval-open" title="Play this interval" onClick={() => replayInterval(interval)}><span className="interval-index">{String(index+1).padStart(2,"0")}</span><span className="interval-times">{fmtTime(interval.start_ms)} <i>→</i> {fmtTime(interval.end_ms)}<small>{fmtTime(interval.end_ms-interval.start_ms)} selected</small></span></button><div className="interval-tools"><button title="Set start to playhead" disabled={!claimHeld} onClick={() => adjustInterval(interval.id,"start_ms")}>I</button><button title="Set end to playhead" disabled={!claimHeld} onClick={() => adjustInterval(interval.id,"end_ms")}>O</button><button className="remove-interval" aria-label={`Delete interval ${index+1}`} disabled={!claimHeld} onClick={() => removeInterval(interval.id)}>×</button></div></div>)}
                {draft.intervals.length === 0 && <div className="interval-empty"><span>⌁</span><strong>No intervals yet</strong><p>Pause at the first visible plate and press <kbd>I</kbd>. Press <kbd>O</kbd> after the plate leaves.</p></div>}
              </div>
              {draft.intervals.length > 0 && <div className="interval-summary"><span>Selected time</span><strong>{fmtTime(draft.intervals.reduce((sum,interval) => sum + interval.end_ms-interval.start_ms,0))}</strong></div>}
              <div className="review-status"><span className={`status-dot ${draft.status}`} /><div><strong>{draft.status.replace("_"," ")}</strong><span>{draft.status === "reviewed" ? `Last saved ${draftRef.current?.updatedAtMs ? new Date(draftRef.current.updatedAtMs).toLocaleTimeString() : ""}` : "Mark reviewed when this file is checked"}</span></div></div>
            </aside>
          </div>
        </> : <div className="welcome-view"><div className="welcome-art"><div className="art-road"><span /><span /><span /></div><div className="art-car"><div className="car-window" /><div className="car-plate">PLATE</div><div className="wheel left" /><div className="wheel right" /></div><div className="scan-frame"><i /><i /><i /><i /></div></div><p className="eyebrow">PLATE REVIEW WORKSPACE</p><h1>Review at your own pace.</h1><p className="welcome-copy">Choose a recording, find the moments with visible plates, and mark the intervals to prepare for segmentation.</p><div className="welcome-steps"><span><i>01</i>Open a video</span><span><i>02</i>Mark with I and O</span><span><i>03</i>Export for SAM</span></div><div className="welcome-note">Your review progress saves as you work.</div></div>}
      </section>
    </main>

    <footer className="bottom-bar"><div><span className="disk-dot" />Original videos remain on the server</div><div>{selectedForExport.size > 0 && <button className="export-button" onClick={() => void exportSelected()}>Export {selectedForExport.size} reviewed video{selectedForExport.size === 1 ? "" : "s"} <span>↗</span></button>}<span className="footer-status">{activeJobCount ? `${activeJobCount} export job(s) running` : "Export queue idle"}</span></div></footer>

    {jobs.length > 0 && <section className="jobs-dock" aria-label="Export jobs"><button className="jobs-header" aria-expanded={jobsExpanded} onClick={() => setJobsExpanded((value) => !value)}><strong>Export jobs</strong><span>{activeJobCount} active · {jobs.length} total</span><i>{jobsExpanded ? "−" : "+"}</i></button>{jobsExpanded && jobs.map((job) => <div className="job-row" key={job.id}><div className="job-line"><span>{job.status === "complete" ? "Export complete" : job.phase}</span><b>{job.status === "complete" ? "DONE" : `${Math.round(job.progress*100)}%`}</b></div><div className="job-progress"><i style={{ width: `${job.progress*100}%` }} /></div>{job.error && <span className="job-error">{job.error}</span>}{job.items.filter((item) => item.error).map((item) => <span className="job-error" key={item.id}>{item.video_id.slice(0,8)}: {item.error}</span>)}{job.items.flatMap((item) => item.outputs).map((output) => <a className="output-link" key={`${job.id}-${output.file_name}`} href={`/api/exports/${job.id}/files/${encodeURIComponent(output.file_name)}`} download={output.file_name}>{output.file_name}<span>{humanBytes(output.bytes)}</span></a>)}{(job.status === "complete" || job.status === "failed") && <a className="manifest-link" href={`/api/exports/${job.id}/manifest`} download={`${job.id}-manifest.json`}>Download manifest</a>}{job.status === "failed" && <button className="small-button" onClick={() => void api<ExportJob>(`/api/exports/${job.id}/retry`, json("POST", {})).then((updated) => setJobs((current) => current.map((item) => item.id === updated.id ? updated : item))).catch((reason) => setError(reason.message))}>Retry failed files</button>}</div>)}</section>}

    {helpOpen && <div className="modal-backdrop" onMouseDown={(event) => { if(event.target===event.currentTarget) setHelpOpen(false); }}><section className="shortcut-modal" role="dialog" aria-modal="true" aria-labelledby="shortcut-title"><button className="modal-close" aria-label="Close" onClick={() => setHelpOpen(false)}>×</button><p className="eyebrow">KEYBOARD WORKFLOW</p><h2 id="shortcut-title">Review without reaching for the mouse.</h2><div className="shortcut-list"><span>Play / pause</span><kbd>Space</kbd><span>Mark interval start / end</span><kbd>I / O</kbd><span>Step 100 ms</span><kbd>← / →</kbd><span>Step 1 s / 10 s</span><kbd>Shift / Alt + arrows</kbd><span>Change speed</span><kbd>J / L</kbd><span>Zoom / reset</span><kbd>+ / − / 0</kbd><span>Undo / redo</span><kbd>Ctrl + Z / Shift</kbd><span>Mark reviewed / next</span><kbd>R / N</kbd></div><p className="muted">Shortcuts pause when a text field is active. Scroll over the video to zoom around the pointer.</p></section></div>}
  </div>;
}
