//! Watch-state endpoints: progress reporting and watched/unwatched marks.
//! In Phase 4 these writes replicate across the cluster (ARCHITECTURE §2.2);
//! the handler shape is unchanged.

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;

use super::dto::WatchDto;
use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ProgressRequest {
    pub position_ms: i64,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

/// POST /api/v1/items/:id/progress — report playback position. Crossing 95%
/// auto-marks the item watched (handled in the store).
pub async fn progress(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<ProgressRequest>,
) -> Result<Json<WatchDto>, ApiError> {
    if state.store.get_item(id).await?.is_none() {
        return Err(ApiError::NotFound("item"));
    }
    let position = req.position_ms.max(0);
    let watch = state
        .store
        .put_progress(user.id, id, position, req.duration_ms)
        .await?;
    // Feed the Trakt scrobbler (fire-and-forget; a beat every ~5s while the
    // player is open, and the watched flip triggers the scrobble stop).
    let pct = match watch.duration_ms.filter(|d| *d > 0) {
        Some(dur) => (watch.position_ms as f64 / dur as f64 * 100.0).clamp(0.0, 100.0),
        None => 0.0,
    };
    state.trakt.on_progress(user.id, id, pct, watch.watched);
    Ok(Json(watch.into()))
}

/// POST /api/v1/items/:id/scrobble — mark watched.
pub async fn scrobble(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_item(id).await?.is_none() {
        return Err(ApiError::NotFound("item"));
    }
    state.store.set_watched(user.id, id, true).await?;
    state.trakt.request_sync(); // propagate the manual mark promptly
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/v1/items/:id/unscrobble — mark unwatched (clears progress).
pub async fn unscrobble(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_item(id).await?.is_none() {
        return Err(ApiError::NotFound("item"));
    }
    state.store.set_watched(user.id, id, false).await?;
    state.trakt.request_sync(); // an explicit un-watch removes on Trakt too
    Ok(Json(serde_json::json!({ "ok": true })))
}
