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
use serde::Deserialize;
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

#[derive(Deserialize)]
pub struct DecisionQuery {
    pub profile: Option<String>,
}

#[derive(serde::Serialize)]
pub struct DecisionResponse {
    pub file_id: i64,
    #[serde(flatten)]
    pub decision: Decision,
    /// The URL the client should use to play, given the verdict.
    pub play_url: String,
}

/// GET /api/v1/files/:id/decision?profile=web-h264
pub async fn decision(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<DecisionQuery>,
) -> Result<Json<DecisionResponse>, ApiError> {
    let file = load_file(&state, id).await?;
    let profile = q
        .profile
        .as_deref()
        .and_then(playback::profile)
        .unwrap_or_else(playback::default_profile);
    let decision = playback::decide(&file, profile);

    let play_url = match decision.method {
        playback::PlaybackMethod::DirectPlay => format!("/api/v1/files/{id}/direct"),
        _ => format!("/api/v1/files/{id}/stream.mp4"),
    };
    Ok(Json(DecisionResponse {
        file_id: id,
        decision,
        play_url,
    }))
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

#[derive(Deserialize)]
pub struct StreamQuery {
    /// Start offset in seconds (used for resume; remux fast-seeks the input).
    pub start: Option<f64>,
    pub profile: Option<String>,
}

/// GET /api/v1/files/:id/stream.mp4 — fragmented-MP4 remux, optional start.
pub async fn stream_mp4(
    _user: AuthUser,
    State(state): State<AppState>,
    AxPath(id): AxPath<i64>,
    Query(q): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let file = load_file(&state, id).await?;
    let profile = q
        .profile
        .as_deref()
        .and_then(playback::profile)
        .unwrap_or_else(playback::default_profile);
    let decision = playback::decide(&file, profile);
    remux(&file.path, q.start, decision.transcode_audio).await
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
) -> Result<Response, ApiError> {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    // Input-side seek (fast) for resume.
    if let Some(s) = start.filter(|s| *s > 0.0) {
        cmd.arg("-ss").arg(format!("{s:.3}"));
    }
    cmd.arg("-i").arg(path);
    // First video + first audio, no subtitles into the MP4.
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?", "-sn"]);
    cmd.args(["-c:v", "copy"]);
    if transcode_audio {
        cmd.args(["-c:a", "aac", "-ac", "2", "-b:a", "256k"]);
    } else {
        cmd.args(["-c:a", "copy"]);
    }
    // Fragmented MP4 so it streams without a seekable output.
    cmd.args([
        "-movflags",
        "frag_keyframe+empty_moov+default_base_moof",
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
}
