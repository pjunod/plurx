//! Media inspection via `ffprobe`.
//!
//! ffprobe-as-subprocess is the pragmatic ground truth every shipping media
//! server uses (ARCHITECTURE §4): it reliably reports codec profiles, bit
//! depth, HDR transfer characteristics, audio layouts, and subtitle streams
//! across every container. The raw JSON is retained verbatim so the Phase 2
//! decision engine can consult fields we don't model yet.

use std::path::Path;

use serde_json::Value;

use crate::domain::{AudioStream, ProbeResult, SubtitleStream};
use crate::error::ProbeError;

/// The ffprobe binary name; overridable via `PLURX_FFPROBE` for jellyfin-ffmpeg
/// or a pinned path.
fn ffprobe_bin() -> String {
    std::env::var("PLURX_FFPROBE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "ffprobe".to_owned())
}

/// Probe a file. Returns a best-effort [`ProbeResult`]; unreadable/duration-less
/// files still yield a result (with `None` fields) rather than an error, so a
/// weird file doesn't abort a scan. Errors are reserved for ffprobe being
/// missing or emitting unparseable output.
pub async fn probe(path: &Path) -> Result<ProbeResult, ProbeError> {
    let output = tokio::process::Command::new(ffprobe_bin())
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .await
        .map_err(|e| ProbeError::Spawn(e.to_string()))?;

    if !output.status.success() {
        return Err(ProbeError::Failed {
            path: path.display().to_string(),
            code: output.status.code(),
        });
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| ProbeError::Parse(format!("ffprobe json: {e}")))?;
    let mut result = parse_probe_json(&json);
    // Container comes from the extension — the decision engine keys on it
    // ("mkv" → remux, "mp4" → direct) and it's more reliable than ffmpeg's
    // comma-joined format_name.
    result.container = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    Ok(result)
}

/// Pure parser over ffprobe JSON — unit-testable without spawning anything.
pub fn parse_probe_json(json: &Value) -> ProbeResult {
    let mut result = ProbeResult {
        raw_json: serde_json::to_string(json).ok(),
        ..Default::default()
    };

    if let Some(format) = json.get("format") {
        result.duration_ms = format
            .get("duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|secs| (secs * 1000.0) as i64);
        result.bitrate = format
            .get("bit_rate")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok());
    }

    let empty = Vec::new();
    let streams = json
        .get("streams")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    let (mut audio_i, mut sub_i) = (0i64, 0i64);
    let mut video_seen = false;

    for stream in streams {
        match stream.get("codec_type").and_then(|v| v.as_str()) {
            Some("video") if !video_seen => {
                // Skip attached cover art / thumbnails.
                if is_attached_pic(stream) {
                    continue;
                }
                video_seen = true;
                result.video_codec = str_field(stream, "codec_name");
                result.video_profile = str_field(stream, "profile");
                result.width = int_field(stream, "width");
                result.height = int_field(stream, "height");
                result.bit_depth = video_bit_depth(stream);
                result.hdr = detect_hdr(stream);
                result.hdr_format = detect_hdr_format(stream);
            }
            Some("audio") => {
                result.audio_streams.push(AudioStream {
                    index: audio_i,
                    codec: str_field(stream, "codec_name").unwrap_or_default(),
                    channels: int_field(stream, "channels"),
                    language: tag(stream, "language"),
                    title: tag(stream, "title"),
                    default: disposition(stream, "default"),
                });
                audio_i += 1;
            }
            Some("subtitle") => {
                result.subtitle_streams.push(SubtitleStream {
                    index: sub_i,
                    codec: str_field(stream, "codec_name").unwrap_or_default(),
                    language: tag(stream, "language"),
                    title: tag(stream, "title"),
                    default: disposition(stream, "default"),
                    forced: disposition(stream, "forced"),
                });
                sub_i += 1;
            }
            _ => {}
        }
    }
    result
}

fn str_field(stream: &Value, key: &str) -> Option<String> {
    stream
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn int_field(stream: &Value, key: &str) -> Option<i64> {
    stream.get(key).and_then(|v| v.as_i64())
}

fn tag(stream: &Value, key: &str) -> Option<String> {
    stream
        .get("tags")
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn disposition(stream: &Value, key: &str) -> bool {
    stream
        .get("disposition")
        .and_then(|d| d.get(key))
        .and_then(|v| v.as_i64())
        .map(|n| n != 0)
        .unwrap_or(false)
}

fn is_attached_pic(stream: &Value) -> bool {
    disposition(stream, "attached_pic")
}

/// Bit depth from `bits_per_raw_sample`, falling back to the pixel format
/// (e.g. `yuv420p10le` → 10).
fn video_bit_depth(stream: &Value) -> Option<i64> {
    if let Some(bits) = stream
        .get("bits_per_raw_sample")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok())
    {
        return Some(bits);
    }
    let pix = stream.get("pix_fmt").and_then(|v| v.as_str())?;
    if pix.contains("12le") || pix.contains("12be") || pix.contains("p012") {
        Some(12)
    } else if pix.contains("10le") || pix.contains("10be") || pix.contains("p010") {
        Some(10)
    } else {
        Some(8)
    }
}

/// Classify HDR from color transfer + Dolby Vision side data.
/// Returns "dolby_vision" | "hdr10" | "hlg" | None (SDR/unknown).
fn detect_hdr(stream: &Value) -> Option<String> {
    // Dolby Vision: DOVI config in side_data, or a DV codec tag/profile.
    let has_dovi_side_data = stream
        .get("side_data_list")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter().any(|sd| {
                sd.get("side_data_type")
                    .and_then(|v| v.as_str())
                    .map(|t| t.contains("DOVI") || t.contains("Dolby Vision"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let dv_tag = stream
        .get("codec_tag_string")
        .and_then(|v| v.as_str())
        .map(|t| matches!(t, "dvh1" | "dvhe" | "dav1" | "dvav"))
        .unwrap_or(false);
    if has_dovi_side_data || dv_tag {
        return Some("dolby_vision".to_owned());
    }

    match stream.get("color_transfer").and_then(|v| v.as_str()) {
        Some("smpte2084") => Some("hdr10".to_owned()),
        Some("arib-std-b67") => Some("hlg".to_owned()),
        _ => None,
    }
}

/// A richer, human HDR label for display — the Dolby Vision profile number and
/// compatibility, HDR10+ vs HDR10, HLG. Parallels [`detect_hdr`] (which stays
/// coarse for the decision engine); returns None for SDR.
fn detect_hdr_format(stream: &Value) -> Option<String> {
    let side = stream.get("side_data_list").and_then(|v| v.as_array());

    // Dolby Vision: pull the profile + base-layer compatibility from the DOVI
    // configuration record. Compatibility id tells you what a non-DV client
    // sees: 1 = HDR10, 6 = Blu-ray HDR10, 4 = HLG, 2 = SDR.
    if let Some(list) = side {
        for sd in list {
            let t = sd
                .get("side_data_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if t.contains("DOVI") || t.contains("Dolby Vision") {
                let mut label = match sd.get("dv_profile").and_then(|v| v.as_i64()) {
                    Some(p) => format!("Dolby Vision · Profile {p}"),
                    None => "Dolby Vision".to_owned(),
                };
                match sd
                    .get("dv_bl_signal_compatibility_id")
                    .and_then(|v| v.as_i64())
                {
                    Some(1) | Some(6) => label.push_str(" (HDR10-compatible)"),
                    Some(4) => label.push_str(" (HLG-compatible)"),
                    _ => {}
                }
                return Some(label);
            }
        }
    }
    // A DV codec tag with no config record: name it without a profile.
    if stream
        .get("codec_tag_string")
        .and_then(|v| v.as_str())
        .map(|t| matches!(t, "dvh1" | "dvhe" | "dav1" | "dvav"))
        .unwrap_or(false)
    {
        return Some("Dolby Vision".to_owned());
    }

    // HDR10+ carries dynamic metadata (SMPTE 2094-40) as side data.
    if let Some(list) = side {
        if list.iter().any(|sd| {
            sd.get("side_data_type")
                .and_then(|v| v.as_str())
                .map(|t| t.contains("Dynamic Metadata") || t.contains("SMPTE2094"))
                .unwrap_or(false)
        }) {
            return Some("HDR10+".to_owned());
        }
    }

    match stream.get("color_transfer").and_then(|v| v.as_str()) {
        Some("smpte2084") => Some("HDR10".to_owned()),
        Some("arib-std-b67") => Some("HLG".to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_hdr10_movie() {
        let j = json!({
            "format": { "duration": "7200.5", "bit_rate": "25000000" },
            "streams": [
                { "codec_type": "video", "codec_name": "hevc", "profile": "Main 10",
                  "width": 3840, "height": 2160, "pix_fmt": "yuv420p10le",
                  "color_transfer": "smpte2084" },
                { "codec_type": "audio", "codec_name": "truehd", "channels": 8,
                  "disposition": { "default": 1 }, "tags": { "language": "eng" } },
                { "codec_type": "audio", "codec_name": "ac3", "channels": 6,
                  "tags": { "language": "fre" } },
                { "codec_type": "subtitle", "codec_name": "subrip",
                  "disposition": { "default": 0, "forced": 0 },
                  "tags": { "language": "eng" } }
            ]
        });
        let p = parse_probe_json(&j);
        assert_eq!(p.duration_ms, Some(7_200_500));
        assert_eq!(p.bitrate, Some(25_000_000));
        assert_eq!(p.video_codec.as_deref(), Some("hevc"));
        assert_eq!(p.width, Some(3840));
        assert_eq!(p.bit_depth, Some(10));
        assert_eq!(p.hdr.as_deref(), Some("hdr10"));
        assert_eq!(p.audio_streams.len(), 2);
        assert_eq!(p.audio_streams[0].codec, "truehd");
        assert_eq!(p.audio_streams[0].channels, Some(8));
        assert!(p.audio_streams[0].default);
        assert_eq!(p.audio_streams[1].index, 1);
        assert_eq!(p.subtitle_streams.len(), 1);
        assert_eq!(p.subtitle_streams[0].language.as_deref(), Some("eng"));
    }

    #[test]
    fn hdr_format_reports_dv_profile_and_compat() {
        let j = json!({
            "streams": [{
                "codec_type": "video", "codec_name": "hevc",
                "side_data_list": [{
                    "side_data_type": "DOVI configuration record",
                    "dv_profile": 7, "dv_bl_signal_compatibility_id": 6
                }]
            }]
        });
        let p = parse_probe_json(&j);
        assert_eq!(p.hdr.as_deref(), Some("dolby_vision")); // coarse type unchanged
        assert_eq!(
            p.hdr_format.as_deref(),
            Some("Dolby Vision · Profile 7 (HDR10-compatible)")
        );
    }

    #[test]
    fn hdr_format_plain_hdr10() {
        let j = json!({
            "streams": [{ "codec_type": "video", "codec_name": "hevc",
                          "color_transfer": "smpte2084" }]
        });
        assert_eq!(parse_probe_json(&j).hdr_format.as_deref(), Some("HDR10"));
    }

    #[test]
    fn detects_dolby_vision_via_side_data() {
        let j = json!({
            "streams": [{
                "codec_type": "video", "codec_name": "hevc",
                "side_data_list": [{ "side_data_type": "DOVI configuration record" }]
            }]
        });
        assert_eq!(parse_probe_json(&j).hdr.as_deref(), Some("dolby_vision"));
    }

    #[test]
    fn plain_sdr_has_no_hdr() {
        let j = json!({
            "format": { "duration": "1200.0" },
            "streams": [
                { "codec_type": "video", "codec_name": "h264", "width": 1920,
                  "height": 1080, "pix_fmt": "yuv420p" }
            ]
        });
        let p = parse_probe_json(&j);
        assert_eq!(p.hdr, None);
        assert_eq!(p.bit_depth, Some(8));
        assert!(p.audio_streams.is_empty());
    }

    #[test]
    fn skips_attached_cover_art() {
        let j = json!({
            "streams": [
                { "codec_type": "video", "codec_name": "mjpeg",
                  "disposition": { "attached_pic": 1 } },
                { "codec_type": "video", "codec_name": "h264", "width": 1280, "height": 720 }
            ]
        });
        let p = parse_probe_json(&j);
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.width, Some(1280));
    }
}
