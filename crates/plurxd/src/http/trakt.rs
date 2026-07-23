//! Trakt endpoints: status, device-code linking, and manual sync.
//!
//! Admin-gated for v1 — the server owner's account is the one that links
//! (docs/FEATURES.md §9). Per-user linking is a UI change away: the manager
//! and store already key everything by user id.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use super::error::ApiError;
use super::extract::AdminUser;
use crate::state::AppState;

#[derive(Serialize)]
pub struct PendingDto {
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: i64,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct TraktStatusDto {
    /// Client id + secret are saved (the integration can be used).
    pub configured: bool,
    pub linked: bool,
    pub trakt_username: Option<String>,
    pub connected_at: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub syncing: bool,
    /// Last sync summary or error, human-shaped.
    pub note: Option<String>,
    /// A device-code link in flight (show the code, keep polling status).
    pub pending: Option<PendingDto>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn status_dto(state: &AppState, user_id: i64) -> TraktStatusDto {
    let st = state.trakt.status(user_id).await;
    TraktStatusDto {
        configured: st.configured,
        linked: st.auth.is_some(),
        trakt_username: st.auth.as_ref().and_then(|a| a.trakt_username.clone()),
        connected_at: st.auth.as_ref().map(|a| a.connected_at),
        last_sync_at: st.auth.as_ref().map(|a| a.last_sync_at).filter(|t| *t > 0),
        syncing: st.syncing,
        note: st.note,
        pending: st.pending.map(|p| PendingDto {
            user_code: p.user_code,
            verification_url: p.verification_url,
            expires_in: (p.expires_at - now_unix()).max(0),
            error: p.error,
        }),
    }
}

/// GET /api/v1/trakt/status (admin)
pub async fn status(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<TraktStatusDto>, ApiError> {
    Ok(Json(status_dto(&state, user.id).await))
}

/// POST /api/v1/trakt/link (admin) — begin the device-code flow.
pub async fn link(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<TraktStatusDto>, ApiError> {
    state
        .trakt
        .link_start(user.id)
        .await
        .map_err(ApiError::Conflict)?;
    Ok(Json(status_dto(&state, user.id).await))
}

/// DELETE /api/v1/trakt/link (admin) — disconnect the account.
pub async fn unlink(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<TraktStatusDto>, ApiError> {
    state
        .trakt
        .unlink(user.id)
        .await
        .map_err(ApiError::Internal)?;
    Ok(Json(status_dto(&state, user.id).await))
}

/// POST /api/v1/trakt/sync (admin) — run a sync now.
pub async fn sync_now(
    AdminUser(user): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<TraktStatusDto>, ApiError> {
    state.trakt.request_sync();
    Ok(Json(status_dto(&state, user.id).await))
}
