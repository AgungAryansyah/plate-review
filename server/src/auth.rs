use crate::{
    api::{ApiError, AppState, LoginBody, UserView},
    db::now_ms,
};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const COOKIE: &str = "plate_review_session";
const SESSION_MS: i64 = 12 * 60 * 60 * 1000;
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Default)]
pub struct LoginLimiter(Arc<Mutex<HashMap<String, Vec<Instant>>>>);

impl LoginLimiter {
    fn blocked(&self, key: &str) -> bool {
        let mut entries = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let attempts = entries.entry(key.to_owned()).or_default();
        attempts.retain(|at| now.duration_since(*at) < FAILURE_WINDOW);
        attempts.len() >= MAX_FAILURES
    }
    fn failure(&self, key: &str) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(key.to_owned())
            .or_default()
            .push(Instant::now());
    }
    fn success(&self, key: &str) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key);
    }
}

#[derive(Serialize)]
struct LoginResponse {
    user: UserView,
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    if password.chars().count() < 12 || password.len() > 1024 {
        anyhow::bail!("Password must contain 12 to 1024 characters");
    }
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .to_string())
}

fn password_matches(encoded: &str, candidate: &str) -> bool {
    PasswordHash::new(encoded).ok().is_some_and(|hash| {
        Argon2::default()
            .verify_password(candidate.as_bytes(), &hash)
            .is_ok()
    })
}

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    role: &str,
    password: &str,
) -> anyhow::Result<()> {
    let username = username.trim();
    if username.len() < 3
        || username.len() > 48
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!("Username must use 3 to 48 letters, numbers, dots, underscores, or dashes");
    }
    if !matches!(role, "admin" | "reviewer") {
        anyhow::bail!("Role must be admin or reviewer");
    }
    let hash = hash_password(password)?;
    if role != "admin"
        && sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM users WHERE role = 'admin' AND disabled = 0",
        )
        .fetch_one(pool)
        .await?
            == 0
    {
        anyhow::bail!("Create the first active account with --role admin");
    }
    sqlx::query("INSERT INTO users (id, username, password_hash, role, created_at_ms) VALUES (?, ?, ?, ?, ?)")
        .bind(Uuid::new_v4().to_string()).bind(username).bind(hash).bind(role).bind(now_ms()).execute(pool).await?;
    Ok(())
}

pub async fn reset_password(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> anyhow::Result<()> {
    let hash = hash_password(password)?;
    let result =
        sqlx::query("UPDATE users SET password_hash = ? WHERE username = ? COLLATE NOCASE")
            .bind(hash)
            .bind(username.trim())
            .execute(pool)
            .await?;
    if result.rows_affected() == 0 {
        anyhow::bail!("Unknown username");
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = ? COLLATE NOCASE)").bind(username.trim()).execute(pool).await?;
    Ok(())
}

pub async fn disable_user(pool: &SqlitePool, username: &str) -> anyhow::Result<()> {
    let result = sqlx::query("UPDATE users SET disabled = 1 WHERE username = ? COLLATE NOCASE")
        .bind(username.trim())
        .execute(pool)
        .await?;
    if result.rows_affected() == 0 {
        anyhow::bail!("Unknown username");
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = ? COLLATE NOCASE)").bind(username.trim()).execute(pool).await?;
    Ok(())
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiError> {
    let key = client_key(&state, &headers, peer.ip().to_string());
    if state.login_limiter.blocked(&key) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many sign-in attempts. Try again later.",
        ));
    }
    if body.password.len() > 1024 {
        state.login_limiter.failure(&key);
        return Err(ApiError::unauthorized());
    }
    let user = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT id, username, password_hash, disabled FROM users WHERE username = ? COLLATE NOCASE",
    )
    .bind(body.username.trim())
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::internal)?;
    let Some((id, username, hash, disabled)) = user else {
        state.login_limiter.failure(&key);
        return Err(ApiError::unauthorized());
    };
    if disabled != 0 || !password_matches(&hash, &body.password) {
        state.login_limiter.failure(&key);
        return Err(ApiError::unauthorized());
    }
    state.login_limiter.success(&key);
    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(&id)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let token = Uuid::new_v4().simple().to_string();
    let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
    let now = now_ms();
    sqlx::query("INSERT INTO sessions (token_hash, user_id, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?)")
        .bind(token_hash).bind(&id).bind(now).bind(now + SESSION_MS).execute(&state.pool).await.map_err(ApiError::internal)?;
    let cookie = session_cookie(&token, state.settings.secure_cookie, false);
    let mut response = (
        StatusCode::OK,
        Json(LoginResponse {
            user: UserView { id, username, role },
        }),
    )
        .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    Ok(response)
}

pub async fn require_user(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = session_token(request.headers()).ok_or_else(ApiError::unauthorized)?;
    let token_hash = blake3::hash(token.as_bytes()).to_hex().to_string();
    let now = now_ms();
    let user = sqlx::query_as::<_, (String, String, String)>("SELECT u.id, u.username, u.role FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ? AND s.expires_at_ms > ? AND u.disabled = 0")
        .bind(token_hash).bind(now).fetch_optional(&state.pool).await.map_err(ApiError::internal)?;
    let Some((id, username, role)) = user else {
        return Err(ApiError::unauthorized());
    };
    request
        .extensions_mut()
        .insert(UserView { id, username, role });
    Ok(next.run(request).await)
}

pub async fn check_origin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        if origin != Some(state.settings.app_origin.as_str()) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Request origin is not allowed",
            ));
        }
    }
    Ok(next.run(request).await)
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some(token) = session_token(&headers) {
        let hash = blake3::hash(token.as_bytes()).to_hex().to_string();
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(hash)
            .execute(&state.pool)
            .await
            .map_err(ApiError::internal)?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie("", state.settings.secure_cookie, true))
            .map_err(ApiError::internal)?,
    );
    Ok(response)
}

pub async fn me(
    axum::extract::Extension(user): axum::extract::Extension<UserView>,
) -> Json<UserView> {
    Json(user)
}

pub fn require_admin(user: &UserView) -> Result<(), ApiError> {
    if user.role == "admin" {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| (name == COOKIE).then_some(value))
}

fn session_cookie(token: &str, secure_cookie: bool, clear: bool) -> String {
    let secure = if secure_cookie { "; Secure" } else { "" };
    if clear {
        format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}")
    } else {
        format!(
            "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
            SESSION_MS / 1000,
            secure
        )
    }
}

fn client_key(state: &AppState, headers: &HeaderMap, peer: String) -> String {
    if state.settings.trust_proxy_headers
        && let Some(value) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
    {
        return value.trim().to_owned();
    }
    peer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_are_argon2_and_reject_short_passwords() {
        assert!(hash_password("too-short").is_err());
        let encoded = hash_password("correct horse battery").unwrap();
        assert!(encoded.starts_with("$argon2id$"));
        assert!(password_matches(&encoded, "correct horse battery"));
        assert!(!password_matches(&encoded, "wrong password"));
    }

    #[test]
    fn sessions_use_secure_cookie_flags_when_configured() {
        let cookie = session_cookie("token", true, false);
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
        assert!(!cookie.contains(";;"));
    }
}
