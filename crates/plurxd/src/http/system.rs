//! Server identity, first-run setup, settings, and scan status.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::Json;
use plurx_core::auth;
use plurx_core::store::keys;
use serde::{Deserialize, Serialize};

use super::auth::LoginResponse;
use super::error::ApiError;
use super::extract::{AdminUser, AuthUser};
use crate::state::{AppState, ScanStatus};

#[derive(Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: &'static str,
    pub instance_id: String,
    pub uptime_seconds: u64,
    /// True when no users exist yet — the web app shows first-run setup.
    pub setup_required: bool,
}

/// GET /api/v1/server — public; drives the client's setup-vs-login decision.
pub async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfo>, ApiError> {
    let instance_id = state.store.instance_id().await?;
    let setup_required = state.store.count_users().await? == 0;
    Ok(Json(ServerInfo {
        name: state.server_name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        instance_id,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        setup_required,
    }))
}

#[derive(Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

/// POST /api/v1/setup — create the first (admin) user. Allowed only while no
/// users exist; auto-logs-in on success.
pub async fn setup(
    State(state): State<AppState>,
    Json(req): Json<SetupRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    if state.store.count_users().await? > 0 {
        return Err(ApiError::Conflict("setup already completed".into()));
    }
    if req.username.trim().is_empty() || req.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "username required and password must be at least 8 characters".into(),
        ));
    }
    let hash = auth::hash_password(&req.password).map_err(|e| ApiError::Internal(e.to_string()))?;
    let user = state
        .store
        .create_user(req.username.trim(), &hash, true)
        .await?;

    let token = auth::generate_token().map_err(|e| ApiError::Internal(e.to_string()))?;
    let token_hash = auth::hash_token(&token);
    state
        .store
        .create_token(&token_hash, user.id, Some("setup"))
        .await?;
    Ok(Json(LoginResponse {
        token,
        user: user.into(),
    }))
}

#[derive(Serialize)]
pub struct SystemDto {
    pub name: String,
    pub version: &'static str,
    pub instance_id: String,
    pub uptime_seconds: u64,
    pub users: i64,
    pub libraries: usize,
    pub active_transcodes: usize,
    #[serde(flatten)]
    pub info: crate::state::SystemInfo,
}

/// GET /api/v1/system (admin) — environment diagnostics for the settings
/// page: paths, ffmpeg, detected encoders, counts.
pub async fn system_info(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<SystemDto>, ApiError> {
    Ok(Json(SystemDto {
        name: state.server_name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        instance_id: state.store.instance_id().await?,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        users: state.store.count_users().await?,
        libraries: state.store.list_libraries().await?.len(),
        active_transcodes: state.transcode.active_sessions().await,
        info: (*state.system).clone(),
    }))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    /// Minimum severity to include ("error" … "trace"). Default: everything
    /// the server's log filter captured.
    #[serde(default = "default_log_level")]
    pub level: String,
    #[serde(default = "default_log_limit")]
    pub limit: usize,
}

fn default_log_level() -> String {
    "trace".to_owned()
}
fn default_log_limit() -> usize {
    500
}

/// GET /api/v1/system/logs (admin) — recent log lines, oldest first.
pub async fn logs(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> Json<Vec<crate::logbuf::LogEntry>> {
    Json(state.logs.tail(&q.level, q.limit.min(2000)))
}

#[derive(Serialize)]
pub struct SettingsDto {
    pub tmdb_configured: bool,
}

/// GET /api/v1/settings (admin)
pub async fn get_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<SettingsDto>, ApiError> {
    let tmdb_configured = state
        .store
        .get_setting(keys::TMDB_API_KEY)
        .await?
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    Ok(Json(SettingsDto { tmdb_configured }))
}

#[derive(Deserialize)]
pub struct UpdateSettings {
    /// Set the TMDB API key. Empty string clears it. Absent leaves it as-is.
    pub tmdb_api_key: Option<String>,
}

/// PUT /api/v1/settings (admin)
pub async fn update_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsDto>, ApiError> {
    if let Some(key) = req.tmdb_api_key {
        state
            .store
            .put_setting(keys::TMDB_API_KEY, key.trim())
            .await?;
    }
    let tmdb_configured = state
        .store
        .get_setting(keys::TMDB_API_KEY)
        .await?
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    Ok(Json(SettingsDto { tmdb_configured }))
}

/// GET /api/v1/scan/status — per-library scan status (keyed by library id).
/// Any authenticated user may look; scans aren't a secret, but strangers
/// shouldn't see filesystem paths in problem messages.
pub async fn scan_status(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Json<HashMap<i64, ScanStatus>> {
    Json(state.jobs.all_statuses().await)
}

/// GET /metrics — Prometheus text exposition (unauthenticated; counts only).
pub async fn metrics(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let uptime = state.started_at.elapsed().as_secs();
    let sessions = state.transcode.active_sessions().await;
    let libraries = state
        .store
        .list_libraries()
        .await
        .map(|l| l.len())
        .unwrap_or(0);
    let users = state.store.count_users().await.unwrap_or(0);

    let body = format!(
        "# HELP plurx_build_info Build information.\n\
         # TYPE plurx_build_info gauge\n\
         plurx_build_info{{version=\"{version}\"}} 1\n\
         # HELP plurx_uptime_seconds Seconds since this node started.\n\
         # TYPE plurx_uptime_seconds gauge\n\
         plurx_uptime_seconds {uptime}\n\
         # HELP plurx_transcode_sessions_active Live transcode sessions.\n\
         # TYPE plurx_transcode_sessions_active gauge\n\
         plurx_transcode_sessions_active {sessions}\n\
         # HELP plurx_libraries_total Configured libraries.\n\
         # TYPE plurx_libraries_total gauge\n\
         plurx_libraries_total {libraries}\n\
         # HELP plurx_users_total Registered users.\n\
         # TYPE plurx_users_total gauge\n\
         plurx_users_total {users}\n",
        version = env!("CARGO_PKG_VERSION"),
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}
