//! Serving stills from `home` libraries.
//!
//! Photos are static bytes: no decision engine, no transcode, no session.
//! They must never touch the playback pipeline (docs/HOMEVIDEO-PLAN.md §8.3).
//!
//! Browsers honor EXIF orientation on `<img>` natively
//! (`image-orientation: from-image` is the default), so the original is served
//! untouched — there is no rotation pipeline here and shouldn't be one.

use axum::extract::{Path, Query, State};
use axum::response::Response;
use plurx_core::domain::ItemKind;
use serde::Deserialize;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct PhotoQuery {
    /// `thumb` serves the cached grid thumbnail; anything else (including
    /// absent) serves the original file.
    pub size: Option<String>,
}

/// GET /api/v1/items/:id/photo — original bytes, with range support.
/// GET /api/v1/items/:id/photo?size=thumb — the artwork-cache thumbnail.
pub async fn serve(
    _user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<PhotoQuery>,
    headers: axum::http::HeaderMap,
) -> Result<Response, ApiError> {
    let item = state
        .store
        .get_item(id)
        .await?
        .ok_or(ApiError::NotFound("photo"))?;
    if item.kind != ItemKind::Photo {
        return Err(ApiError::NotFound("photo"));
    }

    if q.size.as_deref() == Some("thumb") {
        // Before local enrichment has run there is no thumbnail yet. Serving
        // the original beats a broken image in the grid.
        if let Some(name) = item.poster_path.clone() {
            return super::images::serve_cluster_artwork(&state, &name).await;
        }
    }

    let file = state
        .store
        .files_for_item(id)
        .await?
        .into_iter()
        .next()
        .ok_or(ApiError::NotFound("photo file"))?;
    super::stream::serve_file_range(&file.path, &headers).await
}
