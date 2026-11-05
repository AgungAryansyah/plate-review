use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::{auth::LoginLimiter, config::Settings};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub settings: Settings,
    pub login_limiter: LoginLimiter,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue")
    }
    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "You do not have access to this action",
        )
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "Request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The request failed. Check the server log.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Interval {
    pub id: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Unreviewed,
    InProgress,
    Reviewed,
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unreviewed => "unreviewed",
            Self::InProgress => "in_progress",
            Self::Reviewed => "reviewed",
        }
    }
}

impl TryFrom<&str> for ReviewStatus {
    type Error = ApiError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "unreviewed" => Ok(Self::Unreviewed),
            "in_progress" => Ok(Self::InProgress),
            "reviewed" => Ok(Self::Reviewed),
            _ => Err(ApiError::bad("Unknown review status")),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewSnapshot {
    pub revision: i64,
    pub status: ReviewStatus,
    pub intervals: Vec<Interval>,
    pub updated_by: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Video {
    pub id: String,
    pub source_path: String,
    pub camera_id: String,
    pub camera_name: String,
    pub session_id: String,
    pub session_started_at: String,
    pub session_kind: String,
    pub source_status: String,
    pub duration_ms: i64,
    pub width: i64,
    pub height: i64,
    pub codec: String,
    pub frame_rate: String,
    pub interruption_count: i64,
    pub review: ReviewSnapshot,
    pub claim: Option<ClaimView>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ClaimView {
    pub username: String,
    pub expires_at_ms: i64,
    pub mine: bool,
}

#[derive(Serialize, Clone)]
pub struct UserView {
    pub id: String,
    pub username: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct ClaimBody {
    pub client_id: String,
}

#[derive(Deserialize)]
pub struct SaveReviewBody {
    pub client_id: String,
    pub mutation_id: String,
    pub expected_revision: i64,
    pub status: String,
    pub intervals: Vec<Interval>,
}

#[derive(Deserialize)]
pub struct SavePositionBody {
    pub position_ms: i64,
    pub zoom: f64,
}

#[derive(Deserialize, Default)]
pub struct VideoFilters {
    pub status: Option<String>,
    pub camera: Option<String>,
    pub q: Option<String>,
}
