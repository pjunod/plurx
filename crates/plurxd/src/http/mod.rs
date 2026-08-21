//! HTTP surface of a plurxd node: liveness/readiness plus the native `/api/v1`.
//!
//! The native API is JSON. An OpenAPI description will be generated from these
//! routes as they stabilize (clients on five platforms consume it). The
//! Plex-compat façade (a separate crate) and playback routes mount alongside
//! in later slices.

mod auth;
mod browse;
mod cluster;
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
mod network;
mod offline;
mod pgs_overlay;
mod photos;
mod plex;
pub(crate) mod publication;
mod reading;
mod scan;
pub(crate) mod stream;
pub(crate) mod system;
mod trakt;

/// Unmodified User-Agent strings as the shipping clients actually send them.
/// Shared with the HTTP wire tests so the write-then-read proof and the
/// classifier unit tests cannot drift onto different inputs.
#[cfg(test)]
pub(crate) mod test_agents {
    pub(crate) const CHROME_WINDOWS_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
    pub(crate) const CHROME_MACOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
    pub(crate) const CHROME_ANDROID_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36";
    pub(crate) const SAFARI_MACOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15";
    pub(crate) const SAFARI_IOS_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Mobile/15E148 Safari/604.1";
    pub(crate) const EDGE_WINDOWS_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
    pub(crate) const FIREFOX_WINDOWS_UA: &str =
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:130.0) Gecko/20100101 Firefox/130.0";
    pub(crate) const FIREFOX_IOS_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/130.0 Mobile/15E148 Safari/605.1.15";
    pub(crate) const APPLE_NATIVE_UA: &str = "Plurx/59 CFNetwork/1498.700.2 Darwin/23.6.0";
    pub(crate) const ANDROID_NATIVE_UA: &str = "okhttp/5.1.0";
}

mod users;
mod watch;
mod web;

use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
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
            "/activity/offline/{id}",
            axum::routing::delete(system::stop_offline_package),
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
        .route("/system/playback-events", get(system::playback_events))
        // Membership control is admin-only. The two join routes below are the
        // exception: their single-use token is its own narrow credential.
        .route("/cluster/join-tokens", post(cluster::issue_join_token))
        .route("/cluster/nodes", get(cluster::nodes))
        .route("/cluster/nodes/{node_id}", delete(cluster::remove_node))
        .route("/cluster/join/redeem", post(cluster::redeem_join))
        .route("/cluster/join/finalize", post(cluster::finalize_join))
        // What the libraries hold, in transcoder terms — the census PERF-PLAN
        // §5 needs to say whether the GPU tone-map reaches a real library.
        .route("/system/library-shape", get(system::library_shape))
        // Re-measure storage. POST because it costs real I/O against the
        // library, and separate from GET /system so that reading the last
        // numbers is never the thing that goes and takes new ones.
        .route("/system/storage", post(system::remeasure_storage))
        .route(
            "/system/search-index/rebuild",
            post(system::rebuild_search_index),
        )
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
        .route(
            "/libraries/{id}/root-identity/reset",
            post(libraries::reset_root_identity),
        )
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
        .route(
            "/items/{id}/reading-state",
            get(reading::get_state)
                .put(reading::put_state)
                .delete(reading::delete_state)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        // Playback
        .route("/files/{id}/decision", get(stream::decision))
        .route("/files/{id}/audio-offset", put(stream::set_audio_offset))
        // App-managed offline viewing. JSON/package ownership uses bearer
        // auth; only immutable child media uses the package-scoped capability.
        .route("/files/{id}/offline-options", get(offline::options))
        .route("/files/{id}/offline-packages", post(offline::create))
        .route(
            "/offline/packages/{id}",
            get(offline::package_status).delete(offline::delete_package),
        )
        .route("/offline/packages/{id}/lease", put(offline::put_lease))
        .route(
            "/offline/packages/{id}/complete",
            post(offline::complete_package),
        )
        .route("/offline/media/{token}/master.m3u8", get(offline::master))
        .route("/offline/media/{token}/index.m3u8", get(offline::playlist))
        .route(
            "/offline/media/{token}/subs/{index}/{segment}",
            get(offline::subtitle),
        )
        .route("/offline/media/{token}/{segment}", get(offline::segment))
        .route("/files/{id}/direct", get(stream::direct))
        .route("/files/{id}/content", get(stream::book_content))
        .route("/files/{id}/publication", post(publication::open))
        .route("/publication/{session}", delete(publication::close))
        .route(
            "/publication/{session}/{*resource}",
            get(publication::resource),
        )
        .route("/files/{id}/stream.mp4", get(stream::stream_mp4))
        .route(
            "/files/{id}/subs/{index}/overlay.json",
            get(pgs_overlay::manifest),
        )
        .route(
            "/files/{id}/subs/{index}/overlay/{generation}/objects/{object}",
            get(pgs_overlay::object),
        )
        // How a progressive remux is doing. Session auth, not capability: the
        // id is the client's own playback id, so the check is that the asker
        // owns the stream.
        .route("/stream/{id}/status", get(stream::stream_status))
        // The final segment accepts both `0` (legacy) and `0.vtt` (the
        // documented sidecar URL); the handler validates the suffix/index.
        .route("/files/{id}/subs/{subtitle}", get(stream::subtitles_vtt))
        // Creating a stream spawns a process and supersedes its predecessor,
        // so it is a POST. The GET is a deprecated bridge over the same path.
        .route("/files/{id}/hls/sessions", post(hls::create))
        .route("/files/{id}/hls/start", get(hls::start))
        .route(
            "/hls/{session}/master.m3u8",
            get(hls::master_playlist_response),
        )
        .route("/hls/{session}/index.m3u8", get(hls::playlist))
        .route("/hls/{session}/video.m3u8", get(hls::video_playlist))
        .route(
            "/hls/{session}/subs/{index}/index.m3u8",
            get(hls::subtitle_playlist),
        )
        .route(
            "/hls/{session}/subs/{index}/{segment}",
            get(hls::subtitle_vtt),
        )
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
        .route("/assets/playback-policy.js", get(web::playback_policy_js))
        .route("/assets/reader.js", get(web::reader_js))
        .route("/assets/reader.css", get(web::reader_css))
        .route("/connect.svg", get(web::connect_qr))
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
        // Never put capability credentials or query-string tokens in a span.
        // HLS session ids and offline media tokens are bearer credentials even
        // though they live in the path; query strings can also contain Plex
        // tokens. The access log only needs the redacted route-shaped target.
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                tracing::info_span!(
                    "http_request",
                    method = %request.method(),
                    target = %safe_trace_target(request.uri()),
                    version = ?request.version(),
                )
            }),
        )
        .with_state(state)
}

fn safe_trace_target(uri: &Uri) -> String {
    let mut segments = uri.path().split('/').collect::<Vec<_>>();
    for marker in ["media", "hls", "publication"] {
        if let Some(index) = segments.iter().position(|segment| *segment == marker) {
            let is_capability_route = match marker {
                "media" => index >= 2 && segments.get(index.wrapping_sub(1)) == Some(&"offline"),
                "hls" => true,
                "publication" => true,
                _ => false,
            };
            if is_capability_route && index + 1 < segments.len() {
                segments[index + 1] = "[REDACTED]";
            }
        }
    }
    // Intentionally omit the entire query rather than trying to enumerate
    // every present and future spelling of an access token.
    segments.join("/")
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
    use std::io::Write;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use plurx_core::store::SqliteStore;
    use serde_json::{json, Value};
    use tower::ServiceExt;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    #[test]
    fn trace_targets_omit_queries_and_redact_capability_paths() {
        let ordinary: Uri = "/api/v1/search?q=secret&X-Plex-Token=credential"
            .parse()
            .expect("uri");
        assert_eq!(safe_trace_target(&ordinary), "/api/v1/search");

        let offline: Uri = "/api/v1/offline/media/abcdef/master.m3u8?token=other"
            .parse()
            .expect("uri");
        assert_eq!(
            safe_trace_target(&offline),
            "/api/v1/offline/media/[REDACTED]/master.m3u8"
        );

        let hls: Uri = "/api/v1/hls/session-secret/seg00001.ts"
            .parse()
            .expect("uri");
        assert_eq!(
            safe_trace_target(&hls),
            "/api/v1/hls/[REDACTED]/seg00001.ts"
        );

        let publication: Uri = "/api/v1/publication/session-secret/OEBPS/chapter.xhtml"
            .parse()
            .expect("uri");
        assert_eq!(
            safe_trace_target(&publication),
            "/api/v1/publication/[REDACTED]/OEBPS/chapter.xhtml"
        );
    }

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

    async fn call_text(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.clone().oneshot(req).await.expect("response");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        (
            status,
            String::from_utf8(bytes.to_vec()).expect("UTF-8 body"),
        )
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

    /// Curator announces book imports through the same targeted-scan seam as
    /// video. The path identifies the local edition; explicit Curator keys are
    /// the only evidence Cinema uses to relate text and audio editions.
    #[tokio::test]
    async fn a_curator_book_import_reaches_the_books_library() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let book = dir.path().join("Ursula K. Le Guin/The Dispossessed");
        std::fs::create_dir_all(&book).expect("mkdir");
        std::fs::write(book.join("The Dispossessed.epub"), b"epub fixture").expect("write");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Books", "kind": "books", "paths": [dir.path()] }),
            ),
        )
        .await;

        let body = scan_and_settle(
            &app,
            &key,
            json!({
                "path": book,
                "hint": "book",
                "book": {
                    "title": "The Dispossessed",
                    "author": "Ursula K. Le Guin",
                    "medium": "ebook",
                    "work_id": "curator:openlibrary:OL87320W",
                    "edition_id": "curator:item:84:ebook"
                },
                "correlation_id": "t-84-books",
                "source": "monarr"
            }),
        )
        .await;
        assert_eq!(body["correlation_id"], "t-84-books");
        let item_id = body["items"][0]["item_id"].as_i64().expect("item id");
        let (status, detail) =
            call(&app, get(&format!("/api/v1/items/{item_id}"), Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["item"]["kind"], "book");
        assert_eq!(detail["item"]["title"], "The Dispossessed");
        assert_eq!(detail["item"]["author"], "Ursula K. Le Guin");
        assert_eq!(
            detail["item"]["book_work_id"],
            "curator:openlibrary:OL87320W"
        );
        assert_eq!(detail["item"]["book_edition_id"], "curator:item:84:ebook");
        assert_eq!(detail["item"]["book_metadata_source"], "curator");
    }

    #[tokio::test]
    async fn curator_book_metadata_is_bounded_and_books_only() {
        let app = test_app();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger"])).await;

        let books = tempfile::tempdir().expect("books");
        let book_path = books.path().join("Book");
        std::fs::create_dir_all(&book_path).expect("book dir");
        std::fs::write(book_path.join("Book.epub"), b"epub fixture").expect("book file");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Books", "kind": "books", "paths": [books.path()] }),
            ),
        )
        .await;

        let base = json!({
            "path": book_path,
            "hint": "book",
            "book": {
                "title": "Book",
                "author": "Author",
                "medium": "ebook",
                "work_id": "curator:work:1",
                "edition_id": "curator:edition:1"
            }
        });
        let mut hostile_cover = base.clone();
        hostile_cover["book"]["cover_url"] = json!("https://example.com/cover.jpg");
        let (status, body) = call(&app, post("/api/v1/scan", Some(&key), hostile_cover)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Open Library"));

        let mut wrong_medium = base;
        wrong_medium["book"]["medium"] = json!("pdf");
        let (status, body) = call(&app, post("/api/v1/scan", Some(&key), wrong_medium)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let movies = tempfile::tempdir().expect("movies");
        std::fs::write(movies.path().join("Movie.mkv"), b"movie").expect("movie");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "Movies", "kind": "movies", "paths": [movies.path()] }),
            ),
        )
        .await;
        let (status, body) = call(
            &app,
            post(
                "/api/v1/scan",
                Some(&key),
                json!({
                    "path": movies.path(),
                    "hint": "book",
                    "book": {
                        "title": "Not a book",
                        "author": "Author",
                        "medium": "ebook",
                        "work_id": "curator:work:2",
                        "edition_id": "curator:edition:2"
                    }
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Books library"));
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

    /// The rail prefers the show's poster plurx already has.
    ///
    /// A series whose next episode is airing often already has a poster in
    /// plurx. That copy wins over monarr's provider reference, and it is
    /// resolved by TMDB id, never by title, for the same reason every other
    /// seam in this integration is. Entries without either source still fall
    /// back to initials rather than borrowing an unrelated local poster.
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
                      "tmdbId": 999999,
                      "posterPath": "https://static.tvmaze.com/poster.jpg" },
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
        std::fs::create_dir_all(&state.artwork_dir).expect("artwork dir");
        std::fs::write(state.artwork_dir.join("show.jpg"), b"show poster").expect("show poster");
        let provider_url =
            reqwest::Url::parse("https://static.tvmaze.com/poster.jpg").expect("provider URL");
        let provider_file = super::comingsoon::artwork_cache_filename(&provider_url);
        std::fs::write(state.artwork_dir.join(&provider_file), b"future poster")
            .expect("provider poster");
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

        assert_eq!(
            e[1]["poster"],
            format!("/api/v1/images/{provider_file}"),
            "a film not yet in the library uses monarr's cached provider art: {body}"
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

    /// Monarr puts a show's id in `series`, not item-level `ids`. The receiver
    /// used to build its IdHints Option from `ids` alone, so the authoritative
    /// series id disappeared before the scan job ever saw it.
    #[tokio::test]
    async fn a_series_only_id_reaches_the_show_row() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let key = scan_key(&app, &admin, json!(["scan:trigger", "status:read"])).await;

        let dir = tempfile::tempdir().expect("tmp");
        let season = dir.path().join("Severance (2022)/Season 01");
        std::fs::create_dir_all(&season).expect("mkdir");
        std::fs::write(season.join("Severance.S01E01.mkv"), b"x").expect("write");
        call(
            &app,
            post(
                "/api/v1/libraries",
                Some(&admin),
                json!({ "name": "TV", "kind": "shows", "paths": [dir.path()] }),
            ),
        )
        .await;

        let body = scan_and_settle(
            &app,
            &key,
            json!({
                "path": season,
                "series": { "tmdb": 95396 },
                "hint": "episode",
                "source": "monarr"
            }),
        )
        .await;
        let mut item = state
            .store
            .get_item(body["items"][0]["item_id"].as_i64().expect("episode id"))
            .await
            .expect("get episode")
            .expect("episode");
        while let Some(parent) = item.parent_id {
            item = state
                .store
                .get_item(parent)
                .await
                .expect("get parent")
                .expect("parent");
        }
        assert_eq!(item.kind, plurx_core::domain::ItemKind::Show);
        assert_eq!(
            item.tmdb_id,
            Some(95396),
            "the series field was accepted by HTTP but dropped before apply_ids"
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

    /// `?genre=` narrows the grid on the server, and narrows `total` with it.
    ///
    /// The failure this rules out is the easy half-implementation: filter the
    /// page but count the library. The grid would then say "48 items", show
    /// three, and paginate into empty screens — which reads as a broken
    /// server rather than as a filter that works.
    #[tokio::test]
    async fn a_genre_filter_narrows_the_grid_and_its_total_on_the_server() {
        use plurx_core::domain::{ItemKind, LibraryKind, MetadataPatch, NewItem, NewLibrary};

        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;
        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![],
                anime: false,
            })
            .await
            .expect("library");

        for (title, genres) in [
            ("Heat", vec!["Action", "Crime"]),
            ("Alien", vec!["Horror", "Science Fiction"]),
            ("Aliens", vec!["Action", "Science Fiction"]),
            ("Paddington", vec![]),
        ] {
            let id = state
                .store
                .insert_item(&NewItem {
                    library_id: lib.id,
                    kind: ItemKind::Movie,
                    parent_id: None,
                    title: title.into(),
                    year: None,
                    season_number: None,
                    episode_number: None,
                })
                .await
                .expect("item");
            if !genres.is_empty() {
                state
                    .store
                    .apply_metadata(
                        id,
                        &MetadataPatch {
                            genres: Some(genres.iter().map(|g| (*g).to_owned()).collect()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("genres");
            }
        }

        let page = |uri: String| {
            let app = app.clone();
            let admin = admin.clone();
            async move {
                let (status, body) = call(&app, get(&uri, Some(&admin))).await;
                assert_eq!(status, StatusCode::OK, "{uri}: {body}");
                let titles: Vec<String> = body["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .map(|i| i["title"].as_str().expect("title").to_owned())
                    .collect();
                (titles, body["total"].as_i64().expect("total"))
            }
        };

        // No parameter is byte-for-byte the old behaviour.
        let (all, total) = page(format!("/api/v1/libraries/{}/items", lib.id)).await;
        assert_eq!(all.len(), 4);
        assert_eq!(total, 4);

        let (action, total) =
            page(format!("/api/v1/libraries/{}/items?genre=Action", lib.id)).await;
        assert_eq!(action, vec!["Aliens".to_owned(), "Heat".to_owned()]);
        assert_eq!(
            total, 2,
            "the total must be the filtered total, not the library's"
        );

        // Case-insensitive: the value comes off a URL somebody typed.
        let (lower, _) = page(format!(
            "/api/v1/libraries/{}/items?genre=science%20fiction",
            lib.id
        ))
        .await;
        assert_eq!(lower, vec!["Alien".to_owned(), "Aliens".to_owned()]);

        // Substrings must not match, or "Action" would drag in "Action &
        // Adventure" and the facet counts would stop adding up.
        let (partial, total) = page(format!("/api/v1/libraries/{}/items?genre=Act", lib.id)).await;
        assert!(partial.is_empty(), "got {partial:?}");
        assert_eq!(total, 0);

        // A genre nobody has is an empty page, not an error.
        let (none, total) = page(format!("/api/v1/libraries/{}/items?genre=Polka", lib.id)).await;
        assert!(none.is_empty());
        assert_eq!(total, 0);

        // Blank means "no filter", not "the genre named empty string".
        let (blank, total) = page(format!("/api/v1/libraries/{}/items?genre=%20", lib.id)).await;
        assert_eq!(blank.len(), 4);
        assert_eq!(total, 4);

        // And the additive DTO field is on every item, always present.
        let (_, body) = call(
            &app,
            get(&format!("/api/v1/libraries/{}/items", lib.id), Some(&admin)),
        )
        .await;
        for item in body["items"].as_array().expect("items") {
            assert!(
                item["genres"].is_array(),
                "every item DTO carries genres, empty rather than absent: {item}"
            );
        }
    }

    /// Arming the backfill rewinds its cursor.
    ///
    /// The failure this prevents is quiet and permanent: an operator disarms a
    /// run half way (or a run ends with titles it could not reach), arms it
    /// again later expecting those titles to be retried, and instead the pass
    /// resumes past them and reports success having done nothing.
    #[tokio::test]
    async fn arming_the_genre_backfill_rewinds_its_cursor() {
        use plurx_core::store::keys;

        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;

        // Off by default. An upgrade must not start re-fetching a catalogue.
        let (status, body) = call(&app, get("/api/v1/settings", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["genre_backfill"], json!(false));

        // Pretend a previous run got half way and stopped.
        state
            .store
            .put_setting(keys::GENRE_BACKFILL_CURSOR, "4242")
            .await
            .expect("cursor");

        let (status, body) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "genre_backfill": true }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["genre_backfill"], json!(true));
        assert_eq!(
            state
                .store
                .get_setting(keys::GENRE_BACKFILL_CURSOR)
                .await
                .expect("setting")
                .as_deref(),
            Some("0"),
            "arming must start from the top, or the titles the last run \
             skipped are skipped forever"
        );

        // Disarming leaves the cursor alone: a run stopped part way should be
        // resumable by arming it again *without* redoing what it finished, if
        // that is what the operator wants — the rewind is the arm's doing, so
        // it happens at a moment they chose.
        state
            .store
            .put_setting(keys::GENRE_BACKFILL_CURSOR, "99")
            .await
            .expect("cursor");
        let (status, body) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "genre_backfill": false }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["genre_backfill"], json!(false));
        assert_eq!(
            state
                .store
                .get_setting(keys::GENRE_BACKFILL_CURSOR)
                .await
                .expect("setting")
                .as_deref(),
            Some("99")
        );
    }

    /// N1's two settings move as one complete replicated pair. JSON null
    /// clears the optional quality override. The response shows the requested
    /// values; the manager carries the separately validated effective answer
    /// used by sessions.
    #[tokio::test]
    async fn rate_control_settings_validate_publish_and_restore() {
        use plurx_core::transcode::{EffectiveRateControl, Encoder};

        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;
        let (status, initial) = call(&app, get("/api/v1/settings", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(initial["transcode_rate_mode"], "bitrate");
        assert!(initial["transcode_quality"].is_null(), "{initial}");

        let (status, bad) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({
                    "transcode_rate_mode": "cq",
                    "transcode_quality": null
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");

        let (status, partial) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "transcode_rate_mode": "quality" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{partial}");
        assert!(
            partial["error"]
                .as_str()
                .is_some_and(|error| error.contains("provided together")),
            "{partial}"
        );

        let (status, quality) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({
                    "transcode_rate_mode": "quality",
                    "transcode_quality": 22
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{quality}");
        assert_eq!(quality["transcode_rate_mode"], "quality");
        assert_eq!(quality["transcode_quality"], 22);
        assert_eq!(
            state.transcode.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Qvbr { quality: 22 },
            "the production software args were behavior-probed before publication"
        );

        let (status, restored) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({
                    "transcode_rate_mode": "bitrate",
                    "transcode_quality": null
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{restored}");
        assert_eq!(restored["transcode_rate_mode"], "bitrate");
        assert!(restored["transcode_quality"].is_null(), "{restored}");
        assert_eq!(
            state.transcode.effective_rate_control(Encoder::Software),
            EffectiveRateControl::Vbr
        );
    }

    #[tokio::test]
    async fn rate_control_settings_return_conflict_without_mutating_while_a_viewer_waits() {
        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;
        let _viewer = state.transcode.test_mark_live_waiting();

        let (status, body) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({
                    "transcode_rate_mode": "quality",
                    "transcode_quality": 22
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|error| error.contains("validation deferred")),
            "{body}"
        );
        assert_eq!(
            state
                .store
                .get_setting_pair(
                    plurx_core::store::keys::TRANSCODE_RATE_MODE,
                    plurx_core::store::keys::TRANSCODE_QUALITY,
                )
                .await
                .expect("settings pair"),
            (None, None),
            "the rejected PUT must not write either requested field"
        );
    }

    #[tokio::test]
    async fn settings_report_a_corrupt_quality_pair_as_fail_closed_bitrate() {
        let (app, state) = test_app_with_state();
        let admin = setup_admin(&app).await;
        state
            .store
            .put_settings(&[
                (plurx_core::store::keys::TRANSCODE_RATE_MODE, "quality"),
                (plurx_core::store::keys::TRANSCODE_QUALITY, "256"),
            ])
            .await
            .expect("corrupt durable pair");

        let (status, settings) = call(&app, get("/api/v1/settings", Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        assert_eq!(settings["transcode_rate_mode"], "bitrate");
        assert!(settings["transcode_quality"].is_null(), "{settings}");
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
        assert!(
            body["encoders"]["quality_rc"].is_object(),
            "the fleet census must expose each family's behavioral quality-mode verdict: {body}"
        );
        assert_eq!(body["replication"]["backend"], "sqlite");
        assert_eq!(body["replication"]["health"], "single_node");
        assert_eq!(body["replication"]["clustered"], false);
        assert!(
            body["replication"]["explanation"]
                .as_str()
                .is_some_and(|text| text.contains("stored only on this server")),
            "SQLite must not be presented as synced: {body}"
        );
        let replication = body["replication"].as_object().expect("status object");
        assert_eq!(
            replication
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "backend",
                "checked_at",
                "clustered",
                "explanation",
                "health",
                "last_applied_index",
                "last_applied_term",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            "the status payload must not grow user, media, path, token, or membership data"
        );
        assert_eq!(body["users"], 1);
    }

    #[tokio::test]
    async fn cluster_membership_controls_are_admin_only_and_sqlite_is_explicit() {
        let app = test_app();
        let (status, _) = call(&app, get("/api/v1/cluster/nodes", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        let (status, body) = call(&app, get("/api/v1/cluster/nodes", Some(&admin))).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "membership_unavailable");

        let (status, body) = call(
            &app,
            post(
                "/api/v1/cluster/join-tokens",
                Some(&admin),
                json!({ "expires_in_seconds": 600 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "membership_unavailable");

        let (status, body) = call(
            &app,
            post(
                "/api/v1/cluster/join/redeem",
                None,
                json!({
                    "token_digest": "00".repeat(32),
                    "raft_id": 2,
                    "node_id": "joining-node",
                    "raft_address": "127.0.0.1:32411",
                    "api_address": "127.0.0.1:32412",
                    "schema_version": 6,
                    "protocol_version": 4
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "membership_unavailable");

        let (status, _) = call(
            &app,
            post(
                "/api/v1/cluster/join/redeem",
                None,
                json!({
                    "token": "a complete join token must never cross this route",
                    "node_id": "joining-node"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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

        // Persistence is asynchronous by contract: poll the admin reader, not
        // the handler response, and prove the accepted report became a row.
        let mut rows = serde_json::Value::Null;
        for _ in 0..50 {
            let (read_status, body) = call(
                &app,
                get(
                    "/api/v1/system/playback-events?event=playback_failed&limit=5000",
                    Some(&admin),
                ),
            )
            .await;
            assert_eq!(read_status, StatusCode::OK);
            if body.as_array().is_some_and(|rows| !rows.is_empty()) {
                rows = body;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let row = rows
            .as_array()
            .and_then(|rows| rows.first())
            .expect("client log was persisted asynchronously");
        assert_eq!(row["event"], "playback_failed");
        assert_eq!(row["method"], "remux");
        assert!(row["user_id"].as_i64().is_some());
        assert!(row["speed_recent"].is_null(), "no live session was joined");

        let (status, _) = call(&app, get("/api/v1/system/playback-events", None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "telemetry_retain_days": 0 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call(
            &app,
            post(
                "/api/v1/client-log",
                Some(&admin),
                json!({ "event": "disabled_telemetry_proof", "message": "still logs" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let (_, body) = call(
            &app,
            get(
                "/api/v1/system/playback-events?event=disabled_telemetry_proof",
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(body.as_array().map(Vec::len), Some(0));

        // A near-empty body is tolerated too (all fields optional).
        let (status, _) = call(&app, post("/api/v1/client-log", Some(&admin), json!({}))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn client_telemetry_updates_the_matching_network_prior() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let user = state
            .store
            .get_user_by_username("paul")
            .await
            .expect("admin lookup")
            .expect("admin user");
        state
            .store
            .put_setting(plurx_core::store::keys::PLAYBACK_NETWORK_PRIORS, "1")
            .await
            .expect("enable priors");

        let report =
            |event: &'static str, bandwidth: i64, height: i64, detail: Option<&'static str>| {
                let mut request = post(
                    "/api/v1/client-log",
                    Some(&admin),
                    json!({
                        "event": event,
                        "bandwidth": bandwidth,
                        "height": height,
                        "detail": detail,
                        "ua": "Safari"
                    }),
                );
                request.headers_mut().insert(
                    "x-forwarded-for",
                    axum::http::HeaderValue::from_static("198.51.100.77"),
                );
                request.headers_mut().insert(
                    axum::http::header::USER_AGENT,
                    axum::http::HeaderValue::from_static(super::test_agents::SAFARI_MACOS_UA),
                );
                request
            };
        let prior = || {
            state
                .store
                .network_prior(user.id, "safari", "198.51.100.0/24")
        };

        let (status, _) = call(&app, report("ttff", 8_000, 1080, None)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        for _ in 0..100 {
            if prior()
                .await
                .expect("first prior lookup")
                .is_some_and(|prior| prior.sample_count == 1)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            prior()
                .await
                .expect("first prior lookup")
                .expect("first observation")
                .sustained_kbps,
            Some(8_000)
        );

        let (status, _) = call(&app, report("stall", 4_000, 720, Some("supply:empty"))).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let mut updated = None;
        for _ in 0..100 {
            let current = prior().await.expect("updated prior lookup");
            if current
                .as_ref()
                .is_some_and(|prior| prior.sample_count == 2)
            {
                updated = current;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let updated = updated.expect("second observation");
        assert_eq!(updated.sustained_kbps, Some(7_000));
        assert_eq!(updated.worst_rung_height, Some(720));
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

    fn test_state_with_pgs_overlay() -> (Router, AppState) {
        let store = SqliteStore::open_in_memory().expect("store");
        let base = std::env::temp_dir().join(format!("plurx-pgs-api-{}", uuid::Uuid::new_v4()));
        let mut state = AppState::new(
            "test".into(),
            Arc::new(store),
            test_dirs(&base),
            "test-node".into(),
            Default::default(),
            Default::default(),
            Arc::new(crate::logbuf::LogBuffer::new(64)),
        );
        state.pgs_overlay_enabled = true;
        (router(state.clone()), state)
    }

    #[tokio::test]
    async fn reading_state_api_is_authenticated_revision_bound_and_ordered() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let library = state
            .store
            .create_library(&NewLibrary {
                name: "Reading API Books".into(),
                kind: LibraryKind::Books,
                paths: vec![std::path::PathBuf::from("/reading-api")],
                anime: false,
            })
            .await
            .expect("books library");
        let book = state
            .store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Book,
                parent_id: None,
                title: "Reading API Contract".into(),
                year: Some(2026),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("book");
        let file = state
            .store
            .upsert_file(
                book,
                "/reading-api/contract.epub",
                4_096,
                100,
                &ProbeResult::default(),
            )
            .await
            .expect("book file");

        let uri = format!("/api/v1/items/{book}/reading-state?file_id={file}");
        assert_eq!(
            call(&app, get(&uri, None)).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, empty) = call(&app, get(&uri, Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{empty}");
        assert!(empty["state"].is_null());
        assert_eq!(empty["stale"], false);

        let put_uri = format!("/api/v1/items/{book}/reading-state");
        let (status, saved) = call(
            &app,
            put(
                &put_uri,
                Some(&admin),
                json!({
                    "file_id": file,
                    "revision": { "size": 4096, "mtime": 100 },
                    "locator": {
                        "version": 1,
                        "href": "Text/chapter-3.xhtml#paragraph-2",
                        "locations": { "progression": 0.6, "totalProgression": 0.6 }
                    },
                    "progression": 0.6,
                    "completed": false,
                    "recorded_at": 200
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["file_id"], file);
        assert_eq!(saved["revision"], json!({ "size": 4096, "mtime": 100 }));
        assert_eq!(saved["progression"], 0.6);

        // An older offline close event returns the durable winner; it cannot
        // rewind the state saved by a newer device.
        let (status, winner) = call(
            &app,
            put(
                &put_uri,
                Some(&admin),
                json!({
                    "file_id": file,
                    "revision": { "size": 4096, "mtime": 100 },
                    "locator": { "version": 1, "href": "Text/chapter-1.xhtml" },
                    "progression": 0.1,
                    "completed": false,
                    "recorded_at": 100
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{winner}");
        assert_eq!(winner["progression"], 0.6);
        assert_eq!(
            winner["locator"]["href"],
            "Text/chapter-3.xhtml#paragraph-2"
        );

        let (status, detail) =
            call(&app, get(&format!("/api/v1/items/{book}"), Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["reading"]["progression"], 0.6);

        let (status, conflict) = call(
            &app,
            put(
                &put_uri,
                Some(&admin),
                json!({
                    "file_id": file,
                    "revision": { "size": 4097, "mtime": 100 },
                    "locator": { "version": 1, "href": "chapter.xhtml" },
                    "progression": 0.7,
                    "completed": false
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{conflict}");

        let same_file = state
            .store
            .upsert_file(
                book,
                "/reading-api/contract.epub",
                4_100,
                101,
                &ProbeResult::default(),
            )
            .await
            .expect("replace revision");
        assert_eq!(same_file, file);
        let (status, stale) = call(&app, get(&uri, Some(&admin))).await;
        assert_eq!(status, StatusCode::OK, "{stale}");
        assert_eq!(stale["stale"], true);
        assert!(stale["state"].is_null(), "{stale}");
        let (_, detail) = call(&app, get(&format!("/api/v1/items/{book}"), Some(&admin))).await;
        assert!(detail["reading"].is_null(), "{detail}");

        let (status, current) = call(
            &app,
            put(
                &put_uri,
                Some(&admin),
                json!({
                    "file_id": file,
                    "revision": { "size": 4100, "mtime": 101 },
                    "locator": { "version": 1, "href": "Text/chapter-4.xhtml" },
                    "progression": 1.0,
                    "completed": true,
                    "recorded_at": 50
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{current}");
        assert_eq!(current["completed"], true);
        assert_eq!(current["updated_at"], 50);

        assert_eq!(
            call(&app, delete(&uri, Some(&admin))).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&app, delete(&uri, Some(&admin))).await.0,
            StatusCode::NO_CONTENT,
            "deletion is idempotent"
        );
        assert!(call(&app, get(&uri, Some(&admin))).await.1["state"].is_null());
    }

    #[tokio::test]
    async fn reading_state_api_rejects_non_books_invalid_locators_and_large_bodies() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let library = state
            .store
            .create_library(&NewLibrary {
                name: "Reading Validation Books".into(),
                kind: LibraryKind::Books,
                paths: vec![std::path::PathBuf::from("/reading-validation")],
                anime: false,
            })
            .await
            .expect("books library");
        let mut items = Vec::new();
        for (kind, name, extension) in [
            (ItemKind::Book, "Text", "epub"),
            (ItemKind::Audiobook, "Audio", "m4b"),
        ] {
            let item = state
                .store
                .insert_item(&NewItem {
                    library_id: library.id,
                    kind,
                    parent_id: None,
                    title: name.into(),
                    year: None,
                    season_number: None,
                    episode_number: None,
                })
                .await
                .expect("item");
            let file = state
                .store
                .upsert_file(
                    item,
                    &format!("/reading-validation/{name}.{extension}"),
                    100,
                    10,
                    &ProbeResult::default(),
                )
                .await
                .expect("file");
            items.push((item, file));
        }
        let (book, book_file) = items[0];
        let (audio, audio_file) = items[1];
        let payload = |locator: Value, progression: f64| {
            json!({
                "file_id": book_file,
                "revision": { "size": 100, "mtime": 10 },
                "locator": locator,
                "progression": progression,
                "completed": false
            })
        };

        for locator in [
            json!({ "version": 2, "href": "chapter.xhtml" }),
            json!({ "version": 1, "href": "../secret" }),
            json!({ "version": 1, "href": "https://example.com/chapter" }),
            json!({
                "version": 1,
                "href": "chapter.xhtml",
                "locations": { "progression": 1.1 }
            }),
        ] {
            let (status, body) = call(
                &app,
                put(
                    &format!("/api/v1/items/{book}/reading-state"),
                    Some(&admin),
                    payload(locator, 0.5),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        }
        assert_eq!(
            call(
                &app,
                put(
                    &format!("/api/v1/items/{book}/reading-state"),
                    Some(&admin),
                    payload(json!({ "version": 1, "href": "chapter.xhtml" }), 1.1),
                ),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );

        let (status, body) = call(
            &app,
            put(
                &format!("/api/v1/items/{audio}/reading-state"),
                Some(&admin),
                json!({
                    "file_id": audio_file,
                    "revision": { "size": 100, "mtime": 10 },
                    "locator": { "version": 1, "href": "chapter.xhtml" },
                    "progression": 0.5,
                    "completed": false
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let oversized = "x".repeat(70 * 1024);
        let (status, _) = call(
            &app,
            put(
                &format!("/api/v1/items/{book}/reading-state"),
                Some(&admin),
                payload(json!({ "version": 1, "href": oversized }), 0.5),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    fn write_epub_fixture(path: &std::path::Path) {
        let file = std::fs::File::create(path).expect("EPUB fixture file");
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, body) in [
            ("mimetype", "application/epub+zip"),
            (
                "META-INF/container.xml",
                r#"<container><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/book.opf",
                r#"<package><metadata><title>HTTP Proof</title></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="chapter" href="Text/chapter.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="chapter"/></spine></package>"#,
            ),
            (
                "OEBPS/nav.xhtml",
                r#"<html xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol><li><a href="Text/chapter.xhtml">Chapter</a></li></ol></nav></body></html>"#,
            ),
            (
                "OEBPS/Text/chapter.xhtml",
                r#"<html><body><script>fetch('https://example.com/leak')</script><h1>Chapter</h1></body></html>"#,
            ),
        ] {
            writer
                .start_file(name, options)
                .expect("EPUB fixture entry");
            writer
                .write_all(body.as_bytes())
                .expect("EPUB fixture bytes");
        }
        writer.finish().expect("finish EPUB fixture");
    }

    #[tokio::test]
    async fn publication_api_is_authenticated_scoped_and_script_network_closed() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};

        let directory = tempfile::tempdir().expect("publication directory");
        let path = directory.path().join("proof.epub");
        write_epub_fixture(&path);
        let metadata = std::fs::metadata(&path).expect("EPUB metadata");
        let size = i64::try_from(metadata.len()).expect("fixture size");
        let mtime = metadata
            .modified()
            .expect("fixture mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("fixture after epoch")
            .as_secs() as i64;

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let library = state
            .store
            .create_library(&NewLibrary {
                name: "Publication API Books".into(),
                kind: LibraryKind::Books,
                paths: vec![directory.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("books library");
        let item = state
            .store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Book,
                parent_id: None,
                title: "HTTP Proof".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("book");
        let file = state
            .store
            .upsert_file(
                item,
                &path.to_string_lossy(),
                size,
                mtime,
                &ProbeResult::default(),
            )
            .await
            .expect("book file");
        let open_uri = format!("/api/v1/files/{file}/publication");
        assert_eq!(
            call(&app, post(&open_uri, None, json!({}))).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, opened) = call(&app, post(&open_uri, Some(&admin), json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{opened}");
        assert_eq!(opened["publication"]["metadata"]["title"], "HTTP Proof");
        assert_eq!(opened["publication"]["toc"][0]["title"], "Chapter");
        assert_eq!(opened["limits"]["entries"], 20_000);
        assert_eq!(opened["limits"]["concurrent_resource_reads"], 8);
        assert_eq!(opened["limits"]["resource_chunk_bytes"], 65_536);

        let resource_uri = format!(
            "{}OEBPS/Text/chapter.xhtml",
            opened["resource_base"].as_str().expect("resource base")
        );
        let response = app
            .clone()
            .oneshot(get(&resource_uri, None))
            .await
            .expect("publication resource");
        assert_eq!(response.status(), StatusCode::OK);
        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .expect("resource CSP");
        assert!(csp.contains("script-src 'none'"), "{csp}");
        assert!(csp.contains("connect-src 'none'"), "{csp}");
        assert!(csp.contains("img-src 'self' data:"), "{csp}");
        assert!(response.headers().get("content-length").is_some());
        assert!(response.headers().get("referrer-policy").is_some());
        let resource_body = response
            .into_body()
            .collect()
            .await
            .expect("streamed publication body")
            .to_bytes();
        assert!(
            resource_body
                .windows(b"<h1>Chapter</h1>".len())
                .any(|window| window == b"<h1>Chapter</h1>"),
            "the decompressed resource must reach the HTTP body"
        );

        call(
            &app,
            post(
                "/api/v1/users",
                Some(&admin),
                json!({ "username": "reader", "password": "longenough" }),
            ),
        )
        .await;
        let (_, login) = call(
            &app,
            post(
                "/api/v1/auth/login",
                None,
                json!({ "username": "reader", "password": "longenough" }),
            ),
        )
        .await;
        let reader = login["token"].as_str().expect("reader token").to_owned();
        let session = opened["session_id"].as_str().expect("session id");
        assert_eq!(
            call(
                &app,
                delete_req(&format!("/api/v1/publication/{session}"), Some(&reader),),
            )
            .await
            .0,
            StatusCode::NOT_FOUND,
            "a different user must not discover or revoke the capability"
        );
        assert_eq!(
            app.clone()
                .oneshot(get(&resource_uri, None))
                .await
                .expect("publication resource after foreign close")
                .status(),
            StatusCode::OK
        );

        // A capability is bound to the exact bytes that were parsed. Even a
        // still-valid session must not blend a new ZIP revision with the old
        // manifest.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen EPUB")
            .write_all(b"changed")
            .expect("replace EPUB revision");
        assert_eq!(
            app.clone()
                .oneshot(get(&resource_uri, None))
                .await
                .expect("changed resource")
                .status(),
            StatusCode::CONFLICT
        );

        assert_eq!(
            call(
                &app,
                delete_req(&format!("/api/v1/publication/{session}"), Some(&admin)),
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            app.oneshot(get(&resource_uri, None))
                .await
                .expect("closed resource")
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn offline_package_api_is_idempotent_owned_and_stable_before_production() {
        use plurx_core::domain::{
            AudioStream, ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult, SubtitleStream,
        };
        use plurx_core::transcode::EffectiveRateControl;

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let source_dir = tempfile::tempdir().expect("source");
        let source = source_dir.path().join("movie.mkv");
        std::fs::write(&source, b"offline fixture").expect("source bytes");
        let library = state
            .store
            .create_library(&NewLibrary {
                name: "Offline".into(),
                kind: LibraryKind::Movies,
                paths: vec![source_dir.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("library");
        let movie = state
            .store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Flight".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let file_id = state
            .store
            .upsert_file(
                movie,
                &source.to_string_lossy(),
                15,
                7,
                &ProbeResult {
                    duration_ms: Some(90_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "truehd".into(),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    subtitle_streams: vec![
                        SubtitleStream {
                            index: 0,
                            codec: "hdmv_pgs_subtitle".into(),
                            language: Some("eng".into()),
                            forced: true,
                            ..Default::default()
                        },
                        SubtitleStream {
                            index: 1,
                            codec: "subrip".into(),
                            language: Some("eng".into()),
                            ..Default::default()
                        },
                    ],
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("file");

        let (status_code, options) = call(
            &app,
            get(
                &format!("/api/v1/files/{file_id}/offline-options"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::OK, "{options}");
        assert_eq!(options["recommended_subtitle_index"], Value::Null);
        assert_eq!(options["subtitles"][0]["offline_mode"], "unavailable");
        assert_eq!(options["subtitles"][1]["offline_mode"], "native");

        let request_id = uuid::Uuid::new_v4().to_string();
        let create = || {
            post(
                &format!("/api/v1/files/{file_id}/offline-packages"),
                Some(&admin),
                json!({
                    "request_id": request_id.clone(),
                    "height": 720,
                    "audio_index": 0,
                    "subtitle_index": 1
                }),
            )
        };
        let (status_code, first) = call(&app, create()).await;
        assert_eq!(status_code, StatusCode::ACCEPTED, "{first}");
        assert_eq!(first["state"], "queued");
        let package_id = first["id"].as_str().expect("package id").to_owned();

        // Rate control is server policy, not part of the client's idempotent
        // request. A lost create response retried after an administrator flips
        // that policy must recover the original package and its first-write
        // snapshot, never conflict or silently retarget it.
        let (status_code, quality) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({
                    "transcode_rate_mode": "quality",
                    "transcode_quality": 22
                }),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::OK, "{quality}");
        let selected = state.transcode.test_publish_supported_quality(22).await;
        assert_eq!(
            state.transcode.effective_rate_control(selected),
            EffectiveRateControl::Qvbr { quality: 22 },
            "the retry must be made under a different effective server policy"
        );

        let (status_code, retry) = call(&app, create()).await;
        assert_eq!(status_code, StatusCode::ACCEPTED, "{retry}");
        assert_eq!(retry["id"], package_id);
        assert_eq!(
            state
                .store
                .offline_package_for_user(&package_id, 1)
                .await
                .expect("retry lookup")
                .expect("original package")
                .effective_rate_control,
            "vbr"
        );

        let (status_code, activity) =
            call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert_eq!(status_code, StatusCode::OK, "{activity}");
        assert_eq!(activity["offline"][0]["kind"], "prepare");
        assert_eq!(activity["offline"][0]["title"], "Flight");
        assert_eq!(activity["offline"][0]["state"], "queued");
        assert_eq!(activity["offline"][0]["user"], "paul");
        assert_eq!(activity["offline"][0]["id"], package_id);

        let (status_code, second) = call(
            &app,
            post(
                &format!("/api/v1/files/{file_id}/offline-packages"),
                Some(&admin),
                json!({
                    "request_id": uuid::Uuid::new_v4().to_string(),
                    "height": 720,
                    "audio_index": 0,
                    "subtitle_index": 1
                }),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::ACCEPTED, "{second}");
        let second_id = second["id"].as_str().expect("second package id");
        assert_eq!(
            call(
                &app,
                delete(&format!("/api/v1/activity/offline/{second_id}"), None),
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED,
        );
        assert_eq!(
            call(
                &app,
                delete(
                    &format!("/api/v1/activity/offline/{second_id}"),
                    Some(&admin),
                ),
            )
            .await
            .0,
            StatusCode::OK,
        );
        assert!(state
            .store
            .offline_package_for_user(second_id, 1)
            .await
            .expect("cancelled lookup")
            .is_none());
        assert!(state
            .store
            .offline_package_for_user(&package_id, 1)
            .await
            .expect("unrelated lookup")
            .is_some());

        let (status_code, summary) = call(&app, get("/api/v1/activity", Some(&admin))).await;
        assert_eq!(status_code, StatusCode::OK, "{summary}");
        assert!(summary
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| {
                row["kind"] == "offline_prepare" && row["label"] == "Preparing offline · Flight"
            })));

        let (status_code, metrics) = call_text(&app, get("/metrics", None)).await;
        assert_eq!(status_code, StatusCode::OK);
        assert!(metrics.contains("plurx_offline_packages{state=\"queued\"} 1"));
        assert!(metrics.contains("plurx_offline_requests_total{height=\"720\"} 2"));
        assert!(metrics.contains("plurx_cache_protected_entries{reason=\"active_playback\"} 0"));
        assert!(
            !metrics.contains("Flight"),
            "titles must never become labels"
        );
        assert!(
            !metrics.contains(&package_id),
            "package ids must never become labels"
        );

        let (status_code, conflict) = call(
            &app,
            post(
                &format!("/api/v1/files/{file_id}/offline-packages"),
                Some(&admin),
                json!({
                    "request_id": request_id.clone(),
                    "height": 480,
                    "audio_index": 0,
                    "subtitle_index": 1
                }),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::CONFLICT, "{conflict}");
        assert_eq!(conflict["code"], "request_conflict");

        let (status_code, _) = call(
            &app,
            get(&format!("/api/v1/offline/packages/{package_id}"), None),
        )
        .await;
        assert_eq!(status_code, StatusCode::UNAUTHORIZED);

        let token = "a".repeat(64);
        let (status_code, lease) = call(
            &app,
            put(
                &format!("/api/v1/offline/packages/{package_id}/lease"),
                Some(&admin),
                json!({ "token": token }),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::CONFLICT, "{lease}");
        assert_eq!(lease["code"], "package_not_ready");

        let cache_relative = "te/test-recipe";
        let cache_dir = state.cache_dir.join(cache_relative);
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .expect("cache dir");
        tokio::fs::write(cache_dir.join("index.m3u8"), b"#EXTM3U\n#EXT-X-ENDLIST\n")
            .await
            .expect("playlist");
        state
            .store
            .claim_cache_entry("test-recipe", file_id, 7, &state.node_id, cache_relative)
            .await
            .expect("cache claim");
        state
            .store
            .complete_cache_entry("test-recipe", &state.node_id, 24)
            .await
            .expect("complete cache");
        assert!(state
            .store
            .mark_offline_package_ready(&package_id, &state.node_id, "test-recipe", 15, 90_000)
            .await
            .expect("mark ready"));
        let (status_code, lease) = call(
            &app,
            put(
                &format!("/api/v1/offline/packages/{package_id}/lease"),
                Some(&admin),
                json!({ "token": "a".repeat(64) }),
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::CREATED, "{lease}");

        state
            .store
            .put_setting(plurx_core::store::keys::OFFLINE_ENABLED, "0")
            .await
            .expect("disable offline");
        let (status_code, disabled) = call_text(
            &app,
            get(
                &format!("/api/v1/offline/media/{}/index.m3u8", "a".repeat(64)),
                None,
            ),
        )
        .await;
        assert_eq!(status_code, StatusCode::SERVICE_UNAVAILABLE, "{disabled}");
        state
            .store
            .put_setting(plurx_core::store::keys::OFFLINE_ENABLED, "1")
            .await
            .expect("enable offline");

        let eviction = state.transcode.begin_cache_eviction_for_test("test-recipe");
        let (status_code, _) = call_text(
            &app,
            get(
                &format!("/api/v1/offline/media/{}/index.m3u8", "a".repeat(64)),
                None,
            ),
        )
        .await;
        assert_eq!(
            status_code,
            StatusCode::GONE,
            "an offline read entered while orphan cleanup owned its recipe"
        );
        drop(eviction);
        state.offline.record_transfer(&package_id, 7);
        let (status_code, activity) =
            call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert_eq!(status_code, StatusCode::OK, "{activity}");
        assert_eq!(activity["offline"][0]["kind"], "send");
        assert_eq!(activity["offline"][0]["bytes_sent"], 7);

        for attempt in ["first completion", "idempotent retry"] {
            let (status_code, body) = call(
                &app,
                post(
                    &format!("/api/v1/offline/packages/{package_id}/complete"),
                    Some(&admin),
                    json!({}),
                ),
            )
            .await;
            assert_eq!(status_code, StatusCode::NO_CONTENT, "{attempt}: {body}");
        }
        assert!(state
            .store
            .offline_package_for_user(&package_id, 1)
            .await
            .expect("package lookup")
            .is_none());
        let (_, metrics) = call_text(&app, get("/metrics", None)).await;
        assert!(metrics.contains("plurx_offline_packages{state=\"ready\"} 0"));
        assert!(metrics.contains("plurx_offline_cancellations_total 1"));
        assert!(metrics.contains("plurx_offline_transfer_bytes_total 7"));
    }

    #[tokio::test]
    async fn pgs_overlay_routes_are_default_off() {
        let (app, _) = test_state();
        let admin = setup_admin(&app).await;
        assert_eq!(
            status_of(
                &app,
                get("/api/v1/files/1/subs/0/overlay.json", Some(&admin)),
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn pgs_overlay_contract_is_authenticated_typed_and_immutable() {
        use plurx_core::domain::{
            ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult, SubtitleStream,
        };
        use sha2::{Digest, Sha256};

        let (app, state) = test_state_with_pgs_overlay();
        let admin = setup_admin(&app).await;
        let source_dir =
            std::env::temp_dir().join(format!("plurx-pgs-source-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("movie.mkv");
        std::fs::write(&source, b"fixture").expect("source");
        let library = state
            .store
            .create_library(&NewLibrary {
                name: "PGS".into(),
                kind: LibraryKind::Movies,
                paths: vec![source_dir],
                anime: false,
            })
            .await
            .expect("library");
        let movie = state
            .store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Overlay".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");
        let probe = ProbeResult {
            duration_ms: Some(10_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            subtitle_streams: vec![
                SubtitleStream {
                    index: 0,
                    codec: "hdmv_pgs_subtitle".into(),
                    ..Default::default()
                },
                SubtitleStream {
                    index: 1,
                    codec: "subrip".into(),
                    ..Default::default()
                },
                SubtitleStream {
                    index: 2,
                    codec: "pgssub".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let file_id = state
            .store
            .upsert_file(movie, &source.to_string_lossy(), 7, 1, &probe)
            .await
            .expect("file");
        let file = state
            .store
            .get_file(file_id)
            .await
            .expect("read file")
            .expect("file exists");

        let generation = crate::pgs_overlay::generation(&file, 0);
        let generation_dir = crate::pgs_overlay::generation_dir(&state.subs_dir, &file, 0);
        let objects_dir = generation_dir.join("objects");
        std::fs::create_dir_all(&objects_dir).expect("objects");
        let png = b"content-addressed-png-fixture";
        let object_hash = hex::encode(Sha256::digest(png));
        std::fs::write(objects_dir.join(format!("{object_hash}.png")), png).expect("object");
        let seeded_manifest = json!({
            "schema": 1,
            "generation": generation,
            "file_id": file_id,
            "track_index": 0,
            "kind": "pgs",
            "timebase": "source_ms",
            "duration_ms": 10000,
            "cues": []
        });
        std::fs::write(
            generation_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&seeded_manifest).expect("manifest json"),
        )
        .expect("manifest");

        let manifest_uri = format!("/api/v1/files/{file_id}/subs/0/overlay.json");
        assert_eq!(
            status_of(&app, get(&manifest_uri, None)).await,
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(get(&manifest_uri, Some(&admin)))
            .await
            .expect("manifest response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "private, no-cache");
        assert!(response.headers().get("etag").is_some());

        let object_uri = format!(
            "/api/v1/files/{file_id}/subs/0/overlay/{generation}/objects/{object_hash}.png"
        );
        assert_eq!(
            status_of(&app, get(&object_uri, None)).await,
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(get(&object_uri, Some(&admin)))
            .await
            .expect("object response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(
            response.headers()["cache-control"],
            "private, max-age=31536000, immutable"
        );
        assert_eq!(response.headers()["etag"], format!("\"{object_hash}\""));

        assert_eq!(
            status_of(
                &app,
                get(
                    &format!(
                        "/api/v1/files/{file_id}/subs/0/overlay/wrong/objects/{object_hash}.png"
                    ),
                    Some(&admin),
                ),
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(
                &app,
                get(
                    &format!("/api/v1/files/{file_id}/subs/1/overlay.json"),
                    Some(&admin),
                ),
            )
            .await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        // A power-loss-shaped published directory must not become a permanent
        // 500. The next manifest read removes it, owns a fresh preparation,
        // and returns the ordinary retry contract.
        std::fs::write(generation_dir.join("manifest.json"), b"").expect("tear published manifest");
        let (status, body) = call(&app, get(&manifest_uri, Some(&admin))).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["state"], "preparing");

        // A second PGS track has no cache. Preparation is detached and the
        // request returns promptly instead of blocking video startup.
        let (status, body) = call(
            &app,
            get(
                &format!("/api/v1/files/{file_id}/subs/2/overlay.json"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["state"], "preparing");
        assert_eq!(body["retry_after_ms"], 1000);
    }

    struct Seed {
        lib: i64,
        movie: i64,
        file: i64,
        show: i64,
        season: i64,
        ep: i64,
    }

    struct AudiobookSeed {
        item: i64,
    }

    async fn seed_audiobook(state: &AppState) -> AudiobookSeed {
        use plurx_core::domain::{
            AudioStream, ItemKind, LibraryKind, MetadataPatch, NewItem, NewLibrary, ProbeResult,
        };

        let dir = std::env::temp_dir().join(format!("plurx-audiobook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("audiobook dir");
        let library = state
            .store
            .create_library(&NewLibrary {
                name: format!("Audiobooks {}", uuid::Uuid::new_v4()),
                kind: LibraryKind::Books,
                paths: vec![dir.clone()],
                anime: false,
            })
            .await
            .expect("audiobook library");
        let item = state
            .store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Audiobook,
                parent_id: None,
                title: "The Contract Audiobook".into(),
                year: Some(2026),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("audiobook item");

        // Insert in deliberately hostile lexical/insertion order. The detail
        // endpoint owns playback order and must return 1, 2, 10.
        for (filename, duration_ms) in [
            ("Part 10.mp3", 300_000),
            ("Part 2.mp3", 120_000),
            ("Part 1.mp3", 60_000),
        ] {
            let path = dir.join(filename);
            std::fs::write(&path, b"audio fixture").expect("audiobook part");
            let chapters = if filename == "Part 1.mp3" {
                Some(
                    r#"{"chapters":[{"id":0,"start_time":"0.000","end_time":"30.000","tags":{"title":"Opening"}}]}"#
                        .to_owned(),
                )
            } else {
                Some(r#"{"chapters":[]}"#.to_owned())
            };
            state
                .store
                .upsert_file(
                    item,
                    &path.to_string_lossy(),
                    13,
                    duration_ms,
                    &ProbeResult {
                        duration_ms: Some(duration_ms),
                        container: Some("mp3".into()),
                        audio_streams: vec![AudioStream {
                            index: 0,
                            codec: "mp3".into(),
                            channels: Some(2),
                            default: true,
                            ..Default::default()
                        }],
                        raw_json: chapters,
                        ..Default::default()
                    },
                )
                .await
                .expect("audiobook file");
        }
        state
            .store
            .apply_metadata(
                item,
                &MetadataPatch {
                    runtime_ms: Some(480_000),
                    ..Default::default()
                },
            )
            .await
            .expect("audiobook runtime");
        AudiobookSeed { item }
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
        // The badge field reaches the wire through `Decision`'s flatten, so
        // nothing in plurxd would notice if it stopped. Which *grade* each
        // delivery reports is pinned in playback/mod.rs; what this pins is
        // that the key is there at all, on every method.
        assert_eq!(body["delivered_dynamic_range"], "sdr", "{body}");

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

    /// A caller-selected audio track must replace the policy default before
    /// capability evaluation. This is the retained regression for #265: with
    /// the production selection handling removed, the second request silently
    /// returns the first request's direct-play verdict.
    #[tokio::test]
    async fn selected_unsupported_audio_changes_the_delivery_decision() {
        use plurx_core::domain::{AudioStream, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let dir =
            std::env::temp_dir().join(format!("plurx-selected-audio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("media dir");
        let path = dir.join("Two Audio Tracks.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("media file");
        let file = state
            .store
            .upsert_file(
                seeded.movie,
                &path.to_string_lossy(),
                24,
                1,
                &ProbeResult {
                    duration_ms: Some(60_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![
                        AudioStream {
                            index: 0,
                            codec: "aac".into(),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        AudioStream {
                            index: 1,
                            codec: "dts".into(),
                            language: Some("fre".into()),
                            default: false,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        let caps = "vcodec=h264&acodec=aac&container=mp4&hdr=0";

        let (status, default) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{default}");
        assert_eq!(default["method"], "direct_play", "{default}");
        assert!(
            default.get("selection").is_none(),
            "the no-selection response keeps its prior JSON shape: {default}"
        );

        let (status, selected) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}&audio=1"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected["method"], "remux", "{selected}");
        assert_eq!(selected["delivery"]["mode"], "remux", "{selected}");
        assert_eq!(selected["delivery"]["aac"], true, "{selected}");
        assert_eq!(selected["selection"]["audio_index"], 1, "{selected}");
        assert_eq!(
            selected["delivery"]["url"],
            format!("/api/v1/files/{file}/stream.mp4?audio=1"),
            "the plan must carry the selection it was decided for: {selected}"
        );
        assert_eq!(
            selected["delivery"]["audio"], selected["selection"]["audio_index"],
            "{selected}"
        );
        assert!(
            selected["reasons"]
                .as_array()
                .is_some_and(|reasons| reasons.iter().any(|reason| {
                    reason
                        .as_str()
                        .is_some_and(|reason| reason.contains("audio codec dts unsupported"))
                })),
            "{selected}"
        );

        let (status, invalid) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}&audio=9"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
        assert_eq!(invalid["error"], "unknown audio track", "{invalid}");

        assert_eq!(
            state
                .store
                .get_setting(plurx_core::store::keys::AUDIO_LANG)
                .await
                .expect("audio setting"),
            None,
            "request-local selection must not create or change a setting"
        );
    }

    /// A verdict a client cannot execute is worse than no verdict: it plays
    /// the wrong language and nothing says so. Direct play hands over the raw
    /// file, whose audio track the browser picks from the container flags, so
    /// selecting any other track has to change the *plan* — and the plan URL
    /// has to carry the selection, because a bare `stream.mp4` re-derives the
    /// language policy instead. Retained regression for #265 review finding 1.
    #[tokio::test]
    async fn the_delivery_plan_carries_a_selected_non_default_audio_track() {
        use plurx_core::domain::{AudioStream, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let dir = std::env::temp_dir().join(format!("plurx-plan-audio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("media dir");
        let path = dir.join("Dual Language.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("media file");
        // Both tracks decode on this client and the container is native, so
        // nothing but the selection can move the verdict off direct play.
        let file = state
            .store
            .upsert_file(
                seeded.movie,
                &path.to_string_lossy(),
                25,
                1,
                &ProbeResult {
                    duration_ms: Some(60_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![
                        AudioStream {
                            index: 0,
                            codec: "aac".into(),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        AudioStream {
                            index: 1,
                            codec: "aac".into(),
                            language: Some("fre".into()),
                            default: false,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        let caps = "vcodec=h264&acodec=aac&container=mp4&hdr=0";

        // The container default: direct play is genuinely executable, and the
        // plan stays the raw file.
        let (status, container_default) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}&audio=0"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{container_default}");
        assert_eq!(
            container_default["method"], "direct_play",
            "{container_default}"
        );
        assert_eq!(
            container_default["delivery"]["url"],
            format!("/api/v1/files/{file}/direct"),
            "{container_default}"
        );

        // The second track: decodable, but unreachable through the raw file.
        let (status, selected) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}&audio=1"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected["selection"]["audio_index"], 1, "{selected}");
        assert_ne!(
            selected["method"], "direct_play",
            "direct play cannot map a non-container-default track: {selected}"
        );
        assert_eq!(selected["method"], "remux", "{selected}");
        assert_eq!(selected["delivery"]["mode"], "remux", "{selected}");
        assert_eq!(
            selected["delivery"]["aac"], false,
            "the selected track decodes on this client, so only the mapping changed: {selected}"
        );
        assert!(
            selected["reasons"]
                .as_array()
                .is_some_and(|reasons| reasons.iter().any(|reason| reason
                    .as_str()
                    .is_some_and(|reason| reason.contains("not the container default")))),
            "the downgrade has to say why: {selected}"
        );

        // The plan is executable: its URL names the selection, and following
        // it reaches the remux endpoint rather than a rejected query.
        let plan_url = selected["delivery"]["url"]
            .as_str()
            .expect("a remux plan has a URL")
            .to_owned();
        assert_eq!(
            plan_url,
            format!("/api/v1/files/{file}/stream.mp4?audio=1"),
            "{selected}"
        );
        assert_eq!(
            selected["delivery"]["audio"], selected["selection"]["audio_index"],
            "the HLS transport takes the selection in its session body: {selected}"
        );
        assert_eq!(selected["play_url"], plan_url, "{selected}");
        assert_eq!(
            status_of(&app, get(&plan_url, Some(&admin))).await,
            StatusCode::OK,
            "the server must serve the plan URL it just handed out"
        );

        assert_eq!(
            state
                .store
                .get_setting(plurx_core::store::keys::AUDIO_LANG)
                .await
                .expect("audio setting"),
            None,
            "request-local selection must not create or change a setting"
        );
    }

    /// Echoing an explicit audio choice must not make the policy-default
    /// subtitle part of delivery. Subtitle burn-in is request-local too: the
    /// client has to send `subtitle=` before it can change the verdict.
    #[tokio::test]
    async fn audio_only_selection_does_not_apply_the_policy_default_subtitle() {
        use plurx_core::domain::{AudioStream, ProbeResult, SubtitleStream};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let dir = std::env::temp_dir().join(format!(
            "plurx-audio-only-policy-subtitle-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("media dir");
        let path = dir.join("Foreign Audio With PGS.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("media file");
        let file = state
            .store
            .upsert_file(
                seeded.movie,
                &path.to_string_lossy(),
                24,
                1,
                &ProbeResult {
                    duration_ms: Some(60_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        language: Some("fre".into()),
                        default: true,
                        ..Default::default()
                    }],
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "hdmv_pgs_subtitle".into(),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        let caps = "vcodec=h264&acodec=aac&container=mp4&hdr=0";

        let (status, default) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{default}");
        assert_eq!(default["method"], "direct_play", "{default}");

        let (status, audio_only) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/decision?{caps}&audio=0"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{audio_only}");
        assert_eq!(audio_only["method"], default["method"], "{audio_only}");
        assert_eq!(audio_only["delivery"], default["delivery"], "{audio_only}");
        assert_eq!(audio_only["reasons"], default["reasons"], "{audio_only}");
        assert_eq!(
            audio_only["transcode_audio"], default["transcode_audio"],
            "{audio_only}"
        );
        assert_eq!(audio_only["selection"]["audio_index"], 0, "{audio_only}");
        assert_eq!(
            audio_only["selection"]["subtitle_index"], 0,
            "the effective policy subtitle remains visible without changing delivery: {audio_only}"
        );
        assert_eq!(
            audio_only["selection"]["subtitle_requires_burn_in"], true,
            "{audio_only}"
        );
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
        assert_eq!(body["delivered_dynamic_range"], "sdr", "{body}");

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
        assert_eq!(body["delivered_dynamic_range"], "sdr", "{body}");
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

    /// The write path and the read paths have to agree on the primary key.
    /// They did not: `/client-log` classified from the player's own `ua` field
    /// and wrote under `chrome`, while `/decision` and session-create had no
    /// hint and fell through to a header test that matched `AppleWebKit` and
    /// read under `apple`. Every browser prior was maintained and never read.
    /// So this test refuses to touch the store directly — it reports telemetry
    /// and reads the prior back over the wire, with one unmodified shipping
    /// User-Agent on every request, which is the only shape that can catch it.
    #[tokio::test]
    async fn a_browser_reads_back_the_prior_it_reported_over_the_wire() {
        use super::test_agents::{CHROME_WINDOWS_UA, FIREFOX_WINDOWS_UA};

        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;
        state
            .store
            .put_setting(plurx_core::store::keys::PLAYBACK_NETWORK_PRIORS, "1")
            .await
            .expect("enable priors");

        let as_client = |mut request: Request<Body>, ua: &'static str| {
            request.headers_mut().insert(
                "x-forwarded-for",
                axum::http::HeaderValue::from_static("203.0.113.91"),
            );
            request.headers_mut().insert(
                axum::http::header::USER_AGENT,
                axum::http::HeaderValue::from_static(ua),
            );
            request
        };
        let decision_url = format!(
            "/api/v1/files/{}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0",
            s.file
        );

        // The web player reports over `/client-log`, exactly as it ships:
        // no `client` query parameter anywhere, and the `ua` field carrying
        // its own short label rather than the header.
        let (status, _) = call(
            &app,
            as_client(
                post(
                    "/api/v1/client-log",
                    Some(&admin),
                    json!({
                        "event": "ttff",
                        "bandwidth": 9_000,
                        "height": 1080,
                        "ua": "Chrome"
                    }),
                ),
                CHROME_WINDOWS_UA,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Recording is asynchronous, so poll the wire rather than the store.
        let mut decision = json!({});
        for _ in 0..200 {
            let (status, body) = call(
                &app,
                as_client(get(&decision_url, Some(&admin)), CHROME_WINDOWS_UA),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            if body.get("prior_kbps").is_some() {
                decision = body;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            decision
                .get("prior_kbps")
                .and_then(serde_json::Value::as_u64),
            Some(9_000),
            "the browser must read back the prior it just reported: {decision}"
        );

        let (status, session) = call(
            &app,
            as_client(
                post(
                    &format!("/api/v1/files/{}/hls/sessions", s.file),
                    Some(&admin),
                    json!({ "playback_id": "browser-prior-roundtrip", "copy": true }),
                ),
                CHROME_WINDOWS_UA,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{session}");
        assert_eq!(
            session
                .get("prior_kbps")
                .and_then(serde_json::Value::as_u64),
            Some(9_000),
            "session-create must resolve the same key as /decision: {session}"
        );
        if let Some(session_id) = session["session_id"].as_str() {
            state.transcode.stop_session(session_id, "test").await;
        }

        // The class is still part of the key: a different browser on the same
        // /24 starts cold rather than inheriting somebody else's link history.
        let (status, other) = call(
            &app,
            as_client(get(&decision_url, Some(&admin)), FIREFOX_WINDOWS_UA),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{other}");
        assert!(
            other.get("prior_kbps").is_none(),
            "a different client class must not read the Chrome prior: {other}"
        );
    }

    #[tokio::test]
    async fn network_prior_is_opt_in_and_additive_on_decision_and_session_wires() {
        use plurx_core::domain::NetworkPriorObservation;

        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;
        let request_with_network = |mut request: Request<Body>, ua: &'static str| {
            request.headers_mut().insert(
                "x-forwarded-for",
                axum::http::HeaderValue::from_static("192.0.2.143"),
            );
            request.headers_mut().insert(
                axum::http::header::USER_AGENT,
                axum::http::HeaderValue::from_static(ua),
            );
            request
        };
        let decision_url = format!(
            "/api/v1/files/{}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0&client=apple",
            s.file
        );

        let (status, cold) = call(
            &app,
            request_with_network(get(&decision_url, Some(&admin)), "Apple AVPlayer"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cold}");
        assert!(
            cold.get("prior_kbps").is_none(),
            "the default-off response must be byte-for-byte additive: {cold}"
        );
        let (status, cold_session) = call(
            &app,
            request_with_network(
                post(
                    &format!("/api/v1/files/{}/hls/sessions", s.file),
                    Some(&admin),
                    json!({ "playback_id": "network-prior-cold-wire", "copy": true }),
                ),
                "Apple AVPlayer",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{cold_session}");
        assert!(
            cold_session.get("prior_kbps").is_none(),
            "the default-off session response must omit the additive key: {cold_session}"
        );
        if let Some(session_id) = cold_session["session_id"].as_str() {
            state.transcode.stop_session(session_id, "test").await;
        }

        let (status, settings) = call(
            &app,
            put(
                "/api/v1/settings",
                Some(&admin),
                json!({ "playback_network_priors": true }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{settings}");
        assert_eq!(settings["playback_network_priors"], true);
        let user = state
            .store
            .get_user_by_username("paul")
            .await
            .expect("admin lookup")
            .expect("admin user");
        state
            .store
            .observe_network_prior(&NetworkPriorObservation {
                user_id: user.id,
                client_class: "apple".to_owned(),
                network_fingerprint: "192.0.2.0/24".to_owned(),
                throughput_kbps: Some(20_000),
                observed_at_ms: 1_700_000_000_000,
                ..NetworkPriorObservation::default()
            })
            .await
            .expect("seed prior");

        let (status, warm) = call(
            &app,
            request_with_network(get(&decision_url, Some(&admin)), "Apple AVPlayer"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{warm}");
        assert_eq!(warm["prior_kbps"], 20_000, "{warm}");

        let (status, session) = call(
            &app,
            request_with_network(
                post(
                    &format!("/api/v1/files/{}/hls/sessions", s.file),
                    Some(&admin),
                    json!({ "playback_id": "network-prior-wire", "copy": true }),
                ),
                "Apple AVPlayer",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{session}");
        assert_eq!(session["prior_kbps"], 20_000, "{session}");
        if let Some(session_id) = session["session_id"].as_str() {
            state.transcode.stop_session(session_id, "test").await;
        }
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
            "/assets/playback-policy.js",
            "/assets/reader.js",
            "/assets/reader.css",
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
    async fn season_detail_children_include_batched_resolution_and_hdr_facts() {
        use plurx_core::domain::{AudioStream, ItemKind, NewItem, ProbeResult};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let path =
            std::env::temp_dir().join(format!("plurx-season-facts-{}.mkv", uuid::Uuid::new_v4()));
        state
            .store
            .upsert_file(
                seeded.ep,
                &path.to_string_lossy(),
                80_000_000_000,
                2,
                &ProbeResult {
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(3840),
                    height: Some(2160),
                    bit_depth: Some(10),
                    hdr: Some("dolby_vision".into()),
                    hdr_format: Some("Dolby Vision · Profile 7 (HDR10-compatible)".into()),
                    bitrate: Some(48_000_000),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "truehd".into(),
                        channels: Some(8),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("4K episode file");

        let second_episode = state
            .store
            .insert_item(&NewItem {
                library_id: seeded.lib,
                kind: ItemKind::Episode,
                parent_id: Some(seeded.season),
                title: "The Detail".into(),
                year: None,
                season_number: Some(1),
                episode_number: Some(2),
            })
            .await
            .expect("second episode");
        let second_path = std::env::temp_dir().join(format!(
            "plurx-season-facts-second-{}.mp4",
            uuid::Uuid::new_v4()
        ));
        state
            .store
            .upsert_file(
                second_episode,
                &second_path.to_string_lossy(),
                7_000_000_000,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1280),
                    height: Some(720),
                    bit_depth: Some(8),
                    hdr: Some("hdr10".into()),
                    hdr_format: Some("HDR10".into()),
                    bitrate: Some(5_000_000),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        channels: Some(2),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("720p episode file");

        let (status, season) = call(
            &app,
            get(&format!("/api/v1/items/{}", seeded.season), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{season}");
        let episodes = season["children"].as_array().expect("season episodes");
        assert_eq!(episodes.len(), 2, "{season}");
        let episode = &episodes[0];
        assert_eq!(episode["id"], seeded.ep);
        assert_eq!(episode["resolution"], 2160);
        assert_eq!(
            episode["media"],
            json!({
                "files": 2,
                "bytes": 80_000_000_042_i64,
                "video": "HEVC",
                "height": 2160,
                "hdr": "dolby_vision",
                "hdr_format": "Dolby Vision · Profile 7 (HDR10-compatible)",
                "audio": "TrueHD 7.1",
                "container": "MKV"
            })
        );
        let second = &episodes[1];
        assert_eq!(second["id"], second_episode);
        assert_eq!(second["resolution"], 720);
        assert_eq!(
            second["media"],
            json!({
                "files": 1,
                "bytes": 7_000_000_000_i64,
                "video": "H.264",
                "height": 720,
                "hdr": "hdr10",
                "hdr_format": "HDR10",
                "audio": "AAC 2.0",
                "container": "MP4"
            })
        );
    }

    /// ffprobe reports a container's language tag verbatim, and muxers write
    /// `JPN` as readily as `jpn`. Every surface that answers "which audio
    /// track" must therefore fold case identically, or the server advertises a
    /// default on the detail screen that playback then declines to use. The
    /// shared predicate exists precisely so these cannot drift apart; this
    /// test is what fails when a caller reintroduces its own inline match.
    #[tokio::test]
    async fn an_uppercase_language_tag_selects_the_same_audio_on_every_surface() {
        use plurx_core::domain::{AudioStream, ProbeResult, SubtitleStream};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let dir =
            std::env::temp_dir().join(format!("plurx-uppercase-lang-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("media dir");
        let path = dir.join("Shouty Tags.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("media file");
        let file = state
            .store
            .upsert_file(
                seeded.movie,
                &path.to_string_lossy(),
                31,
                1,
                &ProbeResult {
                    duration_ms: Some(60_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![
                        AudioStream {
                            index: 0,
                            codec: "aac".into(),
                            channels: Some(2),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        AudioStream {
                            index: 1,
                            codec: "aac".into(),
                            channels: Some(2),
                            // Upper case on purpose: this is the whole test.
                            language: Some("JPN".into()),
                            ..Default::default()
                        },
                    ],
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "subrip".into(),
                        language: Some("eng".into()),
                        ..Default::default()
                    }],
                    // Marks the row probed, which the offline path requires
                    // before it will quote anything.
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("uppercase-tag file");

        // The shared original-audio rule fires on `JPN`, so every surface must
        // land on the Japanese track paired with the English subtitle — never
        // on the container-default English audio with subtitles off.
        let (status, detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", seeded.movie), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        let shouty = detail["files"]
            .as_array()
            .expect("detail files")
            .iter()
            .find(|f| f["filename"] == "Shouty Tags.mp4")
            .unwrap_or_else(|| panic!("missing uppercase-tag file: {detail}"));
        assert_eq!(
            shouty["playback_defaults"]["audio"]["selected_index"], 1,
            "{shouty}"
        );
        assert_eq!(
            shouty["playback_defaults"]["subtitle"]["selected_index"], 0,
            "{shouty}"
        );

        let (status, decision) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{decision}");
        // An unselected `/decision` keeps its historical shape and marks the
        // policy default on the track itself rather than emitting `selection`.
        let marked_default = |tracks: &str| -> Value {
            decision[tracks]
                .as_array()
                .unwrap_or_else(|| panic!("{tracks} array: {decision}"))
                .iter()
                .find(|track| track["default"] == true)
                .map(|track| track["index"].clone())
                .unwrap_or(Value::Null)
        };
        assert_eq!(
            marked_default("audio"),
            shouty["playback_defaults"]["audio"]["selected_index"],
            "/decision disagreed with the detail screen about an uppercase tag: {decision}"
        );
        assert_eq!(
            marked_default("subtitles"),
            shouty["playback_defaults"]["subtitle"]["selected_index"],
            "{decision}"
        );

        let (status, offline) = call(
            &app,
            get(
                &format!("/api/v1/files/{file}/offline-options"),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{offline}");
        assert_eq!(
            offline["recommended_audio_index"],
            shouty["playback_defaults"]["audio"]["selected_index"],
            "the offline recommendation disagreed with the advertised default: {offline}"
        );
        assert_eq!(
            offline["recommended_subtitle_index"],
            shouty["playback_defaults"]["subtitle"]["selected_index"],
            "{offline}"
        );

        // The fourth surface, and the only one that actually plays the file.
        // The other three merely describe it, so a session that disagreed with
        // them would advertise Japanese and then deliver English — the failure
        // this whole test exists to catch. Compared against the advertised
        // default rather than a literal `1`, so a later change to the policy
        // moves all four together or fails here.
        let session_audio = state
            .transcode
            .session_audio_index(
                &state
                    .store
                    .get_file(file)
                    .await
                    .expect("file lookup")
                    .expect("uppercase-tag file row"),
            )
            .await;
        assert_eq!(
            serde_json::json!(session_audio),
            shouty["playback_defaults"]["audio"]["selected_index"],
            "a live session disagreed with the advertised default about an \
             uppercase tag: session chose {session_audio:?}"
        );
    }

    #[tokio::test]
    async fn item_detail_reports_policy_defaults_and_language_match_states() {
        use plurx_core::domain::{AudioStream, ProbeResult, SubtitleStream};

        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let dir =
            std::env::temp_dir().join(format!("plurx-detail-tracks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("media dir");

        let french_path = dir.join("French Subtitles.mp4");
        std::fs::write(&french_path, b"present").expect("French media");
        state
            .store
            .upsert_file(
                seeded.ep,
                &french_path.to_string_lossy(),
                7,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "subrip".into(),
                        language: Some("fre".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("French subtitle file");

        // Deliberately absent on disk: stored track facts still have a useful
        // answer, and rendering detail must not become a playback decision.
        let no_subs_path = dir.join("Missing No Subtitles.mp4");
        state
            .store
            .upsert_file(
                seeded.ep,
                &no_subs_path.to_string_lossy(),
                8,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("subtitle-free file");

        let anime_path = dir.join("Dual Audio.mp4");
        std::fs::write(&anime_path, b"present").expect("dual-audio media");
        state
            .store
            .upsert_file(
                seeded.ep,
                &anime_path.to_string_lossy(),
                9,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    audio_streams: vec![
                        AudioStream {
                            index: 0,
                            codec: "aac".into(),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        AudioStream {
                            index: 1,
                            codec: "aac".into(),
                            language: Some("jpn".into()),
                            ..Default::default()
                        },
                    ],
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "subrip".into(),
                        language: Some("eng".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("dual-audio file");

        let untagged_path = dir.join("Untagged Tracks.mp4");
        std::fs::write(&untagged_path, b"present").expect("untagged media");
        state
            .store
            .upsert_file(
                seeded.ep,
                &untagged_path.to_string_lossy(),
                10,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        language: None,
                        default: true,
                        ..Default::default()
                    }],
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "subrip".into(),
                        language: None,
                        default: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("untagged file");

        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry()
            .with(crate::logbuf::BufferLayer(Arc::clone(&state.logs)));
        let _log_guard = tracing::subscriber::set_default(subscriber);
        let (status, detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", seeded.ep), Some(&admin)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        let files = detail["files"].as_array().expect("detail files");
        let named = |filename: &str| {
            files
                .iter()
                .find(|file| file["filename"] == filename)
                .unwrap_or_else(|| panic!("missing {filename}: {detail}"))
        };

        let ordinary = named("Heat.mp4");
        assert_eq!(ordinary["playback_defaults"]["audio"]["selected_index"], 0);
        assert_eq!(
            ordinary["playback_defaults"]["audio"]["preferred_language_status"],
            "selected"
        );
        assert_eq!(
            ordinary["playback_defaults"]["subtitle"]["selected_index"],
            Value::Null
        );
        assert_eq!(
            ordinary["playback_defaults"]["subtitle"]["preferred_language_status"], "available",
            "English subtitles exist but Auto leaves them off over English audio: {ordinary}"
        );

        let french = named("French Subtitles.mp4");
        assert_eq!(
            french["playback_defaults"]["subtitle"]["preferred_language"],
            "eng"
        );
        assert_eq!(
            french["playback_defaults"]["subtitle"]["preferred_language_status"], "missing",
            "{french}"
        );

        let none = named("Missing No Subtitles.mp4");
        assert_eq!(none["available"], false, "{none}");
        assert_eq!(
            none["playback_defaults"]["subtitle"]["preferred_language_status"], "no_tracks",
            "{none}"
        );

        let anime = named("Dual Audio.mp4");
        assert_eq!(anime["playback_defaults"]["audio"]["selected_index"], 1);
        assert_eq!(
            anime["playback_defaults"]["audio"]["preferred_language_status"], "available",
            "English is present, but the shared original-audio rule selects Japanese: {anime}"
        );
        assert_eq!(
            anime["playback_defaults"]["subtitle"]["selected_index"], 0,
            "{anime}"
        );
        assert_eq!(
            anime["playback_defaults"]["subtitle"]["preferred_language_status"], "selected",
            "{anime}"
        );

        let untagged = named("Untagged Tracks.mp4");
        assert_eq!(
            untagged["playback_defaults"]["audio"]["preferred_language_status"], "unknown",
            "an absent language tag is not evidence that English audio is missing: {untagged}"
        );
        assert_eq!(
            untagged["playback_defaults"]["subtitle"]["preferred_language_status"], "unknown",
            "an absent language tag is not evidence that English subtitles are missing: {untagged}"
        );
        assert_eq!(
            state.transcode.active_sessions().await,
            0,
            "browsing detail must not create playback work"
        );
        assert!(
            state
                .logs
                .tail("trace", usize::MAX)
                .iter()
                .all(|entry| !entry.message.contains("playback capability decision")),
            "browsing detail must not emit a playback decision log"
        );
    }

    #[tokio::test]
    async fn shared_native_api_fixture_uses_the_servers_live_wire_keys() {
        const FIXTURE: &str = include_str!("../../../../tests/contracts/native-api.json");

        fn assert_pointers(contract: &Value, live: &Value, pointers: &[&str]) {
            for pointer in pointers {
                let expected = contract
                    .pointer(pointer)
                    .unwrap_or_else(|| panic!("contract fixture has no {pointer}"));
                let actual = live
                    .pointer(pointer)
                    .unwrap_or_else(|| panic!("live response has no {pointer}: {live}"));
                if !actual.is_null() {
                    assert_eq!(
                        std::mem::discriminant(expected),
                        std::mem::discriminant(actual),
                        "wire type changed at {pointer}: fixture={expected}, live={actual}"
                    );
                }
            }
        }

        let contract: Value = serde_json::from_str(FIXTURE).expect("native API fixture JSON");
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let audiobook = seed_audiobook(&state).await;
        let (_, server) = call(&app, get("/api/v1/server", None)).await;
        assert_pointers(
            &contract["server"],
            &server,
            &[
                "/setup_required",
                "/name",
                "/version",
                "/build",
                "/instance_id",
            ],
        );

        let (_, detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", seeded.ep), Some(&admin)),
        )
        .await;
        assert_pointers(
            &contract["item_detail"],
            &detail,
            &[
                "/item/id",
                "/item/library_id",
                "/item/kind",
                "/item/title",
                "/item/added_at",
                "/item/updated_at",
                "/item/tags",
                "/item/genres",
                "/files/0/id",
                "/files/0/filename",
                "/files/0/video_codec",
                "/files/0/audio_streams",
                "/files/0/subtitle_streams",
                "/files/0/playback_defaults/audio/selected_index",
                "/files/0/playback_defaults/audio/preferred_language",
                "/files/0/playback_defaults/audio/preferred_language_status",
                "/files/0/playback_defaults/subtitle/selected_index",
                "/files/0/playback_defaults/subtitle/preferred_language",
                "/files/0/playback_defaults/subtitle/preferred_language_status",
                "/files/0/available",
                "/files/0/probed",
                "/children",
                "/ancestors",
                "/reading",
            ],
        );

        let (_, audiobook_detail) = call(
            &app,
            get(&format!("/api/v1/items/{}", audiobook.item), Some(&admin)),
        )
        .await;
        assert_pointers(
            &contract["audiobook_detail"],
            &audiobook_detail,
            &[
                "/item/kind",
                "/item/runtime_ms",
                "/files/0/chapters/0/index",
                "/files/0/chapters/0/title",
                "/files/0/chapters/0/start_ms",
                "/files/0/chapters/0/end_ms",
                "/files/1/part_offset_ms",
            ],
        );
        assert_eq!(
            audiobook_detail["files"]
                .as_array()
                .expect("audiobook files")
                .iter()
                .map(|file| file["filename"].as_str().expect("filename"))
                .collect::<Vec<_>>(),
            ["Part 1.mp3", "Part 2.mp3", "Part 10.mp3"]
        );
        assert!(
            audiobook_detail["files"][0].get("part_offset_ms").is_none(),
            "the zero offset keeps its backwards-compatible omitted shape"
        );
        assert_eq!(audiobook_detail["files"][1]["part_offset_ms"], 60_000);
        assert_eq!(audiobook_detail["files"][2]["part_offset_ms"], 180_000);

        let (_, page) = call(
            &app,
            get(
                &format!("/api/v1/libraries/{}/items", seeded.lib),
                Some(&admin),
            ),
        )
        .await;
        assert_pointers(
            &contract["page"],
            &page,
            &["/items", "/total", "/offset", "/limit"],
        );

        let (_, decision) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{}/decision?vcodec=h264,hevc&acodec=aac&container=mp4&hdr=1",
                    seeded.file
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_pointers(
            &contract["decision"],
            &decision,
            &[
                "/file_id",
                "/method",
                "/play_url",
                "/delivery/mode",
                "/source/container",
                "/reasons",
                "/audio",
                "/subtitles",
                "/markers",
                "/ladder",
                "/audio_offset_ms",
                "/delivered_dynamic_range",
            ],
        );
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
                &format!("/api/v1/items/{}/progress", s.ep),
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
        // The legacy A/V endpoint still accepts an old client, but no longer
        // persists its correction into later plays.
        let (status, offset) = call(
            &app,
            put(
                &format!("/api/v1/files/{}/audio-offset", s.file),
                Some(&admin),
                json!({ "offset_ms": 250 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(offset["audio_offset_ms"], 250);
        assert_eq!(
            state
                .store
                .get_file(s.file)
                .await
                .expect("read file")
                .expect("file")
                .audio_offset_ms,
            0,
            "manual sync belongs to the active play, not the media file"
        );
        // A value left behind by an older server is ignored as well: opening
        // the title again always begins at neutral sync.
        state
            .store
            .set_file_audio_offset(s.file, 250)
            .await
            .expect("seed historical audio offset");
        let (status, decision) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{}/decision?vcodec=h264,hevc&acodec=aac&container=mp4&hdr=0",
                    s.file
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(decision["audio_offset_ms"], 0);
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
        assert_eq!(before["offline_enabled"], true);
        assert_eq!(before["offline_max_gb"], 25);
        assert_eq!(before["offline_max_gb_per_user"], 15);
        assert_eq!(before["offline_max_rows_per_user"], 50);
        assert_eq!(before["hls_typeless_sliding"], false);
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
                        "artwork_retry_mins": 0, "offline_enabled": false,
                        "offline_max_gb": 40, "offline_max_gb_per_user": 20,
                        "offline_max_rows_per_user": 75,
                        "hls_typeless_sliding": true }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(after["scan_on_startup"], true);
        assert_eq!(after["probe_retry_mins"], 1440);
        assert_eq!(after["offline_enabled"], false);
        assert_eq!(after["offline_max_gb"], 40);
        assert_eq!(after["offline_max_gb_per_user"], 20);
        assert_eq!(after["offline_max_rows_per_user"], 75);
        assert_eq!(after["hls_typeless_sliding"], true);
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
    async fn both_progress_routes_use_the_coalescer_and_dated_writes_win() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let seeded = seed_content(&state).await;
        let user_id = state
            .store
            .list_users()
            .await
            .expect("users")
            .into_iter()
            .find(|user| user.is_admin)
            .expect("admin user")
            .id;

        let api_progress = |position_ms: i64, recorded_at: Option<i64>| {
            post(
                &format!("/api/v1/items/{}/progress", seeded.movie),
                Some(&admin),
                json!({
                    "position_ms": position_ms,
                    "duration_ms": 9_000_000,
                    "recorded_at": recorded_at,
                }),
            )
        };
        call(&app, api_progress(1_000, None)).await;
        let (_, coalesced) = call(&app, api_progress(50_000, None)).await;
        assert_eq!(
            coalesced["position_ms"], 1_000,
            "the HTTP body remains the coherent durable row while a beat is pending"
        );
        assert_eq!(
            state
                .store
                .watch_state(user_id, seeded.movie)
                .await
                .expect("watch state")
                .expect("watch row")
                .position_ms,
            1_000,
            "the second REST beat must take the coalescer path"
        );

        let imported_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Unix clock")
            .as_secs() as i64;
        call(&app, api_progress(80_000, Some(imported_at))).await;
        state.progress.drain().await.expect("drain stale beat");
        assert_eq!(
            state
                .store
                .watch_state(user_id, seeded.movie)
                .await
                .expect("watch state")
                .expect("watch row")
                .position_ms,
            80_000,
            "the dated bypass must invalidate an older pending beat"
        );

        let plex = |position_ms: i64| {
            Request::builder()
                .uri(format!(
                    "/:/timeline?ratingKey={}&time={position_ms}&duration=9000000",
                    seeded.ep
                ))
                .header("x-plex-token", &admin)
                .body(Body::empty())
                .expect("Plex timeline request")
        };
        assert!(status_of(&app, plex(1_000)).await.is_success());
        assert!(status_of(&app, plex(60_000)).await.is_success());
        state.progress.drain().await.expect("drain Plex beat");
        assert_eq!(
            state
                .store
                .watch_state(user_id, seeded.ep)
                .await
                .expect("Plex watch state")
                .expect("Plex watch row")
                .position_ms,
            60_000,
            "the Plex timeline must share the coalescer"
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
        let (_stream, _guard) = state
            .streams
            .register("pb-1-s1", 9999, "someone-else", 42, 4.0);
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

    /// The registry is filled from the detached task the delivery seam
    /// spawns, because the request that triggers it is busy writing a film to
    /// a socket. Wait for it the way the activity page does — by looking again
    /// — rather than by sleeping a guessed interval and hoping.
    async fn settled_deliveries(app: &Router, token: &str, want: usize) -> Vec<Value> {
        for _ in 0..200 {
            let (_, page) = call(app, get("/api/v1/activity/detail", Some(token))).await;
            let rows = page["deliveries"].as_array().cloned().unwrap_or_default();
            if rows.len() >= want {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (_, page) = call(app, get("/api/v1/activity/detail", Some(token))).await;
        page["deliveries"].as_array().cloned().unwrap_or_default()
    }

    fn ranged(uri: &str, from: u64, to: u64) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("range", format!("bytes={from}-{to}"))
            .body(Body::empty())
            .expect("req")
    }

    /// Direct play is a *storm* of ranged 206s, not a connection: a seeking
    /// browser makes dozens of short requests against one file. One row per
    /// request would put a dozen phantom viewers on the activity page for one
    /// person on one sofa, which is the whole reason the registry is keyed by
    /// the player rather than the request.
    #[tokio::test]
    async fn a_direct_play_is_one_viewer_however_many_ranges_it_takes() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        let uri = format!("/api/v1/files/{}/direct?token={admin}", s.file);
        for from in 0..12u64 {
            let status = app
                .clone()
                .oneshot(ranged(&uri, from, from + 1))
                .await
                .expect("r")
                .status();
            assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        }

        let began = std::time::Instant::now();
        let rows = settled_deliveries(&app, &admin, 1).await;
        let visible_in = began.elapsed();
        eprintln!("direct play visible {visible_in:?} after the last range request");
        assert!(
            visible_in < crate::delivery::BEACON_INTERVAL,
            "a start must show up inside one beacon, not on the next one: {visible_in:?}"
        );
        assert_eq!(rows.len(), 1, "twelve requests, one viewer: {rows:?}");
        assert_eq!(rows[0]["method"], "direct");
        // `seed_content` re-upserts this path onto the episode, so the file's
        // item is "The Target" — the title is resolved from the file, not
        // assumed from whatever created it.
        assert_eq!(rows[0]["title"], "The Target");
        assert_eq!(rows[0]["user"], "paul");
        assert_eq!(rows[0]["file_id"], s.file);
        assert!(
            rows[0]["session_id"].is_null(),
            "direct play holds no session — that is the point"
        );

        // And nothing was invented on the old array to say so.
        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert!(
            page["sessions"].as_array().expect("sessions").is_empty(),
            "the sessions array still means HLS sessions: {page}"
        );

        // Two beats of silence is a quiet player, not a gone one.
        state
            .direct_plays
            .backdate(crate::delivery::BEACON_INTERVAL * 2);
        assert_eq!(
            settled_deliveries(&app, &admin, 1).await.len(),
            1,
            "a missed beacon must not evict somebody who is still watching"
        );
        // The rest of the timeout, with no beacon and no range request, is a
        // closed tab — which announces nothing, so silence is all there is.
        state
            .direct_plays
            .backdate(crate::delivery::IDLE_TIMEOUT - crate::delivery::BEACON_INTERVAL * 2);
        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert!(
            page["deliveries"].as_array().expect("array").is_empty(),
            "expired after the idle timeout: {page}"
        );

        // The progress beacon alone keeps a paused player listed — it has
        // buffered the rest of the film and will not fetch another byte.
        let status = app
            .clone()
            .oneshot(ranged(&uri, 0, 1))
            .await
            .expect("r")
            .status();
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(settled_deliveries(&app, &admin, 1).await.len(), 1);
        state
            .direct_plays
            .backdate(crate::delivery::IDLE_TIMEOUT - std::time::Duration::from_secs(1));
        let (st, _) = call(
            &app,
            post(
                &format!("/api/v1/items/{}/progress", s.ep),
                Some(&admin),
                json!({ "position_ms": 60_000, "duration_ms": 9_000_000 }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        state
            .direct_plays
            .backdate(std::time::Duration::from_secs(2));
        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert_eq!(
            page["deliveries"].as_array().expect("array").len(),
            1,
            "the beacon reset the clock a range request never would have: {page}"
        );
    }

    /// All four routes at once, each under its own name, from the three places
    /// that actually know: the transcode manager, the progressive registry,
    /// and the direct-play registry that S2 added because nothing else held
    /// one. Each row disappears the way its delivery ends.
    #[tokio::test]
    async fn every_delivery_method_is_listed_under_its_own_name() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;
        // Two software sessions coexist below; on a 2-core runner the
        // machine-derived pool would refuse the second and fail this test for
        // a reason that has nothing to do with what it is testing.
        state
            .store
            .put_setting(plurx_core::store::keys::SW_POOL_THREADS, "64")
            .await
            .expect("pool headroom");

        // 1. direct — a range request, no session anywhere.
        let status = app
            .clone()
            .oneshot(ranged(
                &format!("/api/v1/files/{}/direct?token={admin}", s.file),
                0,
                3,
            ))
            .await
            .expect("r")
            .status();
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);

        // 2. remux — the response body owns the registration, so holding the
        // response is holding the stream open, exactly as a player does.
        let remux = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/files/{}/stream.mp4?stream=pb-remux", s.file),
                Some(&admin),
            ))
            .await
            .expect("r");
        assert_eq!(remux.status(), StatusCode::OK);

        // 3 & 4. the two HLS kinds, which differ only in what ffmpeg is asked
        // to do — and never in the encoder label, which is why the method is
        // recorded rather than inferred.
        let (st, copy) = call(
            &app,
            post(
                &format!("/api/v1/files/{}/hls/sessions", s.file),
                Some(&admin),
                json!({ "playback_id": "pb-copy", "copy": true }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{copy}");
        let (st, tx) = call(
            &app,
            post(
                &format!("/api/v1/files/{}/hls/sessions", s.file),
                Some(&admin),
                json!({ "playback_id": "pb-transcode", "height": 720 }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{tx}");

        let rows = settled_deliveries(&app, &admin, 4).await;
        let mut methods: Vec<&str> = rows
            .iter()
            .map(|r| r["method"].as_str().unwrap_or("?"))
            .collect();
        methods.sort_unstable();
        assert_eq!(
            methods,
            ["direct", "hls-copy", "remux", "transcode"],
            "four routes, four names: {rows:?}"
        );
        for row in &rows {
            assert_eq!(row["user"], "paul", "{row}");
            assert_eq!(row["title"], "The Target", "{row}");
            assert_eq!(row["file_id"], s.file, "{row}");
        }
        // Only the two that really are sessions carry a session id, and they
        // are the two the old array has always listed.
        let with_session = rows.iter().filter(|r| !r["session_id"].is_null()).count();
        assert_eq!(with_session, 2, "{rows:?}");
        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        let sessions = page["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), 2, "unchanged in meaning: {page}");
        assert!(
            sessions[0].get("encoder").is_some() && sessions[0].get("user_name").is_some(),
            "and unchanged in shape: {page}"
        );

        // Each row ends the way its delivery does. The two sessions when they
        // are stopped...
        for id in [&copy["session_id"], &tx["session_id"]] {
            let id = id.as_str().expect("session id");
            assert!(state.transcode.stop_session(id, "test").await);
        }
        // ...the remux when its body is dropped, which is the same moment
        // `kill_on_drop` takes its ffmpeg down...
        drop(remux);
        // ...and the direct play only when it goes quiet, because nothing
        // about it ever ends.
        state.direct_plays.backdate(crate::delivery::IDLE_TIMEOUT);

        let (_, page) = call(&app, get("/api/v1/activity/detail", Some(&admin))).await;
        assert!(
            page["deliveries"].as_array().expect("array").is_empty(),
            "everything went with the thing that was delivering it: {page}"
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

        // The grid asks the same question the detail page does — a library
        // filtered by "Watched" has to classify the show card itself — so the
        // list carries the same rollup, batched for the whole page.
        let (_, page) = call(
            &app,
            get(&format!("/api/v1/libraries/{}/items", s.lib), Some(&admin)),
        )
        .await;
        let row = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["id"] == s.show)
            .expect("the show is on the page")
            .clone();
        assert_eq!(row["rollup"]["leaves"], 1);
        assert_eq!(row["rollup"]["watched"], 0);
        let movie_row = page["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["id"] == s.movie)
            .expect("the movie is on the page")
            .clone();
        assert!(
            movie_row["rollup"].is_null(),
            "a leaf still answers with its own watch row, not a rollup"
        );

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
            get_q(&format!("/api/v1/files/{file}/subs/0.vtt?token={admin}")),
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
        // This endpoint now mirrors the live video playlist. Replace the
        // general router fixture's placeholder bytes with a tiny real source
        // so the copy segmenter can publish that playlist.
        let media = state
            .store
            .get_file(s.file)
            .await
            .expect("file read")
            .expect("file");
        let made = std::process::Command::new(crate::ffmpeg::ffmpeg_bin())
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=10:d=16",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=stereo:sample_rate=48000",
                "-t",
                "16",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "20",
                "-c:a",
                "aac",
            ])
            .arg(&media.path)
            .output()
            .expect("spawn ffmpeg");
        assert!(
            made.status.success(),
            "HLS fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );

        // Unknown session → 404 for both playlist and segment.
        assert_eq!(
            status_of(&app, get_q("/api/v1/hls/nosuchsession/index.m3u8")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(&app, get_q("/api/v1/hls/nosuchsession/master.m3u8")).await,
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

        // Apple opts into a master playlist whose child subtitle URLs are
        // authenticated by the unguessable session capability. AVPlayer adds
        // no bearer header to these autonomous requests.
        let (st, native) = call(
            &app,
            post(
                &format!("/api/v1/files/{}/hls/sessions", s.file),
                Some(&admin),
                json!({
                    "playback_id": "apple-native-subs",
                    "request_id": "native-subs-attempt",
                    // Deliberately NOT on a keyframe. The fixture above is
                    // 10 fps with `-g 20`, so its keyframes are every 2 s and
                    // a copy session asked for 10.5 s actually begins at
                    // 10.0 s — `-noaccurate_seek` seeks backwards and
                    // `-avoid_negative_ts make_zero` calls that keyframe t=0.
                    // A start that lands exactly on a keyframe (which 10.0
                    // did) makes this test pass whether cues are shifted by
                    // the request or by the media origin, which is how the
                    // half-second lead below shipped.
                    "start": 10.5,
                    "copy": true,
                    "native_subtitles": true,
                    "subtitle": 0
                }),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{native}");
        assert_eq!(
            native["media_origin_ms"], 10_000,
            "copy-session local time zero must map to the preceding keyframe: {native}"
        );
        // The session's own answer, on the wire: `StartResponse` skips the key
        // when it is None, so this is the only thing that would catch it
        // silently disappearing from every create.
        assert_eq!(native["delivered_dynamic_range"], "sdr", "{native}");
        let session = native["session_id"].as_str().expect("session");
        let expected_playlist = format!("/api/v1/hls/{session}/master.m3u8?subtitle=0");
        assert_eq!(
            native["playlist_url"].as_str(),
            Some(expected_playlist.as_str())
        );

        // Progressive copy has no JSON start envelope, so it exposes the same
        // keyframe-aligned origin before attachment in a response header.
        let progressive = app
            .clone()
            .oneshot(get(
                &format!("/api/v1/files/{}/stream.mp4?start=10.5", s.file),
                Some(&admin),
            ))
            .await
            .expect("progressive response");
        assert_eq!(progressive.status(), StatusCode::OK);
        assert_eq!(
            progressive
                .headers()
                .get(stream::MEDIA_ORIGIN_MS_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("10000")
        );
        drop(progressive);

        let (status, master) = body_of(
            &app,
            get_q(&format!("/api/v1/hls/{session}/master.m3u8?subtitle=0")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let master = String::from_utf8(master).expect("master utf8");
        assert!(master.contains("TYPE=SUBTITLES"), "{master}");
        assert!(master.contains("LANGUAGE=\"en\",DEFAULT=YES"), "{master}");
        assert!(master.contains("subs/0/index.m3u8"), "{master}");

        // Sessions returned before the dedicated path was introduced remain
        // playable for their lifetime through the query-form bridge.
        let (legacy_status, legacy_master) = body_of(
            &app,
            get_q(&format!(
                "/api/v1/hls/{session}/index.m3u8?native=1&subtitle=0"
            )),
        )
        .await;
        assert_eq!(legacy_status, StatusCode::OK);
        assert_eq!(legacy_master, master.as_bytes());

        let (status, subtitle_playlist) = body_of(
            &app,
            get_q(&format!("/api/v1/hls/{session}/subs/0/index.m3u8")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let subtitle_playlist = String::from_utf8(subtitle_playlist).expect("playlist utf8");
        assert!(
            subtitle_playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"),
            "{subtitle_playlist}"
        );
        assert!(
            subtitle_playlist.contains("seg00000.vtt"),
            "{subtitle_playlist}"
        );
        assert!(subtitle_playlist.contains("#EXT-X-ENDLIST"));

        // A cold embedded subtitle is a full-file extraction. AVPlayer gives
        // each VTT segment only about two seconds and holds the video in
        // `AVPlayerWaitingToMinimizeStallsReason` when that request does not
        // answer, so the segment route must return a valid empty window while
        // the sidecar warms instead of awaiting the scan.
        let (status, cold) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            body_of(
                &app,
                get_q(&format!("/api/v1/hls/{session}/subs/0/seg00000.vtt")),
            ),
        )
        .await
        .expect("a cold subtitle segment must answer before AVPlayer's deadline");
        assert_eq!(status, StatusCode::OK);
        let cold = String::from_utf8(cold).expect("cold VTT utf8");
        assert!(cold.starts_with("WEBVTT"), "{cold}");
        assert!(
            cold.contains("X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000"),
            "the empty answer still has to align with its video segment: {cold}"
        );

        // Seed the extraction cache so the handler test can focus on the
        // capability and timeline mapping rather than the placeholder MP4.
        let file = state
            .store
            .get_file(s.file)
            .await
            .expect("file read")
            .expect("file");
        tokio::fs::create_dir_all(&state.subs_dir)
            .await
            .expect("subs dir");
        let cached = crate::subtitles::vtt_path(&state.subs_dir, &file, 0);
        tokio::fs::write(
            cached,
            "WEBVTT\n\n00:00:09.000 --> 00:00:11.000\ncrossing\n\n00:00:15.000 --> 00:00:16.000\nafter\n",
        )
        .await
        .expect("cached VTT");
        let (status, shifted) = body_of(
            &app,
            get_q(&format!("/api/v1/hls/{session}/subs/0/seg00000.vtt")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let shifted = String::from_utf8(shifted).expect("VTT utf8");
        // The cue is authored 9.000 → 11.000. The session's media begins at
        // the 10.0 s keyframe, so it belongs at −1.000 → 1.000 and is emitted
        // from the window edge to 1.000. Shifting by the REQUESTED 10.5 s
        // instead would end it at 0.500 — the whole of P0-2, at this
        // fixture's two-second GOP. On a 4K film's GOP the same mistake is
        // seconds of visible lead.
        assert!(
            shifted.contains("00:00:00.000 --> 00:00:01.000"),
            "cues must be shifted by the session's media origin, not by the \
             requested start:\n{shifted}"
        );
        assert!(
            !shifted.contains("--> 00:00:00.500"),
            "cues are being shifted by the requested start (P0-2):\n{shifted}"
        );
        assert!(shifted.contains("X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000"));

        // The old file-id route is still bearer protected; only URLs rooted
        // in the live session capability are headerless.
        assert_eq!(
            status_of(&app, get_q(&format!("/api/v1/files/{}/subs/0", s.file))).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_of(
                &app,
                get_q("/api/v1/hls/not-the-capability/subs/0/index.m3u8")
            )
            .await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(
                &app,
                get_q(&format!("/api/v1/hls/{session}/subs/0/not-a-segment.vtt"))
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    /// A file whose subtitle list can express every native-subtitle refusal:
    /// an English SRT that converts cleanly, an Italian SRT flagged as the
    /// *container* default (the track route-level Off must never resurrect),
    /// a PGS bitmap and a styled ASS track — neither of which can become a
    /// WebVTT rendition. `seed_content` deliberately keeps one English SRT and
    /// other tests pin that shape, so this stands up its own library, and its
    /// own real (if tiny) clip, because the session routes need a source
    /// ffmpeg can actually open.
    ///
    /// The clip carries no subtitle data — nothing here extracts cues, it all
    /// stops at validation — so the tracks live in the probe only.
    async fn seed_mixed_subtitles(state: &AppState, hdr: Option<&str>) -> i64 {
        use plurx_core::domain::{
            AudioStream, ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult, SubtitleStream,
        };

        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Mixed subs".into(),
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
                title: "Il Sorpasso".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("movie");

        let dir = std::env::temp_dir().join(format!("plurx-mixedsubs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("mixed.mp4");
        let made = std::process::Command::new(crate::ffmpeg::ffmpeg_bin())
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=64x64:r=10:d=8",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=channel_layout=stereo:sample_rate=48000",
                "-t",
                "8",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "20",
                "-c:a",
                "aac",
            ])
            .arg(&path)
            .output()
            .expect("spawn ffmpeg");
        assert!(
            made.status.success(),
            "mixed-subtitle fixture encode failed: {}",
            String::from_utf8_lossy(&made.stderr)
        );

        let probe = ProbeResult {
            duration_ms: Some(8_000),
            container: Some("mp4".into()),
            video_codec: Some("h264".into()),
            width: Some(64),
            height: Some(64),
            bit_depth: Some(if hdr.is_some() { 10 } else { 8 }),
            hdr: hdr.map(str::to_owned),
            hdr_format: hdr.map(str::to_owned),
            bitrate: Some(400_000),
            audio_streams: vec![AudioStream {
                index: 0,
                codec: "aac".into(),
                channels: Some(2),
                language: Some("eng".into()),
                default: true,
                ..Default::default()
            }],
            subtitle_streams: vec![
                SubtitleStream {
                    index: 0,
                    codec: "subrip".into(),
                    language: Some("eng".into()),
                    ..Default::default()
                },
                // The container's own default, in a language nobody asked
                // for. §3.3: never a fallback.
                SubtitleStream {
                    index: 1,
                    codec: "subrip".into(),
                    language: Some("ita".into()),
                    default: true,
                    ..Default::default()
                },
                SubtitleStream {
                    index: 2,
                    codec: "hdmv_pgs_subtitle".into(),
                    language: Some("eng".into()),
                    ..Default::default()
                },
                SubtitleStream {
                    index: 3,
                    codec: "ass".into(),
                    language: Some("eng".into()),
                    title: Some("Signs & Songs".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        state
            .store
            .upsert_file(movie, &path.to_string_lossy(), 8_888, 3, &probe)
            .await
            .expect("file")
    }

    /// A native selection the server cannot serve must die at `create`, before
    /// an encoder exists — the handoff's "server validation still rejects"
    /// claim, which nothing tested. Two refusals are owed: an index off the end
    /// of the decision's subtitle list, and an index naming a track whose
    /// pixels (PGS) or styling (ASS) cannot survive conversion to WebVTT. The
    /// error text is asserted too, because a 400 for the wrong reason (a
    /// malformed body, say) would otherwise pass this test.
    #[tokio::test]
    async fn hls_create_rejects_native_subtitle_indices_it_cannot_serve() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let file = seed_mixed_subtitles(&state, None).await;

        for (index, why) in [
            (9_i64, "unknown native subtitle track"),
            (2, "the selected subtitle requires burn-in"),
            (3, "the selected subtitle requires burn-in"),
        ] {
            let (status, body) = call(
                &app,
                post(
                    &format!("/api/v1/files/{file}/hls/sessions"),
                    Some(&admin),
                    json!({
                        "playback_id": "native-validation",
                        "copy": true,
                        "native_subtitles": true,
                        "subtitle": index
                    }),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "native subtitle {index} cannot become a WebVTT rendition and \
                 must be refused before an encoder is spawned: {body}"
            );
            assert_eq!(
                body["error"].as_str(),
                Some(why),
                "the 400 for native subtitle {index} must say why it was \
                 refused, or the client cannot tell a bad index from a bad \
                 request: {body}"
            );
        }

        // The control: the same request with a convertible track is accepted,
        // so the refusals above are about the subtitle index and nothing else.
        let (status, body) = call(
            &app,
            post(
                &format!("/api/v1/files/{file}/hls/sessions"),
                Some(&admin),
                json!({
                    "playback_id": "native-validation",
                    "copy": true,
                    "native_subtitles": true,
                    "subtitle": 0
                }),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a text track that converts must still be accepted: {body}"
        );
    }

    /// A pre-guard client can ask the server to burn a bitmap track without
    /// saying what video delivery it is replacing. The server must not treat
    /// that missing context as permission to turn a known HDR source into
    /// H.264 SDR. The refusal happens before playback accounting or ffmpeg.
    #[tokio::test]
    async fn hls_create_refuses_hdr_subtitle_burns_at_the_server_boundary() {
        crate::transcode::require_ffmpeg();
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let file = seed_mixed_subtitles(&state, Some("hdr10")).await;

        let (status, preflight) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=1&subtitle=2"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{preflight}");
        assert_eq!(preflight["method"], "direct_play", "{preflight}");
        assert_eq!(
            preflight["selection"]["subtitle_requires_burn_in"], true,
            "{preflight}"
        );
        assert_eq!(
            preflight["selection"]["subtitle_burn_in_blocked_by_hdr"], true,
            "selection preflight must preserve the established HDR refusal: {preflight}"
        );
        assert_eq!(preflight["reasons"], json!([]), "{preflight}");

        let (status, body) = call(
            &app,
            post(
                &format!("/api/v1/files/{file}/hls/sessions"),
                Some(&admin),
                json!({
                    "playback_id": "old-client-hdr-burn",
                    "height": 64,
                    "subtitle_burn": 2
                }),
            ),
        )
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["code"], "hdr_subtitle_burn_refused");
        assert_eq!(
            body["error"],
            "That subtitle requires an SDR burn-in. HDR playback was kept unchanged."
        );
    }

    /// `/decision` used to promise more than the server would accept: a
    /// `mov_text` track came back `text: true` and nothing else, so a client
    /// offered it, asked for it as a native rendition, and got a 400 from a
    /// master that never listed it. `native` closes that gap on the wire —
    /// this walks the whole loop on one file that carries all three classes.
    #[tokio::test]
    async fn decision_marks_which_subtitles_can_be_native_hls_renditions() {
        let (app, state) = test_state();
        let admin = setup_admin(&app).await;
        let s = seed_content(&state).await;

        let dir = std::env::temp_dir().join(format!("plurx-subs-dto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("WEB-DL.mp4");
        std::fs::write(&path, b"\x00\x00\x00\x18ftypmp42 placeholder").expect("write");
        let sub = |index: i64, codec: &str| plurx_core::domain::SubtitleStream {
            index,
            codec: codec.into(),
            language: Some("eng".into()),
            ..Default::default()
        };
        let probe = plurx_core::domain::ProbeResult {
            duration_ms: Some(600_000),
            container: Some("mp4".into()),
            video_codec: Some("h264".into()),
            width: Some(3840),
            height: Some(2160),
            audio_streams: vec![plurx_core::domain::AudioStream {
                index: 0,
                codec: "aac".into(),
                channels: Some(2),
                default: true,
                ..Default::default()
            }],
            subtitle_streams: vec![
                sub(0, "subrip"),
                sub(1, "mov_text"),
                sub(2, "ass"),
                sub(3, "hdmv_pgs_subtitle"),
            ],
            ..Default::default()
        };
        let file = state
            .store
            .upsert_file(s.movie, &path.to_string_lossy(), 88, 1, &probe)
            .await
            .expect("file");

        let (status, body) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let flags: Vec<(&str, bool, bool)> = body["subtitles"]
            .as_array()
            .expect("subtitles")
            .iter()
            .map(|t| {
                (
                    t["codec"].as_str().expect("codec"),
                    t["text"].as_bool().expect("text"),
                    t["native"].as_bool().expect("native"),
                )
            })
            .collect();
        assert_eq!(
            flags,
            vec![
                ("subrip", true, true),
                ("mov_text", true, false),
                ("ass", true, false),
                ("hdmv_pgs_subtitle", false, false),
            ],
            "{body}"
        );

        let (status, selected) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0&subtitle=3"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        assert_eq!(selected["selection"]["subtitle_index"], 3, "{selected}");
        assert_eq!(
            selected["selection"]["subtitle_requires_burn_in"], true,
            "bitmap subtitles without an enabled overlay must disclose their cost: {selected}"
        );
        assert_eq!(
            selected["selection"]["subtitle_burn_in_blocked_by_hdr"], false,
            "{selected}"
        );
        assert_eq!(selected["subtitles"][3]["default"], true, "{selected}");
        assert_eq!(selected["method"], "transcode", "{selected}");
        assert_eq!(selected["delivery"]["mode"], "transcode", "{selected}");
        assert!(
            selected["reasons"]
                .as_array()
                .is_some_and(|reasons| reasons.iter().any(|reason| {
                    reason
                        .as_str()
                        .is_some_and(|reason| reason.contains("requires burn-in"))
                })),
            "{selected}"
        );

        let (status, off) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0&subtitle=-1"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{off}");
        assert_eq!(off["selection"]["subtitle_index"], Value::Null, "{off}");
        assert_eq!(
            off["selection"]["subtitle_requires_burn_in"], false,
            "{off}"
        );
        assert_eq!(
            off["selection"]["subtitle_burn_in_blocked_by_hdr"], false,
            "{off}"
        );
        assert!(
            off["subtitles"]
                .as_array()
                .is_some_and(|tracks| tracks.iter().all(|track| track["default"] == false)),
            "Off must clear the effective subtitle default: {off}"
        );

        let (status, invalid) = call(
            &app,
            get(
                &format!(
                    "/api/v1/files/{file}/decision?vcodec=h264&acodec=aac&container=mp4&hdr=0&subtitle=9"
                ),
                Some(&admin),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
        assert_eq!(invalid["error"], "unknown subtitle track", "{invalid}");

        // The promise `native: false` makes, kept: asking for one of these by
        // index is refused before a session is ever spawned.
        for index in [1, 2, 3] {
            let (status, why) = call(
                &app,
                post(
                    &format!("/api/v1/files/{file}/hls/sessions"),
                    Some(&admin),
                    json!({
                        "playback_id": format!("non-native-{index}"),
                        "request_id": format!("non-native-attempt-{index}"),
                        "native_subtitles": true,
                        "subtitle": index
                    }),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "index {index}: {why}");
        }

        // And the promise `text: true` makes: the sidecar route accepts
        // `mov_text` and ASS (it only turns bitmap tracks away), so these
        // tracks are still watchable — that is the path a client uses for
        // them. The placeholder MP4 has no real streams, so ffmpeg fails and
        // the answer is a 500; a bitmap track never gets that far.
        for (index, expected) in [
            (1, StatusCode::INTERNAL_SERVER_ERROR),
            (2, StatusCode::INTERNAL_SERVER_ERROR),
            (3, StatusCode::BAD_REQUEST),
        ] {
            assert_eq!(
                status_of(
                    &app,
                    get_q(&format!(
                        "/api/v1/files/{file}/subs/{index}.vtt?token={admin}"
                    )),
                )
                .await,
                expected,
                "sidecar for stream {index}"
            );
        }
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

    /// Seed one movie with two versions: the 2160p Dolby Vision remux and the
    /// 720p copy. Returns (library id, item id).
    async fn seed_two_version_movie(state: &AppState) -> (i64, i64) {
        use plurx_core::domain::{
            AudioStream, ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult,
        };
        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Movies".into(),
                kind: LibraryKind::Movies,
                paths: vec![std::path::PathBuf::from("/media/movies")],
                anime: false,
            })
            .await
            .expect("lib");
        let item = state
            .store
            .insert_item(&NewItem {
                library_id: lib.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Blade Runner 2049".into(),
                year: Some(2017),
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        state
            .store
            .upsert_file(
                item,
                "/media/movies/Blade Runner 2049 (2017) 2160p.mkv",
                69_000_000_000,
                1,
                &ProbeResult {
                    duration_ms: Some(9_780_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(3840),
                    height: Some(2160),
                    bit_depth: Some(10),
                    hdr: Some("dolby_vision".into()),
                    hdr_format: Some("Dolby Vision · Profile 7 (HDR10-compatible)".into()),
                    bitrate: Some(56_000_000),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "truehd".into(),
                        channels: Some(8),
                        language: Some("eng".into()),
                        title: None,
                        default: true,
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("file a");
        state
            .store
            .upsert_file(
                item,
                "/media/movies/Blade Runner 2049 (2017) 720p.mp4",
                6_800_000_000,
                1,
                &ProbeResult {
                    duration_ms: Some(9_780_000),
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1280),
                    height: Some(720),
                    bitrate: Some(5_000_000),
                    audio_streams: vec![AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        channels: Some(2),
                        language: Some("eng".into()),
                        title: None,
                        default: true,
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("file b");
        (lib.id, item)
    }

    async fn raw_body(app: &Router, uri: &str, token: &str) -> String {
        let resp = app
            .clone()
            .oneshot(get(uri, Some(token)))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    /// The pre-S1 library-list body, captured from the build before the
    /// `media` block existed and frozen here as a literal. Only the two epoch
    /// stamps are substituted, and they come from the store rather than from
    /// the response, so nothing in this golden is derived from the code it
    /// judges. Every byte of shape — key order, which optional fields appear,
    /// the absence of `media` — is pre-S1's.
    const PRE_S1_LIST_BODY: &str = concat!(
        r#"{"items":[{"id":1,"library_id":1,"kind":"movie","parent_id":null,"#,
        r#""title":"Blade Runner 2049","year":2017,"overview":null,"#,
        r#""season_number":null,"episode_number":null,"air_date":null,"#,
        r#""runtime_ms":null,"added_at":{added},"updated_at":{updated},"#,
        // S3 landed `genres` on ItemDto after this golden was captured. It is
        // additive on its own terms (always present, empty rather than absent,
        // and covered by S3's own tests), so the baseline this test defends
        // moved by exactly that one field. Updating the golden is the correct
        // resolution; deleting the test would not be. Anything else appearing
        // here is what it is still watching for.
        r#""recorded_at":null,"tags":[],"genres":[],"tmdb_id":null,"imdb_id":null,"#,
        r#""poster":null,"backdrop":null,"resolution":2160}],"#,
        r#""total":1,"offset":0,"limit":60}"#,
    );

    /// Additive means additive: a client that does not ask for facts gets the
    /// bytes it got before S1 shipped, not "the same JSON modulo a null". A
    /// new key — even `"media":null` — is a parse error in the strict decoders
    /// on the tolerant-Android side of the fleet, so this is checked as bytes,
    /// not as a `serde_json::Value` comparison that would forgive it.
    #[tokio::test]
    async fn library_list_without_facts_is_byte_identical_to_pre_s1() {
        let (app, state) = test_app_with_state();
        let token = setup_admin(&app).await;
        let (lib, item) = seed_two_version_movie(&state).await;
        let stamped = state
            .store
            .get_item(item)
            .await
            .expect("get")
            .expect("item");
        let expected = PRE_S1_LIST_BODY
            .replace("{added}", &stamped.added_at.to_string())
            .replace("{updated}", &stamped.updated_at.to_string());

        let plain = raw_body(&app, &format!("/api/v1/libraries/{lib}/items"), &token).await;
        assert_eq!(plain, expected, "the default list body moved");
        // Explicitly off, and a value that is not the opt-in: same bytes. The
        // param is a switch, not a hint.
        for uri in [
            format!("/api/v1/libraries/{lib}/items?facts=0"),
            format!("/api/v1/libraries/{lib}/items?sort=title&facts=0"),
        ] {
            assert_eq!(raw_body(&app, &uri, &token).await, expected, "{uri}");
        }
    }

    /// With the param: one block per playable item, aggregated across every
    /// version — and removing that block from the bytes gives back the pre-S1
    /// response exactly, which is what "additive" has to mean.
    #[tokio::test]
    async fn facts_block_aggregates_every_version_of_an_item() {
        let (app, state) = test_app_with_state();
        let token = setup_admin(&app).await;
        let (lib, item) = seed_two_version_movie(&state).await;
        let stamped = state
            .store
            .get_item(item)
            .await
            .expect("get")
            .expect("item");
        let expected = PRE_S1_LIST_BODY
            .replace("{added}", &stamped.added_at.to_string())
            .replace("{updated}", &stamped.updated_at.to_string());

        let body = raw_body(
            &app,
            &format!("/api/v1/libraries/{lib}/items?facts=1"),
            &token,
        )
        .await;
        // The 4K DV remux is the item's face; the 720p copy still counts
        // toward files and bytes. A 720p answer here would be the library
        // reading as worse than it is.
        let block = concat!(
            r#""media":{"files":2,"bytes":75800000000,"video":"HEVC","height":2160,"#,
            r#""hdr":"dolby_vision","hdr_format":"Dolby Vision · Profile 7 (HDR10-compatible)","audio":"TrueHD 7.1","container":"MKV"}"#,
        );
        assert!(body.contains(block), "no aggregate block in {body}");
        assert_eq!(
            body.replace(&format!(",{block}"), ""),
            expected,
            "the facts response is more than pre-S1 plus the block"
        );
    }

    /// Photos and folders have nothing to badge — a photo has no codec or
    /// dynamic range, and a folder's bytes are its children's, not its own.
    /// They must come back bare even with the param on, so a client can use
    /// "has a media block" as "this is playable".
    #[tokio::test]
    async fn facts_skip_photos_and_folders() {
        use plurx_core::domain::{ItemKind, LibraryKind, NewItem, NewLibrary, ProbeResult};
        let (app, state) = test_app_with_state();
        let token = setup_admin(&app).await;
        let lib = state
            .store
            .create_library(&NewLibrary {
                name: "Home".into(),
                kind: LibraryKind::Home,
                paths: vec![std::path::PathBuf::from("/media/home")],
                anime: false,
            })
            .await
            .expect("lib");
        let new = |kind, title: &str| NewItem {
            library_id: lib.id,
            kind,
            parent_id: None,
            title: title.to_owned(),
            year: None,
            season_number: None,
            episode_number: None,
        };
        let folder = state
            .store
            .insert_item(&new(ItemKind::Folder, "2019"))
            .await
            .expect("folder");
        let photo = state
            .store
            .insert_item(&new(ItemKind::Photo, "IMG_4021"))
            .await
            .expect("photo");
        let video = state
            .store
            .insert_item(&new(ItemKind::Video, "Beach day"))
            .await
            .expect("video");
        // The photo has a file, so "no block" cannot be an accident of having
        // nothing to aggregate — it is the kind filter doing its job.
        state
            .store
            .upsert_file(
                photo,
                "/media/home/IMG_4021.jpg",
                4_200_000,
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
            .upsert_file(
                video,
                "/media/home/Beach day.mp4",
                120_000_000,
                1,
                &ProbeResult {
                    container: Some("mp4".into()),
                    video_codec: Some("h264".into()),
                    width: Some(1920),
                    height: Some(1080),
                    bitrate: Some(8_000_000),
                    audio_streams: vec![plurx_core::domain::AudioStream {
                        index: 0,
                        codec: "aac".into(),
                        channels: Some(2),
                        language: None,
                        title: None,
                        default: true,
                    }],
                    ..Default::default()
                },
            )
            .await
            .expect("video file");

        let (status, body) = call(
            &app,
            get(
                &format!("/api/v1/libraries/{}/items?facts=1", lib.id),
                Some(&token),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let by_id = |id: i64| -> Value {
            body["items"]
                .as_array()
                .expect("items")
                .iter()
                .find(|i| i["id"] == json!(id))
                .cloned()
                .expect("item on the page")
        };
        assert_eq!(by_id(folder)["media"], Value::Null, "folder got a block");
        assert_eq!(by_id(photo)["media"], Value::Null, "photo got a block");
        assert_eq!(
            by_id(video)["media"],
            json!({
                "files": 1, "bytes": 120_000_000, "video": "H.264",
                "height": 1080, "audio": "AAC 2.0", "container": "MP4"
            }),
            "an SDR home video should carry no hdr key at all"
        );
    }
}
