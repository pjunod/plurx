//! HTTP surface of a plurxd node: liveness/readiness plus the native `/api/v1`.
//!
//! The native API is JSON. An OpenAPI description will be generated from these
//! routes as they stabilize (clients on five platforms consume it). The
//! Plex-compat façade (a separate crate) and playback routes mount alongside
//! in later slices.

mod auth;
mod browse;
mod dto;
mod error;
mod extract;
mod hls;
mod images;
mod libraries;
mod plex;
mod stream;
mod system;
mod watch;
mod web;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        // System / auth (public where noted)
        .route("/server", get(system::server_info))
        .route("/setup", post(system::setup))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/me", get(auth::me))
        .route(
            "/settings",
            get(system::get_settings).put(system::update_settings),
        )
        .route("/scan/status", get(system::scan_status))
        .route("/system", get(system::system_info))
        .route("/system/logs", get(system::logs))
        // Libraries
        .route("/libraries", get(libraries::list).post(libraries::create))
        .route(
            "/libraries/{id}",
            put(libraries::update).delete(libraries::delete),
        )
        .route("/libraries/{id}/scan", post(libraries::scan))
        .route("/libraries/{id}/items", get(browse::list_items))
        // Browse
        .route("/items/{id}", get(browse::item_detail))
        .route("/hubs", get(browse::hubs))
        .route("/search", get(browse::search))
        // Watch
        .route("/items/{id}/progress", post(watch::progress))
        .route("/items/{id}/scrobble", post(watch::scrobble))
        .route("/items/{id}/unscrobble", post(watch::unscrobble))
        // Playback
        .route("/files/{id}/decision", get(stream::decision))
        .route("/files/{id}/direct", get(stream::direct))
        .route("/files/{id}/stream.mp4", get(stream::stream_mp4))
        .route("/files/{id}/hls/start", get(hls::start))
        .route("/hls/{session}/index.m3u8", get(hls::playlist))
        .route("/hls/{session}/{segment}", get(hls::segment))
        // Images
        .route("/images/{filename}", get(images::serve));

    // Plex-compat Tier 1 façade at Plex's absolute paths (docs/CLIENTS.md §3).
    // Plex uses literal `:` path segments (`/:/timeline`, `/photo/:/transcode`)
    // which axum 0.8 rejects by default — `without_v07_checks` matches them
    // literally (we still use `{capture}` syntax for real captures).
    let plex_routes = Router::new()
        .without_v07_checks()
        .route("/identity", get(plex::identity))
        .route("/library", get(plex::library_root))
        .route("/library/sections", get(plex::sections))
        .route("/library/sections/{id}/all", get(plex::section_all))
        .route("/library/metadata/{key}", get(plex::metadata))
        .route("/library/metadata/{key}/children", get(plex::children))
        .route("/library/metadata/{key}/{kind}", get(plex::image))
        .route("/library/parts/{file_id}/{mtime}/{name}", get(plex::part))
        .route("/photo/:/transcode", get(plex::photo_transcode))
        .route("/:/timeline", get(plex::timeline))
        .route("/:/scrobble", get(plex::scrobble))
        .route("/:/unscrobble", get(plex::unscrobble))
        .route("/search", get(plex::search))
        .route("/hubs/search", get(plex::search));

    Router::new()
        // Also opted out of the v0.7 checks so the merged Plex `:` routes pass.
        .without_v07_checks()
        // `/` serves the web app for browsers, Plex capabilities for Plex clients.
        .route("/", get(root_dispatch))
        .route("/assets/hls.min.js", get(web::hls_js))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(system::metrics))
        .nest("/api/v1", api)
        .merge(plex_routes)
        .fallback(web::fallback)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Root path: Plex clients get the capabilities container; browsers get the app.
async fn root_dispatch(
    state: State<AppState>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if plex::looks_like_plex(&headers) {
        match plex::root(state).await {
            Ok(resp) => resp,
            Err(e) => e.into_response(),
        }
    } else {
        web::index().await.into_response()
    }
}

/// Liveness: the process is up. Never touches storage.
async fn healthz() -> &'static str {
    "ok\n"
}

/// Readiness: this node can do work (storage answers).
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match state.store.ping().await {
        Ok(()) => (StatusCode::OK, "ready\n"),
        Err(error) => {
            tracing::warn!(%error, "readiness probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, "store unavailable\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use plurx_core::store::SqliteStore;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;

    fn test_app() -> Router {
        let store = SqliteStore::open_in_memory().expect("store");
        let base = std::env::temp_dir().join(format!("plurx-test-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(
            "test".into(),
            Arc::new(store),
            base.join("artwork"),
            base.join("transcode"),
            Default::default(),
            Default::default(),
            Arc::new(crate::logbuf::LogBuffer::new(64)),
        );
        router(state)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.expect("response");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    fn get(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).expect("req")
    }

    fn post(uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("req")
    }

    async fn setup_admin(app: &Router) -> String {
        let (status, body) = call(
            app,
            post(
                "/api/v1/setup",
                None,
                json!({ "username": "paul", "password": "supersecret" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "setup failed: {body}");
        body["token"].as_str().expect("token").to_owned()
    }

    #[tokio::test]
    async fn health_endpoints() {
        let app = test_app();
        let resp = app
            .clone()
            .oneshot(get("/healthz", None))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app.oneshot(get("/readyz", None)).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn setup_then_login_flow() {
        let app = test_app();

        // Fresh server reports setup_required.
        let (_, info) = call(&app, get("/api/v1/server", None)).await;
        assert_eq!(info["setup_required"], true);

        // Setup creates the admin and returns a working token.
        let token = setup_admin(&app).await;
        let (status, me) = call(&app, get("/api/v1/me", Some(&token))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["username"], "paul");
        assert_eq!(me["is_admin"], true);

        // setup_required now false; a second setup is rejected.
        let (_, info) = call(&app, get("/api/v1/server", None)).await;
        assert_eq!(info["setup_required"], false);
        let (status, _) = call(
            &app,
            post(
                "/api/v1/setup",
                None,
                json!({ "username": "x", "password": "supersecret" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        // Login with the right/wrong password.
        let (status, body) = call(
            &app,
            post(
                "/api/v1/auth/login",
                None,
                json!({ "username": "paul", "password": "supersecret" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["token"].is_string());
        let (status, _) = call(
            &app,
            post(
                "/api/v1/auth/login",
                None,
                json!({ "username": "paul", "password": "wrong" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_is_required_and_admin_enforced() {
        let app = test_app();
        // No token → 401.
        let (status, _) = call(&app, get("/api/v1/libraries", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        // Admin can create a library (validation still applies).
        let (status, _) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": ["/tmp/none"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Bad kind → 400.
        let (status, _) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "X", "kind": "bogus", "paths": ["/tmp/none"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Empty paths → 400.
        let (status, _) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Y", "kind": "movies", "paths": [] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn system_info_is_admin_only() {
        let app = test_app();
        let (status, _) = call(&app, get("/api/v1/system", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        let (status, body) = call(&app, get("/api/v1/system", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["version"].is_string());
        assert!(body["encoders"].is_object());
        assert_eq!(body["users"], 1);
    }

    #[tokio::test]
    async fn logs_endpoint_is_admin_only() {
        let app = test_app();
        let (status, _) = call(&app, get("/api/v1/system/logs", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        let (status, body) = call(
            &app,
            get("/api/v1/system/logs?level=info&limit=50", Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_array());
    }

    #[tokio::test]
    async fn scan_status_requires_auth_and_reports_problems() {
        let app = test_app();
        // Unauthenticated → 401.
        let (status, _) = call(&app, get("/api/v1/scan/status", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        // Create a library pointing at a path that does not exist — the auto
        // scan must finish with a visible problem, not a silent all-zero.
        let (status, lib) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": ["/definitely/not/here"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let lib_id = lib["id"].as_i64().expect("lib id").to_string();

        // Poll until the background scan finishes (missing path → instant).
        let mut last = Value::Null;
        for _ in 0..100 {
            let (status, body) = call(&app, get("/api/v1/scan/status", Some(&admin))).await;
            assert_eq!(status, StatusCode::OK);
            last = body[&lib_id].clone();
            if !last["running"].as_bool().unwrap_or(true) && !last.is_null() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(last["running"], false, "scan never finished: {last}");
        let problems = last["last_scan"]["problems"]
            .as_array()
            .expect("problems array")
            .clone();
        assert!(
            problems
                .iter()
                .any(|p| p.as_str().unwrap_or("").contains("does not exist")),
            "expected a missing-path problem, got: {problems:?}"
        );
        assert_eq!(last["last_scan"]["errors"], 1);
    }

    #[tokio::test]
    async fn browse_and_watch_progress() {
        let app = test_app();
        let token = setup_admin(&app).await;

        // Create a library, then seed an item directly via the store isn't
        // possible through the API, so exercise the empty responses + a
        // progress round-trip against a manually inserted item.
        let (_, lib) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&token),
                json!({ "name": "M", "kind": "movies", "paths": ["/tmp/none"] }),
            ),
        )
        .await;
        let lib_id = lib["id"].as_i64().expect("lib id");

        // Empty library lists cleanly.
        let (status, list) = call(
            &app,
            get(&format!("/api/v1/libraries/{lib_id}/items"), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["total"], 0);

        // Hubs and search are empty but well-formed.
        let (status, hubs) = call(&app, get("/api/v1/hubs", Some(&token))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(hubs["continue_watching"].is_array());
        let (status, _) = call(&app, get("/api/v1/search?q=nothing", Some(&token))).await;
        assert_eq!(status, StatusCode::OK);

        // Progress on a nonexistent item → 404.
        let (status, _) = call(
            &app,
            post(
                "/api/v1/items/999/progress",
                Some(&token),
                json!({ "position_ms": 1000 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
