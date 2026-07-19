//! Library management. Listing is open to any authenticated user; mutations
//! require admin. Creating or editing a library kicks off a background scan.

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::Json;
use plurx_core::domain::{LibraryKind, NewLibrary};
use serde::{Deserialize, Serialize};

use super::dto::LibraryDto;
use super::error::ApiError;
use super::extract::{AdminUser, AuthUser};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LibraryInput {
    pub name: String,
    pub kind: String,
    pub paths: Vec<String>,
    /// Flag a shows library as anime (absolute numbering + AniList).
    #[serde(default)]
    pub anime: bool,
}

impl LibraryInput {
    fn validate(self) -> Result<NewLibrary, ApiError> {
        let kind = LibraryKind::parse(&self.kind)
            .ok_or_else(|| ApiError::BadRequest(format!("unknown library kind `{}`", self.kind)))?;
        if self.name.trim().is_empty() {
            return Err(ApiError::BadRequest("library name is required".into()));
        }
        let paths: Vec<PathBuf> = self
            .paths
            .into_iter()
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
        if paths.is_empty() {
            return Err(ApiError::BadRequest("at least one path is required".into()));
        }
        // Anime only applies to shows libraries.
        let anime = self.anime && kind == LibraryKind::Shows;
        Ok(NewLibrary {
            name: self.name.trim().to_owned(),
            kind,
            paths,
            anime,
        })
    }
}

#[derive(Serialize)]
pub struct ScanTriggered {
    pub started: bool,
}

/// GET /api/v1/libraries
pub async fn list(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<LibraryDto>>, ApiError> {
    let libraries = state.store.list_libraries().await?;
    Ok(Json(libraries.into_iter().map(Into::into).collect()))
}

/// POST /api/v1/libraries (admin) — create and scan.
pub async fn create(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(input): Json<LibraryInput>,
) -> Result<Json<LibraryDto>, ApiError> {
    let new = input.validate()?;
    let library = state.store.create_library(&new).await.map_err(|e| {
        // A duplicate name is the common user error worth surfacing clearly.
        if e.to_string().contains("UNIQUE") {
            ApiError::Conflict(format!("a library named `{}` already exists", new.name))
        } else {
            ApiError::from(e)
        }
    })?;
    state.jobs.trigger_scan(library.id).await;
    Ok(Json(library.into()))
}

/// PUT /api/v1/libraries/:id (admin) — update and rescan.
pub async fn update(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<LibraryInput>,
) -> Result<Json<LibraryDto>, ApiError> {
    let new = input.validate()?;
    let library = state
        .store
        .update_library(id, &new)
        .await?
        .ok_or(ApiError::NotFound("library"))?;
    state.jobs.trigger_scan(library.id).await;
    Ok(Json(library.into()))
}

/// DELETE /api/v1/libraries/:id (admin)
pub async fn delete(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.delete_library(id).await? {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err(ApiError::NotFound("library"))
    }
}

/// POST /api/v1/libraries/:id/scan (admin)
pub async fn scan(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ScanTriggered>, ApiError> {
    if state.store.get_library(id).await?.is_none() {
        return Err(ApiError::NotFound("library"));
    }
    let started = state.jobs.trigger_scan(id).await;
    Ok(Json(ScanTriggered { started }))
}
