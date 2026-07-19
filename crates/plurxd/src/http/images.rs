//! Serve cached artwork. Auth via `?token=` since `<img>` tags can't set
//! headers (see `extract`). Filenames are validated to prevent traversal.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

/// GET /api/v1/images/:filename
pub async fn serve(
    _user: AuthUser,
    State(state): State<AppState>,
    Path(filename): Path<String>,
) -> Result<Response, ApiError> {
    // Only a bare filename is allowed — no directories, no traversal.
    let candidate = std::path::Path::new(&filename);
    let safe_name = candidate
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| *n == filename && !n.is_empty())
        .ok_or_else(|| ApiError::BadRequest("invalid image name".into()))?;

    let path = state.artwork_dir.join(safe_name);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::NotFound("image"))?;

    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, mime),
            // Artwork is content-stable per item; cache aggressively.
            (header::CACHE_CONTROL, "public, max-age=604800".to_owned()),
        ],
        bytes,
    )
        .into_response())
}
