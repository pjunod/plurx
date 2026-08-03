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
use tokio::io::AsyncReadExt;

use plurx_core::domain::{MediaFile, SubtitleStream};
use plurx_core::playback::PlaybackMethod;
use plurx_core::tracks::is_native_text_subtitle;

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct StartQuery {
    /// Target height (e.g. 1080, 720). Omitted means Auto (server-chosen).
    /// Ignored when `copy=1`.
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
    /// The whole stream already exists on disk (a pre-transcode cache hit).
    /// The player treats it like direct play: seek by `currentTime`, and don't
    /// arm the stall watchdog's restart — see `StartInfo::vod`.
    pub vod: bool,
    /// The quality ladder for this source, top rung first — the rungs the
    /// menu and the Auto controller move between, so the client never
    /// hardcodes them (ADAPTIVE-QUALITY.md Phase 1).
    pub ladder: Vec<crate::transcode::Rung>,
    /// What dynamic range the bytes of *this session* carry
    /// (`"dolby_vision" | "hdr10" | "hlg" | "sdr"`). It overrides the
    /// decision's answer the moment the session attaches, because a burn or
    /// a manually-picked rung forces a transcode the decision never promised
    /// (MEDIA-BADGES-PLAN §3.2). Absent when the source file vanished from
    /// the store mid-request: the client keeps whatever it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_dynamic_range: Option<&'static str>,
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
    /// Target height for a transcode. Ignored when `copy` is set. Omitted
    /// means Auto, and Auto is the SERVER's decision: the rung depends on
    /// which encoder wins, and the player only learns that from the response
    /// to this request (see `TranscodeManager::auto_height`).
    pub height: Option<i64>,
    /// Subtitle stream to burn into the picture. Only for the ones a client
    /// cannot render itself — a bitmap track (PGS/VobSub) has no text to send,
    /// so the only way to show it is to draw it into the frames.
    pub subtitle_burn: Option<i64>,
    /// Advertise the source's lossless WebVTT-convertible tracks in an HLS
    /// master playlist. Native Apple clients opt in; older HLS consumers keep
    /// receiving the media playlist shape they already understand.
    pub native_subtitles: Option<bool>,
    /// Initially selected native rendition. This changes only HLS metadata,
    /// never the video recipe or the encoder choice.
    pub subtitle: Option<i64>,
    pub start: Option<f64>,
    pub audio: Option<i64>,
    /// Copy the source video into HLS rather than re-encoding it.
    pub copy: Option<bool>,
    /// With `copy`: re-encode the audio the client can't take.
    pub aac: Option<bool>,
    /// With `copy`: retain Dolby Vision signaling and dynamic metadata because
    /// the decision established that this client supports the source profile.
    pub preserve_dolby_vision: Option<bool>,
    /// Manual A/V correction for this playback attempt only. Positive delays
    /// audio. It is carried into every seek/reopen by the client and is never
    /// written back to the media file.
    pub audio_offset_ms: Option<i64>,
}

impl CreateSession {
    /// `height` is fully resolved by the caller — Auto answered, explicit
    /// rungs snapped, the source-height promise honored — because resolution
    /// needs the store and the encoder choice, and because it must be settled
    /// before the request is fingerprinted, or two identical requests could
    /// not recognise each other.
    fn into_request(self, file_id: i64, height: i64) -> crate::transcode::SessionRequest {
        use crate::transcode::SessionKind;
        let kind = if self.copy == Some(true) {
            SessionKind::Copy {
                aac: self.aac == Some(true),
                preserve_dolby_vision: self.preserve_dolby_vision == Some(true),
            }
        } else {
            SessionKind::Transcode { height }
        };
        crate::transcode::SessionRequest {
            file_id,
            playback_id: self.playback_id,
            request_id: self.request_id,
            kind,
            start_seconds: self.start.unwrap_or(0.0).max(0.0),
            audio_index: self.audio.filter(|a| *a >= 0),
            subtitle_burn: self.subtitle_burn.filter(|s| *s >= 0),
            audio_offset_ms: self.audio_offset_ms.unwrap_or(0).clamp(-15_000, 15_000),
        }
    }
}

/// The dynamic range this session puts on the wire, read off the session it
/// actually built rather than the decision that suggested it — a burn or a
/// forced rung produces a transcode `/decision` never promised, and the
/// badge has to follow the session (MEDIA-BADGES-PLAN §3.2).
///
/// `None` only when the source row could not be loaded; there is nothing
/// honest to say about a file we cannot see.
fn session_delivered_dynamic_range(
    source: Option<&MediaFile>,
    kind: &crate::transcode::SessionKind,
) -> Option<&'static str> {
    use crate::transcode::SessionKind;
    let file = source?;
    // One helper for both wire fields, so the decision and the session can
    // never disagree about the same delivery.
    let (method, preserve) = match kind {
        SessionKind::Copy {
            preserve_dolby_vision,
            ..
        } => (PlaybackMethod::Remux, *preserve_dolby_vision),
        SessionKind::Transcode { .. } => (PlaybackMethod::Transcode, false),
    };
    Some(plurx_core::playback::delivered_dynamic_range(
        file, method, preserve,
    ))
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
    // The source height answers three things now: Auto, the ladder in the
    // response, and the snap's source-height escape. One read, from the read
    // pool.
    let source = state.store.get_file(id).await?;
    let source_height = source.as_ref().and_then(|f| f.height);
    let height = match req.height {
        // Auto: the server's own choice already lands where it means to —
        // snapping it would re-decide policy (a 900p source deliberately
        // transcodes at 900: no scaler in the chain at all).
        None => state.transcode.auto_height(source_height).await,
        // The source's own height is the Original/forced-burn promise
        // (see the player's sessionHeight): never snapped, never downgraded.
        Some(h) if Some(h) == source_height => h,
        // An explicit rung from a menu: snap strays onto the ladder;
        // above-ladder heights pass through as what they are.
        Some(h) => crate::transcode::snap_height(h),
    }
    .clamp(crate::transcode::MIN_HEIGHT, crate::transcode::MAX_HEIGHT);
    let native_subtitles = req.native_subtitles == Some(true);
    let native_subtitle = req.subtitle.filter(|s| *s >= 0);
    if native_subtitles {
        if let Some(index) = native_subtitle {
            let track = source
                .as_ref()
                .and_then(|f| f.subtitle_streams.get(index as usize))
                .ok_or_else(|| ApiError::BadRequest("unknown native subtitle track".into()))?;
            if !is_native_text_subtitle(&track.codec) {
                return Err(ApiError::BadRequest(
                    "the selected subtitle requires burn-in".into(),
                ));
            }
        }
    }
    let request = req.into_request(id, height);
    let delivered = session_delivered_dynamic_range(source.as_ref(), &request.kind);
    // A session being created is playback beginning — the honest moment for
    // the scrobble that used to fire from `/decision`. The method is the one
    // this request settled on rather than one read back off an encoder label:
    // `encoder` says "cached" on a cache hit and changes under a
    // hardware→software fallback, so a copy-remux inferred from it could
    // report itself as a transcode.
    let method = match request.kind {
        crate::transcode::SessionKind::Copy { .. } => crate::delivery::Method::HlsCopy,
        crate::transcode::SessionKind::Transcode { .. } => crate::delivery::Method::Transcode,
    };
    crate::playstart::note_playback_started(
        &state,
        user.id,
        &user.username,
        id,
        method,
        Some(&request.playback_id),
    );
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
                tracing::warn!(file = id, "session create failed: {e}");
                ApiError::Internal(e)
            }
        })?;
    let playlist_url = if native_subtitles {
        // Give the multivariant playlist its own path. AVPlayer caches HLS
        // resources by URL and can otherwise conflate `index.m3u8?native=1`
        // with the child `index.m3u8` media playlist referenced by that
        // master. Keep the query form in `playlist` for clients holding an
        // older session response, but new sessions use the unambiguous path.
        let master = format!("/api/v1/hls/{}/master.m3u8", info.session_id);
        match native_subtitle {
            Some(index) => format!("{master}?subtitle={index}"),
            None => master,
        }
    } else {
        info.playlist_url
    };
    Ok(Json(StartResponse {
        session_id: info.session_id,
        playlist_url,
        duration_ms: info.duration_ms,
        start_seconds: info.start_seconds,
        encoder: info.encoder.to_owned(),
        vod: info.vod,
        ladder: crate::transcode::ladder(source_height),
        delivered_dynamic_range: delivered,
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
    state
        .transcode
        .stop_session(&session, "released by client")
        .await;
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
        subtitle_burn: None, // the deprecated GET bridge never offered a burn
        native_subtitles: None,
        subtitle: None,
        start: q.start,
        audio: q.audio,
        copy: Some(q.copy == Some(1)),
        aac: Some(q.aac == Some(1)),
        preserve_dolby_vision: Some(false),
        audio_offset_ms: None,
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

#[derive(Default, Deserialize)]
pub struct PlaylistQuery {
    pub native: Option<u8>,
    pub subtitle: Option<i64>,
    /// Temporary physical-device isolation mode for the Apple HDR master.
    /// The session UUID remains the capability; these modes only remove
    /// declarations from the playlist and never expose another resource.
    pub diagnostic: Option<String>,
}

fn playlist_response(bytes: Vec<u8>) -> Response {
    (
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
        .into_response()
}

async fn session_file(
    state: &AppState,
    session: &str,
) -> Result<(crate::transcode::HlsContext, MediaFile), ApiError> {
    let context = state
        .transcode
        .hls_context(session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    let file = state
        .store
        .get_file(context.file_id)
        .await?
        .ok_or(ApiError::NotFound("file"))?;
    Ok((context, file))
}

/// GET /api/v1/hls/:session/index.m3u8 — capability auth (see module docs).
///
/// Existing clients receive the historical media playlist. The `?native=1`
/// form remains as a compatibility bridge for Apple sessions created before
/// the dedicated master-playlist path existed.
pub async fn playlist(
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
    Query(query): Query<PlaylistQuery>,
) -> Result<Response, ApiError> {
    if query.native != Some(1) {
        return video_playlist(State(state), AxPath(session)).await;
    }
    let (context, file) = session_file(&state, &session).await?;
    let context = exact_hls_context(&state, &session, context).await;
    Ok(playlist_response(
        master_playlist(&file, query.subtitle, &context).into_bytes(),
    ))
}

/// The multivariant playlist used by Apple clients for native subtitles and
/// HDR variant signaling.
///
/// It has a distinct path from the child media playlist so AVPlayer cannot
/// collapse the two resources when applying URL-query cache normalization.
pub async fn master_playlist_response(
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
    Query(query): Query<PlaylistQuery>,
) -> Result<Response, ApiError> {
    let (context, file) = session_file(&state, &session).await?;
    let context = exact_hls_context(&state, &session, context).await;
    Ok(playlist_response(
        master_playlist_diagnostic(&file, query.subtitle, &context, query.diagnostic.as_deref())
            .into_bytes(),
    ))
}

/// The video rendition referenced by the native-subtitle HLS master.
pub async fn video_playlist(
    State(state): State<AppState>,
    AxPath(session): AxPath<String>,
) -> Result<Response, ApiError> {
    let bytes = state
        .transcode
        .playlist(&session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    Ok(playlist_response(bytes))
}

/// One native WebVTT rendition's media playlist. Its segments mirror the
/// video rendition so AVPlayer sees matching playlist types and timelines.
/// Every child resource is still cut from the one cached sidecar.
pub async fn subtitle_playlist(
    State(state): State<AppState>,
    AxPath((session, index)): AxPath<(String, i64)>,
) -> Result<Response, ApiError> {
    let (_, file) = session_file(&state, &session).await?;
    let track = file
        .subtitle_streams
        .get(index as usize)
        .ok_or(ApiError::NotFound("subtitle track"))?;
    if !is_native_text_subtitle(&track.codec) {
        return Err(ApiError::BadRequest(
            "this subtitle requires burn-in".into(),
        ));
    }
    let video = state
        .transcode
        .playlist(&session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    Ok(playlist_response(
        subtitle_media_playlist(&video).into_bytes(),
    ))
}

/// Capability-authenticated VTT data for AVPlayer's autonomous child fetch.
/// Each resource mirrors one video segment's time window. Cues are shifted
/// onto the session-relative video timeline, so a session opened at a
/// resume/seek offset still presents captions at the right frame.
pub async fn subtitle_vtt(
    State(state): State<AppState>,
    AxPath((session, index, segment)): AxPath<(String, i64, String)>,
) -> Result<Response, ApiError> {
    let (context, file) = session_file(&state, &session).await?;
    let track = file
        .subtitle_streams
        .get(index as usize)
        .ok_or(ApiError::NotFound("subtitle track"))?;
    if !is_native_text_subtitle(&track.codec) {
        return Err(ApiError::BadRequest(
            "this subtitle requires burn-in".into(),
        ));
    }
    let sequence = segment
        .strip_prefix("seg")
        .and_then(|value| value.strip_suffix(".vtt"))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(ApiError::NotFound("subtitle segment"))?;
    let video = state
        .transcode
        .playlist(&session)
        .await
        .ok_or(ApiError::NotFound("transcode session"))?;
    let timeline = subtitle_timeline(&video);
    let window = timeline
        .segments
        .iter()
        .find(|window| window.sequence == sequence)
        .ok_or(ApiError::NotFound("subtitle segment"))?;
    let cached = crate::subtitles::ensure_vtt(&state.subs_dir, &file, index)
        .await
        .map_err(|why| {
            tracing::warn!(
                file_id = file.id,
                index,
                "subtitle extraction failed: {why}"
            );
            ApiError::Internal("subtitle extraction failed".into())
        })?;
    let bytes = tokio::fs::read(cached)
        .await
        .map_err(|e| ApiError::Internal(format!("reading extracted subtitles: {e}")))?;
    tracing::info!(
        session_id = %session,
        file_id = file.id,
        index,
        codec = %track.codec,
        language = track.language.as_deref().unwrap_or("und"),
        title = track.title.as_deref().unwrap_or(""),
        start_seconds = context.start_seconds,
        "serving native HLS WebVTT subtitle"
    );
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        // The session's MEDIA origin, not the offset that was requested. A
        // copy session seeks with `-noaccurate_seek`, so its timeline begins
        // at the keyframe before the requested start — shifting cues by the
        // request made them lead the picture by up to a whole GOP (1–6 s on a
        // 4K film) on every resumed or seeked copy session, which is the
        // flagship Apple path.
        slice_webvtt(
            &bytes,
            context.media_origin_seconds,
            window.start_seconds,
            window.end_seconds,
        ),
    )
        .into_response())
}

fn quoted(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '"' => '\'',
            '\r' | '\n' => ' ',
            other => other,
        })
        .collect()
}

/// BCP-47 for the `LANGUAGE=` attribute. One line, because the knowledge of
/// which spellings mean the same language belongs to `plurx_core::tracks` —
/// this module used to keep a ten-language copy of it, which silently passed
/// "dut"/"cze"/"gre" through as non-BCP-47 and defeated viewer-language
/// matching for every language the copy had not learned.
fn language_tag(raw: Option<&str>) -> &str {
    plurx_core::tracks::bcp47_tag(raw)
}

/// Replace the scanner's best-effort HEVC tier with the exact declaration
/// carried by the HLS initialization segment.
///
/// ffprobe records Main/Main10 and level in the library row but commonly
/// omits the tier. Guessing Main tier then advertises `L150` for a High-tier
/// `hvcC` record (`H150`), and AVPlayer treats the only HDR variant as
/// unsupported. The init segment is the canonical description of the bytes
/// this session actually publishes, including any Dolby Vision stripping.
async fn exact_hls_context(
    state: &AppState,
    session: &str,
    mut context: crate::transcode::HlsContext,
) -> crate::transcode::HlsContext {
    let fallback_video = context.codecs.split(',').next().unwrap_or_default();
    let Some(sample_entry) = ["hvc1", "hev1", "dvh1", "dvhe"]
        .into_iter()
        .find(|entry| fallback_video.starts_with(entry))
    else {
        return context;
    };
    let Some(opened) = state.transcode.segment(session, "init.mp4").await else {
        return context;
    };
    // Initialization segments are a few KiB. Bound malformed input so a
    // playlist request can never allocate without limit.
    let mut init = Vec::with_capacity(opened.len.min(64 * 1024) as usize);
    let mut reader = opened.file.take(1024 * 1024);
    if reader.read_to_end(&mut init).await.is_err() {
        return context;
    }
    let Some(video) = hevc_codec_from_init(&init, sample_entry) else {
        return context;
    };
    context.codecs = match context.codecs.split_once(',') {
        Some((_, audio)) if !audio.trim().is_empty() => format!("{video},{}", audio.trim()),
        _ => video,
    };
    context
}

/// RFC 6381 identifier from the HEVCDecoderConfigurationRecord in `hvcC`.
fn hevc_codec_from_init(init: &[u8], sample_entry: &str) -> Option<String> {
    let type_at = init.windows(4).position(|window| window == b"hvcC")?;
    let box_start = type_at.checked_sub(4)?;
    let box_size = u32::from_be_bytes(init.get(box_start..type_at)?.try_into().ok()?) as usize;
    let box_end = box_start.checked_add(box_size)?;
    if box_size < 21 || box_end > init.len() {
        return None;
    }
    let payload = init.get(type_at + 4..box_end)?;
    if payload.len() < 13 || payload[0] != 1 {
        return None;
    }

    let profile_byte = payload[1];
    let profile_space = match profile_byte >> 6 {
        0 => "",
        1 => "A",
        2 => "B",
        3 => "C",
        _ => return None,
    };
    let profile = profile_byte & 0x1f;
    let tier = if profile_byte & 0x20 == 0 { 'L' } else { 'H' };
    let compatibility = u32::from_be_bytes(payload.get(2..6)?.try_into().ok()?).reverse_bits();
    let level = payload[12];
    let constraints = payload.get(6..12)?;
    let constraints_len = constraints
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    let constraints = constraints[..constraints_len]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();

    let mut codec =
        format!("{sample_entry}.{profile_space}{profile}.{compatibility:X}.{tier}{level}");
    if !constraints.is_empty() {
        codec.push('.');
        codec.push_str(&constraints);
    }
    Some(codec)
}

/// The human half of a rendition's `NAME`. Display names are a presentation
/// concern, so they live here rather than in core — but the set is kept in
/// step with the alias table `language_tag` reads, so a language that matches
/// never renders as a bare three-letter code.
fn language_name(raw: Option<&str>) -> &str {
    match language_tag(raw) {
        "en" => "English",
        "it" => "Italian",
        "ja" => "Japanese",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "ru" => "Russian",
        "hi" => "Hindi",
        "ar" => "Arabic",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "pl" => "Polish",
        "no" => "Norwegian",
        "da" => "Danish",
        "fi" => "Finnish",
        "tr" => "Turkish",
        "th" => "Thai",
        "vi" => "Vietnamese",
        "uk" => "Ukrainian",
        "cs" => "Czech",
        "el" => "Greek",
        "he" => "Hebrew",
        "hu" => "Hungarian",
        "ro" => "Romanian",
        other => other,
    }
}

fn subtitle_name(track: &SubtitleStream, ordinal: usize) -> String {
    let language = language_name(track.language.as_deref());
    match track
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(title) if language != "und" => format!("{language} · {title}"),
        Some(title) => title.to_owned(),
        None if language != "und" => language.to_owned(),
        None => format!("Subtitle {}", ordinal + 1),
    }
}

/// Does a track *title* declare the track forced?
///
/// Title-based detection is contract, not cleanup fodder: file 5615's forced
/// Italian track carries `disposition.forced = false` and the title "Forced",
/// and that regression case is why this exists at all (handoff §3.4).
///
/// But a substring test is too eager. "Non-Forced" and "Unforced" are real
/// titles, and classifying them FORCED=YES hides an ordinary subtitle track
/// from Apple's subtitle menu entirely — a forced rendition is only offered
/// when the presentation language matches. So: match "forced" on word
/// boundaries, and reject it when the preceding word negates it. "Unforced"
/// falls out for free, because the `n` in front of it is not a boundary.
fn title_marks_forced(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut consumed = 0usize;
    while let Some(at) = rest.find("forced") {
        let start = consumed + at;
        let end = start + "forced".len();
        let before = lower[..start].chars().next_back();
        let after = lower[end..].chars().next();
        let bounded =
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric);
        if bounded && !negated_before(&lower[..start]) {
            return true;
        }
        consumed = end;
        rest = &lower[end..];
    }
    false
}

/// The word immediately before a "forced" occurrence, when it turns the claim
/// around. Separators are skipped, so "non-forced", "non forced" and
/// "not forced" are all caught by the same rule.
fn negated_before(prefix: &str) -> bool {
    let trimmed = prefix.trim_end_matches(|c: char| !c.is_alphanumeric());
    let word = trimmed
        .rsplit(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("");
    matches!(word, "non" | "not" | "no" | "never")
}

fn track_is_forced(track: &SubtitleStream) -> bool {
    track.forced || track.title.as_deref().is_some_and(title_marks_forced)
}

/// The Apple accessibility `CHARACTERISTICS` for an SDH / hard-of-hearing
/// rendition — the tag that lets a viewer who needs captions find them by
/// what they *do* rather than by what someone happened to name them.
///
/// The container's own `hearing_impaired` disposition answers first: it is
/// the authored fact, and it is right even for a track called "English 2".
/// The title sniff stays as the fallback, because a library probed before
/// plurx read that disposition has nothing else to go on and re-probing is
/// manual — so the naming convention keeps working exactly as it did.
fn subtitle_characteristics(track: &SubtitleStream) -> Option<&'static str> {
    const ACCESSIBILITY: &str =
        "public.accessibility.transcribes-spoken-dialog,public.accessibility.describes-music-and-sound";
    if track.hearing_impaired {
        return Some(ACCESSIBILITY);
    }
    let title = track.title.as_deref()?.to_ascii_lowercase();
    [
        "sdh",
        "closed caption",
        "closed-caption",
        "hard of hearing",
        "non udenti",
    ]
    .iter()
    .any(|marker| title.contains(marker))
    .then_some(ACCESSIBILITY)
}

/// Rendition `NAME`s, made unique.
///
/// RFC 8216 §4.3.4.1 makes NAME a MUST-unique quoted string within a group,
/// and two same-language untitled tracks otherwise both render as "English".
/// AVFoundation is entitled to merge them, and the client resolves options by
/// name — so a duplicate is not a cosmetic wart, it is the client selecting
/// the wrong track or none at all. Disambiguate by occurrence, leaving the
/// first one alone so the common single-track case reads naturally.
fn unique_subtitle_names(native: &[(usize, &SubtitleStream)]) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    native
        .iter()
        .map(|(index, track)| {
            let base = subtitle_name(track, *index);
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            match *count {
                1 => base,
                n => format!("{base} ({n})"),
            }
        })
        .collect()
}

/// Candidate master-playlist changes from the §5.4 ladder, each off by
/// default and enabled one at a time for one deploy.
///
/// Every master regression in this arc passed unit tests and failed on the
/// physical device, so the device is the only oracle and the ladder exists to
/// keep exactly one variable moving per deploy. These are compiled in but
/// inert until an operator sets the variable, which is what lets a rung be
/// tried, observed on Bedroom, and either kept or dropped without another
/// build. Once a rung is accepted, delete the flag and make it unconditional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct MasterRungs {
    /// `CLOSED-CAPTIONS=NONE` on the variant. Apple's authoring rules ask for
    /// it, and it also stops AVFoundation synthesising a phantom
    /// closed-caption option into the `.legible` group.
    closed_captions_none: bool,
    /// `AUTOSELECT=YES` on forced renditions, which Apple's authoring rules
    /// require and this master currently withholds when two forced tracks
    /// share a language.
    forced_autoselect: bool,
}

impl MasterRungs {
    fn enabled(name: &str) -> bool {
        std::env::var(name).is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }

    /// Read once: an operator sets these before plurxd starts, and a master
    /// that could change shape between two fetches of the same session is a
    /// worse problem than any rung solves.
    fn active() -> Self {
        static ACTIVE: std::sync::OnceLock<MasterRungs> = std::sync::OnceLock::new();
        *ACTIVE.get_or_init(|| MasterRungs {
            closed_captions_none: Self::enabled("PLURX_HLS_CLOSED_CAPTIONS_NONE"),
            forced_autoselect: Self::enabled("PLURX_HLS_FORCED_AUTOSELECT"),
        })
    }
}

fn master_playlist(
    file: &MediaFile,
    selected: Option<i64>,
    context: &crate::transcode::HlsContext,
) -> String {
    master_playlist_with_shape(
        file,
        selected,
        context,
        MasterRungs::active(),
        MasterShape::default(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MasterShape {
    subtitles: bool,
    codecs: bool,
    video_range: bool,
}

impl Default for MasterShape {
    fn default() -> Self {
        Self {
            subtitles: true,
            codecs: true,
            video_range: true,
        }
    }
}

fn master_playlist_diagnostic(
    file: &MediaFile,
    selected: Option<i64>,
    context: &crate::transcode::HlsContext,
    diagnostic: Option<&str>,
) -> String {
    let shape = match diagnostic {
        Some("video-only") => MasterShape {
            subtitles: false,
            codecs: false,
            video_range: false,
        },
        Some("video-only-codecs") => MasterShape {
            subtitles: false,
            codecs: true,
            video_range: false,
        },
        Some("video-only-range") => MasterShape {
            subtitles: false,
            codecs: false,
            video_range: true,
        },
        Some("video-only-hdr") => MasterShape {
            subtitles: false,
            codecs: true,
            video_range: true,
        },
        _ => MasterShape::default(),
    };
    master_playlist_with_shape(file, selected, context, MasterRungs::active(), shape)
}

#[cfg(test)]
fn master_playlist_with(
    file: &MediaFile,
    selected: Option<i64>,
    context: &crate::transcode::HlsContext,
    rungs: MasterRungs,
) -> String {
    master_playlist_with_shape(file, selected, context, rungs, MasterShape::default())
}

fn master_playlist_with_shape(
    file: &MediaFile,
    selected: Option<i64>,
    context: &crate::transcode::HlsContext,
    rungs: MasterRungs,
    shape: MasterShape,
) -> String {
    let native: Vec<(usize, &SubtitleStream)> = file
        .subtitle_streams
        .iter()
        .enumerate()
        .filter(|(_, track)| is_native_text_subtitle(&track.codec))
        .collect();
    let names = unique_subtitle_names(&native);
    // Copy/remux sessions can contain open GOPs, so the video rendition does
    // not promise independently decodable segments. The master must not make
    // that stronger claim on its behalf: AVPlayer acts on it at a resume
    // boundary and can reject an otherwise playable copied HEVC/DV stream.
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    for (ordinal, (index, track)) in native.iter().enumerate() {
        if !shape.subtitles {
            break;
        }
        // The query describes this player's selection. No selected index is
        // an explicit Off, not permission to resurrect a foreign-language
        // container default behind the client's back.
        let forced = track_is_forced(track);
        // A forced rendition is a narrow set of cues the player may use when
        // its language matches the presentation. Apple's authoring examples
        // deliberately keep those renditions DEFAULT=NO: DEFAULT=YES means
        // "play this whole selection absent any user choice", which is not
        // the same thing as FORCED=YES and makes AVPlayer reject or repeatedly
        // reload some subtitle groups. The Apple client explicitly selects
        // its preferred media option once the item exposes the legible group.
        let default = !forced && selected.is_some_and(|pick| pick == *index as i64);
        let characteristics = subtitle_characteristics(track);
        // RFC 8216 requires every AUTOSELECT=YES member of a rendition group
        // to have a unique LANGUAGE/ASSOC-LANGUAGE/FORCED/CHARACTERISTICS
        // combination. AVPlayer rejects the entire master when two ordinary
        // tracks share that tuple, so exact duplicates stay manually
        // selectable. DEFAULT=YES itself requires AUTOSELECT=YES.
        let tuple_copies = native
            .iter()
            .filter(|(_, other)| {
                language_tag(other.language.as_deref()) == language_tag(track.language.as_deref())
                    && track_is_forced(other) == forced
                    && subtitle_characteristics(other) == characteristics
            })
            .count();
        // Ladder rung (P2-11): Apple's authoring rules say a forced rendition
        // is AUTOSELECT=YES — the player is meant to reach it on its own when
        // the presentation language matches. Today two forced tracks sharing
        // a language both come out AUTOSELECT=NO, because RFC 8216's
        // uniqueness rule is written in terms of AUTOSELECT=YES members and
        // AVPlayer has been observed rejecting a whole master over it. The
        // two rules genuinely conflict; only the device settles which one it
        // enforces, so this is a rung and not a fix.
        let autoselect = default || tuple_copies == 1 || (rungs.forced_autoselect && forced);
        let characteristics = characteristics
            .map(|value| format!(",CHARACTERISTICS=\"{value}\""))
            .unwrap_or_default();
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",LANGUAGE=\"{}\",DEFAULT={},AUTOSELECT={},FORCED={}{},URI=\"subs/{index}/index.m3u8\"\n",
            quoted(&names[ordinal]),
            quoted(language_tag(track.language.as_deref())),
            if default { "YES" } else { "NO" },
            if autoselect { "YES" } else { "NO" },
            if forced { "YES" } else { "NO" },
            characteristics,
        ));
    }
    let bandwidth = file.bitrate.unwrap_or(25_000_000).max(128_000);
    out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}"));
    // HDR is not self-describing at HLS's variant-selection layer. Apple
    // requires VIDEO-RANGE before it opens the HEVC init segment, and CODECS
    // must name the exact Main10 profile/level carried by this session. The
    // source flag alone is not sufficient: an H.264 session from the same HDR
    // source has already been tone-mapped, so its master must remain SDR.
    let video_codec = context.codecs.split(',').next().unwrap_or_default();
    let hevc = ["hvc1", "hev1", "dvh1", "dvhe"]
        .iter()
        .any(|prefix| video_codec.starts_with(prefix));
    let video_range = if hevc {
        match file.hdr.as_deref() {
            Some("dolby_vision" | "hdr10" | "hdr10_plus") => Some("PQ"),
            Some("hlg") => Some("HLG"),
            _ => None,
        }
    } else {
        None
    };
    if let Some(video_range) = video_range {
        if shape.video_range {
            out.push_str(&format!(",VIDEO-RANGE={video_range}"));
        }
        if shape.codecs {
            out.push_str(&format!(",CODECS=\"{}\"", quoted(&context.codecs)));
        }
        if shape.codecs {
            if let Some(supplemental) = context.supplemental_codecs.as_deref() {
                out.push_str(&format!(
                    ",SUPPLEMENTAL-CODECS=\"{}\"",
                    quoted(supplemental)
                ));
            }
        }
    }
    // Ladder rung: the variant carries no CLOSED-CAPTIONS attribute, and
    // Apple's authoring rules say a variant with no captions must say so.
    // Absent it, AVFoundation is entitled to synthesise a phantom
    // closed-caption option into the `.legible` group — which shifts every
    // option ordinal underneath it. Correct HLS authoring, and untested on
    // the device, which is exactly what a rung is.
    if rungs.closed_captions_none {
        out.push_str(",CLOSED-CAPTIONS=NONE");
    }
    if shape.subtitles && !native.is_empty() {
        out.push_str(",SUBTITLES=\"subs\"");
    }
    out.push('\n');
    // Resolve to the historical media-playlist URL. The master itself lives
    // at `master.m3u8`, so this child path is unambiguous without relying on
    // query-string distinctions in AVPlayer's HLS resource cache.
    out.push_str("index.m3u8\n");
    out
}

#[derive(Debug, Clone, PartialEq)]
struct SubtitleWindow {
    sequence: u64,
    start_seconds: f64,
    end_seconds: f64,
    duration: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct SubtitleTimeline {
    target_duration: u64,
    media_sequence: u64,
    playlist_type: Option<String>,
    endlist: bool,
    segments: Vec<SubtitleWindow>,
}

fn subtitle_timeline(video_playlist: &[u8]) -> SubtitleTimeline {
    let text = String::from_utf8_lossy(video_playlist);
    let mut target_duration = 1;
    let mut media_sequence = 0;
    let mut playlist_type = None;
    let mut pending_duration = None;
    let mut durations = Vec::new();
    let mut endlist = false;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = value.parse().unwrap_or(1).max(1);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            playlist_type = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            pending_duration = value
                .split_once(',')
                .map_or(value, |(duration, _)| duration)
                .parse::<f64>()
                .ok();
        } else if line == "#EXT-X-ENDLIST" {
            endlist = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some(duration) = pending_duration.take().filter(|value| *value > 0.0) {
                durations.push(duration);
            }
        }
    }

    let mut start_seconds = 0.0;
    let segments = durations
        .into_iter()
        .enumerate()
        .map(|(ordinal, duration)| {
            let end_seconds = start_seconds + duration;
            let window = SubtitleWindow {
                sequence: media_sequence + ordinal as u64,
                start_seconds,
                end_seconds,
                duration,
            };
            start_seconds = end_seconds;
            window
        })
        .collect();
    SubtitleTimeline {
        target_duration,
        media_sequence,
        playlist_type,
        endlist,
        segments,
    }
}

fn subtitle_media_playlist(video_playlist: &[u8]) -> String {
    let timeline = subtitle_timeline(video_playlist);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
        timeline.target_duration, timeline.media_sequence
    );
    if let Some(kind) = &timeline.playlist_type {
        out.push_str(&format!("#EXT-X-PLAYLIST-TYPE:{kind}\n"));
    }
    for window in &timeline.segments {
        out.push_str(&format!(
            "#EXTINF:{:.6},\nseg{:05}.vtt\n",
            window.duration, window.sequence
        ));
    }
    if timeline.endlist {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    out
}

fn parse_vtt_timestamp(raw: &str) -> Option<f64> {
    let fields: Vec<&str> = raw.trim().split(':').collect();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [minutes, seconds] => (
            0.0,
            minutes.parse::<f64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<f64>().ok()?,
            minutes.parse::<f64>().ok()?,
            seconds.parse::<f64>().ok()?,
        ),
        _ => return None,
    };
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn format_vtt_timestamp(seconds: f64) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    let hours = millis / 3_600_000;
    let minutes = millis / 60_000 % 60;
    let seconds = millis / 1000 % 60;
    let millis = millis % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn slice_webvtt(
    bytes: &[u8],
    offset_seconds: f64,
    segment_start: f64,
    segment_end: f64,
) -> Vec<u8> {
    let normalized = String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut shifted = Vec::new();
    for block in normalized.split("\n\n") {
        let mut lines: Vec<String> = block.lines().map(str::to_owned).collect();
        let Some((line_index, start, end, settings)) =
            lines.iter().enumerate().find_map(|(line_index, line)| {
                let (left, right) = line.split_once("-->")?;
                let right = right.trim_start();
                let end_token = right.split_whitespace().next()?;
                let start = parse_vtt_timestamp(left)?;
                let end = parse_vtt_timestamp(end_token)?;
                let settings = right[end_token.len()..].trim_start().to_owned();
                Some((line_index, start, end, settings))
            })
        else {
            shifted.push(block.to_owned());
            continue;
        };
        let start = start - offset_seconds;
        let end = end - offset_seconds;
        if end <= segment_start || start >= segment_end {
            continue;
        }
        let settings = if settings.is_empty() {
            String::new()
        } else {
            format!(" {settings}")
        };
        // A cue that outlives this segment keeps its AUTHORED end time. It
        // used to be clipped to the segment boundary and re-emitted, clipped
        // again, into the next one — so a line of dialogue spanning a 6 s
        // boundary was torn in half and flickered at the seam, and its
        // authored duration was destroyed in both halves. The cue is now
        // emitted whole in every segment window it intersects; a player that
        // sees the same cue twice reconciles it by identity, and worst case
        // draws the same text over the same interval twice, which is
        // invisible. Only the leading edge is still clamped, and only because
        // it has to be: cue times in this scheme are segment-local and WebVTT
        // has no way to spell a negative one. (Carrying session-absolute cue
        // times with X-TIMESTAMP-MAP anchored at 0 would remove that last
        // clamp — it is a change to a wire shape the device has already
        // validated, so it belongs on the §5.4 ladder, not here.)
        lines[line_index] = format!(
            "{} --> {}{}",
            format_vtt_timestamp(start.max(segment_start) - segment_start),
            format_vtt_timestamp(end - segment_start),
            settings
        );
        shifted.push(lines.join("\n"));
    }
    if let Some(header) = shifted
        .first_mut()
        .filter(|header| header.starts_with("WEBVTT"))
    {
        let timestamp = ((segment_start.max(0.0) * 90_000.0).round() as u64) % (1_u64 << 33);
        let mut lines: Vec<String> = header
            .lines()
            .filter(|line| !line.starts_with("X-TIMESTAMP-MAP="))
            .map(str::to_owned)
            .collect();
        lines.push(format!(
            "X-TIMESTAMP-MAP=MPEGTS:{timestamp},LOCAL:00:00:00.000"
        ));
        *header = lines.join("\n");
    }
    let mut out = shifted.join("\n\n");
    out.push_str("\n\n");
    out.into_bytes()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hls_file(subtitle_streams: Vec<SubtitleStream>) -> MediaFile {
        MediaFile {
            id: 5615,
            item_id: 1,
            path: "/media/Scary Movie.mkv".into(),
            size: 19_000_000_000,
            mtime: 1,
            duration_ms: Some(120_000),
            container: Some("mkv".into()),
            video_codec: Some("hevc".into()),
            video_profile: None,
            width: Some(3840),
            height: Some(2160),
            bit_depth: Some(10),
            hdr: Some("dolby_vision".into()),
            hdr_format: Some("Dolby Vision".into()),
            bitrate: Some(40_000_000),
            audio_streams: vec![],
            subtitle_streams,
            scanned_at: 0,
            audio_offset_ms: 0,
            probed: true,
        }
    }

    fn hls_context(
        codecs: &str,
        supplemental_codecs: Option<&str>,
    ) -> crate::transcode::HlsContext {
        crate::transcode::HlsContext {
            file_id: 5615,
            start_seconds: 0.0,
            media_origin_seconds: 0.0,
            codecs: codecs.into(),
            supplemental_codecs: supplemental_codecs.map(str::to_owned),
        }
    }

    fn sdr_context() -> crate::transcode::HlsContext {
        hls_context("avc1.640034,mp4a.40.2", None)
    }

    fn sub(
        codec: &str,
        language: &str,
        title: &str,
        default: bool,
        forced: bool,
    ) -> SubtitleStream {
        SubtitleStream {
            index: 0,
            codec: codec.into(),
            language: Some(language.into()),
            title: Some(title.into()),
            default,
            forced,
            hearing_impaired: false,
        }
    }

    #[test]
    fn playback_audio_offset_is_bounded_and_carried_by_the_session() {
        let request = CreateSession {
            playback_id: "player".into(),
            request_id: Some("attempt".into()),
            height: None,
            subtitle_burn: None,
            native_subtitles: None,
            subtitle: None,
            start: Some(12.0),
            audio: None,
            copy: Some(false),
            aac: None,
            preserve_dolby_vision: None,
            audio_offset_ms: Some(20_000),
        }
        .into_request(7, 1080);

        assert_eq!(request.audio_offset_ms, 15_000);
        assert_eq!(request.file_id, 7);
    }

    #[test]
    fn bitmap_fallback_still_carries_an_explicit_burn_request() {
        let request = CreateSession {
            playback_id: "apple-bitmap".into(),
            request_id: None,
            height: Some(2160),
            subtitle_burn: Some(5),
            native_subtitles: Some(true),
            subtitle: None,
            start: None,
            audio: None,
            copy: None,
            aac: None,
            preserve_dolby_vision: None,
            audio_offset_ms: None,
        }
        .into_request(5615, 2160);
        assert_eq!(request.subtitle_burn, Some(5));
        assert!(matches!(
            request.kind,
            crate::transcode::SessionKind::Transcode { height: 2160 }
        ));
    }

    /// The session's answer is the one that wins once playback attaches, so
    /// it has to be read off the session that was built — not off the
    /// decision that suggested one. A DV remux the client can take delivers
    /// Dolby Vision; the same file behind a rung or a burn delivers SDR,
    /// because the encoder tone-maps it.
    #[test]
    fn a_session_reports_the_dynamic_range_of_the_stream_it_just_built() {
        let file = hls_file(vec![]);
        let copy = |preserve: bool| crate::transcode::SessionKind::Copy {
            aac: false,
            preserve_dolby_vision: preserve,
        };
        assert_eq!(
            session_delivered_dynamic_range(Some(&file), &copy(true)),
            Some("dolby_vision")
        );
        // Stripped: what reaches the client is the compatible base layer.
        let mut base = file.clone();
        base.hdr_format = Some("Dolby Vision · Profile 7 (HDR10-compatible)".into());
        assert_eq!(
            session_delivered_dynamic_range(Some(&base), &copy(false)),
            Some("hdr10")
        );
        assert_eq!(
            session_delivered_dynamic_range(
                Some(&file),
                &crate::transcode::SessionKind::Transcode { height: 1080 }
            ),
            Some("sdr"),
            "every transcode is H.264 8-bit, whatever the source carried"
        );
        // A file that vanished from the store mid-request says nothing at
        // all rather than guessing; the client keeps what it had.
        assert_eq!(session_delivered_dynamic_range(None, &copy(true)), None);
    }

    #[test]
    fn native_hls_master_advertises_selection_language_names_and_forced_metadata() {
        let file = hls_file(vec![
            sub("subrip", "ita", "Forced", true, true),
            sub("subrip", "ita", "Regular", false, false),
            sub("subrip", "eng", "Forced", false, false),
            sub("subrip", "eng", "Regular", false, false),
            sub("webvtt", "eng", "SDH", false, false),
        ]);
        let master = master_playlist(&file, Some(2), &sdr_context());

        assert!(master.starts_with("#EXTM3U\n#EXT-X-VERSION:7\n"));
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=40000000,SUBTITLES=\"subs\""));
        assert!(!master.contains("CODECS="));
        assert!(!master.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(master.contains("NAME=\"English · Forced\",LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES,FORCED=YES,URI=\"subs/2/index.m3u8\""));
        assert!(master.contains(
            "NAME=\"Italian · Forced\",LANGUAGE=\"it\",DEFAULT=NO,AUTOSELECT=YES,FORCED=YES"
        ));
        assert!(master.contains("NAME=\"English · Regular\",LANGUAGE=\"en\",DEFAULT=NO"));
        assert!(master.contains("NAME=\"English · SDH\",LANGUAGE=\"en\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog,public.accessibility.describes-music-and-sound\""));
        assert!(master.ends_with("index.m3u8\n"));
    }

    #[test]
    fn hdr_master_declares_the_range_and_exact_session_codecs() {
        let file = hls_file(vec![]);
        let stripped = hls_context("hvc1.2.4.L150.B0,mp4a.40.2", None);
        let master = master_playlist(&file, None, &stripped);
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=40000000,VIDEO-RANGE=PQ,\
             CODECS=\"hvc1.2.4.L150.B0,mp4a.40.2\""
        ));

        let compatible_dv = hls_context("hvc1.2.4.L150.B0,ec-3", Some("dvh1.08.10/db1p"));
        let master = master_playlist(&file, None, &compatible_dv);
        assert!(master.contains("VIDEO-RANGE=PQ"));
        assert!(master.contains("CODECS=\"hvc1.2.4.L150.B0,ec-3\""));
        assert!(master.contains("SUPPLEMENTAL-CODECS=\"dvh1.08.10/db1p\""));

        // A tone-mapped session keeps its source's HDR library metadata, but
        // its H.264 bytes and master are SDR.
        let transcoded = hls_context("avc1.640034,mp4a.40.2", None);
        let master = master_playlist(&file, None, &transcoded);
        assert!(!master.contains("VIDEO-RANGE="));
        assert!(!master.contains("CODECS="));
    }

    #[test]
    fn hdr_diagnostics_isolate_master_attributes_without_changing_the_child() {
        let file = hls_file(vec![sub("subrip", "eng", "Forced", true, false)]);
        let context = hls_context("hvc1.2.4.L150.B0,ec-3", None);

        let minimal = master_playlist_diagnostic(&file, None, &context, Some("video-only"));
        assert_eq!(
            minimal,
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH=40000000\nindex.m3u8\n"
        );

        let range = master_playlist_diagnostic(&file, None, &context, Some("video-only-range"));
        assert!(range.contains("BANDWIDTH=40000000,VIDEO-RANGE=PQ\n"));
        assert!(!range.contains("CODECS="));
        assert!(!range.contains("SUBTITLES="));

        let codecs = master_playlist_diagnostic(&file, None, &context, Some("video-only-codecs"));
        assert!(codecs.contains("CODECS=\"hvc1.2.4.L150.B0,ec-3\""));
        assert!(!codecs.contains("VIDEO-RANGE="));

        let hdr = master_playlist_diagnostic(&file, None, &context, Some("video-only-hdr"));
        assert!(hdr.contains("VIDEO-RANGE=PQ"));
        assert!(hdr.contains("CODECS=\"hvc1.2.4.L150.B0,ec-3\""));
        assert!(hdr.ends_with("index.m3u8\n"));
    }

    #[test]
    fn hevc_codec_uses_the_published_profile_tier_level_and_constraints() {
        // hvcC version 1, Main 10, High tier, compatibility 0x20000000
        // (RFC 6381 reverse-bit value 4), constraint B0, level 150. This is
        // the header carried by the live HDR regression file's init segment.
        let mut init = vec![0, 0, 0, 21];
        init.extend_from_slice(b"hvcC");
        init.extend_from_slice(&[1, 0x22, 0x20, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 150]);

        assert_eq!(
            hevc_codec_from_init(&init, "hvc1").as_deref(),
            Some("hvc1.2.4.H150.B0")
        );
        assert!(hevc_codec_from_init(&init[..12], "hvc1").is_none());
    }

    /// Who gets the accessibility tag: the muxer's answer first, the track's
    /// name only as a fallback. The fallback is not decoration — files
    /// probed before plurx read the disposition carry `false` for it, and
    /// re-probing is a manual scan, so the naming convention is still the
    /// only signal an existing library has.
    #[test]
    fn sdh_renditions_are_tagged_from_the_disposition_before_the_title() {
        let accessibility = "CHARACTERISTICS=\"public.accessibility.transcribes-spoken-dialog,public.accessibility.describes-music-and-sound\"";
        let flagged = SubtitleStream {
            hearing_impaired: true,
            ..sub("subrip", "eng", "English", false, false)
        };
        assert!(subtitle_characteristics(&flagged).is_some());

        let file = hls_file(vec![
            flagged,
            sub("subrip", "eng", "English SDH", false, false),
            sub("subrip", "eng", "Regular", false, false),
        ]);
        let master = master_playlist(&file, None, &sdr_context());
        assert_eq!(
            master.matches(accessibility).count(),
            2,
            "the flagged track and the named one, and only those: {master}"
        );
        let line = |name: &str| {
            master
                .lines()
                .find(|line| line.contains(&format!("NAME=\"{name}\"")))
                .unwrap_or_else(|| panic!("no rendition named {name} in {master}"))
        };
        assert!(
            line("English · English").contains(accessibility),
            "the disposition tags a track its title says nothing about"
        );
        assert!(line("English · English SDH").contains(accessibility));
        assert!(!line("English · Regular").contains(accessibility));

        // A track with no title at all is not an accessibility track by
        // default — silence is not a claim.
        let untitled = SubtitleStream {
            title: None,
            ..sub("subrip", "eng", "", false, false)
        };
        assert_eq!(subtitle_characteristics(&untitled), None);
    }

    #[test]
    fn duplicate_manual_renditions_do_not_violate_hls_autoselect_uniqueness() {
        let file = hls_file(vec![
            sub("subrip", "eng", "Regular", false, false),
            sub("webvtt", "eng", "Alternate", false, false),
        ]);
        let master = master_playlist(&file, None, &sdr_context());
        assert!(!master.contains("CODECS="));
        assert_eq!(master.matches("AUTOSELECT=NO").count(), 2);

        let selected = master_playlist(&file, Some(1), &sdr_context());
        assert!(selected
            .contains("NAME=\"English · Alternate\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES"));
    }

    #[test]
    fn bitmap_and_styled_subtitles_stay_out_of_native_renditions() {
        let file = hls_file(vec![
            sub("subrip", "eng", "Regular", false, false),
            sub("hdmv_pgs_subtitle", "eng", "PGS", false, false),
            sub("dvd_subtitle", "eng", "VobSub", false, false),
            sub("ass", "eng", "Styled Signs", false, false),
            sub("ssa", "eng", "Styled Dialogue", false, false),
            // The MP4 case, and the common one: every WEB-DL carries these.
            // `SubTrackDto.text` says true of them (they do have text to
            // extract), which is exactly why `native` exists — the master
            // must not carry a rendition this path cannot slice.
            sub("mov_text", "eng", "MP4 Timed Text", false, false),
        ]);
        let master = master_playlist(&file, None, &sdr_context());
        assert!(master.contains("subs/0/index.m3u8"));
        for index in 1..=5 {
            assert!(!master.contains(&format!("subs/{index}/index.m3u8")));
        }
    }

    #[test]
    fn subtitle_playlist_and_vtt_mirror_video_segments_at_resume_timeline() {
        let video = b"#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXTINF:4.000000,\nseg00000.m4s\n#EXTINF:6.000000,\nseg00001.m4s\n";
        let playlist = subtitle_media_playlist(video);
        assert!(playlist.contains("#EXT-X-TARGETDURATION:6"));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(playlist.contains("#EXTINF:4.000000,\nseg00000.vtt"));
        assert!(playlist.contains("#EXTINF:6.000000,\nseg00001.vtt"));
        assert!(!playlist.contains("#EXT-X-ENDLIST"));

        let source = b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\npast\n\ncue-id\n00:00:09.000 --> 00:00:11.000 align:start\ncrossing\n\n00:00:15.250 --> 00:00:16.500\nfuture\n";
        let first = String::from_utf8(slice_webvtt(source, 10.0, 0.0, 4.0)).expect("utf8");
        assert!(first.contains("X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000"));
        assert!(!first.contains("past"));
        assert!(first.contains("00:00:00.000 --> 00:00:01.000 align:start"));
        assert!(!first.contains("future"));

        let second = String::from_utf8(slice_webvtt(source, 10.0, 4.0, 10.0)).expect("utf8");
        assert!(second.contains("X-TIMESTAMP-MAP=MPEGTS:360000,LOCAL:00:00:00.000"));
        assert!(!second.contains("crossing"));
        assert!(second.contains("00:00:01.250 --> 00:00:02.500"));

        let finished = subtitle_media_playlist(
            b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4.0,\nseg00000.m4s\n#EXT-X-ENDLIST\n",
        );
        assert!(finished.ends_with("seg00000.vtt\n#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn forced_detection_reads_words_not_substrings() {
        // The contract case: disposition says nothing, the title says Forced.
        assert!(track_is_forced(&sub(
            "subrip", "ita", "Forced", false, false
        )));
        assert!(track_is_forced(&sub(
            "subrip",
            "eng",
            "English (Forced)",
            false,
            false
        )));
        assert!(track_is_forced(&sub(
            "subrip",
            "eng",
            "forced signs",
            false,
            false
        )));
        // Disposition alone is still enough.
        assert!(track_is_forced(&sub(
            "subrip", "eng", "Regular", false, true
        )));

        // The bug: a substring test hid these tracks from Apple's subtitle
        // menu entirely, because a forced rendition is only offered when the
        // presentation language matches.
        assert!(!track_is_forced(&sub(
            "subrip",
            "eng",
            "Non-Forced",
            false,
            false
        )));
        assert!(!track_is_forced(&sub(
            "subrip",
            "eng",
            "non forced",
            false,
            false
        )));
        assert!(!track_is_forced(&sub(
            "subrip",
            "eng",
            "Not Forced",
            false,
            false
        )));
        assert!(!track_is_forced(&sub(
            "subrip", "eng", "Unforced", false, false
        )));
        assert!(!track_is_forced(&sub(
            "subrip",
            "eng",
            "Reinforced Audio",
            false,
            false
        )));
    }

    #[test]
    fn language_tags_come_from_the_shared_alias_table() {
        // The ten this module used to know.
        assert_eq!(language_tag(Some("eng")), "en");
        assert_eq!(language_tag(Some("zho")), "zh");
        // And the ones it did not, which used to reach AVPlayer as
        // non-BCP-47 codes no viewer preference could ever match.
        assert_eq!(language_tag(Some("dut")), "nl");
        assert_eq!(language_tag(Some("cze")), "cs");
        assert_eq!(language_tag(Some("gre")), "el");
        assert_eq!(language_tag(Some("rum")), "ro");
        assert_eq!(language_name(Some("cze")), "Czech");
        // Unknown stays unknown rather than becoming a guess.
        assert_eq!(language_tag(Some("xyz")), "xyz");
        assert_eq!(language_tag(None), "und");
    }

    #[test]
    fn rendition_names_are_unique_within_the_group() {
        // Two untitled English tracks: RFC 8216 §4.3.4.1 makes NAME
        // MUST-unique, and the client resolves options by name — so a
        // duplicate is the client picking the wrong track, not a wart.
        let file = hls_file(vec![
            SubtitleStream {
                index: 0,
                codec: "subrip".into(),
                language: Some("eng".into()),
                title: None,
                default: false,
                forced: false,
                hearing_impaired: false,
            },
            SubtitleStream {
                index: 1,
                codec: "subrip".into(),
                language: Some("eng".into()),
                title: None,
                default: false,
                forced: false,
                hearing_impaired: false,
            },
        ]);
        let master = master_playlist_with(&file, None, &sdr_context(), MasterRungs::default());
        assert_eq!(master.matches("NAME=\"English\"").count(), 1, "{master}");
        assert!(master.contains("NAME=\"English (2)\""), "{master}");
    }

    #[test]
    fn rendition_attributes_survive_hostile_titles() {
        // A quoted-string attribute has no escape, so a quote, a comma or a
        // line break in a title is not a formatting problem — it is a
        // playlist that no longer parses.
        let file = hls_file(vec![sub(
            "subrip",
            "eng",
            "The \"Good\" One, v2\r\nsecond line",
            false,
            false,
        )]);
        let master = master_playlist_with(&file, None, &sdr_context(), MasterRungs::default());
        let line = master
            .lines()
            .find(|line| line.starts_with("#EXT-X-MEDIA:"))
            .expect("a rendition line");
        assert_eq!(
            line.matches('"').count() % 2,
            0,
            "unbalanced quotes: {line}"
        );
        assert!(!line.contains("\"Good\""), "{line}");
        assert!(line.contains("URI=\"subs/0/index.m3u8\""), "{line}");
        assert_eq!(
            master
                .lines()
                .filter(|l| l.starts_with("#EXT-X-MEDIA:"))
                .count(),
            1
        );
    }

    #[test]
    fn ladder_rungs_are_inert_until_an_operator_lights_them() {
        let file = hls_file(vec![
            sub("subrip", "ita", "Forced", false, true),
            sub("subrip", "ita", "Forced Signs", false, true),
        ]);

        // Default: the shape that plays on the device today. Two forced
        // tracks share a language, so RFC 8216's uniqueness rule keeps them
        // manually selectable.
        let shipped = master_playlist_with(&file, None, &sdr_context(), MasterRungs::default());
        assert!(!shipped.contains("CLOSED-CAPTIONS"), "{shipped}");
        assert_eq!(shipped.matches("AUTOSELECT=NO").count(), 2, "{shipped}");
        assert!(!shipped.contains("CODECS="), "{shipped}");

        // Rung 1, alone.
        let captions = master_playlist_with(
            &file,
            None,
            &sdr_context(),
            MasterRungs {
                closed_captions_none: true,
                ..MasterRungs::default()
            },
        );
        assert!(
            captions.contains(
                "#EXT-X-STREAM-INF:BANDWIDTH=40000000,CLOSED-CAPTIONS=NONE,SUBTITLES=\"subs\""
            ),
            "{captions}"
        );
        assert_eq!(captions.matches("AUTOSELECT=NO").count(), 2, "{captions}");

        // Rung 2, alone.
        let forced = master_playlist_with(
            &file,
            None,
            &sdr_context(),
            MasterRungs {
                forced_autoselect: true,
                ..MasterRungs::default()
            },
        );
        assert!(!forced.contains("CLOSED-CAPTIONS"), "{forced}");
        assert_eq!(forced.matches("AUTOSELECT=YES").count(), 2, "{forced}");
    }

    #[test]
    fn a_cue_spanning_a_segment_boundary_keeps_its_authored_end() {
        // 6 s windows; the cue runs 5.0 → 8.0, straddling the boundary.
        let source = b"WEBVTT\n\nspan\n00:00:05.000 --> 00:00:08.000\ncrossing\n";
        let first = String::from_utf8(slice_webvtt(source, 0.0, 0.0, 6.0)).expect("utf8");
        let second = String::from_utf8(slice_webvtt(source, 0.0, 6.0, 12.0)).expect("utf8");

        // It appears in both windows it intersects...
        assert!(first.contains("crossing"), "{first}");
        assert!(second.contains("crossing"), "{second}");
        // ...and the copy in the first window runs to its AUTHORED end rather
        // than being cut off at the boundary. Clipping it there is what tore
        // a line of dialogue in half and flickered at every 6 s seam.
        assert!(first.contains("00:00:05.000 --> 00:00:08.000"), "{first}");
        // The trailing copy still starts at the window edge, because cue
        // times in this scheme are segment-local and WebVTT cannot spell a
        // negative one. Its identifier is what lets a player reconcile the
        // two.
        assert!(second.contains("00:00:00.000 --> 00:00:02.000"), "{second}");
        assert!(second.contains("span"), "{second}");
    }

    #[test]
    fn cue_shifting_is_relative_to_the_media_origin_not_the_request() {
        // The P0-2 shape, as the slicer sees it: a copy session asked to
        // start at 12.3 s whose media actually begins at the 10 s keyframe.
        // A cue authored at 14.0 s belongs 4.0 s into the session, not 1.7.
        let source = b"WEBVTT\n\n00:00:14.000 --> 00:00:16.000\nline\n";
        let correct = String::from_utf8(slice_webvtt(source, 10.0, 0.0, 6.0)).expect("utf8");
        assert!(
            correct.contains("00:00:04.000 --> 00:00:06.000"),
            "{correct}"
        );

        let by_request = String::from_utf8(slice_webvtt(source, 12.3, 0.0, 6.0)).expect("utf8");
        assert!(by_request.contains("00:00:01.700"), "{by_request}");
        assert!(
            !by_request.contains("00:00:04.000 -->"),
            "shifting by the request leads the picture by the seek's distance from its keyframe"
        );
    }
}
