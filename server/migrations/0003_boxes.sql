CREATE TABLE box_clips (
    id TEXT PRIMARY KEY NOT NULL,
    source_video_id TEXT NOT NULL,
    export_job_id TEXT NOT NULL,
    name TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    bytes INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    source_hash TEXT,
    meta_json TEXT,
    tracks_json TEXT NOT NULL DEFAULT '[]',
    review_json TEXT NOT NULL DEFAULT '[]',
    settings_json TEXT NOT NULL DEFAULT '{}',
    revision INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE box_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    clip_id TEXT NOT NULL REFERENCES box_clips(id),
    requested_by TEXT NOT NULL REFERENCES users(id),
    status TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    phase TEXT NOT NULL DEFAULT 'queued',
    error TEXT,
    logs_json TEXT NOT NULL DEFAULT '[]',
    source_hash TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER
);
CREATE UNIQUE INDEX box_job_active ON box_jobs(clip_id) WHERE status IN ('queued', 'running');
CREATE INDEX box_jobs_recent ON box_jobs(clip_id, created_at_ms DESC);
