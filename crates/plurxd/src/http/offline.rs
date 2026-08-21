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
use plurx_core::tracks::{
    is_native_text_subtitle, prefers_original_audio, select_tracks, LangPrefs, SubMode,
};
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
    let selected = select_tracks(
        &file.audio_streams,
        &file.subtitle_streams,
        prefers_original_audio(&file.audio_streams),
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
    let effective_rate_control = state
        .transcode
        .effective_rate_control_for_new_offline_package(&file)
        .await
        .map_err(|message| {
            typed(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_media",
                message,
            )
        })?
        .snapshot_value();
    let new = NewOfflinePackage {
        id: uuid::Uuid::new_v4().to_string(),
        request_id: request.request_id,
        user_id: user.id,
        file_id,
        node_id: state.node_id.clone(),
        source_path: file.path.to_string_lossy().into_owned(),
        source_size: file.size,
        source_mtime: file.mtime,
        effective_rate_control,
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
        OfflineCreateOutcome::NodeIsTombstone => Err(typed(
            StatusCode::SERVICE_UNAVAILABLE,
            "node_removed",
            "This node has been removed from the cluster and can no longer create offline downloads. Point your client at a surviving node.",
        ))
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
    if !enabled(state).await? {
        return Err(typed(
            StatusCode::SERVICE_UNAVAILABLE,
            "offline_disabled",
            "Offline viewing is disabled by the server administrator.",
        ));
    }
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

    use http_body_util::BodyExt;
    use plurx_core::domain::{
        AudioStream, ItemKind, LibraryKind, NewItem, NewLibrary, OfflineCreateOutcome, ProbeResult,
        SubtitleStream, User,
    };
    use plurx_core::store::{SqliteStore, Store};
    use plurx_core::transcode::EffectiveRateControl;
    use serde_json::Value;
    use std::sync::Arc;

    struct Fixture {
        state: AppState,
        user: User,
        file: plurx_core::domain::MediaFile,
        library_id: i64,
        _root: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("root");
        let source = root.path().join("movie.mkv");
        std::fs::write(&source, b"offline HTTP fixture").expect("source");
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open_in_memory().expect("store"));
        let user = store
            .create_user("traveller", "hash", false)
            .await
            .expect("user");
        let library = store
            .create_library(&NewLibrary {
                name: "Offline".into(),
                kind: LibraryKind::Movies,
                paths: vec![root.path().to_path_buf()],
                anime: false,
            })
            .await
            .expect("library");
        let item = store
            .insert_item(&NewItem {
                library_id: library.id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Flight".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("item");
        let file_id = store
            .upsert_file(
                item,
                &source.to_string_lossy(),
                20,
                7,
                &ProbeResult {
                    duration_ms: Some(90_000),
                    container: Some("mkv".into()),
                    video_codec: Some("hevc".into()),
                    width: Some(1920),
                    height: Some(1080),
                    audio_streams: vec![
                        AudioStream {
                            index: 0,
                            codec: "aac".into(),
                            channels: Some(2),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        AudioStream {
                            index: 1,
                            codec: "aac".into(),
                            channels: Some(2),
                            language: Some("jpn".into()),
                            ..Default::default()
                        },
                    ],
                    subtitle_streams: vec![
                        SubtitleStream {
                            index: 4,
                            codec: "subrip".into(),
                            language: Some("eng".into()),
                            default: true,
                            ..Default::default()
                        },
                        SubtitleStream {
                            index: 5,
                            codec: "hdmv_pgs_subtitle".into(),
                            language: Some("eng".into()),
                            ..Default::default()
                        },
                    ],
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("file");
        let dirs = crate::state::Dirs {
            artwork: root.path().join("artwork"),
            transcode: root.path().join("transcode"),
            cache: root.path().join("cache"),
            subs: root.path().join("subs"),
        };
        for dir in [&dirs.artwork, &dirs.transcode, &dirs.cache, &dirs.subs] {
            std::fs::create_dir_all(dir).expect("state directory");
        }
        let state = AppState::new(
            "test".into(),
            Arc::clone(&store),
            dirs,
            "test-node".into(),
            Default::default(),
            Default::default(),
            Arc::new(crate::logbuf::LogBuffer::new(16)),
        );
        let file = state
            .store
            .get_file(file_id)
            .await
            .expect("file lookup")
            .expect("file");
        Fixture {
            state,
            user,
            file,
            library_id: library.id,
            _root: root,
        }
    }

    async fn json_response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("response body")
            .to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("JSON response")
        };
        (status, body)
    }

    async fn result_response<T: IntoResponse>(result: Result<T, ApiError>) -> (StatusCode, Value) {
        match result {
            Ok(value) => json_response(value.into_response()).await,
            Err(error) => json_response(error.into_response()).await,
        }
    }

    async fn error_response(error: ApiError) -> (StatusCode, Value) {
        json_response(error.into_response()).await
    }

    fn request(request_id: &str) -> CreatePackage {
        CreatePackage {
            request_id: request_id.into(),
            height: 720,
            audio_index: Some(0),
            subtitle_index: Some(4),
        }
    }

    async fn create_response(
        fixture: &Fixture,
        file_id: i64,
        request: CreatePackage,
    ) -> (StatusCode, Value) {
        result_response(
            create(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath(file_id),
                Json(request),
            )
            .await,
        )
        .await
    }

    async fn insert_unprobed(fixture: &Fixture, duration_ms: Option<i64>) -> i64 {
        let item = fixture
            .state
            .store
            .insert_item(&NewItem {
                library_id: fixture.library_id,
                kind: ItemKind::Movie,
                parent_id: None,
                title: "Unprobed".into(),
                year: None,
                season_number: None,
                episode_number: None,
            })
            .await
            .expect("unprobed item");
        fixture
            .state
            .store
            .upsert_file(
                item,
                &format!("/missing/unprobed-{item}.mkv"),
                1,
                1,
                &ProbeResult {
                    duration_ms,
                    raw_json: None,
                    ..Default::default()
                },
            )
            .await
            .expect("unprobed file")
    }

    async fn new_package_with_source_snapshot(
        fixture: &Fixture,
        id: &str,
        subtitle_mode: &str,
        subtitle_index: Option<i64>,
        file_id: i64,
        source_snapshot: (String, i64, i64),
    ) -> OfflinePackage {
        let (source_path, source_size, source_mtime) = source_snapshot;
        let package = NewOfflinePackage {
            id: id.into(),
            request_id: format!("request-{id}"),
            user_id: fixture.user.id,
            file_id,
            node_id: "test-node".into(),
            source_path,
            source_size,
            source_mtime,
            effective_rate_control: EffectiveRateControl::Vbr.snapshot_value(),
            target_height: 720,
            output_width: Some(1280),
            output_height: Some(720),
            audio_index: Some(0),
            audio_offset_ms: 0,
            subtitle_index,
            subtitle_language: subtitle_index.map(|_| "en".into()),
            subtitle_mode: subtitle_mode.into(),
            estimated_bytes: 100,
            reserved_bytes: 120,
            expires_at: i64::MAX,
        };
        let created = fixture
            .state
            .store
            .create_offline_package(&package, 20, 10_000, 20_000)
            .await
            .expect("create package");
        let OfflineCreateOutcome::Created(package) = created else {
            panic!("new package was not created: {created:?}");
        };
        package
    }

    async fn new_package(
        fixture: &Fixture,
        id: &str,
        subtitle_mode: &str,
        subtitle_index: Option<i64>,
        file_id: i64,
    ) -> OfflinePackage {
        new_package_with_source_snapshot(
            fixture,
            id,
            subtitle_mode,
            subtitle_index,
            file_id,
            (
                fixture.file.path.to_string_lossy().into_owned(),
                fixture.file.size,
                fixture.file.mtime,
            ),
        )
        .await
    }

    async fn ready_package(
        fixture: &Fixture,
        id: &str,
        subtitle_mode: &str,
        subtitle_index: Option<i64>,
    ) -> OfflinePackage {
        new_package(fixture, id, subtitle_mode, subtitle_index, fixture.file.id).await;
        fixture
            .state
            .store
            .claim_next_offline_package("test-node")
            .await
            .expect("claim")
            .expect("package");
        let relative = format!("ready/{id}");
        let dir = fixture.state.cache_dir.join(&relative);
        tokio::fs::create_dir_all(&dir).await.expect("cache dir");
        tokio::fs::write(
            dir.join("index.m3u8"),
            b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:90,\nseg00000.ts\n#EXT-X-ENDLIST\n",
        )
        .await
        .expect("playlist");
        tokio::fs::write(dir.join("seg00000.ts"), b"portable-video")
            .await
            .expect("segment");
        fixture
            .state
            .store
            .claim_cache_entry(id, fixture.file.id, 7, "test-node", &relative)
            .await
            .expect("cache claim");
        fixture
            .state
            .store
            .complete_cache_entry(id, "test-node", 100)
            .await
            .expect("complete cache");
        assert!(fixture
            .state
            .store
            .mark_offline_package_ready(id, "test-node", id, 100, 90_000)
            .await
            .expect("mark ready"));
        fixture
            .state
            .store
            .offline_package_for_user(id, fixture.user.id)
            .await
            .expect("ready lookup")
            .expect("ready package")
    }

    async fn lease(fixture: &Fixture, package_id: &str, token: &str) -> (StatusCode, Value) {
        result_response(
            put_lease(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath(package_id.into()),
                Json(PutLease {
                    token: token.into(),
                }),
            )
            .await,
        )
        .await
    }

    async fn ready_subtitle_lease(fixture: &Fixture, package_id: &str) -> String {
        fixture
            .state
            .store
            .claim_next_offline_package("test-node")
            .await
            .expect("claim")
            .expect("package");
        assert!(fixture
            .state
            .store
            .mark_offline_package_ready(package_id, "test-node", "unused", 10, 90_000)
            .await
            .expect("mark ready"));
        let token = "1".repeat(64);
        assert_eq!(
            lease(fixture, package_id, &token).await.0,
            StatusCode::CREATED
        );
        token
    }

    async fn replace_subtitle_streams(fixture: &Fixture, streams: Vec<SubtitleStream>) {
        let file_id = fixture
            .state
            .store
            .upsert_file(
                fixture.file.item_id,
                &fixture.file.path.to_string_lossy(),
                fixture.file.size,
                fixture.file.mtime,
                &ProbeResult {
                    duration_ms: fixture.file.duration_ms,
                    subtitle_streams: streams,
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("replace subtitle streams");
        assert_eq!(file_id, fixture.file.id);
    }

    async fn assert_changed_subtitle_source(
        fixture: &Fixture,
        package_id: &str,
        token: String,
        index: i64,
    ) {
        let package = fixture
            .state
            .store
            .offline_package_for_user(package_id, fixture.user.id)
            .await
            .expect("package lookup")
            .expect("package");
        let sidecar = crate::subtitles::vtt_path_for_identity(
            &fixture.state.subs_dir,
            package.file_id,
            index,
            package.source_size,
            package.source_mtime,
        );
        assert!(
            !tokio::fs::try_exists(&sidecar)
                .await
                .expect("sidecar lookup"),
            "the recovery guard must be exercised after the sidecar is pruned"
        );

        let (code, body) = error_response(
            subtitle(
                State(fixture.state.clone()),
                AxPath((token, index, "seg00000.vtt".into())),
            )
            .await
            .expect_err("changed subtitle source"),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(body["code"], "source_changed");
        assert_eq!(
            body["message"],
            "The source for this offline subtitle has changed."
        );
    }

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

    #[tokio::test]
    async fn options_follow_the_shared_track_policy_and_refuse_unusable_sources() {
        let fixture = fixture().await;
        let available = options(
            AuthUser(fixture.user.clone()),
            State(fixture.state.clone()),
            AxPath(fixture.file.id),
            Query(OptionsQuery {
                audio_lang: Some("jpn".into()),
                subtitle_lang: Some("eng".into()),
                subtitle_mode: Some("always".into()),
            }),
        )
        .await
        .expect("options")
        .0;
        assert_eq!(available.file_id, fixture.file.id);
        assert_eq!(available.recommended_audio_index, Some(1));
        assert_eq!(available.recommended_subtitle_index, Some(4));
        assert_eq!(available.qualities.len(), 4);
        assert_eq!(available.qualities[0].height, 1080);
        assert_eq!(available.qualities[0].label, "High");
        assert!(available.qualities[0].reserved_bytes > available.qualities[0].estimated_bytes);
        assert_eq!(available.audio.len(), 2);
        assert_eq!(available.subtitles[0].offline_mode, "native");
        assert_eq!(available.subtitles[1].offline_mode, "unavailable");

        fixture
            .state
            .store
            .put_setting(keys::OFFLINE_ENABLED, "false")
            .await
            .expect("disable offline");
        let (code, body) = error_response(
            options(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath(fixture.file.id),
                Query(OptionsQuery {
                    audio_lang: None,
                    subtitle_lang: None,
                    subtitle_mode: None,
                }),
            )
            .await
            .expect_err("disabled options"),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "offline_disabled");
        fixture
            .state
            .store
            .put_setting(keys::OFFLINE_ENABLED, "1")
            .await
            .expect("enable offline");

        let unprobed = insert_unprobed(&fixture, None).await;
        let (code, body) = error_response(
            options(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath(unprobed),
                Query(OptionsQuery {
                    audio_lang: None,
                    subtitle_lang: None,
                    subtitle_mode: None,
                }),
            )
            .await
            .expect_err("unprobed options"),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(body["code"], "source_unavailable");
    }

    #[tokio::test]
    async fn create_validates_every_client_control_before_persisting_work() {
        let fixture = fixture().await;
        fixture
            .state
            .store
            .put_setting(keys::OFFLINE_ENABLED, "no")
            .await
            .expect("disable offline");
        let (code, body) = create_response(
            &fixture,
            fixture.file.id,
            request(&uuid::Uuid::new_v4().to_string()),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "offline_disabled");
        fixture
            .state
            .store
            .put_setting(keys::OFFLINE_ENABLED, "1")
            .await
            .expect("enable offline");

        let (code, body) = create_response(&fixture, fixture.file.id, request("not-a-uuid")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request_id");

        let (code, body) = create_response(
            &fixture,
            999_999,
            request(&uuid::Uuid::new_v4().to_string()),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "file not found");

        for duration in [None, Some(90_000)] {
            let file_id = insert_unprobed(&fixture, duration).await;
            let (code, body) = create_response(
                &fixture,
                file_id,
                request(&uuid::Uuid::new_v4().to_string()),
            )
            .await;
            assert_eq!(code, StatusCode::CONFLICT, "{body}");
            assert_eq!(body["code"], "source_unavailable");
        }

        let mut invalid_quality = request(&uuid::Uuid::new_v4().to_string());
        invalid_quality.height = 999;
        let (code, body) = create_response(&fixture, fixture.file.id, invalid_quality).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_quality");

        let mut invalid_audio = request(&uuid::Uuid::new_v4().to_string());
        invalid_audio.audio_index = Some(99);
        let (code, body) = create_response(&fixture, fixture.file.id, invalid_audio).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_track");

        let mut missing_subtitle = request(&uuid::Uuid::new_v4().to_string());
        missing_subtitle.subtitle_index = Some(99);
        let (code, body) = create_response(&fixture, fixture.file.id, missing_subtitle).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_track");

        let mut bitmap_subtitle = request(&uuid::Uuid::new_v4().to_string());
        bitmap_subtitle.subtitle_index = Some(5);
        let (code, body) = create_response(&fixture, fixture.file.id, bitmap_subtitle).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "invalid_track");
        assert_eq!(
            body["message"],
            "This subtitle format cannot be included in a one-tap download yet."
        );
    }

    #[tokio::test]
    async fn create_reports_each_bounded_quota_without_creating_a_package() {
        let fixture = fixture().await;
        let cases = [
            (
                keys::OFFLINE_MAX_ROWS_PER_USER,
                "0",
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                "registry",
            ),
            (
                keys::OFFLINE_MAX_GB_PER_USER,
                "-1",
                StatusCode::INSUFFICIENT_STORAGE,
                "quota_exceeded",
                "user_bytes",
            ),
            (
                keys::OFFLINE_MAX_GB,
                "0",
                StatusCode::INSUFFICIENT_STORAGE,
                "insufficient_storage",
                "global_bytes",
            ),
        ];
        for (index, (key, value, expected_status, expected_code, metric)) in
            cases.into_iter().enumerate()
        {
            fixture
                .state
                .store
                .put_settings(&[
                    (keys::OFFLINE_MAX_ROWS_PER_USER, "50"),
                    (keys::OFFLINE_MAX_GB_PER_USER, "15"),
                    (keys::OFFLINE_MAX_GB, "25"),
                    (key, value),
                ])
                .await
                .expect("quota setting");
            let (code, body) = create_response(
                &fixture,
                fixture.file.id,
                request(&uuid::Uuid::new_v4().to_string()),
            )
            .await;
            assert_eq!(code, expected_status, "case {index}: {body}");
            assert_eq!(body["code"], expected_code);
            assert!(fixture
                .state
                .offline
                .prometheus()
                .contains(&format!("reason=\"{metric}\"}} 1")));
            let packages = fixture
                .state
                .store
                .offline_package_stats("test-node", now_unix())
                .await
                .expect("offline package stats");
            assert_eq!(
                packages,
                plurx_core::domain::OfflinePackageStats::default(),
                "case {index} persisted work after a quota rejection"
            );
        }
    }

    #[tokio::test]
    async fn an_idempotent_create_returns_the_original_ready_state() {
        let fixture = fixture().await;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (code, first) = create_response(&fixture, fixture.file.id, request(&request_id)).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{first}");
        assert_eq!(first["state"], "queued");
        assert_eq!(first["progress"], Value::Null);
        let package_id = first["id"].as_str().expect("package id");
        fixture
            .state
            .store
            .claim_next_offline_package("test-node")
            .await
            .expect("claim")
            .expect("package");
        assert!(fixture
            .state
            .store
            .mark_offline_package_ready(package_id, "test-node", "ready-recipe", 456, 90_000)
            .await
            .expect("ready"));

        let (code, retry) = create_response(&fixture, fixture.file.id, request(&request_id)).await;
        assert_eq!(code, StatusCode::OK, "{retry}");
        assert_eq!(retry["id"], package_id);
        assert_eq!(retry["state"], "ready");
        assert_eq!(retry["bytes_ready"], 456);
        assert_eq!(retry["duration_ms"], 90_000);

        let package = fixture
            .state
            .store
            .offline_package_for_user(package_id, fixture.user.id)
            .await
            .expect("package")
            .expect("package");
        let mut failed = package.clone();
        failed.progress_millis = 1_500;
        failed.error_code = Some("encoder_failed".into());
        failed.error_message = Some("Preparation failed".into());
        let status = status(failed);
        assert_eq!(status.progress, Some(1.0));
        assert_eq!(status.error.expect("typed error").code, "encoder_failed");
    }

    #[tokio::test]
    async fn package_status_renews_only_the_owning_users_intent() {
        let fixture = fixture().await;
        let package = ready_package(&fixture, "status", "none", None).await;
        let token_hash = token_hash(&"1".repeat(64)).expect("lease hash");
        let poll_started_at = now_unix();
        let expired_at = poll_started_at.saturating_sub(1);
        assert!(matches!(
            fixture
                .state
                .store
                .put_offline_lease(&package.id, fixture.user.id, &token_hash, expired_at,)
                .await
                .expect("seed expired lease"),
            OfflineLeaseOutcome::Created(_)
        ));

        let status = package_status(
            AuthUser(fixture.user.clone()),
            State(fixture.state.clone()),
            AxPath(package.id.clone()),
        )
        .await
        .expect("owned status")
        .0;
        assert_eq!(status.id, package.id);
        assert_eq!(status.state, "ready");
        let renewed = fixture
            .state
            .store
            .offline_package_for_user(&package.id, fixture.user.id)
            .await
            .expect("renewed package lookup")
            .expect("renewed package");
        assert!(
            renewed.expires_at >= poll_started_at.saturating_add(PACKAGE_TTL_SECS),
            "status poll did not extend package expiry: {}",
            renewed.expires_at
        );
        assert!(fixture
            .state
            .store
            .offline_package_for_lease(&token_hash, poll_started_at, i64::MAX)
            .await
            .expect("renewed lease lookup")
            .is_some());

        let stranger = fixture
            .state
            .store
            .create_user("stranger", "hash", false)
            .await
            .expect("stranger");
        let (code, body) = error_response(
            package_status(
                AuthUser(stranger),
                State(fixture.state.clone()),
                AxPath(package.id),
            )
            .await
            .expect_err("cross-user status"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "offline package not found");
    }

    #[tokio::test]
    async fn lease_creation_is_stable_renewable_and_refuses_rotation() {
        let fixture = fixture().await;
        ready_package(&fixture, "stable-lease", "none", None).await;
        let token = "a".repeat(64);
        let (code, first) = lease(&fixture, "stable-lease", &token).await;
        assert_eq!(code, StatusCode::CREATED, "{first}");
        assert_eq!(
            first["manifest_url"],
            format!("/api/v1/offline/media/{token}/master.m3u8")
        );
        assert_eq!(first["bytes"], 100);
        assert_eq!(first["duration_ms"], 90_000);

        let (code, renewed) = lease(&fixture, "stable-lease", &token).await;
        assert_eq!(code, StatusCode::OK, "{renewed}");
        assert_eq!(renewed["manifest_url"], first["manifest_url"]);
        assert!(renewed["expires_at"].as_i64() >= first["expires_at"].as_i64());

        let (code, conflict) = lease(&fixture, "stable-lease", &"b".repeat(64)).await;
        assert_eq!(code, StatusCode::CONFLICT, "{conflict}");
        assert_eq!(conflict["code"], "lease_conflict");
    }

    #[tokio::test]
    async fn completed_device_download_releases_server_state_without_counting_cancellation() {
        let fixture = fixture().await;
        ready_package(&fixture, "completed", "none", None).await;
        let token = "c".repeat(64);
        assert_eq!(
            lease(&fixture, "completed", &token).await.0,
            StatusCode::CREATED
        );
        fixture.state.offline.record_transfer("completed", 10);

        let code = complete_package(
            AuthUser(fixture.user.clone()),
            State(fixture.state.clone()),
            AxPath("completed".into()),
        )
        .await
        .expect("complete package");
        assert_eq!(code, StatusCode::NO_CONTENT);
        assert!(fixture
            .state
            .store
            .offline_package_for_user("completed", fixture.user.id)
            .await
            .expect("package lookup")
            .is_none());
        assert!(fixture
            .state
            .store
            .offline_package_for_lease(&token_hash(&token).expect("hash"), now_unix(), i64::MAX)
            .await
            .expect("lease lookup")
            .is_none());
        assert_eq!(fixture.state.offline.transfer_bytes("completed"), None);
        assert!(fixture
            .state
            .offline
            .prometheus()
            .contains("plurx_offline_cancellations_total 0"));

        assert_eq!(
            complete_package(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath("completed".into()),
            )
            .await
            .expect("idempotent completion"),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    async fn cancellation_deletes_only_owned_intent_and_records_the_terminal_result() {
        let fixture = fixture().await;
        let package = new_package(&fixture, "cancelled", "none", None, fixture.file.id).await;
        fixture.state.offline.record_transfer(&package.id, 5);

        assert_eq!(
            delete_package(
                AuthUser(fixture.user.clone()),
                State(fixture.state.clone()),
                AxPath(package.id.clone()),
            )
            .await
            .expect("cancel package"),
            StatusCode::NO_CONTENT
        );
        assert!(fixture
            .state
            .store
            .offline_package_for_user(&package.id, fixture.user.id)
            .await
            .expect("package lookup")
            .is_none());
        assert_eq!(fixture.state.offline.transfer_bytes(&package.id), None);
        let metrics = fixture.state.offline.prometheus();
        assert!(metrics.contains("plurx_offline_cancellations_total 1"));
        assert!(metrics.contains("plurx_offline_prepare_seconds_count{result=\"cancelled\"} 1"));
    }

    #[tokio::test]
    async fn leased_media_is_immutable_typed_and_scoped_to_the_selected_package() {
        let fixture = fixture().await;
        let package = ready_package(&fixture, "media", "native", Some(4)).await;
        let sidecar = crate::subtitles::vtt_path_for_identity(
            &fixture.state.subs_dir,
            package.file_id,
            4,
            package.source_size,
            package.source_mtime,
        );
        tokio::fs::create_dir_all(sidecar.parent().expect("sidecar parent"))
            .await
            .expect("sidecar parent");
        tokio::fs::write(&sidecar, b"WEBVTT\n\n00:00.000 --> 00:01.000\nHello\n")
            .await
            .expect("sidecar");
        let token = "d".repeat(64);
        assert_eq!(
            lease(&fixture, &package.id, &token).await.0,
            StatusCode::CREATED
        );

        let master = master(State(fixture.state.clone()), AxPath(token.clone()))
            .await
            .expect("master");
        assert_eq!(master.status(), StatusCode::OK);
        assert_eq!(
            master.headers()[header::CONTENT_TYPE],
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(
            master.headers()[header::CACHE_CONTROL],
            "private, max-age=604800, immutable"
        );

        let media_playlist = playlist(State(fixture.state.clone()), AxPath(token.clone()))
            .await
            .expect("playlist");
        assert_eq!(media_playlist.status(), StatusCode::OK);

        let media_segment = segment(
            State(fixture.state.clone()),
            AxPath((token.clone(), "seg00000.ts".into())),
        )
        .await
        .expect("segment");
        assert_eq!(media_segment.headers()[header::CONTENT_TYPE], "video/mp2t");
        let expected_transfer_bytes = crate::offline::master_playlist(&package).len()
            + b"#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:90,\nseg00000.ts\n#EXT-X-ENDLIST\n"
                .len()
            + b"portable-video".len();
        assert_eq!(
            fixture.state.offline.transfer_bytes(&package.id),
            Some(expected_transfer_bytes as u64)
        );

        let subtitles = subtitle(
            State(fixture.state.clone()),
            AxPath((token.clone(), 4, "index.m3u8".into())),
        )
        .await
        .expect("subtitle playlist");
        assert_eq!(subtitles.status(), StatusCode::OK);
        let subtitles = subtitle(
            State(fixture.state.clone()),
            AxPath((token.clone(), 4, "seg00000.vtt".into())),
        )
        .await
        .expect("subtitle segment");
        assert_eq!(
            subtitles.headers()[header::CONTENT_TYPE],
            "text/vtt; charset=utf-8"
        );

        for path in [
            (token.clone(), 5, "seg00000.vtt".into()),
            (token.clone(), 4, "seg00001.vtt".into()),
        ] {
            let (code, _) = error_response(
                subtitle(State(fixture.state.clone()), AxPath(path))
                    .await
                    .expect_err("unselected subtitle resource"),
            )
            .await;
            assert_eq!(code, StatusCode::NOT_FOUND);
        }
        let (code, _) = error_response(
            segment(
                State(fixture.state.clone()),
                AxPath((token.clone(), "../seg00000.ts".into())),
            )
            .await
            .expect_err("unsafe segment"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = error_response(
            segment(
                State(fixture.state.clone()),
                AxPath((token.clone(), "seg99999.ts".into())),
            )
            .await
            .expect_err("missing segment"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);

        let dir = fixture.state.cache_dir.join("ready/media");
        tokio::fs::write(dir.join("index.m3u8"), b"#EXTM3U\n")
            .await
            .expect("replace playlist");
        let (code, body) = error_response(
            playlist(State(fixture.state.clone()), AxPath(token))
                .await
                .expect_err("unfinished playlist"),
        )
        .await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "internal server error");
    }

    #[tokio::test]
    async fn package_directory_refuses_wrong_nodes_missing_rows_and_unsafe_cache_paths() {
        let fixture = fixture().await;
        let package = ready_package(&fixture, "directory", "none", None).await;

        let mut wrong_node = package.clone();
        wrong_node.node_id = "other-node".into();
        assert_eq!(
            error_response(
                package_dir(&fixture.state, &wrong_node)
                    .await
                    .err()
                    .expect("wrong node"),
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );

        let mut unpublished = package.clone();
        unpublished.recipe_hash = None;
        assert_eq!(
            error_response(
                package_dir(&fixture.state, &unpublished)
                    .await
                    .err()
                    .expect("unpublished package"),
            )
            .await
            .0,
            StatusCode::CONFLICT
        );

        let mut missing = package.clone();
        missing.recipe_hash = Some("missing-cache-row".into());
        let (code, body) = error_response(
            package_dir(&fixture.state, &missing)
                .await
                .err()
                .expect("missing cache row"),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(body["code"], "package_evicted");

        fixture
            .state
            .store
            .claim_cache_entry(
                "unsafe-cache-row",
                fixture.file.id,
                7,
                "test-node",
                "../escape",
            )
            .await
            .expect("unsafe cache claim");
        fixture
            .state
            .store
            .complete_cache_entry("unsafe-cache-row", "test-node", 1)
            .await
            .expect("unsafe cache completion");
        let mut unsafe_path = package;
        unsafe_path.recipe_hash = Some("unsafe-cache-row".into());
        assert_eq!(
            error_response(
                package_dir(&fixture.state, &unsafe_path)
                    .await
                    .err()
                    .expect("unsafe cache path"),
            )
            .await
            .0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn expired_disabled_and_unknown_leases_cannot_read_package_state() {
        let fixture = fixture().await;
        ready_package(&fixture, "expired", "none", None).await;
        let token = "e".repeat(64);
        let hash = token_hash(&token).expect("hash");
        assert!(matches!(
            fixture
                .state
                .store
                .put_offline_lease("expired", fixture.user.id, &hash, now_unix() - 1)
                .await
                .expect("expired lease"),
            OfflineLeaseOutcome::Created(_)
        ));
        let (code, body) = error_response(
            authorized_package(&fixture.state, &token)
                .await
                .expect_err("expired capability"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "package_expired");

        let (code, body) = error_response(
            authorized_package(&fixture.state, &"f".repeat(64))
                .await
                .expect_err("unknown capability"),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "package_expired");

        fixture
            .state
            .store
            .put_setting(keys::OFFLINE_ENABLED, "0")
            .await
            .expect("disable offline");
        let (code, body) = error_response(
            authorized_package(&fixture.state, &token)
                .await
                .expect_err("disabled media"),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "offline_disabled");
    }

    #[tokio::test]
    async fn pruned_subtitle_rejects_a_changed_source_path() {
        let fixture = fixture().await;
        new_package_with_source_snapshot(
            &fixture,
            "changed-path",
            "native",
            Some(4),
            fixture.file.id,
            (
                fixture
                    .file
                    .path
                    .with_file_name("movie-before-rescan.mkv")
                    .to_string_lossy()
                    .into_owned(),
                fixture.file.size,
                fixture.file.mtime,
            ),
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "changed-path").await;

        assert_changed_subtitle_source(&fixture, "changed-path", token, 4).await;
    }

    #[tokio::test]
    async fn pruned_subtitle_rejects_a_changed_source_size() {
        let fixture = fixture().await;
        new_package_with_source_snapshot(
            &fixture,
            "changed-size",
            "native",
            Some(4),
            fixture.file.id,
            (
                fixture.file.path.to_string_lossy().into_owned(),
                fixture.file.size + 1,
                fixture.file.mtime,
            ),
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "changed-size").await;

        assert_changed_subtitle_source(&fixture, "changed-size", token, 4).await;
    }

    #[tokio::test]
    async fn pruned_subtitle_rejects_a_changed_source_mtime() {
        let fixture = fixture().await;
        new_package_with_source_snapshot(
            &fixture,
            "changed-mtime",
            "native",
            Some(4),
            fixture.file.id,
            (
                fixture.file.path.to_string_lossy().into_owned(),
                fixture.file.size,
                fixture.file.mtime + 1,
            ),
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "changed-mtime").await;

        assert_changed_subtitle_source(&fixture, "changed-mtime", token, 4).await;
    }

    #[tokio::test]
    async fn pruned_subtitle_rejects_a_removed_source_stream() {
        let fixture = fixture().await;
        new_package(
            &fixture,
            "removed-stream",
            "native",
            Some(4),
            fixture.file.id,
        )
        .await;
        replace_subtitle_streams(
            &fixture,
            vec![SubtitleStream {
                index: 5,
                codec: "subrip".into(),
                language: Some("eng".into()),
                ..Default::default()
            }],
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "removed-stream").await;

        assert_changed_subtitle_source(&fixture, "removed-stream", token, 4).await;
    }

    #[tokio::test]
    async fn pruned_subtitle_rejects_a_source_stream_that_is_no_longer_native_text() {
        let fixture = fixture().await;
        new_package(
            &fixture,
            "changed-stream-codec",
            "native",
            Some(4),
            fixture.file.id,
        )
        .await;
        replace_subtitle_streams(
            &fixture,
            vec![SubtitleStream {
                index: 4,
                codec: "ass".into(),
                language: Some("eng".into()),
                ..Default::default()
            }],
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "changed-stream-codec").await;

        assert_changed_subtitle_source(&fixture, "changed-stream-codec", token, 4).await;
    }

    #[tokio::test]
    async fn pruned_subtitle_recovers_from_an_unchanged_source() {
        crate::transcode::require_ffmpeg();
        let mut fixture = fixture().await;
        let subtitle_input = fixture.file.path.with_extension("srt");
        tokio::fs::write(
            &subtitle_input,
            b"1\n00:00:00,000 --> 00:00:01,000\nRecovered offline subtitle\n",
        )
        .await
        .expect("subtitle fixture");
        let output = tokio::process::Command::new(crate::ffmpeg::ffmpeg_bin())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "srt",
                "-i",
            ])
            .arg(&subtitle_input)
            .args(["-c:s", "srt"])
            .arg(&fixture.file.path)
            .output()
            .await
            .expect("build subtitle source");
        assert!(
            output.status.success(),
            "building subtitle source failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata = tokio::fs::metadata(&fixture.file.path)
            .await
            .expect("source metadata");
        let source_size = i64::try_from(metadata.len()).expect("source size");
        let source_mtime = i64::try_from(
            metadata
                .modified()
                .expect("source mtime")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("source after epoch")
                .as_secs(),
        )
        .expect("mtime fits i64");
        let file_id = fixture
            .state
            .store
            .upsert_file(
                fixture.file.item_id,
                &fixture.file.path.to_string_lossy(),
                source_size,
                source_mtime,
                &ProbeResult {
                    duration_ms: Some(1_000),
                    container: Some("matroska".into()),
                    subtitle_streams: vec![SubtitleStream {
                        index: 0,
                        codec: "subrip".into(),
                        language: Some("eng".into()),
                        default: true,
                        ..Default::default()
                    }],
                    raw_json: Some("{}".into()),
                    ..Default::default()
                },
            )
            .await
            .expect("record recoverable source");
        assert_eq!(file_id, fixture.file.id);
        fixture.file = fixture
            .state
            .store
            .get_file(file_id)
            .await
            .expect("file lookup")
            .expect("file");
        new_package(
            &fixture,
            "unchanged-source",
            "native",
            Some(0),
            fixture.file.id,
        )
        .await;
        let token = ready_subtitle_lease(&fixture, "unchanged-source").await;
        let sidecar = crate::subtitles::vtt_path_for_identity(
            &fixture.state.subs_dir,
            fixture.file.id,
            0,
            fixture.file.size,
            fixture.file.mtime,
        );
        assert!(
            !tokio::fs::try_exists(&sidecar)
                .await
                .expect("sidecar lookup"),
            "the happy path must begin with a pruned sidecar"
        );

        let response = subtitle(
            State(fixture.state.clone()),
            AxPath((token, 0, "seg00000.vtt".into())),
        )
        .await
        .expect("recover subtitle");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("subtitle response")
            .to_bytes();
        assert!(
            String::from_utf8_lossy(&body).contains("Recovered offline subtitle"),
            "recovered response did not contain the source subtitle"
        );
        assert!(
            tokio::fs::try_exists(&sidecar)
                .await
                .expect("sidecar lookup"),
            "unchanged-source recovery did not republish the pruned sidecar"
        );
    }

    #[tokio::test]
    async fn missing_subtitle_source_is_a_distinct_typed_gone_response() {
        let fixture = fixture().await;
        new_package(&fixture, "missing-source", "native", Some(4), 999_999).await;
        let token = ready_subtitle_lease(&fixture, "missing-source").await;

        let (code, body) = error_response(
            subtitle(
                State(fixture.state.clone()),
                AxPath((token, 4, "seg00000.vtt".into())),
            )
            .await
            .expect_err("missing source"),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(body["code"], "source_changed");
        assert_eq!(
            body["message"],
            "The source for this offline subtitle is no longer available."
        );
    }
}
