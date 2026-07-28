//! HLS transcode endpoints. `start` creates a session (authenticated) and
//! returns the playlist URL; the playlist and segments are then fetched by
//! whatever HLS player the session ends up in.
//!
//! Playlist/segment requests authenticate by *capability*: the session id is
//! a v4 UUID (122 random bits) minted for an authenticated user, unguessable,
//! and short-lived (reaped on idle). No header requirement means dumb
//! fetchers can play the stream — Safari's native HLS, and crucially an
//! Apple TV during AirPlay, which fetches the URL itself with no way to
//! attach our bearer token. Same model Plex uses; also what Phase 4 wants,
//! since any cluster node can serve a session id without seeing the login.

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct StartQuery {
    /// Target height (e.g. 1080, 720). Defaults to 1080. Ignored when `copy=1`.
    pub height: Option<i64>,
    /// Start offset in seconds (resume / seek).
    pub start: Option<f64>,
    /// Audio stream to use (`a:{audio}`); overrides the automatic pick.
    pub audio: Option<i64>,
    /// `copy=1` → a copy-video HLS session (repackage the source video into
    /// fMP4 HLS untouched, transcode audio only). For players that can't take a
    /// progressive fMP4 remux but decode HEVC/HDR natively via HLS (Safari).
    pub copy: Option<u8>,
    /// With `copy`: `aac=1` transcodes the audio to AAC (the codec the client
    /// can't take), `aac=0` copies it. The client knows which from `/decision`.
    pub aac: Option<u8>,
}

#[derive(Serialize)]
pub struct StartResponse {
    pub session_id: String,
    pub playlist_url: String,
    pub duration_ms: Option<i64>,
    pub start_seconds: f64,
    pub encoder: String,
}

/// Everything a client must say to open a stream.
///
/// A body rather than a query string, and a POST rather than a GET, because
/// this call spawns a process and kills its predecessor. A GET that does that
/// is a trap: GET is idempotent by definition, so anything entitled to replay
/// one — a retry, a prefetch, an intermediary — could spawn a second encoder
/// and orphan the first.
#[derive(Deserialize)]
pub struct CreateSession {
    /// Stable for one player instance. Supersession is keyed by it, so two
    /// devices on one account no longer kill each other's streams.
    pub playback_id: String,
    /// Optional idempotency key for this one attempt.
    pub request_id: Option<String>,
    /// Target height for a transcode. Ignored when `copy` is set.
    pub height: Option<i64>,
    pub start: Option<f64>,
    pub audio: Option<i64>,
    /// Copy the source video into HLS rather than re-encoding it.
    pub copy: Option<bool>,
    /// With `copy`: re-encode the audio the client can't take.
    pub aac: Option<bool>,
}

impl CreateSession {
    fn into_request(self, file_id: i64) -> crate::transcode::SessionRequest {
        use crate::transcode::SessionKind;
        let kind = if self.copy == Some(true) {
            SessionKind::Copy {
                aac: self.aac == Some(true),
            }
        } else {
            SessionKind::Transcode {
                height: self.height.unwrap_or(1080).clamp(144, 2160),
            }
        };
        crate::transcode::SessionRequest {
            file_id,
            playback_id: self.playback_id,
            request_id: self.request_id,
            kind,
            start_seconds: self.start.unwrap_or(0.0).max(0.0),
            audio_index: self.audio.filter(|a| *a >= 0),
        }
    }
}

/// POST /api/v1/files/:id/hls/sessions — create a stream, or recover the one
/// an identical request already created.
pub async fn create(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<CreateSession>,
) -> Result<Json<StartResponse>, ApiError> {
    if req.playback_id.trim().is_empty() {
        return Err(ApiError::BadRequest("playback_id is required".into()));
    }
    let request = req.into_request(id);
    let info = state
        .transcode
        .create_session(&request, &user.username)
        .await
        // A reused request id asking for a different stream is the client's
        // mistake, not the server's; say which it is.
        .map_err(|e| {
            if e.contains("already used") {
                ApiError::Conflict(e)
            } else {
                ApiError::Internal(e)
            }
        })?;
    Ok(Json(StartResponse {
        session_id: info.session_id,
        playlist_url: info.playlist_url,
        duration_ms: info.duration_ms,
        start_seconds: info.start_seconds,
        encoder: info.encoder.to_owned(),
    }))
}

/// DELETE /api/v1/hls/:session — the player is done with this stream.
///
/// Capability auth, like the playlist and segment routes: the session id *is*
/// the credential, and requiring a header would stop the browser sending this
/// with `keepalive` as a tab closes — which is the whole point. Without it a
/// finished stream keeps its encoder for the 60-second idle timeout plus a
/// reaper tick, holding a hardware slot nobody is watching.
///
/// Idempotent: deleting a session that has already gone is a success, because
/// the caller's intent ("this must not be running") is satisfied either way.
pub async fn delete(State(state): State<AppState>, AxPath(session): AxPath<String>) -> StatusCode {
    state.transcode.stop_session(&session).await;
    StatusCode::NO_CONTENT
}

/// GET /api/v1/files/:id/hls/start — **deprecated**; use `POST …/hls/sessions`.
///
/// Kept as a bridge for clients that predate the POST route, and implemented
/// over the same creation path so it cannot bypass identity or cleanup. Its
/// one concession: with no `playback_id` to key supersession by, it
/// synthesises the old (viewer, file) key, so its behaviour is exactly what it
/// was — including the two-devices-one-account collision the new route fixes.
pub async fn start(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<StartQuery>,
) -> Result<Json<StartResponse>, ApiError> {
    let legacy = CreateSession {
        playback_id: format!("legacy:{}:{id}", user.username),
        request_id: None,
        height: q.height,
        start: q.start,
        audio: q.audio,
        copy: Some(q.copy == Some(1)),
        aac: Some(q.aac == Some(1)),
    };
    create(AuthUser(user), State(state), AxPath(id), Json(legacy)).await
}

/// GET /api/v1/hls/:session/status — how the session is actually doing.
///
/// Capability auth like its siblings (the session id is the credential), and
/// deliberately outside the `{segment}` route: a static path segment wins over
/// a parameter in the router, and `status` is not a valid segment name anyway.
///
/// This exists because the first question every buffering report asks —
/// *is the server keeping up?* — had no answer anywhere in the product. The
/// player puts `speed` and `ahead_seconds` in the stats overlay, so "why did
/// it stutter" resolves to a number instead of a theory.
pub async fn status(
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
) -> Result<Json<crate::transcode::SessionInfo>, ApiError> {
    state
        .transcode
        .session_status(&session)
        .await
        .map(Json)
        .ok_or(ApiError::NotFound("transcode session"))
}

/// GET /api/v1/hls/:session/index.m3u8 — capability auth (see module docs).
pub async fn playlist(
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
) -> Result<Response, ApiError> {
    let bytes = state
        .transcode
        .playlist(&session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl".to_owned(),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// GET /api/v1/hls/:session/:segment — capability auth (see module docs).
pub async fn segment(
    State(state): State<AppState>,
    AxPath((session, seg)): AxPath<(String, String)>,
) -> Result<Response, ApiError> {
    let opened = state
        .transcode
        .segment(&session, &seg)
        .await
        .ok_or(ApiError::NotFound("segment"))?;
    // MPEG-TS segments (transcode) vs fMP4 init/segments (copy-video path).
    let content_type = if seg.ends_with(".ts") {
        "video/mp2t"
    } else {
        "video/mp4"
    };
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (header::CONTENT_LENGTH, opened.len.to_string()),
            // A finished segment never changes: ffmpeg writes `.tmp` and
            // renames, and nothing rewrites the final name. The URI carries a
            // session id, so the bytes behind it are unique to this session and
            // safe to hold — `private` because that id is a capability, and
            // `immutable` so a reload or a retry costs nothing. (The playlist
            // stays `no-store`: it grows for the session's whole life.)
            (
                header::CACHE_CONTROL,
                "private, max-age=3600, immutable".to_owned(),
            ),
        ],
        // Streamed rather than buffered: a 4K copy segment is ~35 MB, and
        // reading it into memory before the first byte goes out is an
        // allocation and a copy per request for data on its way to a socket.
        Body::from_stream(tokio_util::io::ReaderStream::new(opened.file)),
    )
        .into_response())
}
