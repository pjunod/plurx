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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Dirs, SystemInfo};
    use crate::trakt::TraktManager;
    use plurx_core::domain::TraktAuth;
    use plurx_core::store::{keys, SqliteStore, Store};
    use serde_json::json;
    use std::sync::Arc;

    async fn serve_device_code() -> String {
        let app = axum::Router::new().fallback(|| async {
            Json(json!({
                "device_code": "device-code",
                "user_code": "ABCD",
                "verification_url": "https://trakt.tv/activate",
                "expires_in": 600,
                "interval": 600
            }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Trakt fake");
        let addr = listener.local_addr().expect("Trakt fake address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn status_projection_preserves_link_and_pending_details() {
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store
            .create_user("admin", "hash", true)
            .await
            .expect("user");
        store
            .put_setting(keys::TRAKT_CLIENT_ID, "client")
            .await
            .expect("client id");
        store
            .put_setting(keys::TRAKT_CLIENT_SECRET, "secret")
            .await
            .expect("client secret");
        store
            .put_trakt_auth(&TraktAuth {
                user_id: user.id,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 3600,
                trakt_username: Some("neo".into()),
                connected_at: 123,
                last_sync_at: 456,
                last_activities: None,
            })
            .await
            .expect("linked auth");

        let temp = tempfile::tempdir().expect("state directory");
        let dirs = Dirs {
            artwork: temp.path().join("artwork"),
            transcode: temp.path().join("transcode"),
            cache: temp.path().join("cache"),
            subs: temp.path().join("subs"),
        };
        let mut state = AppState::new(
            "test".into(),
            Arc::clone(&store),
            dirs,
            "test-node".into(),
            Default::default(),
            SystemInfo::default(),
            Arc::new(crate::logbuf::LogBuffer::new(8)),
        );
        state.trakt = Arc::new(TraktManager::new(
            Arc::clone(&store),
            serve_device_code().await,
        ));

        let linked = status_dto(&state, user.id).await;
        assert!(linked.configured);
        assert!(linked.linked);
        assert_eq!(linked.trakt_username.as_deref(), Some("neo"));
        assert_eq!(linked.connected_at, Some(123));
        assert_eq!(linked.last_sync_at, Some(456));
        assert!(linked.pending.is_none());

        state.trakt.unlink(user.id).await.expect("unlink");
        let Json(linked) = link(AdminUser(user), State(state.clone()))
            .await
            .expect("begin device link");
        let pending = linked.pending.expect("pending projection");
        assert_eq!(pending.user_code, "ABCD");
        assert_eq!(pending.verification_url, "https://trakt.tv/activate");
        assert!((599..=600).contains(&pending.expires_in));
        assert!(pending.error.is_none());
    }
}
