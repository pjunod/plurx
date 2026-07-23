//! The embedded single-page web app.
//!
//! Bundled into the binary (single-binary promise, works offline). It's a
//! self-contained HTML/CSS/JS file using hash routing, so the server only
//! needs to serve it at `/` and as a fallback for any non-API GET path.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxPath, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../web/index.html");
/// hls.js (bundled for the transcode playback path; keeps the single-binary,
/// works-offline promise instead of a CDN dependency).
const HLS_JS: &str = include_str!("../web/hls.min.js");
/// PWA manifest + icons — bundled so "Add to Home Screen" (iOS) and installable
/// PWA (Android/desktop) work with no external assets.
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const ICON_192: &[u8] = include_bytes!("../web/icons/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../web/icons/icon-512.png");
const ICON_MASKABLE: &[u8] = include_bytes!("../web/icons/maskable-512.png");
const APPLE_TOUCH: &[u8] = include_bytes!("../web/icons/apple-touch-icon.png");

/// Serve the web app shell.
pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Serve the bundled hls.js.
pub async fn hls_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        HLS_JS,
    )
        .into_response()
}

/// Serve the PWA manifest (enables install / Add-to-Home-Screen).
pub async fn manifest() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        MANIFEST,
    )
        .into_response()
}

/// Serve one of the embedded PWA / apple-touch icons by name.
pub async fn icon(AxPath(name): AxPath<String>) -> Response {
    let bytes: &'static [u8] = match name.as_str() {
        "icon-192.png" => ICON_192,
        "icon-512.png" => ICON_512,
        "maskable-512.png" => ICON_MASKABLE,
        "apple-touch-icon.png" => APPLE_TOUCH,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        bytes,
    )
        .into_response()
}

/// Resolve the Android APK to serve, if one is published: `PLURX_ANDROID_APK`
/// (an explicit path) wins, else `<data_dir>/plurx-android.apk`. `None` when no
/// file is present, so the web UI's download link stays hidden.
pub fn android_apk_path(data_dir: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PLURX_ANDROID_APK") {
        if !p.is_empty() {
            let pb = PathBuf::from(p);
            return pb.is_file().then_some(pb);
        }
    }
    let pb = Path::new(data_dir).join("plurx-android.apk");
    pb.is_file().then_some(pb)
}

/// Serve the Android APK for sideloading. Unauthenticated on purpose: it's the
/// client app binary, not user data, and a TV's Downloader/browser can't attach
/// a bearer token anyway.
pub async fn download_android(State(state): State<AppState>) -> Response {
    let Some(path) = android_apk_path(&state.system.data_dir) else {
        return (StatusCode::NOT_FOUND, "no Android app published").into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (
                    header::CONTENT_TYPE,
                    "application/vnd.android.package-archive",
                ),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"plurx.apk\"",
                ),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no Android app published").into_response(),
    }
}

/// Fallback for unmatched routes: serve the app for browser navigations,
/// but return a JSON 404 for anything under `/api` so API clients get a clean
/// error instead of a page of HTML.
pub async fn fallback(uri: axum::http::Uri) -> Response {
    if uri.path().starts_with("/api") {
        (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":"not found"}"#,
        )
            .into_response()
    } else {
        Html(INDEX_HTML).into_response()
    }
}
