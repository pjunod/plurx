//! Plex Media Server API compatibility façade.
//!
//! Tier 1 target (docs/CLIENTS.md §3): clients that connect directly to a
//! server — Composite for Kodi, PlexKodiConnect, python-plexapi tooling,
//! Home Assistant. The façade translates the PMS HTTP surface (XML
//! `MediaContainer` by default, JSON via `Accept`) onto plurx-core services,
//! plus a GDM discovery responder. It never contacts plex.tv (REQ-PLEX-3).
//!
//! Phase 0 reserves the crate boundary; Phase 2 implements the endpoints.
//! plurxd does not mount this router until then.

use axum::http::StatusCode;
use axum::Router;

/// Placeholder router: answers every path with 501 so accidental early
/// mounting is loud, not silent.
pub fn router() -> Router {
    Router::new().fallback(not_implemented)
}

async fn not_implemented() -> (StatusCode, &'static str) {
    (
        StatusCode::NOT_IMPLEMENTED,
        "plex-compat: not implemented until Phase 2 (docs/ROADMAP.md)",
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn every_route_is_501_for_now() {
        for path in ["/identity", "/library/sections", "/anything/else"] {
            let response = super::router()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::NOT_IMPLEMENTED,
                "unexpected status for {path}"
            );
        }
    }
}
