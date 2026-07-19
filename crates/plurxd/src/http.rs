//! HTTP surface of a plurxd node.
//!
//! Phase 0: liveness/readiness plus the seed of the native API (`/api/v1`).
//! The native API is JSON; an OpenAPI description is added as endpoints grow
//! real shape in Phase 1 (clients on five platforms will consume it).

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use plurx_core::store::Store;
use serde::Serialize;

#[derive(Clone)]
pub struct AppState {
    pub server_name: String,
    pub store: Arc<dyn Store>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(server_name: String, store: Arc<dyn Store>) -> Self {
        AppState {
            server_name,
            store,
            started_at: Instant::now(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/server", get(server_info))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Liveness: the process is up and serving. Never touches storage.
async fn healthz() -> &'static str {
    "ok\n"
}

/// Readiness: this node can actually do work (storage answers).
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.ping().await {
        Ok(()) => (StatusCode::OK, "ready\n"),
        Err(error) => {
            tracing::warn!(%error, "readiness probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, "store unavailable\n")
        }
    }
}

#[derive(Serialize)]
struct ServerInfo {
    name: String,
    version: &'static str,
    instance_id: String,
    uptime_seconds: u64,
}

async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfo>, StatusCode> {
    let instance_id = state.store.instance_id().await.map_err(|error| {
        tracing::error!(%error, "failed to read instance id");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(ServerInfo {
        name: state.server_name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        instance_id,
        uptime_seconds: state.started_at.elapsed().as_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use plurx_core::store::SqliteStore;
    use tower::ServiceExt;

    use super::*;

    fn test_router() -> Router {
        let store = SqliteStore::open_in_memory().expect("in-memory store");
        router(AppState::new("test".to_owned(), Arc::new(store)))
    }

    async fn get_path(router: Router, path: &str) -> (StatusCode, Vec<u8>) {
        let response = router
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes()
            .to_vec();
        (status, body)
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let (status, body) = get_path(test_router(), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, b"ok\n");
    }

    #[tokio::test]
    async fn readyz_is_ok_with_live_store() {
        let (status, _) = get_path(test_router(), "/readyz").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn server_info_reports_identity() {
        let (status, body) = get_path(test_router(), "/api/v1/server").await;
        assert_eq!(status, StatusCode::OK);

        let info: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(info["name"], "test");
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        let id = info["instance_id"].as_str().expect("instance_id string");
        uuid::Uuid::parse_str(id).expect("instance_id is a uuid");
    }

    #[tokio::test]
    async fn unknown_routes_are_404() {
        let (status, _) = get_path(test_router(), "/api/v1/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
