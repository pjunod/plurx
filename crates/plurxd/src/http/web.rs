//! The embedded single-page web app.
//!
//! Bundled into the binary (single-binary promise, works offline). It's a
//! self-contained HTML/CSS/JS file using hash routing, so the server only
//! needs to serve it at `/` and as a fallback for any non-API GET path.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use qrcode::{render::svg, QrCode};
use serde::Deserialize;

use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../web/index.html");
/// Pure playback-routing policy, separated from the player adapter so the
/// decisions that change bytes or transport can run under Node unit tests.
const PLAYBACK_POLICY_JS: &str = include_str!("../web/playback-policy.js");
/// EPUB pagination, locator, and sandbox-frame policy. Kept out of the app
/// shell so native WebViews can reuse the same navigator in M3.
const READER_JS: &str = include_str!("../web/reader.js");
const READER_CSS: &str = include_str!("../web/reader.css");
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

/// Serve the web player's unit-tested routing policy.
pub async fn playback_policy_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        PLAYBACK_POLICY_JS,
    )
        .into_response()
}

/// Serve the unit-tested EPUB navigator shared by the browser reader.
pub async fn reader_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        READER_JS,
    )
        .into_response()
}

/// Serve the trusted reader chrome; publication styles stay inside the frame.
pub async fn reader_css() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        READER_CSS,
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct ConnectQrQuery {
    origin: String,
}

/// Render the current browser origin as a QR code native clients can scan.
/// It deliberately contains no credential: scanning chooses a server, then
/// the app signs in normally so passwords and tokens never cross the code.
pub async fn connect_qr(Query(query): Query<ConnectQrQuery>) -> Response {
    let Ok(svg) = connection_qr_svg(&query.origin) else {
        return (StatusCode::BAD_REQUEST, "invalid server origin").into_response();
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/svg+xml; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
        ],
        svg,
    )
        .into_response()
}

fn connection_qr_svg(origin: &str) -> Result<String, ()> {
    let origin = origin.trim().trim_end_matches('/');
    let uri: Uri = origin.parse().map_err(|_| ())?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
    {
        return Err(());
    }
    let code = QrCode::new(origin.as_bytes()).map_err(|_| ())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#111217"))
        .light_color(svg::Color("#ffffff"))
        .build())
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

#[cfg(test)]
mod tests {
    use super::{connection_qr_svg, INDEX_HTML, PLAYBACK_POLICY_JS, READER_JS};

    #[test]
    fn app_shell_shows_the_running_build_to_signed_in_and_signed_out_users() {
        assert_eq!(
            INDEX_HTML.matches("Version ${esc(buildLabel())}").count(),
            2
        );
    }

    #[test]
    fn app_shell_loads_the_tested_playback_policy_before_the_player() {
        let policy = INDEX_HTML
            .find("/assets/playback-policy.js")
            .expect("policy script");
        let player = INDEX_HTML
            .find("const PlaybackPolicy")
            .expect("player script");
        assert!(policy < player);
        assert!(PLAYBACK_POLICY_JS.contains("function initialRoute"));
    }

    #[test]
    fn app_shell_loads_the_reader_boundary_before_its_route() {
        let asset = INDEX_HTML.find("/assets/reader.js").expect("reader asset");
        let route = INDEX_HTML
            .find("async function viewReader")
            .expect("reader route");
        assert!(asset < route);
        assert!(READER_JS.contains("class FrameNavigator"));
        assert!(READER_JS.contains("stripExecutableMarkup"));
    }

    #[test]
    fn connection_qr_accepts_only_an_http_server_origin() {
        let svg = connection_qr_svg("http://192.168.4.14:32400/").expect("valid origin");
        assert!(svg.starts_with("<?xml"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("#111217"));

        assert!(connection_qr_svg("javascript:alert(1)").is_err());
        assert!(connection_qr_svg("http://server.test/path").is_err());
        assert!(connection_qr_svg("").is_err());
    }

    #[test]
    fn signed_in_account_menu_can_show_the_server_qr_without_signing_out() {
        let menu = INDEX_HTML
            .find("function profileMenuHtml()")
            .expect("account menu");
        let qr_action = INDEX_HTML
            .find("Show server QR code")
            .expect("signed-in QR action");
        let sign_out = INDEX_HTML[menu..]
            .find("Sign out")
            .map(|offset| menu + offset)
            .expect("sign-out action");
        assert!(menu < qr_action && qr_action < sign_out);
        assert!(INDEX_HTML.contains("function showConnectQr()"));
        assert!(INDEX_HTML.contains("/connect.svg?origin=${encodeURIComponent(location.origin)}"));
    }
}
