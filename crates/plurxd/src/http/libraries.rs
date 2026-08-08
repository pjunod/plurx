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

/// The automatic schedule for one library, in minutes. `0` turns a job off.
#[derive(Deserialize)]
pub struct ScheduleInput {
    #[serde(default)]
    pub scan_interval_mins: i64,
    #[serde(default)]
    pub refresh_interval_mins: i64,
}

/// PUT /api/v1/libraries/:id/schedule (admin) — set the automatic intervals.
///
/// Separate from the library update because that one rescans on save: changing
/// "scan every 6 hours" to "every 12" should not itself start a scan, or the
/// settings page becomes a way to hammer a NAS by fiddling with a dropdown.
pub async fn set_schedule(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<ScheduleInput>,
) -> Result<Json<LibraryDto>, ApiError> {
    // A floor rather than a free-for-all: scanning a real library every minute
    // is a NAS denial-of-service with a schedule attached, and the loop only
    // ticks once a minute anyway. 0 (off) stays exactly 0.
    const MIN_INTERVAL_MINS: i64 = 15;
    for (label, value) in [
        ("scan_interval_mins", input.scan_interval_mins),
        ("refresh_interval_mins", input.refresh_interval_mins),
    ] {
        if value < 0 {
            return Err(ApiError::BadRequest(format!("{label} cannot be negative")));
        }
        if value > 0 && value < MIN_INTERVAL_MINS {
            return Err(ApiError::BadRequest(format!(
                "{label} must be 0 (off) or at least {MIN_INTERVAL_MINS} minutes"
            )));
        }
    }
    let library = state
        .store
        .set_library_schedule(id, input.scan_interval_mins, input.refresh_interval_mins)
        .await?
        .ok_or(ApiError::NotFound("library"))?;
    tracing::info!(
        library = id,
        scan_interval_mins = library.scan_interval_mins,
        refresh_interval_mins = library.refresh_interval_mins,
        "library schedule updated"
    );
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

/// POST /api/v1/libraries/:id/refresh (admin) — rescan + force a full metadata
/// refresh (re-fetches even already-matched items, e.g. to backfill season art).
pub async fn refresh(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ScanTriggered>, ApiError> {
    if state.store.get_library(id).await?.is_none() {
        return Err(ApiError::NotFound("library"));
    }
    let started = state.jobs.trigger_refresh(id).await;
    Ok(Json(ScanTriggered { started }))
}

/// POST /api/v1/libraries/:id/root-identity/reset (admin) — allow the next
/// verified non-empty scan to establish a deliberately replaced mount.
pub async fn reset_root_identity(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_library(id).await?.is_none() {
        return Err(ApiError::NotFound("library"));
    }
    let cleared = state.store.reset_library_root_fingerprint(id).await?;
    Ok(Json(serde_json::json!({ "ok": true, "cleared": cleared })))
}
