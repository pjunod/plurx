//! HLS transcode endpoints. `start` creates a session and returns the playlist
//! URL; the playlist and segments are then fetched by the client's HLS player
//! (hls.js on the web). Auth is by bearer header (hls.js sets it on every XHR)
//! or `?token=` on the start call.

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct StartQuery {
    /// Target height (e.g. 1080, 720). Defaults to 1080.
    pub height: Option<i64>,
    /// Start offset in seconds (resume / seek).
    pub start: Option<f64>,
}

#[derive(Serialize)]
pub struct StartResponse {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
    pub encoder: String,
}

/// GET /api/v1/files/:id/hls/start
pub async fn start(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<StartQuery>,
) -> Result<Json<StartResponse>, ApiError> {
    let height = q.height.unwrap_or(1080).clamp(144, 2160);
    let start = q.start.unwrap_or(0.0).max(0.0);
    let info = state
        .transcode
        .start(id, height, start)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(StartResponse {
        session_id: info.session_id,
        playlist_url: info.playlist_url,
        duration_ms: info.duration_ms,
        start_seconds: info.start_seconds,
        encoder: info.encoder.to_owned(),
    }))
}

/// GET /api/v1/hls/:session/index.m3u8
pub async fn playlist(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
) -> Result<Response, ApiError> {
    let bytes = state
        .transcode
        .playlist(&session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl".to_owned(),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// GET /api/v1/hls/:session/:segment
pub async fn segment(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath((session, seg)): AxPath<(String, String)>,
) -> Result<Response, ApiError> {
    let bytes = state
        .transcode
        .segment(&session, &seg)
        .await
        .ok_or(ApiError::NotFound("segment"))?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "video/mp2t".to_owned()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}
