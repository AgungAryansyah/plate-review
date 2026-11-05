mod api;
mod auth;
mod catalog;
mod config;
mod db;
mod exports;
mod media;
mod review;

use anyhow::{Context, Result};
use axum::{
    Router, middleware,
    routing::{get, post, put},
};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

use crate::{api::AppState, auth::LoginLimiter, config::Settings};

#[derive(Parser)]
#[command(name = "plate-review", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    CreateUser {
        username: String,
        #[arg(long, default_value = "reviewer")]
        role: String,
    },
    ResetPassword {
        username: String,
    },
    DisableUser {
        username: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::CreateUser { username, role } => user_command(&username, Some(&role), false).await,
        Command::ResetPassword { username } => user_command(&username, None, true).await,
        Command::DisableUser { username } => disable_user(&username).await,
    }
}

async fn user_command(username: &str, role: Option<&str>, reset: bool) -> Result<()> {
    let pool = admin_database().await?;
    let password = rpassword::prompt_password("Password (minimum 12 characters): ")?;
    if reset {
        auth::reset_password(&pool, username, &password).await?;
    } else {
        auth::create_user(&pool, username, role.unwrap_or("reviewer"), &password).await?;
    }
    println!("Account updated.");
    Ok(())
}

async fn disable_user(username: &str) -> Result<()> {
    let pool = admin_database().await?;
    auth::disable_user(&pool, username).await?;
    println!("Account disabled.");
    Ok(())
}

async fn admin_database() -> Result<sqlx::SqlitePool> {
    let path = config::database_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    db::connect(&path).await
}

async fn serve() -> Result<()> {
    let settings = Settings::from_env()?;
    let database = settings.ensure_directories()?;
    let pool = db::connect(&database)
        .await
        .context("Cannot open application database")?;
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE disabled = 0")
        .fetch_one(&pool)
        .await?
        == 0
    {
        tracing::warn!(
            "No active users exist. Run `plate-review create-user <name> --role admin` before opening the app."
        );
    }
    let summary = catalog::scan(&settings, &pool)
        .await
        .context("Cannot index source videos")?;
    tracing::info!(
        phase = "catalog",
        files = summary.files_checked,
        available = summary.available,
        changed = summary.changed,
        missing = summary.missing,
        "Video catalog ready"
    );
    let state = AppState {
        pool,
        settings: settings.clone(),
        login_limiter: LoginLimiter::default(),
    };
    let api = Router::new()
        .route("/api/videos", get(review::list_videos))
        .route("/api/videos/{id}", get(review::get_video))
        .route(
            "/api/videos/{id}/claim",
            post(review::claim_video).delete(review::release_claim),
        )
        .route("/api/videos/{id}/claim/heartbeat", post(review::heartbeat))
        .route("/api/videos/{id}/review", put(review::save_review))
        .route("/api/videos/{id}/position", put(review::save_position))
        .route(
            "/api/videos/{id}/stream",
            get(media::stream).head(media::stream),
        )
        .route("/api/admin/scan", post(review::scan_library))
        .route(
            "/api/admin/videos/{id}/accept-source",
            post(review::accept_changed_source),
        )
        .route(
            "/api/exports",
            post(exports::create_job).get(exports::list_jobs),
        )
        .route("/api/exports/{id}", get(exports::get_job))
        .route("/api/exports/{id}/retry", post(exports::retry_job))
        .route(
            "/api/exports/{id}/manifest",
            get(exports::download_manifest),
        )
        .route(
            "/api/exports/{id}/files/{name}",
            get(exports::download_file).head(exports::download_file),
        )
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/me", get(auth::me))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_user,
        ))
        .with_state(state.clone());
    let app = Router::new()
        .route("/api/auth/login", post(auth::login))
        .route("/api/health", get(health))
        .nest_service(
            "/",
            ServeDir::new(&settings.web_dir).append_index_html_on_directories(true),
        )
        .merge(api)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::check_origin,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());
    tokio::spawn(exports::worker(state.clone()));
    let listener = TcpListener::bind(settings.bind)
        .await
        .with_context(|| format!("Cannot bind to {}", settings.bind))?;
    tracing::info!(address = %settings.bind, "Plate Review server is ready");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn health() -> &'static str {
    "ok"
}
