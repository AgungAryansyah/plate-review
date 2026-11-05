PRAGMA foreign_keys = ON;

CREATE TABLE users (
    id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'reviewer')),
    disabled INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0, 1)),
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at_ms);

CREATE TABLE videos (
    id TEXT PRIMARY KEY NOT NULL,
    source_path TEXT NOT NULL UNIQUE,
    source_size INTEGER NOT NULL,
    source_mtime_ms INTEGER NOT NULL,
    camera_id TEXT NOT NULL,
    camera_name TEXT NOT NULL,
    session_id TEXT NOT NULL,
    session_started_at TEXT NOT NULL,
    session_kind TEXT NOT NULL,
    source_status TEXT NOT NULL CHECK (source_status IN ('available', 'changed', 'missing')),
    duration_ms INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    codec TEXT NOT NULL,
    frame_rate TEXT NOT NULL,
    interruption_count INTEGER NOT NULL DEFAULT 0,
    sort_key TEXT NOT NULL
);
CREATE INDEX videos_sort ON videos(session_started_at DESC, camera_id, session_id, sort_key);

CREATE TABLE reviews (
    video_id TEXT PRIMARY KEY NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'unreviewed' CHECK (status IN ('unreviewed', 'in_progress', 'reviewed')),
    intervals_json TEXT NOT NULL DEFAULT '[]',
    updated_by TEXT REFERENCES users(id),
    updated_at_ms INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE edit_claims (
    video_id TEXT PRIMARY KEY NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_id TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL
);

CREATE TABLE review_mutations (
    mutation_id TEXT PRIMARY KEY NOT NULL,
    video_id TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    response_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE playback_positions (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL REFERENCES videos(id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    zoom REAL NOT NULL DEFAULT 1.0,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (user_id, video_id)
);

