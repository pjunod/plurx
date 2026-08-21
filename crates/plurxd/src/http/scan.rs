//! `POST /api/v1/scan` — "scan exactly this path", for other applications.
//!
//! The fast path in the monarr pipeline: when monarr finishes importing into
//! a library folder it says so, instead of plurx discovering it on the next
//! scheduled sweep. Key-scoped, never user-token: see
//! [`crate::http::extract::ScopedKey`] for why the wall runs both ways.

use std::path::PathBuf;

use axum::extract::{Path as UrlPath, State};
use axum::http::request::Parts;
use axum::Json;
use plurx_core::domain::{scopes, Library};
use plurx_core::scan::TargetError;
use serde::{Deserialize, Serialize};

use super::error::ApiError;
use super::extract::ScopedKey;
use crate::state::{AppState, IdHints, ScanRequest};

#[derive(Debug, Deserialize, Default)]
pub struct Ids {
    #[serde(default)]
    pub tmdb: Option<i64>,
    #[serde(default)]
    pub imdb: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ScanRequestBody {
    /// Absolute path — a directory or a single file.
    pub path: String,
    #[serde(default)]
    pub ids: Option<Ids>,
    /// `movie` | `episode` | `season` | `book`. Advisory: the library's own kind
    /// decides how a file is parsed, so this is only used to pick which
    /// item an id applies to. `book` carries no provider id; it names the
    /// Curator import honestly while the Books library derives file identity.
    #[serde(default)]
    pub hint: Option<String>,
    /// For episodes, the id of the SHOW — an episode's own tmdb id is not
    /// what identifies the series it belongs to.
    #[serde(default)]
    pub series: Option<Ids>,
    /// Echoed back and logged, so one grep reconstructs a transfer across
    /// every application it passed through.
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Serialize)]
pub struct ScanAccepted {
    pub status: &'static str,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
}

/// POST /api/v1/scan (scope `scan:trigger`)
pub async fn scan(
    State(state): State<AppState>,
    parts: Parts,
    Json(body): Json<ScanRequestBody>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), ApiError> {
    let mut parts = parts;
    let _key = ScopedKey::require(&mut parts, &state, scopes::SCAN_TRIGGER).await?;
    // Counted here, before the path is resolved: a request rejected for a
    // path-mapping mistake still proves the other application reached plurx
    // with a working key, and that is the first thing anyone debugging this
    // needs to rule in or out.
    state.jobs.metrics().count_notification();

    let path = PathBuf::from(&body.path);
    if !path.is_absolute() {
        return Err(ApiError::BadRequest(format!(
            "`{}` is not an absolute path — plurx and the caller may not share a \
             working directory, so relative paths cannot be resolved",
            body.path
        )));
    }

    // plurx resolves the library, so the caller never needs to know plurx's
    // library ids — one less thing to keep in sync between two applications.
    let libraries = state.store.list_libraries().await?;
    let Some(library) = library_for_path(&libraries, &path) else {
        // Self-explaining on purpose: a path-mapping mistake between two
        // containers is the likeliest cause by a wide margin, and naming the
        // roots turns a support conversation into a glance.
        return Err(ApiError::Unprocessable(serde_json::json!({
            "error": "path is not under any library root",
            "path": body.path,
            "roots": libraries
                .iter()
                .flat_map(|l| l.paths.iter().map(|p| p.display().to_string()))
                .collect::<Vec<_>>(),
        })));
    };

    let request_id = format!("sr-{}", uuid::Uuid::new_v4().simple());
    // The ids ride ON the request so the job applies them, whether it runs
    // now or is drained from the pending queue later. An endpoint that
    // applied them itself would drop them for every request that arrived
    // while a scan was running — which is most of them, since creating or
    // scanning a library makes it busy.
    let episodeish = matches!(body.hint.as_deref(), Some("episode") | Some("season"))
        || body.series.as_ref().is_some_and(|s| s.tmdb.is_some());
    // A series import deliberately carries the SHOW id in `series`, with no
    // item-level `ids` object. Keying this Option off `body.ids` alone drops
    // exactly the hint TV imports send, leaving the targeted scan to guess
    // from the folder name instead of using the manager's authoritative id.
    let ids = (body.ids.is_some() || body.series.is_some()).then(|| IdHints {
        tmdb: body.ids.as_ref().and_then(|i| i.tmdb),
        imdb: body.ids.as_ref().and_then(|i| i.imdb.clone()),
        series_tmdb: body.series.as_ref().and_then(|s| s.tmdb),
        episodeish,
    });
    let req = ScanRequest {
        id: request_id.clone(),
        library_id: library.id,
        path: path.clone(),
        ids,
        correlation_id: body.correlation_id.clone(),
        source: body.source.clone(),
    };

    match state.jobs.request_scan(req).await {
        Ok(Some(scan)) => Ok((
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "status": "scanned",
                "library_id": library.id,
                "request_id": request_id,
                "report": scan.report,
                "items": scan.items,
                "correlation_id": body.correlation_id,
            })),
        )),
        // Busy: queued rather than dropped. 202 with an id to poll, because
        // silently losing the request would leave a season half-indexed.
        Ok(None) => Ok((
            axum::http::StatusCode::ACCEPTED,
            Json(serde_json::to_value(ScanAccepted {
                status: "queued",
                request_id,
                correlation_id: body.correlation_id,
            })?),
        )),
        Err(TargetError::OutsideRoots { path, roots }) => {
            Err(ApiError::Unprocessable(serde_json::json!({
                "error": "path is not under any library root",
                "path": path,
                "roots": roots,
            })))
        }
        Err(TargetError::Store(e)) => Err(ApiError::from(e)),
    }
}

/// GET /api/v1/scan/requests/{id} (scope `status:read`)
pub async fn request_status(
    State(state): State<AppState>,
    parts: Parts,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut parts = parts;
    let _key = ScopedKey::require(&mut parts, &state, scopes::STATUS_READ).await?;
    match state.jobs.scan_request(&id).await {
        Some(rec) => Ok(Json(serde_json::to_value(rec)?)),
        None => Err(ApiError::NotFound("scan request")),
    }
}

/// The library whose roots contain `path`.
///
/// Longest matching root wins, so a library nested inside another (`/media`
/// and `/media/kids`) resolves to the more specific one rather than to
/// whichever happened to be created first.
fn library_for_path<'a>(libraries: &'a [Library], path: &std::path::Path) -> Option<&'a Library> {
    let canonical = path.canonicalize().ok()?;
    libraries
        .iter()
        .filter_map(|lib| {
            lib.paths
                .iter()
                .filter_map(|root| {
                    let root = root.canonicalize().unwrap_or_else(|_| root.clone());
                    // Component-wise: `/data` must not match `/database`.
                    (canonical == root || canonical.starts_with(&root))
                        .then(|| root.components().count())
                })
                .max()
                .map(|depth| (depth, lib))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, lib)| lib)
}
