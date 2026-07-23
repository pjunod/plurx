//! Server identity, first-run setup, settings, and scan status.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
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
    /// True when an Android APK is published (so the web UI shows the download
    /// link on Android). See `web::android_apk_path`.
    pub android_app: bool,
}

/// GET /api/v1/server — public; drives the client's setup-vs-login decision.
pub async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfo>, ApiError> {
    let instance_id = state.store.instance_id().await?;
    let setup_required = state.store.count_users().await? == 0;
    let android_app = super::web::android_apk_path(&state.system.data_dir).is_some();
    Ok(Json(ServerInfo {
        name: state.server_name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        instance_id,
        uptime_seconds: state.started_at.elapsed().as_secs(),
        setup_required,
        android_app,
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

/// A client-side playback problem the browser reports back to the server.
///
/// Why this exists: when a browser refuses a stream — Safari rejecting a codec,
/// or a direct-play file it won't progressive-play — *nothing runs server-side
/// to fail*, so `Settings → Logs` stays empty and the failure is invisible
/// unless the user opens dev tools. Forwarding the browser's own error here puts
/// it in the same log the admin already reads. All fields optional so the client
/// can send only what's relevant; everything is length-clipped before logging.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct ClientLog {
    /// "error" | "warn" — anything but "error" logs at WARN.
    pub level: String,
    /// Short machine tag: "playback_failed" | "stream_rejected" | "hls_fatal" | "stall".
    pub event: String,
    /// Human-readable summary (e.g. "format not supported by this browser").
    pub message: String,
    /// Delivery path in play at the time: "direct_play" | "remux" | "transcode".
    pub method: Option<String>,
    /// `HTMLMediaElement.error.code` (1..=4), when the failure is a media error.
    pub code: Option<i64>,
    /// Title being played, for cross-referencing with the library.
    pub title: Option<String>,
    /// File id being played.
    pub file_id: Option<i64>,
    /// Source video codec the decision picked, e.g. "hevc" — the usual Safari culprit.
    pub vcodec: Option<String>,
    /// Stream URL (query/token stripped by the client).
    pub src: Option<String>,
    /// Extra detail (hls.js error type, stall verdict, …).
    pub detail: Option<String>,
    /// Browser label the client computed ("Safari" | "Chrome" | …).
    pub ua: Option<String>,
}

/// POST /api/v1/client-log — any signed-in user. Records one browser playback
/// error into the server log ring so it surfaces in `Settings → Logs`. Bounded
/// by per-field clipping (this is diagnostics, not an audit trail), and tagged
/// with the `plurxd::client` target so it's visibly a client report.
pub async fn client_log(_user: AuthUser, Json(ev): Json<ClientLog>) -> StatusCode {
    /// Trim and cap one field so a client can't spam oversized log lines.
    fn clip(s: &str, n: usize) -> String {
        let s = s.trim();
        match s.char_indices().nth(n) {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s.to_owned(),
        }
    }
    fn field(v: &Option<String>, n: usize) -> Option<String> {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip(s, n))
    }

    let event = {
        let e = clip(&ev.event, 40);
        if e.is_empty() {
            "event".to_owned()
        } else {
            e
        }
    };
    let mut line = match field(&ev.ua, 24) {
        Some(ua) => format!("client[{ua}] {event}"),
        None => format!("client {event}"),
    };
    if let Some(m) = field(&ev.method, 16) {
        line.push_str(&format!(" method={m}"));
    }
    if let Some(v) = field(&ev.vcodec, 16) {
        line.push_str(&format!(" vcodec={v}"));
    }
    if let Some(c) = ev.code {
        line.push_str(&format!(" code={c}"));
    }
    let msg = clip(&ev.message, 200);
    if !msg.is_empty() {
        line.push_str(&format!(": {msg}"));
    }
    if let Some(t) = field(&ev.title, 120) {
        line.push_str(&format!(" — {t}"));
    }
    if let Some(id) = ev.file_id {
        line.push_str(&format!(" file={id}"));
    }
    if let Some(s) = field(&ev.src, 160) {
        line.push_str(&format!(" src={s}"));
    }
    if let Some(d) = field(&ev.detail, 200) {
        line.push_str(&format!(" [{d}]"));
    }

    // Both WARN and ERROR clear the default `info` filter, so either shows in
    // the admin log without the operator touching PLURX_LOG.
    if ev.level.eq_ignore_ascii_case("error") {
        tracing::error!(target: "plurxd::client", "{line}");
    } else {
        tracing::warn!(target: "plurxd::client", "{line}");
    }
    StatusCode::NO_CONTENT
}

#[derive(Serialize)]
pub struct SettingsDto {
    pub tmdb_configured: bool,
    /// The stored TMDB key itself. This endpoint is admin-only and the key is
    /// low-sensitivity (read-only metadata), so the admin who set it can see
    /// and copy it back — the web UI masks it until clicked. Empty when unset.
    pub tmdb_api_key: String,
    pub omdb_configured: bool,
    /// The stored OMDb key (Rotten Tomatoes / Metacritic / IMDb ratings). Same
    /// admin-only, mask-until-clicked treatment as the TMDB key.
    pub omdb_api_key: String,
    /// Trakt app credentials (the admin's own API app), same treatment.
    pub trakt_configured: bool,
    pub trakt_client_id: String,
    pub trakt_client_secret: String,
    /// Playback language defaults (docs/FEATURES.md §7): ISO 639 codes and the
    /// subtitle mode "auto" | "always" | "off".
    pub default_audio_lang: String,
    pub default_sub_lang: String,
    pub sub_mode: String,
}

async fn settings_dto(state: &AppState) -> Result<SettingsDto, ApiError> {
    let tmdb_api_key = state
        .store
        .get_setting(keys::TMDB_API_KEY)
        .await?
        .unwrap_or_default();
    let omdb_api_key = state
        .store
        .get_setting(keys::OMDB_API_KEY)
        .await?
        .unwrap_or_default();
    let trakt_client_id = state
        .store
        .get_setting(keys::TRAKT_CLIENT_ID)
        .await?
        .unwrap_or_default();
    let trakt_client_secret = state
        .store
        .get_setting(keys::TRAKT_CLIENT_SECRET)
        .await?
        .unwrap_or_default();
    let prefs = state.transcode.lang_prefs().await;
    Ok(SettingsDto {
        tmdb_configured: !tmdb_api_key.is_empty(),
        tmdb_api_key,
        omdb_configured: !omdb_api_key.is_empty(),
        omdb_api_key,
        trakt_configured: !trakt_client_id.is_empty() && !trakt_client_secret.is_empty(),
        trakt_client_id,
        trakt_client_secret,
        default_audio_lang: prefs.audio_lang,
        default_sub_lang: prefs.sub_lang,
        sub_mode: prefs.sub_mode.as_str().to_owned(),
    })
}

/// GET /api/v1/settings (admin)
pub async fn get_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> Result<Json<SettingsDto>, ApiError> {
    Ok(Json(settings_dto(&state).await?))
}

#[derive(Deserialize)]
pub struct UpdateSettings {
    /// Set the TMDB API key. Empty string clears it. Absent leaves it as-is.
    pub tmdb_api_key: Option<String>,
    /// Set the OMDb API key. Empty string clears it. Absent leaves it as-is.
    pub omdb_api_key: Option<String>,
    /// Trakt app credentials; same empty-clears semantics.
    pub trakt_client_id: Option<String>,
    pub trakt_client_secret: Option<String>,
    /// Playback language defaults. ISO 639 codes ("eng"); mode is
    /// "auto" | "always" | "off".
    pub default_audio_lang: Option<String>,
    pub default_sub_lang: Option<String>,
    pub sub_mode: Option<String>,
}

/// PUT /api/v1/settings (admin)
pub async fn update_settings(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateSettings>,
) -> Result<Json<SettingsDto>, ApiError> {
    let pairs: [(&str, &Option<String>); 6] = [
        (keys::TMDB_API_KEY, &req.tmdb_api_key),
        (keys::OMDB_API_KEY, &req.omdb_api_key),
        (keys::TRAKT_CLIENT_ID, &req.trakt_client_id),
        (keys::TRAKT_CLIENT_SECRET, &req.trakt_client_secret),
        (keys::AUDIO_LANG, &req.default_audio_lang),
        (keys::SUB_LANG, &req.default_sub_lang),
    ];
    for (key, value) in pairs {
        if let Some(value) = value {
            state.store.put_setting(key, value.trim()).await?;
        }
    }
    if let Some(mode) = &req.sub_mode {
        // Normalize through the parser so only valid modes are stored.
        let mode = plurx_core::tracks::SubMode::parse(mode.trim()).as_str();
        state.store.put_setting(keys::SUB_MODE, mode).await?;
    }
    Ok(Json(settings_dto(&state).await?))
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

/// One thing the server is doing right now. Deliberately generic — future
/// task kinds (file moves, renames, backups) reuse the same shape and the
/// same global indicator in every client.
#[derive(Serialize)]
pub struct Activity {
    /// Machine-readable kind: "scan" | "enrich" | "stream" (more later).
    pub kind: &'static str,
    /// Short human label, e.g. "Scanning Movies".
    pub label: String,
    /// Optional detail, e.g. "412 of 3801 files".
    pub detail: Option<String>,
    /// 0–100 when a meaningful percentage exists.
    pub percent: Option<u8>,
}

/// GET /api/v1/activity — everything in flight, for the always-visible
/// indicator in the app header. Empty array = the server is idle.
pub async fn activity(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Activity>>, ApiError> {
    let mut activities = Vec::new();

    let names: HashMap<i64, String> = state
        .store
        .list_libraries()
        .await?
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let mut statuses: Vec<_> = state
        .jobs
        .all_statuses()
        .await
        .into_iter()
        .filter(|(_, s)| s.running)
        .collect();
    statuses.sort_by_key(|(id, _)| *id);
    for (id, status) in statuses {
        let name = names.get(&id).cloned().unwrap_or_else(|| format!("#{id}"));
        let enriching = status.phase.as_deref() == Some("enriching");
        let (kind, label) = if enriching {
            ("enrich", format!("Fetching metadata for {name}"))
        } else {
            ("scan", format!("Scanning {name}"))
        };
        let (detail, percent) = match status.progress.filter(|_| !enriching) {
            Some(p) if p.found > 0 => (
                Some(format!("{} of {} files", p.processed, p.found)),
                Some(((p.processed * 100 / p.found).min(100)) as u8),
            ),
            _ => (None, None),
        };
        activities.push(Activity {
            kind,
            label,
            detail,
            percent,
        });
    }

    let streams = state.transcode.active_sessions().await;
    if streams > 0 {
        activities.push(Activity {
            kind: "stream",
            label: if streams == 1 {
                "1 active stream".to_owned()
            } else {
                format!("{streams} active streams")
            },
            detail: None,
            percent: None,
        });
    }

    if let Some((label, detail)) = state.trakt.activity().await {
        activities.push(Activity {
            kind: "trakt",
            label,
            detail,
            percent: None,
        });
    }

    Ok(Json(activities))
}

/// GET /api/v1/activity/detail — the activity page: live playback sessions,
/// per-library scan state, and the Trakt sync story, all in one shape. Any
/// authenticated user may look (it's their household server); the stop action
/// below is admin-only.
pub async fn activity_detail(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let sessions = state.transcode.list_sessions().await;
    let names: HashMap<i64, String> = state
        .store
        .list_libraries()
        .await?
        .into_iter()
        .map(|l| (l.id, l.name))
        .collect();
    let scans: Vec<serde_json::Value> = state
        .jobs
        .all_statuses()
        .await
        .into_iter()
        .map(|(id, st)| {
            serde_json::json!({
                "library_id": id,
                "library": names.get(&id).cloned().unwrap_or_else(|| format!("#{id}")),
                "status": st,
            })
        })
        .collect();
    let trakt = state.trakt.status(0).await; // page shows server-wide state
    let linked = state
        .store
        .list_trakt_auth()
        .await?
        .into_iter()
        .next()
        .map(|a| {
            serde_json::json!({
                "trakt_username": a.trakt_username,
                "last_sync_at": (a.last_sync_at > 0).then_some(a.last_sync_at),
            })
        });
    Ok(Json(serde_json::json!({
        "sessions": sessions,
        "scans": scans,
        "trakt": {
            "configured": trakt.configured,
            "linked": linked,
            "syncing": trakt.syncing,
            "note": trakt.note,
        },
    })))
}

/// DELETE /api/v1/activity/sessions/:id (admin) — stop a transcode session.
pub async fn stop_session(
    _admin: AdminUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stopped = state.transcode.stop_session(&id).await;
    if !stopped {
        return Err(ApiError::NotFound("session"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
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
