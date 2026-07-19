//! The embedded single-page web app.
//!
//! Bundled into the binary (single-binary promise, works offline). It's a
//! self-contained HTML/CSS/JS file using hash routing, so the server only
//! needs to serve it at `/` and as a fallback for any non-API GET path.

use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};

const INDEX_HTML: &str = include_str!("../web/index.html");
/// hls.js (bundled for the transcode playback path; keeps the single-binary,
/// works-offline promise instead of a CDN dependency).
const HLS_JS: &str = include_str!("../web/hls.min.js");

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
