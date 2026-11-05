use crate::{
    api::{
        ApiError, AppState, ClaimBody, ClaimView, Interval, ReviewSnapshot, ReviewStatus,
        SavePositionBody, SaveReviewBody, UserView, Video, VideoFilters,
    },
    catalog,
    db::now_ms,
};
use axum::{
    Json,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
};
use serde::Serialize;
use std::collections::HashSet;
use uuid::Uuid;

const CLAIM_MS: i64 = 120_000;
const MAX_INTERVALS: usize = 500;

#[derive(sqlx::FromRow)]
struct VideoRow {
    id: String,
    source_path: String,
    camera_id: String,
    camera_name: String,
    session_id: String,
    session_started_at: String,
    session_kind: String,
    source_status: String,
    duration_ms: i64,
    width: i64,
    height: i64,
    codec: String,
    frame_rate: String,
    interruption_count: i64,
    revision: Option<i64>,
    review_status: Option<String>,
    intervals_json: Option<String>,
    updated_by: Option<String>,
    updated_at_ms: Option<i64>,
    claimant_id: Option<String>,
    claimant_name: Option<String>,
    claim_expires: Option<i64>,
}

#[derive(Serialize)]
pub struct VideoDetail {
    pub video: Video,
    pub position_ms: i64,
    pub zoom: f64,
}

#[derive(Serialize)]
pub struct ScanResponse {
    pub files_checked: usize,
    pub available: usize,
    pub changed: usize,
    pub missing: usize,
}

pub async fn list_videos(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Query(filters): Query<VideoFilters>,
) -> Result<Json<Vec<Video>>, ApiError> {
    let rows = sqlx::query_as::<_, VideoRow>("SELECT v.*, r.revision, r.status AS review_status, r.intervals_json, updated.username AS updated_by, r.updated_at_ms, c.user_id AS claimant_id, owner.username AS claimant_name, c.expires_at_ms AS claim_expires FROM videos v LEFT JOIN reviews r ON r.video_id=v.id LEFT JOIN users updated ON updated.id=r.updated_by LEFT JOIN edit_claims c ON c.video_id=v.id AND c.expires_at_ms>? LEFT JOIN users owner ON owner.id=c.user_id ORDER BY v.session_started_at DESC, v.camera_id, v.session_id, v.sort_key")
        .bind(now_ms()).fetch_all(&state.pool).await.map_err(ApiError::internal)?;
    let videos = rows
        .into_iter()
        .filter_map(|row| to_video(row, &user.id).ok())
        .filter(|video| {
            filters
                .status
                .as_deref()
                .is_none_or(|status| status == "all" || video.review.status.as_str() == status)
        })
        .filter(|video| {
            filters
                .camera
                .as_deref()
                .is_none_or(|camera| camera == "all" || video.camera_id == camera)
        })
        .filter(|video| {
            filters.q.as_deref().is_none_or(|term| {
                let term = term.to_lowercase();
                video.camera_name.to_lowercase().contains(&term)
                    || video.camera_id.contains(&term)
                    || video.session_id.to_lowercase().contains(&term)
                    || video.source_path.to_lowercase().contains(&term)
            })
        })
        .collect();
    Ok(Json(videos))
}

pub async fn get_video(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<VideoDetail>, ApiError> {
    let video = fetch_video(&state, &id, &user.id).await?;
    let (position_ms, zoom) = sqlx::query_as::<_, (i64, f64)>(
        "SELECT position_ms, zoom FROM playback_positions WHERE user_id=? AND video_id=?",
    )
    .bind(&user.id)
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?
    .unwrap_or((0, 1.0));
    Ok(Json(VideoDetail {
        video,
        position_ms,
        zoom,
    }))
}

pub async fn claim_video(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<Json<Video>, ApiError> {
    validate_uuid(&body.client_id, "client_id")?;
    let source: Option<String> = sqlx::query_scalar("SELECT source_status FROM videos WHERE id=?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    ensure_available(source)?;
    let now = now_ms();
    let expires = now + CLAIM_MS;
    let result = sqlx::query("INSERT INTO edit_claims (video_id,user_id,client_id,expires_at_ms) VALUES (?,?,?,?) ON CONFLICT(video_id) DO UPDATE SET user_id=excluded.user_id, client_id=excluded.client_id, expires_at_ms=excluded.expires_at_ms WHERE edit_claims.expires_at_ms<=? OR (edit_claims.user_id=excluded.user_id AND edit_claims.client_id=excluded.client_id)")
        .bind(&id).bind(&user.id).bind(&body.client_id).bind(expires).bind(now).execute(&state.pool).await.map_err(ApiError::internal)?;
    if result.rows_affected() == 0 {
        let owner: Option<String> = sqlx::query_scalar("SELECT u.username FROM edit_claims c JOIN users u ON u.id=c.user_id WHERE c.video_id=? AND c.expires_at_ms>?")
            .bind(&id).bind(now).fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
        return Err(ApiError::conflict(format!(
            "This video is being edited by {}",
            owner.unwrap_or_else(|| "another reviewer".into())
        )));
    }
    Ok(Json(fetch_video(&state, &id, &user.id).await?))
}

pub async fn heartbeat(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<axum::http::StatusCode, ApiError> {
    validate_uuid(&body.client_id, "client_id")?;
    let result = sqlx::query("UPDATE edit_claims SET expires_at_ms=? WHERE video_id=? AND user_id=? AND client_id=? AND expires_at_ms>?")
        .bind(now_ms() + CLAIM_MS).bind(id).bind(user.id).bind(body.client_id).bind(now_ms()).execute(&state.pool).await.map_err(ApiError::internal)?;
    if result.rows_affected() == 0 {
        return Err(ApiError::conflict(
            "The edit claim expired. Save a local copy and claim the video again.",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn release_claim(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
    Json(body): Json<ClaimBody>,
) -> Result<axum::http::StatusCode, ApiError> {
    validate_uuid(&body.client_id, "client_id")?;
    sqlx::query("DELETE FROM edit_claims WHERE video_id=? AND user_id=? AND client_id=?")
        .bind(id)
        .bind(user.id)
        .bind(body.client_id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn save_review(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
    Json(body): Json<SaveReviewBody>,
) -> Result<Json<ReviewSnapshot>, ApiError> {
    validate_uuid(&body.client_id, "client_id")?;
    validate_uuid(&body.mutation_id, "mutation_id")?;
    let requested_status = ReviewStatus::try_from(body.status.as_str())?;
    let mut tx = state.pool.begin().await.map_err(ApiError::internal)?;
    if let Some((mutation_video, mutation_user, response)) =
        sqlx::query_as::<_, (String, String, String)>(
            "SELECT video_id,user_id,response_json FROM review_mutations WHERE mutation_id=?",
        )
        .bind(&body.mutation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(ApiError::internal)?
    {
        if mutation_user != user.id || mutation_video != id {
            return Err(ApiError::conflict(
                "This edit request ID belongs to another request",
            ));
        }
        let result = serde_json::from_str(&response).map_err(ApiError::internal)?;
        tx.rollback().await.map_err(ApiError::internal)?;
        return Ok(Json(result));
    }
    let source = sqlx::query_as::<_, (String, i64)>(
        "SELECT source_status,duration_ms FROM videos WHERE id=?",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::internal)?;
    let Some((source_status, duration_ms)) = source else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Video not found"));
    };
    if source_status != "available" {
        return Err(ApiError::new(
            StatusCode::GONE,
            format!("Video source is {source_status}"),
        ));
    }
    let now = now_ms();
    let claim = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM edit_claims WHERE video_id=? AND user_id=? AND client_id=? AND expires_at_ms>?")
        .bind(&id).bind(&user.id).bind(&body.client_id).bind(now).fetch_one(&mut *tx).await.map_err(ApiError::internal)?;
    if claim == 0 {
        return Err(ApiError::conflict(
            "This edit claim expired. Save a local copy and claim the video again.",
        ));
    }
    validate_intervals(&body.intervals, duration_ms)?;
    let previous = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT revision,status,intervals_json FROM reviews WHERE video_id=?",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::internal)?;
    let (revision, previous_status, previous_json) =
        previous.unwrap_or((0, "unreviewed".into(), "[]".into()));
    if revision != body.expected_revision {
        return Err(ApiError::conflict(
            "Review changed since this editor loaded it. Reload or resolve the saved draft.",
        ));
    }
    let intervals_json = serde_json::to_string(&body.intervals).map_err(ApiError::internal)?;
    let status = if previous_status == "reviewed"
        && intervals_json != previous_json
        && requested_status == ReviewStatus::Reviewed
    {
        ReviewStatus::InProgress
    } else {
        requested_status
    };
    let result = ReviewSnapshot {
        revision: revision + 1,
        status: status.clone(),
        intervals: body.intervals,
        updated_by: Some(user.username.clone()),
        updated_at_ms: now,
    };
    let response_json = serde_json::to_string(&result).map_err(ApiError::internal)?;
    sqlx::query("INSERT INTO reviews (video_id,revision,status,intervals_json,updated_by,updated_at_ms) VALUES (?,?,?,?,?,?) ON CONFLICT(video_id) DO UPDATE SET revision=excluded.revision,status=excluded.status,intervals_json=excluded.intervals_json,updated_by=excluded.updated_by,updated_at_ms=excluded.updated_at_ms")
        .bind(&id).bind(result.revision).bind(status.as_str()).bind(intervals_json).bind(&user.id).bind(now).execute(&mut *tx).await.map_err(ApiError::internal)?;
    sqlx::query("INSERT INTO review_mutations (mutation_id,video_id,user_id,response_json,created_at_ms) VALUES (?,?,?,?,?)")
        .bind(&body.mutation_id).bind(&id).bind(&user.id).bind(response_json).bind(now).execute(&mut *tx).await.map_err(ApiError::internal)?;
    tx.commit().await.map_err(ApiError::internal)?;
    Ok(Json(result))
}

pub async fn save_position(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
    Json(body): Json<SavePositionBody>,
) -> Result<axum::http::StatusCode, ApiError> {
    if body.position_ms < 0 || !body.zoom.is_finite() || !(0.25..=8.0).contains(&body.zoom) {
        return Err(ApiError::bad("Playback position or zoom is out of range"));
    }
    let duration: Option<i64> = sqlx::query_scalar(
        "SELECT duration_ms FROM videos WHERE id=? AND source_status='available'",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let Some(duration) = duration else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "Available video not found",
        ));
    };
    let position = body.position_ms.min(duration);
    sqlx::query("INSERT INTO playback_positions (user_id,video_id,position_ms,zoom,updated_at_ms) VALUES (?,?,?,?,?) ON CONFLICT(user_id,video_id) DO UPDATE SET position_ms=excluded.position_ms,zoom=excluded.zoom,updated_at_ms=excluded.updated_at_ms")
        .bind(user.id).bind(id).bind(position).bind(body.zoom).bind(now_ms()).execute(&state.pool).await.map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn scan_library(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
) -> Result<Json<ScanResponse>, ApiError> {
    crate::auth::require_admin(&user)?;
    let summary = catalog::scan(&state.settings, &state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(ScanResponse {
        files_checked: summary.files_checked,
        available: summary.available,
        changed: summary.changed,
        missing: summary.missing,
    }))
}

pub async fn accept_changed_source(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    crate::auth::require_admin(&user)?;
    let relative: Option<String> =
        sqlx::query_scalar("SELECT source_path FROM videos WHERE id=? AND source_status='changed'")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await
            .map_err(ApiError::internal)?;
    let Some(relative) = relative else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "Changed video not found",
        ));
    };
    let root = state.settings.source_dir.clone();
    let path = tokio::task::spawn_blocking(move || {
        let path = root.join(relative).canonicalize()?;
        if !path.starts_with(&root) {
            anyhow::bail!("Source path is outside the source directory");
        }
        let metadata = std::fs::metadata(&path)?;
        if !metadata.is_file() || metadata.len() == 0 {
            anyhow::bail!("Accepted source must be a non-empty file");
        }
        Ok::<_, anyhow::Error>((path, metadata))
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    let mtime = path
        .1
        .modified()
        .map_err(ApiError::internal)?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    sqlx::query(
        "UPDATE videos SET source_status='available',source_size=?,source_mtime_ms=? WHERE id=?",
    )
    .bind(path.1.len() as i64)
    .bind(mtime)
    .bind(id)
    .execute(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn fetch_video(state: &AppState, id: &str, user_id: &str) -> Result<Video, ApiError> {
    let row = sqlx::query_as::<_, VideoRow>("SELECT v.*, r.revision, r.status AS review_status, r.intervals_json, updated.username AS updated_by, r.updated_at_ms, c.user_id AS claimant_id, owner.username AS claimant_name, c.expires_at_ms AS claim_expires FROM videos v LEFT JOIN reviews r ON r.video_id=v.id LEFT JOIN users updated ON updated.id=r.updated_by LEFT JOIN edit_claims c ON c.video_id=v.id AND c.expires_at_ms>? LEFT JOIN users owner ON owner.id=c.user_id WHERE v.id=?")
        .bind(now_ms()).bind(id).fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
    let Some(row) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Video not found"));
    };
    to_video(row, user_id)
}

fn to_video(row: VideoRow, user_id: &str) -> Result<Video, ApiError> {
    let intervals = serde_json::from_str(row.intervals_json.as_deref().unwrap_or("[]"))
        .map_err(ApiError::internal)?;
    let review = ReviewSnapshot {
        revision: row.revision.unwrap_or(0),
        status: ReviewStatus::try_from(row.review_status.as_deref().unwrap_or("unreviewed"))?,
        intervals,
        updated_by: row.updated_by,
        updated_at_ms: row.updated_at_ms.unwrap_or(0),
    };
    let claim =
        row.claim_expires
            .zip(row.claimant_name.clone())
            .map(|(expires_at_ms, username)| ClaimView {
                mine: row.claimant_id.as_deref() == Some(user_id),
                username,
                expires_at_ms,
            });
    Ok(Video {
        id: row.id,
        source_path: row.source_path,
        camera_id: row.camera_id,
        camera_name: row.camera_name,
        session_id: row.session_id,
        session_started_at: row.session_started_at,
        session_kind: row.session_kind,
        source_status: row.source_status,
        duration_ms: row.duration_ms,
        width: row.width,
        height: row.height,
        codec: row.codec,
        frame_rate: row.frame_rate,
        interruption_count: row.interruption_count,
        review,
        claim,
    })
}

fn validate_intervals(intervals: &[Interval], duration_ms: i64) -> Result<(), ApiError> {
    if intervals.len() > MAX_INTERVALS {
        return Err(ApiError::bad("A video can have at most 500 intervals"));
    }
    let mut ids = HashSet::new();
    for interval in intervals {
        validate_uuid(&interval.id, "interval_id")?;
        if !ids.insert(&interval.id) {
            return Err(ApiError::bad("Interval IDs must be unique"));
        }
        if interval.start_ms < 0
            || interval.end_ms <= interval.start_ms
            || interval.end_ms > duration_ms
        {
            return Err(ApiError::bad(
                "Each interval must have an end after its start and remain inside the video",
            ));
        }
    }
    Ok(())
}

fn validate_uuid(value: &str, name: &str) -> Result<(), ApiError> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|_| ApiError::bad(format!("{name} must be a UUID")))
}

fn ensure_available(status: Option<String>) -> Result<(), ApiError> {
    match status.as_deref() {
        Some("available") => Ok(()),
        Some("changed") => Err(ApiError::new(
            StatusCode::GONE,
            "An administrator must accept the changed source before editing",
        )),
        Some("missing") => Err(ApiError::new(
            StatusCode::GONE,
            "The source file is missing",
        )),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "Video not found")),
        Some(_) => Err(ApiError::new(StatusCode::GONE, "Video is unavailable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_intervals_outside_source_duration() {
        let interval = Interval {
            id: Uuid::new_v4().to_string(),
            start_ms: 400,
            end_ms: 10_001,
        };
        assert!(validate_intervals(&[interval], 10_000).is_err());
    }

    #[test]
    fn rejects_duplicate_interval_ids() {
        let id = Uuid::new_v4().to_string();
        let intervals = [
            Interval {
                id: id.clone(),
                start_ms: 0,
                end_ms: 100,
            },
            Interval {
                id,
                start_ms: 200,
                end_ms: 300,
            },
        ];
        assert!(validate_intervals(&intervals, 1000).is_err());
    }
}
