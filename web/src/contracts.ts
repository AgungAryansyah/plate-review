export type ReviewStatus = "unreviewed" | "in_progress" | "reviewed";

export interface Interval {
  id: string;
  start_ms: number;
  end_ms: number;
}

export interface ReviewSnapshot {
  revision: number;
  status: ReviewStatus;
  intervals: Interval[];
  updated_by: string | null;
  updated_at_ms: number;
}

export interface Claim {
  username: string;
  expires_at_ms: number;
  mine: boolean;
}

export interface Video {
  id: string;
  source_path: string;
  camera_id: string;
  camera_name: string;
  session_id: string;
  session_started_at: string;
  session_kind: string;
  source_status: "available" | "changed" | "missing";
  duration_ms: number;
  width: number;
  height: number;
  codec: string;
  frame_rate: string;
  interruption_count: number;
  review: ReviewSnapshot;
  claim: Claim | null;
}

export interface VideoDetail {
  video: Video;
  position_ms: number;
  zoom: number;
}

export interface User {
  id: string;
  username: string;
  role: "admin" | "reviewer";
}

export interface ExportOutput {
  file_name: string;
  interval_ids: string[];
  requested_start_ms: number;
  requested_end_ms: number;
  padded_start_ms: number;
  padded_end_ms: number;
  actual_start_ms: number;
  actual_end_ms: number;
  duration_ms: number;
  bytes: number;
}

export interface ExportJob {
  id: string;
  status: "queued" | "running" | "complete" | "failed";
  phase: string;
  total_items: number;
  completed_items: number;
  failed_items: number;
  progress: number;
  error: string | null;
  created_at_ms: number;
  started_at_ms: number | null;
  ended_at_ms: number | null;
  items: {
    id: string;
    video_id: string;
    status: string;
    progress: number;
    error: string | null;
    outputs: ExportOutput[];
  }[];
}

export interface ReviewDraft {
  key: string;
  userId: string;
  videoId: string;
  clientId: string;
  intervals: Interval[];
  pendingStartMs: number | null;
  status: ReviewStatus;
  baseRevision: number;
  pendingMutationId: string | null;
  positionMs: number;
  zoom: number;
  updatedAtMs: number;
}
