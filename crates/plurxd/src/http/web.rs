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
/// The connectivity taxonomy: classifies a failed request and hands back the
/// user copy for it, so no surface renders the browser's `TypeError` text.
/// Separated for the same reason as the playback policy — it is pure, and it
/// is tested under Node against `tests/contracts/connectivity-copy.json`.
const CONNECTIVITY_JS: &str = include_str!("../web/connectivity.js");
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

/// Serve the web app's unit-tested connectivity classifier.
pub async fn connectivity_js() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        CONNECTIVITY_JS,
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
    use super::{connection_qr_svg, CONNECTIVITY_JS, INDEX_HTML, PLAYBACK_POLICY_JS};

    /// The copy contract the web, Android and Apple clients all read. Pulled in
    /// here so the *shipped* classifier — the bytes the browser receives — is
    /// checked against it, not just the copy under Node.
    const CONNECTIVITY_CONTRACT: &str =
        include_str!("../../../../tests/contracts/connectivity-copy.json");

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
    fn shipped_connectivity_classifier_carries_every_class_in_the_contract() {
        let contract: serde_json::Value =
            serde_json::from_str(CONNECTIVITY_CONTRACT).expect("connectivity contract parses");
        let classes = contract["classes"]
            .as_object()
            .expect("contract lists classes");
        assert!(!classes.is_empty());
        // A class the taxonomy gains and this client silently lacks is the
        // exact failure this test exists to catch: the JS must name it, and
        // must carry its copy, or the shipped app has a hole with no string.
        for (id, class) in classes {
            assert!(
                CONNECTIVITY_JS.contains(&format!("{id}: Object.freeze({{")),
                "connectivity.js is missing the {id} class"
            );
            for field in ["title", "detail", "short"] {
                let copy = class[field].as_str().expect("contract copy is a string");
                assert!(
                    CONNECTIVITY_JS.contains(copy),
                    "connectivity.js is missing the {id} {field} copy"
                );
            }
        }
        let fallback = contract["server_fallback"].as_str().expect("fallback");
        assert!(CONNECTIVITY_JS.contains(&format!("\"{fallback}\"")));
        // The sentence a client says *instead of* a class. It is in the
        // contract so it cannot be reworded per platform (docs §4).
        let credentials = contract["credentials_message"]
            .as_str()
            .expect("contract carries credentials_message");
        assert!(
            CONNECTIVITY_JS.contains(&format!("\"{credentials}\"")),
            "connectivity.js is missing credentials_message"
        );
        assert!(CONNECTIVITY_JS.contains("function classify"));
        assert!(CONNECTIVITY_JS.contains("function describe"));
        assert!(CONNECTIVITY_JS.contains("function renderableActions"));
        assert!(CONNECTIVITY_JS.contains("function deadlineFor"));
        // The browser's own words are diagnostics, never user copy.
        assert!(!CONNECTIVITY_JS.contains("Failed to fetch"));
    }

    #[test]
    fn app_shell_loads_the_tested_connectivity_classifier_before_the_app() {
        let asset = INDEX_HTML
            .find("/assets/connectivity.js")
            .expect("connectivity script");
        let binding = INDEX_HTML
            .find("const Connectivity")
            .expect("app script binding");
        assert!(asset < binding);
        assert!(INDEX_HTML.contains("window.PlurxConnectivity"));
        assert!(INDEX_HTML.contains("function renderConnectionError"));
        // A missing asset fails at load with a sentence, rather than running on
        // and dying with a TypeError inside an unrelated failure later. A shim
        // would be worse: PlaybackPolicy has eleven members and all are called.
        assert!(INDEX_HTML.contains("if(!PlaybackPolicy || !Connectivity){"));
        assert!(!INDEX_HTML.contains("window.PlurxPlaybackPolicy || {"));
    }

    /// The web app's JavaScript is never executed by any suite — it is
    /// `include_str!`-embedded, and `scripts/js-check` only parses it. So the
    /// decisions live in `connectivity.js`, where Node drives them, and this
    /// test pins the *wiring*: that the shell actually calls each one, and that
    /// the handful of one-line guards a refactor can silently drop are present.
    /// Each assertion below names the deletion it is here to fail on.
    #[test]
    fn app_shell_wires_every_connectivity_decision_it_is_supposed_to_use() {
        // Deleting `signal:` from the fetch options, or the deadline policy
        // call, silently restores "Loading… forever" against a blackholing host.
        assert!(INDEX_HTML.contains("Connectivity.deadlineFor(path)"));
        assert!(INDEX_HTML.contains("signal:deadline.signal"));
        assert!(INDEX_HTML.contains("deadline.release()"));
        // Deleting the action loop leaves a full-surface error with no way out,
        // which is `every_error_offers_retry` broken in the one place it shows.
        assert!(INDEX_HTML.contains("Connectivity.renderableActions(d.id,{canChangeServer:false})"));
        assert!(INDEX_HTML.contains("acts.appendChild(b)"));
        // Sign-in must not take the session-ending path, and must say the
        // contract's sentence rather than the server's raw 4xx body (docs §4).
        assert!(INDEX_HTML.contains("Connectivity.CREDENTIALS_MESSAGE"));
        assert!(INDEX_HTML.contains(
            "if(signin && (res.status===401 || res.status===403)) throw credentialsError();"
        ));
        assert!(INDEX_HTML.contains("api(\"/auth/login\",{method:\"POST\",signin:true,"));
        // Three surfaces render the full state: boot, the activity view, and
        // the router. Dropping any one of them puts a raw message back.
        assert_eq!(INDEX_HTML.matches("renderConnectionError(").count(), 4);
        assert!(INDEX_HTML
            .contains("renderConnectionError(m||document.getElementById(\"app\"), e, render)"));
        assert!(INDEX_HTML.contains("renderConnectionError(m, e, retryActivity)"));
        // A stale request may not paint over the view that replaced it, and may
        // not stop that view's timer. Restoring the unconditional setPageTimer,
        // or dropping either generation guard, is what this catches.
        assert_eq!(
            INDEX_HTML
                .matches(
                    "if(viewIsCurrent(gen) && !ACT_ERROR) setPageTimer(renderActivityBody, 3000);"
                )
                .count(),
            2
        );
        // Twice, not once: the activity body has to re-check after its await on
        // BOTH paths. A failure that paints late is the loud bug; a success
        // that paints late quietly replaces the page the viewer asked for.
        assert_eq!(
            INDEX_HTML
                .matches("if(!viewIsCurrent(gen) || location.hash!==\"#/activity\") return;")
                .count(),
            2
        );
        assert!(INDEX_HTML.contains("if(e.message!==\"unauthorized\" && viewIsCurrent(gen)){"));
        assert!(INDEX_HTML.contains("function viewIsCurrent(gen)"));
        // The lightbox reuses one <img>, so its failure state has to be cleared
        // or one dead photo mislabels every photo after it.
        assert!(INDEX_HTML.contains("imgRestore(img); img.src=photoUrl(p.id);"));
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
