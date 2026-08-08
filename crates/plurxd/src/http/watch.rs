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
    /// Unix seconds when an offline client observed this final state. Older
    /// states are ignored by the store instead of rewinding newer playback.
    #[serde(default)]
    pub recorded_at: Option<i64>,
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
    // Read before writing, so "already watched" and "just became watched"
    // are distinguishable — otherwise every beat after the crossing would
    // re-notify.
    let was_watched = state
        .store
        .watch_state(user.id, id)
        .await?
        .map(|w| w.watched)
        .unwrap_or(false);
    let watch = if req.recorded_at.is_some() {
        // Imported/offline facts carry their own ordering clock and are rare,
        // semantically complete writes rather than an active player's beat.
        state
            .store
            .put_progress_at(user.id, id, position, req.duration_ms, req.recorded_at)
            .await?
    } else {
        let update = state
            .progress
            .put(user.id, id, position, req.duration_ms)
            .await?;
        if !update.committed {
            tracing::trace!(user_id = user.id, item_id = id, "coalesced progress beat");
        }
        update.watch
    };
    let applied = req.recorded_at.is_none_or(|at| at >= watch.updated_at);
    // This beat is also the heartbeat for a direct play (`crate::delivery`).
    // It is the only signal that reaches the server from a player which has
    // stopped fetching: a viewer who paused with the rest of the film already
    // buffered makes no further range requests, and without this would drop
    // off the activity page while still sitting in front of it. In-memory and
    // synchronous — a hash lookup, not a store read — because every open
    // player in the house arrives here every few seconds.
    if req.recorded_at.is_none() {
        state.direct_plays.touch_item(user.id, id);
    }
    // Feed the Trakt scrobbler (fire-and-forget; a beat every ~5s while the
    // player is open, and the watched flip triggers the scrobble stop).
    let pct = match watch.duration_ms.filter(|d| *d > 0) {
        Some(dur) => (watch.position_ms as f64 / dur as f64 * 100.0).clamp(0.0, 100.0),
        None => 0.0,
    };
    if applied {
        state.trakt.on_progress(user.id, id, pct, watch.watched);
    }
    // The 95% crossing is what makes this the interesting hook: it is the
    // moment somebody finished something, without them pressing anything.
    // `put_progress` only flips `watched` on the crossing, so this fires
    // once per item rather than on every 5-second beat.
    if applied && watch.watched && !was_watched {
        state.watched.on_watched(user.id, id).await;
    }
    Ok(Json(watch.into()))
}

/// POST /api/v1/items/:id/scrobble — mark watched. On a show, season, or
/// folder this marks every episode underneath: the container is a name for its
/// children, and marking only the container would leave Next Up cheerfully
/// offering episode one of a series you just said you'd seen.
pub async fn scrobble(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_item(id).await?.is_none() {
        return Err(ApiError::NotFound("item"));
    }
    let changed = state.store.set_watched_tree(user.id, id, true).await?;
    // Notify per episode that actually flipped. Re-marking a finished series
    // changes nothing and so says nothing — the alternative is re-announcing
    // forty episodes every time somebody clicks the button twice.
    for item in &changed {
        state.watched.on_watched(user.id, *item).await;
    }
    state.trakt.request_sync(); // propagate the manual mark promptly
    Ok(Json(
        serde_json::json!({ "ok": true, "updated": changed.len() }),
    ))
}

/// POST /api/v1/items/:id/unscrobble — mark unwatched (clears progress).
/// Cascades the same way `scrobble` does.
pub async fn unscrobble(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_item(id).await?.is_none() {
        return Err(ApiError::NotFound("item"));
    }
    let changed = state.store.set_watched_tree(user.id, id, false).await?;
    state.trakt.request_sync(); // an explicit un-watch removes on Trakt too
    Ok(Json(
        serde_json::json!({ "ok": true, "updated": changed.len() }),
    ))
}
