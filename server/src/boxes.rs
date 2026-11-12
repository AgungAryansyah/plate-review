//! Clip-first SAM proposal review. Original clips remain immutable; reviews only
//! select track IDs, assign one plate text, and configure export filters.
use crate::{
    api::{ApiError, AppState, UserView},
    db::now_ms,
    exports::ExportOutput,
    media,
};
use axum::{
    Json,
    body::Body,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::{Path as FsPath, PathBuf},
    sync::OnceLock,
};
use tokio::{
    process::Command,
    sync::{Mutex, Semaphore},
    time::{Duration, sleep},
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Meta {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub frame_count: u64,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Detection {
    pub frame: u64,
    pub timestamp_ms: u64,
    pub x_min: f64,
    pub y_min: f64,
    pub x_max: f64,
    pub y_max: f64,
    pub confidence: f64,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Track {
    pub id: String,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub plate_text: String,
    pub detections: Vec<Detection>,
}
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
pub struct Choice {
    pub id: String,
    pub selected: bool,
    pub plate_text: String,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Settings {
    pub min_confidence: f64,
    pub jitter_px: f64,
    pub smoothing_radius: u64,
    pub minimum_frames: usize,
    pub max_area_fraction: f64,
    pub max_size_ratio: f64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            min_confidence: 0.25,
            jitter_px: 8.,
            smoothing_radius: 2,
            minimum_frames: 2,
            max_area_fraction: 0.08,
            max_size_ratio: 3.,
        }
    }
}
#[derive(Serialize)]
pub struct Clip {
    pub id: String,
    pub source_video_id: String,
    pub export_job_id: String,
    pub name: String,
    pub bytes: i64,
    pub duration_ms: i64,
    pub status: String,
    pub progress: f64,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Job {
    pub id: String,
    pub status: String,
    pub progress: f64,
    pub phase: String,
    pub logs: Vec<String>,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct Detail {
    pub clip: Clip,
    pub meta: Meta,
    pub tracks: Vec<Track>,
    pub settings: Settings,
    pub revision: i64,
    pub job: Option<Job>,
}
#[derive(Deserialize)]
pub struct SaveBody {
    expected_revision: i64,
    tracks: Vec<Choice>,
    settings: Settings,
}
#[derive(Deserialize, Default)]
pub struct FrameQuery {
    timestamp_ms: Option<u64>,
}
#[derive(Deserialize)]
struct Proposal {
    source_sha256: String,
    processed_frames: u64,
    meta: Meta,
    tracks: Vec<Track>,
}

pub fn queue_root(state: &AppState) -> PathBuf {
    std::env::var_os("PLATE_REVIEW_SAM_QUEUE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.settings.state_dir.join("sam-jobs"))
}

async fn discover(state: &AppState) -> Result<(), ApiError> {
    let rows = sqlx::query(
        "SELECT job_id,video_id,outputs_json FROM export_items WHERE status='complete'",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    for row in rows {
        let job: String = row.get("job_id");
        if Uuid::parse_str(&job).is_err() {
            continue;
        }
        let source: String = row.get("video_id");
        let outputs: Vec<ExportOutput> =
            serde_json::from_str(&row.get::<String, _>("outputs_json"))
                .map_err(ApiError::internal)?;
        for out in outputs {
            if FsPath::new(&out.file_name)
                .file_name()
                .and_then(|v| v.to_str())
                != Some(&out.file_name)
            {
                continue;
            }
            let relative = format!("{job}/{}", out.file_name);
            let id = Uuid::new_v5(&Uuid::NAMESPACE_URL, relative.as_bytes()).to_string();
            let file = state.settings.export_dir.join(&relative);
            let Ok(metadata) = tokio::fs::metadata(file).await else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            sqlx::query("INSERT OR IGNORE INTO box_clips(id,source_video_id,export_job_id,name,relative_path,bytes,duration_ms,created_at_ms) VALUES(?,?,?,?,?,?,?,?)")
                .bind(id).bind(&source).bind(&job).bind(out.file_name).bind(relative).bind(metadata.len() as i64).bind(out.duration_ms).bind(now_ms())
                .execute(&state.pool).await.map_err(ApiError::internal)?;
        }
    }
    Ok(())
}

async fn latest_job(state: &AppState, id: &str) -> Result<Option<Job>, ApiError> {
    let row = sqlx::query("SELECT id,status,progress,phase,logs_json,error FROM box_jobs WHERE clip_id=? ORDER BY created_at_ms DESC LIMIT 1")
        .bind(id).fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
    Ok(row.map(|r| Job {
        id: r.get("id"),
        status: r.get("status"),
        progress: r.get("progress"),
        phase: r.get("phase"),
        logs: serde_json::from_str(&r.get::<String, _>("logs_json")).unwrap_or_default(),
        error: r.get("error"),
    }))
}
async fn clip(state: &AppState, id: &str) -> Result<Clip, ApiError> {
    let r = sqlx::query(
        "SELECT id,source_video_id,export_job_id,name,bytes,duration_ms FROM box_clips WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Clip not found"))?;
    let job = latest_job(state, id).await?;
    Ok(Clip {
        id: r.get("id"),
        source_video_id: r.get("source_video_id"),
        export_job_id: r.get("export_job_id"),
        name: r.get("name"),
        bytes: r.get("bytes"),
        duration_ms: r.get("duration_ms"),
        status: job
            .as_ref()
            .map(|j| j.status.clone())
            .unwrap_or_else(|| "unsegmented".into()),
        progress: job.as_ref().map(|j| j.progress).unwrap_or(0.),
        error: job.and_then(|j| j.error),
    })
}
async fn source_path(state: &AppState, id: &str) -> Result<PathBuf, ApiError> {
    let relative =
        sqlx::query_scalar::<_, String>("SELECT relative_path FROM box_clips WHERE id=?")
            .bind(id)
            .fetch_optional(&state.pool)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Clip not found"))?;
    let root = tokio::fs::canonicalize(&state.settings.export_dir)
        .await
        .map_err(ApiError::internal)?;
    let path = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|_| ApiError::new(StatusCode::GONE, "Clip file is missing"))?;
    if !path.starts_with(root) {
        return Err(ApiError::forbidden());
    }
    Ok(path)
}
async fn probe(path: &FsPath) -> Result<Meta, ApiError> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate,nb_frames:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .await
        .map_err(ApiError::internal)?;
    if !out.status.success() {
        return Err(ApiError::bad("Cannot read clip metadata"));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(ApiError::internal)?;
    let s = &v["streams"][0];
    let rate = s["avg_frame_rate"].as_str().unwrap_or("25/1");
    let (a, b) = rate.split_once('/').unwrap_or((rate, "1"));
    let fps = a.parse::<f64>().unwrap_or(25.) / b.parse::<f64>().unwrap_or(1.);
    let duration = v["format"]["duration"]
        .as_str()
        .unwrap_or("0")
        .parse::<f64>()
        .unwrap_or(0.);
    let meta = Meta {
        width: s["width"].as_u64().unwrap_or(0) as u32,
        height: s["height"].as_u64().unwrap_or(0) as u32,
        fps,
        frame_count: s["nb_frames"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or((duration * fps).round() as u64),
    };
    if meta.width == 0
        || meta.height == 0
        || !meta.fps.is_finite()
        || meta.fps <= 0.
        || meta.frame_count == 0
    {
        return Err(ApiError::bad("Clip has invalid video metadata"));
    }
    Ok(meta)
}
async fn detail(state: &AppState, id: &str) -> Result<Detail, ApiError> {
    let row = sqlx::query(
        "SELECT meta_json,tracks_json,review_json,settings_json,revision FROM box_clips WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Clip not found"))?;
    let meta = match row.get::<Option<String>, _>("meta_json") {
        Some(v) => serde_json::from_str(&v).map_err(ApiError::internal)?,
        None => {
            let m = probe(&source_path(state, id).await?).await?;
            sqlx::query("UPDATE box_clips SET meta_json=? WHERE id=?")
                .bind(serde_json::to_string(&m).map_err(ApiError::internal)?)
                .bind(id)
                .execute(&state.pool)
                .await
                .map_err(ApiError::internal)?;
            m
        }
    };
    let mut tracks: Vec<Track> =
        serde_json::from_str(&row.get::<String, _>("tracks_json")).map_err(ApiError::internal)?;
    let reviews: Vec<Choice> =
        serde_json::from_str(&row.get::<String, _>("review_json")).map_err(ApiError::internal)?;
    let choices: HashMap<_, _> = reviews.into_iter().map(|c| (c.id.clone(), c)).collect();
    for t in &mut tracks {
        if let Some(c) = choices.get(&t.id) {
            t.selected = c.selected;
            t.plate_text = c.plate_text.clone();
        }
    }
    Ok(Detail {
        clip: clip(state, id).await?,
        meta,
        tracks,
        settings: serde_json::from_str(&row.get::<String, _>("settings_json")).unwrap_or_default(),
        revision: row.get("revision"),
        job: latest_job(state, id).await?,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
) -> Result<Json<Vec<Clip>>, ApiError> {
    discover(&state).await?;
    sync_jobs(&state).await?;
    let ids = sqlx::query_scalar::<_, String>(
        "SELECT id FROM box_clips ORDER BY created_at_ms DESC,name",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let mut clips = Vec::new();
    for id in ids {
        clips.push(clip(&state, &id).await?);
    }
    Ok(Json(clips))
}
pub async fn get(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<Detail>, ApiError> {
    sync_jobs(&state).await?;
    Ok(Json(detail(&state, &id).await?))
}
pub async fn stream(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, ApiError> {
    media::serve_file(source_path(&state, &id).await?, &headers, &method).await
}

pub async fn frame(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
    Query(query): Query<FrameQuery>,
) -> Result<Response, ApiError> {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    let _slot = SLOTS
        .get_or_init(|| Semaphore::new(2))
        .acquire()
        .await
        .map_err(ApiError::internal)?;
    let d = detail(&state, &id).await?;
    let ms = query
        .timestamp_ms
        .unwrap_or(0)
        .min(d.clip.duration_ms.max(1) as u64 - 1);
    let path = source_path(&state, &id).await?;
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-threads",
            "2",
            "-ss",
            &format!("{:.6}", ms as f64 / 1000.),
            "-i",
        ])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "-threads",
            "2",
            "pipe:1",
        ])
        .output()
        .await
        .map_err(ApiError::internal)?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err(ApiError::bad("Could not decode this source frame"));
    }
    Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(output.stdout))
        .map_err(ApiError::internal)
}

fn validate_settings(s: &Settings) -> Result<(), ApiError> {
    if !s.min_confidence.is_finite()
        || !(0. ..=1.).contains(&s.min_confidence)
        || !s.jitter_px.is_finite()
        || !(0. ..=500.).contains(&s.jitter_px)
        || s.smoothing_radius > 30
        || s.minimum_frames == 0
        || s.minimum_frames > 100000
        || !s.max_area_fraction.is_finite()
        || !(0.00001..=1.).contains(&s.max_area_fraction)
        || !s.max_size_ratio.is_finite()
        || !(1. ..=100.).contains(&s.max_size_ratio)
    {
        return Err(ApiError::bad("Invalid box filter settings"));
    }
    Ok(())
}
pub async fn save(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
    Json(mut body): Json<SaveBody>,
) -> Result<Json<Detail>, ApiError> {
    validate_settings(&body.settings)?;
    let d = detail(&state, &id).await?;
    let valid: HashSet<_> = d.tracks.iter().map(|t| t.id.as_str()).collect();
    let mut seen = HashSet::new();
    for c in &mut body.tracks {
        c.plate_text = c
            .plate_text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();
        if !valid.contains(c.id.as_str())
            || !seen.insert(&c.id)
            || c.plate_text.len() > 64
            || !c
                .plate_text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
        {
            return Err(ApiError::bad("Invalid box ID or plate text"));
        }
    }
    if seen.len() != valid.len() {
        return Err(ApiError::bad("The review must contain every track ID"));
    }
    let update=sqlx::query("UPDATE box_clips SET review_json=?,settings_json=?,revision=revision+1 WHERE id=? AND revision=?")
        .bind(serde_json::to_string(&body.tracks).map_err(ApiError::internal)?).bind(serde_json::to_string(&body.settings).map_err(ApiError::internal)?).bind(&id).bind(body.expected_revision).execute(&state.pool).await.map_err(ApiError::internal)?;
    if update.rows_affected() == 0 {
        return Err(ApiError::conflict(
            "This clip was updated in another session. Reload before saving.",
        ));
    }
    Ok(Json(detail(&state, &id).await?))
}

fn sha256(path: &FsPath) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
async fn atomic_json(path: &FsPath, value: &impl Serialize) -> Result<(), ApiError> {
    let tmp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    tokio::fs::write(
        &tmp,
        serde_json::to_vec_pretty(value).map_err(ApiError::internal)?,
    )
    .await
    .map_err(ApiError::internal)?;
    tokio::fs::rename(tmp, path)
        .await
        .map_err(ApiError::internal)
}

pub async fn segment(
    State(state): State<AppState>,
    Extension(user): Extension<UserView>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    sync_jobs(&state).await?;
    Ok(Json(queue_clip(&state, &id, &user.id).await?))
}

async fn queue_clip(state: &AppState, id: &str, requested_by: &str) -> Result<Job, ApiError> {
    static ADMISSION: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ADMISSION.get_or_init(|| Mutex::new(())).lock().await;
    if let Some(job) = latest_job(state, id).await? {
        if matches!(job.status.as_str(), "queued" | "running" | "complete") {
            return Ok(job);
        }
    }
    let d = detail(&state, &id).await?;
    let path = source_path(&state, &id).await?;
    let source_hash = tokio::task::spawn_blocking(move || sha256(&path))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let relative =
        sqlx::query_scalar::<_, String>("SELECT relative_path FROM box_clips WHERE id=?")
            .bind(&id)
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::internal)?;
    let job_id = Uuid::new_v4().to_string();
    let dir = queue_root(&state).join(&job_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(ApiError::internal)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o2775))
            .await
            .map_err(ApiError::internal)?;
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::internal)?;
    sqlx::query("INSERT INTO box_jobs(id,clip_id,requested_by,status,source_hash,created_at_ms) VALUES(?,?,?,'queued',?,?)").bind(&job_id).bind(id).bind(requested_by).bind(&source_hash).bind(now_ms()).execute(&mut *tx).await.map_err(|e|ApiError::conflict(format!("Cannot queue clip: {e}")))?;
    sqlx::query("UPDATE box_clips SET source_hash=? WHERE id=?")
        .bind(&source_hash)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::internal)?;
    let request = serde_json::json!({"schema":"plate-sam-job/v1","job_id":job_id,"clip_id":id,"source_relative":relative,"source_sha256":source_hash,"meta":d.meta,"created_at_ms":now_ms(),"ocr_enabled":false});
    atomic_json(&dir.join("request.json"), &request).await?;
    tx.commit().await.map_err(ApiError::internal)?;
    tokio::fs::write(dir.join("READY"), b"")
        .await
        .map_err(ApiError::internal)?;
    latest_job(state, id)
        .await?
        .ok_or_else(|| ApiError::bad("Job was not queued"))
}

async fn sync_jobs(state: &AppState) -> Result<(), ApiError> {
    static SYNC: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = SYNC.get_or_init(|| Mutex::new(())).lock().await;
    let jobs = sqlx::query(
        "SELECT id,clip_id,source_hash FROM box_jobs WHERE status IN ('queued','running')",
    )
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    for row in jobs {
        let id: String = row.get("id");
        let clip_id: String = row.get("clip_id");
        let expected: String = row.get("source_hash");
        let dir = queue_root(state).join(&id);
        // Recover a committed job interrupted between its DB commit and ready marker.
        if dir.join("request.json").exists() && !dir.join("READY").exists() {
            tokio::fs::write(dir.join("READY"), b"")
                .await
                .map_err(ApiError::internal)?;
        }
        let Ok(bytes) = tokio::fs::read(dir.join("status.json")).await else {
            continue;
        };
        let Ok(mut status) = serde_json::from_slice::<Job>(&bytes) else {
            continue;
        };
        status.progress = if status.progress.is_finite() {
            status.progress.clamp(0., 1.)
        } else {
            0.
        };
        if !["queued", "running", "complete", "failed"].contains(&status.status.as_str()) {
            continue;
        }
        if status.status == "complete" {
            let parsed = match tokio::fs::read(dir.join("proposals.json")).await {
                Ok(bytes) => serde_json::from_slice::<Proposal>(&bytes).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let validated = parsed.and_then(|proposal| {
                validate_proposal(&proposal, &expected)?;
                Ok(proposal)
            });
            match validated {
                Err(message) => {
                    status.status = "failed".into();
                    status.phase = "failed".into();
                    status.error = Some(message);
                }
                Ok(proposal) => {
                    let raw: Vec<Track> = proposal
                        .tracks
                        .into_iter()
                        .map(|mut t| {
                            t.selected = false;
                            t.plate_text.clear();
                            t
                        })
                        .collect();
                    sqlx::query("UPDATE box_clips SET tracks_json=?,meta_json=?,review_json='[]',revision=revision+1 WHERE id=?")
                        .bind(serde_json::to_string(&raw).map_err(ApiError::internal)?).bind(serde_json::to_string(&proposal.meta).map_err(ApiError::internal)?).bind(&clip_id).execute(&state.pool).await.map_err(ApiError::internal)?;
                }
            }
        }

        let terminal = matches!(status.status.as_str(), "complete" | "failed");
        sqlx::query("UPDATE box_jobs SET status=?,progress=?,phase=?,logs_json=?,error=?,ended_at_ms=? WHERE id=? AND status IN ('queued','running')")
            .bind(status.status).bind(status.progress).bind(status.phase).bind(serde_json::to_string(&status.logs).map_err(ApiError::internal)?).bind(status.error).bind(if terminal{Some(now_ms())}else{None}).bind(id).execute(&state.pool).await.map_err(ApiError::internal)?;
    }
    Ok(())
}
fn validate_proposal(p: &Proposal, hash: &str) -> Result<(), String> {
    if p.source_sha256 != hash {
        return Err("Source checksum differs from queued clip".into());
    }
    if p.meta.width == 0
        || p.meta.height == 0
        || p.processed_frames != p.meta.frame_count
        || p.meta.frame_count == 0
        || !p.meta.fps.is_finite()
        || p.meta.fps <= 0.
    {
        return Err("SAM did not cover the complete clip timeline".into());
    }
    let mut ids = HashSet::new();
    for t in &p.tracks {
        if t.id.is_empty() || !ids.insert(&t.id) {
            return Err("Duplicate or empty SAM box ID".into());
        }
        let mut frames = HashSet::new();
        for d in &t.detections {
            if d.frame >= p.meta.frame_count
                || !frames.insert(d.frame)
                || ![d.x_min, d.y_min, d.x_max, d.y_max, d.confidence]
                    .iter()
                    .all(|x| x.is_finite())
                || !(0. ..=1.).contains(&d.confidence)
                || d.x_min < 0.
                || d.y_min < 0.
                || d.x_max > p.meta.width as f64
                || d.y_max > p.meta.height as f64
                || d.x_max <= d.x_min
                || d.y_max <= d.y_min
            {
                return Err(format!("Invalid SAM coordinates in box {}", t.id));
            }
        }
    }
    Ok(())
}

async fn admit_new_clips(state: &AppState) -> Result<(), ApiError> {
    if queue_root(state).join("PAUSED").exists()
        || std::env::var("PLATE_REVIEW_AUTO_SEGMENT").is_ok_and(|v| v == "false")
    {
        return Ok(());
    }
    // Admit one clip at a time; this bounds CPU probing/hashing without loading SAM.
    // Failed jobs require an explicit retry and are never silently looped forever.
    let next = sqlx::query("SELECT c.id,e.requested_by FROM box_clips c JOIN export_jobs e ON e.id=c.export_job_id WHERE NOT EXISTS (SELECT 1 FROM box_jobs j WHERE j.clip_id=c.id) ORDER BY c.created_at_ms,c.name LIMIT 1")
        .fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
    if let Some(row) = next {
        queue_clip(
            state,
            &row.get::<String, _>("id"),
            &row.get::<String, _>("requested_by"),
        )
        .await?;
    }
    Ok(())
}

pub async fn worker(state: AppState) {
    let mut discovery_tick = 0;
    loop {
        if discovery_tick == 0 {
            if let Err(e) = discover(&state).await {
                tracing::error!(error=%e.message,"Clip discovery failed");
            }
        }
        discovery_tick = (discovery_tick + 1) % 15;
        if let Err(e) = sync_jobs(&state).await {
            tracing::error!(error=%e.message,"SAM status synchronization failed");
        }
        if let Err(e) = admit_new_clips(&state).await {
            tracing::error!(error=%e.message,"Automatic SAM admission failed");
        }
        sleep(Duration::from_secs(2)).await;
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}
pub fn filtered(t: &Track, s: &Settings, m: &Meta) -> Vec<Detection> {
    let mut raw: Vec<_> = t
        .detections
        .iter()
        .filter(|d| {
            d.confidence >= s.min_confidence
                && (d.x_max - d.x_min) * (d.y_max - d.y_min)
                    <= m.width as f64 * m.height as f64 * s.max_area_fraction
        })
        .cloned()
        .collect();
    raw.sort_by_key(|d| d.frame);
    if raw.len() < s.minimum_frames {
        return vec![];
    }
    let mut result = Vec::new();
    for d in &raw {
        let near: Vec<_> = raw
            .iter()
            .filter(|x| x.frame.abs_diff(d.frame) <= s.smoothing_radius)
            .collect();
        let bx = [d.x_min, d.y_min, d.x_max - d.x_min, d.y_max - d.y_min];
        let med = [
            median(near.iter().map(|x| x.x_min).collect()),
            median(near.iter().map(|x| x.y_min).collect()),
            median(near.iter().map(|x| x.x_max - x.x_min).collect()),
            median(near.iter().map(|x| x.y_max - x.y_min).collect()),
        ];
        if near.len() >= 3
            && (bx[2] > med[2] * s.max_size_ratio
                || bx[3] > med[3] * s.max_size_ratio
                || bx[2] * s.max_size_ratio < med[2]
                || bx[3] * s.max_size_ratio < med[3])
        {
            continue;
        }
        let mut box_out = d.clone();
        if s.smoothing_radius > 0
            && s.jitter_px > 0.
            && bx
                .iter()
                .zip(med)
                .all(|(a, b)| (a - b).abs() <= s.jitter_px)
        {
            box_out.x_min = med[0];
            box_out.y_min = med[1];
            box_out.x_max = med[0] + med[2];
            box_out.y_max = med[1] + med[3];
        }
        result.push(box_out);
    }
    if result.len() < s.minimum_frames {
        vec![]
    } else {
        result
    }
}

pub async fn export(
    State(state): State<AppState>,
    Extension(_user): Extension<UserView>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let d = detail(&state, &id).await?;
    let source = source_path(&state, &id).await?;
    let chosen: Vec<_> = d
        .tracks
        .iter()
        .filter(|t| t.selected && !t.plate_text.trim().is_empty())
        .map(|t| {
            (
                t.id.clone(),
                t.plate_text.clone(),
                filtered(t, &d.settings, &d.meta),
            )
        })
        .filter(|(_, _, ds)| !ds.is_empty())
        .collect();
    if chosen.is_empty() {
        return Err(ApiError::bad(
            "Select at least one box, enter its plate text, and save before export",
        ));
    }
    let root = std::env::var_os("PLATE_REVIEW_BOX_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state.settings.state_dir.join("boxes"))
        .join("datasets");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(ApiError::internal)?;
    let destination = root.join(format!("{}.zip", Uuid::new_v4()));
    let dest = destination.clone();
    let video_id = id.clone();
    tokio::task::spawn_blocking(move||->anyhow::Result<()>{
        let mut csv=csv::Writer::from_writer(Vec::new());csv.write_record(["video_id","box_id","timestamp_ms","x_min","y_min","x_max","y_max","plate_text"])?;
        for (box_id,plate_text,ds) in chosen{for b in ds{csv.serialize((&video_id,&box_id,b.timestamp_ms,b.x_min,b.y_min,b.x_max,b.y_max,&plate_text))?;}}
        let options=zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let file=std::fs::File::create(&dest)?;let mut zip=zip::ZipWriter::new(file);
        zip.start_file("annotations.csv",options)?;zip.write_all(&csv.into_inner()?)?;
        zip.start_file(format!("{video_id}.mp4"),options)?;std::io::copy(&mut std::fs::File::open(&source)?,&mut zip)?;
        zip.start_file("manifest.json",options)?;zip.write_all(&serde_json::to_vec_pretty(&serde_json::json!({"schema":"plate-box-dataset/v1","video_id":video_id,"source_video_id":d.clip.source_video_id,"source_name":d.clip.name,"raw_video":"byte-for-byte original exported clip","meta":d.meta,"settings":d.settings,"review_revision":d.revision}))?)?;
        zip.finish()?;Ok(())
    }).await.map_err(ApiError::internal)?.map_err(ApiError::internal)?;
    let mut response = media::serve_file(destination.clone(), &headers, &Method::GET).await?;
    // Linux keeps the open streaming file descriptor valid after unlinking.
    let _ = tokio::fs::remove_file(&destination).await;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{id}.dataset.zip\""))
            .map_err(ApiError::internal)?,
    );
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn det(frame: u64, x: f64, w: f64) -> Detection {
        Detection {
            frame,
            timestamp_ms: frame * 40,
            x_min: x,
            y_min: 100.,
            x_max: x + w,
            y_max: 120.,
            confidence: 0.9,
        }
    }
    #[test]
    fn filters_confidence_area_and_single_frame_size_spikes() {
        let m = Meta {
            width: 1920,
            height: 1080,
            fps: 25.,
            frame_count: 5,
        };
        let t = Track {
            id: "1".into(),
            selected: true,
            plate_text: "N 1 A".into(),
            detections: vec![
                det(0, 100., 30.),
                det(1, 101., 30.),
                det(2, 100., 400.),
                det(3, 102., 30.),
                det(4, 100., 30.),
            ],
        };
        let mut s = Settings::default();
        s.max_area_fraction = 1.;
        let ds = filtered(&t, &s, &m);
        assert_eq!(ds.len(), 4);
        assert!(!ds.iter().any(|d| d.frame == 2));
    }
    #[test]
    fn smoothes_small_jitter_without_inventing_frames() {
        let m = Meta {
            width: 1920,
            height: 1080,
            fps: 25.,
            frame_count: 9,
        };
        let t = Track {
            id: "1".into(),
            selected: true,
            plate_text: "N 1 A".into(),
            detections: vec![det(2, 100., 30.), det(3, 104., 30.), det(4, 101., 30.)],
        };
        let ds = filtered(&t, &Settings::default(), &m);
        assert_eq!(ds.len(), 3);
        assert_eq!(ds[1].x_min, 101.);
        assert_eq!(ds[0].frame, 2);
    }
    #[tokio::test]
    async fn dataset_export_has_exact_columns_selected_tracks_and_identical_raw_video() {
        use http_body_util::BodyExt;
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("source");
        let export_dir = temp.path().join("exports");
        let state_dir = temp.path().join("state");
        for p in [&source_dir, &export_dir, &state_dir] {
            std::fs::create_dir_all(p).unwrap();
        }
        let raw = b"immutable original source bytes";
        std::fs::write(export_dir.join("clip.mp4"), raw).unwrap();
        let pool = crate::db::connect(&state_dir.join("test.sqlite"))
            .await
            .unwrap();
        let meta = Meta {
            width: 1920,
            height: 1080,
            fps: 25.,
            frame_count: 5,
        };
        let tracks = vec![
            Track {
                id: "kept".into(),
                selected: false,
                plate_text: String::new(),
                detections: vec![det(0, 100., 30.), det(1, 101., 30.)],
            },
            Track {
                id: "excluded".into(),
                selected: false,
                plate_text: String::new(),
                detections: vec![det(0, 200., 30.), det(1, 201., 30.)],
            },
            Track {
                id: "blank".into(),
                selected: false,
                plate_text: String::new(),
                detections: vec![det(0, 300., 30.), det(1, 301., 30.)],
            },
        ];
        let choices = vec![
            Choice {
                id: "kept".into(),
                selected: true,
                plate_text: "N 5176 WP".into(),
            },
            Choice {
                id: "excluded".into(),
                selected: false,
                plate_text: "L 1234 AB".into(),
            },
            Choice {
                id: "blank".into(),
                selected: true,
                plate_text: String::new(),
            },
        ];
        sqlx::query("INSERT INTO box_clips(id,source_video_id,export_job_id,name,relative_path,bytes,duration_ms,meta_json,tracks_json,review_json,created_at_ms) VALUES('clip','recording','export','clip.mp4','clip.mp4',?,200,?,?,?,0)")
            .bind(raw.len() as i64).bind(serde_json::to_string(&meta).unwrap()).bind(serde_json::to_string(&tracks).unwrap()).bind(serde_json::to_string(&choices).unwrap()).execute(&pool).await.unwrap();
        let state = AppState {
            pool,
            settings: crate::config::Settings {
                source_dir,
                state_dir,
                export_dir,
                web_dir: temp.path().join("web"),
                bind: "127.0.0.1:0".parse().unwrap(),
                app_origin: "http://127.0.0.1".into(),
                secure_cookie: false,
                trust_proxy_headers: false,
            },
            login_limiter: crate::auth::LoginLimiter::default(),
        };
        // Automatic admission is idempotent and respects the same durable queue as manual jobs.
        sqlx::query("INSERT INTO users(id,username,password_hash,role,created_at_ms) VALUES('owner','owner','unused','reviewer',0)").execute(&state.pool).await.unwrap();
        sqlx::query("INSERT INTO export_jobs(id,requested_by,status,total_items,created_at_ms) VALUES('export','owner','complete',1,0)").execute(&state.pool).await.unwrap();
        admit_new_clips(&state).await.unwrap();
        admit_new_clips(&state).await.unwrap();
        let job = latest_job(&state, "clip").await.unwrap().unwrap();
        assert!(queue_root(&state).join(&job.id).join("READY").exists());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM box_jobs")
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            1
        );
        // A failed job is retained for explicit retry rather than repeatedly auto-admitted.
        sqlx::query("UPDATE box_jobs SET status='failed'")
            .execute(&state.pool)
            .await
            .unwrap();
        admit_new_clips(&state).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM box_jobs")
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            1
        );
        let response = export(
            State(state),
            Extension(UserView {
                id: "test".into(),
                username: "test".into(),
                role: "reviewer".into(),
            }),
            Path("clip".into()),
            HeaderMap::new(),
        )
        .await
        .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut csv_text = String::new();
        archive
            .by_name("annotations.csv")
            .unwrap()
            .read_to_string(&mut csv_text)
            .unwrap();
        let mut reader = csv::Reader::from_reader(csv_text.as_bytes());
        assert_eq!(
            reader.headers().unwrap().iter().collect::<Vec<_>>(),
            vec![
                "video_id",
                "box_id",
                "timestamp_ms",
                "x_min",
                "y_min",
                "x_max",
                "y_max",
                "plate_text"
            ]
        );
        let rows = reader.records().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| &row[1] == "kept" && &row[7] == "N 5176 WP")
        );
        let mut actual = Vec::new();
        archive
            .by_name("clip.mp4")
            .unwrap()
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, raw);
    }
}
