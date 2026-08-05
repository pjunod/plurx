//! Authenticated `pgs-v1` manifest and immutable object routes.

use std::path::Path;

use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plurx_core::domain::MediaFile;
use plurx_core::tracks::is_pgs_subtitle;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::pgs_overlay::{self, OverlayError, PrepareState};
use crate::state::AppState;

async fn file_and_track(state: &AppState, id: i64, index: i64) -> Result<MediaFile, ApiError> {
    if !state.pgs_overlay_enabled {
        return Err(ApiError::NotFound("PGS overlay"));
    }
    if index < 0 {
        return Err(ApiError::NotFound("subtitle track"));
    }
    let file = state
        .store
        .get_file(id)
        .await?
        .ok_or(ApiError::NotFound("file"))?;
    let stream = file
        .subtitle_streams
        .get(index as usize)
        .ok_or(ApiError::NotFound("subtitle track"))?;
    if !is_pgs_subtitle(&stream.codec) {
        return Err(ApiError::UnsupportedMedia(
            "subtitle track is not supported by pgs-v1".into(),
        ));
    }
    Ok(file)
}

pub async fn manifest(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(i64, i64)>,
) -> Result<Response, ApiError> {
    let file = file_and_track(&state, id, index).await?;
    match pgs_overlay::prepare(&state.subs_dir, &file, index)
        .await
        .map_err(map_overlay_error)?
    {
        PrepareState::Preparing => {
            let mut response = (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "state": "preparing",
                    "retry_after_ms": 1000
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            );
            Ok(response)
        }
        PrepareState::Ready(path) => {
            if let Some(generation_dir) = path.parent() {
                pgs_overlay::record_access(generation_dir).await;
            }
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|error| ApiError::Internal(format!("reading PGS manifest: {error}")))?;
            let etag = format!("\"{}\"", pgs_overlay::generation(&file, index));
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-cache"),
            );
            response.headers_mut().insert(
                header::ETAG,
                HeaderValue::from_str(&etag)
                    .map_err(|error| ApiError::Internal(format!("PGS ETag: {error}")))?,
            );
            Ok(response)
        }
    }
}

pub async fn object(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath((id, index, generation, object)): AxPath<(i64, i64, String, String)>,
) -> Result<Response, ApiError> {
    let file = file_and_track(&state, id, index).await?;
    if generation != pgs_overlay::generation(&file, index) {
        return Err(ApiError::NotFound("PGS overlay generation"));
    }
    let hash = object
        .strip_suffix(".png")
        .ok_or(ApiError::NotFound("PGS overlay object"))?;
    let path = pgs_overlay::object_path(&state.subs_dir, &file, index, hash)
        .ok_or(ApiError::NotFound("PGS overlay object"))?;
    if tokio::fs::metadata(pgs_overlay::manifest_path(&state.subs_dir, &file, index))
        .await
        .is_err()
    {
        return Err(ApiError::NotFound("PGS overlay generation"));
    }
    if let Some(generation_dir) = path.parent().and_then(Path::parent) {
        pgs_overlay::record_access(generation_dir).await;
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ApiError::NotFound("PGS overlay object"),
            _ => ApiError::Internal(format!("reading PGS overlay object: {error}")),
        })?;
    let etag = format!("\"{hash}\"");
    let mut response =
        (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], bytes).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag)
            .map_err(|error| ApiError::Internal(format!("PGS object ETag: {error}")))?,
    );
    Ok(response)
}

fn map_overlay_error(error: OverlayError) -> ApiError {
    match error {
        OverlayError::Malformed(why) => ApiError::Unprocessable(serde_json::json!({
            "error": "malformed PGS stream",
            "detail": why
        })),
        OverlayError::Limit(why) => ApiError::Unprocessable(serde_json::json!({
            "error": "PGS safety limit exceeded",
            "detail": why
        })),
        OverlayError::SourceChanged => {
            ApiError::Conflict("media source changed while its PGS overlay was preparing".into())
        }
        OverlayError::Unavailable(why) => {
            tracing::warn!(error = %why, "PGS overlay preparation is unavailable");
            ApiError::ServiceUnavailable(
                "PGS overlay preparation is temporarily unavailable".into(),
            )
        }
        OverlayError::Internal(why) => ApiError::Internal(why),
    }
}
