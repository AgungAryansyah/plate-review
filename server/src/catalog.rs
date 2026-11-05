use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;
use std::{
    fs,
    path::{Component, Path},
    time::UNIX_EPOCH,
};
use tracing::info;
use uuid::Uuid;

use crate::config::Settings;

#[derive(Clone, Debug)]
pub struct VideoSource {
    pub id: String,
    pub source_path: String,
    pub source_size: i64,
    pub source_mtime_ms: i64,
    pub camera_id: String,
    pub camera_name: String,
    pub session_id: String,
    pub session_started_at: String,
    pub session_kind: String,
    pub duration_ms: i64,
    pub width: i64,
    pub height: i64,
    pub codec: String,
    pub frame_rate: String,
    pub interruption_count: i64,
    pub sort_key: String,
}

#[derive(Clone, Debug)]
pub struct ScanSummary {
    pub files_checked: usize,
    pub available: usize,
    pub changed: usize,
    pub missing: usize,
}

pub async fn scan(settings: &Settings, pool: &SqlitePool) -> Result<ScanSummary> {
    let root = settings.source_dir.clone();
    let rows = tokio::task::spawn_blocking(move || discover(&root)).await??;
    info!(
        phase = "catalog",
        completed = rows.len(),
        total = rows.len(),
        "Source file scan complete"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE videos SET source_status = 'missing'")
        .execute(&mut *tx)
        .await?;
    let mut summary = ScanSummary {
        files_checked: rows.len(),
        available: 0,
        changed: 0,
        missing: 0,
    };
    for row in &rows {
        let existing = sqlx::query_as::<_, (i64, i64, String)>(
            "SELECT source_size, source_mtime_ms, source_status FROM videos WHERE source_path = ?",
        )
        .bind(&row.source_path)
        .fetch_optional(&mut *tx)
        .await?;
        let status = match existing {
            Some((_, _, previous)) if previous == "changed" => "changed",
            Some((size, mtime, _)) if size != row.source_size || mtime != row.source_mtime_ms => {
                "changed"
            }
            _ => "available",
        };
        sqlx::query("INSERT INTO videos (id, source_path, source_size, source_mtime_ms, camera_id, camera_name, session_id, session_started_at, session_kind, source_status, duration_ms, width, height, codec, frame_rate, interruption_count, sort_key) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(source_path) DO UPDATE SET source_size=excluded.source_size, source_mtime_ms=excluded.source_mtime_ms, camera_id=excluded.camera_id, camera_name=excluded.camera_name, session_id=excluded.session_id, session_started_at=excluded.session_started_at, session_kind=excluded.session_kind, source_status=excluded.source_status, duration_ms=excluded.duration_ms, width=excluded.width, height=excluded.height, codec=excluded.codec, frame_rate=excluded.frame_rate, interruption_count=excluded.interruption_count, sort_key=excluded.sort_key")
            .bind(&row.id).bind(&row.source_path).bind(row.source_size).bind(row.source_mtime_ms)
            .bind(&row.camera_id).bind(&row.camera_name).bind(&row.session_id).bind(&row.session_started_at)
            .bind(&row.session_kind).bind(status).bind(row.duration_ms).bind(row.width).bind(row.height)
            .bind(&row.codec).bind(&row.frame_rate).bind(row.interruption_count).bind(&row.sort_key)
            .execute(&mut *tx).await?;
        match status {
            "available" => summary.available += 1,
            "changed" => summary.changed += 1,
            _ => {}
        }
    }
    summary.missing =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM videos WHERE source_status = 'missing'")
            .fetch_one(&mut *tx)
            .await? as usize;
    tx.commit().await?;
    Ok(summary)
}

fn discover(root: &Path) -> Result<Vec<VideoSource>> {
    let mut found = Vec::new();
    let sessions = fs::read_dir(root)
        .with_context(|| format!("Cannot read source directory {}", root.display()))?;
    let mut candidates = 0usize;
    for entry in sessions {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let session_dir = entry.path();
        let metadata_path = session_dir.join("metadata.json");
        if !metadata_path.is_file() {
            continue;
        }
        let metadata: Value = serde_json::from_slice(
            &fs::read(&metadata_path)
                .with_context(|| format!("Cannot read {}", metadata_path.display()))?,
        )?;
        let camera = &metadata["camera"];
        let camera_id = scalar_text(&camera["id"]).unwrap_or_default();
        let camera_name = scalar_text(&camera["name"]).unwrap_or_else(|| camera_id.clone());
        let started_at = metadata["started_at"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let kind = metadata["kind"].as_str().unwrap_or("recording").to_owned();
        let interruption_count = metadata["interruptions"]
            .as_array()
            .map_or(0, |rows| rows.len() as i64);
        let chunks = metadata["chunks"].as_array().cloned().unwrap_or_default();
        for chunk in chunks {
            candidates += 1;
            if candidates.is_multiple_of(50) {
                info!(
                    phase = "catalog",
                    completed = candidates,
                    "Indexing source chunks"
                );
            }
            let Some(name) = chunk["file"].as_str() else {
                continue;
            };
            let component = Path::new(name);
            if name.ends_with(".part.mp4")
                || component.components().count() != 1
                || component
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
                || component.extension().and_then(|ext| ext.to_str()) != Some("mp4")
            {
                continue;
            }
            let path = session_dir.join(component);
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let stat = fs::metadata(&path)?;
            if !stat.is_file() {
                continue;
            }
            let Some(duration) = chunk["duration_seconds"]
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
            else {
                continue;
            };
            let width = chunk["width"].as_i64().unwrap_or(0);
            let height = chunk["height"].as_i64().unwrap_or(0);
            let codec = chunk["codec"].as_str().unwrap_or("").to_owned();
            if width <= 0 || height <= 0 || codec.is_empty() || stat.len() == 0 {
                continue;
            }
            let mtime = stat
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(i64::MAX as u128) as i64;
            let source_path = relative.to_string_lossy().replace('\\', "/");
            let id = format!(
                "{}",
                Uuid::new_v5(&Uuid::NAMESPACE_URL, source_path.as_bytes())
            );
            let sort_key = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            found.push(VideoSource {
                id,
                source_path,
                source_size: stat.len().min(i64::MAX as u64) as i64,
                source_mtime_ms: mtime,
                camera_id: camera_id.clone(),
                camera_name: camera_name.clone(),
                session_id: entry.file_name().to_string_lossy().into_owned(),
                session_started_at: started_at.clone(),
                session_kind: kind.clone(),
                duration_ms: (duration * 1000.0).round() as i64,
                width,
                height,
                codec,
                frame_rate: chunk["frame_rate"].as_str().unwrap_or("unknown").to_owned(),
                interruption_count,
                sort_key,
            });
        }
    }
    found.sort_by(|a, b| {
        (
            &a.session_started_at,
            &a.camera_id,
            &a.session_id,
            &a.sort_key,
        )
            .cmp(&(
                &b.session_started_at,
                &b.camera_id,
                &b.session_id,
                &b.sort_key,
            ))
    });
    Ok(found)
}

fn scalar_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|n| n.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};

    fn fixture(root: &Path) {
        let session = root.join("recording_camera91_test");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("attempt_001_000000.mp4"), b"fixture-video").unwrap();
        fs::write(session.join("attempt_001_000001.part.mp4"), b"partial").unwrap();
        fs::write(session.join("attempt_001_000002.ts"), b"transport-stream").unwrap();
        fs::write(session.join("metadata.json"), r#"{"camera":{"id":"91","name":"Perwakilan"},"started_at":"2026-10-09T10:00:00+07:00","kind":"recording","interruptions":[],"chunks":[{"file":"attempt_001_000000.mp4","duration_seconds":300.2,"width":1920,"height":1080,"codec":"h264","frame_rate":"25/1"},{"file":"attempt_001_000001.part.mp4","duration_seconds":4,"width":1920,"height":1080,"codec":"h264"},{"file":"../escape.mp4","duration_seconds":3,"width":1920,"height":1080,"codec":"h264"}]}"#).unwrap();
    }

    #[test]
    fn discovers_only_verified_mp4_paths_from_metadata() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let items = discover(root.path()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].camera_id, "91");
        assert_eq!(items[0].duration_ms, 300_200);
        assert_eq!(
            items[0].source_path,
            "recording_camera91_test/attempt_001_000000.mp4"
        );
    }

    #[tokio::test]
    async fn scan_marks_missing_and_changed_sources() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path());
        let state = tempfile::tempdir().unwrap();
        let settings = Settings {
            source_dir: root.path().to_path_buf(),
            state_dir: state.path().to_path_buf(),
            export_dir: state.path().join("exports"),
            web_dir: state.path().join("web"),
            bind: "127.0.0.1:8080".parse().unwrap(),
            app_origin: "http://127.0.0.1:5173".into(),
            secure_cookie: false,
            trust_proxy_headers: false,
        };
        let pool: SqlitePool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let first = scan(&settings, &pool).await.unwrap();
        assert_eq!(first.available, 1);
        let file = root
            .path()
            .join("recording_camera91_test/attempt_001_000000.mp4");
        fs::write(&file, b"changed-size").unwrap();
        assert_eq!(scan(&settings, &pool).await.unwrap().changed, 1);
        fs::remove_file(file).unwrap();
        assert_eq!(scan(&settings, &pool).await.unwrap().missing, 1);
    }
}
