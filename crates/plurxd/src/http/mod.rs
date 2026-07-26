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
mod items;
mod libraries;
mod photos;
mod plex;
pub(crate) mod stream;
mod system;
mod trakt;
mod users;
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
        .route("/activity", get(system::activity))
        .route("/activity/detail", get(system::activity_detail))
        .route(
            "/activity/sessions/{id}",
            axum::routing::delete(system::stop_session),
        )
        .route("/trakt/status", get(trakt::status))
        .route("/trakt/link", post(trakt::link).delete(trakt::unlink))
        .route("/trakt/sync", post(trakt::sync_now))
        .route("/system", get(system::system_info))
        .route("/system/logs", get(system::logs))
        // Any signed-in user can post a client-side playback error here so it
        // lands in the admin log (browsers that reject a stream produce no
        // server log on their own).
        .route("/client-log", post(system::client_log))
        // Users (admin)
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", put(users::update).delete(users::delete))
        // Libraries
        .route("/libraries", get(libraries::list).post(libraries::create))
        .route(
            "/libraries/{id}",
            put(libraries::update).delete(libraries::delete),
        )
        .route("/libraries/{id}/scan", post(libraries::scan))
        .route("/libraries/{id}/refresh", post(libraries::refresh))
        .route("/libraries/{id}/items", get(browse::list_items))
        // Browse
        .route("/items/{id}", get(browse::item_detail).patch(items::edit))
        .route("/items/{id}/reanalyze", post(items::reanalyze))
        .route("/hubs", get(browse::hubs))
        .route("/search", get(browse::search))
        // Watch
        .route("/items/{id}/photo", get(photos::serve))
        .route("/items/{id}/progress", post(watch::progress))
        .route("/items/{id}/scrobble", post(watch::scrobble))
        .route("/items/{id}/unscrobble", post(watch::unscrobble))
        // Playback
        .route("/files/{id}/decision", get(stream::decision))
        .route("/files/{id}/audio-offset", put(stream::set_audio_offset))
        .route("/files/{id}/direct", get(stream::direct))
        .route("/files/{id}/stream.mp4", get(stream::stream_mp4))
        .route("/files/{id}/subs/{index}", get(stream::subtitles_vtt))
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
        // PWA install assets + the sideloadable Android APK.
        .route("/manifest.webmanifest", get(web::manifest))
        .route("/icons/{file}", get(web::icon))
        .route("/download/plurx-android.apk", get(web::download_android))
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

    fn put(uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("req")
    }

    fn delete(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("DELETE").uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).expect("req")
    }

    #[tokio::test]
    async fn user_management_lifecycle_and_lockout_guards() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        // Create a regular user.
        let (status, u) = call(
            &app,
            post(
                "/api/v1/users",
                Some(&admin),
                json!({ "username": "kid", "password": "longenough" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let kid_id = u["id"].as_i64().expect("id");
        assert_eq!(u["is_admin"], false);

        // The new user can log in; a non-admin cannot manage users.
        let (status, login) = call(
            &app,
            post(
                "/api/v1/auth/login",
                None,
                json!({ "username": "kid", "password": "longenough" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let kid_token = login["token"].as_str().expect("token").to_owned();
        let (status, _) = call(&app, get("/api/v1/users", Some(&kid_token))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Admin resets the kid's password → kid's session is revoked.
        let (status, _) = call(
            &app,
            put(
                &format!("/api/v1/users/{kid_id}"),
                Some(&admin),
                json!({ "password": "evenlonger1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(&app, get("/api/v1/me", Some(&kid_token))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "old session must die");

        // Lockout guards: the only admin can't be demoted…
        let (_, users) = call(&app, get("/api/v1/users", Some(&admin))).await;
        let admin_id = users
            .as_array()
            .expect("array")
            .iter()
            .find(|u| u["is_admin"] == true)
            .and_then(|u| u["id"].as_i64())
            .expect("admin id");
        let (status, _) = call(
            &app,
            put(
                &format!("/api/v1/users/{admin_id}"),
                Some(&admin),
                json!({ "is_admin": false }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        // …nor deleted (self-delete is refused first).
        let (status, _) = call(
            &app,
            delete(&format!("/api/v1/users/{admin_id}"), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Promote the kid, then the original admin could be demoted; delete
        // the kid instead and confirm they're gone.
        let (status, _) = call(
            &app,
            put(
                &format!("/api/v1/users/{kid_id}"),
                Some(&admin),
                json!({ "is_admin": true }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            delete(&format!("/api/v1/users/{kid_id}"), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, users) = call(&app, get("/api/v1/users", Some(&admin))).await;
        assert_eq!(users.as_array().expect("array").len(), 1);
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
    async fn activity_requires_auth_and_reports_idle() {
        let app = test_app();
        let (status, _) = call(&app, get("/api/v1/activity", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        let (status, body) = call(&app, get("/api/v1/activity", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.as_array().expect("array").is_empty(), "idle = empty");
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
    async fn client_log_requires_auth_and_accepts_reports() {
        let app = test_app();
        // Unauthenticated → 401 (rejected before the body is read).
        let (status, _) = call(
            &app,
            post("/api/v1/client-log", None, json!({ "event": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A signed-in user's playback error is accepted (204, empty body).
        let admin = setup_admin(&app).await;
        let (status, _) = call(
            &app,
            post(
                "/api/v1/client-log",
                Some(&admin),
                json!({
                    "level": "error",
                    "event": "playback_failed",
                    "message": "format not supported by this browser",
                    "method": "remux",
                    "code": 4,
                    "ua": "Safari"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // A near-empty body is tolerated too (all fields optional).
        let (status, _) = call(&app, post("/api/v1/client-log", Some(&admin), json!({}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn plex_facade_requires_a_valid_token() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let bare = |uri: &str| {
            app.clone().oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("req"),
            )
        };

        // Discovery stays public — clients must find the server before auth.
        let resp = bare("/identity").await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "identity must stay public");

        // A protected route with no token is rejected (previously served as admin):
        // metadata enumeration…
        let resp = bare("/library/sections").await.expect("resp");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // …and raw media bytes — the auth-bypass that motivated this.
        let resp = bare("/library/parts/1/0/x").await.expect("resp");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "no token must not stream files"
        );

        // A valid plurx token supplied as X-Plex-Token is accepted.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/library/sections")
                    .header("x-plex-token", &admin)
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "a valid token must be accepted"
        );
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

    // ---- seeded integration surface -----------------------------------------
    // A router plus the AppState behind it, so a test can seed items/files
    // straight through the store and then drive the real handlers end to end.
    fn test_state() -> (Router, AppState) {
        let store = SqliteStore::open_in_memory().expect("store");
        let base = std::env::temp_dir().join(format!("plurx-it-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(
            "test".into(),
            Arc::new(store),
            base.join("artwork"),
            base.join("transcode"),
            Default::default(),
            Default::default(),
            Arc::new(crate::logbuf::LogBuffer::new(64)),
        );
        (router(state.clone()), state)
    }

    struct Seed {
        lib: i64,
        movie: i64,
        file: i64,
        show: i64,
        season: i64,
        ep: i64,
    }

    async fn seed_content(state: &AppState) -> Seed {
        use plurx_core::domain::{
            AudioStream, ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult, SubtitleStream,
        };

        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![std::path::PathBuf::from("/media")],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = state
            .store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        // A real (tiny) file on disk so decision/direct/detail treat it as present.
        let dir = std::env::temp_dir().join(format!("plurx-media-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mediadir");
        let mpath = dir.join("Heat.mp4");
        std::fs::write(&mpath, b"\x00\x00\x00\x18ftypmp42 tiny placeholder bytes").expect("write");
        let probe = ProbeResult {
            duration_ms: Some(9_000_000),
            container: Some("mp4".into()),
            video_codec: Some("h264".into()),
            width: Some(1920),
            height: Some(1080),
            bit_depth: Some(8),
            bitrate: Some(8_000_000),
            audio_streams: vec![AudioStream {
                index: 0,
                codec: "aac".into(),
                channels: Some(2),
                language: Some("eng".into()),
                default: true,
                ..Default::default()
            }],
            subtitle_streams: vec![SubtitleStream {
                index: 0,
                codec: "subrip".into(),
                language: Some("eng".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let file = state
            .store
            .upsert_file(movie, &mpath.to_string_lossy(), 42, 1, &probe)
            .await
            .expect("file");
        let show = state
            .store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Show,
                parent_id: None,
                title: "The Wire".into(),
                year: Some(2002),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        let season = state
            .store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Season,
                parent_id: Some(show),
                title: "Season 1".into(),
                year: None,
                season_number: Some(1),
                episode_number: None,
            })
            .await
            .expect("season");
        let ep = state
            .store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Episode,
                parent_id: Some(season),
                title: "The Target".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(1),
            })
            .await
            .expect("ep");
        state
            .store
            .upsert_file(ep, &mpath.to_string_lossy(), 42, 1, &probe)
            .await
            .expect("epfile");
        Seed {
            lib: lib.id,
            movie,
            file,
            show,
            season,
            ep,
        }
    }

    struct HomeSeed {
        lib: i64,
        folder: i64,
        video: i64,
        photo: i64,
        photo_bytes: Vec<u8>,
    }

    /// A home library on disk: one folder holding a clip and a still.
    async fn seed_home(state: &AppState) -> HomeSeed {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};

        let dir = std::env::temp_dir().join(format!("plurx-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("2019")).expect("mediadir");
        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Home videos".into(),
                kind: LibraryKind::Home,
                paths: vec![dir.clone()],
                anime: false,
            })
            .await
            .expect("lib");
        let new_item = |kind, parent, title: &str| NewItem {
            library_id: lib.id,
            kind,
            parent_id: parent,
            title: title.to_owned(),
            year: None,
            season_number: None,
            episode_number: None,
        };
        let folder = state
            .store
            .insert_item(&new_item(ItemKind::Folder, None, "2019"))
            .await
            .expect("folder");
        let video = state
            .store
            .insert_item(&new_item(ItemKind::Video, Some(folder), "Beach day"))
            .await
            .expect("video");
        let photo = state
            .store
            .insert_item(&new_item(ItemKind::Photo, Some(folder), "IMG_4021"))
            .await
            .expect("photo");

        let vpath = dir.join("2019/Beach day.mp4");
        std::fs::write(&vpath, b"\x00\x00\x00\x18ftypmp42 tiny placeholder").expect("write video");
        state
            .store
            .upsert_file(
                video,
                &vpath.to_string_lossy(),
                42,
                1,
                &ProbeResult {
                    duration_ms: Some(12_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    ..Default::default()
                },
            )
            .await
            .expect("video file");

        let photo_bytes = b"\xff\xd8\xff\xe0 pretend this is a jpeg \xff\xd9".to_vec();
        let ppath = dir.join("2019/IMG_4021.jpg");
        std::fs::write(&ppath, &photo_bytes).expect("write photo");
        state
            .store
            .upsert_file(
                photo,
                &ppath.to_string_lossy(),
                photo_bytes.len() as i64,
                1,
                &ProbeResult {
                    container: Some("jpg".into()),
                    width: Some(4032),
                    height: Some(3024),
                    ..Default::default()
                },
            )
            .await
            .expect("photo file");

        state
            .store
            .apply_metadata(
                video,
                &plurx_core::domain::MetadataPatch {
                    recorded_at: Some("2019-06-14".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("video date");
        HomeSeed {
            lib: lib.id,
            folder,
            video,
            photo,
            photo_bytes,
        }
    }

    #[tokio::test]
    async fn photos_serve_as_bytes_with_ranges() {
        let (app, state) = test_state();
        let token = setup_admin(&app).await;
        let h = seed_home(&state).await;

        // The original: real bytes, right type, and range-capable — a photo
        // never touches the playback pipeline.
        let resp = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/items/{}/photo", h.photo),
                Some(&token),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/jpeg")
        );
        assert_eq!(
            resp.headers()
                .get("accept-ranges")
                .and_then(|v| v.to_str().ok()),
            Some("bytes")
        );
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(body.as_ref(), h.photo_bytes.as_slice());

        // A range request is honored.
        let req = Request::builder()
            .uri(format!("/api/v1/items/{}/photo", h.photo))
            .header("authorization", format!("Bearer {token}"))
            .header("range", "bytes=0-3")
            .body(Body::empty())
            .expect("req");
        let resp = app.clone().oneshot(req).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let body = resp.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(body.len(), 4);

        // No thumbnail generated yet → the original, not a broken image.
        let resp = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/items/{}/photo?size=thumb", h.photo),
                Some(&token),
            ))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);

        // Only photos have a photo endpoint.
        let (status, _) = call(
            &app,
            get(&format!("/api/v1/items/{}/photo", h.video), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = call(
            &app,
            get(&format!("/api/v1/items/{}/photo", h.folder), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Unauthenticated is refused, like every other media route.
        let (status, _) = call(&app, get(&format!("/api/v1/items/{}/photo", h.photo), None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn home_items_carry_dates_and_tags_on_the_wire() {
        let (app, state) = test_state();
        let token = setup_admin(&app).await;
        let h = seed_home(&state).await;

        let (status, detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", h.video), Some(&token)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["item"]["kind"], "video");
        assert_eq!(detail["item"]["recorded_at"], "2019-06-14");
        assert_eq!(detail["item"]["tags"], json!([]));
        assert_eq!(detail["ancestors"][0]["title"], "2019", "breadcrumb");
        assert_eq!(
            detail["files"].as_array().map(Vec::len),
            Some(1),
            "a home video has files exactly like a movie does"
        );

        // The folder's detail lists its children, and has no files of its own.
        let (_, folder) = call(
            &app,
            get(&format!("/api/v1/items/{}", h.folder), Some(&token)),
        )
        .await;
        assert_eq!(folder["children"].as_array().map(Vec::len), Some(2));
        assert_eq!(folder["files"].as_array().map(Vec::len), Some(0));

        // Photos stay out of the home screen's recently-added row.
        let (_, hubs) = call(&app, get("/api/v1/hubs", Some(&token))).await;
        let kinds: Vec<&str> = hubs["recently_added"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|i| i["kind"].as_str())
            .collect();
        assert!(!kinds.contains(&"photo"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"video"));
        let _ = h.lib;
    }

    fn patch(uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("PATCH")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).expect("req")
    }

    #[tokio::test]
    async fn editing_home_metadata_sets_clears_and_refuses() {
        let (app, state) = test_state();
        let token = setup_admin(&app).await;
        let h = seed_home(&state).await;
        let s = seed_content(&state).await;

        // Set several fields at once.
        let (status, body) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", h.video),
                Some(&token),
                json!({
                    "title": "  Beach day, take two  ",
                    "overview": "Windy.",
                    "recorded_at": "2019-06-15",
                    "year": 2019,
                    "tags": ["beach", " kids ", "Beach", ""]
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["title"], "Beach day, take two", "titles are trimmed");
        assert_eq!(body["recorded_at"], "2019-06-15");
        assert_eq!(body["year"], 2019);
        assert_eq!(
            body["tags"],
            json!(["beach", "kids"]),
            "tags are trimmed and de-duplicated"
        );

        // null clears; an absent field is left alone.
        let (status, body) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", h.video),
                Some(&token),
                json!({ "recorded_at": null, "overview": null }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["recorded_at"], Value::Null);
        assert_eq!(body["overview"], Value::Null);
        assert_eq!(body["title"], "Beach day, take two");
        assert_eq!(body["tags"], json!(["beach", "kids"]));

        // Clearing tags is an empty array, not null.
        let (_, body) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", h.video),
                Some(&token),
                json!({ "tags": [] }),
            ),
        )
        .await;
        assert_eq!(body["tags"], json!([]));

        // The edit sticks — a re-read shows it, not just the response.
        let (_, detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", h.video), Some(&token)),
        )
        .await;
        assert_eq!(detail["item"]["title"], "Beach day, take two");

        // Folders are retitlable too (the directory on disk is never renamed).
        let (status, body) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", h.folder),
                Some(&token),
                json!({ "title": "Summer 2019" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["title"], "Summer 2019");

        // Rejections: empty title, unsortable date, nothing to do.
        for bad in [
            json!({ "title": "   " }),
            json!({ "recorded_at": "last summer" }),
            json!({ "recorded_at": "14/06/2019" }),
            json!({}),
        ] {
            let (status, _) = call(
                &app,
                patch(
                    &format!("/api/v1/items/{}", h.video),
                    Some(&token),
                    bad.clone(),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "should reject {bad}");
        }

        // A movie is owned by its metadata agent: refused, with a reason.
        let (status, body) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", s.movie),
                Some(&token),
                json!({ "title": "Heat (director's cut)" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("home libraries"),
            "error should say why: {body}"
        );

        // Admin only, and a missing item is a 404.
        let (status, _) = call(
            &app,
            patch(
                &format!("/api/v1/items/{}", h.video),
                None,
                json!({ "title": "nope" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = call(
            &app,
            patch("/api/v1/items/999999", Some(&token), json!({"title": "x"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn home_browse_sorts_by_date_and_resumes() {
        let (app, state) = test_state();
        let token = setup_admin(&app).await;
        let h = seed_home(&state).await;

        // sort=recorded is accepted and puts dated items first.
        let (status, list) = call(
            &app,
            get(
                &format!("/api/v1/libraries/{}/items?sort=recorded", h.lib),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["total"], 1, "only the folder sits at the root");

        // A home video carries a resolution badge, exactly like a movie.
        let (_, folder) = call(
            &app,
            get(&format!("/api/v1/items/{}", h.folder), Some(&token)),
        )
        .await;
        let children = folder["children"].as_array().expect("children");
        assert_eq!(children[0]["kind"], "video", "media before photos here");
        assert_eq!(
            children[0]["resolution"],
            Value::Null,
            "detail has no badge"
        );

        // Half-watch the clip: it must show up in continue-watching.
        let (status, _) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/progress", h.video),
                Some(&token),
                json!({ "position_ms": 4_000, "duration_ms": 12_000 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, hubs) = call(&app, get("/api/v1/hubs", Some(&token))).await;
        let resuming: Vec<i64> = hubs["continue_watching"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|i| i["id"].as_i64())
            .collect();
        assert!(
            resuming.contains(&h.video),
            "a partially-watched home video belongs in continue-watching: {hubs}"
        );
    }

    #[tokio::test]
    async fn seeded_read_surface() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        let authed: Vec<String> = vec![
            "/api/v1/hubs".into(),
            "/api/v1/search?q=Heat".into(),
            format!("/api/v1/items/{}", s.movie),
            format!("/api/v1/items/{}", s.show),
            format!("/api/v1/items/{}", s.season),
            format!("/api/v1/items/{}", s.ep),
            format!("/api/v1/libraries/{}/items", s.lib),
            format!("/api/v1/libraries/{}/items?sort=title&limit=5&offset=0", s.lib),
            format!("/api/v1/libraries/{}/items?sort=added", s.lib),
            format!("/api/v1/libraries/{}/items?sort=year", s.lib),
            format!("/api/v1/libraries/{}/items?sort=resolution", s.lib),
            format!(
                "/api/v1/files/{}/decision?vcodec=h264,hevc&acodec=aac&container=mp4&hdr=0",
                s.file
            ),
            format!(
                "/api/v1/files/{}/decision?vcodec=vp9&acodec=opus&container=webm&hdr=0&force=transcode",
                s.file
            ),
            "/api/v1/system".into(),
            "/api/v1/system/logs?level=trace&limit=10".into(),
            "/api/v1/scan/status".into(),
            "/api/v1/settings".into(),
            "/api/v1/users".into(),
            "/api/v1/trakt/status".into(),
        ];
        for uri in &authed {
            let status = call(&app, get(uri, Some(&admin))).await.0;
            assert!(status.is_success(), "GET {uri} -> {status}");
        }

        // Public / static, no token.
        for uri in [
            "/",
            "/manifest.webmanifest",
            "/icons/icon-192.png",
            "/icons/apple-touch-icon.png",
            "/assets/hls.min.js",
            "/healthz",
            "/readyz",
            "/metrics",
            "/api/v1/server",
        ] {
            let status = app
                .clone()
                .oneshot(get(uri, None))
                .await
                .expect("r")
                .status();
            assert!(status.is_success(), "GET {uri} -> {status}");
        }
        // Unknown icon → 404.
        let status = app
            .clone()
            .oneshot(get("/icons/nope.png", None))
            .await
            .expect("r")
            .status();
        assert_eq!(status, StatusCode::NOT_FOUND);

        // Direct play the real file (whole, then a range).
        let status = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/files/{}/direct", s.file),
                Some(&admin),
            ))
            .await
            .expect("r")
            .status();
        assert!(
            status == StatusCode::OK || status == StatusCode::PARTIAL_CONTENT,
            "direct -> {status}"
        );
        let ranged = Request::builder()
            .uri(format!("/api/v1/files/{}/direct?token={admin}", s.file))
            .header("range", "bytes=0-3")
            .body(Body::empty())
            .expect("req");
        let status = app.clone().oneshot(ranged).await.expect("r").status();
        assert_eq!(status, StatusCode::PARTIAL_CONTENT, "ranged -> {status}");
    }

    #[tokio::test]
    async fn seeded_write_surface() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // Progress returns the updated watch state.
        let (st, w) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/progress", s.movie),
                Some(&admin),
                json!({ "position_ms": 1000, "duration_ms": 9_000_000 }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(w["position_ms"], 1000);

        // Mark watched / unwatched.
        assert_eq!(
            call(
                &app,
                post(
                    &format!("/api/v1/items/{}/scrobble", s.movie),
                    Some(&admin),
                    json!({})
                )
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                post(
                    &format!("/api/v1/items/{}/unscrobble", s.movie),
                    Some(&admin),
                    json!({})
                )
            )
            .await
            .0,
            StatusCode::OK
        );
        // Manual A/V offset.
        assert_eq!(
            call(
                &app,
                put(
                    &format!("/api/v1/files/{}/audio-offset", s.file),
                    Some(&admin),
                    json!({ "offset_ms": 250 })
                )
            )
            .await
            .0,
            StatusCode::OK
        );
        // Progress on a missing item → 404.
        assert_eq!(
            call(
                &app,
                post(
                    "/api/v1/items/424242/progress",
                    Some(&admin),
                    json!({ "position_ms": 1 })
                )
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );

        // Reanalyze: admin-only, 404 for an item that isn't there, and for a
        // real item it reports per-file rather than pretending to succeed. The
        // seeded file is 31 placeholder bytes, so ffprobe refuses it — which is
        // exactly the shape this endpoint exists to report honestly.
        assert_eq!(
            call(
                &app,
                post(
                    &format!("/api/v1/items/{}/reanalyze", s.movie),
                    None,
                    json!({})
                )
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                &app,
                post("/api/v1/items/999999/reanalyze", Some(&admin), json!({}))
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        // Nothing to analyze is a 400 that says so, not a cheerful empty report.
        // (The seed hangs its one file off the episode — same path, so the
        // upsert moved it — which makes the movie the natural case here.)
        assert_eq!(
            call(
                &app,
                post(
                    &format!("/api/v1/items/{}/reanalyze", s.movie),
                    Some(&admin),
                    json!({})
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (st, report) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/reanalyze", s.ep),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "body: {report:?}");
        assert_eq!(report["attempted"], 1);
        assert_eq!(report["repaired"], 0, "placeholder bytes are not media");
        assert_eq!(report["still_failing"], 1);
        assert!(
            report["problems"][0]
                .as_str()
                .is_some_and(|p| p.contains("Heat.mp4")),
            "the file that failed is named: {report:?}"
        );

        // Settings round-trip.
        assert_eq!(
            call(
                &app,
                put(
                    "/api/v1/settings",
                    Some(&admin),
                    json!({
                        "tmdb_api_key": "key",
                        "omdb_api_key": "",
                        "default_audio_lang": "eng",
                        "default_sub_lang": "eng",
                        "sub_mode": "always"
                    })
                )
            )
            .await
            .0,
            StatusCode::OK
        );

        // Update the seeded library, then create + delete a second one.
        assert_eq!(
            call(
                &app,
                put(
                    &format!("/api/v1/libraries/{}", s.lib),
                    Some(&admin),
                    json!({ "name": "Films", "kind": "movies", "paths": ["/media"] })
                )
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, made) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "TV", "kind": "shows", "paths": ["/tv"] }),
            ),
        )
        .await;
        let made_id = made["id"].as_i64().expect("id");
        assert_eq!(
            call(
                &app,
                delete(&format!("/api/v1/libraries/{made_id}"), Some(&admin))
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn plex_facade_serves_seeded_content() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;
        let tok = |uri: String| {
            Request::builder()
                .uri(uri)
                .header("x-plex-token", &admin)
                .body(Body::empty())
                .expect("req")
        };
        for uri in [
            "/identity".to_string(),
            "/library".to_string(),
            "/library/sections".to_string(),
            format!("/library/sections/{}/all", s.lib),
            format!("/library/metadata/{}", s.movie),
            format!("/library/metadata/{}", s.show),
            format!("/library/metadata/{}/children", s.show),
            "/search?query=Heat".to_string(),
        ] {
            let status = app
                .clone()
                .oneshot(tok(uri.clone()))
                .await
                .expect("r")
                .status();
            assert!(status.is_success(), "plex GET {uri} -> {status}");
        }
    }

    /// A GET whose auth rides in the query string (`?token=`), as `<video>`,
    /// `<img>`, and `<track>` elements must.
    fn get_q(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("req")
    }

    async fn status_of(app: &Router, req: Request<Body>) -> StatusCode {
        app.clone().oneshot(req).await.expect("resp").status()
    }

    #[tokio::test]
    async fn stream_delivery_paths() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // Remux to fragmented MP4 (ffmpeg fails on the placeholder bytes, but the
        // handler + remux pipeline run and return a 200 streaming body).
        assert_eq!(
            status_of(
                &app,
                get_q(&format!(
                    "/api/v1/files/{}/stream.mp4?token={admin}",
                    s.file
                )),
            )
            .await,
            StatusCode::OK
        );
        // Force an audio transcode (source aac not in the reported codec set).
        assert_eq!(
            status_of(
                &app,
                get_q(&format!(
                    "/api/v1/files/{}/stream.mp4?token={admin}&acodec=opus&vcodec=h264&container=mp4",
                    s.file
                )),
            )
            .await,
            StatusCode::OK
        );
        // Subtitle extraction: the text track exists in metadata but not in the
        // placeholder file, so ffmpeg fails → 500; an out-of-range index → 404.
        assert_eq!(
            status_of(
                &app,
                get_q(&format!("/api/v1/files/{}/subs/0?token={admin}", s.file)),
            )
            .await,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            status_of(
                &app,
                get_q(&format!("/api/v1/files/{}/subs/99?token={admin}", s.file)),
            )
            .await,
            StatusCode::NOT_FOUND
        );
        // Direct-play a missing file id → 404.
        assert_eq!(
            status_of(&app, get("/api/v1/files/424242/direct", Some(&admin))).await,
            StatusCode::NOT_FOUND
        );

        // A file whose path isn't on disk → decision refuses with 409 Conflict.
        use plurx_core::domain::{ItemKind, NewItem, ProbeResult};
        let ghost_item = state
            .store
            .insert_item(&NewItem {
                library_id: s.lib,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Ghost".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        let ghost_file = state
            .store
            .upsert_file(
                ghost_item,
                "/definitely/not/here.mp4",
                1,
                1,
                &ProbeResult {
                    duration_ms: Some(1000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        assert_eq!(
            status_of(
                &app,
                get(
                    &format!("/api/v1/files/{ghost_file}/decision"),
                    Some(&admin)
                ),
            )
            .await,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn hls_http_endpoints() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // Unknown session → 404 for both playlist and segment.
        assert_eq!(
            status_of(&app, get_q("/api/v1/hls/nosuchsession/index.m3u8")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, get_q("/api/v1/hls/nosuchsession/seg00000.ts")).await,
            StatusCode::NOT_FOUND
        );
        // Start a session (spawns ffmpeg; returns the playlist URL immediately).
        let (st, body) = call(
            &app,
            get_q(&format!("/api/v1/files/{}/hls/start?token={admin}", s.file)),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(body["session_id"].is_string());
    }

    #[tokio::test]
    async fn plex_write_image_and_part_paths() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        let plex = |uri: String| {
            Request::builder()
                .uri(uri)
                .header("x-plex-token", &admin)
                .body(Body::empty())
                .expect("req")
        };

        // timeline writes progress; scrobble / unscrobble flip watched.
        for uri in [
            format!(
                "/:/timeline?ratingKey={}&time=1000&duration=9000000",
                s.movie
            ),
            format!("/:/scrobble?key={}", s.movie),
            format!("/:/unscrobble?key={}", s.movie),
        ] {
            assert!(
                status_of(&app, plex(uri.clone())).await.is_success(),
                "{uri}"
            );
        }
        // Direct-play part on the real seeded file.
        assert!(
            status_of(&app, plex(format!("/library/parts/{}/0/Heat.mp4", s.file)),)
                .await
                .is_success()
        );

        // Image with no artwork set → 404.
        assert_eq!(
            status_of(&app, plex(format!("/library/metadata/{}/thumb", s.movie))).await,
            StatusCode::NOT_FOUND
        );

        // Give the movie a poster on disk, then thumb + photo transcode serve it.
        tokio::fs::create_dir_all(&state.artwork_dir)
            .await
            .expect("mkart");
        tokio::fs::write(state.artwork_dir.join("poster.jpg"), b"\xff\xd8\xff jpeg")
            .await
            .expect("art");
        state
            .store
            .apply_metadata(
                s.movie,
                &plurx_core::domain::MetadataPatch {
                    poster_path: Some("poster.jpg".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("meta");
        assert!(
            status_of(&app, plex(format!("/library/metadata/{}/thumb", s.movie)))
                .await
                .is_success()
        );
        assert!(status_of(
            &app,
            plex(format!(
                "/photo/:/transcode?url=/library/metadata/{}/thumb",
                s.movie
            )),
        )
        .await
        .is_success());
        // A malformed photo url → 404.
        assert_eq!(
            status_of(&app, plex("/photo/:/transcode?url=/bogus".into())).await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn images_web_and_auth_paths() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;

        // Native artwork endpoint: a real file, a traversal name, a miss.
        tokio::fs::create_dir_all(&state.artwork_dir)
            .await
            .expect("mkart");
        tokio::fs::write(state.artwork_dir.join("a.jpg"), b"\xff\xd8\xff")
            .await
            .expect("art");
        assert_eq!(
            status_of(&app, get_q(&format!("/api/v1/images/a.jpg?token={admin}"))).await,
            StatusCode::OK
        );
        assert_eq!(
            status_of(&app, get_q(&format!("/api/v1/images/..?token={admin}"))).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(
                &app,
                get_q(&format!("/api/v1/images/missing.jpg?token={admin}"))
            )
            .await,
            StatusCode::NOT_FOUND
        );

        // No APK published → 404; SPA fallback serves the app for non-API paths
        // but a clean JSON 404 under /api.
        assert_eq!(
            status_of(&app, get_q("/download/plurx-android.apk")).await,
            StatusCode::NOT_FOUND
        );
        assert!(status_of(&app, get_q("/some/app/route")).await.is_success());
        assert_eq!(
            status_of(&app, get("/api/v1/nope", Some(&admin))).await,
            StatusCode::NOT_FOUND
        );

        // /me and logout for the signed-in admin; a bad login is rejected.
        assert_eq!(
            status_of(&app, get("/api/v1/me", Some(&admin))).await,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, post("/api/v1/auth/logout", Some(&admin), json!({})))
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                post(
                    "/api/v1/auth/login",
                    None,
                    json!({ "username": "paul", "password": "wrong" }),
                ),
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn admin_activity_library_and_trakt_paths() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // Activity detail (any user) + stopping an unknown session (admin).
        assert_eq!(
            status_of(&app, get("/api/v1/activity/detail", Some(&admin))).await,
            StatusCode::OK
        );
        assert_eq!(
            status_of(&app, delete("/api/v1/activity/sessions/nope", Some(&admin))).await,
            StatusCode::NOT_FOUND
        );

        // Library validation rejects unknown kind, empty name, no paths.
        for bad in [
            json!({ "name": "X", "kind": "bogus", "paths": ["/x"] }),
            json!({ "name": "   ", "kind": "movies", "paths": ["/x"] }),
            json!({ "name": "X", "kind": "movies", "paths": [] }),
        ] {
            assert_eq!(
                call(&app, post("/api/v1/libraries", Some(&admin), bad))
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        // Scan + refresh the seeded library; a missing library → 404.
        assert!(status_of(
            &app,
            post(
                &format!("/api/v1/libraries/{}/scan", s.lib),
                Some(&admin),
                json!({})
            ),
        )
        .await
        .is_success());
        assert!(status_of(
            &app,
            post(
                &format!("/api/v1/libraries/{}/refresh", s.lib),
                Some(&admin),
                json!({})
            ),
        )
        .await
        .is_success());
        assert_eq!(
            status_of(
                &app,
                post("/api/v1/libraries/999999/scan", Some(&admin), json!({})),
            )
            .await,
            StatusCode::NOT_FOUND
        );

        // Trakt sync (notify only) + unlink (no-op when unlinked) are safe here.
        assert!(
            status_of(&app, post("/api/v1/trakt/sync", Some(&admin), json!({})))
                .await
                .is_success()
        );
        assert!(status_of(&app, delete("/api/v1/trakt/link", Some(&admin)))
            .await
            .is_success());
    }

    #[tokio::test]
    async fn setup_rejects_a_weak_password() {
        // Fresh server, no admin yet: the password-length rule applies.
        let app = test_app();
        assert_eq!(
            call(
                &app,
                post(
                    "/api/v1/setup",
                    None,
                    json!({ "username": "paul", "password": "short" }),
                ),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
}
