export type BoxClipStatus = "unsegmented" | "queued" | "running" | "complete" | "failed";

export interface BoxClip {
  id: string;
  source_video_id: string;
  export_job_id: string;
  name: string;
  bytes: number;
  duration_ms: number;
  status: BoxClipStatus;
  progress: number;
  error: string | null;
}

export interface Detection {
  frame: number;
  timestamp_ms: number;
  x_min: number;
  y_min: number;
  x_max: number;
  y_max: number;
  confidence: number;
}

export interface BoxTrack {
  id: string;
  selected: boolean;
  plate_text: string;
  detections: Detection[];
}

export interface BoxSettings {
  min_confidence: number;
  jitter_px: number;
  smoothing_radius: number;
  minimum_frames: number;
  max_area_fraction: number;
  max_size_ratio: number;
}

export interface BoxJob {
  status: BoxClipStatus;
  progress: number;
  phase: string;
  logs: string[];
  error: string | null;
}

export interface BoxClipDetail {
  clip: BoxClip;
  meta: { width: number; height: number; fps: number; frame_count: number };
  tracks: BoxTrack[];
  settings: BoxSettings;
  revision: number;
  job: BoxJob | null;
}
