CREATE TABLE export_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    requested_by TEXT NOT NULL REFERENCES users(id),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'complete', 'failed')),
    phase TEXT NOT NULL DEFAULT 'queued',
    total_items INTEGER NOT NULL,
    completed_items INTEGER NOT NULL DEFAULT 0,
    failed_items INTEGER NOT NULL DEFAULT 0,
    progress REAL NOT NULL DEFAULT 0,
    padding_ms INTEGER NOT NULL DEFAULT 500,
    error TEXT,
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    ended_at_ms INTEGER
);
CREATE INDEX export_jobs_recent ON export_jobs(created_at_ms DESC);

CREATE TABLE export_items (
    id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL REFERENCES export_jobs(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL REFERENCES videos(id),
    review_revision INTEGER NOT NULL,
    intervals_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'complete', 'failed')),
    progress REAL NOT NULL DEFAULT 0,
    outputs_json TEXT NOT NULL DEFAULT '[]',
    error TEXT
);
CREATE INDEX export_items_queue ON export_items(status, job_id);
