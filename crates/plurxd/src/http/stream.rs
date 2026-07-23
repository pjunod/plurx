//! Media delivery: direct-play (HTTP range serving of the raw file) and remux
//! (on-the-fly fragmented MP4 via ffmpeg `-c copy`, audio re-encoded only when
//! the target can't take the source codec). Full video transcode is Phase 2;
//! a Transcode verdict here still attempts a remux and says so in `/decision`.

use std::path::Path;
use std::process::Stdio;

use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use plurx_core::domain::MediaFile;
use plurx_core::playback::{self, Decision};
use plurx_core::tracks::is_bitmap_subtitle;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::error::ApiError;
use super::extract::AuthUser;
use crate::state::AppState;

/// ffmpeg binary, overridable via `PLURX_FFMPEG` (jellyfin-ffmpeg / pinned path).
fn ffmpeg_bin() -> String {
    std::env::var("PLURX_FFMPEG")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffmpeg".to_owned())
}

/// ffprobe binary, overridable via `PLURX_FFPROBE` (jellyfin-ffmpeg / pinned).
fn ffprobe_bin() -> String {
    std::env::var("PLURX_FFPROBE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned())
}

async fn load_file(state: &AppState, id: i64) -> Result<MediaFile, ApiError> {
    state
        .store
        .get_file(id)
        .await?
        .ok_or(ApiError::NotFound("file"))
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("mp4") | Some("m4v") | Some("mov") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        Some("ts") | Some("m2ts") => "video/mp2t",
        Some("avi") => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

/// Runtime browser capabilities + a manual quality override, sent by the web
/// player so the server only transcodes what this specific browser can't play.
/// All optional and back-compatible: absent caps fall back to the named
/// `profile` (default `web-h264`). CSV fields are lowercase codec/container
/// short names.
#[derive(Deserialize, Default, Clone)]
pub struct Caps {
    /// Named fallback profile when no caps are reported (e.g. `web-h264`).
    pub profile: Option<String>,
    /// Video codecs the browser can decode, e.g. `h264,hevc,av1`.
    pub vcodec: Option<String>,
    /// Audio codecs, e.g. `aac,ac3,eac3,opus,flac`.
    pub acodec: Option<String>,
    /// Containers playable via `<video src>` (never mkv), e.g. `mp4,webm`.
    pub container: Option<String>,
    /// Max height to direct-play (omit = uncapped; a decodable 4K stream
    /// direct-plays and the browser downscales).
    pub maxheight: Option<i64>,
    /// 1 when HDR may be shown directly (browser decodes it AND display is HDR).
    pub hdr: Option<u8>,
    /// Manual override: `auto` (default) | `original` | `transcode`.
    pub force: Option<String>,
}

fn csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

impl Caps {
    /// True when the client reported real capabilities (vs. only a named profile).
    fn has_caps(&self) -> bool {
        self.vcodec.is_some() || self.acodec.is_some() || self.container.is_some()
    }

    /// The effective device profile: a runtime-probed one when caps were sent,
    /// else the named/default profile (cloned to a single owned type).
    fn profile(&self) -> playback::DeviceProfile {
        if self.has_caps() {
            let containers = {
                let c = csv(&self.container);
                if c.is_empty() {
                    vec!["mp4".into(), "webm".into(), "mov".into()]
                } else {
                    c
                }
            };
            let vcodec = {
                let v = csv(&self.vcodec);
                if v.is_empty() {
                    vec!["h264".into()]
                } else {
                    v
                }
            };
            let acodec = {
                let a = csv(&self.acodec);
                if a.is_empty() {
                    vec!["aac".into(), "mp3".into()]
                } else {
                    a
                }
            };
            playback::caps_profile(
                containers,
                vcodec,
                acodec,
                self.maxheight,
                self.hdr == Some(1),
            )
        } else {
            self.profile
                .as_deref()
                .and_then(playback::profile)
                .unwrap_or_else(playback::default_profile)
                .clone()
        }
    }

    fn force(&self) -> playback::Force {
        self.force
            .as_deref()
            .map(playback::Force::parse)
            .unwrap_or(playback::Force::Auto)
    }

    /// The decision this client should get for `file`.
    fn decide(&self, file: &MediaFile) -> Decision {
        playback::decide_forced(file, &self.profile(), self.force())
    }
}

#[derive(Serialize)]
pub struct AudioTrackDto {
    /// Position among audio streams (`a:{index}` for ffmpeg mapping).
    pub index: i64,
    pub codec: String,
    pub channels: Option<i64>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Serialize)]
pub struct SubTrackDto {
    /// Position among subtitle streams (`s:{index}`).
    pub index: i64,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// Text subs convert to WebVTT for a selectable `<track>`; bitmap subs
    /// (PGS/VobSub) can't and are only burnable via transcode.
    pub text: bool,
}

/// A compact description of the source file's video, for the stats overlay's
/// "source → target" line. Numbers the player already has no cheap way to learn
/// (the browser only sees the transcoded output).
#[derive(Serialize)]
pub struct SourceSummary {
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub video_profile: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub bit_depth: Option<i64>,
    /// "hdr10" | "hlg" | "dolby_vision" | null (SDR/unknown).
    pub hdr: Option<String>,
    /// Rich HDR label for display ("Dolby Vision · Profile 7 (HDR10-compatible)").
    pub hdr_format: Option<String>,
    /// Overall bitrate in bits/sec, if the container reported one.
    pub bitrate: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// A skippable region of the timeline (opening titles, end credits). Derived
/// from real chapter markers when the file has them, otherwise a conservative
/// heuristic for end credits only (`chapter: false`).
#[derive(Serialize)]
pub struct Marker {
    /// "intro" | "credits".
    pub kind: String,
    /// Button label, e.g. "Skip Intro".
    pub label: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// True when this came from an actual chapter title; false for the
    /// duration-based credits guess (so the UI can hedge the wording).
    pub chapter: bool,
}

#[derive(Serialize)]
pub struct DecisionResponse {
    pub file_id: i64,
    #[serde(flatten)]
    pub decision: Decision,
    /// The URL the client should use to play, given the verdict.
    pub play_url: String,
    /// Source video/container facts for the stats overlay.
    pub source: SourceSummary,
    /// Selectable audio tracks (for the player's audio-language menu).
    pub audio: Vec<AudioTrackDto>,
    /// Selectable text subtitle tracks (served as WebVTT sidecars).
    pub subtitles: Vec<SubTrackDto>,
    /// Skippable intro/credits regions (chapter-derived where possible).
    pub markers: Vec<Marker>,
    /// Persisted manual A/V sync correction for this file (positive = audio
    /// later). The player's sync menu edits this and restarts the stream.
    pub audio_offset_ms: i64,
    /// What the container itself declares (audio start − video start), when
    /// nonzero. Diagnostic only — declared offsets are already honored.
    pub declared_offset_ms: Option<i64>,
}

fn source_summary(file: &MediaFile) -> SourceSummary {
    SourceSummary {
        container: file.container.clone(),
        video_codec: file.video_codec.clone(),
        video_profile: file.video_profile.clone(),
        width: file.width,
        height: file.height,
        bit_depth: file.bit_depth,
        hdr: file.hdr.clone(),
        hdr_format: file.hdr_format.clone(),
        bitrate: file.bitrate,
        duration_ms: file.duration_ms,
    }
}

fn audio_tracks(file: &MediaFile) -> Vec<AudioTrackDto> {
    file.audio_streams
        .iter()
        .enumerate()
        .map(|(i, a)| AudioTrackDto {
            index: i as i64,
            codec: a.codec.clone(),
            channels: a.channels,
            language: a.language.clone(),
            title: a.title.clone(),
            default: a.default,
        })
        .collect()
}

fn sub_tracks(file: &MediaFile) -> Vec<SubTrackDto> {
    file.subtitle_streams
        .iter()
        .enumerate()
        .map(|(i, s)| SubTrackDto {
            index: i as i64,
            codec: s.codec.clone(),
            language: s.language.clone(),
            title: s.title.clone(),
            default: s.default,
            forced: s.forced,
            text: !is_bitmap_subtitle(&s.codec),
        })
        .collect()
}

/// Classify a chapter title as an intro or end-credits marker. Case-insensitive
/// substring match against the conventions used by MakeMKV, anime releases, and
/// hand-authored chapters. Returns the marker kind + button label, or `None`.
fn classify_chapter(title: &str) -> Option<(&'static str, &'static str)> {
    let t = title.trim().to_lowercase();
    // Exact single-token anime conventions (OP/ED, non-credit variants).
    if matches!(t.as_str(), "op" | "ncop") {
        return Some(("intro", "Skip Intro"));
    }
    if matches!(t.as_str(), "ed" | "nced") {
        return Some(("credits", "Skip Credits"));
    }
    let intro_kw = [
        "intro",
        "opening",
        "cold open",
        "previously on",
        "recap",
        "title sequence",
        "main titles",
    ];
    let credit_kw = [
        "end credit",
        "credits",
        "ending",
        "outro",
        "closing",
        "next episode",
        "preview",
    ];
    // "Opening Credits" is the front titles, not the tail — intro wins.
    let is_opening_titles = t.contains("opening") && t.contains("credits");
    if is_opening_titles || (intro_kw.iter().any(|k| t.contains(k)) && !t.contains("credit")) {
        return Some(("intro", "Skip Intro"));
    }
    if credit_kw.iter().any(|k| t.contains(k)) {
        return Some(("credits", "Skip Credits"));
    }
    None
}

/// Probe the file's chapters (one `ffprobe` call) and derive skippable
/// intro/credits markers. Falls back to a single duration-based end-credits
/// guess when the file has no usable chapter markers, so the "Skip Credits"
/// affordance still exists on the common case of a chapterless episode.
async fn markers_for(path: &Path, duration_ms: Option<i64>) -> Vec<Marker> {
    let mut out = Vec::new();
    let probe = tokio::process::Command::new(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_chapters",
            "-i",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await;

    if let Ok(o) = probe {
        if o.status.success() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
                if let Some(chapters) = v.get("chapters").and_then(|c| c.as_array()) {
                    for ch in chapters {
                        let title = ch
                            .get("tags")
                            .and_then(|t| t.get("title"))
                            .and_then(|t| t.as_str())
                            .unwrap_or("");
                        let Some((kind, label)) = classify_chapter(title) else {
                            continue;
                        };
                        let start_ms = ch
                            .get("start_time")
                            .and_then(|s| s.as_str())
                            .and_then(|s| s.parse::<f64>().ok())
                            .map(|s| (s * 1000.0) as i64);
                        let end_ms = ch
                            .get("end_time")
                            .and_then(|s| s.as_str())
                            .and_then(|s| s.parse::<f64>().ok())
                            .map(|s| (s * 1000.0) as i64);
                        if let (Some(start_ms), Some(end_ms)) = (start_ms, end_ms) {
                            if end_ms > start_ms {
                                out.push(Marker {
                                    kind: kind.to_owned(),
                                    label: label.to_owned(),
                                    start_ms,
                                    end_ms,
                                    chapter: true,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Heuristic end-credits fallback: only when chapters gave us nothing and we
    // know the runtime. Conservative window (last 60s, or 8% for long films),
    // marked chapter:false so the UI can label it as an estimate.
    let has_credits = out.iter().any(|m| m.kind == "credits");
    if !has_credits {
        if let Some(dur) = duration_ms.filter(|d| *d > 5 * 60_000) {
            let tail = (dur / 12).clamp(45_000, 150_000);
            out.push(Marker {
                kind: "credits".to_owned(),
                label: "Skip Credits".to_owned(),
                start_ms: dur - tail,
                end_ms: dur,
                chapter: false,
            });
        }
    }

    out.sort_by_key(|m| m.start_ms);
    out
}

/// GET /api/v1/files/:id/decision — the web player sends `?vcodec=…&acodec=…&
/// container=…&hdr=…&force=…` (runtime browser capabilities + quality override);
/// native clients still pass `?profile=`.
pub async fn decision(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<Caps>,
) -> Result<Json<DecisionResponse>, ApiError> {
    let file = load_file(&state, id).await?;
    // Never hand back a play URL for a file that isn't on disk — the client
    // would open a player that can never load (the unmounted-share case).
    if tokio::fs::metadata(&file.path).await.is_err() {
        return Err(ApiError::Conflict(
            "this media file is missing on the server — its library path may be \
             unmounted, moved, or renamed"
                .into(),
        ));
    }
    let decision = q.decide(&file);

    let play_url = match decision.method {
        playback::PlaybackMethod::DirectPlay => format!("/api/v1/files/{id}/direct"),
        _ => format!("/api/v1/files/{id}/stream.mp4"),
    };
    let markers = markers_for(&file.path, file.duration_ms).await;

    // Default-track flags: the same selection rule the transcoder burns by —
    // anime dual-audio prefers the original + subs, everything else honors the
    // server's language preferences (Settings → Playback defaults).
    let prefer_original = file
        .audio_streams
        .iter()
        .any(|a| matches!(a.language.as_deref(), Some("jpn" | "ja" | "jp")))
        && file.audio_streams.len() > 1;
    let prefs = state.transcode.lang_prefs().await;
    let selection = plurx_core::tracks::select_tracks(
        &file.audio_streams,
        &file.subtitle_streams,
        prefer_original,
        &prefs,
    );
    let mut audio = audio_tracks(&file);
    if let Some(pick) = selection.audio_index {
        for a in &mut audio {
            a.default = a.index == pick;
        }
    }
    let mut subtitles = sub_tracks(&file);
    for s in &mut subtitles {
        s.default = selection.subtitle_index == Some(s.index);
    }

    // Tell Trakt "watching now" (fire-and-forget), resuming at the known spot.
    let start_pct = state
        .store
        .watch_state(user.id, file.item_id)
        .await
        .ok()
        .flatten()
        .and_then(|w| {
            w.duration_ms
                .filter(|d| *d > 0)
                .map(|d| (w.position_ms as f64 / d as f64 * 100.0).clamp(0.0, 100.0))
        })
        .unwrap_or(0.0);
    state.trakt.on_start(user.id, file.item_id, start_pct);

    Ok(Json(DecisionResponse {
        file_id: id,
        source: source_summary(&file),
        decision,
        play_url,
        audio,
        subtitles,
        markers,
        audio_offset_ms: file.audio_offset_ms,
        declared_offset_ms: declared_av_offset(&state, id).await,
    }))
}

/// The container's own per-stream start-time story: audio start minus video
/// start, in ms, from the scan-time ffprobe JSON. Display-only — a *declared*
/// offset is usually correct sync (ffmpeg honors it), so it's never
/// auto-applied; it's shown in the player's sync menu as a diagnostic.
async fn declared_av_offset(state: &AppState, file_id: i64) -> Option<i64> {
    let raw = state
        .store
        .get_file_probe_json(file_id)
        .await
        .ok()
        .flatten()?;
    let probe: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let streams = probe.get("streams")?.as_array()?;
    let start_of = |kind: &str| -> Option<f64> {
        streams
            .iter()
            .find(|s| s.get("codec_type").and_then(|v| v.as_str()) == Some(kind))
            .and_then(|s| s.get("start_time"))
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<f64>().ok())
    };
    let (v, a) = (start_of("video")?, start_of("audio")?);
    let ms = ((a - v) * 1000.0).round() as i64;
    (ms != 0).then_some(ms)
}

#[derive(Deserialize)]
pub struct AudioOffsetRequest {
    pub offset_ms: i64,
}

/// PUT /api/v1/files/:id/audio-offset — persist a manual A/V sync correction
/// (positive = delay audio). Sticks to the file, so the fix survives across
/// sessions and users; the player restarts its stream to apply it.
pub async fn set_audio_offset(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Json(req): Json<AudioOffsetRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if state.store.get_file(id).await?.is_none() {
        return Err(ApiError::NotFound("file"));
    }
    // ±15s covers any real-world desync; beyond that it's a different problem.
    let offset = req.offset_ms.clamp(-15_000, 15_000);
    state.store.set_file_audio_offset(id, offset).await?;
    Ok(Json(serde_json::json!({ "audio_offset_ms": offset })))
}

/// GET /api/v1/files/:id/subs/{index}.vtt — extract subtitle stream `index`
/// and convert it to WebVTT on the fly, for a native `<track>`. Auth is by
/// `?token=` (a `<track>` element can't set headers). Text subs only; a bitmap
/// sub (PGS/VobSub) can't become VTT and returns 415.
pub async fn subtitles_vtt(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath((id, index)): AxPath<(i64, i64)>,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let stream = file
        .subtitle_streams
        .get(index as usize)
        .ok_or(ApiError::NotFound("subtitle track"))?;
    if is_bitmap_subtitle(&stream.codec) {
        return Err(ApiError::BadRequest(
            "this is a bitmap subtitle (PGS/VobSub) and can't be shown as text; \
             it can only be burned in during transcode"
                .into(),
        ));
    }
    let out = tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&file.path)
        .args(["-map", &format!("0:s:{index}"), "-f", "webvtt", "pipe:1"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| ApiError::Internal(format!("spawning ffmpeg: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(
            file_id = id,
            index,
            "subtitle extraction failed: {}",
            why.trim()
        );
        return Err(ApiError::Internal("subtitle extraction failed".into()));
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out.stdout,
    )
        .into_response())
}

/// GET /api/v1/files/:id/direct — raw file with HTTP range support.
pub async fn direct(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    serve_file_range(&file.path, &headers).await
}

// The caps fields are inlined (not `#[serde(flatten)]`ed) because axum's
// urlencoded Query decoder doesn't support flatten.
#[derive(Deserialize)]
pub struct StreamQuery {
    /// Start offset in seconds (used for resume; remux fast-seeks the input).
    pub start: Option<f64>,
    /// Which audio stream to map (`a:{audio}`); default 0. Lets the client
    /// switch audio language — a non-default pick forces a remux so the chosen
    /// track is the one in the MP4.
    pub audio: Option<i64>,
    // Same runtime-caps fields as `/decision`, so the remux copies the audio
    // when the browser can play it (vs. re-encoding to AAC needlessly).
    pub profile: Option<String>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub container: Option<String>,
    pub maxheight: Option<i64>,
    pub hdr: Option<u8>,
    pub force: Option<String>,
}

impl StreamQuery {
    fn caps(&self) -> Caps {
        Caps {
            profile: self.profile.clone(),
            vcodec: self.vcodec.clone(),
            acodec: self.acodec.clone(),
            container: self.container.clone(),
            maxheight: self.maxheight,
            hdr: self.hdr,
            force: self.force.clone(),
        }
    }
}

/// GET /api/v1/files/:id/stream.mp4 — fragmented-MP4 remux, optional start.
pub async fn stream_mp4(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let decision = q.caps().decide(&file);
    let audio = q.audio.unwrap_or(0).max(0);
    // Copy HEVC gets an `hvc1` tag so Safari's <video> accepts the fMP4 (an
    // `hev1`-tagged MKV copy otherwise plays audio-only / black in Safari).
    let hevc = matches!(file.video_codec.as_deref(), Some("hevc" | "h265"));
    remux(
        &file.path,
        q.start,
        decision.transcode_audio,
        audio,
        file.audio_offset_ms,
        hevc,
    )
    .await
}

// --- direct-play range serving ---------------------------------------------

/// Parse a single-range `Range: bytes=start-end` header against a known length.
/// Returns `(start, end_inclusive)`.
fn parse_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    let raw = headers.get(header::RANGE)?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    // Only the first range is honored (browsers send one).
    let first = spec.split(',').next()?.trim();
    let (start_s, end_s) = first.split_once('-')?;
    let (start, end) = if start_s.is_empty() {
        // Suffix range: bytes=-N → last N bytes.
        let n: u64 = end_s.parse().ok()?;
        if n == 0 {
            return None;
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = start_s.parse().ok()?;
        let end = if end_s.is_empty() {
            len - 1
        } else {
            end_s.parse::<u64>().ok()?.min(len - 1)
        };
        (start, end)
    };
    if start > end || start >= len {
        return None;
    }
    Some((start, end))
}

/// HTTP range serving of a file (direct play). Shared by the native part
/// endpoint and the Plex-compat `/library/parts/...` endpoint.
pub(crate) async fn serve_file_range(
    path: &Path,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let mut fh = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::NotFound("file on disk"))?;
    let len = fh
        .metadata()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .len();
    let ctype = content_type(path);

    match parse_range(headers, len) {
        Some((start, end)) => {
            let count = end - start + 1;
            fh.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let stream = tokio_util::io::ReaderStream::new(fh.take(count));
            Ok((
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ctype.to_owned()),
                    (header::ACCEPT_RANGES, "bytes".to_owned()),
                    (header::CONTENT_LENGTH, count.to_string()),
                    (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")),
                ],
                Body::from_stream(stream),
            )
                .into_response())
        }
        None => {
            let stream = tokio_util::io::ReaderStream::new(fh);
            Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ctype.to_owned()),
                    (header::ACCEPT_RANGES, "bytes".to_owned()),
                    (header::CONTENT_LENGTH, len.to_string()),
                ],
                Body::from_stream(stream),
            )
                .into_response())
        }
    }
}

// --- remux ------------------------------------------------------------------

async fn remux(
    path: &Path,
    start: Option<f64>,
    transcode_audio: bool,
    audio_index: i64,
    audio_offset_ms: i64,
    hevc: bool,
) -> Result<Response, ApiError> {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    // Input-side seek (fast) for resume.
    if let Some(s) = start.filter(|s| *s > 0.0) {
        cmd.arg("-ss").arg(format!("{s:.3}"));
    }
    cmd.arg("-i").arg(path);
    // A persisted A/V sync correction rides in on a second input of the same
    // file, shifted with -itsoffset (positive = audio later) and used only for
    // its audio. Same input-seek so resume stays aligned; make_zero below
    // shifts all streams by one shared amount, preserving the correction.
    let audio_input = if audio_offset_ms != 0 {
        if let Some(s) = start.filter(|s| *s > 0.0) {
            cmd.arg("-ss").arg(format!("{s:.3}"));
        }
        cmd.arg("-itsoffset")
            .arg(format!("{:.3}", audio_offset_ms as f64 / 1000.0));
        cmd.arg("-i").arg(path);
        1
    } else {
        0
    };
    // Video + the chosen audio track, no subtitles into the MP4.
    cmd.args([
        "-map",
        "0:v:0",
        "-map",
        &format!("{audio_input}:a:{audio_index}?"),
        "-sn",
    ]);
    cmd.args(["-c:v", "copy"]);
    // Safari only decodes HEVC in MP4 when the sample entry is tagged `hvc1`;
    // MKV HEVC is commonly `hev1`, which Safari renders black. Harmless for a
    // stream that's already hvc1. Video-stream-scoped so H.264 is untouched.
    if hevc {
        cmd.args(["-tag:v", "hvc1"]);
    }
    if transcode_audio {
        cmd.args(["-c:a", "aac", "-ac", "2", "-b:a", "256k"]);
    } else {
        cmd.args(["-c:a", "copy"]);
    }
    // Fragmented MP4 so it streams without a seekable output.
    // `-avoid_negative_ts make_zero` normalizes the first timestamp to zero: a
    // source container that starts at a non-zero (or negative) PTS — very common
    // in MKV remuxes — otherwise yields a first fragment with a non-zero
    // baseMediaDecodeTime that some browsers sit on forever (gray screen, no
    // error). Harmless when the input already starts at zero.
    // `delay_moov` holds the init moov until the first packet, so codecs whose
    // sample entry needs a packet peek — AC-3/E-AC-3 copy especially — don't
    // fail with "cannot write moov atom before AC3 packets". Harmless for
    // AAC/H.264 (verified: ftyp+moov still lead the stream).
    cmd.args([
        "-avoid_negative_ts",
        "make_zero",
        "-movflags",
        "frag_keyframe+empty_moov+default_base_moof+delay_moov",
        "-f",
        "mp4",
        "pipe:1",
    ]);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ApiError::Internal(format!("spawning ffmpeg: {e}")))?;

    // Surface remux failures: ffmpeg runs at -loglevel error, so a codec/copy
    // problem (e.g. jellyfin-ffmpeg refusing a stream the old build accepted)
    // otherwise yields an empty pipe and a blank player with nothing logged.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!("remux ffmpeg: {line}");
            }
        });
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ApiError::Internal("ffmpeg stdout unavailable".into()))?;

    // Stream ffmpeg stdout; the Child rides along in the stream state and is
    // killed (kill_on_drop) if the client disconnects mid-stream.
    let reader = tokio::io::BufReader::new(stdout);
    let stream = futures_util::stream::unfold((child, reader), |(child, mut reader)| async move {
        let mut buf = vec![0u8; 64 * 1024];
        match reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from(buf)),
                    (child, reader),
                ))
            }
            Err(e) => {
                tracing::warn!(error = %e, "remux stream read error");
                None
            }
        }
    });

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "video/mp4")],
        Body::from_stream(stream),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_range(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, value.parse().expect("valid header"));
        h
    }

    #[test]
    fn range_parsing() {
        let len = 1000;
        assert_eq!(
            parse_range(&headers_with_range("bytes=0-99"), len),
            Some((0, 99))
        );
        assert_eq!(
            parse_range(&headers_with_range("bytes=100-"), len),
            Some((100, 999))
        );
        assert_eq!(
            parse_range(&headers_with_range("bytes=-100"), len),
            Some((900, 999))
        );
        // Open end clamps to len-1.
        assert_eq!(
            parse_range(&headers_with_range("bytes=0-99999"), len),
            Some((0, 999))
        );
        // Invalid / out of range.
        assert_eq!(
            parse_range(&headers_with_range("bytes=2000-3000"), len),
            None
        );
        assert_eq!(parse_range(&headers_with_range("bytes=500-100"), len), None);
        assert_eq!(parse_range(&HeaderMap::new(), len), None);
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type(Path::new("a.mp4")), "video/mp4");
        assert_eq!(content_type(Path::new("a.mkv")), "video/x-matroska");
        assert_eq!(content_type(Path::new("a.webm")), "video/webm");
    }

    #[test]
    fn chapter_classification() {
        assert_eq!(classify_chapter("Intro").map(|m| m.0), Some("intro"));
        assert_eq!(classify_chapter("Opening").map(|m| m.0), Some("intro"));
        assert_eq!(classify_chapter("OP").map(|m| m.0), Some("intro"));
        assert_eq!(
            classify_chapter("Previously On").map(|m| m.0),
            Some("intro")
        );
        assert_eq!(
            classify_chapter("End Credits").map(|m| m.0),
            Some("credits")
        );
        assert_eq!(classify_chapter("Ending").map(|m| m.0), Some("credits"));
        assert_eq!(classify_chapter("ED").map(|m| m.0), Some("credits"));
        // "Opening Credits" is the intro, not the end credits.
        assert_eq!(
            classify_chapter("Opening Credits").map(|m| m.0),
            Some("intro")
        );
        // Ordinary content chapters are not markers.
        assert_eq!(classify_chapter("Chapter 1"), None);
        assert_eq!(classify_chapter("The Heist"), None);
    }
}
