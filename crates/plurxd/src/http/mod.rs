//! HTTP surface of a plurxd node: liveness/readiness plus the native `/api/v1`.
//!
//! The native API is JSON. An OpenAPI description will be generated from these
//! routes as they stabilize (clients on five platforms consume it). The
//! Plex-compat façade (a separate crate) and playback routes mount alongside
//! in later slices.

mod auth;
mod browse;
pub mod comingsoon;
pub use comingsoon::ComingSoonCache;
mod dto;
mod error;
mod extract;
mod hls;
mod images;
mod items;
mod keys;
mod libraries;
mod photos;
mod plex;
mod scan;
pub(crate) mod stream;
pub(crate) mod system;
mod trakt;
mod users;
mod watch;
mod web;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
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
        .route(
            "/activity/producer",
            axum::routing::delete(system::stop_producer),
        )
        .route("/trakt/status", get(trakt::status))
        .route("/trakt/link", post(trakt::link).delete(trakt::unlink))
        .route("/trakt/sync", post(trakt::sync_now))
        .route("/system", get(system::system_info))
        .route("/system/logs", get(system::logs))
        // What the libraries hold, in transcoder terms — the census PERF-PLAN
        // §5 needs to say whether the GPU tone-map reaches a real library.
        .route("/system/library-shape", get(system::library_shape))
        // Re-measure storage. POST because it costs real I/O against the
        // library, and separate from GET /system so that reading the last
        // numbers is never the thing that goes and takes new ones.
        .route("/system/storage", post(system::remeasure_storage))
        // Any signed-in user can post a client-side playback error here so it
        // lands in the admin log (browsers that reject a stream produce no
        // server log on their own).
        .route("/client-log", post(system::client_log))
        // Users (admin)
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", put(users::update).delete(users::delete))
        // API keys (admin) — the machine credential. Managing keys is a
        // user action; USING one is not, and those routes take ScopedKey.
        .route("/keys", get(keys::list).post(keys::create))
        .route("/keys/{id}", delete(keys::delete))
        // Targeted scan — key-scoped, for other applications. Not under
        // /libraries/{id} on purpose: the caller knows a path, not a plurx
        // library id, and plurx resolving it is one less thing for two
        // applications to keep in sync.
        .route("/coming-soon", get(comingsoon::coming_soon))
        .route("/monarr/status", get(comingsoon::monarr_status))
        .route("/scan", post(scan::scan))
        .route("/scan/requests/{id}", get(scan::request_status))
        // Libraries
        .route("/libraries", get(libraries::list).post(libraries::create))
        .route(
            "/libraries/{id}",
            put(libraries::update).delete(libraries::delete),
        )
        .route("/libraries/{id}/schedule", put(libraries::set_schedule))
        .route("/libraries/{id}/scan", post(libraries::scan))
        .route("/libraries/{id}/refresh", post(libraries::refresh))
        .route("/libraries/{id}/items", get(browse::list_items))
        // Browse
        .route("/items/{id}", get(browse::item_detail).patch(items::edit))
        .route("/items/{id}/reanalyze", post(items::reanalyze))
        .route("/items/{id}/refresh-artwork", post(items::refresh_artwork))
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
        // How a progressive remux is doing. Session auth, not capability: the
        // id is the client's own playback id, so the check is that the asker
        // owns the stream.
        .route("/stream/{id}/status", get(stream::stream_status))
        .route("/files/{id}/subs/{index}", get(stream::subtitles_vtt))
        // Creating a stream spawns a process and supersedes its predecessor,
        // so it is a POST. The GET is a deprecated bridge over the same path.
        .route("/files/{id}/hls/sessions", post(hls::create))
        .route("/files/{id}/hls/start", get(hls::start))
        .route("/hls/{session}/index.m3u8", get(hls::playlist))
        // Before the `{segment}` catch-all in intent, though the router
        // prefers the static segment regardless of registration order.
        .route("/hls/{session}/status", get(hls::status))
        // Capability auth (the session id is the credential) so a closing tab
        // can send this with `keepalive`, which cannot set headers.
        .route("/hls/{session}", delete(hls::delete))
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

    fn test_dirs(base: &std::path::Path) -> crate::state::Dirs {
        crate::state::Dirs {
            artwork: base.join("artwork"),
            transcode: base.join("transcode"),
            cache: base.join("cache"),
            subs: base.join("subs"),
        }
    }

    fn test_app() -> Router {
        test_app_with_state().0
    }

    /// The same app, plus the state behind it — for tests that have to put the
    /// server into a condition a request cannot create, like a pre-transcode
    /// pass already running.
    fn test_app_with_state() -> (Router, AppState) {
        let store = SqliteStore::open_in_memory().expect("store");
        let base = std::env::temp_dir().join(format!("plurx-test-{}", uuid::Uuid::new_v4()));
        let state = AppState::new(
            "test".into(),
            Arc::new(store),
            test_dirs(&base),
            "test-node".into(),
            Default::default(),
            Default::default(),
            Arc::new(crate::logbuf::LogBuffer::new(64)),
        );
        (router(state.clone()), state)
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

    fn delete_req(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method("DELETE").uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).expect("req")
    }

    // ---- scoped API keys (integration plan P1) --------------------------
    //
    // The security claim is the entire reason keys exist, so it is asserted
    // here rather than trusted: a key can do what its scopes say and NOTHING
    // else — above all it cannot read the settings blob, which holds the
    // TMDB/Trakt secrets an admin token would hand over wholesale.

    #[tokio::test]
    async fn a_key_is_shown_once_and_never_again() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        let (status, created) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&admin),
                json!({ "name": "monarr", "scopes": ["scan:trigger"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create failed: {created}");
        let secret = created["key_secret"].as_str().expect("secret");
        assert!(secret.starts_with("plx_"), "secret = {secret}");

        let (status, listed) = call(&app, get("/api/v1/keys", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        let text = listed.to_string();
        assert!(
            !text.contains(secret),
            "the list handed the secret back — it must be unrecoverable: {text}"
        );
        assert!(
            !text.contains("key_hash"),
            "the stored hash has no business on a settings screen: {text}"
        );
        assert_eq!(listed[0]["name"], "monarr");
        assert_eq!(listed[0]["scopes"][0], "scan:trigger");
    }

    #[tokio::test]
    async fn only_an_admin_manages_keys() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        // A key is not a user and must not be able to mint more keys —
        // otherwise the narrow credential is one request away from being a
        // wide one.
        let (_, created) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&admin),
                json!({ "name": "monarr", "scopes": ["scan:trigger"] }),
            ),
        )
        .await;
        let secret = created["key_secret"].as_str().expect("secret").to_owned();

        let (status, _) = call(&app, get("/api/v1/keys", Some(&secret))).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a key was accepted on a user route"
        );
        let (status, _) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&secret),
                json!({"name":"x","scopes":["scan:trigger"]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a key minted another key");

        let (status, _) = call(&app, get("/api/v1/keys", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // The reason the whole concept exists. If this ever passes with a 200,
    // handing monarr a key is handing it the TMDB and Trakt secrets.
    #[tokio::test]
    async fn a_key_cannot_read_the_settings_that_hold_every_secret() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let (_, created) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&admin),
                json!({ "name": "monarr", "scopes": ["scan:trigger", "status:read"] }),
            ),
        )
        .await;
        let secret = created["key_secret"].as_str().expect("secret").to_owned();

        for uri in ["/api/v1/settings", "/api/v1/users", "/api/v1/me"] {
            let (status, body) = call(&app, get(uri, Some(&secret))).await;
            assert!(
                status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
                "a scan key reached {uri} with {status}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn keys_are_created_with_real_scopes_or_not_at_all() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        // A typo'd scope would otherwise store fine and authorize nothing —
        // failing hours later, in another application, as a 403.
        let (status, body) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&admin),
                json!({ "name": "typo", "scopes": ["scan:triggr"] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.to_string().contains("scan:trigger"),
            "the error should name the valid scopes: {body}"
        );

        for bad in [
            json!({ "name": "", "scopes": ["scan:trigger"] }),
            json!({ "name": "no scopes", "scopes": [] }),
        ] {
            let (status, _) = call(&app, post("/api/v1/keys", Some(&admin), bad)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn revoking_a_key_stops_it() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let (_, created) = call(
            &app,
            post(
                "/api/v1/keys",
                Some(&admin),
                json!({ "name": "monarr", "scopes": ["scan:trigger"] }),
            ),
        )
        .await;
        let id = created["id"].as_i64().expect("id");

        let (status, _) = call(
            &app,
            delete_req(&format!("/api/v1/keys/{id}"), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, listed) = call(&app, get("/api/v1/keys", Some(&admin))).await;
        assert_eq!(listed.as_array().expect("array").len(), 0);

        let (status, _) = call(
            &app,
            delete_req(&format!("/api/v1/keys/{id}"), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "revoking twice should 404");
    }

    // ---- targeted scan endpoint (integration plan P3) -------------------

    async fn scan_key(app: &Router, admin: &str, scopes: Value) -> String {
        let (status, created) = call(
            app,
            post(
                "/api/v1/keys",
                Some(admin),
                json!({ "name": "monarr", "scopes": scopes }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "key create: {created}");
        created["key_secret"].as_str().expect("secret").to_owned()
    }

    /// The route exists for machines. A user token opening it would mean the
    /// scoped key bought nothing — you could still just hand over an admin
    /// token and be done.
    #[tokio::test]
    async fn the_scan_route_takes_a_key_and_only_a_key() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let body = json!({ "path": "/tmp" });

        let (status, _) = call(&app, post("/api/v1/scan", Some(&admin), body.clone())).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an admin TOKEN opened a key-scoped route"
        );
        let (status, _) = call(&app, post("/api/v1/scan", None, body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A real key without scan:trigger is Forbidden, not Unauthorized —
        // the credential is valid, the answer is about permission.
        let status_only = scan_key(&app, &admin, json!(["status:read"])).await;
        let (status, _) = call(&app, post("/api/v1/scan", Some(&status_only), body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    /// The same key, in either header.
    ///
    /// `X-Api-Key` is what every *arr application sends, and plurx read only
    /// `Authorization: Bearer`. A perfectly good key therefore came back 401 —
    /// which reads as "your key is wrong" and sends you off to re-mint a
    /// credential that was never the problem. The `plx_` prefix is what
    /// separates a key from a login token, and it does that identically
    /// whichever header carried the secret, so being fussy bought nothing.
    #[tokio::test]
    async fn a_key_works_in_either_header() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger"])).await;
        let body = json!({ "path": "/definitely/not/a/library/root" });

        // Authorization: Bearer — reaches the handler, which then rejects the
        // path rather than the credential.
        let (bearer, _) = call(&app, post("/api/v1/scan", Some(&key), body.clone())).await;

        // X-Api-Key must reach exactly the same place.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/scan")
            .header("content-type", "application/json")
            .header("x-api-key", &key)
            .body(axum::body::Body::from(body.to_string()))
            .expect("request");
        let (header, _) = call(&app, req).await;
        assert_eq!(
            header, bearer,
            "X-Api-Key must authenticate the same key Authorization does"
        );
        assert_ne!(
            header,
            StatusCode::UNAUTHORIZED,
            "a valid key must not read as a bad one"
        );

        // And it still routes on the prefix: a login token in that header is
        // the wrong KIND of credential, not a permission problem.
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/scan")
            .header("content-type", "application/json")
            .header("x-api-key", &admin)
            .body(axum::body::Body::from(body.to_string()))
            .expect("request");
        let (status, _) = call(&app, req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a user token in X-Api-Key must not open a key-scoped route"
        );
    }

    /// A path-mapping mistake between two containers is the likeliest failure
    /// of this whole integration. The error has to diagnose itself.
    #[tokio::test]
    async fn a_path_outside_every_library_names_the_roots() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger"])).await;
        let dir = tempfile::tempdir().expect("tmp");
        let (status, _) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let elsewhere = tempfile::tempdir().expect("tmp2");
        let (status, body) = call(
            &app,
            post(
                "/api/v1/scan",
                Some(&key),
                json!({ "path": elsewhere.path(), "correlation_id": "t-42-a3f9c1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "path is not under any library root");
        assert!(
            body["roots"]
                .as_array()
                .expect("roots")
                .iter()
                .any(|r| r.as_str() == Some(&dir.path().display().to_string())),
            "the roots plurx checked must be in the body: {body}"
        );
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_with_a_reason() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger"])).await;
        let (status, body) = call(
            &app,
            post("/api/v1/scan", Some(&key), json!({ "path": "movies/Heat" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("absolute"),
            "{body}"
        );
    }

    /// Ask for a scan and get the finished answer, whichever path it took.
    ///
    /// Both outcomes are contract: 200 when the library is idle, 202 + a
    /// request id when a scan is already running (creating a library starts
    /// one, so this is the common case, not an edge). The point of the queue
    /// is that a 202 still ends in the same answer — so the helper follows it
    /// and the assertions are about the RESULT, not about which door it came
    /// through.
    async fn scan_and_settle(app: &Router, key: &str, body: Value) -> Value {
        let (status, first) = call(app, post("/api/v1/scan", Some(key), body)).await;
        if status == StatusCode::OK {
            return first;
        }
        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "unexpected scan response: {first}"
        );
        let id = first["request_id"].as_str().expect("request id").to_owned();
        for _ in 0..200 {
            let (status, rec) =
                call(app, get(&format!("/api/v1/scan/requests/{id}"), Some(key))).await;
            assert_eq!(status, StatusCode::OK, "{rec}");
            match rec["status"].as_str() {
                Some("done") => return rec,
                Some("failed") => panic!("queued scan failed: {rec}"),
                _ => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        panic!("queued scan never completed — the pending queue was dropped, not drained");
    }

    /// A request that arrives while the library is scanning must be QUEUED,
    /// not dropped. Importing a season fires one per episode within seconds,
    /// and `trigger` returning false for all but the first would leave most
    /// of the season unindexed with nothing anywhere saying so.
    #[tokio::test]
    async fn a_request_during_a_running_scan_is_queued_and_still_answered() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let movie = dir.path().join("Heat (1995)");
        std::fs::create_dir_all(&movie).expect("mkdir");
        std::fs::write(movie.join("Heat (1995).mkv"), b"x").expect("write");
        // Creating the library starts a scan, so the next request lands busy.
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;

        let rec = scan_and_settle(
            &app,
            &key,
            json!({ "path": movie, "correlation_id": "t-7-bbb" }),
        )
        .await;
        assert!(
            rec["report"].is_object(),
            "a queued request must end with a real report: {rec}"
        );
        assert_eq!(rec["correlation_id"], "t-7-bbb");
    }

    /// The happy path: a real folder under a real library, scanned now, with
    /// the answer in the response rather than a promise to look later.
    #[tokio::test]
    async fn a_scan_returns_the_report_and_what_it_placed() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let movie = dir.path().join("Heat (1995)");
        std::fs::create_dir_all(&movie).expect("mkdir");
        std::fs::write(movie.join("Heat (1995).mkv"), b"x").expect("write");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;

        let body = scan_and_settle(
            &app,
            &key,
            json!({
                "path": movie,
                "ids": { "tmdb": 949 },
                "hint": "movie",
                "correlation_id": "t-42-a3f9c1",
                "source": "monarr"
            }),
        )
        .await;
        // `added` when this request indexed it; `unchanged` when the
        // library's own start-up scan got there first. Either way the file
        // is in the library and the caller was told which item it became —
        // asserting `added` specifically would be asserting a race.
        assert!(
            body["report"]["added"].as_i64().unwrap_or(0)
                + body["report"]["unchanged"].as_i64().unwrap_or(0)
                >= 1,
            "nothing was recorded: {body}"
        );
        assert_eq!(
            body["correlation_id"], "t-42-a3f9c1",
            "the id must come back, or a caller cannot tie the answer to its question"
        );
        let items = body["items"].as_array().expect("items");
        assert_eq!(items.len(), 1);
        let item_id = items[0]["item_id"].as_i64().expect("item id");
        assert!(items[0]["file_id"].as_i64().expect("file id") > 0);

        // The ids the caller supplied were applied — this is what lets plurx
        // skip title+year guessing, which is the step that puts the wrong
        // poster on a remake.
        let (status, item) =
            call(&app, get(&format!("/api/v1/items/{item_id}"), Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            item["item"]["tmdb_id"], 949,
            "caller-supplied id not applied: {item}"
        );

        // And the request is inspectable afterwards, by the key that made it.
        let request_id = body["request_id"].as_str().expect("request id");
        let (status, rec) = call(
            &app,
            get(&format!("/api/v1/scan/requests/{request_id}"), Some(&key)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rec}");
        assert_eq!(rec["status"], "done");
        assert_eq!(rec["correlation_id"], "t-42-a3f9c1");
        assert_eq!(rec["source"], "monarr");
    }

    /// The rail is absent, not broken, when no monarr is paired — and a
    /// paired monarr that is down must not take the home screen with it.
    #[tokio::test]
    async fn the_coming_soon_rail_is_absent_unpaired_and_empty_when_monarr_is_down() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        let (status, body) = call(&app, get("/api/v1/coming-soon", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["configured"], false, "nothing paired: {body}");
        assert_eq!(body["entries"].as_array().expect("entries").len(), 0);

        // Point it at a port with nothing on it.
        let (status, _) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": "http://127.0.0.1:1", "monarr_api_key": "k" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = call(&app, get("/api/v1/coming-soon", Some(&admin))).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a monarr that is down must not fail the home screen: {body}"
        );
        assert_eq!(body["configured"], true);
        assert_eq!(body["entries"].as_array().expect("entries").len(), 0);
    }

    /// The whole point of proxying: the monarr key stays on the server. A
    /// browser holding it would hold a credential that can edit the library.
    #[tokio::test]
    async fn the_rail_forwards_the_calendar_without_handing_out_the_key() {
        use axum::routing::get as axget;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let monarr = axum::Router::new().route(
            "/api/v1/calendar",
            axget(move |headers: axum::http::HeaderMap| {
                let sink = sink.clone();
                async move {
                    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
                        sink.lock().expect("lock").push(k.to_owned());
                    }
                    axum::Json(json!([
                        { "date": "2026-08-01", "kind": "episode", "mediaItemId": 7,
                          "title": "Severance", "detail": "S02E01 — Hello", "hasFile": false }
                    ]))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, monarr).await;
        });

        let app = test_app();
        let admin = setup_admin(&app).await;
        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": base, "monarr_api_key": "monarr-secret" }),
            ),
        )
        .await;

        let (status, body) = call(&app, get("/api/v1/coming-soon", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let entries = body["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1, "{body}");
        assert_eq!(entries[0]["title"], "Severance");
        assert_eq!(entries[0]["detail"], "S02E01 — Hello");
        assert_eq!(entries[0]["has_file"], false);
        // monarr's own item id is not forwarded: it means nothing here, and
        // publishing it invites somebody to build on it.
        assert!(entries[0].get("mediaItemId").is_none(), "{body}");

        assert_eq!(
            seen.lock().expect("lock").as_slice(),
            ["monarr-secret"],
            "plurxd must present the key itself"
        );
        let raw = serde_json::to_string(&body).expect("json");
        assert!(
            !raw.contains("monarr-secret"),
            "the key reached the browser: {raw}"
        );
    }

    /// The rail shows the show's poster plurx already has.
    ///
    /// The entries used to be text-only, because the calendar carries no
    /// artwork and plurx made no attempt to find any. It does not need to
    /// fetch one: a series whose next episode is airing is a series you
    /// already have, so the poster is sitting in the artwork cache — and it
    /// is resolved by TMDB id, never by title, for the same reason every
    /// other seam in this integration is.
    #[tokio::test]
    async fn the_rail_wears_the_artwork_plurx_already_has() {
        use axum::routing::get as axget;
        let monarr = axum::Router::new().route(
            "/api/v1/calendar",
            axget(|| async {
                axum::Json(json!([
                    // In the library: TMDB 1399, and the id is the SHOW's.
                    { "date": "2026-08-01", "kind": "episode", "mediaItemId": 7,
                      "title": "The Show", "detail": "S04E02 — Griffin Incident",
                      "hasFile": false, "tmdbId": 1399 },
                    // Not in the library: nothing local to wear.
                    { "date": "2026-08-02", "kind": "movie", "mediaItemId": 8,
                      "title": "Some Film", "detail": "", "hasFile": false,
                      "tmdbId": 999999 },
                    // No ids at all: must not match the first row of its kind.
                    { "date": "2026-08-03", "kind": "movie", "mediaItemId": 9,
                      "title": "Nameless", "detail": "", "hasFile": false }
                ]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, monarr).await;
        });

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let lib = state
            .store
            .create_library(&plurx_core::domain::NewLibrary {
                name: "TV".into(),
                kind: plurx_core::domain::LibraryKind::Shows,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let show = state
            .store
            .insert_item(&plurx_core::domain::NewItem {
                library_id: lib.id,
                kind: plurx_core::domain::ItemKind::Show,
                parent_id: None,
                title: "The Show".into(),
                year: Some(2022),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("show");
        state
            .store
            .apply_metadata(
                show,
                &plurx_core::domain::MetadataPatch {
                    tmdb_id: Some(1399),
                    poster_path: Some("show.jpg".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("patch");
        // A movie that also holds TMDB 1399. The id spaces are separate, so
        // this must NOT be picked for the episode row.
        let decoy = state
            .store
            .insert_item(&plurx_core::domain::NewItem {
                library_id: lib.id,
                kind: plurx_core::domain::ItemKind::Movie,
                parent_id: None,
                title: "Decoy".into(),
                year: Some(2001),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("decoy");
        state
            .store
            .apply_metadata(
                decoy,
                &plurx_core::domain::MetadataPatch {
                    tmdb_id: Some(1399),
                    poster_path: Some("decoy.jpg".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("patch");

        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": base, "monarr_api_key": "k" }),
            ),
        )
        .await;

        let (status, body) = call(&app, get("/api/v1/coming-soon", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let e = body["entries"].as_array().expect("entries");
        assert_eq!(e.len(), 3, "{body}");

        assert_eq!(
            e[0]["poster"], "/api/v1/images/show.jpg",
            "an airing episode must wear its SHOW's poster: {body}"
        );
        assert_eq!(e[0]["item_id"], show, "and click through to the show");

        assert!(
            e[1].get("poster").is_none(),
            "a film that is not in the library has no local artwork to wear: {body}"
        );
        assert!(e[1].get("item_id").is_none());

        assert!(
            e[2].get("poster").is_none(),
            "an entry with no ids must match nothing — matching the first row \
             of its kind would put the wrong picture on the wrong title: {body}"
        );

        // monarr's ids are for resolving, not for publishing.
        assert!(e[0].get("tmdb_id").is_none(), "{body}");
        assert!(e[0].get("imdb_id").is_none(), "{body}");
    }

    /// A settings page that only repeats what you typed cannot answer "did
    /// it work". This one says whether monarr actually answered.
    #[tokio::test]
    async fn the_monarr_card_says_whether_the_pairing_actually_works() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        let (status, body) = call(&app, get("/api/v1/monarr/status", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["configured"], false, "nothing paired yet: {body}");
        assert_eq!(body["reachable"], false);

        // Paired but pointing at nothing: configured, and honest about it.
        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": "http://127.0.0.1:1", "monarr_api_key": "k" }),
            ),
        )
        .await;
        let (_, body) = call(&app, get("/api/v1/monarr/status", Some(&admin))).await;
        assert_eq!(body["configured"], true);
        assert_eq!(body["reachable"], false, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains("cannot reach"),
            "the reason must be readable, got {body}"
        );

        // A monarr that answers, and one that rejects the key: different
        // problems, different fixes, so they must not read the same.
        use axum::routing::get as axget;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let monarr = axum::Router::new().route(
                "/api/v1/system/status",
                axget(|headers: axum::http::HeaderMap| async move {
                    if headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some("right") {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(json!({ "version": "0.9.0" })).into_response()
                }),
            );
            let _ = axum::serve(listener, monarr).await;
        });

        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": base.clone(), "monarr_api_key": "wrong" }),
            ),
        )
        .await;
        let (_, body) = call(&app, get("/api/v1/monarr/status", Some(&admin))).await;
        assert_eq!(body["reachable"], false);
        assert!(
            body["error"]
                .as_str()
                .unwrap_or("")
                .contains("rejected the API key"),
            "a bad key must not read as an unreachable server: {body}"
        );

        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": base, "monarr_api_key": "right" }),
            ),
        )
        .await;
        let (_, body) = call(&app, get("/api/v1/monarr/status", Some(&admin))).await;
        assert_eq!(body["reachable"], true, "{body}");
        assert_eq!(body["version"], "0.9.0");

        // A bare host is what a person types, and it used to produce
        // "builder error" — a message about our HTTP client rather than
        // about their setting. It is completed on save, and the settings
        // response shows the completed form so the fix is visible rather
        // than magic.
        let bare = base.trim_start_matches("http://").to_owned();
        let (_, body) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": bare, "monarr_api_key": "right" }),
            ),
        )
        .await;
        assert_eq!(
            body["monarr_url"], base,
            "a schemeless host must be completed on save, and shown back: {body}"
        );
        let (_, body) = call(&app, get("/api/v1/monarr/status", Some(&admin))).await;
        assert_eq!(
            body["reachable"], true,
            "a bare host must still work: {body}"
        );
    }

    /// The URL people actually type, and what plurx dials for it.
    #[test]
    fn a_bare_host_is_completed_with_monarrs_own_default_port() {
        use super::comingsoon::normalize_monarr_url as norm;
        // Nothing but a host: assume http and monarr's default port, because
        // there is nothing else it could mean.
        assert_eq!(
            norm("host.docker.internal"),
            "http://host.docker.internal:7676"
        );
        assert_eq!(norm("  monarr/ "), "http://monarr:7676");
        // A port given without a scheme is a port they chose: keep it.
        assert_eq!(norm("monarr:9000"), "http://monarr:9000");
        // A scheme given is a decision made — respected in full, including
        // the implied port. Guessing 7676 onto an https URL behind a reverse
        // proxy would break a setup that was already correct.
        assert_eq!(
            norm("https://monarr.example.com"),
            "https://monarr.example.com"
        );
        assert_eq!(norm("http://10.0.0.4:7676/"), "http://10.0.0.4:7676");
        // Empty stays empty: "not configured" is a state, not a bad URL.
        assert_eq!(norm("   "), "");
    }

    /// A typo must come back as a typo, never as nonsense.
    ///
    /// The first version of this matched on the literal `://`, so one
    /// mistyped slash did not look like a scheme, was treated as a bare
    /// hostname, and came back as `http://http:/monarr:7676:7676`. The
    /// person then cannot see what they typed, and the error is about our
    /// guess rather than their input.
    #[test]
    fn a_mistyped_url_is_repaired_or_returned_untouched_never_mangled() {
        use super::comingsoon::normalize_monarr_url as norm;

        // One slash is unambiguous: nothing else could be meant.
        assert_eq!(norm("http:/monarr:7676"), "http://monarr:7676");
        assert_eq!(
            norm("https:/monarr.example.com"),
            "https://monarr.example.com"
        );
        // ...and it must not then also acquire a second port.
        assert!(!norm("http:/monarr:7676").contains(":7676:7676"));

        // Anything that cannot be turned into a real URL comes straight
        // back (bar the trailing-slash trim), so the failure message names
        // their string rather than our guess at it.
        for bad in ["http://", "http:", "://monarr", ":7676", "http://:7676"] {
            let out = norm(bad);
            assert_eq!(
                out, bad,
                "unrepairable input must come back exactly as typed, got {out:?}"
            );
        }

        // Nothing this function produces may fail to parse. That is the
        // property the original bug violated.
        for input in [
            "monarr",
            "monarr:7676",
            "http:/monarr:7676",
            "http://monarr:7676",
            "https://monarr.example.com/",
            "10.0.0.4",
            "[::1]:7676",
        ] {
            let out = norm(input);
            assert!(
                reqwest::Url::parse(&out).is_ok(),
                "norm({input:?}) produced {out:?}, which is not a URL"
            );
        }
    }

    /// Nothing is sent until an admin turns it on. This is the guard on a
    /// decision with a cost: per-user watch state is viewing history, and
    /// this ships it to an application that has no other reason to hold it.
    #[tokio::test]
    async fn watch_state_goes_nowhere_until_someone_turns_it_on() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let lib = state
            .store
            .create_library(&plurx_core::domain::NewLibrary {
                name: "Movies".into(),
                kind: plurx_core::domain::LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = state
            .store
            .insert_item(&plurx_core::domain::NewItem {
                library_id: lib.id,
                kind: plurx_core::domain::ItemKind::Movie,
                parent_id: None,
                title: "Heat".into(),
                year: Some(1995),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        state
            .store
            .apply_metadata(
                movie,
                &plurx_core::domain::MetadataPatch {
                    tmdb_id: Some(949),
                    ..Default::default()
                },
            )
            .await
            .expect("ids");

        // Off (the default): marking watched queues nothing.
        let (status, _) = call(
            &app,
            post(
                &format!("/api/v1/items/{movie}/scrobble"),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        settle().await;
        assert!(
            state.store.due_watched(10).await.expect("due").is_empty(),
            "watch state left plurx without anyone enabling it"
        );

        // On: an actual transition queues one, addressed by id.
        let (status, _) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": "http://monarr:7676", "monarr_api_key": "k",
                        "monarr_watched_sync": true }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Marking something already watched is not news. The film is still
        // watched from the click above, so this announces nothing — otherwise
        // a double-click on a series would re-announce every episode in it.
        let (status, _) = call(
            &app,
            post(
                &format!("/api/v1/items/{movie}/scrobble"),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        settle().await;
        assert!(
            state.store.due_watched(10).await.expect("due").is_empty(),
            "a redundant mark announced itself anyway"
        );

        // A real un-watch → watch round trip does announce, exactly once.
        for action in ["unscrobble", "scrobble"] {
            let (status, _) = call(
                &app,
                post(
                    &format!("/api/v1/items/{movie}/{action}"),
                    Some(&admin),
                    json!({}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        settle().await;

        let queued = state.store.due_watched(10).await.expect("due");
        assert_eq!(queued.len(), 1, "nothing queued once enabled");
        let ev: serde_json::Value = serde_json::from_str(&queued[0].payload).expect("payload json");
        assert_eq!(ev["event"], "watched");
        assert_eq!(ev["kind"], "movie");
        assert_eq!(ev["tmdb"], 949);
        assert_eq!(ev["user"], "paul", "per-user by decision: {ev}");
        assert!(ev["watched_at"].as_i64().unwrap_or(0) > 0);
    }

    /// An item monarr could not match is not worth sending. Sending a title
    /// and letting monarr guess is the exact mistake the rest of this
    /// integration exists to remove, just pointed the other way.
    #[tokio::test]
    async fn an_item_with_no_ids_is_not_announced() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let lib = state
            .store
            .create_library(&plurx_core::domain::NewLibrary {
                name: "Movies".into(),
                kind: plurx_core::domain::LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("lib");
        let movie = state
            .store
            .insert_item(&plurx_core::domain::NewItem {
                library_id: lib.id,
                kind: plurx_core::domain::ItemKind::Movie,
                parent_id: None,
                title: "Some Home Movie".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "monarr_url": "http://monarr:7676", "monarr_api_key": "k",
                        "monarr_watched_sync": true }),
            ),
        )
        .await;

        call(
            &app,
            post(
                &format!("/api/v1/items/{movie}/scrobble"),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        settle().await;
        assert!(
            state.store.due_watched(10).await.expect("due").is_empty(),
            "an unidentifiable item was announced anyway"
        );
    }

    /// `on_watched` spawns, so a test has to let the runtime get to it.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// The counters that answer "is the fast path actually being used?".
    ///
    /// A plurx scanning 400 times a day tells you nothing. 398 scheduled and
    /// 2 targeted tells you the integration has quietly stopped and the slow
    /// sweep is carrying everything — which looks completely fine from the
    /// library, just slower, for as long as nobody checks.
    #[tokio::test]
    async fn scans_and_notifications_are_counted_by_what_asked_for_them() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let movie = dir.path().join("Heat (1995)");
        std::fs::create_dir_all(&movie).expect("mkdir");
        std::fs::write(movie.join("Heat (1995).mkv"), b"x").expect("write");
        // Creating a library is a manual scan.
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;
        scan_and_settle(&app, &key, json!({ "path": movie, "source": "monarr" })).await;

        // A request plurx cannot place still counts as contact: it proves
        // the caller reached us with a working key, which is what separates
        // "fix the path mapping" from "check the URL".
        let (status, _) = call(
            &app,
            post(
                "/api/v1/scan",
                Some(&key),
                json!({ "path": "/nowhere/at/all", "source": "monarr" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (_, counts) = state.jobs.metrics().snapshot();
        assert_eq!(counts, 2, "both requests reached plurx, so both count");

        let (by_trigger, _) = state.jobs.metrics().snapshot();
        let map: std::collections::HashMap<_, _> = by_trigger.into_iter().collect();
        assert!(map["manual"] >= 1, "creating a library is a manual scan");
        assert!(
            map["targeted"] >= 1,
            "the placed request was a targeted scan"
        );
        assert_eq!(map["scheduled"], 0);

        // And it is all visible without reading the log.
        let (status, sys) = call(&app, get("/api/v1/system", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{sys}");
        assert_eq!(sys["integration"]["notifications_received"], 2);
        assert_eq!(sys["integration"]["scans_by_trigger"]["targeted"], 1);
        assert_eq!(
            sys["integration"]["last_notification_source"], "monarr",
            "who last called must be visible: {sys}"
        );

        // The Prometheus surface carries the same two facts.
        let (status, _) = call(&app, get("/metrics", None)).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The seam between "another app told us the id" and "go and enrich it".
    ///
    /// These two features can each be right and still combine into an item
    /// that is permanently blank: enrichment used to read "has a TMDB id" as
    /// "already enriched", so an item that ARRIVED with an id would never be
    /// given a title, an overview or a poster. Nothing errors; the item just
    /// sits there looking like a filename forever. Asserted end to end
    /// because neither half's own tests can see it.
    #[tokio::test]
    async fn an_item_that_arrives_with_an_id_is_still_queued_for_enrichment() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let movie = dir.path().join("Heat (1995)");
        std::fs::create_dir_all(&movie).expect("mkdir");
        std::fs::write(movie.join("Heat (1995).mkv"), b"x").expect("write");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;

        let body = scan_and_settle(
            &app,
            &key,
            json!({ "path": movie, "ids": { "tmdb": 949 }, "hint": "movie", "source": "monarr" }),
        )
        .await;
        let item_id = body["items"][0]["item_id"].as_i64().expect("item id");

        let queued = state
            .store
            .items_needing_metadata(None, false, None)
            .await
            .expect("needing");
        let mine = queued.iter().find(|i| i.id == item_id);
        assert!(
            mine.is_some(),
            "the id-carrying item was dropped from the enrichment queue — it \
             would stay titled after its own filename forever"
        );
        assert_eq!(
            mine.and_then(|i| i.tmdb_id),
            Some(949),
            "and it must carry the id INTO enrichment, so the search is skipped"
        );
    }

    /// A scan of one folder must not disturb the rest of the library. This is
    /// the no-prune property, asserted through the HTTP surface as well as in
    /// the core, because it is the one that would destroy data.
    #[tokio::test]
    async fn a_targeted_scan_leaves_the_rest_of_the_library_alone() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        for name in ["Heat (1995)", "Alien (1979)"] {
            let d = dir.path().join(name);
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join(format!("{name}.mkv")), b"x").expect("write");
        }
        let (_, lib) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [dir.path()] }),
            ),
        )
        .await;
        let lib_id = lib["id"].as_i64().expect("library id");

        // Index both, the normal way.
        for name in ["Heat (1995)", "Alien (1979)"] {
            scan_and_settle(&app, &key, json!({ "path": dir.path().join(name) })).await;
        }
        let (_, before) = call(
            &app,
            get(&format!("/api/v1/libraries/{lib_id}/items"), Some(&admin)),
        )
        .await;
        assert_eq!(before["items"].as_array().expect("items").len(), 2);

        // Re-scan just one of them.
        scan_and_settle(
            &app,
            &key,
            json!({ "path": dir.path().join("Heat (1995)") }),
        )
        .await;

        let (_, after) = call(
            &app,
            get(&format!("/api/v1/libraries/{lib_id}/items"), Some(&admin)),
        )
        .await;
        assert_eq!(
            after["items"].as_array().expect("items").len(),
            2,
            "scanning one folder removed the other — a targeted scan must never prune"
        );
    }

    #[tokio::test]
    async fn an_unknown_request_id_is_a_404() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["status:read"])).await;
        let (status, _) = call(&app, get("/api/v1/scan/requests/sr-nope", Some(&key))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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

    /// The storage numbers have to reach `/system`, and an unmeasured server
    /// has to say so rather than reporting zeroes. A probe that runs perfectly
    /// and never surfaces is the same to an operator as one that never ran.
    #[tokio::test]
    async fn storage_is_reported_and_remeasurable() {
        let app = test_app();
        let admin = setup_admin(&app).await;

        // Before any probe: present, and honestly empty.
        let (status, body) = call(&app, get("/api/v1/system", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["storage"]["ran"], false);
        assert!(body["storage"]["measured_at"].is_null());

        // Re-measuring with no libraries is a no-op rather than an error: there
        // is nothing to read, which is not a failure to read.
        let (status, _) = call(
            &app,
            post("/api/v1/system/storage", Some(&admin), json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // With a library, the probe runs and the mount appears — here with no
        // file big enough to sample, which is the honest answer for the path
        // and still tells the operator the path resolved.
        let dir = tempfile::tempdir().expect("tempdir");
        let (status, _) = call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({"name": "M", "kind": "movies", "paths": [dir.path()]}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, st) = call(
            &app,
            post("/api/v1/system/storage", Some(&admin), json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(st["ran"], true);
        assert_eq!(st["mounts"].as_array().expect("mounts").len(), 1);
        assert!(st["mounts"][0]["note"].is_string());

        // And it is the same report GET /system serves, not a second one.
        let (_, body) = call(&app, get("/api/v1/system", Some(&admin))).await;
        assert_eq!(body["storage"]["measured_at"], st["measured_at"]);
    }

    #[tokio::test]
    async fn storage_remeasure_is_admin_only() {
        let app = test_app();
        let (status, _) = call(&app, post("/api/v1/system/storage", None, json!({}))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
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

    // The gap this closes: a producer pass holds an encoder for up to six
    // hours, and until it reported itself the only way to find out what a busy
    // ffmpeg was doing — or why it had chosen that file — was `ps` on the box.
    #[tokio::test]
    async fn a_pre_transcode_pass_says_what_it_is_doing_and_why() {
        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;

        let (_, body) = call(&app, get("/api/v1/activity", Some(&admin))).await;
        assert!(
            body.as_array().expect("array").is_empty(),
            "no pass running = nothing claimed"
        );

        state
            .jobs
            .set_producing(Some(crate::state::ProducingNow {
                title: "Willow".into(),
                reason: crate::produce::REASON_IN_PROGRESS,
                index: 2,
                total: 12,
            }))
            .await;

        let (status, body) = call(&app, get("/api/v1/activity", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        let acts = body.as_array().expect("array");
        let a = acts
            .iter()
            .find(|a| a["kind"] == "produce")
            .expect("the pass is in the activity feed");
        assert!(
            a["label"].as_str().unwrap_or_default().contains("Willow"),
            "the title is named: {a}"
        );
        let detail = a["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("in progress") && detail.contains("2 of 12"),
            "why it was chosen, and how far in: {detail}"
        );

        // And on the page that has a stop button next to it.
        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert_eq!(page["producing"]["title"], "Willow");
    }

    #[tokio::test]
    async fn stopping_the_producer_is_admin_only_and_needs_one_to_be_running() {
        let (app, state) = test_app_with_state();
        let (status, _) = call(&app, delete("/api/v1/activity/producer", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        call(
            &app,
            post(
                "/api/v1/users",
                Some(&admin),
                json!({ "username": "viewer", "password": "longenough" }),
            ),
        )
        .await;
        let (_, login) = call(
            &app,
            post(
                "/api/v1/auth/login",
                None,
                json!({ "username": "viewer", "password": "longenough" }),
            ),
        )
        .await;
        let viewer = login["token"].as_str().expect("token").to_owned();
        let (status, _) = call(&app, delete("/api/v1/activity/producer", Some(&viewer))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "not a viewer's button");

        // Nothing running: say so rather than pretending to have stopped it.
        let (status, _) = call(&app, delete("/api/v1/activity/producer", Some(&admin))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert!(
            !state.jobs.stop_producing(),
            "idle producer cannot be stopped"
        );
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
            test_dirs(&base),
            "test-node".into(),
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

    /// A big remux gets told to go through MSE; an ordinary direct play does
    /// not. Both halves matter — the second is the regression that would
    /// reroute a whole library's worth of files that were working.
    #[tokio::test]
    async fn a_high_bitrate_remux_is_hinted_toward_segments() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // The seeded file is an 8 Mb/s H.264 MP4: a browser that takes it
        // direct-plays, and there is no transport choice to hint about.
        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0",
                    s.file
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["method"], "direct_play");
        assert!(body["prefer_segmented"].is_null(), "{body}");

        // A 69 Mb/s HEVC MKV with TrueHD: the video decodes, the container and
        // the audio do not, so it remuxes — and it is far too fast for the
        // browser's 2.2 s progressive buffer.
        let dir = std::env::temp_dir().join(format!("plurx-big-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mediadir");
        let path = dir.join("Big.mkv");
        std::fs::write(&path, b"\x1a\x45\xdf\xa3 tiny placeholder").expect("write");
        let probe = plurx_core::domain::ProbeResult {
            duration_ms: Some(9_000_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            bitrate: Some(69_000_000),
            audio_streams: vec![plurx_core::domain::AudioStream {
                index: 0,
                codec: "truehd".into(),
                channels: Some(8),
                language: Some("eng".into()),
                default: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let big = state
            .store
            .upsert_file(s.movie, &path.to_string_lossy(), 99, 1, &probe)
            .await
            .expect("file");

        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{big}/decision?vcodec=h264,hevc&acodec=aac&container=mp4&hdr=1"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["method"], "remux", "{body}");
        let why = body["prefer_segmented"].as_str().unwrap_or_default();
        assert!(why.contains("69 Mb/s"), "{body}");
    }

    /// `/decision` must say what to DO, not just what was decided. Clients
    /// that re-derived policy from `method` got it differently wrong on every
    /// platform — Android played transcode verdicts through the copy-only
    /// progressive path, Apple re-encoded remux verdicts at a hardcoded
    /// 1080p — so the response now carries an executable plan per verdict.
    #[tokio::test]
    async fn the_decision_carries_an_executable_delivery_plan() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // Direct play: the plan is the file itself.
        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0",
                    s.file
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["delivery"]["mode"], "direct", "{body}");
        assert_eq!(body["delivery"]["url"], body["play_url"], "{body}");

        // Remux (HEVC decodes, MKV + TrueHD don't): the plan offers both
        // envelopes for the same copied bytes — the progressive URL, and the
        // copy-session POST for players that need HLS transport — and settles
        // the audio question so no client re-derives it.
        let dir = std::env::temp_dir().join(format!("plurx-plan-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("mediadir");
        let path = dir.join("Plan.mkv");
        std::fs::write(&path, b"\x1a\x45\xdf\xa3 tiny placeholder").expect("write");
        let probe = plurx_core::domain::ProbeResult {
            duration_ms: Some(9_000_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            bitrate: Some(69_000_000),
            audio_streams: vec![plurx_core::domain::AudioStream {
                index: 0,
                codec: "truehd".into(),
                channels: Some(8),
                language: Some("eng".into()),
                default: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mkv = state
            .store
            .upsert_file(s.movie, &path.to_string_lossy(), 98, 1, &probe)
            .await
            .expect("file");
        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{mkv}/decision?vcodec=h264,hevc&acodec=aac&container=mp4&hdr=1"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["method"], "remux", "{body}");
        assert_eq!(body["delivery"]["mode"], "remux", "{body}");
        assert_eq!(
            body["delivery"]["url"],
            format!("/api/v1/files/{mkv}/stream.mp4"),
            "{body}"
        );
        assert_eq!(
            body["delivery"]["sessions_url"],
            format!("/api/v1/files/{mkv}/hls/sessions"),
            "{body}"
        );
        assert_eq!(
            body["delivery"]["aac"], true,
            "TrueHD is not in the client's acodec list, so the copy session must re-encode it: {body}"
        );

        // Transcode: the plan is a session create, and deliberately carries no
        // height — Auto belongs to the server (the rung depends on which
        // encoder wins, which only the create response knows).
        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0&force=transcode",
                    s.file
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["method"], "transcode", "{body}");
        assert_eq!(body["delivery"]["mode"], "transcode", "{body}");
        assert_eq!(
            body["delivery"]["sessions_url"],
            format!("/api/v1/files/{}/hls/sessions", s.file),
            "{body}"
        );
        assert!(
            body["delivery"].get("height").is_none(),
            "Auto is the server's call, not a field in the plan: {body}"
        );
    }

    /// ADAPTIVE-QUALITY Phase 1, the server half: the decision and the
    /// session response both carry the source-filtered ladder, a stray
    /// explicit height snaps onto it, and a request for the source's own
    /// height — the Original/forced-burn promise — passes through unsnapped.
    #[tokio::test]
    async fn the_ladder_is_advertised_and_stray_heights_snap() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // A 900p source: not itself a rung, which is the interesting case.
        let dir = std::env::temp_dir().join(format!("plurx-ladder-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("Odd.mkv");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("write");
        let probe = plurx_core::domain::ProbeResult {
            duration_ms: Some(600_000),
            container: Some("mkv".into()),
            video_codec: Some("h264".into()),
            width: Some(1600),
            height: Some(900),
            audio_streams: vec![plurx_core::domain::AudioStream {
                index: 0,
                codec: "aac".into(),
                channels: Some(2),
                default: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let odd = state
            .store
            .upsert_file(s.movie, &path.to_string_lossy(), 77, 1, &probe)
            .await
            .expect("file");

        // The decision advertises the ladder, filtered to the source: a 900p
        // file offers 720 and below, priced both ways.
        let (status, body) = call(
            &app,
            get(
                &format!("/api/v1/files/{odd}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let heights: Vec<i64> = body["ladder"]
            .as_array()
            .expect("ladder")
            .iter()
            .map(|r| r["height"].as_i64().expect("height"))
            .collect();
        assert_eq!(
            heights,
            vec![720, 480, 360],
            "source-filtered, top first: {body}"
        );
        assert_eq!(body["ladder"][0]["total_kbps"], 4_160, "{body}");
        assert_eq!(body["ladder"][0]["peak_kbps"], 6_160, "{body}");

        // A stray explicit height snaps onto the ladder (850 → 720)…
        let (status, body) = call(
            &app,
            post(
                &format!("/api/v1/files/{odd}/hls/sessions"),
                Some(&admin),
                json!({ "playback_id": "pb-snap", "height": 850 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body["ladder"].as_array().is_some_and(|l| !l.is_empty()),
            "the session response carries the ladder too: {body}"
        );
        let sid = body["session_id"].as_str().expect("sid").to_owned();
        let info = state
            .transcode
            .session_status(&sid)
            .await
            .expect("session status");
        assert_eq!(info.target_height, 720, "850 is a stray; 720 is its rung");
        // Release it before the next create: this test is about the snap,
        // and on a 2-core runner the machine-derived CPU pool would refuse
        // a second coexisting software session for the wrong reason.
        assert!(state.transcode.stop_session(&sid, "test").await);

        // …and the source's own height does not: that request is the
        // Original/forced-burn promise, and snapping it to 720 would be
        // exactly the silent downgrade the burn fix removed.
        let (status, body) = call(
            &app,
            post(
                &format!("/api/v1/files/{odd}/hls/sessions"),
                Some(&admin),
                json!({ "playback_id": "pb-promise", "height": 900 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sid = body["session_id"].as_str().expect("sid").to_owned();
        let info = state
            .transcode
            .session_status(&sid)
            .await
            .expect("session status");
        assert_eq!(
            info.target_height, 900,
            "the source's own height is a promise"
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

        // Refresh artwork: reanalyze's sibling, same auth and the same 404. No
        // TMDB key is configured in this fixture, so the pass has nothing to
        // ask — the endpoint still has to answer honestly rather than error.
        assert_eq!(
            call(
                &app,
                post(
                    &format!("/api/v1/items/{}/refresh-artwork", s.movie),
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
                post(
                    "/api/v1/items/999999/refresh-artwork",
                    Some(&admin),
                    json!({})
                )
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let (st, art) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/refresh-artwork", s.movie),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "body: {art:?}");
        assert!(art["poster"].is_null(), "no provider key, no poster");

        // Schedules: minutes, 0 = off, and a floor under anything non-zero so a
        // dropdown can't turn a NAS into a treadmill.
        assert_eq!(
            call(
                &app,
                put(
                    &format!("/api/v1/libraries/{}/schedule", s.lib),
                    Some(&admin),
                    json!({ "scan_interval_mins": 5 })
                )
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (st, lib) = call(
            &app,
            put(
                &format!("/api/v1/libraries/{}/schedule", s.lib),
                Some(&admin),
                json!({ "scan_interval_mins": 360, "refresh_interval_mins": 10080 }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(lib["scan_interval_mins"], 360);
        assert_eq!(lib["refresh_interval_mins"], 10080);
        // Readable back through the library list, which is where the settings
        // page gets the value it renders.
        let (_, libs) = call(&app, get("/api/v1/libraries", Some(&admin))).await;
        assert_eq!(libs[0]["scan_interval_mins"], 360);
        // Turning it off is an ordinary update, not a special case.
        let (_, lib) = call(
            &app,
            put(
                &format!("/api/v1/libraries/{}/schedule", s.lib),
                Some(&admin),
                json!({ "scan_interval_mins": 0, "refresh_interval_mins": 0 }),
            ),
        )
        .await;
        assert_eq!(lib["scan_interval_mins"], 0);

        // Scan-at-startup is a plain boolean round-trip through the same
        // settings endpoint, off unless asked for.
        let (_, before) = call(&app, get("/api/v1/settings", Some(&admin))).await;
        assert_eq!(before["scan_on_startup"], false);
        // The artwork sweep is the one job that reads back as on before
        // anybody has touched it. A 0 here would mean a fresh install never
        // repairs a poster it failed to download.
        assert_eq!(before["artwork_retry_mins"], 30);
        let (st, after) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "scan_on_startup": true, "probe_retry_mins": 1440,
                        "artwork_retry_mins": 0 }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(after["scan_on_startup"], true);
        assert_eq!(after["probe_retry_mins"], 1440);
        assert_eq!(
            after["artwork_retry_mins"], 0,
            "an explicit 0 must survive the default, or the job cannot be turned off"
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
    async fn a_progressive_streams_status_is_private_to_its_owner() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;

        // Nothing registered: not found, not an empty success. The player uses
        // the difference to decide whether it has a server to report on at all.
        assert_eq!(
            call(&app, get("/api/v1/stream/pb-1-s1/status", Some(&admin)))
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // The id is the client's own playback id rather than a capability, so
        // it is guessable by construction — ownership is what protects it.
        let (_stream, _guard) = state.streams.register("pb-1-s1", 9999, 42, 4.0);
        assert_eq!(
            call(&app, get("/api/v1/stream/pb-1-s1/status", Some(&admin)))
                .await
                .0,
            StatusCode::NOT_FOUND,
            "another user's stream is not visible"
        );
        assert_eq!(
            call(&app, get("/api/v1/stream/pb-1-s1/status", None))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn marking_a_show_watched_reaches_its_episodes() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        // A show has no watch state of its own, so the detail response carries
        // a rollup instead — without it a client has nothing to label a
        // mark-watched control from, since the response only reaches as far as
        // the seasons and those are empty too.
        let (st, show) = call(
            &app,
            get(&format!("/api/v1/items/{}", s.show), Some(&admin)),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(show["item"]["rollup"]["leaves"], 1);
        assert_eq!(show["item"]["rollup"]["watched"], 0);
        assert!(show["item"]["watch"].is_null(), "a show is not watchable");

        // Marking the show marks the episode underneath it.
        let (st, r) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/scrobble", s.show),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["updated"], 1);
        let (_, ep) = call(&app, get(&format!("/api/v1/items/{}", s.ep), Some(&admin))).await;
        assert_eq!(ep["item"]["watch"]["watched"], true);
        let (_, show) = call(
            &app,
            get(&format!("/api/v1/items/{}", s.show), Some(&admin)),
        )
        .await;
        assert_eq!(show["item"]["rollup"]["watched"], 1);

        // A second click is honest about having done nothing.
        let (_, r) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/scrobble", s.show),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(r["updated"], 0);

        // And unscrobble walks the same tree back.
        let (st, r) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/unscrobble", s.show),
                Some(&admin),
                json!({}),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["updated"], 1);
        let (_, ep) = call(&app, get(&format!("/api/v1/items/{}", s.ep), Some(&admin))).await;
        assert_eq!(ep["item"]["watch"]["watched"], false);

        // A movie carries no rollup — it has a `watch` row of its own, and the
        // client picks its control off that.
        let (_, movie) = call(
            &app,
            get(&format!("/api/v1/items/{}", s.movie), Some(&admin)),
        )
        .await;
        assert!(movie["item"]["rollup"].is_null());
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

    async fn body_of(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let resp = app.clone().oneshot(req).await.expect("resp");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        (status, bytes.to_vec())
    }

    /// The §3.3 contract: the first request extracts and files the VTT under
    /// the source's fingerprint; the second is served from that file without
    /// ffmpeg reading the source again — proven by editing the cached entry
    /// and getting the edit back.
    #[tokio::test]
    async fn subtitle_extraction_is_cached_by_source_identity() {
        crate::transcode::require_ffmpeg();
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;

        // A real MKV with a real SRT track, so extraction actually runs.
        let dir = std::env::temp_dir().join(format!("plurx-subs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let srt = dir.join("s.srt");
        std::fs::write(&srt, "1\n00:00:00,000 --> 00:00:01,000\nhello plurx\n\n").expect("srt");
        let mkv = dir.join("subbed.mkv");
        let made = std::process::Command::new(crate::ffmpeg::ffmpeg_bin())
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=10:d=1",
                "-i",
            ])
            .arg(&srt)
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-c:s", "srt"])
            .arg(&mkv)
            .output()
            .expect("spawn ffmpeg");
        assert!(
            made.status.success(),
            "fixture mux failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );

        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Subbed".into(),
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
                title: "Subbed".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let probe = ProbeResult {
            duration_ms: Some(1_000),
            container: Some("mkv".into()),
            video_codec: Some("h264".into()),
            subtitle_streams: vec![plurx_core::domain::SubtitleStream {
                index: 0,
                codec: "subrip".into(),
                language: Some("eng".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let file = state
            .store
            .upsert_file(movie, &mkv.to_string_lossy(), 4242, 7, &probe)
            .await
            .expect("file");

        let (status, body) = body_of(
            &app,
            get_q(&format!("/api/v1/files/{file}/subs/0?token={admin}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("WEBVTT"), "{text}");
        assert!(text.contains("hello plurx"), "{text}");

        // Exactly one entry, keyed by (file, stream, size, mtime).
        let cached = state.subs_dir.join(format!("f{file}-s0-4242-7.vtt"));
        assert!(
            tokio::fs::metadata(&cached).await.is_ok(),
            "the extraction must be filed under the source's fingerprint"
        );

        // Edit the cached entry; a second request returns the edit — which it
        // can only do by serving the cache instead of re-reading the source.
        tokio::fs::write(&cached, "WEBVTT\n\ncache-proof\n")
            .await
            .expect("edit cache");
        let (status, body) = body_of(
            &app,
            get_q(&format!("/api/v1/files/{file}/subs/0?token={admin}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&body).contains("cache-proof"),
            "the second request must come from the cache"
        );

        // A replaced source is a different fingerprint: not served the stale
        // entry.
        let refreshed = state
            .store
            .upsert_file(movie, &mkv.to_string_lossy(), 4242, 8, &probe)
            .await
            .expect("refreshed");
        assert_eq!(refreshed, file, "same row, new mtime");
        let (status, body) = body_of(
            &app,
            get_q(&format!("/api/v1/files/{file}/subs/0?token={admin}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&body).contains("hello plurx"),
            "a changed fingerprint must re-extract, not serve the stale entry"
        );
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
