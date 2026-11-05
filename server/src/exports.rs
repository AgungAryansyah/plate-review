use crate::{
    api::{ApiError, AppState, Interval, UserView},
    db::now_ms,
    media,
};
use anyhow::Context;
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::{
    collections::HashSet,
    io::{BufRead, BufReader},
    path::Path as FsPath,
    process::{Command, Stdio},
    time::Instant,
};
use tokio::{
    io::AsyncBufReadExt,
    sync::mpsc,
    time::{Duration, sleep},
};
use tracing::{error, info, warn};
use uuid::Uuid;

const DEFAULT_PADDING_MS: i64 = 500;

#[derive(Deserialize)]
pub struct CreateJobBody {
    pub video_ids: Vec<String>,
    pub padding_ms: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExportOutput {
    pub file_name: String,
    pub interval_ids: Vec<String>,
    pub requested_start_ms: i64,
    pub requested_end_ms: i64,
    pub padded_start_ms: i64,
    pub padded_end_ms: i64,
    pub actual_start_ms: i64,
    pub actual_end_ms: i64,
    pub duration_ms: i64,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobView {
    pub id: String,
    pub status: String,
    pub phase: String,
    pub total_items: i64,
    pub completed_items: i64,
    pub failed_items: i64,
    pub progress: f64,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub ended_at_ms: Option<i64>,
    pub items: Vec<JobItemView>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JobItemView {
    pub id: String,
    pub video_id: String,
    pub status: String,
    pub progress: f64,
    pub error: Option<String>,
    pub outputs: Vec<ExportOutput>,
}

#[derive(Clone, Debug)]
struct KeptRange {
    start_ms: i64,
    end_ms: i64,
    requested_start_ms: i64,
    requested_end_ms: i64,
    padded_start_ms: i64,
    padded_end_ms: i64,
    interval_ids: Vec<String>,
}

#[derive(Clone, Copy)]
struct ExportProgress {
    item_index: i64,
    total: i64,
    revision: i64,
}

struct ExportItemTask<'a> {
    job_id: &'a str,
    item_id: &'a str,
    video_id: &'a str,
    intervals_json: &'a str,
    progress: ExportProgress,
}

struct ClipTask<'a> {
    job_id: &'a str,
    item_id: &'a str,
    source: &'a FsPath,
    destination: &'a FsPath,
    start_ms: i64,
    duration_ms: i64,
    progress: ExportProgress,
    range_index: usize,
    range_total: usize,
}

pub async fn create_job(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Json(body): Json<CreateJobBody>,
) -> Result<Json<JobView>, ApiError> {
    if body.video_ids.is_empty() || body.video_ids.len() > 100 {
        return Err(ApiError::bad("Select between 1 and 100 reviewed videos"));
    }
    let padding_ms = body.padding_ms.unwrap_or(DEFAULT_PADDING_MS);
    if !(0..=10_000).contains(&padding_ms) {
        return Err(ApiError::bad(
            "Padding must be from 0 to 10000 milliseconds",
        ));
    }
    let mut ids = HashSet::new();
    if body
        .video_ids
        .iter()
        .any(|id| Uuid::parse_str(id).is_err() || !ids.insert(id))
    {
        return Err(ApiError::bad("Video IDs must be unique UUIDs"));
    }
    let job_id = Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await.map_err(ApiError::internal)?;
    let mut records = Vec::new();
    for video_id in &body.video_ids {
        let row = sqlx::query_as::<_, (i64, String, String, String, i64, String)>("SELECT r.revision,r.status,r.intervals_json,v.source_status,v.duration_ms,v.source_path FROM reviews r JOIN videos v ON v.id=r.video_id WHERE v.id=?")
            .bind(video_id).fetch_optional(&mut *tx).await.map_err(ApiError::internal)?;
        let Some((revision, status, intervals_json, source_status, _, _)) = row else {
            return Err(ApiError::bad(format!("Video {video_id} has no review")));
        };
        if status != "reviewed" {
            return Err(ApiError::bad(format!(
                "Video {video_id} must be marked reviewed before export"
            )));
        }
        if source_status != "available" {
            return Err(ApiError::bad(format!(
                "Video {video_id} is {source_status}"
            )));
        }
        let intervals: Vec<Interval> =
            serde_json::from_str(&intervals_json).map_err(ApiError::internal)?;
        if intervals.is_empty() {
            return Err(ApiError::bad(format!(
                "Video {video_id} has no marked intervals"
            )));
        }
        records.push((video_id.clone(), revision, intervals));
    }
    let now = now_ms();
    sqlx::query("INSERT INTO export_jobs(id,requested_by,status,phase,total_items,padding_ms,created_at_ms) VALUES(?,?,'queued','queued',?,?,?)")
        .bind(&job_id).bind(&user.id).bind(records.len() as i64).bind(padding_ms).bind(now).execute(&mut *tx).await.map_err(ApiError::internal)?;
    for (video_id, revision, intervals) in records {
        sqlx::query("INSERT INTO export_items(id,job_id,video_id,review_revision,intervals_json,status) VALUES(?,?,?,?,?,'queued')")
            .bind(Uuid::new_v4().to_string()).bind(&job_id).bind(video_id).bind(revision).bind(serde_json::to_string(&intervals).map_err(ApiError::internal)?)
            .execute(&mut *tx).await.map_err(ApiError::internal)?;
    }
    tx.commit().await.map_err(ApiError::internal)?;
    Ok(Json(fetch_job(&state, &job_id).await?))
}

pub async fn list_jobs(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
) -> Result<Json<Vec<JobView>>, ApiError> {
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM export_jobs ORDER BY created_at_ms DESC LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let mut jobs = Vec::new();
    for id in ids {
        jobs.push(fetch_job(&state, &id).await?);
    }
    Ok(Json(jobs))
}

pub async fn get_job(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    Ok(Json(fetch_job(&state, &id).await?))
}

pub async fn download_manifest(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM export_jobs WHERE id=?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    if exists.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Export job not found"));
    }
    let root = tokio::fs::canonicalize(state.settings.export_dir.join(&id))
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Export manifest is not ready"))?;
    let path = tokio::fs::canonicalize(root.join("manifest.json"))
        .await
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "Export manifest is not ready"))?;
    if !path.starts_with(&root) {
        return Err(ApiError::forbidden());
    }
    let content = tokio::fs::read(path).await.map_err(ApiError::internal)?;
    serde_json::from_slice(&content)
        .map(Json)
        .map_err(ApiError::internal)
}

pub async fn retry_job(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id=?")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let Some(status) = status else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Export job not found"));
    };
    if status != "failed" {
        return Err(ApiError::conflict("Only failed export jobs can be retried"));
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::internal)?;
    sqlx::query("UPDATE export_items SET status='queued',progress=0,error=NULL WHERE job_id=? AND status='failed'").bind(&id).execute(&mut *tx).await.map_err(ApiError::internal)?;
    sqlx::query("UPDATE export_jobs SET status='queued',phase='retry queued',failed_items=0,progress=CAST(completed_items AS REAL)/total_items,error=NULL,ended_at_ms=NULL WHERE id=?").bind(&id).execute(&mut *tx).await.map_err(ApiError::internal)?;
    tx.commit().await.map_err(ApiError::internal)?;
    info!(job_id=%id, requested_by=%user.username, "Retrying failed export items");
    Ok(Json(fetch_job(&state, &id).await?))
}

pub async fn download_file(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, ApiError> {
    let outputs = sqlx::query_scalar::<_, String>(
        "SELECT i.outputs_json FROM export_items i WHERE i.job_id=? AND i.status='complete'",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let mut found = false;
    for json in outputs {
        let files: Vec<ExportOutput> = serde_json::from_str(&json).map_err(ApiError::internal)?;
        if files.iter().any(|output| output.file_name == name) {
            found = true;
            break;
        }
    }
    if !found {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "Export file not found",
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(ApiError::bad("Invalid export file name"));
    }
    let root = state
        .settings
        .export_dir
        .join(&id)
        .canonicalize()
        .map_err(ApiError::internal)?;
    let path = root
        .join(&name)
        .canonicalize()
        .map_err(ApiError::internal)?;
    if !path.starts_with(&root) {
        return Err(ApiError::forbidden());
    }
    media::serve_file(path, &headers, &method).await
}

pub async fn worker(state: AppState) {
    if let Err(error) = sqlx::query("UPDATE export_jobs SET status='queued',phase='resuming after restart' WHERE status='running'").execute(&state.pool).await {
        error!(%error, "Could not resume export jobs at startup");
    }
    if let Err(error) =
        sqlx::query("UPDATE export_items SET status='queued',progress=0 WHERE status='running'")
            .execute(&state.pool)
            .await
    {
        error!(%error, "Could not resume export items at startup");
    }
    loop {
        match run_next(&state).await {
            Ok(true) => {}
            Ok(false) => sleep(Duration::from_secs(1)).await,
            Err(error) => {
                error!(%error, "Export worker failed");
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn run_next(state: &AppState) -> anyhow::Result<bool> {
    let next = sqlx::query_scalar::<_, String>(
        "SELECT id FROM export_jobs WHERE status='queued' ORDER BY created_at_ms LIMIT 1",
    )
    .fetch_optional(&state.pool)
    .await?;
    let Some(job_id) = next else {
        return Ok(false);
    };
    let now = now_ms();
    sqlx::query("UPDATE export_jobs SET status='running',phase='starting',started_at_ms=COALESCE(started_at_ms,?),error=NULL WHERE id=? AND status='queued'").bind(now).bind(&job_id).execute(&state.pool).await?;
    let total_items =
        sqlx::query_scalar::<_, i64>("SELECT total_items FROM export_jobs WHERE id=?")
            .bind(&job_id)
            .fetch_one(&state.pool)
            .await?;
    info!(phase="export", job_id=%job_id, completed=0, total_items, "Export job started");
    if let Err(error) = process_job(state, &job_id).await {
        let message = error.to_string();
        let _ = sqlx::query(
            "UPDATE export_items SET status='failed',error=? WHERE job_id=? AND status='running'",
        )
        .bind(&message)
        .bind(&job_id)
        .execute(&state.pool)
        .await;
        let _ = sqlx::query("UPDATE export_jobs SET status='failed',phase='failed',error=?,ended_at_ms=? WHERE id=?").bind(&message).bind(now_ms()).bind(&job_id).execute(&state.pool).await;
        return Err(error);
    }
    Ok(true)
}

async fn process_job(state: &AppState, job_id: &str) -> anyhow::Result<()> {
    let items = sqlx::query_as::<_, (String,String,String,i64)>("SELECT i.id,i.video_id,i.intervals_json,i.review_revision FROM export_items i WHERE i.job_id=? AND i.status='queued' ORDER BY i.id")
        .bind(job_id).fetch_all(&state.pool).await?;
    let job_started = Instant::now();
    let total = sqlx::query_scalar::<_, i64>("SELECT total_items FROM export_jobs WHERE id=?")
        .bind(job_id)
        .fetch_one(&state.pool)
        .await?;
    let completed_before = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM export_items WHERE job_id=? AND status='complete'",
    )
    .bind(job_id)
    .fetch_one(&state.pool)
    .await?;
    for (index, (item_id, video_id, intervals_json, revision)) in items.iter().enumerate() {
        let item_index = completed_before + index as i64;
        sqlx::query("UPDATE export_items SET status='running',progress=0,error=NULL WHERE id=?")
            .bind(item_id)
            .execute(&state.pool)
            .await?;
        sqlx::query("UPDATE export_jobs SET phase=? WHERE id=?")
            .bind(format!("preparing file {}/{}", item_index + 1, total))
            .bind(job_id)
            .execute(&state.pool)
            .await?;
        let result = export_item(
            state,
            ExportItemTask {
                job_id,
                item_id,
                video_id,
                intervals_json,
                progress: ExportProgress {
                    item_index,
                    total,
                    revision: *revision,
                },
            },
        )
        .await;
        match result {
            Ok(outputs) => {
                sqlx::query("UPDATE export_items SET status='complete',progress=1,outputs_json=?,error=NULL WHERE id=?")
                    .bind(serde_json::to_string(&outputs)?).bind(item_id).execute(&state.pool).await?;
                info!(phase="export", job_id=%job_id, completed=item_index+1, total, percent=((item_index+1) as f64 / total as f64 * 100.0), elapsed_seconds=job_started.elapsed().as_secs_f64(), "Video export complete");
            }
            Err(error) => {
                warn!(job_id=%job_id, video_id=%video_id, %error, "Video export failed");
                sqlx::query("UPDATE export_items SET status='failed',error=? WHERE id=?")
                    .bind(error.to_string())
                    .bind(item_id)
                    .execute(&state.pool)
                    .await?;
            }
        }
        let completed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM export_items WHERE job_id=? AND status='complete'",
        )
        .bind(job_id)
        .fetch_one(&state.pool)
        .await?;
        let failed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM export_items WHERE job_id=? AND status='failed'",
        )
        .bind(job_id)
        .fetch_one(&state.pool)
        .await?;
        let current: f64 = sqlx::query_scalar("SELECT COALESCE(avg(progress),0.0) FROM export_items WHERE job_id=? AND status!='complete' AND status!='failed'").bind(job_id).fetch_one(&state.pool).await?;
        let progress = ((completed as f64 + current) / total.max(1) as f64).clamp(0.0, 1.0);
        sqlx::query(
            "UPDATE export_jobs SET completed_items=?,failed_items=?,progress=? WHERE id=?",
        )
        .bind(completed)
        .bind(failed)
        .bind(progress)
        .bind(job_id)
        .execute(&state.pool)
        .await?;
    }
    let (completed, failed): (i64, i64) =
        sqlx::query_as("SELECT completed_items,failed_items FROM export_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&state.pool)
            .await?;
    let status = if failed == 0 { "complete" } else { "failed" };
    let summary = if failed == 0 {
        None
    } else {
        Some(format!("{failed} of {total} video files failed"))
    };
    write_manifest(state, job_id, status).await?;
    sqlx::query(
        "UPDATE export_jobs SET status=?,phase=?,progress=?,error=?,ended_at_ms=? WHERE id=?",
    )
    .bind(status)
    .bind(status)
    .bind(if failed == 0 {
        1.0
    } else {
        completed as f64 / total.max(1) as f64
    })
    .bind(summary)
    .bind(now_ms())
    .bind(job_id)
    .execute(&state.pool)
    .await?;
    info!(
        phase = "export",
        job_id,
        completed,
        failed,
        total,
        elapsed_seconds = job_started.elapsed().as_secs_f64(),
        "Export job finished"
    );
    Ok(())
}

async fn export_item(
    state: &AppState,
    task: ExportItemTask<'_>,
) -> anyhow::Result<Vec<ExportOutput>> {
    let ExportItemTask {
        job_id,
        item_id,
        video_id,
        intervals_json,
        progress,
    } = task;
    let ExportProgress {
        item_index,
        total,
        revision: _,
    } = progress;
    let row = sqlx::query("SELECT source_path,duration_ms,frame_rate,camera_id,camera_name,session_id,source_size,source_mtime_ms FROM videos WHERE id=? AND source_status='available'").bind(video_id).fetch_optional(&state.pool).await?.context("Source video is no longer available")?;
    let source_path: String = row.try_get("source_path")?;
    let duration_ms: i64 = row.try_get("duration_ms")?;
    let frame_rate: String = row.try_get("frame_rate")?;
    let camera_id: String = row.try_get("camera_id")?;
    let expected_size: i64 = row.try_get("source_size")?;
    let expected_mtime: i64 = row.try_get("source_mtime_ms")?;
    let root = state.settings.source_dir.clone();
    let source = tokio::fs::canonicalize(root.join(&source_path))
        .await
        .context("Cannot locate source video")?;
    if !source.starts_with(&root) {
        anyhow::bail!("Source path is outside the configured media directory");
    }
    let metadata = tokio::fs::metadata(&source).await?;
    if !metadata.is_file() {
        anyhow::bail!("Source video is not a regular file");
    }
    let source_mtime = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    if metadata.len() as i64 != expected_size || source_mtime != expected_mtime {
        anyhow::bail!(
            "Source changed after indexing; scan and accept the new source before export"
        );
    }
    let intervals: Vec<Interval> = serde_json::from_str(intervals_json)?;
    let requested_padding: i64 =
        sqlx::query_scalar("SELECT padding_ms FROM export_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&state.pool)
            .await?;
    let requested_padding = requested_padding.clamp(0, 10_000);
    let source_for_probe = source.clone();
    let estimate = estimate_frame_count(duration_ms, &frame_rate);
    sqlx::query("UPDATE export_jobs SET phase=? WHERE id=?")
        .bind(format!(
            "file {}/{}: indexing keyframes",
            item_index + 1,
            total
        ))
        .bind(job_id)
        .execute(&state.pool)
        .await?;
    info!(
        phase = "keyframes",
        item_id,
        completed = 0,
        total_estimated = estimate,
        eta = "unavailable",
        "Scanning source keyframes"
    );
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let probe = tokio::task::spawn_blocking(move || {
        probe_keyframes(&source_for_probe, estimate, progress_tx)
    });
    while let Some((frames, elapsed, eta)) = progress_rx.recv().await {
        let fraction = (frames as f64 / estimate as f64).clamp(0.0, 0.999);
        let item_progress = fraction * 0.1;
        report_item_progress(
            state,
            job_id,
            item_id,
            item_index,
            total,
            item_progress,
            &format!(
                "file {}/{}: indexing keyframes {:.1}%",
                item_index + 1,
                total,
                fraction * 100.0
            ),
        )
        .await?;
        info!(phase="keyframes", job_id, completed=frames, total_estimated=estimate, percent=fraction*100.0, elapsed_seconds=elapsed, eta=%eta, "Keyframe scan progress");
    }
    let keyframes = probe.await??;
    if keyframes.is_empty() {
        anyhow::bail!("FFprobe found no video keyframes");
    }
    report_item_progress(
        state,
        job_id,
        item_id,
        item_index,
        total,
        0.1,
        &format!("file {}/{}: preparing ranges", item_index + 1, total),
    )
    .await?;
    let mut ranges = prepare_ranges(&intervals, duration_ms, requested_padding, &keyframes);
    coalesce_ranges(&mut ranges);
    let range_count = ranges.len().max(1);
    let job_dir = state.settings.export_dir.join(job_id);
    tokio::fs::create_dir_all(&job_dir).await?;
    let basename = FsPath::new(&source_path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("video");
    let safe_name: String = basename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe_camera_id: String = camera_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut outputs: Vec<ExportOutput> =
        sqlx::query_scalar::<_, String>("SELECT outputs_json FROM export_items WHERE id=?")
            .bind(item_id)
            .fetch_one(&state.pool)
            .await
            .and_then(|json| {
                serde_json::from_str(&json).map_err(|error| sqlx::Error::Decode(Box::new(error)))
            })?;
    for (range_index, range) in ranges.into_iter().enumerate() {
        let name = format!(
            "camera-{safe_camera_id}_{}_{}_{}-{}.mp4",
            &video_id[..8],
            safe_name,
            range.start_ms,
            range.end_ms
        );
        let destination = job_dir.join(&name);
        if let Some(saved) = outputs.iter().find(|output| output.file_name == name) {
            if tokio::fs::metadata(&destination)
                .await
                .is_ok_and(|metadata| metadata.len() == saved.bytes)
            {
                verify_clip(&destination).await?;
                let done = (range_index + 1) as f64 / range_count as f64;
                report_item_progress(
                    state,
                    job_id,
                    item_id,
                    item_index,
                    total,
                    0.1 + 0.9 * done,
                    &format!(
                        "file {}/{}: verified range {}/{}",
                        item_index + 1,
                        total,
                        range_index + 1,
                        range_count
                    ),
                )
                .await?;
                continue;
            }
            outputs.retain(|output| output.file_name != name);
        }
        let temporary = job_dir.join(format!(".{}-{item_id}.part.mp4", range_index));
        let span = (range.end_ms - range.start_ms).max(1);
        sqlx::query("UPDATE export_jobs SET phase=? WHERE id=?")
            .bind(format!(
                "file {}/{}: exporting range {}/{}",
                item_index + 1,
                total,
                range_index + 1,
                range_count
            ))
            .bind(job_id)
            .execute(&state.pool)
            .await?;
        run_ffmpeg(
            state,
            ClipTask {
                job_id,
                item_id,
                source: &source,
                destination: &temporary,
                start_ms: range.start_ms,
                duration_ms: span,
                progress,
                range_index,
                range_total: range_count,
            },
        )
        .await?;
        let clip_duration_ms = verify_clip(&temporary).await?;
        let actual_end_ms = range.start_ms + clip_duration_ms;
        if range.start_ms > range.padded_start_ms || actual_end_ms + 40 < range.padded_end_ms {
            anyhow::bail!("The exported clip does not cover the selected range and its padding");
        }
        let output_metadata = tokio::fs::metadata(&temporary).await?;
        tokio::fs::rename(&temporary, &destination).await?;
        outputs.push(ExportOutput {
            file_name: name,
            interval_ids: range.interval_ids,
            requested_start_ms: range.requested_start_ms,
            requested_end_ms: range.requested_end_ms,
            padded_start_ms: range.padded_start_ms,
            padded_end_ms: range.padded_end_ms,
            actual_start_ms: range.start_ms,
            actual_end_ms,
            duration_ms: clip_duration_ms,
            bytes: output_metadata.len(),
        });
        sqlx::query("UPDATE export_items SET outputs_json=? WHERE id=?")
            .bind(serde_json::to_string(&outputs)?)
            .bind(item_id)
            .execute(&state.pool)
            .await?;
        let done = outputs.len() as f64 / range_count as f64;
        report_item_progress(
            state,
            job_id,
            item_id,
            item_index,
            total,
            (0.1 + 0.9 * done).clamp(0.1, 0.99),
            &format!(
                "file {}/{}: verified range {}/{}",
                item_index + 1,
                total,
                range_index + 1,
                range_count
            ),
        )
        .await?;
    }
    Ok(outputs)
}

fn estimate_frame_count(duration_ms: i64, frame_rate: &str) -> usize {
    let parsed = frame_rate
        .split_once('/')
        .and_then(|(numerator, denominator)| {
            let numerator = numerator.parse::<f64>().ok()?;
            let denominator = denominator.parse::<f64>().ok()?;
            (denominator > 0.0).then_some(numerator / denominator)
        })
        .or_else(|| frame_rate.parse::<f64>().ok())
        .filter(|rate| rate.is_finite() && *rate > 0.0)
        .unwrap_or(25.0);
    ((duration_ms.max(0) as f64 / 1000.0 * parsed).ceil() as usize).max(1)
}

async fn report_item_progress(
    state: &AppState,
    job_id: &str,
    item_id: &str,
    item_index: i64,
    total: i64,
    progress: f64,
    phase: &str,
) -> anyhow::Result<()> {
    let progress = progress.clamp(0.0, 0.99);
    let job_progress = ((item_index as f64 + progress) / total.max(1) as f64).clamp(0.0, 0.999);
    sqlx::query("UPDATE export_items SET progress=? WHERE id=?")
        .bind(progress)
        .bind(item_id)
        .execute(&state.pool)
        .await?;
    sqlx::query("UPDATE export_jobs SET progress=?,phase=? WHERE id=?")
        .bind(job_progress)
        .bind(phase)
        .bind(job_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

fn prepare_ranges(
    intervals: &[Interval],
    duration_ms: i64,
    padding_ms: i64,
    keyframes: &[i64],
) -> Vec<KeptRange> {
    intervals
        .iter()
        .map(|interval| {
            let requested_start = (interval.start_ms - padding_ms).max(0);
            let requested_end = (interval.end_ms + padding_ms).min(duration_ms);
            let start = keyframes
                .iter()
                .copied()
                .take_while(|time| *time <= requested_start)
                .last()
                .unwrap_or(keyframes[0]);
            let end = keyframes
                .iter()
                .copied()
                .find(|time| *time >= requested_end)
                .unwrap_or(duration_ms);
            KeptRange {
                start_ms: start,
                end_ms: end.max(start + 1).min(duration_ms),
                requested_start_ms: interval.start_ms,
                requested_end_ms: interval.end_ms,
                padded_start_ms: requested_start,
                padded_end_ms: requested_end,
                interval_ids: vec![interval.id.clone()],
            }
        })
        .collect()
}

fn coalesce_ranges(ranges: &mut Vec<KeptRange>) {
    ranges.sort_by_key(|range| range.start_ms);
    let mut merged: Vec<KeptRange> = Vec::new();
    for range in ranges.drain(..) {
        if let Some(previous) = merged
            .last_mut()
            .filter(|previous| range.start_ms <= previous.end_ms)
        {
            previous.requested_start_ms = previous.requested_start_ms.min(range.requested_start_ms);
            previous.requested_end_ms = previous.requested_end_ms.max(range.requested_end_ms);
            previous.padded_start_ms = previous.padded_start_ms.min(range.padded_start_ms);
            previous.padded_end_ms = previous.padded_end_ms.max(range.padded_end_ms);
            previous.end_ms = previous.end_ms.max(range.end_ms);
            previous.interval_ids.extend(range.interval_ids);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

fn probe_keyframes(
    path: &FsPath,
    estimated_frames: usize,
    progress: mpsc::UnboundedSender<(usize, f64, String)>,
) -> anyhow::Result<Vec<i64>> {
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("FFprobe is required to find keyframes")?;
    let stdout = child.stdout.take().context("Cannot read FFprobe output")?;
    let reader = BufReader::new(stdout);
    let started = Instant::now();
    let mut frames = 0usize;
    let mut keyframes = Vec::new();
    for line in reader.lines() {
        let line = line?;
        frames += 1;
        if frames.is_multiple_of(250) {
            let percent = (frames as f64 / estimated_frames as f64 * 100.0).min(99.9);
            let eta = if frames > 0 {
                format!(
                    "{:.1}s",
                    started.elapsed().as_secs_f64()
                        * (estimated_frames.saturating_sub(frames) as f64)
                        / frames as f64
                )
            } else {
                "unavailable".into()
            };
            let _ = progress.send((frames, started.elapsed().as_secs_f64(), eta.clone()));
            info!(phase="keyframes", completed=frames, total_estimated=estimated_frames, percent, elapsed_seconds=started.elapsed().as_secs_f64(), eta=%eta, "Reading source packets");
        }
        let Some((time, flags)) = line.trim().split_once(',') else {
            continue;
        };
        if flags.trim().starts_with('K')
            && let Ok(seconds) = time.parse::<f64>()
            && seconds.is_finite()
        {
            keyframes.push((seconds * 1000.0).round() as i64);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("FFprobe failed while reading keyframes");
    }
    keyframes.sort_unstable();
    keyframes.dedup();
    let elapsed = started.elapsed().as_secs_f64();
    let _ = progress.send((frames, elapsed, "0s".into()));
    info!(
        phase = "keyframes",
        completed = frames,
        total_estimated = estimated_frames,
        percent = 100,
        elapsed_seconds = elapsed,
        eta = "0s",
        "Keyframe scan complete"
    );
    Ok(keyframes)
}

async fn run_ffmpeg(state: &AppState, task: ClipTask<'_>) -> anyhow::Result<()> {
    let ClipTask {
        job_id,
        item_id,
        source,
        destination,
        start_ms,
        duration_ms,
        progress,
        range_index,
        range_total,
    } = task;
    let ExportProgress {
        item_index,
        total,
        revision,
    } = progress;
    let start = format!("{:.3}", start_ms as f64 / 1000.0);
    let duration = format!("{:.3}", duration_ms as f64 / 1000.0);
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-ss",
            &start,
            "-i",
        ])
        .arg(source)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-t",
            &duration,
            "-c:v",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
        ])
        .arg(destination)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("FFmpeg is required to export clips")?;
    let stdout = child.stdout.take().context("Cannot read FFmpeg progress")?;
    let stderr = child.stderr.take().context("Cannot read FFmpeg errors")?;
    let error_reader = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut bytes = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes).await;
        String::from_utf8_lossy(&bytes)
            .chars()
            .take(6000)
            .collect::<String>()
    });
    let started = Instant::now();
    let mut reader = tokio::io::BufReader::new(stdout).lines();
    let mut last_report = Instant::now();
    let mut clip_progress = 0.0_f64;
    while let Some(line) = reader.next_line().await? {
        if let Some(value) = line
            .strip_prefix("out_time_us=")
            .or_else(|| line.strip_prefix("out_time_ms="))
            && let Ok(microseconds) = value.parse::<i64>()
        {
            clip_progress = (microseconds as f64 / (duration_ms as f64 * 1000.0)).clamp(0.0, 1.0);
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            let item_progress =
                0.1 + 0.9 * ((range_index as f64 + clip_progress) / range_total.max(1) as f64);
            let total_progress =
                ((item_index as f64 + item_progress) / total.max(1) as f64).clamp(0.0, 0.999);
            let eta = if clip_progress > 0.01 {
                format!(
                    "{:.1}s",
                    started.elapsed().as_secs_f64() * (1.0 - clip_progress) / clip_progress
                )
            } else {
                "unavailable".into()
            };
            let phase = format!(
                "file {}/{}: exporting range {}/{} ({:.1}%)",
                item_index + 1,
                total,
                range_index + 1,
                range_total,
                clip_progress * 100.0
            );
            report_item_progress(
                state,
                job_id,
                item_id,
                item_index,
                total,
                item_progress,
                &phase,
            )
            .await?;
            info!(phase="export", job_id, completed=(item_index as f64+item_progress), total, percent=total_progress*100.0, elapsed_seconds=started.elapsed().as_secs_f64(),eta=%eta, review_revision=revision,"FFmpeg export progress");
            last_report = Instant::now();
        }
    }
    let status = child.wait().await?;
    let errors = error_reader.await.unwrap_or_default();
    if !status.success() {
        anyhow::bail!("FFmpeg failed: {}", errors.trim());
    }
    Ok(())
}

async fn verify_clip(path: &FsPath) -> anyhow::Result<i64> {
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .context("Cannot inspect exported clip")?;
    if !probe.status.success() {
        anyhow::bail!("FFprobe rejected the exported clip");
    }
    let seconds = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse::<f64>()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        anyhow::bail!("Exported clip has no video duration");
    }
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-xerror", "-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-f", "null", "-"])
        .output()
        .await
        .context("Cannot run FFmpeg integrity check")?;
    if !output.status.success() || !output.stderr.is_empty() {
        anyhow::bail!(
            "Exported clip failed full video decoding: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok((seconds * 1000.0).round() as i64)
}

async fn fetch_job(state: &AppState, id: &str) -> Result<JobView, ApiError> {
    let row = sqlx::query("SELECT id,status,phase,total_items,completed_items,failed_items,progress,error,created_at_ms,started_at_ms,ended_at_ms FROM export_jobs WHERE id=?").bind(id).fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
    let Some(row) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Export job not found"));
    };
    let mut items = Vec::new();
    for item in sqlx::query("SELECT id,video_id,status,progress,error,outputs_json FROM export_items WHERE job_id=? ORDER BY id").bind(id).fetch_all(&state.pool).await.map_err(ApiError::internal)? {
        items.push(JobItemView {
            id:item.try_get("id").map_err(ApiError::internal)?, video_id:item.try_get("video_id").map_err(ApiError::internal)?,
            status:item.try_get("status").map_err(ApiError::internal)?, progress:item.try_get("progress").map_err(ApiError::internal)?,
            error:item.try_get("error").map_err(ApiError::internal)?, outputs:serde_json::from_str(item.try_get::<String,_>("outputs_json").map_err(ApiError::internal)?.as_str()).map_err(ApiError::internal)?,
        });
    }
    Ok(JobView {
        id: row.try_get("id").map_err(ApiError::internal)?,
        status: row.try_get("status").map_err(ApiError::internal)?,
        phase: row.try_get("phase").map_err(ApiError::internal)?,
        total_items: row.try_get("total_items").map_err(ApiError::internal)?,
        completed_items: row.try_get("completed_items").map_err(ApiError::internal)?,
        failed_items: row.try_get("failed_items").map_err(ApiError::internal)?,
        progress: row.try_get("progress").map_err(ApiError::internal)?,
        error: row.try_get("error").map_err(ApiError::internal)?,
        created_at_ms: row.try_get("created_at_ms").map_err(ApiError::internal)?,
        started_at_ms: row.try_get("started_at_ms").map_err(ApiError::internal)?,
        ended_at_ms: row.try_get("ended_at_ms").map_err(ApiError::internal)?,
        items,
    })
}

async fn write_manifest(state: &AppState, job_id: &str, status: &str) -> anyhow::Result<()> {
    let job_dir = state.settings.export_dir.join(job_id);
    tokio::fs::create_dir_all(&job_dir).await?;
    let (created_at_ms, padding_ms): (i64, i64) =
        sqlx::query_as("SELECT created_at_ms,padding_ms FROM export_jobs WHERE id=?")
            .bind(job_id)
            .fetch_one(&state.pool)
            .await?;
    let items = sqlx::query("SELECT i.video_id,i.review_revision,i.status,i.intervals_json,i.outputs_json,v.camera_id,v.camera_name,v.session_id,v.source_path,v.session_started_at FROM export_items i JOIN videos v ON v.id=i.video_id WHERE i.job_id=? ORDER BY i.id").bind(job_id).fetch_all(&state.pool).await?;
    let mut videos = Vec::new();
    for row in items {
        videos.push(serde_json::json!({
            "video_id":row.try_get::<String,_>("video_id")?,
            "review_revision":row.try_get::<i64,_>("review_revision")?,
            "status":row.try_get::<String,_>("status")?,
            "camera_id":row.try_get::<String,_>("camera_id")?,
            "camera_name":row.try_get::<String,_>("camera_name")?,
            "session_id":row.try_get::<String,_>("session_id")?,
            "session_started_at":row.try_get::<String,_>("session_started_at")?,
            "source_path":row.try_get::<String,_>("source_path")?,
            "intervals":serde_json::from_str::<Vec<Interval>>(&row.try_get::<String,_>("intervals_json")?)?,
            "outputs":serde_json::from_str::<Vec<ExportOutput>>(&row.try_get::<String,_>("outputs_json")?)?
        }));
    }
    let manifest = serde_json::json!({"job_id":job_id,"status":status,"created_at_ms":created_at_ms,"padding_ms":padding_ms,"videos":videos});
    let target = job_dir.join("manifest.json");
    let temporary = job_dir.join("manifest.json.part");
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(&manifest)?).await?;
    tokio::fs::rename(temporary, target).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::LoginLimiter, config::Settings};
    use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
    use std::{fs, path::PathBuf};

    #[test]
    fn snaps_intervals_outward_and_coalesces_overlap() {
        let intervals = vec![
            Interval {
                id: "a".into(),
                start_ms: 1250,
                end_ms: 2750,
            },
            Interval {
                id: "b".into(),
                start_ms: 3200,
                end_ms: 3900,
            },
        ];
        let mut ranges = prepare_ranges(&intervals, 5000, 500, &[0, 1000, 2000, 3000, 4000]);
        coalesce_ranges(&mut ranges);
        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start_ms, ranges[0].end_ms), (0, 5000));
        assert_eq!(ranges[0].interval_ids, vec!["a", "b"]);
        assert_eq!(
            (ranges[0].requested_start_ms, ranges[0].requested_end_ms),
            (1250, 3900)
        );
        assert_eq!(
            (ranges[0].padded_start_ms, ranges[0].padded_end_ms),
            (750, 4400)
        );
    }

    #[test]
    fn clamps_padding_and_final_boundary_to_source() {
        let intervals = vec![Interval {
            id: "a".into(),
            start_ms: 4800,
            end_ms: 5000,
        }];
        let ranges = prepare_ranges(&intervals, 5000, 500, &[0, 1000, 2000, 3000, 4000]);
        assert_eq!((ranges[0].start_ms, ranges[0].end_ms), (4000, 5000));
        assert_eq!(
            (ranges[0].requested_start_ms, ranges[0].requested_end_ms),
            (4800, 5000)
        );
        assert_eq!(
            (ranges[0].padded_start_ms, ranges[0].padded_end_ms),
            (4300, 5000)
        );
    }

    #[test]
    fn estimates_keyframe_scan_work_from_source_frame_rate() {
        assert_eq!(estimate_frame_count(2000, "30/1"), 60);
        assert_eq!(estimate_frame_count(2000, "unknown"), 50);
    }

    #[tokio::test]
    async fn exports_and_manifests_a_valid_stream_copy_clip() {
        let source_root = tempfile::tempdir().unwrap();
        let export_root = tempfile::tempdir().unwrap();
        let source_relative = "session/chunk.mp4";
        let source = source_root.path().join(source_relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let generated = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=25:duration=5",
                "-an",
                "-c:v",
                "libx264",
                "-g",
                "25",
                "-keyint_min",
                "25",
                "-sc_threshold",
                "0",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let metadata = fs::metadata(&source).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let pool: SqlitePool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let user_id = Uuid::new_v4().to_string();
        let video_id = Uuid::new_v4().to_string();
        let job_id = Uuid::new_v4().to_string();
        let item_id = Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO users(id,username,password_hash,role,created_at_ms) VALUES (?, 'reviewer', 'x', 'reviewer', 0)").bind(&user_id).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO videos(id,source_path,source_size,source_mtime_ms,camera_id,camera_name,session_id,session_started_at,session_kind,source_status,duration_ms,width,height,codec,frame_rate,sort_key) VALUES (?,?,?,?,'91','Camera','session','2026-01-01','recording','available',5000,160,90,'h264','25/1','chunk.mp4')")
            .bind(&video_id).bind(source_relative).bind(metadata.len() as i64).bind(mtime).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO export_jobs(id,requested_by,status,phase,total_items,padding_ms,created_at_ms) VALUES (?,?,'running','exporting',1,500,0)").bind(&job_id).bind(&user_id).execute(&pool).await.unwrap();
        let intervals = vec![Interval {
            id: Uuid::new_v4().to_string(),
            start_ms: 1250,
            end_ms: 2750,
        }];
        sqlx::query("INSERT INTO export_items(id,job_id,video_id,review_revision,intervals_json,status) VALUES (?,?,?,?,?,'queued')")
            .bind(&item_id).bind(&job_id).bind(&video_id).bind(1_i64).bind(serde_json::to_string(&intervals).unwrap()).execute(&pool).await.unwrap();
        let state = AppState {
            pool: pool.clone(),
            settings: Settings {
                source_dir: source_root.path().to_path_buf(),
                state_dir: PathBuf::from("."),
                export_dir: export_root.path().to_path_buf(),
                web_dir: PathBuf::from("web/dist"),
                bind: "127.0.0.1:8080".parse().unwrap(),
                app_origin: "http://127.0.0.1:5173".into(),
                secure_cookie: false,
                trust_proxy_headers: false,
            },
            login_limiter: LoginLimiter::default(),
        };

        process_job(&state, &job_id).await.unwrap();

        let status: String = sqlx::query_scalar("SELECT status FROM export_jobs WHERE id=?")
            .bind(&job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "complete");
        let output_json: String =
            sqlx::query_scalar("SELECT outputs_json FROM export_items WHERE id=?")
                .bind(&item_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let outputs: Vec<ExportOutput> = serde_json::from_str(&output_json).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            (outputs[0].requested_start_ms, outputs[0].requested_end_ms),
            (1250, 2750)
        );
        assert_eq!(
            (outputs[0].padded_start_ms, outputs[0].padded_end_ms),
            (750, 3250)
        );
        assert!(outputs[0].actual_end_ms >= outputs[0].padded_end_ms);
        assert!(
            export_root
                .path()
                .join(&job_id)
                .join(&outputs[0].file_name)
                .is_file()
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(export_root.path().join(&job_id).join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["status"], "complete");
        assert_eq!(manifest["padding_ms"], 500);
        assert_eq!(manifest["videos"][0]["intervals"][0]["start_ms"], 1250);
        assert_eq!(manifest["videos"][0]["review_revision"], 1);
        assert_eq!(
            manifest["videos"][0]["outputs"][0]["file_name"],
            outputs[0].file_name
        );
    }
}
