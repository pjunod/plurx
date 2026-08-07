//! App-managed offline package API and scoped HLS capability routes.

use std::path::{Component, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plurx_core::domain::{
    NewOfflinePackage, OfflineCreateOutcome, OfflineLeaseOutcome, OfflinePackage,
};
use plurx_core::store::keys;
use plurx_core::tracks::{is_native_text_subtitle, select_tracks, LangPrefs, SubMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::offline::OfflineQuota;
use crate::state::AppState;

const PACKAGE_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub(crate) const DEFAULT_GLOBAL_GB: i64 = 25;
pub(crate) const DEFAULT_USER_GB: i64 = 15;
pub(crate) const DEFAULT_USER_ROWS: i64 = 50;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn typed(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::typed(status, code, message)
}

async fn enabled(state: &AppState) -> Result<bool, ApiError> {
    Ok(!matches!(
        state
            .store
            .get_setting(keys::OFFLINE_ENABLED)
            .await?
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    ))
}

async fn integer_setting(state: &AppState, key: &str, default: i64) -> Result<i64, ApiError> {
    Ok(state
        .store
        .get_setting(key)
        .await?
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(default)
        .max(0))
}

fn gib(value: i64) -> i64 {
    value.saturating_mul(1024 * 1024 * 1024)
}

fn offline_language_tag(language: Option<&str>) -> String {
    let tag = plurx_core::tracks::bcp47_tag(language);
    if tag.len() <= 35
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        tag.to_owned()
    } else {
        "und".to_owned()
    }
}

#[derive(Debug, Deserialize)]
pub struct OptionsQuery {
    audio_lang: Option<String>,
    subtitle_lang: Option<String>,
    subtitle_mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct QualityOption {
    height: i64,
    label: &'static str,
    estimated_bytes: i64,
    reserved_bytes: i64,
}

#[derive(Debug, Serialize)]
struct AudioOption {
    index: i64,
    codec: String,
    channels: Option<i64>,
    language: Option<String>,
    title: Option<String>,
    default: bool,
}

#[derive(Debug, Serialize)]
struct SubtitleOption {
    index: i64,
    codec: String,
    language: Option<String>,
    title: Option<String>,
    default: bool,
    forced: bool,
    offline_mode: &'static str,
}

#[derive(Debug, Serialize)]
pub struct OfflineOptions {
    file_id: i64,
    qualities: Vec<QualityOption>,
    audio: Vec<AudioOption>,
    subtitles: Vec<SubtitleOption>,
    recommended_audio_index: Option<i64>,
    recommended_subtitle_index: Option<i64>,
}

pub async fn options(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(file_id): AxPath<i64>,
    Query(query): Query<OptionsQuery>,
) -> Result<Json<OfflineOptions>, ApiError> {
    if !enabled(&state).await? {
        return Err(typed(
            StatusCode::SERVICE_UNAVAILABLE,
            "offline_disabled",
            "Offline viewing is disabled on this server.",
        ));
    }
    let file = state
        .store
        .get_file(file_id)
        .await?
        .ok_or(ApiError::NotFound("file"))?;
    if !file.probed || file.duration_ms.is_none_or(|duration| duration <= 0) {
        return Err(typed(
            StatusCode::CONFLICT,
            "source_unavailable",
            "The media must be analyzed before it can be downloaded.",
        ));
    }

    let duration_ms = file.duration_ms.unwrap_or_default();
    let qualities = crate::transcode::ladder(file.height)
        .into_iter()
        .filter(|rung| rung.height <= 1080)
        .map(|rung| QualityOption {
            height: rung.height,
            label: if rung.height <= 720 {
                "Standard"
            } else {
                "High"
            },
            estimated_bytes: duration_ms
                .saturating_mul(rung.total_kbps as i64)
                .saturating_div(8),
            reserved_bytes: duration_ms
                .saturating_mul(rung.peak_kbps as i64)
                .saturating_div(8),
        })
        .collect();

    let prefs = LangPrefs {
        audio_lang: query.audio_lang.unwrap_or_else(|| "eng".to_owned()),
        sub_lang: query.subtitle_lang.unwrap_or_else(|| "eng".to_owned()),
        sub_mode: SubMode::parse(query.subtitle_mode.as_deref().unwrap_or("auto")),
    };
    let prefer_original = file.audio_streams.len() > 1
        && file
            .audio_streams
            .iter()
            .any(|stream| matches!(stream.language.as_deref(), Some("jpn" | "ja" | "jp")));
    let selected = select_tracks(
        &file.audio_streams,
        &file.subtitle_streams,
        prefer_original,
        &prefs,
    );
    let recommended_subtitle_index = selected.subtitle_index.filter(|index| {
        file.subtitle_streams
            .iter()
            .find(|stream| stream.index == *index)
            .is_some_and(|stream| is_native_text_subtitle(&stream.codec))
    });

    Ok(Json(OfflineOptions {
        file_id,
        qualities,
        audio: file
            .audio_streams
            .into_iter()
            .map(|stream| AudioOption {
                index: stream.index,
                codec: stream.codec,
                channels: stream.channels,
                language: stream.language,
                title: stream.title,
                default: stream.default,
            })
            .collect(),
        subtitles: file
            .subtitle_streams
            .into_iter()
            .map(|stream| SubtitleOption {
                index: stream.index,
                offline_mode: if is_native_text_subtitle(&stream.codec) {
                    "native"
                } else {
                    "unavailable"
                },
                codec: stream.codec,
                language: stream.language,
                title: stream.title,
                default: stream.default,
                forced: stream.forced,
            })
            .collect(),
        recommended_audio_index: selected.audio_index,
        recommended_subtitle_index,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreatePackage {
    request_id: String,
    height: i64,
    audio_index: Option<i64>,
    subtitle_index: Option<i64>,
}

#[derive(Debug, Serialize)]
struct PackageOutput {
    height: i64,
    video_codec: &'static str,
    audio_codec: &'static str,
    dynamic_range: &'static str,
    subtitle_mode: String,
}

#[derive(Debug, Serialize)]
struct PackageError {
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
pub struct PackageStatus {
    id: String,
    state: String,
    phase: String,
    status_url: String,
    progress: Option<f64>,
    bytes_ready: i64,
    estimated_bytes: i64,
    actual_bytes: Option<i64>,
    duration_ms: Option<i64>,
    output: PackageOutput,
    error: Option<PackageError>,
}

fn status(package: OfflinePackage) -> PackageStatus {
    let progress = (package.progress_millis > 0)
        .then_some(package.progress_millis.clamp(0, 1000) as f64 / 1000.0);
    PackageStatus {
        status_url: format!("/api/v1/offline/packages/{}", package.id),
        bytes_ready: package.actual_bytes.unwrap_or(0),
        output: PackageOutput {
            height: package.target_height,
            video_codec: "h264",
            audio_codec: "aac",
            dynamic_range: "sdr",
            subtitle_mode: package.subtitle_mode.clone(),
        },
        error: package.error_code.as_ref().map(|code| PackageError {
            code: code.clone(),
            message: package.error_message.clone().unwrap_or_default(),
        }),
        id: package.id,
        state: package.state,
        phase: package.phase,
        progress,
        estimated_bytes: package.estimated_bytes,
        actual_bytes: package.actual_bytes,
        duration_ms: package.duration_ms,
    }
}

pub async fn create(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(file_id): AxPath<i64>,
    Json(request): Json<CreatePackage>,
) -> Result<impl IntoResponse, ApiError> {
    if !enabled(&state).await? {
        return Err(typed(
            StatusCode::SERVICE_UNAVAILABLE,
            "offline_disabled",
            "Offline viewing is disabled on this server.",
        ));
    }
    uuid::Uuid::parse_str(&request.request_id).map_err(|_| {
        typed(
            StatusCode::BAD_REQUEST,
            "invalid_request_id",
            "request_id must be a UUID.",
        )
    })?;
    let file = state
        .store
        .get_file(file_id)
        .await?
        .ok_or(ApiError::NotFound("file"))?;
    let duration_ms = file.duration_ms.filter(|value| *value > 0).ok_or_else(|| {
        typed(
            StatusCode::CONFLICT,
            "source_unavailable",
            "The media must be analyzed before it can be downloaded.",
        )
    })?;
    if !file.probed {
        return Err(typed(
            StatusCode::CONFLICT,
            "source_unavailable",
            "The media must be analyzed before it can be downloaded.",
        ));
    }
    let rung = crate::transcode::ladder(file.height)
        .into_iter()
        .find(|rung| rung.height == request.height && rung.height <= 1080)
        .ok_or_else(|| {
            typed(
                StatusCode::BAD_REQUEST,
                "invalid_quality",
                "Choose one of the advertised offline quality heights.",
            )
        })?;
    if request.audio_index.is_some_and(|index| {
        !file
            .audio_streams
            .iter()
            .any(|stream| stream.index == index)
    }) {
        return Err(typed(
            StatusCode::BAD_REQUEST,
            "invalid_track",
            "The selected audio track is not available.",
        ));
    }
    let (subtitle_mode, subtitle_language) = match request.subtitle_index {
        None => ("none", None),
        Some(index) => {
            let stream = file
                .subtitle_streams
                .iter()
                .find(|stream| stream.index == index)
                .ok_or_else(|| {
                    typed(
                        StatusCode::BAD_REQUEST,
                        "invalid_track",
                        "The selected subtitle track is not available.",
                    )
                })?;
            if !is_native_text_subtitle(&stream.codec) {
                return Err(typed(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_track",
                    "This subtitle format cannot be included in a one-tap download yet.",
                ));
            }
            (
                "native",
                Some(offline_language_tag(stream.language.as_deref())),
            )
        }
    };

    let expires_at = now_unix().saturating_add(PACKAGE_TTL_SECS);
    let output_size = plurx_core::transcode::output_size(&file, rung.height);
    let new = NewOfflinePackage {
        id: uuid::Uuid::new_v4().to_string(),
        request_id: request.request_id,
        user_id: user.id,
        file_id,
        node_id: state.node_id.clone(),
        source_path: file.path.to_string_lossy().into_owned(),
        source_size: file.size,
        source_mtime: file.mtime,
        target_height: rung.height,
        output_width: output_size.map(|(width, _)| width),
        output_height: output_size.map(|(_, height)| height),
        audio_index: request.audio_index,
        audio_offset_ms: file.audio_offset_ms,
        subtitle_index: request.subtitle_index,
        subtitle_language,
        subtitle_mode: subtitle_mode.to_owned(),
        estimated_bytes: duration_ms
            .saturating_mul(rung.total_kbps as i64)
            .saturating_div(8),
        reserved_bytes: duration_ms
            .saturating_mul(rung.peak_kbps as i64)
            .saturating_div(8),
        expires_at,
    };
    let max_rows =
        integer_setting(&state, keys::OFFLINE_MAX_ROWS_PER_USER, DEFAULT_USER_ROWS).await?;
    let max_user =
        gib(integer_setting(&state, keys::OFFLINE_MAX_GB_PER_USER, DEFAULT_USER_GB).await?);
    let max_global = gib(integer_setting(&state, keys::OFFLINE_MAX_GB, DEFAULT_GLOBAL_GB).await?);
    match state
        .store
        .create_offline_package(&new, max_rows, max_user, max_global)
        .await?
    {
        OfflineCreateOutcome::Created(package) => {
            state.offline.record_request(package.target_height);
            Ok((StatusCode::ACCEPTED, Json(status(package))))
        }
        OfflineCreateOutcome::Existing(package) => {
            let code = if package.state == "ready" {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            Ok((code, Json(status(package))))
        }
        OfflineCreateOutcome::RequestConflict => Err(typed(
            StatusCode::CONFLICT,
            "request_conflict",
            "That request_id was already used with different download options.",
        )),
        OfflineCreateOutcome::RowLimit { limit } => {
            state.offline.record_quota_rejection(OfflineQuota::Registry);
            Err(typed(
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                format!("The offline package registry limit is {limit} items."),
            ))
        }
        OfflineCreateOutcome::ByteLimit { used, limit } => {
            state
                .offline
                .record_quota_rejection(OfflineQuota::UserBytes);
            Err(typed(
                StatusCode::INSUFFICIENT_STORAGE,
                "quota_exceeded",
                format!("This profile has reserved {used} of {limit} offline bytes."),
            ))
        }
        OfflineCreateOutcome::GlobalByteLimit { used, limit } => {
            state
                .offline
                .record_quota_rejection(OfflineQuota::GlobalBytes);
            Err(typed(
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
                format!("The server has reserved {used} of {limit} offline bytes."),
            ))
        }
    }
}

pub async fn package_status(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(package_id): AxPath<String>,
) -> Result<Json<PackageStatus>, ApiError> {
    let package = state
        .store
        .renew_offline_package_for_user(
            &package_id,
            user.id,
            now_unix().saturating_add(PACKAGE_TTL_SECS),
        )
        .await?
        .ok_or(ApiError::NotFound("offline package"))?;
    Ok(Json(status(package)))
}

#[derive(Debug, Deserialize)]
pub struct PutLease {
    token: String,
}

#[derive(Debug, Serialize)]
pub struct LeaseResponse {
    manifest_url: String,
    expires_at: i64,
    bytes: i64,
    duration_ms: i64,
}

fn token_hash(token: &str) -> Result<String, ApiError> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(typed(
            StatusCode::BAD_REQUEST,
            "invalid_lease",
            "The lease token must be 64 lowercase hexadecimal characters.",
        ));
    }
    let bytes = hex::decode(token).map_err(|_| {
        typed(
            StatusCode::BAD_REQUEST,
            "invalid_lease",
            "The lease token is not valid hexadecimal.",
        )
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub async fn put_lease(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(package_id): AxPath<String>,
    Json(request): Json<PutLease>,
) -> Result<impl IntoResponse, ApiError> {
    let hash = token_hash(&request.token)?;
    let package = state
        .store
        .offline_package_for_user(&package_id, user.id)
        .await?
        .ok_or(ApiError::NotFound("offline package"))?;
    let expires_at = now_unix().saturating_add(PACKAGE_TTL_SECS);
    let outcome = state
        .store
        .put_offline_lease(&package_id, user.id, &hash, expires_at)
        .await?;
    let status = match outcome {
        OfflineLeaseOutcome::Created(_) => StatusCode::CREATED,
        OfflineLeaseOutcome::Renewed(_) => StatusCode::OK,
        OfflineLeaseOutcome::PackageNotReady => {
            return Err(typed(
                StatusCode::CONFLICT,
                "package_not_ready",
                "The offline package is still being prepared.",
            ));
        }
        OfflineLeaseOutcome::TokenConflict => {
            return Err(typed(
                StatusCode::CONFLICT,
                "lease_conflict",
                "This package already has a different active lease token.",
            ));
        }
    };
    Ok((
        status,
        Json(LeaseResponse {
            manifest_url: format!("/api/v1/offline/media/{}/master.m3u8", request.token),
            expires_at,
            bytes: package.actual_bytes.unwrap_or(package.estimated_bytes),
            duration_ms: package.duration_ms.unwrap_or(0),
        }),
    ))
}

pub async fn delete_package(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(package_id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    release_package(&state, &package_id, user.id, ReleaseKind::Cancelled).await
}

/// A successful device download acknowledges that the server-side intent and
/// pin are no longer needed. Completion deliberately has the same release
/// effect as cancellation, but remains a named endpoint so the client action
/// and the lifecycle contract are unambiguous.
pub async fn complete_package(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(package_id): AxPath<String>,
) -> Result<StatusCode, ApiError> {
    release_package(&state, &package_id, user.id, ReleaseKind::Completed).await
}

#[derive(Clone, Copy)]
enum ReleaseKind {
    Cancelled,
    Completed,
}

async fn release_package(
    state: &AppState,
    package_id: &str,
    user_id: i64,
    kind: ReleaseKind,
) -> Result<StatusCode, ApiError> {
    let Some(package) = state
        .store
        .offline_package_for_user(package_id, user_id)
        .await?
    else {
        return Ok(StatusCode::NO_CONTENT);
    };
    state.offline.cancel(package_id).await;
    // Release is idempotent. Returning the same result after a lost response
    // lets clients safely retry without learning whether another user owns an
    // opaque package id.
    let deleted = state
        .store
        .delete_offline_package(package_id, user_id)
        .await?;
    if deleted {
        if matches!(kind, ReleaseKind::Cancelled) {
            state.offline.record_cancellation(&package);
        }
        state.offline.forget_transfer(package_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn authorized_package(state: &AppState, token: &str) -> Result<OfflinePackage, ApiError> {
    let hash = token_hash(token)?;
    let now = now_unix();
    state
        .store
        .offline_package_for_lease(&hash, now, now.saturating_add(PACKAGE_TTL_SECS))
        .await?
        .ok_or_else(|| {
            typed(
                StatusCode::NOT_FOUND,
                "package_expired",
                "The offline package is unavailable or its lease expired.",
            )
        })
}

async fn package_dir(
    state: &AppState,
    package: &OfflinePackage,
) -> Result<(PathBuf, crate::cachekeep::CacheReadGuard), ApiError> {
    if package.node_id != state.node_id {
        return Err(typed(
            StatusCode::NOT_FOUND,
            "package_unavailable",
            "This package belongs to another server node.",
        ));
    }
    let recipe = package.recipe_hash.as_deref().ok_or_else(|| {
        typed(
            StatusCode::CONFLICT,
            "package_not_ready",
            "The offline package has not been published.",
        )
    })?;
    // Claim the recipe before either the row lookup or the filesystem read,
    // exactly as cached playback does. Offline leases pin ordinary LRU
    // eviction, but they cannot prevent a source-file cascade from removing
    // the row and exposing the directory to the orphan pass.
    let cache_reader = state
        .transcode
        .cache_readers()
        .begin_read(recipe)
        .ok_or_else(|| {
            typed(
                StatusCode::GONE,
                "package_evicted",
                "The prepared package is being removed from the server.",
            )
        })?;
    let cached = state
        .store
        .cache_hit(recipe, &state.node_id)
        .await?
        .ok_or_else(|| {
            typed(
                StatusCode::GONE,
                "package_evicted",
                "The prepared package is no longer available on the server.",
            )
        })?;
    let relative = PathBuf::from(cached.relative_dir);
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ApiError::Internal(
            "offline cache row contains an unsafe relative directory".to_owned(),
        ));
    }
    Ok((state.cache_dir.join(relative), cache_reader))
}

fn hls_response(
    state: &AppState,
    package: &OfflinePackage,
    bytes: Vec<u8>,
    content_type: &'static str,
) -> Response {
    state.offline.record_transfer(&package.id, bytes.len());
    let mut response = Body::from(bytes).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=604800, immutable"),
    );
    response
}

pub async fn master(
    State(state): State<AppState>,
    AxPath(token): AxPath<String>,
) -> Result<Response, ApiError> {
    let package = authorized_package(&state, &token).await?;
    // The synthetic master is only valid while the immutable child recipe is
    // still present. Fail the root request coherently instead of letting the
    // downloader discover eviction one child request later.
    let (_dir, _cache_reader) = package_dir(&state, &package).await?;
    let playlist = crate::offline::master_playlist(&package);
    Ok(hls_response(
        &state,
        &package,
        playlist.into_bytes(),
        "application/vnd.apple.mpegurl",
    ))
}

pub async fn playlist(
    State(state): State<AppState>,
    AxPath(token): AxPath<String>,
) -> Result<Response, ApiError> {
    let package = authorized_package(&state, &token).await?;
    let (dir, _cache_reader) = package_dir(&state, &package).await?;
    let bytes = tokio::fs::read(dir.join("index.m3u8"))
        .await
        .map_err(|_| typed(StatusCode::GONE, "package_evicted", "Playlist is missing."))?;
    if !String::from_utf8_lossy(&bytes).contains("#EXT-X-ENDLIST") {
        return Err(ApiError::Internal(
            "ready offline package does not contain a VOD playlist".to_owned(),
        ));
    }
    Ok(hls_response(
        &state,
        &package,
        bytes,
        "application/vnd.apple.mpegurl",
    ))
}

fn safe_ts_segment(segment: &str) -> bool {
    segment.len() == "seg00000.ts".len()
        && segment.starts_with("seg")
        && segment.ends_with(".ts")
        && segment[3..8].bytes().all(|byte| byte.is_ascii_digit())
}

pub async fn segment(
    State(state): State<AppState>,
    AxPath((token, segment)): AxPath<(String, String)>,
) -> Result<Response, ApiError> {
    if !safe_ts_segment(&segment) {
        return Err(ApiError::NotFound("offline segment"));
    }
    let package = authorized_package(&state, &token).await?;
    let (dir, _cache_reader) = package_dir(&state, &package).await?;
    let bytes = tokio::fs::read(dir.join(segment))
        .await
        .map_err(|_| ApiError::NotFound("offline segment"))?;
    Ok(hls_response(&state, &package, bytes, "video/mp2t"))
}

pub async fn subtitle(
    State(state): State<AppState>,
    AxPath(path): AxPath<(String, i64, String)>,
) -> Result<Response, ApiError> {
    let (token, index, segment) = path;
    let package = authorized_package(&state, &token).await?;
    if package.subtitle_mode != "native" || package.subtitle_index != Some(index) {
        return Err(ApiError::NotFound("offline subtitle"));
    }
    if segment == "index.m3u8" {
        let duration = package.duration_ms.ok_or_else(|| {
            ApiError::Internal("ready offline package has no duration".to_owned())
        })?;
        return Ok(hls_response(
            &state,
            &package,
            crate::offline::subtitle_playlist(duration).into_bytes(),
            "application/vnd.apple.mpegurl",
        ));
    }
    if segment != "seg00000.vtt" {
        return Err(ApiError::NotFound("offline subtitle"));
    }
    let sidecar = crate::subtitles::vtt_path_for_identity(
        &state.subs_dir,
        package.file_id,
        index,
        package.source_size,
        package.source_mtime,
    );
    let bytes = match tokio::fs::read(&sidecar).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Subtitle cache retention is independent from the offline pin.
            // Recreate a pruned sidecar only when the file row still names the
            // exact bytes snapshotted by this package.
            let file = state
                .store
                .get_file(package.file_id)
                .await?
                .ok_or_else(|| {
                    typed(
                        StatusCode::GONE,
                        "source_changed",
                        "The source for this offline subtitle is no longer available.",
                    )
                })?;
            let source_matches = file.path.to_string_lossy() == package.source_path
                && file.size == package.source_size
                && file.mtime == package.source_mtime
                && file
                    .subtitle_streams
                    .iter()
                    .any(|stream| stream.index == index && is_native_text_subtitle(&stream.codec));
            if !source_matches {
                return Err(typed(
                    StatusCode::GONE,
                    "source_changed",
                    "The source for this offline subtitle has changed.",
                ));
            }
            let recovered = crate::subtitles::ensure_vtt(&state.subs_dir, &file, index)
                .await
                .map_err(|message| {
                    tracing::warn!(
                        package_id = %package.id,
                        subtitle_index = index,
                        error = %message,
                        "offline subtitle recovery failed"
                    );
                    typed(
                        StatusCode::GONE,
                        "subtitle_unavailable",
                        "The offline subtitle could not be restored.",
                    )
                })?;
            tokio::fs::read(recovered).await.map_err(|_| {
                typed(
                    StatusCode::GONE,
                    "subtitle_unavailable",
                    "The offline subtitle could not be restored.",
                )
            })?
        }
        Err(error) => return Err(ApiError::Internal(error.to_string())),
    };
    Ok(hls_response(
        &state,
        &package,
        bytes,
        "text/vtt; charset=utf-8",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_tokens_are_exact_lowercase_hex() {
        assert!(token_hash(&"a".repeat(64)).is_ok());
        assert!(token_hash(&"A".repeat(64)).is_err());
        assert!(token_hash(&"a".repeat(63)).is_err());
        assert!(token_hash(&format!("{}g", "a".repeat(63))).is_err());
    }

    #[test]
    fn media_names_cannot_escape_the_package() {
        assert!(safe_ts_segment("seg00000.ts"));
        assert!(safe_ts_segment("seg99999.ts"));
        assert!(!safe_ts_segment("seg0.ts"));
        assert!(!safe_ts_segment("../seg00000.ts"));
        assert!(!safe_ts_segment("seg00000.m4s"));
    }

    #[test]
    fn offline_language_tags_are_playlist_safe() {
        assert_eq!(offline_language_tag(Some("eng")), "en");
        assert_eq!(offline_language_tag(Some("pt-BR")), "pt-BR");
        assert_eq!(offline_language_tag(Some("en\"\n#EXT-X-KEY")), "und");
    }
}
